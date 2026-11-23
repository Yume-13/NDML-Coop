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

//! What the co-op UI says during a run, chosen from the session's facts (pure).
//! Five primitives in the game's own styles: P1 hint bar ([`Bar`]), P2
//! wave-alert-style message ([`AlertView`]), P3 HUD line ([`Hud`]), P4
//! pause-screen relabel ([`PauseText`]) and P5 waiting card ([`Card`]).
//! [`RunMessages`] keeps the timers.
//!
//! A lost link never ends the wait by itself: "no signal" on the HUD line, then
//! the joining game waits on its pause screen and the host counts down to it (`crate::wait`).

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use super::lan::bad_link;
use crate::gate::GatePhase;
use crate::wait::WaitView;

/// How long a P1 note stays after an event.
pub const NOTE_FOR: Duration = Duration::from_secs(6);
/// How long the start note and the session notes stay.
pub const START_NOTE_FOR: Duration = Duration::from_secs(4);
/// The alert holds this many steps before it fades (as `oWaveAlert`).
pub const ALERT_HOLD_STEPS: u32 = 240;
/// Alpha lost per step while fading.
pub const ALERT_FADE: f64 = 0.0075;
/// Silence from a linked partner after which the HUD line says "no signal"
/// (short dropouts of a home connection stay quiet).
pub const NO_SIGNAL_AFTER: Duration = Duration::from_secs(2);
/// Silence after which the run waits for the link ([`crate::wait`]).
pub const WAITING_AFTER: Duration = crate::wait::WAIT_AFTER;

/// How long nothing has arrived from the partner (the received packet count,
/// keepalives included, sampled every frame), and when this game last woke
/// from its own stall (a frame gap of `wait::OWN_STALL` or more).
#[derive(Debug, Clone, Copy, Default)]
pub struct SilenceWatch {
    last: Option<(u64, Instant)>,
    frame: Option<Instant>,
    woke: Option<Instant>,
}

impl SilenceWatch {
    /// The silence so far, given the partner's packet count (`None`: none).
    pub fn update(&mut self, received: Option<u64>, now: Instant) -> Option<Duration> {
        if self
            .frame
            .is_some_and(|t| now.saturating_duration_since(t) >= crate::wait::OWN_STALL)
        {
            self.woke = Some(now);
        }
        self.frame = Some(now);
        let Some(n) = received else {
            self.last = None;
            return None;
        };
        match self.last {
            Some((m, since)) if m == n => Some(now.saturating_duration_since(since)),
            _ => {
                self.last = Some((n, now));
                Some(Duration::ZERO)
            }
        }
    }

    /// When this game last woke from its own stall.
    #[must_use]
    pub fn woke(&self) -> Option<Instant> {
        self.woke
    }
}

/// P4: the pause screen's strings for this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseText {
    /// Row 0 ("Resume Run") relabelled.
    pub row0: Option<String>,
    /// `confirmPause` on row 0 is refused (with the move sound).
    pub row0_blocked: bool,
    /// The last row and its YES/NO question.
    pub quit: Option<(String, String)>,
}

/// P1: a hint bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bar {
    pub text: String,
    /// "In progress": a slow alpha pulse.
    pub pulse: bool,
    /// In the middle of the screen (the waiting card's fallback).
    pub centre: bool,
}

impl Bar {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            pulse: false,
            centre: false,
        }
    }
    fn pulsing(mut self) -> Self {
        self.pulse = true;
        self
    }
}

/// P3: the link line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hud {
    pub name: String,
    /// The number part (`42 ms`) or a word.
    pub detail: Option<String>,
    /// Draw `detail` in the negative colour.
    pub bad: bool,
}

/// P5: the waiting card over a dimmed screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// `fontTitle`, upper case.
    pub title: String,
    /// `fontMedium`.
    pub subtitle: String,
    /// `fontSmall`, optional.
    pub note: Option<String>,
}

/// P2: the alert this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertView {
    pub text: String,
    pub scale: f64,
    pub alpha: f64,
}

/// Everything in-run for one frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunUi {
    pub pause: Option<PauseText>,
    pub bars: Vec<Bar>,
    pub hud: Option<Hud>,
    pub card: Option<Card>,
    pub alert: Option<AlertView>,
}

/// Shared pause: play resumes when every player has resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedPause {
    /// This player is on the pause menu, and so is the partner.
    BothPaused,
    /// This player is on the pause menu; the partner has resumed and waits.
    PartnerReady,
    /// This player resumed; the partner is still paused (held on the pause screen).
    WaitingForPartner,
}

/// Upgrade wait gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeWait {
    /// This player is done; the partner is still choosing (world frozen).
    PartnerChoosing,
    /// This player is choosing; the partner is done.
    PartnerReady,
    /// As `PartnerChoosing`, past `upgrade_wait_seconds`: Esc stops waiting.
    PartnerChoosingLong,
    /// Nothing to choose (or down): held on the pause screen meanwhile.
    HeldPaused,
    /// As `HeldPaused`, past `upgrade_wait_seconds`: Esc stops waiting.
    HeldPausedLong,
    /// This player paused while the partner opened the upgrade screen.
    ResumeToChoose,
}

/// What the session rules (`crate::rules`) want shown this frame; what
/// [`RunFacts`] passes wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionFacts {
    pub pause: Option<SharedPause>,
    pub upgrade: Option<UpgradeWait>,
    /// This player's ship is down: whole seconds until it respawns (game time).
    pub me_down: Option<u64>,
    /// ... alone (no partner here): it respawns where it went down.
    pub me_down_alone: bool,
    /// The partner's ship is down: whole seconds until it respawns.
    pub partner_down: Option<u64>,
}

/// A one-off session event (alert or note).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// This player's ship went down.
    Down,
    /// The partner's ship went down.
    PartnerDown,
    /// This player's ship respawned.
    Back,
    /// The partner's ship respawned.
    PartnerBack,
    /// Every ship is down: the run is over.
    AllDown,
}

/// The session as the run UI needs it.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // independent facts about the session
pub struct RunFacts<'a> {
    pub coop_run: bool,
    pub linked: bool,
    pub gate: GatePhase,
    /// Display name (already made drawable).
    pub partner: &'a str,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub shared_pause: Option<SharedPause>,
    pub upgrade: Option<UpgradeWait>,
    /// While linked: how long nothing has arrived from the partner.
    pub silent_for: Option<Duration>,
    /// Host: the partner left this run; it plays on alone until the run ends.
    pub separate: bool,
    /// The wait for a lost link ([`crate::wait`]).
    pub wait: WaitView,
    /// The pause screen is up.
    pub paused: bool,
    /// This game hosts (or hosted when last linked).
    pub host: bool,
    /// Joining game: its run is over (the host is gone for good).
    pub over: bool,
}

/// Something happened to the partner's part in this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// Host: the partner is gone for good.
    Left { name: String },
    /// The link is back after a wait; `long`: it became the shared pause.
    Back { long: bool },
}

/// The config's switches for the run messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    /// The HUD line (P3; `overlay`).
    pub hud: bool,
}

impl Default for Switches {
    fn default() -> Self {
        Self { hud: true }
    }
}

/// Start gate, partner not in yet.
#[must_use]
pub fn start_gate(partner: &str) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some(format!("Waiting for {partner}...")),
            row0_blocked: true,
            quit: None,
        },
        Bar::new(format!(
            "The run starts for both of you when {partner} arrives."
        ))
        .pulsing(),
    )
}

/// The run is refused: the game modes differ.
#[must_use]
pub fn modes_differ() -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some("Modes differ".into()),
            row0_blocked: true,
            quit: None,
        },
        Bar::new("Both players must pick the same game mode. Quit and pick it again together."),
    )
}

/// Shared pause: this player resumed; the partner is still paused.
#[must_use]
pub fn pause_waiting(partner: &str) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some(format!("Waiting for {partner}...")),
            row0_blocked: true,
            quit: None,
        },
        Bar::new(format!("{partner} is still paused.")),
    )
}

/// Shared pause: both players are on their pause menus.
#[must_use]
pub fn both_paused(partner: &str) -> Bar {
    Bar::new(format!(
        "{partner} is paused too. Play resumes when you have both resumed."
    ))
}

/// Shared pause: the partner has resumed and waits.
#[must_use]
pub fn pause_partner_ready(partner: &str) -> Bar {
    Bar::new(format!("{partner} is ready and waiting for you."))
}

/// Upgrade screen: this player is done, the partner is still choosing.
#[must_use]
pub fn upgrade_waiting(partner: &str) -> Card {
    Card {
        title: "WAITING".into(),
        subtitle: format!("Waiting for {partner}..."),
        note: Some(format!("{partner} is choosing an upgrade.")),
    }
}

/// Upgrade screen: this player is choosing, the partner is done.
#[must_use]
pub fn upgrade_partner_ready(partner: &str) -> Bar {
    Bar::new(format!("{partner} is ready and waiting for you."))
}

/// Upgrade screen: this player is done, the partner has been choosing long.
#[must_use]
pub fn upgrade_waiting_long(partner: &str) -> Card {
    Card {
        title: "WAITING".into(),
        subtitle: format!("Waiting for {partner}..."),
        note: Some(format!("Press Esc to play on. {partner} can still choose.")),
    }
}

/// Upgrade screen open for the partner; this game waits on the pause screen.
#[must_use]
pub fn upgrade_held_paused(partner: &str, long: bool) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some(if long {
                "Play On".to_owned()
            } else {
                format!("Waiting for {partner}...")
            }),
            row0_blocked: !long,
            quit: None,
        },
        Bar::new(if long {
            format!("{partner} is choosing an upgrade. Press Esc to play on.")
        } else {
            format!("{partner} is choosing an upgrade.")
        })
        .pulsing(),
    )
}

/// This player paused while the partner opened the upgrade screen.
#[must_use]
pub fn resume_to_choose(partner: &str) -> Bar {
    Bar::new(format!(
        "{partner} is choosing an upgrade. Resume to choose yours."
    ))
}

/// This player's ship is down (`None`: alone, it comes back where it went down).
#[must_use]
pub fn you_are_down(partner: Option<&str>, secs: u64) -> Bar {
    Bar::new(match partner {
        Some(p) => format!("Your ship is down. Back next to {p} in {secs} s."),
        None => format!("Your ship is down. Back in {secs} s."),
    })
    .pulsing()
}

/// The partner's ship is down.
#[must_use]
pub fn partner_is_down(partner: &str, secs: u64) -> Bar {
    Bar::new(format!("{partner} is down. Back in {secs} s.")).pulsing()
}

/// A ship respawned (`None`: this player's).
#[must_use]
pub fn back_in_the_fight(partner: Option<&str>) -> Bar {
    Bar::new(match partner {
        Some(p) => format!("{p} is back in the fight."),
        None => "Back in the fight.".into(),
    })
}

/// Every ship is down.
#[must_use]
pub fn all_down() -> Bar {
    Bar::new("Both ships are down. The run is over.")
}

/// The alert for a ship going down (`None`: this player's).
#[must_use]
pub fn down_alert(partner: Option<&str>) -> String {
    match partner {
        Some(p) => super::text::upper(&format!("{p} is down")),
        None => "SHIP DOWN".into(),
    }
}

/// The pause screen's last row in a co-op run and its question (co-op runs
/// are not saved). A host leaving ends the partner's run too.
#[must_use]
pub fn leave_run(partner: &str, linked: bool, at_gate: bool, host: bool) -> (String, String) {
    if linked {
        (
            "Leave Co-op Run".into(),
            if at_gate {
                "Leave this co-op run?".into()
            } else if host {
                format!("Leave this co-op run? {partner}'s run ends too.")
            } else {
                format!("Leave this co-op run? {partner} keeps playing.")
            },
        )
    } else {
        (
            "Quit Run".into(),
            "Quit this run? Co-op runs are not saved.".into(),
        )
    }
}

/// Host: the link is lost; play goes on and pauses in `secs`.
#[must_use]
pub fn playing_on(partner: &str, secs: u64) -> Bar {
    Bar::new(format!("No signal from {partner}. Pausing in {secs} s...")).pulsing()
}

/// Waiting for the link on the pause screen; the bar names the way out.
#[must_use]
pub fn link_wait(partner: &str) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some(format!("Waiting for {partner}...")),
            row0_blocked: true,
            quit: None,
        },
        Bar::new(format!(
            "Lost the link to {partner}. Leave Co-op Run returns to the menu."
        ))
        .pulsing(),
    )
}

/// Waiting for the link while the upgrade screen is open.
#[must_use]
pub fn link_wait_unpaused(partner: &str) -> Bar {
    Bar::new(format!(
        "Waiting for {partner}'s connection. Leave Co-op Run is in the pause menu after this \
         screen."
    ))
    .pulsing()
}

/// Joining game: the host is gone for good and this run is over.
#[must_use]
pub fn run_over(partner: &str) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some("The run is over".into()),
            row0_blocked: true,
            quit: None,
        },
        Bar::new(format!("{partner} left. The run is over.")),
    )
}

/// The partner is gone for this run (left for good, or left for the menu).
#[must_use]
pub fn partner_left() -> Bar {
    Bar::new("Playing on alone. Saves stay off for this run.")
}

/// The link is back. `long`: the wait became the shared pause.
#[must_use]
pub fn partner_back(partner: &str, long: bool) -> Bar {
    Bar::new(if long {
        format!("{partner} is back. Play resumes when you have both resumed.")
    } else {
        format!("{partner} is back.")
    })
}

/// The HUD line while nothing arrives from the partner.
#[must_use]
pub fn hud_no_signal(partner: &str) -> Hud {
    Hud {
        name: partner.to_owned(),
        detail: Some("no signal".into()),
        bad: true,
    }
}

/// Host: the HUD line while linked with a partner who left this run.
#[must_use]
pub fn hud_next_run(partner: &str) -> Hud {
    Hud {
        name: partner.to_owned(),
        detail: Some("co-op from the next run".into()),
        bad: false,
    }
}

/// The HUD line while the link is down and being retried.
#[must_use]
pub fn hud_reconnecting(partner: &str) -> Hud {
    Hud {
        name: partner.to_owned(),
        detail: Some("reconnecting...".into()),
        bad: true,
    }
}

/// A co-op run starts.
#[must_use]
pub fn coop_started(partner: &str) -> Bar {
    Bar::new(format!("Co-op with {partner}"))
}

/// The link line.
#[must_use]
pub fn hud(partner: &str, rtt_ms: Option<f64>, loss_pct: Option<f64>) -> Hud {
    Hud {
        name: partner.to_owned(),
        detail: rtt_ms.map(|r| format!("{r:.0} ms")),
        bad: rtt_ms.is_some_and(|r| bad_link(r, loss_pct.unwrap_or(0.0))),
    }
}

/// The link line on the loadout screen, in the run's words.
#[must_use]
pub fn loadout_hud(
    partner: &str,
    linked: bool,
    reconnecting: bool,
    rtt_ms: Option<f64>,
    loss_pct: Option<f64>,
    silent_for: Option<Duration>,
) -> Option<Hud> {
    if linked {
        Some(if silent_for.is_some_and(|s| s >= NO_SIGNAL_AFTER) {
            hud_no_signal(partner)
        } else {
            hud(partner, rtt_ms, loss_pct)
        })
    } else {
        reconnecting.then(|| hud_reconnecting(partner))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Alert {
    text: String,
    step: u32,
    scale: f64,
    alpha: f64,
}

impl Alert {
    fn new(text: String) -> Self {
        Self {
            text,
            step: 0,
            scale: 0.001,
            alpha: 1.0,
        }
    }

    /// One step of the wave alert's motion; `None` once faded out.
    fn advance(&mut self) -> Option<AlertView> {
        self.step += 1;
        self.scale += (1.0 - self.scale) / 7.5;
        if self.step > ALERT_HOLD_STEPS {
            self.alpha -= ALERT_FADE;
        }
        (self.alpha > 0.0).then(|| AlertView {
            text: self.text.clone(),
            scale: self.scale,
            alpha: self.alpha,
        })
    }
}

/// The run messages' timers and state.
#[derive(Debug, Clone, Default)]
pub struct RunMessages {
    running_since: Option<Instant>,
    /// The run waits for the link (whose, since when).
    lost: Option<(String, Instant)>,
    /// The next return is a long one (the shared pause).
    back_long: bool,
    /// The partner left this run for good (since when).
    left: Option<(String, Instant)>,
    /// "Sam is back" (text, since when).
    back: Option<(Bar, Instant)>,
    alert: Option<Alert>,
    /// The session rules' state this frame (`set_session`).
    session: SessionFacts,
    /// Session events not shown yet.
    session_events: Vec<SessionEvent>,
    /// A session note ("Back in the fight.") and since when.
    session_note: Option<(Bar, Instant)>,
}

impl RunMessages {
    /// A new run (or the menu): forget everything.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The session rules' state, set every frame before the GUI draw.
    pub fn set_session(&mut self, facts: SessionFacts) {
        self.session = facts;
    }

    /// A session event: shown at the next frame.
    pub fn session_event(&mut self, ev: SessionEvent) {
        self.session_events.push(ev);
    }

    /// Alerts and notes for the session events, and the session's bars.
    fn session_bars(&mut self, name: &str, ui: &mut RunUi, now: Instant) {
        for ev in std::mem::take(&mut self.session_events) {
            match ev {
                SessionEvent::Down => self.alert = Some(Alert::new(down_alert(None))),
                SessionEvent::PartnerDown => {
                    self.alert = Some(Alert::new(down_alert(Some(name))));
                }
                SessionEvent::Back => self.session_note = Some((back_in_the_fight(None), now)),
                SessionEvent::PartnerBack => {
                    self.session_note = Some((back_in_the_fight(Some(name)), now));
                }
                SessionEvent::AllDown => self.session_note = Some((all_down(), now)),
            }
        }
        if let Some(s) = self.session.me_down {
            let next_to = (!self.session.me_down_alone).then_some(name);
            ui.bars.push(you_are_down(next_to, s));
        }
        if let Some(s) = self.session.partner_down {
            ui.bars.push(partner_is_down(name, s));
        }
        if let Some((bar, at)) = &self.session_note {
            if now.saturating_duration_since(*at) < START_NOTE_FOR {
                ui.bars.push(bar.clone());
            } else {
                self.session_note = None;
            }
        }
    }

    /// Something happened to the partner's part in this run.
    pub fn event(&mut self, ev: LinkEvent, now: Instant) {
        if !matches!(ev, LinkEvent::Back { .. }) {
            // Gone for this run: no "is back" note beside it.
            self.back = None;
            self.back_long = false;
        }
        match ev {
            LinkEvent::Left { name } => {
                self.alert = Some(Alert::new(super::text::upper(&format!("{name} left"))));
                self.lost = None;
                self.left = Some((name, now));
            }
            LinkEvent::Back { long } => self.back_long = long,
        }
    }

    /// The UI for this frame (call once per GUI draw: the alert advances).
    pub fn frame(&mut self, f: &RunFacts<'_>, sw: Switches, now: Instant) -> RunUi {
        let mut ui = RunUi::default();
        if !(f.coop_run || f.linked) {
            self.reset();
            return ui;
        }
        let name = f.partner;
        if f.over && f.coop_run {
            // The joining player's run is over: only that, until the menu.
            let (p, b) = run_over(name);
            ui.pause = Some(p);
            ui.bars.push(b);
            if let Some(p) = &mut ui.pause {
                p.quit = Some(leave_run(name, false, false, false));
            }
            self.finish(&mut ui);
            return ui;
        }
        let waiting = f.wait != WaitView::None;
        self.track_wait(f, now);
        match f.gate {
            GatePhase::Holding if f.coop_run && waiting => {
                let (p, b) = link_wait(name);
                ui.pause = Some(p);
                ui.bars.push(b);
            }
            GatePhase::Holding if f.linked && f.coop_run => {
                let (p, b) = start_gate(name);
                ui.pause = Some(p);
                ui.bars.push(b);
            }
            GatePhase::Refused { .. } if f.coop_run => {
                let (p, b) = modes_differ();
                ui.pause = Some(p);
                ui.bars.push(b);
            }
            GatePhase::Running if f.coop_run && f.linked && !f.separate => {
                let since = *self.running_since.get_or_insert(now);
                if now.saturating_duration_since(since) < START_NOTE_FOR {
                    ui.bars.push(coop_started(name));
                }
            }
            _ => {}
        }
        if ui.pause.is_none() {
            match f.shared_pause.or(self.session.pause) {
                Some(SharedPause::WaitingForPartner) => {
                    let (p, b) = pause_waiting(name);
                    ui.pause = Some(p);
                    ui.bars.push(b);
                }
                Some(SharedPause::PartnerReady) => ui.bars.push(pause_partner_ready(name)),
                Some(SharedPause::BothPaused) => ui.bars.push(both_paused(name)),
                None => {}
            }
        }
        match f.upgrade.or(self.session.upgrade) {
            Some(UpgradeWait::PartnerChoosing) => ui.card = Some(upgrade_waiting(name)),
            Some(UpgradeWait::PartnerChoosingLong) => ui.card = Some(upgrade_waiting_long(name)),
            Some(UpgradeWait::PartnerReady) => ui.bars.push(upgrade_partner_ready(name)),
            Some(w @ (UpgradeWait::HeldPaused | UpgradeWait::HeldPausedLong)) => {
                let (p, b) = upgrade_held_paused(name, w == UpgradeWait::HeldPausedLong);
                // The held pause screen says why, over a partner's pause text.
                ui.pause = Some(p);
                ui.bars.push(b);
            }
            Some(UpgradeWait::ResumeToChoose) => ui.bars.push(resume_to_choose(name)),
            None => {}
        }
        self.session_bars(name, &mut ui, now);
        wait_bars(f, &mut ui);
        if f.coop_run {
            pause_rows_text(f, waiting, &mut ui);
        }
        self.link_bars(f, &mut ui, now);
        let silent = f.silent_for.filter(|_| f.linked && f.coop_run);
        if sw.hud && f.linked {
            ui.hud = Some(if f.coop_run && f.separate {
                hud_next_run(name)
            } else if silent.is_some_and(|s| s >= NO_SIGNAL_AFTER) {
                hud_no_signal(name)
            } else if f.coop_run {
                hud(name, f.rtt_ms, f.loss_pct)
            } else {
                Hud {
                    name: name.to_owned(),
                    detail: Some("co-op from the next run".into()),
                    bad: false,
                }
            });
        } else if sw.hud && f.coop_run && waiting {
            ui.hud = Some(hud_reconnecting(name));
        }
        self.finish(&mut ui);
        ui
    }

    /// The wait's start and end: CONNECTION LOST while the host plays on
    /// (never over the pause screen), "Sam is back." at the return.
    fn track_wait(&mut self, f: &RunFacts<'_>, now: Instant) {
        if f.wait != WaitView::None {
            if self.lost.is_none() {
                self.lost = Some((f.partner.to_owned(), now));
                self.back = None;
                if matches!(f.wait, WaitView::PlayingOn { .. }) && !f.paused {
                    self.alert = Some(Alert::new("CONNECTION LOST".into()));
                }
            }
        } else if let Some((who, _)) = self.lost.take() {
            if f.linked && !f.separate {
                self.back = Some((partner_back(&who, self.back_long), now));
            }
            self.back_long = false;
        }
    }
}

/// A co-op run's pause menu: the last row ("Leave Co-op Run" or "Quit Run").
fn pause_rows_text(f: &RunFacts<'_>, waiting: bool, ui: &mut RunUi) {
    // Held at the start gate (not a wait): the partner has not arrived.
    let at_gate = matches!(f.gate, GatePhase::Holding) && !waiting;
    let quit = Some(leave_run(
        f.partner,
        (f.linked || waiting) && !f.separate,
        at_gate,
        f.host,
    ));
    let p = ui.pause.get_or_insert(PauseText {
        row0: None,
        row0_blocked: false,
        quit: None,
    });
    p.quit = quit;
}

/// While the run waits: the host's countdown, or the frozen game's waiting text.
fn wait_bars(f: &RunFacts<'_>, ui: &mut RunUi) {
    let name = f.partner;
    match f.wait {
        WaitView::None => {}
        WaitView::PlayingOn { left } => {
            let secs = left.as_secs_f64().ceil();
            ui.bars.push(playing_on(name, secs as u64));
        }
        WaitView::Frozen if f.paused => {
            let (p, b) = link_wait(name);
            ui.pause = Some(p);
            ui.bars.retain(|x| !x.pulse);
            ui.bars.insert(0, b);
        }
        WaitView::Frozen => ui.bars.insert(0, link_wait_unpaused(name)),
    }
}

impl RunMessages {
    /// The notes after a change: the partner left, back.
    fn link_bars(&mut self, f: &RunFacts<'_>, ui: &mut RunUi, now: Instant) {
        if let Some((_, at)) = &self.left
            && now.saturating_duration_since(*at) < NOTE_FOR
        {
            ui.bars.push(partner_left());
        }
        if let Some((bar, at)) = &self.back {
            if now.saturating_duration_since(*at) < NOTE_FOR && f.linked {
                if bar.text.ends_with("both resumed.") {
                    // It says what the shared pause's own bar would.
                    let both = both_paused(f.partner).text;
                    ui.bars.retain(|b| b.text != both);
                }
                ui.bars.push(bar.clone());
            } else {
                self.back = None;
            }
        }
    }

    /// Advance the alert.
    fn finish(&mut self, ui: &mut RunUi) {
        if let Some(a) = &mut self.alert {
            match a.advance() {
                Some(v) => ui.alert = Some(v),
                None => self.alert = None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loadout_link_line_uses_the_runs_words() {
        let s = |x: f64| Some(Duration::from_secs_f64(x));
        assert_eq!(
            loadout_hud("Sam", true, false, Some(42.0), Some(0.0), s(0.1)),
            Some(hud("Sam", Some(42.0), Some(0.0)))
        );
        assert_eq!(
            loadout_hud("Sam", true, false, Some(42.0), None, s(5.0)),
            Some(hud_no_signal("Sam"))
        );
        assert_eq!(
            loadout_hud("Sam", false, true, None, None, None),
            Some(hud_reconnecting("Sam"))
        );
        assert_eq!(loadout_hud("Sam", false, false, None, None, None), None);
    }
    use ndml_net::PeerId;

    fn facts(gate: GatePhase) -> RunFacts<'static> {
        RunFacts {
            coop_run: true,
            linked: true,
            gate,
            partner: "Sam",
            rtt_ms: Some(42.0),
            loss_pct: Some(0.0),
            shared_pause: None,
            upgrade: None,
            silent_for: Some(Duration::ZERO),
            separate: false,
            wait: WaitView::None,
            paused: false,
            host: true,
            over: false,
        }
    }

    fn texts(ui: &RunUi) -> Vec<&str> {
        ui.bars.iter().map(|b| b.text.as_str()).collect()
    }

    /// Host: a partner linked again after it left is in its next run.
    #[test]
    fn a_partner_back_after_it_left_reads_next_run() {
        let mut m = RunMessages::default();
        let f = RunFacts {
            separate: true,
            silent_for: Some(Duration::from_secs(5)),
            ..facts(GatePhase::Running)
        };
        let ui = m.frame(&f, Switches::default(), Instant::now());
        assert_eq!(ui.hud, Some(hud_next_run("Sam")));
        assert_eq!(
            ui.hud.unwrap().detail.as_deref(),
            Some("co-op from the next run")
        );
    }

    /// The leave question: at the start gate it names nobody; in the run the
    /// joining player's says the host keeps playing and the host's that the
    /// partner's run ends too; after the partner has gone, the plain quit.
    #[test]
    fn the_leave_question_says_what_happens_to_the_partners_run() {
        let sw = Switches::default();
        let t0 = Instant::now();
        let quit = |ui: RunUi| ui.pause.and_then(|p| p.quit).map(|q| q.1);
        for (host, on) in [
            (false, "Leave this co-op run? Sam keeps playing."),
            (true, "Leave this co-op run? Sam's run ends too."),
        ] {
            let mut m = RunMessages::default();
            let held = RunFacts {
                paused: true,
                host,
                ..facts(GatePhase::Holding)
            };
            assert_eq!(
                quit(m.frame(&held, sw, t0)).as_deref(),
                Some("Leave this co-op run?"),
                "host {host}"
            );
            let running = RunFacts {
                gate: GatePhase::Running,
                ..held.clone()
            };
            assert_eq!(
                quit(m.frame(&running, sw, t0)).as_deref(),
                Some(on),
                "host {host}"
            );
            let waiting = RunFacts {
                wait: WaitView::Frozen,
                linked: false,
                ..held.clone()
            };
            assert_eq!(
                quit(m.frame(&waiting, sw, t0)).as_deref(),
                Some(on),
                "host {host}: the wait"
            );
            let gone = RunFacts {
                separate: true,
                ..running
            };
            assert_eq!(
                quit(m.frame(&gone, sw, t0)).as_deref(),
                Some("Quit this run? Co-op runs are not saved."),
                "host {host}: the partner gone"
            );
        }
    }

    /// The joining player: four rows while waiting, and the way out named;
    /// once the host is gone for good, only the run-over message.
    #[test]
    fn the_joining_player_waits_with_four_rows_and_sees_the_run_end() {
        let t0 = Instant::now();
        let sw = Switches::default();
        let mut m = RunMessages::default();
        let frozen = RunFacts {
            wait: WaitView::Frozen,
            paused: true,
            linked: false,
            ..facts(GatePhase::Running)
        };
        let ui = m.frame(&frozen, sw, t0);
        assert_eq!(
            texts(&ui),
            ["Lost the link to Sam. Leave Co-op Run returns to the menu."]
        );
        let p = ui.pause.unwrap();
        assert_eq!(p.row0.as_deref(), Some("Waiting for Sam..."));
        assert_eq!(p.quit.unwrap().0, "Leave Co-op Run");
        let upgrade = RunFacts {
            paused: false,
            ..frozen.clone()
        };
        assert!(
            texts(&m.frame(&upgrade, sw, t0))[0].contains("Leave Co-op Run is in the pause menu")
        );
        let over = RunFacts {
            over: true,
            wait: WaitView::None,
            ..frozen.clone()
        };
        let ui = m.frame(&over, sw, t0);
        assert_eq!(texts(&ui), ["Sam left. The run is over."]);
        let p = ui.pause.unwrap();
        assert_eq!(
            (p.row0.as_deref(), p.row0_blocked),
            (Some("The run is over"), true)
        );
        assert_eq!(p.quit.unwrap().0, "Quit Run");
    }

    #[test]
    fn a_frame_gap_of_a_second_is_a_local_stall() {
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);
        let mut w = SilenceWatch::default();
        for m in (0..=900).step_by(300) {
            w.update(Some(1), ms(m));
        }
        assert_eq!(w.woke(), None, "frames 300 ms apart");
        w.update(Some(1), ms(12_900));
        assert_eq!(w.woke(), Some(ms(12_900)));
        w.update(None, ms(12_917));
        assert_eq!(
            w.woke(),
            Some(ms(12_900)),
            "kept: the wait's grace reads it"
        );
    }

    #[test]
    fn a_silent_partner_reads_no_signal_before_the_wait() {
        let mut m = RunMessages::default();
        let t0 = Instant::now();
        let mut f = facts(GatePhase::Running);
        m.frame(&f, Switches::default(), t0); // the start note's clock
        let at = |s: f64| t0 + START_NOTE_FOR + Duration::from_secs_f64(s);
        let mut w = SilenceWatch::default();
        assert_eq!(w.update(Some(10), at(0.0)), Some(Duration::ZERO));
        f.silent_for = w.update(Some(10), at(1.5));
        let ui = m.frame(&f, Switches::default(), at(1.5));
        assert_eq!(
            ui.hud.unwrap().detail.as_deref(),
            Some("42 ms"),
            "a home line's short dropout says nothing"
        );
        assert!(ui.bars.is_empty());
        f.silent_for = w.update(Some(10), at(2.5));
        let ui = m.frame(&f, Switches::default(), at(2.5));
        assert_eq!(ui.hud, Some(hud_no_signal("Sam")));
        assert!(ui.bars.is_empty(), "a blip plays through");
        f.silent_for = w.update(Some(11), at(2.8));
        let ui = m.frame(&f, Switches::default(), at(2.8));
        assert_eq!(ui.hud.unwrap().detail.as_deref(), Some("42 ms"));
        assert!(ui.bars.is_empty(), "no 'is back' after a blip");
        assert_eq!(w.update(None, at(15.0)), None);
    }

    #[test]
    fn the_host_counts_down_to_the_freeze() {
        let mut m = RunMessages::default();
        let t0 = Instant::now();
        let sw = Switches::default();
        let mut f = facts(GatePhase::Running);
        m.frame(&f, sw, t0);
        let t = t0 + START_NOTE_FOR;
        f.silent_for = Some(Duration::from_secs(3));
        f.wait = WaitView::PlayingOn {
            left: Duration::from_millis(11_500),
        };
        let ui = m.frame(&f, sw, t);
        assert_eq!(ui.alert.as_ref().unwrap().text, "CONNECTION LOST");
        assert_eq!(texts(&ui), ["No signal from Sam. Pausing in 12 s..."]);
        assert!(ui.bars[0].pulse);
        assert_eq!(ui.hud, Some(hud_no_signal("Sam")));
        let p = ui.pause.unwrap();
        assert_eq!(p.quit.unwrap().0, "Leave Co-op Run");
        // Unlinked (the endpoint timed out): the line says it retries.
        f.linked = false;
        f.wait = WaitView::PlayingOn {
            left: Duration::from_secs(4),
        };
        let ui = m.frame(&f, sw, t + Duration::from_secs(7));
        assert_eq!(texts(&ui), ["No signal from Sam. Pausing in 4 s..."]);
        assert_eq!(ui.hud, Some(hud_reconnecting("Sam")));
        assert!(ui.alert.is_some(), "the alert shown once, still on");
    }

    #[test]
    fn a_frozen_game_waits_on_its_pause_screen_with_the_way_out() {
        let mut m = RunMessages::default();
        let t0 = Instant::now();
        let sw = Switches::default();
        let f = RunFacts {
            linked: false,
            wait: WaitView::Frozen,
            paused: true,
            shared_pause: Some(SharedPause::BothPaused),
            ..facts(GatePhase::Running)
        };
        let ui = m.frame(&f, sw, t0);
        assert!(ui.alert.is_none(), "nothing over the pause screen");
        let p = ui.pause.as_ref().unwrap();
        assert_eq!(p.row0.as_deref(), Some("Waiting for Sam..."));
        assert!(p.row0_blocked);
        assert_eq!(p.quit.as_ref().unwrap().0, "Leave Co-op Run");
        assert_eq!(
            texts(&ui)[0],
            "Lost the link to Sam. Leave Co-op Run returns to the menu."
        );
        // Frozen on the upgrade screen: how to reach the menu.
        let ui = m.frame(
            &RunFacts {
                paused: false,
                shared_pause: None,
                ..f.clone()
            },
            sw,
            t0,
        );
        assert!(texts(&ui)[0].contains("Leave Co-op Run is in the pause menu after this screen"));
        // At the start gate the wait speaks instead of the gate.
        let ui = m.frame(
            &RunFacts {
                gate: GatePhase::Holding,
                shared_pause: None,
                ..f
            },
            sw,
            t0,
        );
        assert_eq!(
            ui.pause.as_ref().unwrap().row0.as_deref(),
            Some("Waiting for Sam...")
        );
        assert!(texts(&ui)[0].starts_with("Lost the link to Sam."));
        assert_eq!(ui.pause.unwrap().quit.unwrap().0, "Leave Co-op Run");
    }

    #[test]
    fn the_return_says_back_or_names_the_shared_pause() {
        let t0 = Instant::now();
        let sw = Switches::default();
        let waiting = RunFacts {
            linked: false,
            wait: WaitView::Frozen,
            paused: true,
            ..facts(GatePhase::Running)
        };
        let back = facts(GatePhase::Running);
        let mut m = RunMessages::default();
        m.frame(&waiting, sw, t0);
        m.event(LinkEvent::Back { long: false }, t0);
        let ui = m.frame(&back, sw, t0 + Duration::from_secs(1));
        assert!(texts(&ui).contains(&"Sam is back."), "{:?}", ui.bars);
        let ui = m.frame(&back, sw, t0 + NOTE_FOR + Duration::from_secs(2));
        assert!(!texts(&ui).contains(&"Sam is back."), "the note fades");
        let mut m = RunMessages::default();
        m.frame(&waiting, sw, t0);
        m.event(LinkEvent::Back { long: true }, t0);
        let ui = m.frame(&back, sw, t0 + Duration::from_secs(1));
        assert!(
            texts(&ui).contains(&"Sam is back. Play resumes when you have both resumed."),
            "{:?}",
            ui.bars
        );
        assert!(ui.alert.is_none(), "no alert at any point");
        // In the shared pause it replaces that pause's own bar (one line).
        let paused = RunFacts {
            shared_pause: Some(SharedPause::BothPaused),
            paused: true,
            ..back.clone()
        };
        let ui = m.frame(&paused, sw, t0 + Duration::from_secs(2));
        let t = texts(&ui);
        assert!(t.contains(&"Sam is back. Play resumes when you have both resumed."));
        assert!(!t.contains(&both_paused("Sam").text.as_str()), "{t:?}");
    }

    /// The partner left just after the return: no "Sam is back." beside it;
    /// the host's last row reads Quit Run from then on.
    #[test]
    fn leaving_just_after_the_return_drops_the_back_note() {
        let t0 = Instant::now();
        let sw = Switches::default();
        let mut m = RunMessages::default();
        let frozen = RunFacts {
            wait: WaitView::Frozen,
            paused: true,
            ..facts(GatePhase::Running)
        };
        m.frame(&frozen, sw, t0);
        m.event(LinkEvent::Back { long: true }, t0);
        let back = RunFacts {
            paused: true,
            ..facts(GatePhase::Running)
        };
        let ui = m.frame(&back, sw, t0);
        assert!(texts(&ui).iter().any(|t| t.starts_with("Sam is back.")));
        m.event(LinkEvent::Left { name: "Sam".into() }, t0);
        let gone = RunFacts {
            separate: true,
            ..back
        };
        let ui = m.frame(&gone, sw, t0);
        let t = texts(&ui);
        assert!(t.iter().all(|t| !t.contains("is back")), "{t:?}");
        assert!(
            t.iter().any(|t| t.starts_with("Playing on alone.")),
            "{t:?}"
        );
        assert_eq!(ui.pause.unwrap().quit.unwrap().0, "Quit Run");
    }

    #[test]
    fn solo_runs_show_nothing() {
        let mut m = RunMessages::default();
        let f = RunFacts {
            coop_run: false,
            linked: false,
            ..facts(GatePhase::Idle)
        };
        assert_eq!(
            m.frame(&f, Switches::default(), Instant::now()),
            RunUi::default()
        );
    }

    #[test]
    fn start_gate_relabels_and_blocks_resume() {
        let mut m = RunMessages::default();
        let ui = m.frame(
            &facts(GatePhase::Holding),
            Switches::default(),
            Instant::now(),
        );
        let p = ui.pause.unwrap();
        assert_eq!(p.row0.as_deref(), Some("Waiting for Sam..."));
        assert!(p.row0_blocked);
        assert_eq!(p.quit.unwrap().0, "Leave Co-op Run");
        assert!(ui.bars[0].text.contains("when Sam arrives") && ui.bars[0].pulse);
        assert!(ui.hud.is_some());
    }

    #[test]
    fn refused_run_says_modes_differ() {
        let mut m = RunMessages::default();
        let ui = m.frame(
            &facts(GatePhase::Refused { peer: PeerId(1) }),
            Switches::default(),
            Instant::now(),
        );
        assert_eq!(ui.pause.unwrap().row0.as_deref(), Some("Modes differ"));
        assert!(ui.bars[0].text.contains("same game mode"));
    }

    #[test]
    fn start_note_then_hud_only() {
        let mut m = RunMessages::default();
        let t0 = Instant::now();
        let ui = m.frame(&facts(GatePhase::Running), Switches::default(), t0);
        assert_eq!(ui.bars, vec![coop_started("Sam")]);
        assert_eq!(ui.hud.as_ref().unwrap().detail.as_deref(), Some("42 ms"));
        let ui = m.frame(
            &facts(GatePhase::Running),
            Switches::default(),
            t0 + START_NOTE_FOR,
        );
        assert!(ui.bars.is_empty());
        let row0 = ui.pause.unwrap();
        assert!(row0.row0.is_none() && !row0.row0_blocked, "resume is free");
    }

    #[test]
    fn bad_links_are_negative() {
        assert!(!hud("Sam", Some(149.0), Some(1.0)).bad);
        assert!(hud("Sam", Some(151.0), None).bad);
        assert!(hud("Sam", Some(40.0), Some(6.0)).bad);
        assert!(hud("Sam", None, None).detail.is_none());
    }

    #[test]
    fn partner_left_alert_grows_holds_and_fades() {
        let mut m = RunMessages::default();
        let t0 = Instant::now();
        m.event(LinkEvent::Left { name: "Sam".into() }, t0);
        let mut f = facts(GatePhase::Running);
        f.linked = false;
        let first = m.frame(&f, Switches::default(), t0).alert.unwrap();
        assert_eq!(first.text, "SAM LEFT");
        assert!(first.scale < 0.2 && (first.alpha - 1.0).abs() < 1e-9);
        let mut last = first;
        let mut frames = 1;
        while let Some(a) = m.frame(&f, Switches::default(), t0).alert {
            assert!(a.scale >= last.scale - 1e-12);
            last = a;
            frames += 1;
        }
        assert!(last.scale > 0.99);
        assert!(
            (360..=380).contains(&frames),
            "{frames}: 240 hold + ~133 fade"
        );
    }

    #[test]
    fn session_messages() {
        assert_eq!(
            pause_waiting("Sam").0.row0.as_deref(),
            Some("Waiting for Sam...")
        );
        assert_eq!(
            both_paused("Sam").text,
            "Sam is paused too. Play resumes when you have both resumed."
        );
        assert_eq!(
            pause_partner_ready("Sam").text,
            "Sam is ready and waiting for you."
        );
        let c = upgrade_waiting("Sam");
        assert_eq!(
            (c.title.as_str(), c.subtitle.as_str()),
            ("WAITING", "Waiting for Sam...")
        );
        assert_eq!(
            upgrade_partner_ready("Sam").text,
            "Sam is ready and waiting for you."
        );
        assert_eq!(
            leave_run("Sam", true, false, false).1,
            "Leave this co-op run? Sam keeps playing."
        );
        assert_eq!(
            leave_run("Sam", true, false, true).1,
            "Leave this co-op run? Sam's run ends too."
        );
        for host in [false, true] {
            assert_eq!(
                leave_run("Sam", true, true, host).1,
                "Leave this co-op run?"
            );
            assert_eq!(
                leave_run("Sam", false, false, host).1,
                "Quit this run? Co-op runs are not saved."
            );
        }
    }

    /// Messages whose "Co-op with Sam" note is over, and a time after it.
    fn past_start_note(f: &RunFacts<'_>) -> (RunMessages, Instant) {
        let mut m = RunMessages::default();
        let t = Instant::now();
        let _ = m.frame(f, Switches::default(), t);
        (m, t + Duration::from_secs(9))
    }

    #[test]
    fn session_facts_choose_the_native_pieces() {
        let f = facts(GatePhase::Running);
        let sw = Switches::default();
        let (mut m, t0) = past_start_note(&f);
        m.set_session(SessionFacts {
            pause: Some(SharedPause::WaitingForPartner),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        let p = ui.pause.unwrap();
        assert_eq!(p.row0.as_deref(), Some("Waiting for Sam..."));
        assert!(p.row0_blocked);
        assert_eq!(p.quit.unwrap().0, "Leave Co-op Run");
        assert!(ui.bars.iter().any(|b| b.text == "Sam is still paused."));
        // Both on their menus: the menu is the game's own (row 0 free).
        m.set_session(SessionFacts {
            pause: Some(SharedPause::BothPaused),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        assert!(!ui.pause.unwrap().row0_blocked);
        assert!(ui.bars[0].text.contains("paused too"));
        // Waiting on the upgrade screen: the card; after the wait, Esc.
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::PartnerChoosing),
            ..SessionFacts::default()
        });
        assert_eq!(m.frame(&f, sw, t0).card, Some(upgrade_waiting("Sam")));
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::PartnerChoosingLong),
            ..SessionFacts::default()
        });
        let card = m.frame(&f, sw, t0).card.unwrap();
        assert!(card.note.unwrap().starts_with("Press Esc to play on."));
        // Nothing to choose: the pause screen says why; Esc after the wait.
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::HeldPaused),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        let p = ui.pause.unwrap();
        assert_eq!(p.row0.as_deref(), Some("Waiting for Sam..."));
        assert!(p.row0_blocked);
        assert!(ui.bars[0].text == "Sam is choosing an upgrade." && ui.bars[0].pulse);
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::HeldPausedLong),
            ..SessionFacts::default()
        });
        let p = m.frame(&f, sw, t0).pause.unwrap();
        assert_eq!(p.row0.as_deref(), Some("Play On"));
        assert!(!p.row0_blocked);
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::ResumeToChoose),
            ..SessionFacts::default()
        });
        assert!(
            m.frame(&f, sw, t0).bars[0]
                .text
                .contains("Resume to choose")
        );
        // The partner ready while this player chooses.
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::PartnerReady),
            ..SessionFacts::default()
        });
        assert_eq!(m.frame(&f, sw, t0).bars, vec![upgrade_partner_ready("Sam")]);
        // What the caller passes wins over the session.
        let mut g = facts(GatePhase::Running);
        g.upgrade = Some(UpgradeWait::PartnerChoosing);
        assert!(m.frame(&g, sw, t0).card.is_some());
    }

    #[test]
    fn downed_countdowns_alerts_and_notes() {
        let f = facts(GatePhase::Running);
        let sw = Switches::default();
        let (mut m, t0) = past_start_note(&f);
        m.session_event(SessionEvent::Down);
        m.set_session(SessionFacts {
            me_down: Some(32),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        assert_eq!(ui.alert.unwrap().text, "SHIP DOWN");
        assert_eq!(
            ui.bars,
            vec![you_are_down(Some("Sam"), 32)],
            "the countdown names who you come back to"
        );
        assert!(ui.bars[0].text.contains("Back next to Sam in 32 s") && ui.bars[0].pulse);
        // Alone, the ship comes back where it went down: nobody named.
        let mut alone = past_start_note(&f).0;
        alone.set_session(SessionFacts {
            me_down: Some(32),
            me_down_alone: true,
            ..SessionFacts::default()
        });
        let ui = alone.frame(&f, sw, t0);
        assert_eq!(ui.bars, vec![you_are_down(None, 32)]);
        assert_eq!(ui.bars[0].text, "Your ship is down. Back in 32 s.");
        let (mut m, t0) = past_start_note(&f);
        m.session_event(SessionEvent::PartnerDown);
        m.set_session(SessionFacts {
            partner_down: Some(5),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        assert_eq!(ui.alert.unwrap().text, "SAM IS DOWN");
        assert!(
            ui.bars
                .iter()
                .any(|b| b.text == "Sam is down. Back in 5 s.")
        );
        // Back: a short note.
        m.set_session(SessionFacts::default());
        m.session_event(SessionEvent::PartnerBack);
        let ui = m.frame(&f, sw, t0);
        assert!(
            ui.bars
                .iter()
                .any(|b| b.text == "Sam is back in the fight.")
        );
        let ui = m.frame(&f, sw, t0 + START_NOTE_FOR);
        assert!(!ui.bars.iter().any(|b| b.text.contains("back in the fight")));
        m.session_event(SessionEvent::Back);
        assert!(m.frame(&f, sw, t0).bars.contains(&back_in_the_fight(None)));
        m.session_event(SessionEvent::AllDown);
        assert!(m.frame(&f, sw, t0).bars.contains(&all_down()));
    }
}
