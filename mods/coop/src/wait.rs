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

//! A co-op run's wait for a lost link. Pure.
//!
//! Nothing ends the wait by itself. A silence under [`WAIT_AFTER`] plays
//! through; past it the run waits: the joining game freezes at once, the host
//! plays on for `host_plays_on_seconds`, then freezes too. The link back ends
//! the wait; a freeze of [`LONG_FREEZE`] or more becomes the shared pause.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

/// Silence after which a co-op run waits for the link.
pub const WAIT_AFTER: Duration = Duration::from_secs(3);
/// A freeze this long turns into the shared pause at the return.
pub const LONG_FREEZE: Duration = Duration::from_secs(10);
/// The host plays on alone this long by default (`host_plays_on_seconds`).
pub const PLAYS_ON_DEFAULT: Duration = Duration::from_secs(15);
/// A frame gap this long is this game's own stall: silence counts from the wake.
pub const OWN_STALL: Duration = Duration::from_secs(1);

/// This frame's facts.
#[derive(Debug, Clone, Copy)]
pub struct WaitInput {
    /// A co-op run whose partner is still in it.
    pub active: bool,
    /// The partner is linked.
    pub linked: bool,
    /// While linked: how long nothing has arrived from the partner.
    pub silent_for: Option<Duration>,
    /// This game hosts the session.
    pub is_host: bool,
    /// Host: how long it plays on alone.
    pub plays_on: Duration,
    /// When this game last woke from its own stall ([`OWN_STALL`]).
    pub woke: Option<Instant>,
    pub now: Instant,
}

/// What changed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEvent {
    /// The wait began: the partner counts as gone for the session rules.
    Lost,
    /// The link is back. `long`: frozen [`LONG_FREEZE`] or more.
    Back { long: bool },
}

/// What the player is shown about the wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitView {
    /// Nothing (linked, a blip, or no co-op run to wait in).
    None,
    /// Host: playing on alone; the game pauses in `left`.
    PlayingOn { left: Duration },
    /// Frozen until the link is back.
    Frozen,
}

/// See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkWait {
    /// Waiting since (the last packet heard).
    since: Option<Instant>,
    /// Frozen since.
    frozen_at: Option<Instant>,
    /// The role when the wait began (an unlinked game cannot tell).
    host: bool,
    plays_on: Duration,
}

impl LinkWait {
    /// One frame.
    pub fn update(&mut self, i: &WaitInput) -> Option<WaitEvent> {
        if !i.active {
            *self = Self::default();
            return None;
        }
        let silence = i.silent_for.filter(|_| i.linked);
        let Some(_) = self.since else {
            if i.woke
                .is_some_and(|w| i.now.saturating_duration_since(w) < WAIT_AFTER)
            {
                // A local stall: give the link its usual grace from the wake.
                return None;
            }
            let start = match silence {
                Some(s) if s >= WAIT_AFTER => i.now.checked_sub(s).unwrap_or(i.now),
                _ if !i.linked => i.now,
                _ => return None,
            };
            // After a local stall, the partner's time counts from the wake.
            let start = i.woke.map_or(start, |w| start.max(w));
            self.since = Some(start);
            self.host = i.is_host;
            self.plays_on = i.plays_on;
            self.freeze_check(i.now);
            return Some(WaitEvent::Lost);
        };
        if i.linked && silence.is_some_and(|s| s < WAIT_AFTER) {
            let long = self
                .frozen_at
                .is_some_and(|t| i.now.saturating_duration_since(t) >= LONG_FREEZE);
            *self = Self::default();
            return Some(WaitEvent::Back { long });
        }
        self.plays_on = i.plays_on;
        self.freeze_check(i.now);
        None
    }

    fn freeze_check(&mut self, now: Instant) {
        if self.frozen_at.is_some() {
            return;
        }
        let Some(since) = self.since else { return };
        if !self.host || now.saturating_duration_since(since) >= self.plays_on {
            self.frozen_at = Some(now);
        }
    }

    /// The run waits for the link.
    #[must_use]
    pub fn waiting(&self) -> bool {
        self.since.is_some()
    }

    /// The game must be held frozen now.
    #[must_use]
    pub fn hold(&self) -> bool {
        self.frozen_at.is_some()
    }

    /// What to show.
    #[must_use]
    pub fn view(&self, now: Instant) -> WaitView {
        match (self.since, self.frozen_at) {
            (None, _) => WaitView::None,
            (Some(_), Some(_)) => WaitView::Frozen,
            (Some(since), None) => WaitView::PlayingOn {
                left: self
                    .plays_on
                    .saturating_sub(now.saturating_duration_since(since)),
            },
        }
    }

    /// Forget the wait (the partner gone, a new run).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    struct Fixture {
        w: LinkWait,
        t0: Instant,
        host: bool,
        woke: Option<f64>,
    }

    impl Fixture {
        fn new(host: bool) -> Self {
            Self {
                w: LinkWait::default(),
                t0: Instant::now(),
                host,
                woke: None,
            }
        }

        /// The frame at `at` (from the start) with the link `linked` and
        /// `silent` seconds of silence (while linked).
        fn at(&mut self, at: f64, linked: bool, silent: f64) -> Option<WaitEvent> {
            self.w.update(&WaitInput {
                active: true,
                linked,
                silent_for: linked.then(|| Duration::from_secs_f64(silent)),
                is_host: self.host,
                plays_on: PLAYS_ON_DEFAULT,
                woke: self.woke.map(|w| self.t0 + Duration::from_secs_f64(w)),
                now: self.t0 + Duration::from_secs_f64(at),
            })
        }

        /// A cut from 10 s to `10 + len` (the endpoint times out after 10 s
        /// of silence and reconnects at the end), every 100 ms; the events.
        fn cut(&mut self, len: f64) -> Vec<(f64, WaitEvent)> {
            let mut evs = Vec::new();
            let mut t = 0.0;
            while t < 10.0 + len + 5.0 {
                let silent = if (10.0..10.0 + len).contains(&t) {
                    t - 10.0
                } else {
                    0.0
                };
                let linked = silent < 10.0;
                if let Some(e) = self.at(t, linked, silent) {
                    evs.push((t, e));
                }
                t = ((t + 0.1) * 10.0).round() / 10.0;
            }
            evs
        }

        fn now(&self, at: f64) -> Instant {
            self.t0 + Duration::from_secs_f64(at)
        }
    }

    /// A local 12 s stall wakes to a timed-out link: no wait if the
    /// partner answers; otherwise the wait counts from the wake.
    #[test]
    fn a_local_stall_gets_the_usual_grace() {
        for host in [false, true] {
            let mut r = Fixture::new(host);
            for k in 0..=100 {
                assert_eq!(r.at(f64::from(k) * 0.1, true, 0.0), None);
            }
            // Asleep 10.0 .. 22.0; wakes unlinked, then linked again.
            r.woke = Some(22.0);
            assert_eq!(r.at(22.0, false, 0.0), None, "host {host}");
            assert_eq!(r.at(22.1, true, 12.1), None);
            assert_eq!(r.at(22.2, true, 0.0), None);
            assert!(!r.w.waiting() && !r.w.hold());
        }
        let mut r = Fixture::new(true);
        r.at(10.0, true, 0.0);
        r.woke = Some(22.0);
        let mut evs = Vec::new();
        let mut t = 22.0;
        while t < 36.95 {
            if let Some(e) = r.at(t, true, t - 10.0) {
                evs.push((t, e));
            }
            t = ((t + 0.1) * 10.0).round() / 10.0;
        }
        assert_eq!(evs, vec![(25.0, WaitEvent::Lost)]);
        assert!(!r.w.hold() && r.w.view(r.now(36.9)) != WaitView::Frozen);
        assert!(r.w.waiting());
        r.at(37.0, true, 27.0);
        assert!(r.w.hold(), "15 s after the wake");
    }

    #[test]
    fn a_blip_under_three_seconds_plays_through() {
        for host in [false, true] {
            let mut r = Fixture::new(host);
            assert!(r.cut(2.0).is_empty(), "host {host}");
            assert!(!r.w.hold());
        }
    }

    #[test]
    fn an_eight_second_cut_freezes_the_joining_game_only() {
        let mut j = Fixture::new(false);
        let evs = j.cut(8.0);
        assert_eq!(
            evs,
            vec![
                (13.0, WaitEvent::Lost),
                (18.0, WaitEvent::Back { long: false })
            ]
        );
        let mut j = Fixture::new(false);
        j.at(0.0, true, 0.0);
        j.at(13.0, true, 3.0);
        assert!(j.w.hold(), "frozen from the start of the wait");
        assert_eq!(j.w.view(j.now(13.0)), WaitView::Frozen);
        let mut h = Fixture::new(true);
        for (t, s) in [(13.0, 3.0), (17.9, 7.9)] {
            h.at(t, true, s);
            assert!(!h.w.hold(), "the host plays on");
        }
        assert_eq!(
            h.w.view(h.now(17.9)),
            WaitView::PlayingOn {
                left: Duration::from_millis(7100)
            }
        );
        assert_eq!(h.at(18.0, true, 0.0), Some(WaitEvent::Back { long: false }));
        assert_eq!(h.w.view(h.now(18.0)), WaitView::None);
    }

    #[test]
    fn a_twenty_five_second_cut_freezes_the_host_from_fifteen() {
        let mut h = Fixture::new(true);
        h.at(0.0, true, 0.0);
        h.at(13.0, true, 3.0);
        h.at(24.9, false, 0.0);
        assert!(!h.w.hold());
        h.at(25.0, false, 0.0);
        assert!(h.w.hold(), "15 s after the last packet");
        assert_eq!(h.at(35.0, true, 0.0), Some(WaitEvent::Back { long: true }));
        let mut j = Fixture::new(false);
        let evs = j.cut(25.0);
        assert_eq!(evs[1], (35.0, WaitEvent::Back { long: true }));
    }

    #[test]
    fn a_freeze_under_ten_seconds_resumes_by_itself() {
        // Joining game: a 12 s cut is a 9 s freeze; a 13 s cut, 10 s.
        assert_eq!(
            Fixture::new(false).cut(12.0)[1].1,
            WaitEvent::Back { long: false }
        );
        assert_eq!(
            Fixture::new(false).cut(13.0)[1].1,
            WaitEvent::Back { long: true }
        );
        // Host: never frozen inside its 15 s; 24 s is a 9 s freeze.
        assert_eq!(
            Fixture::new(true).cut(14.0)[1].1,
            WaitEvent::Back { long: false }
        );
        assert_eq!(
            Fixture::new(true).cut(24.0)[1].1,
            WaitEvent::Back { long: false }
        );
        assert_eq!(
            Fixture::new(true).cut(25.0)[1].1,
            WaitEvent::Back { long: true }
        );
    }

    #[test]
    fn a_cut_of_minutes_ends_nothing() {
        for host in [false, true] {
            let mut r = Fixture::new(host);
            r.at(0.0, true, 0.0);
            r.at(13.0, true, 3.0);
            for t in (20..3600).step_by(7) {
                assert_eq!(r.at(f64::from(t), false, 0.0), None);
                let frozen = !host || t >= 25;
                assert!(r.w.waiting(), "host {host} at {t}");
                assert_eq!(r.w.hold(), frozen, "host {host} at {t}");
            }
            assert_eq!(
                r.at(3600.0, true, 0.0),
                Some(WaitEvent::Back { long: true })
            );
        }
    }

    #[test]
    fn a_run_started_without_the_link_waits_from_its_first_frame() {
        let mut h = Fixture::new(true);
        assert_eq!(h.at(0.0, false, 0.0), Some(WaitEvent::Lost));
        assert!(!h.w.hold());
        h.at(15.0, false, 0.0);
        assert!(h.w.hold());
    }

    #[test]
    fn the_role_is_the_one_from_the_start_of_the_wait() {
        // Unlinked, a game cannot tell its role: the wait keeps it.
        let mut h = Fixture::new(true);
        h.at(13.0, true, 3.0);
        h.host = false;
        h.at(20.0, false, 0.0);
        assert!(!h.w.hold(), "still the host: plays on");
    }

    #[test]
    fn no_wait_outside_a_co_op_run_and_none_kept_after_it() {
        let mut r = Fixture::new(false);
        r.at(13.0, true, 3.0);
        assert!(r.w.hold());
        let off = r.w.update(&WaitInput {
            active: false,
            linked: false,
            silent_for: None,
            is_host: false,
            plays_on: PLAYS_ON_DEFAULT,
            woke: None,
            now: r.now(14.0),
        });
        assert_eq!(off, None, "no co-op run any more: no Back, just over");
        assert!(!r.w.waiting() && !r.w.hold());
        assert_eq!(r.w.view(r.now(14.0)), WaitView::None);
    }

    #[test]
    fn the_host_setting_counts() {
        let mut h = Fixture::new(true);
        h.w.update(&WaitInput {
            active: true,
            linked: true,
            silent_for: Some(3 * S),
            is_host: true,
            plays_on: Duration::ZERO,
            woke: None,
            now: h.now(13.0),
        });
        assert!(h.w.hold(), "0 s: the host freezes with the joining game");
    }
}
