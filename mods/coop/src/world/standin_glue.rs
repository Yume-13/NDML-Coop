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

//! The stand-in bracket's game side (the rule: [`crate::world::standin`]).
//!
//! * Host: the event observer (Step kinds, alarms, user events, Destroy) and
//!   hooks on the death scripts open and close brackets for the instances the
//!   frame's [`Plan`] wants. The plan is rebuilt at each run step, after the
//!   avatars have moved.
//! * While no partner has a place, the observer returns after one load; for
//!   an object with no bracketed event, after a bit test.
//! * The observer never takes the world's lock and runs no GML code.
//! * The `chooseTarget` after-hook writes the id
//!   [`WorldHooks::standin_target`] gives.
//! * A bracket whose end was never seen is closed at the first event of the
//!   host's ship, at the run step, and when the world goes off.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use ndml_net::msg::PeerId;
use ndml_sdk::{Call, Instance, ModContext, ObservedEvent, Value, Variable};

use super::{ROLE_HOST, WorldHooks};
use crate::world::entities::EntityKey;
use crate::world::standin::{
    self, Active, Begin, Bracket, End, Events, Now, Plan, Saved, StandIn, Use,
};

/// GameMaker's event kinds the bracket covers: Destroy, alarms, the Step
/// kinds (Begin, Step, End: one kind, numbers 0 to 2), and Other for its
/// user events.
const KIND_DESTROY: i32 = 1;
const KIND_ALARM: i32 = 2;
const KIND_STEP: i32 = 3;
const KIND_OTHER: i32 = 7;
/// User events are Other numbers 10 to 25.
const USER_EVENTS: std::ops::RangeInclusive<i32> = 10..=25;
/// Object indices tested (as `decoy_glue`).
const OBJECT_SCAN: u32 = 1400;
/// Frames between tries to resolve the tables when a name is missing.
const RESOLVE_EVERY: u64 = 600;

/// A partner has a place this frame: the observer does its work.
static ON: AtomicBool = AtomicBool::new(false);
/// A bracket is open: every observed event counts (nesting, home).
static OPEN: AtomicBool = AtomicBool::new(false);
/// `oPlayer`'s index (-1 unknown): its events close a leftover bracket.
static PLAYER: AtomicI32 = AtomicI32::new(-1);
/// The frame of the last failed resolve.
static RESOLVE_TRIED: AtomicU64 = AtomicU64::new(0);
static ST: Mutex<Option<GlueState>> = Mutex::new(None);
static VARS: OnceLock<Vars> = OnceLock::new();
/// The bracketable events by object ([`standin::bracketable`]), set once
/// every table name resolved, and the objects that have any as a bit set:
/// read without a lock.
static EVENTS: OnceLock<(HashMap<i32, Events>, Vec<u64>)> = OnceLock::new();

fn has_bit(bits: &[u64], object: i32) -> bool {
    usize::try_from(object)
        .ok()
        .and_then(|i| bits.get(i / 64).map(|w| ((w >> (i % 64)) & 1) == 1))
        .unwrap_or(false)
}

/// How event (kind, number) of `object` is bracketed, if it is.
fn use_of(object: i32, kind: i32, number: i32) -> Option<Use> {
    let (m, bits) = EVENTS.get()?;
    if !has_bit(bits, object) {
        return None;
    }
    m.get(&object)?.get(kind, number)
}

struct Vars {
    cloaked: Option<Variable>,
    target: Option<Variable>,
}

#[derive(Default)]
struct GlueState {
    plan: Plan,
    bracket: Bracket,
    /// `oPowers`'s index.
    powers: Option<u32>,
}

fn state<R>(f: impl FnOnce(&mut GlueState) -> R) -> R {
    let mut g = ST.lock().unwrap_or_else(PoisonError::into_inner);
    f(g.get_or_insert_with(GlueState::default))
}

/// Resolve the variables, observe the bracketed event kinds and wrap the
/// death scripts.
pub(super) fn register(ctx: &ModContext, w: &std::sync::Arc<WorldHooks>) {
    let _ = VARS.set(Vars {
        cloaked: ctx.variable(crate::names::VAR_CLOAKED).ok(),
        target: w.h.target,
    });
    for kind in [KIND_STEP, KIND_ALARM, KIND_OTHER, KIND_DESTROY] {
        if let Err(e) = ctx.observe_event(kind, -1, observed) {
            log::warn!(
                "co-op stand-in: {e}; on the host, enemy code that names the player by name finds the host's ship even when it is after the joining player"
            );
            return;
        }
    }
    let mut wrapped = 0;
    for name in standin::DEATH_SCRIPTS {
        match ctx.hook(name, death_before, death_after) {
            Ok(()) => wrapped += 1,
            Err(e) => log::warn!(
                "co-op stand-in: {e}; a death inside a bracket runs with the ship standing in"
            ),
        }
    }
    log::info!(
        "co-op stand-in: observing Step, alarm, user and Destroy events, {wrapped} of {} death scripts wrapped (on the host, an instance after the joining player finds that player where it names the player)",
        standin::DEATH_SCRIPTS.len()
    );
}

/// The world is off (no co-op run, or not the host's): a bracket left open
/// is closed, nothing is bracketed.
pub(super) fn off(call: &Call<'_>) {
    if !ON.swap(false, Ordering::Relaxed) && !OPEN.load(Ordering::Relaxed) {
        return;
    }
    if let Some(me) = call.instance() {
        close_stale(&me, "the world went off");
    }
    state(|s| s.plan = Plan::default());
}

/// A bracket left open: the ship back now.
fn close_stale(me: &Instance<'_>, why: &str) {
    let a = state(|s| s.bracket.take_stale());
    OPEN.store(false, Ordering::Relaxed);
    if let Some(a) = a {
        restore(me, &a, true);
        log::warn!(
            "co-op stand-in: a bracket of object {} was left open (its event's end was not seen); the ship was put back ({why})",
            a.object
        );
    }
}

fn observed(e: &ObservedEvent<'_>) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    if e.kind == KIND_OTHER && !USER_EVENTS.contains(&e.number) {
        return;
    }
    let open = OPEN.load(Ordering::Relaxed);
    let u = if e.kind == KIND_DESTROY {
        None
    } else {
        use_of(e.instance_object, e.kind, e.number)
    };
    if !open && u.is_none() {
        return;
    }
    let Some(i) = e.instance() else { return };
    if open && !e.after && e.instance_object == PLAYER.load(Ordering::Relaxed) {
        // The host's ship acts: no bracket is open legitimately now.
        close_stale(&i, "before the host's ship's own event");
    }
    if e.after {
        after(&i);
    } else {
        before(&i, Some(e.id), e.instance_object, u, e.kind == KIND_DESTROY);
    }
}

fn death_before(call: &mut Call<'_>) -> bool {
    if !OPEN.load(Ordering::Relaxed) {
        return false;
    }
    let Some(me) = call.instance() else {
        return false;
    };
    before(&me, None, -1, None, true);
    true
}

fn death_after(call: &mut Call<'_>, counted: bool) {
    if counted && let Some(me) = call.instance() {
        after(&me);
    }
}

fn before(me: &Instance<'_>, id: Option<i32>, object: i32, u: Option<Use>, home: bool) {
    let key = me.key();
    // A swap is possible: the places it is decided by, read outside the lock.
    let ship = state(|s| {
        (!s.bracket.is_open() && !home && u.is_some_and(|u| s.plan.wants(key, u)))
            .then(|| s.plan.ship.clone())
            .flatten()
    });
    let places = ship.as_ref().and_then(|ship| {
        let inst = (me.builtin_f64("x")?, me.builtin_f64("y")?);
        Some((inst, ship_place(me, ship)?))
    });
    let (begin, at, powers) = state(|s| {
        let at = match (u, places) {
            (Some(u), Some((inst, ship_at))) => s.plan.choose(key, u, inst, ship_at).cloned(),
            _ => None,
        };
        let b = s.bracket.begin(key, at.is_some(), home);
        (b, at, s.powers)
    });
    match begin {
        Begin::Pass | Begin::Nested => {}
        Begin::Home => {
            if let Some(a) = state(|s| s.bracket.active_mut().cloned()) {
                restore(me, &a, false);
            }
        }
        Begin::Swap => {
            let (Some(ship), Some(at), Some(u), Some(id)) = (ship, at, u, id) else {
                return;
            };
            let inst = Value::Real(f64::from(id));
            let Some(a) = swap(me, key, inst, object, ship, powers, at, u.retarget) else {
                return;
            };
            state(|s| {
                s.bracket.open(a);
            });
            OPEN.store(true, Ordering::Relaxed);
        }
    }
}

fn ship_place(me: &Instance<'_>, ship: &Value) -> Option<(f64, f64)> {
    let mut at = None;
    let _ = me.with(ship, |s, _| {
        if let (Some(x), Some(y)) = (s.builtin_f64("x"), s.builtin_f64("y")) {
            at = Some((x, y));
        }
    });
    at
}

/// Stand the ship at `at`; what was there, to put back. `None`: nothing
/// changed. `inst`: the bracketed instance's id (its `target`, written here
/// with `retarget` when `me` is that instance).
#[allow(clippy::too_many_arguments)] // one bracket's parts
fn swap(
    me: &Instance<'_>,
    key: usize,
    inst: Value,
    object: i32,
    ship: Value,
    powers: Option<u32>,
    at: StandIn,
    retarget: bool,
) -> Option<Active> {
    let vars = VARS.get()?;
    let mut saved = None;
    let _ = me.with(&ship, |s, _| {
        let (Some(x), Some(y)) = (s.builtin_f64("x"), s.builtin_f64("y")) else {
            return;
        };
        if s.set_builtin_f64("x", at.x).is_err() || s.set_builtin_f64("y", at.y).is_err() {
            let _ = s.set_builtin_f64("x", x);
            let _ = s.set_builtin_f64("y", y);
            return;
        }
        let vis = s.builtin_f64("visible").unwrap_or(1.0);
        let visible = (vis < 0.5 && s.set_builtin_f64("visible", 1.0).is_ok()).then_some(vis);
        saved = Some(Saved {
            x,
            y,
            hspeed: s.builtin_f64("hspeed").unwrap_or(0.0),
            vspeed: s.builtin_f64("vspeed").unwrap_or(0.0),
            visible,
            cloaked: None,
        });
    });
    let mut saved = saved?;
    if let (Some(p), Some(v)) = (powers, vars.cloaked) {
        let _ = me.with(&Value::object(p), |pw, _| {
            if let Some(c) = pw.get_value(v)
                && c.as_f64().is_some_and(|x| x != 0.0)
                && pw.set_f64(v, 0.0)
            {
                saved.cloaked = Some(c);
            }
        });
    }
    // In an identity event, a target naming the avatar names the ship.
    let mut target = None;
    if retarget
        && let Some(v) = vars.target
        && me.get_value(v).is_some_and(|t| same(&t, &at.avatar))
        && me.set(v, ship.clone()).is_ok()
    {
        target = Some((at.avatar.clone(), ship.clone()));
    }
    Some(Active {
        key,
        inst,
        object,
        ship,
        powers,
        saved,
        at,
        retarget,
        target,
    })
}

fn after(me: &Instance<'_>) {
    let end = state(|s| s.bracket.end(me.key()));
    match end {
        End::Nothing => {}
        End::Resume => {
            // Stand in again from where the ship is now (what the event
            // at home did to it stays). `me` is the instance whose event
            // went home: the bracket's `target` was left as it was.
            let Some(a) = state(|s| s.bracket.active_mut().cloned()) else {
                return;
            };
            let Some(b) = swap(
                me,
                a.key,
                a.inst.clone(),
                a.object,
                a.ship.clone(),
                a.powers,
                a.at.clone(),
                false,
            ) else {
                // Could not stand in again: the bracket ends here (the
                // ship is home), and its target goes back to the avatar.
                restore_target(me, &a);
                state(|s| s.bracket.take_stale());
                OPEN.store(false, Ordering::Relaxed);
                return;
            };
            state(|s| {
                if let Some(x) = s.bracket.active_mut() {
                    // The target written at the first swap stays recorded.
                    *x = Active {
                        target: x.target.take(),
                        retarget: x.retarget,
                        ..b
                    };
                }
            });
        }
        End::Close(a) => {
            OPEN.store(false, Ordering::Relaxed);
            let moved = restore(me, &a, true);
            let _ = moved;
        }
    }
}

/// The bracketed instance's `target` as it is now (by its id).
fn target_now(me: &Instance<'_>, a: &Active, v: Variable) -> Option<Value> {
    let mut t = None;
    let _ = me.with(&a.inst, |i, _| t = i.get_value(v));
    t
}

/// Put back what [`swap`] changed ([`standin::restore_writes`]), the
/// bracketed instance's `target` too with `with_target` (not while the ship
/// only goes home for a moment): whether the event itself moved the ship or
/// changed its speeds meanwhile.
fn restore(me: &Instance<'_>, a: &Active, with_target: bool) -> bool {
    let vars = VARS.get();
    let tv = vars
        .and_then(|x| x.target)
        .filter(|_| with_target && a.target.is_some());
    let mut now = Now {
        x: None,
        y: None,
        hspeed: None,
        vspeed: None,
        target: tv.and_then(|v| target_now(me, a, v)),
    };
    let _ = me.with(&a.ship, |s, _| {
        now.x = s.builtin_f64("x");
        now.y = s.builtin_f64("y");
        now.hspeed = s.builtin_f64("hspeed");
        now.vspeed = s.builtin_f64("vspeed");
    });
    let w = standin::restore_writes(a, &now, same);
    let _ = me.with(&a.ship, |s, _| {
        let _ = s.set_builtin_f64("x", w.x);
        let _ = s.set_builtin_f64("y", w.y);
        if let Some((h, v)) = w.speeds {
            let _ = s.set_builtin_f64("hspeed", h);
            let _ = s.set_builtin_f64("vspeed", v);
        }
        if let Some(v) = w.visible {
            let _ = s.set_builtin_f64("visible", v);
        }
    });
    if let (Some(c), Some(p), Some(v)) = (w.cloaked, a.powers, vars.and_then(|x| x.cloaked)) {
        let _ = me.with(&Value::object(p), |pw, _| {
            let _ = pw.set(v, c.clone());
        });
    }
    if let (Some(avatar), Some(v)) = (w.target, tv) {
        let _ = me.with(&a.inst, |i, _| {
            let _ = i.set(v, avatar.clone());
        });
    }
    w.moved
}

/// Only the bracketed instance's `target`, back to the avatar if it still
/// names the ship.
fn restore_target(me: &Instance<'_>, a: &Active) {
    let (Some(v), Some((avatar, ship))) = (VARS.get().and_then(|x| x.target), &a.target) else {
        return;
    };
    if target_now(me, a, v).is_some_and(|t| same(&t, ship)) {
        let _ = me.with(&a.inst, |i, _| {
            let _ = i.set(v, avatar.clone());
        });
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (crate::ids::id_number(a), crate::ids::id_number(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

impl WorldHooks {
    /// The host's run step, after the avatars moved (`running`: a running
    /// frame): this frame's plan. Anything else: nothing is bracketed.
    pub(super) fn standin_run_step(&self, call: &Call<'_>, running: bool) {
        if OPEN.load(Ordering::Relaxed)
            && let Some(me) = call.instance()
        {
            close_stale(&me, "at the run step");
        }
        if self.role() != ROLE_HOST || !running {
            off(call);
            return;
        }
        let frame = self.lock().frame;
        if EVENTS.get().is_none()
            && frame.saturating_sub(RESOLVE_TRIED.load(Ordering::Relaxed)) >= RESOLVE_EVERY
        {
            self.standin_objects(call, frame);
        }
        let plan = {
            let st = self.lock();
            st.me.map_or_else(Plan::default, |me| {
                let stand: Vec<(PeerId, StandIn)> = st
                    .partners
                    .iter()
                    .filter_map(|p| {
                        let (avatar, d) = st.ents.target(EntityKey::ship(p.peer))?;
                        Some((
                            p.peer,
                            StandIn {
                                x: f64::from(d.pos.x),
                                y: f64::from(d.pos.y),
                                avatar,
                            },
                        ))
                    })
                    .collect();
                let shots = st
                    .shots
                    .host
                    .live
                    .iter()
                    .filter_map(|(k, s)| s.target.map(|p| (*k, p)));
                Plan::new(me, st.local.id.clone(), stand, st.host.assignments(), shots)
            })
        };
        let on = !plan.is_empty();
        state(|s| s.plan = plan);
        ON.store(on, Ordering::Relaxed);
    }

    /// The bracketable events ([`EVENTS`]), `oPlayer` and `oPowers`, once
    /// every name of the tables resolves (else tried again later, with a
    /// warning: nothing is bracketed meanwhile).
    fn standin_objects(&self, call: &Call<'_>, frame: u64) {
        let names: Vec<(Option<u32>, &str, standin::Need, standin::Kind)> = standin::BRACKETED
            .iter()
            .map(|(n, e, need, kind)| (self.asset(call, n), *e, *need, *kind))
            .collect();
        let writers: Vec<Option<u32>> = standin::NOT_BRACKETED
            .iter()
            .map(|(n, _)| self.asset(call, n))
            .collect();
        let player = self.asset(call, "oPlayer");
        let missing: Vec<&str> = standin::BRACKETED
            .iter()
            .zip(&names)
            .filter(|(_, x)| x.0.is_none())
            .map(|(b, _)| b.0)
            .chain(
                standin::NOT_BRACKETED
                    .iter()
                    .zip(&writers)
                    .filter(|(_, w)| w.is_none())
                    .map(|(b, _)| b.0),
            )
            .chain(player.is_none().then_some("oPlayer"))
            .collect();
        if !missing.is_empty() {
            RESOLVE_TRIED.store(frame.max(1), Ordering::Relaxed);
            log::warn!(
                "co-op stand-in: objects not found {missing:?}: nothing is bracketed (tried again in 10 s)"
            );
            return;
        }
        let wanted: Vec<(u32, &str, standin::Need, standin::Kind)> = names
            .iter()
            .filter_map(|(o, e, n, k)| Some(((*o)?, *e, *n, *k)))
            .collect();
        let writers: Vec<u32> = writers.into_iter().flatten().collect();
        let events = standin::bracketable(
            (0..OBJECT_SCAN).filter(|&i| self.object_known(call, i)),
            |i, r| i == r || self.ancestor(call, i, Some(r)),
            &wanted,
            &writers,
        );
        let mut bits = vec![0_u64; OBJECT_SCAN.div_ceil(64) as usize];
        for &o in events.keys() {
            if let Ok(i) = usize::try_from(o)
                && let Some(w) = bits.get_mut(i / 64)
            {
                *w |= 1 << (i % 64);
            }
        }
        let powers = self.asset(call, "oPowers");
        log::info!(
            "co-op stand-in: {} objects with bracketed events (from the {} named, less the {} that write to a player's thing), oPowers {}",
            events.len(),
            standin::BRACKETED.len(),
            standin::NOT_BRACKETED.len(),
            if powers.is_some() {
                "found"
            } else {
                "missing (cloak not swapped)"
            }
        );
        PLAYER.store(
            player.and_then(|p| i32::try_from(p).ok()).unwrap_or(-1),
            Ordering::Relaxed,
        );
        let _ = EVENTS.set((events, bits));
        state(|s| s.powers = powers);
    }

    /// The id the `chooseTarget` after-hook writes for the instance at
    /// `key`: the ship inside an identity bracket for it (recorded, so the
    /// bracket's end puts `avatar` back), else `avatar`.
    pub(super) fn standin_target(key: usize, avatar: Value) -> Value {
        if !OPEN.load(Ordering::Relaxed) {
            return avatar;
        }
        state(|s| {
            if s.bracket.is_home() {
                return avatar;
            }
            let Some(a) = s.bracket.open_for_mut(key).filter(|a| a.retarget) else {
                return avatar;
            };
            a.target = Some((avatar, a.ship.clone()));
            a.ship.clone()
        })
    }

    /// The `chooseTarget` after-hook left the game's own choice for the
    /// instance at `key` (now the host's): a bracket open for it keeps
    /// that choice at its end.
    pub(super) fn standin_target_kept(key: usize) {
        if !OPEN.load(Ordering::Relaxed) {
            return;
        }
        state(|s| {
            if let Some(a) = s.bracket.open_for_mut(key) {
                a.target = None;
            }
        });
    }
}
