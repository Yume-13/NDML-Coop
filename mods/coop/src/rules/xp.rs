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

//! Shared experience level: the host's XP is the shared one; each client
//! takes every level through its own level-up code (`expControl` levels up
//! when `oController.xp >= maxexp` and raises `global.baseLevel`).
//!
//! * The host sends `XpState { xp, level }` (`level` = `global.baseLevel`):
//!   at once on a level change, at most 10 a second for `xp`, every 2 s.
//! * A client writes only `oController.xp`: `maxexp` for each level the host
//!   gained, else the host's `xp` capped below `maxexp`. Levels count from
//!   the host's changes, and writes happen only while the world runs.
//! * Levels a build grants without touching `baseLevel` (challenger kills,
//!   `mGrandeur`, `mQuantumDecoherence`, `mRegression`,
//!   `mTransmogrification`) stay that player's own.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ndml_net::msg::Message;

/// XP values read from the game this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct XpRead {
    /// `oController.xp`.
    pub xp: f64,
    /// `oController.maxexp`.
    pub maxexp: f64,
    /// `global.baseLevel`.
    pub base: f64,
}

/// `xp` changes are sent at most this often.
pub const SEND_EVERY: Duration = Duration::from_millis(100);
/// The state is re-sent this often even without a change.
pub const HEARTBEAT: Duration = Duration::from_secs(2);
/// Frames a client waits for its own `expControl` to take a level.
pub const LEVEL_WAIT_FRAMES: u32 = 30;
/// What a client writes below `maxexp` at most.
const BELOW_MAX: f64 = 0.001;

fn level_of(base: f64) -> u16 {
    if base.is_finite() {
        base.round().clamp(0.0, f64::from(u16::MAX)) as u16
    } else {
        0
    }
}

/// Host side: when to send the XP state.
#[derive(Debug, Clone, Default)]
pub struct HostXp {
    last: Option<(f64, u16)>,
    at: Option<Instant>,
}

impl HostXp {
    /// Forget what was sent (a partner back from a lost link gets it again).
    pub fn resend(&mut self) {
        self.last = None;
    }

    /// The message to send for this frame's reading, if any.
    pub fn sample(&mut self, r: XpRead, now: Instant) -> Option<Message> {
        if !r.xp.is_finite() {
            return None;
        }
        let level = level_of(r.base);
        let since = self.at.map(|t| now.saturating_duration_since(t));
        let due = match self.last {
            None => true,
            Some((_, l)) if l != level => true,
            Some((x, _)) if (x - r.xp).abs() > 1e-9 => since.is_none_or(|s| s >= SEND_EVERY),
            Some(_) => since.is_none_or(|s| s >= HEARTBEAT),
        };
        if !due {
            return None;
        }
        self.last = Some((r.xp, level));
        self.at = Some(now);
        Some(Message::XpState { xp: r.xp, level })
    }
}

/// What a client does with `oController.xp` this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum XpStep {
    /// Nothing.
    None,
    /// Write this value.
    Write(f64),
    /// The game did not take a level within [`LEVEL_WAIT_FRAMES`]; the
    /// levels still owed were dropped (log once).
    GaveUp,
}

/// Client side: mirror the host's state through the game's own level-ups.
#[derive(Debug, Clone, Default)]
pub struct ClientXp {
    host: Option<(f64, u16)>,
    seen: Option<u16>,
    owed: u32,
    caught_up: bool,
    awaiting: Option<(f64, u32)>,
}

impl ClientXp {
    /// The host's `XpState`.
    pub fn on_state(&mut self, xp: f64, level: u16) {
        if let Some(seen) = self.seen
            && level > seen
        {
            log::info!("co-op rules: the host reached level {level}");
            self.owed += u32::from(level - seen);
        }
        self.seen = Some(self.seen.map_or(level, |s| s.max(level)));
        self.host = Some((xp, level));
    }

    /// Levels the host has and this game has not taken yet.
    #[must_use]
    pub fn owed(&self) -> u32 {
        self.owed
    }

    /// This frame: `running` = the world runs (not paused or frozen).
    pub fn apply(&mut self, r: XpRead, running: bool) -> XpStep {
        let Some((host_xp, host_level)) = self.host else {
            return XpStep::None;
        };
        if !self.caught_up {
            // First state of the run: catch up from where this game stands.
            self.caught_up = true;
            self.owed = u32::from(host_level.saturating_sub(level_of(r.base)));
            self.seen = Some(host_level);
        }
        if !running || !r.maxexp.is_finite() || r.maxexp <= 0.0 {
            return XpStep::None;
        }
        if let Some((before, frames)) = self.awaiting {
            if r.base >= before + 0.5 {
                self.awaiting = None;
                self.owed = self.owed.saturating_sub(1);
            } else if frames >= LEVEL_WAIT_FRAMES {
                self.awaiting = None;
                self.owed = 0;
                return XpStep::GaveUp;
            } else {
                self.awaiting = Some((before, frames + 1));
                return XpStep::None;
            }
        }
        if self.owed > 0 {
            self.awaiting = Some((r.base, 0));
            return XpStep::Write(r.maxexp.max(r.xp));
        }
        let target = host_xp.clamp(0.0, (r.maxexp - BELOW_MAX).max(0.0));
        if (r.xp - target).abs() > 1e-6 {
            XpStep::Write(target)
        } else {
            XpStep::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(xp: f64, base: f64) -> XpRead {
        XpRead {
            xp,
            maxexp: 20.0 + base * 14.0,
            base,
        }
    }

    #[test]
    fn host_sends_levels_at_once_and_xp_rate_limited() {
        let t = Instant::now();
        let mut h = HostXp::default();
        assert!(h.sample(read(0.0, 0.0), t).is_some(), "first");
        assert!(h.sample(read(0.0, 0.0), t).is_none(), "unchanged");
        assert!(h.sample(read(3.0, 0.0), t).is_none(), "xp, too soon");
        let m = h.sample(read(3.0, 0.0), t + SEND_EVERY).unwrap();
        assert_eq!(m, Message::XpState { xp: 3.0, level: 0 });
        assert_eq!(
            h.sample(read(0.0, 1.0), t + SEND_EVERY),
            Some(Message::XpState { xp: 0.0, level: 1 }),
            "a level goes out at once"
        );
        assert!(
            h.sample(read(0.0, 1.0), t + SEND_EVERY + HEARTBEAT)
                .is_some()
        );
        h.resend();
        assert!(
            h.sample(read(0.0, 1.0), t + SEND_EVERY + HEARTBEAT)
                .is_some()
        );
    }

    /// Plays the game's `expControl` on a client: at `xp >= maxexp` it resets
    /// and levels up.
    fn exp_control(r: &mut XpRead) {
        if r.xp >= r.maxexp {
            r.xp = 0.0;
            r.base += 1.0;
            r.maxexp = 20.0 + r.base * 14.0;
        }
    }

    #[test]
    fn client_takes_each_level_through_its_own_level_up() {
        let mut c = ClientXp::default();
        let mut game = read(0.0, 0.0);
        assert_eq!(c.apply(game, true), XpStep::None, "nothing heard yet");
        c.on_state(5.0, 0);
        assert_eq!(c.apply(game, true), XpStep::Write(5.0));
        game.xp = 5.0;
        // The host gains two levels at once.
        c.on_state(1.0, 1);
        c.on_state(2.0, 2);
        assert_eq!(c.owed(), 2);
        let mut levels = 0;
        for _ in 0..20 {
            let step = c.apply(game, true);
            assert_ne!(step, XpStep::GaveUp);
            if let XpStep::Write(v) = step {
                game.xp = v;
            }
            let before = game.base;
            exp_control(&mut game);
            if game.base > before {
                levels += 1;
            }
        }
        assert_eq!(levels, 2, "exactly the host's levels");
        assert_eq!(c.owed(), 0);
        assert!((game.base - 2.0).abs() < 1e-9);
        assert!((game.xp - 2.0).abs() < 1e-9, "then the host's xp");
    }

    #[test]
    fn client_never_levels_on_an_xp_value_alone() {
        let mut c = ClientXp::default();
        let game = read(0.0, 0.0);
        c.on_state(25.0, 0); // above this game's maxexp (20): the host levels next step
        let step = c.apply(game, true);
        assert!(
            matches!(step, XpStep::Write(v) if v < game.maxexp),
            "{step:?}"
        );
    }

    #[test]
    fn client_waits_while_frozen_and_catches_up_late() {
        let mut c = ClientXp::default();
        let mut game = read(0.0, 1.0);
        c.on_state(4.0, 3);
        assert_eq!(c.apply(game, false), XpStep::None, "frozen");
        assert_eq!(c.owed(), 2, "caught up from base 1 to 3");
        assert_eq!(c.apply(game, true), XpStep::Write(game.maxexp));
        // The game never levels: give up after the wait.
        let mut last = XpStep::None;
        for _ in 0..=LEVEL_WAIT_FRAMES {
            last = c.apply(game, true);
        }
        assert_eq!(last, XpStep::GaveUp);
        assert_eq!(c.owed(), 0);
        game.xp = 0.0;
        assert_eq!(c.apply(game, true), XpStep::Write(4.0));
    }

    #[test]
    fn a_local_base_change_does_not_hand_out_levels() {
        let mut c = ClientXp::default();
        let mut game = read(0.0, 0.0);
        c.on_state(1.0, 0);
        let _ = c.apply(game, true);
        game.base = 5.0; // a build effect moved baseLevel here
        c.on_state(2.0, 0);
        assert_eq!(c.owed(), 0);
        assert_eq!(c.apply(game, true), XpStep::Write(2.0));
    }
}
