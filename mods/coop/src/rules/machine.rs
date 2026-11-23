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

use super::score::{ClientScore, HostScore, Score};
use super::settings::RulesSettings;
use super::xp::{ClientXp, HostXp, XpRead, XpStep};
use crate::net::Out;
use crate::ui::run::{SessionEvent, SessionFacts, SharedPause, UpgradeWait};

mod state_sync;

/// One game frame (the run tick runs at 60 steps per second).
pub const FRAME: Duration = Duration::from_nanos(16_666_667);
/// Frames a pause or unpause the mod made itself stays expected.
const OWN_WINDOW: u64 = 15;
/// Frames between two calls of the same pause action.
const PRESS_EVERY: u64 = 30;
/// Frames a forced `gotoUpgrade` may take to open the screen.
const GOTO_WAIT: u64 = 20;
/// Frames a release (`leaveUpgrade`) may take before it is called again.
const RELEASE_RETRY: u64 = 20;
/// Retries between two warn lines about a release that does not take.
const RELEASE_WARN_EVERY: u32 = 90;
/// Frames a client waits for mirrored levels before it has nothing to choose.
const OWED_WAIT: u64 = 90;
/// A client re-sends an unconfirmed downed report this often.
const DOWN_RESEND: Duration = Duration::from_secs(2);
/// How far behind the partner a respawned ship appears, pixels.
pub const RESPAWN_OFFSET: f64 = 140.0;

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
    /// The partner's ship: `x`, `y`, `vx`, `vy` (for the respawn point).
    pub partner: Option<(f64, f64, f64, f64)>,
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
    /// Visible again, brief `iFrames`, moved here (`None`: where it is).
    Revive(Option<(f64, f64)>),
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
    /// Respawning: position resolved at the next frame, then `oPowers`, then `oPlayer`.
    Reviving {
        pos: Option<(f64, f64)>,
        resolved: bool,
        powers_done: bool,
    },
    /// Everyone is down: the forced death was (or is to be) applied.
    Dying {
        applied: bool,
    },
}

/// The point [`RESPAWN_OFFSET`] behind the partner's motion (to its left when
/// it hardly moves); the game's `wrapPlayer` wraps a point past the edge.
#[must_use]
pub fn respawn_point(partner: (f64, f64, f64, f64)) -> (f64, f64) {
    let (px, py, vx, vy) = partner;
    let speed = vx.hypot(vy);
    let (dx, dy) = if speed > 1.0 {
        (-vx / speed, -vy / speed)
    } else {
        (-1.0, 0.0)
    };
    (px + dx * RESPAWN_OFFSET, py + dy * RESPAWN_OFFSET)
}

fn wire_ms(d: Duration) -> u32 {
    ndml_net::time::to_wire_ms(d)
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
    escaped: Option<u16>,
    wait_since: Option<Instant>,
    goto_sent: Option<u64>,
    owed_since: Option<u64>,
    // Downed.
    respawn: RespawnRules,
    down: Ship,
    force_death: bool,
    game_over_seen: bool,
    /// A respawn timed by this game alone (nobody confirmed its report).
    solo_respawn: Option<Duration>,
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
    outs: Vec<Out>,
    events: Vec<SessionEvent>,
    /// Converging from the partner's statement (`state_sync`).
    sync: state_sync::Sync,
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
            escaped: None,
            wait_since: None,
            goto_sent: None,
            owed_since: None,
            respawn: RespawnRules::new(PeerSet::EMPTY, respawn),
            down: Ship::Up,
            force_death: false,
            game_over_seen: false,
            solo_respawn: None,
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
            outs: Vec::new(),
            events: Vec::new(),
            sync: state_sync::Sync::default(),
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

    /// The partner left this run for good: each game plays alone until it ends.
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

    /// Game time of this run (advances only while the world runs).
    #[must_use]
    pub fn game_time(&self) -> Duration {
        self.game_time
    }

    /// `oPowers.playerControl` from this frame.
    pub fn set_control(&mut self, control: Option<bool>) {
        self.control = control;
    }

    /// The session's upgrade wait (the host's when sent, else the local one).
    pub fn set_upgrade_wait(&mut self, upgrade_wait: Option<Duration>) {
        self.cfg.upgrade_wait = upgrade_wait;
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
        self.respawn.remove_peer(peer);
        if interrupted {
            self.interrupted_at = Some(now);
        } else {
            self.separated = true;
        }
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
        self.escaped = None;
    }

    /// A partner came back: rounds held apart are dropped. Whoever is still
    /// on their pause menu pauses everyone again; the host announces its open
    /// upgrade round, and a joining game on its screen proposes one.
    fn rejoin_rounds(&mut self, now: Instant) {
        let on_menu = self.pause.holds(self.me) && !self.close_sent;
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
            if let Some(r) = self.upgrade.active() {
                log::info!(
                    "co-op rules: upgrade round {r} is open here; telling the returning game"
                );
                self.bcast(Message::UpgradeOpen { round: r });
            }
        } else {
            // The host's numbering from here: a gate with no history.
            self.upgrade = UpgradeGate::new(self.peers);
            self.proposed = None;
            self.escaped = None;
            self.done_sent = None;
            if self.screen == Screen::Choosing {
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
        self.pause.set_peers(self.peers);
        let ev = self.upgrade.set_peers(self.peers);
        self.on_upgrade(ev);
        self.rejoin_rounds(now);
        self.respawn.add_peer(peer);
        self.host_xp.resend();
        self.host_score.resend();
        log::info!("co-op rules: {peer} is back");
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
            Message::UpgradeDone { peer, round } => self.on_upgrade_done(peer, round),
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

    fn on_upgrade_done(&mut self, peer: PeerId, round: u16) {
        if !self.active {
            return;
        }
        let round = if self.is_host {
            let Some(r) = self.upgrade.resolve_done(round) else {
                return;
            };
            self.bcast(Message::UpgradeDone { peer, round: r });
            r
        } else {
            round
        };
        let ev = self.upgrade.done(peer, round);
        self.on_upgrade(ev);
    }

    fn on_upgrade(&mut self, ev: UpgradeEvent) {
        let mut ev = ev;
        loop {
            match ev {
                UpgradeEvent::Opened(r) => {
                    log::info!("co-op rules: upgrade round {r} open");
                    self.goto_sent = None;
                    self.owed_since = None;
                    self.escaped = None;
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
                        });
                    }
                    return;
                }
                UpgradeEvent::Released(r) => {
                    log::info!("co-op rules: upgrade round {r} released");
                    self.wait_since = None;
                    self.proposed = None;
                    match self.upgrade.next_queued() {
                        Some(next) => ev = next,
                        None => return,
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
        if !self.game_over_seen {
            self.game_over_seen = true;
            self.events.push(SessionEvent::AllDown);
        }
        self.force_death = true;
        if matches!(self.down, Ship::Down { .. } | Ship::Reviving { .. }) {
            self.down = Ship::Dying { applied: false };
        }
    }

    fn respawned(&mut self, peer: PeerId) {
        if peer == self.me {
            if !matches!(self.down, Ship::Down { .. }) {
                // Up already: a second revive would teleport a flying ship.
                log::info!("co-op rules: told this ship respawned, but it is not down; ignored");
                return;
            }
            log::info!("co-op rules: respawning");
            self.down = Ship::Reviving {
                pos: None,
                resolved: false,
                powers_done: false,
            };
            self.events.push(SessionEvent::Back);
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
            self.bcast(Message::UpgradeDone {
                peer: self.me,
                round: r,
            });
            let ev = self.upgrade.done(self.me, r);
            self.on_upgrade(ev);
        } else {
            self.send_host(Message::UpgradeDone {
                peer: self.me,
                round,
            });
        }
    }

    // ---- hooks --------------------------------------------------------------

    /// The link wait's pause becomes this player's (`crate::wait`): the shared
    /// pause opens. Call after [`Self::peer_joined`].
    pub fn adopt_pause(&mut self, now: Instant) {
        if !self.active || self.separated || self.i_hold() {
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
        if !self.active || self.screen != Screen::Idle {
            return UnpauseCall::Allow;
        }
        if self.pause.is_paused() || self.pause_request.is_some() {
            if self.i_hold() {
                // This player's Resume: out of the round; the last one out resumes.
                let others = self.others_hold();
                self.local_unpause();
                if !others {
                    return UnpauseCall::Allow;
                }
            }
            return UnpauseCall::Block;
        }
        if self.hold_for_round() {
            if self.escape_offered(now) {
                self.escape(now);
                return UnpauseCall::Allow;
            }
            return UnpauseCall::Block;
        }
        UnpauseCall::Allow
    }

    /// `gotoUpgradeDeferred` ran: the upgrade screen (or a chained one) opened.
    pub fn upgrade_opened(&mut self) {
        if !self.active || self.screen == Screen::Choosing {
            return;
        }
        self.screen = Screen::Choosing;
        let expected = self.goto_sent.take().is_some();
        if expected || self.respawn.is_game_over() {
            return;
        }
        if self.upgrade.active().is_some() && self.upgrade.is_waiting_on(self.me) {
            return; // choosing for the open round
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
    }

    /// `leaveUpgradeDeferred` is about to run. Returns true to skip it: the
    /// world stays frozen until the round is released.
    pub fn upgrade_leaving(&mut self, now: Instant) -> bool {
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
            self.wait_since = Some(now);
            log::info!("co-op rules: chose; holding the upgrade screen for the partner");
            true
        } else {
            false
        }
    }

    /// The ship would die now (`checkDestroyPlayer`, no Last Stand left).
    /// Solo or alone: the game's own death.
    pub fn death(&mut self, now: Instant) -> Death {
        if !self.active || self.force_death || self.respawn.is_game_over() {
            return Death::Die;
        }
        if self.is_down() {
            return Death::Down;
        }
        if self.alone() {
            return Death::Die;
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

    /// What `oPowers_Step_0` does this frame.
    #[must_use]
    pub fn powers_action(&self) -> PowersAct {
        match self.down {
            Ship::Down { .. } => PowersAct::HoldDown,
            Ship::Reviving {
                resolved: true,
                powers_done: false,
                ..
            } => PowersAct::Revive,
            Ship::Dying { applied: false } => PowersAct::ForceDeath,
            Ship::Reviving { .. } | Ship::Up | Ship::Dying { applied: true } => PowersAct::None,
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

    /// What `oPlayer_Step_0` does this frame.
    #[must_use]
    pub fn player_action(&self) -> PlayerAct {
        match self.down {
            Ship::Down { .. }
            | Ship::Reviving {
                powers_done: false, ..
            } => PlayerAct::HoldDown,
            Ship::Reviving { pos, .. } => PlayerAct::Revive(pos),
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
        self.upgrade.active().filter(|r| self.escaped != Some(*r))
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

    fn escape_offered(&self, now: Instant) -> bool {
        self.cfg.upgrade_wait.is_some_and(|w| {
            self.wait_since
                .is_some_and(|t| now.saturating_duration_since(t) >= w)
        })
    }

    /// Held on the upgrade screen and Esc would stop waiting (the run tick
    /// polls the back key then).
    #[must_use]
    pub fn held_escape_offered(&self, now: Instant) -> bool {
        self.screen == Screen::Held && self.others_choosing() && self.escape_offered(now)
    }

    /// Stop waiting for the partners still choosing: mark them done for this
    /// round (they keep choosing; their game resumes when they finish).
    pub fn escape(&mut self, _now: Instant) {
        let Some(r) = self.open_round() else { return };
        log::info!("co-op rules: stopped waiting for upgrade round {r}");
        let waiting: Vec<PeerId> = self
            .upgrade
            .waiting_on()
            .minus(one(self.me))
            .iter()
            .collect();
        for p in waiting {
            if self.seq() {
                self.bcast(Message::UpgradeDone { peer: p, round: r });
                let ev = self.upgrade.done(p, r);
                self.on_upgrade(ev);
            } else {
                self.send_host(Message::UpgradeDone { peer: p, round: r });
            }
        }
        self.escaped = Some(r);
        self.wait_since = None;
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
        self.frame += 1;
        let f = self.frame;
        let mut actions = Vec::new();
        // Stated at the game over too (a joining game that missed it).
        self.state_tick();
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
        self.downed_tick(o, now);
        self.xp_tick(o, running, now, &mut actions);
        self.score_tick(o, running, now, &mut actions);
        self.direct(o, now, &mut actions);
        actions
    }

    fn downed_tick(&mut self, o: &Observed, now: Instant) {
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
        }
        if self.solo_respawn.is_some_and(|at| self.game_time >= at) {
            self.solo_respawn = None;
            if matches!(self.down, Ship::Down { .. }) {
                self.respawned(self.me);
            }
        }
        if let Ship::Reviving { pos, resolved, .. } = &mut self.down
            && !*resolved
        {
            *pos = o.partner.map(respawn_point);
            *resolved = true;
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

    fn direct(&mut self, o: &Observed, now: Instant, actions: &mut Vec<Action>) {
        if self.direct_screen(now, actions) {
            return;
        }
        let f = self.frame;
        let mine = self.i_hold();
        if let Some(r) = self.open_round().filter(|_| self.i_owe()) {
            self.direct_choose(r, o, mine, actions);
            return;
        }
        let hold_round = self.hold_for_round();
        if hold_round && self.wait_since.is_none() {
            self.wait_since = Some(now);
        }
        // The shared pause holds this game while anyone is on a pause menu,
        // except while only its own Resume awaits the host's relay.
        let held = self.pause.is_paused() && !self.only_my_close_pending();
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
        } else if o.paused && self.mod_paused && !mine && self.press_ok(false) {
            self.own_unpause = Some(f);
            actions.push(Action::PressUnpause);
        }
    }

    /// The upgrade screen's own states. True when it decided this frame.
    fn direct_screen(&mut self, now: Instant, actions: &mut Vec<Action>) -> bool {
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
                if self.others_choosing() || self.pending_open() || self.others_hold() {
                    if self.wait_since.is_none() {
                        self.wait_since = Some(now);
                    }
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
                    self.wait_since = None;
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

    /// A round is open and this player still has to choose.
    fn direct_choose(&mut self, r: u16, o: &Observed, mine: bool, actions: &mut Vec<Action>) {
        let f = self.frame;
        if self.is_down() {
            self.local_done(r);
            return;
        }
        if self.client_xp.owed() > 0 {
            let since = *self.owed_since.get_or_insert(f);
            if f.saturating_sub(since) < OWED_WAIT {
                return;
            }
        }
        let can = o.levelup.is_some_and(|l| l >= 1.0) && self.control != Some(false);
        if !can {
            log::info!("co-op rules: nothing to choose in round {r}; waiting for the partner");
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
            if !mine && self.press_ok(false) {
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
        let long = self.escape_offered(now);
        s.upgrade = match self.screen {
            Screen::Held if self.others_choosing() || self.pending_open() => Some(if long {
                UpgradeWait::PartnerChoosingLong
            } else {
                UpgradeWait::PartnerChoosing
            }),
            Screen::Choosing if self.open_round().is_some() && !self.others_choosing() => {
                Some(UpgradeWait::PartnerReady)
            }
            Screen::Idle if self.hold_for_round() => Some(if long {
                UpgradeWait::HeldPausedLong
            } else {
                UpgradeWait::HeldPaused
            }),
            Screen::Idle if self.i_owe() && paused && self.i_hold() => {
                Some(UpgradeWait::ResumeToChoose)
            }
            _ => None,
        };
        if self.respawn.is_game_over() {
            return s;
        }
        let gt = self.game_time;
        if matches!(self.down, Ship::Down { .. }) {
            let left = self
                .respawn
                .remaining(self.me, gt)
                .or_else(|| self.solo_respawn.map(|at| at.saturating_sub(gt)))
                .unwrap_or(self.cfg.respawn);
            s.me_down = Some(ceil_secs(left));
            s.me_down_alone = self.alone();
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
        // The host resumes last: its game resumes, the client is resumed.
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Allow);
        p.deliver();
        assert!(!p.host.pause.is_paused() && !p.client.pause.is_paused());
        let (h, c) = p.frame(running(), paused());
        assert!(h.is_empty(), "the host's own Resume was the last one");
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
        // The client resumes through its game's own Resume (the flag falls).
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);
        p.frame(paused(), running());
        assert!(!p.host.pause.is_paused(), "released");
        let (h, _) = p.frame(paused(), running());
        assert_eq!(h, vec![Action::PressUnpause]);
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
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);
        p.frame(paused(), running());
        assert!(!p.host.pause.is_paused() && !p.client.pause.is_paused());
        let (h, _) = p.frame(paused(), running());
        assert_eq!(h, vec![Action::PressUnpause]);
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
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);
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

    /// The joining game resuming last is not paused again while its Resume
    /// waits for the host's relay.
    #[test]
    fn a_client_resuming_last_is_not_paused_again_before_the_relay() {
        let mut p = Pair::new(cfg());
        let in_flight = client_resumes_last(&mut p);
        for _ in 0..5 {
            let c = p.client.frame(&running(), p.t);
            assert!(!c.contains(&Action::PressPause), "paused again: {c:?}");
        }
        // The relay: nothing changes here; the host's game resumes.
        to_host(&mut p, in_flight);
        p.deliver();
        assert!(!p.client.close_sent);
        assert!(!p.client.pause.is_paused() && !p.host.pause.is_paused());
        let c = p.client.frame(&running(), p.t);
        assert!(c.is_empty(), "{c:?}");
        let h = p.host.frame(&paused(), p.t);
        assert_eq!(h, vec![Action::PressUnpause]);
    }

    #[test]
    fn the_hosts_player_pausing_again_while_the_clients_resume_is_in_flight_pauses_the_client() {
        let mut p = Pair::new(cfg());
        let in_flight = client_resumes_last(&mut p);
        assert!(
            !p.client
                .frame(&running(), p.t)
                .contains(&Action::PressPause)
        );
        // The host takes the client's Resume, resumes and pauses again.
        to_host(&mut p, in_flight);
        let h = p.host.frame(&paused(), p.t);
        assert_eq!(h, vec![Action::PressUnpause]);
        p.host.frame(&running(), p.t);
        p.host.frame(&paused(), p.t);
        assert!(p.host.pause.holds(HOST));
        p.deliver();
        // A new round: everyone is in it (each game shows its pause menu).
        assert!(p.client.pause.holds(HOST) && p.client.pause.holds(CLIENT));
        assert!(!p.client.close_sent);
        let c = p.client.frame(&running(), p.t);
        assert_eq!(
            c,
            vec![Action::PressPause],
            "the new round pauses the client"
        );
        // The host's pause arrives with the client's Resume unanswered: held.
        let mut q = Pair::new(cfg());
        let in_flight = client_resumes_last(&mut q);
        q.client.on_message(HOST, &Message::Pause { by: HOST }, q.t);
        assert!(q.client.pause.holds(HOST) && q.client.close_sent);
        let c = q.client.frame(&running(), q.t);
        assert_eq!(c, vec![Action::PressPause]);
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
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);
        p.frame(paused(), running());
        assert!(!p.host.pause.is_paused());
        let (h, _) = p.frame(paused(), running());
        assert_eq!(h, vec![Action::PressUnpause]);
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
        // The client chooses: not held, the round releases, the host leaves.
        assert!(!p.client.upgrade_leaving(p.t));
        p.deliver();
        assert_eq!(p.host.upgrade.last_released(), Some(1));
        let (h, _) = p.frame(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
        assert!(
            !p.host.upgrade_leaving(p.t),
            "the host's release goes through"
        );
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
        assert!(!p.host.upgrade_leaving(p.t));
        p.deliver();
        let (_, c) = p.frame(running(), running());
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
        // After the wait, Esc plays on without the host.
        p.later(Duration::from_secs(61));
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::HeldPausedLong)
        );
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);
        p.deliver();
        assert!(
            !p.host.upgrade.is_waiting_on(HOST),
            "the host was marked done"
        );
        // The host is still on its screen; when it finishes it is not held.
        p.host.upgrade_opened();
        assert!(!p.host.upgrade_leaving(p.t));
    }

    #[test]
    fn held_upgrade_screen_offers_esc_after_the_wait() {
        let mut p = Pair::new(cfg());
        p.host.upgrade_opened();
        p.deliver();
        assert!(p.host.upgrade_leaving(p.t));
        assert!(!p.host.held_escape_offered(p.t));
        p.later(Duration::from_secs(61));
        assert!(p.host.held_escape_offered(p.t));
        assert_eq!(
            p.host.facts(p.t, false).upgrade,
            Some(UpgradeWait::PartnerChoosingLong)
        );
        p.host.escape(p.t);
        p.deliver();
        let (h, _) = p.frame(running(), running());
        assert_eq!(h, vec![Action::ReleaseUpgrade]);
    }

    /// On the held card the pause key moves the game to its pause screen,
    /// with any wait setting.
    #[test]
    fn held_on_the_card_the_pause_key_waits_on_the_pause_screen() {
        let never = RulesSettings {
            upgrade_wait: None,
            ..cfg()
        };
        // The partner idles on its upgrade screen; the wait is "never".
        let mut p = Pair::new(never);
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
        assert!(!p.client.held_escape_offered(p.t));
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
        // Resume and Esc are held there (no wait set).
        p.later(Duration::from_secs(600));
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        // The host chooses: the client's game resumes with the round.
        assert!(!p.host.upgrade_leaving(p.t));
        p.deliver();
        let (_, c) = p.frame(running(), paused());
        assert_eq!(c, vec![Action::PressUnpause]);

        // With the default wait, Play On (Esc) is offered there after it.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.client.upgrade_leaving(p.t));
        p.client.pause_key_on_card();
        p.frame(running(), running());
        assert!(!p.client.upgrade_leaving(p.t));
        p.frame(running(), running());
        p.frame(running(), paused());
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Block);
        p.later(Duration::from_secs(61));
        p.frame(running(), paused());
        assert_eq!(
            p.client.facts(p.t, true).upgrade,
            Some(UpgradeWait::HeldPausedLong)
        );
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);

        // Held only by the partner's pause menu: the key moves the client to
        // the shared pause's screen.
        let mut p = Pair::new(cfg());
        p.frame(running(), running());
        p.host.upgrade_opened();
        p.deliver();
        p.frame(running(), running());
        p.client.upgrade_opened();
        assert!(p.client.upgrade_leaving(p.t));
        p.deliver();
        // The host chooses and pauses before the client's card is released.
        assert!(!p.host.upgrade_leaving(p.t));
        p.host.frame(&paused(), p.t);
        p.deliver();
        p.later(Duration::from_secs(600));
        p.frame(paused(), running());
        assert_eq!(p.client.screen, Screen::Held);
        assert!(!p.client.held_escape_offered(p.t));
        p.client.pause_key_on_card();
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
        assert_eq!(p.host.unpause_request(false, p.t), UnpauseCall::Allow);
        p.deliver();
        let (h, c) = p.frame(running(), paused());
        assert!(h.is_empty());
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
        assert!(!p.host.upgrade_leaving(p.t), "the host is not held");
        p.deliver();
        let (_, c) = p.frame(running(), running());
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
        assert!(!p.host.upgrade_leaving(p.t));
        p.deliver();
        let (_, c) = p.frame(running(), running());
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
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);
        p.deliver();
        let (h, _) = p.frame(paused(), running());
        assert_eq!(h, vec![Action::PressUnpause], "both resumed: play on");
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
        assert_eq!(p.client.unpause_request(false, p.t), UnpauseCall::Allow);
        let in_flight = p.client.take_outs();
        for _ in 0..5 {
            let c = p.client.frame(&running(), p.t);
            assert!(!c.contains(&Action::PressPause), "paused again: {c:?}");
        }
        to_host(&mut p, in_flight);
        p.deliver();
        let h = p.host.frame(&paused(), p.t);
        assert_eq!(h, vec![Action::PressUnpause]);
        assert!(!p.client.pause.is_paused() && !p.client.close_sent);
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
        assert_eq!(c, vec![Action::PressUnpause], "the client resumes");
        let c = p.client.frame(&running(), p.t);
        assert!(!c.contains(&Action::PressPause), "{c:?}");
        // The client's reaches the host: released there; its relay changes
        // nothing.
        to_host(&mut p, client_outs);
        let h = p.host.frame(&paused(), p.t);
        assert_eq!(h, vec![Action::PressUnpause]);
        p.deliver();
        let c = p.client.frame(&running(), p.t);
        assert!(c.is_empty(), "{c:?}");
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
    fn client_goes_down_and_respawns_next_to_the_host() {
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
        let host_pos = Observed {
            partner: Some((500.0, 300.0, 4.0, 0.0)),
            ..running()
        };
        for _ in 0..602 {
            p.frame(running(), host_pos);
        }
        assert_eq!(p.client.powers_action(), PowersAct::Revive);
        p.client.powers_applied();
        let act = p.client.player_action();
        assert!(
            matches!(act, PlayerAct::Revive(Some((x, y)))
                if (x - (500.0 - RESPAWN_OFFSET)).abs() < 1e-6 && (y - 300.0).abs() < 1e-6),
            "{act:?}"
        );
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

    #[test]
    fn host_down_while_client_alive_and_downed_player_skips_rounds() {
        let mut p = Pair::new(cfg());
        run_frames(&mut p, 2);
        assert_eq!(p.host.death(p.t), Death::Down);
        p.deliver();
        assert!(p.client.respawn.is_down(HOST));
        // The client opens a round: the downed host reports done and waits.
        p.client.upgrade_opened();
        p.deliver();
        let (h, _) = p.frame(running(), running());
        assert!(h.is_empty());
        assert!(
            !p.host.upgrade.is_waiting_on(HOST),
            "a downed player is not waited for"
        );
        let (h, _) = p.frame(running(), running());
        assert_eq!(h, vec![Action::PressPause], "held on the pause screen");
    }

    #[test]
    fn last_partner_gone_while_down_respawns_on_our_own_timer() {
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
        // Alone, a death is a death.
        p.client.powers_applied();
        p.client.player_applied();
        assert_eq!(p.client.death(p.t), Death::Die);
    }

    /// A down unconfirmed across a link wait hands the time left to the host
    /// and revives once; the host never has it down while it flies.
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
        // The host going down now is not a game over.
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

    /// Never linked (a solo game with the mod installed): the game's own
    /// death, every time.
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

    #[test]
    fn respawn_point_is_behind_the_partner() {
        // Past the edge: left as it is (the game wraps the ship).
        let (x, y) = respawn_point((50.0, 400.0, 10.0, 0.0));
        assert!((x - (50.0 - RESPAWN_OFFSET)).abs() < 1e-6);
        assert!((y - 400.0).abs() < 1e-6);
        let (x, _) = respawn_point((500.0, 400.0, 0.0, 0.0));
        assert!(
            (x - (500.0 - RESPAWN_OFFSET)).abs() < 1e-6,
            "still: to the left"
        );
    }
}
