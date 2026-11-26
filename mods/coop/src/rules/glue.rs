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
//! them through; hot-path hooks (`checkDestroyPlayer`, `pressPause`,
//! `oPowers`' Begin Step and Step) return after one atomic load when off.
//!
//! On the held card (`Rules::frozen_unseen`) `oController.pause` reads 1
//! only around `oPowers_Step_1` and `oPowers_Step_0`, which freezes `oPowers`
//! without running or drawing the pause menu.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ndml_sdk::{Builtin, Call, Error, Instance, ModContext, Script, Value, Variable};

use super::banish::{RunLatch, SESSION_BANISHED};
use super::machine::{
    Action, Death, Observed, PauseCall, PlayerAct, PowersAct, RunCtx, UnpauseCall,
};
use super::score::Score;
use super::xp::XpRead;
use crate::game::{self, call_b, info_once, num, truthy, warn_once};
use crate::gate::GatePhase;
use crate::names::{self, rules_fn, rules_obj, rules_var as rv};
use crate::ui::run::{CountKind, SessionEvent};
use crate::{Shared, net};

/// Everything the rules resolve at init, plus the hot-path flags.
#[derive(Debug)]
pub struct RulesHooks {
    /// A pause/unpause/upgrade call the mod makes itself is in flight.
    own: AtomicBool,
    /// The rules are on for this run.
    active: AtomicBool,
    /// The world is held frozen where the game cannot tell (the card after
    /// choosing): `oPowers`' steps see `oController.pause = 1`.
    frozen_unseen: AtomicBool,
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
    attraction: Option<Variable>,
    shield_max: Option<Variable>,
    hull_reserved: Option<Variable>,
    last_stand: Option<Variable>,
    allow_death: Option<Variable>,
    goto_upgrade: Option<Script>,
    leave_upgrade: Option<Script>,
    refresh_loadout: Option<Script>,
    update_score: Option<Script>,
    release_shoot: Option<Script>,
    banish_mod: Option<Script>,
    /// `oPowers.banishMods` and `ds_list_find_index`: the banish read back.
    banish_list: Option<Variable>,
    list_find: Option<Builtin>,
    /// The session's banished upgrades applied for this run ([`banish`]).
    banished: Mutex<RunLatch<(usize, Option<f64>)>>,
    window_has_focus: Option<Builtin>,
    asset_get_index: Option<Builtin>,
    kb_pressed: Option<Builtin>,
    /// The respawn's fly-in: `event_perform` and the kaleidoscope setting it
    /// holds off for the call ([`fly_in`]).
    event_perform: Option<Builtin>,
    no_kaleidoscope: Option<Variable>,
    /// The respawn's protected frames, watched from the fly-in's start
    /// (`protect`).
    respawn_watch: Mutex<Option<super::protect::Watch>>,
    /// `oPowers` looked up yet, and what was found.
    powers: Mutex<(bool, Option<Value>)>,
    /// `oController`, the same way.
    controller: Mutex<(bool, Option<Value>)>,
    /// What a down does to what follows the ship (`downed`).
    pub(crate) downed: super::downed::Downed,
    /// What follows the ship is gone (this game's ship is down; `downed`).
    followers_hidden: AtomicBool,
    firing: Option<Variable>,
    ataraxia: Option<Variable>,
    alpha: Option<Variable>,
    alpha_max: Option<Variable>,
    /// Steps left of a fade-in ([`FADE_STEPS`]; 0: none).
    fade: AtomicU32,
    /// The upgrade screen's pieces ([`names::UPGRADE_PIECES`]), looked up
    /// once.
    pieces: Mutex<Option<Vec<Value>>>,
}

/// Milliseconds since the Unix epoch (for the count's log lines).
pub(crate) fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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
            frozen_unseen: AtomicBool::new(false),
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
            attraction: v(rv::POWERUP_ATTRACTION),
            shield_max: v(rv::SHIELD_MAX),
            hull_reserved: v(rv::HULL_RESERVED),
            last_stand: v(rv::LAST_STAND),
            allow_death: v(rv::ALLOW_PLAYER_DEATH),
            goto_upgrade: sc(names::GOTO_UPGRADE),
            leave_upgrade: sc(names::LEAVE_UPGRADE),
            refresh_loadout: sc(names::REFRESH_LOADOUT),
            update_score: sc(names::UPDATE_SCORE),
            release_shoot: sc(names::RELEASE_SHOOT),
            banish_mod: sc(names::BANISH_MOD),
            banish_list: v(rv::BANISH_MODS),
            list_find: b(rules_fn::DS_LIST_FIND_INDEX),
            banished: Mutex::new(RunLatch::default()),
            window_has_focus: b(rules_fn::WINDOW_HAS_FOCUS),
            asset_get_index: b(rules_fn::ASSET_GET_INDEX),
            kb_pressed: b(rules_fn::KEYBOARD_CHECK_PRESSED),
            event_perform: b(rules_fn::EVENT_PERFORM),
            no_kaleidoscope: v(rv::DISABLE_KALEIDOSCOPE),
            powers: Mutex::new((false, None)),
            controller: Mutex::new((false, None)),
            downed: super::downed::Downed::resolve(ctx),
            followers_hidden: AtomicBool::new(false),
            respawn_watch: Mutex::new(None),
            firing: v(rv::FIRING),
            ataraxia: v(rv::ATARAXIA),
            alpha: v(rv::ALPHA),
            alpha_max: v(rv::ALPHA_MAX),
            fade: AtomicU32::new(0),
            pieces: Mutex::new(None),
        }
    }

    /// The run ended (menu): the hot-path hooks go quiet, and a down of that
    /// run is over.
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.frozen_unseen.store(false, Ordering::Relaxed);
        self.fade.store(0, Ordering::Relaxed);
        self.downed.end_run();
        self.banished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reset();
    }

    fn own(&self) -> bool {
        self.own.load(Ordering::Relaxed)
    }

    fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// `oPowers` as an object ref (`asset_get_index`, cached).
    fn powers_obj(&self, call: &Call<'_>) -> Option<Value> {
        self.object_ref(
            call,
            &self.powers,
            rules_obj::POWERS,
            "co-op rules: oPowers not found; the downed state is off",
        )
    }

    /// `oController` as an object ref (cached).
    fn controller_obj(&self, call: &Call<'_>) -> Option<Value> {
        self.object_ref(
            call,
            &self.controller,
            rules_obj::CONTROLLER,
            "co-op rules: oController not found; the held card does not freeze oPowers",
        )
    }

    fn object_ref(
        &self,
        call: &Call<'_>,
        slot: &Mutex<(bool, Option<Value>)>,
        name: &'static str,
        missing: &str,
    ) -> Option<Value> {
        let mut cache = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.0 {
            return cache.1.clone();
        }
        let found = call_b(call, self.asset_get_index, &[name.into()]).filter(|v| match v {
            Value::Ref(_) => true,
            other => other.as_f64().is_some_and(|x| x >= 0.0),
        });
        if found.is_none() {
            warn_once(name, missing);
        }
        *cache = (true, found.clone());
        found
    }

    /// The rules' "held where the game cannot tell", for the `oPowers`
    /// steps (logged as it starts).
    fn set_frozen_unseen(&self, on: bool) {
        if !on {
            self.frozen_unseen.store(false, Ordering::Relaxed);
        } else if !self.frozen_unseen.swap(true, Ordering::Relaxed) {
            log::info!(
                "co-op rules: held on the card: oPowers' steps see the game's pause (its timers stand)"
            );
        }
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
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    hook_or_warn(
        names::GOTO_UPGRADE,
        ctx.hook(
            names::GOTO_UPGRADE,
            move |call| {
                note_start_ask(&c1);
                lift_before(call, &c1, "gotoUpgrade")
            },
            move |call, lifted| lift_after(call, &c2, lifted),
        ),
    );
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    hook_or_warn(
        names::GOTO_UPGRADE_DEFERRED,
        ctx.hook(
            names::GOTO_UPGRADE_DEFERRED,
            move |call| lift_before(call, &c1, "gotoUpgradeDeferred"),
            move |call, lifted| {
                lift_after(call, &c2, lifted);
                upgrade_opened_after(call, &c2);
            },
        ),
    );
    register_fade(ctx, shared);
    let c = Arc::clone(shared);
    hook_or_warn(
        names::LEAVE_UPGRADE_DEFERRED,
        ctx.hook_before(names::LEAVE_UPGRADE_DEFERRED, move |call| {
            leave_before(call, &c);
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
    register_pull(ctx, shared);
    register_salvage(ctx, shared);
    // The ending's cutscene uses the ship: a downed ship's makes are not
    // ended from here on.
    let c = Arc::clone(shared);
    hook_or_warn(
        names::BEGIN_GAME_ENDING,
        ctx.hook_before(names::BEGIN_GAME_ENDING, move |call| {
            c.rules.downed.release(call, "the ending begins");
        }),
    );
    let c = Arc::clone(shared);
    hook_or_warn(
        names::CHECK_DESTROY_PLAYER,
        ctx.hook_before(names::CHECK_DESTROY_PLAYER, move |call| {
            death_before(call, &c);
        }),
    );
}

/// While this ship is down and hidden, a construct's death drops no armour
/// fragment (`Downed::salvage_off`).
fn register_salvage(ctx: &ModContext, shared: &Arc<Shared>) {
    let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
    hook_or_warn(
        names::DESTROY_CONSTRUCT,
        ctx.hook(
            names::DESTROY_CONSTRUCT,
            move |call| c1.rules.downed.salvage_off(call),
            move |call, out| c2.rules.downed.salvage_back(call, &out),
        ),
    );
}

/// While this ship is down and hidden, no pickup is pulled to it.
fn register_pull(ctx: &ModContext, shared: &Arc<Shared>) {
    for name in [names::POWERUP_ATTRACT, names::POWERUP_ATTRACT_PERM] {
        let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
        hook_or_warn(
            name,
            ctx.hook(
                name,
                move |call| pull_off(call, &c1),
                move |call, was| pull_back(call, &c2, was),
            ),
        );
    }
}

/// Before a pickup's pull, while this ship is down and hidden: Magnetism
/// (`oPowers.powerupAttraction`) reads 0, so the pickup takes the game's
/// out-of-range branch. Returns the value to put back after the call.
fn pull_off(call: &Call<'_>, c: &Shared) -> Option<f64> {
    let h = &c.rules;
    if !h.downed.hidden() {
        return None;
    }
    let (Some(var), Some(powers)) = (h.attraction, h.powers_obj(call)) else {
        return None;
    };
    let mut was = None;
    let _ = call.with(&powers, |p, _| {
        if let Some(a) = p.get_f64(var)
            && p.set_f64(var, 0.0)
        {
            was = Some(a);
        }
    });
    was
}

/// A number of `obj`'s first instance (for a log line).
fn num_of(call: &Call<'_>, obj: &Value, v: Option<Variable>) -> Option<f64> {
    let mut out = None;
    let _ = call.with(obj, |p, _| {
        if out.is_none() {
            out = num(&p, v);
        }
    });
    out
}

/// After a pickup's pull: the Magnetism value back as it was.
fn pull_back(call: &Call<'_>, c: &Shared, was: Option<f64>) {
    let (Some(a), Some(var)) = (was, c.rules.attraction) else {
        return;
    };
    let Some(powers) = c.rules.powers_obj(call) else {
        return;
    };
    let _ = call.with(&powers, |p, _| {
        if !p.set_f64(var, a) {
            warn_once(
                "rules-pull-back",
                "co-op rules: oPowers.powerupAttraction could not be put back after a pickup's pull",
            );
        }
    });
}

/// Before `leaveUpgradeDeferred`: hold the card, the original skipped, while
/// the partner chooses.
fn leave_before(call: &mut Call<'_>, c: &Shared) {
    if !c.rules.active() {
        return;
    }
    let hold = {
        let mut g = c.lock();
        let hold = g.rules.upgrade_leaving(Instant::now());
        let outs = g.rules.take_outs();
        net::apply(&g.net, outs);
        c.rules.set_frozen_unseen(g.rules.frozen_unseen());
        hold
    };
    if hold {
        call.skip_original();
    }
}

/// After `gotoUpgradeDeferred`: a screen opened unless the result is false.
/// A round the partner opened starts the fade-in.
fn upgrade_opened_after(call: &Call<'_>, s: &Shared) {
    if !s.rules.active() {
        return;
    }
    if call.result().as_ref().and_then(as_flag) == Some(false) {
        info_once(
            "rules-goto-refused",
            "co-op rules: gotoUpgradeDeferred opened no screen (the game's guards)",
        );
        return;
    }
    let forced = {
        let mut g = s.lock();
        let forced = g.rules.upgrade_opened();
        let outs = g.rules.take_outs();
        net::apply(&g.net, outs);
        s.rules.set_frozen_unseen(g.rules.frozen_unseen());
        forced
    };
    if forced {
        s.rules.fade.store(FADE_STEPS, Ordering::Relaxed);
        log::info!("co-op rules: the partner's round opened this screen: it fades in");
    } else {
        // The player's own screen (or a chained one): the game's own timing.
        s.rules.fade.store(0, Ordering::Relaxed);
    }
}

/// Before `gotoUpgrade` and `gotoUpgradeDeferred`: `playerControl` is 1 for
/// the call when `Rules::lifts_control` (the game's guards refuse at 0).
/// Returns the value to put back, if it was lifted.
fn lift_before(call: &Call<'_>, s: &Shared, what: &str) -> Option<f64> {
    if !s.rules.active() || !s.lock().rules.lifts_control() {
        return None;
    }
    let (powers, var) = (s.rules.powers_obj(call)?, s.rules.control?);
    let mut old = None;
    let mut firing = None;
    let _ = call.with(&powers, |p, _| {
        if old.is_none()
            && let Some(back) = p.get_f64(var).and_then(control_to_restore)
            && p.set_f64(var, 1.0)
        {
            old = Some(back);
            firing = truthy(&p, s.rules.firing);
        }
    });
    let old = old?;
    log::info!(
        "co-op rules: down: playerControl lifted for {what} (the partner's round; firing {firing:?})"
    );
    Some(old)
}

/// The `playerControl` value to put back after a lift. `None` when it was 1.
fn control_to_restore(found: f64) -> Option<f64> {
    ((found - 1.0).abs() > f64::EPSILON).then_some(found)
}

/// After those calls: control as it was found.
fn lift_after(call: &Call<'_>, s: &Shared, saved: Option<f64>) {
    let (Some(old), Some(powers)) = (saved, s.rules.powers_obj(call)) else {
        return;
    };
    let _ = call.with(&powers, |p, _| set(&p, s.rules.control, old));
}

/// Steps a screen the partner opened takes to fade in (the game's own is
/// 20). Picks, Esc, rerolls and banishes wait for them.
pub const FADE_STEPS: u32 = 60;

/// The held card's freeze of `oPowers` (`freeze_before`), the fade-in and
/// its locks.
fn register_fade(ctx: &ModContext, shared: &Arc<Shared>) {
    for name in [names::POWERS_BEGIN_STEP, names::POWERS_STEP] {
        let (c1, c2) = (Arc::clone(shared), Arc::clone(shared));
        hook_or_warn(
            name,
            ctx.hook(
                name,
                move |call| freeze_before(call, &c1),
                move |call, saved| freeze_after(call, &c2, saved),
            ),
        );
    }
    let c = Arc::clone(shared);
    hook_or_warn(
        names::UPGRADE_STEP,
        ctx.hook_before(names::UPGRADE_STEP, move |call| fade_step(call, &c)),
    );
    for name in [
        names::CONFIRM_UPGRADE,
        names::DECLINE_UPGRADE,
        names::REROLL_UPGRADE,
        names::BANISH_UPGRADE,
    ] {
        let c = Arc::clone(shared);
        hook_or_warn(
            name,
            ctx.hook_before(name, move |call| {
                let left = c.rules.fade.load(Ordering::Relaxed);
                if left > 0 {
                    call.skip_original();
                    log::info!("co-op rules: {name} waits for the fade-in ({left} steps left)");
                }
            }),
        );
    }
}

/// Before `oUpgrade_Step_0` (as `oUpgrade`) while a fade runs: the screen
/// and its pieces no brighter than the fade's share so far. The game's step
/// then adds 0.05 to `alpha`.
fn fade_step(call: &Call<'_>, s: &Shared) {
    let h = &s.rules;
    let left = h.fade.load(Ordering::Relaxed);
    if left == 0 {
        return;
    }
    h.fade.store(left - 1, Ordering::Relaxed);
    let share = f64::from(FADE_STEPS - left + 1) / f64::from(FADE_STEPS);
    if let Some(me) = call.instance()
        && num(&me, h.alpha).is_some_and(|a| a > share - 0.05)
    {
        set(&me, h.alpha, (share - 0.05).max(0.0));
    }
    let pieces = {
        let mut cache = h.pieces.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get_or_insert_with(|| {
                names::UPGRADE_PIECES
                    .iter()
                    .filter_map(|n| {
                        call_b(call, h.asset_get_index, &[(*n).into()])
                            .filter(|v| v.as_f64().is_none_or(|x| x >= 0.0))
                    })
                    .collect()
            })
            .clone()
    };
    for p in &pieces {
        let _ = call.with(p, |i, _| {
            let cap = share * num(&i, h.alpha_max).unwrap_or(1.0);
            if i.builtin_f64("image_alpha").is_some_and(|a| a > cap) {
                let _ = i.set_builtin_f64("image_alpha", cap);
            }
        });
    }
    if left == 1 {
        log::info!("co-op rules: the fade-in is over ({FADE_STEPS} steps)");
    }
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

/// Before `oPowers_Step_1` and `oPowers_Step_0` (as `oPowers`): on the held
/// card, `oController.pause = 1` for the event. Returns the value to put back.
fn freeze_before(call: &Call<'_>, s: &Shared) -> Option<f64> {
    if !s.rules.frozen_unseen.load(Ordering::Relaxed) {
        return None;
    }
    let (ctl, var) = (s.rules.controller_obj(call)?, s.h.pause?);
    let mut old = None;
    let _ = call.with(&ctl, |c, _| {
        if old.is_none()
            && let Some(p) = c.get_f64(var)
            && (p - 1.0).abs() > f64::EPSILON
            && c.set_f64(var, 1.0)
        {
            old = Some(p);
        }
    });
    old
}

/// After those events: `oController.pause` as it was read.
fn freeze_after(call: &Call<'_>, s: &Shared, saved: Option<f64>) {
    let (Some(p), Some(ctl), Some(var)) = (saved, s.rules.controller_obj(call), s.h.pause) else {
        return;
    };
    let _ = call.with(&ctl, |c, _| {
        let _ = c.set_f64(var, p);
    });
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
        // The link wait and a run that is over hold the game like the gate.
        let d = if c.core.wait_hold() || c.core.run_over().is_some() {
            UnpauseCall::Block
        } else if c.rules.counting() {
            // Closing the pause screen while the resume count runs: "hold
            // on", and the screen stays. The count resumes the game.
            c.rules.count_hold_on(Instant::now());
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
    if d != Death::Down {
        // The game's own death runs now: what it makes with the ship as
        // `self` is never ended as a downed ship's.
        h.downed.release(call, "the game's own death runs");
    }
    if d == Death::Down {
        call.skip_original();
        if let Some(me) = call.instance() {
            release_trigger(&me, s);
        }
        // Hold the recipe from this very frame on.
        let _ = call.with(&powers, |p, _| hold_powers(&p, s));
        if let Some(me) = call.instance() {
            hold_player(&me, s);
        }
        let magnetism = num_of(call, &powers, h.attraction);
        log::info!(
            "co-op rules: ship down (hidden, invulnerable, no control; hull {hull:.1}, hullReserved {reserved}; Magnetism {magnetism:?}, read as 0 by the pickups' pull while down)"
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

/// At the down, on `oPlayer`: let go of the trigger with `releaseShoot`, as
/// the game does when it takes control away. A charged weapon releases its
/// charge once.
pub(super) fn release_trigger(me: &Instance<'_>, s: &Shared) {
    let Some(script) = s.rules.release_shoot else {
        warn_once(
            "rules-release",
            "co-op rules: releaseShoot not found; a charge held at a down goes on",
        );
        return;
    };
    if let Err(e) = me.call_script(script, &[Value::Undefined]) {
        warn_once(
            "rules-release-call",
            &format!("co-op rules: releaseShoot at the down: {e}"),
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
            // The respawn's protection (`protect`) from here, so the frame
            // before the fly-in is covered too.
            set(me, h.invulnerable, f64::from(super::protect::FRAMES));
            set(me, h.control, 1.0);
            set(me, s.h.cloaked, 0.0);
            set(me, h.can_fire, 1.0);
            let before_refresh = num(me, s.h.hull);
            // The value the down left, read before the refresh recomputes it.
            let magnetism = num(me, h.attraction);
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
                "co-op rules: revived (hull {hull_max}, shield {shield_max}; hull read back {before_refresh:?} before refreshLoadout, {after_refresh:?} after, {:?} at the end; Magnetism {magnetism:?} before refreshLoadout, {:?} after)",
                num(me, s.h.hull),
                num(me, h.attraction),
            );
        }
        PowersAct::ForceDeath => {
            *h.respawn_watch
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
            h.downed
                .release(call, "everyone is down; the game's own death follows");
            set(me, h.invulnerable, 0.0);
            set(me, s.h.hull, 0.0);
            s.lock().rules.powers_applied();
            log::info!(
                "co-op rules: everyone is down; the game's own death follows (Magnetism {:?})",
                num(me, h.attraction)
            );
        }
    }
}

/// From `oPlayer_Step_0` (as `oPlayer`), before the ship is read.
pub(crate) fn player_step(call: &Call<'_>, me: &Instance<'_>, s: &Shared) {
    let act = s.lock().rules.player_action();
    match act {
        PlayerAct::None => watch_protection(call, s),
        PlayerAct::HoldDown => {
            *s.rules
                .respawn_watch
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
            hold_player(me, s);
        }
        PlayerAct::Revive => {
            set(me, s.rules.iframes, f64::from(super::protect::FRAMES));
            let _ = me.set_builtin_f64("visible", 1.0);
            fly_in(call, me, s);
            protect_from_now(call, s);
            s.lock().rules.player_applied();
            watch_protection(call, s);
        }
    }
}

/// The respawn's protection from the fly-in's start (`protect`):
/// `invulnerableTime` set again, and the watch of what the ship takes in it.
fn protect_from_now(call: &Call<'_>, s: &Shared) {
    let h = &s.rules;
    let frames = f64::from(super::protect::FRAMES);
    let mut read = None;
    if let Some(powers) = h.powers_obj(call) {
        let _ = call.with(&powers, |p, _| {
            if read.is_none() {
                set(&p, h.invulnerable, frames);
                read = Some((num(&p, s.h.hull), num(&p, s.h.shield)));
            }
        });
    }
    let watch = if let Some((Some(hull), Some(shield))) = read {
        Some(super::protect::Watch::start(hull, shield))
    } else {
        warn_once(
            "rules-protect",
            "co-op rules: oPowers not read at the respawn; its protection is the revive's",
        );
        None
    };
    *h.respawn_watch
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = watch;
}

/// One ship step of the respawn's protected frames (`protect`): what the
/// ship took in them, logged at their end.
fn watch_protection(call: &Call<'_>, s: &Shared) {
    let h = &s.rules;
    let mut watch = h
        .respawn_watch
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Some(w) = watch.as_mut() else { return };
    let Some(powers) = h.powers_obj(call) else {
        *watch = None;
        return;
    };
    let mut read = None;
    let _ = call.with(&powers, |p, _| {
        read.get_or_insert((
            num(&p, s.h.hull),
            num(&p, s.h.shield),
            num(&p, h.invulnerable),
        ));
    });
    let Some((Some(hull), Some(shield), Some(invulnerable))) = read else {
        *watch = None;
        return;
    };
    if let Some(r) = w.frame(hull, shield, invulnerable) {
        *watch = None;
        log::info!(
            "co-op rules: respawn protection over: {} frames from the fly-in (invulnerableTime and iFrames {}); {}: hull {:.1}, lowest {:.1}; shield {:.1}, lowest {:.1}; invulnerableTime {:.0} on the first frame, {:.0} on the last",
            r.frames,
            super::protect::FRAMES,
            if r.hurt() { "hurt" } else { "not hurt" },
            r.hull,
            r.lowest_hull,
            r.shield,
            r.lowest_shield,
            r.first_invulnerable,
            r.last_invulnerable,
        );
    }
}

/// The ship flies back in as after the Singularity: the game's own
/// `oPlayer_Alarm_3`, run now as the ship. The kaleidoscope cutscene is held
/// off for the call with the game's own setting.
fn fly_in(call: &Call<'_>, me: &Instance<'_>, s: &Shared) {
    let h = &s.rules;
    let Some(perform) = h.event_perform else {
        warn_once(
            "rules-flyin",
            "co-op rules: event_perform not found; the ship comes back where it went down",
        );
        return;
    };
    let global = call.global();
    // Held off only when it could be read, then put back exactly.
    let kaleidoscope = h
        .no_kaleidoscope
        .zip(global.as_ref())
        .and_then(|(v, g)| g.get_value(v).map(|was| (v, was)))
        .filter(|(v, _)| global.as_ref().is_some_and(|g| g.set(*v, 1.0).is_ok()));
    // `ev_alarm` is event type 2.
    let ran = me.call_builtin(perform, &[2.0.into(), 3.0.into()]);
    if let (Some((v, was)), Some(g)) = (kaleidoscope, global.as_ref())
        && g.set(v, was).is_err()
    {
        warn_once(
            "rules-flyin-setting",
            "co-op rules: the kaleidoscope setting could not be put back after the fly-in",
        );
    }
    if let Err(e) = ran {
        warn_once(
            "rules-flyin-call",
            &format!("co-op rules: the respawn's fly-in (oPlayer_Alarm_3): {e}"),
        );
        return;
    }
    let read = |n: &str| me.builtin_f64(n);
    let wire = {
        let g = s.lock();
        ndml_net::time::to_wire_ms(g.net.local_time(Instant::now()))
    };
    log::info!(
        "co-op rules: respawn fly-in: the game's own (oPlayer_Alarm_3) from ({:.0}, {:.0}), speed {:.1}, direction {:.1}, no kaleidoscope; at wire {wire} ms, wall {} ms",
        read("x").unwrap_or(f64::NAN),
        read("y").unwrap_or(f64::NAN),
        read("speed").unwrap_or(f64::NAN),
        read("direction").unwrap_or(f64::NAN),
        wall_ms()
    );
}

/// What the run tick read about the run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tick {
    pub key: (usize, Option<f64>),
    pub paused: bool,
    pub gameplay: bool,
    pub game_over: bool,
}

/// Before `gotoUpgrade`, ahead of the gate's release: the screen is kept to
/// ask for at the release (`gate::StartAsk`).
fn note_start_ask(s: &Shared) {
    if s.lock().core.note_start_ask() {
        log::info!(
            "co-op: the game asked for an upgrade screen before the start gate let the run go; asked again at the release"
        );
    }
}

/// The upgrade screen asked for before the start gate's release, asked for
/// again with `gotoUpgrade` on the first unpaused frame after it.
pub(crate) fn start_ask_step(call: &Call<'_>, s: &Shared, paused: bool) {
    let due = {
        let mut c = s.lock();
        let open = c.rules.upgrade_choosing();
        let due = c.core.start_ask_due(paused, open);
        if due {
            // Its screen is this player's own, even if the partner's round
            // asked for one this frame too (no fade-in).
            c.rules.start_screen_asked();
        }
        due
    };
    if !due {
        return;
    }
    let Some(script) = s.rules.goto_upgrade else {
        warn_once(
            "start-ask",
            "co-op: unable to find gotoUpgrade; the run's opening screen stays unasked",
        );
        return;
    };
    log::info!(
        "co-op: the start gate let the run go: the upgrade screen asked for before it, asked for again"
    );
    if let Err(e) = call.call_script(script, &[]) {
        warn_once(
            "start-ask-call",
            &format!("co-op: unable to ask for the run's opening upgrade screen ({e})"),
        );
    }
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
    let ataraxia = ataraxia_taken(call, s);
    let mut ready_cue = false;
    let mut ticks: Vec<(CountKind, Option<u8>)> = Vec::new();
    let hidden;
    let poll_count;
    let (actions, poll_pause, down, banish_ready) = {
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
        let banish_ready = ctx.coop_run && ctx.gate_running && tick.gameplay;
        // The partner's new Ready on its new game screen: the same cue,
        // wherever this player is in its run.
        ready_cue |= c.ready_notice.take_cue();
        c.rules.sync_run(&ctx);
        let obs = Observed {
            paused: tick.paused,
            gameplay: tick.gameplay,
            game_over: tick.game_over,
            levelup,
            xp,
            score,
            held: c.core.wait_hold() || c.core.run_over().is_some(),
            ataraxia,
        };
        let rtt = c
            .net
            .lobby_status()
            .as_ref()
            .and_then(|st| st.connected_peers().next().map(|p| p.rtt_ms))
            .filter(|ms| ms.is_finite() && *ms >= 0.0);
        if let Some(ms) = rtt {
            c.rules.set_rtt(Duration::from_secs_f64(ms / 1000.0));
        }
        let actions = c.rules.frame(&obs, now);
        let _ = c.rules.take_score_sent();
        let outs = c.rules.take_outs();
        net::apply(&c.net, outs);
        let facts = c.rules.facts(now, tick.paused);
        c.run_msgs.set_session(facts);
        for e in c.rules.take_events() {
            ready_cue |= e == SessionEvent::PartnerReady;
            match e {
                SessionEvent::CountTick(k, n) => ticks.push((k, Some(n))),
                SessionEvent::CountGo(k) => ticks.push((k, None)),
                SessionEvent::ResumedNoCount => {
                    log::info!(
                        "co-op rules: play resumes with no count at wall {} ms",
                        wall_ms()
                    );
                }
                _ => {}
            }
            c.run_msgs.session_event(e);
        }
        poll_count = c.rules.counting_on_card();
        let down = c.rules.is_down();
        hidden = c.rules.ship_hidden();
        c.core.set_downed(down);
        hooks.active.store(c.rules.active(), Ordering::Relaxed);
        hooks.set_frozen_unseen(c.rules.frozen_unseen());
        (actions, c.rules.held_on_card(), down, banish_ready)
    };
    let banish_due = hooks
        .banished
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .due(tick.key, banish_ready);
    if banish_due {
        banish_for_session(call, s);
    }
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
    if !hidden {
        // Everyone down, or up: from here nothing the ship makes is ended.
        hooks
            .downed
            .release(call, "the ship is no longer down and hidden (the rules)");
    }
    // A count's numbers and its go (a count called off has no go).
    for (k, n) in &ticks {
        let (said, cue) = n.map_or_else(
            || ("go".to_owned(), crate::ui::cues::Cue::Go),
            |n| (n.to_string(), crate::ui::cues::Cue::Tick(n)),
        );
        log::info!(
            "co-op rules: {} count: {said} at wall {} ms",
            k.name(),
            wall_ms()
        );
        crate::ui::glue::play_cue(call, s, cue, now);
    }
    if ready_cue {
        crate::ui::glue::play_cue(call, s, crate::ui::cues::Cue::PartnerReady, now);
    } else if ticks.is_empty() {
        crate::ui::glue::cue_tick(call, s, now);
    }
    if poll_count && (back_pressed(call, s) || pause_pressed(call, s)) {
        // Esc or the pause key on the held card while the resume count
        // runs: "hold on".
        let mut g = s.lock();
        g.rules.count_hold_on(now);
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

/// Whether the build has Ataraxia (`oPowers.ataraxia`).
fn ataraxia_taken(call: &Call<'_>, s: &Shared) -> bool {
    let h = &s.rules;
    let Some(powers) = h.powers_obj(call) else {
        return false;
    };
    let mut taken = false;
    let _ = call.with(&powers, |p, _| {
        taken |= truthy(&p, h.ataraxia).unwrap_or(false);
    });
    taken
}

/// The session's banished upgrades ([`super::banish`]), through the game's
/// own `banishMod` as `oPowers`, once per co-op run past the start gate.
fn banish_for_session(call: &Call<'_>, s: &Shared) {
    let h = &s.rules;
    let (Some(script), Some(powers)) = (h.banish_mod, h.powers_obj(call)) else {
        warn_once(
            "rules-banish",
            "co-op rules: banishMod or oPowers not found; Last Stand stays in the pools",
        );
        return;
    };
    let mut done = Vec::new();
    for name in SESSION_BANISHED {
        let Some(idx) = call_b(call, h.asset_get_index, &[(*name).into()])
            .as_ref()
            .and_then(crate::world::classify::object_index)
        else {
            warn_once(
                "rules-banish-name",
                &format!("co-op rules: {name} not found"),
            );
            continue;
        };
        // An object reference, as the game's own calls pass it.
        let r = Value::Ref(ndml_sdk::abi::OBJECT_REF_TAG | u64::from(idx));
        let mut list = None;
        let _ = call.with(&powers, |p, _| {
            match p.call_script(script, std::slice::from_ref(&r)) {
                Ok(_) => list = h.banish_list.and_then(|v| p.get_value(v)),
                Err(e) => warn_once(
                    "rules-banish-call",
                    &format!("co-op rules: banishMod({name}): {e}"),
                ),
            }
        });
        // Read back from the game's own list (`ds_list_find_index`).
        if let Some(list) = list {
            let at = call_b(call, h.list_find, &[list, r])
                .and_then(|v| v.as_f64())
                .map_or_else(|| "?".to_owned(), |i| format!("{i:.0}"));
            done.push(format!("{name} (banishMods index {at})"));
        }
    }
    log::info!(
        "co-op rules: banished for this co-op run (the game's own banish): {}",
        done.join(", ")
    );
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

/// The pause key or pad button this frame, read as the game's own Step
/// reads them (`inputButtonPressed(menuKey)`, `gamepadPressed(player1,
/// gamepadTogglePause)`). On the held card the game's `pressPause` cannot
/// tell the key from a focus-loss pause, so the rules poll this instead.
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

#[cfg(test)]
mod tests {
    use super::control_to_restore;

    #[test]
    fn the_control_lift_puts_back_what_it_found() {
        assert_eq!(control_to_restore(0.0), Some(0.0), "down: lifted, 0 after");
        assert_eq!(control_to_restore(1.0), None, "already 1: left alone");
    }
}
