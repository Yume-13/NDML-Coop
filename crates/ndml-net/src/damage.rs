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

//! Shared damage as a plain number.
//!
//! The side that does not own an enemy keeps a cumulative total of its own
//! damage per enemy ([`Reporter`]) and sends it until acknowledged: a total
//! is idempotent, so no reliable channel is needed. The owner ([`Ledger`])
//! applies only the increase and acks with the hull afterwards. The shooter
//! shows the owner's hull minus its unapplied damage and claims a kill at 0;
//! claims are numbered per (peer, enemy), and a repeated number is the same
//! claim in flight.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use crate::msg::{AckEntry, DamageAck, DamageEntry, DamageReport, Message, NetId, PeerId};

/// Unacknowledged totals are sent again after this many frames.
pub const RESEND_FRAMES: u64 = 6;
/// The owner keeps acknowledging an enemy this many frames after a change.
pub const ACK_HOLD_FRAMES: u32 = 30;
/// Amounts below this are noise (float rounding of hull writes).
pub const EPS: f32 = 1e-3;

#[derive(Debug, Clone, Copy, Default)]
struct Rep {
    total: f32,
    /// Applied by the owner (from the newest ack).
    acked: f32,
    /// The owner's hull in the newest ack, with that ack's frame.
    ack_hull: Option<(f32, u32)>,
    /// Frame and total of the last send.
    sent: Option<(u64, f32)>,
    /// The open kill claim's number (0: none).
    claim: u16,
    /// The owner has judged the open claim.
    judged: bool,
    /// Own damage in flight since all earlier damage was applied.
    burst: Option<Burst>,
}

/// The start of own damage in flight (see [`Reporter::view`]).
#[derive(Debug, Clone, Copy)]
struct Burst {
    /// The owner's hull as seen when the damage began. It holds all earlier
    /// damage and none of this.
    base: f32,
    /// The total before it.
    from: f32,
}

impl Rep {
    fn unacked(&self) -> f32 {
        (self.total - self.acked).max(0.0)
    }
}

/// The shooter's side: own damage per enemy, what the owner has applied, and
/// the reports to send.
#[derive(Debug, Clone, Default)]
pub struct Reporter {
    enemies: BTreeMap<NetId, Rep>,
    /// The last claim number given out per enemy, kept after `forget` and
    /// `clear`: a number never goes back.
    last: BTreeMap<NetId, u16>,
}

impl Reporter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Local damage to `net_id`'s copy (ignored unless positive and finite).
    /// `seen_hull`: the owner's hull from its newest snapshot at the time.
    pub fn add(&mut self, net_id: NetId, amount: f32, seen_hull: f32) {
        if amount.is_finite() && amount > 0.0 {
            let r = self.enemies.entry(net_id).or_default();
            if r.unacked() <= EPS {
                r.burst = Some(Burst {
                    base: seen_hull,
                    from: r.total,
                });
            }
            r.total += amount;
        }
    }

    /// Everything this peer has removed from `net_id`.
    #[must_use]
    pub fn total(&self, net_id: NetId) -> f32 {
        self.enemies.get(&net_id).map_or(0.0, |r| r.total)
    }

    /// Own damage the owner has not applied yet.
    #[must_use]
    pub fn unacked(&self, net_id: NetId) -> f32 {
        self.enemies.get(&net_id).map_or(0.0, Rep::unacked)
    }

    /// The hull to show for `net_id`, given the owner's hull from its newest
    /// snapshot. While own damage is in flight the base is a hull that holds
    /// none of it: the newest ack that applied some of it, else the owner's
    /// hull when it began. The snapshot may already hold the damage in flight.
    #[must_use]
    pub fn view(&self, net_id: NetId, snapshot_hull: f32) -> f32 {
        let Some(r) = self.enemies.get(&net_id) else {
            return snapshot_hull;
        };
        let pending = r.unacked();
        if pending <= EPS {
            return snapshot_hull;
        }
        let fresh_ack = r
            .ack_hull
            .filter(|_| r.burst.is_none_or(|b| r.acked > b.from + EPS));
        let base = match (fresh_ack, r.burst) {
            (Some((h, _)), _) => h,
            (None, Some(b)) => b.base,
            (None, None) => snapshot_hull,
        };
        base - pending
    }

    /// This peer's copy of `net_id` died here; the next reports say so.
    pub fn claim_kill(&mut self, net_id: NetId) {
        let last = self.last.entry(net_id).or_default();
        *last = last.wrapping_add(1).max(1);
        let r = self.enemies.entry(net_id).or_default();
        r.claim = *last;
        r.judged = false;
        r.sent = None;
    }

    /// The owner refused the claim: drop it, keep the total.
    pub fn unclaim(&mut self, net_id: NetId) {
        if let Some(r) = self.enemies.get_mut(&net_id) {
            r.claim = 0;
            r.judged = false;
            r.sent = None;
        }
    }

    /// Whether a kill of `net_id` was claimed.
    #[must_use]
    pub fn claimed(&self, net_id: NetId) -> bool {
        self.enemies.get(&net_id).is_some_and(|r| r.claim > 0)
    }

    /// The number of the latest claim on `net_id` (open or not; 0 none).
    #[must_use]
    pub fn last_claim(&self, net_id: NetId) -> u16 {
        self.last.get(&net_id).copied().unwrap_or(0)
    }

    /// The owner's ack for `me` (others' acks are ignored; older ones too).
    pub fn on_ack(&mut self, me: PeerId, ack: &DamageAck) {
        if ack.peer != me {
            return;
        }
        for e in &ack.entries {
            let Some(r) = self.enemies.get_mut(&e.net_id) else {
                continue;
            };
            if r.ack_hull.is_some_and(|(_, f)| f > ack.frame) {
                continue;
            }
            r.acked = e.applied;
            r.ack_hull = Some((e.hull, ack.frame));
            r.judged |= r.claim > 0 && e.judged >= r.claim;
        }
    }

    /// Reports due at `frame`: changed totals, and unacknowledged ones every
    /// [`RESEND_FRAMES`].
    pub fn take_reports(&mut self, by: PeerId, frame: u64) -> Vec<Message> {
        let mut entries = Vec::new();
        for (&net_id, r) in &mut self.enemies {
            let pending = r.unacked() > EPS || (r.claim > 0 && !r.judged);
            let due = match r.sent {
                None => r.total > EPS || r.claim > 0,
                Some((at, total)) => {
                    (r.total - total).abs() > EPS
                        || (pending && frame.saturating_sub(at) >= RESEND_FRAMES)
                }
            };
            if due {
                r.sent = Some((frame, r.total));
                entries.push(DamageEntry {
                    net_id,
                    total: r.total,
                    claim: r.claim,
                });
            }
        }
        entries
            .chunks(DamageReport::MAX_ENTRIES)
            .map(|c| {
                Message::DamageReport(DamageReport {
                    by,
                    entries: c.to_vec(),
                })
            })
            .collect()
    }

    /// The enemy is gone for good: forget it.
    pub fn forget(&mut self, net_id: NetId) {
        self.enemies.remove(&net_id);
    }

    pub fn clear(&mut self) {
        self.enemies.clear();
    }

    /// The owner sent its world again: totals and open claims stay and go out
    /// again at once; the view waits for a fresh ack.
    pub fn resync(&mut self) {
        for r in self.enemies.values_mut() {
            r.sent = None;
            r.ack_hull = None;
        }
    }

    /// Every enemy with a total or a claim here.
    #[must_use]
    pub fn net_ids(&self) -> Vec<NetId> {
        self.enemies.keys().copied().collect()
    }

    /// Enemies with own damage in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.enemies.values().filter(|r| r.unacked() > EPS).count()
    }
}

/// What a report entry means to the owner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Applied {
    /// Hull to remove now (0 when nothing new).
    pub delta: f32,
    /// A kill claim the owner has not judged yet (0: none).
    pub claim: u16,
}

#[derive(Debug, Clone, Copy, Default)]
struct Acct {
    applied: f32,
    /// Frame of the last change (acks go out until it is old).
    changed: u32,
    /// The highest claim number judged (confirmed or refused; 0 none).
    judged: u16,
}

/// The owner's side: per (peer, enemy) what it has applied.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    accts: BTreeMap<(PeerId, NetId), Acct>,
    frame: u32,
}

impl Ledger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One report entry from `by`: the increase to apply now, and any open
    /// claim.
    pub fn report(&mut self, by: PeerId, e: &DamageEntry) -> Applied {
        let frame = self.frame;
        let a = self.accts.entry((by, e.net_id)).or_default();
        let delta = if e.total.is_finite() && e.total > a.applied + EPS {
            let d = e.total - a.applied;
            a.applied = e.total;
            d
        } else {
            0.0
        };
        a.changed = frame;
        let claim = if e.claim > a.judged { e.claim } else { 0 };
        Applied { delta, claim }
    }

    /// Claim `claim` of `by` on `net_id` (and older ones) has been judged.
    pub fn judged(&mut self, by: PeerId, net_id: NetId, claim: u16) {
        let frame = self.frame;
        if let Some(a) = self.accts.get_mut(&(by, net_id)) {
            a.judged = a.judged.max(claim);
            a.changed = frame;
        }
    }

    /// Everything `by` has had applied to `net_id`.
    #[must_use]
    pub fn applied(&self, by: PeerId, net_id: NetId) -> f32 {
        self.accts.get(&(by, net_id)).map_or(0.0, |a| a.applied)
    }

    /// Advance one owner frame; returns the acks for every account changed in
    /// the last [`ACK_HOLD_FRAMES`], with the hull now (unknown enemies skipped).
    pub fn acks(&mut self, hull_of: impl Fn(NetId) -> Option<f32>) -> Vec<Message> {
        self.frame = self.frame.wrapping_add(1);
        let frame = self.frame;
        let mut per_peer: BTreeMap<PeerId, Vec<AckEntry>> = BTreeMap::new();
        for (&(peer, net_id), a) in &self.accts {
            if frame.wrapping_sub(a.changed) > ACK_HOLD_FRAMES {
                continue;
            }
            if let Some(hull) = hull_of(net_id) {
                per_peer.entry(peer).or_default().push(AckEntry {
                    net_id,
                    applied: a.applied,
                    hull,
                    judged: a.judged,
                });
            }
        }
        let mut out = Vec::new();
        for (peer, entries) in per_peer {
            for c in entries.chunks(DamageAck::MAX_ENTRIES) {
                out.push(Message::DamageAck(DamageAck {
                    peer,
                    frame,
                    entries: c.to_vec(),
                }));
            }
        }
        out
    }

    /// The enemy is gone: drop its accounts.
    pub fn forget(&mut self, net_id: NetId) {
        self.accts.retain(|&(_, n), _| n != net_id);
    }

    pub fn clear(&mut self) {
        self.accts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: PeerId = PeerId(1);

    fn entries(m: &[Message]) -> Vec<DamageEntry> {
        m.iter()
            .flat_map(|m| match m {
                Message::DamageReport(r) => r.entries.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn ack_of(m: &[Message]) -> DamageAck {
        m.iter()
            .find_map(|m| match m {
                Message::DamageAck(a) => Some(a.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn totals_are_sent_on_change_and_repeated_until_acked() {
        let mut r = Reporter::new();
        assert!(r.take_reports(ME, 0).is_empty());
        r.add(5, 10.0, 100.0);
        r.add(5, f32::NAN, 100.0);
        r.add(5, -3.0, 100.0);
        let e = entries(&r.take_reports(ME, 1));
        assert_eq!(e.len(), 1);
        assert!((e[0].total - 10.0).abs() < 1e-6 && e[0].claim == 0);
        // Nothing new, not yet due again.
        assert!(r.take_reports(ME, 2).is_empty());
        // Unacked: repeated after RESEND_FRAMES.
        assert_eq!(entries(&r.take_reports(ME, 1 + RESEND_FRAMES)).len(), 1);
        // Acked: quiet.
        r.on_ack(
            ME,
            &DamageAck {
                peer: ME,
                frame: 3,
                entries: vec![AckEntry {
                    net_id: 5,
                    applied: 10.0,
                    hull: 90.0,
                    judged: 0,
                }],
            },
        );
        assert!(r.take_reports(ME, 100).is_empty());
        r.add(5, 2.5, 90.0);
        let e = entries(&r.take_reports(ME, 101));
        assert!((e[0].total - 12.5).abs() < 1e-6);
    }

    #[test]
    fn the_view_subtracts_only_unapplied_own_damage() {
        let mut r = Reporter::new();
        assert!((r.view(5, 100.0) - 100.0).abs() < 1e-6);
        r.add(5, 30.0, 100.0);
        // Nothing acked yet: the snapshot's hull lacks this side's 30.
        assert!((r.view(5, 100.0) - 70.0).abs() < 1e-6);
        // The owner applied 20 of it; its hull then was 75 (it took 5 itself).
        r.on_ack(
            ME,
            &DamageAck {
                peer: ME,
                frame: 10,
                entries: vec![AckEntry {
                    net_id: 5,
                    applied: 20.0,
                    hull: 75.0,
                    judged: 0,
                }],
            },
        );
        assert!((r.view(5, 999.0) - 65.0).abs() < 1e-6);
        // An older ack changes nothing; another peer's neither.
        r.on_ack(
            ME,
            &DamageAck {
                peer: ME,
                frame: 9,
                entries: vec![AckEntry {
                    net_id: 5,
                    applied: 0.0,
                    hull: 100.0,
                    judged: 0,
                }],
            },
        );
        r.on_ack(
            ME,
            &DamageAck {
                peer: PeerId(2),
                frame: 11,
                entries: vec![AckEntry {
                    net_id: 5,
                    applied: 0.0,
                    hull: 1.0,
                    judged: 0,
                }],
            },
        );
        assert!((r.view(5, 999.0) - 65.0).abs() < 1e-6);
        // All applied: the snapshot's hull is right.
        r.on_ack(
            ME,
            &DamageAck {
                peer: ME,
                frame: 12,
                entries: vec![AckEntry {
                    net_id: 5,
                    applied: 30.0,
                    hull: 65.0,
                    judged: 0,
                }],
            },
        );
        assert!((r.view(5, 60.0) - 60.0).abs() < 1e-6);
        assert_eq!(r.in_flight(), 0);
    }

    #[test]
    fn kill_claims_ride_on_the_reports() {
        let mut r = Reporter::new();
        r.add(9, 50.0, 50.0);
        let _ = r.take_reports(ME, 1);
        r.claim_kill(9);
        assert!(r.claimed(9));
        let e = entries(&r.take_reports(ME, 2));
        assert_eq!(e[0].claim, 1);
        // Repeated until the owner says it judged the claim.
        r.on_ack(
            ME,
            &DamageAck {
                peer: ME,
                frame: 1,
                entries: vec![AckEntry {
                    net_id: 9,
                    applied: 50.0,
                    hull: 0.0,
                    judged: 0,
                }],
            },
        );
        assert_eq!(entries(&r.take_reports(ME, 2 + RESEND_FRAMES))[0].claim, 1);
        r.on_ack(
            ME,
            &DamageAck {
                peer: ME,
                frame: 2,
                entries: vec![AckEntry {
                    net_id: 9,
                    applied: 50.0,
                    hull: 0.0,
                    judged: 1,
                }],
            },
        );
        assert!(r.take_reports(ME, 40).is_empty());
        // Refused, then claimed again: a new number, open until judged.
        r.unclaim(9);
        assert!(!r.claimed(9) && r.last_claim(9) == 1);
        r.claim_kill(9);
        assert_eq!(entries(&r.take_reports(ME, 41))[0].claim, 2);
        // An ack naming only the old number does not answer it.
        r.on_ack(
            ME,
            &DamageAck {
                peer: ME,
                frame: 3,
                entries: vec![AckEntry {
                    net_id: 9,
                    applied: 50.0,
                    hull: 0.0,
                    judged: 1,
                }],
            },
        );
        assert_eq!(entries(&r.take_reports(ME, 41 + RESEND_FRAMES))[0].claim, 2);
        r.forget(9);
        assert!(!r.claimed(9));
        assert!(r.take_reports(ME, 50).is_empty());
        // The numbers never go back.
        r.clear();
        r.claim_kill(9);
        assert_eq!(entries(&r.take_reports(ME, 51))[0].claim, 3);
    }

    #[test]
    fn the_owner_applies_increases_once_whatever_the_order() {
        let mut l = Ledger::new();
        let e = |total, claim| DamageEntry {
            net_id: 4,
            total,
            claim,
        };
        assert!((l.report(ME, &e(10.0, 0)).delta - 10.0).abs() < 1e-6);
        // Duplicate and reordered (older) totals apply nothing.
        assert!(l.report(ME, &e(10.0, 0)).delta.abs() < 1e-6);
        assert!(l.report(ME, &e(4.0, 0)).delta.abs() < 1e-6);
        assert!((l.report(ME, &e(25.0, 0)).delta - 15.0).abs() < 1e-6);
        // Per peer.
        assert!((l.report(PeerId(2), &e(5.0, 0)).delta - 5.0).abs() < 1e-6);
        assert!((l.applied(ME, 4) - 25.0).abs() < 1e-6);
        // Once judged, a repeat of that number is the claim in flight, even
        // with more damage; only a higher number is a new claim.
        let a = l.report(ME, &e(30.0, 1));
        assert!(a.claim == 1 && (a.delta - 5.0).abs() < 1e-6);
        l.judged(ME, 4, 1);
        assert_eq!(l.report(ME, &e(30.0, 1)).claim, 0);
        assert_eq!(l.report(ME, &e(31.0, 1)).claim, 0);
        assert_eq!(l.report(ME, &e(31.0, 2)).claim, 2);
    }

    #[test]
    fn acks_follow_changes_for_a_while() {
        let mut l = Ledger::new();
        let _ = l.report(
            ME,
            &DamageEntry {
                net_id: 4,
                total: 10.0,
                claim: 0,
            },
        );
        let a = ack_of(&l.acks(|_| Some(90.0)));
        assert_eq!(a.peer, ME);
        assert_eq!(a.entries.len(), 1);
        assert!((a.entries[0].applied - 10.0).abs() < 1e-6);
        assert!((a.entries[0].hull - 90.0).abs() < 1e-6);
        for _ in 0..ACK_HOLD_FRAMES {
            let _ = l.acks(|_| Some(90.0));
        }
        assert!(l.acks(|_| Some(90.0)).is_empty(), "quiet once old");
        // Unknown enemies are skipped; forgotten ones too.
        let _ = l.report(
            ME,
            &DamageEntry {
                net_id: 4,
                total: 11.0,
                claim: 0,
            },
        );
        assert!(l.acks(|_| None).is_empty());
        l.forget(4);
        assert!((l.applied(ME, 4)).abs() < 1e-6);
    }

    #[test]
    fn a_resync_keeps_totals_so_the_first_hit_after_it_counts() {
        // 300 applied before a long cut: after the resync the next hit of 50
        // must reach the owner at once.
        let mut r = Reporter::new();
        let mut l = Ledger::new();
        r.add(9, 300.0, 1000.0);
        for e in entries(&r.take_reports(ME, 0)) {
            assert!((l.report(ME, &e).delta - 300.0).abs() < 1e-3);
        }
        r.on_ack(ME, &ack_of(&l.acks(|_| Some(700.0))));
        r.resync();
        // Sent again at once (once: acknowledged already, nothing pending).
        let again = entries(&r.take_reports(ME, 1));
        assert_eq!(again.len(), 1);
        assert!(l.report(ME, &again[0]).delta.abs() < 1e-3, "nothing new");
        assert!(entries(&r.take_reports(ME, 1 + RESEND_FRAMES)).is_empty());
        r.add(9, 50.0, 700.0);
        let e = entries(&r.take_reports(ME, 2 + RESEND_FRAMES));
        assert!(
            (l.report(ME, &e[0]).delta - 50.0).abs() < 1e-3,
            "the first hit counts"
        );
        // The view waits for a fresh ack.
        assert!((r.view(9, 700.0) - 650.0).abs() < 1e-3);
    }

    #[test]
    fn a_resync_keeps_an_open_claim_and_sends_it_again() {
        let mut r = Reporter::new();
        r.add(4, 80.0, 80.0);
        r.claim_kill(4);
        let first = entries(&r.take_reports(ME, 0));
        assert_eq!(first[0].claim, 1);
        r.resync();
        let again = entries(&r.take_reports(ME, 1));
        assert_eq!(again.len(), 1);
        assert_eq!(
            (again[0].net_id, again[0].claim),
            (4, 1),
            "the same claim, not a new one"
        );
        assert_eq!(r.net_ids(), vec![4]);
    }

    #[test]
    fn reporter_and_ledger_agree_over_a_lossy_link() {
        // A third of the reports and half the acks lost: the totals agree.
        let mut rng = crate::rng::Rng::new(7);
        let mut r = Reporter::new();
        let mut l = Ledger::new();
        let mut owner_hull = 500.0_f32;
        for f in 0..200u64 {
            if f < 100 {
                r.add(1, 3.0, owner_hull);
            }
            let msgs = r.take_reports(ME, f);
            if rng.chance(0.67) {
                for e in entries(&msgs) {
                    owner_hull -= l.report(ME, &e).delta;
                }
            }
            for m in l.acks(|_| Some(owner_hull)) {
                if let Message::DamageAck(a) = m
                    && rng.chance(0.5)
                {
                    r.on_ack(ME, &a);
                }
            }
        }
        assert!((owner_hull - 200.0).abs() < 1e-3, "{owner_hull}");
        assert!((r.view(1, owner_hull) - 200.0).abs() < 1e-3);
    }

    fn ack(frame: u32, net_id: NetId, applied: f32, hull: f32) -> DamageAck {
        DamageAck {
            peer: ME,
            frame,
            entries: vec![AckEntry {
                net_id,
                applied,
                hull,
                judged: 0,
            }],
        }
    }

    #[test]
    fn a_snapshot_holding_the_damage_in_flight_does_not_count_it_twice() {
        // An enemy at 40 takes a hit of 30. The owner's snapshot (hull 10)
        // comes before its ack: the view stays 10, not 10 - 30.
        let mut r = Reporter::new();
        r.add(5, 30.0, 40.0);
        assert!((r.view(5, 40.0) - 10.0).abs() < 1e-4);
        assert!((r.view(5, 10.0) - 10.0).abs() < 1e-4, "not -20");
        r.on_ack(ME, &ack(40, 5, 30.0, 10.0));
        assert!((r.view(5, 10.0) - 10.0).abs() < 1e-4);
        // A second hit is in flight on top of the ack's hull.
        r.add(5, 30.0, 10.0);
        assert!(r.view(5, 10.0) < 0.0);
    }

    #[test]
    fn an_ack_from_before_the_damage_in_flight_is_not_its_base() {
        // The last ack (applied 85, hull 5) predates a hit of 25 on an enemy
        // the owner has repaired to 200: the view is 175, not 5 - 25.
        let mut r = Reporter::new();
        r.add(7, 85.0, 90.0);
        r.on_ack(ME, &ack(100, 7, 85.0, 5.0));
        assert!((r.view(7, 5.0) - 5.0).abs() < 1e-4);
        r.add(7, 25.0, 200.0);
        assert!((r.view(7, 200.0) - 175.0).abs() < 1e-4, "not -20");
        // The same ack repeated changes nothing. The ack of the new hit is the
        // base then.
        r.on_ack(ME, &ack(101, 7, 85.0, 200.0));
        assert!((r.view(7, 175.0) - 175.0).abs() < 1e-4);
        r.on_ack(ME, &ack(130, 7, 110.0, 175.0));
        assert!((r.view(7, 175.0) - 175.0).abs() < 1e-4);
    }
}
