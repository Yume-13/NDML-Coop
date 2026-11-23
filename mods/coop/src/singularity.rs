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

//! The shared Singularity: if either player enters it, both are caught
//! (cutscene, hull and shield drain, fly-in).
//!
//! `oBlackHole` is a hazard the host's world makes and replicates. While a ship overlaps it, a
//! 90-step timer runs down and hull and shield drain; at 0 the ship is parked off the arena with
//! `alarm[3] = 120` and the hole closes; `oPlayer_Alarm_3` flies it back in.
//!
//! A game whose ship is caught tells the host, which tells every joining game; each other game
//! catches its own ship and takes the same share off hull and shield. A downed ship is skipped
//! (the hole still closes); a catch older than [`MAX_AGE_MS`] is dropped; a second catch within
//! [`REPEAT`] of the ship's own is not applied. `singularity_glue.rs` holds the game side.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ndml_net::msg::{Message, PeerId, SingularityEntry};

/// A catch older than this on arrival is dropped.
pub const MAX_AGE_MS: u32 = 3_000;

/// A ship caught this recently is not caught again (both entered at once).
pub const REPEAT: Duration = Duration::from_secs(4);

/// Most catches waiting for one step.
pub const MAX_PENDING: usize = 8;

/// The session role this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// No linked co-op run: the game's own Singularity, untouched.
    Off,
    Host,
    Client,
}

/// A message to send.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// To the host.
    ToHost(Message),
    /// To every joining game.
    Broadcast(Message),
}

/// What this game is to do at its ship's next step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pending {
    pub entry: SingularityEntry,
    /// Catch this game's own ship (false: only close the hole).
    pub catch_ship: bool,
}

/// What happened to catches (read by the tests).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub own: u32,
    pub sent: u32,
    pub received: u32,
    pub stale: u32,
    pub echo: u32,
    pub repeat: u32,
    pub applied: u32,
    pub skipped_down: u32,
    /// Entries with a value that is not a number (dropped).
    pub bad: u32,
}

/// The decision state.
#[derive(Debug, Default)]
pub struct Singularity {
    own_at: Option<Instant>,
    /// Catches waiting for the ship's next step, oldest first (at most
    /// [`MAX_PENDING`]; each closes its hole, the first catches the ship).
    pending: Vec<Pending>,
    outs: Vec<Out>,
    pub counts: Counts,
}

impl Singularity {
    /// This game's own ship was caught (by the hole or its copy).
    pub fn own_catch(&mut self, role: Role, entry: SingularityEntry, now: Instant) {
        self.own_at = Some(now);
        self.counts.own += 1;
        match role {
            Role::Off => {}
            Role::Host => {
                self.outs
                    .push(Out::Broadcast(Message::SingularityCatch(entry)));
                self.counts.sent += 1;
            }
            Role::Client => {
                self.outs
                    .push(Out::ToHost(Message::SingularityEntered(entry)));
                self.counts.sent += 1;
            }
        }
    }

    /// A Singularity message from `from`, `age_ms` old on arrival by the
    /// sender's clock (`None`: clock not known yet, taken as fresh).
    pub fn on_message(
        &mut self,
        role: Role,
        me: PeerId,
        from: PeerId,
        msg: &Message,
        age_ms: Option<u32>,
        now: Instant,
    ) {
        let (entry, from_client) = match (role, msg) {
            (Role::Host, Message::SingularityEntered(e)) => {
                (SingularityEntry { by: from, ..*e }, true)
            }
            (Role::Client, Message::SingularityCatch(e)) if from.is_host() => (*e, false),
            _ => return,
        };
        self.counts.received += 1;
        let Some(entry) = sane(entry) else {
            self.counts.bad += 1;
            return;
        };
        if entry.by == me {
            self.counts.echo += 1;
            return;
        }
        if age_ms.is_some_and(|a| a > MAX_AGE_MS) {
            self.counts.stale += 1;
            return;
        }
        if from_client {
            // Every other joining game catches too (the enterer ignores it).
            self.outs
                .push(Out::Broadcast(Message::SingularityCatch(entry)));
            self.counts.sent += 1;
        }
        let repeat = self.own_at.is_some_and(|t| now.duration_since(t) < REPEAT);
        if repeat {
            self.counts.repeat += 1;
        }
        if self.pending.len() < MAX_PENDING {
            self.pending.push(Pending {
                entry,
                catch_ship: !repeat,
            });
        }
    }

    /// Take the catches to apply at the ship's next step. Every one closes its
    /// hole; at most the first catches the ship, and not when the ship is down,
    /// out of control, or caught by its own hole within [`REPEAT`].
    pub fn take_pending(&mut self, down: bool, in_control: bool, now: Instant) -> Vec<Pending> {
        let mut out = std::mem::take(&mut self.pending);
        for p in &mut out {
            if p.catch_ship && down {
                p.catch_ship = false;
                self.counts.skipped_down += 1;
            }
            let own_recent = self
                .own_at
                .is_some_and(|t| now.saturating_duration_since(t) < REPEAT);
            if p.catch_ship && (!in_control || own_recent) {
                p.catch_ship = false;
                self.counts.repeat += 1;
            }
            if p.catch_ship {
                self.own_at = Some(now);
                self.counts.applied += 1;
            }
        }
        out
    }

    /// Whether a catch waits for the ship's next step.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Messages to send.
    pub fn take_outs(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.outs)
    }

    /// A new run or the end of the link: nothing carries over but the counts.
    pub fn reset(&mut self) {
        let counts = self.counts;
        *self = Self::default();
        self.counts = counts;
    }
}

/// An entry from the network made safe to apply: `None` unless place and
/// shares are finite; shares clamped to [0, 1].
#[must_use]
pub fn sane(e: SingularityEntry) -> Option<SingularityEntry> {
    let finite = [e.x, e.y, e.hull, e.shield].iter().all(|v| v.is_finite());
    finite.then(|| SingularityEntry {
        hull: e.hull.clamp(0.0, 1.0),
        shield: e.shield.clamp(0.0, 1.0),
        ..e
    })
}

/// Whether a message is this module's.
#[must_use]
pub fn is_mine(msg: &Message) -> bool {
    matches!(
        msg,
        Message::SingularityEntered(_) | Message::SingularityCatch(_)
    )
}

/// The game's drain factor for one step with control: the timer fell by
/// `bop = min(image_xscale, 1)` and hull and shield are multiplied by
/// `1 - 0.02 * bop`. 1 when the timer did not fall.
#[must_use]
pub fn step_factor(timer_before: f64, timer_after: f64) -> f64 {
    let bop = timer_before - timer_after;
    if bop.is_finite() && bop > 0.0 {
        (1.0 - 0.02 * bop.min(1.0)).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// The part of the sender's share still to take from a ship whose own drain
/// already took `own_so_far`, so both end on the same share (at most 1).
#[must_use]
pub fn still_to_take(share: f64, own_so_far: f64) -> f64 {
    if !(own_so_far.is_finite() && own_so_far > 0.0 && own_so_far < 1.0) {
        return share;
    }
    (share / own_so_far).clamp(0.0, 1.0)
}

/// `value` after the drain share, as the game's drain leaves it: only values
/// above 2 are drained, never below 2. A non-finite share counts as 1.
#[must_use]
pub fn drained(value: f64, share: f64) -> f64 {
    let share = if share.is_finite() {
        share.clamp(0.0, 1.0)
    } else {
        1.0
    };
    if value > 2.0 {
        (value * share).max(2.0)
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(by: PeerId) -> SingularityEntry {
        SingularityEntry {
            by,
            x: 100.0,
            y: 200.0,
            hull: 0.16,
            shield: 0.5,
            time_ms: 1000,
        }
    }

    const CLIENT: PeerId = PeerId(1);

    #[test]
    fn the_hosts_own_catch_goes_to_every_joining_game() {
        let now = Instant::now();
        let mut h = Singularity::default();
        h.own_catch(Role::Host, entry(PeerId::HOST), now);
        assert_eq!(
            h.take_outs(),
            vec![Out::Broadcast(Message::SingularityCatch(entry(
                PeerId::HOST
            )))]
        );
        let mut c = Singularity::default();
        c.on_message(
            Role::Client,
            CLIENT,
            PeerId::HOST,
            &Message::SingularityCatch(entry(PeerId::HOST)),
            Some(60),
            now,
        );
        let p = c.take_pending(false, true, now).into_iter().next().unwrap();
        assert!(p.catch_ship);
        assert!((p.entry.hull - 0.16).abs() < 1e-6);
        assert!(
            c.take_outs().is_empty(),
            "a joining game sends nothing back"
        );
    }

    #[test]
    fn a_joining_ships_catch_reaches_the_host_and_comes_back_ignored() {
        let now = Instant::now();
        let mut c = Singularity::default();
        c.own_catch(Role::Client, entry(CLIENT), now);
        let outs: Vec<Out> = c.take_outs();
        assert_eq!(outs.len(), 1);
        let Some(Out::ToHost(m)) = outs.first().cloned() else {
            unreachable!("to the host: {outs:?}")
        };
        let mut h = Singularity::default();
        // The host names the sender itself.
        let claimed = Message::SingularityEntered(entry(PeerId(7)));
        h.on_message(Role::Host, PeerId::HOST, CLIENT, &claimed, Some(40), now);
        let p = h.take_pending(false, true, now).into_iter().next().unwrap();
        assert_eq!(p.entry.by, CLIENT);
        assert!(p.catch_ship);
        let outs: Vec<Out> = h.take_outs();
        assert_eq!(outs.len(), 1);
        let Some(Out::Broadcast(back)) = outs.first().cloned() else {
            unreachable!("broadcast: {outs:?}")
        };
        assert!(matches!(m, Message::SingularityEntered(_)));
        c.on_message(Role::Client, CLIENT, PeerId::HOST, &back, Some(80), now);
        assert!(
            c.take_pending(false, true, now).is_empty(),
            "its own, already caught"
        );
        assert_eq!(c.counts.echo, 1);
    }

    #[test]
    fn down_stale_and_double_catches() {
        let now = Instant::now();
        let msg = Message::SingularityCatch(entry(PeerId::HOST));
        // Down: the hole closes, the ship is left.
        let mut c = Singularity::default();
        c.on_message(Role::Client, CLIENT, PeerId::HOST, &msg, Some(10), now);
        let p = c.take_pending(true, true, now).into_iter().next().unwrap();
        assert!(!p.catch_ship);
        assert_eq!(c.counts.skipped_down, 1);
        // Older than the limit: dropped.
        c.on_message(Role::Client, CLIENT, PeerId::HOST, &msg, Some(3_001), now);
        assert!(c.take_pending(false, true, now).is_empty());
        assert_eq!(c.counts.stale, 1);
        // Clock not known yet: taken.
        c.on_message(Role::Client, CLIENT, PeerId::HOST, &msg, None, now);
        assert!(
            c.take_pending(false, true, now)
                .into_iter()
                .next()
                .unwrap()
                .catch_ship
        );
        // Both entered at once: the ship is not caught a second time.
        let mut h = Singularity::default();
        h.own_catch(Role::Host, entry(PeerId::HOST), now);
        h.on_message(
            Role::Host,
            PeerId::HOST,
            CLIENT,
            &Message::SingularityEntered(entry(CLIENT)),
            Some(30),
            now + Duration::from_millis(200),
        );
        let p = h.take_pending(false, true, now).into_iter().next().unwrap();
        assert!(!p.catch_ship);
        assert_eq!(h.counts.repeat, 1);
        // Not linked: the game's own, nothing sent; messages ignored.
        let mut s = Singularity::default();
        s.own_catch(Role::Off, entry(PeerId::HOST), now);
        assert!(s.take_outs().is_empty());
        s.on_message(Role::Off, PeerId::HOST, CLIENT, &msg, Some(0), now);
        assert!(!s.has_pending());
        // Only the host's catch counts on a joining game.
        let mut c2 = Singularity::default();
        c2.on_message(Role::Client, CLIENT, PeerId(2), &msg, Some(0), now);
        assert!(!c2.has_pending());
    }

    #[test]
    fn the_drain_factor() {
        // A full-size hole: the timer falls by 1 a step, 2 % a step.
        assert!((step_factor(90.0, 89.0) - 0.98).abs() < 1e-9);
        // A small one (image_xscale 0.5): 1 %.
        assert!((step_factor(40.0, 39.5) - 0.99).abs() < 1e-9);
        assert!(
            (step_factor(90.0, 90.0) - 1.0).abs() < 1e-9,
            "no fall, no drain"
        );
        assert!(
            (step_factor(0.0, 90.0) - 1.0).abs() < 1e-9,
            "a reset is no drain"
        );
        // Over a full entry: 0.98^90.
        let full = (0..90).fold(1.0, |f, i| {
            f * step_factor(90.0 - f64::from(i), 89.0 - f64::from(i))
        });
        assert!((full - 0.98_f64.powi(90)).abs() < 1e-9);
        assert!((drained(115.0, 0.162) - 18.63).abs() < 1e-9);
        assert!(
            (drained(10.0, 0.162) - 2.0).abs() < 1e-9,
            "the drain stops at 2"
        );
        assert!(
            (drained(1.5, 0.162) - 1.5).abs() < 1e-9,
            "not at 2 or below"
        );
    }

    #[test]
    fn what_comes_off_the_network_is_bounded() {
        let now = Instant::now();
        let mut e = entry(PeerId::HOST);
        // Shares above 1 or below 0 are clamped.
        e.hull = 5.0;
        e.shield = -3.0;
        let s = sane(e).unwrap();
        assert!((s.hull - 1.0).abs() < 1e-6 && s.shield.abs() < 1e-6);
        // Not numbers: dropped, counted, nothing pending.
        let mut c = Singularity::default();
        for bad in [f32::INFINITY, f32::NAN, f32::NEG_INFINITY] {
            let mut e = entry(PeerId::HOST);
            e.hull = bad;
            c.on_message(
                Role::Client,
                CLIENT,
                PeerId::HOST,
                &Message::SingularityCatch(e),
                Some(10),
                now,
            );
            let mut e = entry(PeerId::HOST);
            e.x = bad;
            c.on_message(
                Role::Client,
                CLIENT,
                PeerId::HOST,
                &Message::SingularityCatch(e),
                Some(10),
                now,
            );
        }
        assert!(!c.has_pending());
        assert_eq!(c.counts.bad, 6);
        // A clamped share applied: hull never rises, never becomes infinite.
        let mut e = entry(PeerId::HOST);
        e.hull = 7.0;
        c.on_message(
            Role::Client,
            CLIENT,
            PeerId::HOST,
            &Message::SingularityCatch(e),
            Some(10),
            now,
        );
        let p = c.take_pending(false, true, now).into_iter().next().unwrap();
        assert!((drained(80.0, f64::from(p.entry.hull)) - 80.0).abs() < 1e-9);
        assert!((drained(80.0, f64::INFINITY) - 80.0).abs() < 1e-9);
        assert!((drained(80.0, f64::NAN) - 80.0).abs() < 1e-9);
        assert!((drained(80.0, 3.0) - 80.0).abs() < 1e-9);
        assert!((drained(80.0, -1.0) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_catch_is_judged_again_when_it_is_applied() {
        let now = Instant::now();
        let msg = Message::SingularityCatch(entry(PeerId::HOST));
        // This game's own hole caught the ship in the frame the catch is
        // applied: out of control at the step, so not again.
        let mut c = Singularity::default();
        c.on_message(Role::Client, CLIENT, PeerId::HOST, &msg, Some(20), now);
        let p = c
            .take_pending(false, false, now)
            .into_iter()
            .next()
            .unwrap();
        assert!(!p.catch_ship, "already caught by its own hole");
        assert_eq!(c.counts.repeat, 1);
        assert_eq!(c.counts.applied, 0);
        // The own catch was seen between the message and the step.
        let mut c = Singularity::default();
        c.on_message(Role::Client, CLIENT, PeerId::HOST, &msg, Some(20), now);
        c.own_catch(Role::Client, entry(CLIENT), now + Duration::from_millis(5));
        let p = c
            .take_pending(false, true, now + Duration::from_millis(10))
            .into_iter()
            .next()
            .unwrap();
        assert!(!p.catch_ship, "its own catch came after the message");
        // In control and no own catch: caught.
        let mut c = Singularity::default();
        c.on_message(Role::Client, CLIENT, PeerId::HOST, &msg, Some(20), now);
        assert!(
            c.take_pending(false, true, now)
                .into_iter()
                .next()
                .unwrap()
                .catch_ship
        );
        assert_eq!(c.counts.applied, 1);
    }

    #[test]
    fn two_catches_in_one_step_catch_the_ship_once_and_close_both_holes() {
        let now = Instant::now();
        // Two joining players' catches in one frame.
        let mut h = Singularity::default();
        for p in [PeerId(1), PeerId(2)] {
            h.on_message(
                Role::Host,
                PeerId::HOST,
                p,
                &Message::SingularityEntered(entry(p)),
                Some(20),
                now,
            );
        }
        let ps = h.take_pending(false, true, now);
        assert_eq!(ps.len(), 2, "both holes close");
        assert!(ps[0].catch_ship && !ps[1].catch_ship, "the ship once");
        assert_eq!(h.counts.applied, 1);
        assert!(!h.has_pending());
        // A flood is bounded.
        for _ in 0..20 {
            h.on_message(
                Role::Host,
                PeerId::HOST,
                PeerId(1),
                &Message::SingularityEntered(entry(PeerId(1))),
                Some(20),
                now,
            );
        }
        assert_eq!(h.take_pending(false, true, now).len(), MAX_PENDING);
    }

    #[test]
    fn a_ship_part_way_through_its_own_drain_ends_on_the_same_share() {
        // Not draining: the whole share.
        assert!((still_to_take(0.16, 1.0) - 0.16).abs() < 1e-9);
        // Its own drain took 0.5 already: 0.32 more, 0.16 in all.
        let more = still_to_take(0.16, 0.5);
        assert!((more - 0.32).abs() < 1e-9);
        assert!((0.5 * more - 0.16).abs() < 1e-9);
        // Already past the share: nothing more.
        assert!((still_to_take(0.16, 0.1) - 1.0).abs() < 1e-9);
        // Not a number: the share as sent.
        assert!((still_to_take(0.16, f64::NAN) - 0.16).abs() < 1e-9);
    }
}
