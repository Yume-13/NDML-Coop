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

//! Shared pickups: the host's orbs, collectable by every player, each one
//! credited once (the glue is `world/orbs_glue.rs`).
//!
//! The joining game vetoes its own `oCollectable` creates and shows the
//! host's orbs as copies running their own Step. A pickup there is instant
//! (the copy's own `Collision_oPlayer` without `updateScore`) and sends one
//! `OrbCollect`; the host credits the first collect for a living orb with
//! its own `updateScore` and broadcasts the winner. An orb that expires on
//! the host is gone for everyone.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ndml_net::msg::{Message, PeerId};

/// A collect this many host frames or less after the orb was taken is
/// contested (within a round trip); older ones are late.
pub const CONTESTED_FRAMES: u64 = 30;
/// Gone orbs are remembered this many frames (late collects are told
/// apart from unknown ids).
const GONE_FRAMES: u64 = 600;

/// The collectables (every `oCollectable` child); an `OrbSpawn` names one by
/// its index here.
pub const KINDS: [&str; 10] = [
    "oBounty",
    "oBountyFirst",
    "oBountyLarge",
    "oBountyMega",
    "oBountySerpentine",
    "oArmorFragment",
    "oHypercharge",
    "oMediCharge",
    "oNanocharge",
    "oReroll",
];
/// The first this many [`KINDS`] are the bounty orbs, whose pickup calls
/// `updateScore`; for the others the host only removes its orb.
pub const SCORED: usize = 5;

/// The pickup event of a kind.
#[must_use]
pub fn pickup_event(kind: &str) -> String {
    format!("gml_Object_{kind}_Collision_oPlayer")
}

/// Whether a kind's pickup adds score and XP (see [`SCORED`]).
#[must_use]
pub fn scored(kind: u8) -> bool {
    usize::from(kind) < SCORED
}

/// The kinds whose effect every player gets when one picks it up. Their
/// pickup writes only the local `oPowers`, so it can run on any game.
pub const SHARED: [&str; 3] = ["oHypercharge", "oNanocharge", "oReroll"];

/// Whether a kind's effect goes to every player (see [`SHARED`]).
#[must_use]
pub fn shared(kind: u8) -> bool {
    KINDS
        .get(usize::from(kind))
        .is_some_and(|k| SHARED.contains(k))
}

/// Host: whether a collect of `kind` with this outcome is shared: the
/// host's own touch (`None`), a credit, or a collect that came after the
/// orb ran out here (the collector's screen showed the pickup).
#[must_use]
pub fn host_shares(kind: u8, verdict: Option<OrbVerdict>) -> bool {
    shared(kind)
        && matches!(
            verdict,
            None | Some(OrbVerdict::Credit | OrbVerdict::Expired)
        )
}

/// Shared pickups seen are remembered this many frames of play (orb ids
/// wrap at `u16`); frozen or downed frames do not count.
const SEEN_FRAMES: u64 = 3600;

/// Shared pickup counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedCounts {
    /// Host: `PickupShared` sent.
    pub sent: u64,
    /// Told of a partner's shared pickup.
    pub got: u64,
    /// Applied here (its pickup run for this game's ship).
    pub applied: u64,
    /// Not applied: this game had picked that orb up itself (contested).
    pub own: u64,
    /// Not applied: no instance could be made for it.
    pub failed: u64,
    /// Not applied: told of again, already queued or applied.
    pub repeats: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Own,
    Queued,
}

/// The shared pickups this game gets: each once, never its own, queued
/// while frozen or down and run one per frame.
#[derive(Debug, Clone, Default)]
pub struct SharedPickups {
    seen: HashMap<u16, (Seen, u64)>,
    queue: std::collections::VecDeque<u8>,
    frame: u64,
    pub counts: SharedCounts,
}

impl SharedPickups {
    /// A frame; only one this game plays (`playing`: not frozen, its ship
    /// not down) ages what was seen.
    pub fn frame(&mut self, playing: bool) {
        if !playing {
            return;
        }
        self.frame += 1;
        let f = self.frame;
        self.seen
            .retain(|_, (_, at)| f.saturating_sub(*at) <= SEEN_FRAMES);
    }

    /// This game's own ship picked up `orb_id` (its own pickup ran).
    pub fn own(&mut self, orb_id: u16) {
        self.seen.entry(orb_id).or_insert((Seen::Own, self.frame));
    }

    /// A shared pickup credited to `by`. Returns whether it was queued.
    pub fn offer(&mut self, orb_id: u16, kind: u8, by: PeerId, me: PeerId) -> bool {
        if by == me || !shared(kind) {
            return false;
        }
        match self.seen.get(&orb_id) {
            Some((Seen::Queued, _)) => {
                self.counts.repeats += 1;
                false
            }
            Some((Seen::Own, _)) => {
                self.counts.got += 1;
                self.counts.own += 1;
                false
            }
            None => {
                self.counts.got += 1;
                self.seen.insert(orb_id, (Seen::Queued, self.frame));
                self.queue.push_back(kind);
                true
            }
        }
    }

    /// The kind to run this frame: one per frame, only while the game can
    /// use it (`running`: not frozen, its ship not down).
    pub fn next(&mut self, running: bool) -> Option<u8> {
        if running {
            self.queue.pop_front()
        } else {
            None
        }
    }

    /// Put back a kind [`Self::next`] gave that could not run yet (no ship).
    pub fn put_back(&mut self, kind: u8) {
        self.queue.push_front(kind);
    }

    /// Waiting to run.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
}

/// The host's answer to a collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrbVerdict {
    /// The collector's: credit it now (destroy the orb, add its points).
    Credit,
    /// Already taken by `by` (the collector's screen showed it anyway:
    /// `contested` when within a round trip).
    Taken { by: PeerId, contested: bool },
    /// Expired before the collect arrived.
    Expired,
    /// Not an orb this host made (or long gone).
    Unknown,
}

/// Host orb counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostOrbCounts {
    pub spawned: u64,
    pub own: u64,
    pub credited: u64,
    pub contested: u64,
    pub late: u64,
    pub expired: u64,
    pub unknown: u64,
}

#[derive(Debug, Clone, Copy)]
enum Orb {
    Alive,
    Taken { by: PeerId, at: u64 },
    Gone { at: u64 },
}

/// Whether the orb tables see a collectable: not one made and picked up
/// while a shared pickup runs here (`applying`).
#[must_use]
pub fn tables_see(applying: bool) -> bool {
    !applying
}

/// The host's orbs and who took each.
#[derive(Debug, Clone, Default)]
pub struct HostOrbs {
    orbs: HashMap<u16, Orb>,
    /// Each remembered orb's kind (as long as the orb is).
    kinds: HashMap<u16, u8>,
    frame: u64,
    pub counts: HostOrbCounts,
}

impl HostOrbs {
    /// A host frame.
    pub fn frame(&mut self) {
        self.frame += 1;
        let f = self.frame;
        self.orbs.retain(|_, o| match o {
            Orb::Alive => true,
            Orb::Taken { at, .. } | Orb::Gone { at } => f.saturating_sub(*at) <= GONE_FRAMES,
        });
        let orbs = &self.orbs;
        self.kinds.retain(|id, _| orbs.contains_key(id));
    }

    /// The host's world made an orb of `kind` ([`KINDS`] index).
    pub fn spawned(&mut self, orb_id: u16, kind: u8) {
        self.counts.spawned += 1;
        self.orbs.insert(orb_id, Orb::Alive);
        self.kinds.insert(orb_id, kind);
    }

    /// An orb's kind while it is remembered.
    #[must_use]
    pub fn kind(&self, orb_id: u16) -> Option<u8> {
        self.kinds.get(&orb_id).copied()
    }

    /// The host's own ship took the orb (its own pickup ran). Returns the
    /// broadcast.
    pub fn own(&mut self, orb_id: u16) -> Option<Message> {
        match self.orbs.get(&orb_id) {
            Some(Orb::Alive) => {
                self.counts.own += 1;
                self.orbs.insert(
                    orb_id,
                    Orb::Taken {
                        by: PeerId::HOST,
                        at: self.frame,
                    },
                );
                Some(Message::OrbCollect {
                    orb_id,
                    peer: PeerId::HOST,
                })
            }
            _ => None,
        }
    }

    /// A partner's collect.
    pub fn collect(&mut self, orb_id: u16, peer: PeerId) -> OrbVerdict {
        let frame = self.frame;
        let v = match self.orbs.get(&orb_id) {
            Some(Orb::Alive) => {
                self.orbs.insert(
                    orb_id,
                    Orb::Taken {
                        by: peer,
                        at: frame,
                    },
                );
                OrbVerdict::Credit
            }
            Some(Orb::Taken { by, at }) => OrbVerdict::Taken {
                by: *by,
                contested: frame.saturating_sub(*at) <= CONTESTED_FRAMES,
            },
            Some(Orb::Gone { .. }) => OrbVerdict::Expired,
            None => OrbVerdict::Unknown,
        };
        match v {
            OrbVerdict::Credit => self.counts.credited += 1,
            OrbVerdict::Taken {
                contested: true, ..
            } => self.counts.contested += 1,
            OrbVerdict::Taken { .. } => self.counts.late += 1,
            OrbVerdict::Expired => self.counts.expired += 1,
            OrbVerdict::Unknown => self.counts.unknown += 1,
        }
        v
    }

    /// The orb left the host's world without a collect (its timer).
    pub fn gone(&mut self, orb_id: u16) {
        if matches!(self.orbs.get(&orb_id), Some(Orb::Alive)) {
            self.orbs.insert(orb_id, Orb::Gone { at: self.frame });
        }
    }
}

/// Client orb counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientOrbCounts {
    /// Collected on this screen, confirmed as this player's, taken by
    /// another first, gone with no answer.
    pub collected: u64,
    pub confirmed: u64,
    pub lost: u64,
    pub unanswered: u64,
}

/// The joining side's collects in flight.
#[derive(Debug, Clone, Default)]
pub struct ClientOrbs {
    pending: HashMap<u16, u64>,
    frame: u64,
    pub counts: ClientOrbCounts,
}

impl ClientOrbs {
    /// A frame.
    pub fn frame(&mut self) {
        self.frame += 1;
    }

    /// The local ship touched the copy of `orb_id`: the message to send, the
    /// first time only.
    pub fn touched(&mut self, orb_id: u16, me: PeerId) -> Option<Message> {
        if self.pending.contains_key(&orb_id) {
            return None;
        }
        self.counts.collected += 1;
        self.pending.insert(orb_id, self.frame);
        Some(Message::OrbCollect { orb_id, peer: me })
    }

    /// The host's `OrbCollect`. Returns whether this game must remove its copy.
    pub fn on_collect(&mut self, orb_id: u16, by: PeerId, me: PeerId) -> bool {
        match self.pending.remove(&orb_id) {
            Some(_) if by == me => {
                self.counts.confirmed += 1;
                false
            }
            Some(_) => {
                self.counts.lost += 1;
                false
            }
            None => by != me,
        }
    }

    /// The orb's despawn: a collect still pending was never answered.
    pub fn on_gone(&mut self, orb_id: u16) {
        if self.pending.remove(&orb_id).is_some() {
            self.counts.unanswered += 1;
        }
    }

    /// Collects in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// A lost link's return: the unanswered collects to send again, oldest
    /// first.
    #[must_use]
    pub fn resend(&self, me: PeerId) -> Vec<Message> {
        let mut ids: Vec<(u64, u16)> = self.pending.iter().map(|(id, at)| (*at, *id)).collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|(_, orb_id)| Message::OrbCollect { orb_id, peer: me })
            .collect()
    }

    /// Whether the host's `OrbSpawn` of `orb_id` needs a copy here (none,
    /// not held, not taken here and awaiting the answer).
    #[must_use]
    pub fn wants_copy(&self, orb_id: u16, has_copy: bool, held: bool) -> bool {
        !(has_copy || held || self.pending.contains_key(&orb_id))
    }
}

/// Host: an orb message a lost link may have taken, sent again at the return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Collect { orb_id: u16, peer: PeerId },
    Shared { orb_id: u16, kind: u8, by: PeerId },
}

impl Ended {
    /// The orb messages worth keeping (`OrbSpawn` is not: the live orbs are
    /// sent from the world as they are then).
    #[must_use]
    pub fn of(m: &Message) -> Option<Self> {
        match *m {
            Message::OrbCollect { orb_id, peer } => Some(Self::Collect { orb_id, peer }),
            Message::PickupShared { orb_id, kind, by } => Some(Self::Shared { orb_id, kind, by }),
            _ => None,
        }
    }

    #[must_use]
    pub fn message(self) -> Message {
        match self {
            Self::Collect { orb_id, peer } => Message::OrbCollect { orb_id, peer },
            Self::Shared { orb_id, kind, by } => Message::PickupShared { orb_id, kind, by },
        }
    }
}

/// Frames looked back from the notice of a lost link (past the endpoint's
/// 10 s timeout): 15 s at 60 fps.
pub const RESYNC_LOOKBACK: u64 = 900;
/// At most this many kept (16 bytes each).
pub const RESYNC_CAP: usize = 4096;

/// Host: the orb collects and shared pickups of the last
/// [`RESYNC_LOOKBACK`] frames and of an outage, sent again at the return
/// (each is safe to send twice).
#[derive(Debug, Clone, Default)]
pub struct HostResync {
    ring: std::collections::VecDeque<(u64, Ended)>,
    /// The first frame to send again, from the lost link's notice.
    since: Option<u64>,
    /// Dropped at the cap (the oldest first).
    pub dropped: u64,
}

impl HostResync {
    /// Sent at `frame`. Returns true the first time the cap drops one.
    pub fn record(&mut self, frame: u64, e: Ended) -> bool {
        if self.since.is_none() {
            let keep = frame.saturating_sub(RESYNC_LOOKBACK);
            while self.ring.front().is_some_and(|(at, _)| *at < keep) {
                self.ring.pop_front();
            }
        }
        let mut first = false;
        if self.ring.len() >= RESYNC_CAP {
            self.ring.pop_front();
            self.dropped += 1;
            first = self.dropped == 1;
        }
        self.ring.push_back((frame, e));
        first
    }

    /// The world's notice of a lost link at `frame`: from here on nothing is
    /// pruned but by the cap.
    pub fn interrupted(&mut self, frame: u64) {
        self.since
            .get_or_insert(frame.saturating_sub(RESYNC_LOOKBACK));
    }

    /// The link back: what to send again (oldest first); nothing without a
    /// notice before it.
    pub fn resumed(&mut self) -> Vec<Ended> {
        let Some(since) = self.since.take() else {
            return Vec::new();
        };
        self.ring
            .iter()
            .filter(|(at, _)| *at >= since)
            .map(|(_, e)| *e)
            .collect()
    }
}

/// Joining side: the host's orb spawns not made here yet (this game frozen,
/// or over the per-frame cap). An orb taken or run out while its spawn is
/// held is never made.
#[derive(Debug)]
pub struct HeldSpawns<T> {
    items: Vec<(u16, T)>,
}

impl<T> Default for HeldSpawns<T> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<T> HeldSpawns<T> {
    pub fn push(&mut self, orb_id: u16, spawn: T) {
        self.items.push((orb_id, spawn));
    }

    /// Whether the spawn of `orb_id` is held.
    #[must_use]
    pub fn holds(&self, orb_id: u16) -> bool {
        self.items.iter().any(|(id, _)| *id == orb_id)
    }

    /// The host's `OrbCollect` for `orb_id` (taken by anyone, or run out).
    pub fn gone(&mut self, orb_id: u16) {
        self.items.retain(|(id, _)| *id != orb_id);
    }

    /// The spawns to make this frame: none while frozen, else up to `max`,
    /// oldest first.
    pub fn take(&mut self, frozen: bool, max: usize) -> Vec<T> {
        if frozen {
            return Vec::new();
        }
        let n = self.items.len().min(max);
        self.items.drain(..n).map(|(_, s)| s).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_spawn_whose_orb_is_gone_is_never_made() {
        let mut held = HeldSpawns::default();
        held.push(7, "taken while frozen");
        held.push(8, "still there");
        assert!(held.take(true, 64).is_empty(), "nothing made while frozen");
        held.gone(7);
        held.gone(99); // an orb already made or unknown: nothing held
        assert_eq!(held.take(false, 64), vec!["still there"]);
        assert!(held.take(false, 64).is_empty());
        // The cap: the oldest first, the rest on later frames.
        let mut held = HeldSpawns::default();
        for id in 0..5 {
            held.push(id, id);
        }
        assert_eq!(held.take(false, 2), vec![0, 1]);
        held.gone(3);
        assert_eq!(held.take(false, 2), vec![2, 4]);
    }

    #[test]
    fn the_kinds_fit_a_byte_and_the_bounty_orbs_come_first() {
        assert!(u8::try_from(KINDS.len()).is_ok());
        let mut k = KINDS.to_vec();
        k.sort_unstable();
        k.dedup();
        assert_eq!(k.len(), KINDS.len(), "no kind twice");
        assert!(KINDS[..SCORED].iter().all(|n| n.starts_with("oBounty")));
        assert!(!KINDS[SCORED..].iter().any(|n| n.starts_with("oBounty")));
        assert!(scored(0) && !scored(5));
        assert_eq!(
            pickup_event("oBounty"),
            "gml_Object_oBounty_Collision_oPlayer"
        );
    }

    const ME: PeerId = PeerId(1);

    #[test]
    fn each_orb_is_credited_once() {
        let mut h = HostOrbs::default();
        let mut c = ClientOrbs::default();
        h.spawned(5, 0);
        h.spawned(6, 0);
        // The joining player takes 5: instant on its screen, credited once.
        let m = c.touched(5, ME).expect("first touch sends");
        assert!(c.touched(5, ME).is_none(), "once");
        let Message::OrbCollect { orb_id, peer } = m else {
            unreachable!("OrbCollect")
        };
        assert_eq!(h.collect(orb_id, peer), OrbVerdict::Credit);
        assert_eq!(
            h.collect(orb_id, peer),
            OrbVerdict::Taken {
                by: ME,
                contested: true
            },
            "a repeat credits nothing"
        );
        assert!(!c.on_collect(5, ME, ME));
        // The host's ship takes 6.
        assert_eq!(
            h.own(6),
            Some(Message::OrbCollect {
                orb_id: 6,
                peer: PeerId::HOST
            })
        );
        assert!(h.own(6).is_none());
        assert!(c.on_collect(6, PeerId::HOST, ME), "the copy goes");
        assert_eq!((h.counts.credited, h.counts.own), (1, 1));
        assert_eq!((c.counts.collected, c.counts.confirmed), (1, 1));
    }

    #[test]
    fn contested_late_expired_and_unknown_collects() {
        let mut h = HostOrbs::default();
        let mut c = ClientOrbs::default();
        h.spawned(1, 0);
        h.spawned(2, 0);
        h.spawned(3, 0);
        // Both took 1 within a round trip: the host first.
        assert!(h.own(1).is_some());
        let _ = c.touched(1, ME);
        for _ in 0..10 {
            h.frame();
        }
        assert_eq!(
            h.collect(1, ME),
            OrbVerdict::Taken {
                by: PeerId::HOST,
                contested: true
            }
        );
        assert!(!c.on_collect(1, PeerId::HOST, ME));
        assert_eq!(c.counts.lost, 1);
        // A collect long after the host took it: late.
        assert!(h.own(2).is_some());
        for _ in 0..=CONTESTED_FRAMES {
            h.frame();
        }
        assert!(matches!(
            h.collect(2, ME),
            OrbVerdict::Taken {
                contested: false,
                ..
            }
        ));
        // Expired on the host before the collect arrived.
        h.gone(3);
        assert_eq!(h.collect(3, ME), OrbVerdict::Expired);
        assert_eq!(h.collect(99, ME), OrbVerdict::Unknown);
        assert_eq!(
            (
                h.counts.contested,
                h.counts.late,
                h.counts.expired,
                h.counts.unknown
            ),
            (1, 1, 1, 1)
        );
        // A collect whose orb despawned with no answer.
        let _ = c.touched(7, ME);
        c.on_gone(7);
        assert_eq!((c.counts.unanswered, c.in_flight()), (1, 0));
        // Gone orbs are forgotten after a while.
        for _ in 0..=GONE_FRAMES {
            h.frame();
        }
        assert_eq!(h.collect(3, ME), OrbVerdict::Unknown);
        assert_eq!(h.kind(3), None, "its kind forgotten with it");
    }

    fn kind(name: &str) -> u8 {
        u8::try_from(KINDS.iter().position(|k| *k == name).unwrap()).unwrap()
    }

    /// Hyperboost, Nanocharge and Reroll go to both; armour, Medi-Charge and
    /// the bounty orbs (the Serpentine's too) only to the collector.
    #[test]
    fn which_kinds_are_shared_and_which_outcomes_share_them() {
        for k in SHARED {
            assert!(shared(kind(k)), "{k}");
        }
        for k in [
            "oArmorFragment",
            "oMediCharge",
            "oBountySerpentine",
            "oBounty",
        ] {
            assert!(!shared(kind(k)), "{k}");
        }
        assert!(!shared(200));
        let hyper = kind("oHypercharge");
        assert!(host_shares(hyper, None), "the host's own touch");
        assert!(host_shares(hyper, Some(OrbVerdict::Credit)));
        assert!(
            host_shares(hyper, Some(OrbVerdict::Expired)),
            "the collector's screen showed it"
        );
        let taken = OrbVerdict::Taken {
            by: PeerId::HOST,
            contested: true,
        };
        assert!(
            !host_shares(hyper, Some(taken)),
            "already shared by its taker"
        );
        assert!(!host_shares(hyper, Some(OrbVerdict::Unknown)));
        assert!(!host_shares(kind("oArmorFragment"), None));
        let mut h = HostOrbs::default();
        h.spawned(4, hyper);
        assert_eq!(h.kind(4), Some(hyper));
    }

    /// Each shared pickup once per player, never their own; queued while
    /// unusable, one per frame.
    #[test]
    fn shared_pickups_once_each_queued_while_not_running() {
        let (hyper, nano, reroll) = (kind("oHypercharge"), kind("oNanocharge"), kind("oReroll"));
        let mut s = SharedPickups::default();
        assert!(!s.offer(1, hyper, ME, ME), "its taker gets nothing more");
        assert!(
            !s.offer(1, kind("oMediCharge"), PeerId::HOST, ME),
            "not shared"
        );
        // Contested: this game had taken 2 itself.
        s.own(2);
        assert!(!s.offer(2, nano, PeerId::HOST, ME));
        // Queued while frozen or down, then one per frame.
        assert!(s.offer(3, hyper, PeerId::HOST, ME));
        assert!(
            !s.offer(3, hyper, PeerId::HOST, ME),
            "a repeat is not applied twice"
        );
        assert!(s.offer(4, reroll, PeerId::HOST, ME));
        assert_eq!(s.next(false), None);
        assert_eq!(s.queued(), 2);
        assert_eq!(s.next(true), Some(hyper));
        // No ship yet: back at the front.
        s.put_back(reroll);
        assert_eq!(s.queued(), 2);
        assert_eq!(s.next(true), Some(reroll));
        assert_eq!(s.next(true), Some(reroll));
        assert_eq!(s.next(true), None);
        assert_eq!((s.counts.got, s.counts.own), (3, 1));
        // An id comes back much later (ids wrap): a new pickup.
        for _ in 0..=SEEN_FRAMES {
            s.frame(true);
        }
        assert!(s.offer(3, hyper, PeerId::HOST, ME));
    }

    /// A pickup sent again after a freeze of any length is not applied twice.
    #[test]
    fn a_pickup_seen_before_a_long_freeze_is_not_applied_again() {
        let hyper = kind("oHypercharge");
        let mut s = SharedPickups::default();
        assert!(s.offer(5, hyper, PeerId::HOST, ME));
        assert_eq!(s.next(true), Some(hyper));
        s.own(6);
        // Ten minutes frozen (the host's longest play-on), then a little play.
        for _ in 0..36_000 {
            s.frame(false);
        }
        for _ in 0..100 {
            s.frame(true);
        }
        assert!(
            !s.offer(5, hyper, PeerId::HOST, ME),
            "sent again: already applied"
        );
        assert!(!s.offer(6, hyper, PeerId::HOST, ME), "sent again: its own");
        assert_eq!(s.next(true), None);
        // A full window of play still lets a wrapped id through.
        for _ in 0..=SEEN_FRAMES {
            s.frame(true);
        }
        assert!(s.offer(5, hyper, PeerId::HOST, ME));
    }

    /// The resync ring sends the lost window once; nothing older or twice.
    #[test]
    fn host_resync_sends_the_lost_window_once() {
        let c = |orb_id| Ended::Collect {
            orb_id,
            peer: PeerId::HOST,
        };
        let sh = Ended::Shared {
            orb_id: 9,
            kind: kind("oReroll"),
            by: PeerId::HOST,
        };
        let mut r = HostResync::default();
        assert!(r.resumed().is_empty(), "no notice: nothing");
        r.record(100, c(1)); // long before the cut
        r.record(2000, c(2)); // after the cut, before the notice
        r.interrupted(2500);
        r.record(3000, sh); // during the outage (a host playing on)
        r.record(9000, c(3)); // a long outage keeps everything
        assert_eq!(r.resumed(), vec![c(2), sh, c(3)]);
        assert!(r.resumed().is_empty(), "a second return sends nothing more");
        assert_eq!(Ended::of(&sh.message()), Some(sh));
        // No orb in the lost window: nothing to send.
        let mut r = HostResync::default();
        r.record(100, c(1));
        r.interrupted(5000);
        assert!(r.resumed().is_empty());
    }

    /// The cap: the oldest dropped, the first drop reported once.
    #[test]
    fn host_resync_is_bounded() {
        let mut r = HostResync::default();
        r.interrupted(0);
        let firsts = (0..RESYNC_CAP + 5)
            .filter(|&i| {
                r.record(
                    i as u64,
                    Ended::Collect {
                        orb_id: i as u16,
                        peer: PeerId::HOST,
                    },
                )
            })
            .count();
        assert_eq!((firsts, r.dropped), (1, 5));
        let sent = r.resumed();
        assert_eq!(sent.len(), RESYNC_CAP);
        assert_eq!(
            sent[0],
            Ended::Collect {
                orb_id: 5,
                peer: PeerId::HOST
            }
        );
    }

    /// The joining side at a return: unanswered collects sent again first;
    /// the resync makes no second copy and never one of an orb gone.
    #[test]
    fn the_joining_side_at_a_return() {
        let mut c = ClientOrbs::default();
        c.touched(7, ME);
        c.frame();
        c.touched(4, ME);
        assert_eq!(
            c.resend(ME),
            vec![
                Message::OrbCollect {
                    orb_id: 7,
                    peer: ME
                },
                Message::OrbCollect {
                    orb_id: 4,
                    peer: ME
                }
            ],
            "oldest first"
        );
        let copies = [8u16];
        let mut held = HeldSpawns::default();
        held.push(10, 10u16);
        let resync = |live: &[u16], held: &mut HeldSpawns<u16>, copies: &[u16]| {
            for &id in live {
                if c.wants_copy(id, copies.contains(&id), held.holds(id)) {
                    held.push(id, id);
                }
            }
        };
        // The host's live orbs then (12 too: it ends in the same frame).
        resync(&[7, 8, 10, 11, 12], &mut held, &copies);
        // The same frame's collects: 12 ended.
        held.gone(12);
        let made = held.take(false, 64);
        assert_eq!(made, vec![10, 11], "7 pending, 8 a copy, 12 gone");
        let copies: Vec<u16> = copies.iter().copied().chain(made).collect();
        resync(&[7, 8, 10, 11], &mut held, &copies);
        assert!(held.take(false, 64).is_empty(), "no second copy");
    }

    /// The flag's rule: what is made and picked up while a shared pickup
    /// runs here is no orb and no collect.
    #[test]
    fn the_applied_instance_is_nothing_to_the_tables() {
        assert!(tables_see(false));
        assert!(!tables_see(true));
    }
}
