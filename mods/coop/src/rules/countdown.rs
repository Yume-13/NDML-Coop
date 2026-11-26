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

//! "Count down, then go" (pure): the numbers 3, 2, 1 a second apart, then
//! go. Used for the resume of a shared hold ([`super::machine`]), the start
//! of a run and the respawn alert of a downed ship.
//!
//! `lead` delays the whole count: the host learns of a release one link delay
//! before the joining game and waits half the round trip.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

/// What happened since the last [`Countdown::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing new.
    Wait,
    /// The number `n` is now showing (3, 2, 1 for a 3 s count): its tick.
    Tick(u8),
    /// The count is over: go (once).
    Go,
}

/// A count of whole seconds that ends at a fixed moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Countdown {
    ends: Instant,
    /// The first number (the length in whole seconds).
    top: u8,
    /// The number last announced (`u8::MAX` before the first).
    told: u8,
    over: bool,
}

impl Countdown {
    /// A count of `length` (whole seconds are the numbers shown) that starts
    /// `lead` after `now`.
    #[must_use]
    pub fn new(now: Instant, length: Duration, lead: Duration) -> Self {
        Self {
            ends: now + lead + length,
            top: secs_up(length),
            told: u8::MAX,
            over: false,
        }
    }

    /// Time left until go.
    #[must_use]
    pub fn left(&self, now: Instant) -> Duration {
        self.ends.saturating_duration_since(now)
    }

    /// The number showing at `now`: the whole seconds left, rounded up, and
    /// the first number through the lead. `None` once over.
    #[must_use]
    pub fn number(&self, now: Instant) -> Option<u8> {
        let left = self.left(now);
        (!left.is_zero()).then(|| secs_up(left).min(self.top))
    }

    /// How far into the current second the count is, 0 to 1 (for a pulse).
    #[must_use]
    pub fn phase(&self, now: Instant) -> f64 {
        let ms = self.left(now).as_millis() % 1000;
        // 1000 ms left in the second at its start: phase 0.
        #[allow(clippy::cast_precision_loss)] // under 1000
        let p = 1.0 - ms as f64 / 1000.0;
        if ms == 0 { 0.0 } else { p }
    }

    /// The count at `now`: a new number's tick, go, or nothing.
    pub fn step(&mut self, now: Instant) -> Step {
        if self.over {
            return Step::Wait;
        }
        let left = self.left(now);
        if left.is_zero() {
            self.over = true;
            return Step::Go;
        }
        // Through the lead the first number shows, silent: its tick comes
        // when its second starts, with the partner's.
        let n = secs_up(left);
        if n <= self.top && n != self.told {
            self.told = n;
            Step::Tick(n)
        } else {
            Step::Wait
        }
    }

    /// Went (the go was handed out).
    #[must_use]
    pub fn is_over(&self) -> bool {
        self.over
    }
}

/// Whole seconds in `d`, rounded up, as shown (at most 255).
#[must_use]
pub fn secs_up(d: Duration) -> u8 {
    // In nanoseconds: under a millisecond left is still the number 1, never
    // a 0 from the milliseconds rounded down.
    let s = d.as_nanos().div_ceil(1_000_000_000);
    u8::try_from(s).unwrap_or(u8::MAX)
}

/// The numbers of the respawn alert: the last whole seconds a downed ship
/// waits, `left` being the respawn clock's time left. `None` above
/// [`ALERT_FROM`] seconds or at 0.
#[must_use]
pub fn alert_number(left: Duration) -> Option<u8> {
    let n = secs_up(left);
    (1..=ALERT_FROM).contains(&n).then_some(n)
}

/// The respawn alert's first number.
pub const ALERT_FROM: u8 = 3;

/// The count at a run's start, once both players are ready.
pub const START_COUNT: Duration = Duration::from_secs(3);

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn three_ticks_a_second_apart_then_go_once() {
        let t = Instant::now();
        let mut c = Countdown::new(t, 3 * S, Duration::ZERO);
        assert_eq!(c.step(t), Step::Tick(3));
        assert_eq!(c.step(t + S / 2), Step::Wait);
        assert_eq!(c.step(t + S + Duration::from_millis(1)), Step::Tick(2));
        assert_eq!(c.step(t + 2 * S + Duration::from_millis(1)), Step::Tick(1));
        assert_eq!(c.number(t + 2 * S + S / 2), Some(1));
        assert_eq!(c.step(t + 3 * S), Step::Go);
        assert_eq!(c.step(t + 4 * S), Step::Wait, "go once");
        assert!(c.is_over());
    }

    #[test]
    fn a_lead_moves_the_whole_count() {
        let t = Instant::now();
        let lead = Duration::from_millis(85);
        let mut host = Countdown::new(t, 3 * S, lead);
        // The joining game hears of the release one link delay later.
        let mut client = Countdown::new(t + lead, 3 * S, Duration::ZERO);
        assert_eq!(host.left(t + lead), client.left(t + lead));
        assert_eq!(host.number(t), Some(3), "the lead shows the first number");
        assert_eq!(host.step(t), Step::Wait, "its tick waits for the partner's");
        assert_eq!(host.step(t + lead), Step::Tick(3));
        assert_eq!(client.step(t + lead), Step::Tick(3));
        assert_eq!(host.step(t + 3 * S + lead), Step::Go);
        assert_eq!(client.step(t + 3 * S + lead), Step::Go);
    }

    #[test]
    fn a_skipped_frame_still_ticks_the_number_it_lands_on() {
        let t = Instant::now();
        let mut c = Countdown::new(t, 3 * S, Duration::ZERO);
        assert_eq!(c.step(t), Step::Tick(3));
        assert_eq!(c.step(t + 2 * S + S / 2), Step::Tick(1), "2 was skipped");
        assert_eq!(c.step(t + 5 * S), Step::Go);
    }

    #[test]
    fn under_a_millisecond_left_is_still_one() {
        let t = Instant::now();
        let mut c = Countdown::new(t, 3 * S, Duration::ZERO);
        assert_eq!(c.step(t), Step::Tick(3));
        let late = t + 2 * S + Duration::from_micros(999_700);
        assert_eq!(c.number(late), Some(1));
        assert_eq!(c.step(late), Step::Tick(1), "no number 0");
        assert_eq!(c.step(late), Step::Wait);
        assert_eq!(c.step(t + 3 * S), Step::Go);
        assert_eq!(secs_up(Duration::from_micros(1)), 1);
        assert_eq!(alert_number(Duration::from_micros(400)), Some(1));
    }

    #[test]
    fn the_phase_runs_through_each_second() {
        let t = Instant::now();
        let c = Countdown::new(t, 3 * S, Duration::ZERO);
        assert!(c.phase(t) < 0.01);
        assert!((c.phase(t + S / 2) - 0.5).abs() < 0.01);
    }

    #[test]
    fn the_respawn_alert_numbers_are_the_last_three_seconds() {
        assert_eq!(alert_number(Duration::from_millis(3500)), None);
        assert_eq!(alert_number(Duration::from_millis(3000)), Some(3));
        assert_eq!(alert_number(Duration::from_millis(2001)), Some(3));
        assert_eq!(alert_number(Duration::from_millis(2000)), Some(2));
        assert_eq!(alert_number(Duration::from_millis(1)), Some(1));
        assert_eq!(alert_number(Duration::ZERO), None);
    }
}
