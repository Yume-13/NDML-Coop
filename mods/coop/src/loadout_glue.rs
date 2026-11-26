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
//! * `resumeRun` (after): the next run is a resumed one, never a co-op run.
//! * `resetProgress` (after): while held, what it wrote becomes the player's own picks.
//! * `oLoadout_Step_0` (before): network pump; host: picks on change; joining game: hold the
//!   host's picks, make a waiting start, the bar line.
//! * `leaveLoadout` (before): nothing waits.
//! * `oLoadout_Draw_64` (before + after): the Start row reads "Ready" for the draw; a refused
//!   Resume Run's reason in the screen's warning line; the bar line and link line.
//!
//! [`menu_frame`] (from `lib.rs`) gives the player's picks back and sends the
//! host's.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Instant;

use ndml_net::Message;
use ndml_net::msg::DailyPick;
use ndml_sdk::{Call, Error, Instance, ModContext, Script, Value, Variable};

use crate::Shared;
use crate::game::{num, script, var, warn_once};
use crate::loadout::{self, Confirm, Fades, MODE_FLAGS, Note, Own, Rules, SavedRunGuard};
use crate::names;
use crate::net::{self, Out};
use crate::ui::text;

/// Everything resolved at init.
#[derive(Debug)]
struct Handles {
    /// `oLoadout.pos` (0: Start).
    pos: Option<Variable>,
    /// `oLoadout.fade` (1 once a new run's start was taken) and
    /// `oLoadout.fadeBlack` (1 once a resumed run's was).
    fade: Option<Variable>,
    fade_black: Option<Variable>,
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
    /// The screen's red warning line in resume mode: its text
    /// (`global.strLoadoutResumeRunNote`) and its colour (`oLoadout.rgbG`, `rgbB`).
    resume_note: Option<Variable>,
    rgb_g: Option<Variable>,
    rgb_b: Option<Variable>,
    /// `global.loaded_run_state` and `leaveLoadout`, for `loadout::SavedRunGuard`.
    loaded_run: Option<Variable>,
    leave: Option<Script>,
}

impl Handles {
    /// The refused Resume Run can be said in the screen's own red line.
    fn red_line(&self) -> bool {
        self.resume_note.is_some() && self.rgb_g.is_some() && self.rgb_b.is_some()
    }
}

impl Handles {
    fn resolve(ctx: &ModContext) -> Self {
        Self {
            pos: var(ctx, names::VAR_LOADOUT_POS),
            fade: var(ctx, names::VAR_LOADOUT_FADE),
            fade_black: var(ctx, names::VAR_LOADOUT_FADE_BLACK),
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
            resume_note: var(ctx, names::GLOBAL_LOADOUT_RESUME_NOTE),
            rgb_g: var(ctx, names::VAR_LOADOUT_RGB_G),
            rgb_b: var(ctx, names::VAR_LOADOUT_RGB_B),
            loaded_run: var(ctx, names::GLOBAL_LOADED_RUN_STATE),
            leave: script(ctx, names::LEAVE_LOADOUT),
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
    if let Some(why) = loadout::run_safety(&failed) {
        log::warn!(
            "co-op: co-op runs are off in this game ({why}); linking still works in the menus, every run is solo"
        );
        s.lock().core.coop_runs_off(why);
    }
    let missing = loadout::missing_hooks(&loadout::NEEDED_HOOKS, &failed);
    // The rule works without the Start label and the bar.
    let h = HANDLES.get();
    let mut handles = vec![
        ("oLoadout.pos", h.is_some_and(|h| h.pos.is_some())),
        ("oLoadout.fade", h.is_some_and(|h| h.fade.is_some())),
        (
            "oLoadout.fadeBlack",
            h.is_some_and(|h| h.fade_black.is_some()),
        ),
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
        ctx.hook_after(names::RESUME_RUN, move |_| resumed_start(&s)),
    );
    let (s, hh) = (Arc::clone(shared), Arc::clone(&h));
    hook_or_warn(
        names::LOADOUT_STEP,
        ctx.hook_before(names::LOADOUT_STEP, move |call| step(call, &s, &hh)),
    );
    let s = Arc::clone(shared);
    hook_or_warn(
        names::LEAVE_LOADOUT,
        ctx.hook_before(names::LEAVE_LOADOUT, move |call| leave_before(call, &s)),
    );
    let (s1, h1, s2) = (Arc::clone(shared), Arc::clone(&h), Arc::clone(shared));
    hook_or_warn(
        names::LOADOUT_DRAW_GUI,
        ctx.hook(
            names::LOADOUT_DRAW_GUI,
            move |call| label_before(call, &s1, &h1),
            move |call, swapped| {
                let (note, hud, alert, count) = {
                    let mut c = s2.lock();
                    let sc = &c.loadout_screen;
                    let note = sc.note.clone().map(|t| crate::ui::run::Bar {
                        pulse: sc.pulse,
                        ..crate::ui::run::Bar::new(t)
                    });
                    let (hud, count) = (sc.hud.clone(), sc.count);
                    if sc.pulse
                        && let Some(text) = note.as_ref().map(|b| b.text.clone())
                    {
                        crate::ui::glue::ready_shown(
                            &mut c,
                            crate::ui::run::ReadyPlace::NewGameScreen,
                            &text,
                        );
                    }
                    (note, hud, c.ready_notice.step(), count)
                };
                crate::ui::glue::paint_screen_note(call, &s2, note, alert, hud, count);
                label_after(call, swapped);
            },
        ),
    );
}

/// Back from the new game screen (`leaveLoadout`): nothing waits, a start
/// count is called off. After the count's go the back is skipped.
fn leave_before(call: &mut Call<'_>, s: &Shared) {
    let mut c = s.lock();
    if c.loadout.start_locked() {
        call.skip_original();
        log::info!("co-op: back on the new game screen skipped (the start count's go is out)");
        return;
    }
    if c.loadout.ready() {
        log::info!("co-op: the waiting start is let go (back from the new game screen)");
    }
    c.loadout.cancel();
    c.loadout_screen = Screen::default();
}

/// What the new game screen shows this frame (set by its step).
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // independent facts of one visit, each read alone
pub(crate) struct Screen {
    /// The bar line (either player's).
    pub note: Option<String>,
    /// The bar pulses: the partner is ready, this player not yet.
    pub pulse: bool,
    /// The run HUD's link line (with `overlay` on).
    pub hud: Option<crate::ui::run::Hud>,
    /// The start count's number, while it runs.
    pub count: Option<crate::ui::run::CountView>,
    /// The Start row reads "Ready".
    pub ready: bool,
    /// The swap was logged on this visit.
    logged: bool,
    /// Steps since a refused Resume Run press. The red line strobes, then
    /// holds the mod's sentence until the next press or leaving the screen.
    warn: Option<u32>,
    /// What the red line says (set by the step while `warn` is on).
    warn_line: Option<String>,
    /// The red line's colour could not be written: the bar says it instead.
    warn_failed: bool,
    /// This visit came from the Game Start whose saved-run question the mod
    /// answered (`ui::glue`).
    preanswered: bool,
    /// Since when the link has been out on this visit.
    unlinked_since: Option<std::time::Instant>,
    /// The guard took the player back to the menu (once).
    left: bool,
}

impl Screen {
    /// The refused Resume Run is said by the screen's red line, else by the bar.
    fn red_line(&self, h: &Handles) -> bool {
        h.red_line() && !self.warn_failed
    }
}

/// The Start row's label while the joining player's screen is the host's.
const READY: &str = "Ready";

/// The game's own strings, put aside for one draw; the next step or menu
/// frame puts them back if the after-hook never ran.
static LABEL_PENDING: std::sync::Mutex<Vec<(Variable, Value)>> = std::sync::Mutex::new(Vec::new());

/// Before `oLoadout_Draw_64`: "Ready" for this draw when the screen is
/// locked, and the mod's sentence in the red line after a refused Resume Run.
/// Where a string cannot be written, the game's own words stay.
fn label_before(call: &Call<'_>, s: &Shared, h: &Handles) -> Vec<(Variable, Value)> {
    restore_label(call);
    let (ready, log_it, warn) = {
        let mut c = s.lock();
        let sc = &mut c.loadout_screen;
        let ready = sc.ready;
        let log_it = ready && !std::mem::replace(&mut sc.logged, true);
        let warn = sc.warn_line.clone().filter(|_| sc.red_line(h));
        (ready, log_it, warn)
    };
    let mut out = Vec::new();
    if ready {
        if let Some(v) = h.start_label {
            if swap_text(call, v, READY, "loadout-label", "Start label", &mut out) && log_it {
                log::info!(
                    "co-op: the new game screen's Start reads \"{READY}\" (linked: the run starts when both players are ready)"
                );
            }
        } else {
            warn_once(
                "loadout-label",
                "co-op: the new game screen's Start label was not found; it keeps the game's own word",
            );
        }
    }
    if let (Some(line), Some(v)) = (warn, h.resume_note) {
        swap_text(call, v, &line, "loadout-note", "warning line", &mut out);
    }
    if let Ok(mut p) = LABEL_PENDING.lock() {
        p.clone_from(&out);
    }
    out
}

/// Write `text` into the global `v` for this draw, the value it replaces into
/// `out`. `false` (one warning under `key`) when it cannot be.
fn swap_text(
    call: &Call<'_>,
    v: Variable,
    text: &str,
    key: &str,
    what: &str,
    out: &mut Vec<(Variable, Value)>,
) -> bool {
    let Some(g) = call.global() else { return false };
    let Some(old) = g.get_value(v) else {
        return false;
    };
    if let Err(e) = g.set(v, text) {
        warn_once(
            key,
            &format!(
                "co-op: cannot write the new game screen's {what} ({e}); it keeps the game's own words"
            ),
        );
        return false;
    }
    out.push((v, old));
    true
}

/// After the draw: the game's own strings back.
fn label_after(call: &Call<'_>, swapped: Vec<(Variable, Value)>) {
    if swapped.is_empty() {
        return;
    }
    if let Ok(mut p) = LABEL_PENDING.lock() {
        p.clear();
    }
    for (v, old) in swapped.into_iter().rev() {
        put_label(call, v, old);
    }
}

/// Strings put aside and not put back go back.
fn restore_label(call: &Call<'_>) {
    let pending = LABEL_PENDING
        .lock()
        .ok()
        .map_or_default(|mut p| std::mem::take(&mut *p));
    if pending.is_empty() {
        return;
    }
    log::warn!("co-op: the new game screen's strings put back after a draw that did not finish");
    for (v, old) in pending.into_iter().rev() {
        put_label(call, v, old);
    }
}

fn put_label(call: &Call<'_>, v: Variable, old: Value) {
    if let Some(g) = call.global()
        && let Err(e) = g.set(v, old)
    {
        warn_once(
            "loadout-label-back",
            &format!("co-op: cannot put a new game screen string back ({e})"),
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
    /// A Start of the game's own (the fades before it). `send`: the host's
    /// new run, whose settings go out once its fade begins.
    OwnStart {
        fades: Fades,
        send: bool,
        /// The Start resumes a saved run (`choseResumeRun`).
        resumed: bool,
    },
    /// The host's start is applied (the fades before it).
    Applied {
        fades: Fades,
    },
}

/// The screen's two fades now (`fade`, `fadeBlack`).
fn fades(me: &Instance<'_>, h: &Handles) -> Fades {
    (num(me, h.fade), num(me, h.fade_black))
}

/// The game took a Start in this call (`before`: the fades before it): one
/// of its fades began (`loadout::start_began`).
fn fade_began(me: &Instance<'_>, h: &Handles, before: Fades) -> bool {
    loadout::start_began(before, fades(me, h))
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
    // The saved run's guard: no solo start over a saved run the game never
    // asked about (`loadout::SavedRunGuard`).
    if start && !resumed && saved_run_guard(call, s, h, Instant::now()) != SavedRunGuard::Off {
        call.skip_original();
        crate::ui::glue::play_move(call, s);
        log::info!("co-op: a solo Start over the saved run refused (the link is out)");
        return Before::Nothing;
    }
    let mut c = s.lock();
    let linked = c.core.linked();
    let host = linked && c.core.is_host();
    // Already waiting: another press is not another line.
    let was_ready = c.loadout.ready();
    // Any press ends the mod's red line.
    c.loadout_screen.warn = None;
    c.loadout_screen.warn_line = None;
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
            fades: fades(&me, h),
            send,
            resumed,
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
            // The screen's red line strobes from this step on.
            s.lock().loadout_screen.warn = Some(0);
            log::info!("co-op: Resume Run refused (playing together)");
            Before::Nothing
        }
        Confirm::Wait => {
            call.skip_original();
            crate::ui::glue::play_select(call, s);
            if !was_ready {
                log::info!(
                    "co-op: Ready on the new game screen ({}); the run starts when both players are",
                    if host { "host" } else { "joining player" }
                );
            }
            Before::Nothing
        }
        Confirm::Unready => {
            call.skip_original();
            crate::ui::glue::play_move(call, s);
            log::info!("co-op: Ready taken back on the new game screen");
            Before::Nothing
        }
        Confirm::Apply(r) => {
            write_rules(call, Some(&me), s, h, &r);
            log::info!(
                "co-op: the host's run settings apply: {}",
                r.describe(&|i| names::MODE_TITLES[i].1.to_owned())
            );
            Before::Applied {
                fades: fades(&me, h),
            }
        }
    }
}

fn confirm_after(call: &mut Call<'_>, s: &Shared, h: &Handles, before: Before) {
    match before {
        Before::Nothing => {}
        Before::Applied { fades } => {
            // Not taken when the fade did not begin: the host's start is kept
            // for the next Start. Without an instance, taken.
            let taken = call.instance().is_none_or(|me| fade_began(&me, h, fades));
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
            let mut c = s.lock();
            c.loadout.confirm_done();
            start_taken(s, &mut c);
            drop(c);
            let n = SKIPPED_WRITES.swap(0, std::sync::atomic::Ordering::Relaxed);
            log::info!(
                "co-op: the host's start applied; {n} settings writes skipped (this player's saved mode picks untouched)"
            );
        }
        Before::OwnStart {
            fades,
            send,
            resumed,
        } => {
            let Some(me) = call.instance() else { return };
            if !fade_began(&me, h, fades) {
                return;
            }
            // A resumed run's Start too (`fadeBlack`).
            let mut c = s.lock();
            c.loadout.started_own();
            if resumed {
                // Its run is a resumed one from this press on.
                c.core.resumed_next();
                log::info!("co-op: a saved run's Resume Start taken: its run is solo");
            } else {
                start_taken(s, &mut c);
            }
            drop(c);
            if send {
                let rules = read_rules(call, Some(&me), s, h);
                send_start(s, &rules);
            }
        }
    }
}

/// The game took a new run's Start (either role, the rule on or off).
/// Taken while linked, its run is a co-op run and its resume file is off
/// (`Core::start_taken`).
fn start_taken(s: &Shared, c: &mut crate::Coop) {
    c.core.start_taken();
    s.flags.set_start_held(c.core.start_held());
    if c.core.start_held() {
        log::info!("co-op: a new run's Start taken while linked: its run is a co-op run");
    }
}

/// Host: its run started with `rules`; the joining game's start follows.
fn send_start(s: &Shared, rules: &Rules) {
    let mut c = s.lock();
    if !(c.core.is_host() && c.loadout.enabled()) {
        return;
    }
    // Kept with or without the link, for `start_again`.
    c.host_sent.started(rules);
    if !c.core.linked() {
        return;
    }
    log::info!(
        "co-op: run settings sent to the joining game: {}",
        rules.describe(&|i| names::MODE_TITLES[i].1.to_owned())
    );
    net::apply(&c.net, vec![Out::Broadcast(message(true, rules, true))]);
}

/// Host, at a new link: a run that waits for the partner at the start gate
/// sends its start again.
pub(crate) fn start_again(c: &crate::Coop) -> Option<Message> {
    let waiting = c.core.coop_run() && c.core.gate_phase() != crate::gate::GatePhase::Running;
    let rules = c.host_sent.start_again(waiting && c.loadout.enabled())?;
    log::info!(
        "co-op: re-linked while this run waits for the partner at its start gate: its start sent again ({})",
        rules.describe(&|i| names::MODE_TITLES[i].1.to_owned())
    );
    Some(message(true, rules, false))
}

/// Any game: a resumed run is starting (`resumeRun`, from `oLoadout_Alarm_2`).
/// The run stays solo, and a partner linked by its first frame is turned away
/// (`Core::resumed_next`).
fn resumed_start(s: &Shared) {
    let mut c = s.lock();
    let linked = c.core.linked();
    c.core.resumed_next();
    drop(c);
    log::info!(
        "co-op: a saved run is resumed{}: it is played solo",
        if linked {
            " while a link came up during its start (the partner is turned away)"
        } else {
            ""
        }
    );
}

/// The wire form (`on_loadout`: sent from the host's new game screen).
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

/// The new game screen's daily picks and name, as they are.
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

/// Hold `r`: the flags and (on the new game screen) the daily's picks.
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

/// A saved run is loaded (`global.loaded_run_state` 1). Unreadable: taken as one.
fn saved_run(call: &Call<'_>, h: &Handles) -> bool {
    loadout::saved_run_state(loaded_run_read(call, h))
}

fn loaded_run_read(call: &Call<'_>, h: &Handles) -> Option<f64> {
    call.global()
        .zip(h.loaded_run)
        .and_then(|(g, v)| g.get_value(v))
        .and_then(|v| v.as_f64())
}

/// `global.loaded_run_state` as read now (`None`: it cannot be read).
pub(crate) fn loaded_run_value(call: &Call<'_>) -> Option<f64> {
    loaded_run_read(call, HANDLES.get()?)
}

/// The saved run's guard on this frame (`loadout::SavedRunGuard`), its
/// clock kept on the screen's state.
fn saved_run_guard(call: &Call<'_>, s: &Shared, h: &Handles, now: Instant) -> SavedRunGuard {
    let saved = saved_run(call, h);
    let mut c = s.lock();
    let linked = c.core.linked();
    if c.menu.take_preanswered() {
        c.loadout_screen.preanswered = true;
    }
    let sc = &mut c.loadout_screen;
    if linked {
        sc.unlinked_since = None;
    } else if sc.preanswered {
        sc.unlinked_since.get_or_insert(now);
    }
    loadout::saved_run_guard(
        sc.preanswered,
        saved,
        linked,
        sc.unlinked_since.map(|t| now.saturating_duration_since(t)),
    )
}

/// A new game screen frame of the saved run's guard: with the link gone, back
/// to the menu once.
fn guard_step(call: &Call<'_>, me: &Instance<'_>, s: &Shared, h: &Handles, now: Instant) {
    if saved_run_guard(call, s, h, now) != SavedRunGuard::BackToMenu {
        return;
    }
    let first = {
        let mut c = s.lock();
        let first = !std::mem::replace(&mut c.loadout_screen.left, true);
        if first {
            let line = loadout::saved_run_kept_line(&partner_or_last(&c));
            c.link_notice = Some(crate::ui::run::LinkNotice::for_room(
                line,
                now,
                crate::ui::run::NoticeRoom::Menus,
            ));
        }
        first
    };
    if first {
        log::info!(
            "co-op: the link is gone on the new game screen and a saved run exists: back to the menu, where Game Start asks the game's own question"
        );
        let Some(leave) = h.leave else {
            warn_once(
                "loadout-leave",
                "co-op: leaveLoadout not found; a solo Start stays refused on this screen",
            );
            return;
        };
        if let Err(e) = me.call_script(leave, &[]) {
            warn_once(
                "loadout-leave",
                &format!(
                    "co-op: leaving the new game screen failed ({e}); a solo Start stays refused"
                ),
            );
        }
    }
}

/// The partner's name as the screen shows it, or the one last seen. Never
/// for a log line.
pub(crate) fn partner_shown(c: &crate::Coop) -> String {
    partner_or_last(c)
}

/// The partner's name, or the one last seen (the link is gone).
fn partner_or_last(c: &crate::Coop) -> String {
    if c.core.linked() {
        partner_name(c)
    } else if c.last_partner.is_empty() {
        text::FALLBACK_PARTNER.to_owned()
    } else {
        c.last_partner.clone()
    }
}

/// A new game screen frame: the network pump; the host's picks out; the
/// joining game's hold, waiting start and bar line.
fn step(call: &mut Call<'_>, s: &Shared, h: &Handles) {
    restore_label(call);
    let now = std::time::Instant::now();
    let Some(me) = call.instance() else { return };
    strobe(&me, s, h);
    guard_step(call, &me, s, h, now);
    let mut c = s.lock();
    clear_stuck_skip(&mut c);
    c.pump(now);
    // On the new game screen while linked: the link's move here is spent.
    let linked = c.core.linked();
    c.menu.to_new_game.observe(linked);
    if linked {
        c.menu.to_new_game.spent();
    }
    if c.core.linked() && c.core.is_host() {
        host_step(call, &me, s, h, c, now);
        return;
    }
    let linked_joiner = c.core.linked();
    let frame = c
        .loadout
        .loadout_frame(linked_joiner, || read_own(call, Some(&me), s, h));
    // Before the host's are written: the resume file is off first.
    sync_held(s, &c);
    let partner = partner_name(&c);
    let start_word = start_word(h, true);
    // A refused Resume Run is the red line's, where it can be drawn.
    let note = match &frame.note {
        Note::NoResume if c.loadout_screen.red_line(h) => c.loadout.note_past_resume(),
        n => n.clone(),
    };
    c.loadout_screen.pulse = note == Note::PartnerReady;
    c.loadout_screen.note = loadout::bar_line(
        &note,
        c.loadout.host_on_loadout(),
        &partner,
        start_word,
        &|r| r.describe(&|i| title(call, h, i)),
    );
    c.loadout_screen.ready = c.loadout.locked(linked_joiner);
    c.loadout_screen.hud = link_line(&mut c, now);
    let due = c.loadout.start_due();
    quiet_notice_in_count(&mut c);
    let cue = c.ready_notice.take_cue();
    drop(c);
    ready_cue(call, s, cue, now);
    start_count_frame(call, s, now);
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
        let before = fades(&me, h);
        if let Err(e) = me.call_script(confirm, &[]) {
            log::warn!("co-op: the waiting start could not be made ({e}); press Start again");
            let mut c = s.lock();
            c.loadout.cancel();
            // Not begun: the host's start is kept, the latch let go.
            if !fade_began(&me, h, before) {
                c.loadout.start_not_taken();
                sync_held(s, &c);
            }
        }
    }
}

/// A linked host's new game screen frame: its picks out, its bar line, "Ready"
/// on its Start row, and the run's start once both are ready ([`host_go`]).
fn host_step(
    call: &Call<'_>,
    me: &Instance<'_>,
    s: &Shared,
    h: &Handles,
    mut c: std::sync::MutexGuard<'_, crate::Coop>,
    now: Instant,
) {
    if c.loadout.enabled() {
        let picks = read_rules(call, Some(me), s, h);
        if let Some(r) = c.host_sent.picks(picks, true) {
            net::apply(&c.net, vec![Out::Broadcast(message(false, &r, true))]);
        }
    }
    // A refused Resume Run is the red line's, where it can be drawn.
    let partner = partner_name(&c);
    let note = match c.loadout.host_note() {
        Note::NoResume if c.loadout_screen.red_line(h) => Note::None,
        n => n,
    };
    let word = start_word(h, c.loadout.enabled());
    c.loadout_screen.pulse = note == Note::PartnerReady;
    c.loadout_screen.note = loadout::bar_line(&note, true, &partner, word, &|_| String::new());
    // Start is Ready while linked, with the rule on.
    c.loadout_screen.ready = c.loadout.enabled();
    c.loadout_screen.hud = link_line(&mut c, now);
    // Hosting: anything held from a link as the joining game comes back.
    let back = if c.loadout.held() {
        c.loadout.release()
    } else {
        None
    };
    drop(c);
    if let Some(own) = back
        && give_back(call, Some((me, h)), s, &own)
    {
        log::info!("co-op: hosting now; this player's mode picks are back");
    }
    // The start count's numbers, then its next move (`Joiner::host_frame`).
    start_count_frame(call, s, now);
    let mut c = s.lock();
    c.loadout.host_frame(now);
    c.send_start_count();
    let go = c.loadout.press_due();
    quiet_notice_in_count(&mut c);
    let cue = c.ready_notice.take_cue();
    drop(c);
    ready_cue(call, s, cue, now);
    if go {
        host_go(me, s, h);
    }
}

/// While the start count runs, the partner's Ready has no alert or cue.
fn quiet_notice_in_count(c: &mut crate::Coop) {
    if c.loadout.start_count().is_some() {
        c.ready_notice.clear();
    }
}

/// The start count on this new game screen (either role): its tick at each
/// number and the number for the draw.
fn start_count_frame(call: &Call<'_>, s: &Shared, now: Instant) {
    let shown = s.lock().loadout.start_count();
    let view = shown.and_then(|(round, mut count)| {
        let (view, _go) = crate::ui::glue::count_frame(
            call,
            s,
            &mut count,
            crate::ui::run::CountKind::Start,
            now,
        );
        s.lock().loadout.set_start_count(round, count);
        view
    });
    s.lock().loadout_screen.count = view;
}

/// What the Start row reads: "Ready" while linked (`ready_mode`) where the
/// label can be swapped, else the game's own word.
fn start_word(h: &Handles, ready_mode: bool) -> &'static str {
    if ready_mode && h.start_label.is_some() {
        READY
    } else {
        "Start"
    }
}

/// The partner's new Ready: its cue (`cue`), else the cue's next notes.
fn ready_cue(call: &Call<'_>, s: &Shared, cue: bool, now: Instant) {
    if cue {
        crate::ui::glue::play_cue(call, s, crate::ui::cues::Cue::PartnerReady, now);
    } else {
        crate::ui::glue::cue_tick(call, s, now);
    }
}

/// Host, at the start count's go ([`loadout::Joiner::press_due`]): the game's
/// Start press through `confirmLoadout` (`pos` 0). A press the game does not
/// take is made again each frame until `loadout::GO_WAIT` calls the count off.
fn host_go(me: &Instance<'_>, s: &Shared, h: &Handles) {
    let (Some(pos), Some(confirm)) = (h.pos, h.confirm) else {
        // Not reached: the rule is off without these (`decide`).
        return;
    };
    s.lock().loadout.begin_go();
    let _ = me.set_f64(pos, 0.0);
    let before = fades(me, h);
    let r = me.call_script(confirm, &[]);
    let taken = r.is_ok() && fade_began(me, h, before);
    s.lock().loadout.go_done(taken);
    if taken {
        log::info!("co-op: both players are ready: the run starts");
    } else {
        warn_once(
            "loadout-go",
            &format!(
                "co-op: both players are ready but the game did not take the start ({r:?}); tried again each frame"
            ),
        );
    }
}

/// A new game screen step after a refused Resume Run: the red line flashes
/// (`loadout::warn_strobe`, before the game's step) and holds the mod's
/// sentence. A failed write hands the sentence to the bar.
fn strobe(me: &Instance<'_>, s: &Shared, h: &Handles) {
    let level = {
        let mut c = s.lock();
        let Some(n) = c.loadout_screen.warn else {
            return;
        };
        let line = loadout::no_resume_line(&partner_name(&c));
        let sc = &mut c.loadout_screen;
        sc.warn = Some(n.saturating_add(1));
        sc.warn_line = Some(line);
        if !sc.red_line(h) {
            return;
        }
        loadout::warn_strobe(n)
    };
    let Some(level) = level else { return };
    let ok = [h.rgb_g, h.rgb_b]
        .into_iter()
        .flatten()
        .all(|v| me.set_f64(v, level));
    if !ok {
        warn_once(
            "loadout-red-line",
            "co-op: the new game screen's warning colour cannot be written; the refused Resume Run is said in a bar",
        );
        s.lock().loadout_screen.warn_failed = true;
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
