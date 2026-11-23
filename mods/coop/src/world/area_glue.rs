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

//! Area effects' game side; the rule and the family are in [`crate::world::area`].
//!
//! * Joining side: calls to `applyTemporal` (the ship's or a turret's) and the
//!   Vortex pulls are watched; each run frame they go to the host with the
//!   build's values.
//! * Host: each partner's newest state is replayed round its avatar: the values
//!   put on `oPowers`, the game's own script run as the avatar, both put back.
//! * With shared enemy shots the host's Temporal state goes the other way, so a
//!   shot the host slows is slowed on the joining screen too.

#![forbid(unsafe_code)]

use std::sync::OnceLock;

use ndml_net::msg::{AreaState, Message, PeerId};
use ndml_sdk::{Builtin, Call, ModContext, Script, Value, Variable};

use super::names::builtin_var as bv;
use super::{ROLE_CLIENT, ROLE_HOST, WorldHooks};
use crate::world::area::{self, AreaRecv, AreaSend, Seen};
use crate::world::classify::object_index;
use crate::world::entities::EntityKey;

struct Handles {
    values: Vec<Option<Variable>>,
    apply: Option<Script>,
    /// `vortexGravity` (the host makes every Vortex's pull with it).
    pull: Option<Script>,
    asset: Option<Builtin>,
}

static AH: OnceLock<Handles> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
struct Objects {
    player: u32,
    turret: Option<u32>,
    powers: u32,
    sanctuary: Option<u32>,
}

/// Per-session state.
#[derive(Debug, Default)]
pub(super) struct AreaGlue {
    objects: Option<Objects>,
    tried: bool,
    /// Joining side: calls watched since the last run frame.
    seen: Vec<Seen>,
    send: AreaSend,
    recv: AreaRecv,
}

impl AreaGlue {
    /// Forget the watched calls and the states sent and received.
    pub(super) fn clear(&mut self) {
        self.seen.clear();
        self.send.reset();
        self.recv.clear();
    }
}

/// Resolve the handles and watch `applyTemporal` and the Vortex pulls (init).
pub(super) fn register(ctx: &ModContext, w: &std::sync::Arc<WorldHooks>) {
    let _ = AH.set(Handles {
        values: area::VALUES.iter().map(|n| ctx.variable(n).ok()).collect(),
        apply: ctx.script(area::APPLY_TEMPORAL).ok(),
        pull: ctx.script(area::VORTEX_GRAVITY).ok(),
        asset: ctx.builtin("asset_get_index").ok(),
    });
    let c = std::sync::Arc::clone(w);
    super::hook_or_warn(
        area::APPLY_TEMPORAL,
        ctx.hook_before(area::APPLY_TEMPORAL, move |call| c.area_seen(call)),
    );
    for (name, released) in [(area::VORTEX_GRAVITY, false), (area::VORTEX_GRAVITY2, true)] {
        let c = std::sync::Arc::clone(w);
        super::hook_or_warn(
            name,
            ctx.hook_before(name, move |call| c.area_vortex_seen(call, released)),
        );
    }
}

fn pos(i: &ndml_sdk::Instance<'_>) -> Option<(f64, f64)> {
    Some((i.builtin_f64(bv::X)?, i.builtin_f64(bv::Y)?))
}

impl WorldHooks {
    fn area_objects(&self, call: &Call<'_>) -> Option<Objects> {
        {
            let st = self.lock();
            if st.area.objects.is_some() || st.area.tried {
                return st.area.objects;
            }
        }
        let h = AH.get()?;
        let asset = |n: &str| {
            h.asset
                .and_then(|b| call.call_builtin(b, &[Value::Str(n.into())]).ok())
                .as_ref()
                .and_then(object_index)
        };
        let o = match (asset("oPlayer"), asset("oPowers")) {
            (Some(player), Some(powers)) => Some(Objects {
                player,
                turret: asset("oConstructTurret"),
                powers,
                sanctuary: asset("oSanctuaryController"),
            }),
            _ => None,
        };
        let mut st = self.lock();
        st.area.tried = true;
        st.area.objects = o;
        if o.is_none() {
            log::warn!("co-op area: oPlayer or oPowers not found; area effects off");
        }
        o
    }

    /// Before `applyTemporal`: where the caller is (joining side; the host
    /// too with shared enemy shots on).
    fn area_seen(&self, call: &mut Call<'_>) {
        if self.role() != ROLE_CLIENT && !(self.role() == ROLE_HOST && self.shots_on()) {
            return;
        }
        let Some(o) = self.area_objects(call) else {
            return;
        };
        let Some(me) = call.instance() else { return };
        let Some((x, y)) = pos(&me) else { return };
        let obj = me
            .builtin_value(bv::OBJECT_INDEX)
            .ok()
            .as_ref()
            .and_then(object_index);
        let mut st = self.lock();
        // A bound on a frame's calls (the ship and its riders).
        if st.area.seen.len() < 256 {
            st.area.seen.push(Seen::temporal(
                x,
                y,
                obj == Some(o.player),
                obj.is_some() && obj == o.turret,
            ));
        }
    }

    /// Joining side, before a Vortex's pull (`vortexGravity`, `vortexGravity2`):
    /// its place and size go to the host, which pulls its enemies
    /// ([`crate::world::area`]). The pull is `with (oSolid)`, and every lasting
    /// `oSolid` here is a host copy, so the local call is skipped.
    fn area_vortex_seen(&self, call: &mut Call<'_>, released: bool) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let Some(me) = call.instance() else {
            return;
        };
        let Some((x, y)) = pos(&me) else {
            return;
        };
        let scale = me.builtin_f64(bv::IMAGE_XSCALE).unwrap_or(f64::NAN) as f32;
        {
            let mut st = self.lock();
            if st.area.seen.len() < 256 {
                st.area.seen.push(Seen::vortex(x, y, scale, released));
            }
        }
        call.skip_original();
    }

    /// Host: one Vortex's pull, the game's `vortexGravity` as the avatar at
    /// the shot's place with the shot's size and the partner's
    /// `polarInversion`; all put back.
    fn area_pull(
        call: &Call<'_>,
        o: Objects,
        pull: Option<Script>,
        avatar: &Value,
        (ax, ay): (f64, f64),
        values: &[f32],
        c: &ndml_net::msg::AreaCenter,
    ) {
        let (Some(pull), Some((vals, scale))) = (pull, area::vortex_values(values, c)) else {
            return;
        };
        let own = Self::area_swap(call, o, &vals);
        let (cx, cy) = (ax + f64::from(c.dx), ay + f64::from(c.dy));
        let _ = call.with(avatar, |i, _| {
            let old_scale = i.builtin_f64(bv::IMAGE_XSCALE);
            let set = i.set_builtin_f64(bv::X, cx).is_ok()
                && i.set_builtin_f64(bv::Y, cy).is_ok()
                && i.set_builtin_f64(bv::IMAGE_XSCALE, scale).is_ok();
            if set {
                let _ = i.call_script(pull, &[]);
            }
            let _ = i.set_builtin_f64(bv::X, ax);
            let _ = i.set_builtin_f64(bv::Y, ay);
            if let Some(s) = old_scale {
                let _ = i.set_builtin_f64(bv::IMAGE_XSCALE, s);
            }
        });
        Self::area_restore(call, o, &own);
    }

    /// The build's values ([`area::VALUES`] of `oPowers`; `NaN` unset).
    fn area_values(call: &Call<'_>, o: Objects) -> Vec<f32> {
        let mut out = vec![f32::NAN; area::VALUES.len()];
        let Some(h) = AH.get() else { return out };
        let _ = call.with(&Value::object(o.powers), |p, _| {
            for (slot, v) in out.iter_mut().zip(&h.values) {
                if let Some(x) = v.and_then(|v| p.get_f64(v)) {
                    *slot = x as f32;
                }
            }
        });
        out
    }

    fn first_pos(call: &Call<'_>, obj: Option<u32>) -> Option<(f64, f64)> {
        let mut at = None;
        let _ = call.with(&Value::object(obj?), |i, _| {
            if at.is_none() {
                at = pos(&i);
            }
        });
        at
    }

    /// Joining side, each run frame: this frame's state to the host when due.
    /// The host too with shared enemy shots on: its Temporal centres only (to
    /// the joining games, for the shots they show).
    pub(super) fn area_client(&self, call: &Call<'_>, me: PeerId, now_ms: u32) -> Vec<Message> {
        let host = self.role() == ROLE_HOST;
        if !(self.role() == ROLE_CLIENT || (host && self.shots_on())) {
            return Vec::new();
        }
        let Some(o) = self.area_objects(call) else {
            return Vec::new();
        };
        let seen = std::mem::take(&mut self.lock().area.seen);
        let values = Self::area_values(call, o);
        let sanctuary = if values.get(2).is_some_and(|v| *v != 0.0 && !v.is_nan()) {
            Self::first_pos(call, o.sanctuary)
        } else {
            None
        };
        // The ship's place (instance 0 of `oPlayer`, the one the game's own
        // call is made from).
        let ship = Self::first_pos(call, Some(o.player));
        let temporal = values[area::SHIELD_TYPE] == area::TEMPORAL;
        let mut centers = area::centers(&seen, ship, sanctuary, temporal);
        if host {
            // The host's pull moves its own enemies, posed on the joining
            // screens already.
            centers.retain(|c| c.kind == area::KIND_TEMPORAL);
        }
        let frame = self.lock().frame;
        let due = self.lock().area.send.due(values, centers, frame);
        let mut out = Vec::new();
        if let Some((values, centers)) = due {
            out.push(Message::AreaState(AreaState {
                peer: me,
                time_ms: now_ms,
                names: area::names_hash(),
                values,
                centers,
            }));
        }
        out
    }

    /// Host, each run frame: the partners' states, and their calls made
    /// round their avatars while the game runs.
    pub(super) fn area_host(&self, call: &Call<'_>, inbox: &[Message]) {
        let (frame, frozen) = {
            let mut st = self.lock();
            let frame = st.frame;
            for m in inbox {
                if let Message::AreaState(s) = m {
                    st.area.recv.take(s.clone(), frame);
                }
            }
            (frame, st.frozen)
        };
        // Frozen: the game deactivated every enemy; nothing acts.
        if frozen {
            return;
        }
        self.area_apply(call, frame, false);
    }

    /// Joining side, before the frame's messages are used: the host's
    /// Temporal states (shared enemy shots on).
    pub(super) fn area_inbox(&self, inbox: &mut Vec<Message>) {
        if self.role() != ROLE_CLIENT || !self.shots_on() {
            return;
        }
        let mut st = self.lock();
        let frame = st.frame;
        inbox.retain(|m| match m {
            Message::AreaState(s) => {
                st.area.recv.take(s.clone(), frame);
                false
            }
            _ => true,
        });
    }

    /// Joining side, each running frame (shared enemy shots on): the host's
    /// Temporal calls made round its avatar here, with its values, so the
    /// shots it slows are slowed here too.
    pub(super) fn area_partner(&self, call: &Call<'_>) {
        if self.role() != ROLE_CLIENT || !self.shots_on() {
            return;
        }
        let (frame, frozen) = {
            let st = self.lock();
            (st.frame, st.frozen)
        };
        if !frozen {
            self.area_apply(call, frame, true);
        }
    }

    /// Each partner's live state: its calls made round its avatar
    /// (`temporal_only`: the Temporal shield's, no Vortex).
    fn area_apply(&self, call: &Call<'_>, frame: u64, temporal_only: bool) {
        let Some(o) = self.area_objects(call) else {
            return;
        };
        let (apply, pull) = match AH.get() {
            Some(h) => (h.apply, h.pull),
            None => return,
        };
        let live = self.lock().area.recv.live(frame);
        for s in live {
            let avatar = self.lock().ents.id(EntityKey::ship(s.peer));
            let Some(avatar) = avatar else {
                continue;
            };
            let mut at = None;
            let _ = call.with(&avatar, |i, _| at = pos(&i));
            let Some((ax, ay)) = at else {
                continue;
            };
            for c in &s.centers {
                if c.kind == area::KIND_VORTEX && temporal_only {
                    continue;
                }
                if c.kind == area::KIND_VORTEX {
                    Self::area_pull(call, o, pull, &avatar, (ax, ay), &s.values, c);
                    continue;
                }
                let Some(apply) = apply else { continue };
                let Some(vals) = area::call_values(&s.values, c.turret) else {
                    continue;
                };
                let own = Self::area_swap(call, o, &vals);
                let (cx, cy) = (ax + f64::from(c.dx), ay + f64::from(c.dy));
                let _ = call.with(&avatar, |i, _| {
                    let moved = i.set_builtin_f64(bv::X, cx).is_ok()
                        && i.set_builtin_f64(bv::Y, cy).is_ok();
                    if moved {
                        let _ = i.call_script(apply, &[]);
                    }
                    let _ = i.set_builtin_f64(bv::X, ax);
                    let _ = i.set_builtin_f64(bv::Y, ay);
                });
                Self::area_restore(call, o, &own);
            }
        }
    }

    /// Put `vals` on `oPowers`; the values they replaced, to put back.
    fn area_swap(call: &Call<'_>, o: Objects, vals: &[(usize, f64)]) -> Vec<(Variable, Value)> {
        let mut own = Vec::new();
        let Some(h) = AH.get() else { return own };
        let _ = call.with(&Value::object(o.powers), |p, _| {
            for &(i, v) in vals {
                let Some(var) = h.values.get(i).copied().flatten() else {
                    continue;
                };
                if let Some(mine) = p.get_value(var)
                    && p.set(var, Value::Real(v)).is_ok()
                {
                    own.push((var, mine));
                }
            }
        });
        own
    }

    fn area_restore(call: &Call<'_>, o: Objects, own: &[(Variable, Value)]) {
        if own.is_empty() {
            return;
        }
        let _ = call.with(&Value::object(o.powers), |p, _| {
            for (var, v) in own {
                let _ = p.set(*var, v.clone());
            }
        });
    }
}
