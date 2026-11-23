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

//! Ships: packing the local ship into `PlayerState`/`PlayerLook`, and one
//! interpolation track per remote ship.
//!
//! The local ship is read in `oPlayer`'s step; hull and the cloak come from
//! `oPowers`. Speeds are px per step; the wire carries px per second.
//! Remote ships are drawn at the partner's clock minus the interpolation
//! delay, extrapolated along the sent velocity for at most the configured
//! cap. The sender marks moves its ship's motion does not explain (a wrap, a
//! respawn) with a new segment (`PlayerState.seg`); the viewer jumps there.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ndml_net::interp::{Pose, Sample, Smoother, Track, Vec2};
use ndml_net::msg::{PeerId, PlayerLook, PlayerLookFrame, PlayerState};
use ndml_net::time;
use ndml_sdk::Value;

/// Ref payload tag of a sprite.
pub const SPRITE_REF_TAG: u64 = 0x0100_0001_0000_0000;
/// Mask of a ref's type tag.
const REF_TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;
/// A remote ship not heard from for this long is hidden.
pub const STALE_AFTER: Duration = Duration::from_secs(1);
/// Corrections fade with this half-life.
pub const SMOOTH_HALF_LIFE: Duration = Duration::from_millis(80);
/// Jumps larger than this (px) are applied at once (respawn, teleport).
pub const SNAP_DISTANCE: f32 = 300.0;
/// A step between two states that neither their velocities nor the step
/// before it explain to within this (px) is an unmarked jump (a blink).
const JUMP_PX: f32 = 48.0;
/// Past the newest state the sent velocity is checked against the positions
/// of this long before it.
const MOTION_WINDOW: Duration = Duration::from_millis(50);
/// They disagree by more than this (px/s) plus a quarter of the sent speed:
/// the positions' own velocity is used.
const MOTION_DISAGREE: f32 = 60.0;
/// States kept for that check.
const RECENT: usize = 8;
/// Looks are sent at most this often.
pub const LOOK_INTERVAL: Duration = Duration::from_millis(250);
/// `PlayerState` is sent at most this often (60 Hz, with slack for frame jitter).
pub const STATE_INTERVAL: Duration = Duration::from_millis(14);
/// Snapshot interval assumed by the adaptive interpolation delay.
pub const STATE_PERIOD: Duration = Duration::from_nanos(16_666_667);

/// What the ship's step showed this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShipRead {
    pub x: f64,
    pub y: f64,
    pub hspeed: f64,
    pub vspeed: f64,
    pub angle: f64,
    /// Sprite index (`None` if `sprite_index` could not be read).
    pub sprite: Option<u32>,
    pub frame: f64,
    pub xscale: f64,
    pub yscale: f64,
    pub blend: f64,
    pub alpha: f64,
    pub visible: bool,
}

/// What `oPowers` showed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PowersRead {
    pub hull: f64,
    pub hull_max: f64,
    pub shield: f64,
    pub cloaked: bool,
}

/// A sprite index from a `sprite_index` value: a sprite ref or a plain number.
#[must_use]
pub fn sprite_index(v: &Value) -> Option<u32> {
    match *v {
        Value::Ref(p) if p & REF_TAG_MASK == SPRITE_REF_TAG => Some((p & 0xFFFF_FFFF) as u32),
        _ => {
            let f = v.as_f64()?;
            (f.is_finite() && f >= 0.0 && f < f64::from(u32::MAX)).then_some(f as u32)
        }
    }
}

/// The value to pass to drawing builtins for a sprite index.
#[must_use]
pub fn sprite_value(index: u32) -> Value {
    Value::Ref(SPRITE_REF_TAG | u64::from(index))
}

fn finite(v: f64) -> f32 {
    if v.is_finite() { v as f32 } else { 0.0 }
}

/// The state message for the local ship. `steps_per_sec` converts speeds.
#[must_use]
pub fn pack_state(
    peer: PeerId,
    time_ms: u32,
    ship: &ShipRead,
    powers: Option<&PowersRead>,
    steps_per_sec: f64,
) -> PlayerState {
    let sps = if steps_per_sec.is_finite() && steps_per_sec > 0.0 {
        steps_per_sec
    } else {
        60.0
    };
    PlayerState {
        peer,
        time_ms,
        x: finite(ship.x),
        y: finite(ship.y),
        vx: finite(ship.hspeed * sps),
        vy: finite(ship.vspeed * sps),
        angle: finite(ship.angle),
        sprite: ship
            .sprite
            .map_or(u16::MAX, |s| u16::try_from(s).unwrap_or(u16::MAX)),
        frame: if ship.frame.is_finite() && ship.frame >= 0.0 {
            (ship.frame.floor() as u64 % 256) as u8
        } else {
            0
        },
        firing: false,
        hull: powers.map_or(0.0, |p| finite(p.hull)),
        downed: false,
        seg: 0,
    }
}

/// The look message for the local ship.
#[must_use]
pub fn pack_look(peer: PeerId, ship: &ShipRead, powers: Option<&PowersRead>) -> PlayerLook {
    PlayerLook {
        peer,
        xscale: finite(ship.xscale),
        yscale: finite(ship.yscale),
        blend: if ship.blend.is_finite() && ship.blend >= 0.0 {
            (ship.blend as u64 & 0x00FF_FFFF) as u32
        } else {
            0x00FF_FFFF
        },
        alpha: finite(ship.alpha).clamp(0.0, 1.0),
        visible: ship.visible,
        cloaked: powers.is_some_and(|p| p.cloaked),
        hull_max: powers.map_or(0.0, |p| finite(p.hull_max)),
    }
}

/// Continuous look samples kept per partner (about 1 s, one per state tick).
pub const LOOK_FRAMES_KEPT: usize = 64;
/// How long past its newest sample a continuous look is still used.
pub const LOOK_HOLD: Duration = Duration::from_millis(500);
/// How long past its newest sample a continuous look keeps changing the way
/// it was (then it holds).
pub const LOOK_EXTRAPOLATE: Duration = Duration::from_millis(100);

/// The continuously changing part of a ship's look.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LookNow {
    pub xscale: f32,
    pub yscale: f32,
    pub alpha: f32,
    pub blend: u32,
}

/// Pack the continuous look of `look` at sender time `time_ms` (casts take clamped values).
#[must_use]
pub fn pack_look_frame(look: &PlayerLook, time_ms: u32) -> PlayerLookFrame {
    PlayerLookFrame {
        peer: look.peer,
        time_ms,
        xscale: look.xscale,
        yscale: look.yscale,
        blend: look.blend,
        alpha: (look.alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
    }
}

/// A colour `f` of the way from `a` to `b`, per 8-bit channel (each cast in 0..=255).
#[must_use]
pub fn lerp_rgb(a: u32, b: u32, f: f32) -> u32 {
    let f = f.clamp(0.0, 1.0);
    (0..3).fold(0, |acc, i| {
        let ca = f32::from(((a >> (8 * i)) & 0xFF) as u8);
        let cb = f32::from(((b >> (8 * i)) & 0xFF) as u8);
        let c = (ca + (cb - ca) * f).round() as u32;
        acc | (c.min(255) << (8 * i))
    })
}

/// Whether `new` differs visibly from `old` (worth a reliable message).
#[must_use]
pub fn look_changed(old: &PlayerLook, new: &PlayerLook) -> bool {
    let far = |a: f32, b: f32, tol: f32| (a - b).abs() > tol;
    far(old.xscale, new.xscale, 0.01)
        || far(old.yscale, new.yscale, 0.01)
        || far(old.alpha, new.alpha, 0.05)
        || far(old.hull_max, new.hull_max, 0.5)
        || old.blend != new.blend
        || old.visible != new.visible
        || old.cloaked != new.cloaked
}

/// One partner's ship as received.
#[derive(Debug, Clone)]
pub struct RemoteShip {
    track: Track,
    /// Continuous look samples (`PlayerLookFrame`), oldest first.
    look_frames: std::collections::VecDeque<(Duration, LookNow)>,
    smoother: Smoother,
    last: Option<PlayerState>,
    look: Option<PlayerLook>,
    last_rx: Option<Instant>,
    last_pose_at: Option<Instant>,
    /// The render time of the last pose (where corrections are measured).
    last_render: Option<Duration>,
    /// The newest states.
    recent: std::collections::VecDeque<Recent>,
}

/// A state as [`RemoteShip`] keeps it for the motion checks.
#[derive(Debug, Clone, Copy)]
struct Recent {
    time: Duration,
    pos: Vec2,
    vel: Vec2,
    seg: u8,
}

impl RemoteShip {
    /// A ship that extrapolates at most `extrapolation`.
    #[must_use]
    pub fn new(extrapolation: Duration) -> Self {
        Self {
            track: Track::new(extrapolation),
            look_frames: std::collections::VecDeque::new(),
            smoother: Smoother::new(SMOOTH_HALF_LIFE, SNAP_DISTANCE),
            last: None,
            look: None,
            last_rx: None,
            last_pose_at: None,
            last_render: None,
            recent: std::collections::VecDeque::with_capacity(RECENT),
        }
    }

    /// A state arrived. The change it makes at the last render time is
    /// hidden by the smoother.
    pub fn push(&mut self, ps: &PlayerState, now: Instant) {
        let before = self.last_render.and_then(|r| self.target_at(r));
        let state = Recent {
            time: time::from_wire_ms(ps.time_ms),
            pos: Vec2::new(ps.x, ps.y),
            vel: Vec2::new(ps.vx, ps.vy),
            seg: ps.seg,
        };
        let kept = self.track.push(Sample {
            time: state.time,
            pos: state.pos,
            vel: Some(state.vel),
            angle: ps.angle,
            seg: ps.seg,
        });
        if kept {
            self.last = Some(*ps);
            self.last_rx = Some(now);
            if self.recent.len() == RECENT {
                self.recent.pop_front();
            }
            self.recent.push_back(state);
            // Across a jump the change is no correction to hide.
            if !self.jumped_lately()
                && let (Some(b), Some(a)) =
                    (before, self.last_render.and_then(|r| self.target_at(r)))
                && a.seg == b.seg
            {
                self.smoother.absorb(
                    a.pos - b.pos,
                    ndml_net::interp::angle_delta(b.angle, a.angle),
                );
            }
        }
    }

    /// The pose at partner time `render`: interpolated, or past the newest
    /// state along its velocity and turn. The sent velocity gives way to the
    /// positions' own when they disagree (a ship held in place by collisions).
    fn target_at(&self, render: Duration) -> Option<Pose> {
        let mut target = self.track.sample(render)?;
        if target.extrapolated {
            let (ahead, rate) = self.track.turn_past_newest(render);
            target.angle += rate * ahead;
            if let Some(moved) = self.moved_velocity()
                && (moved - target.vel).len() > MOTION_DISAGREE + 0.25 * target.vel.len()
            {
                target.pos = target.pos + (moved - target.vel) * ahead;
                target.vel = moved;
            }
        }
        Some(target)
    }

    /// Whether one of the last three steps between states is a jump.
    fn jumped_lately(&self) -> bool {
        let n = self.recent.len();
        (n.saturating_sub(3).max(1)..n).any(|i| self.jump_into(i))
    }

    /// Whether the step into `recent[i]` is a jump: another segment, or a
    /// step neither the velocities nor the step before explain (the game can
    /// move a ship whose speed is zero, steadily).
    fn jump_into(&self, i: usize) -> bool {
        let (a, b) = (self.recent[i - 1], self.recent[i]);
        if a.seg != b.seg {
            return true;
        }
        let dt = b.time.saturating_sub(a.time).as_secs_f32();
        let step = b.pos - a.pos;
        if (step - (a.vel + b.vel) * (0.5 * dt)).len() <= JUMP_PX {
            return false;
        }
        let Some(z) = i.checked_sub(2).map(|k| self.recent[k]) else {
            return true;
        };
        let dt0 = a.time.saturating_sub(z.time).as_secs_f32();
        z.seg != a.seg || dt0 <= 0.0 || (step - (a.pos - z.pos) * (dt / dt0)).len() > JUMP_PX
    }

    /// The velocity the newest states' positions show over
    /// [`MOTION_WINDOW`], if no jump lies in it.
    fn moved_velocity(&self) -> Option<Vec2> {
        let newest = *self.recent.back()?;
        let mut first = None;
        for i in (1..self.recent.len()).rev() {
            if self.jump_into(i) {
                break;
            }
            let older = self.recent[i - 1];
            first = Some(older);
            if newest.time.saturating_sub(older.time) >= MOTION_WINDOW {
                break;
            }
        }
        let first = first?;
        let dt = newest.time.saturating_sub(first.time);
        (dt >= MOTION_WINDOW / 2).then(|| (newest.pos - first.pos) * (1.0 / dt.as_secs_f32()))
    }

    /// A continuous look sample arrived (kept in time order; a stale one is
    /// dropped).
    pub fn push_look_frame(&mut self, f: &PlayerLookFrame) {
        let t = time::from_wire_ms(f.time_ms);
        if self.look_frames.back().is_some_and(|(b, _)| *b >= t) {
            return;
        }
        self.look_frames.push_back((
            t,
            LookNow {
                xscale: f.xscale,
                yscale: f.yscale,
                alpha: f32::from(f.alpha) / 255.0,
                blend: f.blend,
            },
        ));
        while self.look_frames.len() > LOOK_FRAMES_KEPT {
            self.look_frames.pop_front();
        }
    }

    /// The continuous look at partner time `render`: interpolated; past the
    /// newest, continued for [`LOOK_EXTRAPOLATE`], then held for
    /// [`LOOK_HOLD`]; `None` without covering samples.
    #[must_use]
    pub fn look_at(&self, render: Duration) -> Option<LookNow> {
        let count = self.look_frames.len();
        let (t_new, newest) = *self.look_frames.back()?;
        let (next, ahead) = if render >= t_new {
            let past = render.saturating_sub(t_new);
            if past > LOOK_HOLD {
                return None;
            }
            if count < 2 || past > LOOK_EXTRAPOLATE {
                return Some(newest);
            }
            (count - 1, true)
        } else {
            match self.look_frames.iter().position(|(t, _)| *t > render)? {
                0 => return None,
                at => (at, false),
            }
        };
        let (ta, a) = self.look_frames[next - 1];
        let (tb, b) = self.look_frames[next];
        let f =
            render.saturating_sub(ta).as_secs_f32() / tb.saturating_sub(ta).as_secs_f32().max(1e-6);
        let lerp = |x: f32, y: f32| x + (y - x) * f;
        let scale = |x: f32, y: f32| {
            let v = lerp(x, y);
            if y >= 0.0 { v.max(0.0) } else { v.min(0.0) }
        };
        Some(LookNow {
            xscale: scale(a.xscale, b.xscale),
            yscale: scale(a.yscale, b.yscale),
            alpha: lerp(a.alpha, b.alpha).clamp(0.0, 1.0),
            // A colour is the nearer sample's, never a mix of two.
            blend: if ahead || f >= 0.5 { b.blend } else { a.blend },
        })
    }

    /// A look arrived.
    pub fn set_look(&mut self, look: PlayerLook) {
        self.look = Some(look);
    }

    /// The last look, if any.
    #[must_use]
    pub fn look(&self) -> Option<&PlayerLook> {
        self.look.as_ref()
    }

    /// The newest state.
    #[must_use]
    pub fn latest(&self) -> Option<&PlayerState> {
        self.last.as_ref()
    }

    /// Heard from within [`STALE_AFTER`].
    #[must_use]
    pub fn is_fresh(&self, now: Instant) -> bool {
        self.last_rx
            .is_some_and(|t| now.saturating_duration_since(t) < STALE_AFTER)
    }

    /// The pose to draw at partner time `render`, smoothed (call once per
    /// drawn frame).
    pub fn pose(&mut self, render: Duration, now: Instant) -> Option<Pose> {
        let target = self.target_at(render)?;
        let dt = self
            .last_pose_at
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        self.last_pose_at = Some(now);
        self.last_render = Some(render);
        self.track.prune_before(render);
        // Corrections were absorbed in `push`; a new segment shows at once.
        Some(self.smoother.show(target, dt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fade_is_interpolated_at_the_render_time() {
        let mut r = RemoteShip::new(Duration::from_millis(250));
        let frame = |ms: u32, alpha: u8| PlayerLookFrame {
            peer: PeerId(2),
            time_ms: ms,
            xscale: 1.0,
            yscale: 1.0,
            blend: 0xFF_FFFF,
            alpha,
        };
        assert!(r.look_at(Duration::from_millis(10)).is_none(), "no samples");
        r.push_look_frame(&frame(1000, 255));
        r.push_look_frame(&frame(1033, 64));
        r.push_look_frame(&frame(1020, 0)); // older than the newest: dropped
        let mid = r.look_at(Duration::from_millis(1016)).unwrap();
        assert!(
            (mid.alpha - f32::midpoint(1.0, 64.0 / 255.0)).abs() < 0.03,
            "{mid:?}"
        );
        assert!(
            r.look_at(Duration::from_millis(900)).is_none(),
            "before the first"
        );
        let on = r.look_at(Duration::from_millis(1040)).unwrap();
        assert!(on.alpha < 64.0 / 255.0, "goes on past the newest: {on:?}");
        let held = r.look_at(Duration::from_millis(1400)).unwrap();
        assert!((held.alpha - 64.0 / 255.0).abs() < 1e-3);
        assert!(
            r.look_at(Duration::from_secs(2)).is_none(),
            "too old to hold"
        );
    }

    #[test]
    fn a_colour_steps_to_the_nearer_sample_never_a_mix() {
        let mut r = RemoteShip::new(Duration::from_millis(250));
        let frame = |ms: u32, blend: u32| PlayerLookFrame {
            peer: PeerId(2),
            time_ms: ms,
            xscale: 1.0,
            yscale: 1.0,
            blend,
            alpha: 255,
        };
        r.push_look_frame(&frame(1000, 0xE8_FFC0));
        r.push_look_frame(&frame(1033, 0xFF_F2CF));
        let at = |ms| r.look_at(Duration::from_millis(ms)).unwrap().blend;
        assert_eq!(at(1010), 0xE8_FFC0);
        assert_eq!(at(1025), 0xFF_F2CF);
    }

    #[test]
    fn colours_blend_per_channel() {
        assert_eq!(lerp_rgb(0x00_00FF, 0xFF_0000, 0.0), 0x00_00FF);
        assert_eq!(lerp_rgb(0x00_00FF, 0xFF_0000, 1.0), 0xFF_0000);
        assert_eq!(lerp_rgb(0x00_00FF, 0xFF_0000, 0.5), 0x80_0080);
        assert_eq!(lerp_rgb(0xFF_FFFF, 0xFF_FFFF, 0.3), 0xFF_FFFF);
    }

    fn ship() -> ShipRead {
        ShipRead {
            x: 100.0,
            y: 200.0,
            hspeed: 2.0,
            vspeed: -1.0,
            angle: 90.0,
            sprite: Some(0x338),
            frame: 3.7,
            xscale: 1.0,
            yscale: 1.0,
            blend: f64::from(0x00FF_FFFFu32),
            alpha: 1.0,
            visible: true,
        }
    }

    #[test]
    fn sprite_values_round_trip() {
        assert_eq!(
            sprite_index(&Value::Ref(0x0100_0001_0000_0338)),
            Some(0x338)
        );
        assert_eq!(sprite_index(&Value::Real(12.0)), Some(12));
        assert_eq!(sprite_index(&Value::Real(-1.0)), None);
        assert_eq!(sprite_index(&Value::Undefined), None);
        assert_eq!(
            sprite_index(&Value::Ref(0x0100_0000_0000_025b)),
            None,
            "an object ref is not a sprite"
        );
        assert_eq!(sprite_index(&sprite_value(77)), Some(77));
    }

    #[test]
    fn state_packing_converts_units_and_survives_bad_input() {
        let p = PowersRead {
            hull: 80.0,
            hull_max: 100.0,
            shield: 5.0,
            cloaked: false,
        };
        let s = pack_state(PeerId(1), 1234, &ship(), Some(&p), 60.0);
        assert_eq!(s.peer, PeerId(1));
        assert_eq!(s.time_ms, 1234);
        assert!((s.vx - 120.0).abs() < 1e-3 && (s.vy + 60.0).abs() < 1e-3);
        assert_eq!(s.sprite, 0x338);
        assert_eq!(s.frame, 3);
        assert!((s.hull - 80.0).abs() < 1e-3);
        let wild = ShipRead {
            x: f64::NAN,
            sprite: None,
            frame: -3.0,
            ..ship()
        };
        let s = pack_state(PeerId(1), 0, &wild, None, f64::NAN);
        assert!(s.x.abs() < f32::EPSILON);
        assert!(
            (s.vx - 120.0).abs() < 1e-3,
            "bad room_speed falls back to 60"
        );
        assert_eq!(s.sprite, u16::MAX);
        assert_eq!(s.frame, 0);
        // Round trip through the wire.
        let m = ndml_net::Message::PlayerState(s);
        assert_eq!(ndml_net::Message::decode(&m.encode().unwrap()).unwrap(), m);
    }

    #[test]
    fn look_changes_are_detected_with_tolerance() {
        let p = PowersRead {
            hull_max: 100.0,
            ..PowersRead::default()
        };
        let a = pack_look(PeerId(0), &ship(), Some(&p));
        assert_eq!(a.blend, 0x00FF_FFFF);
        assert!(!look_changed(&a, &a));
        let mut b = a;
        b.alpha = 0.98;
        assert!(!look_changed(&a, &b), "tiny alpha flicker is ignored");
        b.alpha = 0.5;
        assert!(look_changed(&a, &b));
        let mut c = a;
        c.cloaked = true;
        assert!(look_changed(&a, &c));
    }

    fn state(ms: u32, x: f32, vx: f32) -> PlayerState {
        PlayerState {
            peer: PeerId(1),
            time_ms: ms,
            x,
            y: 500.0,
            vx,
            vy: 0.0,
            angle: 0.0,
            sprite: 1,
            frame: 0,
            firing: false,
            hull: 1.0,
            downed: false,
            seg: 0,
        }
    }

    #[test]
    fn a_ship_held_in_place_is_not_drawn_along_its_sent_speed() {
        let t0 = Instant::now();
        // Thrusting at 650 px/s against something that holds it at x = 1270.
        let mut r = RemoteShip::new(Duration::from_millis(250));
        for i in 0..6u32 {
            let x = 1270.0 + if i % 2 == 0 { 0.0 } else { 3.0 };
            r.push(&state(i * 17, x, 650.0), t0);
        }
        let mut x = 0.0;
        for f in 0..8u32 {
            let render = Duration::from_millis(u64::from(5 * 17 + 10 * f));
            let p = r.pose(render, t0 + render).unwrap();
            x = p.pos.x;
        }
        assert!((x - 1270.0).abs() < 15.0, "drawn at {x}");
        // Flying freely the sent velocity is used past the newest state.
        let mut r = RemoteShip::new(Duration::from_millis(250));
        for i in 0..6u32 {
            r.push(
                &state(
                    i * 17,
                    100.0 + 0.65 * f32::from(u16::try_from(i * 17).unwrap()),
                    650.0,
                ),
                t0,
            );
        }
        let render = Duration::from_millis(5 * 17 + 80);
        let p = r.pose(render, t0).unwrap();
        let want = 100.0 + 0.65 * 165.0;
        assert!(p.extrapolated && (p.pos.x - want).abs() < 1.0, "{p:?}");
    }

    #[test]
    fn a_ship_the_game_moves_with_zero_speed_is_drawn_on_time() {
        // Moved at 440 px/s with `hspeed`/`vspeed` 0, states every 33 ms:
        // drawn 60 ms past the newest state where the ship is then.
        for speed in [440.0f32, 2000.0] {
            let t0 = Instant::now();
            let mut r = RemoteShip::new(Duration::from_millis(250));
            let y = |ms: u32| 1000.0 - speed * f32::from(u16::try_from(ms).unwrap()) / 1000.0;
            for i in 0..8u32 {
                r.push(
                    &PlayerState {
                        y: y(i * 33),
                        ..state(i * 33, 1152.0, 0.0)
                    },
                    t0,
                );
            }
            let render = Duration::from_millis(7 * 33 + 60);
            let p = r.pose(render, t0 + render).unwrap();
            let want = y(7 * 33 + 60);
            assert!(
                p.extrapolated && (p.pos.y - want).abs() < 2.0,
                "{speed} px/s: drawn at {} for {want}",
                p.pos.y
            );
        }
    }

    #[test]
    fn a_blink_arriving_past_the_newest_state_is_not_eased() {
        // Marked by the sender (a new segment) or not.
        for seg in [1u8, 0] {
            let t0 = Instant::now();
            let mut r = RemoteShip::new(Duration::from_millis(250));
            let x = |ms: u16| 100.0 + 0.3 * f32::from(ms);
            for i in 0..4u16 {
                r.push(&state(u32::from(i) * 17, x(i * 17), 300.0), t0);
            }
            // Drawn past the newest state (51 ms) when the blink arrives.
            for f in 0..3u64 {
                let render = Duration::from_millis(55 + 10 * f);
                r.pose(render, t0 + render).unwrap();
            }
            r.push(
                &PlayerState {
                    seg,
                    ..state(68, x(68) + 220.0, 300.0)
                },
                t0,
            );
            let render = Duration::from_millis(76);
            let p = r.pose(render, t0 + render).unwrap();
            let want = x(76) + 220.0;
            assert!(
                (p.pos.x - want).abs() < 2.0,
                "seg {seg}: eased: {} vs {want}",
                p.pos.x
            );
        }
    }

    #[test]
    fn remote_ship_interpolates_wraps_and_goes_stale() {
        let t0 = Instant::now();
        let mut r = RemoteShip::new(Duration::from_millis(250));
        // Flying right at 600 px/s across the right edge of a 1000 px
        // battlefield the viewer knows nothing of: the sender marks the wrap.
        let mut segs = ndml_net::interp::Segments::default();
        for i in 0..10u32 {
            let ms = i * 16;
            let x = (990.0 + 600.0 * f64::from(ms) / 1000.0) % 1000.0;
            let seg = segs.observe(Vec2::new(x as f32, 500.0));
            r.push(
                &PlayerState {
                    seg,
                    peer: PeerId(1),
                    time_ms: ms,
                    x: x as f32,
                    y: 500.0,
                    vx: 600.0,
                    vy: 0.0,
                    angle: 0.0,
                    sprite: 1,
                    frame: 0,
                    firing: false,
                    hull: 1.0,
                    downed: false,
                },
                t0,
            );
        }
        let mut fresh = r.clone();
        // Between the samples at 16 ms (x 999.6) and 32 ms (x 9.2, wrapped).
        let p = r.pose(Duration::from_millis(24), t0).unwrap();
        assert!(!p.extrapolated);
        assert!(
            (999.0..1010.0).contains(&p.pos.x) || (-10.0..10.0).contains(&p.pos.x),
            "never swept across the battlefield: {}",
            p.pos.x
        );
        // Past the newest sample (144 ms): extrapolated, capped at 250 ms
        // (unfolded: the receiving game wraps it).
        let late = fresh.pose(Duration::from_secs(1), t0).unwrap();
        assert!(late.extrapolated);
        let newest = (990.0 + 600.0 * 0.144) % 1000.0;
        let expect = newest + 600.0 * 0.25;
        assert!(
            (f64::from(late.pos.x) - expect).abs() < 40.0,
            "{} vs {expect}",
            late.pos.x
        );
        assert!(r.is_fresh(t0 + Duration::from_millis(500)));
        assert!(!r.is_fresh(t0 + STALE_AFTER));
        // Old or duplicate samples are ignored.
        let before = *r.latest().unwrap();
        r.push(
            &PlayerState {
                time_ms: 0,
                ..before
            },
            t0,
        );
        assert_eq!(r.latest().unwrap().time_ms, before.time_ms);
    }
}
