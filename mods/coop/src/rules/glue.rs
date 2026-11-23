// MIT License
//
// Copyright © 2026 Yume-13
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! The session rules' hooks and game calls. Pause and upgrade calls run from
//! the run tick (as `oController`); the downed state is held on `oPowers`
//! and `oPlayer`. The mod's own pause calls are flagged so its hooks let
//! them through; hot-path hooks return after one atomic load when off.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ndml_sdk::{Builtin, Call, Error, Instance, ModContext, Script, Value, Variable};

use super::machine::{
    Action, Death, Observed, PauseCall, PlayerAct, PowersAct, RunCtx, UnpauseCall,
};
use super::score::Score;
use super::xp::XpRead;
use crate::game::{self, call_b, info_once, num, truthy, warn_once};
use crate::gate::GatePhase;
use crate::names::{self, rules_fn, rules_obj, rules_var as rv};
use crate::{Shared, net};

/// Everything the rules resolve at init, plus the hot-path flags.
#[derive(Debug)]
pub struct RulesHooks {
    /// A pause/unpause/upgrade call the mod makes itself is in flight.
    own: AtomicBool,
    /// The rules are on for this run.
    active: AtomicBool,
    xp: Option<Variable>,
    max_exp: Option<Variable>,
    base_level: Option<Variable>,
    levelup: Option<Variable>,
    /// Globals: `playerScore`, `scoreVerifier`, `scoreVerifierModulus`,
    /// `bossKills`, `starEaterKills`.
    score: [Option<Variable>; 5],
    final_boss: Option<Variable>,
    invulnerable: Option<Variable>,
    iframes: Option<Variable>,
    control: Option<Variable>,
    can_fire: Option<Variable>,
    shield_max: Option<Variable>,
    hull_reserved: Option<Variable>,
    last_stand: Option<Variable>,
    allow_death: Option<Variable>,
    goto_upgrade: Option<Script>,
    leave_upgrade: Option<Script>,
    refresh_loadout: Option<Script>,
    update_score: Option<Script>,
    window_has_focus: Option<Builtin>,
    asset_get_index: Option<Builtin>,
    kb_pressed: Option<Builtin>,
    /// `oPowers` looked up yet, and what was found.
    powers: Mutex<(bool, Option<Value>)>,
    /// What a down does to what follows the ship (`downed`).
    pub(crate) downed: super::downed::Downed,
    /// What follows the ship is gone (this game's ship is down; `downed`).
    followers_hidden: AtomicBool,
}

impl RulesHooks {
    /// Resolve every name (missing ones are logged; what needs them is off).
    #[must_use]
    pub fn resolve(ctx: &ModContext) -> Self {
        let v = |n| game::var(ctx, n);
        let sc = |n| game::script(ctx, n);
        let b = |n| game::builtin(ctx, n);
        Self {
            own: AtomicBool::new(false),
            active: AtomicBool::new(false),
            xp: v(rv::XP),
            max_exp: v(rv::MAX_EXP),
            base_level: v(rv::BASE_LEVEL),
            levelup: v(rv::LEVELUP),
            score: [
                v(rv::PLAYER_SCORE),
                v(rv::SCORE_VERIFIER),
                v(rv::SCORE_MODULUS),
                v(rv::BOSS_KILLS),
                v(rv::STAR_EATER_KILLS),
            ],
            final_boss: v(rv::FINAL_BOSS_DEFEATED),
            invulnerable: v(rv::INVULNERABLE_TIME),
            iframes: v(rv::IFRAMES),
            control: v(rv::PLAYER_CONTROL),
            can_fire: v(rv::CAN_FIRE),
            shield_max: v(rv::SHIELD_MAX),
            hull_reserved: v(rv::HULL_RESERVED),
            last_stand: v(rv::LAST_STAND),
            allow_death: v(rv::ALLOW_PLAYER_DEATH),
            goto_upgrade: sc(names::GOTO_UPGRADE),
            leave_upgrade: sc(names::LEAVE_UPGRADE),
            refresh_loadout: sc(names::REFRESH_LOADOUT),
            update_score: sc(names::UPDATE_SCORE),
            window_has_focus: b(rules_fn::WINDOW_HAS_FOCUS),
            asset_get_index: b(rules_fn::ASSET_GET_INDEX),
            kb_pressed: b(rules_fn::KEYBOARD_CHECK_PRESSED),
            powers: Mutex::new((false, None)),
            downed: super::downed::Downed::resolve(ctx),
            followers_hidden: AtomicBool::new(false),
        }
    }

    /// The run ended (menu): the hot-path hooks go quiet, and a down of that
    /// run is over.
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.downed.end_run();
    }

    fn own(&self) -> bool {
        self.own.load(Ordering::Relaxed)
    }

    fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// `oPowers` as an object ref (`asset_get_index`, cached).
    fn powers_obj(&self, call: &Call<'_>) -> Option<Value> {
        let mut cache = self.powers.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.0 {
            return cache.1.clone();
        }
        let found =
            call_b(call, self.asset_get_index, &[rules_obj::POWERS.into()]).filter(|v| match v {
                Value::Ref(_) => true,
                other => other.as_f64().is_some_and(|x| x >= 0.0),
            });
        if found.is_none() {
            warn_once(
                "rules-powers",
                "co-op rules: oPowers not found; the downed state is off",
            );
        }
        *cache = (true, found.clone());
        found
    }
}

/// Run `f` (a call into the game) flagged as the mod's own.
pub(crate) fn own_call<T>(s: &Shared, f: impl FnOnce() -> T) -> T {
    s.rules.own.store(true, Ordering::Relaxed);
    let r = f();
    s.rules.own.store(false, Ordering::Relaxed);
    r
}

fn hook_or_warn(name: &str, r: Result<(), Error>) {
    if let Err(e) = r {
        log::warn!("co-op rules: {name}: {e}; that rule is off");
    }
}

/// Register the session rules' hooks.
pub(crate) fn register(ctx: &ModContext, shared: &Arc<Shared>) {
    let c = Arc::clone(shared);
    hook_or_warn(
        names::HOOK_PRESS_PAUSE,
        ctx.hook_before(names::HOOK_PRESS_PAUSE, move |call| pause_before(call, &c)),
    );
    let c = Arc::clone(shared);
    hook_or_warn(
        names::HOOK_PRESS_UNPAUSE,
        ctx.hook_before(names::HOOK_PRESS_UNPAUSE, move |call| {
            unpause_before(call, &c);
        }),
    );
    let c = Arc::clone(shared);
    hook_or_warn(
        names::GOTO_UPGRADE_DEFERRED,
        ctx.hook_after(names::GOTO_UPGRADE_DEFERRED, move |_| {
            if c.rules.active() {
                let mut g = c.lock();
                g.rules.upgrade_opened();
                let outs = g.rules.take_outs();
                net::apply(&g.net, outs);
            }
        }),
    );
    let c = Arc::clone(shared);
    hook_or_warn(
        names::LEAVE_UPGRADE_DEFERRED,
        ctx.hook_before(names::LEAVE_UPGRADE_DEFERRED, move |call| {
            if !c.rules.active() {
                return;
            }
            let hold = {
                let mut g = c.lock();
                let hold = g.rules.upgrade_leaving(Instant::now());
                let outs = g.rules.take_outs();
                net::apply(&g.net, outs);
                hold
            };
            if hold {
                call.skip_original();
            }
        }),
    );
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    hook_or_warn(
        names::REFRESH_LOADOUT,
        ctx.hook(
            names::REFRESH_LOADOUT,
            move |call| {
                c1.rules.downed.refresh_enter();
                c1.rules.downed.rebuild_enter(call);
            },
            move |call, ()| {
                c2.rules.downed.refresh_exit();
                c2.rules.downed.rebuild_exit(call);
            },
        ),
    );
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    hook_or_warn(
        names::REFRESH_DEFENSES,
        ctx.hook(
            names::REFRESH_DEFENSES,
            move |call| c1.rules.downed.rebuild_enter(call),
            move |call, ()| c2.rules.downed.rebuild_exit(call),
        ),
    );
    for name in ["instance_create_depth", "instance_create_layer"] {
        let c = Arc::clone(shared);
        hook_or_warn(
            name,
            ctx.hook_builtin(
                name,
                |_| {},
                move |call, ()| c.rules.downed.after_create(call),
            ),
        );
    }
    let c = Arc::clone(shared);
    hook_or_warn(
        names::CHECK_DESTROY_PLAYER,
        ctx.hook_before(names::CHECK_DESTROY_PLAYER, move |call| {
            death_before(call, &c);
        }),
    );
}

/// Register the shared score's hook (`updateScore`, skipped on the joining
/// game).
pub(crate) fn register_score(ctx: &ModContext, shared: &Arc<Shared>) {
    let c = Arc::clone(shared);
    hook_or_warn(
        names::UPDATE_SCORE,
        ctx.hook_before(names::UPDATE_SCORE, move |call| {
            update_score_before(call, &c);
        }),
    );
}

/// Before `updateScore` on the joining game: skipped and made again with 0
/// points and 0 XP, so its `oDrawScore` popup still shows (the host's score).
fn update_score_before(call: &mut Call<'_>, s: &Shared) {
    if s.rules.own() {
        return;
    }
    let mut g = s.lock();
    if !g.rules.skips_own_score() {
        return;
    }
    g.rules.own_score_skipped();
    drop(g);
    call.skip_original();
    info_once(
        "score-skip",
        "co-op rules: this game's own updateScore made with 0 points and 0 XP (the host's score is the run's)",
    );
    let Some(script) = s.rules.update_score else {
        return;
    };
    let mut args = vec![Value::Real(0.0), Value::Real(0.0)];
    args.extend((2..call.argc()).map(|i| call.arg_value(i).unwrap_or(Value::Undefined)));
    if let Err(e) = own_call(s, || call.call_script(script, &args)) {
        warn_once(
            "score-zero",
            &format!("co-op rules: updateScore(0, 0): {e}"),
        );
    }
}

/// The global score numbers and kill counters, when all five read.
fn read_score(g: &Instance<'_>, h: &RulesHooks) -> Option<Score> {
    let [score, verifier, modulus, boss_kills, star_eater_kills] = h.score.map(|v| num(g, v));
    Some(Score {
        score: score?,
        verifier: verifier?,
        modulus: modulus?,
        boss_kills: boss_kills?,
        star_eater_kills: star_eater_kills?,
    })
}

fn write_score(g: &Instance<'_>, h: &RulesHooks, s: Score) {
    let vals = [
        s.score,
        s.verifier,
        s.modulus,
        s.boss_kills,
        s.star_eater_kills,
    ];
    for (v, x) in h.score.iter().zip(vals) {
        set(g, *v, x);
    }
}

/// Before `oDrawEndScore_Create_0` on the joining game: the host's score
/// numbers and kill counters are written (none heard: a score of 0), and
/// `finalBossDefeated` becomes 1 when the host's run killed the Star Eater.
pub(crate) fn end_screen_before(call: &Call<'_>, s: &Shared) {
    let h = &s.rules;
    let Some(g) = call.global() else { return };
    let Some(own) = read_score(&g, h) else {
        warn_once(
            "score-end",
            "co-op rules: the score globals did not read; the end screen keeps this game's own",
        );
        return;
    };
    let (decided, counts) = {
        let l = s.lock();
        (l.rules.end_screen_score(own), l.rules.score_counts())
    };
    let Some((sc, final_boss)) = decided else {
        if h.active() {
            // The host (or a game left alone): its own numbers, logged.
            log::info!(
                "co-op score: end screen reads score {} (verifier {}, modulus {}), boss kills {}, Star Eater kills {}; this game's own",
                own.score,
                own.verifier,
                own.modulus,
                own.boss_kills,
                own.star_eater_kills
            );
        }
        return;
    };
    write_score(&g, h, sc);
    if final_boss && num(&g, h.final_boss).is_some_and(|v| v < 0.5) {
        set(&g, h.final_boss, 1.0);
        log::info!(
            "co-op rules: finalBossDefeated set to 1 (the host's run killed the Star Eater)"
        );
    }
    log::info!(
        "co-op score: end screen reads score {} (verifier {}, modulus {}), boss kills {}, Star Eater kills {}; {}; this game's own score was {}; own updateScore skipped {}, host writes {}",
        sc.score,
        sc.verifier,
        sc.modulus,
        sc.boss_kills,
        sc.star_eater_kills,
        if counts.2.is_some() {
            "the host's last state"
        } else {
            "no state heard: 0"
        },
        own.score,
        counts.0,
        counts.1
    );
}

fn as_flag(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| v.as_f64().map(|x| x > 0.5))
}

fn pause_before(call: &mut Call<'_>, s: &Shared) {
    if s.rules.own() || !s.rules.active() {
        return;
    }
    let focus_lost = call_b(call, s.rules.window_has_focus, &[])
        .as_ref()
        .and_then(as_flag)
        .is_some_and(|f| !f);
    let (d, focus_rule) = {
        let mut g = s.lock();
        let d = g.rules.pause_request(focus_lost);
        // Which rule skipped it: the focus switch, or the frozen upgrade screen.
        (d, focus_lost && !g.rules.settings().pause_on_focus_loss)
    };
    if d == PauseCall::Skip {
        call.skip_original();
        info_once(
            if focus_rule {
                "rules-skip-focus"
            } else {
                "rules-skip-pause"
            },
            if focus_rule {
                "co-op rules: the game lost focus; not pausing (pause_on_focus_loss = false)"
            } else {
                "co-op rules: no pause on the upgrade screen (the world is already frozen)"
            },
        );
    }
}

fn unpause_before(call: &mut Call<'_>, s: &Shared) {
    if s.rules.own() {
        return;
    }
    let d = {
        let mut g = s.lock();
        let c = &mut *g;
        let gate_holding = c.core.coop_run()
            && matches!(
                c.core.gate_phase(),
                GatePhase::Holding | GatePhase::Refused { .. }
            );
        // The link wait holds the game like the gate (`wait`): the last row
        // (Leave Co-op Run) is the way out, not Resume; a joining game
        // whose run is over stands until it leaves.
        let d = if c.core.wait_hold() || c.core.run_over().is_some() {
            UnpauseCall::Block
        } else {
            c.rules.unpause_request(gate_holding, Instant::now())
        };
        let outs = c.rules.take_outs();
        net::apply(&c.net, outs);
        d
    };
    if d == UnpauseCall::Block {
        call.skip_original();
        info_once(
            "rules-block-unpause",
            "co-op rules: resume blocked (the start gate, the link wait, a partner's pause, or a partner choosing)",
        );
    }
}

/// `checkDestroyPlayer` before (as `oPlayer`).
fn death_before(call: &mut Call<'_>, s: &Shared) {
    if !s.rules.active() {
        return;
    }
    let h = &s.rules;
    let Some(powers) = h.powers_obj(call) else {
        return;
    };
    let mut read = None;
    let _ = call.with(&powers, |p, _| {
        if read.is_none() {
            read = Some((
                num(&p, h.invulnerable).unwrap_or(1.0),
                num(&p, s.h.hull).unwrap_or(1.0),
                num(&p, h.hull_reserved).unwrap_or(0.0),
                truthy(&p, h.last_stand).unwrap_or(false),
            ));
        }
    });
    let Some((inv, hull, reserved, last_stand)) = read else {
        return;
    };
    let allow = call
        .global()
        .and_then(|g| truthy(&g, h.allow_death))
        .unwrap_or(true);
    if !(inv <= 0.0 && allow && (hull <= 0.0 || reserved >= 1.0)) {
        return;
    }
    if last_stand {
        info_once(
            "rules-last-stand",
            "co-op rules: Last Stand revives the ship (not a down)",
        );
        return;
    }
    let d = {
        let mut g = s.lock();
        let d = g.rules.death(Instant::now());
        let outs = g.rules.take_outs();
        net::apply(&g.net, outs);
        d
    };
    if d == Death::Down {
        call.skip_original();
        // Hold the recipe from this very frame on.
        let _ = call.with(&powers, |p, _| hold_powers(&p, s));
        if let Some(me) = call.instance() {
            hold_player(&me, s);
        }
        log::info!(
            "co-op rules: ship down (hidden, invulnerable, no control; hull {hull:.1}, hullReserved {reserved})"
        );
    }
}

fn set(inst: &Instance<'_>, v: Option<Variable>, x: f64) {
    if let Some(v) = v
        && !inst.set_f64(v, x)
    {
        warn_once(
            "rules-set",
            "co-op rules: a ship variable is not a number; the downed state may show",
        );
    }
}

/// Downed, on `oPowers` (held every frame).
fn hold_powers(p: &Instance<'_>, s: &Shared) {
    let h = &s.rules;
    set(p, h.invulnerable, 100_000.0);
    set(p, h.control, 0.0);
    set(p, s.h.cloaked, 1.0);
    set(p, h.can_fire, 0.0);
}

/// Downed, on `oPlayer`. The hidden ship is also held still, so the camera
/// stays where it went down instead of drifting on with its last velocity.
fn hold_player(me: &Instance<'_>, s: &Shared) {
    set(me, s.rules.iframes, 100_000.0);
    let _ = me.set_builtin_f64("visible", 0.0);
    let _ = me.set_builtin_f64("hspeed", 0.0);
    let _ = me.set_builtin_f64("vspeed", 0.0);
}

/// From `oPowers_Step_0` after (as `oPowers`).
pub(crate) fn powers_step(call: &Call<'_>, me: &Instance<'_>, s: &Shared) {
    let h = &s.rules;
    let control = truthy(me, h.control);
    let act = {
        let mut g = s.lock();
        g.rules.set_control(control);
        g.rules.powers_action()
    };
    match act {
        PowersAct::None => {}
        PowersAct::HoldDown => hold_powers(me, s),
        PowersAct::Revive => {
            // Not while frozen; what the down took comes back first and the
            // revive goes on next frame, so `refreshLoadout` finds it active.
            if !h.downed.ship_active(call) || h.downed.back(call) {
                hold_powers(me, s);
                return;
            }
            let hull_max = num(me, s.h.hull_max).unwrap_or(0.0);
            let shield_max = num(me, h.shield_max).unwrap_or(0.0);
            set(me, s.h.hull, hull_max);
            set(me, s.h.shield, shield_max);
            set(me, h.invulnerable, 74.0);
            set(me, h.control, 1.0);
            set(me, s.h.cloaked, 0.0);
            set(me, h.can_fire, 1.0);
            let before_refresh = num(me, s.h.hull);
            // checkDestroyPlayer's Last Stand revive calls it inside `with (oPowers)`.
            if let Some(refresh) = h.refresh_loadout
                && let Err(e) = me.call_script(refresh, &[])
            {
                warn_once(
                    "rules-refresh",
                    &format!("co-op rules: refreshLoadout: {e}"),
                );
            }
            let after_refresh = num(me, s.h.hull);
            // Full hull and shield once more after the loadout refresh.
            set(me, s.h.hull, hull_max);
            set(me, s.h.shield, shield_max);
            h.downed.rebuilt(call);
            s.lock().rules.powers_applied();
            log::info!(
                "co-op rules: revived (hull {hull_max}, shield {shield_max}; hull read back {before_refresh:?} before refreshLoadout, {after_refresh:?} after, {:?} at the end)",
                num(me, s.h.hull),
            );
        }
        PowersAct::ForceDeath => {
            set(me, h.invulnerable, 0.0);
            set(me, s.h.hull, 0.0);
            s.lock().rules.powers_applied();
            log::info!("co-op rules: everyone is down; the game's own death follows");
        }
    }
}

/// From `oPlayer_Step_0` (as `oPlayer`), before the ship is read.
pub(crate) fn player_step(me: &Instance<'_>, s: &Shared) {
    let act = s.lock().rules.player_action();
    match act {
        PlayerAct::None => {}
        PlayerAct::HoldDown => hold_player(me, s),
        PlayerAct::Revive(pos) => {
            set(me, s.rules.iframes, 180.0);
            let _ = me.set_builtin_f64("visible", 1.0);
            if let Some((x, y)) = pos {
                let _ = me.set_builtin_f64("x", x);
                let _ = me.set_builtin_f64("y", y);
                let _ = me.set_builtin_f64("hspeed", 0.0);
                let _ = me.set_builtin_f64("vspeed", 0.0);
                log::info!(
                    "co-op rules: ship back at ({x:.0}, {y:.0}), next to the partner; iFrames 180"
                );
            } else {
                log::info!(
                    "co-op rules: ship back where it went down (no partner pose); iFrames 180"
                );
            }
            s.lock().rules.player_applied();
        }
    }
}

/// What the run tick read about the run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tick {
    pub key: (usize, Option<f64>),
    pub paused: bool,
    pub gameplay: bool,
    pub game_over: bool,
}

/// The run tick's part (as `oController`, after the start gate's hold).
#[allow(clippy::too_many_lines)] // one tick, read -> decide -> act, kept in order
pub(crate) fn run_tick(call: &Call<'_>, s: &Shared, tick: Tick, now: Instant) {
    let hooks = &s.rules;
    let Some(me) = call.instance() else { return };
    let globals = call.global();
    let levelup = globals.as_ref().and_then(|g| num(g, hooks.levelup));
    let xp = match (
        num(&me, hooks.xp),
        num(&me, hooks.max_exp),
        globals.as_ref().and_then(|g| num(g, hooks.base_level)),
    ) {
        (Some(xp), Some(maxexp), Some(base)) => Some(XpRead { xp, maxexp, base }),
        _ => None,
    };
    let score = globals.as_ref().and_then(|g| read_score(g, hooks));
    let (actions, poll_escape, poll_pause, down) = {
        let mut guard = s.lock();
        let c = &mut *guard;
        let ctx = RunCtx {
            key: tick.key,
            coop_run: c.core.coop_run(),
            linked: c.core.linked(),
            gate_running: c.core.gate_phase() == GatePhase::Running,
            me: c.core.me(),
            is_host: c.core.is_host(),
            partners: c.core.partners().keys().copied().collect(),
        };
        c.rules.sync_run(&ctx);
        let partner = c.core.partners().values().next().and_then(|p| {
            p.ship.latest().map(|ps| {
                (
                    f64::from(ps.x),
                    f64::from(ps.y),
                    f64::from(ps.vx) / 60.0,
                    f64::from(ps.vy) / 60.0,
                )
            })
        });
        let obs = Observed {
            paused: tick.paused,
            gameplay: tick.gameplay,
            game_over: tick.game_over,
            levelup,
            xp,
            score,
            partner,
        };
        let actions = c.rules.frame(&obs, now);
        let _ = c.rules.take_score_sent();
        let outs = c.rules.take_outs();
        net::apply(&c.net, outs);
        let facts = c.rules.facts(now, tick.paused);
        c.run_msgs.set_session(facts);
        for e in c.rules.take_events() {
            c.run_msgs.session_event(e);
        }
        let down = c.rules.is_down();
        c.core.set_downed(down);
        hooks.active.store(c.rules.active(), Ordering::Relaxed);
        (
            actions,
            c.rules.held_escape_offered(now),
            c.rules.held_on_card(),
            down,
        )
    };
    if down != hooks.followers_hidden.load(Ordering::Relaxed) {
        // A down applied in a frozen frame (the death and a level-up's
        // freeze together) would find no ship (`oPlayer` is deactivated)
        // and never hide its followers: it waits for the ship.
        if !down || hooks.downed.ship_reachable(call) {
            hooks.followers_hidden.store(down, Ordering::Relaxed);
            if down {
                hooks.downed.down(call);
            }
        }
    } else if down {
        hooks.downed.hold(call);
    }
    if poll_escape && back_pressed(call, s) {
        let mut g = s.lock();
        g.rules.escape(now);
        let outs = g.rules.take_outs();
        net::apply(&g.net, outs);
    }
    if poll_pause && pause_pressed(call, s) {
        s.lock().rules.pause_key_on_card();
    }
    for a in actions {
        execute(call, &me, s, a);
    }
}

fn execute(call: &Call<'_>, me: &Instance<'_>, s: &Shared, a: Action) {
    let h = &s.rules;
    let script = match a {
        Action::SetXp(v) => {
            set(me, h.xp, v);
            return;
        }
        Action::SetScore(sc) => {
            if let Some(g) = call.global() {
                write_score(&g, h, sc);
            }
            return;
        }
        Action::PressPause => s.h.press_pause,
        Action::PressUnpause => s.h.press_unpause,
        Action::GotoUpgrade => h.goto_upgrade,
        Action::ReleaseUpgrade => h.leave_upgrade,
    };
    let Some(script) = script else {
        warn_once(
            "rules-script",
            &format!("co-op rules: the script for {a:?} is missing; that rule can't act"),
        );
        return;
    };
    log::debug!("co-op rules: {a:?}");
    if let Err(e) = own_call(s, || call.call_script(script, &[])) {
        warn_once("rules-call", &format!("co-op rules: {a:?}: {e}"));
    }
}

/// The pause key or pad button this frame, read as the game's Step reads
/// them; polled on the held upgrade card, where `pressPause` also comes
/// from focus loss.
fn pause_pressed(call: &Call<'_>, s: &Shared) -> bool {
    let k = &s.ui.keys;
    let global = |v: Option<Variable>| call.global().and_then(|g| g.get_value(v?));
    let key = match (s.ui.input_pressed, global(k.pause)) {
        (Some(sc), Some(code)) => call
            .call_script(sc, &[code])
            .ok()
            .as_ref()
            .and_then(as_flag)
            .unwrap_or(false),
        _ => false,
    };
    let pad = match (s.ui.pad_pressed, global(k.player1), global(k.pad_pause)) {
        (Some(sc), Some(dev), Some(btn)) => call
            .call_script(sc, &[dev, btn])
            .ok()
            .as_ref()
            .and_then(as_flag)
            .unwrap_or(false),
        _ => false,
    };
    key || pad
}

/// The back key this frame: Esc, the game's back binding, or the gamepad's
/// back button (as the pause menu reads them).
fn back_pressed(call: &Call<'_>, s: &Shared) -> bool {
    let k = &s.ui.keys;
    let global = |v: Option<Variable>| call.global().and_then(|g| g.get_value(v?));
    let esc = call_b(call, s.rules.kb_pressed, &[27.0.into()])
        .as_ref()
        .and_then(as_flag)
        .unwrap_or(false);
    let key = match (s.ui.input_pressed, global(k.back)) {
        (Some(sc), Some(code)) => call
            .call_script(sc, &[code])
            .ok()
            .as_ref()
            .and_then(as_flag)
            .unwrap_or(false),
        _ => false,
    };
    let pad = match (s.ui.pad_pressed, global(k.player1), global(k.pad_back)) {
        (Some(sc), Some(dev), Some(btn)) => call
            .call_script(sc, &[dev, btn])
            .ok()
            .as_ref()
            .and_then(as_flag)
            .unwrap_or(false),
        _ => false,
    };
    esc || key || pad
}
