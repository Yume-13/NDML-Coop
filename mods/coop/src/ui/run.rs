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
    /// `confirmPause` on row 0 opens this player's own upgrade screen.
    pub row0_choose: bool,
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
    /// The traffic line above it (`show_traffic`): "48 KB/s up  21 KB/s down".
    pub traffic: Option<String>,
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

/// A count on screen: the edge glow and the number with its word.
#[derive(Debug, Clone, PartialEq)]
pub struct CountShow {
    pub number: u8,
    /// Above the number, upper case.
    pub label: &'static str,
    /// 0 to 1 through the current second (the pulse).
    pub phase: f64,
    /// The edge glow (every count).
    pub edge: bool,
    /// The number on the middle panel, else on the respawn alert's strip.
    pub panel: bool,
    /// The screen dimmed under the pause screen's rows (the resume count).
    pub dim: bool,
}

/// Everything in-run for one frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunUi {
    pub pause: Option<PauseText>,
    pub bars: Vec<Bar>,
    pub hud: Option<Hud>,
    pub card: Option<Card>,
    pub alert: Option<AlertView>,
    pub count: Option<CountShow>,
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
    /// Nothing to choose (or down): held on the pause screen meanwhile.
    HeldPaused,
    /// This player has Ataraxia: held on the pause screen for the partner.
    AtaraxiaHeld,
    /// This player paused while the partner opened the upgrade screen.
    ResumeToChoose,
}

/// What the session rules (`crate::rules`) want shown this frame; what
/// [`RunFacts`] passes wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionFacts {
    pub pause: Option<SharedPause>,
    pub upgrade: Option<UpgradeWait>,
    /// A count showing (the resume's, or the respawn alert's last seconds).
    pub count: Option<CountView>,
    /// This player's ship is down: whole seconds until it respawns (game time).
    pub me_down: Option<u64>,
    /// The partner's ship is down: whole seconds until it respawns.
    pub partner_down: Option<u64>,
    /// The upgrade round's end will count (the host's state).
    pub upgrade_counts: bool,
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
    /// The partner became ready while this player holds the round: cue only.
    PartnerReady,
    /// A count's number `n` starts: its tick sounds.
    CountTick(CountKind, u8),
    /// A count is over: play goes on.
    CountGo(CountKind),
    /// An upgrade round ended without a count: play goes on, no sound.
    ResumedNoCount,
}

/// Which count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountKind {
    /// Before play resumes after a shared hold (both games).
    Resume,
    /// The last seconds before this player's ship returns (its game only).
    Respawn,
    /// Before a run starts (both games; the flow's "both ready").
    Start,
}

impl CountKind {
    /// The word above the number.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Resume => "RESUMING",
            Self::Respawn => "BACK IN",
            Self::Start => "STARTING",
        }
    }

    /// The count's name in log lines.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Respawn => "respawn",
            Self::Start => "start",
        }
    }
}

/// The count this frame, as the rules see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountView {
    pub kind: CountKind,
    pub number: u8,
    /// Thousandths into the current second.
    pub phase_pm: u16,
}

impl CountView {
    /// As shown.
    #[must_use]
    pub fn show(self) -> CountShow {
        CountShow {
            number: self.number,
            label: self.kind.label(),
            phase: f64::from(self.phase_pm) / 1000.0,
            edge: true,
            panel: self.kind != CountKind::Respawn,
            dim: self.kind == CountKind::Resume,
        }
    }
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
    /// The partner is gone for this run, even with the link back.
    pub separate: bool,
    /// The wait for a lost link ([`crate::wait`]).
    pub wait: WaitView,
    /// The pause screen is up.
    pub paused: bool,
    /// This game hosts (or hosted when last linked).
    pub host: bool,
    /// The run is over (the partner left it by choice), either role.
    pub over: bool,
}

/// Something happened to the partner's part in this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEvent {
    /// The link is back after a wait; `long`: it became the shared pause.
    Back { long: bool },
}

/// The config's switches for the run messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    /// The HUD line (P3; `overlay`).
    pub hud: bool,
    /// The traffic line above it (`show_traffic`).
    pub traffic: bool,
}

impl Default for Switches {
    fn default() -> Self {
        Self {
            hud: true,
            traffic: false,
        }
    }
}

/// Start gate, partner not in yet.
#[must_use]
pub fn start_gate(partner: &str) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some(format!("Waiting for {partner}...")),
            row0_blocked: true,
            row0_choose: false,
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
            row0_choose: false,
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
            row0_choose: false,
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

/// Upgrade screen: this player is done, the partner is choosing.
#[must_use]
pub fn upgrade_waiting(partner: &str, counts: bool) -> Card {
    Card {
        title: "WAITING".into(),
        subtitle: format!("Waiting for {partner}..."),
        note: Some(format!(
            "{partner} is choosing an upgrade. {}",
            super::text::resume_note(counts)
        )),
    }
}

/// Upgrade screen: this player is choosing, the partner is done.
#[must_use]
pub fn upgrade_partner_ready(partner: &str, counts: bool) -> Bar {
    Bar::new(format!(
        "{partner} is ready and waiting for you. {}",
        super::text::resume_note(counts)
    ))
}

/// Upgrade screen open for the partner; this game waits on the pause screen.
#[must_use]
pub fn upgrade_held_paused(partner: &str, counts: bool) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some(format!("Waiting for {partner}...")),
            row0_blocked: true,
            row0_choose: false,
            quit: None,
        },
        held_bar(partner, counts),
    )
}

/// The held pause screen's bar, with or without Ataraxia.
fn held_bar(partner: &str, counts: bool) -> Bar {
    Bar::new(format!(
        "{partner} is choosing an upgrade. {}",
        super::text::resume_note(counts)
    ))
    .pulsing()
}

/// This player has Ataraxia: Space (row 0) opens this player's own screen.
#[must_use]
pub fn upgrade_ataraxia_held(partner: &str, counts: bool) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some("Choose Upgrade".to_owned()),
            row0_blocked: false,
            row0_choose: true,
            quit: None,
        },
        held_bar(partner, counts),
    )
}

/// The pause screen while the resume count runs: its Resume is refused.
#[must_use]
pub fn resume_counting(n: u8) -> PauseText {
    PauseText {
        row0: Some(format!("Resuming in {n}...")),
        row0_blocked: true,
        row0_choose: false,
        quit: None,
    }
}

/// This player paused while the partner opened the upgrade screen.
#[must_use]
pub fn resume_to_choose(partner: &str) -> Bar {
    Bar::new(format!(
        "{partner} is choosing an upgrade. Resume to choose yours."
    ))
}

/// This player's ship is down.
#[must_use]
pub fn you_are_down(secs: u64) -> Bar {
    Bar::new(format!("Your ship is down. Back in {secs} s.")).pulsing()
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
            row0_choose: false,
            quit: None,
        },
        Bar::new(format!(
            "{} Leave Co-op Run returns to the menu.",
            super::text::lost_link(partner, "")
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

/// The partner left the run by choice and this run is over, either role.
#[must_use]
pub fn run_over(partner: &str) -> (PauseText, Bar) {
    (
        PauseText {
            row0: Some("The run is over".into()),
            row0_blocked: true,
            row0_choose: false,
            quit: None,
        },
        Bar::new(format!("{partner} left. The run is over.")),
    )
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
        traffic: None,
    }
}

/// Host: the HUD line while linked with a partner who left this run.
#[must_use]
pub fn hud_next_run(partner: &str) -> Hud {
    Hud {
        name: partner.to_owned(),
        detail: Some("co-op from the next run".into()),
        bad: false,
        traffic: None,
    }
}

/// The HUD line while the link is down and being retried.
#[must_use]
pub fn hud_reconnecting(partner: &str) -> Hud {
    Hud {
        name: partner.to_owned(),
        detail: Some("reconnecting...".into()),
        bad: true,
        traffic: None,
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
        traffic: None,
    }
}

/// The traffic line's text: KB/s of the mod's datagrams each way.
#[must_use]
pub fn traffic_text(up_kbs: f64, down_kbs: f64) -> String {
    let kb = |x: f64| {
        if x < 10.0 {
            format!("{x:.1}")
        } else {
            format!("{x:.0}")
        }
    };
    format!("{} KB/s up  {} KB/s down", kb(up_kbs), kb(down_kbs))
}

/// Puts the traffic line above a shown HUD line. `traffic`: `None` unlinked.
pub fn add_traffic(ui: &mut RunUi, sw: Switches, traffic: Option<(f64, f64)>) {
    if let (true, Some(h), Some((up, down))) = (sw.traffic, ui.hud.as_mut(), traffic) {
        h.traffic = Some(traffic_text(up, down));
    }
}

/// The link line on the new game screen, in the run's words.
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

/// The partner's Ready alert wherever this player is. Never takes input.
#[derive(Debug, Clone, Default)]
pub struct ReadyNotice {
    alert: Option<Alert>,
    /// The cue is due ([`Self::take_cue`]).
    cue: bool,
    /// The places it was drawn on since it began ([`Self::first_on`]).
    places: u8,
}

/// Where the partner's Ready is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyPlace {
    NewGameScreen,
    Menu,
    CoopPage,
    Run,
    EndScreen,
    /// The high-score name entry after the end screen (`oHighscoreEntry`).
    NameEntry,
    /// The high-score table after it (`oHighscoresDynamic`).
    ScoreTable,
}

impl ReadyPlace {
    /// In a log line.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NewGameScreen => "the new game screen",
            Self::Menu => "the main menu",
            Self::CoopPage => "the Co-op page",
            Self::Run => "a run",
            Self::EndScreen => "the end screen",
            Self::NameEntry => "the high-score name entry",
            Self::ScoreTable => "the high-score table",
        }
    }
}

impl ReadyNotice {
    /// The partner has just become ready: the alert and the cue.
    pub fn start(&mut self, text: String) {
        self.alert = Some(Alert::new(text));
        self.cue = true;
        self.places = 0;
    }

    /// The partner is not ready any more (or the link went): no alert, no cue.
    pub fn clear(&mut self) {
        self.alert = None;
        self.cue = false;
        self.places = 0;
    }

    /// It is drawn on `place` now: `true` the first time since it began.
    pub fn first_on(&mut self, place: ReadyPlace) -> bool {
        let bit = 1 << (place as u8);
        let first = self.places & bit == 0;
        self.places |= bit;
        first
    }

    /// The cue to play now, once.
    pub fn take_cue(&mut self) -> bool {
        std::mem::take(&mut self.cue)
    }

    /// One drawn frame: the alert, while it is up.
    pub fn step(&mut self) -> Option<AlertView> {
        let v = self.alert.as_mut()?.advance();
        if v.is_none() {
            self.alert = None;
        }
        v
    }
}

/// The bar while the partner stays ready: pulsing, as "in progress".
#[must_use]
pub fn ready_bar(text: impl Into<String>) -> Bar {
    Bar::new(text).pulsing()
}

/// How long a lost link's notice stays in the menus at most.
pub const LINK_NOTICE_FOR: Duration = Duration::from_secs(60);

/// The screens a lost link's notice belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeRoom {
    Menus,
    NewGameScreen,
    EndScreens,
    Run,
}

impl ReadyPlace {
    /// Its screen for a lost link's notice.
    #[must_use]
    pub fn room(self) -> NoticeRoom {
        match self {
            Self::Menu | Self::CoopPage => NoticeRoom::Menus,
            Self::NewGameScreen => NoticeRoom::NewGameScreen,
            Self::EndScreen | Self::NameEntry | Self::ScoreTable => NoticeRoom::EndScreens,
            Self::Run => NoticeRoom::Run,
        }
    }
}

/// What a link that was up and went says, by how it went. `None`: this player
/// dropped it, or it was never a link.
#[must_use]
pub fn lost_link_line(
    reason: ndml_net::endpoint::DisconnectReason,
    partner: &str,
) -> Option<String> {
    use ndml_net::endpoint::DisconnectReason as R;
    match reason {
        R::Remote => Some(super::text::partner_left(partner)),
        R::Timeout | R::Replaced => Some(super::text::lost_link(partner, "")),
        R::Local | R::HandshakeTimeout | R::Refused(_) => None,
    }
}

/// A lost link in the menus: a bar and an alert. It stays on its first screen
/// until the player leaves it, links again or [`LINK_NOTICE_FOR`] passes.
#[derive(Debug, Clone)]
pub struct LinkNotice {
    line: String,
    since: Instant,
    room: Option<NoticeRoom>,
    /// Made for that screen: until drawn there, another screen does not end it.
    bound: Option<NoticeRoom>,
    alert: Option<Alert>,
    places: u8,
}

impl LinkNotice {
    #[must_use]
    pub fn new(line: String, now: Instant) -> Self {
        let alert = Some(Alert::new(super::text::upper(line.trim_end_matches('.'))));
        Self {
            line,
            since: now,
            room: None,
            bound: None,
            alert,
            places: 0,
        }
    }

    /// A notice for `room` only: drawn there first.
    #[must_use]
    pub fn for_room(line: String, now: Instant, room: NoticeRoom) -> Self {
        Self {
            bound: Some(room),
            ..Self::new(line, now)
        }
    }

    /// Made for another screen, not drawn there: none on `place`, not over.
    #[must_use]
    pub fn waits_for_other(&self, place: ReadyPlace) -> bool {
        self.room.is_none() && self.bound.is_some_and(|b| b != place.room())
    }

    /// One drawn frame on `place`: its bar and this frame's alert. `None` once
    /// it is over: the caller drops it.
    pub fn frame(&mut self, place: ReadyPlace, now: Instant) -> Option<(Bar, Option<AlertView>)> {
        let room = place.room();
        if room == NoticeRoom::Run || now.saturating_duration_since(self.since) > LINK_NOTICE_FOR {
            return None;
        }
        match self.room {
            Some(r) if r != room => return None,
            None => self.room = Some(room),
            Some(_) => {}
        }
        let alert = self.alert.as_mut().and_then(Alert::advance);
        if alert.is_none() {
            self.alert = None;
        }
        Some((Bar::new(self.line.clone()), alert))
    }

    /// It is drawn on `place` now: `true` the first time.
    pub fn first_on(&mut self, place: ReadyPlace) -> bool {
        let bit = 1 << (place as u8);
        let first = self.places & bit == 0;
        self.places |= bit;
        first
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
                SessionEvent::PartnerReady
                | SessionEvent::CountTick(..)
                | SessionEvent::CountGo(_)
                | SessionEvent::ResumedNoCount => {}
            }
        }
        if let Some(s) = self.session.me_down {
            ui.bars.push(you_are_down(s));
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

    /// A count showing (and "Resuming in 3..." on the pause screen).
    fn count_view(&self, f: &RunFacts<'_>, ui: &mut RunUi) {
        if let Some(c) = self.session.count {
            if c.kind == CountKind::Resume && f.paused && ui.pause.is_none() {
                ui.pause = Some(resume_counting(c.number));
            }
            ui.count = Some(c.show());
        }
    }

    /// Something happened to the partner's part in this run.
    pub fn event(&mut self, ev: LinkEvent) {
        match ev {
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
            // The run is over (the partner left): only that, until the menu.
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
        if let Some(w) = f.upgrade.or(self.session.upgrade) {
            upgrade_view(name, w, self.session.upgrade_counts, &mut ui);
        }
        self.count_view(f, &mut ui);
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
                    traffic: None,
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

/// The upgrade round's card, bar or held pause screen.
fn upgrade_view(name: &str, w: UpgradeWait, counts: bool, ui: &mut RunUi) {
    match w {
        UpgradeWait::PartnerChoosing => ui.card = Some(upgrade_waiting(name, counts)),
        UpgradeWait::PartnerReady => ui.bars.push(upgrade_partner_ready(name, counts)),
        UpgradeWait::HeldPaused => {
            let (p, b) = upgrade_held_paused(name, counts);
            // The held pause screen says why, over a partner's pause text.
            ui.pause = Some(p);
            ui.bars.push(b);
        }
        UpgradeWait::AtaraxiaHeld => {
            let (p, b) = upgrade_ataraxia_held(name, counts);
            ui.pause = Some(p);
            ui.bars.push(b);
        }
        UpgradeWait::ResumeToChoose => ui.bars.push(resume_to_choose(name)),
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
        row0_choose: false,
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
    /// The notes after a change: the partner back.
    fn link_bars(&mut self, f: &RunFacts<'_>, ui: &mut RunUi, now: Instant) {
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
    fn a_lost_link_says_how_it_went_and_stays_on_its_screen() {
        use ndml_net::endpoint::DisconnectReason as R;
        assert_eq!(
            lost_link_line(R::Remote, "Sam").as_deref(),
            Some("Sam left.")
        );
        assert_eq!(
            lost_link_line(R::Timeout, "Sam").as_deref(),
            Some("Lost the link to Sam.")
        );
        assert_eq!(
            lost_link_line(R::Replaced, "Sam").as_deref(),
            Some("Lost the link to Sam.")
        );
        assert_eq!(
            lost_link_line(R::Local, "Sam"),
            None,
            "this player dropped it"
        );
        assert_eq!(lost_link_line(R::HandshakeTimeout, "Sam"), None);
        let t = Instant::now();
        let mut n = LinkNotice::new("Lost the link to Sam.".into(), t);
        let (bar, alert) = n.frame(ReadyPlace::CoopPage, t).expect("shown");
        assert_eq!(bar.text, "Lost the link to Sam.");
        assert!(!bar.pulse, "a fact, not a wait");
        assert_eq!(alert.expect("its alert").text, "LOST THE LINK TO SAM");
        assert!(n.first_on(ReadyPlace::CoopPage) && !n.first_on(ReadyPlace::CoopPage));
        // The main menu is the same screen as its Co-op page.
        assert!(
            n.frame(ReadyPlace::Menu, t + Duration::from_secs(1))
                .is_some()
        );
        // The alert fades, the bar stays.
        let mut last = None;
        for i in 0..400 {
            last = n.frame(ReadyPlace::Menu, t + Duration::from_millis(1000 + i * 16));
        }
        let (bar, alert) = last.expect("still up");
        assert!(alert.is_none() && bar.text.starts_with("Lost"));
        assert!(n.clone().frame(ReadyPlace::NewGameScreen, t).is_none());
        assert!(n.clone().frame(ReadyPlace::Run, t).is_none());
        assert!(
            n.frame(
                ReadyPlace::Menu,
                t + LINK_NOTICE_FOR + Duration::from_secs(1)
            )
            .is_none()
        );
        // The end screens are one screen.
        let mut e = LinkNotice::new("Sam left.".into(), t);
        assert!(e.frame(ReadyPlace::EndScreen, t).is_some());
        assert!(e.frame(ReadyPlace::ScoreTable, t).is_some());
        // Made for the menus: it waits for them, then stays there.
        let mut g = LinkNotice::for_room("Kept.".into(), t, NoticeRoom::Menus);
        assert!(g.waits_for_other(ReadyPlace::NewGameScreen));
        assert!(!g.waits_for_other(ReadyPlace::Menu));
        assert!(g.frame(ReadyPlace::Menu, t).is_some());
        assert!(
            !g.waits_for_other(ReadyPlace::NewGameScreen),
            "drawn: as any"
        );
        assert!(g.frame(ReadyPlace::CoopPage, t).is_some());
        assert!(g.frame(ReadyPlace::NewGameScreen, t).is_none());
    }

    /// The alert grows, holds and fades as the wave alert does. Its cue is due
    /// once, and it goes at a clear.
    #[test]
    fn the_ready_notice_alerts_once_and_fades() {
        let mut n = ReadyNotice::default();
        assert!(n.step().is_none() && !n.take_cue());
        n.start("SAM IS READY".into());
        assert!(n.take_cue() && !n.take_cue(), "the cue once");
        let first = n.step().expect("up");
        assert_eq!(first.text, "SAM IS READY");
        let mut last = first;
        let mut steps = 1;
        while let Some(v) = n.step() {
            last = v;
            steps += 1;
        }
        assert!(
            last.scale > 0.99 && steps > ALERT_HOLD_STEPS,
            "grown, held, faded"
        );
        assert!(n.step().is_none(), "gone after its fade");
        n.start("SAM IS READY".into());
        n.clear();
        assert!(n.step().is_none() && !n.take_cue(), "cleared");
        assert!(ready_bar("x").pulse);
    }

    #[test]
    fn the_new_game_screen_link_line_uses_the_runs_words() {
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
        m.event(LinkEvent::Back { long: false });
        let ui = m.frame(&back, sw, t0 + Duration::from_secs(1));
        assert!(texts(&ui).contains(&"Sam is back."), "{:?}", ui.bars);
        let ui = m.frame(&back, sw, t0 + NOTE_FOR + Duration::from_secs(2));
        assert!(!texts(&ui).contains(&"Sam is back."), "the note fades");
        let mut m = RunMessages::default();
        m.frame(&waiting, sw, t0);
        m.event(LinkEvent::Back { long: true });
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

    #[test]
    fn leaving_just_after_the_return_shows_only_the_run_over() {
        let t0 = Instant::now();
        let sw = Switches::default();
        let mut m = RunMessages::default();
        let frozen = RunFacts {
            wait: WaitView::Frozen,
            paused: true,
            ..facts(GatePhase::Running)
        };
        m.frame(&frozen, sw, t0);
        m.event(LinkEvent::Back { long: true });
        let back = RunFacts {
            paused: true,
            ..facts(GatePhase::Running)
        };
        let ui = m.frame(&back, sw, t0);
        assert!(texts(&ui).iter().any(|t| t.starts_with("Sam is back.")));
        for host in [true, false] {
            let gone = RunFacts {
                separate: true,
                over: true,
                host,
                ..back
            };
            let ui = m.frame(&gone, sw, t0);
            assert_eq!(
                texts(&ui),
                vec!["Sam left. The run is over."],
                "host {host}"
            );
            assert_eq!(ui.pause.unwrap().row0.as_deref(), Some("The run is over"));
        }
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
    fn the_connection_lost_alert_grows_holds_and_fades() {
        let mut m = RunMessages::default();
        let t0 = Instant::now();
        let mut f = facts(GatePhase::Running);
        f.linked = false;
        f.wait = WaitView::PlayingOn {
            left: Duration::from_secs(12),
        };
        let first = m.frame(&f, Switches::default(), t0).alert.unwrap();
        assert_eq!(first.text, "CONNECTION LOST");
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
        let c = upgrade_waiting("Sam", false);
        assert_eq!(
            (c.title.as_str(), c.subtitle.as_str()),
            ("WAITING", "Waiting for Sam...")
        );
        assert_eq!(
            upgrade_partner_ready("Sam", false).text,
            "Sam is ready and waiting for you. (no countdown)"
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
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::PartnerChoosing),
            ..SessionFacts::default()
        });
        assert_eq!(
            m.frame(&f, sw, t0).card,
            Some(upgrade_waiting("Sam", false))
        );
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::HeldPaused),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        let p = ui.pause.unwrap();
        assert_eq!(p.row0.as_deref(), Some("Waiting for Sam..."));
        assert!(p.row0_blocked);
        assert!(
            ui.bars[0].text == "Sam is choosing an upgrade. (no countdown)" && ui.bars[0].pulse
        );
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
        assert_eq!(
            m.frame(&f, sw, t0).bars,
            vec![upgrade_partner_ready("Sam", false)]
        );
        // What the caller passes wins over the session.
        let mut g = facts(GatePhase::Running);
        g.upgrade = Some(UpgradeWait::PartnerChoosing);
        assert!(m.frame(&g, sw, t0).card.is_some());
    }

    #[test]
    fn counts_and_the_ataraxia_hold_as_shown() {
        let mut f = facts(GatePhase::Running);
        let sw = Switches::default();
        let (mut m, t0) = past_start_note(&f);
        f.paused = true;
        m.set_session(SessionFacts {
            count: Some(CountView {
                kind: CountKind::Resume,
                number: 2,
                phase_pm: 250,
            }),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        let p = ui.pause.unwrap();
        assert_eq!(p.row0.as_deref(), Some("Resuming in 2..."));
        assert!(p.row0_blocked && !p.row0_choose);
        let c = ui.count.unwrap();
        assert!(c.edge && c.number == 2 && c.label == "RESUMING");
        assert!((c.phase - 0.25).abs() < 1e-9);
        f.paused = false;
        m.set_session(SessionFacts {
            count: Some(CountView {
                kind: CountKind::Respawn,
                number: 3,
                phase_pm: 0,
            }),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        assert_eq!(ui.pause.and_then(|p| p.row0), None, "no pause text");
        let c = ui.count.unwrap();
        assert!(c.edge && !c.panel && !c.dim && c.number == 3 && c.label == "BACK IN");
        let start = CountView {
            kind: CountKind::Start,
            number: 1,
            phase_pm: 0,
        }
        .show();
        assert!(start.edge && start.panel && start.label == "STARTING");
        f.paused = true;
        // No Space prompt on the bar. Row 0 opens this player's screen.
        m.set_session(SessionFacts {
            upgrade: Some(UpgradeWait::AtaraxiaHeld),
            ..SessionFacts::default()
        });
        let ui = m.frame(&f, sw, t0);
        let p = ui.pause.unwrap();
        assert_eq!(p.row0.as_deref(), Some("Choose Upgrade"));
        assert!(p.row0_choose && !p.row0_blocked);
        assert_eq!(
            ui.bars[0].text,
            "Sam is choosing an upgrade. (no countdown)"
        );
    }

    #[test]
    fn the_upgrade_waits_say_whether_the_end_counts() {
        assert_eq!(super::super::text::resume_note(false), "(no countdown)");
        assert_eq!(
            super::super::text::resume_note(true),
            "(countdown on resume)"
        );
        let f = facts(GatePhase::Running);
        let sw = Switches::default();
        let (mut m, t0) = past_start_note(&f);
        let text = |m: &mut RunMessages, w, counts| {
            m.set_session(SessionFacts {
                upgrade: Some(w),
                upgrade_counts: counts,
                ..SessionFacts::default()
            });
            let ui = m.frame(&f, sw, t0);
            ui.card
                .and_then(|c| c.note)
                .or_else(|| {
                    ui.bars
                        .iter()
                        .find(|b| b.text.contains("countdown"))
                        .map(|b| b.text.clone())
                })
                .unwrap_or_default()
        };
        for (w, line) in [
            (UpgradeWait::PartnerChoosing, "Sam is choosing an upgrade."),
            (
                UpgradeWait::PartnerReady,
                "Sam is ready and waiting for you.",
            ),
            (UpgradeWait::HeldPaused, "Sam is choosing an upgrade."),
            (UpgradeWait::AtaraxiaHeld, "Sam is choosing an upgrade."),
        ] {
            assert_eq!(
                text(&mut m, w, false),
                format!("{line} (no countdown)"),
                "{w:?}"
            );
            assert_eq!(
                text(&mut m, w, true),
                format!("{line} (countdown on resume)"),
                "{w:?}"
            );
        }
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
        assert_eq!(ui.bars, vec![you_are_down(32)]);
        assert_eq!(ui.bars[0].text, "Your ship is down. Back in 32 s.");
        assert!(ui.bars[0].pulse);
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
