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

//! A [`Transport`] wrapper that simulates a bad network on the send side:
//! latency, jitter, loss, duplication, reordering and a capped link rate,
//! all from a seeded [`Rng`] so runs are reproducible.
//!
//! Held datagrams go out on the next `send_to` or `recv_from` call (no thread).

#![forbid(unsafe_code)]

use std::collections::BinaryHeap;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use crate::rng::Rng;
use crate::transport::Transport;

/// What the conditioner does to each outgoing datagram.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conditions {
    /// Base one-way delay added to every datagram.
    pub latency: Duration,
    /// Spread around `latency` (triangular distribution).
    pub jitter: Duration,
    /// Probability in `[0, 1]` that a datagram is dropped.
    pub loss: f64,
    /// Probability that a delivered datagram is delivered twice.
    pub duplicate: f64,
    /// Probability that a datagram is held back an extra `latency + jitter`.
    pub reorder: f64,
    /// Link rate in bytes per second (0: unlimited); a datagram that would
    /// wait longer than `queue` is dropped.
    pub rate: f64,
    pub queue: Duration,
}

/// IPv4 and UDP header bytes a datagram takes on the link.
pub const LINK_OVERHEAD: usize = 28;
/// A capped link's queue when none is given.
pub const DEFAULT_QUEUE: Duration = Duration::from_millis(250);

impl Default for Conditions {
    fn default() -> Self {
        Self {
            latency: Duration::ZERO,
            jitter: Duration::ZERO,
            loss: 0.0,
            duplicate: 0.0,
            reorder: 0.0,
            rate: 0.0,
            queue: DEFAULT_QUEUE,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Held {
    due: Duration,
    order: u64,
    to: SocketAddr,
    data: Vec<u8>,
}

impl Ord for Held {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap on (due, order).
        (other.due, other.order).cmp(&(self.due, self.order))
    }
}

impl PartialOrd for Held {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Counters for what the conditioner did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConditionerStats {
    /// Datagrams handed to `send_to`.
    pub offered: u64,
    /// Datagrams dropped.
    pub dropped: u64,
    /// Extra copies created.
    pub duplicated: u64,
    /// Datagrams deliberately held back.
    pub reordered: u64,
    /// Of `dropped`, those dropped by a full link queue.
    pub overflowed: u64,
}

/// See the module docs.
#[derive(Debug)]
pub struct Conditioner<T> {
    inner: T,
    conditions: Conditions,
    rng: Rng,
    held: BinaryHeap<Held>,
    order: u64,
    stats: ConditionerStats,
    /// When the capped link has sent everything queued so far.
    link_free: Duration,
}

impl<T: Transport> Conditioner<T> {
    /// Wrap `inner`. The same `seed` gives the same losses and delays.
    pub fn new(inner: T, conditions: Conditions, seed: u64) -> Self {
        Self {
            inner,
            conditions,
            rng: Rng::new(seed),
            held: BinaryHeap::new(),
            order: 0,
            stats: ConditionerStats::default(),
            link_free: Duration::ZERO,
        }
    }

    /// Change the conditions for datagrams sent from now on.
    pub fn set_conditions(&mut self, conditions: Conditions) {
        self.conditions = conditions;
    }

    /// What has been done so far.
    #[must_use]
    pub fn stats(&self) -> ConditionerStats {
        self.stats
    }

    /// The wrapped transport.
    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    fn delay(&mut self) -> Duration {
        let c = self.conditions;
        let j = c.jitter.as_secs_f64();
        // Triangular in [-j, j].
        let offset = j * (self.rng.next_f64() + self.rng.next_f64() - 1.0);
        let mut d = (c.latency.as_secs_f64() + offset).max(0.0);
        if self.rng.chance(c.reorder) {
            self.stats.reordered += 1;
            d += c.latency.as_secs_f64() + j + 0.005;
        }
        Duration::from_secs_f64(d)
    }

    /// When a datagram of `len` bytes leaves the link, or `None` (queue full).
    fn on_link(&mut self, now: Duration, len: usize) -> Option<Duration> {
        let c = self.conditions;
        if c.rate <= 0.0 {
            return Some(now);
        }
        let start = self.link_free.max(now);
        if start.saturating_sub(now) > c.queue {
            return None;
        }
        #[allow(clippy::cast_precision_loss)] // datagram sizes
        let took = Duration::from_secs_f64((len + LINK_OVERHEAD) as f64 / c.rate);
        self.link_free = start + took;
        Some(self.link_free)
    }

    fn hold(&mut self, due: Duration, to: SocketAddr, data: Vec<u8>) {
        self.order += 1;
        self.held.push(Held {
            due,
            order: self.order,
            to,
            data,
        });
    }

    /// Hand every datagram that is due at `now` to the inner transport.
    pub fn flush(&mut self, now: Duration) -> io::Result<()> {
        while self.held.peek().is_some_and(|h| h.due <= now) {
            let Some(h) = self.held.pop() else { break };
            self.inner.send_to(now, h.to, &h.data)?;
        }
        Ok(())
    }
}

impl<T: Transport> Transport for Conditioner<T> {
    fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        self.stats.offered += 1;
        if self.rng.chance(self.conditions.loss) {
            self.stats.dropped += 1;
            return self.flush(now);
        }
        let Some(sent) = self.on_link(now, data.len()) else {
            self.stats.dropped += 1;
            self.stats.overflowed += 1;
            return self.flush(now);
        };
        let d = self.delay();
        self.hold(sent + d, to, data.to_vec());
        if self.rng.chance(self.conditions.duplicate) {
            self.stats.duplicated += 1;
            let d2 = self.delay();
            self.hold(sent + d2, to, data.to_vec());
        }
        self.flush(now)
    }

    fn recv_from(
        &mut self,
        now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        self.flush(now)?;
        self.inner.recv_from(now, buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Every held datagram goes out now, in due order.
    fn release_held(&mut self, now: Duration) -> io::Result<()> {
        while let Some(h) = self.held.pop() {
            self.inner.send_to(now, h.to, &h.data)?;
        }
        self.inner.release_held(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::LoopbackNet;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    #[test]
    fn delays_drops_and_duplicates_reproducibly() {
        let run = |seed| {
            let net = LoopbackNet::new();
            let conditions = Conditions {
                latency: Duration::from_millis(50),
                jitter: Duration::from_millis(10),
                loss: 0.2,
                duplicate: 0.1,
                reorder: 0.05,
                ..Conditions::default()
            };
            let mut a = Conditioner::new(net.bind(addr(1)).unwrap(), conditions, seed);
            let mut b = net.bind(addr(2)).unwrap();
            let mut arrivals = Vec::new();
            let mut buf = [0u8; 8];
            for ms in 0..2000u64 {
                let now = Duration::from_millis(ms);
                if ms < 1000 {
                    a.send_to(now, addr(2), &ms.to_le_bytes()).unwrap();
                }
                a.flush(now).unwrap();
                while let Some((n, _)) = b.recv_from(now, &mut buf).unwrap() {
                    assert_eq!(n, 8);
                    let sent = u64::from_le_bytes(buf);
                    arrivals.push((sent, ms));
                }
            }
            (arrivals, a.stats())
        };
        let (arrivals, stats) = run(42);
        assert_eq!(run(42).0, arrivals, "seeded runs repeat exactly");
        assert_eq!(stats.offered, 1000);
        // Roughly 20 % loss and 10 % duplication.
        assert!((150..250).contains(&stats.dropped), "{stats:?}");
        assert!((40..130).contains(&stats.duplicated), "{stats:?}");
        assert_eq!(
            arrivals.len() as u64,
            stats.offered - stats.dropped + stats.duplicated
        );
        // Delays stay within latency ± jitter, except reordered ones.
        let max_normal = arrivals
            .iter()
            .map(|(s, r)| r - s)
            .filter(|d| *d <= 61)
            .count();
        assert!(max_normal as u64 >= arrivals.len() as u64 - 2 * stats.reordered - 2);
        assert!(arrivals.iter().all(|(s, r)| r - s >= 40));
        // Something arrived out of order.
        assert!(arrivals.windows(2).any(|w| w[1].0 < w[0].0));
    }

    #[test]
    fn a_capped_link_spaces_datagrams_and_drops_past_its_queue() {
        let net = LoopbackNet::new();
        let conditions = Conditions {
            rate: 10_000.0,
            queue: Duration::from_millis(100),
            ..Conditions::default()
        };
        let mut a = Conditioner::new(net.bind(addr(1)).unwrap(), conditions, 1);
        let mut b = net.bind(addr(2)).unwrap();
        // 100 bytes each: 10 ms on the link; 100 ms of queue.
        let data = [7u8; 72];
        for _ in 0..50 {
            a.send_to(Duration::ZERO, addr(2), &data).unwrap();
        }
        assert_eq!(a.stats().overflowed, 39);
        assert_eq!(a.stats().dropped, 39);
        let mut buf = [0u8; 128];
        let mut arrived = Vec::new();
        for ms in 0..300u64 {
            let now = Duration::from_millis(ms);
            a.flush(now).unwrap();
            while b.recv_from(now, &mut buf).unwrap().is_some() {
                arrived.push(ms);
            }
        }
        let want: Vec<u64> = (1..=11).map(|i| i * 10).collect();
        assert_eq!(arrived, want);
    }
}
