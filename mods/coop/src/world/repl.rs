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

//! Replication constants and the client's render clock. Positions are always
//! as the host has them, never folded (`ndml_net::interp`, segments); a copy
//! is put on the host's interpolated pose after its own Step.

#![forbid(unsafe_code)]

use std::time::Duration;

/// Host: how often the changed part of each replicated instance's state is
/// sent again (a repair).
pub const REPAIR_EVERY: Duration = Duration::from_secs(1);

/// Host: state bytes sent per frame at most, the rest waiting oldest first.
pub const PACE_BYTES: usize = 16 * 1024;

/// A render clock that advances one game step per run step.
///
/// The game runs its steps unevenly in wall time while its enemies move a
/// fixed distance per step, so a copy placed at a wall-clock render time
/// judders. This clock adds one step per run step and drifts towards the
/// wall-clock time (`target`) by [`GAIN`](Self::GAIN) of the difference,
/// snapping past [`SNAP_MS`](Self::SNAP_MS) (a stall, a resync, a clock jump).
#[derive(Debug, Clone, Copy, Default)]
pub struct StepClock {
    shown_ms: Option<f64>,
}

impl StepClock {
    /// One game step (the game's `room_speed` is 60).
    pub const STEP_MS: f64 = 1000.0 / 60.0;
    /// Share of the difference to the wall-clock time removed per step.
    pub const GAIN: f64 = 0.03;
    /// Past this difference the clock jumps to the wall-clock time (ms).
    pub const SNAP_MS: f64 = 200.0;

    /// The time to show this run step, given the wall-clock render time.
    pub fn advance(&mut self, target: Option<Duration>) -> Option<Duration> {
        let Some(target) = target else {
            self.shown_ms = None;
            return None;
        };
        let t = target.as_secs_f64() * 1000.0;
        let shown = match self.shown_ms {
            Some(s) => {
                let s = s + Self::STEP_MS;
                let err = t - s;
                if err.abs() > Self::SNAP_MS {
                    t
                } else {
                    s + err * Self::GAIN
                }
            }
            None => t,
        };
        self.shown_ms = Some(shown);
        Some(Duration::from_secs_f64(shown.max(0.0) / 1000.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_step_clock_moves_one_step_per_frame_on_an_uneven_wall_clock() {
        let mut c = StepClock::default();
        // Uneven wall time: 2, 32, 16 ms apart.
        let pattern = [2u64, 32, 16];
        let mut wall = 10_000u64;
        let mut prev = c.advance(Some(Duration::from_millis(wall))).unwrap();
        let mut steps = Vec::new();
        for i in 0..600 {
            wall += pattern[i % 3];
            let t = c.advance(Some(Duration::from_millis(wall))).unwrap();
            steps.push(t.saturating_sub(prev).as_secs_f64() * 1000.0);
            prev = t;
        }
        // Every step close to 16.7 ms, and the clock stays near the wall.
        assert!(
            steps[60..].iter().all(|s| (s - 16.67).abs() < 1.5),
            "{steps:?}"
        );
        let last = prev.as_secs_f64() * 1000.0;
        assert!((last - f64::from(u32::try_from(wall).unwrap())).abs() < 30.0);
        // A big jump snaps; losing the time resets.
        let t = c.advance(Some(Duration::from_millis(wall + 5000))).unwrap();
        assert_eq!(t, Duration::from_millis(wall + 5000));
        assert_eq!(c.advance(None), None);
    }
}
