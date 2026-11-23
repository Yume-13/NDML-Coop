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

//! The game side of the shared Singularity ([`crate::singularity`]).
//!
//! * Around `oBlackHole_Step_0`: tracks the current entry's drain and detects this ship's own
//!   catch (`playerCaught` set, control lost).
//! * After `oController_Step_0`: role, messages in, catches out.
//! * After `oPlayer_Step_0` (not run while frozen, so a catch waits for the resume): applies a
//!   remote catch as the game's own does, drains the share, closes the hole nearest the sender's.
//!
//! Nothing acts without a linked co-op run.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ndml_net::msg::{Message, PeerId, SingularityEntry};
use ndml_sdk::{Builtin, Call, Error, Instance, ModContext, Script, Value, Variable};

use crate::Shared;
use crate::singularity::{self, Out, Role, Singularity};

/// `oBlackHole`'s entering code.
const HOLE_STEP: &str = "gml_Object_oBlackHole_Step_0";
const PLAYER_STEP: &str = "gml_Object_oPlayer_Step_0";
const CONTROLLER_STEP: &str = "gml_Object_oController_Step_0";

/// Where the game parks a caught ship.
const PARKED: f64 = -2000.0;
/// `alarm[3]` of a caught ship: the fly-in two seconds later.
const FLY_IN_STEPS: f64 = 120.0;
/// The entering timer's full value (`oPowers.blackHoleTimer`).
const TIMER_FULL: f64 = 90.0;

#[derive(Debug, Clone, Copy)]
struct Handles {
    hull: Variable,
    shield: Variable,
    control: Variable,
    timer: Variable,
    caught: Variable,
    fade: Variable,
    in_singularity: Variable,
    darkness: Variable,
    strafing: Variable,
    strafing_count: Variable,
    release_shoot: Script,
    recenter_swords: Script,
    alarm_set: Builtin,
    asset: Builtin,
}

#[derive(Debug, Default)]
struct Objects {
    powers: Option<u32>,
    controller: Option<u32>,
    hole: Option<u32>,
}

/// Messages routed here by the session pump (lib.rs).
static INBOX: Mutex<Vec<(PeerId, Message)>> = Mutex::new(Vec::new());
/// Own catches seen in the hole's step.
static OWN: Mutex<Vec<OwnCatch>> = Mutex::new(Vec::new());
/// (when, hole x, hole y, hull share, shield share).
type OwnCatch = (Instant, f32, f32, f64, f64);
static STATE: Mutex<Option<Singularity>> = Mutex::new(None);
static OBJECTS: Mutex<Objects> = Mutex::new(Objects {
    powers: None,
    controller: None,
    hole: None,
});
/// The drain's shares over the current entry (hull, shield).
static DRAIN: Mutex<(f64, f64)> = Mutex::new((1.0, 1.0));
/// 0 off, 1 host, 2 client (set by the session step).
static ROLE: AtomicU8 = AtomicU8::new(0);
static DOWN: AtomicBool = AtomicBool::new(false);

fn role() -> Role {
    match ROLE.load(Ordering::Relaxed) {
        1 => Role::Host,
        2 => Role::Client,
        _ => Role::Off,
    }
}

/// Queue a session message for the next session step.
pub(crate) fn route(from: PeerId, msg: Message) {
    let mut q = INBOX.lock().unwrap_or_else(PoisonError::into_inner);
    if q.len() < 64 {
        q.push((from, msg));
    }
}

fn state<R>(f: impl FnOnce(&mut Singularity) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    f(g.get_or_insert_with(Singularity::default))
}

/// The object index of `name` (cached; `None` if the game has none).
fn object(
    call: &Call<'_>,
    h: &Handles,
    name: &str,
    slot: fn(&mut Objects) -> &mut Option<u32>,
) -> Option<u32> {
    if let Some(o) = *slot(&mut OBJECTS.lock().unwrap_or_else(PoisonError::into_inner)) {
        return Some(o);
    }
    let found = call
        .call_builtin(h.asset, &[Value::from(name)])
        .ok()
        .as_ref()
        .and_then(crate::world::classify::object_index);
    *slot(&mut OBJECTS.lock().unwrap_or_else(PoisonError::into_inner)) = found;
    found
}

/// `oPowers`' hull, shield, control and entering timer.
fn powers_read(call: &Call<'_>, h: &Handles) -> Option<(f64, f64, bool, f64)> {
    let powers = object(call, h, "oPowers", |o| &mut o.powers)?;
    let mut out = None;
    let _ = call.with(&Value::object(powers), |inst, _| {
        out = Some((
            inst.get_f64(h.hull).unwrap_or(0.0),
            inst.get_f64(h.shield).unwrap_or(0.0),
            inst.get_value(h.control)
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            inst.get_f64(h.timer).unwrap_or(TIMER_FULL),
        ));
    });
    out
}

/// What the hole's step is compared against.
#[derive(Debug, Clone, Copy)]
struct Before {
    caught: bool,
    control: bool,
    timer: f64,
}

fn hole_before(call: &Call<'_>, h: &Handles) -> Option<Before> {
    if role() == Role::Off {
        return None;
    }
    let me = call.instance()?;
    let (_, _, control, timer) = powers_read(call, h)?;
    Some(Before {
        caught: truthy(&me, h.caught),
        control,
        timer,
    })
}

fn hole_after(call: &Call<'_>, h: &Handles, before: Option<Before>) {
    let Some(b) = before else { return };
    let Some(me) = call.instance() else { return };
    let Some((_, _, control, timer)) = powers_read(call, h) else {
        return;
    };
    if DOWN.load(Ordering::Relaxed) && timer < b.timer {
        // A downed ship has no control, so no drain, but the timer would still
        // run down; it stays full so the next entry is not an instant catch.
        if let Some(powers) = object(call, h, "oPowers", |o| &mut o.powers) {
            let _ = call.with(&Value::object(powers), |inst, _| {
                let _ = inst.set_f64(h.timer, TIMER_FULL);
            });
        }
        return;
    }
    let (sh, ss) = {
        let mut d = DRAIN.lock().unwrap_or_else(PoisonError::into_inner);
        if b.timer >= TIMER_FULL - 1e-6 {
            // A new entry: the timer is full until the first drained step.
            *d = (1.0, 1.0);
        }
        if b.control {
            let f = singularity::step_factor(b.timer, timer);
            d.0 *= f;
            d.1 *= f;
        }
        *d
    };
    if !b.caught && truthy(&me, h.caught) && b.control && !control {
        let (x, y) = (
            me.builtin_f64("x").unwrap_or(0.0) as f32,
            me.builtin_f64("y").unwrap_or(0.0) as f32,
        );
        log::info!(
            "co-op singularity: this game's ship was caught at ({x:.0}, {y:.0}); its drain left hull {sh:.3}, shield {ss:.3}"
        );
        OWN.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((Instant::now(), x, y, sh, ss));
    }
}

fn truthy(me: &Instance<'_>, v: Variable) -> bool {
    me.get_value(v).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Once a frame: role, messages, catches out.
fn session_step(s: &Shared) {
    let now = Instant::now();
    let inbox = std::mem::take(&mut *INBOX.lock().unwrap_or_else(PoisonError::into_inner));
    let own = std::mem::take(&mut *OWN.lock().unwrap_or_else(PoisonError::into_inner));
    let c = s.lock();
    let role = if !c.rules.active() {
        Role::Off
    } else if c.core.is_host() {
        Role::Host
    } else {
        Role::Client
    };
    let was = ROLE.swap(
        match role {
            Role::Off => 0,
            Role::Host => 1,
            Role::Client => 2,
        },
        Ordering::Relaxed,
    );
    DOWN.store(c.rules.is_down(), Ordering::Relaxed);
    if role == Role::Off {
        if was != 0 {
            state(Singularity::reset);
        }
        return;
    }
    let me = c.core.me();
    let local_ms = |at: Instant| ndml_net::time::to_wire_ms(c.net.local_time(at));
    for (at, x, y, hull, shield) in own {
        let entry = SingularityEntry {
            by: me,
            x,
            y,
            hull: hull as f32,
            shield: shield as f32,
            time_ms: local_ms(at),
        };
        state(|st| st.own_catch(role, entry, now));
    }
    for (from, msg) in inbox {
        let sent = match &msg {
            Message::SingularityEntered(e) | Message::SingularityCatch(e) => e.time_ms,
            _ => continue,
        };
        // Its age by the sender's clock, as far as it is known here.
        let age = c
            .net
            .peer_info(from)
            .filter(|i| i.clock.is_synced())
            .map(|i| {
                let remote =
                    ndml_net::time::to_wire_ms(i.clock.arrival_time(c.net.local_time(now)));
                remote.saturating_sub(sent)
            });
        log::info!("co-op singularity: {msg:?} from {from:?}, {age:?} ms old");
        // The host relays a joining game's catch stamped in its own clock,
        // which the other joining games measure its age against.
        let msg = match msg {
            Message::SingularityEntered(e) if role == Role::Host => {
                Message::SingularityEntered(SingularityEntry {
                    time_ms: local_ms(now).wrapping_sub(age.unwrap_or(0)),
                    ..e
                })
            }
            other => other,
        };
        state(|st| st.on_message(role, me, from, &msg, age, now));
    }
    for out in state(Singularity::take_outs) {
        match out {
            Out::ToHost(m) => {
                log::info!("co-op singularity: to the host: {m:?}");
                c.net.send(PeerId::HOST, m);
            }
            Out::Broadcast(m) => {
                log::info!("co-op singularity: to every joining game: {m:?}");
                c.net.broadcast(m, None);
            }
        }
    }
}

/// At the ship's step: a catch from the other side.
fn player_step(call: &Call<'_>, h: &Handles) {
    if role() == Role::Off || !state(|st| st.has_pending()) {
        return;
    }
    // Control is read at the step: this frame's own hole may have just
    // caught the ship.
    let in_control = powers_read(call, h).is_none_or(|(_, _, control, _)| control);
    let down = DOWN.load(Ordering::Relaxed);
    let pending = state(|st| st.take_pending(down, in_control, Instant::now()));
    let Some(ship) = call.instance() else { return };
    for p in pending {
        let e = p.entry;
        if p.catch_ship {
            catch_ship(call, h, &ship, e);
        } else {
            log::info!(
                "co-op singularity: {:?} entered; this game's ship is {} (not caught)",
                e.by,
                if down {
                    "down"
                } else if in_control {
                    "caught already (its own catch in the last 4 s)"
                } else {
                    "out of control (caught already, or parked or flying in)"
                }
            );
        }
        close_hole(call, h, e);
    }
}

fn catch_ship(call: &Call<'_>, h: &Handles, ship: &Instance<'_>, e: SingularityEntry) {
    let powers = object(call, h, "oPowers", |o| &mut o.powers);
    // An own entry under way (timer below full): its drain so far counts
    // towards the sender's share.
    let own_so_far = match powers_read(call, h) {
        Some((_, _, _, timer)) if timer < TIMER_FULL - 1e-6 => {
            DRAIN.lock().unwrap_or_else(PoisonError::into_inner).0
        }
        _ => 1.0,
    };
    let (hull_share, shield_share) = (
        singularity::still_to_take(f64::from(e.hull), own_so_far),
        singularity::still_to_take(f64::from(e.shield), own_so_far),
    );
    let mut strafing = false;
    if let Some(p) = powers {
        let _ = call.with(&Value::object(p), |inst, _| {
            strafing = truthy(&inst, h.strafing);
        });
    }
    // The game's own catch, in its order.
    let _ = ship.set(h.strafing_count, 0.0);
    let released = ship
        .call_script(h.release_shoot, &[Value::Bool(!strafing)])
        .is_ok();
    let alarm = ship
        .call_builtin(h.alarm_set, &[3.0.into(), FLY_IN_STEPS.into()])
        .is_ok();
    let _ = ship.set_builtin_f64("speed", 0.0);
    let (mut hull, mut shield) = (None, None);
    if let Some(p) = powers {
        let _ = call.with(&Value::object(p), |inst, _| {
            let _ = inst.set(h.control, false);
            let _ = inst.set(h.in_singularity, true);
            if let Some(v) = inst.get_f64(h.hull) {
                let n = singularity::drained(v, hull_share);
                let _ = inst.set_f64(h.hull, n);
                hull = Some((v, n));
            }
            if let Some(v) = inst.get_f64(h.shield) {
                let n = singularity::drained(v, shield_share);
                let _ = inst.set_f64(h.shield, n);
                shield = Some((v, n));
            }
        });
    }
    let _ = ship.set_builtin_f64("x", PARKED);
    let _ = ship.set_builtin_f64("y", PARKED);
    let swords = ship
        .call_script(h.recenter_swords, &[Value::Undefined])
        .is_ok();
    if let Some(ctl) = object(call, h, "oController", |o| &mut o.controller) {
        let _ = call.with(&Value::object(ctl), |inst, _| {
            let d = inst.get_f64(h.darkness).unwrap_or(0.0);
            if d < 1.0 {
                let _ = inst.set_f64(h.darkness, 1.0);
            }
        });
    }
    log::info!(
        "co-op singularity: {:?} entered; this game's ship caught too (own drain so far {own_so_far:.3}; hull {hull:?}, shield {shield:?}; releaseShoot {released}, alarm[3] {alarm}, swords {swords})",
        e.by
    );
}

/// Close the open hole nearest the sender's, as the game's catch does.
fn close_hole(call: &Call<'_>, h: &Handles, e: SingularityEntry) {
    let Some(hole) = object(call, h, "oBlackHole", |o| &mut o.hole) else {
        return;
    };
    let mut best: Option<(f64, usize)> = None;
    let mut i = 0usize;
    let _ = call.with(&Value::object(hole), |inst, _| {
        if !truthy(&inst, h.fade) {
            let dx = inst.builtin_f64("x").unwrap_or(0.0) - f64::from(e.x);
            let dy = inst.builtin_f64("y").unwrap_or(0.0) - f64::from(e.y);
            let d = dx * dx + dy * dy;
            if best.is_none_or(|b| d < b.0) {
                best = Some((d, i));
            }
        }
        i += 1;
    });
    let Some((d, pick)) = best else {
        log::info!("co-op singularity: no open hole here to close");
        return;
    };
    let mut i = 0usize;
    let _ = call.with(&Value::object(hole), |inst, _| {
        if i == pick {
            let _ = inst.set(h.caught, true);
            let _ = inst.set(h.fade, true);
        }
        i += 1;
    });
    log::info!(
        "co-op singularity: the hole {:.0} px from the sender's closed",
        d.sqrt()
    );
}

/// Install.
pub(crate) fn register(ctx: &ModContext, s: &Arc<Shared>) {
    match install(ctx, s) {
        Ok(()) => log::info!(
            "co-op singularity: either player entering catches both (hooks on the hole's step, the ship's step)"
        ),
        Err(e) => log::warn!("co-op singularity: {e}; each game's Singularity is its own"),
    }
}

fn install(ctx: &ModContext, s: &Arc<Shared>) -> Result<(), Error> {
    let h = Handles {
        hull: ctx.variable("hull")?,
        shield: ctx.variable("shield")?,
        control: ctx.variable("playerControl")?,
        timer: ctx.variable("blackHoleTimer")?,
        caught: ctx.variable("playerCaught")?,
        fade: ctx.variable("fade")?,
        in_singularity: ctx.variable("inSingularity")?,
        darkness: ctx.variable("darknessMyOldFriend")?,
        strafing: ctx.variable("strafingStrikes")?,
        strafing_count: ctx.variable("strafingStrikesCount")?,
        release_shoot: ctx.script("gml_Script_releaseShoot")?,
        recenter_swords: ctx.script("gml_Script_recenterAllSwordsOnWielder")?,
        alarm_set: ctx.builtin("alarm_set")?,
        asset: ctx.builtin("asset_get_index")?,
    };
    ctx.hook(
        HOLE_STEP,
        move |call| hole_before(call, &h),
        move |call, before| hole_after(call, &h, before),
    )?;
    ctx.hook_after(PLAYER_STEP, move |call| player_step(call, &h))?;
    let s = Arc::clone(s);
    ctx.hook_after(CONTROLLER_STEP, move |_| session_step(&s))?;
    Ok(())
}
