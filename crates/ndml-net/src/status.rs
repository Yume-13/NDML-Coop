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

//! Status effects a hit leaves on an enemy besides hull: knockback
//! (`pushed_x/y`), the stun slow (`enemySpeed`), hit flashes and marks
//! (`vulnerabilityMult/Count`, `satFireHits`, `blitzed`).
//!
//! Shaped like shared damage ([`crate::damage`]): a non-owner reports
//! cumulative totals ([`StatusReporter`]) until acknowledged; the owner
//! ([`StatusLedger`]) applies only the increase and merges marks by maximum.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::msg::{NetId, PeerId};

/// Unacknowledged entries are sent again after this many frames.
pub const RESEND_FRAMES: u64 = 6;
/// The owner keeps acknowledging an enemy this many frames after its last
/// change.
pub const ACK_HOLD_FRAMES: u32 = 30;
/// Amounts below this are noise.
pub const EPS: f32 = 1e-3;

/// Marks an enemy carries: the latest values on the shooter's copy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Marks {
    pub vulnerability_mult: f32,
    pub vulnerability_count: f32,
    pub sat_fire_hits: f32,
    pub blitzed: f32,
}

impl Marks {
    /// Each mark the larger of the two (merged by maximum).
    #[must_use]
    pub fn max(self, o: Self) -> Self {
        Self {
            vulnerability_mult: self.vulnerability_mult.max(o.vulnerability_mult),
            vulnerability_count: self.vulnerability_count.max(o.vulnerability_count),
            sat_fire_hits: self.sat_fire_hits.max(o.sat_fire_hits),
            blitzed: self.blitzed.max(o.blitzed),
        }
    }

    fn finite(self) -> bool {
        [
            self.vulnerability_mult,
            self.vulnerability_count,
            self.sat_fire_hits,
            self.blitzed,
        ]
        .iter()
        .all(|v| v.is_finite())
    }
}

/// One enemy in a report: cumulative since its spawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusEntry {
    pub net_id: NetId,
    /// Bumped on every change (0: none yet); knockback applies only from a
    /// newer `seq`, so a late report cannot undo it.
    pub seq: u16,
    pub push_x: f32,
    pub push_y: f32,
    pub slow: f32,
    pub flashes: u32,
    /// Bumped whenever the marks changed (0: none sent yet).
    pub marks_seq: u16,
    pub marks: Marks,
}

/// One enemy in an ack: what the owner has applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusAckEntry {
    pub net_id: NetId,
    pub seq: u16,
    pub push_x: f32,
    pub push_y: f32,
    pub slow: f32,
    pub flashes: u32,
    pub marks_seq: u16,
}

/// The status channel's messages (wire tag 39, unreliable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StatusMsg {
    /// A non-owner's status effects on enemies it does not own.
    Report {
        by: PeerId,
        entries: Vec<StatusEntry>,
    },
    /// The owner's answer: what it applied of `peer`'s reports. `frame`:
    /// the owner's counter (newer acks replace older ones).
    Ack {
        peer: PeerId,
        frame: u32,
        entries: Vec<StatusAckEntry>,
    },
}

impl StatusMsg {
    /// Entries per message (a report entry is at most 3 + 4·4 + 5 + 3 + 16
    /// bytes).
    pub const MAX_ENTRIES: usize = 25;
}

#[derive(Debug, Clone, Copy, Default)]
struct Rep {
    cur: StatusEntry,
    acked: StatusAckEntry,
    ack_frame: Option<u32>,
    sent: Option<(u64, StatusEntry)>,
}

impl Rep {
    fn pending(&self) -> bool {
        self.cur.seq != self.acked.seq
    }

    fn changed(&mut self) {
        self.cur.seq = next_seq(self.cur.seq);
    }
}

/// The counter after `s` (never 0: 0 means nothing yet).
fn next_seq(s: u16) -> u16 {
    s.wrapping_add(1).max(1)
}

/// Whether counter `new` is after `old` (wrapping).
fn newer(new: u16, old: u16) -> bool {
    let d = new.wrapping_sub(old);
    new != 0 && d != 0 && d < u16::MAX / 2
}

/// The newest of held entries per (peer, enemy); entries are cumulative.
#[must_use]
pub fn newest(held: Vec<(PeerId, StatusEntry)>) -> Vec<(PeerId, StatusEntry)> {
    let mut out: Vec<(PeerId, StatusEntry)> = Vec::new();
    for (by, e) in held {
        match out
            .iter_mut()
            .find(|(p, x)| *p == by && x.net_id == e.net_id)
        {
            Some((_, x)) => {
                if newer(e.seq, x.seq) {
                    *x = e;
                }
            }
            None => out.push((by, e)),
        }
    }
    out
}

/// The shooter's side.
#[derive(Debug, Clone, Default)]
pub struct StatusReporter {
    enemies: BTreeMap<NetId, Rep>,
}

impl StatusReporter {
    /// An empty reporter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn rep(&mut self, net_id: NetId) -> &mut Rep {
        let r = self.enemies.entry(net_id).or_default();
        r.cur.net_id = net_id;
        r.acked.net_id = net_id;
        r
    }

    /// Knockback this side gave the copy.
    pub fn push(&mut self, net_id: NetId, dx: f32, dy: f32) {
        if dx.is_finite() && dy.is_finite() && (dx.abs() > EPS || dy.abs() > EPS) {
            let r = self.rep(net_id);
            r.cur.push_x += dx;
            r.cur.push_y += dy;
            r.changed();
        }
    }

    /// Speed this side took off the copy (a stun).
    pub fn slow(&mut self, net_id: NetId, amount: f32) {
        if amount.is_finite() && amount > EPS {
            let r = self.rep(net_id);
            r.cur.slow += amount;
            r.changed();
        }
    }

    /// A hit flash this side made on the copy.
    pub fn flash(&mut self, net_id: NetId) {
        let r = self.rep(net_id);
        r.cur.flashes = r.cur.flashes.saturating_add(1);
        r.changed();
    }

    /// Marks this side just raised on the copy (0 for the others), sent with
    /// a new counter. Marks decay by themselves on every game.
    pub fn marks(&mut self, net_id: NetId, m: Marks) {
        if !m.finite() || m == Marks::default() {
            return;
        }
        let r = self.rep(net_id);
        r.cur.marks = m;
        r.cur.marks_seq = next_seq(r.cur.marks_seq);
        r.changed();
    }

    /// The owner's ack for `me` (others' acks and older ones are ignored).
    pub fn on_ack(&mut self, me: PeerId, peer: PeerId, frame: u32, entries: &[StatusAckEntry]) {
        if peer != me {
            return;
        }
        for e in entries {
            let Some(r) = self.enemies.get_mut(&e.net_id) else {
                continue;
            };
            if r.ack_frame.is_some_and(|f| f > frame) {
                continue;
            }
            r.acked = *e;
            r.ack_frame = Some(frame);
        }
    }

    /// Reports due at `frame`: changed entries, and unacknowledged ones
    /// every [`RESEND_FRAMES`].
    pub fn take_reports(&mut self, by: PeerId, frame: u64) -> Vec<StatusMsg> {
        let mut entries = Vec::new();
        for r in self.enemies.values_mut() {
            let due = match r.sent {
                None => r.pending(),
                Some((at, sent)) => {
                    sent != r.cur || (r.pending() && frame.saturating_sub(at) >= RESEND_FRAMES)
                }
            };
            if due {
                r.sent = Some((frame, r.cur));
                entries.push(r.cur);
            }
        }
        entries
            .chunks(StatusMsg::MAX_ENTRIES)
            .map(|c| StatusMsg::Report {
                by,
                entries: c.to_vec(),
            })
            .collect()
    }

    /// Enemies with something not acknowledged yet.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.enemies.values().filter(|r| r.pending()).count()
    }

    /// The enemy is gone: forget it.
    pub fn forget(&mut self, net_id: NetId) {
        self.enemies.remove(&net_id);
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.enemies.clear();
    }
}

/// What the owner applies for one entry: increases over what it applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatusApply {
    pub net_id: NetId,
    pub by: PeerId,
    pub push_x: f32,
    pub push_y: f32,
    pub slow: f32,
    pub flashes: u32,
    /// New marks to merge (by maximum) with the enemy's own.
    pub marks: Option<Marks>,
}

impl StatusApply {
    /// Nothing to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.push_x.abs() <= EPS
            && self.push_y.abs() <= EPS
            && self.slow <= EPS
            && self.flashes == 0
            && self.marks.is_none()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Acct {
    applied: StatusAckEntry,
    changed: u32,
}

/// The owner's side: per (peer, enemy) what it has applied.
#[derive(Debug, Clone, Default)]
pub struct StatusLedger {
    accts: BTreeMap<(PeerId, NetId), Acct>,
    frame: u32,
}

/// A reported total over what was applied (`None`: old news or not a
/// number).
fn over(total: f32, applied: f32) -> Option<f32> {
    (total.is_finite() && (total - applied).abs() > EPS).then_some(total - applied)
}

impl StatusLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One report entry from `by`: what to apply now.
    pub fn report(&mut self, by: PeerId, e: &StatusEntry) -> StatusApply {
        let frame = self.frame;
        let a = self.accts.entry((by, e.net_id)).or_default();
        a.applied.net_id = e.net_id;
        a.changed = frame;
        let mut out = StatusApply {
            net_id: e.net_id,
            by,
            push_x: 0.0,
            push_y: 0.0,
            slow: 0.0,
            flashes: 0,
            marks: None,
        };
        // Knockback only from a newer report.
        let fresh = newer(e.seq, a.applied.seq);
        if fresh {
            if let Some(d) = over(e.push_x, a.applied.push_x) {
                out.push_x = d;
                a.applied.push_x = e.push_x;
            }
            if let Some(d) = over(e.push_y, a.applied.push_y) {
                out.push_y = d;
                a.applied.push_y = e.push_y;
            }
        }
        if let Some(d) = over(e.slow, a.applied.slow).filter(|d| *d > 0.0) {
            out.slow = d;
            a.applied.slow = e.slow;
        }
        if e.flashes > a.applied.flashes {
            out.flashes = e.flashes - a.applied.flashes;
            a.applied.flashes = e.flashes;
        }
        // Marks: a newer counter (wrapping) brings new values.
        if newer(e.marks_seq, a.applied.marks_seq) && e.marks.finite() {
            out.marks = Some(e.marks);
            a.applied.marks_seq = e.marks_seq;
        }
        if fresh {
            a.applied.seq = e.seq;
        }
        out
    }

    /// Advance one owner frame; the acks to send for enemies changed within
    /// [`ACK_HOLD_FRAMES`] that `known` still knows.
    pub fn acks(&mut self, known: impl Fn(NetId) -> bool) -> Vec<StatusMsg> {
        self.frame = self.frame.wrapping_add(1);
        let frame = self.frame;
        let mut per_peer: BTreeMap<PeerId, Vec<StatusAckEntry>> = BTreeMap::new();
        for (&(peer, net_id), a) in &self.accts {
            if frame.wrapping_sub(a.changed) <= ACK_HOLD_FRAMES && known(net_id) {
                per_peer.entry(peer).or_default().push(a.applied);
            }
        }
        let mut out = Vec::new();
        for (peer, entries) in per_peer {
            for c in entries.chunks(StatusMsg::MAX_ENTRIES) {
                out.push(StatusMsg::Ack {
                    peer,
                    frame,
                    entries: c.to_vec(),
                });
            }
        }
        out
    }

    /// The enemy is gone: drop its accounts.
    pub fn forget(&mut self, net_id: NetId) {
        self.accts.retain(|&(_, n), _| n != net_id);
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.accts.clear();
    }
}

/// The owner's held entries: they wait while it is frozen or the enemy is
/// deactivated (it takes no writes), and apply at the first running frame.
#[derive(Debug, Clone, Default)]
pub struct StatusHold {
    held: Vec<(PeerId, StatusEntry)>,
}

impl StatusHold {
    /// This frame's entries to apply (none while `frozen`), newest per
    /// (peer, enemy).
    pub fn frame(
        &mut self,
        new: Vec<(PeerId, StatusEntry)>,
        frozen: bool,
    ) -> Vec<(PeerId, StatusEntry)> {
        let mut all = std::mem::take(&mut self.held);
        all.extend(new);
        let all = newest(all);
        if frozen {
            self.held = all;
            return Vec::new();
        }
        all
    }

    /// Entries that could not apply this frame: retried next frame.
    pub fn hold(&mut self, again: Vec<(PeerId, StatusEntry)>) {
        self.held.extend(again);
        self.held = newest(std::mem::take(&mut self.held));
    }

    /// Entries waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Nothing waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::Message;
    use crate::rng::Rng;

    const ME: PeerId = PeerId(1);

    /// The entries of the reports in `m`.
    fn reported(m: &[StatusMsg]) -> Vec<StatusEntry> {
        m.iter()
            .flat_map(|m| match m {
                StatusMsg::Report { entries, .. } => entries.clone(),
                StatusMsg::Ack { .. } => Vec::new(),
            })
            .collect()
    }

    fn acks_for(l: &mut StatusLedger, r: &mut StatusReporter) {
        for m in l.acks(|_| true) {
            if let StatusMsg::Ack {
                peer,
                frame,
                entries,
            } = m
            {
                r.on_ack(ME, peer, frame, &entries);
            }
        }
    }

    #[test]
    fn totals_apply_once_and_marks_merge_by_maximum() {
        let mut r = StatusReporter::new();
        let mut l = StatusLedger::new();
        r.push(7, 3.0, -1.0);
        r.slow(7, 0.5);
        r.flash(7);
        r.marks(
            7,
            Marks {
                vulnerability_mult: 1.2,
                vulnerability_count: 60.0,
                ..Marks::default()
            },
        );
        let reps = r.take_reports(ME, 1);
        assert_eq!(reps.len(), 1);
        let entries = reported(&reps);
        let a = l.report(ME, &entries[0]);
        assert_eq!((a.push_x, a.push_y, a.slow, a.flashes), (3.0, -1.0, 0.5, 1));
        assert_eq!(a.marks.map(|m| m.vulnerability_count), Some(60.0));
        // The same report again (a resend): nothing new.
        assert!(l.report(ME, &entries[0]).is_empty());
        // More of each: only the increase.
        r.push(7, 1.0, 1.0);
        r.flash(7);
        let entries = reported(&r.take_reports(ME, 2));
        let a = l.report(ME, &entries[0]);
        assert_eq!((a.push_x, a.push_y, a.slow, a.flashes), (1.0, 1.0, 0.0, 1));
        assert!(a.marks.is_none(), "marks unchanged");
        // Merge by maximum: each mark the larger.
        let own = Marks {
            vulnerability_mult: 1.5,
            vulnerability_count: 10.0,
            sat_fire_hits: 2.0,
            blitzed: 0.0,
        };
        let m = own.max(Marks {
            vulnerability_mult: 1.2,
            vulnerability_count: 60.0,
            ..Marks::default()
        });
        assert_eq!(
            (m.vulnerability_mult, m.vulnerability_count, m.sat_fire_hits),
            (1.5, 60.0, 2.0)
        );
        // Acked: nothing pending, nothing resent.
        acks_for(&mut l, &mut r);
        // No marks (nothing this side raised) are no news; a new one is,
        // even at the value sent before (the mark decayed and was renewed).
        r.marks(7, Marks::default());
        assert!(r.take_reports(ME, 20).is_empty());
        r.marks(
            7,
            Marks {
                vulnerability_mult: 1.2,
                vulnerability_count: 60.0,
                ..Marks::default()
            },
        );
        let entries = reported(&r.take_reports(ME, 21));
        assert_eq!(
            l.report(ME, &entries[0])
                .marks
                .map(|m| m.vulnerability_count),
            Some(60.0)
        );
        acks_for(&mut l, &mut r);
        assert_eq!(r.in_flight(), 0);
        assert!(r.take_reports(ME, 30).is_empty());
    }

    #[test]
    fn a_lossy_link_delivers_every_total_once() {
        let mut rng = Rng::new(9);
        let mut r = StatusReporter::new();
        let mut l = StatusLedger::new();
        let (mut px, mut flashes, mut slow) = (0.0f32, 0u32, 0.0f32);
        for frame in 0..600u64 {
            if frame < 300 {
                let n = (frame % 3) as NetId;
                r.push(n, 0.25, 0.0);
                r.flash(n);
                r.slow(n, 0.125);
            }
            for m in r.take_reports(ME, frame) {
                // 30 % lost, and a duplicate now and then.
                if rng.chance(0.3) {
                    continue;
                }
                let StatusMsg::Report { entries, .. } = m else {
                    continue;
                };
                for _ in 0..if rng.chance(0.1) { 2 } else { 1 } {
                    for e in &entries {
                        let a = l.report(ME, e);
                        px += a.push_x;
                        flashes += a.flashes;
                        slow += a.slow;
                    }
                }
            }
            for m in l.acks(|_| true) {
                if rng.chance(0.3) {
                    continue;
                }
                if let StatusMsg::Ack {
                    peer,
                    frame,
                    entries,
                } = m
                {
                    r.on_ack(ME, peer, frame, &entries);
                }
            }
        }
        assert!((px - 75.0).abs() < 1e-3, "{px}");
        assert_eq!(flashes, 300);
        assert!((slow - 37.5).abs() < 1e-3, "{slow}");
        assert_eq!(r.in_flight(), 0);
    }

    #[test]
    fn a_late_older_report_does_not_undo_knockback() {
        let mut r = StatusReporter::new();
        let mut l = StatusLedger::new();
        r.push(4, 2.0, 0.0);
        let old = reported(&r.take_reports(ME, 1));
        r.push(4, -3.0, 1.0);
        r.flash(4);
        let new = reported(&r.take_reports(ME, 2));
        // The newer one arrives first, the older one after it.
        let a = l.report(ME, &new[0]);
        assert_eq!((a.push_x, a.push_y, a.flashes), (-1.0, 1.0, 1));
        assert!(
            l.report(ME, &old[0]).is_empty(),
            "late older report ignored"
        );
        // Held entries: the newest per (peer, enemy) carries everything.
        let held = newest(vec![(ME, new[0]), (ME, old[0]), (PeerId(3), old[0])]);
        assert_eq!(held, vec![(ME, new[0]), (PeerId(3), old[0])]);
        let held = newest(vec![(ME, old[0]), (ME, new[0])]);
        assert_eq!(held, vec![(ME, new[0])]);
    }

    #[test]
    fn held_while_frozen_and_while_deactivated_then_applied_once() {
        let mut r = StatusReporter::new();
        let mut l = StatusLedger::new();
        let mut hold = StatusHold::default();
        // The owner applies what is active; holds what is not.
        let mut applied = (0.0f32, 0u32);
        let mut step = |hold: &mut StatusHold,
                        l: &mut StatusLedger,
                        new: Vec<(PeerId, StatusEntry)>,
                        frozen: bool,
                        active: bool| {
            let now = hold.frame(new, frozen);
            if !active {
                hold.hold(now);
                return applied;
            }
            for (by, e) in now {
                let a = l.report(by, &e);
                applied.0 += a.push_x;
                applied.1 += a.flashes;
            }
            applied
        };
        let with_by = |m: Vec<StatusMsg>| -> Vec<(PeerId, StatusEntry)> {
            reported(&m).into_iter().map(|e| (ME, e)).collect()
        };
        // Reports arrive while the owner is frozen: nothing applies.
        r.push(5, 1.0, 0.0);
        r.flash(5);
        let _ = step(
            &mut hold,
            &mut l,
            with_by(r.take_reports(ME, 1)),
            true,
            true,
        );
        r.push(5, 2.0, 0.0);
        let got = step(
            &mut hold,
            &mut l,
            with_by(r.take_reports(ME, 2)),
            true,
            true,
        );
        assert_eq!(got, (0.0, 0));
        assert_eq!(hold.len(), 1, "the newest per enemy");
        // Running, but the enemy is deactivated: still held.
        let got = step(&mut hold, &mut l, Vec::new(), false, false);
        assert_eq!((got, hold.len()), ((0.0, 0), 1));
        // The enemy is back: everything applies once; a resend adds nothing.
        let got = step(&mut hold, &mut l, Vec::new(), false, true);
        assert!(hold.is_empty());
        assert_eq!(got, (3.0, 1));
        let resend = reported(&r.take_reports(ME, 2 + RESEND_FRAMES));
        assert!(resend.iter().all(|e| l.report(ME, e).is_empty()));
    }

    #[test]
    fn the_messages_fit_a_datagram() {
        let e = StatusEntry {
            net_id: NetId::MAX,
            seq: u16::MAX,
            push_x: f32::MAX,
            push_y: f32::MIN,
            slow: f32::MAX,
            flashes: u32::MAX,
            marks_seq: u16::MAX,
            marks: Marks {
                vulnerability_mult: f32::MAX,
                vulnerability_count: f32::MAX,
                sat_fire_hits: f32::MAX,
                blitzed: f32::MAX,
            },
        };
        let m = Message::Status(StatusMsg::Report {
            by: PeerId(3),
            entries: vec![e; StatusMsg::MAX_ENTRIES],
        });
        let bytes = m.encode().unwrap();
        assert_eq!(bytes[0], 39);
        assert_eq!(Message::decode(&bytes).unwrap(), m);
        assert_eq!(m.channel(), crate::msg::Channel::Unreliable);
        let a = Message::Status(StatusMsg::Ack {
            peer: PeerId(3),
            frame: u32::MAX,
            entries: vec![
                StatusAckEntry {
                    net_id: NetId::MAX,
                    seq: u16::MAX,
                    push_x: f32::MAX,
                    push_y: f32::MAX,
                    slow: f32::MAX,
                    flashes: u32::MAX,
                    marks_seq: u16::MAX,
                };
                StatusMsg::MAX_ENTRIES
            ],
        });
        assert!(a.encode().is_ok());
    }
}
