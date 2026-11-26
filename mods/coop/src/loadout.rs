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

//! The host's run settings apply to both games of a linked run; the joining
//! player sees them read-only.
//!
//! * The 12 `gameMod*` flags and the daily picks (`dailyChoice`, `dailyModChoice`,
//!   `dailyModChoice2`). Locked modes are played anyway; nothing is unlocked.
//! * The game builds the run from the flags at Start (`confirmLoadout`) and in the run's
//!   first Create events, so the host sends its picks ([`Message::RunRules`]) whenever
//!   they change and when it starts.
//! * The joining game holds the host's picks with the rows locked; its Start means
//!   "ready"; Resume Run is not offered. Its own picks ([`Joiner::held`]) are kept, the host's
//!   are never saved, and its own come back at the first menu frame, or at once on the new
//!   game screen when the link goes. The resume file is not written meanwhile.
//! * Fails closed: on only if every hook in [`NEEDED_HOOKS`] registered and every
//!   variable and script was found ([`rule_off_because`]).
//! * From a Start to the menu nothing is held or given back. A resumed run's Start counts
//!   ([`start_began`]).
//!
//! [`Message::RunRules`]: ndml_net::Message::RunRules

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ndml_net::msg::{DailyPick, StartStep};

use crate::names;
use crate::rules::countdown::{Countdown, START_COUNT};

/// The `gameMod*` flags carried: the first 12 of `names::GLOBAL_MODE`, one
/// bit each in that order.
pub const MODE_FLAGS: usize = 12;
/// `gameModDailyChallenge`'s bit.
pub const DAILY_BIT: usize = 1;

/// A run's settings: the modes and, with the daily on, the daily's picks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub mods: u16,
    pub daily: Option<DailyPick>,
}

/// The 12 flags as bits (`None` = missing, off).
#[must_use]
pub fn mode_bits(flags: &[Option<f64>]) -> u16 {
    let mut mods = 0u16;
    for (i, f) in flags.iter().take(MODE_FLAGS).enumerate() {
        if f.is_some_and(|v| v > 0.5) {
            mods |= 1 << i;
        }
    }
    mods
}

impl Rules {
    /// From the flags (`names::GLOBAL_MODE` order) and the new game screen's
    /// daily picks (kept only with the daily on).
    #[must_use]
    pub fn read(flags: &[Option<f64>], daily: Option<DailyPick>) -> Self {
        let mods = mode_bits(flags);
        let daily = daily.filter(|_| mods & (1 << DAILY_BIT) != 0);
        Self { mods, daily }
    }

    /// Whether mode `i` is on.
    #[must_use]
    pub fn on(&self, i: usize) -> bool {
        i < MODE_FLAGS && self.mods & (1 << i) != 0
    }

    /// In player words: the modes' titles, the daily as
    /// `"<Daily Challenge>: <name>"`; "no game modes" when nothing is on.
    #[must_use]
    pub fn describe(&self, title: &dyn Fn(usize) -> String) -> String {
        let mut parts = Vec::new();
        for i in 0..MODE_FLAGS {
            if !self.on(i) {
                continue;
            }
            let t = title(i);
            match (&self.daily, i == DAILY_BIT) {
                (Some(d), true) if !d.name.trim().is_empty() => {
                    parts.push(format!("{t}: {}", d.name.trim()));
                }
                _ => parts.push(t),
            }
        }
        if parts.is_empty() {
            "no game modes".into()
        } else {
            parts.join(", ")
        }
    }
}

/// Cut a daily's name to [`DailyPick::NAME_MAX`] bytes at a character
/// boundary.
#[must_use]
pub fn clip_name(name: &str) -> String {
    let mut end = name.len().min(DailyPick::NAME_MAX);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// This player's own picks, kept while the host's are held: the 12 flags and
/// the new game screen's daily picks (`None` off the new game screen).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Own {
    pub mods: u16,
    pub loadout_daily: Option<DailyPick>,
}

/// What a new game screen frame of the joining game does
/// ([`Joiner::loadout_frame`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// Put these back first (the joining player's own picks).
    pub restore: Option<Own>,
    /// Hold these in the flags and the new game screen's daily picks (the
    /// host's).
    pub show: Option<Rules>,
    /// The bar line.
    pub note: Note,
}

/// The new game screen's bar line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Note {
    #[default]
    None,
    /// The host's picks ([`bar_line`]).
    Picks(Rules),
    /// The host has started and its start is kept here, Start not pressed.
    HostStarted,
    /// Start pressed (either role): waiting for the partner to be ready too.
    Ready,
    /// Both are ready and the start count runs (either role).
    Counting,
    /// The partner is ready, this player not yet (either role).
    PartnerReady,
    /// Resume Run pressed: not while playing together.
    NoResume,
}

/// The line for a host that has started and waits at its start gate.
#[must_use]
pub fn host_waiting(partner: &str) -> String {
    format!("{partner} is Ready.")
}

/// The partner is ready and this player is on its new game screen
/// (`start_word`: what its Start row reads).
#[must_use]
pub fn partner_ready_line(partner: &str, start_word: &str) -> String {
    format!("{partner} is ready. Press {start_word} to start.")
}

/// The partner is ready and this player is elsewhere (the menu, the Co-op
/// page, the end screen, a run).
#[must_use]
pub fn partner_waiting_line(partner: &str) -> String {
    format!("{partner} is ready on the new game screen and waiting for you.")
}

/// The alert when the partner becomes ready (the wave alert's look).
#[must_use]
pub fn partner_ready_alert(partner: &str) -> String {
    format!("{} IS READY", partner.to_uppercase())
}

/// The Co-op page's line for a linked joining game: the host's run in words,
/// or that it has started and waits.
#[must_use]
pub fn page_line(
    host: Option<&Rules>,
    host_started: bool,
    partner: &str,
    describe: &dyn Fn(&Rules) -> String,
) -> Option<String> {
    let r = host?;
    Some(if host_started {
        host_waiting(partner)
    } else {
        format!("{partner}'s run: {}.", describe(r))
    })
}

/// The longest picks line (characters) the new game screen's bar shows in
/// full. A longer one gives the count of modes instead.
pub const PICKS_LINE_MAX: usize = 100;

/// The bar line for `note`. Until the host is on its new game screen
/// (`host_on_loadout`) the picks line says the host is still choosing.
/// `start_word` is what the Start row reads.
#[must_use]
pub fn bar_line(
    note: &Note,
    host_on_loadout: bool,
    partner: &str,
    start_word: &str,
    describe: &dyn Fn(&Rules) -> String,
) -> Option<String> {
    match note {
        Note::None => None,
        Note::Picks(_) if !host_on_loadout => Some(format!("{partner} is configuring lobby...")),
        Note::Picks(r) => {
            let full = format!(
                "{partner} picks the run: {}. Press {start_word} to begin.",
                describe(r)
            );
            if full.chars().count() <= PICKS_LINE_MAX {
                return Some(full);
            }
            let n = (0..MODE_FLAGS).filter(|&i| r.on(i)).count();
            let modes = if n == 1 { "game mode" } else { "game modes" };
            Some(format!(
                "{partner} picks the run: {n} {modes}. Press {start_word} to begin."
            ))
        }
        Note::HostStarted => Some(format!(
            "{} Press {start_word} to join.",
            host_waiting(partner)
        )),
        Note::Ready => Some(format!(
            "Ready. The run starts when {partner} is ready too. Press {start_word} again to cancel."
        )),
        Note::Counting => Some(format!(
            "Both players are ready. Press {start_word} again to cancel."
        )),
        Note::PartnerReady => Some(partner_ready_line(partner, start_word)),
        Note::NoResume => Some(no_resume_line(partner)),
    }
}

/// Flashes of the refused Resume Run's warning in the new game screen's red
/// line (`names::GLOBAL_LOADOUT_RESUME_NOTE`). The last one fades at the
/// game's own rate.
pub const WARN_FLASHES: u32 = 3;
/// Steps (60 a second) of one fast flash.
pub const WARN_FLASH_STEPS: u32 = 20;

/// The `rgbG`/`rgbB` value (0 = full red, 255 = white) to write `n` steps
/// after the refused press, or `None`: the game's own fade goes on.
#[must_use]
pub fn warn_strobe(n: u32) -> Option<f64> {
    let last = WARN_FLASH_STEPS * (WARN_FLASHES - 1);
    match n {
        n if n > last => None,
        n if n == last => Some(0.0),
        n => Some(255.0 * f64::from(n % WARN_FLASH_STEPS) / f64::from(WARN_FLASH_STEPS)),
    }
}

/// The refused Resume Run's sentence, for the red line or the bar.
#[must_use]
pub fn no_resume_line(partner: &str) -> String {
    format!("Resume Run is not available while you play with {partner}.")
}

/// The saved solo run's guard on a new game screen visit whose saved-run
/// question the mod answered (`preanswered`), with a saved run, unlinked.
/// A solo Start is refused. After [`LINK_GONE`] the player is taken back to
/// the main menu, where a solo Game Start asks the game's own question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedRunGuard {
    /// Nothing to guard.
    Off,
    /// The link is out: a solo Start is refused.
    RefuseStart,
    /// The link is gone: back to the menu (and a solo Start is refused).
    BackToMenu,
}

/// How long the link must have been out before the player is taken back.
pub const LINK_GONE: std::time::Duration = crate::wait::WAIT_AFTER;

/// `global.loaded_run_state` as the guard reads it: a saved run when it
/// reads 1 or cannot be read.
#[must_use]
pub fn saved_run_state(v: Option<f64>) -> bool {
    v.is_none_or(|x| (x - 1.0).abs() < 0.5)
}

/// See [`SavedRunGuard`]. `unlinked_for`: how long the link has been out
/// on this screen (`None` while linked).
#[must_use]
pub fn saved_run_guard(
    preanswered: bool,
    saved_run: bool,
    linked: bool,
    unlinked_for: Option<std::time::Duration>,
) -> SavedRunGuard {
    if !(preanswered && saved_run) || linked {
        return SavedRunGuard::Off;
    }
    if unlinked_for.is_some_and(|d| d >= LINK_GONE) {
        SavedRunGuard::BackToMenu
    } else {
        SavedRunGuard::RefuseStart
    }
}

/// The main menu's warning line after [`SavedRunGuard::BackToMenu`].
#[must_use]
pub fn saved_run_kept_line(partner: &str) -> String {
    format!(
        "{} Your saved run is kept.",
        crate::ui::text::lost_link(partner, "")
    )
}

/// The new game screen's two fades (`oLoadout.fade`, `fadeBlack`).
pub type Fades = (Option<f64>, Option<f64>);

/// A Start was taken by the game in a `confirmLoadout` call: one of the
/// screen's fades went to 1 in it (`before`, `after`: around the call).
/// A new run's Start sets `fade`, a resumed run's sets `fadeBlack`.
#[must_use]
pub fn start_began(before: Fades, after: Fades) -> bool {
    let one = |f: Option<f64>| f.is_some_and(|f| (f - 1.0).abs() < 1e-6);
    (one(after.0) && !one(before.0)) || (one(after.1) && !one(before.1))
}

/// What `confirmLoadout` does ([`Joiner::confirm`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// The game's own (not a linked joining game, nothing held).
    Original,
    /// A mode row on a linked joining game: skipped.
    Locked,
    /// Resume Run on a linked game (joining or host): skipped.
    ResumeBlocked,
    /// Start as Ready (either role, linked): skipped, this player is ready.
    Wait,
    /// Ready pressed again: skipped, this player is not ready.
    Unready,
    /// Start with the host's run: these are written, then the game's own
    /// start runs with its settings writes skipped.
    Apply(Rules),
    /// A press during the fade after an applied start: skipped (the game's
    /// own `confirmLoadout` does nothing then either).
    Starting,
}

/// The game functions the rule needs hooked; without any one of them the
/// host's settings are never held ([`missing_hooks`]).
pub const NEEDED_HOOKS: [&str; 10] = [
    names::CONFIRM_LOADOUT,
    names::INI_WRITE_BOOL,
    names::RESET_PROGRESS,
    names::RESET_DAILY,
    names::RESUME_RUN,
    names::LOADOUT_STEP,
    names::MENU_STEP,
    names::CONTROLLER_STEP,
    names::TRIGGER_SAVE_RUN,
    names::DELETE_RUN_FILES,
];

/// The needed hooks among `failed` (every hook the mod could not register).
#[must_use]
pub fn missing_hooks<'a>(needed: &[&'a str], failed: &[String]) -> Vec<&'a str> {
    needed
        .iter()
        .copied()
        .filter(|n| failed.iter().any(|f| f == n))
        .collect()
}

/// Why no run can be a co-op run in this game (`None`: they can). Needs
/// `confirmLoadout` or `resumeRun`, and both `triggerSaveRun` and
/// `deleteRunFiles`.
#[must_use]
pub fn run_safety(failed: &[String]) -> Option<String> {
    let lost = |n: &str| failed.iter().any(|f| f == n);
    let mut why = Vec::new();
    if lost(names::CONFIRM_LOADOUT) && lost(names::RESUME_RUN) {
        why.push(format!(
            "neither {} nor {} could be hooked: a resumed saved run could not be told from a new one",
            names::CONFIRM_LOADOUT,
            names::RESUME_RUN
        ));
    }
    for n in [names::TRIGGER_SAVE_RUN, names::DELETE_RUN_FILES] {
        if lost(n) {
            why.push(format!(
                "{n} could not be hooked: a co-op run could write or delete the saved run"
            ));
        }
    }
    (!why.is_empty()).then(|| why.join("; "))
}

/// Why the rule cannot be on in this game: hooks that did not register and
/// game names (`handles`: name, found) that were not found. `None`: all there.
#[must_use]
pub fn rule_off_because(missing_hooks: &[&str], handles: &[(&str, bool)]) -> Option<String> {
    let not_found: Vec<&str> = handles
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| *n)
        .collect();
    let mut why = Vec::new();
    if !missing_hooks.is_empty() {
        why.push(format!("not hooked: {}", missing_hooks.join(", ")));
    }
    if !not_found.is_empty() {
        why.push(format!("not found: {}", not_found.join(", ")));
    }
    (!why.is_empty()).then(|| why.join("; "))
}

/// How long the host waits after its go for the joining game's answer, or
/// for its own start to be taken, before it calls the count off.
pub const GO_WAIT: Duration = Duration::from_secs(5);

/// The run's start count: with both players ready, both games count 3, 2, 1
/// ([`START_COUNT`]) on the new game screen and the run starts at go. A Ready
/// taken back, the screen left or the link gone calls it off on both.
///
/// No order of messages starts one game alone:
/// * the host begins at [`Joiner::go_due`] (`Begin`) and sends `Go` at the end
///   of its count;
/// * the joining game takes the `Go` only while it is ready and counting that
///   round, answers `Ack`, and takes no Ready back from then on;
/// * the host starts only on that `Ack` ([`Joiner::press_due`]), the joining
///   game only on the host's `RunRules` start.
///
/// The stream is reliable and ordered, so a take-back sent before the `Go`
/// arrives reaches the host before any `Ack`. Without an answer within
/// [`GO_WAIT`] the host calls the count off. Every message names its round:
/// a late one changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum StartCount {
    #[default]
    Idle,
    /// Host: counting `round`.
    Counting { round: u16, count: Countdown },
    /// Host: its count is over and its `Go` went out at `since`.
    GoSent { round: u16, since: Instant },
    /// Host: the joining game took the go (at `since`): the start is pressed.
    Acked { round: u16, since: Instant },
    /// Joining game: counting the host's `round`.
    Following { round: u16, count: Countdown },
    /// Joining game: the host's go taken. Its start follows the host's.
    Committed { round: u16, count: Countdown },
}

impl StartCount {
    /// The host's state (it sends `Cancel` when called off).
    fn host_round(self) -> Option<u16> {
        match self {
            Self::Counting { round, .. }
            | Self::GoSent { round, .. }
            | Self::Acked { round, .. } => Some(round),
            _ => None,
        }
    }
}

/// The joining game's side of the rule, and the host's last sent picks.
/// Pure; the game side is `loadout_glue`.
#[allow(clippy::struct_excessive_bools)] // independent states of the screen, each read alone
#[derive(Debug, Clone, Default)]
pub struct Joiner {
    /// Every hook the rule needs is live ([`Self::enable`]); nothing is held
    /// while off.
    enabled: bool,
    /// An applied start is under way: the host's start stays held until the
    /// menu, whatever the link does.
    starting: bool,
    /// The host's current picks (any `RunRules`).
    host: Option<Rules>,
    /// The host's last `RunRules` came from its new game screen.
    host_on_loadout: bool,
    /// The host started a run with these, not taken yet.
    host_start: Option<Rules>,
    /// This player is Ready on the new game screen (either role, linked).
    /// See [`Self::go_due`].
    ready: bool,
    /// The partner said it is Ready (`Message::Ready`), until it says it is
    /// not or the link changes.
    partner_ready: bool,
    /// Host: the start both Readies made is being pressed ([`Self::begin_go`]).
    going: bool,
    /// The last bar line was about Resume Run (until the next press).
    no_resume: bool,
    /// The joining player's own picks while the host's may be in the flags,
    /// kept until written back ([`Self::restored`]).
    own: Option<Own>,
    /// What is held in the flags now.
    held: Option<Rules>,
    /// The host's start is being applied: `resetDailyChallenges` gets these
    /// back, `ini_write_bool` is skipped.
    applying: Option<Rules>,
    /// The start count ([`StartCount`]).
    start: StartCount,
    /// Host: the last count's round.
    round: u16,
    /// Its messages to send, in order (`Message::StartCount`).
    start_outs: Vec<(u16, StartStep)>,
    /// Joining game: the link went after this game took the host's go
    /// (`Committed`) and before its start arrived. The host's start, sent
    /// again at the re-link, is then taken without a press ([`Self::start_due`]).
    /// Spent by that start, a run of its own, a new count, or the host's picks
    /// without a start.
    word_given: bool,
}

impl Joiner {
    /// A joiner whose hooks are all live (tests).
    #[must_use]
    pub fn on() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Every hook the rule needs registered: the rule may hold the host's
    /// settings from now on.
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// The rule is on in this game.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The host's `RunRules` (ignored while the rule is off).
    pub fn on_rules(&mut self, start: bool, rules: Rules) {
        if !self.enabled {
            return;
        }
        if !start && std::mem::take(&mut self.word_given) {
            log::info!(
                "co-op: the host's picks came without its start: the start both agreed to is spent (its run is over)"
            );
        }
        self.host_start = start.then(|| rules.clone());
        self.host = Some(rules);
    }

    /// Whether the host is on its new game screen; only the bar line reads it.
    pub fn host_screen(&mut self, on_loadout: bool) {
        if self.enabled {
            self.host_on_loadout = on_loadout;
        }
    }

    /// The host's last `RunRules` came from its new game screen.
    #[must_use]
    pub fn host_on_loadout(&self) -> bool {
        self.host_on_loadout
    }

    /// The host (or the link) is gone: what it sent is forgotten. The joining
    /// player's own picks stay kept until the next restore point.
    pub fn host_gone(&mut self) {
        self.host = None;
        self.host_on_loadout = false;
        self.host_start = None;
        self.no_resume = false;
        self.link_changed();
    }

    /// The link came or went (either role): both players start unready.
    pub fn link_changed(&mut self) {
        self.ready = false;
        self.partner_ready = false;
        self.going = false;
        if matches!(self.start, StartCount::Committed { .. }) {
            // This player's word was given and the host may have started:
            // its start is taken when it comes again (`word_given`).
            self.word_given = true;
            log::info!(
                "co-op: the link changed after this game took the host's go: the host's start is taken if it comes again"
            );
        } else if self.start != StartCount::Idle {
            log::info!("co-op: the start count stops: the link changed; nobody starts");
        }
        // Nothing to tell: the partner sees the same link change.
        self.start = StartCount::Idle;
        self.start_outs.clear();
    }

    /// The partner's `Ready { ready }`: `true` when it has just become ready.
    /// Ignored with the rule off. A Ready taken back calls the count off.
    pub fn partner(&mut self, ready: bool) -> bool {
        if !self.enabled {
            return false;
        }
        let news = ready && !self.partner_ready;
        self.partner_ready = ready;
        if !ready
            && matches!(
                self.start,
                StartCount::Counting { .. } | StartCount::GoSent { .. }
            )
        {
            self.call_off("the partner took Ready back");
        }
        news
    }

    /// The partner is Ready on its new game screen.
    #[must_use]
    pub fn partner_ready(&self) -> bool {
        self.partner_ready
    }

    /// Both are ready: the host begins the start count ([`Self::host_frame`]).
    /// The only place a linked run's start begins.
    #[must_use]
    pub fn go_due(&self) -> bool {
        self.enabled && self.ready && self.partner_ready && !self.starting && !self.going
    }

    /// Host: the joining game took the count's go: make the start
    /// (`loadout_glue::host_go`).
    #[must_use]
    pub fn press_due(&self) -> bool {
        self.enabled
            && matches!(self.start, StartCount::Acked { .. })
            && !self.starting
            && !self.going
    }

    /// Host: the start [`Self::press_due`] asked for is being pressed.
    pub fn begin_go(&mut self) {
        self.going = true;
    }

    /// Host: that press returned. `taken`: the game began its start.
    pub fn go_done(&mut self, taken: bool) {
        self.going = false;
        if taken {
            self.started_own();
        }
    }

    /// Host, each frame of its new game screen: the start count's next move.
    pub fn host_frame(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        let both = self.ready && self.partner_ready && !self.starting;
        match self.start {
            StartCount::Idle => {
                if self.go_due() {
                    self.round = self.round.wrapping_add(1);
                    let round = self.round;
                    self.start = StartCount::Counting {
                        round,
                        count: Countdown::new(now, START_COUNT, Duration::ZERO),
                    };
                    self.start_outs.push((round, StartStep::Begin));
                    log::info!(
                        "co-op: both players are ready: start count {round} begins ({} s)",
                        START_COUNT.as_secs()
                    );
                }
            }
            StartCount::Counting { round, count } => {
                if !both {
                    self.call_off("a Ready is gone");
                } else if count.left(now).is_zero() {
                    self.start = StartCount::GoSent { round, since: now };
                    self.start_outs.push((round, StartStep::Go));
                    log::info!(
                        "co-op: start count {round}: go; the run starts once the joining game takes it"
                    );
                }
            }
            StartCount::GoSent { since, .. } => {
                if !both {
                    self.call_off("a Ready is gone");
                } else if now.saturating_duration_since(since) >= GO_WAIT {
                    self.call_off("no answer to the go");
                }
            }
            StartCount::Acked { since, .. } => {
                if !self.going && now.saturating_duration_since(since) >= GO_WAIT {
                    self.call_off("the game did not take the start");
                }
            }
            // A joining game's count on a host (not reached: a role
            // changes only with the link).
            StartCount::Following { .. } | StartCount::Committed { .. } => {
                self.start = StartCount::Idle;
            }
        }
    }

    /// The count stops here; the host tells the joining game (`Cancel`).
    fn call_off(&mut self, why: &str) {
        match self.start {
            StartCount::Idle => return,
            s => {
                if let Some(round) = s.host_round() {
                    self.start_outs.push((round, StartStep::Cancel));
                    log::info!("co-op: start count {round} called off ({why}): nobody starts");
                } else {
                    log::info!("co-op: the host's start count stops here ({why})");
                }
            }
        }
        self.start = StartCount::Idle;
    }

    /// A `Message::StartCount` (`host_here`: this game is the host;
    /// `from_host`: the host sent it). The host takes only the joining game's
    /// `Ack`, the joining game only the host's `Begin`, `Go` and `Cancel`.
    /// Anything of another round changes nothing.
    pub fn start_msg(
        &mut self,
        host_here: bool,
        from_host: bool,
        round: u16,
        step: StartStep,
        now: Instant,
    ) {
        if !self.enabled {
            return;
        }
        match (host_here, from_host, step, self.start) {
            (true, false, StartStep::Ack, StartCount::GoSent { round: r, .. }) if r == round => {
                self.start = StartCount::Acked { round, since: now };
                log::info!("co-op: start count {round}: the joining game took the go");
            }
            (false, true, StartStep::Begin, _) => {
                self.word_given = false;
                if self.ready && !self.starting {
                    self.start = StartCount::Following {
                        round,
                        count: Countdown::new(now, START_COUNT, Duration::ZERO),
                    };
                    log::info!("co-op: the host's start count {round} begins here");
                } else {
                    // This player's taken-back Ready calls the count off at the host.
                    self.start = StartCount::Idle;
                    log::info!(
                        "co-op: the host's start count {round} not followed (not ready here)"
                    );
                }
            }
            (false, true, StartStep::Go, StartCount::Following { round: r, count })
                if r == round && self.ready =>
            {
                self.start = StartCount::Committed { round, count };
                self.start_outs.push((round, StartStep::Ack));
                log::info!(
                    "co-op: the host's start count {round}: go taken; this game follows the host's start"
                );
            }
            (
                false,
                true,
                StartStep::Cancel,
                StartCount::Following { round: r, .. } | StartCount::Committed { round: r, .. },
            ) if r == round => {
                self.start = StartCount::Idle;
                log::info!("co-op: the host called its start count {round} off: nobody starts");
            }
            _ => {
                log::info!(
                    "co-op: start count {round}: {step:?} not taken here (late or not ours)"
                );
            }
        }
    }

    /// The start count showing here (either role): its round and count.
    #[must_use]
    pub fn start_count(&self) -> Option<(u16, Countdown)> {
        match self.start {
            StartCount::Counting { round, count }
            | StartCount::Following { round, count }
            | StartCount::Committed { round, count } => Some((round, count)),
            _ => None,
        }
    }

    /// The glue's steps of that count (`Countdown::step`), kept.
    pub fn set_start_count(&mut self, round: u16, c: Countdown) {
        if let StartCount::Counting { round: r, count }
        | StartCount::Following { round: r, count }
        | StartCount::Committed { round: r, count } = &mut self.start
            && *r == round
        {
            *count = c;
        }
    }

    /// After the count's go and before the run: Ready and back do nothing.
    #[must_use]
    pub fn start_locked(&self) -> bool {
        matches!(
            self.start,
            StartCount::GoSent { .. } | StartCount::Acked { .. } | StartCount::Committed { .. }
        )
    }

    /// The start count's messages to send, in order.
    pub fn take_start_outs(&mut self) -> Vec<(u16, StartStep)> {
        std::mem::take(&mut self.start_outs)
    }

    /// The count's bar line, while it runs or after its go.
    fn count_note(&self) -> Option<Note> {
        match self.start {
            StartCount::Idle => None,
            StartCount::Counting { .. } | StartCount::Following { .. } => Some(Note::Counting),
            _ => Some(Note::None),
        }
    }

    /// The host has sent its picks in this link.
    #[must_use]
    pub fn shares(&self) -> bool {
        self.host.is_some()
    }

    /// The host's settings may be in the flags (the player's own are kept
    /// here). The resume file is not written while this holds.
    #[must_use]
    pub fn held(&self) -> bool {
        self.own.is_some()
    }

    /// What the flags hold now, if the host's.
    #[must_use]
    pub fn held_rules(&self) -> Option<&Rules> {
        self.held.as_ref()
    }

    /// The host's current picks (for the Co-op page).
    #[must_use]
    pub fn host_picks(&self) -> Option<&Rules> {
        self.host.as_ref()
    }

    /// The host has started a run that waits for the joining player.
    #[must_use]
    pub fn host_started(&self) -> bool {
        self.host_start.is_some()
    }

    /// This player is Ready and waiting.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.ready
    }

    /// The rules being applied by its own start call.
    #[must_use]
    pub fn applying(&self) -> Option<&Rules> {
        self.applying.as_ref()
    }

    /// `confirmLoadout`'s settings writes are skipped now.
    #[must_use]
    pub fn skip_writes(&self) -> bool {
        self.applying.is_some()
    }

    /// The game's own `confirmLoadout` may run now: nothing of the host's is
    /// held and no locked screen is up (the fallback when the hook fails).
    #[must_use]
    pub fn original_safe(&self, linked: bool) -> bool {
        !(self.held() || (self.enabled && linked && self.shares()))
    }

    /// The joining player's new game screen shows the host's picks, read only.
    #[must_use]
    pub fn locked(&self, linked_joiner: bool) -> bool {
        self.enabled && linked_joiner && self.shares() && !self.starting
    }

    fn hold(&mut self, own_now: impl FnOnce() -> Own, rules: &Rules) {
        if self.own.is_none() {
            self.own = Some(own_now());
        }
        self.held = Some(rules.clone());
    }

    /// The player's own picks to write back (kept until [`Self::restored`]).
    pub fn release(&mut self) -> Option<Own> {
        self.ready = false;
        self.no_resume = false;
        self.starting = false;
        self.call_off("this player is not ready any more");
        self.own.clone()
    }

    /// The player's own picks were written back: nothing is held now.
    pub fn restored(&mut self) {
        self.own = None;
        self.held = None;
    }

    /// Any game frame: clears a write skip left by a `confirmLoadout` that
    /// never returned. `true` = one was cleared.
    pub fn frame_tick(&mut self) -> bool {
        self.applying.take().is_some()
    }

    /// A frame of the new game screen. `linked_joiner`: linked, and not the
    /// host. `own_now` reads the player's picks (called only when taken).
    pub fn loadout_frame(&mut self, linked_joiner: bool, own_now: impl FnOnce() -> Own) -> Frame {
        if self.starting {
            // The fade after an applied start: nothing changes until the menu.
            return Frame::default();
        }
        if !(linked_joiner && self.enabled) {
            self.ready = false;
            self.no_resume = false;
            return Frame {
                restore: self.release(),
                ..Frame::default()
            };
        }
        let Some(host) = self.host.clone() else {
            // The host's picks not here yet: the game's own screen.
            return Frame::default();
        };
        self.hold(own_now, &host);
        let note = if self.no_resume {
            Note::NoResume
        } else {
            self.note_past_resume()
        };
        Frame {
            restore: None,
            show: Some(host),
            note,
        }
    }

    /// The joining player's bar line without a refused Resume Run: the count,
    /// ready, the host started, the partner ready, or the host's picks.
    #[must_use]
    pub fn note_past_resume(&self) -> Note {
        if let Some(n) = self.count_note() {
            n
        } else if self.ready {
            Note::Ready
        } else if self.host_start.is_some() {
            Note::HostStarted
        } else if self.partner_ready {
            Note::PartnerReady
        } else {
            self.host.clone().map_or(Note::None, Note::Picks)
        }
    }

    /// A main menu frame: the player's own picks come back. The menu writes
    /// no flag, so holding again on the new game screen is safe.
    pub fn menu_frame(&mut self) -> Option<Own> {
        self.release()
    }

    /// `confirmLoadout` on a linked host's new game screen: a Start that
    /// resumes a saved run is refused, and Start toggles Ready. The press
    /// [`Self::begin_go`] makes, a mode row, and every press with the rule off
    /// are the game's own.
    pub fn host_confirm(&mut self, pos0: bool, resumed: bool) -> Confirm {
        // Never a linked Resume Run, the rule on or off.
        self.no_resume = pos0 && resumed;
        if self.no_resume {
            return Confirm::ResumeBlocked;
        }
        if self.going {
            return Confirm::Original;
        }
        if !(self.enabled && pos0) {
            // The run's settings stay locked while the start count runs
            // and after its go.
            return if self.enabled && self.start != StartCount::Idle {
                Confirm::Locked
            } else {
                Confirm::Original
            };
        }
        if self.starting || self.start_locked() {
            return Confirm::Starting;
        }
        self.press_ready()
    }

    /// Start as Ready: toggles Ready. Taken back, it calls a start count off.
    fn press_ready(&mut self) -> Confirm {
        self.ready = !self.ready;
        if self.ready {
            Confirm::Wait
        } else {
            self.call_off("Ready taken back");
            Confirm::Unready
        }
    }

    /// A linked host's bar line: [`Note::NoResume`] after a refused Resume Run,
    /// else the Ready line.
    #[must_use]
    pub fn host_note(&self) -> Note {
        if !self.enabled {
            Note::None
        } else if self.no_resume {
            Note::NoResume
        } else {
            self.ready_note().unwrap_or(Note::None)
        }
    }

    /// Either role's Ready line: the start count's, this player's, else the
    /// partner's.
    fn ready_note(&self) -> Option<Note> {
        if let Some(n) = self.count_note() {
            Some(n)
        } else if self.ready {
            Some(Note::Ready)
        } else if self.partner_ready {
            Some(Note::PartnerReady)
        } else {
            None
        }
    }

    /// `confirmLoadout` was called. `pos0`: the Start row; `resumed`: the
    /// Start resumes a saved run. The glue writes `restore` first and skips
    /// the press if that fails.
    pub fn confirm(
        &mut self,
        linked_joiner: bool,
        pos0: bool,
        resumed: bool,
        own_now: impl FnOnce() -> Own,
    ) -> (Option<Own>, Confirm) {
        if self.starting {
            return (None, Confirm::Starting);
        }
        if !(linked_joiner && self.enabled && self.shares()) {
            // The game's own press; anything held goes back first. A linked
            // Resume Run is refused all the same.
            let restore = self.release();
            if linked_joiner && pos0 && resumed {
                self.no_resume = true;
                return (restore, Confirm::ResumeBlocked);
            }
            return (restore, Confirm::Original);
        }
        self.no_resume = false;
        if !pos0 {
            return (None, Confirm::Locked);
        }
        if resumed {
            self.no_resume = true;
            return (None, Confirm::ResumeBlocked);
        }
        // The host has started: this press applies its start. Else Start
        // toggles Ready.
        let Some(r) = self.host_start.take() else {
            // The host's go taken: the run follows.
            if self.start_locked() {
                return (None, Confirm::Starting);
            }
            return (None, self.press_ready());
        };
        self.hold(own_now, &r);
        self.ready = false;
        self.partner_ready = false;
        self.word_given = false;
        self.starting = true;
        self.start = StartCount::Idle;
        self.applying = Some(r.clone());
        (None, Confirm::Apply(r))
    }

    /// The game's own Start began its fade: until the menu nothing is held or
    /// given back. Only with the rule on, so a solo player's presses are never
    /// latched.
    pub fn started_own(&mut self) {
        if self.enabled {
            self.starting = true;
        }
        // A run start spends both Readies (and its count).
        self.ready = false;
        self.partner_ready = false;
        self.word_given = false;
        self.start = StartCount::Idle;
    }

    /// The game did not take an applied start (its fade did not begin): the
    /// host's start is kept again for the next Start and the latch let go.
    pub fn start_not_taken(&mut self) {
        if let Some(r) = self.applying.take() {
            if self.host_start.is_none() {
                self.host_start = Some(r);
            }
            self.starting = false;
        }
    }

    /// `confirmLoadout` returned.
    pub fn confirm_done(&mut self) {
        self.applying = None;
    }

    /// The host has started and this player is Ready, or gave its word before
    /// a link change (`word_given`): make the start through `confirmLoadout`.
    #[must_use]
    pub fn start_due(&self) -> bool {
        (self.ready || self.word_given) && self.host_start.is_some() && !self.starting
    }

    /// Back from the new game screen: nothing waits, a start count is called off.
    pub fn cancel(&mut self) {
        self.ready = false;
        self.no_resume = false;
        self.call_off("the new game screen left");
    }

    /// `resetProgress` ran and left `mods` in the flags: while held, those
    /// become the player's own picks, and what was held is to be written back.
    pub fn reset_progress(&mut self, mods: u16) -> Option<Rules> {
        let own = self.own.as_mut()?;
        own.mods = mods;
        self.held.clone()
    }
}

/// The host's side: what it last sent, so it sends on change. Nothing between
/// its start and its next menu frame: a `start` false then would take back a
/// start the joining game has not used yet.
#[derive(Debug, Clone, Default)]
pub struct HostSent {
    /// The picks and whether they came from the new game screen.
    last: Option<(Rules, bool)>,
    started: bool,
    /// The start of the run under way (until the menu), sent or not
    /// ([`Self::start_again`]).
    start: Option<Rules>,
}

impl HostSent {
    /// Its picks this frame (`on_loadout`: the new game screen, else the
    /// menu): `Some` = send them. A move between the two sends too.
    pub fn picks(&mut self, now: Rules, on_loadout: bool) -> Option<Rules> {
        let now = (now, on_loadout);
        if self.started || self.last.as_ref() == Some(&now) {
            return None;
        }
        self.last = Some(now.clone());
        Some(now.0)
    }

    /// Its run started with `rules` (`start` true sent if linked).
    pub fn started(&mut self, rules: &Rules) {
        self.started = true;
        self.start = Some(rules.clone());
    }

    /// A re-link while its run waits for the partner at its start gate
    /// (`waiting`): that run's start, to send again.
    #[must_use]
    pub fn start_again(&self, waiting: bool) -> Option<&Rules> {
        self.start.as_ref().filter(|_| waiting)
    }

    /// A main menu frame after a run: its picks go out again (telling the
    /// joining game the start is spent).
    pub fn menu(&mut self) {
        self.start = None;
        if std::mem::take(&mut self.started) {
            self.last = None;
        }
    }

    /// Send again at the next frame (a new link).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Picks a test says must not be read: a read shows up as these.
    fn sentinel() -> Own {
        Own {
            mods: 0xF000,
            loadout_daily: None,
        }
    }

    fn pick(choice: i16) -> DailyPick {
        DailyPick {
            choice,
            modifier: 2,
            modifier2: 5,
            name: format!("Daily {choice}"),
        }
    }

    fn flags(on: &[usize]) -> Vec<Option<f64>> {
        (0..15)
            .map(|i| Some(if on.contains(&i) { 1.0 } else { 0.0 }))
            .collect()
    }

    fn rules(mods: u16) -> Rules {
        Rules { mods, daily: None }
    }

    #[test]
    fn rules_read_the_flags_and_keep_the_daily_only_with_it_on() {
        let r = Rules::read(&flags(&[4, 9]), Some(pick(3)));
        assert_eq!(r.mods, (1 << 4) | (1 << 9));
        assert_eq!(
            r.daily, None,
            "no daily flag: the new game screen's picks are not this run's"
        );
        let r = Rules::read(&flags(&[DAILY_BIT]), Some(pick(3)));
        assert_eq!(r.daily, Some(pick(3)));
        // The base difficulty factors after the 12 flags are not modes.
        let r = Rules::read(&flags(&[12, 13, 14]), None);
        assert_eq!(r.mods, 0);
        assert_eq!(mode_bits(&[None, Some(1.0), Some(0.2)]), 0b10);
    }

    #[test]
    fn described_in_player_words() {
        let title = |i: usize| format!("Mode{i}");
        assert_eq!(Rules::default().describe(&title), "no game modes");
        let r = Rules::read(&flags(&[DAILY_BIT, 4]), Some(pick(3)));
        assert_eq!(r.describe(&title), "Mode1: Daily 3, Mode4");
    }

    #[test]
    fn a_name_is_cut_at_a_character_boundary() {
        let long = "é".repeat(40);
        let c = clip_name(&long);
        assert!(c.len() <= DailyPick::NAME_MAX && long.starts_with(&c));
    }

    #[test]
    fn the_host_sends_on_change_and_again_after_a_reset() {
        let mut h = HostSent::default();
        assert_eq!(h.picks(rules(1), false), Some(rules(1)));
        assert_eq!(h.picks(rules(1), false), None);
        assert_eq!(
            h.picks(rules(1), true),
            Some(rules(1)),
            "on its new game screen"
        );
        assert_eq!(h.picks(rules(1), true), None);
        assert_eq!(h.picks(rules(3), true), Some(rules(3)));
        h.reset();
        assert_eq!(h.picks(rules(3), true), Some(rules(3)));
        // From its start to its next menu frame: nothing.
        h.started(&rules(3));
        h.reset();
        assert_eq!(h.picks(rules(3), true), None);
        assert_eq!(h.picks(rules(5), true), None);
        h.menu();
        assert_eq!(h.picks(rules(5), false), Some(rules(5)), "the run is over");
        h.menu();
        assert_eq!(h.picks(rules(5), false), None, "once");
        assert_eq!(
            h.picks(rules(5), true),
            Some(rules(5)),
            "back on its new game screen"
        );
    }

    #[test]
    fn a_long_picks_line_gives_the_count() {
        let name = "Sixteen Chars Nm"; // the longest name drawn (`NAME_MAX`)
        assert_eq!(name.chars().count(), crate::ui::text::NAME_MAX);
        let title = |i: usize| {
            ["DANGER ZONE", "MAYHEM", "ELITE ENEMY", "HOSTILE UNIVERSE"][i % 4].to_owned()
        };
        let describe = |r: &Rules| r.describe(&title);
        let bar = |mods: u16| {
            bar_line(&Note::Picks(rules(mods)), true, name, "Ready", &describe).unwrap()
        };
        // Two modes: the list, within the limit.
        let two = bar(0b11);
        assert!(two.chars().count() <= PICKS_LINE_MAX);
        assert_eq!(
            two,
            format!("{name} picks the run: DANGER ZONE, MAYHEM. Press Ready to begin.")
        );
        // Four: past it, the count.
        let full = format!(
            "{name} picks the run: {}. Press Ready to begin.",
            describe(&rules(0b1111))
        );
        assert!(full.chars().count() > PICKS_LINE_MAX);
        assert_eq!(
            bar(0b1111),
            format!("{name} picks the run: 4 game modes. Press Ready to begin.")
        );
        // The Co-op page keeps the list.
        assert_eq!(
            page_line(Some(&rules(0b1111)), false, name, &describe),
            Some(format!("{name}'s run: {}.", describe(&rules(0b1111))))
        );
    }

    #[test]
    fn the_joining_bar_says_the_host_is_choosing_until_its_new_game_screen_speaks() {
        let describe = |r: &Rules| r.describe(&|i| format!("Mode{i}"));
        let bar = |j: &Joiner, note: &Note| {
            bar_line(note, j.host_on_loadout(), "Sam", "Ready", &describe)
        };
        let mut j = Joiner::on();
        // The host still in its menu.
        j.host_screen(false);
        j.on_rules(false, rules(0b10));
        assert_eq!(
            bar(&j, &Note::Picks(rules(0b10))).as_deref(),
            Some("Sam is configuring lobby...")
        );
        // On its new game screen, nothing picked.
        j.host_screen(true);
        j.on_rules(false, rules(0));
        assert_eq!(
            bar(&j, &Note::Picks(rules(0))).as_deref(),
            Some("Sam picks the run: no game modes. Press Ready to begin.")
        );
        // On it, with modes.
        j.on_rules(false, rules(0b110));
        assert_eq!(
            bar(&j, &Note::Picks(rules(0b110))).as_deref(),
            Some("Sam picks the run: Mode1, Mode2. Press Ready to begin.")
        );
        // Only the sentence: the lock and the after-press line do not read it.
        j.host_screen(false);
        assert!(j.locked(true));
        assert_eq!(
            bar(&j, &Note::Ready).as_deref(),
            Some("Ready. The run starts when Sam is ready too. Press Ready again to cancel.")
        );
        assert_eq!(bar(&j, &Note::None), None);
        // The host started before the joining player pressed: its start is
        // kept, the bar and the page say it waits.
        let page = |j: &Joiner| page_line(j.host_picks(), j.host_started(), "Sam", &describe);
        j.host_screen(true);
        assert_eq!(page(&j).as_deref(), Some("Sam's run: Mode1, Mode2."));
        j.on_rules(true, rules(0b110));
        let f = j.loadout_frame(true, Own::default);
        assert_eq!(
            (f.note.clone(), f.show),
            (Note::HostStarted, Some(rules(0b110)))
        );
        assert_eq!(
            bar(&j, &f.note).as_deref(),
            Some("Sam is Ready. Press Ready to join.")
        );
        assert_eq!(page(&j).as_deref(), Some("Sam is Ready."));
        let _ = j.menu_frame();
        j.restored();
        assert!(j.host_started(), "kept through the joining menu");
        // The host left its gate: its picks again without a start.
        j.on_rules(false, rules(0b110));
        assert_eq!(
            j.loadout_frame(true, Own::default).note,
            Note::Picks(rules(0b110))
        );
        assert_eq!(page(&j).as_deref(), Some("Sam's run: Mode1, Mode2."));
        // Started again, and the press takes it.
        j.on_rules(true, rules(0b110));
        let (_, c) = j.confirm(true, true, false, Own::default);
        assert_eq!(c, Confirm::Apply(rules(0b110)));
        assert!(!j.host_started());
        assert_eq!(
            bar(&j, &Note::HostStarted)
                .as_deref()
                .map(|s| s.contains("to join")),
            Some(true)
        );
        assert_eq!(
            page_line(None, true, "Sam", &describe),
            None,
            "no host here"
        );
        let _ = j.menu_frame();
        j.restored();
        j.host_screen(true);
        j.host_gone();
        assert!(!j.host_on_loadout(), "forgotten with the host");
        let mut off = Joiner::default();
        off.host_screen(true);
        assert!(!off.host_on_loadout(), "the rule off");
    }

    /// A linked pair's two games, each with the rule on; the joining one
    /// has the host's picks.
    fn pair() -> (Joiner, Joiner) {
        let mut joiner = Joiner::on();
        joiner.on_rules(false, rules(0b10));
        (Joiner::on(), joiner)
    }

    /// The host's go press (`loadout_glue::host_go`), taken by the game.
    fn host_presses(host: &mut Joiner) {
        assert!(host.press_due());
        host.begin_go();
        assert_eq!(
            host.host_confirm(true, false),
            Confirm::Original,
            "the go press is the game's own"
        );
        host.go_done(true);
    }

    /// Both ready: the start count on both, its go, the joining game's
    /// answer, and the host's start (each message handed over at once).
    /// Returns the round.
    fn count_and_go(host: &mut Joiner, joiner: &mut Joiner, t: Instant) -> u16 {
        assert!(host.go_due());
        host.host_frame(t);
        let begin = host.take_start_outs();
        let [(round, StartStep::Begin)] = begin[..] else {
            panic!("{begin:?}")
        };
        joiner.start_msg(false, true, round, StartStep::Begin, t);
        assert!(host.start_count().is_some() && joiner.start_count().is_some());
        assert_eq!(host.host_note(), Note::Counting);
        assert_eq!(joiner.note_past_resume(), Note::Counting);
        assert!(!host.press_due(), "not before the count's end");
        host.host_frame(t + START_COUNT / 2);
        assert!(host.take_start_outs().is_empty(), "counting");
        host.host_frame(t + START_COUNT);
        assert_eq!(host.take_start_outs(), [(round, StartStep::Go)]);
        assert!(
            host.start_locked() && !host.press_due(),
            "waits for the joining game"
        );
        joiner.start_msg(false, true, round, StartStep::Go, t + START_COUNT);
        assert_eq!(joiner.take_start_outs(), [(round, StartStep::Ack)]);
        assert!(joiner.start_locked());
        host.start_msg(true, false, round, StartStep::Ack, t + START_COUNT);
        host_presses(host);
        round
    }

    /// The link goes after the joining game's `Ack` and before the host's start
    /// reaches it. The start said again at the re-link is taken with no press.
    /// Picks without a start leave the joining player out of that run.
    #[test]
    fn a_start_lost_with_the_link_is_taken_when_said_again() {
        let t = Instant::now();
        for host_still_in_its_run in [true, false] {
            let (mut host, mut joiner) = pair();
            let _ = host.host_confirm(true, false);
            let _ = joiner.confirm(true, true, false, sentinel);
            host.partner(true);
            joiner.partner(true);
            let _ = count_and_go(&mut host, &mut joiner, t);
            let mut sent = HostSent::default();
            sent.started(&rules(0b10));
            // The link goes: the host's start never came; both unready.
            joiner.link_changed();
            host.link_changed();
            assert!(!joiner.ready() && !joiner.start_due());
            if host_still_in_its_run {
                let again = sent.start_again(true).cloned().expect("said again");
                joiner.on_rules(true, again);
                assert!(joiner.start_due(), "taken with no press");
                let (_, c) = joiner.confirm(true, true, false, Own::default);
                assert!(matches!(c, Confirm::Apply(_)), "{c:?}");
            } else {
                sent.menu();
                assert_eq!(sent.start_again(true), None, "that run is over");
                joiner.on_rules(false, rules(0b10));
                assert!(!joiner.start_due(), "the joining player stays out");
                // A later start (a new count, both ready again) needs a press.
                joiner.on_rules(true, rules(0b10));
                assert!(
                    !joiner.start_due(),
                    "not without the joining player's Ready"
                );
            }
        }
        // Not at its gate (a run in play), nothing is said again.
        let mut sent = HostSent::default();
        sent.started(&rules(1));
        assert_eq!(sent.start_again(false), None);
    }

    /// The host Ready first, then the joining player; the run starts only
    /// then, made by the host, followed by the joining game.
    #[test]
    fn host_ready_first_then_joiner_starts_both() {
        let (mut host, mut joiner) = pair();
        assert_eq!(host.host_confirm(true, false), Confirm::Wait);
        assert!(
            host.ready() && !host.go_due(),
            "the host waits on its screen"
        );
        assert_eq!(host.host_note(), Note::Ready);
        assert!(joiner.partner(true), "news: the notice and its cue");
        assert!(!joiner.partner(true), "said again: no second notice");
        assert_eq!(joiner.note_past_resume(), Note::PartnerReady);
        assert_eq!(joiner.confirm(true, true, false, sentinel).1, Confirm::Wait);
        assert!(joiner.ready() && !joiner.start_due());
        assert!(host.partner(true));
        count_and_go(&mut host, &mut joiner, Instant::now());
        assert!(
            !host.ready() && !host.partner_ready() && !host.go_due(),
            "spent"
        );
        assert_eq!(host.host_confirm(true, false), Confirm::Starting);
        // The joining game follows the host's start.
        joiner.on_rules(true, rules(0b10));
        assert!(joiner.start_due());
        let (_, c) = joiner.confirm(true, true, false, || Own {
            mods: 0b1,
            loadout_daily: None,
        });
        assert_eq!(c, Confirm::Apply(rules(0b10)));
        assert!(!joiner.ready() && !joiner.partner_ready());
    }

    /// The joining player Ready first, then the host.
    #[test]
    fn joiner_ready_first_then_host_starts_both() {
        let (mut host, mut joiner) = pair();
        assert_eq!(joiner.confirm(true, true, false, sentinel).1, Confirm::Wait);
        assert_eq!(joiner.note_past_resume(), Note::Ready);
        assert!(host.partner(true));
        assert_eq!(host.host_note(), Note::PartnerReady);
        assert!(!host.go_due(), "not before the host's own Ready");
        assert_eq!(host.host_confirm(true, false), Confirm::Wait);
        count_and_go(&mut host, &mut joiner, Instant::now());
        joiner.on_rules(true, rules(0b10));
        assert!(joiner.start_due());
    }

    /// Either player takes Ready back by pressing it again: no run.
    #[test]
    fn ready_pressed_again_is_taken_back() {
        let (mut host, mut joiner) = pair();
        assert_eq!(host.host_confirm(true, false), Confirm::Wait);
        assert_eq!(host.host_confirm(true, false), Confirm::Unready);
        assert!(!host.ready());
        assert!(host.partner(true));
        assert!(!host.go_due(), "the host took it back");
        assert_eq!(joiner.confirm(true, true, false, sentinel).1, Confirm::Wait);
        assert_eq!(
            joiner.confirm(true, true, false, sentinel).1,
            Confirm::Unready
        );
        assert!(!joiner.ready());
        host.partner(false);
        assert!(!host.partner_ready() && host.host_note() == Note::None);
        // A mode row stays the host's own, Ready or not.
        assert_eq!(host.host_confirm(false, false), Confirm::Original);
    }

    /// Ready goes on leaving the screen, at the menu (the partner's stays), at
    /// a run start and at a link change.
    #[test]
    fn each_clearing_event_takes_ready_back() {
        let ready_pair = || {
            let (mut host, _) = pair();
            let _ = host.host_confirm(true, false);
            host.partner(true);
            host
        };
        let mut h = ready_pair();
        h.cancel();
        assert!(!h.ready() && h.partner_ready(), "back from the screen");
        let mut h = ready_pair();
        let _ = h.menu_frame();
        assert!(!h.ready() && h.partner_ready(), "the menu");
        let mut h = ready_pair();
        h.started_own();
        assert!(!h.ready() && !h.partner_ready(), "a run start");
        let mut h = ready_pair();
        h.link_changed();
        assert!(!h.ready() && !h.partner_ready() && !h.go_due(), "the link");
        assert!(h.partner(true), "after a re-link a Ready is news again");
        let (_, mut j) = pair();
        let _ = j.confirm(true, true, false, sentinel);
        j.partner(true);
        j.host_gone();
        assert!(!j.ready() && !j.partner_ready(), "the host gone");
        // Unlinked on the screen: the joining player's goes with the host's
        // picks.
        let (_, mut j) = pair();
        let _ = j.confirm(true, true, false, sentinel);
        let _ = j.loadout_frame(false, sentinel);
        assert!(!j.ready());
    }

    /// A go press the game did not take leaves both ready. Past [`GO_WAIT`] the
    /// count is called off.
    #[test]
    fn a_go_the_game_did_not_take_is_asked_again() {
        let (mut host, mut joiner) = pair();
        let t = Instant::now();
        let _ = host.host_confirm(true, false);
        host.partner(true);
        let _ = joiner.confirm(true, true, false, sentinel);
        joiner.partner(true);
        host.host_frame(t);
        let round = host.round;
        joiner.start_msg(false, true, round, StartStep::Begin, t);
        host.host_frame(t + START_COUNT);
        joiner.start_msg(false, true, round, StartStep::Go, t + START_COUNT);
        host.start_msg(true, false, round, StartStep::Ack, t + START_COUNT);
        let _ = (host.take_start_outs(), joiner.take_start_outs());
        host.begin_go();
        assert!(!host.press_due() && !host.go_due(), "one press at a time");
        host.go_done(false);
        assert!(host.press_due() && host.ready() && host.partner_ready());
        host.host_frame(t + START_COUNT + GO_WAIT);
        assert_eq!(host.take_start_outs(), [(round, StartStep::Cancel)]);
        joiner.start_msg(
            false,
            true,
            round,
            StartStep::Cancel,
            t + START_COUNT + GO_WAIT,
        );
        assert!(
            !joiner.start_locked() && joiner.ready(),
            "the joining game's word given back"
        );
        assert!(!host.press_due());
        // Both still ready: a new count.
        host.host_frame(t + START_COUNT + GO_WAIT);
        assert_eq!(host.take_start_outs(), [(round + 1, StartStep::Begin)]);
    }

    /// The host's mode rows are locked during the count. After its go, Ready and
    /// back do nothing on either side.
    #[test]
    fn a_start_counts_on_both_then_goes() {
        let (mut host, mut joiner) = pair();
        let t = Instant::now();
        let _ = host.host_confirm(true, false);
        let _ = joiner.confirm(true, true, false, sentinel);
        host.partner(true);
        joiner.partner(true);
        host.host_frame(t);
        let round = host.round;
        assert_eq!(host.take_start_outs(), [(round, StartStep::Begin)]);
        joiner.start_msg(false, true, round, StartStep::Begin, t);
        let (_, hc) = host.start_count().unwrap();
        let (_, jc) = joiner.start_count().unwrap();
        assert_eq!(hc.left(t), START_COUNT);
        assert_eq!(jc.left(t), START_COUNT, "each counts 3 s from its begin");
        assert_eq!(host.host_confirm(false, false), Confirm::Locked, "rows");
        host.host_frame(t + START_COUNT);
        assert_eq!(host.take_start_outs(), [(round, StartStep::Go)]);
        assert_eq!(host.host_confirm(true, false), Confirm::Starting);
        assert!(host.ready(), "a press after go changes nothing");
        joiner.start_msg(false, true, round, StartStep::Go, t + START_COUNT);
        assert_eq!(joiner.take_start_outs(), [(round, StartStep::Ack)]);
        assert_eq!(
            joiner.confirm(true, true, false, sentinel).1,
            Confirm::Starting
        );
        assert!(joiner.ready() && joiner.start_locked(), "no way back now");
        assert_eq!(joiner.note_past_resume(), Note::None);
        host.start_msg(true, false, round, StartStep::Ack, t + START_COUNT);
        host_presses(&mut host);
        assert!(host.start_count().is_none() && !host.start_locked());
        joiner.on_rules(true, rules(0b10));
        assert!(joiner.start_due());
        assert_eq!(
            joiner.confirm(true, true, false, Own::default).1,
            Confirm::Apply(rules(0b10))
        );
        assert!(!joiner.start_locked());
    }

    /// Either player's Ready, the host's back and the link each call the count
    /// off. A normal start follows.
    #[test]
    fn a_take_back_during_the_count_calls_it_off_on_both() {
        let counting = |t: Instant| {
            let (mut host, mut joiner) = pair();
            let _ = host.host_confirm(true, false);
            let _ = joiner.confirm(true, true, false, sentinel);
            host.partner(true);
            joiner.partner(true);
            host.host_frame(t);
            let round = host.round;
            let _ = host.take_start_outs();
            joiner.start_msg(false, true, round, StartStep::Begin, t);
            (host, joiner, round)
        };
        let t = Instant::now();
        let mid = t + START_COUNT / 2;
        // The host's Ready.
        let (mut host, mut joiner, r) = counting(t);
        assert_eq!(host.host_confirm(true, false), Confirm::Unready);
        assert_eq!(host.take_start_outs(), [(r, StartStep::Cancel)]);
        joiner.start_msg(false, true, r, StartStep::Cancel, mid);
        joiner.partner(false);
        assert!(host.start_count().is_none() && joiner.start_count().is_none());
        assert!(
            !host.ready() && joiner.ready(),
            "the joining player is still ready"
        );
        host.host_frame(t + 2 * START_COUNT);
        assert!(host.take_start_outs().is_empty(), "nobody starts");
        assert_eq!(host.host_confirm(true, false), Confirm::Wait);
        joiner.partner(true);
        count_and_go(&mut host, &mut joiner, t + 2 * START_COUNT);
        // The joining player's.
        let (mut host, mut joiner, r) = counting(t);
        assert_eq!(
            joiner.confirm(true, true, false, sentinel).1,
            Confirm::Unready
        );
        assert!(joiner.start_count().is_none(), "gone here at once");
        assert!(
            joiner.take_start_outs().is_empty(),
            "the joining game's Ready false says it"
        );
        host.partner(false);
        assert_eq!(host.take_start_outs(), [(r, StartStep::Cancel)]);
        joiner.start_msg(false, true, r, StartStep::Cancel, mid);
        assert!(host.ready() && !joiner.ready(), "the host is still ready");
        host.host_frame(t + 2 * START_COUNT);
        assert!(host.take_start_outs().is_empty());
        let _ = joiner.confirm(true, true, false, sentinel);
        host.partner(true);
        count_and_go(&mut host, &mut joiner, t + 2 * START_COUNT);
        // The host's back, the link.
        let (mut host, _, r) = counting(t);
        host.cancel();
        assert_eq!(host.take_start_outs(), [(r, StartStep::Cancel)]);
        let (mut host, mut joiner, _) = counting(t);
        host.link_changed();
        joiner.host_gone();
        assert!(host.start_count().is_none() && joiner.start_count().is_none());
        assert!(host.take_start_outs().is_empty(), "no link to tell");
    }

    /// A link lost during the start count, run as `Coop::pump` and the new game
    /// screen run it: no count drawn, the lost-link line shown, no go. A solo
    /// Start after it has no count, and a new link counts again.
    #[test]
    fn a_link_lost_in_the_count_is_told_and_starts_nobody() {
        use crate::ui::run::{LinkNotice, ReadyPlace, lost_link_line};
        use ndml_net::endpoint::DisconnectReason as R;
        let counting = |t: Instant| {
            let (mut host, mut joiner) = pair();
            let _ = host.host_confirm(true, false);
            let _ = joiner.confirm(true, true, false, sentinel);
            host.partner(true);
            joiner.partner(true);
            host.host_frame(t);
            let round = host.round;
            assert_eq!(host.take_start_outs(), [(round, StartStep::Begin)]);
            joiner.start_msg(false, true, round, StartStep::Begin, t);
            assert!(host.start_count().is_some() && joiner.start_count().is_some());
            (host, joiner)
        };
        let t = Instant::now();
        let lost = t + START_COUNT / 3;
        for host_side in [true, false] {
            let (mut host, mut joiner) = counting(t);
            // The side left behind, as the pump has it.
            let (me, reason, line) = if host_side {
                (&mut host, R::Remote, "Sam left.")
            } else {
                joiner.host_gone();
                (&mut joiner, R::Timeout, "Lost the link to Sam.")
            };
            let mut notice = LinkNotice::new(lost_link_line(reason, "Sam").expect("told"), lost);
            me.link_changed();
            // Every frame to past the count's end: no number (so no tick and
            // no go cue), the notice in the note's place, nothing sent.
            let mut now = lost;
            while now <= t + 2 * START_COUNT {
                me.host_frame(now);
                assert!(me.start_count().is_none(), "no count drawn");
                let (bar, _) = notice
                    .frame(ReadyPlace::NewGameScreen, now)
                    .expect("the notice on the new game screen");
                assert_eq!(bar.text, line);
                assert!(me.take_start_outs().is_empty(), "nothing to send");
                assert!(!me.go_due() && !me.press_due() && !me.start_due());
                assert!(!me.ready() && !me.partner_ready() && !me.start_locked());
                now += Duration::from_millis(16);
            }
            assert_ne!(me.host_note(), Note::Counting);
            assert_ne!(me.note_past_resume(), Note::Counting);
            // A solo Start: the game's own, no count after it.
            let mut solo = me.clone();
            assert_eq!(
                solo.confirm(false, true, false, sentinel).1,
                Confirm::Original,
                "a solo start"
            );
            solo.started_own();
            solo.host_frame(now);
            assert!(solo.start_count().is_none() && solo.take_start_outs().is_empty());
            // Linked again (both start unready): a normal start, its count.
            host.link_changed();
            joiner.link_changed();
            joiner.on_rules(false, rules(0b10));
            assert_eq!(host.host_confirm(true, false), Confirm::Wait);
            assert_eq!(joiner.confirm(true, true, false, sentinel).1, Confirm::Wait);
            host.partner(true);
            joiner.partner(true);
            count_and_go(&mut host, &mut joiner, now);
        }
    }

    /// A linked Resume Run is refused with the rule off too. Without the hooks
    /// that guard the resume file, every run is solo (`run_safety`).
    #[test]
    fn the_saved_runs_rules_fail_safe() {
        let mut off = Joiner::default();
        assert_eq!(
            off.host_confirm(true, true),
            Confirm::ResumeBlocked,
            "host, rule off"
        );
        assert_eq!(
            off.host_confirm(true, false),
            Confirm::Original,
            "its Start"
        );
        let mut off = Joiner::default();
        assert_eq!(
            off.confirm(true, true, true, sentinel).1,
            Confirm::ResumeBlocked,
            "joining, rule off"
        );
        let mut early = Joiner::on();
        assert_eq!(
            early.confirm(true, true, true, sentinel).1,
            Confirm::ResumeBlocked,
            "joining, the host's picks not in yet"
        );
        let mut solo = Joiner::default();
        assert_eq!(
            solo.confirm(false, true, true, sentinel).1,
            Confirm::Original,
            "unlinked: the game's own Resume Run"
        );
        let f = |names: &[&str]| -> Vec<String> { names.iter().map(|n| (*n).to_owned()).collect() };
        assert_eq!(run_safety(&[]), None);
        assert_eq!(
            run_safety(&f(&[names::RESUME_RUN])),
            None,
            "its press tells it"
        );
        assert_eq!(
            run_safety(&f(&[names::CONFIRM_LOADOUT])),
            None,
            "resumeRun tells it"
        );
        let both = run_safety(&f(&[names::RESUME_RUN, names::CONFIRM_LOADOUT])).expect("off");
        assert!(both.contains("could not be told"), "{both}");
        for n in [names::TRIGGER_SAVE_RUN, names::DELETE_RUN_FILES] {
            let why = run_safety(&f(&[n])).expect("off");
            assert!(why.contains("write or delete the saved run"), "{why}");
        }
    }

    /// No count without a partner: a solo start (and a resumed one) is the
    /// game's own.
    #[test]
    fn a_solo_start_has_no_count() {
        let mut alone = Joiner::on();
        assert_eq!(
            alone.confirm(false, true, false, sentinel).1,
            Confirm::Original
        );
        alone.host_frame(Instant::now());
        assert!(alone.start_count().is_none() && alone.take_start_outs().is_empty());
        assert_eq!(
            alone.confirm(false, true, true, sentinel).1,
            Confirm::Original,
            "a resume"
        );
        let mut off = Joiner::default();
        let _ = off.host_confirm(true, false);
        off.partner(true);
        off.host_frame(Instant::now());
        assert!(off.start_count().is_none(), "the rule off");
    }

    /// Named orders of a take-back crossing the host's messages.
    /// [`no_order_of_messages_starts_one_game_alone`] tries every order.
    #[test]
    fn a_take_back_crossing_the_hosts_messages_starts_nobody() {
        let t = Instant::now();
        let end = t + START_COUNT;
        let ready_pair = || {
            let (mut host, mut joiner) = pair();
            let _ = host.host_confirm(true, false);
            let _ = joiner.confirm(true, true, false, sentinel);
            host.partner(true);
            joiner.partner(true);
            (host, joiner)
        };
        // 1. The joining player's take-back crosses the host's Begin.
        let (mut host, mut joiner) = ready_pair();
        host.host_frame(t);
        let r = host.round;
        let _ = joiner.confirm(true, true, false, sentinel);
        joiner.start_msg(false, true, r, StartStep::Begin, t);
        assert!(joiner.start_count().is_none());
        host.partner(false);
        assert_eq!(
            host.take_start_outs(),
            [(r, StartStep::Begin), (r, StartStep::Cancel)]
        );
        host.host_frame(end);
        assert!(!host.press_due() && host.take_start_outs().is_empty());
        // 2. The take-back crosses the host's Go.
        let (mut host, mut joiner) = ready_pair();
        host.host_frame(t);
        let r = host.round;
        joiner.start_msg(false, true, r, StartStep::Begin, t);
        host.host_frame(end);
        let _ = joiner.confirm(true, true, false, sentinel);
        joiner.start_msg(false, true, r, StartStep::Go, end);
        assert!(joiner.take_start_outs().is_empty(), "no Ack");
        host.partner(false);
        assert!(!host.press_due());
        assert_eq!(
            host.take_start_outs(),
            [
                (r, StartStep::Begin),
                (r, StartStep::Go),
                (r, StartStep::Cancel)
            ]
        );
        joiner.start_msg(false, true, r, StartStep::Cancel, end);
        host.host_frame(end + GO_WAIT);
        assert!(!host.press_due() && !joiner.start_locked());
        // 3. A take-back and a Ready again while the Go is on its way: no Ack,
        //    and the host counts again.
        let (mut host, mut joiner) = ready_pair();
        host.host_frame(t);
        let r = host.round;
        joiner.start_msg(false, true, r, StartStep::Begin, t);
        host.host_frame(end);
        let _ = joiner.confirm(true, true, false, sentinel);
        let _ = joiner.confirm(true, true, false, sentinel);
        assert!(joiner.ready());
        joiner.start_msg(false, true, r, StartStep::Go, end);
        assert!(joiner.take_start_outs().is_empty(), "not counting: no Ack");
        host.partner(false);
        host.partner(true);
        host.host_frame(end);
        assert_eq!(
            host.take_start_outs(),
            [
                (r, StartStep::Begin),
                (r, StartStep::Go),
                (r, StartStep::Cancel),
                (r + 1, StartStep::Begin)
            ]
        );
        joiner.start_msg(false, true, r, StartStep::Cancel, end);
        joiner.start_msg(false, true, r + 1, StartStep::Begin, end);
        assert_eq!(joiner.start_count().map(|c| c.0), Some(r + 1));
        // 4. The host's give-up crosses the joining game's Ack.
        let (mut host, mut joiner) = ready_pair();
        host.host_frame(t);
        let r = host.round;
        joiner.start_msg(false, true, r, StartStep::Begin, t);
        host.host_frame(end);
        joiner.start_msg(false, true, r, StartStep::Go, end);
        assert_eq!(joiner.take_start_outs(), [(r, StartStep::Ack)]);
        host.host_frame(end + GO_WAIT);
        host.start_msg(true, false, r, StartStep::Ack, end + GO_WAIT);
        assert!(!host.press_due(), "late");
        joiner.start_msg(false, true, r, StartStep::Cancel, end + GO_WAIT);
        assert!(!joiner.start_locked() && !joiner.start_due());
    }

    /// A message on the wire between the two games (one reliable, ordered
    /// stream each way).
    #[derive(Debug, Clone, Copy)]
    enum Wire {
        Ready(bool),
        Start(u16, StartStep),
        /// The host's `RunRules` start.
        Started,
        /// The host's `RunRules` picks without a start.
        Picks,
    }

    /// Two linked games and the wire between them, run as `Coop::pump` runs
    /// them.
    #[derive(Debug, Clone)]
    struct Link {
        host: Joiner,
        join: Joiner,
        to_join: std::collections::VecDeque<Wire>,
        to_host: std::collections::VecDeque<Wire>,
        told: (bool, bool),
        t: Instant,
        ticks: u8,
        host_started: bool,
        join_started: bool,
        /// Presses left (each side): a take-back, then a Ready again.
        presses: (u8, u8),
        /// A take-back made while that side's count showed, and no Ready
        /// pressed since: then no run may start.
        took_back_in_count: (bool, bool),
        /// Link changes left.
        relinks: u8,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Act {
        ToJoin,
        ToHost,
        HostFrame,
        Tick,
        LongTick,
        HostPress,
        JoinPress,
        /// The link goes and comes back: the wire is cleared, both are unready.
        Relink,
    }

    impl Link {
        /// Both on their new game screens, ready, each told of the other.
        fn ready(t: Instant) -> Self {
            let (mut host, mut join) = pair();
            let _ = host.host_confirm(true, false);
            let _ = join.confirm(true, true, false, sentinel);
            host.partner(true);
            join.partner(true);
            Self {
                host,
                join,
                to_join: std::collections::VecDeque::new(),
                to_host: std::collections::VecDeque::new(),
                told: (true, true),
                t,
                ticks: 0,
                host_started: false,
                join_started: false,
                presses: (2, 2),
                took_back_in_count: (false, false),
                relinks: 1,
            }
        }

        fn flush(&mut self) {
            for (r, s) in self.host.take_start_outs() {
                self.to_join.push_back(Wire::Start(r, s));
            }
            if self.host.ready() != self.told.0 {
                self.told.0 = self.host.ready();
                self.to_join.push_back(Wire::Ready(self.told.0));
            }
            for (r, s) in self.join.take_start_outs() {
                self.to_host.push_back(Wire::Start(r, s));
            }
            if self.join.ready() != self.told.1 {
                self.told.1 = self.join.ready();
                self.to_host.push_back(Wire::Ready(self.told.1));
            }
        }

        /// The joining game's new game screen step: the waiting start made
        /// once the host's start is here (`start_due`).
        fn join_step(&mut self) {
            if self.join.start_due() {
                let (_, c) = self.join.confirm(true, true, false, Own::default);
                assert!(matches!(c, Confirm::Apply(_)), "{c:?}");
                self.join_started = true;
            }
        }

        fn can(&self, a: Act) -> bool {
            match a {
                Act::ToJoin => !self.to_join.is_empty(),
                Act::ToHost => !self.to_host.is_empty(),
                Act::HostFrame => !self.host_started,
                Act::Tick => self.ticks < 4,
                Act::LongTick => self.ticks < 5,
                Act::HostPress => self.presses.0 > 0 && !self.host_started,
                Act::JoinPress => self.presses.1 > 0 && !self.join_started,
                Act::Relink => self.relinks > 0,
            }
        }

        fn act(&mut self, a: Act) {
            match a {
                Act::ToJoin => match self.to_join.pop_front() {
                    Some(Wire::Ready(r)) => {
                        self.join.partner(r);
                    }
                    Some(Wire::Start(r, s)) => self.join.start_msg(false, true, r, s, self.t),
                    Some(Wire::Started) => self.join.on_rules(true, rules(0b10)),
                    Some(Wire::Picks) => self.join.on_rules(false, rules(0b10)),
                    None => {}
                },
                Act::ToHost => match self.to_host.pop_front() {
                    Some(Wire::Ready(r)) => {
                        self.host.partner(r);
                    }
                    Some(Wire::Start(r, s)) => self.host.start_msg(true, false, r, s, self.t),
                    Some(Wire::Started | Wire::Picks) | None => {}
                },
                Act::HostFrame => {
                    self.host.host_frame(self.t);
                    if self.host.press_due() {
                        host_presses(&mut self.host);
                        self.host_started = true;
                        // `send_start` from the fade's begin.
                        self.flush();
                        self.to_join.push_back(Wire::Started);
                    }
                }
                Act::Tick => {
                    self.ticks += 1;
                    self.t += Duration::from_secs(1);
                }
                Act::LongTick => {
                    self.ticks = 5;
                    self.t += GO_WAIT + Duration::from_secs(1);
                }
                Act::HostPress => {
                    self.presses.0 -= 1;
                    let in_count = self.host.start_count().is_some();
                    let was = self.host.ready();
                    let c = self.host.host_confirm(true, false);
                    if was && c == Confirm::Unready {
                        self.took_back_in_count.0 |= in_count;
                    } else if c == Confirm::Wait {
                        self.took_back_in_count.0 = false;
                    }
                }
                Act::Relink => {
                    self.relinks -= 1;
                    self.to_join.clear();
                    self.to_host.clear();
                    self.host.link_changed();
                    self.join.link_changed();
                    // A partner that heard nothing in this link takes "not
                    // ready" (`Coop::sync_ready`).
                    self.told = (false, false);
                    // `start_again` while its run waits at the gate, else its picks.
                    let w = if self.host_started && !self.join_started {
                        Wire::Started
                    } else {
                        Wire::Picks
                    };
                    self.to_join.push_back(w);
                    // A Ready pressed again before the re-link is spent.
                    self.took_back_in_count = (false, false);
                }
                Act::JoinPress => {
                    self.presses.1 -= 1;
                    let in_count = self.join.start_count().is_some();
                    let was = self.join.ready();
                    let (_, c) = self.join.confirm(true, true, false, sentinel);
                    if was && c == Confirm::Unready {
                        self.took_back_in_count.1 |= in_count;
                    } else if c == Confirm::Wait {
                        self.took_back_in_count.1 = false;
                    }
                }
            }
            self.flush();
            self.join_step();
            self.flush();
        }

        /// No more presses: everything on the wire is read, the host's
        /// frames run on with time, until nothing moves.
        fn settle(&mut self) {
            for _ in 0..64 {
                if self.can(Act::ToJoin) {
                    self.act(Act::ToJoin);
                } else if self.can(Act::ToHost) {
                    self.act(Act::ToHost);
                } else {
                    let before = (self.host.start, self.join.start);
                    self.t += Duration::from_secs(1);
                    self.act(Act::HostFrame);
                    let quiet = self.to_join.is_empty() && self.to_host.is_empty();
                    if quiet
                        && before == (self.host.start, self.join.start)
                        && self.host.start == StartCount::Idle
                        && (self.host_started || !self.host.go_due())
                    {
                        return;
                    }
                }
            }
            panic!("never settled: {self:?}");
        }

        fn check(&self, trail: &[Act]) {
            let mut end = self.clone();
            end.settle();
            assert_eq!(
                end.host_started, end.join_started,
                "one game started alone after {trail:?}"
            );
            if self.took_back_in_count.0 || self.took_back_in_count.1 {
                assert!(
                    !end.host_started,
                    "a run started after a take-back during the count ({trail:?})"
                );
            }
        }
    }

    /// Every order of the two games' steps up to a depth, each settled: never
    /// one game starting alone, never a run after a Ready taken back in the count.
    #[test]
    fn no_order_of_messages_starts_one_game_alone() {
        const ACTS: [Act; 8] = [
            Act::ToJoin,
            Act::ToHost,
            Act::HostFrame,
            Act::Tick,
            Act::LongTick,
            Act::HostPress,
            Act::JoinPress,
            Act::Relink,
        ];
        fn walk(link: &Link, trail: &mut Vec<Act>, depth: usize, n: &mut u64) {
            link.check(trail);
            *n += 1;
            if depth == 0 {
                return;
            }
            for a in ACTS {
                if link.can(a) {
                    let mut next = link.clone();
                    next.act(a);
                    trail.push(a);
                    walk(&next, trail, depth - 1, n);
                    trail.pop();
                }
            }
        }
        let mut n = 0;
        walk(&Link::ready(Instant::now()), &mut Vec::new(), 9, &mut n);
        assert!(n > 100_000, "{n} orders");
    }

    #[test]
    fn with_the_rule_off_nothing_waits_on_a_ready() {
        let mut off = Joiner::default();
        assert!(!off.partner(true) && !off.partner_ready());
        assert_eq!(off.host_confirm(true, false), Confirm::Original);
        assert!(!off.go_due());
        assert_eq!(
            off.confirm(true, true, false, sentinel).1,
            Confirm::Original
        );
    }

    #[test]
    fn a_resume_never_waits_on_a_ready() {
        let (mut host, mut joiner) = pair();
        let _ = host.host_confirm(true, false);
        host.partner(true);
        assert_eq!(host.host_confirm(true, true), Confirm::ResumeBlocked);
        assert!(host.ready(), "the refused press changes no Ready");
        joiner.partner(true);
        assert_eq!(
            joiner.confirm(true, true, true, sentinel).1,
            Confirm::ResumeBlocked
        );
        assert!(!joiner.ready());
        let mut alone = Joiner::on();
        assert_eq!(
            alone.confirm(false, true, true, sentinel).1,
            Confirm::Original
        );
    }

    #[test]
    fn the_ready_lines_name_the_state() {
        let describe = |_: &Rules| String::new();
        assert_eq!(
            bar_line(&Note::PartnerReady, true, "Sam", "Ready", &describe).as_deref(),
            Some("Sam is ready. Press Ready to start.")
        );
        assert_eq!(
            partner_waiting_line("Sam"),
            "Sam is ready on the new game screen and waiting for you."
        );
        assert_eq!(partner_ready_alert("Sam"), "SAM IS READY");
    }

    #[test]
    fn a_start_is_either_fade_begun() {
        let off = (Some(0.0), Some(0.0));
        assert!(start_began(off, (Some(1.0), Some(0.0))), "a new run's");
        assert!(start_began(off, (Some(0.0), Some(1.0))), "a resumed run's");
        assert!(!start_began(off, off), "refused or busy");
        assert!(!start_began((Some(1.0), Some(0.0)), (Some(1.0), Some(0.0))));
        assert!(!start_began((Some(0.0), Some(1.0)), (Some(0.0), Some(1.0))));
        assert!(start_began((None, None), (None, Some(1.0))));
        assert!(!start_began((None, None), (None, None)));
    }

    /// A link that comes up during an unlinked Resume Run's start holds nothing:
    /// the resume file stays on.
    #[test]
    fn a_link_during_a_resumes_start_holds_nothing() {
        let mut j = Joiner::on();
        let (restore, c) = j.confirm(false, true, true, sentinel);
        assert_eq!((restore, c), (None, Confirm::Original));
        assert!(start_began((Some(0.0), Some(0.0)), (Some(0.0), Some(1.0))));
        j.started_own();
        j.on_rules(false, rules(0b100));
        assert_eq!(j.loadout_frame(true, sentinel), Frame::default());
        assert!(!j.held(), "the resume file stays on");
        j.on_rules(true, rules(0b100));
        assert_eq!(j.confirm(true, true, true, sentinel).1, Confirm::Starting);
        assert!(!j.held() && !j.start_due());
        // The menu after the run: a new link may share again.
        assert_eq!(j.menu_frame(), None);
        j.on_rules(false, rules(0b100));
        let mine = Own {
            mods: 0b1,
            loadout_daily: None,
        };
        assert!(j.loadout_frame(true, || mine).show.is_some());
    }

    #[test]
    fn a_linked_new_game_screen_holds_the_hosts_picks_and_locks_the_rows() {
        let mut j = Joiner::on();
        let mine = Own {
            mods: 0b1,
            loadout_daily: Some(pick(1)),
        };
        // Linked, nothing from the host yet: the game's own screen.
        let f = j.loadout_frame(true, || mine.clone());
        assert_eq!(f, Frame::default());
        assert!(!j.held());
        assert_eq!(
            j.confirm(true, false, false, || mine.clone()).1,
            Confirm::Original
        );
        j.on_rules(false, rules(0b100));
        let f = j.loadout_frame(true, || mine.clone());
        assert_eq!(f.show, Some(rules(0b100)));
        assert_eq!(f.note, Note::Picks(rules(0b100)));
        assert!(j.held());
        // The own picks are read once.
        let f = j.loadout_frame(true, sentinel);
        assert_eq!(f.show, Some(rules(0b100)));
        assert_eq!(j.confirm(true, false, false, sentinel).1, Confirm::Locked);
        // Start before the host: ready, waiting.
        assert_eq!(j.confirm(true, true, false, sentinel).1, Confirm::Wait);
        assert_eq!(j.loadout_frame(true, sentinel).note, Note::Ready);
        assert!(!j.start_due());
        j.on_rules(true, rules(0b110));
        assert!(j.start_due());
        let (restore, c) = j.confirm(true, true, false, sentinel);
        assert_eq!((restore, c), (None, Confirm::Apply(rules(0b110))));
        assert!(j.skip_writes());
        assert_eq!(j.applying(), Some(&rules(0b110)));
        j.confirm_done();
        assert!(!j.skip_writes() && !j.start_due() && !j.ready());
        assert_eq!(j.held_rules(), Some(&rules(0b110)));
        // The run's end: the own picks back, offered again until written.
        assert_eq!(j.menu_frame(), Some(mine.clone()));
        assert!(j.held(), "kept until the write is known to have happened");
        assert_eq!(j.menu_frame(), Some(mine), "the write failed: again");
        j.restored();
        assert!(!j.held());
        assert_eq!(j.menu_frame(), None, "once written");
    }

    #[test]
    fn the_start_row_reads_ready_only_while_the_joining_screen_is_the_hosts() {
        let mut j = Joiner::on();
        assert!(!j.locked(true), "nothing from the host yet");
        j.on_rules(false, rules(0b1));
        assert!(j.locked(true));
        assert!(!j.locked(false), "unlinked, or the host");
        j.on_rules(true, rules(0b1));
        let _ = j.confirm(true, true, false, Own::default);
        assert!(!j.locked(true), "a start under way");
        let _ = j.menu_frame();
        j.restored();
        assert!(j.locked(true), "the next visit");
        j.host_gone();
        assert!(!j.locked(true), "the host gone");
        let mut off = Joiner::default();
        off.on_rules(false, rules(0b1));
        assert!(!off.locked(true), "the rule off");
    }

    #[test]
    fn a_joiner_without_every_hook_never_holds() {
        let mut j = Joiner::default();
        assert!(!j.enabled(), "off until the glue enables it");
        j.on_rules(false, rules(0b100));
        j.on_rules(true, rules(0b100));
        assert!(!j.shares());
        assert_eq!(j.loadout_frame(true, sentinel), Frame::default());
        assert_eq!(j.confirm(true, true, false, sentinel).1, Confirm::Original);
        assert_eq!(j.confirm(true, false, false, sentinel).1, Confirm::Original);
        assert!(!j.held() && j.original_safe(true));
        let failed = vec!["gml_Script_ini_write_bool".to_owned(), "other".to_owned()];
        assert_eq!(
            missing_hooks(&NEEDED_HOOKS, &failed),
            vec!["gml_Script_ini_write_bool"]
        );
        assert_eq!(
            missing_hooks(&NEEDED_HOOKS, &["other".to_owned()]),
            Vec::<&str>::new()
        );
    }

    /// An applied Start the game did not take (its fade back to the menu was
    /// running, or the call failed): the host's start stays to be taken.
    #[test]
    fn an_applied_start_the_game_did_not_take_is_not_lost() {
        let mut j = Joiner::on();
        let r = rules(0b100);
        j.on_rules(true, r.clone());
        assert_eq!(j.loadout_frame(true, Own::default).note, Note::HostStarted);
        let (_, d) = j.confirm(true, true, false, Own::default);
        assert_eq!(d, Confirm::Apply(r.clone()));
        assert!(!j.host_started() && j.skip_writes());
        // The after-hook: the fade did not begin.
        j.start_not_taken();
        j.confirm_done();
        assert!(j.host_started() && !j.skip_writes() && !j.ready());
        assert!(
            j.locked(true),
            "no start under way: the joining screen is the host's again"
        );
        // alarm[1]: the menu; the own picks back.
        assert!(j.menu_frame().is_some());
        j.restored();
        let f = j.loadout_frame(true, Own::default);
        assert_eq!(
            f.note,
            Note::HostStarted,
            "the host still waits at its gate"
        );
        let (_, d) = j.confirm(true, true, false, Own::default);
        assert_eq!(d, Confirm::Apply(r), "the Start still joins the host's run");
        // Taken this time: nothing to put back (and a second call is harmless).
        j.confirm_done();
        j.start_not_taken();
        assert!(!j.host_started());
    }

    /// A missing game variable or script turns the rule off like a missing
    /// hook, with one line naming everything missing.
    #[test]
    fn a_missing_handle_turns_the_rule_off_like_a_missing_hook() {
        let all = [
            ("oLoadout.pos", true),
            ("oLoadout.fade", true),
            ("gml_Script_confirmLoadout (script)", true),
            ("gameModMayhem", true),
        ];
        assert_eq!(rule_off_because(&[], &all), None, "everything there: on");
        let mut no_pos = all;
        no_pos[0].1 = false;
        no_pos[3].1 = false;
        assert_eq!(
            rule_off_because(&[], &no_pos).as_deref(),
            Some("not found: oLoadout.pos, gameModMayhem")
        );
        assert_eq!(
            rule_off_because(&[names::MENU_STEP], &no_pos).as_deref(),
            Some(
                format!(
                    "not hooked: {}; not found: oLoadout.pos, gameModMayhem",
                    names::MENU_STEP
                )
                .as_str()
            )
        );
        assert_eq!(
            rule_off_because(&[names::MENU_STEP], &all).as_deref(),
            Some(format!("not hooked: {}", names::MENU_STEP).as_str())
        );
    }

    /// The rule off and no menu frame ever coming: a solo player's later runs
    /// still start.
    #[test]
    fn rule_off_and_no_menu_frame_a_second_run_can_still_be_started() {
        let mut j = Joiner::default();
        assert_eq!(
            j.confirm(false, true, false, sentinel),
            (None, Confirm::Original)
        );
        j.started_own(); // the fade began
        let f = j.loadout_frame(false, sentinel);
        assert_eq!(f.restore, None);
        for _ in 0..3 {
            let (_, row) = j.confirm(false, false, false, sentinel);
            let (_, start) = j.confirm(false, true, false, sentinel);
            assert_eq!((row, start), (Confirm::Original, Confirm::Original));
            j.started_own();
        }
        // On, the latch holds until the menu.
        let mut on = Joiner::on();
        let _ = on.confirm(false, true, false, sentinel);
        on.started_own();
        assert_eq!(
            on.confirm(false, true, false, sentinel).1,
            Confirm::Starting
        );
        let _ = on.menu_frame();
        assert_eq!(
            on.confirm(false, true, false, sentinel).1,
            Confirm::Original
        );
    }

    #[test]
    fn the_original_press_is_unsafe_while_anything_is_held_or_locked() {
        let mut j = Joiner::on();
        assert!(j.original_safe(true), "linked, nothing from the host yet");
        j.on_rules(false, rules(0b1));
        assert!(!j.original_safe(true), "the locked screen");
        assert!(j.original_safe(false));
        let _ = j.loadout_frame(true, Own::default);
        assert!(!j.original_safe(false), "unlinked with the host's held");
        // A role change to host: the own picks are written back first.
        assert_eq!(j.release(), Some(Own::default()));
        j.restored();
        assert!(j.original_safe(false));
    }

    #[test]
    fn a_stuck_write_skip_is_cleared_at_the_next_frame() {
        let mut j = Joiner::on();
        j.on_rules(true, rules(0b11));
        let _ = j.confirm(true, true, false, Own::default);
        assert!(j.skip_writes());
        // `confirmLoadout` never returned (a GML error): no after-hook.
        assert!(j.frame_tick());
        assert!(!j.skip_writes());
        assert!(!j.frame_tick(), "once");
    }

    #[test]
    fn a_link_lost_during_the_fade_keeps_the_hosts_start_until_the_menu() {
        let mut j = Joiner::on();
        let mine = Own {
            mods: 0b1000,
            loadout_daily: Some(pick(4)),
        };
        j.on_rules(true, rules(0b11));
        let _ = j.loadout_frame(true, || mine.clone());
        assert_eq!(
            j.confirm(true, true, false, sentinel).1,
            Confirm::Apply(rules(0b11))
        );
        j.confirm_done();
        j.host_gone();
        // The new game screen still steps for 10 to 20 frames: the own picks
        // stay out of the run being made, and a press is skipped.
        assert_eq!(j.loadout_frame(false, sentinel), Frame::default());
        assert_eq!(j.confirm(false, true, false, sentinel).1, Confirm::Starting);
        assert_eq!(j.held_rules(), Some(&rules(0b11)));
        assert!(j.held(), "the resume file stays off");
        assert_eq!(j.menu_frame(), Some(mine));
    }

    #[test]
    fn resume_run_is_refused_for_a_linked_host_and_joiner_and_taken_unlinked() {
        let describe = |r: &Rules| r.describe(&|i| format!("Mode{i}"));
        let line = "Resume Run is not available while you play with Sam.";
        // The linked host (the glue routes its presses to `host_confirm`).
        let mut host = Joiner::on();
        assert_eq!(host.host_confirm(true, true), Confirm::ResumeBlocked);
        assert_eq!(host.host_note(), Note::NoResume);
        assert_eq!(
            bar_line(&host.host_note(), true, "Sam", "Start", &describe).as_deref(),
            Some(line)
        );
        assert_eq!(
            host.host_confirm(false, true),
            Confirm::Original,
            "a mode row"
        );
        assert_eq!(host.host_note(), Note::None, "until the next press");
        assert_eq!(
            host.host_confirm(true, false),
            Confirm::Wait,
            "a new run's Start is Ready while linked"
        );
        assert_eq!(host.host_note(), Note::Ready);
        let _ = host.host_confirm(true, true);
        host.cancel();
        assert_eq!(host.host_note(), Note::None, "back from the screen");
        // The rule off: refused all the same (the saved run's rule).
        let mut off = Joiner::default();
        assert_eq!(off.host_confirm(true, true), Confirm::ResumeBlocked);
        // The linked joiner: the same refusal, the same line.
        let mut joiner = Joiner::on();
        joiner.on_rules(false, rules(0));
        assert_eq!(
            joiner.confirm(true, true, true, Own::default).1,
            Confirm::ResumeBlocked
        );
        let note = joiner.loadout_frame(true, Own::default).note;
        assert_eq!(
            bar_line(&note, true, "Sam", "Ready", &describe).as_deref(),
            Some(line)
        );
        // Unlinked: taken.
        let mut solo = Joiner::on();
        assert_eq!(
            solo.confirm(false, true, true, Own::default).1,
            Confirm::Original
        );
    }

    #[test]
    fn resume_is_not_offered_and_back_lets_a_wait_go() {
        let mut j = Joiner::on();
        j.on_rules(false, rules(0));
        let _ = j.loadout_frame(true, Own::default);
        assert_eq!(
            j.confirm(true, true, true, Own::default).1,
            Confirm::ResumeBlocked
        );
        assert_eq!(j.loadout_frame(true, Own::default).note, Note::NoResume);
        assert_eq!(
            j.note_past_resume(),
            Note::Picks(rules(0)),
            "the bar beside the red line keeps the host's picks"
        );
        assert_eq!(j.confirm(true, true, false, Own::default).1, Confirm::Wait);
        assert_eq!(j.loadout_frame(true, Own::default).note, Note::Ready);
        j.cancel();
        assert_eq!(
            j.loadout_frame(true, Own::default).note,
            Note::Picks(rules(0))
        );
    }

    /// After the pre-answered Game Start, a lost link never turns the new
    /// game screen's Start into a solo run over a saved run.
    #[test]
    fn a_saved_run_is_never_overwritten_after_the_preanswered_start() {
        use std::time::Duration;
        let s = Duration::from_secs;
        // Linked: nothing to guard (a co-op run never writes the file).
        assert_eq!(saved_run_guard(true, true, true, None), SavedRunGuard::Off);
        // The saved run's state as read: 1 is a saved run; one that cannot
        // be read is taken as one (the guard stays on).
        assert!(saved_run_state(Some(1.0)));
        assert!(!saved_run_state(Some(0.0)) && !saved_run_state(Some(-1.0)));
        assert!(saved_run_state(None), "unreadable: guarded");
        assert_eq!(
            saved_run_guard(true, saved_run_state(None), false, None),
            SavedRunGuard::RefuseStart
        );
        // A link wait: no solo start.
        for d in [
            None,
            Some(Duration::ZERO),
            Some(Duration::from_millis(2999)),
        ] {
            assert_eq!(
                saved_run_guard(true, true, false, d),
                SavedRunGuard::RefuseStart,
                "{d:?}"
            );
        }
        // Gone: back to the menu, where the game asks.
        assert_eq!(
            saved_run_guard(true, true, false, Some(LINK_GONE)),
            SavedRunGuard::BackToMenu
        );
        assert_eq!(
            saved_run_guard(true, true, false, Some(s(600))),
            SavedRunGuard::BackToMenu
        );
        // No saved run (nothing to lose), or a visit the game asked about
        // itself (unlinked Game Start, Resume Run): the game's own screen.
        assert_eq!(
            saved_run_guard(true, false, false, Some(s(9))),
            SavedRunGuard::Off
        );
        assert_eq!(
            saved_run_guard(false, true, false, Some(s(9))),
            SavedRunGuard::Off
        );
        assert_eq!(
            saved_run_kept_line("Sam"),
            "Lost the link to Sam. Your saved run is kept."
        );
    }

    #[test]
    fn the_refused_resume_warning_flashes_three_times_then_fades_as_the_game() {
        // Three reds: each flash starts at full red.
        let reds: Vec<u32> = (0..400).filter(|&n| warn_strobe(n) == Some(0.0)).collect();
        assert_eq!(reds, vec![0, WARN_FLASH_STEPS, 2 * WARN_FLASH_STEPS]);
        // A fast fade inside a flash, never white (the line stays shown
        // while `rgbB < 255`).
        for n in 1..WARN_FLASH_STEPS {
            let (a, b) = (warn_strobe(n).unwrap(), warn_strobe(n - 1).unwrap());
            assert!(a > b && a < 255.0, "{n}: {a}");
        }
        // After the third red the game's own fade runs.
        assert_eq!(warn_strobe(2 * WARN_FLASH_STEPS + 1), None);
        assert_eq!(
            no_resume_line("Sam"),
            "Resume Run is not available while you play with Sam."
        );
        assert!(!no_resume_line("Sam").contains("loadout"));
    }

    #[test]
    fn a_lost_host_on_the_new_game_screen_gives_the_own_picks_back_at_once() {
        let mut j = Joiner::on();
        let mine = Own {
            mods: 0b1000,
            loadout_daily: None,
        };
        j.on_rules(false, rules(0b1));
        let _ = j.loadout_frame(true, || mine.clone());
        j.host_gone();
        assert!(j.held(), "kept until the next frame");
        let f = j.loadout_frame(false, sentinel);
        assert_eq!(f.restore, Some(mine.clone()));
        assert_eq!(f.show, None);
        j.restored();
        // Or a press in that same frame, before the frame's restore.
        j.on_rules(false, rules(0b1));
        let _ = j.loadout_frame(true, || mine.clone());
        let (restore, c) = j.confirm(false, false, false, sentinel);
        assert_eq!((restore, c), (Some(mine), Confirm::Original));
    }

    #[test]
    fn a_lost_link_mid_run_keeps_the_run_settings_until_the_menu() {
        let mut j = Joiner::on();
        let mine = Own::default();
        j.on_rules(true, rules(0b11));
        let _ = j.confirm(true, true, false, || mine.clone());
        j.confirm_done();
        j.host_gone();
        assert_eq!(
            j.held_rules(),
            Some(&rules(0b11)),
            "the run goes on with them"
        );
        assert_eq!(j.menu_frame(), Some(mine));
    }

    #[test]
    fn reset_progress_while_held_becomes_the_own_picks() {
        let mut j = Joiner::on();
        assert_eq!(j.reset_progress(0), None, "not held");
        j.on_rules(true, rules(0b11));
        let _ = j.confirm(true, true, false, || Own {
            mods: 0b1111,
            loadout_daily: Some(pick(2)),
        });
        j.confirm_done();
        assert_eq!(
            j.reset_progress(0),
            Some(rules(0b11)),
            "the run's settings go back in"
        );
        assert_eq!(
            j.menu_frame(),
            Some(Own {
                mods: 0,
                loadout_daily: Some(pick(2))
            })
        );
    }

    /// A model of the game as the rule sees it: flags in memory, the save, the
    /// resume file, the current screen, and the host. Every `confirmLoadout`
    /// write goes through [`Sim::confirm`], every resume-file write through
    /// [`Sim::save_run`].
    #[allow(clippy::struct_excessive_bools)] // the model's independent facts
    #[derive(Debug, Clone)]
    struct Sim {
        j: Joiner,
        mem: u16,
        disk: u16,
        /// What the player chose (the last own toggle, or a reset).
        chosen: u16,
        linked: bool,
        room: Room,
        host: u16,
        host_started: bool,
        /// The game made its run from the host's current start.
        used_start: bool,
        /// The flags `configureDifficulty` read at the Start.
        start_rules: Option<u16>,
        /// The flags the run's objects read when the fade ended.
        run_rules: Option<u16>,
        /// The co-op run latch (`save_protect`): set at the run's first
        /// frame when linked, cleared at its end; no grace period.
        latch: bool,
        /// A resume-file write carried flags the player did not choose.
        resume_bad: bool,
        /// A run's objects read other flags than its Start did.
        mixed: bool,
        /// The next write of the own picks back fails.
        fail_restore: bool,
        /// The last write of the own picks back failed.
        restore_failed: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Room {
        Menu,
        Loadout,
        /// The new game screen's 10 to 20 frames after a Start.
        Fade,
        /// The new game screen's 10 frames back to the menu (`alarm[1]`):
        /// the game's own `confirmLoadout` does nothing.
        Leaving,
        Run,
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Link,
        Unlink,
        HostPicks(u16),
        HostStart,
        HostMenu,
        ToLoadout,
        /// Back from the new game screen: its fade to the menu begins.
        Back,
        ToMenu,
        Row(u8),
        Start,
        FadeEnds,
        /// `triggerSaveRun` (a wave, a pause, a focus loss, a timer).
        SaveRun,
        RunEnds,
        Reset,
        /// Game End (`triggerSaveRun`), then the game is gone.
        Close,
        FailRestore,
    }

    const OPS: [Op; 17] = [
        Op::Link,
        Op::Unlink,
        Op::HostPicks(0b1010),
        Op::HostStart,
        Op::HostMenu,
        Op::ToLoadout,
        Op::Back,
        Op::ToMenu,
        Op::Row(1),
        Op::Row(4),
        Op::Start,
        Op::FadeEnds,
        Op::SaveRun,
        Op::RunEnds,
        Op::Reset,
        Op::Close,
        Op::FailRestore,
    ];

    impl Sim {
        fn new() -> Self {
            Self {
                j: Joiner::on(),
                mem: 0b1,
                disk: 0b1,
                chosen: 0b1,
                linked: false,
                room: Room::Menu,
                host: 0b0110,
                host_started: false,
                used_start: false,
                start_rules: None,
                run_rules: None,
                latch: false,
                resume_bad: false,
                mixed: false,
                fail_restore: false,
                restore_failed: false,
            }
        }

        fn own(&self) -> Own {
            Own {
                mods: self.mem,
                loadout_daily: None,
            }
        }

        /// The own picks written back, as the glue does it: `false` = the
        /// write failed and the copy stays kept.
        fn restore(&mut self, o: Option<Own>) -> bool {
            let Some(o) = o else { return true };
            self.restore_failed = std::mem::take(&mut self.fail_restore);
            if self.restore_failed {
                return false;
            }
            self.mem = o.mods;
            self.j.restored();
            true
        }

        /// The new game screen's or the menu's frame, as the before-hooks
        /// run them.
        fn frame(&mut self) {
            match self.room {
                Room::Loadout | Room::Fade | Room::Leaving => {
                    let _ = self.j.frame_tick();
                    let own = self.own();
                    let f = self.j.loadout_frame(self.linked, || own);
                    self.restore(f.restore);
                    if let Some(r) = f.show {
                        self.mem = r.mods;
                    }
                    if self.j.start_due() {
                        self.confirm(0);
                    }
                }
                Room::Menu => {
                    let _ = self.j.frame_tick();
                    let o = self.j.menu_frame();
                    self.restore(o);
                }
                Room::Run => {}
            }
        }

        /// `confirmLoadout` and its writes.
        fn confirm(&mut self, pos: u8) {
            let pos0 = pos == 0;
            let own = self.own();
            let (restore, c) = self.j.confirm(self.linked, pos0, false, || own);
            if !self.restore(restore) {
                // The own picks could not be written back: the press is skipped.
                self.j.confirm_done();
                return;
            }
            match c {
                // The game's own does nothing during the fade (`alarm[2]`)
                // or the fade back to the menu (`alarm[1]`).
                Confirm::Original if matches!(self.room, Room::Fade | Room::Leaving) => {}
                // The host's are written, the game's own does nothing: the
                // after-hook puts the host's start back.
                Confirm::Apply(r) if self.room == Room::Leaving => {
                    self.mem = r.mods;
                    self.j.start_not_taken();
                }
                Confirm::Original => {
                    if pos0 {
                        self.room = Room::Fade;
                        self.start_rules = Some(self.mem);
                        // The after-hook saw the fade start.
                        self.j.started_own();
                    } else {
                        self.mem ^= 1 << (pos - 1);
                        self.chosen = self.mem;
                    }
                    // Every press writes the 12 picks.
                    if !self.j.skip_writes() {
                        self.disk = self.mem;
                    }
                }
                Confirm::Apply(r) => {
                    self.mem = r.mods;
                    self.room = Room::Fade;
                    self.used_start = true;
                    self.start_rules = Some(self.mem);
                    if !self.j.skip_writes() {
                        self.disk = self.mem;
                    }
                }
                Confirm::Locked
                | Confirm::ResumeBlocked
                | Confirm::Wait
                | Confirm::Unready
                | Confirm::Starting => {}
            }
            self.j.confirm_done();
        }

        /// `triggerSaveRun`: skipped while latched or while the host's are
        /// held; the game's own guard writes only during a run.
        fn save_run(&mut self) {
            if self.latch || self.j.held() {
                return;
            }
            if self.room == Room::Run && self.mem != self.chosen {
                self.resume_bad = true;
            }
        }

        /// The link and the host's messages.
        fn host_op(&mut self, op: Op) {
            match op {
                Op::Link if !self.linked && self.room != Room::Run => {
                    self.linked = true;
                    self.host_started = false;
                    // The host sends its picks at a link.
                    self.j.on_rules(false, rules(self.host));
                }
                Op::Unlink if self.linked => {
                    self.linked = false;
                    self.host_started = false;
                    self.j.host_gone();
                }
                Op::HostPicks(m) if self.linked && !self.host_started => {
                    self.host ^= m;
                    self.j.on_rules(false, rules(self.host));
                }
                Op::HostStart if self.linked && !self.host_started => {
                    self.host_started = true;
                    self.used_start = false;
                    self.j.on_rules(true, rules(self.host));
                }
                Op::HostMenu if self.linked && self.host_started => {
                    self.host_started = false;
                    self.used_start = false;
                    self.j.on_rules(false, rules(self.host));
                }
                _ => {}
            }
        }

        fn apply(&mut self, op: Op) {
            match op {
                Op::Link | Op::Unlink | Op::HostPicks(_) | Op::HostStart | Op::HostMenu => {
                    self.host_op(op);
                }
                Op::ToLoadout => {
                    if self.room == Room::Menu {
                        self.room = Room::Loadout;
                    }
                }
                Op::Back => {
                    if self.room == Room::Loadout {
                        // `leaveLoadout`: nothing waits.
                        self.j.cancel();
                        self.room = Room::Leaving;
                    }
                }
                Op::ToMenu => {
                    if self.room == Room::Loadout {
                        self.j.cancel();
                        self.room = Room::Menu;
                    } else if self.room == Room::Leaving {
                        self.room = Room::Menu;
                    }
                }
                Op::Row(i) => {
                    if matches!(self.room, Room::Loadout | Room::Fade | Room::Leaving) {
                        self.confirm(i);
                    }
                }
                Op::Start => {
                    if matches!(self.room, Room::Loadout | Room::Fade | Room::Leaving) {
                        self.confirm(0);
                    }
                }
                Op::FadeEnds => {
                    if self.room == Room::Fade {
                        self.room = Room::Run;
                        self.run_rules = Some(self.mem);
                        self.latch = self.linked;
                        if self.start_rules != self.run_rules {
                            self.mixed = true;
                        }
                    }
                }
                Op::SaveRun => self.save_run(),
                Op::RunEnds => {
                    if self.room == Room::Run {
                        // The end screen writes records, not settings.
                        self.room = Room::Menu;
                        self.start_rules = None;
                        self.run_rules = None;
                        self.latch = false;
                    }
                }
                // resetProgress (from the menu, or the pause screen in a
                // run): defaults to memory and the save.
                Op::Reset if matches!(self.room, Room::Menu | Room::Run) => {
                    self.mem = 0;
                    self.disk = 0;
                    self.chosen = 0;
                    if let Some(r) = self.j.reset_progress(self.mem) {
                        self.mem = r.mods;
                    }
                }
                Op::Close => {
                    // Game End saves the run first; the next boot reads the save.
                    self.save_run();
                    let (disk, chosen, bad) = (self.disk, self.chosen, self.resume_bad);
                    *self = Self::new();
                    self.mem = disk;
                    self.disk = disk;
                    self.chosen = chosen;
                    self.resume_bad = bad;
                }
                Op::Reset => {}
                Op::FailRestore => self.fail_restore = true,
            }
            self.frame();
        }

        fn check(&self, trail: &[Op]) {
            assert_eq!(
                self.disk, self.chosen,
                "the save holds only what the player chose: {trail:?}"
            );
            assert!(
                !self.resume_bad,
                "the resume file holds only what the player chose: {trail:?}"
            );
            assert!(
                !self.mixed,
                "a run is made from one set of settings: {trail:?}"
            );
            if !self.j.held() {
                assert_eq!(
                    self.mem, self.chosen,
                    "nothing held: the own picks in memory: {trail:?}"
                );
            }
            if self.room == Room::Menu && !self.restore_failed {
                assert!(
                    !self.j.held(),
                    "the menu gives the own picks back: {trail:?}"
                );
            }
            if self.room == Room::Loadout && !self.linked && !self.restore_failed {
                assert!(
                    !self.j.held(),
                    "unlinked new game screen: the own picks: {trail:?}"
                );
            }
            if self.room == Room::Fade && self.j.held() {
                assert_eq!(
                    Some(self.mem),
                    self.start_rules,
                    "the fade keeps the start's settings: {trail:?}"
                );
            }
            if self.room == Room::Run && self.j.held() {
                assert_eq!(
                    Some(self.mem),
                    self.run_rules,
                    "a run keeps the settings it was made from: {trail:?}"
                );
            }
            if self.linked
                && self.host_started
                && !self.used_start
                && matches!(self.room, Room::Menu | Room::Loadout | Room::Leaving)
            {
                assert!(
                    self.j.host_started(),
                    "the host's start waits until it is taken: {trail:?}"
                );
            }
        }
    }

    /// Every sequence of 6 steps (17^6), each followed by the frame that
    /// runs after it.
    #[test]
    fn no_sequence_leaves_a_flag_in_the_save_the_player_did_not_choose() {
        fn walk(sim: &Sim, trail: &mut Vec<Op>, depth: usize, n: &mut u64) {
            if depth == 0 {
                *n += 1;
                return;
            }
            for op in OPS {
                let mut s = sim.clone();
                trail.push(op);
                s.apply(op);
                s.check(trail);
                walk(&s, trail, depth - 1, n);
                trail.pop();
            }
        }
        let mut n = 0;
        walk(&Sim::new(), &mut Vec::new(), 6, &mut n);
        assert_eq!(n, 17u64.pow(6));
    }

    /// Long random sequences (a fixed LCG, reproducible).
    #[test]
    fn long_random_sequences_keep_the_rule() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..4000 {
            let mut s = Sim::new();
            let mut trail = Vec::new();
            for _ in 0..80 {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let op = OPS[usize::try_from(x >> 59).unwrap_or(0) % OPS.len()];
                trail.push(op);
                s.apply(op);
                s.check(&trail);
            }
        }
    }

    fn run_ops(ops: &[Op]) -> Sim {
        let mut s = Sim::new();
        for &op in ops {
            s.apply(op);
            s.check(ops);
        }
        s
    }

    #[test]
    fn a_run_from_the_host_start_holds_its_settings_whatever_the_joiner_picked() {
        let mut s = run_ops(&[
            Op::Link,
            Op::HostPicks(0b1010),
            Op::ToLoadout,
            Op::Row(1),
            Op::Start,
            Op::HostStart,
            Op::FadeEnds,
        ]);
        assert_eq!(s.room, Room::Run);
        assert_eq!(s.mem, 0b1100, "the host's");
        assert_eq!(s.disk, 0b1, "the own picks untouched (the row was locked)");
        s.apply(Op::Close);
        assert_eq!(
            s.mem, 0b1,
            "closed mid-run: the next boot has the own picks"
        );
    }

    /// A run started with the host's held and the link lost before the first
    /// run frame (no latch): the resume file stays off all the same.
    #[test]
    fn a_held_run_without_the_latch_writes_no_resume_file() {
        let s = run_ops(&[
            Op::Link,
            Op::ToLoadout,
            Op::HostStart,
            Op::Start,
            Op::Unlink,
            Op::FadeEnds,
            Op::SaveRun,
            Op::Close,
        ]);
        assert!(!s.resume_bad);
        let s = run_ops(&[
            Op::Link,
            Op::ToLoadout,
            Op::HostStart,
            Op::Start,
            Op::Unlink,
            Op::FadeEnds,
        ]);
        assert!(!s.latch && s.j.held(), "the window the latch misses");
    }

    /// A failed write-back skips the next press and is tried again at the
    /// next frame.
    #[test]
    fn a_failed_write_back_keeps_the_own_copy_and_skips_the_press() {
        let mut s = run_ops(&[Op::Link, Op::ToLoadout, Op::Unlink]);
        assert!(!s.j.held(), "written back at the unlinked frame");
        s = run_ops(&[Op::Link, Op::ToLoadout]);
        assert!(s.j.held());
        s.linked = false;
        s.j.host_gone();
        s.fail_restore = true;
        s.confirm(1);
        assert_eq!(s.disk, 0b1, "the press was skipped");
        assert!(s.j.held(), "the own copy kept");
        s.frame();
        assert!(!s.j.held());
        assert_eq!(s.mem, 0b1);
    }
}
