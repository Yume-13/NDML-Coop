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

//! The slow-frames line in the plain log. Pure.
//!
//! One INFO line when this game's frame rate has averaged under [`SLOW_FPS`]
//! over [`WINDOW_SECS`] seconds of running play, one more at most every
//! [`REPEAT_EVERY`] while it lasts, and a closing line with the stretch's
//! length once a whole window averages [`SLOW_FPS`] or more again.
//! Running play is a run on screen, not paused and not frozen in a hold or a
//! link wait. Anything else starts the window again. The game pauses itself
//! while its window is in the background (`pressPause` each unfocused frame).
//!
//! A line holds the window's frame rate, the mod's hook time, the frame's
//! split between step and GUI draw, the CPU shares, the scene's counts, the
//! link and, on the joining game, the host's world speed from its enemy
//! snapshots. No names, ids or paths. It is written from the game thread (the
//! run step), never from the network path.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::link_log::{NetCounts, SteamCounts};

/// Under this average frame rate over a window, the game runs slow.
pub const SLOW_FPS: f64 = 45.0;
/// Seconds of running play in a window.
pub const WINDOW_SECS: usize = 10;
/// While it lasts, one line at most this often.
pub const REPEAT_EVERY: Duration = Duration::from_secs(30);
/// The game's frame budget (`room_speed` 60).
pub const FRAME_BUDGET: Duration = Duration::from_nanos(16_666_667);
/// A frame over two budgets counts as long.
pub const LONG_FRAME: Duration = Duration::from_nanos(2 * 16_666_667);
/// The host's enemy snapshots: one every this many of its steps
/// (`world::host::SNAPSHOT_EVERY`).
pub const HOST_STEPS_PER_SNAPSHOT: u64 = crate::world::host::SNAPSHOT_EVERY;
/// Bytes in a KB (as the link line counts them).
const KB: f64 = 1024.0;

/// This frame's facts (every run step).
#[derive(Debug, Clone, Copy)]
pub struct SlowFrame {
    pub now: Instant,
    /// Running play (see the module docs).
    pub running: bool,
    /// The mod's hook time since the last frame (step side, draw side),
    /// when it was timed all through it.
    pub mod_time: Option<(Duration, Duration)>,
    /// The controller's last GUI draw.
    pub drawn_at: Option<Instant>,
}

/// Counts read once a second (totals; the line shows their growth).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    /// The process's CPU time and this (the game) thread's.
    pub cpu: Option<(Duration, Duration)>,
    pub net: Option<NetCounts>,
    pub steam: Option<SteamCounts>,
    /// Joining game: the host's enemy snapshots taken (distinct times) and
    /// the newest one's stamp (its clock, ms).
    pub host_snaps: Option<(u64, u32)>,
}

/// One second of running play.
#[derive(Debug, Clone, Copy, Default)]
struct Second {
    wall: Duration,
    frames: u32,
    worst: Duration,
    long: u32,
    /// The mod's time, and the frames it was timed in.
    mod_step: Duration,
    mod_draw: Duration,
    timed: u32,
    /// Step to GUI draw, GUI draw to the next step, frames with both.
    to_draw: Duration,
    to_step: Duration,
    split: u32,
    /// The totals at its end.
    end: Totals,
    /// The host's snapshots (2 of its steps each) and its clock's advance over
    /// them. Counted only in a second that follows one with snapshots: a
    /// stretch without enemies sends none.
    host_snaps: u64,
    host_clock_ms: u64,
}

impl Second {
    fn fps(&self) -> f64 {
        let w = self.wall.as_secs_f64();
        if w > 0.0 {
            f64::from(self.frames) / w
        } else {
            0.0
        }
    }
}

/// What a line says (see [`Due::line`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// The first window under [`SLOW_FPS`].
    Slow,
    /// Still slow, `for` since the stretch began.
    Still { slow_for: Duration },
    /// A whole window at [`SLOW_FPS`] or more: the stretch lasted `lasted`.
    Over { lasted: Duration },
}

/// A window's numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub wall: Duration,
    pub fps: f64,
    pub lowest_fps: f64,
    pub worst: Duration,
    /// Share of frames over [`LONG_FRAME`] (0..1).
    pub long: f64,
    /// The mod's time per frame (step, draw) and the seconds it was timed.
    pub mod_time: Option<(Duration, Duration, f64)>,
    /// Per frame: step to GUI draw, GUI draw to the next step.
    pub split: Option<(Duration, Duration)>,
    /// Shares of the wall time (1.0 = one core all the time): process, game
    /// thread.
    pub cpu: Option<(f64, f64)>,
    /// Round trip, loss (0..1), KB/s up and down, resends, sends Steam
    /// refused (`None`: no Steam).
    pub link: Option<(Duration, f64, f64, f64, u64, Option<u64>)>,
    /// The host's world: its steps' time and its clock's (seconds).
    pub host_world: Option<(f64, f64)>,
}

/// How much is going on: counts the mod keeps, read outside the session lock
/// when a line is due.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scene {
    /// "host", "joining" or "solo".
    pub role: &'static str,
    pub wave: Option<u32>,
    pub enemies: usize,
    pub enemy_shots: usize,
    /// The partner's player-side instances drawn here (the mirror).
    pub partner_mirrored: usize,
    /// This player's own shots, drones and constructs the mirror sends.
    pub own: usize,
}

/// A line due now: what it says and its window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Due {
    pub kind: Kind,
    pub window: Window,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stretch {
    Calm,
    /// Slow since `since` (the start of its first window); the last line.
    Slow {
        since: Instant,
        last: Instant,
    },
}

/// See the module docs.
#[derive(Debug, Clone)]
pub struct SlowLog {
    last_frame: Option<Instant>,
    cur: Second,
    /// The last [`WINDOW_SECS`] seconds, oldest first from `head`.
    ring: [Second; WINDOW_SECS],
    len: usize,
    head: usize,
    /// The totals at the window's start.
    start: Option<Totals>,
    /// When the window's oldest second began.
    window_from: Option<Instant>,
    /// The start of each second in the ring (for `window_from`).
    starts: [Option<Instant>; WINDOW_SECS],
    cur_from: Option<Instant>,
    stretch: Stretch,
    /// The totals at the last second's end (or the window's first frame),
    /// and whether the host's snapshots grew in that second.
    last_totals: Option<Totals>,
    snaps_flowing: bool,
    /// The glue's switch: the mod's hooks were timed since the last frame.
    pub hooks_timed: bool,
    /// Time the mod's hooks from the next frame on.
    timing: bool,
}

impl Default for SlowLog {
    fn default() -> Self {
        Self {
            last_frame: None,
            cur: Second::default(),
            ring: [Second::default(); WINDOW_SECS],
            len: 0,
            head: 0,
            start: None,
            window_from: None,
            starts: [None; WINDOW_SECS],
            cur_from: None,
            stretch: Stretch::Calm,
            last_totals: None,
            snaps_flowing: false,
            hooks_timed: false,
            timing: false,
        }
    }
}

impl SlowLog {
    /// The mod's hooks should be timed: a second was slow, or the stretch is on.
    #[must_use]
    pub fn timing(&self) -> bool {
        self.timing
    }

    /// The window starts again (not running play): the stretch stays.
    fn restart_window(&mut self) {
        self.last_frame = None;
        self.cur = Second::default();
        self.len = 0;
        self.head = 0;
        self.start = None;
        self.last_totals = None;
        self.snaps_flowing = false;
        self.window_from = None;
        self.cur_from = None;
        self.timing = matches!(self.stretch, Stretch::Slow { .. });
    }

    /// One run step. `totals` is read once a second (and at a window's
    /// first frame). Returns the line due, if any.
    pub fn frame(&mut self, f: &SlowFrame, totals: impl FnOnce() -> Totals) -> Option<Due> {
        if !f.running {
            if self.last_frame.is_some() || self.len > 0 {
                self.restart_window();
            }
            return None;
        }
        let Some(prev) = self.last_frame.replace(f.now) else {
            let t = totals();
            self.start = Some(t);
            self.last_totals = Some(t);
            self.cur_from = Some(f.now);
            return None;
        };
        let dt = f.now.saturating_duration_since(prev);
        let c = &mut self.cur;
        c.wall += dt;
        c.frames += 1;
        c.worst = c.worst.max(dt);
        if dt > LONG_FRAME {
            c.long += 1;
        }
        if let Some((s, d)) = f.mod_time {
            c.mod_step += s;
            c.mod_draw += d;
            c.timed += 1;
        }
        if let Some(d) = f.drawn_at.filter(|&d| d >= prev && d <= f.now) {
            c.to_draw += d - prev;
            c.to_step += f.now - d;
            c.split += 1;
        }
        if c.wall < Duration::from_secs(1) {
            return None;
        }
        self.cur.end = totals();
        self.host_second();
        self.push();
        self.timing = self.cur_was_slow() || matches!(self.stretch, Stretch::Slow { .. });
        self.cur = Second::default();
        self.cur_from = Some(f.now);
        if self.len < WINDOW_SECS {
            return None;
        }
        self.judge(f.now)
    }

    /// The second just ended: the host's snapshots in it, if they came
    /// without a break.
    fn host_second(&mut self) {
        let was = self.last_totals.and_then(|t| t.host_snaps);
        let grew = match (was, self.cur.end.host_snaps) {
            (Some((n0, t0)), Some((n1, t1))) if n1 > n0 => {
                if self.snaps_flowing {
                    self.cur.host_snaps = n1 - n0;
                    self.cur.host_clock_ms = u64::from(t1.wrapping_sub(t0));
                }
                true
            }
            _ => false,
        };
        self.snaps_flowing = grew;
        self.last_totals = Some(self.cur.end);
    }

    fn cur_was_slow(&self) -> bool {
        self.len > 0 && self.ring[(self.head + self.len - 1) % WINDOW_SECS].fps() < SLOW_FPS
    }

    fn push(&mut self) {
        if self.len == WINDOW_SECS {
            // The oldest leaves: its end is the window's start.
            self.start = Some(self.ring[self.head].end);
            self.head = (self.head + 1) % WINDOW_SECS;
            self.len -= 1;
        }
        let i = (self.head + self.len) % WINDOW_SECS;
        self.ring[i] = self.cur;
        self.starts[i] = self.cur_from;
        self.len += 1;
        self.window_from = self.starts[self.head];
    }

    fn judge(&mut self, now: Instant) -> Option<Due> {
        let w = self.window();
        let slow = w.fps < SLOW_FPS;
        match (self.stretch, slow) {
            (Stretch::Calm, true) => {
                let since = self.window_from.unwrap_or(now);
                self.stretch = Stretch::Slow { since, last: now };
                self.timing = true;
                Some(Due {
                    kind: Kind::Slow,
                    window: w,
                })
            }
            (Stretch::Slow { since, last }, true) => {
                if now.saturating_duration_since(last) < REPEAT_EVERY {
                    return None;
                }
                self.stretch = Stretch::Slow { since, last: now };
                Some(Due {
                    kind: Kind::Still {
                        slow_for: now.saturating_duration_since(since),
                    },
                    window: w,
                })
            }
            (Stretch::Slow { since, .. }, false) => {
                let end = self.window_from.unwrap_or(now);
                self.stretch = Stretch::Calm;
                self.timing = false;
                Some(Due {
                    kind: Kind::Over {
                        lasted: end.saturating_duration_since(since),
                    },
                    window: w,
                })
            }
            (Stretch::Calm, false) => None,
        }
    }

    /// The window's numbers (the ring's seconds).
    fn window(&self) -> Window {
        let secs = (0..self.len).map(|k| &self.ring[(self.head + k) % WINDOW_SECS]);
        let mut all = Second::default();
        let mut lowest = f64::INFINITY;
        for s in secs {
            all.wall += s.wall;
            all.frames += s.frames;
            all.worst = all.worst.max(s.worst);
            all.long += s.long;
            all.mod_step += s.mod_step;
            all.mod_draw += s.mod_draw;
            all.timed += s.timed;
            all.to_draw += s.to_draw;
            all.to_step += s.to_step;
            all.split += s.split;
            all.host_snaps += s.host_snaps;
            all.host_clock_ms += s.host_clock_ms;
            lowest = lowest.min(s.fps());
        }
        let end = self.ring[(self.head + self.len.max(1) - 1) % WINDOW_SECS].end;
        let start = self.start.unwrap_or_default();
        let wall = all.wall.as_secs_f64().max(1e-3);
        let per = |d: Duration, n: u32| d / n.max(1);
        let timed_secs = if all.frames > 0 {
            wall * f64::from(all.timed) / f64::from(all.frames)
        } else {
            0.0
        };
        Window {
            wall: all.wall,
            fps: all.fps(),
            lowest_fps: if lowest.is_finite() { lowest } else { 0.0 },
            worst: all.worst,
            long: f64::from(all.long) / f64::from(all.frames.max(1)),
            mod_time: (all.timed > 0).then(|| {
                (
                    per(all.mod_step, all.timed),
                    per(all.mod_draw, all.timed),
                    timed_secs,
                )
            }),
            split: (all.split > 0)
                .then(|| (per(all.to_draw, all.split), per(all.to_step, all.split))),
            cpu: match (start.cpu, end.cpu) {
                (Some((p0, t0)), Some((p1, t1))) => Some((
                    p1.saturating_sub(p0).as_secs_f64() / wall,
                    t1.saturating_sub(t0).as_secs_f64() / wall,
                )),
                _ => None,
            },
            link: end.net.map(|n| {
                let b = start.net.unwrap_or_default();
                #[allow(clippy::cast_precision_loss)] // byte counts for a rate
                let rate = |now: u64, then: u64| grew(now, then) as f64 / wall / KB;
                (
                    n.rtt,
                    n.loss,
                    rate(n.bytes_sent, b.bytes_sent),
                    rate(n.bytes_recv, b.bytes_recv),
                    grew(n.resends, b.resends),
                    end.steam
                        .map(|s| grew(s.refused, start.steam.unwrap_or_default().refused)),
                )
            }),
            // At least a second of the host's clock with snapshots.
            #[allow(clippy::cast_precision_loss)] // snapshot counts, ms
            host_world: (all.host_clock_ms >= 1000).then(|| {
                (
                    (all.host_snaps * HOST_STEPS_PER_SNAPSHOT) as f64 / 60.0,
                    all.host_clock_ms as f64 / 1000.0,
                )
            }),
        }
    }
}

/// A counter's growth since `base`. A counter below `base` belongs to a new
/// connection and counts whole.
fn grew(now: u64, base: u64) -> u64 {
    if now >= base { now - base } else { now }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl Due {
    /// The line, with `scene` read now.
    #[must_use]
    #[allow(clippy::too_many_lines)] // one clause per part, in the line's order
    pub fn line(&self, scene: &Scene) -> String {
        let w = &self.window;
        let mut s = String::from("co-op slow frames: ");
        match self.kind {
            Kind::Slow => {
                let _ = write!(
                    s,
                    "under {SLOW_FPS:.0} fps over {:.1} s of play",
                    w.wall.as_secs_f64()
                );
            }
            Kind::Still { slow_for } => {
                let _ = write!(
                    s,
                    "still under {SLOW_FPS:.0} fps (slow for {:.0} s), the last {:.1} s of play",
                    slow_for.as_secs_f64(),
                    w.wall.as_secs_f64()
                );
            }
            Kind::Over { lasted } => {
                let _ = write!(
                    s,
                    "back to {SLOW_FPS:.0} fps or more for {:.1} s of play; the slow stretch lasted {:.0} s; frames {:.1} fps average, lowest second {:.1} fps, worst frame {:.0} ms",
                    w.wall.as_secs_f64(),
                    lasted.as_secs_f64(),
                    w.fps,
                    w.lowest_fps,
                    ms(w.worst)
                );
                return s;
            }
        }
        let _ = write!(
            s,
            "; frames {:.1} fps average, lowest second {:.1} fps, worst frame {:.0} ms, {:.1} % over {:.1} ms",
            w.fps,
            w.lowest_fps,
            ms(w.worst),
            w.long * 100.0,
            ms(LONG_FRAME)
        );
        match w.mod_time {
            Some((st, dr, secs)) => {
                let _ = write!(
                    s,
                    "; the mod's own time per frame: step side {:.2} ms, draw side {:.2} ms (timed over {secs:.1} s)",
                    ms(st),
                    ms(dr)
                );
            }
            None => s.push_str("; the mod's own time: -"),
        }
        match w.split {
            Some((a, b)) => {
                let _ = write!(
                    s,
                    "; frame split: step to GUI draw {:.1} ms, GUI draw to next step {:.1} ms (present and wait)",
                    ms(a),
                    ms(b)
                );
            }
            None => s.push_str("; frame split: -"),
        }
        match w.cpu {
            Some((p, t)) => {
                let _ = write!(
                    s,
                    "; CPU: process {:.0} % of one core, game thread {:.0} %",
                    p * 100.0,
                    t * 100.0
                );
            }
            None => s.push_str("; CPU: -"),
        }
        let _ = write!(s, "; scene: {}, wave ", scene.role);
        match scene.wave {
            Some(v) => {
                let _ = write!(s, "{v}");
            }
            None => s.push('-'),
        }
        let _ = write!(
            s,
            ", enemies {}, enemy shots {}, partner's mirrored {}, own shots and constructs {}",
            scene.enemies, scene.enemy_shots, scene.partner_mirrored, scene.own
        );
        match w.link {
            Some((rtt, loss, up, down, resends, refused)) => {
                let _ = write!(
                    s,
                    "; link: rtt {:.0} ms, loss {:.1} %, up {up:.1} KB/s, down {down:.1} KB/s, resends {resends}",
                    ms(rtt),
                    loss * 100.0
                );
                match refused {
                    Some(r) => {
                        let _ = write!(s, ", sends Steam refused {r}");
                    }
                    None => s.push_str(", no Steam"),
                }
            }
            None => s.push_str("; link: none"),
        }
        if scene.role == "joining" {
            match w.host_world {
                Some((ran, clock)) if clock > 0.0 => {
                    let _ = write!(
                        s,
                        "; the host's world: {ran:.1} s in {clock:.1} s of its clock ({:.0} % speed, from its enemy snapshots)",
                        ran / clock * 100.0
                    );
                }
                _ => s.push_str("; the host's world: - (no enemy snapshots in the window)"),
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Feed {
        log: SlowLog,
        t0: Instant,
        /// Seconds since `t0` of the last frame.
        t: f64,
        cpu: (f64, f64),
        snaps: (u64, u32),
        /// The host's steps so far, and the snapshot they were last at.
        host_steps: f64,
        snap_at: u64,
        /// The host sends snapshots (it has enemies).
        snaps_on: bool,
        lines: Vec<(f64, Due)>,
    }

    impl Feed {
        fn new() -> Self {
            Self {
                log: SlowLog::default(),
                t0: Instant::now(),
                t: 0.0,
                cpu: (0.0, 0.0),
                snaps: (0, 0),
                host_steps: 0.0,
                snap_at: 0,
                snaps_on: true,
                lines: Vec::new(),
            }
        }

        /// `secs` of frames at `fps` (running or not); the host's world at
        /// `host_speed` (snapshots every 2 of its steps).
        fn run(&mut self, secs: f64, fps: f64, running: bool, host_speed: f64) {
            let end = self.t + secs;
            let dt = 1.0 / fps;
            while self.t + dt <= end + 1e-9 {
                self.t += dt;
                // A busy game thread: 0.9 of a core, the process 1.3.
                self.cpu.0 += dt * 1.3;
                self.cpu.1 += dt * 0.9;
                self.host_steps += 60.0 * host_speed * dt;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let at = (self.host_steps / 2.0) as u64;
                if at > self.snap_at {
                    if self.snaps_on {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let stamp = (self.t * 1000.0) as u32;
                        self.snaps = (self.snaps.0 + (at - self.snap_at), stamp);
                    }
                    self.snap_at = at;
                }
                let now = self.t0 + Duration::from_secs_f64(self.t);
                let f = SlowFrame {
                    now,
                    running,
                    mod_time: self
                        .log
                        .timing()
                        .then_some((Duration::from_micros(600), Duration::from_micros(200))),
                    drawn_at: now.checked_sub(Duration::from_secs_f64(dt * 0.6)),
                };
                let (cpu, snaps) = (self.cpu, self.snaps);
                let totals = move || Totals {
                    cpu: Some((
                        Duration::from_secs_f64(cpu.0),
                        Duration::from_secs_f64(cpu.1),
                    )),
                    net: None,
                    steam: None,
                    host_snaps: Some(snaps),
                };
                if let Some(d) = self.log.frame(&f, totals) {
                    self.lines.push((self.t, d));
                }
            }
        }
    }

    #[test]
    fn a_game_at_full_speed_logs_nothing() {
        let mut r = Feed::new();
        r.run(600.0, 60.0, true, 1.0);
        r.run(60.0, 46.0, true, 1.0);
        assert!(r.lines.is_empty());
        assert!(!r.log.timing(), "no timing while fast");
    }

    #[test]
    fn a_window_under_45_fps_makes_a_line_then_one_every_30_s_and_a_closing_one() {
        let mut r = Feed::new();
        r.run(30.0, 60.0, true, 1.0);
        // At 20 fps a 10 s window falls under 45 with its fourth slow
        // second (20 * 4 + 60 * 6 = 440 frames).
        r.run(3.0, 20.0, true, 1.0);
        assert!(
            r.lines.is_empty(),
            "three slow seconds: 480 frames in the window"
        );
        assert!(r.log.timing(), "a slow second turns the timing on");
        r.run(82.0, 20.0, true, 1.0);
        r.run(30.0, 60.0, true, 1.0);
        let kinds: Vec<_> = r.lines.iter().map(|(t, d)| (t.round(), d.kind)).collect();
        assert_eq!(kinds.len(), 4, "{kinds:#?}");
        assert_eq!(kinds[0].1, Kind::Slow);
        assert!((kinds[0].0 - 34.0).abs() <= 1.0, "{kinds:?}");
        assert!(
            matches!(kinds[1].1, Kind::Still { .. }) && matches!(kinds[2].1, Kind::Still { .. })
        );
        assert!((kinds[1].0 - kinds[0].0 - 30.0).abs() <= 1.0, "{kinds:?}");
        assert!((kinds[2].0 - kinds[1].0 - 30.0).abs() <= 1.0, "{kinds:?}");
        // Recovered once a whole window averages 45 or more: its seventh
        // fast second (3 slow left: 60 + 420 = 480 frames).
        let Kind::Over { lasted } = kinds[3].1 else {
            panic!("{kinds:?}")
        };
        assert!((kinds[3].0 - 122.0).abs() <= 1.0, "{kinds:?}");
        // From its first window's start (24 s) to the closing window's (112 s).
        assert!((lasted.as_secs_f64() - 88.0).abs() <= 2.0, "{lasted:?}");
        assert!(!r.log.timing(), "the timing goes off with the stretch");
    }

    #[test]
    fn menus_pauses_and_holds_are_not_play() {
        let mut r = Feed::new();
        // Slow seconds split by a pause never make a window.
        for _ in 0..5 {
            r.run(8.0, 20.0, true, 1.0);
            r.run(20.0, 60.0, false, 1.0);
        }
        assert!(r.lines.is_empty());
        // A slow stretch survives a pause: no closing line for it there.
        r.run(12.0, 20.0, true, 1.0);
        assert_eq!(r.lines.len(), 1);
        r.run(60.0, 60.0, false, 1.0);
        assert!(r.log.timing(), "still slow: timed");
        r.run(5.0, 20.0, true, 1.0);
        assert_eq!(r.lines.len(), 1, "a window starts again after the pause");
        // 5 slow and 5 fast seconds: 400 frames, still slow (30 s since its
        // line: one more); then the window comes back.
        r.run(15.0, 60.0, true, 1.0);
        let kinds: Vec<_> = r.lines.iter().map(|(_, d)| d.kind).collect();
        assert!(
            matches!(
                kinds[..],
                [Kind::Slow, Kind::Still { .. }, Kind::Over { .. }]
            ),
            "{kinds:?}"
        );
    }

    #[test]
    fn the_line_says_each_number_with_its_unit_and_no_name() {
        let w = Window {
            wall: Duration::from_millis(10_020),
            fps: 38.24,
            lowest_fps: 31.0,
            worst: Duration::from_millis(92),
            long: 0.142,
            mod_time: Some((Duration::from_micros(620), Duration::from_micros(205), 9.0)),
            split: Some((Duration::from_micros(11_300), Duration::from_micros(14_900))),
            cpu: Some((0.85, 0.97)),
            link: Some((Duration::from_millis(172), 0.004, 76.5, 90.1, 12, Some(0))),
            host_world: Some((7.5, 10.0)),
        };
        let scene = Scene {
            role: "joining",
            wave: Some(142),
            enemies: 87,
            enemy_shots: 214,
            partner_mirrored: 160,
            own: 95,
        };
        let d = Due {
            kind: Kind::Slow,
            window: w,
        };
        assert_eq!(
            d.line(&scene),
            "co-op slow frames: under 45 fps over 10.0 s of play; frames 38.2 fps average, lowest second 31.0 fps, worst frame 92 ms, 14.2 % over 33.3 ms; the mod's own time per frame: step side 0.62 ms, draw side 0.20 ms (timed over 9.0 s); frame split: step to GUI draw 11.3 ms, GUI draw to next step 14.9 ms (present and wait); CPU: process 85 % of one core, game thread 97 %; scene: joining, wave 142, enemies 87, enemy shots 214, partner's mirrored 160, own shots and constructs 95; link: rtt 172 ms, loss 0.4 %, up 76.5 KB/s, down 90.1 KB/s, resends 12, sends Steam refused 0; the host's world: 7.5 s in 10.0 s of its clock (75 % speed, from its enemy snapshots)"
        );
        let host = Scene {
            role: "host",
            ..scene
        };
        let d = Due {
            kind: Kind::Still {
                slow_for: Duration::from_secs(41),
            },
            window: Window {
                link: None,
                mod_time: None,
                cpu: None,
                ..w
            },
        };
        let line = d.line(&host);
        assert!(
            line.starts_with(
                "co-op slow frames: still under 45 fps (slow for 41 s), the last 10.0 s of play; "
            ),
            "{line}"
        );
        assert!(
            line.contains("; the mod's own time: -; ")
                && line.contains("; CPU: -; ")
                && line.ends_with("; link: none"),
            "{line}"
        );
        let d = Due {
            kind: Kind::Over {
                lasted: Duration::from_secs(73),
            },
            window: Window {
                fps: 58.9,
                lowest_fps: 52.0,
                worst: Duration::from_millis(40),
                ..w
            },
        };
        assert_eq!(
            d.line(&host),
            "co-op slow frames: back to 45 fps or more for 10.0 s of play; the slow stretch lasted 73 s; frames 58.9 fps average, lowest second 52.0 fps, worst frame 40 ms"
        );
    }

    #[test]
    #[allow(clippy::many_single_char_names)] // a window's numbers
    fn the_window_measures_frames_cpu_and_the_hosts_world() {
        let mut r = Feed::new();
        // This game at 32 fps from the start, the host's world at 75 %.
        r.run(15.0, 32.0, true, 0.75);
        let (_, d) = r.lines[0];
        assert_eq!(d.kind, Kind::Slow);
        let w = d.window;
        assert!(
            (w.fps - 32.0).abs() < 0.5 && (w.lowest_fps - 32.0).abs() < 1.5,
            "{w:?}"
        );
        assert!((w.wall.as_secs_f64() - 10.0).abs() < 0.2);
        assert!(w.long.abs() < 1e-9, "31 ms frames are not over two budgets");
        let (p, t) = w.cpu.unwrap();
        assert!((p - 1.3).abs() < 0.05 && (t - 0.9).abs() < 0.05, "{p} {t}");
        let (a, b) = w.split.unwrap();
        assert!(
            (ms(a) - 12.5).abs() < 0.5 && (ms(b) - 18.75).abs() < 0.5,
            "{a:?} {b:?}"
        );
        let (st, dr, secs) = w.mod_time.unwrap();
        assert_eq!(
            (st, dr),
            (Duration::from_micros(600), Duration::from_micros(200))
        );
        assert!(
            (secs - 9.0).abs() < 0.2,
            "untimed in the first second: {secs}"
        );
        let (ran, clock) = w.host_world.unwrap();
        assert!((ran / clock - 0.75).abs() < 0.03, "{ran} {clock}");
        assert!(
            (clock - 9.0).abs() < 0.2,
            "its first second has no second before it: {clock}"
        );
    }

    /// A stretch without enemies sends no snapshots; its gap is not the
    /// host's world standing still.
    #[test]
    fn a_gap_in_the_hosts_snapshots_is_left_out() {
        let mut r = Feed::new();
        r.run(4.0, 32.0, true, 0.5);
        r.snaps_on = false;
        r.run(4.0, 32.0, true, 0.5);
        r.snaps_on = true;
        r.run(4.0, 32.0, true, 0.5);
        let (_, d) = r.lines[0];
        let (ran, clock) = d.window.host_world.unwrap();
        assert!((ran / clock - 0.5).abs() < 0.05, "{ran} {clock}");
        assert!(
            clock > 3.5 && clock < 6.5,
            "the seconds with snapshots only: {clock}"
        );
        // None at all: said so.
        let mut r = Feed::new();
        r.snaps_on = false;
        r.run(12.0, 32.0, true, 0.5);
        assert_eq!(r.lines[0].1.window.host_world, None);
    }
}
