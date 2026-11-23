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

//! The hooks of [`crate::loadout`]: the host's run settings apply to both
//! games of a linked run.
//!
//! * `confirmLoadout` (before + after): host: a new run's Start sends its settings, Resume Run
//!   refused while linked; joining game: rows locked, Resume Run refused, Start waits for or
//!   applies the host's start.
//! * `resetDailyChallenges` (after): the flags again while the host's start is applied.
//! * `ini_write_bool` (before): skipped while the host's start is applied.
//! * `resumeRun` (after): host: a resumed run's settings sent.
//! * `resetProgress` (after): while held, what it wrote becomes the player's own picks.
//! * `oLoadout_Step_0` (before): network pump; host: picks on change; joining game: hold the
//!   host's picks, make a waiting start, the bar line.
//! * `leaveLoadout` (before): nothing waits.
//! * `oLoadout_Draw_64` (before + after): the Start row reads "Ready" for the draw; the bar line
//!   and link line.
//!
//! [`menu_frame`] (from `lib.rs`) gives the player's picks back and sends the
//! host's.

#![forbid(unsafe_code)]

use std::sync::Arc;

use ndml_net::Message;
use ndml_net::msg::DailyPick;
use ndml_sdk::{Call, Error, Instance, ModContext, Script, Value, Variable};

use crate::Shared;
use crate::game::{num, script, var, warn_once};
use crate::loadout::{self, Confirm, MODE_FLAGS, Own, Rules};
use crate::names;
use crate::net::{self, Out};
use crate::ui::text;

/// Everything resolved at init.
#[derive(Debug)]
struct Handles {
    /// `oLoadout.pos` (0: Start).
    pos: Option<Variable>,
    /// `oLoadout.fade` (1 once a start was taken).
    fade: Option<Variable>,
    daily_choice: Option<Variable>,
    daily_mod: Option<Variable>,
    daily_mod2: Option<Variable>,
    daily_string: Option<Variable>,
    /// `global.choseResumeRun`.
    resumed: Option<Variable>,
    /// The modes' titles (`names::MODE_TITLES`).
    titles: Vec<Option<Variable>>,
    confirm: Option<Script>,
    /// `global.strLoadoutStart`, the Start row's label.
    start_label: Option<Variable>,
}

impl Handles {
    fn resolve(ctx: &ModContext) -> Self {
        Self {
            pos: var(ctx, names::VAR_LOADOUT_POS),
            fade: var(ctx, names::VAR_LOADOUT_FADE),
            daily_choice: var(ctx, names::VAR_DAILY_CHOICE),
            daily_mod: var(ctx, names::VAR_DAILY_MOD),
            daily_mod2: var(ctx, names::VAR_DAILY_MOD2),
            daily_string: var(ctx, names::VAR_DAILY_STRING),
            resumed: var(ctx, names::GLOBAL_CHOSE_RESUME_RUN),
            titles: names::MODE_TITLES
                .iter()
                .map(|(v, _)| var(ctx, v))
                .collect(),
            confirm: script(ctx, names::CONFIRM_LOADOUT),
            start_label: var(ctx, names::GLOBAL_LOADOUT_START),
        }
    }
}

fn hook_or_warn(name: &str, r: Result<(), Error>) {
    if let Err(e) = r {
        log::warn!("co-op: unable to hook {name} ({e})");
        note_failed(name);
    }
}

/// `ini_write_bool` calls skipped during the applied start (for its log).
static SKIPPED_WRITES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Every hook the mod failed to register (`lib.rs` reports its own here).
static FAILED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A hook that did not register.
pub(crate) fn note_failed(name: &str) {
    if let Ok(mut f) = FAILED.lock() {
        f.push(name.to_owned());
    }
}

/// After every hook is registered: turn the rule on only if everything it
/// needs is there. Off, each game plays its own picks.
pub(crate) fn decide(s: &Shared) {
    let failed = FAILED.lock().map_or_default(|f| f.clone());
    let missing = loadout::missing_hooks(&loadout::NEEDED_HOOKS, &failed);
    // The Start label is not needed: without it the rule still works.
    let h = HANDLES.get();
    let mut handles = vec![
        ("oLoadout.pos", h.is_some_and(|h| h.pos.is_some())),
        ("oLoadout.fade", h.is_some_and(|h| h.fade.is_some())),
        (
            "gml_Script_confirmLoadout (script)",
            h.is_some_and(|h| h.confirm.is_some()),
        ),
        (
            "global.choseResumeRun",
            h.is_some_and(|h| h.resumed.is_some()),
        ),
    ];
    handles.extend(
        s.h.mode
            .iter()
            .take(MODE_FLAGS)
            .map(|(name, v)| (*name, v.is_some())),
    );
    match loadout::rule_off_because(&missing, &handles) {
        None => {
            s.lock().loadout.enable();
            log::info!(
                "co-op: the host's run settings rule is on (every hook and game variable it needs found)"
            );
        }
        Some(why) => log::warn!(
            "co-op: the host's run settings are off in this game ({why}); each player plays their own game mode picks, and a co-op run with different picks is refused at its start"
        ),
    }
}

/// Keep the resume file off while the host's settings may be in the flags.
/// Called under the lock after every change.
fn sync_held(s: &Shared, c: &crate::Coop) {
    s.flags.set_rules_held(c.loadout.held());
}

/// Install the hooks.
pub(crate) fn register(ctx: &ModContext, shared: &Arc<Shared>) {
    let h = Arc::new(Handles::resolve(ctx));
    let _ = HANDLES.set(Arc::clone(&h));
    let (s1, h1, s2, h2) = (
        Arc::clone(shared),
        Arc::clone(&h),
        Arc::clone(shared),
        Arc::clone(&h),
    );
    hook_or_warn(
        names::CONFIRM_LOADOUT,
        ctx.hook(
            names::CONFIRM_LOADOUT,
            move |call| confirm_before(call, &s1, &h1),
            move |call, before| confirm_after(call, &s2, &h2, before),
        ),
    );
    let s = Arc::clone(shared);
    hook_or_warn(
        names::RESET_DAILY,
        ctx.hook_after(names::RESET_DAILY, move |call| {
            let applying = s.lock().loadout.applying().cloned();
            if let Some(r) = applying {
                write_modes(call, &s, r.mods);
            }
        }),
    );
    let s = Arc::clone(shared);
    hook_or_warn(
        names::INI_WRITE_BOOL,
        ctx.hook_before(names::INI_WRITE_BOOL, move |call| {
            if s.lock().loadout.skip_writes() {
                call.skip_original();
                SKIPPED_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }),
    );
    let s = Arc::clone(shared);
    hook_or_warn(
        names::RESET_PROGRESS,
        ctx.hook_after(names::RESET_PROGRESS, move |call| {
            let mods = read_mods(call, &s);
            let back = s.lock().loadout.reset_progress(mods);
            if let Some(r) = back {
                log::info!(
                    "co-op: progress reset while the host's run settings are held: the reset's mode picks are this player's now"
                );
                write_modes(call, &s, r.mods);
            }
        }),
    );
    let s = Arc::clone(shared);
    hook_or_warn(
        names::RESUME_RUN,
        ctx.hook_after(names::RESUME_RUN, move |call| resumed_start(call, &s)),
    );
    let (s, hh) = (Arc::clone(shared), Arc::clone(&h));
    hook_or_warn(
        names::LOADOUT_STEP,
        ctx.hook_before(names::LOADOUT_STEP, move |call| step(call, &s, &hh)),
    );
    let s = Arc::clone(shared);
    hook_or_warn(
        names::LEAVE_LOADOUT,
        ctx.hook_before(names::LEAVE_LOADOUT, move |_| {
            let mut c = s.lock();
            if c.loadout.ready() {
                log::info!("co-op: the waiting start is let go (back from the new game screen)");
            }
            c.loadout.cancel();
            c.loadout_screen = Screen::default();
        }),
    );
    let (s1, h1, s2) = (Arc::clone(shared), Arc::clone(&h), Arc::clone(shared));
    hook_or_warn(
        names::LOADOUT_DRAW_GUI,
        ctx.hook(
            names::LOADOUT_DRAW_GUI,
            move |call| label_before(call, &s1, &h1),
            move |call, swapped| {
                let (note, hud) = {
                    let c = s2.lock();
                    (c.loadout_screen.note.clone(), c.loadout_screen.hud.clone())
                };
                crate::ui::glue::paint_note(call, &s2, note.as_deref(), hud);
                label_after(call, swapped);
            },
        ),
    );
}

/// What the loadout screen shows this frame (set by its step).
#[derive(Debug, Clone, Default)]
pub(crate) struct Screen {
    /// The bar line.
    pub note: Option<String>,
    /// The run HUD's link line (with `overlay` on).
    pub hud: Option<crate::ui::run::Hud>,
    /// The Start row reads "Ready".
    pub ready: bool,
    /// The swap was logged on this visit.
    logged: bool,
}

/// The Start row's label while the joining player's screen is the host's.
const READY: &str = "Ready";

/// The game's own label, put aside for one draw; the next step or menu frame
/// puts it back if the after-hook never ran.
static LABEL_PENDING: std::sync::Mutex<Option<(Variable, Value)>> = std::sync::Mutex::new(None);

/// Before `oLoadout_Draw_64`: "Ready" for this draw when the screen is
/// locked. Without the label, the game's own "Start" stays.
fn label_before(call: &Call<'_>, s: &Shared, h: &Handles) -> Option<(Variable, Value)> {
    restore_label(call);
    let log_it = {
        let mut c = s.lock();
        if !c.loadout_screen.ready {
            return None;
        }
        !std::mem::replace(&mut c.loadout_screen.logged, true)
    };
    let Some(v) = h.start_label else {
        warn_once(
            "loadout-label",
            "co-op: the new game screen's Start label was not found; it keeps the game's own word",
        );
        return None;
    };
    let g = call.global()?;
    let old = g.get_value(v)?;
    if let Err(e) = g.set(v, READY) {
        warn_once(
            "loadout-label",
            &format!(
                "co-op: cannot relabel the new game screen's Start ({e}); it keeps the game's own word"
            ),
        );
        return None;
    }
    if log_it {
        log::info!(
            "co-op: the new game screen's Start reads \"{READY}\" (the host's run settings)"
        );
    }
    if let Ok(mut p) = LABEL_PENDING.lock() {
        *p = Some((v, old.clone()));
    }
    Some((v, old))
}

/// After the draw: the game's own label back.
fn label_after(call: &Call<'_>, swapped: Option<(Variable, Value)>) {
    if let Some((v, old)) = swapped {
        if let Ok(mut p) = LABEL_PENDING.lock() {
            *p = None;
        }
        put_label(call, v, old);
    }
}

/// A label put aside and not put back yet goes back now.
fn restore_label(call: &Call<'_>) {
    let pending = LABEL_PENDING.lock().ok().and_then(|mut p| p.take());
    if let Some((v, old)) = pending {
        log::warn!(
            "co-op: the new game screen's Start label put back after a draw that did not finish"
        );
        put_label(call, v, old);
    }
}

fn put_label(call: &Call<'_>, v: Variable, old: Value) {
    if let Some(g) = call.global()
        && let Err(e) = g.set(v, old)
    {
        warn_once(
            "loadout-label-back",
            &format!("co-op: cannot put the new game screen's Start label back ({e})"),
        );
    }
}

/// The link line for this frame (`overlay` on), as the run HUD says it.
fn link_line(c: &mut crate::Coop, now: std::time::Instant) -> Option<crate::ui::run::Hud> {
    if !c.settings.ui.switches(c.settings.overlay).hud {
        return None;
    }
    let linked = c.core.linked();
    let partner = c.core.partners().keys().next().copied();
    let info = partner
        .and_then(|p| c.net.peer_info(p))
        .filter(|i| linked && i.connected);
    let received = info.as_ref().map(|i| i.stats.packets_recv);
    let silent = c.silence.update(received, now);
    let status = c.net.lobby_status();
    let peer = status
        .as_ref()
        .and_then(|st| st.connected_peers().next().cloned());
    let reconnecting = !linked && c.steam.view().reconnecting();
    if linked {
        c.last_partner = partner_name(c);
    }
    let name = if c.last_partner.is_empty() {
        text::FALLBACK_PARTNER.to_owned()
    } else {
        c.last_partner.clone()
    };
    crate::ui::run::loadout_hud(
        &name,
        linked,
        reconnecting,
        peer.as_ref().map(|p| p.rtt_ms),
        peer.as_ref().map(|p| p.loss_pct),
        silent,
    )
}

/// What the before-hook did, for the after-hook.
#[derive(Debug, Clone, Copy)]
enum Before {
    Nothing,
    /// A Start of the game's own (`fade` before it). `send`: the host's new
    /// run, whose settings go out once its fade begins.
    OwnStart {
        fade: Option<f64>,
        send: bool,
    },
    /// The host's start is applied (`fade` before it).
    Applied {
        fade: Option<f64>,
    },
}

/// The game's own Start began its fade in this call (`fade` became 1). The
/// game's `confirmLoadout` returns at once while `alarm[1]` or `alarm[2]` runs.
fn fade_began(me: &Instance<'_>, h: &Handles, before: Option<f64>) -> bool {
    num(me, h.fade).is_some_and(|f| (f - 1.0).abs() < 1e-6)
        && before.is_none_or(|f| (f - 1.0).abs() >= 1e-6)
}

fn pos0(me: &Instance<'_>, h: &Handles) -> bool {
    num(me, h.pos).is_some_and(|p| p.abs() < 0.5)
}

fn resumed(call: &Call<'_>, h: &Handles) -> bool {
    call.global()
        .and_then(|g| num(&g, h.resumed))
        .is_some_and(|v| v > 0.5)
}

/// Fails closed: on a panic the press is skipped unless the game's own may
/// run safely.
fn confirm_before(call: &mut Call<'_>, s: &Shared, h: &Handles) -> Before {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| confirm_decide(call, s, h)));
    r.unwrap_or_else(|_| {
        let safe = fail_closed(call, s);
        log::warn!(
            "co-op: the new game screen's press hook failed; {}",
            if safe {
                "the game's own press runs"
            } else {
                "the press is skipped"
            }
        );
        Before::Nothing
    })
}

/// Skip the press unless the game's own may run; `true` = it may.
fn fail_closed(call: &mut Call<'_>, s: &Shared) -> bool {
    let safe = {
        let c = s.lock();
        c.loadout.original_safe(c.core.linked())
    };
    if !safe {
        call.skip_original();
    }
    safe
}

fn confirm_decide(call: &mut Call<'_>, s: &Shared, h: &Handles) -> Before {
    let Some(me) = call.instance() else {
        fail_closed(call, s);
        return Before::Nothing;
    };
    let start = pos0(&me, h);
    let resumed = resumed(call, h);
    let mut c = s.lock();
    let linked = c.core.linked();
    let host = linked && c.core.is_host();
    // Already waiting: another press is not another line.
    let was_ready = c.loadout.ready();
    let (restore, decision) = if host {
        // Anything still held from a link as the joining game comes back first.
        let back = if c.loadout.held() {
            c.loadout.release()
        } else {
            None
        };
        (back, c.loadout.host_confirm(start, resumed))
    } else {
        c.loadout
            .confirm(linked, start, resumed, || read_own(call, Some(&me), s, h))
    };
    sync_held(s, &c);
    let send = host && c.loadout.enabled();
    drop(c);
    if let Some(own) = restore {
        if !give_back(call, Some((&me, h)), s, &own) {
            call.skip_original();
            warn_once(
                "loadout-give-back",
                "co-op: this player's mode picks could not be written back; the new game screen's press is skipped",
            );
            return Before::Nothing;
        }
        log::info!(
            "co-op: the link went on the new game screen; this player's mode picks are back"
        );
    }
    match decision {
        Confirm::Original if start => Before::OwnStart {
            fade: num(&me, h.fade),
            send,
        },
        Confirm::Original => Before::Nothing,
        Confirm::Starting => {
            call.skip_original();
            Before::Nothing
        }
        Confirm::Locked => {
            call.skip_original();
            crate::ui::glue::play_move(call, s);
            Before::Nothing
        }
        Confirm::ResumeBlocked => {
            call.skip_original();
            crate::ui::glue::play_move(call, s);
            log::info!("co-op: Resume Run refused (playing together)");
            Before::Nothing
        }
        Confirm::Wait => {
            call.skip_original();
            crate::ui::glue::play_select(call, s);
            if !was_ready {
                log::info!("co-op: start waits on the new game screen for the host's start");
            }
            Before::Nothing
        }
        Confirm::Apply(r) => {
            write_rules(call, Some(&me), s, h, &r);
            log::info!(
                "co-op: the host's run settings apply: {}",
                r.describe(&|i| names::MODE_TITLES[i].1.to_owned())
            );
            Before::Applied {
                fade: num(&me, h.fade),
            }
        }
    }
}

fn confirm_after(call: &mut Call<'_>, s: &Shared, h: &Handles, before: Before) {
    match before {
        Before::Nothing => {}
        Before::Applied { fade } => {
            // Not taken when the fade did not begin: the host's start is kept
            // for the next Start. Without an instance, taken.
            let taken = call.instance().is_none_or(|me| fade_began(&me, h, fade));
            if !taken {
                let mut c = s.lock();
                c.loadout.start_not_taken();
                sync_held(s, &c);
                drop(c);
                SKIPPED_WRITES.store(0, std::sync::atomic::Ordering::Relaxed);
                log::info!(
                    "co-op: the game did not take the host's start (its new game screen was busy); kept for this player's next Start"
                );
                return;
            }
            // The flags once more: this game's daily and account locks may
            // have changed them during the start; both games must hash the
            // same 12 flags at the start gate.
            let applied = s.lock().loadout.applying().cloned();
            if let Some(r) = applied {
                write_modes(call, s, r.mods);
            }
            s.lock().loadout.confirm_done();
            let n = SKIPPED_WRITES.swap(0, std::sync::atomic::Ordering::Relaxed);
            log::info!(
                "co-op: the host's start applied; {n} settings writes skipped (this player's saved mode picks untouched)"
            );
        }
        Before::OwnStart { fade, send } => {
            let Some(me) = call.instance() else { return };
            if !fade_began(&me, h, fade) {
                return;
            }
            s.lock().loadout.started_own();
            if send {
                let rules = read_rules(call, Some(&me), s, h);
                send_start(s, &rules);
            }
        }
    }
}

/// Host: its run started with `rules`; the joining game's start follows.
fn send_start(s: &Shared, rules: &Rules) {
    let mut c = s.lock();
    if !(c.core.linked() && c.core.is_host() && c.loadout.enabled()) {
        return;
    }
    log::info!(
        "co-op: run settings sent to the joining game: {}",
        rules.describe(&|i| names::MODE_TITLES[i].1.to_owned())
    );
    net::apply(&c.net, vec![Out::Broadcast(message(true, rules, true))]);
    c.host_sent.started();
}

/// Host: a resumed run (a resume pressed unlinked whose link completed before
/// `resumeRun`). `resumeRun` restores the saved flags, so they are read after
/// it; the daily's picks are not in the save, so none are sent.
fn resumed_start(call: &Call<'_>, s: &Shared) {
    let rules = Rules {
        mods: read_mods(call, s),
        daily: None,
    };
    send_start(s, &rules);
}

/// The wire form (`on_loadout`: sent from the host's loadout screen).
fn message(start: bool, r: &Rules, on_loadout: bool) -> Message {
    Message::RunRules {
        start,
        mods: r.mods,
        daily: r.daily.clone(),
        on_loadout,
    }
}

/// The host's `RunRules`, on a joining game.
pub(crate) fn on_rules(
    c: &mut crate::Coop,
    start: bool,
    mods: u16,
    daily: Option<DailyPick>,
    on_loadout: bool,
) {
    c.loadout.host_screen(on_loadout);
    let rules = Rules { mods, daily };
    if start {
        log::info!(
            "co-op: the host started a run: {}",
            rules.describe(&|i| names::MODE_TITLES[i].1.to_owned())
        );
    } else if c.loadout.host_picks() != Some(&rules) {
        log::info!(
            "co-op: the host's run settings: {}",
            rules.describe(&|i| names::MODE_TITLES[i].1.to_owned())
        );
    }
    c.loadout.on_rules(start, rules);
}

/// The 12 flags as bits.
fn read_mods(call: &Call<'_>, s: &Shared) -> u16 {
    let g = call.global();
    let flags: Vec<Option<f64>> =
        s.h.mode
            .iter()
            .take(MODE_FLAGS)
            .map(|(_, v)| g.as_ref().and_then(|g| num(g, *v)))
            .collect();
    loadout::mode_bits(&flags)
}

/// The loadout screen's daily picks and name, as they are.
fn read_daily(me: &Instance<'_>, h: &Handles) -> Option<DailyPick> {
    let int = |v| num(me, v).map(|x| x.round() as i16);
    let (choice, modifier, modifier2) =
        (int(h.daily_choice)?, int(h.daily_mod)?, int(h.daily_mod2)?);
    let name = h
        .daily_string
        .and_then(|v| me.get_value(v))
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    Some(DailyPick {
        choice,
        modifier,
        modifier2,
        name,
    })
}

/// What this game picked (the host's, to send).
fn read_rules(call: &Call<'_>, me: Option<&Instance<'_>>, s: &Shared, h: &Handles) -> Rules {
    let mods = read_mods(call, s);
    let daily = me.and_then(|me| read_daily(me, h)).map(|mut d| {
        d.name = loadout::clip_name(&d.name);
        d
    });
    Rules {
        mods,
        daily: daily.filter(|_| mods & (1 << loadout::DAILY_BIT) != 0),
    }
}

fn read_own(call: &Call<'_>, me: Option<&Instance<'_>>, s: &Shared, h: &Handles) -> Own {
    Own {
        mods: read_mods(call, s),
        loadout_daily: me.and_then(|me| read_daily(me, h)),
    }
}

/// Write the 12 flags from `mods`; `true` = every flag with a handle was
/// written.
fn write_modes(call: &Call<'_>, s: &Shared, mods: u16) -> bool {
    let Some(g) = call.global() else { return false };
    let mut ok = true;
    for (i, (_, v)) in s.h.mode.iter().take(MODE_FLAGS).enumerate() {
        if let Some(v) = v {
            ok &= g.set(*v, mods & (1 << i) != 0).is_ok();
        }
    }
    ok
}

fn write_daily(me: &Instance<'_>, h: &Handles, d: &DailyPick) -> bool {
    let mut ok = true;
    for (v, x) in [
        (h.daily_choice, d.choice),
        (h.daily_mod, d.modifier),
        (h.daily_mod2, d.modifier2),
    ] {
        if let Some(v) = v {
            ok &= me.set_f64(v, f64::from(x));
        }
    }
    if let Some(v) = h.daily_string
        && !d.name.is_empty()
    {
        ok &= me.set(v, d.name.as_str()).is_ok();
    }
    ok
}

/// Hold `r`: the flags and (on the loadout screen) the daily's picks.
fn write_rules(call: &Call<'_>, me: Option<&Instance<'_>>, s: &Shared, h: &Handles, r: &Rules) {
    write_modes(call, s, r.mods);
    if let (Some(me), Some(d)) = (me, &r.daily) {
        write_daily(me, h, d);
    }
}

/// Write the player's own picks back; the kept copy is dropped only when every
/// write succeeded. `false` = not written, tried again next frame.
fn give_back(
    call: &Call<'_>,
    loadout: Option<(&Instance<'_>, &Handles)>,
    s: &Shared,
    own: &Own,
) -> bool {
    let mut ok = write_modes(call, s, own.mods);
    if let (Some((me, h)), Some(d)) = (loadout, &own.loadout_daily) {
        ok &= write_daily(me, h, d);
    }
    if ok {
        let mut c = s.lock();
        c.loadout.restored();
        sync_held(s, &c);
    }
    ok
}

/// The partner's name as the screen shows it.
fn partner_name(c: &crate::Coop) -> String {
    c.core.partners().values().next().map_or_else(
        || text::FALLBACK_PARTNER.to_owned(),
        |p| text::display_name(&p.name, &c.lang, text::FALLBACK_PARTNER),
    )
}

/// Clear a write skip left on by a `confirmLoadout` that never returned.
fn clear_stuck_skip(c: &mut crate::Coop) {
    if c.loadout.frame_tick() {
        log::warn!(
            "co-op: a start's settings-write skip was left on (its confirmLoadout never returned); cleared"
        );
    }
}

/// A loadout frame: the network pump; the host's picks out; the joining
/// game's hold, waiting start and bar line.
fn step(call: &mut Call<'_>, s: &Shared, h: &Handles) {
    restore_label(call);
    let now = std::time::Instant::now();
    let Some(me) = call.instance() else { return };
    let mut c = s.lock();
    clear_stuck_skip(&mut c);
    c.pump(now);
    if c.core.linked() && c.core.is_host() {
        if c.loadout.enabled() {
            let picks = read_rules(call, Some(&me), s, h);
            if let Some(r) = c.host_sent.picks(picks, true) {
                net::apply(&c.net, vec![Out::Broadcast(message(false, &r, true))]);
            }
        }
        // Only after a refused Resume Run.
        let partner = partner_name(&c);
        c.loadout_screen.note =
            loadout::bar_line(&c.loadout.host_note(), true, &partner, "Start", &|_| {
                String::new()
            });
        c.loadout_screen.ready = false;
        c.loadout_screen.hud = link_line(&mut c, now);
        let back = if c.loadout.held() {
            c.loadout.release()
        } else {
            None
        };
        drop(c);
        if let Some(own) = back
            && give_back(call, Some((&me, h)), s, &own)
        {
            log::info!("co-op: hosting now; this player's mode picks are back");
        }
        return;
    }
    let linked_joiner = c.core.linked();
    let frame = c
        .loadout
        .loadout_frame(linked_joiner, || read_own(call, Some(&me), s, h));
    // Before the host's are written: the resume file is off first.
    sync_held(s, &c);
    let partner = partner_name(&c);
    let start_word = if h.start_label.is_some() {
        READY
    } else {
        "Start"
    };
    c.loadout_screen.note = loadout::bar_line(
        &frame.note,
        c.loadout.host_on_loadout(),
        &partner,
        start_word,
        &|r| r.describe(&|i| title(call, h, i)),
    );
    c.loadout_screen.ready = c.loadout.locked(linked_joiner);
    c.loadout_screen.hud = link_line(&mut c, now);
    let due = c.loadout.start_due();
    drop(c);
    if let Some(own) = &frame.restore {
        if give_back(call, Some((&me, h)), s, own) {
            log::info!("co-op: unlinked on the new game screen; this player's mode picks are back");
        } else {
            warn_once(
                "loadout-give-back",
                "co-op: this player's mode picks could not be written back; tried again each frame, the new game screen's presses skipped until then",
            );
        }
    }
    if let Some(r) = &frame.show {
        write_rules(call, Some(&me), s, h, r);
    }
    if due && let (Some(pos), Some(confirm)) = (h.pos, h.confirm) {
        let _ = me.set_f64(pos, 0.0);
        log::info!("co-op: the host started; this game's waiting start is made");
        let fade = num(&me, h.fade);
        if let Err(e) = me.call_script(confirm, &[]) {
            log::warn!("co-op: the waiting start could not be made ({e}); press Start again");
            let mut c = s.lock();
            c.loadout.cancel();
            // Not begun: the host's start is kept, the latch let go.
            if !fade_began(&me, h, fade) {
                c.loadout.start_not_taken();
                sync_held(s, &c);
            }
        }
    }
}

/// A main menu frame: the player's own picks back, the host's picks out, and
/// the Co-op page's line for a joining game.
pub(crate) fn menu_frame(call: &Call<'_>, s: &Shared) {
    restore_label(call);
    let h = HANDLES.get();
    let mut c = s.lock();
    clear_stuck_skip(&mut c);
    c.host_sent.menu();
    let host = c.core.linked() && c.core.is_host();
    if host && c.loadout.enabled() {
        // No `oLoadout` in the menu: the flags only, no daily picks.
        let picks = Rules {
            mods: read_mods(call, s),
            daily: None,
        };
        if let Some(r) = c.host_sent.picks(picks, false) {
            net::apply(&c.net, vec![Out::Broadcast(message(false, &r, false))]);
        }
    }
    let line = if c.core.linked() && !host {
        loadout::page_line(
            c.loadout.host_picks(),
            c.loadout.host_started(),
            &partner_name(&c),
            &|r| {
                r.describe(&|i| {
                    h.map_or_else(|| names::MODE_TITLES[i].1.to_owned(), |h| title(call, h, i))
                })
            },
        )
    } else {
        None
    };
    c.page.set_host_run(line);
    let restore = c.loadout.menu_frame();
    c.loadout_screen = Screen::default();
    drop(c);
    if let Some(own) = restore {
        if give_back(call, None, s, &own) {
            log::info!("co-op: in the menu; this player's mode picks are back");
        } else {
            warn_once(
                "loadout-give-back-menu",
                "co-op: this player's mode picks could not be written back in the menu; tried again each frame",
            );
        }
    }
}

/// The handles, for the menu frame (`register` sets them).
static HANDLES: std::sync::OnceLock<Arc<Handles>> = std::sync::OnceLock::new();

/// Mode `i`'s title as this game shows it (its `strUnlocksTitle*`), else the
/// English one.
fn title(call: &Call<'_>, h: &Handles, i: usize) -> String {
    call.global()
        .zip(h.titles.get(i).copied().flatten())
        .and_then(|(g, v)| g.get_value(v))
        .and_then(|v| v.as_str().map(str::to_owned))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| names::MODE_TITLES[i].1.to_owned())
}
