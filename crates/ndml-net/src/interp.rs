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

//! Per-entity snapshot buffers, interpolation, capped extrapolation and smoothed
//! corrections.
//!
//! Positions stay as the sender's game had them; the receiving game wraps
//! them itself. Each sample carries a segment counter ([`Segments`]) that
//! goes up when the sender's game moved the instance by more than its motion
//! explains: interpolation never crosses a segment. Angles are GML degrees.
//! Times are remote (sender) clock times.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::ops::{Add, Mul, Sub};
use std::time::Duration;

/// A 2D vector in game pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub fn len(self) -> f32 {
        self.x.hypot(self.y)
    }
}

impl Add for Vec2 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Vec2 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f32> for Vec2 {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self::new(self.x * k, self.y * k)
    }
}

/// A wrap-around rectangle `[x, x + width) × [y, y + height)`, for wrapped
/// distances. Interpolation and smoothing do not use it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Arena {
    pub x: f32,
    pub y: f32,
    /// Width (> 0).
    pub width: f32,
    /// Height (> 0).
    pub height: f32,
}

fn wrap_axis(d: f32, size: f32) -> f32 {
    if size <= 0.0 {
        return d;
    }
    let r = (d + size / 2.0).rem_euclid(size) - size / 2.0;
    if r.is_finite() { r } else { d }
}

impl Arena {
    /// Shortest wrapped vector from `a` to `b`.
    #[must_use]
    pub fn delta(&self, a: Vec2, b: Vec2) -> Vec2 {
        Vec2::new(
            wrap_axis(b.x - a.x, self.width),
            wrap_axis(b.y - a.y, self.height),
        )
    }

    /// `p` folded back into the arena.
    #[must_use]
    pub fn wrap(&self, p: Vec2) -> Vec2 {
        let fold = |v: f32, lo: f32, size: f32| {
            if size <= 0.0 {
                v
            } else {
                lo + (v - lo).rem_euclid(size)
            }
        };
        Vec2::new(
            fold(p.x, self.x, self.width),
            fold(p.y, self.y, self.height),
        )
    }
}

/// Shortest signed angle from `a` to `b`, degrees in `(-180, 180]`.
#[must_use]
pub fn angle_delta(a: f32, b: f32) -> f32 {
    wrap_axis(b - a, 360.0)
}

/// The smallest move between two observations that can be a discontinuity
/// (px): below it a change of velocity is motion.
pub const JUMP_MIN_PX: f32 = 96.0;
/// A move is a discontinuity when it differs from the previous one by more
/// than [`JUMP_MIN_PX`] plus this many times the previous move's length.
pub const JUMP_STEP_FACTOR: f32 = 2.5;
/// A first move (nothing before it to compare with) is a discontinuity only
/// when longer than this (px): a wrap or a placement, not a fast shot.
pub const FIRST_JUMP_PX: f32 = 600.0;

/// The sender's discontinuity counter for one instance: observe its position
/// at a steady rate and send the returned segment with it. A move its
/// previous move does not explain (a wrap, a blink, a placement) starts a
/// new segment.
#[derive(Debug, Clone, Copy, Default)]
pub struct Segments {
    /// Last position and the motion that reached it (a jump keeps the old one).
    last: Option<(Vec2, Vec2)>,
    /// The last move observed, a jump included.
    raw: Vec2,
    /// A move was observed (the first one has no motion before it).
    moved: bool,
    seg: u8,
    jumps: u64,
}

impl Segments {
    /// Observe the position now; returns the segment it is in.
    pub fn observe(&mut self, pos: Vec2) -> u8 {
        if !(pos.x.is_finite() && pos.y.is_finite()) {
            return self.seg;
        }
        match self.last {
            Some((p, step)) => {
                let d = pos - p;
                let unexplained =
                    |by: Vec2| (d - by).len() > JUMP_MIN_PX + JUMP_STEP_FACTOR * by.len();
                // A first move is motion unless field-sized, and so is a move
                // that repeats the one before it: a fast shot is not a jump
                // every step. Only a repeat: a reversal would always pass the
                // 2.5 x allowance.
                let raw = std::mem::replace(&mut self.raw, d);
                let first = !std::mem::replace(&mut self.moved, true);
                if (first && d.len() > FIRST_JUMP_PX)
                    || (!first && unexplained(step) && (d - raw).len() > JUMP_MIN_PX)
                {
                    self.seg = self.seg.wrapping_add(1);
                    self.jumps += 1;
                    // The motion goes on as before the jump.
                    self.last = Some((pos, step));
                } else {
                    self.last = Some((pos, d));
                }
            }
            None => self.last = Some((pos, Vec2::default())),
        }
        self.seg
    }

    #[must_use]
    pub fn seg(&self) -> u8 {
        self.seg
    }

    /// Discontinuities seen so far.
    #[must_use]
    pub fn jumps(&self) -> u64 {
        self.jumps
    }

    /// Start a new segment now (a sender that placed the instance itself).
    pub fn mark(&mut self) {
        self.seg = self.seg.wrapping_add(1);
        self.jumps += 1;
        self.last = None;
    }
}

/// One received state of an entity at a remote time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Remote clock time of this state.
    pub time: Duration,
    pub pos: Vec2,
    /// Velocity in px/s if the sender knows it; otherwise derived.
    pub vel: Option<Vec2>,
    /// Facing, degrees.
    pub angle: f32,
    /// The sender's segment ([`Segments`]): not interpolated across.
    pub seg: u8,
}

/// An interpolated or extrapolated state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    /// Position, as the sender's game had it.
    pub pos: Vec2,
    /// Velocity estimate, px/s.
    pub vel: Vec2,
    /// Facing, degrees.
    pub angle: f32,
    /// True when the render time was past the newest sample.
    pub extrapolated: bool,
    /// The segment shown (a smoother jumps when it changes).
    pub seg: u8,
}

/// Samples kept per entity.
const TRACK_CAPACITY: usize = 32;

/// The snapshot buffer of one entity.
#[derive(Debug, Clone)]
pub struct Track {
    samples: VecDeque<Sample>,
    max_extrapolation: Duration,
    /// Angles interpolated linearly instead of with the cubic below.
    linear_angle: bool,
    /// Held at the newest sample until a newer one comes ([`Track::stop`]).
    stopped: bool,
}

impl Track {
    /// A buffer that extrapolates at most `max_extrapolation`, then holds.
    #[must_use]
    pub fn new(max_extrapolation: Duration) -> Self {
        Self {
            samples: VecDeque::with_capacity(TRACK_CAPACITY),
            max_extrapolation,
            linear_angle: false,
            stopped: false,
        }
    }

    /// Add a sample unless it is not newer than the newest; returns whether kept.
    pub fn push(&mut self, s: Sample) -> bool {
        if self.samples.back().is_some_and(|b| s.time <= b.time) {
            return false;
        }
        if self.samples.len() == TRACK_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
        self.stopped = false;
        true
    }

    /// The source stopped (its game froze): show the newest pose, not an
    /// extrapolated one, until a newer sample comes.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// The extrapolation cap in force: none while stopped.
    fn cap(&self) -> Duration {
        if self.stopped {
            Duration::ZERO
        } else {
            self.max_extrapolation
        }
    }

    /// Oldest sample still buffered.
    #[must_use]
    pub fn first(&self) -> Option<&Sample> {
        self.samples.front()
    }

    /// Newest sample.
    #[must_use]
    pub fn latest(&self) -> Option<&Sample> {
        self.samples.back()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Interpolate angles linearly (true) or with the cubic (false, the default).
    pub fn set_linear_angle(&mut self, on: bool) {
        self.linear_angle = on;
    }

    /// How far past the newest sample `t` is (capped like extrapolation), and
    /// the turn rate of the newest two samples in degrees/s (0 if fewer or
    /// under 5 ms apart; capped at `MAX_TURN_RATE`).
    #[must_use]
    pub fn turn_past_newest(&self, t: Duration) -> (f32, f32) {
        let n = self.samples.len();
        let Some(last) = self.samples.back() else {
            return (0.0, 0.0);
        };
        let ahead = t.saturating_sub(last.time).min(self.cap()).as_secs_f32();
        let rate = if n >= 2 {
            let prev = &self.samples[n - 2];
            let dt = last.time.saturating_sub(prev.time).as_secs_f32();
            if dt >= 0.005 {
                (angle_delta(prev.angle, last.angle) / dt).clamp(-MAX_TURN_RATE, MAX_TURN_RATE)
            } else {
                0.0
            }
        } else {
            0.0
        };
        (ahead, rate)
    }

    /// Whether the pose at `t` is held: at least the extrapolation cap past
    /// the newest sample (stopped: from the newest sample on).
    #[must_use]
    pub fn holds_at(&self, t: Duration) -> bool {
        self.samples.back().is_some_and(|last| {
            if self.stopped {
                t >= last.time
            } else {
                t.saturating_sub(last.time) >= self.max_extrapolation
            }
        })
    }

    /// Drop samples that no render time at or after `t` can need: keeps the
    /// one bracketing `t` and the one before it (the cubics' tangent there).
    pub fn prune_before(&mut self, t: Duration) {
        while self.samples.len() > 3 && self.samples[2].time <= t {
            self.samples.pop_front();
        }
    }

    /// Velocity at sample `idx`: the sent one, else from a neighbouring pair
    /// in the same segment, else zero.
    fn velocity_at(&self, idx: usize) -> Vec2 {
        if let Some(v) = self.samples[idx].vel {
            return v;
        }
        let pairs = [
            (idx.checked_sub(1), Some(idx)),
            (Some(idx), idx.checked_add(1)),
            (idx.checked_sub(2), idx.checked_sub(1)),
        ];
        for (a, b) in pairs {
            let (Some(a), Some(b)) = (
                a.and_then(|i| self.samples.get(i)),
                b.and_then(|i| self.samples.get(i)),
            ) else {
                continue;
            };
            let dt = (b.time.as_secs_f64() - a.time.as_secs_f64()) as f32;
            if dt > 0.0 && a.seg == b.seg {
                return (b.pos - a.pos) * (1.0 / dt);
            }
        }
        Vec2::default()
    }

    /// State at remote time `t`: interpolated within a segment (across a
    /// segment change each side moves along its velocity, switching half way),
    /// extrapolated past the newest (capped), or the oldest before everything.
    #[must_use]
    pub fn sample(&self, t: Duration) -> Option<Pose> {
        let first = self.samples.front()?;
        let last = self.samples.back()?;
        if t <= first.time {
            return Some(Pose {
                pos: first.pos,
                vel: self.velocity_at(0),
                angle: first.angle,
                extrapolated: false,
                seg: first.seg,
            });
        }
        if t >= last.time {
            let vel = self.velocity_at(self.samples.len() - 1);
            let ahead = t.saturating_sub(last.time).min(self.cap()).as_secs_f32();
            return Some(Pose {
                pos: last.pos + vel * ahead,
                vel,
                angle: last.angle,
                extrapolated: t > last.time && !self.stopped,
                seg: last.seg,
            });
        }
        let idx = self.samples.partition_point(|s| s.time <= t);
        let (from, to) = (&self.samples[idx - 1], &self.samples[idx]);
        let span_s = to.time.saturating_sub(from.time).as_secs_f64();
        let frac = (t.saturating_sub(from.time).as_secs_f64() / span_s) as f32;
        let step = to.pos - from.pos;
        let angle = self.angle_between(idx, frac);
        if from.seg != to.seg {
            let (pos, vel, seg) = if frac < 0.5 {
                let v = self.velocity_at(idx - 1);
                (
                    from.pos + v * t.saturating_sub(from.time).as_secs_f32(),
                    v,
                    from.seg,
                )
            } else {
                let v = self.velocity_at(idx);
                (
                    to.pos - v * to.time.saturating_sub(t).as_secs_f32(),
                    v,
                    to.seg,
                )
            };
            return Some(Pose {
                pos,
                vel,
                angle,
                extrapolated: false,
                seg,
            });
        }
        // Cubic Hermite with Catmull-Rom tangents (the segment's own slope
        // where a neighbour is missing or in another segment).
        let seg_rate = step * (1.0 / span_s as f32);
        let tangent = |a: Option<&Sample>, b: Option<&Sample>| -> Vec2 {
            match (a, b) {
                (Some(a), Some(b)) if a.seg == b.seg => {
                    let dt = b.time.saturating_sub(a.time).as_secs_f32();
                    if dt > 0.0 {
                        (b.pos - a.pos) * (1.0 / dt)
                    } else {
                        seg_rate
                    }
                }
                _ => seg_rate,
            }
        };
        let before = idx.checked_sub(2).and_then(|i| self.samples.get(i));
        let m1 = tangent(before, Some(to));
        let m2 = tangent(Some(from), self.samples.get(idx + 1));
        let (u, d) = (frac, span_s as f32);
        let (u2, u3) = (u * u, u * u * u);
        let pos = from.pos * (2.0 * u3 - 3.0 * u2 + 1.0)
            + m1 * ((u3 - 2.0 * u2 + u) * d)
            + to.pos * (3.0 * u2 - 2.0 * u3)
            + m2 * ((u3 - u2) * d);
        let vel = match (from.vel, to.vel) {
            (Some(va), Some(vb)) => va + (vb - va) * frac,
            _ => {
                step * ((6.0 * u - 6.0 * u2) / d)
                    + m1 * (3.0 * u2 - 4.0 * u + 1.0)
                    + m2 * (3.0 * u2 - 2.0 * u)
            }
        };
        Some(Pose {
            pos,
            vel,
            angle,
            extrapolated: false,
            seg: from.seg,
        })
    }

    /// The angle a fraction `u` from sample `idx - 1` to `idx`: a cubic
    /// Hermite on unwrapped angles, as for position (a linear turn rate jumps
    /// at each snapshot, which a beam's length magnifies).
    fn angle_between(&self, idx: usize, u: f32) -> f32 {
        let (from, to) = (&self.samples[idx - 1], &self.samples[idx]);
        let step = angle_delta(from.angle, to.angle);
        if self.linear_angle {
            return from.angle + step * u;
        }
        let d = to.time.saturating_sub(from.time).as_secs_f32();
        if d <= 0.0 {
            return from.angle + step * u;
        }
        let seg = step / d;
        let m1 = idx
            .checked_sub(2)
            .and_then(|i| self.samples.get(i))
            .and_then(|b| {
                let dt = to.time.saturating_sub(b.time).as_secs_f32();
                (dt > 0.0).then(|| (angle_delta(b.angle, from.angle) + step) / dt)
            })
            .unwrap_or(seg);
        let m2 = self
            .samples
            .get(idx + 1)
            .and_then(|n| {
                let dt = n.time.saturating_sub(from.time).as_secs_f32();
                (dt > 0.0).then(|| (step + angle_delta(to.angle, n.angle)) / dt)
            })
            .unwrap_or(seg);
        let (u2, u3) = (u * u, u * u * u);
        from.angle * (2.0 * u3 - 3.0 * u2 + 1.0)
            + m1 * ((u3 - 2.0 * u2 + u) * d)
            + (from.angle + step) * (3.0 * u2 - 2.0 * u3)
            + m2 * ((u3 - u2) * d)
    }
}

/// The fastest turn the smoother continues (degrees per second).
const MAX_TURN_RATE: f32 = 720.0;

/// Hides corrections as an offset that decays with the half-life. A new
/// segment or a jump past `snap_distance` is applied at once.
#[derive(Debug, Clone)]
pub struct Smoother {
    half_life: Duration,
    snap_distance: f32,
    last: Option<(Vec2, Vec2, f32, u8)>, // target pos, target vel, target angle, segment
    offset: Vec2,
    angle_offset: f32,
    /// The target's steady turn rate (degrees/s): expected, so not eased.
    turn_rate: f32,
}

impl Smoother {
    /// See the type docs.
    #[must_use]
    pub fn new(half_life: Duration, snap_distance: f32) -> Self {
        Self {
            half_life,
            snap_distance,
            last: None,
            offset: Vec2::default(),
            angle_offset: 0.0,
            turn_rate: 0.0,
        }
    }

    /// The pose to display for `target` (`dt` since the previous call).
    pub fn update(&mut self, target: Pose, dt: Duration) -> Pose {
        let decay = if self.half_life.is_zero() {
            0.0
        } else {
            0.5f32.powf(dt.as_secs_f32() / self.half_life.as_secs_f32())
        };
        if let Some((pos, vel, angle, seg)) = self.last {
            let secs = dt.as_secs_f32();
            let expected = pos + vel * secs;
            let jump = target.pos - expected;
            let turned = angle_delta(angle, target.angle);
            if seg != target.seg || jump.len() > self.snap_distance {
                self.offset = Vec2::default();
                // A wrap keeps the nose's correction (it did not turn).
                if seg == target.seg {
                    self.angle_offset = 0.0;
                    self.turn_rate = 0.0;
                }
            } else {
                self.offset = self.offset * decay - jump;
                // Only the turn beyond the steady rate is a correction.
                let surprise = turned - self.turn_rate * secs;
                self.angle_offset = (self.angle_offset * decay - surprise).clamp(-180.0, 180.0);
                // Only frames of 5 ms or more count; capped and averaged.
                if secs >= 0.005 {
                    let raw = (turned / secs).clamp(-MAX_TURN_RATE, MAX_TURN_RATE);
                    self.turn_rate = f32::midpoint(self.turn_rate, raw);
                }
            }
        }
        self.last = Some((target.pos, target.vel, target.angle, target.seg));
        Pose {
            pos: target.pos + self.offset,
            angle: target.angle + self.angle_offset,
            ..target
        }
    }

    /// The target moved by `d` and turned by `d_angle` (newer samples): hidden
    /// as a decaying offset unless past the snap distance. For [`show`](Self::show).
    pub fn absorb(&mut self, d: Vec2, d_angle: f32) {
        if d.len() > self.snap_distance {
            self.offset = Vec2::default();
            self.angle_offset = 0.0;
        } else {
            self.offset = self.offset - d;
            self.angle_offset = (self.angle_offset - d_angle).clamp(-180.0, 180.0);
        }
    }

    /// `target` plus the remaining absorbed correction, decayed over `dt`; a
    /// new segment drops the position's correction.
    pub fn show(&mut self, target: Pose, dt: Duration) -> Pose {
        let decay = if self.half_life.is_zero() {
            0.0
        } else {
            0.5f32.powf(dt.as_secs_f32() / self.half_life.as_secs_f32())
        };
        if self.last.is_some_and(|(.., seg)| seg != target.seg) {
            self.offset = Vec2::default();
        }
        self.last = Some((target.pos, target.vel, target.angle, target.seg));
        self.offset = self.offset * decay;
        self.angle_offset *= decay;
        Pose {
            pos: target.pos + self.offset,
            angle: target.angle + self.angle_offset,
            ..target
        }
    }

    /// Size of the correction still being hidden.
    #[must_use]
    pub fn offset(&self) -> Vec2 {
        self.offset
    }
}

#[cfg(test)]
// Short names for poses; small integer times, so float casts are exact.
#[allow(clippy::many_single_char_names, clippy::cast_precision_loss)]
mod tests {
    use super::*;
    use crate::clock::ClockSync;
    use crate::rng::Rng;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn s(t: u64, x: f32, y: f32) -> Sample {
        Sample {
            time: ms(t),
            pos: Vec2::new(x, y),
            vel: None,
            angle: 0.0,
            seg: 0,
        }
    }

    const ARENA: Arena = Arena {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    };

    #[test]
    fn an_enemy_wrapping_through_a_margin_is_never_shown_inside_the_room() {
        // Moving up, wrapping from y -100 to 1400: the segment changes there.
        let mut t = Track::new(ms(200));
        for (i, y, seg) in [
            (0u64, -97.0, 0),
            (1, -100.0, 0),
            (2, 1400.0 - 3.0, 1),
            (3, 1394.0, 1),
        ] {
            assert!(t.push(Sample {
                seg,
                ..s(i * 33, 500.0, y)
            }));
        }
        for ms_ in 0..=99 {
            let p = t.sample(ms(ms_)).unwrap();
            assert!(p.pos.y < -90.0 || p.pos.y > 1390.0, "{ms_} ms: {:?}", p.pos);
            assert_eq!(p.seg, u8::from(p.pos.y >= 0.0));
        }
        let p = t.sample(ms(83)).unwrap();
        assert!((p.pos.y - 1395.5).abs() < 0.6, "{:?}", p.pos);
        // The velocity is never taken across the jump.
        assert!(p.vel.y.abs() < 200.0, "{:?}", p.vel);
    }

    #[test]
    fn a_pose_is_held_from_the_extrapolation_cap_on() {
        let mut t = Track::new(ms(250));
        assert!(!t.holds_at(ms(0)), "no samples: nothing is held");
        t.push(s(0, 0.0, 0.0));
        t.push(s(100, 10.0, 0.0));
        assert!(!t.holds_at(ms(50)), "interpolated");
        assert!(!t.holds_at(ms(349)), "still extrapolated");
        assert!(t.holds_at(ms(350)));
        // The pose no longer moves from there on.
        assert_eq!(t.sample(ms(350)), t.sample(ms(900)));
        assert!(t.holds_at(ms(900)));
        t.push(s(1000, 100.0, 0.0));
        assert!(!t.holds_at(ms(900)), "a newer sample moves it again");
    }

    #[test]
    fn a_stopped_track_holds_its_newest_pose_until_a_newer_sample() {
        // Frozen after its sample at 100: held at x 10, not extrapolated.
        let mut t = Track::new(ms(250));
        t.push(s(0, 0.0, 0.0));
        t.push(s(100, 10.0, 0.0));
        t.stop();
        assert!(!t.holds_at(ms(50)), "interpolated as before");
        assert!((t.sample(ms(50)).unwrap().pos.x - 5.0).abs() < 0.5);
        for at in [100, 150, 900, 5000] {
            assert!(t.holds_at(ms(at)), "{at}");
            let p = t.sample(ms(at)).unwrap();
            assert!(
                (p.pos.x - 10.0).abs() < 1e-4 && !p.extrapolated,
                "{at}: {p:?}"
            );
        }
        assert_eq!(t.turn_past_newest(ms(900)).0, 0.0);
        // The freeze's end: the next sample is where it stood.
        t.push(s(5100, 10.0, 0.0));
        t.push(s(5133, 13.3, 0.0));
        assert!(!t.holds_at(ms(5200)), "moving again");
        assert!(t.sample(ms(5200)).unwrap().extrapolated);
        assert!((t.sample(ms(5100)).unwrap().pos.x - 10.0).abs() < 1e-4);
    }

    #[test]
    fn segments_mark_what_motion_does_not_explain() {
        let mut g = Segments::default();
        // Steady motion, a turn, a hard bounce of a fast shot: one segment.
        let mut x = 0.0;
        for v in [10.0, 12.0, 12.0, -30.0, 40.0, -40.0, -40.0] {
            x += v;
            assert_eq!(g.observe(Vec2::new(x, 0.0)), 0, "at {x}");
        }
        // A wrap across 2400 px.
        x += 2400.0 - 40.0;
        assert_eq!(g.observe(Vec2::new(x, 0.0)), 1);
        // Motion goes on in the new segment.
        x -= 40.0;
        assert_eq!(g.observe(Vec2::new(x, 0.0)), 1);
        // A blink of 300 px from rest.
        let mut b = Segments::default();
        b.observe(Vec2::new(0.0, 0.0));
        b.observe(Vec2::new(0.0, 0.0));
        assert_eq!(b.observe(Vec2::new(300.0, 0.0)), 1);
        assert_eq!(b.jumps(), 1);
        // A dash starting at 80 px a step is motion.
        let mut d = Segments::default();
        d.observe(Vec2::new(0.0, 0.0));
        assert_eq!(d.observe(Vec2::new(0.0, 80.0)), 0);
        // A shot faster than JUMP_MIN_PX from its first frame: motion.
        let mut r = Segments::default();
        let segs: Vec<u8> = (0..8)
            .map(|i| r.observe(Vec2::new(142.0 * i as f32, 0.0)))
            .collect();
        assert_eq!(segs, [0; 8]);
        // Flipped across the field and back every frame: a segment each flip.
        let mut f = Segments::default();
        f.observe(Vec2::new(2839.4, 209.9));
        f.observe(Vec2::new(2839.4, 209.9));
        for i in 1..=8u8 {
            let x = if i % 2 == 1 { -533.7 } else { 2836.6 };
            assert_eq!(f.observe(Vec2::new(x, 209.9 - 3.5 * f32::from(i))), i);
        }
        // NaN is not observed; the counter wraps at 256.
        assert_eq!(d.observe(Vec2::new(f32::NAN, 0.0)), 0);
        for _ in 0..256 {
            d.mark();
        }
        assert_eq!(d.seg(), 0);
    }

    #[test]
    fn smoother_jumps_at_a_new_segment() {
        let mut sm = Smoother::new(ms(100), 10_000.0);
        let pose = |x: f32, seg: u8| Pose {
            pos: Vec2::new(x, 0.0),
            vel: Vec2::new(0.0, 0.0),
            angle: 0.0,
            extrapolated: false,
            seg,
        };
        let dt = ms(16);
        sm.update(pose(0.0, 3), dt);
        // A small correction in the segment is eased...
        assert!(sm.update(pose(10.0, 3), dt).pos.x.abs() < 1e-3);
        // ...a jump to a new segment is shown at once, whatever its size.
        assert_eq!(sm.update(pose(120.0, 4), dt).pos.x, 120.0);
        assert_eq!(sm.offset(), Vec2::default());
    }

    #[test]
    fn unfolded_samples_follow_a_curve_without_corners() {
        // A straight line at uneven sample times stays exact.
        let mut t = Track::new(ms(200));
        for i in [0u64, 30, 67, 100, 131] {
            assert!(t.push(s(i, i as f32 * 2.0, 10.0)));
        }
        for at in [15u64, 50, 80, 115] {
            let p = t.sample(ms(at)).unwrap();
            assert!(
                (p.pos.x - at as f32 * 2.0).abs() < 1e-3,
                "{at}: {:?}",
                p.pos
            );
            assert!((p.vel.x - 2000.0).abs() < 0.5, "{at}: {:?}", p.vel);
        }
        // On a circle: no spike in the second difference at a sample.
        let mut c = Track::new(ms(200));
        let on = |k: f32| Vec2::new(100.0 * (k * 0.2).cos(), 100.0 * (k * 0.2).sin());
        for k in 0..8u64 {
            let p = on(k as f32);
            assert!(c.push(s(k * 33, p.x, p.y)));
        }
        let at = |m: u64| c.sample(ms(m)).unwrap().pos;
        let jolt = |m: u64| {
            let d = at(m - 1) - at(m) * 2.0 + at(m + 1);
            d.x.hypot(d.y)
        };
        assert!(
            jolt(132) < 2.0 * jolt(148) + 1e-3,
            "{} {}",
            jolt(132),
            jolt(148)
        );
    }

    #[test]
    fn interpolates_between_samples() {
        let mut t = Track::new(ms(200));
        assert!(t.sample(ms(0)).is_none());
        assert!(t.push(s(100, 0.0, 0.0)));
        assert!(t.push(s(200, 100.0, 50.0)));
        assert!(!t.push(s(150, 9.0, 9.0)), "older than newest");
        let p = t.sample(ms(150)).unwrap();
        assert_eq!(p.pos, Vec2::new(50.0, 25.0));
        assert!(!p.extrapolated);
        assert!((p.vel.x - 1000.0).abs() < 0.01);
        let before = t.sample(ms(10)).unwrap();
        assert_eq!(before.pos, Vec2::new(0.0, 0.0));
    }

    #[test]
    fn extrapolates_with_a_cap() {
        let mut t = Track::new(ms(100));
        t.push(s(0, 0.0, 0.0));
        t.push(s(100, 10.0, 0.0)); // 100 px/s
        let p = t.sample(ms(150)).unwrap();
        assert!(p.extrapolated);
        assert!((p.pos.x - 15.0).abs() < 1e-3);
        let far = t.sample(ms(5000)).unwrap();
        assert!((far.pos.x - 20.0).abs() < 1e-3, "capped at 100 ms");
        // A sender-provided velocity wins over the derived one.
        let mut t = Track::new(ms(100));
        t.push(Sample {
            vel: Some(Vec2::new(0.0, -200.0)),
            ..s(0, 5.0, 5.0)
        });
        let p = t.sample(ms(50)).unwrap();
        assert_eq!(p.pos, Vec2::new(5.0, -5.0));
    }

    #[test]
    fn angles_take_the_short_way() {
        assert!((angle_delta(350.0, 10.0) - 20.0).abs() < 1e-4);
        assert!((angle_delta(10.0, 350.0) + 20.0).abs() < 1e-4);
        let mut t = Track::new(ms(0));
        t.push(Sample {
            angle: 350.0,
            ..s(0, 0.0, 0.0)
        });
        t.push(Sample {
            angle: 10.0,
            ..s(100, 0.0, 0.0)
        });
        let p = t.sample(ms(50)).unwrap();
        assert!((p.angle - 360.0).abs() < 1e-3, "{}", p.angle);
    }

    #[test]
    fn arena_wrap_and_delta() {
        assert_eq!(
            ARENA.wrap(Vec2::new(-10.0, 1090.0)),
            Vec2::new(1910.0, 10.0)
        );
        assert_eq!(
            ARENA.delta(Vec2::new(5.0, 5.0), Vec2::new(1915.0, 5.0)),
            Vec2::new(-10.0, 0.0)
        );
        let degenerate = Arena {
            width: 0.0,
            ..ARENA
        };
        assert_eq!(degenerate.wrap(Vec2::new(-3.0, 1.0)).x, -3.0);
    }

    /// A sweep with a changing turn rate: the cubic's angular acceleration
    /// stays near the true one; the linear one jumps at every snapshot.
    #[test]
    fn cubic_angles_turn_smoothly() {
        let sweep = |t: f32| 350.0 + 40.0 * (t * 2.0).sin();
        let acc95 = |linear: bool| {
            let mut track = Track::new(ms(100));
            track.set_linear_angle(linear);
            for i in 0..90u64 {
                let t = i as f32 / 30.0;
                track.push(Sample {
                    time: Duration::from_secs_f32(t),
                    pos: Vec2::new(0.0, 0.0),
                    vel: None,
                    angle: sweep(t).rem_euclid(360.0),
                    seg: 0,
                });
            }
            let a: Vec<f32> = (10..170)
                .map(|f| {
                    let t = Duration::from_secs_f32(f as f32 / 60.0);
                    track.sample(t).unwrap().angle
                })
                .collect();
            let d: Vec<f32> = a.windows(2).map(|w| angle_delta(w[0], w[1])).collect();
            let mut acc: Vec<f32> = d.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
            acc.sort_by(f32::total_cmp);
            acc[acc.len() * 95 / 100]
        };
        // The true sweep: at most 40 * 4 / 3600 deg/frame^2 = 0.044.
        let (lin, cub) = (acc95(true), acc95(false));
        assert!(cub < 0.05, "cubic {cub}");
        assert!(lin > 1.7 * cub, "linear {lin} cubic {cub}");
    }

    #[test]
    fn absorbed_corrections_fade_and_motion_is_never_one() {
        let mut sm = Smoother::new(ms(80), 300.0);
        let pose = |x: f32| Pose {
            pos: Vec2::new(x, 0.0),
            vel: Vec2::new(650.0, 0.0),
            angle: 0.0,
            extrapolated: false,
            seg: 0,
        };
        // Standing still while its velocity says 650 px/s: shown where it is.
        for _ in 0..30 {
            assert_eq!(sm.show(pose(10.0), ms(16)).pos.x, 10.0);
        }
        // A newer state moved the target back by 20 px: eased over.
        sm.absorb(Vec2::new(-20.0, 0.0), 0.0);
        let first = sm.show(pose(-10.0), ms(16)).pos.x;
        assert!(first > 0.0 && first < 10.0, "{first}");
        let mut x = first;
        for _ in 0..40 {
            x = sm.show(pose(-10.0), ms(16)).pos.x;
        }
        assert!((x + 10.0).abs() < 0.1, "{x}");
        // Past the snap distance: at once.
        sm.absorb(Vec2::new(400.0, 0.0), 0.0);
        assert_eq!(sm.show(pose(390.0), ms(16)).pos.x, 390.0);
        // A pending correction is dropped at a new segment.
        sm.absorb(Vec2::new(-20.0, 0.0), 0.0);
        let wrapped = Pose {
            seg: 1,
            ..pose(5.0)
        };
        assert_eq!(sm.show(wrapped, ms(16)).pos.x, 5.0);
    }

    #[test]
    fn prune_keeps_the_bracket() {
        let mut t = Track::new(ms(100));
        for i in 0..10 {
            t.push(s(i * 10, 0.0, 0.0));
        }
        t.prune_before(ms(55));
        assert_eq!(t.samples.front().unwrap().time, ms(40));
        assert!(t.sample(ms(55)).is_some());
    }

    #[test]
    fn smoother_hides_small_jumps_and_snaps_big_ones() {
        let mut sm = Smoother::new(ms(100), 200.0);
        let pose = |x: f32| Pose {
            pos: Vec2::new(x, 0.0),
            vel: Vec2::new(0.0, 0.0),
            angle: 0.0,
            extrapolated: false,
            seg: 0,
        };
        let dt = ms(16);
        assert_eq!(sm.update(pose(0.0), dt).pos.x, 0.0);
        // Target corrects by 10 px: display stays put, then eases over.
        let shown = sm.update(pose(10.0), dt);
        assert!(shown.pos.x.abs() < 1e-3);
        let mut x = 0.0;
        for _ in 0..60 {
            x = sm.update(pose(10.0), dt).pos.x;
        }
        assert!((x - 10.0).abs() < 0.1, "converged to {x}");
        // A teleport snaps.
        let shown = sm.update(pose(900.0), dt);
        assert_eq!(shown.pos.x, 900.0);
        assert_eq!(sm.offset(), Vec2::default());
    }

    #[test]
    fn turn_past_newest_continues_a_turn_capped() {
        let mut t = Track::new(ms(100));
        t.push(Sample {
            time: ms(0),
            pos: Vec2::new(0.0, 0.0),
            vel: None,
            angle: 350.0,
            seg: 0,
        });
        t.push(Sample {
            time: ms(20),
            pos: Vec2::new(0.0, 0.0),
            vel: None,
            seg: 0,
            angle: 2.0, // 12 degrees across 0 in 20 ms: 600 per second
        });
        let (ahead, rate) = t.turn_past_newest(ms(50));
        assert!(
            (ahead - 0.03).abs() < 1e-4 && (rate - 600.0).abs() < 0.5,
            "{ahead} {rate}"
        );
        // Capped: never further than the extrapolation limit.
        let (ahead, _) = t.turn_past_newest(ms(500));
        assert!((ahead - 0.1).abs() < 1e-4, "{ahead}");
    }

    #[test]
    fn smoother_turn_rate_survives_uneven_frames_and_clock_steps() {
        let mut sm = Smoother::new(ms(100), 200.0);
        let mut t = 0.0f32;
        let mut worst: f32 = 0.0;
        for i in 0..240u16 {
            // Frames of 1 and 31 ms; every 50th the render clock steps 20 ms.
            let dt_ms = if i % 2 == 0 { 1 } else { 31 };
            t += dt_ms as f32 / 1000.0 + if i % 50 == 0 { 0.02 } else { 0.0 };
            let angle = (300.0 * t).rem_euclid(360.0);
            let pose = Pose {
                pos: Vec2::new(0.0, 0.0),
                vel: Vec2::new(0.0, 0.0),
                angle,
                extrapolated: false,
                seg: 0,
            };
            let shown = sm.update(pose, ms(dt_ms));
            if i > 40 {
                worst = worst.max(angle_delta(angle, shown.angle).abs());
            }
        }
        assert!(worst < 8.0, "shown nose off by {worst} degrees");
    }

    #[test]
    fn smoother_does_not_trail_a_steady_turn() {
        let mut sm = Smoother::new(ms(100), 200.0);
        let dt = ms(16);
        let mut worst: f32 = 0.0;
        for i in 0..120u16 {
            // 300 degrees a second.
            let angle = (f32::from(i) * 4.8).rem_euclid(360.0);
            let pose = Pose {
                pos: Vec2::new(0.0, 0.0),
                vel: Vec2::new(0.0, 0.0),
                angle,
                extrapolated: false,
                seg: 0,
            };
            let shown = sm.update(pose, dt);
            // Past the start of the turn (eased like any surprise).
            if i > 40 {
                worst = worst.max(angle_delta(angle, shown.angle).abs());
            }
        }
        assert!(worst < 0.5, "shown nose trails by {worst} degrees");
    }

    /// End to end: 30 Hz snapshots of a circling enemy, 60 ± 15 ms latency
    /// and 5 % loss, rendered at 60 Hz: the error stays small.
    #[test]
    fn interpolation_error_is_bounded_under_jitter_and_loss() {
        let path = |t: f64| {
            let a = t * 1.5; // rad/s, radius 200 -> 300 px/s
            Vec2::new(
                960.0 + 200.0 * a.cos() as f32,
                540.0 + 200.0 * a.sin() as f32,
            )
        };
        let mut rng = Rng::new(21);
        let mut clock = ClockSync::new();
        let mut track = Track::new(ms(250));
        // In-flight packets: (arrival local ms, send remote ms).
        let mut flight: Vec<(f64, u64)> = Vec::new();
        let remote_offset = 3000.0; // remote clock ahead by 3 s
        let mut worst: f32 = 0.0;
        let mut extrapolated = 0;
        let mut frames = 0;
        for local_ms in 0..20_000u64 {
            let lt = local_ms as f64;
            // Host sends every 33 ms of its clock.
            if local_ms % 33 == 0 && !rng.chance(0.05) {
                let j = 15.0 * (rng.next_f64() + rng.next_f64() - 1.0);
                flight.push((lt + 60.0 + j, (lt + remote_offset) as u64));
            }
            flight.retain(|&(arrive, sent)| {
                if arrive <= lt {
                    let tr = Duration::from_millis(sent);
                    clock.observe(tr, ms(local_ms));
                    track.push(Sample {
                        time: tr,
                        pos: path(sent as f64 / 1000.0),
                        vel: None,
                        angle: 0.0,
                        seg: 0,
                    });
                    false
                } else {
                    true
                }
            });
            if local_ms % 16 == 0 && local_ms > 2000 {
                let delay = clock.interp_delay(ms(33));
                let rt = clock.render_time(ms(local_ms), delay);
                let pose = track.sample(rt).unwrap();
                let err = (pose.pos - path(rt.as_secs_f64())).len();
                worst = worst.max(err);
                extrapolated += usize::from(pose.extrapolated);
                frames += 1;
                track.prune_before(rt);
            }
        }
        // Chords over two lost snapshots sag about 1 px; the clock adds about 1 px.
        assert!(worst < 4.0, "worst error {worst} px");
        assert!(
            extrapolated * 100 < frames,
            "extrapolated {extrapolated}/{frames} frames"
        );
    }
}
