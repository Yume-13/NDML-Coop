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

//! The rules converge from state as well as events, so lost rules messages
//! never leave a game held.
//!
//! Each machine states its rules 4 times a second ([`Message::RulesState`],
//! latest wins): the host its pause, rounds, downs and game over; the joining
//! game its pause hold, screen round and choice. A statement that differs for
//! [`GRACE`] with nothing of this machine's own in flight is adopted, forward
//! only; neither side adopts the other's word about its own hold or down.
//!
//! Per link ([`Link`]) each side counts the rules messages it sends and gets;
//! a statement carries the counts, an old one never overrides, and messages an
//! adopted statement covers are dropped on arrival. Messages counted but
//! missing for [`LOST_AFTER`] are written off ([`Link::lost_check`]).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ndml_net::msg::{Message, PeerId, RulesState};
use ndml_net::session::PeerSet;

use super::{Rules, Screen, Ship, one, wire_ms};
use crate::net::Out;

/// Frames between two statements (4 a second).
pub(super) const STATE_EVERY: u64 = 15;
/// How long the two may differ before the statement is adopted.
pub(super) const GRACE: Duration = Duration::from_secs(3);
/// Messages a statement counts that have not arrived after this long of
/// statements are taken as lost ([`Link::lost_check`]).
pub(super) const LOST_AFTER: Duration = Duration::from_secs(30);
/// A press of this player's this recent (frames) blocks any adoption here.
const PRESS_GRACE: u64 = 180;

/// The rules messages the counts cover.
pub(super) fn counted(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Pause { .. }
            | Message::Unpause { .. }
            | Message::UpgradeOpen { .. }
            | Message::UpgradeDone { .. }
            | Message::Downed { .. }
            | Message::Respawned { .. }
            | Message::GameOver
    )
}

/// One link's counts, on this side of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Link {
    /// This side's count of the link ([`RulesState::link`]).
    pub(super) id: u16,
    /// The partner's `link` its last statement carried (0: none since this
    /// side's started).
    pub(super) seen: u16,
    /// This side's rules messages on it, as the partner counts them.
    pub(super) sent: u32,
    /// The partner's rules messages that arrived on it.
    pub(super) got: u32,
    /// Arrivals to drop: covered by an adopted statement.
    pub(super) skip: u32,
    /// Since when the partner's statement and this side's differ.
    since: Option<Instant>,
    /// The self-check: since when every current statement counted messages
    /// that had not arrived ([`Link::lost_check`]).
    owed: Option<Instant>,
}

impl Link {
    pub(super) fn new(id: u16) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }

    /// One of this side's rules messages went out on it.
    pub(super) fn sent_one(&mut self) {
        self.sent += 1;
    }

    /// One of the partner's arrived: false to drop it (covered).
    pub(super) fn arrived(&mut self) -> bool {
        if self.skip > 0 {
            self.skip -= 1;
            return false;
        }
        self.got += 1;
        true
    }

    /// This side's counts, into its statement.
    pub(super) fn stamp(&self, s: &mut RulesState) {
        (s.seq, s.ack, s.link, s.echo) = (self.sent, self.got, self.id, self.seen);
    }

    /// The partner's statement arrived: false if it is from an earlier link
    /// of the partner's. `sent` takes its `ack` (as it is on a link new here,
    /// else only upward), so it never runs ahead of what arrived.
    pub(super) fn heard(&mut self, s: &RulesState) -> bool {
        if s.link == self.seen {
            self.sent = self.sent.max(s.ack);
            return true;
        }
        // Counted up from `seen` (wrapping): ahead by less than half the
        // range is a later link.
        if self.seen != 0 && s.link.wrapping_sub(self.seen) >= 0x8000 {
            return false;
        }
        self.seen = s.link;
        self.sent = s.ack;
        true
    }

    /// The statement counts on this side of the link as it is now and is not
    /// behind what arrived.
    pub(super) fn current(&self, s: &RulesState) -> bool {
        s.echo == self.id && s.seq >= self.got
    }

    /// The statement is adopted: the messages it covers still on their way
    /// are dropped when they arrive (the lane is ordered: the next ones).
    pub(super) fn adopt(&mut self, s: &RulesState) {
        self.skip += s.seq - self.got;
        self.got = s.seq;
        self.since = None;
    }

    /// The self-check, on each current statement: when every statement for
    /// `window` counts messages that have not arrived, they are counted as
    /// arrived. Returns how many.
    pub(super) fn lost_check(
        &mut self,
        s: &RulesState,
        now: Instant,
        window: Duration,
    ) -> Option<u32> {
        let missing = s.seq.saturating_sub(self.got) + self.skip;
        if missing == 0 {
            self.owed = None;
            return None;
        }
        let since = *self.owed.get_or_insert(now);
        if now.saturating_duration_since(since) < window {
            return None;
        }
        self.got = self.got.max(s.seq);
        self.skip = 0;
        self.owed = None;
        Some(missing)
    }
}

/// The bookkeeping (reset with the run, but for the link count).
#[derive(Debug, Clone, Default)]
pub(super) struct Sync {
    links: BTreeMap<PeerId, Link>,
    /// The last link count ([`Link::id`]) handed out.
    last_link: u16,
    /// The frame of this player's last request to the host.
    press: Option<u64>,
    adopted: u32,
}

impl Sync {
    /// The bookkeeping for the next run: link counts keep counting up (a
    /// partner still on this run's link sees the next one as new).
    pub(super) fn next_run(&self) -> Self {
        Self {
            last_link: self.last_link,
            ..Self::default()
        }
    }

    /// The counts as one line (adoptions; per link: skip, link numbers, sent, got).
    #[cfg(test)]
    pub(super) fn count_line(&self) -> String {
        use std::fmt::Write as _;
        let mut s = format!("co-op rules: adopted from state: {}", self.adopted);
        for (p, l) in &self.links {
            let _ = write!(
                s,
                "; {p}: skip {}, link {}/{}, sent {}, got {}",
                l.skip, l.id, l.seen, l.sent, l.got
            );
        }
        s
    }

    fn link(&mut self, p: PeerId) -> &mut Link {
        let last = &mut self.last_link;
        self.links.entry(p).or_insert_with(|| {
            *last = last.wrapping_add(1).max(1);
            Link::new(*last)
        })
    }
}

impl Rules {
    /// Count a rules message sent to `to` (`None`: every partner).
    pub(super) fn note_out(&mut self, to: Option<PeerId>, msg: &Message) {
        if !counted(msg) {
            return;
        }
        if !self.is_host && !matches!(msg, Message::Downed { .. }) {
            // A request of the joining player's to the host is a press (its
            // down report is resent until answered; it blocks nothing).
            self.sync.press = Some(self.frame);
        }
        let me = self.me;
        let to: Vec<PeerId> = to.map_or_else(
            || self.peers.iter().filter(|p| *p != me).collect(),
            |p| vec![p],
        );
        for p in to {
            self.sync.link(p).sent_one();
        }
    }

    /// A rules message from `from` arrived: false to drop it (covered by an
    /// adopted statement).
    pub(super) fn note_in(&mut self, from: PeerId, msg: &Message) -> bool {
        if !counted(msg) {
            return true;
        }
        let fresh = self.sync.link(from).arrived();
        if !fresh {
            log::debug!("co-op rules: {from}'s {msg:?} is in the state adopted already");
        }
        fresh
    }

    /// The link to `peer` is gone, or silent for the wait: its counts start
    /// over on a new link count (the partner's, kept or not, catch up from
    /// this side's statements).
    pub(super) fn sync_link_lost(&mut self, peer: PeerId) {
        self.sync.links.remove(&peer);
    }

    /// Statements this run adopted.
    #[must_use]
    pub fn adopted(&self) -> u32 {
        self.sync.adopted
    }

    /// Every [`STATE_EVERY`] frames: this side's statement to each partner.
    pub(super) fn state_tick(&mut self) {
        if !self.active || self.separated || self.alone() {
            return;
        }
        if !self.frame.is_multiple_of(STATE_EVERY) {
            return;
        }
        let me = self.me;
        let to: Vec<PeerId> = if self.is_host {
            self.peers.iter().filter(|p| *p != me).collect()
        } else {
            vec![PeerId::HOST]
        };
        for p in to {
            let mut s = self.statement();
            self.sync.link(p).stamp(&mut s);
            self.outs.push(Out::Send(p, Message::RulesState(s)));
        }
    }

    /// This side's statement but for the link's counts ([`Link::stamp`]).
    pub(super) fn statement(&self) -> RulesState {
        let mut s = RulesState {
            peer: self.me,
            seq: 0,
            ack: 0,
            link: 0,
            echo: 0,
            flags: 0,
            holders: 0,
            round: 0,
            done: 0,
            chosen: 0,
            released: 0,
            downs: Vec::new(),
        };
        if self.is_host {
            if self.respawn.is_game_over() {
                s.flags |= RulesState::GAME_OVER;
            }
            s.holders = self.pause.holders().bits();
            if let Some(r) = self.upgrade.active() {
                s.round = r;
                s.done = self.peers.minus(self.upgrade.waiting_on()).bits();
            }
            s.released = self.upgrade.last_released().unwrap_or(0);
            let gt = self.game_time;
            s.downs = self
                .respawn
                .downed_peers()
                .iter()
                .map(|p| {
                    (
                        p,
                        self.respawn
                            .remaining(p, gt)
                            .map_or(0, |d| wire_ms(d).max(1)),
                    )
                })
                .collect();
        } else {
            if self.i_hold() {
                s.flags |= RulesState::HOLD;
            }
            if matches!(self.down, Ship::Down { .. }) {
                s.flags |= RulesState::DOWN;
            }
            if self.screen != Screen::Idle {
                s.round = self.open_round().or(self.proposed).unwrap_or(0);
            }
            s.chosen = self.done_sent.unwrap_or(0);
        }
        s
    }

    /// The partner's statement arrived.
    pub(super) fn on_state(&mut self, from: PeerId, s: &RulesState, now: Instant) {
        if !self.active || self.separated || self.alone() || !self.peers.contains(from) {
            return;
        }
        if !self.is_host && from != PeerId::HOST {
            return;
        }
        let link = self.sync.link(from);
        if !link.heard(s) {
            return;
        }
        let current = link.current(s);
        if current && let Some(n) = link.lost_check(s, now, LOST_AFTER) {
            log::warn!(
                "co-op rules: {n} rules message(s) from {from} never arrived in {} s of its statements; counted as lost",
                LOST_AFTER.as_secs()
            );
        }
        let differs = current
            && if self.is_host {
                self.host_differs(from, s)
            } else {
                self.client_differs(s)
            };
        let link = self.sync.link(from);
        if !differs {
            link.since = None;
            return;
        }
        // Never on the first statement that differs (the link's counts
        // after a local start over included: `since` starts with it).
        let since = *link.since.get_or_insert(now);
        // Never under the joining player's hands: its own press on its way,
        // or its screen open on a choice.
        let busy = !self.is_host
            && (self.screen == Screen::Choosing
                || self
                    .sync
                    .press
                    .is_some_and(|f| self.frame.saturating_sub(f) < PRESS_GRACE));
        if now.saturating_duration_since(since) < GRACE || busy {
            return;
        }
        self.sync.link(from).adopt(s);
        self.sync.adopted += 1;
        if self.is_host {
            self.host_adopt(from, s, now);
        } else {
            self.client_adopt(s, now);
        }
    }

    /// Joining game: the pause holders the host's statement means here (its
    /// own place is its own).
    fn holders_target(&self, s: &RulesState) -> PeerSet {
        let mut t = PeerSet::from_bits(s.holders).minus(one(self.me));
        if self.i_hold() {
            t.insert(self.me);
        }
        PeerSet::from_bits(t.bits() & self.peers.bits())
    }

    /// The host released a round this game still has open, or is held for.
    fn release_due(&self, s: &RulesState) -> bool {
        s.released > 0
            && (self.upgrade.active().is_some_and(|a| a <= s.released)
                || (self.pending_open() && self.proposed.is_some_and(|p| p <= s.released)))
    }

    /// The host's open round, unknown here.
    fn open_due(&self, s: &RulesState) -> bool {
        s.round > 0
            && self.upgrade.active().is_none_or(|a| a < s.round)
            && self.upgrade.last_released().is_none_or(|l| l < s.round)
    }

    /// Who the host has done in the round open on both, that this game lacks.
    fn done_due(&self, s: &RulesState) -> PeerSet {
        if s.round == 0 || self.upgrade.active() != Some(s.round) {
            return PeerSet::EMPTY;
        }
        let mut host = PeerSet::from_bits(s.done);
        if self.done_sent != Some(s.round) {
            host.remove(self.me);
        }
        host.minus(self.peers.minus(self.upgrade.waiting_on()))
    }

    /// The host's stated downs that differ here: (down there, not here), (down
    /// here, respawned there); this ship only while down (it reports its own).
    fn downs_due(&self, s: &RulesState) -> (Vec<(PeerId, u32)>, Vec<PeerId>) {
        if self.respawn.is_game_over() {
            // Nobody comes back after the end, and a down there is moot.
            return (Vec::new(), Vec::new());
        }
        let mine_down = matches!(self.down, Ship::Down { .. });
        let ok = |p: PeerId| p != self.me || mine_down;
        let there: PeerSet = s.downs.iter().map(|d| d.0).collect();
        let new = s
            .downs
            .iter()
            .filter(|(p, _)| ok(*p) && !self.respawn.is_down(*p))
            .copied()
            .collect();
        let back = self
            .respawn
            .downed_peers()
            .minus(there)
            .iter()
            .filter(|p| ok(*p))
            .collect();
        (new, back)
    }

    fn client_differs(&self, s: &RulesState) -> bool {
        let over = s.flags & RulesState::GAME_OVER != 0 && !self.game_over_seen;
        let rounds = self.release_due(s) || self.open_due(s) || !self.done_due(s).is_empty();
        let pause = self.holders_target(s) != self.pause.holders();
        let (new, back) = self.downs_due(s);
        over || rounds || pause || !new.is_empty() || !back.is_empty()
    }

    fn client_adopt(&mut self, s: &RulesState, now: Instant) {
        if s.flags & RulesState::GAME_OVER != 0 && !self.game_over_seen {
            log::info!("co-op rules: game over from the host's state");
            self.respawn.end();
            self.on_game_over();
            return;
        }
        if self.release_due(s) {
            log::info!(
                "co-op rules: round {} released from the host's state",
                s.released
            );
            if self.proposed.is_some_and(|p| p <= s.released) {
                self.proposed = None;
            }
            let ev = self.upgrade.adopt_released(s.released);
            self.on_upgrade(ev);
            self.wait_since = None;
        }
        if self.open_due(s) {
            log::info!("co-op rules: round {} open from the host's state", s.round);
            let ev = self.upgrade.open(s.round);
            self.on_upgrade(ev);
        }
        for p in self.done_due(s).iter() {
            log::info!(
                "co-op rules: {p} chose in round {}, from the host's state",
                s.round
            );
            let ev = self.upgrade.done(p, s.round);
            self.on_upgrade(ev);
        }
        let mine = PeerSet::from_bits(s.holders).contains(self.me);
        if !mine && s.round > 0 && self.pause_request.take().is_some() {
            // Turned down: the world is frozen for the host's round (a
            // pause that never reached the host is taken there from this
            // game's statement instead).
            log::info!(
                "co-op rules: the local pause was turned down for the host's round, from its state"
            );
        }
        // This game's own place as it holds it now, before the host's
        // answer to its request clears that request.
        let t = self.holders_target(s);
        if mine {
            self.pause_request = None;
        } else {
            self.close_sent = false;
        }
        let was = self.pause.is_paused();
        if self.pause.adopt(t) {
            if t.is_empty() {
                self.pause_since = None;
                log::info!("co-op rules: play resumes from the host's state");
            } else {
                if !was {
                    self.pause_since = Some(now);
                }
                log::info!(
                    "co-op rules: the shared pause from the host's state ({} on their menu)",
                    t.len()
                );
            }
        }
        let gt = self.game_time;
        let (new, back) = self.downs_due(s);
        for (p, ms) in new {
            log::info!("co-op rules: {p} down from the host's state");
            self.respawn
                .downed_until(p, gt + ndml_net::time::from_wire_ms(ms));
            if p == self.me {
                self.down = Ship::Down { reported: None };
            } else {
                self.events.push(super::SessionEvent::PartnerDown);
            }
        }
        for p in back {
            if self.respawn.respawned(p) {
                log::info!("co-op rules: {p} respawned, from the host's state");
                self.respawned(p);
            }
        }
    }

    /// Host: the joining game's pause hold differs, and the host would take it.
    fn hold_due(&self, c: PeerId, s: &RulesState) -> Option<bool> {
        let hold = s.flags & RulesState::HOLD != 0;
        let here = self.pause.holds(c);
        (hold != here && (here || self.upgrade.active().is_none())).then_some(hold)
    }

    /// Host: the joining game's screen is on a round the host has not opened.
    fn client_open_due(&self, s: &RulesState) -> bool {
        s.round > 0
            && self.upgrade.active().is_none()
            && self.upgrade.last_released().is_none_or(|l| l < s.round)
    }

    /// Host: the joining player chose in the open round and the host still
    /// waits on it.
    fn client_done_due(&self, c: PeerId, s: &RulesState) -> bool {
        s.chosen > 0
            && self.upgrade.is_waiting_on(c)
            && self.upgrade.resolve_done(s.chosen) == self.upgrade.active()
    }

    /// Host: it has the joining ship down while it flies. That game reports its
    /// own down; a stale one here could end the run if the host's went down too.
    fn client_up_due(&self, c: PeerId, s: &RulesState) -> bool {
        s.flags & RulesState::DOWN == 0 && self.respawn.is_down(c) && !self.respawn.is_game_over()
    }

    fn host_differs(&self, c: PeerId, s: &RulesState) -> bool {
        self.hold_due(c, s).is_some()
            || self.client_open_due(s)
            || self.client_done_due(c, s)
            || self.client_up_due(c, s)
    }

    fn host_adopt(&mut self, c: PeerId, s: &RulesState, now: Instant) {
        match self.hold_due(c, s) {
            Some(true) => {
                log::info!("co-op rules: {c} is on their pause menu, from their state");
                self.on_pause(c, c, now);
            }
            Some(false) => {
                log::info!("co-op rules: {c} resumed, from their state");
                self.on_unpause(c);
            }
            None => {}
        }
        if self.client_open_due(s) {
            log::info!(
                "co-op rules: {c}'s upgrade screen opens round {}, from their state",
                s.round
            );
            self.on_upgrade_open(s.round);
        }
        if self.client_done_due(c, s) {
            log::info!(
                "co-op rules: {c} chose in round {}, from their state",
                s.chosen
            );
            self.on_upgrade_done(c, s.chosen);
        }
        if self.client_up_due(c, s) && self.respawn.respawned(c) {
            log::info!("co-op rules: {c}'s ship is up, from their state");
            self.bcast(Message::Respawned { peer: c });
            self.respawned(c);
        }
    }
}

#[cfg(test)]
mod tests;
