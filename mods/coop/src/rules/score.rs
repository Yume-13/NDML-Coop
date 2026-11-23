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

//! Shared score: the host's score is the run's one score, in play and on the
//! end screen. Only `updateScore` adds to `global.playerScore` (through
//! `scoreVerifier` and `scoreVerifierModulus`).
//!
//! * The host sends `ScoreState` (the three score numbers, `bossKills`,
//!   `starEaterKills`): on change at most 10 a second, every 2 s, and once
//!   more before `GameOver`.
//! * A client writes the host's numbers while its world runs and before
//!   `oDrawEndScore_Create_0` (none heard: 0). Its own `updateScore` is
//!   replayed with 0 points and 0 XP, so the popup still shows; the host
//!   credits its pickups (`rules/orbs.rs`).
//! * The joining game's `global.finalBossDefeated` becomes 1 when the host's
//!   run killed the Star Eater.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ndml_net::msg::Message;

/// Score changes are sent at most this often.
pub const SEND_EVERY: Duration = Duration::from_millis(100);
/// The state is re-sent this often even without a change.
pub const HEARTBEAT: Duration = Duration::from_secs(2);

/// The game's score numbers and the run's kill counters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    /// `global.playerScore`.
    pub score: f64,
    /// `global.scoreVerifier`.
    pub verifier: f64,
    /// `global.scoreVerifierModulus`.
    pub modulus: f64,
    /// `global.bossKills`.
    pub boss_kills: f64,
    /// `global.starEaterKills`.
    pub star_eater_kills: f64,
}

impl Score {
    fn finite(&self) -> bool {
        [
            self.score,
            self.verifier,
            self.modulus,
            self.boss_kills,
            self.star_eater_kills,
        ]
        .iter()
        .all(|v| v.is_finite())
    }

    /// From a `ScoreState`.
    #[must_use]
    pub fn from_message(m: &Message) -> Option<Self> {
        match *m {
            Message::ScoreState {
                score,
                verifier,
                modulus,
                boss_kills,
                star_eater_kills,
            } => Some(Self {
                score,
                verifier,
                modulus,
                boss_kills,
                star_eater_kills,
            }),
            _ => None,
        }
    }

    fn message(self) -> Message {
        Message::ScoreState {
            score: self.score,
            verifier: self.verifier,
            modulus: self.modulus,
            boss_kills: self.boss_kills,
            star_eater_kills: self.star_eater_kills,
        }
    }
}

/// Host side: when to send the score.
#[derive(Debug, Clone, Default)]
pub struct HostScore {
    last: Option<Score>,
    at: Option<Instant>,
}

impl HostScore {
    /// Forget what was sent (a partner back from a lost link gets it again).
    pub fn resend(&mut self) {
        self.last = None;
    }

    /// The message to send for this frame's reading, if any.
    pub fn sample(&mut self, s: Score, now: Instant) -> Option<Message> {
        if !s.finite() {
            return None;
        }
        let since = self.at.map(|t| now.saturating_duration_since(t));
        let due = match self.last {
            None => true,
            Some(l) if l != s => since.is_none_or(|d| d >= SEND_EVERY),
            Some(_) => since.is_none_or(|d| d >= HEARTBEAT),
        };
        if !due {
            return None;
        }
        self.last = Some(s);
        self.at = Some(now);
        Some(s.message())
    }

    /// The last state before `GameOver`, sent whatever the timers say.
    #[must_use]
    pub fn last_word(&mut self, s: Score, now: Instant) -> Option<Message> {
        if !s.finite() {
            return None;
        }
        self.last = Some(s);
        self.at = Some(now);
        Some(s.message())
    }
}

/// Client side: the host's score on this game.
#[derive(Debug, Clone, Default)]
pub struct ClientScore {
    host: Option<Score>,
    skipped: u64,
    writes: u64,
}

impl ClientScore {
    /// The host's `ScoreState`.
    pub fn on_state(&mut self, s: Score) {
        if s.finite() {
            self.host = Some(s);
        }
    }

    /// Forget the host's numbers (a new run).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The host's score as last heard.
    #[must_use]
    pub fn host(&self) -> Option<Score> {
        self.host
    }

    /// This frame: what to write, if this game's numbers differ from the
    /// host's. `running`: the world runs.
    pub fn apply(&mut self, own: Score, running: bool) -> Option<Score> {
        let h = self.host?;
        if !running || h == own {
            return None;
        }
        self.writes += 1;
        Some(h)
    }

    /// Before the end screen's create: the numbers it must read. The host's
    /// last state; none heard: this game's own with a score of 0 (its own
    /// `updateScore` never ran, and a number of its own is never larger).
    #[must_use]
    pub fn end_screen(&self, own: Score) -> Score {
        self.host.unwrap_or(Score {
            score: 0.0,
            verifier: 0.0,
            ..own
        })
    }

    /// The host's run has killed the Star Eater (the joining game's
    /// `finalBossDefeated` becomes 1).
    #[must_use]
    pub fn final_boss(&self) -> bool {
        self.host.is_some_and(|h| h.star_eater_kills > 0.0)
    }

    /// This game's own `updateScore` was skipped.
    pub fn own_skipped(&mut self) {
        self.skipped += 1;
    }

    /// (own `updateScore` calls skipped, writes of the host's score).
    #[must_use]
    pub fn counts(&self) -> (u64, u64) {
        (self.skipped, self.writes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(score: f64) -> Score {
        Score {
            score,
            verifier: 3.0,
            modulus: 13.0,
            boss_kills: 1.0,
            star_eater_kills: 0.0,
        }
    }

    #[test]
    fn the_host_sends_changes_at_most_ten_a_second_and_a_heartbeat() {
        let t0 = Instant::now();
        let mut h = HostScore::default();
        assert!(h.sample(s(0.0), t0).is_some(), "first reading");
        assert!(h.sample(s(0.0), t0 + Duration::from_millis(500)).is_none());
        assert!(
            h.sample(s(130.0), t0 + Duration::from_millis(50)).is_none(),
            "too soon"
        );
        let m = h.sample(s(130.0), t0 + Duration::from_millis(120));
        assert_eq!(m.as_ref().and_then(Score::from_message), Some(s(130.0)));
        assert!(h.sample(s(130.0), t0 + Duration::from_secs(1)).is_none());
        assert!(
            h.sample(s(130.0), t0 + Duration::from_millis(2200))
                .is_some(),
            "heartbeat"
        );
        // A boss kill alone is a change.
        let k = Score {
            boss_kills: 2.0,
            ..s(130.0)
        };
        assert!(h.sample(k, t0 + Duration::from_millis(2400)).is_some());
        // Before GameOver: at once.
        assert!(
            h.last_word(s(260.0), t0 + Duration::from_millis(2410))
                .is_some()
        );
        assert!(h.sample(s(f64::NAN), t0 + Duration::from_secs(9)).is_none());
        h.resend();
        assert!(
            h.sample(s(260.0), t0 + Duration::from_millis(2420))
                .is_some()
        );
    }

    #[test]
    fn the_client_writes_the_hosts_score_while_running_only() {
        let mut c = ClientScore::default();
        assert_eq!(c.apply(s(0.0), true), None, "nothing heard yet");
        c.on_state(s(390.0));
        assert_eq!(c.apply(s(0.0), false), None, "frozen: no write");
        assert_eq!(c.apply(s(0.0), true), Some(s(390.0)));
        assert_eq!(c.apply(s(390.0), true), None, "already the same");
        // Its own pickup code ran: skipped and counted; the host's value
        // stands.
        c.own_skipped();
        assert_eq!(c.apply(s(403.0), true), Some(s(390.0)));
        assert_eq!(c.counts(), (1, 2));
        c.on_state(Score {
            score: f64::INFINITY,
            ..s(0.0)
        });
        assert_eq!(c.host(), Some(s(390.0)), "a bad state is ignored");
    }

    #[test]
    fn the_end_screen_reads_the_hosts_numbers_or_a_safe_zero() {
        let mut c = ClientScore::default();
        let own = Score {
            boss_kills: 0.0,
            ..s(52.0)
        };
        let e = c.end_screen(own);
        assert_eq!((e.score, e.verifier), (0.0, 0.0), "none heard: zero");
        assert!(
            (e.modulus - 13.0).abs() < f64::EPSILON,
            "the game's own modulus stays"
        );
        assert!(!c.final_boss());
        let host = Score {
            star_eater_kills: 1.0,
            ..s(9_100.0)
        };
        c.on_state(host);
        assert_eq!(c.end_screen(own), host, "the host's, kills included");
        assert!(c.final_boss());
        c.clear();
        assert_eq!(c.host(), None);
    }
}
