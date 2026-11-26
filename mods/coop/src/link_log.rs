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

//! The link line in the plain log, and the traffic numbers the HUD shows.
//! Pure.
//!
//! One INFO line at each link-wait loss and return, and once per
//! [`TROUBLE_EVERY`] while the link is in trouble: the round trip, the loss,
//! the KB/s each way and the counts since the last line. No names, ids or
//! addresses.
//!
//! The line is written from the game thread (the run tick), never from the
//! network path. Nothing is logged per packet.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ndml_net::endpoint::Stats;

/// While the link is in trouble, one line this often.
pub const TROUBLE_EVERY: Duration = Duration::from_secs(30);
/// The log line's KB/s are over this many seconds.
pub const RATE_SECS: usize = 10;
/// Silence that counts as trouble (the HUD's "no signal").
pub const TROUBLE_SILENCE: Duration = crate::ui::run::NO_SIGNAL_AFTER;
/// A frame gap that counts as trouble (this game's own stall).
pub const TROUBLE_GAP: Duration = crate::wait::OWN_STALL;
/// Bytes in a KB.
const KB: f64 = 1024.0;

/// The endpoint's counters for the partner's connection.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NetCounts {
    pub rtt: Duration,
    /// Recent fraction of this game's packets lost (0..1).
    pub loss: f64,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
    pub resends: u64,
    pub duplicates: u64,
    pub stale_packets: u64,
    pub damaged_packets: u64,
}

impl From<&Stats> for NetCounts {
    fn from(s: &Stats) -> Self {
        Self {
            rtt: s.rtt,
            loss: s.loss,
            bytes_sent: s.bytes_sent,
            bytes_recv: s.bytes_recv,
            resends: s.resends,
            duplicates: s.duplicates,
            stale_packets: s.stale_packets,
            damaged_packets: s.damaged_packets,
        }
    }
}

/// The Steam connector's totals this session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SteamCounts {
    /// Sends Steam refused (`steam_net_packet_send` returned false).
    pub refused: u64,
    /// Datagrams dropped by the bridge between the network and game threads
    /// (either way, on a full channel).
    pub bridge_drops: u64,
}

/// This frame's facts.
#[derive(Debug, Clone, Copy)]
pub struct LinkFrame {
    pub now: Instant,
    /// The partner's connection (`None`: not linked now).
    pub net: Option<NetCounts>,
    /// `None`: Steam is not in use on this machine.
    pub steam: Option<SteamCounts>,
    /// How long nothing new has come from the partner.
    pub silent_for: Option<Duration>,
    /// The run waits for the link (`crate::wait`).
    pub waiting: bool,
}

/// A counter's growth since `base`. A counter below `base` belongs to a new
/// connection and counts whole.
fn grew(now: u64, base: u64) -> u64 {
    if now >= base { now - base } else { now }
}

/// See the module docs.
#[derive(Debug, Clone, Default)]
pub struct LinkLog {
    last_frame: Option<Instant>,
    /// One sample a second: when, bytes sent, bytes received (newest last).
    samples: VecDeque<(Instant, u64, u64)>,
    net: Option<NetCounts>,
    steam: Option<SteamCounts>,
    /// The counters at the last line.
    base_net: Option<NetCounts>,
    base_steam: SteamCounts,
    /// Since the last line (or the run's first frame).
    since: Option<Instant>,
    trouble: bool,
    longest_gap: Duration,
    /// The last loss line (for the return's).
    lost_at: Option<Instant>,
}

impl LinkLog {
    /// Forget everything (outside a co-op run).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// One run frame. Returns the trouble line when one is due.
    pub fn frame(&mut self, f: &LinkFrame) -> Option<String> {
        if let Some(t) = self.last_frame {
            let gap = f.now.saturating_duration_since(t);
            self.longest_gap = self.longest_gap.max(gap);
            if gap >= TROUBLE_GAP {
                self.trouble = true;
            }
        }
        self.last_frame = Some(f.now);
        let since = *self.since.get_or_insert(f.now);
        if let Some(n) = f.net {
            self.sample(f.now, &n);
            if self.base_net.is_none() {
                self.base_net = Some(n);
            }
            self.net = Some(n);
        }
        if let Some(s) = f.steam {
            if self.steam.is_none() {
                self.base_steam = s;
            }
            if s.refused > self.base_steam.refused || s.bridge_drops > self.base_steam.bridge_drops
            {
                self.trouble = true;
            }
            self.steam = Some(s);
        }
        if f.waiting || f.silent_for.is_some_and(|s| s >= TROUBLE_SILENCE) {
            self.trouble = true;
        }
        if f.now.saturating_duration_since(since) < TROUBLE_EVERY {
            return None;
        }
        if self.trouble {
            Some(self.line("trouble in the last 30 s", f.now))
        } else {
            self.restart(f.now);
            None
        }
    }

    fn sample(&mut self, now: Instant, n: &NetCounts) {
        if self
            .samples
            .back()
            .is_some_and(|&(_, s, r)| n.bytes_sent < s || n.bytes_recv < r)
        {
            // A new connection: its counters start again.
            self.samples.clear();
        }
        let due = self
            .samples
            .back()
            .is_none_or(|&(t, _, _)| now.saturating_duration_since(t) >= Duration::from_secs(1));
        if due {
            self.samples.push_back((now, n.bytes_sent, n.bytes_recv));
            while self.samples.len() > RATE_SECS + 1 {
                self.samples.pop_front();
            }
        }
    }

    /// KB/s up and down over the last `secs` samples (one second at the least).
    /// `None` before there are two.
    fn rate(&self, secs: usize) -> Option<(f64, f64)> {
        let new = *self.samples.back()?;
        let i = self.samples.len().saturating_sub(secs + 1);
        let old = *self.samples.get(i)?;
        let dt = new.0.saturating_duration_since(old.0).as_secs_f64();
        #[allow(clippy::cast_precision_loss)] // byte counts for a rate
        (dt >= 0.5).then(|| {
            (
                (new.1 - old.1) as f64 / dt / KB,
                (new.2 - old.2) as f64 / dt / KB,
            )
        })
    }

    /// KB/s up and down over the last second, for the HUD's traffic line.
    #[must_use]
    pub fn traffic(&self) -> Option<(f64, f64)> {
        self.rate(1)
    }

    /// The line for `what` (a loss, a return, trouble). The counts start again
    /// from `now`.
    pub fn line(&mut self, what: &str, now: Instant) -> String {
        let mut s = format!("co-op link: {what}: ");
        match self.net {
            Some(n) => {
                let _ = write!(
                    s,
                    "rtt {:.0} ms, loss {:.1} %",
                    n.rtt.as_secs_f64() * 1000.0,
                    n.loss * 100.0
                );
            }
            None => s.push_str("rtt -, loss -"),
        }
        match self.rate(RATE_SECS) {
            Some((up, down)) => {
                let _ = write!(
                    s,
                    ", up {up:.1} KB/s, down {down:.1} KB/s (last {RATE_SECS} s)"
                );
            }
            None => s.push_str(", up -, down -"),
        }
        let over = self
            .since
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        let _ = write!(s, "; over {:.0} s:", over.as_secs_f64());
        match (self.net, self.base_net) {
            (Some(n), Some(b)) => {
                let _ = write!(
                    s,
                    " resends {}, duplicates {}, stale packets {}, damaged packets {}",
                    grew(n.resends, b.resends),
                    grew(n.duplicates, b.duplicates),
                    grew(n.stale_packets, b.stale_packets),
                    grew(n.damaged_packets, b.damaged_packets)
                );
            }
            _ => s.push_str(" resends -, duplicates -, stale packets -, damaged packets -"),
        }
        match self.steam {
            Some(st) => {
                let _ = write!(
                    s,
                    ", sends Steam refused {}, bridge drops {}",
                    grew(st.refused, self.base_steam.refused),
                    grew(st.bridge_drops, self.base_steam.bridge_drops)
                );
            }
            None => s.push_str(", no Steam"),
        }
        let _ = write!(
            s,
            ", longest frame gap {:.2} s",
            self.longest_gap.as_secs_f64()
        );
        self.restart(now);
        s
    }

    /// A new window from `now`: the counts at this moment are the base.
    fn restart(&mut self, now: Instant) {
        self.since = Some(now);
        self.trouble = false;
        self.longest_gap = Duration::ZERO;
        self.base_net = self.net;
        if let Some(s) = self.steam {
            self.base_steam = s;
        }
    }

    /// The wait began (`crate::wait`): its line.
    pub fn lost(&mut self, now: Instant) -> String {
        self.lost_at = Some(now);
        self.line("lost; the run waits for the link", now)
    }

    /// The wait ended. `long`: the freeze became the shared pause.
    pub fn back(&mut self, now: Instant, long: bool) -> String {
        let after = self
            .lost_at
            .take()
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        let what = format!(
            "back {:.1} s after the loss{}",
            after.as_secs_f64(),
            if long { " (the shared pause)" } else { "" }
        );
        self.line(&what, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Feed {
        log: LinkLog,
        t0: Instant,
        net: NetCounts,
        steam: Option<SteamCounts>,
    }

    impl Feed {
        fn new() -> Self {
            Self {
                log: LinkLog::default(),
                t0: Instant::now(),
                net: NetCounts {
                    rtt: Duration::from_millis(172),
                    loss: 0.04,
                    ..NetCounts::default()
                },
                steam: Some(SteamCounts::default()),
            }
        }

        fn now(&self, s: f64) -> Instant {
            self.t0 + Duration::from_secs_f64(s)
        }

        /// Frames every 1/60 s from `from` to `to` at `up`/`down` bytes a second.
        /// Returns the lines that came out.
        fn run(&mut self, from: f64, to: f64, up: u64, down: u64, silent: f64) -> Vec<String> {
            let mut out = Vec::new();
            let mut k = 0u32;
            loop {
                let t = from + f64::from(k) / 60.0;
                if t >= to {
                    return out;
                }
                k += 1;
                self.net.bytes_sent += up / 60;
                self.net.bytes_recv += down / 60;
                let f = LinkFrame {
                    now: self.now(t),
                    net: Some(self.net),
                    steam: self.steam,
                    silent_for: Some(Duration::from_secs_f64(silent)),
                    waiting: false,
                };
                out.extend(self.log.frame(&f));
            }
        }
    }

    #[test]
    fn a_healthy_link_logs_nothing() {
        let mut r = Feed::new();
        assert!(r.run(0.0, 600.0, 48_000, 21_000, 0.0).is_empty());
    }

    #[test]
    fn a_loss_line_has_every_count_and_no_name() {
        let mut r = Feed::new();
        r.run(0.0, 20.0, 48 * 1024, 21 * 1024, 0.0);
        r.net.resends += 12;
        r.net.duplicates += 3;
        r.net.damaged_packets += 2;
        r.steam = Some(SteamCounts {
            refused: 5,
            bridge_drops: 0,
        });
        r.run(20.0, 21.0, 0, 0, 0.0);
        let line = r.log.line("lost (3.0 s without a packet)", r.now(21.0));
        assert!(
            line.starts_with(
                "co-op link: lost (3.0 s without a packet): rtt 172 ms, loss 4.0 %, up "
            ),
            "{line}"
        );
        assert!(line.contains("KB/s (last 10 s); over 21 s: resends 12, duplicates 3, stale packets 0, damaged packets 2, sends Steam refused 5, bridge drops 0, longest frame gap 0.02 s"), "{line}");
        // The counts start again after a line.
        let line = r.log.line("back", r.now(22.0));
        assert!(line.contains("over 1 s: resends 0, duplicates 0"), "{line}");
        assert!(line.contains("refused 0"), "{line}");
    }

    #[test]
    fn trouble_is_logged_once_per_thirty_seconds_while_it_lasts() {
        let mut r = Feed::new();
        r.run(0.0, 10.0, 1000, 1000, 0.0);
        // Steam refuses sends from 10 s to 70 s.
        let mut lines = Vec::new();
        for k in 0..60u32 {
            r.steam = Some(SteamCounts {
                refused: u64::from(k) + 1,
                bridge_drops: 0,
            });
            let t = 10.0 + f64::from(k);
            lines.extend(r.run(t, t + 1.0, 1000, 1000, 0.0));
        }
        lines.extend(r.run(70.0, 200.0, 1000, 1000, 0.0));
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert!(lines[0].starts_with("co-op link: trouble in the last 30 s: "));
        assert!(lines[0].contains("sends Steam refused 21,"), "{}", lines[0]);
        assert!(lines[1].contains("sends Steam refused 30,"), "{}", lines[1]);
        assert!(lines[2].contains("sends Steam refused 9,"), "{}", lines[2]);
    }

    #[test]
    fn silence_and_a_long_frame_are_trouble() {
        let mut r = Feed::new();
        let lines = r.run(0.0, 31.0, 1000, 1000, 2.5);
        assert_eq!(lines.len(), 1, "no signal");
        let mut r = Feed::new();
        r.run(0.0, 5.0, 1000, 1000, 0.0);
        let lines = r.run(6.5, 40.0, 1000, 1000, 0.0);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].ends_with("longest frame gap 1.52 s"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn rates_are_kb_per_second_and_survive_a_new_connection() {
        let mut r = Feed::new();
        r.run(0.0, 12.0, 60 * 1024, 30 * 1024, 0.0);
        let (up, down) = r.log.traffic().unwrap();
        assert!(
            (up - 60.0).abs() < 1.5 && (down - 30.0).abs() < 1.5,
            "{up} {down}"
        );
        let (up, _) = r.log.rate(RATE_SECS).unwrap();
        assert!((up - 60.0).abs() < 1.5, "{up}");
        // A reconnect: the counters start at 0 again.
        r.net.bytes_sent = 0;
        r.net.bytes_recv = 0;
        r.run(12.0, 12.5, 60 * 1024, 30 * 1024, 0.0);
        assert_eq!(r.log.traffic(), None, "one sample after the restart");
        r.run(12.5, 15.0, 60 * 1024, 30 * 1024, 0.0);
        let (up, _) = r.log.traffic().unwrap();
        assert!((up - 60.0).abs() < 1.5, "{up}");
    }

    #[test]
    fn without_a_link_or_steam_the_line_says_so() {
        let mut log = LinkLog::default();
        let t = Instant::now();
        log.frame(&LinkFrame {
            now: t,
            net: None,
            steam: None,
            silent_for: None,
            waiting: true,
        });
        let line = log.line("lost", t);
        assert_eq!(
            line,
            "co-op link: lost: rtt -, loss -, up -, down -; over 0 s: resends -, duplicates -, stale packets -, damaged packets -, no Steam, longest frame gap 0.00 s"
        );
    }
}
