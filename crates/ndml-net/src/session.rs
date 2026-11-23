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

//! Co-op session rules as pure state machines. All of them work for any
//! number of peers (up to 32).
//!
//! The host is the sequencer: clients send requests, the host applies them and
//! relays every accepted event, and clients apply only relayed events, so all
//! peers reach the same state. [`RespawnRules`] takes game time.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use crate::msg::PeerId;

/// A set of peers (ids 0..=31) as a bitmask.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PeerSet(u32);

impl PeerSet {
    /// No peers.
    pub const EMPTY: Self = Self(0);

    /// Peers `0..n`.
    #[must_use]
    pub fn first(n: u8) -> Self {
        Self(1u32.checked_shl(u32::from(n)).map_or(u32::MAX, |b| b - 1))
    }

    fn bit(p: PeerId) -> u32 {
        1u32.checked_shl(u32::from(p.0)).unwrap_or(0)
    }

    /// Add a peer (ids above 31 are ignored).
    pub fn insert(&mut self, p: PeerId) {
        self.0 |= Self::bit(p);
    }

    /// Remove a peer.
    pub fn remove(&mut self, p: PeerId) {
        self.0 &= !Self::bit(p);
    }

    /// Membership.
    #[must_use]
    pub fn contains(self, p: PeerId) -> bool {
        Self::bit(p) != 0 && self.0 & Self::bit(p) != 0
    }

    /// Number of peers.
    #[must_use]
    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every peer of `other` is in `self`.
    #[must_use]
    pub fn contains_all(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Peers in `self` but not in `other`.
    #[must_use]
    pub fn minus(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// The mask (bit i: peer i), for the wire.
    #[must_use]
    pub fn bits(self) -> u32 {
        self.0
    }

    /// A set from its mask.
    #[must_use]
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Members in id order.
    pub fn iter(self) -> impl Iterator<Item = PeerId> {
        (0..32u8).map(PeerId).filter(move |p| self.contains(*p))
    }
}

impl fmt::Debug for PeerSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter().map(|p| p.0)).finish()
    }
}

impl FromIterator<PeerId> for PeerSet {
    fn from_iter<I: IntoIterator<Item = PeerId>>(iter: I) -> Self {
        let mut s = Self::EMPTY;
        for p in iter {
            s.insert(p);
        }
        s
    }
}

/// Shared pause: a pause puts every peer in the round, each peer's resume
/// takes it out, and play resumes when nobody is left in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PauseGate {
    peers: PeerSet,
    /// Peers still on their pause menu (empty: not paused).
    holders: PeerSet,
    /// Who opened the current round.
    opened_by: Option<PeerId>,
}

impl PauseGate {
    /// A gate for these peers.
    #[must_use]
    pub fn new(peers: PeerSet) -> Self {
        Self {
            peers,
            holders: PeerSet::EMPTY,
            opened_by: None,
        }
    }

    /// `by` pauses. Returns true if this opened the round.
    pub fn pause(&mut self, by: PeerId) -> bool {
        if self.holders.is_empty() {
            self.holders = self.peers;
            self.holders.insert(by);
            self.opened_by = Some(by);
            true
        } else {
            self.holders.insert(by);
            false
        }
    }

    /// `by` resumes. Returns true if that released the round.
    pub fn unpause(&mut self, by: PeerId) -> bool {
        if !self.holders.contains(by) {
            return false;
        }
        self.holders.remove(by);
        self.released()
    }

    fn released(&mut self) -> bool {
        if self.holders.is_empty() {
            self.opened_by = None;
            true
        } else {
            false
        }
    }

    /// A peer left. Returns true if that released the round.
    pub fn peer_left(&mut self, peer: PeerId) -> bool {
        self.peers.remove(peer);
        if !self.holders.contains(peer) {
            return false;
        }
        self.holders.remove(peer);
        self.released()
    }

    /// Change the peer set; a joiner is not in an open round. Returns true
    /// if that released the round.
    pub fn set_peers(&mut self, peers: PeerSet) -> bool {
        self.peers = peers;
        let was = !self.holders.is_empty();
        self.holders = PeerSet(self.holders.0 & peers.0);
        was && self.released()
    }

    /// Who opened the current round.
    #[must_use]
    pub fn paused_by(&self) -> Option<PeerId> {
        self.opened_by
    }

    /// Peers still on their pause menu.
    #[must_use]
    pub fn holders(&self) -> PeerSet {
        self.holders
    }

    /// Whether `peer` is still on their pause menu.
    #[must_use]
    pub fn holds(&self, peer: PeerId) -> bool {
        self.holders.contains(peer)
    }

    /// Whether the game is paused (a round is open).
    #[must_use]
    pub fn is_paused(&self) -> bool {
        !self.holders.is_empty()
    }

    /// End the round whoever is in it (a run that ended). Returns who was.
    pub fn release(&mut self) -> PeerSet {
        self.opened_by = None;
        std::mem::take(&mut self.holders)
    }

    /// Adopt the sequencer's holders. Returns true on a change.
    pub fn adopt(&mut self, holders: PeerSet) -> bool {
        let h = PeerSet(holders.0 & self.peers.0);
        if h == self.holders {
            return false;
        }
        if h.is_empty() || self.holders.is_empty() {
            self.opened_by = None;
        }
        self.holders = h;
        true
    }
}

/// What an [`UpgradeGate`] input did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeEvent {
    /// The upgrade screen opened for everyone (this round).
    Opened(u16),
    /// Everyone is done: all peers may leave the screen.
    Released(u16),
    /// Recorded, nothing changed visibly (or a stale/duplicate input).
    Nothing,
}

/// Shared upgrade screen. Rounds number the level-ups; opens of the same round
/// from several peers merge, and later rounds queue behind the active one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeGate {
    peers: PeerSet,
    active: Option<u16>,
    queued: BTreeSet<u16>,
    done: PeerSet,
    early_done: BTreeMap<u16, PeerSet>,
    last_released: Option<u16>,
}

impl UpgradeGate {
    /// A gate for these peers.
    #[must_use]
    pub fn new(peers: PeerSet) -> Self {
        Self {
            peers,
            active: None,
            queued: BTreeSet::new(),
            done: PeerSet::EMPTY,
            early_done: BTreeMap::new(),
            last_released: None,
        }
    }

    fn is_stale(&self, round: u16) -> bool {
        self.last_released.is_some_and(|r| round <= r)
    }

    /// The screen opens for `round`.
    pub fn open(&mut self, round: u16) -> UpgradeEvent {
        if self.is_stale(round) || self.active == Some(round) {
            return UpgradeEvent::Nothing;
        }
        if self.active.is_some() {
            self.queued.insert(round);
            return UpgradeEvent::Nothing;
        }
        self.activate(round)
    }

    fn activate(&mut self, round: u16) -> UpgradeEvent {
        self.active = Some(round);
        self.done = self.early_done.remove(&round).unwrap_or_default();
        if self.done.contains_all(self.peers) {
            // Everyone already finished (possible after a peer left).
            return self.release();
        }
        UpgradeEvent::Opened(round)
    }

    fn release(&mut self) -> UpgradeEvent {
        let Some(round) = self.active.take() else {
            return UpgradeEvent::Nothing;
        };
        self.last_released = Some(round);
        self.done = PeerSet::EMPTY;
        UpgradeEvent::Released(round)
    }

    /// `peer` finished `round` (`Released` if it was the last). A done for an
    /// unopened round opens it.
    pub fn done(&mut self, peer: PeerId, round: u16) -> UpgradeEvent {
        if self.is_stale(round) {
            return UpgradeEvent::Nothing;
        }
        if self.active != Some(round) {
            self.early_done.entry(round).or_default().insert(peer);
            return self.open(round);
        }
        self.done.insert(peer);
        if self.done.contains_all(self.peers) {
            self.release()
        } else {
            UpgradeEvent::Nothing
        }
    }

    /// After a release, open the next queued round, if any.
    pub fn next_queued(&mut self) -> Option<UpgradeEvent> {
        if self.active.is_some() {
            return None;
        }
        while let Some(round) = self.queued.pop_first() {
            if !self.is_stale(round) {
                return Some(self.activate(round));
            }
        }
        None
    }

    /// Change the peer set (join/leave). Leaving may release the active round.
    pub fn set_peers(&mut self, peers: PeerSet) -> UpgradeEvent {
        self.peers = peers;
        if self.active.is_some() && self.done.contains_all(self.peers) {
            self.release()
        } else {
            UpgradeEvent::Nothing
        }
    }

    /// The open round, if the screen is up.
    #[must_use]
    pub fn active(&self) -> Option<u16> {
        self.active
    }

    /// Whether peers may leave the screen now (no round active).
    #[must_use]
    pub fn may_leave(&self) -> bool {
        self.active.is_none()
    }

    /// Peers still choosing.
    #[must_use]
    pub fn waiting_on(&self) -> PeerSet {
        if self.active.is_some() {
            self.peers.minus(self.done)
        } else {
            PeerSet::EMPTY
        }
    }

    /// The most recently released round.
    #[must_use]
    pub fn last_released(&self) -> Option<u16> {
        self.last_released
    }

    /// The active round, or the one after the last released.
    #[must_use]
    pub fn next_round(&self) -> u16 {
        self.active
            .unwrap_or_else(|| self.last_released.map_or(1, |r| r.saturating_add(1)))
    }

    /// Host: the round a peer's open joins: the active one, or the next if
    /// `round` was already released.
    #[must_use]
    pub fn resolve_open(&self, round: u16) -> u16 {
        match self.active {
            Some(a) => a,
            None if self.is_stale(round) => self.next_round(),
            None => round,
        }
    }

    /// Host: the round a peer's done counts for (the active one), or `None`
    /// if `round` was already released.
    #[must_use]
    pub fn resolve_done(&self, round: u16) -> Option<u16> {
        if self.is_stale(round) {
            None
        } else {
            Some(self.active.unwrap_or(round))
        }
    }

    /// Whether `peer` still has to finish the active round.
    #[must_use]
    pub fn is_waiting_on(&self, peer: PeerId) -> bool {
        self.waiting_on().contains(peer)
    }

    /// The sequencer released every round up to `round`; an active one
    /// among them ends.
    pub fn adopt_released(&mut self, round: u16) -> UpgradeEvent {
        if self.is_stale(round) {
            return UpgradeEvent::Nothing;
        }
        self.last_released = Some(round);
        self.queued.retain(|r| *r > round);
        self.early_done.retain(|r, _| *r > round);
        match self.active {
            Some(a) if a <= round => {
                self.active = None;
                self.done = PeerSet::EMPTY;
                UpgradeEvent::Released(a)
            }
            _ => UpgradeEvent::Nothing,
        }
    }
}

/// What a downed report led to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownOutcome {
    /// The peer waits and respawns at this game time.
    Respawning {
        /// Game time of the respawn.
        at: Duration,
    },
    /// Nobody is left standing.
    GameOver,
    /// Already down, unknown peer, or the game is already over.
    Ignored,
}

/// Default respawn delay.
pub const DEFAULT_RESPAWN: Duration = Duration::from_secs(45);

/// Downed and respawn rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespawnRules {
    respawn: Duration,
    peers: PeerSet,
    down: BTreeMap<PeerId, Duration>,
    game_over: bool,
}

impl RespawnRules {
    /// Rules for these peers with the given respawn delay.
    #[must_use]
    pub fn new(peers: PeerSet, respawn: Duration) -> Self {
        Self {
            respawn,
            peers,
            down: BTreeMap::new(),
            game_over: false,
        }
    }

    /// Host: `peer` died at game time `now`.
    pub fn downed(&mut self, peer: PeerId, now: Duration) -> DownOutcome {
        self.downed_until(peer, now + self.respawn)
    }

    /// The respawn delay for downs from now on.
    pub fn set_respawn(&mut self, respawn: Duration) {
        self.respawn = respawn;
    }

    /// [`downed`](Self::downed) with an explicit respawn time (clients).
    pub fn downed_until(&mut self, peer: PeerId, at: Duration) -> DownOutcome {
        if self.game_over || !self.peers.contains(peer) || self.down.contains_key(&peer) {
            return DownOutcome::Ignored;
        }
        self.down.insert(peer, at);
        self.check_all_down()
            .unwrap_or(DownOutcome::Respawning { at })
    }

    fn check_all_down(&mut self) -> Option<DownOutcome> {
        let alive = self.peers.iter().any(|p| !self.down.contains_key(&p));
        if alive || self.peers.is_empty() {
            None
        } else {
            self.game_over = true;
            Some(DownOutcome::GameOver)
        }
    }

    /// Downed peers whose respawn time has come (host: respawn them and relay).
    #[must_use]
    pub fn due(&self, now: Duration) -> Vec<PeerId> {
        if self.game_over {
            return Vec::new();
        }
        self.down
            .iter()
            .filter(|&(_, &at)| at <= now)
            .map(|(&p, _)| p)
            .collect()
    }

    /// `peer` is back. Returns false if it was not down or the game is over.
    pub fn respawned(&mut self, peer: PeerId) -> bool {
        !self.game_over && self.down.remove(&peer).is_some()
    }

    /// Change the peer set. If everyone left standing is down, the game ends.
    pub fn set_peers(&mut self, peers: PeerSet) -> Option<DownOutcome> {
        self.peers = peers;
        self.down.retain(|p, _| peers.contains(*p));
        if self.game_over {
            None
        } else {
            self.check_all_down()
        }
    }

    /// Whether `peer` is down.
    #[must_use]
    pub fn is_down(&self, peer: PeerId) -> bool {
        self.down.contains_key(&peer)
    }

    /// Downed peers.
    #[must_use]
    pub fn downed_peers(&self) -> PeerSet {
        self.down.keys().copied().collect()
    }

    /// Time left until `peer` respawns.
    #[must_use]
    pub fn remaining(&self, peer: PeerId, now: Duration) -> Option<Duration> {
        self.down.get(&peer).map(|at| at.saturating_sub(now))
    }

    /// Whether the run is over.
    #[must_use]
    pub fn is_game_over(&self) -> bool {
        self.game_over
    }

    /// The configured delay.
    #[must_use]
    pub fn respawn_delay(&self) -> Duration {
        self.respawn
    }

    /// A peer left: forget it without ending the run.
    pub fn remove_peer(&mut self, peer: PeerId) {
        self.peers.remove(peer);
        self.down.remove(&peer);
    }

    /// A peer (re)joined, alive.
    pub fn add_peer(&mut self, peer: PeerId) {
        self.peers.insert(peer);
    }

    /// The run is over (a client applying the host's `GameOver`).
    pub fn end(&mut self) {
        self.game_over = true;
    }

    /// The peers these rules cover.
    #[must_use]
    pub fn peers(&self) -> PeerSet {
        self.peers
    }
}

/// What the run-start gate decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunGateEvent {
    /// Still waiting for these peers.
    Waiting(PeerSet),
    /// Everyone is in with matching modes: start.
    Go,
    /// This peer's game mode differs from the host's: refuse the run.
    Mismatch(PeerId),
}

/// Holds the run until every peer is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunGate {
    expected: PeerSet,
    ready: BTreeMap<PeerId, u32>,
    started: bool,
}

impl RunGate {
    /// A gate waiting for these peers.
    #[must_use]
    pub fn new(expected: PeerSet) -> Self {
        Self {
            expected,
            ready: BTreeMap::new(),
            started: false,
        }
    }

    /// `peer` is in a run with game-mode hash `mode`.
    pub fn ready(&mut self, peer: PeerId, mode: u32) -> RunGateEvent {
        if self.started {
            return RunGateEvent::Go;
        }
        if self.expected.contains(peer) {
            self.ready.insert(peer, mode);
        }
        self.evaluate()
    }

    fn evaluate(&mut self) -> RunGateEvent {
        let in_now: PeerSet = self.ready.keys().copied().collect();
        let missing = self.expected.minus(in_now);
        if !missing.is_empty() {
            return RunGateEvent::Waiting(missing);
        }
        let reference = self
            .ready
            .get(&PeerId::HOST)
            .or_else(|| self.ready.values().next())
            .copied();
        if let Some(bad) = self
            .ready
            .iter()
            .find(|&(_, &m)| Some(m) != reference)
            .map(|(&p, _)| p)
        {
            return RunGateEvent::Mismatch(bad);
        }
        self.started = true;
        RunGateEvent::Go
    }

    /// Change who is expected (a peer left while waiting).
    pub fn set_expected(&mut self, expected: PeerSet) -> RunGateEvent {
        self.expected = expected;
        self.ready.retain(|p, _| expected.contains(*p));
        if self.started {
            RunGateEvent::Go
        } else {
            self.evaluate()
        }
    }

    /// Whether the run has started.
    #[must_use]
    pub fn started(&self) -> bool {
        self.started
    }

    /// Mark the run started (clients applying the host's `Go`).
    pub fn start(&mut self) {
        self.started = true;
    }

    /// Forget everything for the next run.
    pub fn reset(&mut self) {
        self.ready.clear();
        self.started = false;
    }
}

/// What a peer leaving released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerLeft {
    /// The leaver held the pause; the game resumed.
    pub resumed: bool,
    /// Upgrade gate outcome (`Released` if the leaver was the last one choosing).
    pub upgrade: UpgradeEvent,
    /// `GameOver` if everyone still here is down.
    pub respawn: Option<DownOutcome>,
    /// Run gate outcome (`Go` if the leaver was the last one missing).
    pub run: RunGateEvent,
}

/// All co-op rules for one run, plus the peer set they share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    peers: PeerSet,
    /// Shared pause.
    pub pause: PauseGate,
    /// Shared upgrade screen.
    pub upgrade: UpgradeGate,
    /// Downed/respawn and game over.
    pub respawn: RespawnRules,
    /// Run start.
    pub run: RunGate,
}

impl Session {
    /// A session for `peers` with the given respawn delay.
    #[must_use]
    pub fn new(peers: PeerSet, respawn: Duration) -> Self {
        Self {
            peers,
            pause: PauseGate::new(peers),
            upgrade: UpgradeGate::new(peers),
            respawn: RespawnRules::new(peers, respawn),
            run: RunGate::new(peers),
        }
    }

    /// Current peers.
    #[must_use]
    pub fn peers(&self) -> PeerSet {
        self.peers
    }

    /// A peer joined (before the run starts).
    pub fn add_peer(&mut self, peer: PeerId) {
        self.peers.insert(peer);
        self.pause.set_peers(self.peers);
        self.upgrade.set_peers(self.peers);
        self.respawn.set_peers(self.peers);
        self.run.set_expected(self.peers);
    }

    /// A peer left the session: release anything it was holding.
    pub fn remove_peer(&mut self, peer: PeerId) -> PeerLeft {
        self.peers.remove(peer);
        PeerLeft {
            resumed: self.pause.peer_left(peer),
            upgrade: self.upgrade.set_peers(self.peers),
            respawn: self.respawn.set_peers(self.peers),
            run: self.run.set_expected(self.peers),
        }
    }

    /// Whether the shared world is frozen (not started, paused, upgrading, over).
    #[must_use]
    pub fn world_frozen(&self) -> bool {
        !self.run.started()
            || self.pause.is_paused()
            || self.upgrade.active().is_some()
            || self.respawn.is_game_over()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PeerId = PeerId(0);
    const B: PeerId = PeerId(1);
    const C: PeerId = PeerId(2);

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn peer_set_basics() {
        let mut s = PeerSet::first(3);
        assert_eq!(s.len(), 3);
        assert!(s.contains(C) && !s.contains(PeerId(3)));
        s.remove(B);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![A, C]);
        assert!(PeerSet::first(3).contains_all(s));
        assert_eq!(PeerSet::first(32).len(), 32);
        assert!(!PeerSet::first(32).contains(PeerId(40)));
        assert_eq!(format!("{s:?}"), "{0, 2}");
    }

    #[test]
    fn a_pause_holds_everyone_until_each_resumes() {
        let mut p = PauseGate::new(PeerSet::first(2));
        assert!(!p.unpause(A), "not paused");
        assert!(p.pause(B), "B opens the round");
        assert_eq!(p.paused_by(), Some(B));
        assert!(p.holds(A) && p.holds(B), "both menus open");
        assert!(!p.unpause(A), "A closed; B still in it");
        assert!(p.is_paused() && !p.holds(A));
        assert!(!p.unpause(A), "a second close does nothing");
        assert!(!p.pause(A), "A back on its menu: no new round");
        assert!(p.holds(A));
        assert!(!p.unpause(B));
        assert!(p.unpause(A), "the last one out releases");
        assert!(!p.is_paused() && p.paused_by().is_none());
        // Both press at once: both are opens of the same round.
        assert!(p.pause(A));
        assert!(!p.pause(B));
        assert_eq!(p.holders(), PeerSet::first(2));
    }

    #[test]
    fn a_peer_leaving_is_out_of_the_round() {
        let mut p = PauseGate::new(PeerSet::first(2));
        p.pause(A);
        assert!(!p.unpause(A));
        assert!(p.peer_left(B), "B was the last one in it");
        assert!(!p.is_paused());
        assert!(p.pause(A), "alone: a round of one");
        assert!(p.unpause(A));
        // A peer joining mid-round is not in it.
        let mut p = PauseGate::new(PeerSet::first(1));
        p.pause(A);
        assert!(!p.set_peers(PeerSet::first(2)));
        assert!(!p.holds(B));
        assert!(p.unpause(A));
    }

    #[test]
    fn pause_three_peers_and_the_opener_leaving() {
        let mut p = PauseGate::new(PeerSet::first(3));
        assert!(p.pause(C));
        assert!(!p.unpause(A));
        assert!(!p.peer_left(B), "B leaves; C is still in it");
        assert!(p.peer_left(C), "the last one in it leaves: resumed");
        assert!(p.pause(A));
        assert_eq!(p.holders(), PeerSet::first(1), "the two left are gone");
    }

    #[test]
    fn upgrade_nobody_leaves_until_all_done() {
        let mut g = UpgradeGate::new(PeerSet::first(2));
        assert!(g.may_leave());
        assert_eq!(g.open(1), UpgradeEvent::Opened(1));
        assert_eq!(g.open(1), UpgradeEvent::Nothing, "partner's open merges");
        assert_eq!(g.done(A, 1), UpgradeEvent::Nothing);
        assert!(!g.may_leave());
        assert_eq!(g.waiting_on(), [B].into_iter().collect());
        assert_eq!(g.done(A, 1), UpgradeEvent::Nothing, "duplicate");
        assert_eq!(g.done(B, 1), UpgradeEvent::Released(1));
        assert!(g.may_leave());
        assert_eq!(g.open(1), UpgradeEvent::Nothing, "stale open");
        assert_eq!(g.done(B, 1), UpgradeEvent::Nothing, "stale done");
        assert_eq!(g.last_released(), Some(1));
    }

    #[test]
    fn a_pause_round_adopted_from_a_statement() {
        let mut p = PauseGate::new(PeerSet::first(2));
        assert!(!p.adopt(PeerSet::EMPTY), "nothing to change");
        assert!(
            p.adopt(PeerSet::from_bits(0b110)),
            "a peer outside the set is left out"
        );
        assert_eq!(p.holders(), [B].into_iter().collect());
        assert!(p.is_paused() && p.paused_by().is_none());
        assert!(!p.adopt(PeerSet::from_bits(0b10)));
        assert!(p.adopt(PeerSet::EMPTY), "the round ended");
        assert!(!p.is_paused());
        assert!(p.pause(A), "and a new one opens as usual");
        assert_eq!(PeerSet::from_bits(p.holders().bits()), p.holders());
    }

    #[test]
    fn upgrade_rounds_released_from_a_statement() {
        let mut g = UpgradeGate::new(PeerSet::first(2));
        assert_eq!(g.open(1), UpgradeEvent::Opened(1));
        assert_eq!(g.open(2), UpgradeEvent::Nothing, "queued");
        assert_eq!(g.done(A, 3), UpgradeEvent::Nothing, "early");
        assert_eq!(g.done(A, 1), UpgradeEvent::Nothing);
        assert_eq!(g.adopt_released(2), UpgradeEvent::Released(1));
        assert_eq!(g.last_released(), Some(2));
        assert!(g.active().is_none() && g.may_leave());
        assert_eq!(g.adopt_released(2), UpgradeEvent::Nothing, "stale");
        assert_eq!(g.adopt_released(1), UpgradeEvent::Nothing, "older");
        assert_eq!(g.open(2), UpgradeEvent::Nothing, "round 2 is over too");
        assert_eq!(
            g.open(3),
            UpgradeEvent::Opened(3),
            "round 3 kept its early done"
        );
        assert_eq!(g.done(B, 3), UpgradeEvent::Released(3));
    }

    #[test]
    fn upgrade_three_peers_queue_and_leave() {
        let mut g = UpgradeGate::new(PeerSet::first(3));
        assert_eq!(g.open(1), UpgradeEvent::Opened(1));
        assert_eq!(g.open(2), UpgradeEvent::Nothing, "queued");
        assert_eq!(g.done(A, 1), UpgradeEvent::Nothing);
        assert_eq!(g.done(B, 1), UpgradeEvent::Nothing);
        assert_eq!(g.waiting_on(), [C].into_iter().collect());
        // C disconnects: the round releases.
        let mut two = PeerSet::first(3);
        two.remove(C);
        assert_eq!(g.set_peers(two), UpgradeEvent::Released(1));
        assert_eq!(g.next_queued(), Some(UpgradeEvent::Opened(2)));
        assert_eq!(g.next_queued(), None, "round 2 active");
        assert_eq!(g.done(B, 2), UpgradeEvent::Nothing);
        assert_eq!(g.done(A, 2), UpgradeEvent::Released(2));
        assert_eq!(g.next_queued(), None);
    }

    #[test]
    fn upgrade_done_before_open_implies_open() {
        let mut g = UpgradeGate::new(PeerSet::first(2));
        assert_eq!(g.done(B, 5), UpgradeEvent::Opened(5));
        assert_eq!(g.waiting_on(), [A].into_iter().collect());
        assert_eq!(g.done(A, 5), UpgradeEvent::Released(5));
    }

    #[test]
    fn respawn_while_partner_alive() {
        let mut r = RespawnRules::new(PeerSet::first(2), DEFAULT_RESPAWN);
        assert_eq!(
            r.downed(B, secs(10)),
            DownOutcome::Respawning { at: secs(55) }
        );
        assert_eq!(r.downed(B, secs(11)), DownOutcome::Ignored);
        assert_eq!(r.remaining(B, secs(50)), Some(secs(5)));
        assert!(r.due(secs(54)).is_empty());
        assert_eq!(r.due(secs(55)), vec![B]);
        assert!(r.respawned(B));
        assert!(!r.respawned(B));
        assert!(!r.is_down(B));
    }

    #[test]
    fn all_down_is_game_over() {
        let mut r = RespawnRules::new(PeerSet::first(2), secs(45));
        r.downed(A, secs(0));
        assert_eq!(r.downed(B, secs(30)), DownOutcome::GameOver);
        assert!(r.is_game_over());
        assert!(r.due(secs(100)).is_empty(), "no respawns after game over");
        assert!(!r.respawned(A));
        assert_eq!(r.downed(A, secs(1)), DownOutcome::Ignored);
    }

    #[test]
    fn respawn_three_peers() {
        let mut r = RespawnRules::new(PeerSet::first(3), secs(10));
        assert!(matches!(
            r.downed(A, secs(0)),
            DownOutcome::Respawning { .. }
        ));
        assert!(matches!(
            r.downed(B, secs(5)),
            DownOutcome::Respawning { .. }
        ));
        assert_eq!(r.downed_peers(), [A, B].into_iter().collect());
        assert_eq!(r.due(secs(10)), vec![A]);
        assert!(r.respawned(A));
        // C going down now does not end the run: A is back.
        assert!(matches!(
            r.downed(C, secs(11)),
            DownOutcome::Respawning { .. }
        ));
        assert!(!r.is_game_over());
        // A leaving with B and C down ends it.
        let mut bc = PeerSet::first(3);
        bc.remove(A);
        assert_eq!(r.set_peers(bc), Some(DownOutcome::GameOver));
        assert_eq!(r.downed(PeerId(9), secs(0)), DownOutcome::Ignored);
    }

    #[test]
    fn run_gate_holds_until_everyone_is_in() {
        let mut g = RunGate::new(PeerSet::first(2));
        assert_eq!(
            g.ready(B, 7),
            RunGateEvent::Waiting([A].into_iter().collect())
        );
        assert!(!g.started());
        assert_eq!(g.ready(A, 7), RunGateEvent::Go);
        assert!(g.started());
        assert_eq!(
            g.ready(B, 7),
            RunGateEvent::Go,
            "late duplicates are harmless"
        );
        g.reset();
        assert!(!g.started());
    }

    #[test]
    fn run_gate_three_peers_and_mismatch() {
        let mut g = RunGate::new(PeerSet::first(3));
        assert!(matches!(g.ready(A, 1), RunGateEvent::Waiting(_)));
        assert!(matches!(g.ready(C, 2), RunGateEvent::Waiting(_)));
        assert_eq!(g.ready(B, 1), RunGateEvent::Mismatch(C));
        assert!(!g.started());
        // C fixes its mode.
        assert_eq!(g.ready(C, 1), RunGateEvent::Go);
        // A peer leaving while others wait releases the gate.
        let mut g = RunGate::new(PeerSet::first(3));
        g.ready(A, 1);
        g.ready(B, 1);
        let mut ab = PeerSet::first(3);
        ab.remove(C);
        assert_eq!(g.set_expected(ab), RunGateEvent::Go);
    }

    #[test]
    fn pause_release_ends_the_round() {
        let mut p = PauseGate::new(PeerSet::first(2));
        assert!(p.release().is_empty());
        p.pause(B);
        assert_eq!(p.release(), PeerSet::first(2));
        assert!(!p.is_paused());
        assert!(p.pause(A));
    }

    #[test]
    fn upgrade_rounds_resolve_on_the_host() {
        let mut g = UpgradeGate::new(PeerSet::first(2));
        assert_eq!(g.next_round(), 1);
        assert_eq!(g.resolve_open(1), 1);
        assert_eq!(g.open(1), UpgradeEvent::Opened(1));
        assert_eq!(g.next_round(), 1, "the active round");
        assert_eq!(g.resolve_open(7), 1, "an open while active joins it");
        assert_eq!(g.resolve_done(7), Some(1));
        assert!(g.is_waiting_on(A) && g.is_waiting_on(B));
        g.done(A, 1);
        assert!(!g.is_waiting_on(A));
        assert_eq!(g.done(B, 1), UpgradeEvent::Released(1));
        assert!(!g.is_waiting_on(B), "nothing active");
        assert_eq!(g.next_round(), 2);
        assert_eq!(g.resolve_open(1), 2, "a peer that missed the release");
        assert_eq!(g.resolve_done(1), None, "late done for a released round");
        assert_eq!(g.resolve_done(2), Some(2), "done before open");
        assert_eq!(g.done(B, 2), UpgradeEvent::Opened(2));
    }

    #[test]
    fn respawn_remove_peer_never_ends_the_run() {
        let mut r = RespawnRules::new(PeerSet::first(2), secs(10));
        r.downed(A, secs(0));
        r.remove_peer(B);
        assert!(!r.is_game_over(), "A keeps its timer");
        assert_eq!(r.due(secs(10)), vec![A]);
        assert_eq!(r.peers(), [A].into_iter().collect());
        r.add_peer(B);
        assert!(r.peers().contains(B) && !r.is_down(B));
        r.end();
        assert!(r.is_game_over());
        assert!(r.due(secs(99)).is_empty());
    }

    #[test]
    fn session_freezes_the_world() {
        let mut s = Session::new(PeerSet::first(2), secs(45));
        assert!(s.world_frozen(), "before the start");
        s.run.ready(A, 0);
        s.run.ready(B, 0);
        assert!(!s.world_frozen());
        s.pause.pause(B);
        assert!(s.world_frozen());
        let left = s.remove_peer(B);
        assert!(!left.resumed, "A is still on its pause menu");
        assert_eq!(left.respawn, None);
        assert!(s.world_frozen());
        assert!(s.pause.unpause(A));
        assert!(!s.world_frozen(), "the pauser left, A resumed");
        s.add_peer(C);
        assert_eq!(s.peers(), [A, C].into_iter().collect());
        s.remove_peer(C);
        assert_eq!(s.peers(), [A].into_iter().collect());
        s.upgrade.open(1);
        assert_eq!(s.upgrade.done(A, 1), UpgradeEvent::Released(1));
    }
}
