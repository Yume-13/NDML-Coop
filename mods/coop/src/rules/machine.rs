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

//! The session rules for one co-op run (pure). Messages, hook calls and
//! per-frame readings go in; messages ([`Out`]), game actions ([`Action`]),
//! hook decisions and what to show ([`SessionFacts`]) come out. The host
//! sequences; a player left alone sequences for itself.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ndml_net::msg::{Message, PeerId};
use ndml_net::session::{DownOutcome, PauseGate, PeerSet, RespawnRules, UpgradeEvent, UpgradeGate};

use super::countdown::{Countdown, Step, alert_number};
use super::score::{ClientScore, HostScore, Score};
use super::settings::RulesSettings;
use super::xp::{ClientXp, HostXp, XpRead, XpStep};
use crate::net::Out;
use crate::ui::run::{CountKind, CountView, SessionEvent, SessionFacts, SharedPause, UpgradeWait};

mod state_sync;

/// One game frame (the run tick runs at 60 steps per second).
pub const FRAME: Duration = Duration::from_nanos(16_666_667);
/// Frames a pause or unpause the mod made itself stays expected.
const OWN_WINDOW: u64 = 15;
/// Frames between two calls of the same pause action.
const PRESS_EVERY: u64 = 30;
/// Frames a forced `gotoUpgrade` may take to open the screen.
const GOTO_WAIT: u64 = 20;
/// Frames after the start gate's ask (`Rules::start_screen_asked`) in which
/// the screen that opens is this player's own.
const START_SCREEN_FRAMES: u64 = GOTO_WAIT;
/// Frames a release (`leaveUpgrade`) may take before it is called again.
const RELEASE_RETRY: u64 = 20;
/// Retries between two warn lines about a release that does not take.
const RELEASE_WARN_EVERY: u32 = 90;
/// Frames a client waits for mirrored levels before it has nothing to choose.
const OWED_WAIT: u64 = 90;
/// A client re-sends an unconfirmed downed report this often.
const DOWN_RESEND: Duration = Duration::from_secs(2);
/// The "partner ready" cue sounds only for a partner who became ready at
/// least this long after this game entered the round.
pub const READY_CUE_AFTER: Duration = Duration::from_secs(1);
/// The count before play resumes after a shared hold.
pub const RESUME_COUNT: Duration = ndml_net::session::RESUME_COUNT;
/// After an upgrade round the count runs only when a player has waited this
/// long after picking, without a break. The host's word counts.
pub const UPGRADE_COUNT_AFTER: Duration = ndml_net::session::UPGRADE_COUNT_AFTER;

/// The run as the rules need it, each run tick.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // the session's flags, one each
pub struct RunCtx {
    /// `oController` key and `timeGameStart` (a change is a new run).
    pub key: (usize, Option<f64>),
    pub coop_run: bool,
    pub linked: bool,
    /// The start gate let the run go.
    pub gate_running: bool,
    pub me: PeerId,
    pub is_host: bool,
    /// The linked partners (without this game).
    pub partners: PeerSet,
}

/// What the run tick read from the game this frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[allow(clippy::struct_excessive_bools)] // the frame's facts, one each
pub struct Observed {
    /// `oController.pause != 0`.
    pub paused: bool,
    /// `oController.gameplay`.
    pub gameplay: bool,
    /// `oController.gameOver`.
    pub game_over: bool,
    /// `global.levelup` (pending upgrade screens).
    pub levelup: Option<f64>,
    /// XP values (for the mirror).
    pub xp: Option<XpRead>,
    /// The score numbers and kill counters (for the shared score).
    pub score: Option<Score>,
    /// The session holds this game frozen (`state::Core`): the rules never unpause it.
    pub held: bool,
    /// `oPowers.ataraxia`: this player took Ataraxia.
    pub ataraxia: bool,
}

/// A game action for the run tick (as `oController`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// `pressPause()`.
    PressPause,
    /// `pressUnpause()`.
    PressUnpause,
    /// `gotoUpgrade()` (the partner opened the upgrade screen).
    GotoUpgrade,
    /// `leaveUpgrade()`: release a held upgrade screen.
    ReleaseUpgrade,
    /// Write `oController.xp`.
    SetXp(f64),
    /// Write the host's score numbers and kill counters (globals).
    SetScore(Score),
}

/// What to do with a `pressPause` call the mod did not make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseCall {
    Allow,
    Skip,
}

/// What to do with a `pressUnpause` call the mod did not make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpauseCall {
    Allow,
    Block,
}

/// What `checkDestroyPlayer` does when the ship would die.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Death {
    /// The game's own death (and end screen).
    Die,
    /// Skip it: the ship goes down.
    Down,
}

/// What `oPowers_Step_0` (after) does this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowersAct {
    None,
    /// Hold the downed recipe (`invulnerableTime`, `playerControl`, `cloaked`, `canFire`).
    HoldDown,
    /// Full hull and shield, control back, `refreshLoadout` as `oPowers`.
    Revive,
    /// Everyone is down: let the game's death run (`invulnerableTime = 0`, `hull = 0`).
    ForceDeath,
}

/// What `oPlayer_Step_0` does this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayerAct {
    None,
    /// Hold `iFrames` and `visible = false`.
    HoldDown,
    /// Visible again, then the game's own fly-in (`oPlayer_Alarm_3`).
    Revive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// No upgrade screen.
    Idle,
    /// On the upgrade screen (chained screens included).
    Choosing,
    /// Chosen; `leaveUpgradeDeferred` skipped, the world held frozen.
    Held,
    /// `leaveUpgrade` called (at this frame); waiting for the game to leave.
    Releasing(u64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ship {
    Up,
    /// Down. A client's report not yet confirmed carries when it was sent.
    Down {
        reported: Option<Instant>,
    },
    /// Respawning: `oPowers`, then `oPlayer` (the fly-in).
    Reviving {
        powers_done: bool,
    },
    /// Everyone is down: the forced death was (or is to be) applied.
    Dying {
        applied: bool,
    },
}

fn wire_ms(d: Duration) -> u32 {
    ndml_net::time::to_wire_ms(d)
}

/// 0 to 1 as thousandths.
fn permille(x: f64) -> u16 {
    // 0..=1000 by the clamp.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let p = (x.clamp(0.0, 1.0) * 1000.0).round() as u16;
    p
}

fn ceil_secs(d: Duration) -> u64 {
    d.as_secs_f64().ceil() as u64
}

fn one(p: PeerId) -> PeerSet {
    [p].into_iter().collect()
}

/// The session rules of one run.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // independent flags of one run
pub struct Rules {
    cfg: RulesSettings,
    run_key: Option<(usize, Option<f64>)>,
    active: bool,
    separated: bool,
    me: PeerId,
    is_host: bool,
    peers: PeerSet,
    // Shared pause.
    pause: PauseGate,
    pause_since: Option<Instant>,
    /// Client: its pause sent, not yet relayed back by the host.
    pause_request: Option<Instant>,
    /// Client: its resume (out of the pause round) sent, not yet relayed.
    close_sent: bool,
    mod_paused: bool,
    // Upgrade screen.
    upgrade: UpgradeGate,
    screen: Screen,
    /// Retries of the current release (rate-limits the warn line).
    release_retries: u32,
    /// The pause key on the held upgrade screen: wait on the pause screen.
    to_pause_screen: bool,
    proposed: Option<u16>,
    done_sent: Option<u16>,
    goto_sent: Option<u64>,
    owed_since: Option<u64>,
    // Downed.
    respawn: RespawnRules,
    down: Ship,
    force_death: bool,
    game_over_seen: bool,
    /// A respawn timed by this game alone (nobody confirmed its report).
    solo_respawn: Option<Duration>,
    /// A joining game's ship came back on its own clock before the host's
    /// `Respawned`.
    back_untold: bool,
    game_time: Duration,
    control: Option<bool>,
    // XP.
    host_xp: HostXp,
    client_xp: ClientXp,
    xp_gave_up: bool,
    /// `global.baseLevel` last seen (for the level log line).
    last_base: Option<f64>,
    // Score.
    host_score: HostScore,
    client_score: ClientScore,
    /// This game's score numbers last read.
    last_score: Option<Score>,
    /// Host: the state sent this frame.
    score_sent: Option<Score>,
    // Frame bookkeeping.
    frame: u64,
    last_paused: Option<bool>,
    own_pause: Option<u64>,
    own_unpause: Option<u64>,
    last_press: Option<(bool, u64)>,
    interrupted_at: Option<Instant>,
    /// The partner was down when its link was lost: it stays down for the wait.
    partner_down_at_loss: bool,
    /// Host: the run ended while the link was lost, to be told at the return.
    over_untold: bool,
    outs: Vec<Out>,
    events: Vec<SessionEvent>,
    /// Converging from the partner's statement (`state_sync`).
    sync: state_sync::Sync,
    /// Since when this game is in a round it waits in ([`Self::in_round`]).
    round_entered: Option<Instant>,
    /// The "ready and waiting" bar showed last frame.
    partner_ready: bool,
    /// The count before play resumes, from a round's release.
    count: Option<Countdown>,
    /// The open round's picks, each with the time its unbroken wait began.
    picks: Vec<(PeerId, Instant)>,
    /// The release's word on its count, for the release being taken now.
    release_counts: Option<bool>,
    /// Host: the last release counted ([`RulesState::COUNTED`]).
    last_release_counted: bool,
    /// An upgrade round ended with no count: play resumes then.
    quiet: Option<Instant>,
    /// The link's round trip (the host's count waits half of it).
    rtt: Duration,
    /// The latest time an entry point was called with.
    now: Option<Instant>,
    /// The respawn alert's number last told.
    respawn_told: Option<u8>,
    /// Ataraxia: the round held without a screen, and whether Space asked for one.
    ataraxia_round: Option<u16>,
    choose_own: bool,
    /// The frame the start gate asked for this player's own opening screen
    /// (`gate::StartAsk`), for [`START_SCREEN_FRAMES`].
    start_screen: Option<u64>,
}

impl Rules {
    /// Rules with these settings.
    #[must_use]
    pub fn new(cfg: RulesSettings) -> Self {
        let respawn = cfg.respawn;
        Self {
            cfg,
            run_key: None,
            active: false,
            separated: false,
            me: PeerId::HOST,
            is_host: true,
            peers: PeerSet::EMPTY,
            pause: PauseGate::default(),
            pause_since: None,
            pause_request: None,
            close_sent: false,
            mod_paused: false,
            upgrade: UpgradeGate::new(PeerSet::EMPTY),
            screen: Screen::Idle,
            release_retries: 0,
            to_pause_screen: false,
            proposed: None,
            done_sent: None,
            goto_sent: None,
            owed_since: None,
            respawn: RespawnRules::new(PeerSet::EMPTY, respawn),
            down: Ship::Up,
            force_death: false,
            game_over_seen: false,
            solo_respawn: None,
            back_untold: false,
            game_time: Duration::ZERO,
            control: None,
            host_xp: HostXp::default(),
            client_xp: ClientXp::default(),
            xp_gave_up: false,
            last_base: None,
            host_score: HostScore::default(),
            client_score: ClientScore::default(),
            last_score: None,
            score_sent: None,
            frame: 0,
            last_paused: None,
            own_pause: None,
            own_unpause: None,
            last_press: None,
            interrupted_at: None,
            partner_down_at_loss: false,
            over_untold: false,
            outs: Vec::new(),
            events: Vec::new(),
            sync: state_sync::Sync::default(),
            round_entered: None,
            partner_ready: false,
            count: None,
            picks: Vec::new(),
            release_counts: None,
            last_release_counted: false,
            quiet: None,
            rtt: Duration::ZERO,
            now: None,
            respawn_told: None,
            ataraxia_round: None,
            choose_own: false,
            start_screen: None,
        }
    }

    /// The switches.
    #[must_use]
    pub fn settings(&self) -> &RulesSettings {
        &self.cfg
    }

    /// The rules are on for this run (a linked co-op run past the start gate).
    #[must_use]
    pub fn active(&self) -> bool {
        self.active
    }

    /// The partner left this run for good: its rounds and messages are ignored.
    #[must_use]
    pub fn separated(&self) -> bool {
        self.separated
    }

    /// Messages to send.
    pub fn take_outs(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.outs)
    }

    /// Alerts and notes since the last call.
    pub fn take_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }

    /// This player is on the pause menu in an open shared pause.
    #[cfg(test)]
    #[must_use]
    pub fn holding_pause(&self) -> bool {
        self.i_hold()
    }

    /// The world is frozen by the rules where the game cannot tell: on the card
    /// after choosing, or in its release. `oUpgrade` is gone and `pause` is 0
    /// there, so `oPowers_Step_1` reads "running".
    #[must_use]
    pub fn frozen_unseen(&self) -> bool {
        self.active && matches!(self.screen, Screen::Held | Screen::Releasing(_))
    }

    /// The local player is on the upgrade screen, choosing.
    #[must_use]
    pub fn upgrade_choosing(&self) -> bool {
        self.screen == Screen::Choosing
    }

    /// This game's ship is down (for `PlayerState.downed`).
    #[must_use]
    pub fn is_down(&self) -> bool {
        !matches!(self.down, Ship::Up)
    }

    /// The run is over in these rules (a game over seen or decided here).
    #[must_use]
    pub fn run_over(&self) -> bool {
        self.run_ended()
    }

    /// `peer`'s ship is down as these rules know it.
    #[must_use]
    pub fn peer_down(&self, peer: PeerId) -> bool {
        self.respawn.is_down(peer)
    }

    /// Game time of this run (advances only while the world runs).
    #[must_use]
    pub fn game_time(&self) -> Duration {
        self.game_time
    }

    /// `oPowers.playerControl` from this frame.
    pub fn set_control(&mut self, control: Option<bool>) {
        self.control = control;
    }

    /// The link's round trip, as measured (each run tick).
    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    /// A count before play resumes is running.
    #[must_use]
    pub fn counting(&self) -> bool {
        self.count.is_some()
    }

    /// Held on the card while the count runs ([`Self::count_hold_on`]).
    #[must_use]
    pub fn counting_on_card(&self) -> bool {
        self.count.is_some() && self.screen == Screen::Held
    }

    /// The session's respawn time (the host's when sent, else the local one),
    /// for downs from now on.
    pub fn set_respawn(&mut self, respawn: Duration) {
        self.cfg.respawn = respawn;
        self.respawn.set_respawn(respawn);
    }

    /// The run ended (menu, or a new run).
    pub fn end_run(&mut self) {
        let mut fresh = Self::new(self.cfg.clone());
        fresh.sync = self.sync.next_run();
        *self = fresh;
    }

    // ---- emitting ---------------------------------------------------------

    fn alone(&self) -> bool {
        self.peers.len() <= 1
    }

    /// This game decides (the host, or a player left alone).
    fn seq(&self) -> bool {
        self.is_host || self.alone()
    }

    fn bcast(&mut self, msg: Message) {
        if !self.alone() {
            self.note_out(None, &msg);
            self.outs.push(Out::Broadcast(msg));
        }
    }

    fn send_host(&mut self, msg: Message) {
        self.send_to(PeerId::HOST, msg);
    }

    fn send_to(&mut self, to: PeerId, msg: Message) {
        if !self.alone() {
            self.note_out(Some(to), &msg);
            self.outs.push(Out::Send(to, msg));
        }
    }

    // ---- the run ----------------------------------------------------------

    /// Each run tick, before [`frame`](Self::frame): start or end the rules.
    pub fn sync_run(&mut self, r: &RunCtx) {
        // A new run: another controller, or another `timeGameStart`.
        let new_run = self.run_key.is_none_or(|(k, s)| {
            k != r.key.0 || (s.is_some() && r.key.1.is_some() && s != r.key.1)
        });
        if new_run {
            self.end_run();
            self.run_key = Some(r.key);
        }
        if r.linked {
            self.me = r.me;
            self.is_host = r.is_host;
        }
        if !self.active && !self.separated && r.coop_run && r.linked && r.gate_running {
            self.active = true;
            let mut peers = r.partners;
            peers.insert(self.me);
            self.peers = peers;
            self.pause = PauseGate::new(peers);
            self.upgrade = UpgradeGate::new(peers);
            self.respawn = RespawnRules::new(peers, self.cfg.respawn);
            self.last_paused = None;
            log::info!(
                "co-op rules on as {} ({} players): respawn {} s",
                self.me,
                peers.len(),
                self.cfg.respawn.as_secs()
            );
        }
    }

    /// A partner left. `interrupted`: its link is lost and it may come back;
    /// otherwise it is gone for this run.
    pub fn peer_left(&mut self, peer: PeerId, interrupted: bool, now: Instant) {
        self.now = Some(now);
        self.sync_link_lost(peer);
        if !self.active {
            return;
        }
        if !self.peers.contains(peer) {
            if !interrupted && self.interrupted_at.take().is_some() {
                self.separated = true;
                log::info!("co-op rules: {peer} is not coming back; solo rules for this run");
            }
            return;
        }
        self.peers.remove(peer);
        if self.pause.peer_left(peer) {
            self.pause_since = None;
            log::info!("co-op rules: {peer} was the last one paused and left; resumed");
        }
        if self.alone() {
            // This game sequences now: unanswered requests are decided here.
            self.settle_own_requests(now);
            // Nothing to wait for: a fresh gate with only this game.
            self.fresh_upgrade_gate();
        } else {
            let ev = self.upgrade.set_peers(self.peers);
            self.on_upgrade(ev);
        }
        if interrupted {
            self.partner_down_at_loss = self.respawn.is_down(peer);
            self.interrupted_at = Some(now);
        } else {
            self.separated = true;
        }
        self.respawn.remove_peer(peer);
    }

    /// The partner's link is lost and the run waits for it (`crate::wait`).
    fn partner_away(&self) -> bool {
        self.interrupted_at.is_some() && !self.separated
    }

    /// Left alone: its unanswered pause requests are decided as the host would.
    fn settle_own_requests(&mut self, now: Instant) {
        if std::mem::take(&mut self.close_sent) && self.pause.holds(self.me) {
            let released = self.pause.unpause(self.me);
            self.pause_closed(self.me, released);
        }
        if self.pause_request.take().is_some() {
            self.pause_opened(self.me, now);
        }
    }

    /// A fresh upgrade gate for the current peers, the last released round kept.
    fn fresh_upgrade_gate(&mut self) {
        let last = self.upgrade.last_released();
        self.upgrade = UpgradeGate::new(self.peers);
        if let Some(r) = last {
            self.upgrade.open(r);
            self.upgrade.done(self.me, r);
        }
        self.proposed = None;
    }

    /// A partner came back: rounds held apart are dropped. Whoever is still
    /// on their pause menu pauses everyone again; the host announces its open
    /// upgrade round, and a joining game on its screen proposes one.
    fn rejoin_rounds(&mut self, now: Instant) {
        // A game whose run is over holds nobody.
        let over = self.run_ended();
        let on_menu = self.pause.holds(self.me) && !self.close_sent && !over;
        self.pause = PauseGate::new(self.peers);
        self.pause_since = None;
        self.pause_request = None;
        self.close_sent = false;
        if on_menu {
            log::info!(
                "co-op rules: still on the pause menu at the return; pausing together again"
            );
            self.local_pause(now);
        }
        if self.is_host {
            if let Some(r) = self.upgrade.active().filter(|_| !over) {
                log::info!(
                    "co-op rules: upgrade round {r} is open here; telling the returning game"
                );
                self.bcast(Message::UpgradeOpen { round: r });
            }
        } else {
            // The host's numbering from here: a gate with no history.
            self.upgrade = UpgradeGate::new(self.peers);
            self.proposed = None;
            self.done_sent = None;
            if self.screen == Screen::Choosing && !over {
                let r = self.upgrade.next_round();
                log::info!("co-op rules: on the upgrade screen at the return; proposing round {r}");
                self.proposed = Some(r);
                self.send_host(Message::UpgradeOpen { round: r });
            }
        }
    }

    /// A partner (re)joined mid-run (back after a lost link).
    pub fn peer_joined(&mut self, peer: PeerId, now: Instant) {
        if !self.active || self.separated || self.peers.contains(peer) {
            return;
        }
        self.peers.insert(peer);
        self.interrupted_at = None;
        self.partner_down_at_loss = false;
        self.pause.set_peers(self.peers);
        let ev = self.upgrade.set_peers(self.peers);
        self.on_upgrade(ev);
        self.rejoin_rounds(now);
        self.respawn.add_peer(peer);
        self.host_xp.resend();
        self.host_score.resend();
        log::info!("co-op rules: {peer} is back");
        if std::mem::take(&mut self.over_untold) {
            // Everyone went down while the link was lost: told at the return.
            log::info!("co-op rules: the run ended while the link was lost; telling {peer}");
            self.bcast(Message::Downed {
                peer: self.me,
                respawn_ms: 0,
            });
            self.score_last_word();
            self.bcast(Message::GameOver);
            return;
        }
        // Tell the sequencer where this game's ship stands.
        if matches!(self.down, Ship::Down { .. }) {
            // The time left: the own timer if one ran alone (handed over),
            // else the confirmed one.
            let gt = self.game_time;
            let left = self
                .solo_respawn
                .take()
                .map(|at| at.saturating_sub(gt))
                .or_else(|| self.respawn.remaining(self.me, gt))
                .unwrap_or(self.cfg.respawn);
            let respawn_ms = wire_ms(left).max(1);
            if self.is_host {
                // A down from the wait moves from this game's timer to the sequencer's.
                let _ = self.respawn.downed_until(self.me, gt + left);
                self.bcast(Message::Downed {
                    peer: self.me,
                    respawn_ms,
                });
            } else {
                // The host's confirmation sets the local timer again.
                self.respawn.remove_peer(self.me);
                self.respawn.add_peer(self.me);
                self.down = Ship::Down {
                    reported: Some(now),
                };
                self.send_host(Message::Downed {
                    peer: self.me,
                    respawn_ms,
                });
            }
        }
    }

    // ---- network ----------------------------------------------------------

    /// A session message. Returns false for messages that are not the rules'.
    pub fn on_message(&mut self, from: PeerId, msg: &Message, now: Instant) -> bool {
        self.now = Some(now);
        if let Message::RulesState(s) = msg {
            self.on_state(from, s, now);
            return true;
        }
        if !self.note_in(from, msg) {
            return true;
        }
        if self.separated && Self::is_rules_message(msg) {
            // The partner is gone for this run: its rules state does not apply.
            log::debug!("co-op rules: the partner is gone for this run; ignoring {from}'s {msg:?}");
            return true;
        }
        match *msg {
            Message::Pause { by } => self.on_pause(from, by, now),
            Message::Unpause { by } => self.on_unpause(by),
            Message::UpgradeOpen { round } => self.on_upgrade_open(round),
            Message::UpgradeDone { peer, round, count } => {
                self.on_upgrade_done(from, peer, round, count);
            }
            Message::UpgradeChoosing { peer, round } => self.on_upgrade_choosing(peer, round),
            Message::Downed { peer, respawn_ms } => self.on_downed(peer, respawn_ms),
            Message::Respawned { peer } => {
                if self.active && !self.seq() && self.respawn.respawned(peer) {
                    self.respawned(peer);
                }
            }
            Message::GameOver => {
                if self.active && !self.is_host {
                    self.respawn.end();
                    self.on_game_over();
                }
            }
            Message::XpState { xp, level } => {
                if self.active && !self.is_host {
                    self.client_xp.on_state(xp, level);
                }
            }
            Message::ScoreState { .. } => {
                if self.active
                    && !self.is_host
                    && let Some(s) = Score::from_message(msg)
                {
                    self.client_score.on_state(s);
                }
            }
            _ => return false,
        }
        true
    }

    /// The messages [`Self::on_message`] handles.
    fn is_rules_message(msg: &Message) -> bool {
        matches!(
            msg,
            Message::Pause { .. }
                | Message::Unpause { .. }
                | Message::UpgradeOpen { .. }
                | Message::UpgradeDone { .. }
                | Message::UpgradeChoosing { .. }
                | Message::Downed { .. }
                | Message::Respawned { .. }
                | Message::GameOver
                | Message::XpState { .. }
                | Message::ScoreState { .. }
        )
    }

    /// `by` paused (opened their pause menu): the round opens with every
    /// player in it, or `by` is back in the open one.
    fn on_pause(&mut self, from: PeerId, by: PeerId, now: Instant) {
        if !self.active {
            return;
        }
        if self.is_host && self.run_ended() {
            // This run is over: no round to hold anyone in. The requester's pause is its own.
            if by != self.me && !self.pause.holds(by) {
                self.send_to(from, Message::Unpause { by });
            }
            return;
        }
        if self.is_host && self.upgrade.active().is_some() {
            // Turned down: the world is already frozen for an upgrade round.
            if !self.pause.holds(by) {
                self.send_to(from, Message::Unpause { by });
            }
            return;
        }
        self.pause_opened(by, now);
        if self.is_host {
            self.bcast(Message::Pause { by });
        }
        if by == self.me {
            self.pause_request = None;
            self.close_sent = false;
        }
    }

    fn pause_opened(&mut self, by: PeerId, now: Instant) {
        if self.quiet.take().is_some() {
            log::info!("co-op rules: the resume with no count stopped: {by} paused");
        }
        if self.count.take().is_some() {
            log::info!("co-op rules: resume count stopped: {by} paused (hold on)");
            if self.screen == Screen::Held {
                // Counting on the held card: to the pause screen.
                self.to_pause_screen = true;
            }
        }
        if self.pause.pause(by) {
            self.pause_since = Some(now);
            log::info!("co-op rules: paused by {by}; everyone is on their pause menu");
        } else {
            log::info!("co-op rules: {by} is on their pause menu again");
        }
    }

    /// `by` resumed (closed their pause menu); the round ends when nobody
    /// is left in it.
    fn on_unpause(&mut self, by: PeerId) {
        if !self.active {
            return;
        }
        if by == self.me {
            self.pause_request = None;
            self.close_sent = false;
        }
        if !self.pause.holds(by) {
            return;
        }
        let released = self.pause.unpause(by);
        if self.is_host {
            self.bcast(Message::Unpause { by });
        }
        self.pause_closed(by, released);
    }

    fn pause_closed(&mut self, by: PeerId, released: bool) {
        if released {
            self.pause_since = None;
            log::info!("co-op rules: {by} resumed, the last one paused; play resumes");
            self.begin_count("the shared pause ended");
        } else {
            log::info!("co-op rules: {by} resumed; waiting for the others to resume");
        }
    }

    fn on_upgrade_open(&mut self, round: u16) {
        if !self.active {
            return;
        }
        let round = if self.is_host {
            let r = self.upgrade.resolve_open(round);
            self.bcast(Message::UpgradeOpen { round: r });
            r
        } else {
            round
        };
        let ev = self.upgrade.open(round);
        self.on_upgrade(ev);
    }

    fn on_upgrade_done(&mut self, from: PeerId, peer: PeerId, round: u16, count: bool) {
        if !self.active {
            return;
        }
        if self.is_host {
            if peer != from {
                // A player reports only their own pick.
                log::debug!("co-op rules: {from} reported {peer} done in round {round}; ignored");
                return;
            }
            let Some(r) = self.upgrade.resolve_done(round) else {
                return;
            };
            self.host_done(peer, r);
        } else {
            // The host's word on the release's count.
            self.release_counts = Some(count);
            let ev = self.take_done(peer, round);
            self.on_upgrade(ev);
            self.release_counts = None;
        }
    }

    /// `peer` is done with `round`, noting when this game learned of it.
    fn take_done(&mut self, peer: PeerId, round: u16) -> UpgradeEvent {
        let now = self.now.unwrap_or_else(Instant::now);
        if self.upgrade.active() == Some(round) && !self.picks.iter().any(|(p, _)| *p == peer) {
            self.picks.push((peer, now));
        }
        self.upgrade.done(peer, round)
    }

    /// The longest unbroken wait after a pick in the open round.
    fn round_waited(&self, now: Instant) -> Duration {
        self.picks
            .iter()
            .map(|(_, t)| now.saturating_duration_since(*t))
            .max()
            .unwrap_or_default()
    }

    /// Host: `peer` is done with `round`: relayed with the count, then taken.
    fn host_done(&mut self, peer: PeerId, round: u16) {
        let now = self.now.unwrap_or_else(Instant::now);
        let waited = self.round_waited(now);
        let ev = self.take_done(peer, round);
        let count = matches!(ev, UpgradeEvent::Released(_)) && waited >= UPGRADE_COUNT_AFTER;
        if let UpgradeEvent::Released(r) = ev {
            log::info!(
                "co-op rules: upgrade round {r}: the longest wait after a pick {:.1} s: {}",
                waited.as_secs_f64(),
                if count {
                    "the resume count"
                } else {
                    "no count"
                }
            );
        }
        self.bcast(Message::UpgradeDone { peer, round, count });
        self.release_counts = Some(count);
        self.on_upgrade(ev);
        self.release_counts = None;
    }

    /// `peer` chooses again in `round`. The host relays it while the round is open.
    fn on_upgrade_choosing(&mut self, peer: PeerId, round: u16) {
        if !self.active {
            return;
        }
        if self.is_host {
            if self.upgrade.undone(peer, round) {
                // A reopened screen ends that player's wait.
                self.picks.retain(|(p, _)| *p != peer);
                log::info!(
                    "co-op rules: {peer} chooses in round {round} after all; the round waits"
                );
                self.bcast(Message::UpgradeChoosing { peer, round });
            } else if peer != self.me && self.upgrade.last_released().is_some_and(|l| round <= l) {
                // The joining player's Space crossed the round's end: tell it the round ended.
                log::info!(
                    "co-op rules: {peer} would choose in round {round}, already released; told so"
                );
                self.send_to(
                    peer,
                    Message::UpgradeDone {
                        peer,
                        round,
                        count: self.last_release_counted,
                    },
                );
            }
        } else if peer != self.me && self.upgrade.undone(peer, round) {
            self.picks.retain(|(p, _)| *p != peer);
            log::info!("co-op rules: {peer} chooses in round {round} after all; the round waits");
        }
    }

    fn on_upgrade(&mut self, ev: UpgradeEvent) {
        let mut ev = ev;
        loop {
            match ev {
                UpgradeEvent::Opened(r) => {
                    log::info!("co-op rules: upgrade round {r} open");
                    if self.count.take().is_some() {
                        log::info!("co-op rules: resume count stopped: a new upgrade round");
                    }
                    self.quiet = None;
                    self.picks.clear();
                    self.goto_sent = None;
                    self.owed_since = None;
                    if let Some(p) = self.proposed.take()
                        && self.done_sent == Some(p)
                        && p != r
                    {
                        // The done went out under the own number; send it
                        // again under the host's.
                        self.done_sent = Some(r);
                        self.send_host(Message::UpgradeDone {
                            peer: self.me,
                            round: r,
                            count: false,
                        });
                    }
                    return;
                }
                UpgradeEvent::Released(r) => {
                    log::info!("co-op rules: upgrade round {r} released");
                    self.proposed = None;
                    self.picks.clear();
                    // The host's word. A release without one counts.
                    let counts = self.release_counts.take().unwrap_or(true);
                    if self.is_host {
                        self.last_release_counted = counts;
                    }
                    if let Some(next) = self.upgrade.next_queued() {
                        ev = next;
                    } else {
                        // Not for a player still on its own screen.
                        if self.screen != Screen::Choosing {
                            if counts {
                                self.begin_count("the upgrade round ended");
                            } else {
                                self.begin_quiet(r);
                            }
                        }
                        return;
                    }
                }
                UpgradeEvent::Nothing => return,
            }
        }
    }

    fn on_downed(&mut self, peer: PeerId, respawn_ms: u32) {
        if !self.active {
            return;
        }
        if self.is_host {
            // The reporter's ship is already down and waits for an answer.
            let at =
                (respawn_ms > 0).then(|| self.game_time + ndml_net::time::from_wire_ms(respawn_ms));
            self.host_downed(peer, at);
        } else {
            if peer == self.me && !matches!(self.down, Ship::Down { .. }) {
                // Each game reports its own down: the host's word for a ship
                // that flies is stale (the host takes "up" from this state).
                log::info!("co-op rules: the host has this ship down, but it is not; ignored");
                return;
            }
            let at = self.game_time + ndml_net::time::from_wire_ms(respawn_ms);
            let fresh = !self.respawn.is_down(peer);
            let out = self.respawn.downed_until(peer, at);
            if peer == self.me {
                if let Ship::Down { reported } = &mut self.down {
                    *reported = None;
                }
            } else if fresh && out != DownOutcome::Ignored {
                self.events.push(SessionEvent::PartnerDown);
                log::info!("co-op rules: {peer} is down ({respawn_ms} ms)");
            }
        }
    }

    /// Host: `peer` went down (`at`: a respawn time it brought back from a
    /// lost link).
    fn host_downed(&mut self, peer: PeerId, at: Option<Duration>) -> DownOutcome {
        let gt = self.game_time;
        let out = match at {
            Some(at) => self.respawn.downed_until(peer, at),
            None => self.respawn.downed(peer, gt),
        };
        match out {
            DownOutcome::Respawning { at } => {
                log::info!(
                    "co-op rules: {peer} is down; respawn in {:.0} s",
                    at.saturating_sub(gt).as_secs_f64()
                );
                self.bcast(Message::Downed {
                    peer,
                    respawn_ms: wire_ms(at.saturating_sub(gt)).max(1),
                });
                if peer != self.me {
                    self.events.push(SessionEvent::PartnerDown);
                }
            }
            DownOutcome::GameOver => {
                log::info!("co-op rules: {peer} is down and nobody is left standing: game over");
                self.bcast(Message::Downed {
                    peer,
                    respawn_ms: 0,
                });
                self.score_last_word();
                self.bcast(Message::GameOver);
                self.on_game_over();
            }
            DownOutcome::Ignored => {
                if let Some(left) = self.respawn.remaining(peer, gt) {
                    self.bcast(Message::Downed {
                        peer,
                        respawn_ms: wire_ms(left).max(1),
                    });
                }
            }
        }
        out
    }

    fn on_game_over(&mut self) {
        self.count = None;
        self.quiet = None;
        if !self.game_over_seen {
            self.game_over_seen = true;
            self.events.push(SessionEvent::AllDown);
        }
        self.force_death = true;
        if matches!(self.down, Ship::Down { .. } | Ship::Reviving { .. })
            || (self.down == Ship::Up && std::mem::take(&mut self.back_untold))
        {
            // Back on its own clock but down on the host's count: the host's word holds.
            self.down = Ship::Dying { applied: false };
        }
        self.leave_rounds();
    }

    /// The run is over here: it leaves its rounds as a Resume or choice would, holding nobody.
    fn leave_rounds(&mut self) {
        if (self.pause.holds(self.me) && !self.close_sent) || self.pause_request.is_some() {
            log::info!("co-op rules: the run is over here; out of the shared pause");
            self.local_unpause();
        }
        if let Some(r) = self.open_round().filter(|_| self.i_owe()) {
            log::info!("co-op rules: the run is over here; done with upgrade round {r}");
            self.local_done(r);
        }
    }

    /// The run is over here: this game's game over or the host's.
    fn run_ended(&self) -> bool {
        self.game_over_seen || self.respawn.is_game_over()
    }

    /// The session rules keep this game paused (a pause round, or held for the
    /// partner's upgrade round). A lifting link-wait freeze leaves it paused.
    #[must_use]
    pub fn holds_game(&self) -> bool {
        self.active
            && !self.run_ended()
            && ((self.pause.is_paused() && !self.only_my_close_pending()) || self.hold_for_round())
    }

    fn respawned(&mut self, peer: PeerId) {
        if peer == self.me {
            if std::mem::take(&mut self.back_untold) {
                log::info!("co-op rules: the host's word on this ship's respawn (back already)");
                return;
            }
            if !matches!(self.down, Ship::Down { .. }) {
                // Up already: a second revive would teleport a flying ship.
                log::info!("co-op rules: told this ship respawned, but it is not down; ignored");
                return;
            }
            log::info!("co-op rules: respawning");
            self.down = Ship::Reviving { powers_done: false };
            self.events.push(SessionEvent::Back);
            // The respawn alert's go, on the returning game only.
            self.events.push(SessionEvent::CountGo(CountKind::Respawn));
        } else {
            log::info!("co-op rules: {peer} respawned");
            self.events.push(SessionEvent::PartnerBack);
        }
    }

    // ---- local events ------------------------------------------------------

    fn local_pause(&mut self, now: Instant) {
        if self.seq() {
            if self.upgrade.active().is_none() {
                self.pause_opened(self.me, now);
                self.bcast(Message::Pause { by: self.me });
            }
        } else {
            self.pause_request = Some(now);
            self.close_sent = false;
            self.send_host(Message::Pause { by: self.me });
        }
    }

    /// This player resumed: out of the round (once per round).
    fn local_unpause(&mut self) {
        if self.seq() {
            if self.pause.holds(self.me) {
                let released = self.pause.unpause(self.me);
                self.bcast(Message::Unpause { by: self.me });
                self.pause_closed(self.me, released);
            }
        } else if (self.pause.holds(self.me) || self.pause_request.is_some()) && !self.close_sent {
            self.close_sent = true;
            self.send_host(Message::Unpause { by: self.me });
        }
        self.pause_request = None;
    }

    fn local_done(&mut self, round: u16) {
        self.done_sent = Some(round);
        if self.seq() {
            let Some(r) = self.upgrade.resolve_done(round) else {
                return;
            };
            self.host_done(self.me, r);
        } else {
            self.send_host(Message::UpgradeDone {
                peer: self.me,
                round,
                count: false,
            });
        }
    }

    // ---- hooks --------------------------------------------------------------

    /// The link wait's pause becomes this player's (`crate::wait`): the shared
    /// pause opens. Call after [`Self::peer_joined`].
    pub fn adopt_pause(&mut self, now: Instant) {
        if !self.active || self.separated || self.i_hold() || self.run_ended() {
            return;
        }
        log::info!("co-op rules: the link wait's pause becomes the shared pause");
        self.local_pause(now);
    }

    /// The mod itself is about to pause (`true`) or unpause the game.
    pub fn note_own(&mut self, pause: bool) {
        if pause {
            self.own_pause = Some(self.frame);
        } else {
            self.own_unpause = Some(self.frame);
        }
    }

    /// Held on the card after choosing: the run tick polls the pause key.
    #[must_use]
    pub fn held_on_card(&self) -> bool {
        self.screen == Screen::Held && !self.to_pause_screen
    }

    /// The pause key on the held card: this game alone waits on its pause screen
    /// instead (the game's pause over the card would wake the frozen world).
    pub fn pause_key_on_card(&mut self) {
        if !self.held_on_card() {
            return;
        }
        log::info!(
            "co-op rules: the pause key on the held upgrade screen; waiting on the pause screen instead"
        );
        self.to_pause_screen = true;
    }

    /// `pressPause` called by the game (the pause key, or focus loss).
    pub fn pause_request(&mut self, focus_lost: bool) -> PauseCall {
        if !self.active {
            return PauseCall::Allow;
        }
        if self.screen != Screen::Idle {
            // On the upgrade screen or held after it: the world is already frozen.
            return PauseCall::Skip;
        }
        if focus_lost && !self.cfg.pause_on_focus_loss {
            return PauseCall::Skip;
        }
        PauseCall::Allow
    }

    /// `pressUnpause` called by the game (Resume or Esc on the pause screen).
    /// `gate_holding`: the start gate holds this game.
    pub fn unpause_request(&mut self, gate_holding: bool, now: Instant) -> UnpauseCall {
        if gate_holding {
            return UnpauseCall::Block;
        }
        self.now = Some(now);
        if !self.active || self.screen != Screen::Idle || self.run_ended() {
            return UnpauseCall::Allow;
        }
        if self.resuming() {
            // The count or the quiet resume resumes the game.
            return UnpauseCall::Block;
        }
        if self.pause.is_paused() || self.pause_request.is_some() {
            if self.i_hold() {
                // This player's Resume: out of the round. Alone, the last one
                // out resumes here. Otherwise the count resumes this game.
                let others = self.others_hold();
                self.local_unpause();
                if !others && self.alone() {
                    return UnpauseCall::Allow;
                }
            }
            return UnpauseCall::Block;
        }
        if self.hold_for_round() {
            // Held for the partner's round: it ends when both are done.
            return UnpauseCall::Block;
        }
        UnpauseCall::Allow
    }

    /// The glue lifts `oPowers.playerControl` around `gotoUpgrade` and
    /// `gotoUpgradeDeferred` while the ship is out of play (down, coming back or
    /// `Dying`) and the screen is one the rules opened or its chain. Both calls
    /// refuse while `playerControl` is 0.
    #[must_use]
    pub fn lifts_control(&self) -> bool {
        self.active
            && !matches!(self.down, Ship::Up)
            && (matches!(self.screen, Screen::Choosing | Screen::Releasing(_))
                || self.goto_sent.is_some())
    }

    /// `gotoUpgradeDeferred` ran: an upgrade screen opened. True for the first
    /// screen of a round the partner opened here.
    pub fn upgrade_opened(&mut self) -> bool {
        if !self.active || self.screen == Screen::Choosing {
            return false;
        }
        let was_held = self.screen == Screen::Held;
        self.screen = Screen::Choosing;
        let expected = self.goto_sent.take().is_some();
        let own = std::mem::take(&mut self.choose_own);
        let start = self
            .start_screen
            .take()
            .is_some_and(|f| self.frame.saturating_sub(f) <= START_SCREEN_FRAMES);
        if expected {
            // A screen of the partner's round fades in.
            return !own && !start;
        }
        if self.respawn.is_game_over() {
            return false;
        }
        if !matches!(self.down, Ship::Up) {
            // A ship out of play opens no round: a screen here is a released card's chain.
            log::info!("co-op rules: a screen chained here while down; it opens no round");
            return false;
        }
        if self.upgrade.active().is_some() && self.upgrade.is_waiting_on(self.me) {
            return false; // choosing for the open round
        }
        if let Some(r) = self.upgrade.active().filter(|_| was_held) {
            // The upgrade key on the waiting card: its own screen, and the round waits.
            log::info!(
                "co-op rules: this player's own screen opened again on the wait; round {r} waits for it"
            );
            self.choose_again(r);
            return false;
        }
        log::info!("co-op rules: the upgrade screen opened here");
        if self.seq() {
            let r = self.upgrade.resolve_open(self.upgrade.next_round());
            self.bcast(Message::UpgradeOpen { round: r });
            let ev = self.upgrade.open(r);
            self.on_upgrade(ev);
        } else {
            let r = self.upgrade.next_round();
            self.proposed = Some(r);
            self.send_host(Message::UpgradeOpen { round: r });
        }
        false
    }

    /// `leaveUpgradeDeferred` is about to run. Returns true to skip it: the
    /// world stays frozen until the round is released.
    pub fn upgrade_leaving(&mut self, now: Instant) -> bool {
        self.now = Some(now);
        if !self.active {
            return false;
        }
        let was = self.screen;
        self.screen = Screen::Idle;
        if matches!(was, Screen::Releasing(_)) {
            return false;
        }
        let Some(r) = self.open_round().or(self.proposed) else {
            return false;
        };
        if self.done_sent != Some(r) {
            self.local_done(r);
        }
        if self.others_choosing() || self.pending_open() {
            self.screen = Screen::Held;
            log::info!("co-op rules: chose; holding the upgrade screen for the partner");
            true
        } else if self.resuming() || (!self.alone() && self.open_round().is_some()) {
            // The last to choose: held on the card until the count ends.
            self.screen = Screen::Held;
            log::info!("co-op rules: chose last; held on the card for the resume count");
            true
        } else {
            false
        }
    }

    /// The ship would die now (`checkDestroyPlayer`, no Last Stand left).
    /// Solo or alone: the game's own death. While the link is lost: a down on
    /// this game's own timer, or the game over if the partner was down too.
    pub fn death(&mut self, now: Instant) -> Death {
        if !self.active || self.force_death || self.respawn.is_game_over() {
            return Death::Die;
        }
        if self.is_down() {
            return Death::Down;
        }
        if self.alone() {
            return if self.partner_away() {
                self.down_while_away()
            } else {
                Death::Die
            };
        }
        if self.is_host {
            if self.host_downed(self.me, None) == DownOutcome::GameOver {
                return Death::Die;
            }
            self.down = Ship::Down { reported: None };
        } else {
            log::info!("co-op rules: this ship is down; reported to the host");
            self.down = Ship::Down {
                reported: Some(now),
            };
            self.send_host(Message::Downed {
                peer: self.me,
                respawn_ms: 0,
            });
        }
        self.events.push(SessionEvent::Down);
        Death::Down
    }

    /// [`death`](Self::death) while the partner's link is lost.
    fn down_while_away(&mut self) -> Death {
        if self.partner_down_at_loss {
            // The partner, down at the loss, stays down: nobody is left standing.
            log::info!(
                "co-op rules: this ship is down while the link is lost and the partner is down too: game over (told at the return)"
            );
            self.respawn.end();
            self.over_untold = self.is_host;
            self.on_game_over();
            return Death::Die;
        }
        let respawn = self.cfg.respawn;
        log::info!(
            "co-op rules: this ship is down while the link is lost; respawn in {} s on this game's own timer",
            respawn.as_secs()
        );
        self.down = Ship::Down { reported: None };
        self.solo_respawn = Some(self.game_time + respawn);
        self.events.push(SessionEvent::Down);
        Death::Down
    }

    /// What `oPowers_Step_0` does this frame.
    #[must_use]
    pub fn powers_action(&self) -> PowersAct {
        match self.down {
            Ship::Down { .. } => PowersAct::HoldDown,
            Ship::Reviving { powers_done: false } => PowersAct::Revive,
            // Not while its upgrade screen is open or held: `refreshLoadout` gives the hull back.
            Ship::Dying { applied: false } if self.screen == Screen::Idle => PowersAct::ForceDeath,
            Ship::Reviving { .. } | Ship::Up | Ship::Dying { .. } => PowersAct::None,
        }
    }

    /// The `oPowers` part of the action was applied.
    pub fn powers_applied(&mut self) {
        match &mut self.down {
            Ship::Reviving { powers_done, .. } => *powers_done = true,
            Ship::Dying { applied } => *applied = true,
            _ => {}
        }
    }

    /// The ship is down and hidden: down, or respawning but not back yet.
    /// False when up and once the game's own death follows (`Dying`).
    #[must_use]
    pub fn ship_hidden(&self) -> bool {
        matches!(self.down, Ship::Down { .. } | Ship::Reviving { .. })
    }

    /// What `oPlayer_Step_0` does this frame.
    #[must_use]
    pub fn player_action(&self) -> PlayerAct {
        match self.down {
            Ship::Down { .. } | Ship::Reviving { powers_done: false } => PlayerAct::HoldDown,
            Ship::Reviving { powers_done: true } => PlayerAct::Revive,
            Ship::Up | Ship::Dying { .. } => PlayerAct::None,
        }
    }

    /// The `oPlayer` part of a revive was applied: the ship is back.
    pub fn player_applied(&mut self) {
        if matches!(self.down, Ship::Reviving { .. }) {
            self.down = Ship::Up;
        }
    }

    // ---- per frame ------------------------------------------------------------

    /// This player is on the pause menu in the shared pause (or its pause is
    /// on its way to the host).
    fn i_hold(&self) -> bool {
        (self.pause.holds(self.me) && !self.close_sent) || self.pause_request.is_some()
    }

    /// Client: its Resume is on its way and nobody else is in the round.
    /// Every path that lists it in a round again clears `close_sent` first.
    fn only_my_close_pending(&self) -> bool {
        self.close_sent
            && self.pause.holds(self.me)
            && self.pause.holders().minus(one(self.me)).is_empty()
    }

    /// A partner is still on their pause menu in the shared pause.
    fn others_hold(&self) -> bool {
        !self.pause.holders().minus(one(self.me)).is_empty()
    }

    fn open_round(&self) -> Option<u16> {
        self.upgrade.active()
    }

    /// A client's own open not yet numbered by the host.
    fn pending_open(&self) -> bool {
        self.proposed.is_some() && self.upgrade.active().is_none()
    }

    fn others_choosing(&self) -> bool {
        self.open_round().is_some() && !self.upgrade.waiting_on().minus(one(self.me)).is_empty()
    }

    fn i_owe(&self) -> bool {
        self.open_round()
            .is_some_and(|r| self.upgrade.is_waiting_on(self.me) && self.done_sent != Some(r))
    }

    /// Done (or nothing to choose) while others choose: held on the pause screen.
    fn hold_for_round(&self) -> bool {
        self.screen == Screen::Idle && self.others_choosing() && !self.i_owe()
    }

    /// A round was released here. With the partner, both games count 3, 2, 1,
    /// the host after half the round trip. Alone or with the run over, the game
    /// resumes at once.
    fn begin_count(&mut self, why: &str) {
        if !self.active || self.alone() || self.run_ended() {
            self.mod_paused = true;
            return;
        }
        if self.pause.is_paused() || self.open_round().is_some() {
            // The other hold goes on: its end counts.
            log::info!("co-op rules: {why}; the other hold goes on, its end counts");
            return;
        }
        let now = self.now.unwrap_or_else(Instant::now);
        let lead = if self.is_host {
            self.rtt / 2
        } else {
            Duration::ZERO
        };
        log::info!(
            "co-op rules: resume count: {why}; {} s after a lead of {} ms",
            RESUME_COUNT.as_secs(),
            lead.as_millis()
        );
        self.count = Some(Countdown::new(now, RESUME_COUNT, lead));
    }

    /// An upgrade round ended with nobody waiting [`UPGRADE_COUNT_AFTER`]:
    /// play resumes with no count, the host after half the round trip.
    fn begin_quiet(&mut self, round: u16) {
        if !self.active || self.alone() || self.run_ended() {
            self.mod_paused = true;
            return;
        }
        if self.pause.is_paused() || self.open_round().is_some() {
            log::info!(
                "co-op rules: upgrade round {round} ended with no count; the other hold goes on, its end counts"
            );
            return;
        }
        let now = self.now.unwrap_or_else(Instant::now);
        let lead = if self.is_host {
            self.rtt / 2
        } else {
            Duration::ZERO
        };
        log::info!(
            "co-op rules: upgrade round {round} ended with nobody waiting {} s: play resumes with no count after a lead of {} ms",
            UPGRADE_COUNT_AFTER.as_secs(),
            lead.as_millis()
        );
        self.quiet = Some(now + lead);
    }

    /// The count, or the quiet resume, is on its way.
    fn resuming(&self) -> bool {
        self.count.is_some() || self.quiet.is_some()
    }

    /// The count's numbers and its end.
    fn count_tick(&mut self, now: Instant) {
        if self.quiet.is_some_and(|t| now >= t) {
            self.quiet = None;
            self.mod_paused = true;
            log::info!("co-op rules: play resumes with no count");
            self.events.push(SessionEvent::ResumedNoCount);
        }
        let Some(c) = &mut self.count else { return };
        match c.step(now) {
            Step::Tick(n) => self
                .events
                .push(SessionEvent::CountTick(CountKind::Resume, n)),
            Step::Go => {
                self.count = None;
                // Resumed by the mod, whoever closed last.
                self.mod_paused = true;
                log::info!("co-op rules: resume count over: play resumes");
                self.events.push(SessionEvent::CountGo(CountKind::Resume));
            }
            Step::Wait => {}
        }
    }

    /// Esc (or the pause key on the held card) while the count runs: the
    /// count stops and a new shared pause opens with this player in it.
    pub fn count_hold_on(&mut self, now: Instant) {
        self.now = Some(now);
        if self.count.take().is_none() {
            return;
        }
        log::info!("co-op rules: Esc in the resume count: hold on (a new shared pause)");
        if self.screen == Screen::Held {
            self.to_pause_screen = true;
        }
        self.local_pause(now);
    }

    /// Time left on this ship's respawn clock (game time).
    fn my_respawn_left(&self) -> Option<Duration> {
        let gt = self.game_time;
        self.respawn
            .remaining(self.me, gt)
            .or_else(|| self.solo_respawn.map(|at| at.saturating_sub(gt)))
    }

    /// The downed player's 3, 2, 1 before its ship's return, on the respawn clock.
    fn respawn_alert(&mut self) {
        let n = if matches!(self.down, Ship::Down { .. }) && !self.run_ended() {
            self.my_respawn_left().and_then(alert_number)
        } else {
            None
        };
        if n != self.respawn_told {
            if let Some(n) = n {
                log::info!("co-op rules: respawn alert: {n}");
                self.events
                    .push(SessionEvent::CountTick(CountKind::Respawn, n));
            }
            self.respawn_told = n;
        }
    }

    /// Held on the pause screen for the partner's round with Ataraxia: Space opens its own.
    fn ataraxia_held(&self) -> bool {
        self.ataraxia_round.is_some()
            && self.ataraxia_round == self.open_round()
            && self.hold_for_round()
    }

    /// The start gate asks for this player's own opening screen (`gate::StartAsk`).
    pub fn start_screen_asked(&mut self) {
        self.start_screen = Some(self.frame);
    }

    /// Space on the Ataraxia hold (row 0 "Choose Upgrade"): its own screen opens. True when taken.
    pub fn choose_own(&mut self) -> bool {
        if !self.ataraxia_held() || self.choose_own {
            return false;
        }
        let Some(r) = self.open_round() else {
            return false;
        };
        log::info!(
            "co-op rules: Ataraxia: Space; this player's own upgrade screen opens, round {r} waits for it"
        );
        self.choose_own = true;
        self.choose_again(r);
        true
    }

    /// This player chooses again in the open round `r`, which waits for its pick.
    fn choose_again(&mut self, r: u16) {
        self.done_sent = None;
        self.picks.retain(|(p, _)| *p != self.me);
        let msg = Message::UpgradeChoosing {
            peer: self.me,
            round: r,
        };
        self.upgrade.undone(self.me, r);
        if self.seq() {
            self.bcast(msg);
        } else {
            self.send_host(msg);
        }
    }

    /// Open the screen [`Self::choose_own`] asked for: out of the pause, then `gotoUpgrade`.
    fn direct_own(&mut self, o: &Observed, actions: &mut Vec<Action>) {
        let f = self.frame;
        if let Some(t) = self.goto_sent {
            if f.saturating_sub(t) > GOTO_WAIT {
                log::warn!("co-op rules: Ataraxia: the upgrade screen did not open");
                self.goto_sent = None;
                self.choose_own = false;
            }
            return;
        }
        if self.open_round().is_none() {
            // The round ended first: nothing to open for.
            self.choose_own = false;
            return;
        }
        if o.paused {
            if !o.held && self.press_ok(false) {
                self.own_unpause = Some(f);
                actions.push(Action::PressUnpause);
            }
            return;
        }
        if o.gameplay {
            self.goto_sent = Some(f);
            actions.push(Action::GotoUpgrade);
        }
    }

    fn press_ok(&mut self, pause: bool) -> bool {
        let f = self.frame;
        if self
            .last_press
            .is_some_and(|(k, t)| k == pause && f.saturating_sub(t) < PRESS_EVERY)
        {
            return false;
        }
        self.last_press = Some((pause, f));
        true
    }

    /// One run tick. Returns the game actions, in order (at most one deferred
    /// script call).
    pub fn frame(&mut self, o: &Observed, now: Instant) -> Vec<Action> {
        self.now = Some(now);
        self.frame += 1;
        let f = self.frame;
        let mut actions = Vec::new();
        // Stated at the game over too (a joining game that missed it).
        self.state_tick();
        if self.active && (o.game_over || self.run_ended()) {
            self.leave_rounds();
        }
        if !self.active || o.game_over {
            self.last_paused = Some(o.paused);
            return actions;
        }
        if self
            .own_pause
            .is_some_and(|t| f.saturating_sub(t) > OWN_WINDOW)
        {
            self.own_pause = None;
        }
        if self
            .own_unpause
            .is_some_and(|t| f.saturating_sub(t) > OWN_WINDOW)
        {
            self.own_unpause = None;
        }
        let running = o.gameplay && !o.paused && self.screen == Screen::Idle;
        if running {
            self.game_time += FRAME;
        }
        if let Some(prev) = self.last_paused
            && prev != o.paused
        {
            if o.paused {
                if self.own_pause.take().is_none() {
                    self.local_pause(now);
                }
            } else if self.own_unpause.take().is_none() {
                self.local_unpause();
            }
        }
        self.last_paused = Some(o.paused);
        if !o.paused && self.own_pause.is_none() {
            self.mod_paused = false;
        }
        self.downed_tick(now);
        self.respawn_alert();
        self.count_tick(now);
        self.xp_tick(o, running, now, &mut actions);
        self.score_tick(o, running, now, &mut actions);
        self.direct(o, &mut actions);
        self.ready_cue(o.paused, now);
        actions
    }

    /// This game holds up an open round: on its pause menu, or choosing its upgrade.
    fn in_round(&self) -> bool {
        !self.alone()
            && ((self.pause.is_paused() && self.i_hold())
                || (self.screen == Screen::Choosing && self.open_round().is_some()))
    }

    /// [`SessionEvent::PartnerReady`] once as the "ready and waiting" bar starts,
    /// for a partner ready [`READY_CUE_AFTER`] or more into the round.
    fn ready_cue(&mut self, paused: bool, now: Instant) {
        if !self.in_round() {
            self.round_entered = None;
            self.partner_ready = false;
            return;
        }
        let entered = *self.round_entered.get_or_insert(now);
        let f = self.facts(now, paused);
        let ready = f.pause == Some(SharedPause::PartnerReady)
            || f.upgrade == Some(UpgradeWait::PartnerReady);
        if ready && !self.partner_ready {
            let after = now.saturating_duration_since(entered);
            if after >= READY_CUE_AFTER {
                log::info!(
                    "co-op rules: the partner is ready ({:.1} s into the round): the cue",
                    after.as_secs_f64()
                );
                self.events.push(SessionEvent::PartnerReady);
            } else {
                log::info!(
                    "co-op rules: the partner is ready ({:.1} s into the round): no cue",
                    after.as_secs_f64()
                );
            }
        }
        self.partner_ready = ready;
    }

    fn downed_tick(&mut self, now: Instant) {
        if let Ship::Down { reported: Some(at) } = self.down {
            if self.alone() {
                // Nobody to confirm: the own timer (the partner left, it did
                // not die).
                self.solo_respawn = Some(self.game_time + self.cfg.respawn);
                self.down = Ship::Down { reported: None };
            } else if now.saturating_duration_since(at) >= DOWN_RESEND {
                self.down = Ship::Down {
                    reported: Some(now),
                };
                self.send_host(Message::Downed {
                    peer: self.me,
                    respawn_ms: 0,
                });
            }
        }
        if self.seq() {
            for p in self.respawn.due(self.game_time) {
                if self.respawn.respawned(p) {
                    self.bcast(Message::Respawned { peer: p });
                    self.respawned(p);
                }
            }
        } else if matches!(self.down, Ship::Down { reported: None })
            && !self.run_ended()
            && self
                .respawn
                .remaining(self.me, self.game_time)
                .is_some_and(|left| left.is_zero())
        {
            // A joining ship comes back at its confirmed time on its own clock, not at `Respawned`.
            self.respawn.respawned(self.me);
            log::info!("co-op rules: this ship's respawn time came on this game's clock");
            self.respawned(self.me);
            self.back_untold = true;
        }
        if self.solo_respawn.is_some_and(|at| self.game_time >= at) {
            self.solo_respawn = None;
            if matches!(self.down, Ship::Down { .. }) {
                self.respawned(self.me);
            }
        }
    }

    fn xp_tick(&mut self, o: &Observed, running: bool, now: Instant, actions: &mut Vec<Action>) {
        let Some(r) = o.xp else { return };
        if self.last_base.is_some_and(|b| r.base > b) {
            log::info!("co-op rules: this game reached level {}", r.base);
        }
        self.last_base = Some(r.base);
        if self.is_host {
            if !self.alone()
                && let Some(m) = self.host_xp.sample(r, now)
            {
                self.bcast(m);
            }
        } else {
            match self.client_xp.apply(r, running) {
                XpStep::Write(v) => actions.push(Action::SetXp(v)),
                XpStep::GaveUp => {
                    if !self.xp_gave_up {
                        self.xp_gave_up = true;
                        log::warn!(
                            "co-op rules: the game did not take a mirrored level in time; levels may differ from the host's"
                        );
                    }
                }
                XpStep::None => {}
            }
        }
    }

    fn score_tick(&mut self, o: &Observed, running: bool, now: Instant, actions: &mut Vec<Action>) {
        if !self.active {
            return;
        }
        let Some(s) = o.score else { return };
        self.last_score = Some(s);
        if self.is_host {
            if !self.alone()
                && let Some(m) = self.host_score.sample(s, now)
            {
                self.bcast(m);
                self.score_sent = Some(s);
            }
        } else if let Some(w) = self.client_score.apply(s, running) {
            actions.push(Action::SetScore(w));
        }
    }

    /// Host, just before `GameOver`: the score as it is now, so the end
    /// screens read the same numbers.
    fn score_last_word(&mut self) {
        if !self.is_host {
            return;
        }
        if let Some(s) = self.last_score
            && let Some(m) = self.host_score.last_word(s, Instant::now())
        {
            self.bcast(m);
            self.score_sent = Some(s);
        }
    }

    /// Host: the score state sent since the last call.
    pub fn take_score_sent(&mut self) -> Option<Score> {
        self.score_sent.take()
    }

    /// The joining game's own score is the host's: its `updateScore` is
    /// skipped (a linked co-op run, the rules on).
    #[must_use]
    pub fn skips_own_score(&self) -> bool {
        self.active && !self.is_host
    }

    /// The joining game skipped one of its own `updateScore` calls.
    pub fn own_score_skipped(&mut self) {
        self.client_score.own_skipped();
    }

    /// The joining game's end-screen score and whether `finalBossDefeated`
    /// becomes 1 (`None`: not a joining game).
    #[must_use]
    pub fn end_screen_score(&self, own: Score) -> Option<(Score, bool)> {
        self.skips_own_score().then(|| {
            (
                self.client_score.end_screen(own),
                self.client_score.final_boss(),
            )
        })
    }

    /// Joining game: (own `updateScore` calls skipped, writes of the
    /// host's score, the host's score as last heard).
    #[must_use]
    pub fn score_counts(&self) -> (u64, u64, Option<Score>) {
        let (s, w) = self.client_score.counts();
        (s, w, self.client_score.host())
    }

    fn direct(&mut self, o: &Observed, actions: &mut Vec<Action>) {
        if self.direct_screen(actions) {
            return;
        }
        let f = self.frame;
        if self.run_ended() {
            // Everyone is down: out of any pause, since the forced death runs only while
            // the world does. Not while the session holds the game.
            if o.paused && !o.held && self.press_ok(false) {
                self.own_unpause = Some(f);
                actions.push(Action::PressUnpause);
            }
            return;
        }
        let mine = self.i_hold();
        if self.choose_own {
            // This player owes the round again (Space): its screen opens here.
            self.direct_own(o, actions);
            return;
        }
        if let Some(r) = self.open_round().filter(|_| self.i_owe()) {
            self.direct_choose(r, o, mine, actions);
            return;
        }
        let hold_round = self.hold_for_round();
        // The shared pause holds this game while anyone is on a pause menu,
        // except while only its own Resume awaits the host's relay. A joining
        // game's own pause on its way to the host holds it too.
        let held = (self.pause.is_paused() || self.pause_request.is_some())
            && !self.only_my_close_pending();
        if held || hold_round {
            // Paused in the round (on a menu or held): this game is resumed
            // by the mod at the release, unless its own Resume ends it.
            if o.paused {
                self.mod_paused = true;
            } else if self.press_ok(true) {
                self.own_pause = Some(f);
                self.mod_paused = true;
                actions.push(Action::PressPause);
            }
        } else if self.only_my_close_pending() {
            // This game's Resume ended the round: it stays on its pause
            // screen until the relay, and the count after it resumes it.
            if o.paused {
                self.mod_paused = true;
            }
        } else if o.paused
            && self.mod_paused
            && !mine
            && !o.held
            && self.count.is_none()
            && self.press_ok(false)
        {
            // Not while the session holds the game (`mod_paused` stays).
            self.own_unpause = Some(f);
            actions.push(Action::PressUnpause);
        }
    }

    /// The upgrade screen's own states. True when it decided this frame.
    fn direct_screen(&mut self, actions: &mut Vec<Action>) -> bool {
        let f = self.frame;
        match self.screen {
            Screen::Choosing => {
                self.goto_sent = None;
                true
            }
            Screen::Held => {
                if let Some(r) = self.open_round().filter(|_| self.i_owe()) {
                    // A round opened while held after choosing (the host's,
                    // announced at a return): this player chose already.
                    self.local_done(r);
                }
                let waits = self.others_choosing()
                    || self.pending_open()
                    || self.others_hold()
                    || self.resuming()
                    || (!self.alone() && self.open_round().is_some());
                if !self.run_ended() && waits {
                    if self.to_pause_screen {
                        // The pause key: the card goes; `direct` then holds
                        // this game on its pause screen.
                        actions.push(Action::ReleaseUpgrade);
                        self.screen = Screen::Releasing(f);
                        self.release_retries = 0;
                    }
                } else {
                    log::info!("co-op rules: releasing the upgrade screen");
                    actions.push(Action::ReleaseUpgrade);
                    self.screen = Screen::Releasing(f);
                    self.release_retries = 0;
                }
                self.to_pause_screen = false;
                true
            }
            Screen::Releasing(t) => {
                if f.saturating_sub(t) > RELEASE_RETRY {
                    // The first retry, then one line every 90 (about 30 s).
                    self.release_retries += 1;
                    let n = self.release_retries;
                    if n == 1 || n.is_multiple_of(RELEASE_WARN_EVERY) {
                        log::warn!(
                            "co-op rules: the upgrade screen did not close; releasing again ({n} tries)"
                        );
                    }
                    actions.push(Action::ReleaseUpgrade);
                    self.screen = Screen::Releasing(f);
                }
                true
            }
            Screen::Idle => false,
        }
    }

    /// A round is open and this player has not chosen. A downed player
    /// with levels chooses too ([`Self::lifts_control`]).
    fn direct_choose(&mut self, r: u16, o: &Observed, mine: bool, actions: &mut Vec<Action>) {
        let f = self.frame;
        if self.client_xp.owed() > 0 {
            let since = *self.owed_since.get_or_insert(f);
            if f.saturating_sub(since) < OWED_WAIT {
                return;
            }
        }
        // The downed recipe holds `playerControl` at 0: not a reason here.
        let can =
            o.levelup.is_some_and(|l| l >= 1.0) && (self.control != Some(false) || self.is_down());
        if !can {
            log::info!("co-op rules: nothing to choose in round {r}; waiting for the partner");
            self.local_done(r);
            return;
        }
        if o.ataraxia {
            // Ataraxia: done at once, held on the pause screen ([`Self::choose_own`]).
            log::info!(
                "co-op rules: Ataraxia: no upgrade screen for round {r}; waiting for the partner (Space opens this player's own)"
            );
            self.ataraxia_round = Some(r);
            self.local_done(r);
            return;
        }
        if let Some(t) = self.goto_sent {
            if f.saturating_sub(t) > GOTO_WAIT {
                log::warn!(
                    "co-op rules: the upgrade screen did not open; waiting for the partner instead"
                );
                self.goto_sent = None;
                self.local_done(r);
            }
            return;
        }
        if o.paused {
            // Not while the session holds the game: the screen opens after.
            if !mine && !o.held && self.press_ok(false) {
                self.own_unpause = Some(f);
                actions.push(Action::PressUnpause);
            }
            return;
        }
        if o.gameplay {
            self.goto_sent = Some(f);
            actions.push(Action::GotoUpgrade);
        }
    }

    // ---- presentation ------------------------------------------------------------

    /// What to show this frame (`paused`: `oController.pause`).
    #[must_use]
    pub fn facts(&self, now: Instant, paused: bool) -> SessionFacts {
        if !self.active {
            return SessionFacts::default();
        }
        let pause = if self.alone() || !(self.pause.is_paused() || self.pause_request.is_some()) {
            None
        } else if !self.i_hold() {
            self.others_hold().then_some(SharedPause::WaitingForPartner)
        } else if self.others_hold() || self.pause_request.is_some() {
            Some(SharedPause::BothPaused)
        } else {
            Some(SharedPause::PartnerReady)
        };
        let mut s = SessionFacts {
            pause,
            ..SessionFacts::default()
        };
        s.upgrade = match self.screen {
            Screen::Held if self.others_choosing() || self.pending_open() => {
                Some(UpgradeWait::PartnerChoosing)
            }
            Screen::Choosing if self.open_round().is_some() && !self.others_choosing() => {
                Some(UpgradeWait::PartnerReady)
            }
            Screen::Idle if self.hold_for_round() => Some(if self.ataraxia_held() {
                UpgradeWait::AtaraxiaHeld
            } else {
                UpgradeWait::HeldPaused
            }),
            Screen::Idle if self.i_owe() && paused && self.i_hold() => {
                Some(UpgradeWait::ResumeToChoose)
            }
            _ => None,
        };
        if s.upgrade.is_some() {
            s.upgrade_counts = self.round_waited(now) >= UPGRADE_COUNT_AFTER;
        }
        s.count = self.count.as_ref().and_then(|c| {
            c.number(now).map(|n| CountView {
                kind: CountKind::Resume,
                number: n,
                phase_pm: permille(c.phase(now)),
            })
        });
        if self.respawn.is_game_over() {
            return s;
        }
        let gt = self.game_time;
        if s.count.is_none()
            && matches!(self.down, Ship::Down { .. })
            && let Some(left) = self.my_respawn_left()
            && let Some(n) = alert_number(left)
        {
            let into = 1000 - u16::try_from(left.as_millis() % 1000).unwrap_or(0);
            s.count = Some(CountView {
                kind: CountKind::Respawn,
                number: n,
                phase_pm: into % 1000,
            });
        }
        if matches!(self.down, Ship::Down { .. }) {
            let left = self
                .respawn
                .remaining(self.me, gt)
                .or_else(|| self.solo_respawn.map(|at| at.saturating_sub(gt)))
                .unwrap_or(self.cfg.respawn);
            s.me_down = Some(ceil_secs(left));
        }
        s.partner_down = self
            .respawn
            .downed_peers()
            .minus(one(self.me))
            .iter()
            .next()
            .and_then(|p| self.respawn.remaining(p, gt))
            .map(ceil_secs);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: PeerId = PeerId::HOST;
    const CLIENT: PeerId = PeerId(1);

    fn cfg() -> RulesSettings {
        RulesSettings {
            respawn: Duration::from_secs(10),
            ..RulesSettings::default()
        }
    }

    fn ctx(is_host: bool) -> RunCtx {
        RunCtx {
            key: (7, Some(1.0)),
            coop_run: true,
            linked: true,
            gate_running: true,
            me: if is_host { HOST } else { CLIENT },
            is_host,
            partners: one(if is_host { CLIENT } else { HOST }),
        }
    }

    /// The run's world, as each game observes it.
    fn running() -> Observed {
        Observed {
            gameplay: true,
            levelup: Some(1.0),
            ..Observed::default()
        }
    }

    fn paused() -> Observed {
        Observed {
            paused: true,
            ..running()
        }
    }

    /// A host and a client wired back to back.
    struct Pair {
        host: Rules,
        client: Rules,
        t: Instant,
        /// Messages were dropped on purpose (a cut): adopting may be right.
        lossy: bool,
    }

    /// On a lane that loses nothing, nothing is ever adopted from state.
    impl Drop for Pair {
        fn drop(&mut self) {
            if !self.lossy && !std::thread::panicking() {
                assert_eq!(
                    (self.host.adopted(), self.client.adopted()),
                    (0, 0),
                    "adopted from state with no message lost"
                );
            }
        }
    }

    impl Pair {
        fn new(cfg: RulesSettings) -> Self {
            let t = Instant::now();
            let mut host = Rules::new(cfg.clone());
            let mut client = Rules::new(cfg);
            host.sync_run(&ctx(true));
            client.sync_run(&ctx(false));
            assert!(host.active() && client.active());
            Self {
                host,
                client,
                t,
                lossy: false,
            }
        }

        /// Deliver every message until both are quiet.
        fn deliver(&mut self) {
            for _ in 0..10 {
                let h = self.host.take_outs();
                let c = self.client.take_outs();
                if h.is_empty() && c.is_empty() {
                    return;
                }
                for o in h {
                    if let Out::Broadcast(m) | Out::Send(_, m) = o {
                        self.client.on_message(HOST, &m, self.t);
                    }
                }
                for o in c {
                    if let Out::Broadcast(m) | Out::Send(_, m) = o {
                        self.host.on_message(CLIENT, &m, self.t);
                    }
                }
            }
            unreachable!("messages never settled");
        }

        /// One frame on both sides, messages delivered.
        fn frame(&mut self, h: Observed, c: Observed) -> (Vec<Action>, Vec<Action>) {
            let a = self.host.frame(&h, self.t);
            let b = self.client.frame(&c, self.t);
            self.deliver();
            (a, b)
        }

        fn later(&mut self, d: Duration) {
            self.t += d;
        }

        /// What both sent is lost (the link is cut).
        fn drop_outs(&mut self) {
            self.lossy = true;
            self.host.take_outs();
            self.client.take_outs();
        }

        /// The link timed out on both sides (an interrupted link).
        fn timed_out(&mut self) {
            self.drop_outs();
            self.host.peer_left(CLIENT, true, self.t);
            self.client.peer_left(HOST, true, self.t);
            self.drop_outs();
        }

        /// The link is back on both sides.
        fn back(&mut self) {
            self.host.peer_joined(CLIENT, self.t);
            self.client.peer_joined(HOST, self.t);
            self.deliver();
        }

        /// The resume count runs out: past its end, one frame.
        fn count_out(&mut self, h: Observed, c: Observed) -> (Vec<Action>, Vec<Action>) {
            self.later(RESUME_COUNT + Duration::from_millis(50));
            self.frame(h, c)
        }
    }

    #[test]
    fn solo_runs_do_nothing() {
        let t = Instant::now();
        let mut r = Rules::new(cfg());
        r.sync_run(&RunCtx {
            coop_run: false,
            linked: false,
            ..ctx(true)
        });
        assert!(!r.active());
        assert!(r.frame(&paused(), t).is_empty());
        assert_eq!(r.unpause_request(false, t), UnpauseCall::Allow);
        assert_eq!(r.pause_request(true), PauseCall::Allow);
        assert_eq!(r.death(t), Death::Die);
        assert!(!r.upgrade_leaving(t));
        assert_eq!(r.facts(t, true), SessionFacts::default());
        assert!(r.take_outs().is_empty());
        // Not before the start gate lets the run go.
        r.sync_run(&RunCtx {
            gate_running: false,
            ..ctx(true)
        });
        assert!(!r.active());
        assert_eq!(
            r.unpause_request(true, t),
            UnpauseCall::Block,
            "the gate holds"
        );
    }

    #[test]
    fn a_pause_holds_both_until_both_resume() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        // The host's player pauses: everyone is on their pause menu.
        p.frame(paused(), running());
        assert_eq!(p.host.facts(p.t, true).pause, Some(SharedPause::BothPaused));
        let (_, c) = p.frame(paused(), running());
        assert_eq!(c, vec![Action::PressPause], "the client follows");
        let (_, c) = p.frame(paused(), paused());
        assert!(c.is_empty(), "its own pause: no event back");
        assert_eq!(
            p.client.facts(p.t, true).pause,
            Some(SharedPause::BothPaused)
        );
        // The client resumes first: out of the round, held on the pause
        // screen.
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert_eq!(
            p.client.facts(p.t, true).pause,
            Some(SharedPause::WaitingForPartner)
        );
        assert_eq!(
            p.host.facts(p.t, true).pause,
            Some(SharedPause::PartnerReady)
        );
        assert_eq!(
            p.client.unpause_request(false, p.t),
            UnpauseCall::Block,
            "still waiting"
        );
        let (h, c) = p.frame(paused(), paused());
        assert!(h.is_empty() && c.is_empty(), "both stay paused");
        // The host resumes last: both count, then both are resumed.
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert!(!p.host.pause.is_paused() && !p.client.pause.is_paused());
        assert!(p.host.counting() && p.client.counting());
        let (h, c) = p.frame(paused(), paused());
        assert!(h.is_empty() && c.is_empty(), "counting: {h:?} {c:?}");
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty());
        assert_eq!(p.client.facts(p.t, false).pause, None);
        assert_eq!(p.host.facts(p.t, false).pause, None);
    }

    #[test]
    fn client_pause_goes_through_the_host() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(running(), paused());
        assert_eq!(p.host.pause.paused_by(), Some(CLIENT));
        assert_eq!(
            p.client.facts(p.t, true).pause,
            Some(SharedPause::BothPaused)
        );
        let (h, _) = p.frame(running(), paused());
        assert_eq!(h, vec![Action::PressPause]);
        // The host (who did not pause) resumes first: held.
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert!(p.host.pause.is_paused());
        assert_eq!(
            p.client.facts(p.t, true).pause,
            Some(SharedPause::PartnerReady)
        );
        // The client resumes last: both count, then both are resumed.
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.frame(paused(), paused());
        assert!(!p.host.pause.is_paused(), "released");
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    #[test]
    fn both_pressing_pause_at_once_is_one_pause() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        // Both pause in the same frame (neither has heard of the other).
        p.frame(paused(), paused());
        assert_eq!(p.host.pause.holders().len(), 2);
        assert_eq!(p.client.pause.holders().len(), 2);
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.frame(paused(), paused());
        assert!(!p.host.pause.is_paused() && !p.client.pause.is_paused());
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    #[test]
    fn a_partner_leaving_on_its_menu_is_out_of_the_pause() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        // The client, still on its menu, leaves: the host alone holds it.
        p.host.peer_left(CLIENT, false, p.t);
        assert!(p.host.pause.is_paused(), "the host is still on its menu");
        assert_eq!(p.host.facts(p.t, true).pause, None, "alone: no co-op text");
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Allow);
        assert!(!p.host.pause.is_paused());
    }

    #[test]
    fn a_pause_is_never_given_up_by_time() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        p.later(Duration::from_secs(3600));
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        let (h, c) = p.frame(paused(), paused());
        assert!(h.is_empty() && c.is_empty());
        assert!(p.host.pause.is_paused());
    }

    /// The client resumes last in a host pause; its messages are returned,
    /// not delivered.
    fn client_resumes_last(p: &mut Pair) -> Vec<Out> {
        p.frame(running(), running());
        p.frame(paused(), running());
        let (_, c) = p.frame(paused(), running());
        assert_eq!(c, vec![Action::PressPause], "the client follows");
        // Seconds on the pause menus (past the press spacing).
        for _ in 0..PRESS_EVERY * 2 {
            p.frame(paused(), paused());
        }
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        // Blocked: the round's end counts down first.
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        let in_flight = p.client.take_outs();
        assert!(p.client.close_sent && !in_flight.is_empty());
        in_flight
    }

    fn to_host(p: &mut Pair, outs: Vec<Out>) {
        for o in outs {
            if let Out::Broadcast(m) | Out::Send(_, m) = o {
                p.host.on_message(CLIENT, &m, p.t);
            }
        }
    }

    fn to_client(p: &mut Pair, outs: Vec<Out>) {
        for o in outs {
            if let Out::Broadcast(m) | Out::Send(_, m) = o {
                p.client.on_message(HOST, &m, p.t);
            }
        }
    }

    /// The joining game resuming last is not paused again while its Resume
    /// waits for the host's relay.
    #[test]
    fn a_client_resuming_last_is_not_paused_again_before_the_relay() {
        let mut p = Pair::new(cfg());
        let in_flight = client_resumes_last(&mut p);
        for _ in 0..5 {
            let c = p.client.frame(&paused(), p.t);
            assert!(c.is_empty(), "nothing before the relay: {c:?}");
        }
        // The relay: both count, then both are resumed.
        to_host(&mut p, in_flight);
        p.deliver();
        assert!(!p.client.close_sent);
        assert!(!p.client.pause.is_paused() && !p.host.pause.is_paused());
        let c = p.client.frame(&paused(), p.t);
        assert!(c.is_empty(), "{c:?}");
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    #[test]
    fn the_hosts_player_pausing_again_while_the_clients_resume_is_in_flight_pauses_the_client() {
        let mut p = Pair::new(cfg());
        let in_flight = client_resumes_last(&mut p);
        assert!(p.client.frame(&paused(), p.t).is_empty());
        // The host takes the client's Resume, then Esc in the count: both reach it in order.
        to_host(&mut p, in_flight);
        let h = p.host.frame(&paused(), p.t);
        assert!(h.is_empty() && p.host.counting(), "{h:?}");
        p.host.count_hold_on(p.t);
        assert!(p.host.pause.holds(HOST) && !p.host.counting());
        p.deliver();
        // A new round with everyone in it, the client's count stopped.
        assert!(p.client.pause.holds(HOST) && p.client.pause.holds(CLIENT));
        assert!(!p.client.close_sent && !p.client.counting());
        let c = p.client.frame(&paused(), p.t);
        assert!(c.is_empty(), "the new round holds the client: {c:?}");
        let (h, c) = p.count_out(paused(), paused());
        assert!(h.is_empty() && c.is_empty(), "held: {h:?} {c:?}");
        // The host's pause arrives with the client's Resume unanswered: held.
        let mut q = Pair::new(cfg());
        let in_flight = client_resumes_last(&mut q);
        q.client.on_message(HOST, &Message::Pause { by: HOST }, q.t);
        assert!(q.client.pause.holds(HOST) && q.client.close_sent);
        let c = q.client.frame(&paused(), q.t);
        assert!(c.is_empty(), "held: {c:?}");
        assert!(q.client.pause.is_paused());
        to_host(&mut q, in_flight);
    }

    /// `close_sent` from one round never leaves a later round open.
    #[test]
    fn a_resume_in_flight_never_leaves_a_later_pause_open() {
        // The client's own pause again before the relay: a new request, held.
        let mut p = Pair::new(cfg());
        let in_flight = client_resumes_last(&mut p);
        p.client.frame(&running(), p.t);
        let c = p.client.frame(&paused(), p.t);
        assert!(
            !p.client.close_sent && p.client.pause_request.is_some(),
            "{c:?}"
        );
        to_host(&mut p, in_flight);
        p.deliver();
        assert!(p.host.pause.holds(CLIENT) && p.client.pause.holds(CLIENT));
        assert!(!p.client.close_sent);
        let (h, c) = p.frame(running(), paused());
        assert_eq!(
            h,
            vec![Action::PressPause],
            "the host follows the client's new pause"
        );
        assert!(!c.contains(&Action::PressUnpause), "{c:?}");

        // The link lost with the Resume unanswered: decided here.
        let mut p = Pair::new(cfg());
        let _lost = client_resumes_last(&mut p);
        p.timed_out();
        assert!(!p.client.close_sent && !p.client.pause.holds(CLIENT));
        p.back();
        for _ in 0..PRESS_EVERY * 2 {
            p.frame(running(), running());
        }
        p.frame(paused(), running());
        let (_, c) = p.frame(paused(), running());
        assert_eq!(
            c,
            vec![Action::PressPause],
            "a later round holds the client"
        );

        // The run's end: a fresh state.
        let mut p = Pair::new(cfg());
        let _ = client_resumes_last(&mut p);
        p.client.end_run();
        assert!(!p.client.close_sent);
    }

    /// A pause during an upgrade round is turned down; a round opening
    /// during a pause follows each player's pause menu.
    #[test]
    fn pause_and_upgrade_rounds_compose() {
        // A pause during an upgrade round: turned down by the host.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.client
            .on_message(CLIENT, &Message::Pause { by: CLIENT }, p.t);
        p.host
            .on_message(CLIENT, &Message::Pause { by: CLIENT }, p.t);
        assert!(!p.host.pause.is_paused(), "turned down during the round");
        // An upgrade round opening during a pause: the client opened its
        // screen before it heard of the host's pause.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.frame(&paused(), p.t);
        p.client.upgrade_opened();
        p.deliver();
        assert!(p.host.pause.is_paused() && p.host.upgrade.active().is_some());
        assert_eq!(
            p.host.facts(p.t, true).upgrade,
            Some(UpgradeWait::ResumeToChoose)
        );
        // The host resumes from its pause menu: the client is still in the
        // pause, so the host's game stays paused, then goes to choose.
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        let mut acts = Vec::new();
        for _ in 0..3 {
            let (h, _) = p.frame(paused(), running());
            acts.extend(h);
        }
        assert!(acts.contains(&Action::PressUnpause), "{acts:?}");
        let (h, _) = p.frame(running(), running());
        assert_eq!(h, vec![Action::GotoUpgrade]);
        p.host.upgrade_opened();
        // Both choose; the round is released.
        assert!(p.client.upgrade_leaving(p.t) || p.client.upgrade.active().is_some());
        p.deliver();
        let _ = p.host.upgrade_leaving(p.t);
        p.deliver();
        assert!(p.host.upgrade.active().is_none());
        // The client, still in the pause round, is shown its pause menu and
        // resumes; then everyone runs.
        let mut client_acts = Vec::new();
        let mut hs = running();
        for _ in 0..40 {
            let (h, c) = p.frame(hs, running());
            if h.contains(&Action::PressPause) {
                hs = paused();
            }
            if c.contains(&Action::ReleaseUpgrade) {
                assert!(!p.client.upgrade_leaving(p.t), "the game leaves its screen");
            }
            client_acts.extend(c);
        }
        assert!(client_acts.contains(&Action::PressPause), "{client_acts:?}");
        assert_eq!(
            p.client.facts(p.t, true).pause,
            Some(SharedPause::PartnerReady)
        );
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.frame(paused(), paused());
        assert!(!p.host.pause.is_paused());
        assert!(
            p.host.counting() && p.client.counting(),
            "the last hold's end"
        );
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    #[test]
    fn focus_loss_can_be_ignored() {
        let mut p = Pair::new(RulesSettings {
            pause_on_focus_loss: false,
            ..cfg()
        });
        assert_eq!(p.host.pause_request(true), PauseCall::Skip);
        assert_eq!(p.host.pause_request(false), PauseCall::Allow);
        let mut p2 = Pair::new(cfg());
        assert_eq!(p2.host.pause_request(true), PauseCall::Allow);
        p.frame(running(), running());
        p2.frame(running(), running());
    }

    #[test]
    fn the_mods_own_pauses_are_not_a_players() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.note_own(true);
        p.frame(paused(), running());
        assert_eq!(p.host.pause.paused_by(), None);
        assert!(p.client.frame(&running(), p.t).is_empty());
    }

    /// Draft's opening: the partner's round and the gate's ask in one frame give
    /// the client its own screen in either order. Alone, a partner's round fades in.
    #[test]
    fn the_start_gates_own_screen_does_not_fade_in_whatever_the_order() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        let (_, c) = p.frame(running(), running());
        assert_eq!(c, vec![Action::GotoUpgrade]);
        p.client.start_screen_asked();
        assert!(
            !p.client.upgrade_opened(),
            "the client's own opening screen: no fade"
        );
        let mut q = Pair::new(cfg());
        q.frame(running(), running());
        q.host.upgrade_opened();
        q.deliver();
        let (_, c) = q.frame(running(), running());
        assert_eq!(c, vec![Action::GotoUpgrade]);
        assert!(
            q.client.upgrade_opened(),
            "the partner's round alone fades in"
        );
        // An ask whose screen never came does not outlive its frames.
        let mut r = Pair::new(cfg());
        r.frame(running(), running());
        r.client.start_screen_asked();
        run_frames(
            &mut r,
            usize::try_from(START_SCREEN_FRAMES).unwrap_or(20) + 2,
        );
        r.host.upgrade_opened();
        r.deliver();
        let (_, c) = r.frame(running(), running());
        assert_eq!(c, vec![Action::GotoUpgrade]);
        assert!(r.client.upgrade_opened(), "a later round's screen fades in");
    }

    /// Host opens; the client is forced in; host finishes first and waits.
    #[test]
    fn shared_upgrade_screen_waits_for_both() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        assert_eq!(p.client.upgrade.active(), Some(1));
        let (_, c) = p.frame(running(), running());
        assert_eq!(c, vec![Action::GotoUpgrade]);
        p.client.upgrade_opened();
        assert!(
            p.client.take_outs().is_empty(),
            "an expected open is not re-sent"
        );
        // The host chooses first: held.
        assert!(p.host.upgrade_leaving(p.t));
        p.deliver();
        assert_eq!(
            p.host.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerChoosing)
        );
        assert_eq!(
            p.client.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerReady)
        );
        let (h, _) = p.frame(running(), running());
        assert!(h.is_empty(), "still held");
        p.later(UPGRADE_COUNT_AFTER);
        // The client chooses last: both count on their cards, then leave.
        assert!(p.client.upgrade_leaving(p.t), "held for the count");
        p.deliver();
        assert_eq!(p.host.upgrade.last_released(), Some(1));
        assert!(p.host.counting() && p.client.counting());
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty(), "counting: {h:?} {c:?}");
        let (h, c) = p.count_out(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        assert!(
            !p.host.upgrade_leaving(p.t),
            "the host's release goes through"
        );
        assert!(!p.client.upgrade_leaving(p.t));
        assert_eq!(p.host.facts(p.t, false).upgrade, None);
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty());
    }

    /// The client opens and chooses before the host's numbering arrives.
    #[test]
    fn client_open_and_fast_choice_merge_on_the_host() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(
            p.client.upgrade_leaving(p.t),
            "held before the host numbered it"
        );
        assert_eq!(
            p.client.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerChoosing)
        );
        p.deliver();
        assert_eq!(p.host.upgrade.active(), Some(1));
        assert!(!p.host.upgrade.is_waiting_on(CLIENT), "its done counted");
        let (h, c) = p.frame(running(), running());
        assert_eq!(h, vec![Action::GotoUpgrade]);
        assert!(c.is_empty(), "still held");
        p.host.upgrade_opened();
        assert!(p.host.upgrade_leaving(p.t), "the last choice: the count");
        p.deliver();
        let (h, c) = p.count_out(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
    }

    #[test]
    fn nothing_to_choose_holds_on_the_pause_screen() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        let none = Observed {
            levelup: Some(0.0),
            ..running()
        };
        let (_, c) = p.frame(running(), none);
        assert!(c.is_empty(), "reported done this frame");
        assert!(!p.host.upgrade.is_waiting_on(CLIENT));
        let (_, c) = p.frame(running(), none);
        assert_eq!(c, vec![Action::PressPause]);
        let held = Observed {
            paused: true,
            ..none
        };
        p.frame(running(), held);
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::HeldPaused)
        );
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        assert_eq!(
            p.host.pause.paused_by(),
            None,
            "the hold is not a shared pause"
        );
        // However long the wait, Resume is blocked until the host picks.
        p.later(Duration::from_secs(61));
        p.frame(running(), held);
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::HeldPaused)
        );
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert!(
            p.host.upgrade.is_waiting_on(HOST),
            "nobody marks the host done"
        );
        p.host.upgrade_opened();
        assert!(p.host.upgrade_leaving(p.t), "the last pick: the count");
        assert_eq!(p.host.upgrade.active(), None);
    }

    /// The held card stays after 61 s: the round waits for the partner's pick.
    #[test]
    fn the_held_card_waits_however_long() {
        let mut p = Pair::new(cfg());
        p.host.upgrade_opened();
        p.deliver();
        assert!(p.host.upgrade_leaving(p.t));
        p.later(Duration::from_secs(61));
        assert_eq!(
            p.host.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerChoosing)
        );
        let (h, _) = p.frame(running(), running());
        assert!(h.is_empty(), "the card stays: {h:?}");
        assert!(p.host.upgrade.is_waiting_on(CLIENT));
        assert!(p.host.held_on_card());
    }

    /// The host takes a pick only from the player who made it.
    #[test]
    fn nobody_marks_the_partner_done() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        let r = p.host.upgrade.active().unwrap();
        let msg = Message::UpgradeDone {
            peer: HOST,
            round: r,
            count: false,
        };
        assert!(p.host.on_message(CLIENT, &msg, p.t));
        assert!(p.host.upgrade.is_waiting_on(HOST), "ignored");
        assert_eq!(p.host.upgrade.active(), Some(r));
    }

    /// On the held card the pause key moves the game to its pause screen,
    /// the round holding the game there (the partner idling, or only its pause menu).
    #[test]
    fn held_on_the_card_the_pause_key_waits_on_the_pause_screen() {
        // The partner idles on its upgrade screen.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        let (_, c) = p.frame(running(), running());
        assert_eq!(c, vec![Action::GotoUpgrade]);
        p.client.upgrade_opened();
        // Still choosing: the key is not polled.
        assert!(!p.client.held_on_card());
        p.client.pause_key_on_card();
        assert_eq!(p.client.pause_request(false), PauseCall::Skip);
        assert!(!p.client.to_pause_screen);
        let (_, c) = p.frame(running(), running());
        assert!(c.is_empty(), "{c:?}");
        assert_eq!(p.client.screen, Screen::Choosing);
        assert!(p.client.upgrade_leaving(p.t), "held for the host");
        p.later(Duration::from_secs(600));
        p.frame(running(), running());
        assert_eq!(p.client.pause_request(true), PauseCall::Skip, "focus loss");
        assert!(!p.client.to_pause_screen);
        // The key: the card goes, the round holds the client paused.
        assert!(p.client.held_on_card());
        p.client.pause_key_on_card();
        assert!(!p.client.held_on_card(), "polled no more");
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty(), "{h:?}");
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        assert!(!p.client.upgrade_leaving(p.t), "the card goes");
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty(), "{h:?}");
        assert_eq!(c, vec![Action::PressPause]);
        let (h, c) = p.frame(running(), paused());
        assert!(h.is_empty() && c.is_empty(), "{h:?} {c:?}");
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::HeldPaused),
            "Waiting for Sam... on the pause screen, Leave Co-op Run below"
        );
        // The client's only: the host is neither paused nor told.
        assert_eq!(p.host.pause.paused_by(), None);
        assert_eq!(p.client.pause.paused_by(), None);
        assert_eq!(p.host.screen, Screen::Choosing);
        // Resume and Esc are held there, however long.
        p.later(Duration::from_secs(600));
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        // The host chooses last: both count, then both go.
        assert!(p.host.upgrade_leaving(p.t));
        p.deliver();
        let (h, c) = p.frame(running(), paused());
        assert!(h.is_empty() && c.is_empty(), "counting: {h:?} {c:?}");
        let (h, c) = p.count_out(running(), paused());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    /// The host chose last and holds on in the count: both cards go to the pause screens.
    #[test]
    fn esc_in_the_count_on_the_card_takes_both_to_the_pause_screen() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.client.upgrade_leaving(p.t));
        p.deliver();
        p.later(UPGRADE_COUNT_AFTER);
        // The host chooses last and holds on in the count before the client's card goes.
        assert!(p.host.upgrade_leaving(p.t), "held for the count");
        p.deliver();
        assert!(p.host.counting_on_card() && p.client.counting());
        p.host.count_hold_on(p.t);
        assert!(!p.host.counting() && p.host.pause.holds(HOST));
        assert_eq!(p.host.frame(&running(), p.t), vec![Action::ReleaseUpgrade]);
        assert!(!p.host.upgrade_leaving(p.t));
        assert_eq!(p.host.frame(&running(), p.t), vec![Action::PressPause]);
        p.host.frame(&paused(), p.t);
        p.deliver();
        assert!(
            !p.client.counting(),
            "the client's count stopped at the host's pause"
        );
        let (_, c) = p.frame(paused(), running());
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        assert!(!p.client.upgrade_leaving(p.t));
        let (_, c) = p.frame(paused(), running());
        assert_eq!(c, vec![Action::PressPause]);
        p.frame(paused(), paused());
        assert_eq!(p.client.pause.paused_by(), Some(PeerId::HOST));
        assert_eq!(
            p.client.facts(p.t, true).pause,
            Some(SharedPause::BothPaused),
            "in the shared pause like everyone"
        );
    }

    #[test]
    fn pause_is_turned_down_during_a_round() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        // The client's player had pressed pause just before the open arrived.
        p.frame(running(), paused());
        assert_eq!(p.host.pause.paused_by(), None);
        // The client must choose: its own pause is undone first.
        let (_, c) = p.frame(running(), paused());
        assert_eq!(c, vec![Action::PressUnpause]);
        assert_eq!(
            p.host.pause_request(false),
            PauseCall::Skip,
            "on the screen"
        );
    }

    #[test]
    fn a_partner_leaving_releases_everything() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(running(), paused()); // the client pauses
        assert!(p.host.pause.is_paused());
        p.host.upgrade_opened();
        p.deliver();
        assert!(p.host.upgrade_leaving(p.t), "held for the client");
        p.host.peer_left(CLIENT, true, p.t);
        assert!(
            !p.host.pause.holds(CLIENT),
            "the leaver is out of the pause"
        );
        assert!(
            p.host.pause.holds(HOST),
            "the host is on its own pause menu"
        );
        p.host.local_unpause();
        assert!(!p.host.pause.is_paused());
        let (h, _) = p.frame(paused(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert!(!p.host.upgrade_leaving(p.t), "the host's release");
        // Alone, a new open is not held.
        p.host.upgrade_opened();
        assert!(!p.host.upgrade_leaving(p.t));
        // Back: shared again.
        p.host.peer_joined(CLIENT, p.t);
        p.host.upgrade_opened();
        assert!(p.host.upgrade_leaving(p.t));
    }

    /// Both players resume inside a cut: neither game stays paused.
    #[test]
    fn a_resume_inside_a_cut_never_leaves_a_game_paused() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running()); // the host's player pauses
        p.frame(paused(), paused()); // the client follows
        assert!(p.client.pause.is_paused());
        // The cut: both press Resume, nothing gets through.
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.drop_outs();
        let (h, c) = p.frame(paused(), paused());
        assert!(h.is_empty() && c.is_empty(), "both still held, waiting");
        p.timed_out();
        assert!(!p.host.pause.is_paused(), "the host plays on alone");
        assert!(
            !p.client.pause.is_paused(),
            "so does the client: its Resume is decided here"
        );
        let (h, c) = p.frame(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
        p.frame(running(), running());
        // Back: nobody holds anything; both play.
        p.back();
        assert!(!p.host.pause.is_paused() && !p.client.pause.is_paused());
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty());
        assert_eq!(p.client.facts(p.t, false).pause, None);
    }

    /// The same from the other side: the client opened the pause, the
    /// host's Resume and the client's own are both inside the cut.
    #[test]
    fn a_client_pause_resumed_inside_a_cut_never_leaves_a_game_paused() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(running(), paused()); // the client's player pauses
        p.frame(paused(), paused());
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.timed_out();
        assert!(!p.host.pause.is_paused() && !p.client.pause.is_paused());
        let (h, c) = p.frame(paused(), paused());
        assert_eq!(
            (h, c),
            (vec![Action::PressUnpause], vec![Action::PressUnpause])
        );
        p.frame(running(), running());
        p.back();
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty());
        assert!(!p.host.pause.is_paused() && !p.client.pause.is_paused());
    }

    /// A player still on their own pause menu at the return pauses both
    /// again (the other game follows), and its Resume releases both.
    #[test]
    fn still_on_the_menu_at_the_return_pauses_both_again() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        // The client resumes before the cut (heard); the host stays on its menu.
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        p.timed_out();
        assert!(p.host.pause.holds(HOST), "the host is on its menu");
        let (_, c) = p.frame(paused(), paused());
        assert_eq!(c, vec![Action::PressUnpause], "the client plays on alone");
        p.frame(paused(), running());
        p.back();
        assert!(
            p.client.pause.is_paused(),
            "the host's menu pauses the client again"
        );
        let (_, c) = p.frame(paused(), running());
        assert_eq!(c, vec![Action::PressPause]);
        p.frame(paused(), paused());
        // The client's player resumes (held), then the host's: both run.
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
        assert!(!p.host.pause.is_paused() && !p.client.pause.is_paused());
    }

    /// A cut during an upgrade round: each side ends it alone.
    #[test]
    fn an_upgrade_round_across_a_cut_holds_nobody() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        let (_, c) = p.frame(running(), running());
        assert_eq!(c, vec![Action::GotoUpgrade]);
        p.client.upgrade_opened();
        // The client chooses inside the cut: held, its done lost.
        assert!(p.client.upgrade_leaving(p.t));
        p.timed_out();
        let (_, c) = p.frame(running(), running());
        assert_eq!(
            c,
            vec![Action::ReleaseUpgrade],
            "alone: nothing to wait for"
        );
        assert!(!p.client.upgrade_leaving(p.t));
        p.back();
        // The host was still choosing: it leaves its screen unheld.
        assert!(!p.host.upgrade_leaving(p.t));
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty());
        assert!(p.host.upgrade.active().is_none() && p.client.upgrade.active().is_none());
    }

    /// A round the host opened alone reaches the returning client.
    #[test]
    fn the_hosts_round_reaches_the_returning_game() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.client.upgrade_leaving(p.t), "held for the host");
        p.timed_out();
        // Alone, the host ends round 1 and opens another (the client's game
        // has run no frame since: it still sits held).
        assert!(!p.host.upgrade_leaving(p.t));
        assert!(p.host.frame(&running(), p.t).is_empty());
        p.host.upgrade_opened();
        p.back();
        assert_eq!(
            p.client.upgrade.active(),
            p.host.upgrade.active(),
            "the host's round arrived"
        );
        assert!(p.client.upgrade.active().is_some());
        let (_, c) = p.frame(running(), running());
        assert!(c.is_empty(), "held: the host is choosing");
        assert!(
            !p.host.upgrade.is_waiting_on(CLIENT),
            "the held client answered"
        );
        p.later(UPGRADE_COUNT_AFTER);
        assert!(p.host.upgrade_leaving(p.t), "held for the count only");
        p.deliver();
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty(), "counting: {h:?} {c:?}");
        let (h, c) = p.count_out(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
    }

    /// Rounds the client ended alone during the cut do not make the host's
    /// next round stale for it at the return.
    #[test]
    fn rounds_ended_alone_do_not_make_the_hosts_stale() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.timed_out();
        // Alone, the client takes two levels (rounds 1 and 2), the host one.
        for _ in 0..2 {
            p.client.upgrade_opened();
            assert!(!p.client.upgrade_leaving(p.t));
            p.frame(running(), running());
        }
        p.host.upgrade_opened();
        p.back();
        assert_eq!(p.host.upgrade.active(), Some(1));
        assert_eq!(
            p.client.upgrade.active(),
            Some(1),
            "the host's round 1 is not stale here"
        );
        let (_, c) = p.frame(running(), running());
        assert_eq!(c, vec![Action::GotoUpgrade], "the client chooses for it");
    }

    /// The client on its upgrade screen at the return proposes the round;
    /// the host joins it (shared again from there).
    #[test]
    fn choosing_at_the_return_shares_the_round_again() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.timed_out();
        p.client.upgrade_opened();
        p.back();
        assert!(
            p.host.upgrade.active().is_some(),
            "the host numbered the client's round"
        );
        let (h, _) = p.frame(running(), running());
        assert_eq!(h, vec![Action::GotoUpgrade], "the host chooses too");
        p.host.upgrade_opened();
        assert!(
            p.client.upgrade_leaving(p.t),
            "the client waits for the host"
        );
        p.deliver();
        assert!(p.host.upgrade_leaving(p.t), "the last choice: the count");
        p.deliver();
        let (h, c) = p.count_out(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
    }

    /// A lost partner is gone for the run only when told so.
    #[test]
    fn a_lost_partner_is_waited_for_until_told_it_left() {
        let t = Instant::now();
        let mut h = Rules::new(cfg());
        h.sync_run(&ctx(true));
        h.peer_left(CLIENT, true, t);
        h.sync_run(&ctx(true));
        assert!(!h.separated(), "an hour: still the partner's run");
        h.peer_left(CLIENT, false, t + Duration::from_secs(3601));
        assert!(h.separated(), "told while it was away");
        h.peer_joined(CLIENT, t + Duration::from_secs(3700));
        assert!(h.separated(), "back after: still apart");
        h.upgrade_opened();
        assert!(!h.upgrade_leaving(t), "nothing waits for it");
    }

    /// A separated game takes none of the former partner's rounds.
    #[test]
    fn a_separated_game_ignores_the_former_partners_rounds() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        let t = p.t;
        p.client.peer_left(HOST, true, t);
        p.client.peer_left(HOST, false, t);
        assert!(p.client.separated());
        for msg in [
            Message::Pause { by: HOST },
            Message::UpgradeOpen { round: 1 },
            Message::GameOver,
        ] {
            assert!(p.client.on_message(HOST, &msg, t), "still consumed");
        }
        let (_, c) = p.frame(running(), running());
        assert!(c.is_empty(), "no pause, no upgrade hold: {c:?}");
        assert!(p.client.take_outs().is_empty(), "nothing sent back");
    }

    /// A long freeze by the wait becomes the shared pause at the return.
    #[test]
    fn a_long_freeze_becomes_the_shared_pause_for_both() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        p.timed_out();
        // The wait pauses the joining game (the mod's own pause).
        p.client.note_own(true);
        p.frame(running(), paused());
        assert!(p.host.take_outs().is_empty() && p.client.take_outs().is_empty());
        p.later(Duration::from_secs(12));
        p.back();
        p.client.adopt_pause(p.t);
        p.deliver();
        let (h, _) = p.frame(running(), paused());
        assert_eq!(h, vec![Action::PressPause], "the host pauses with it");
        let (_, _) = p.frame(paused(), paused());
        assert_eq!(p.host.facts(p.t, true).pause, Some(SharedPause::BothPaused));
        // The host resumes: held until the joining player does.
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause], "both resumed: play on");
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    /// The same round after a long cut, the joining player resuming last
    /// with its Resume in flight: not paused again before the relay.
    #[test]
    fn the_shared_pause_after_a_long_cut_ends_without_a_second_freeze() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        p.timed_out();
        p.client.note_own(true);
        p.frame(running(), paused());
        p.later(Duration::from_secs(12));
        p.back();
        p.client.adopt_pause(p.t);
        p.deliver();
        for _ in 0..PRESS_EVERY * 2 {
            p.frame(paused(), paused());
        }
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        let in_flight = p.client.take_outs();
        for _ in 0..5 {
            let c = p.client.frame(&paused(), p.t);
            assert!(c.is_empty(), "nothing before the relay: {c:?}");
        }
        to_host(&mut p, in_flight);
        p.deliver();
        assert!(!p.client.pause.is_paused() && !p.client.close_sent);
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    /// Both players resume at once: both resume, neither is paused again.
    #[test]
    fn both_resuming_at_once_resume_both_without_a_second_freeze() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), running());
        for _ in 0..PRESS_EVERY * 2 {
            p.frame(paused(), paused());
        }
        // The client's Resume first (the host still in the round: it waits).
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        let client_outs = p.client.take_outs();
        // The host's at the same moment: the client's has not arrived, so
        // held.
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        let host_outs = p.host.take_outs();
        // The host's Resume reaches the client: its close is the only one
        // left.
        for o in host_outs {
            if let Out::Broadcast(m) | Out::Send(_, m) = o {
                p.client.on_message(HOST, &m, p.t);
            }
        }
        assert!(p.client.close_sent && p.client.pause.holders() == one(CLIENT));
        let c = p.client.frame(&paused(), p.t);
        assert!(c.is_empty(), "the client waits for the relay: {c:?}");
        // The client's reaches the host: both count, then both resume.
        to_host(&mut p, client_outs);
        let h = p.host.frame(&paused(), p.t);
        assert!(h.is_empty(), "counting: {h:?}");
        p.deliver();
        assert!(p.host.counting() && p.client.counting());
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty(), "{h:?} {c:?}");
        assert!(!p.client.pause.is_paused() && !p.host.pause.is_paused());
    }

    /// A short freeze is not adopted: nothing to share at the return.
    #[test]
    fn adopting_needs_an_active_run_and_no_pause_of_ones_own() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        // Paused by the player already: adopting adds nothing.
        p.frame(running(), paused());
        p.deliver();
        let before = p.client.facts(p.t, true);
        p.client.adopt_pause(p.t);
        assert!(p.client.take_outs().is_empty(), "{before:?}");
    }

    fn run_frames(p: &mut Pair, n: usize) {
        for _ in 0..n {
            p.frame(running(), running());
        }
    }

    #[test]
    fn client_goes_down_and_flies_back_in() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        assert_eq!(p.client.powers_action(), PowersAct::HoldDown);
        assert_eq!(p.client.player_action(), PlayerAct::HoldDown);
        assert!(p.client.is_down());
        assert_eq!(p.client.take_events(), vec![SessionEvent::Down]);
        p.deliver();
        assert!(p.host.respawn.is_down(CLIENT));
        assert_eq!(p.host.take_events(), vec![SessionEvent::PartnerDown]);
        assert_eq!(p.client.facts(p.t, false).me_down, Some(10));
        assert_eq!(p.host.facts(p.t, false).partner_down, Some(10));
        // Game time stops while paused: 10 s of running frames bring it back.
        for _ in 0..300 {
            p.frame(paused(), paused());
        }
        assert!(p.client.is_down());
        for _ in 0..602 {
            p.frame(running(), running());
        }
        assert_eq!(p.client.powers_action(), PowersAct::Revive);
        assert_eq!(
            p.client.player_action(),
            PlayerAct::HoldDown,
            "oPowers first"
        );
        p.client.powers_applied();
        // The game's own fly-in, wherever the partner is.
        assert_eq!(p.client.player_action(), PlayerAct::Revive);
        p.client.player_applied();
        assert!(!p.client.is_down());
        assert!(p.client.take_events().contains(&SessionEvent::Back));
        assert!(p.host.take_events().contains(&SessionEvent::PartnerBack));
    }

    #[test]
    fn everyone_down_is_a_normal_game_over_on_both() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        // The host dies too: its own death runs, the client is told.
        assert_eq!(p.host.death(p.t), Death::Die);
        p.deliver();
        assert!(p.client.take_events().contains(&SessionEvent::AllDown));
        assert_eq!(p.client.powers_action(), PowersAct::ForceDeath);
        p.client.powers_applied();
        assert_eq!(p.client.powers_action(), PowersAct::None);
        assert_eq!(
            p.client.death(p.t),
            Death::Die,
            "the forced death goes through"
        );
        assert_eq!(p.client.facts(p.t, false).me_down, None);
    }

    /// A downed player with levels chooses in the partner's round, with none
    /// waits on the pause screen, and never opens a round itself.
    #[test]
    fn a_downed_player_chooses_in_the_partners_round() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.host.death(p.t), Death::Down);
        p.deliver();
        p.host.set_control(Some(false)); // the downed recipe holds it at 0
        assert!(p.client.respawn.is_down(HOST));
        assert!(!p.host.lifts_control(), "nothing of its own to open");
        // The client opens a round: the downed host's screen opens too.
        assert!(!p.client.upgrade_opened(), "the opener's own screen");
        p.deliver();
        let (h, _) = p.frame(running(), running());
        assert_eq!(h, vec![Action::GotoUpgrade]);
        assert!(p.host.lifts_control(), "around the forced opening");
        assert!(p.host.upgrade_opened(), "opened by the partner's round");
        assert!(p.host.lifts_control(), "and its chained screens");
        assert!(p.host.upgrade.is_waiting_on(HOST), "waited for like anyone");
        // The client picks first: held for the downed host.
        assert!(p.client.upgrade_leaving(p.t));
        p.deliver();
        let (_, c) = p.frame(running(), running());
        assert!(c.is_empty(), "still held: {c:?}");
        // The host picks last: both count on their cards, then leave.
        assert!(p.host.upgrade_leaving(p.t), "held for the count");
        p.deliver();
        assert!(!p.host.lifts_control(), "on the held card");
        let (h, c) = p.count_out(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        assert!(!p.host.upgrade_leaving(p.t));
        assert!(!p.host.lifts_control());
        // Nothing to choose while down: done at once, held on the pause screen.
        let none = Observed {
            levelup: Some(0.0),
            ..running()
        };
        assert!(!p.client.upgrade_leaving(p.t));
        p.client.upgrade_opened();
        p.deliver();
        let (h, _) = p.frame(none, running());
        assert!(h.is_empty());
        assert!(
            !p.host.upgrade.is_waiting_on(HOST),
            "nothing to choose: not waited for"
        );
        let (h, _) = p.frame(none, running());
        assert_eq!(h, vec![Action::PressPause], "held on the pause screen");
    }

    /// Two levels to take.
    fn two() -> Observed {
        Observed {
            levelup: Some(2.0),
            ..running()
        }
    }

    /// One side down (the host when `host_down`) on the other's round's screen, two levels.
    fn side(p: &mut Pair, host: bool) -> &mut Rules {
        if host { &mut p.host } else { &mut p.client }
    }

    fn down_on_the_partners_screen(c: RulesSettings, host_down: bool) -> Pair {
        let mut p = Pair::new(c);
        run_frames(&mut p, 2);
        let t = p.t;
        assert_eq!(side(&mut p, host_down).death(t), Death::Down);
        side(&mut p, host_down).set_control(Some(false));
        p.deliver();
        assert!(
            !side(&mut p, !host_down).upgrade_opened(),
            "the opener's own screen"
        );
        p.deliver();
        let (h, c) = if host_down {
            p.frame(two(), running())
        } else {
            p.frame(running(), two())
        };
        assert_eq!(if host_down { h } else { c }, vec![Action::GotoUpgrade]);
        let down = side(&mut p, host_down);
        assert!(down.lifts_control(), "around the forced opening");
        assert!(down.upgrade_opened(), "opened by the partner's round");
        assert!(down.lifts_control(), "its chained screens");
        p
    }

    /// A game over with a downed player on its partner's round's screen: the
    /// chain keeps its lift, its end holds nothing and the forced death follows.
    #[test]
    fn a_game_over_on_a_downed_players_screen_lets_its_chain_through() {
        let mut p = down_on_the_partners_screen(cfg(), true);
        assert!(p.client.upgrade_leaving(p.t));
        p.deliver();
        p.frame(two(), running());
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        assert!(p.host.respawn.is_game_over(), "everyone down");
        assert!(p.host.upgrade_choosing(), "still on its first screen");
        assert!(p.host.lifts_control(), "the game's chain goes through");
        assert_eq!(
            p.host.powers_action(),
            PowersAct::None,
            "no forced death under the screen (a pick's refreshLoadout gives the hull back)"
        );
        // The chain ends (the last level, or Esc): nothing holds it.
        assert!(!p.host.upgrade_leaving(p.t), "not held");
        assert!(!p.host.frozen_unseen());
        assert!(!p.host.lifts_control());
        assert_eq!(p.host.powers_action(), PowersAct::ForceDeath, "now it goes");
        p.host.powers_applied();
        let (h, _) = p.frame(running(), running());
        assert!(h.is_empty(), "nothing to release or open: {h:?}");
        assert_eq!(
            p.host.death(p.t),
            Death::Die,
            "the game's death and end screen"
        );
    }

    /// The other exits for a downed player on its screen (the partner leaving,
    /// the link lost): the chain keeps its lift, and leaving holds nothing.
    #[test]
    fn every_exit_of_the_round_keeps_a_downed_players_chain() {
        for interrupted in [false, true] {
            let mut p = down_on_the_partners_screen(cfg(), true);
            p.drop_outs();
            p.host.peer_left(CLIENT, interrupted, p.t);
            assert!(
                p.host.lifts_control(),
                "the partner gone (interrupted {interrupted})"
            );
            assert!(!p.host.upgrade_leaving(p.t), "nothing to wait for");
            assert!(p.host.frame(&running(), p.t).is_empty());
        }
    }

    /// A downed player's held card released with levels left: lifted, no round.
    #[test]
    fn a_downed_players_released_card_chains_without_a_round() {
        let mut p = down_on_the_partners_screen(cfg(), true);
        assert!(p.host.upgrade_leaving(p.t), "held for the partner");
        assert!(!p.host.lifts_control(), "not on the card");
        p.deliver();
        assert!(p.client.upgrade_leaving(p.t), "the last choice: the count");
        p.deliver();
        let (h, c) = p.count_out(two(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        assert!(!p.client.upgrade_leaving(p.t));
        assert!(p.host.lifts_control(), "the release's chain");
        assert!(!p.host.upgrade_opened(), "no round of its own");
        p.deliver();
        assert!(p.host.upgrade.active().is_none() && p.client.upgrade.active().is_none());
        assert!(!p.host.upgrade_leaving(p.t), "nothing to wait for");
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty(), "{h:?} {c:?}");
    }

    /// The held card is the one hold the game cannot see. The pause-screen holds are real.
    #[test]
    fn the_held_card_is_frozen_where_the_game_cannot_tell() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(
            !p.host.frozen_unseen() && !p.client.frozen_unseen(),
            "choosing"
        );
        assert!(p.host.upgrade_leaving(p.t));
        assert!(p.host.frozen_unseen(), "held on the card");
        p.deliver();
        assert!(p.client.upgrade_leaving(p.t), "the last choice: the count");
        assert!(p.client.frozen_unseen(), "held on the card for the count");
        p.deliver();
        let (h, c) = p.count_out(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        assert!(p.host.frozen_unseen(), "until the game has left the card");
        assert!(!p.host.upgrade_leaving(p.t));
        assert!(!p.client.upgrade_leaving(p.t));
        assert!(!p.host.frozen_unseen() && !p.client.frozen_unseen());
        // Nothing to choose: held on the pause screen, the game's own pause.
        p.host.upgrade_opened();
        p.deliver();
        let none = Observed {
            levelup: Some(0.0),
            ..running()
        };
        p.frame(running(), none);
        p.frame(running(), none);
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::HeldPaused)
        );
        assert!(!p.client.frozen_unseen());
    }

    fn cues(r: &mut Rules) -> usize {
        r.take_events()
            .iter()
            .filter(|e| **e == SessionEvent::PartnerReady)
            .count()
    }

    /// One cue as the bar starts, only for a partner ready a second into the round.
    #[test]
    fn the_partner_ready_cue_sounds_once_and_not_for_an_instant_done() {
        // Upgrade round: the client picks 3 s in, and the host hears it once.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        p.frame(running(), running());
        p.later(Duration::from_secs(3));
        assert!(p.client.upgrade_leaving(p.t));
        p.deliver();
        let mut n = 0;
        for _ in 0..5 {
            p.frame(running(), running());
            n += cues(&mut p.host);
            assert_eq!(cues(&mut p.client), 0, "the held one hears nothing");
        }
        assert_eq!(n, 1);
        assert_eq!(
            p.host.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerReady)
        );
        // Nothing to choose on the client: done at once, no cue.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        let none = Observed {
            levelup: Some(0.0),
            ..running()
        };
        let mut n = 0;
        for _ in 0..5 {
            p.frame(running(), none);
            n += cues(&mut p.host);
        }
        assert_eq!(
            p.host.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerReady)
        );
        assert_eq!(n, 0, "an instant done rings nothing");
        // Shared pause: the client resumes 2 s in, and the host hears it.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        p.later(Duration::from_secs(2));
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        let mut n = 0;
        for _ in 0..3 {
            p.frame(paused(), paused());
            n += cues(&mut p.host);
            assert_eq!(cues(&mut p.client), 0);
        }
        assert_eq!(n, 1);
        // ... and not when it resumes at once.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        p.later(Duration::from_millis(400));
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        let mut n = 0;
        for _ in 0..3 {
            p.frame(paused(), paused());
            n += cues(&mut p.host);
        }
        assert_eq!(
            p.host.facts(p.t, true).pause,
            Some(SharedPause::PartnerReady)
        );
        assert_eq!(n, 0);
    }

    /// When the host's rules ask for the partner-ready sound: the client's Resume 2 s
    /// into a shared pause, the host's Resume and Esc in the count, the client's after `again`.
    fn ready_twice_on_the_pause_path(again: Duration) -> Vec<Instant> {
        let mut p = Pair::new(cfg());
        let mut asks = Vec::new();
        let frames = |p: &mut Pair, asks: &mut Vec<Instant>| {
            for _ in 0..3 {
                p.frame(paused(), paused());
                for _ in 0..cues(&mut p.host) {
                    asks.push(p.t);
                }
            }
        };
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        p.later(Duration::from_secs(2));
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        frames(&mut p, &mut asks);
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        p.frame(paused(), paused());
        assert!(p.host.counting());
        p.host.count_hold_on(p.t);
        p.deliver();
        frames(&mut p, &mut asks);
        p.later(again);
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        frames(&mut p, &mut asks);
        asks
    }

    /// The partner-ready sound is held back for `READY_SOUND_HOLD_OFF` after it
    /// played: two Readies 1.2 s apart give one sound, 6 s apart two.
    #[test]
    fn the_pause_paths_ready_sound_is_held_off_after_it_played() {
        use crate::ui::cues::ReadyHoldOff;
        let rang = |asks: &[Instant]| {
            let mut h = ReadyHoldOff::default();
            asks.iter().filter(|&&t| h.ask(t).is_ok()).count()
        };
        let quick = ready_twice_on_the_pause_path(Duration::from_millis(1200));
        assert_eq!(quick.len(), 2, "the rules ask twice: {quick:?}");
        assert_eq!(rang(&quick), 1, "one sound");
        let apart = ready_twice_on_the_pause_path(Duration::from_secs(6));
        assert_eq!(apart.len(), 2);
        assert_eq!(rang(&apart), 2, "two sounds");
    }

    #[test]
    fn last_partner_gone_while_down_respawns_on_its_own_timer() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        // The report never arrives: the link goes.
        p.client.take_outs();
        p.client.peer_left(HOST, true, p.t);
        for _ in 0..602 {
            p.client.frame(&running(), p.t);
        }
        assert_eq!(p.client.powers_action(), PowersAct::Revive);
        p.client.powers_applied();
        p.client.player_applied();
        // Still waiting for the host: a death is a down.
        assert_eq!(p.client.death(p.t), Death::Down);
        p.client.take_events();
        // The host gone for good: alone, a death is a death.
        p.client.peer_left(HOST, false, p.t);
        for _ in 0..602 {
            p.client.frame(&running(), p.t);
        }
        p.client.powers_applied();
        p.client.player_applied();
        assert!(
            !p.client.is_down(),
            "a down when the wait became final respawns"
        );
        assert_eq!(p.client.death(p.t), Death::Die);
    }

    /// The link is lost on the host's side only (the joining game is frozen).
    fn host_waits(p: &mut Pair) {
        p.drop_outs();
        p.host.peer_left(CLIENT, true, p.t);
        p.client.peer_left(HOST, true, p.t);
        p.drop_outs();
    }

    /// The host dies while playing on in a link wait: a down on its own timer.
    /// The joining game hears the time left at the return. The host respawns once.
    #[test]
    fn a_host_death_during_a_link_wait_is_a_down_told_at_the_return() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        host_waits(&mut p);
        assert_eq!(p.host.death(p.t), Death::Down, "not the run's end");
        assert_eq!(p.host.take_events(), vec![SessionEvent::Down]);
        assert!(p.host.take_outs().is_empty(), "nothing sent while apart");
        assert_eq!(p.host.facts(p.t, false).me_down, Some(10));
        // 3 s of play on, then the return (the client was frozen).
        for _ in 0..180 {
            p.host.frame(&running(), p.t);
        }
        p.back();
        assert!(p.client.take_events().contains(&SessionEvent::PartnerDown));
        let left = p
            .client
            .respawn
            .remaining(HOST, p.client.game_time)
            .unwrap();
        assert!(
            (Duration::from_secs(6)..=Duration::from_secs(7)).contains(&left),
            "the time left: {left:?}"
        );
        let mut revives = 0;
        for _ in 0..600 {
            p.frame(running(), running());
            if p.host.powers_action() == PowersAct::Revive {
                revives += 1;
                p.host.powers_applied();
                p.host.player_applied();
            }
        }
        assert_eq!(revives, 1);
        assert!(!p.host.is_down() && !p.client.respawn.is_down(HOST));
        assert!(p.client.take_events().contains(&SessionEvent::PartnerBack));
        assert!(!p.host.respawn.is_game_over() && !p.client.respawn.is_game_over());
    }

    /// Down in the wait, then the partner dies after the return: game over on both.
    #[test]
    fn down_in_a_wait_then_the_partner_down_after_it_is_the_game_over() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        host_waits(&mut p);
        assert_eq!(p.host.death(p.t), Death::Down);
        p.back();
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        assert!(p.host.respawn.is_game_over());
        assert!(p.client.take_events().contains(&SessionEvent::AllDown));
        assert_eq!(p.host.powers_action(), PowersAct::ForceDeath);
    }

    /// The partner was down when the link went: a host death in the wait is the
    /// game's own death, and the joining game hears of it at the return.
    #[test]
    fn both_down_across_a_wait_is_the_game_over_told_at_the_return() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        host_waits(&mut p);
        assert_eq!(p.host.death(p.t), Death::Die);
        assert!(p.host.respawn.is_game_over());
        assert!(p.host.take_outs().is_empty());
        p.back();
        assert!(p.client.respawn.is_game_over(), "told at the return");
        assert!(p.client.take_events().contains(&SessionEvent::AllDown));
        assert_eq!(p.client.powers_action(), PowersAct::ForceDeath);
    }

    /// Both ships down, in either order: game over. The host's ship leaves "down
    /// and hidden" as the game over is known, before the forced death.
    #[test]
    fn a_game_over_ends_the_hidden_state_before_the_forced_death() {
        // The host first, then the joining ship.
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert!(!p.host.ship_hidden());
        assert_eq!(p.host.death(p.t), Death::Down);
        assert!(p.host.ship_hidden(), "down: hidden");
        p.deliver();
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        assert!(p.host.respawn.is_game_over());
        assert!(!p.host.ship_hidden(), "the game's own death follows");
        assert_eq!(p.host.powers_action(), PowersAct::ForceDeath);
        // The joining ship first, then the host.
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        assert!(p.client.ship_hidden());
        assert_eq!(p.host.death(p.t), Death::Die);
        assert!(!p.host.ship_hidden());
        p.deliver();
        assert!(!p.client.ship_hidden(), "the game's own death follows");
        assert_eq!(p.client.powers_action(), PowersAct::ForceDeath);
    }

    /// The respawn: hidden until the ship's own revive step is applied.
    /// After a wait that became final the next death is the game's own.
    #[test]
    fn the_respawn_ends_the_hidden_state() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.host.death(p.t), Death::Down);
        let mut seen = false;
        for _ in 0..600 {
            p.frame(running(), running());
            if p.host.powers_action() == PowersAct::Revive {
                assert!(p.host.ship_hidden(), "respawning: still hidden");
                p.host.powers_applied();
                p.host.player_applied();
                seen = true;
            }
        }
        assert!(seen);
        assert!(!p.host.ship_hidden(), "back: not hidden");
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        host_waits(&mut p);
        assert_eq!(p.host.death(p.t), Death::Down);
        p.host.peer_left(CLIENT, false, p.t);
        for _ in 0..602 {
            p.host.frame(&running(), p.t);
        }
        assert_eq!(p.host.powers_action(), PowersAct::Revive);
        p.host.powers_applied();
        p.host.player_applied();
        assert!(!p.host.ship_hidden());
        assert_eq!(p.host.death(p.t), Death::Die);
        assert!(!p.host.ship_hidden(), "the game's own death");
    }

    /// The partner leaves for good in the wait while the host is down on its own
    /// timer: nothing is held, and the next death is the game's own.
    #[test]
    fn a_wait_that_becomes_final_holds_nothing_in_the_rules() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        host_waits(&mut p);
        assert_eq!(p.host.death(p.t), Death::Down);
        p.host.peer_left(CLIENT, false, p.t);
        assert!(p.host.separated());
        for _ in 0..602 {
            p.host.frame(&running(), p.t);
        }
        assert_eq!(p.host.powers_action(), PowersAct::Revive);
        p.host.powers_applied();
        p.host.player_applied();
        assert_eq!(p.host.death(p.t), Death::Die, "solo now: the run ends");
    }

    /// The joining game restarted and linked again while the host waited: its
    /// `RunLeft` drops it, and no shared pause or upgrade round waits for it.
    #[test]
    fn a_partner_back_without_the_run_holds_no_pause_or_round() {
        // The pause: the host on its menu through the wait and the return.
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        host_waits(&mut p);
        p.host.frame(&paused(), p.t);
        p.host.peer_joined(CLIENT, p.t);
        p.drop_outs();
        assert_eq!(
            p.host.unpause_request(false, p.t),
            UnpauseCall::Block,
            "the pause again with the client in it"
        );
        p.host.peer_left(CLIENT, false, p.t);
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Allow);
        // The round: the host's screen opened after the return, its pick.
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        host_waits(&mut p);
        p.host.peer_joined(CLIENT, p.t);
        p.drop_outs();
        let _ = p.host.upgrade_opened();
        assert!(
            p.host.upgrade.active().is_some() && p.host.upgrade.is_waiting_on(CLIENT),
            "the round waits for the client"
        );
        p.host.peer_left(CLIENT, false, p.t);
        assert!(!p.host.upgrade_leaving(p.t), "nobody to hold the card for");
        p.host.frame(&running(), p.t);
        assert!(!p.host.frozen_unseen() && !p.host.upgrade_choosing());
    }

    /// With the partner gone from the run or the session, a host death ends the run.
    #[test]
    fn a_death_after_a_real_departure_is_the_games_own() {
        for interrupted_first in [false, true] {
            let mut p = Pair::new(cfg());
            run_frames(&mut p, 2);
            if interrupted_first {
                host_waits(&mut p);
            }
            p.host.peer_left(CLIENT, false, p.t);
            assert_eq!(p.host.death(p.t), Death::Die, "{interrupted_first}");
        }
    }

    /// A held round ends while the session holds the game: the rules unpause after.
    #[test]
    fn the_rules_never_unpause_a_game_the_session_holds() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        // The host pauses: the client is held paused by the rules.
        p.frame(paused(), running());
        let (_, c) = p.frame(paused(), running());
        assert_eq!(c, vec![Action::PressPause]);
        let (_, c) = p.frame(paused(), paused());
        assert!(c.is_empty());
        // The client resumes while the host stays on the menu: held.
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        let (_, c) = p.frame(paused(), paused());
        assert!(c.is_empty());
        // The host leaves the run: the round is released on the client.
        p.client.peer_left(HOST, false, p.t);
        let held = Observed {
            held: true,
            ..paused()
        };
        for _ in 0..60 {
            let c = p.client.frame(&held, p.t);
            assert!(!c.contains(&Action::PressUnpause), "{c:?}");
        }
        let c = p.client.frame(&paused(), p.t);
        assert_eq!(c, vec![Action::PressUnpause], "once the hold lets go");
    }

    /// The host's round announced at the return unpauses the joining game after the hold.
    #[test]
    fn the_round_announced_at_the_return_waits_for_the_hold() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.timed_out();
        let held = Observed {
            held: true,
            ..paused()
        };
        p.client.note_own(true);
        for _ in 0..30 {
            let c = p.client.frame(&held, p.t);
            assert!(c.is_empty(), "in the wait: {c:?}");
            p.host.frame(&running(), p.t);
        }
        p.host.upgrade_opened();
        p.back();
        let mut seen = Vec::new();
        for _ in 0..90 {
            seen.extend(p.client.frame(&held, p.t));
            p.deliver();
        }
        assert!(!seen.contains(&Action::PressUnpause), "{seen:?}");
        // The hold lets go: the rules unpause to open the screen.
        let c = p.client.frame(&paused(), p.t);
        assert_eq!(c, vec![Action::PressUnpause]);
        p.lossy = true;
    }

    /// Both down across a long wait that becomes the shared pause: the host's end
    /// screen is not held, and the joining game is unpaused for its forced death.
    #[test]
    fn told_game_over_behind_an_adopted_pause_reaches_the_death() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        host_waits(&mut p);
        p.client.note_own(true);
        p.client.frame(&paused(), p.t);
        assert_eq!(p.host.death(p.t), Death::Die);
        let over = Observed {
            game_over: true,
            ..running()
        };
        p.host.frame(&over, p.t);
        p.later(Duration::from_secs(14));
        // The return: the rules first, then the long freeze adopted.
        p.host.peer_joined(CLIENT, p.t);
        p.client.peer_joined(HOST, p.t);
        p.client.adopt_pause(p.t);
        p.deliver();
        assert!(p.client.respawn.is_game_over(), "told at the return");
        let mut acts = Vec::new();
        for _ in 0..120 {
            p.later(Duration::from_millis(17));
            p.host.frame(&over, p.t);
            acts.extend(p.client.frame(&paused(), p.t));
            p.deliver();
        }
        assert!(!p.host.pause.holds(HOST), "the host holds nobody");
        assert!(acts.contains(&Action::PressUnpause), "{acts:?}");
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);
        p.client.frame(&running(), p.t);
        assert_eq!(p.client.powers_action(), PowersAct::ForceDeath);
        p.lossy = true;
    }

    /// The same with the pause adopted first. A game whose run is over releases a held card.
    #[test]
    fn a_game_over_releases_pause_and_held_card() {
        // Pause first: the client is on its menu in a shared round.
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        p.frame(paused(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        // The host's game over arrives with the client on the pause menu.
        assert!(p.client.holding_pause());
        p.host.respawn.end();
        p.host.on_game_over();
        p.host.bcast(Message::GameOver);
        p.deliver();
        let over = Observed {
            game_over: true,
            ..running()
        };
        let mut acts = Vec::new();
        for _ in 0..60 {
            p.host.frame(&over, p.t);
            acts.extend(p.client.frame(&paused(), p.t));
            p.deliver();
        }
        assert!(!p.client.pause.is_paused() || !p.client.pause.holds(HOST));
        assert!(acts.contains(&Action::PressUnpause), "{acts:?}");
        // A held card is released, whatever the partner still chooses.
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        p.client.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        assert!(p.host.upgrade_opened());
        assert!(p.client.upgrade_leaving(p.t), "held after choosing");
        p.deliver();
        p.client.respawn.end();
        p.client.on_game_over();
        let c = p.client.frame(&running(), p.t);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        p.lossy = true;
    }

    /// A down unconfirmed across a link wait hands its time to the host and revives once.
    #[test]
    fn a_down_unconfirmed_across_a_link_wait_revives_once_on_the_time_left() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        // The report is lost with the link: both sides wait, apart.
        p.timed_out();
        for _ in 0..300 {
            p.client.frame(&running(), p.t);
            p.host.frame(&running(), p.t);
        }
        p.drop_outs();
        assert!(p.client.is_down());
        p.back();
        let left = p
            .host
            .respawn
            .remaining(CLIENT, p.host.game_time)
            .expect("the host has the client down");
        assert!(
            left <= Duration::from_secs(5) + FRAME * 2,
            "the client hands over the time left, not a fresh timer: {left:?}"
        );
        let mut revives = 0;
        let mut host_down_at = None;
        for f in 0..900 {
            p.frame(running(), running());
            if p.client.powers_action() == PowersAct::Revive {
                revives += 1;
                p.client.powers_applied();
                p.client.player_applied();
            }
            // While the client flies, the host never has it down.
            if !p.client.is_down() {
                assert!(
                    !p.host.respawn.is_down(CLIENT),
                    "frame {f}: down on the host, up on the client"
                );
                host_down_at.get_or_insert(f);
            }
        }
        assert_eq!(revives, 1, "one revive");
        assert!(
            host_down_at.is_some_and(|f| f <= 310),
            "back on the time left: {host_down_at:?}"
        );
        assert_eq!(p.host.death(p.t), Death::Down);
    }

    /// The host's respawn time applies to the client's own timer too.
    #[test]
    fn the_hosts_respawn_time_counts_for_the_clients_own_timer_too() {
        let mut p = Pair::new(cfg());
        p.client.cfg.respawn = Duration::from_secs(30);
        p.client.set_respawn(Duration::from_secs(10));
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.timed_out();
        let mut back_at = None;
        for f in 1..=2000 {
            p.client.frame(&running(), p.t);
            if p.client.powers_action() == PowersAct::Revive {
                back_at.get_or_insert(f);
                p.client.powers_applied();
                p.client.player_applied();
            }
        }
        assert!(
            back_at.is_some_and(|f| (600..=605).contains(&f)),
            "back after the host's 10 s, alone: {back_at:?}"
        );
    }

    /// Never linked (solo with the mod installed): the game's own death every time.
    #[test]
    fn a_solo_game_dies_as_the_game_does() {
        let mut r = Rules::new(cfg());
        let t = Instant::now();
        r.sync_run(&RunCtx {
            key: (1, Some(1.0)),
            coop_run: false,
            linked: false,
            gate_running: true,
            me: PeerId::HOST,
            is_host: true,
            partners: PeerSet::EMPTY,
        });
        assert!(!r.active());
        assert_eq!(r.death(t), Death::Die);
        assert_eq!(r.death(t), Death::Die);
        assert_eq!(r.take_outs(), [] as [crate::net::Out; 0]);
    }

    #[test]
    fn xp_flows_from_host_to_client() {
        let mut p = Pair::new(cfg());
        let host = Observed {
            xp: Some(XpRead {
                xp: 7.0,
                maxexp: 20.0,
                base: 0.0,
            }),
            ..running()
        };
        let client = Observed {
            xp: Some(XpRead {
                xp: 0.0,
                maxexp: 20.0,
                base: 0.0,
            }),
            ..running()
        };
        p.frame(host, client);
        let (_, c) = p.frame(host, client);
        assert_eq!(c, vec![Action::SetXp(7.0)]);
        let frozen = Observed {
            paused: true,
            ..client
        };
        let (_, c) = p.frame(host, frozen);
        assert!(c.is_empty(), "no writes while frozen");
    }

    #[test]
    fn the_hosts_score_is_the_clients_and_its_own_updates_are_skipped() {
        let mut p = Pair::new(cfg());
        let sc = |score| Score {
            score,
            verifier: 4.0,
            modulus: 13.0,
            boss_kills: 2.0,
            star_eater_kills: 0.0,
        };
        let host = Observed {
            score: Some(sc(1_300.0)),
            ..running()
        };
        let client = Observed {
            score: Some(sc(0.0)),
            ..running()
        };
        p.frame(host, client);
        let (_, c) = p.frame(host, client);
        assert_eq!(c, vec![Action::SetScore(sc(1_300.0))]);
        assert!(p.client.skips_own_score() && !p.host.skips_own_score());
        p.client.own_score_skipped();
        assert_eq!(p.client.score_counts(), (1, 1, Some(sc(1_300.0))));
        // The end screen: the host's numbers on the client, nothing on the host.
        assert_eq!(
            p.client.end_screen_score(sc(13.0)),
            Some((sc(1_300.0), false))
        );
        assert_eq!(p.host.end_screen_score(sc(1_300.0)), None);
    }

    #[test]
    fn a_new_run_resets_and_a_missing_stamp_does_not() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        assert!(p.host.pause.is_paused());
        let no_stamp = RunCtx {
            key: (7, None),
            ..ctx(true)
        };
        p.host.sync_run(&no_stamp);
        assert!(p.host.pause.is_paused(), "same run");
        let next = RunCtx {
            key: (7, Some(2.0)),
            ..ctx(true)
        };
        p.host.sync_run(&next);
        assert!(!p.host.pause.is_paused(), "a restart is a new run");
        assert!(p.host.active(), "and the rules start again at once");
    }

    /// A joining ship flies back in on its own clock, with no second revive at `Respawned`.
    #[test]
    fn a_joining_ship_comes_back_on_its_own_clock() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        let _ = p.client.take_events();
        let mut late = Vec::new();
        let mut n = 0;
        while p.client.powers_action() != PowersAct::Revive {
            p.host.frame(&running(), p.t);
            p.client.frame(&running(), p.t);
            // The host's words are late. The client's reach it.
            late.extend(p.host.take_outs());
            for o in p.client.take_outs() {
                if let Out::Broadcast(m) | Out::Send(_, m) = o {
                    p.host.on_message(CLIENT, &m, p.t);
                }
            }
            n += 1;
            assert!(n < 3000, "never back");
        }
        let ev = p.client.take_events();
        assert!(ev.contains(&SessionEvent::Back), "{ev:?}");
        assert!(ev.contains(&SessionEvent::CountGo(CountKind::Respawn)));
        p.client.powers_applied();
        assert_eq!(p.client.player_action(), PlayerAct::Revive);
        p.client.player_applied();
        assert!(!p.client.is_down());
        // The host's word arrives: nothing more happens to the ship.
        for o in late {
            if let Out::Broadcast(m) | Out::Send(_, m) = o {
                p.client.on_message(HOST, &m, p.t);
            }
        }
        assert!(!p.client.is_down());
        assert_eq!(p.client.player_action(), PlayerAct::None);
        assert!(!p.client.take_events().contains(&SessionEvent::Back));
    }

    /// The host goes down while the joining ship is up only on its own clock: game over on both.
    #[test]
    fn a_game_over_in_the_link_delay_after_the_clients_own_clock_ends_its_run() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        let mut n = 0;
        while p.client.powers_action() != PowersAct::Revive {
            // The client's clock a frame ahead: the host's due is not yet.
            p.client.frame(&running(), p.t);
            p.client.take_outs();
            if p.client.powers_action() == PowersAct::Revive {
                break;
            }
            p.host.frame(&running(), p.t);
            p.host.take_outs();
            n += 1;
            assert!(n < 3000, "never back");
        }
        p.client.powers_applied();
        p.client.player_applied();
        assert!(!p.client.is_down());
        assert!(
            p.host.respawn.is_down(CLIENT),
            "not yet on the host's count"
        );
        assert_eq!(p.host.death(p.t), Death::Die, "everyone down there");
        p.deliver();
        assert_eq!(p.client.powers_action(), PowersAct::ForceDeath);
        assert_eq!(p.client.death(p.t), Death::Die);
    }

    /// The host's own ship flies back in too (on its own clock).
    #[test]
    fn the_host_flies_back_in() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.host.death(p.t), Death::Down);
        p.deliver();
        let mut n = 0;
        while p.host.powers_action() != PowersAct::Revive {
            p.frame(running(), running());
            n += 1;
            assert!(n < 3000, "never back");
        }
        p.host.powers_applied();
        assert_eq!(p.host.player_action(), PlayerAct::Revive);
        p.host.player_applied();
        assert!(!p.host.is_down());
        assert!(p.client.take_events().contains(&SessionEvent::PartnerBack));
    }

    // ---- the counts and Ataraxia ----------------------------------------------

    /// Both games in the resume count after a shared pause the client resumed last.
    fn counting_pair() -> Pair {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        for _ in 0..PRESS_EVERY * 2 {
            p.frame(paused(), paused());
        }
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        let (h, c) = p.frame(paused(), paused());
        assert!(h.is_empty() && c.is_empty(), "{h:?} {c:?}");
        assert!(p.host.counting() && p.client.counting());
        p.host.take_events();
        p.client.take_events();
        p
    }

    fn count_events(r: &mut Rules, kind: CountKind) -> (Vec<u8>, bool) {
        let mut ticks = Vec::new();
        let mut go = false;
        for e in r.take_events() {
            match e {
                SessionEvent::CountTick(k, n) if k == kind => ticks.push(n),
                SessionEvent::CountGo(k) if k == kind => go = true,
                _ => {}
            }
        }
        (ticks, go)
    }

    /// The round's end counts 3, 2, 1 on both games, and both leave it in the same frame.
    #[test]
    fn the_resume_count_leaves_both_games_in_the_same_frame_at_a_link_delay() {
        let one_way = Duration::from_millis(85);
        let mut p = Pair::new(cfg());
        p.host.set_rtt(2 * one_way);
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        // The client's Resume reaches the host, its relay the client one way later.
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        let outs = p.client.take_outs();
        to_host(&mut p, outs);
        assert!(p.host.counting() && !p.client.counting());
        let relay = p.host.take_outs();
        p.later(one_way);
        for o in relay {
            if let Out::Broadcast(m) | Out::Send(_, m) = o {
                p.client.on_message(HOST, &m, p.t);
            }
        }
        assert!(p.client.counting());
        p.host.take_events();
        p.client.take_events();
        let (mut ticks, mut go) = ((Vec::new(), Vec::new()), (None, None));
        for i in 0..240u32 {
            let (h, c) = p.frame(paused(), paused());
            let (hf, cf) = (p.host.facts(p.t, true), p.client.facts(p.t, true));
            assert_eq!(
                hf.count.map(|v| v.number),
                cf.count.map(|v| v.number),
                "frame {i}"
            );
            let (th, gh) = count_events(&mut p.host, CountKind::Resume);
            let (tc, gc) = count_events(&mut p.client, CountKind::Resume);
            assert_eq!(th, tc, "the same tick in the same frame ({i})");
            ticks.0.extend(th);
            ticks.1.extend(tc);
            if gh {
                assert_eq!(h, vec![Action::PressUnpause]);
                go.0 = Some(i);
            }
            if gc {
                assert_eq!(c, vec![Action::PressUnpause]);
                go.1 = Some(i);
            }
            p.later(FRAME);
        }
        assert_eq!(ticks.0, vec![3, 2, 1]);
        assert_eq!(ticks.1, vec![3, 2, 1]);
        assert!(go.0.is_some() && go.0 == go.1, "{go:?}");
        // About 3 s of 60 frames, from the relay.
        assert!(go.0.is_some_and(|f| (178..=182).contains(&f)), "{go:?}");
    }

    /// Esc in the count stops it on both and opens a new pause. A Resume in it changes nothing.
    #[test]
    fn esc_in_the_resume_count_holds_on() {
        let mut p = counting_pair();
        p.later(Duration::from_secs(1));
        p.frame(paused(), paused());
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        assert!(p.client.counting(), "a Resume in the count changes nothing");
        assert_eq!(
            p.client.facts(p.t, true).count.map(|c| (c.kind, c.number)),
            Some((CountKind::Resume, 2))
        );
        p.client.count_hold_on(p.t);
        assert!(!p.client.counting());
        p.frame(paused(), paused());
        assert!(
            !p.host.counting(),
            "the host's count stopped at the client's pause"
        );
        assert!(p.host.pause.holds(HOST) && p.host.pause.holds(CLIENT));
        assert!(p.client.pause.holds(HOST) && p.client.pause.holds(CLIENT));
        let (h, c) = p.count_out(paused(), paused());
        assert!(h.is_empty() && c.is_empty(), "held: {h:?} {c:?}");
        assert_eq!(p.host.facts(p.t, true).count, None);
        assert_eq!(p.host.facts(p.t, true).pause, Some(SharedPause::BothPaused));
        let (_, go) = count_events(&mut p.host, CountKind::Resume);
        assert!(!go, "no go for a count held on");
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.frame(paused(), paused());
        assert!(p.host.counting() && p.client.counting());
        let (h, c) = p.count_out(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    /// A link lost in the count lets it run out under the wait's hold. A joining
    /// game whose Resume was lost resumes at once, with no count.
    #[test]
    fn a_link_lost_in_the_resume_count_lets_it_run_out_under_the_wait() {
        let mut p = counting_pair();
        p.timed_out();
        assert!(p.host.counting() && p.client.counting());
        let held = Observed {
            held: true,
            ..paused()
        };
        let (h, c) = p.count_out(held, held);
        assert!(h.is_empty() && c.is_empty(), "the wait holds: {h:?} {c:?}");
        let (h, c) = p.frame(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause]);
        assert_eq!(c, vec![Action::PressUnpause]);

        let mut p = Pair::new(cfg());
        let _lost = client_resumes_last(&mut p);
        p.timed_out();
        assert!(!p.client.counting(), "alone: no count");
        let c = p.client.frame(&paused(), p.t);
        assert_eq!(c, vec![Action::PressUnpause]);
    }

    /// A down during the count changes nothing. A game over ends it with no go.
    #[test]
    fn a_down_in_the_resume_count_keeps_it_and_a_game_over_ends_it() {
        let mut p = counting_pair();
        assert_eq!(p.host.death(p.t), Death::Down);
        p.deliver();
        assert!(p.host.counting() && p.client.counting(), "a down");
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        assert!(p.host.respawn.is_game_over() && p.client.game_over_seen);
        assert!(!p.host.counting() && !p.client.counting());
        let (h, c) = p.frame(paused(), paused());
        assert_eq!(h, vec![Action::PressUnpause], "to the game's end screen");
        assert_eq!(c, vec![Action::PressUnpause]);
        let (h, c) = p.count_out(running(), running());
        assert!(h.is_empty() && c.is_empty(), "{h:?} {c:?}");
        assert!(!count_events(&mut p.host, CountKind::Resume).1);
        assert!(!count_events(&mut p.client, CountKind::Resume).1);
    }

    /// The upgrade round's count runs on both cards. The pause key on a card stops it.
    #[test]
    fn the_upgrade_rounds_end_counts_on_both_cards() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.host.upgrade_leaving(p.t));
        p.deliver();
        p.later(UPGRADE_COUNT_AFTER);
        assert!(p.client.upgrade_leaving(p.t), "the last choice: held");
        p.deliver();
        assert!(p.host.counting_on_card() && p.client.counting_on_card());
        assert!(p.client.frozen_unseen(), "the world stays frozen");
        p.later(Duration::from_millis(1500));
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty());
        assert_eq!(p.client.facts(p.t, false).count.map(|c| c.number), Some(2));
        // The client's pause key on the card: both on their pause screens.
        p.client.count_hold_on(p.t);
        p.deliver();
        let (h, c) = p.frame(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        assert!(!p.host.upgrade_leaving(p.t) && !p.client.upgrade_leaving(p.t));
        let (h, c) = p.frame(running(), running());
        assert_eq!(h, vec![Action::PressPause]);
        assert_eq!(c, vec![Action::PressPause]);
        assert!(!p.host.counting() && p.host.pause.holds(CLIENT));
    }

    /// The respawn alert follows the respawn clock: a pause or a link wait
    /// holds the number. It runs on the downed game only, with one go.
    #[test]
    fn the_respawn_alert_counts_the_last_seconds_on_the_downed_game_only() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.deliver();
        p.host.take_events();
        p.client.take_events();
        let held = Observed {
            held: true,
            ..paused()
        };
        let mut alerts = Vec::new();
        let mut goes = Vec::new();
        let mut run = 0u32;
        while p.client.is_down() && run < 700 {
            // At 8 s of the 10 (the alert shows 2): a pause, then a link wait.
            if run == 480 {
                // A shared pause: on the menus, both resume, the count.
                for _ in 0..300 {
                    p.frame(paused(), paused());
                }
                assert_eq!(
                    p.client.facts(p.t, true).count.map(|c| (c.kind, c.number)),
                    Some((CountKind::Respawn, 2))
                );
                assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
                p.deliver();
                assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
                let mut n = 0;
                loop {
                    let (h, _) = p.frame(paused(), paused());
                    p.later(FRAME);
                    n += 1;
                    if h.contains(&Action::PressUnpause) {
                        break;
                    }
                    assert!(n < 400, "the count never ended");
                }
                // A link wait (the session's hold).
                for _ in 0..300 {
                    p.frame(held, held);
                }
                assert_eq!(
                    count_events(&mut p.client, CountKind::Respawn).0,
                    Vec::<u8>::new(),
                    "the clock is held"
                );
                assert_eq!(
                    p.client.facts(p.t, true).count.map(|c| (c.kind, c.number)),
                    Some((CountKind::Respawn, 2))
                );
            }
            p.frame(running(), running());
            run += 1;
            let (ticks, go) = count_events(&mut p.client, CountKind::Respawn);
            for n in ticks {
                alerts.push((n, run));
            }
            if go {
                goes.push(run);
            }
            assert_eq!(
                count_events(&mut p.host, CountKind::Respawn),
                (Vec::new(), false),
                "nothing on the partner's game"
            );
            if p.client.powers_action() == PowersAct::Revive {
                p.client.powers_applied();
                p.client.player_applied();
            }
        }
        assert!(!p.client.is_down(), "back");
        let at: Vec<u8> = alerts.iter().map(|a| a.0).collect();
        assert_eq!(at, vec![3, 2, 1]);
        // A second apart, from 3 s before the return (frames of game time).
        assert!((419..=422).contains(&alerts[0].1), "{alerts:?}");
        assert!((479..=482).contains(&alerts[1].1), "{alerts:?}");
        assert!((539..=542).contains(&alerts[2].1), "{alerts:?}");
        assert_eq!(goes.len(), 1, "one go: {goes:?}");
        assert!(
            goes[0] >= alerts[2].1,
            "the go after the 1: {goes:?} {alerts:?}"
        );
        for _ in 0..120 {
            p.frame(running(), running());
        }
        assert_eq!(
            count_events(&mut p.client, CountKind::Respawn),
            (Vec::new(), false)
        );
        assert_eq!(p.host.facts(p.t, false).count, None);
    }

    fn ataraxia(o: Observed) -> Observed {
        Observed {
            ataraxia: true,
            ..o
        }
    }

    /// The host's round open, the client (with Ataraxia) on its waiting screen.
    fn ataraxia_held_pair(c: RulesSettings) -> Pair {
        let mut p = Pair::new(c);
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        let (_, c) = p.frame(running(), ataraxia(running()));
        assert!(c.is_empty(), "no upgrade screen: {c:?}");
        assert!(!p.host.upgrade.is_waiting_on(CLIENT), "done at once");
        let (_, c) = p.frame(running(), ataraxia(running()));
        assert_eq!(c, vec![Action::PressPause], "the waiting screen");
        let (_, c) = p.frame(running(), ataraxia(paused()));
        assert!(c.is_empty(), "{c:?}");
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::AtaraxiaHeld)
        );
        p
    }

    /// The host picks first, the client `wait` later: both on their cards at the last pick.
    fn round_picked_after(wait: Duration) -> Pair {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.host.upgrade_leaving(p.t), "the host's pick: held");
        p.deliver();
        p.frame(running(), running());
        p.later(wait);
        p.frame(running(), running());
        assert!(p.client.upgrade_leaving(p.t), "the last pick: held");
        p.deliver();
        p
    }

    /// Nobody waited `UPGRADE_COUNT_AFTER`: both leave their cards with no count or go sound.
    fn resumes_with_no_count(p: &mut Pair) {
        assert!(!p.host.counting() && !p.client.counting());
        let (h, c) = p.frame(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        for r in [&mut p.host, &mut p.client] {
            let ev = r.take_events();
            assert!(ev.contains(&SessionEvent::ResumedNoCount), "{ev:?}");
            assert!(
                !ev.iter()
                    .any(|e| matches!(e, SessionEvent::CountTick(..) | SessionEvent::CountGo(_))),
                "{ev:?}"
            );
        }
    }

    /// The resume count after an upgrade round runs only when a player has
    /// waited `UPGRADE_COUNT_AFTER` after picking when the last pick comes.
    #[test]
    fn the_rounds_end_counts_only_after_a_wait_of_8_s() {
        // Both pick promptly: no count.
        let mut p = round_picked_after(Duration::from_secs(2));
        resumes_with_no_count(&mut p);
        let mut p = round_picked_after(Duration::from_millis(7900));
        assert!(!p.host.facts(p.t, false).upgrade_counts);
        resumes_with_no_count(&mut p);
        // 8.0 s and more: the count, on both.
        for wait in [UPGRADE_COUNT_AFTER, Duration::from_secs(25)] {
            let mut p = round_picked_after(wait);
            assert!(p.host.counting() && p.client.counting(), "{wait:?}");
            let (h, c) = p.frame(running(), running());
            assert!(h.is_empty() && c.is_empty(), "counting: {h:?} {c:?}");
            let (h, c) = p.count_out(running(), running());
            assert_eq!(h, vec![Action::ReleaseUpgrade]);
            assert_eq!(c, vec![Action::ReleaseUpgrade]);
        }
    }

    /// The host's `count` in the relay counts, not the joining game's clock.
    /// With no count the host waits half the round trip.
    #[test]
    fn the_joining_game_takes_the_hosts_word_on_the_count() {
        // The host's relay says "count" though the client saw a short wait.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.host.upgrade_leaving(p.t));
        let held = p.host.take_outs();
        p.later(UPGRADE_COUNT_AFTER);
        to_client(&mut p, held);
        assert!(p.client.upgrade_leaving(p.t));
        p.deliver();
        assert!(p.host.counting() && p.client.counting(), "the host's word");
        // No count, a round trip of 170 ms: the host leaves its card 85 ms
        // after the release, the joining game at the relay.
        let mut p = Pair::new(cfg());
        p.host.set_rtt(Duration::from_millis(170));
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.client.upgrade_leaving(p.t));
        p.deliver();
        assert!(
            p.host.upgrade_leaving(p.t),
            "the host's last pick: held for its lead"
        );
        p.deliver();
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty(), "the lead: {h:?}");
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        p.later(Duration::from_millis(85));
        let (h, _) = p.frame(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
    }

    /// Reopening the screen (Ataraxia's Space) takes the pick out and restarts its wait.
    #[test]
    fn a_reopened_screen_starts_the_wait_again() {
        let reopen_then = |back: Duration| {
            let mut p = ataraxia_held_pair(cfg());
            // On the wait 9 s, then the client's own screen.
            p.later(Duration::from_secs(9));
            p.frame(running(), ataraxia(paused()));
            assert!(p.client.choose_own());
            p.deliver();
            assert!(
                !p.host.facts(p.t, false).upgrade_counts,
                "the reopen ended the client's wait"
            );
            let (_, c) = p.frame(running(), ataraxia(paused()));
            assert_eq!(c, vec![Action::PressUnpause]);
            let (_, c) = p.frame(running(), ataraxia(running()));
            assert_eq!(c, vec![Action::GotoUpgrade]);
            p.client.upgrade_opened();
            p.later(Duration::from_secs(2));
            assert!(
                p.client.upgrade_leaving(p.t),
                "the client's pick: back on the wait"
            );
            p.deliver();
            p.frame(running(), ataraxia(running()));
            p.later(back);
            p.frame(running(), ataraxia(running()));
            assert!(p.host.upgrade_leaving(p.t), "the host's last pick");
            p.deliver();
            p
        };
        // Back on the wait 5 s when the last pick comes (16 s since the
        // round opened, 9 s of it before the reopen): no count.
        let mut p = reopen_then(Duration::from_secs(5));
        assert!(!p.host.counting() && !p.client.counting());
        let (h, _) = p.frame(running(), ataraxia(running()));
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        // An unbroken `UPGRADE_COUNT_AFTER` after the reopen: the count.
        let p = reopen_then(UPGRADE_COUNT_AFTER);
        assert!(p.host.counting() && p.client.counting());
    }

    /// The upgrade key on the waiting card reopens the screen. The round waits for both.
    #[test]
    fn the_upgrade_key_on_the_waiting_card_reopens_inside_the_round() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.client.upgrade_leaving(p.t), "its pick: the wait");
        p.deliver();
        p.later(Duration::from_secs(9));
        p.frame(running(), running());
        assert!(p.host.facts(p.t, false).upgrade_counts, "9 s on the wait");
        // A point left: the client's upgrade key on the card.
        assert!(!p.client.upgrade_opened(), "its own: no fade");
        assert!(p.client.upgrade_choosing());
        p.deliver();
        assert!(
            p.host.upgrade.is_waiting_on(CLIENT),
            "the round waits for the client again"
        );
        assert!(
            !p.host.facts(p.t, false).upgrade_counts,
            "the client's wait starts over"
        );
        assert!(p.host.upgrade_leaving(p.t), "held: the client chooses");
        p.deliver();
        assert!(p.host.upgrade.active().is_some());
        assert_eq!(
            p.host.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerChoosing)
        );
        // The client's pick 2 s on ends the round for both (the host waited 2 s).
        p.later(Duration::from_secs(2));
        assert!(p.client.upgrade_leaving(p.t), "the last pick");
        p.deliver();
        assert!(p.host.upgrade.active().is_none() && p.client.upgrade.active().is_none());
        assert!(!p.host.counting() && !p.client.counting());
        let (h, c) = p.frame(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
    }

    /// A shared pause's end counts however short it was.
    #[test]
    fn a_short_pause_still_counts() {
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.frame(paused(), running());
        p.frame(paused(), paused());
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.deliver();
        assert!(p.host.counting() && p.client.counting());
    }

    /// With Ataraxia, the waiting screen for the partner's round. Space opens its own screen.
    #[test]
    fn ataraxia_waits_on_the_pause_screen_and_space_opens_its_own() {
        let mut p = ataraxia_held_pair(cfg());
        assert_eq!(p.host.screen, Screen::Choosing, "the host chooses");
        assert_eq!(p.client.pause.paused_by(), None, "the client's only");
        assert_eq!(
            p.host.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerReady),
            "the partner reads the client as ready"
        );
        assert!(p.client.choose_own());
        assert!(!p.client.choose_own(), "once");
        let (_, c) = p.frame(running(), ataraxia(paused()));
        assert_eq!(c, vec![Action::PressUnpause]);
        let (_, c) = p.frame(running(), ataraxia(running()));
        assert_eq!(c, vec![Action::GotoUpgrade]);
        assert!(!p.client.upgrade_opened(), "asked for: no fade");
        assert!(p.client.upgrade_choosing());
        assert!(p.client.take_outs().is_empty(), "no round of its own");
        // The client picks first and waits on its card. The host's pick ends the round.
        assert!(p.client.upgrade_leaving(p.t));
        p.deliver();
        assert_eq!(
            p.client.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerChoosing)
        );
        assert!(p.host.upgrade_leaving(p.t), "the last choice: the count");
        p.deliver();
        let (h, c) = p.count_out(running(), ataraxia(running()));
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);

        // The host done first: the client's opened screen holds the round until it picks.
        let mut p = ataraxia_held_pair(cfg());
        assert!(p.client.choose_own());
        p.frame(running(), ataraxia(paused()));
        assert!(
            p.host.upgrade.is_waiting_on(CLIENT),
            "the round waits for the client"
        );
        assert!(p.client.upgrade.is_waiting_on(CLIENT));
        p.frame(running(), ataraxia(running()));
        assert!(!p.client.upgrade_opened());
        assert!(p.host.upgrade_leaving(p.t), "held for the client");
        p.deliver();
        assert!(!p.host.counting() && !p.client.counting());
        assert_eq!(
            p.host.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerChoosing)
        );
        assert_eq!(
            p.client.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerReady)
        );
        p.later(Duration::from_secs(30));
        let (h, c) = p.frame(running(), ataraxia(running()));
        assert!(h.is_empty() && c.is_empty(), "both held: {h:?} {c:?}");
        assert!(
            p.client.upgrade_leaving(p.t),
            "the client's pick: the count"
        );
        p.deliver();
        assert!(p.host.counting() && p.client.counting());
        let (h, c) = p.count_out(running(), ataraxia(running()));
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
        // A statement from either side never makes the client done again.
        assert_eq!(p.host.adopted() + p.client.adopted(), 0);

        // The client's Space crosses the host's pick on the wire: its screen does not open.
        let mut p = ataraxia_held_pair(cfg());
        p.later(UPGRADE_COUNT_AFTER);
        assert!(p.client.choose_own());
        let space = p.client.take_outs();
        assert!(
            p.host.upgrade_leaving(p.t),
            "the host's last pick: the count"
        );
        to_host(&mut p, space);
        p.deliver();
        assert!(p.host.counting() && p.host.upgrade.active().is_none());
        assert!(p.client.upgrade.active().is_none(), "told it ended");
        assert!(p.client.counting());
        let (_, c) = p.frame(running(), ataraxia(paused()));
        assert!(c.is_empty(), "no screen: {c:?}");
        let (h, c) = p.count_out(running(), ataraxia(paused()));
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::PressUnpause]);

        // Space after the round ended: nothing.
        let mut p = ataraxia_held_pair(cfg());
        assert!(p.host.upgrade_leaving(p.t));
        p.deliver();
        assert!(!p.client.choose_own());
    }

    /// The round's end reaches the client with its own screen open: its pick
    /// resumes its game with no count, and the host is not held again.
    #[test]
    fn an_ataraxia_space_crossing_the_rounds_end_keeps_the_open_screen() {
        let mut p = ataraxia_held_pair(cfg());
        p.later(UPGRADE_COUNT_AFTER);

        assert!(p.client.choose_own());
        let space = p.client.take_outs();
        let (_, c) = p.frame(running(), ataraxia(paused()));
        assert_eq!(c, vec![Action::PressUnpause]);
        let (_, c) = p.frame(running(), ataraxia(running()));
        assert_eq!(c, vec![Action::GotoUpgrade]);
        assert!(!p.client.upgrade_opened(), "asked for: no fade");
        // The host picks last before the client's Space reaches it.
        assert!(
            p.host.upgrade_leaving(p.t),
            "the host's last pick: the count"
        );
        to_host(&mut p, space);
        p.deliver();
        assert!(p.host.counting() && p.host.upgrade.active().is_none());
        assert!(!p.host.upgrade.is_waiting_on(CLIENT), "not held again");
        assert!(p.client.upgrade.active().is_none(), "told the round ended");
        assert!(p.client.upgrade_choosing(), "its screen still open");
        assert!(!p.client.counting(), "no count over its open screen");
        assert_eq!(p.client.facts(p.t, true).count, None);
        let (h, c) = p.count_out(running(), ataraxia(running()));
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert!(c.is_empty(), "the client's screen is not closed: {c:?}");
        // The client's pick: the game's own leave runs, nothing held.
        assert!(
            !p.client.upgrade_leaving(p.t),
            "the client's pick counts, its game resumes"
        );
        p.deliver();
        assert!(!p.host.upgrade.is_waiting_on(CLIENT) && p.host.upgrade.active().is_none());
        let (h, c) = p.frame(running(), running());
        assert!(h.is_empty() && c.is_empty(), "{h:?} {c:?}");
    }

    /// The Ataraxia hold blocks Esc, however long. A downed player and the
    /// host with Ataraxia wait the same way, and Space opens the screen.
    #[test]
    fn ataraxia_waits_however_long_and_works_down_and_as_host() {
        let mut p = ataraxia_held_pair(cfg());
        p.later(Duration::from_secs(61));
        p.frame(running(), ataraxia(paused()));
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::AtaraxiaHeld)
        );
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        assert!(p.host.upgrade.is_waiting_on(HOST), "the round waits");

        // Down: the same hold, and Space's screen opens past the guard.
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.client.death(p.t), Death::Down);
        p.client.set_control(Some(false));
        p.deliver();
        assert!(!p.host.upgrade_opened());
        p.deliver();
        let (_, c) = p.frame(running(), ataraxia(running()));
        assert!(c.is_empty(), "{c:?}");
        let (_, c) = p.frame(running(), ataraxia(running()));
        assert_eq!(c, vec![Action::PressPause]);
        p.frame(running(), ataraxia(paused()));
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::AtaraxiaHeld)
        );
        assert!(p.client.choose_own());
        p.frame(running(), ataraxia(paused()));
        let (_, c) = p.frame(running(), ataraxia(running()));
        assert_eq!(c, vec![Action::GotoUpgrade]);
        assert!(p.client.lifts_control(), "around the opening");
        assert!(!p.client.upgrade_opened());

        // The host with Ataraxia, the client's round.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        assert!(!p.client.upgrade_opened());
        p.deliver();
        let (h, _) = p.frame(ataraxia(running()), running());
        assert!(h.is_empty(), "{h:?}");
        let (h, _) = p.frame(ataraxia(running()), running());
        assert_eq!(h, vec![Action::PressPause]);
        p.frame(ataraxia(paused()), running());
        assert_eq!(
            p.host.facts(p.t, true).upgrade,
            Some(UpgradeWait::AtaraxiaHeld)
        );
        assert_eq!(
            p.client.screen,
            Screen::Choosing,
            "the client's screen opens"
        );
        // The host's Space: the round waits for it. The client's pick holds the client.
        assert!(p.host.choose_own());
        p.deliver();
        assert!(
            p.client.upgrade.is_waiting_on(HOST),
            "the relay reached the client"
        );
        let (h, _) = p.frame(ataraxia(paused()), running());
        assert_eq!(h, vec![Action::PressUnpause]);
        let (h, _) = p.frame(ataraxia(running()), running());
        assert_eq!(h, vec![Action::GotoUpgrade]);
        assert!(!p.host.upgrade_opened());
        assert!(
            p.client.upgrade_leaving(p.t),
            "the client waits for the host"
        );
        p.deliver();
        assert!(!p.client.counting() && !p.host.counting());
        assert!(p.host.upgrade_leaving(p.t), "its pick: the count");
        p.deliver();
        let (h, c) = p.count_out(ataraxia(running()), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert_eq!(c, vec![Action::ReleaseUpgrade]);
    }
}
