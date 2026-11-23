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

//! Beam cover from a partner's Shield Bastion and Arc Barrier drones.
//!
//! The game blocks beam damage in the victim's collision event when its own
//! Bastion (shield up, no Shielded Constructs) or an Arc Barrier drone at
//! least 10 frames old lies on the line from the beam's origin. A partner's
//! are proxies here, which `collision_line` cannot test, so the mod tests the
//! line against each proxy's box with the partner's [`CoverState`] and skips
//! the event when covered.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use ndml_net::msg::{CoverState, Message, PeerId};
use ndml_sdk::{Builtin, Call, ModContext, Value, Variable};

use super::names::builtin_var as bv;
use super::{ROLE_OFF, WorldHooks};
use crate::world::classify::object_index;

/// The events the cover is tested before.
const EVENTS: [&str; 2] = [
    "gml_Object_oPlayer_Collision_oEnemyBeam",
    "gml_Object_oPlayerSegment_Collision_oEnemyBeam",
];
/// An Arc Barrier drone covers from this age (frames its proxy has been drawn).
const DRONE_AGE: u64 = 10;
/// The facts are sent again this often (frames) without a change.
const RESEND_EVERY: u64 = 120;

struct Handles {
    shield: Option<Variable>,
    shielded_drones: Option<Variable>,
    arc_barrier: Option<Variable>,
    revenge: Option<Variable>,
    asset: Option<Builtin>,
    /// `sprite_get_xoffset`, `_yoffset`, `_bbox_left`, `_bbox_right`,
    /// `_bbox_top`, `_bbox_bottom`.
    sprite: [Option<Builtin>; 6],
}

static CH: OnceLock<Handles> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
struct Objects {
    powers: u32,
    bastion: u16,
    drone: u16,
}

/// Per-session state.
#[derive(Debug, Default)]
pub(super) struct CoverGlue {
    objects: Option<Objects>,
    tried: bool,
    /// What this game last sent, and when.
    sent: Option<CoverState>,
    sent_frame: u64,
    /// The partners' facts.
    partners: HashMap<PeerId, CoverState>,
    /// Drone proxies by (owner, key): the frame each was first seen.
    seen: HashMap<(PeerId, u16), u64>,
}

impl CoverGlue {
    /// Forget what was sent and the partners' facts.
    pub(super) fn clear(&mut self) {
        self.sent = None;
        self.partners.clear();
        self.seen.clear();
    }
}

/// Resolve the handles and hook the ship's and segments' beam events.
pub(super) fn register(ctx: &ModContext, w: &std::sync::Arc<WorldHooks>) {
    let _ = CH.set(Handles {
        shield: ctx.variable("shield").ok(),
        shielded_drones: ctx.variable("shieldedDrones").ok(),
        arc_barrier: ctx.variable("arcBarrier").ok(),
        revenge: ctx.variable("revenge").ok(),
        asset: ctx.builtin("asset_get_index").ok(),
        sprite: [
            "sprite_get_xoffset",
            "sprite_get_yoffset",
            "sprite_get_bbox_left",
            "sprite_get_bbox_right",
            "sprite_get_bbox_top",
            "sprite_get_bbox_bottom",
        ]
        .map(|n| ctx.builtin(n).ok()),
    });
    for name in EVENTS {
        let c = std::sync::Arc::clone(w);
        super::hook_or_warn(
            name,
            ctx.hook_before(name, move |call| c.cover_before(call)),
        );
    }
}

impl WorldHooks {
    fn cover_objects(&self, call: &Call<'_>) -> Option<Objects> {
        {
            let st = self.lock();
            if st.cover.objects.is_some() || st.cover.tried {
                return st.cover.objects;
            }
        }
        let h = CH.get()?;
        let asset = |n: &str| {
            h.asset
                .and_then(|b| call.call_builtin(b, &[Value::Str(n.into())]).ok())
                .as_ref()
                .and_then(object_index)
        };
        let o = match (
            asset("oPowers"),
            asset("oShieldBastion"),
            asset("oConstructDrone"),
        ) {
            (Some(powers), Some(b), Some(d)) => Some(Objects {
                powers,
                bastion: u16::try_from(b).ok()?,
                drone: u16::try_from(d).ok()?,
            }),
            _ => None,
        };
        let mut st = self.lock();
        st.cover.tried = true;
        st.cover.objects = o;
        if o.is_none() {
            log::warn!(
                "co-op cover: oPowers, oShieldBastion or oConstructDrone not found; beam cover off"
            );
        }
        o
    }

    /// Run step: this game's facts to the partners (on a change and every
    /// [`RESEND_EVERY`] frames).
    pub(super) fn cover_run_step(&self, call: &Call<'_>, me: Option<PeerId>) -> Vec<Message> {
        let (Some(me), Some(o), Some(h)) = (me, self.cover_objects(call), CH.get()) else {
            return Vec::new();
        };
        let mut facts = CoverState {
            peer: me,
            shield_up: false,
            shielded_drones: false,
            arc_barrier: false,
        };
        let truthy = |v: Option<f64>| v.is_some_and(|x| x > 0.5);
        let _ = call.with(&Value::object(o.powers), |p, _| {
            facts.shield_up = h.shield.and_then(|v| p.get_f64(v)).is_some_and(|s| s > 0.0);
            facts.shielded_drones = truthy(h.shielded_drones.and_then(|v| p.get_f64(v)));
            facts.arc_barrier = truthy(h.arc_barrier.and_then(|v| p.get_f64(v)));
        });
        let mut st = self.lock();
        let frame = st.frame;
        let mut out = Vec::new();
        if st.cover.sent != Some(facts) || frame.saturating_sub(st.cover.sent_frame) >= RESEND_EVERY
        {
            st.cover.sent = Some(facts);
            st.cover.sent_frame = frame;
            out.push(Message::CoverState(facts));
        }
        out
    }

    /// Run step: the partners' facts.
    pub(super) fn cover_inbox(&self, inbox: &mut Vec<Message>) {
        let mut st = self.lock();
        let me = st.me;
        inbox.retain(|m| match m {
            Message::CoverState(c) => {
                if Some(c.peer) != me {
                    st.cover.partners.insert(c.peer, *c);
                }
                false
            }
            _ => true,
        });
    }

    /// Before this game's ship's (or segment's) collision with an enemy
    /// beam: a partner's Bastion or Arc Barrier drone on the line from the
    /// beam's origin covers it as its own would.
    fn cover_before(&self, call: &mut Call<'_>) {
        if self.role() == ROLE_OFF {
            return;
        }
        let Some(h) = CH.get() else { return };
        let (Some(me), Some(beam)) = (call.instance(), call.other()) else {
            return;
        };
        if h.revenge
            .and_then(|v| beam.get_f64(v))
            .is_some_and(|r| r != 0.0)
        {
            return;
        }
        let (Some(sx), Some(sy), Some(bx), Some(by)) = (
            me.builtin_f64(bv::X),
            me.builtin_f64(bv::Y),
            beam.builtin_f64(bv::X),
            beam.builtin_f64(bv::Y),
        ) else {
            return;
        };
        let Some(o) = self.cover_objects(call) else {
            return;
        };
        let Some(links) = self.shot.things.get() else {
            return;
        };
        let partners: Vec<CoverState> = {
            let st = self.lock();
            st.cover.partners.values().copied().collect()
        };
        let bastion_on = |p: PeerId| {
            partners
                .iter()
                .any(|c| c.peer == p && c.shield_up && !c.shielded_drones)
        };
        let arc_on = |p: PeerId| partners.iter().any(|c| c.peer == p && c.arc_barrier);
        let frame = self.lock().frame;
        let proxies = (links.proxies_of)(&[o.bastion, o.drone]);
        let mut covered = false;
        for (owner, key, obj, id) in proxies {
            let drone = obj == o.drone;
            if drone {
                let first = *self.lock().cover.seen.entry((owner, key)).or_insert(frame);
                if !arc_on(owner) || frame.saturating_sub(first) < DRONE_AGE {
                    continue;
                }
            } else if !bastion_on(owner) {
                continue;
            }
            let shape = proxy_box(call, h, &id);
            let on = shape
                .as_ref()
                .is_some_and(|b| b.crosses((bx, by), (sx, sy)));
            if on {
                covered = true;
                break;
            }
        }
        if covered {
            call.skip_original();
        }
    }
}

/// A proxy's collision box as drawn: the partner's sprite's bounding box,
/// scaled and turned like the proxy, at its place.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Box2 {
    at: (f64, f64),
    angle_deg: f64,
    /// Left, right, top, bottom in the box's own frame (px, scaled).
    edges: [f64; 4],
}

impl Box2 {
    /// Whether the segment `from`-`to` crosses the box (Liang-Barsky in the
    /// box's frame).
    fn crosses(&self, from: (f64, f64), to: (f64, f64)) -> bool {
        let (sin, cos) = self.angle_deg.to_radians().sin_cos();
        // GML: y down, angles counter-clockwise.
        let local = |p: (f64, f64)| {
            let (dx, dy) = (p.0 - self.at.0, p.1 - self.at.1);
            (dx * cos - dy * sin, dx * sin + dy * cos)
        };
        let (a, b) = (local(from), local(to));
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let [left, right, top, bottom] = self.edges;
        let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
        for (p, q) in [
            (-dx, a.0 - left),
            (dx, right - a.0),
            (-dy, a.1 - top),
            (dy, bottom - a.1),
        ] {
            if p == 0.0 {
                if q < 0.0 {
                    return false;
                }
            } else {
                let r = q / p;
                if p < 0.0 {
                    t0 = t0.max(r);
                } else {
                    t1 = t1.min(r);
                }
                if t0 > t1 {
                    return false;
                }
            }
        }
        true
    }
}

/// The box of the proxy `id` as drawn now (`None`: no sprite).
fn proxy_box(call: &Call<'_>, handles: &Handles, id: &Value) -> Option<Box2> {
    let mut read = None;
    let _ = call.with(id, |p, _| {
        read = (|| {
            Some((
                p.builtin_f64(bv::X)?,
                p.builtin_f64(bv::Y)?,
                sprite_of(&p)?,
                p.builtin_f64(bv::IMAGE_XSCALE).unwrap_or(1.0),
                p.builtin_f64(bv::IMAGE_YSCALE).unwrap_or(1.0),
                p.builtin_f64(bv::IMAGE_ANGLE).unwrap_or(0.0),
            ))
        })();
    });
    let (px, py, sprite, xs, ys, angle) = read?;
    if matches!(sprite, Value::Real(n) if n < 0.0) {
        return None;
    }
    let mut got = [0.0; 6];
    for (slot, b) in got.iter_mut().zip(handles.sprite) {
        *slot = call
            .call_builtin(b?, std::slice::from_ref(&sprite))
            .ok()?
            .as_f64()?;
    }
    let [xoff, yoff, bl, br, bt, bb] = got;
    let (left, right) = ((bl - xoff) * xs, (br + 1.0 - xoff) * xs);
    let (top, bottom) = ((bt - yoff) * ys, (bb + 1.0 - yoff) * ys);
    Some(Box2 {
        at: (px, py),
        angle_deg: angle,
        edges: [
            left.min(right),
            left.max(right),
            top.min(bottom),
            top.max(bottom),
        ],
    })
}

/// `sprite_index` as the game passes it (a sprite ref; `-1`: none).
fn sprite_of(inst: &ndml_sdk::Instance<'_>) -> Option<Value> {
    match inst.builtin_value(bv::SPRITE_INDEX).ok()? {
        v @ Value::Ref(_) => Some(v),
        v => Some(Value::Real(v.as_f64()?)),
    }
}

#[cfg(test)]
mod tests {
    use super::Box2;

    fn flat(angle_deg: f64) -> Box2 {
        Box2 {
            at: (100.0, 100.0),
            angle_deg,
            edges: [-10.0, 10.0, -5.0, 5.0],
        }
    }

    #[test]
    fn a_segment_through_the_box_crosses_it() {
        let b = flat(0.0);
        assert!(b.crosses((0.0, 100.0), (200.0, 100.0)));
        assert!(b.crosses((100.0, 0.0), (100.0, 200.0)));
        assert!(
            !b.crosses((0.0, 106.0), (200.0, 106.0)),
            "6 px off a 5 px half height"
        );
        assert!(!b.crosses((0.0, 100.0), (80.0, 100.0)), "ends short of it");
        assert!(b.crosses((95.0, 99.0), (96.0, 99.0)), "inside");
    }

    #[test]
    fn the_box_turns_with_the_angle() {
        // Turned 90 degrees: 10 px wide in x, 20 px tall in y.
        let b = flat(90.0);
        assert!(
            !b.crosses((107.0, 0.0), (107.0, 200.0)),
            "7 px off a 5 px half width"
        );
        assert!(
            b.crosses((0.0, 108.0), (200.0, 108.0)),
            "8 px off a 10 px half height"
        );
    }
}
