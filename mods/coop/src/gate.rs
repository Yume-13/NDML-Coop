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

//! Run-start gate: each game is held at its first gameplay frame until every
//! peer has arrived, then the host sends `Go` and both run.
//! * [`StartGate`]: the protocol over `ndml_net::session::RunGate`. The
//!   client sends (and repeats) `RunStart { peer, mode }`; the host answers
//!   `Go` or `RunRefused` (game modes differ), then refuses newcomers.
//! * [`Freezer`]: holds the game with the normal pause (`pressPause` as
//!   `oController`), re-pausing if the player unpauses while held.
//! * [`mode_hash`]: the game-mode globals as the `RunStart` hash.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ndml_net::endpoint::RefuseReason;
use ndml_net::msg::{Message, PeerId};
use ndml_net::session::{PeerSet, RunGate, RunGateEvent};

use crate::net::Out;

/// A held client repeats its `RunStart` this often.
pub const RUN_START_REPEAT: Duration = Duration::from_secs(2);

/// Where this game stands at the start gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatePhase {
    /// Not at a gate (menu, or a solo run).
    Idle,
    /// At the first gameplay frame, waiting for the others.
    Holding,
    /// Everyone arrived: the run is on.
    Running,
    /// The game modes differ; the run is refused (still held).
    Refused {
        /// The peer whose mode differs from the host's.
        peer: PeerId,
    },
}

/// See the module docs.
#[derive(Debug, Clone)]
pub struct StartGate {
    run: RunGate,
    phase: GatePhase,
    me: PeerId,
    mode: u32,
    last_sent: Option<Instant>,
    refusing: bool,
}

impl Default for StartGate {
    fn default() -> Self {
        Self {
            run: RunGate::new(PeerSet::first(2)),
            phase: GatePhase::Idle,
            me: PeerId::HOST,
            mode: 0,
            last_sent: None,
            refusing: false,
        }
    }
}

impl StartGate {
    /// The gate phase.
    #[must_use]
    pub fn phase(&self) -> GatePhase {
        self.phase
    }

    /// Whether the game must be held now.
    #[must_use]
    pub fn holding(&self) -> bool {
        matches!(self.phase, GatePhase::Holding | GatePhase::Refused { .. })
    }

    /// Who is expected (this game and its partners).
    pub fn set_peers(&mut self, peers: PeerSet) {
        let ev = self.run.set_expected(peers);
        if self.me.is_host() && self.phase == GatePhase::Holding {
            // Only reached when a partner left: the rest may go.
            if let RunGateEvent::Go = ev {
                self.phase = GatePhase::Running;
            }
        }
    }

    /// At the first gameplay frame of a linked run.
    pub fn arrive(
        &mut self,
        me: PeerId,
        peers: PeerSet,
        mode: u32,
        now: Instant,
        now_ms: u32,
    ) -> Vec<Out> {
        self.me = me;
        self.mode = mode;
        self.phase = GatePhase::Holding;
        self.run.set_expected(peers);
        if me.is_host() {
            let ev = self.run.ready(me, mode);
            self.on_gate(ev, now_ms)
        } else {
            self.last_sent = Some(now);
            vec![Out::Send(
                PeerId::HOST,
                Message::RunStart { peer: me, mode },
            )]
        }
    }

    /// Per frame: a held client repeats its `RunStart`.
    pub fn tick(&mut self, now: Instant) -> Vec<Out> {
        if self.me.is_host() || self.phase != GatePhase::Holding {
            return Vec::new();
        }
        if self
            .last_sent
            .is_some_and(|t| now.duration_since(t) < RUN_START_REPEAT)
        {
            return Vec::new();
        }
        self.last_sent = Some(now);
        vec![Out::Send(
            PeerId::HOST,
            Message::RunStart {
                peer: self.me,
                mode: self.mode,
            },
        )]
    }

    /// Host: a client's `RunStart` arrived (also in the menu, or after `Go`).
    pub fn on_run_start(&mut self, peer: PeerId, mode: u32, now_ms: u32) -> Vec<Out> {
        match self.phase {
            GatePhase::Running if self.me.is_host() => {
                // A repeat that crossed the Go, or a client that restarted.
                vec![Out::Send(peer, Message::Go { start_ms: now_ms })]
            }
            GatePhase::Holding | GatePhase::Refused { .. } => {
                let ev = self.run.ready(peer, mode);
                self.on_gate(ev, now_ms)
            }
            _ => {
                // Kept for this game's arrival.
                let _ = self.run.ready(peer, mode);
                Vec::new()
            }
        }
    }

    fn on_gate(&mut self, ev: RunGateEvent, now_ms: u32) -> Vec<Out> {
        match ev {
            RunGateEvent::Go => {
                self.phase = GatePhase::Running;
                self.refusing = true;
                vec![
                    Out::Broadcast(Message::Go { start_ms: now_ms }),
                    Out::Refusing(Some(RefuseReason::RunInProgress)),
                ]
            }
            RunGateEvent::Mismatch(peer) => {
                if self.phase == (GatePhase::Refused { peer }) {
                    return Vec::new();
                }
                self.phase = GatePhase::Refused { peer };
                vec![Out::Broadcast(Message::RunRefused { peer })]
            }
            RunGateEvent::Waiting(_) => Vec::new(),
        }
    }

    /// Client: the host said go.
    pub fn on_go(&mut self) {
        if self.holding() {
            self.phase = GatePhase::Running;
        }
    }

    /// Client: the host refused the run (modes differ).
    pub fn on_refused(&mut self, peer: PeerId) {
        if self.phase == GatePhase::Holding {
            self.phase = GatePhase::Refused { peer };
        }
    }

    /// Client: the host is back after a lost link in the shared run. A
    /// running game says again that it is in (the host forgot it at the
    /// timeout); a held one repeats on its own ([`Self::tick`]).
    #[must_use]
    pub fn partner_back(&self) -> Vec<Out> {
        if self.me.is_host() || self.phase != GatePhase::Running {
            return Vec::new();
        }
        vec![Out::Send(
            PeerId::HOST,
            Message::RunStart {
                peer: self.me,
                mode: self.mode,
            },
        )]
    }

    /// A partner left (`peers`: who remains, self included).
    pub fn on_peer_left(&mut self, peers: PeerSet) {
        self.set_peers(peers);
        if self.holding() && peers.len() <= 1 {
            self.phase = GatePhase::Running;
        }
    }

    /// The run ended (menu, or a new run without the menu).
    pub fn leave_run(&mut self) -> Vec<Out> {
        self.phase = GatePhase::Idle;
        self.last_sent = None;
        self.run.reset();
        if std::mem::take(&mut self.refusing) {
            vec![Out::Refusing(None)]
        } else {
            Vec::new()
        }
    }
}

/// What the freezer wants done to the game this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeAction {
    /// Nothing.
    None,
    /// Call `pressPause` (as `oController`).
    Pause,
    /// Call `pressUnpause` (as `oController`).
    Unpause,
}

/// Frames between two `pressPause` calls while held (the pause is deferred).
pub const PAUSE_RETRY_FRAMES: u64 = 30;

/// Holds the game paused while the gate holds; releases only a pause it made.
#[derive(Debug, Clone, Default)]
pub struct Freezer {
    made_pause: bool,
    last_call: Option<u64>,
}

impl Freezer {
    /// Decide for this frame. `paused` is `oController.pause != 0`.
    pub fn decide(&mut self, hold: bool, paused: bool, frame: u64) -> FreezeAction {
        if hold {
            if paused {
                return FreezeAction::None;
            }
            if self
                .last_call
                .is_some_and(|f| frame.saturating_sub(f) < PAUSE_RETRY_FRAMES)
            {
                return FreezeAction::None;
            }
            self.made_pause = true;
            self.last_call = Some(frame);
            return FreezeAction::Pause;
        }
        self.last_call = None;
        if std::mem::take(&mut self.made_pause) && paused {
            return FreezeAction::Unpause;
        }
        FreezeAction::None
    }

    /// Forget everything (a new run).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Keep its pause as a player's pause; `true` if there was one.
    pub fn adopt(&mut self) -> bool {
        self.last_call = None;
        std::mem::take(&mut self.made_pause)
    }
}

/// FNV-1a over the mode fields: each name and its value rounded to 1e-4
/// (`None` = the global is missing, hashed as its own marker).
#[must_use]
pub fn mode_hash(fields: &[(&str, Option<f64>)]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
    };
    for (name, v) in fields {
        eat(name.as_bytes());
        eat(b"=");
        match v {
            Some(v) if v.is_finite() => {
                let q = (v * 10_000.0).round() as i64;
                eat(&q.to_le_bytes());
            }
            _ => eat(b"?"),
        }
        eat(b";");
    }
    h
}

/// The mode fields as a log line (to compare two players' logs).
#[must_use]
pub fn mode_summary(fields: &[(&str, Option<f64>)]) -> String {
    fields
        .iter()
        .map(|(n, v)| match v {
            Some(v) => format!("{n}={v}"),
            None => format!("{n}=?"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: PeerId = PeerId::HOST;
    const CLIENT: PeerId = PeerId(1);

    fn two() -> PeerSet {
        PeerSet::first(2)
    }

    fn has_go(outs: &[Out]) -> bool {
        outs.iter()
            .any(|o| matches!(o, Out::Broadcast(Message::Go { .. })))
    }

    #[test]
    fn host_holds_until_the_client_arrives_then_goes() {
        let t = Instant::now();
        let mut g = StartGate::default();
        let outs = g.arrive(HOST, two(), 7, t, 100);
        assert!(outs.is_empty());
        assert!(g.holding());
        let outs = g.on_run_start(CLIENT, 7, 200);
        assert!(has_go(&outs));
        assert!(
            outs.contains(&Out::Refusing(Some(RefuseReason::RunInProgress))),
            "{outs:?}"
        );
        assert_eq!(g.phase(), GatePhase::Running);
        assert!(!g.holding());
        // A late repeat is answered with Go to that peer.
        let outs = g.on_run_start(CLIENT, 7, 300);
        assert_eq!(outs, vec![Out::Send(CLIENT, Message::Go { start_ms: 300 })]);
        // Leaving the run lets newcomers in again.
        assert_eq!(g.leave_run(), vec![Out::Refusing(None)]);
        assert_eq!(g.phase(), GatePhase::Idle);
    }

    #[test]
    fn a_client_that_arrives_first_is_remembered_by_the_host() {
        let t = Instant::now();
        let mut g = StartGate::default();
        assert!(
            g.on_run_start(CLIENT, 7, 50).is_empty(),
            "host still in the menu"
        );
        let outs = g.arrive(HOST, two(), 7, t, 100);
        assert!(has_go(&outs));
    }

    #[test]
    fn mismatched_modes_are_refused_once() {
        let t = Instant::now();
        let mut g = StartGate::default();
        g.arrive(HOST, two(), 7, t, 0);
        let outs = g.on_run_start(CLIENT, 8, 0);
        assert_eq!(
            outs,
            vec![Out::Broadcast(Message::RunRefused { peer: CLIENT })]
        );
        assert_eq!(g.phase(), GatePhase::Refused { peer: CLIENT });
        assert!(g.holding());
        assert!(g.on_run_start(CLIENT, 8, 0).is_empty(), "no repeat spam");
        // The partner leaves: play on alone.
        g.on_peer_left(PeerSet::first(1));
        assert_eq!(g.phase(), GatePhase::Running);
    }

    #[test]
    fn client_sends_run_start_repeats_and_releases_on_go() {
        let t = Instant::now();
        let mut g = StartGate::default();
        let outs = g.arrive(CLIENT, two(), 9, t, 0);
        assert_eq!(
            outs,
            vec![Out::Send(
                HOST,
                Message::RunStart {
                    peer: CLIENT,
                    mode: 9
                }
            )]
        );
        assert!(g.tick(t + Duration::from_millis(500)).is_empty());
        assert_eq!(g.tick(t + RUN_START_REPEAT).len(), 1);
        g.on_go();
        assert_eq!(g.phase(), GatePhase::Running);
        assert!(g.tick(t + RUN_START_REPEAT * 3).is_empty());
        assert!(g.leave_run().is_empty(), "a client never refused anyone");
    }

    #[test]
    fn client_refused_and_host_leaving_releases() {
        let t = Instant::now();
        let mut g = StartGate::default();
        g.arrive(CLIENT, two(), 9, t, 0);
        g.on_refused(CLIENT);
        assert_eq!(g.phase(), GatePhase::Refused { peer: CLIENT });
        g.on_peer_left(PeerSet::first(2).minus(PeerSet::first(1)));
        assert_eq!(g.phase(), GatePhase::Running);
    }

    #[test]
    fn host_waiting_alone_is_released_when_the_partner_leaves() {
        let t = Instant::now();
        let mut g = StartGate::default();
        g.arrive(HOST, two(), 1, t, 0);
        g.on_peer_left(PeerSet::first(1));
        assert_eq!(g.phase(), GatePhase::Running);
    }

    #[test]
    fn freezer_pauses_retries_and_releases_only_its_own_pause() {
        let mut f = Freezer::default();
        assert_eq!(f.decide(true, false, 0), FreezeAction::Pause);
        assert_eq!(f.decide(true, false, 1), FreezeAction::None, "deferred");
        assert_eq!(f.decide(true, true, 5), FreezeAction::None);
        // The player unpaused by hand: pause again after the retry delay.
        assert_eq!(f.decide(true, false, 10), FreezeAction::None);
        assert_eq!(
            f.decide(true, false, PAUSE_RETRY_FRAMES),
            FreezeAction::Pause
        );
        assert_eq!(f.decide(false, true, 40), FreezeAction::Unpause);
        assert_eq!(f.decide(false, true, 41), FreezeAction::None, "once");
        // A pause the player made is never undone.
        let mut f = Freezer::default();
        assert_eq!(f.decide(false, true, 0), FreezeAction::None);
    }

    #[test]
    fn mode_hash_tracks_values_and_missing_globals() {
        let a = [
            ("gameModEndless", Some(1.0)),
            ("baseEnemyHullFactor", Some(1.0)),
        ];
        let b = [
            ("gameModEndless", Some(0.0)),
            ("baseEnemyHullFactor", Some(1.0)),
        ];
        let c = [("gameModEndless", None), ("baseEnemyHullFactor", Some(1.0))];
        let tiny = [
            ("gameModEndless", Some(1.000_000_01)),
            ("baseEnemyHullFactor", Some(1.0)),
        ];
        assert_ne!(mode_hash(&a), mode_hash(&b));
        assert_ne!(mode_hash(&a), mode_hash(&c));
        assert_eq!(mode_hash(&a), mode_hash(&tiny), "rounded to 1e-4");
        assert_eq!(mode_summary(&c), "gameModEndless=? baseEnemyHullFactor=1");
    }
}
