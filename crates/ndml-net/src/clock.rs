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

//! Tracking a peer's clock (for clients: the host's frame clock) and choosing
//! the interpolation delay.
//!
//! The smoothed `local_recv - remote_send` of each arrival, the **baseline**,
//! gives `arrival_time(local) = local - baseline`; `remote_now` adds half the
//! RTT and `render_time` subtracts the interpolation delay. The baseline is a
//! running mean, then an exponential average (gain 1/32); a run of outliers
//! resets it.

#![forbid(unsafe_code)]

use std::time::Duration;

use crate::time;

/// Samples before [`ClockSync::is_synced`] reports true.
const SYNC_SAMPLES: u64 = 8;
/// Running-mean phase length, then exponential averaging with this gain.
const MEAN_SAMPLES: u64 = 32;
/// Consecutive outliers that reset the baseline.
const OUTLIER_RESET: u32 = 16;
/// Bounds for the interpolation delay.
const MIN_DELAY: Duration = Duration::from_millis(16);
const MAX_DELAY: Duration = Duration::from_millis(500);
/// Snapshot interval, for the interpolation delay ([`ClockSync::interp_delay`]).
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_nanos(33_333_333);

/// See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClockSync {
    baseline: f64,
    jitter: f64,
    last_transit: Option<f64>,
    samples: u64,
    outliers: u32,
}

impl ClockSync {
    /// A clock with no samples.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one arrival (remote send time, local receive time).
    pub fn observe(&mut self, remote_send: Duration, local_recv: Duration) {
        let transit = time::signed_secs(remote_send, local_recv);
        if let Some(last) = self.last_transit {
            // RFC 3550: J += (|D| - J) / 16.
            let d = (transit - last).abs();
            if self.samples < SYNC_SAMPLES || d < 0.25 {
                self.jitter += (d - self.jitter) / 16.0;
            }
        }
        self.last_transit = Some(transit);

        if self.samples == 0 {
            self.baseline = transit;
            self.samples = 1;
            return;
        }
        let dev = transit - self.baseline;
        let limit = (8.0 * self.jitter).max(0.25);
        if dev.abs() > limit && self.samples >= SYNC_SAMPLES {
            self.outliers += 1;
            if self.outliers >= OUTLIER_RESET {
                self.baseline = transit;
                self.samples = 1;
                self.outliers = 0;
            }
            return;
        }
        self.outliers = 0;
        self.samples += 1;
        #[allow(clippy::cast_precision_loss)] // samples <= MEAN_SAMPLES here
        let gain = if self.samples <= MEAN_SAMPLES {
            1.0 / self.samples as f64
        } else {
            1.0 / MEAN_SAMPLES as f64
        };
        self.baseline += dev * gain;
    }

    /// Whether enough samples arrived to trust the estimates.
    #[must_use]
    pub fn is_synced(&self) -> bool {
        self.samples >= SYNC_SAMPLES
    }

    /// Smoothed interarrival jitter (RFC 3550).
    #[must_use]
    pub fn jitter(&self) -> Duration {
        time::secs(self.jitter)
    }

    /// Remote timestamp of data arriving at `local`.
    #[must_use]
    pub fn arrival_time(&self, local: Duration) -> Duration {
        time::secs(local.as_secs_f64() - self.baseline)
    }

    /// Estimate of the remote clock at `local`.
    #[must_use]
    pub fn remote_now(&self, local: Duration, rtt: Duration) -> Duration {
        self.arrival_time(local) + rtt / 2
    }

    /// Remote time to render at `local` with the given interpolation delay.
    #[must_use]
    pub fn render_time(&self, local: Duration, delay: Duration) -> Duration {
        self.arrival_time(local).saturating_sub(delay)
    }

    /// Adaptive interpolation delay for snapshots sent every `snapshot_interval`:
    /// `2 * interval + 2 * jitter`, clamped to 16..500 ms.
    #[must_use]
    pub fn interp_delay(&self, snapshot_interval: Duration) -> Duration {
        (snapshot_interval * 2 + self.jitter() * 2).clamp(MIN_DELAY, MAX_DELAY)
    }

    /// Inverse of [`arrival_time`](Self::arrival_time).
    #[must_use]
    pub fn local_time_of(&self, remote: Duration) -> Duration {
        time::secs(remote.as_secs_f64() + self.baseline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    /// Remote clock is 5 s ahead, one-way 40 ms, jitter ±15 ms, 5 % loss.
    fn feed(clock: &mut ClockSync, rng: &mut Rng, from_ms: u64, to_ms: u64) {
        for ms in (from_ms..to_ms).step_by(16) {
            if rng.chance(0.05) {
                continue;
            }
            let local_send = Duration::from_millis(ms);
            let remote_send = local_send + Duration::from_secs(5);
            let j = 0.015 * (rng.next_f64() + rng.next_f64() - 1.0);
            let recv = local_send + Duration::from_secs_f64(0.040 + j);
            clock.observe(remote_send, recv);
        }
    }

    #[test]
    fn converges_under_jitter() {
        let mut c = ClockSync::new();
        let mut rng = Rng::new(11);
        assert!(!c.is_synced());
        feed(&mut c, &mut rng, 0, 3000);
        assert!(c.is_synced());
        let local = Duration::from_secs(3);
        // True remote now = local + 5 s. RTT 80 ms -> rtt/2 = one-way 40 ms.
        let est = c.remote_now(local, Duration::from_millis(80));
        let err = time::signed_secs(local + Duration::from_secs(5), est).abs();
        assert!(err < 0.003, "error {err}");
        // Data arriving now was sent about 40 ms ago.
        let arr = c.arrival_time(local);
        let err = time::signed_secs(local + Duration::from_millis(4960), arr).abs();
        assert!(err < 0.003, "arrival error {err}");
        // Jitter estimate is in the right range and drives the delay.
        assert!(c.jitter() > Duration::from_millis(2) && c.jitter() < Duration::from_millis(15));
        let d = c.interp_delay(Duration::from_millis(33));
        assert!(
            d > Duration::from_millis(66) && d < Duration::from_millis(100),
            "{d:?}"
        );
        assert_eq!(
            c.local_time_of(c.arrival_time(local)).as_millis(),
            local.as_millis()
        );
    }

    #[test]
    fn render_time_moves_smoothly_and_ignores_a_stall() {
        let mut c = ClockSync::new();
        let mut rng = Rng::new(5);
        feed(&mut c, &mut rng, 0, 2000);
        let mut last = c.render_time(Duration::from_secs(2), Duration::from_millis(80));
        for ms in (2000..6000).step_by(16) {
            feed(&mut c, &mut rng, ms, ms + 16);
            if ms == 4000 {
                // One packet stuck in a queue for 600 ms: ignored.
                c.observe(
                    Duration::from_millis(ms + 5000),
                    Duration::from_millis(ms + 640),
                );
            }
            let now = c.render_time(Duration::from_millis(ms + 16), Duration::from_millis(80));
            let step = time::signed_secs(last, now);
            assert!((0.010..0.022).contains(&step), "step {step} at {ms}");
            last = now;
        }
    }

    #[test]
    fn a_real_clock_jump_resets_the_baseline() {
        let mut c = ClockSync::new();
        let mut rng = Rng::new(9);
        feed(&mut c, &mut rng, 0, 1000);
        // Remote clock now 2 s further ahead for good.
        for ms in (1000..1500).step_by(16) {
            c.observe(
                Duration::from_millis(ms + 7000),
                Duration::from_millis(ms + 40),
            );
        }
        let est = c.arrival_time(Duration::from_millis(1500));
        let err = time::signed_secs(Duration::from_millis(1500 + 6960), est).abs();
        assert!(err < 0.005, "{err}");
    }
}
