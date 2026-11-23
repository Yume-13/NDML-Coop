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

//! Pure decisions behind the per-frame pump: when it runs, Steam
//! availability, the hidden listener's life and cost accounting. One active
//! tick: listener, `steam_update()` (the game skips it in the loadout room
//! and end screen), send, receive, lobby getters, then the lobby flow.

#![forbid(unsafe_code)]

use std::time::Duration;

use ndml_net::lobby::ListenerState;

/// Most datagrams sent per tick (about 55 µs each).
pub const MAX_SEND_PER_TICK: usize = 64;
/// Most datagrams received per tick.
pub const MAX_RECV_PER_TICK: usize = 256;
/// While idle (Steam up, no lobby), the pump works every this many ticks.
pub const IDLE_EVERY: u64 = 30;
/// Ticks of pumping after the lobby flow goes idle, so goodbyes still leave.
pub const LINGER_TICKS: u64 = 180;
/// How often availability is checked until it is known (ticks).
pub const AVAILABLE_EVERY: u64 = 60;
/// How many availability checks before giving up for the session.
pub const AVAILABLE_TRIES: u32 = 30;
/// Listener creations that may fail before it is given up for the session.
pub const LISTENER_MAX_FAILURES: u32 = 3;
/// While suppressed, ticks between checks whether the leaderboard is gone.
pub const SUPPRESSED_CHECK_EVERY: u64 = 30;

/// A pump gap this long (the link's own timeout) drops the backlog both ways.
pub const STALE_BACKLOG_AFTER: Duration = Duration::from_secs(10);
/// Most waiting datagrams read and dropped in that one pump.
pub const MAX_DISCARD: usize = 4096;

/// Whether a pump `gap` drops the backlog. Past the link timeout the old
/// link's datagrams would be taken as a reconnect's accept and leave a
/// half-open link.
#[must_use]
pub fn backlog_is_stale(gap: Duration) -> bool {
    gap >= STALE_BACKLOG_AFTER
}

/// Ticks between overlay rechecks while it reads off: Steam hooks the overlay
/// in a few seconds after start, and `IsOverlayEnabled` is false until then.
pub const OVERLAY_RECHECK_EVERY: u64 = 120;

/// Whether to ask `steam_is_overlay_enabled` again this tick.
#[must_use]
pub fn recheck_overlay(overlay: Option<bool>, tick: u64) -> bool {
    overlay == Some(false) && tick.is_multiple_of(OVERLAY_RECHECK_EVERY)
}

/// Whether this tick does work: always while active, every [`IDLE_EVERY`]
/// otherwise.
#[must_use]
pub fn runs(active: bool, tick: u64) -> bool {
    active || tick.is_multiple_of(IDLE_EVERY)
}

/// Whether Steam can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Availability {
    /// Not known yet (checked every [`AVAILABLE_EVERY`] ticks).
    #[default]
    Unknown,
    /// `steam_initialised()` and a nonzero own id.
    Yes,
    /// Not Steam (`global.gog`, no extension), or never came up.
    No,
}

/// What one availability check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// The extension or `steam_initialised` is missing, or `global.gog` is set.
    NotSteam,
    /// Steam isn't (yet) initialised or logged in.
    NotYet,
    /// Up, with the local id.
    Up(u64),
}

/// Fold one check into the availability. `tries` counts checks so far.
#[must_use]
pub fn availability(probe: Probe, tries: u32) -> Availability {
    match probe {
        Probe::NotSteam => Availability::No,
        Probe::Up(id) if id != 0 => Availability::Yes,
        _ if tries >= AVAILABLE_TRIES => Availability::No,
        _ => Availability::Unknown,
    }
}

/// What to do with the listener this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerStep {
    /// Nothing (off, failed, or not this tick).
    Nothing,
    /// Make sure it exists and is active.
    Ensure,
    /// Destroy it and stay away (leaderboard room).
    Destroy,
    /// Suppressed: see whether the leaderboard objects are gone.
    CheckClear,
}

/// The hidden listener's state and creation failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenerPolicy {
    state: ListenerState,
    failures: u32,
}

impl Default for ListenerPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl ListenerPolicy {
    /// A policy: off until the first tick ensures the listener.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: ListenerState::Off,
            failures: 0,
        }
    }

    /// The state for `status.toml` and the UI.
    #[must_use]
    pub fn state(&self) -> ListenerState {
        self.state
    }

    /// Whether invites accepted while the game runs can arrive.
    #[must_use]
    pub fn works(&self) -> bool {
        self.state != ListenerState::Failed
    }

    /// What to do this tick.
    #[must_use]
    pub fn step(&self, leaderboard_room: bool, tick: u64) -> ListenerStep {
        if self.state == ListenerState::Failed {
            return ListenerStep::Nothing;
        }
        if leaderboard_room {
            return if self.state == ListenerState::Suppressed {
                ListenerStep::Nothing
            } else {
                ListenerStep::Destroy
            };
        }
        if self.state == ListenerState::Suppressed {
            return if tick.is_multiple_of(SUPPRESSED_CHECK_EVERY) {
                ListenerStep::CheckClear
            } else {
                ListenerStep::Nothing
            };
        }
        ListenerStep::Ensure
    }

    /// Record an [`ListenerStep::Ensure`] (`Some(false)`: creation failed).
    /// `true` the moment the listener is given up.
    pub fn ensured(&mut self, alive: Option<bool>) -> bool {
        match alive {
            Some(true) => {
                self.state = ListenerState::Active;
                false
            }
            Some(false) => {
                self.failures += 1;
                if self.failures >= LISTENER_MAX_FAILURES {
                    self.state = ListenerState::Failed;
                    return true;
                }
                false
            }
            None => false,
        }
    }

    /// The listener was destroyed to keep out of the leaderboard's way.
    pub fn suppress(&mut self) {
        if self.state != ListenerState::Failed {
            self.state = ListenerState::Suppressed;
        }
    }

    /// The leaderboard is gone: the next tick recreates the listener.
    pub fn clear(&mut self) {
        if self.state == ListenerState::Suppressed {
            self.state = ListenerState::Off;
        }
    }
}

/// Time and traffic of the pump, for the periodic log line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PumpStats {
    /// Ticks that did work.
    pub ticks: u64,
    pub busy: Duration,
    pub max: Duration,
    pub sent: u64,
    pub received: u64,
    /// Sends Steam refused.
    pub send_failed: u64,
}

impl PumpStats {
    /// Record one tick.
    pub fn tick(&mut self, took: Duration, sent: usize, received: usize, failed: usize) {
        self.ticks += 1;
        self.busy += took;
        self.max = self.max.max(took);
        self.sent += sent as u64;
        self.received += received as u64;
        self.send_failed += failed as u64;
    }

    /// One line for the log.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // counters and rates for a log line
    pub fn line(&self, over: Duration) -> String {
        let per = |d: Duration, n: u64| d.as_secs_f64() * 1e6 / (n.max(1) as f64);
        let secs = over.as_secs_f64().max(1e-3);
        format!(
            "{} ticks, mean {:.1} us, max {:.0} us per tick; {:.1} sent/s, {:.1} received/s{}",
            self.ticks,
            per(self.busy, self.ticks),
            self.max.as_secs_f64() * 1e6,
            self.sent as f64 / secs,
            self.received as f64 / secs,
            if self.send_failed > 0 {
                format!(", {} sends refused", self.send_failed)
            } else {
                String::new()
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_stall_past_the_links_timeout_drops_the_backlog() {
        assert!(!backlog_is_stale(Duration::from_millis(17)));
        assert!(!backlog_is_stale(Duration::from_secs(5)));
        assert!(!backlog_is_stale(Duration::from_millis(9_999)));
        assert!(backlog_is_stale(Duration::from_secs(10)));
        assert!(backlog_is_stale(Duration::from_secs(12)));
    }

    #[test]
    fn idle_ticks_are_sparse() {
        assert!(runs(true, 7));
        assert!(!runs(false, 7));
        assert!(runs(false, 30));
    }

    #[test]
    fn an_overlay_read_as_off_is_asked_again() {
        assert!(!recheck_overlay(None, OVERLAY_RECHECK_EVERY));
        assert!(!recheck_overlay(Some(true), OVERLAY_RECHECK_EVERY));
        assert!(recheck_overlay(Some(false), 2 * OVERLAY_RECHECK_EVERY));
        assert!(!recheck_overlay(Some(false), 2 * OVERLAY_RECHECK_EVERY + 1));
        // The idle passes include the recheck ticks.
        assert!(runs(false, OVERLAY_RECHECK_EVERY));
    }

    #[test]
    fn availability_settles() {
        assert_eq!(availability(Probe::NotSteam, 0), Availability::No);
        assert_eq!(availability(Probe::Up(5), 3), Availability::Yes);
        assert_eq!(availability(Probe::Up(0), 3), Availability::Unknown);
        assert_eq!(availability(Probe::NotYet, 1), Availability::Unknown);
        assert_eq!(
            availability(Probe::NotYet, AVAILABLE_TRIES),
            Availability::No
        );
    }

    #[test]
    fn the_listener_keeps_out_of_the_leaderboards_way() {
        let mut p = ListenerPolicy::new();
        assert_eq!(p.step(false, 1), ListenerStep::Ensure);
        assert!(!p.ensured(Some(true)));
        assert_eq!(p.state(), ListenerState::Active);
        // Entering the leaderboard room: destroy once, then wait.
        assert_eq!(p.step(true, 2), ListenerStep::Destroy);
        p.suppress();
        assert_eq!(p.step(true, 3), ListenerStep::Nothing);
        assert_eq!(p.step(false, 31), ListenerStep::Nothing);
        assert_eq!(p.step(false, 60), ListenerStep::CheckClear);
        p.clear();
        assert_eq!(p.step(false, 61), ListenerStep::Ensure);
        // Three failed creations: polling only from then on.
        assert!(!p.ensured(Some(false)));
        assert!(!p.ensured(Some(false)));
        assert!(p.ensured(Some(false)));
        assert_eq!(p.state(), ListenerState::Failed);
        assert!(!p.works());
        assert_eq!(p.step(false, 90), ListenerStep::Nothing);
    }

    #[test]
    fn stats_line() {
        let mut s = PumpStats::default();
        s.tick(Duration::from_micros(100), 2, 3, 0);
        s.tick(Duration::from_micros(300), 2, 1, 1);
        let line = s.line(Duration::from_secs(1));
        assert!(
            line.contains("2 ticks, mean 200.0 us, max 300 us"),
            "{line}"
        );
        assert!(line.contains("4.0 sent/s, 4.0 received/s, 1 sends refused"));
    }
}
