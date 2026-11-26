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

//! Generic state transfer: read an instance's whole state with the game's own
//! reflection builtins, with no knowledge of its object, and write it onto
//! another instance. Variables are read deep (instances as net ids; arrays,
//! structs and methods element by element up to [`MAX_DEPTH`] and
//! [`MAX_LEN`]), plus the built-ins in [`BUILTINS`] and the alarms; writing
//! rebuilds arrays, structs and methods in the game.

#![forbid(unsafe_code)]
// `r`, `v`, `k` are the reflection handles, a value and its kind throughout.
#![allow(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]

use std::collections::HashMap;

use ndml_net::msg::{NetId, StateValue};
use ndml_sdk::abi::OBJECT_REF_TAG;
use ndml_sdk::abi::kind;
use ndml_sdk::{Arg, Builtin, Call, Held, Instance, ModContext, Value};

/// Arrays and structs deeper than this are not transferred.
pub const MAX_DEPTH: usize = 4;
/// Arrays longer (or structs with more members) than this are not transferred.
pub const MAX_LEN: usize = 256;
/// `alarm[0..ALARMS]`.
pub const ALARMS: u8 = 12;

/// Built-in instance variables transferred (index = wire index).
pub const BUILTINS: &[&str] = &[
    "sprite_index",
    "mask_index",
    "image_index",
    "image_speed",
    "image_xscale",
    "image_yscale",
    "image_angle",
    "image_alpha",
    "image_blend",
    "x",
    "y",
    "xstart",
    "ystart",
    "xprevious",
    "yprevious",
    "hspeed",
    "vspeed",
    "speed",
    "direction",
    "friction",
    "gravity",
    "gravity_direction",
    "depth",
    "visible",
    "solid",
];

/// Ref payload tag of an instance by id: `0x0400_0001_<id>`.
pub const INSTANCE_REF_TAG: u64 = 0x0400_0001_0000_0000;
const REF_TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;

/// A game script whose body is empty (an enum declaration file): a copy's
/// method bound to an instance not here is rebuilt on it, so it does nothing
/// (`build`).
pub const NOOP_SCRIPT: &str = "enumBodyTypes";

/// Pseudo built-in index on the wire (never in [`BUILTINS`]): how the host's
/// instance was born, in a first state. [`ORIGIN_COPY`]: by `instance_copy`,
/// so the copy's set-up step must not run again.
pub const ORIGIN_BUILTIN: u8 = 255;
/// [`ORIGIN_BUILTIN`]: made by `instance_copy`.
pub const ORIGIN_COPY: i32 = 1;
/// Pseudo built-in index on the wire (never in [`BUILTINS`]): the instance
/// that made this one when that is not an enemy but is replicated as a
/// dependency (the Star Eater's controller). Only in a first state; the
/// receiver makes the maker first and writes nothing.
pub const MAKER_BUILTIN: u8 = 254;

/// Pseudo built-in index on the wire: the host's instance's age (frames since
/// first seen) when a repair was read; the receiver applies the repair when
/// its copy is that old. Only in repairs.
pub const AGE_BUILTIN: u8 = 253;

/// Pseudo built-in index on the wire: another instance ran this one's user
/// event 1 before its first state (a cargo engine on each car, which makes
/// the car's trails); the receiver runs the copy's user event 1 after set-up.
pub const MAKER_EVENT_BUILTIN: u8 = 252;

/// Whether a first state says another instance ran the host's instance's
/// user event 1 before it ([`MAKER_EVENT_BUILTIN`]).
#[must_use]
pub fn maker_ran_event1(c: &Captured) -> bool {
    c.builtins.iter().any(|(i, _)| *i == MAKER_EVENT_BUILTIN)
}

/// The age a repair was read at ([`AGE_BUILTIN`]), if it says.
#[must_use]
pub fn stamped_age(c: &Captured) -> Option<u64> {
    c.builtins.iter().find_map(|(i, v)| {
        if *i != AGE_BUILTIN {
            return None;
        }
        match v.clone().expanded() {
            StateValue::Real(n) if n >= 0.0 => Some(n as u64),
            StateValue::Int(n) => u64::try_from(n).ok(),
            _ => None,
        }
    })
}

/// How many variables instance `id` holds (`variable_instance_names_count`);
/// `None` if the game lacks the builtin or the call fails. Variables are
/// never removed from an instance, so a new count means new names.
#[must_use]
pub fn names_count(call: &Call<'_>, r: &Reflect, id: &Value) -> Option<usize> {
    let b = r.names_count?;
    let n = call
        .call_builtin(b, std::slice::from_ref(id))
        .ok()?
        .as_f64()?;
    (n >= 0.0).then_some(n as usize)
}

/// Whether a ref payload names an instance by id.
#[must_use]
pub fn is_instance_ref(p: u64) -> bool {
    p & REF_TAG_MASK == INSTANCE_REF_TAG
}

/// Whether a ref payload is a handle each game makes for itself at run time
/// (a layer, `0x0800_0006`): not an asset (`0x01..`) and not an instance
/// (`0x04..`). The copy keeps its own.
#[must_use]
pub fn is_local_handle(p: u64) -> bool {
    let class = p >> 56;
    class != 0x01 && class != 0x04
}

/// Instance variables holding a handle the game makes at run time (ds lists
/// and maps, surfaces, vertex buffers, paths, layers), kept as a number or a
/// ref: each game's own, so never written onto a copy. Every variable assigned
/// the result of a handle-making builtin; sorted (binary search).
pub const GAME_HANDLE_VARS: &[&str] = &[
    "activeCollisions",
    "backgroundCoils",
    "backgroundLayerCoils",
    "backgroundLayerFar",
    "backgroundLayerMid",
    "backgroundLayerNear",
    "backgroundStarsFar",
    "backgroundStarsMid",
    "backgroundStarsNear",
    "banishMods",
    "deathSoundPool",
    "explosion_effects",
    "gearBodies",
    "gearShields",
    "gearWeapons",
    "hitList",
    "looping_effects_list",
    "mods",
    "modsList",
    "negativeModList",
    "neutralModList",
    "parameters_map",
    "playerMods",
    "positiveModList",
    "previousMods",
    "ship_path",
    "small_center_effects",
    "stringScaleCache",
    "unlockList",
    "wave_surf",
    "wave_vb_inside",
    "wave_vb_outside",
    "wildMods",
    "wildModsRare",
    "wildModsVeryRare",
];

/// Whether `name` holds a handle each game makes for itself
/// ([`GAME_HANDLE_VARS`]).
#[must_use]
pub fn is_game_handle_var(name: &str) -> bool {
    GAME_HANDLE_VARS.binary_search(&name).is_ok()
}

/// Whether a first state says the host's instance was made by
/// `instance_copy` ([`ORIGIN_BUILTIN`]).
#[must_use]
pub fn born_by_copy(c: &Captured) -> bool {
    c.builtins.iter().any(|(i, v)| {
        *i == ORIGIN_BUILTIN
            && matches!(v.clone().expanded(), StateValue::Real(n) if (n - f64::from(ORIGIN_COPY)).abs() < 0.5)
    })
}

/// The reflection builtins (resolved by name at init).
#[derive(Debug, Clone, Copy)]
pub struct Reflect {
    names: Builtin,
    get: Builtin,
    set: Builtin,
    array_length: Builtin,
    array_get: Builtin,
    array_create: Builtin,
    array_set: Builtin,
    is_method: Builtin,
    method_get_index: Builtin,
    method_get_self: Builtin,
    method: Builtin,
    struct_names: Builtin,
    struct_get: Builtin,
    struct_set: Builtin,
    json_parse: Builtin,
    exists: Builtin,
    instance_find: Builtin,
    instance_number: Builtin,
    alarm_get: Builtin,
    alarm_set: Builtin,
    asset_get_index: Builtin,
    /// `variable_instance_names_count` (the host's per-frame change test;
    /// optional: without it repairs are periodic only).
    names_count: Option<Builtin>,
}

impl Reflect {
    /// `None` if the game lacks one of them (logged by the caller).
    #[must_use]
    pub fn resolve(ctx: &ModContext) -> Option<Self> {
        let b = |n: &str| ctx.builtin(n).ok();
        Some(Self {
            names: b("variable_instance_get_names")?,
            get: b("variable_instance_get")?,
            set: b("variable_instance_set")?,
            array_length: b("array_length")?,
            array_get: b("array_get")?,
            array_create: b("array_create")?,
            array_set: b("array_set")?,
            is_method: b("is_method")?,
            method_get_index: b("method_get_index")?,
            method_get_self: b("method_get_self")?,
            method: b("method")?,
            struct_names: b("variable_struct_get_names")?,
            struct_get: b("variable_struct_get")?,
            struct_set: b("variable_struct_set")?,
            json_parse: b("json_parse")?,
            exists: b("instance_exists")?,
            instance_find: b("instance_find")?,
            instance_number: b("instance_number")?,
            alarm_get: b("alarm_get")?,
            alarm_set: b("alarm_set")?,
            asset_get_index: b("asset_get_index")?,
            names_count: b("variable_instance_names_count"),
        })
    }
}

/// How an instance reference is sent: the instance's key and id in, its
/// net id out (if it is replicated).
pub type NetOf<'f> = &'f dyn Fn(usize, &Value) -> Option<NetId>;
/// How a net id is found on the receiver: the copy's instance id.
pub type IdOf<'f> = &'f dyn Fn(NetId) -> Option<Value>;

/// One instance's state, as read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Captured {
    /// Instance variables by name.
    pub vars: Vec<(String, StateValue)>,
    /// Built-ins by index into [`BUILTINS`], or a pseudo index.
    pub builtins: Vec<(u8, StateValue)>,
    /// Alarms by index.
    pub alarms: Vec<(u8, i32)>,
}

/// Counts from reading or writing a state.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    /// Variables written.
    pub written: u32,
    /// Variables that failed to write.
    pub failed: u32,
    /// Instance refs whose copy does not exist yet: (variable, net id).
    pub waiting: Vec<(String, NetId)>,
    /// Instance refs whose copy does not exist yet, held: the variable keeps
    /// what the copy had.
    pub held: u32,
    /// Arrays, structs and methods that did not read back as written:
    /// (variable, kind read).
    pub readback_bad: Vec<(String, u32)>,
    /// Receiver: references not resolved to the instance the sender meant:
    /// (variable, how, the sender's target object, the copy's own instance's
    /// object for `own`). How: `noone`, `own` (kept), `raw`, `method` (no-op).
    pub unresolved_here: Vec<(String, &'static str, u16, u16)>,
    /// Sender: instances referred to that are not replicated (key, object,
    /// id), for the dependency rule.
    pub not_replicated: Vec<(usize, u16, Value)>,
}

/// Receiver, before [`apply`]: a reference the sender sent as the only
/// instance of its object ([`StateValue::Singleton`]) that would resolve
/// here to an instance `shared(key)` names becomes [`StateValue::Unresolved`]
/// (the copy keeps its own, else `noone`). A shared attack or wind-up made
/// here by the shot path is the host's, and the sender's variable cannot
/// mean it. Returns how many were changed.
pub fn singletons_not_shared(
    call: &Call<'_>,
    r: &Reflect,
    vars: &mut [(String, StateValue)],
    shared: &dyn Fn(usize) -> bool,
) -> u32 {
    fn one(
        call: &Call<'_>,
        r: &Reflect,
        v: &mut StateValue,
        shared: &dyn Fn(usize) -> bool,
    ) -> u32 {
        match v {
            StateValue::Singleton(o) => {
                let Ok(found) = call.call_builtin(
                    r.instance_find,
                    &[Value::object(u32::from(*o)), Value::Real(0.0)],
                ) else {
                    return 0;
                };
                let mut key = None;
                let _ = call.with(&found, |inst, _| key = Some(inst.key()));
                if key.is_some_and(shared) {
                    *v = StateValue::Unresolved(*o);
                    1
                } else {
                    0
                }
            }
            StateValue::Array(items) => items.iter_mut().map(|it| one(call, r, it, shared)).sum(),
            _ => 0,
        }
    }
    vars.iter_mut().map(|(_, v)| one(call, r, v, shared)).sum()
}

fn arg<'a>(v: &Value) -> Arg<'_, 'a> {
    Arg::Value(v)
}

/// Whether `v` names an instance (a ref that is not an object or another
/// asset the game knows, or a number in the instance id range) that exists.
fn instance_of(call: &Call<'_>, r: &Reflect, v: &Value) -> bool {
    let candidate = match v {
        Value::Ref(p) => *p & 0xFFFF_FFFF_0000_0000 != OBJECT_REF_TAG,
        Value::Real(n) => *n >= 100_000.0 && n.fract() == 0.0,
        Value::Int(n) => *n >= 100_000,
        _ => false,
    };
    candidate
        && call
            .call_builtin(r.exists, std::slice::from_ref(v))
            .ok()
            .and_then(|x| x.as_bool())
            .unwrap_or(false)
}

/// The variables of a state that refer to an instance the receiver has (a
/// replicated one) or to none (`noone`, GML's -4): the ones the host's change
/// test reads every frame.
#[must_use]
pub fn ref_names(c: &Captured) -> Vec<String> {
    c.vars
        .iter()
        .filter(|(_, v)| match v.clone().expanded() {
            StateValue::Instance(_) => true,
            StateValue::Real(n) => n == NOONE,
            _ => false,
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Whether a watched variable's value is still a reference (an instance or
/// `noone`); anything else says the variable is not one (its `noone` was a
/// number's start value, such as `oStarEater.phaseTimer`).
#[must_use]
pub fn is_ref_value(v: &StateValue) -> bool {
    match v.clone().expanded() {
        // `undefined` says nothing either way (a reference cleared so):
        // still watched.
        StateValue::Instance(_)
        | StateValue::Singleton(_)
        | StateValue::Unresolved(_)
        | StateValue::Undefined => true,
        StateValue::Real(n) => n == NOONE,
        _ => false,
    }
}

/// What `id`'s variables `names` hold now, one number each that changes when
/// the value does (an instance ref by its payload, a number by its bits).
#[must_use]
pub fn ref_signature(call: &Call<'_>, r: &Reflect, id: &Value, names: &[String]) -> Vec<u64> {
    names
        .iter()
        .map(|n| {
            let key = Value::Str(n.clone());
            call.call_builtin_held(r.get, &[arg(id), arg(&key)])
                .ok()
                .map_or(u64::MAX, |h| value_bits(&h.value()))
        })
        .collect()
}

/// A value as one number for [`ref_signature`].
fn value_bits(v: &Value) -> u64 {
    match v {
        Value::Ref(p) | Value::Ptr(p) => *p,
        Value::Real(n) => n.to_bits(),
        Value::Int(n) => u64::from_ne_bytes(n.to_ne_bytes()),
        Value::Bool(b) => u64::from(*b),
        Value::Undefined => u64::MAX - 1,
        Value::Str(_) | Value::Other { .. } => u64::MAX - 2,
    }
}

/// Variable `name` of `id` as the game holds it now.
#[must_use]
pub fn get_var(call: &Call<'_>, r: &Reflect, id: &Value, name: &str) -> Option<Value> {
    call.call_builtin(r.get, &[id.clone(), Value::Str(name.to_owned())])
        .ok()
}

/// Read `id`'s whole state. `net_of` maps an instance (its key) to its net id.
#[expect(
    clippy::cast_precision_loss,
    reason = "variable indices: an instance's variable count, far below 2^53"
)]
pub fn capture(
    call: &Call<'_>,
    r: &Reflect,
    id: &Value,
    net_of: NetOf<'_>,
    stats: &mut Stats,
) -> Captured {
    let mut out = Captured::default();
    let Ok(names) = call.call_builtin_held(r.names, &[arg(id)]) else {
        return out;
    };
    let n = call
        .call_builtin_held(r.array_length, &[Arg::Held(&names)])
        .ok()
        .and_then(|h| h.value().as_f64())
        .unwrap_or(0.0) as usize;
    for i in 0..n {
        let idx = Value::Real(i as f64);
        let name = call
            .call_builtin_held(r.array_get, &[Arg::Held(&names), arg(&idx)])
            .ok()
            .map(|h| h.value());
        let Some(Value::Str(name)) = name else {
            continue;
        };
        let key = Value::Str(name.clone());
        let Ok(v) = call.call_builtin_held(r.get, &[arg(id), arg(&key)]) else {
            continue;
        };
        let value = convert(call, r, &v, 0, net_of, stats);
        out.vars.push((name, value));
    }
    out.vars.sort_by(|a, b| a.0.cmp(&b.0));
    let _ = call.with(id, |inst, _| {
        for (i, name) in BUILTINS.iter().enumerate() {
            if let Ok(v) = inst.builtin_value(name) {
                let sv = plain(&v).unwrap_or(StateValue::Unsupported(0));
                out.builtins.push((i as u8, sv));
            }
        }
        for a in 0..ALARMS {
            let left = inst
                .call_builtin(r.alarm_get, &[f64::from(a).into()])
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(-1.0);
            if left >= 0.0 {
                out.alarms.push((a, left as i32));
            }
        }
    });
    out
}

/// One variable of `id` as a state value (`None` when the read failed).
pub fn read_var(
    call: &Call<'_>,
    r: &Reflect,
    id: &Value,
    name: &str,
    net_of: NetOf<'_>,
    stats: &mut Stats,
) -> Option<StateValue> {
    let key = Value::Str(name.to_owned());
    let v = call.call_builtin_held(r.get, &[arg(id), arg(&key)]).ok()?;
    Some(convert(call, r, &v, 0, net_of, stats))
}

/// A failed builtin call in [`raw_vars`]: which builtin, its argument, the
/// error.
pub type VarsError = (&'static str, String, ndml_sdk::Error);

/// Every variable of `id` with its value as the game holds it (arrays,
/// structs and methods as their handles).
///
/// # Errors
/// The first failing builtin call (with the guard on, a GML error comes back
/// here as `Status::GmlError` instead of ending the game).
#[expect(
    clippy::cast_precision_loss,
    reason = "variable indices below MAX_LEN (256)"
)]
pub fn raw_vars(
    call: &Call<'_>,
    r: &Reflect,
    id: &Value,
) -> Result<Vec<(String, Value)>, VarsError> {
    let mut out = Vec::new();
    let names = call
        .call_builtin_held(r.names, &[arg(id)])
        .map_err(|e| ("variable_instance_get_names", String::new(), e))?;
    let n = call
        .call_builtin_held(r.array_length, &[Arg::Held(&names)])
        .map_err(|e| ("array_length", String::new(), e))?
        .value()
        .as_f64()
        .unwrap_or(0.0) as usize;
    for i in 0..n.min(MAX_LEN) {
        let idx = Value::Real(i as f64);
        let name = call
            .call_builtin_held(r.array_get, &[Arg::Held(&names), arg(&idx)])
            .map_err(|e| ("array_get", i.to_string(), e))?
            .value();
        let Value::Str(name) = name else {
            continue;
        };
        let key = Value::Str(name.clone());
        let v = call
            .call_builtin_held(r.get, &[arg(id), arg(&key)])
            .map_err(|e| ("variable_instance_get", name.clone(), e))?;
        out.push((name, v.value()));
    }
    Ok(out)
}

/// The names of instance `id`'s variables, without their values (for what
/// it gained: a maker's new variables, read singly after).
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "variable indices below MAX_LEN (256)"
)]
pub fn var_names(call: &Call<'_>, r: &Reflect, id: &Value) -> Option<Vec<String>> {
    let names = call.call_builtin_held(r.names, &[arg(id)]).ok()?;
    let n = call
        .call_builtin_held(r.array_length, &[Arg::Held(&names)])
        .ok()?
        .value()
        .as_f64()
        .unwrap_or(0.0) as usize;
    let mut out = Vec::with_capacity(n.min(MAX_LEN));
    for i in 0..n.min(MAX_LEN) {
        let idx = Value::Real(i as f64);
        if let Ok(h) = call.call_builtin_held(r.array_get, &[Arg::Held(&names), arg(&idx)])
            && let Value::Str(name) = h.value()
        {
            out.push(name);
        }
    }
    Some(out)
}

/// A value that needs no game call to convert.
fn plain(v: &Value) -> Option<StateValue> {
    Some(match v {
        Value::Undefined => StateValue::Undefined,
        Value::Real(n) => StateValue::Real(*n),
        Value::Int(n) => StateValue::Int(*n),
        Value::Bool(b) => StateValue::Bool(*b),
        Value::Str(s) => StateValue::Str(s.clone()),
        Value::Ref(p) => StateValue::Ref(*p),
        Value::Ptr(_) => StateValue::Unsupported(kind::PTR as u8),
        Value::Other { .. } => return None,
    })
}

fn object_index_of(call: &Call<'_>, v: &Value) -> (Option<usize>, u16) {
    let mut key = None;
    let mut obj = 0u16;
    let _ = call.with(v, |inst, _| {
        key = Some(inst.key());
        obj = inst
            .builtin_value("object_index")
            .ok()
            .as_ref()
            .and_then(super::classify::object_index)
            .and_then(|o| u16::try_from(o).ok())
            .unwrap_or(0);
    });
    (key, obj)
}

#[expect(
    clippy::cast_precision_loss,
    reason = "array and struct indices below MAX_LEN (256), checked first"
)]
fn convert(
    call: &Call<'_>,
    r: &Reflect,
    held: &Held<'_>,
    depth: usize,
    net_of: NetOf<'_>,
    stats: &mut Stats,
) -> StateValue {
    let k = held.kind();
    match k {
        kind::ARRAY => {
            let n = call
                .call_builtin_held(r.array_length, &[Arg::Held(held)])
                .ok()
                .and_then(|h| h.value().as_f64())
                .unwrap_or(0.0) as usize;
            if depth >= MAX_DEPTH || n > MAX_LEN {
                return StateValue::Unsupported(255);
            }
            let mut items = Vec::with_capacity(n);
            for i in 0..n {
                let idx = Value::Real(i as f64);
                match call.call_builtin_held(r.array_get, &[Arg::Held(held), arg(&idx)]) {
                    Ok(e) => items.push(convert(call, r, &e, depth + 1, net_of, stats)),
                    Err(_) => items.push(StateValue::Undefined),
                }
            }
            StateValue::Array(items)
        }
        kind::OBJECT => {
            // An instance held in a variable is an object too (the runner's
            // instance), not a struct: send it as a reference.
            let is_instance = call
                .call_builtin_held(r.exists, &[Arg::Held(held)])
                .ok()
                .and_then(|h| h.value().as_bool())
                .unwrap_or(false);
            if is_instance {
                let id_key = Value::Str("id".to_owned());
                if let Ok(idv) = call.call_builtin_held(r.get, &[Arg::Held(held), arg(&id_key)]) {
                    let v = idv.value();
                    if !matches!(v, Value::Other { .. } | Value::Undefined) {
                        return instance_value(call, r, &v, net_of, stats);
                    }
                }
                return StateValue::Unresolved(0);
            }
            let is_method = call
                .call_builtin_held(r.is_method, &[Arg::Held(held)])
                .ok()
                .and_then(|h| h.value().as_bool())
                .unwrap_or(false);
            if is_method {
                let func = call
                    .call_builtin_held(r.method_get_index, &[Arg::Held(held)])
                    .ok()
                    .map(|h| h.value());
                let func = match func {
                    Some(Value::Real(n)) => n as i64,
                    Some(Value::Int(n)) => n,
                    Some(Value::Ref(p)) => p as i64,
                    _ => return StateValue::Unsupported(254),
                };
                let bound = match call.call_builtin_held(r.method_get_self, &[Arg::Held(held)]) {
                    Ok(s) if depth < MAX_DEPTH => convert(call, r, &s, depth + 1, net_of, stats),
                    _ => StateValue::Undefined,
                };
                return StateValue::Method {
                    func,
                    bound: Box::new(bound),
                };
            }
            // A deactivated instance is an object too and fails
            // `instance_exists`; its `id` is an instance ref. Sent as
            // unresolved, never as a struct of its variables.
            let id_key = Value::Str("id".to_owned());
            if let Ok(idv) = call.call_builtin_held(r.struct_get, &[Arg::Held(held), arg(&id_key)])
                && matches!(idv.value(), Value::Ref(p) if is_instance_ref(p))
            {
                return StateValue::Unresolved(0);
            }
            if depth >= MAX_DEPTH {
                return StateValue::Unsupported(255);
            }
            let Ok(names) = call.call_builtin_held(r.struct_names, &[Arg::Held(held)]) else {
                return StateValue::Unsupported(kind::OBJECT as u8);
            };
            let n = call
                .call_builtin_held(r.array_length, &[Arg::Held(&names)])
                .ok()
                .and_then(|h| h.value().as_f64())
                .unwrap_or(0.0) as usize;
            if n > MAX_LEN {
                return StateValue::Unsupported(255);
            }
            let mut members = Vec::with_capacity(n);
            for i in 0..n {
                let idx = Value::Real(i as f64);
                let Some(Value::Str(name)) = call
                    .call_builtin_held(r.array_get, &[Arg::Held(&names), arg(&idx)])
                    .ok()
                    .map(|h| h.value())
                else {
                    continue;
                };
                let key = Value::Str(name.clone());
                if let Ok(v) = call.call_builtin_held(r.struct_get, &[Arg::Held(held), arg(&key)]) {
                    members.push((name, convert(call, r, &v, depth + 1, net_of, stats)));
                }
            }
            members.sort_by(|a, b| a.0.cmp(&b.0));
            StateValue::Struct(members)
        }
        _ => {
            let v = held.value();
            if instance_of(call, r, &v) {
                return instance_value(call, r, &v, net_of, stats);
            }
            // An instance ref to an inactive instance (deactivated by a
            // freeze, or gone) names nothing, or another instance, here.
            if let Value::Ref(p) = v
                && is_instance_ref(p)
            {
                return StateValue::Unresolved(0);
            }
            match plain(&v) {
                Some(s) => s,
                None => StateValue::Unsupported(k as u8),
            }
        }
    }
}

/// An instance reference: its net id, the only instance of its object, or
/// unresolved (a local instance of this machine).
fn instance_value(
    call: &Call<'_>,
    r: &Reflect,
    v: &Value,
    net_of: NetOf<'_>,
    stats: &mut Stats,
) -> StateValue {
    let (key, obj) = object_index_of(call, v);
    if let Some(net) = key.and_then(|k| net_of(k, v)) {
        return StateValue::Instance(net);
    }
    if let Some(k) = key
        && !stats.not_replicated.iter().any(|d| d.0 == k)
    {
        stats.not_replicated.push((k, obj, v.clone()));
    }
    let single = call
        .call_builtin(r.instance_number, &[Value::object(u32::from(obj))])
        .ok()
        .and_then(|x| x.as_f64())
        == Some(1.0);
    if single {
        return StateValue::Singleton(obj);
    }
    StateValue::Unresolved(obj)
}

/// A value ready to write: plain, or built in the game.
enum Built<'c> {
    Plain(Value),
    Held(Held<'c>),
}

impl<'c> Built<'c> {
    fn arg(&self) -> Arg<'_, 'c> {
        match self {
            Built::Plain(v) => Arg::Value(v),
            Built::Held(h) => Arg::Held(h),
        }
    }
}

const NOONE: f64 = -4.0;

#[expect(
    clippy::cast_precision_loss,
    reason = "lengths and indices of a received array, far below 2^53"
)]
fn build<'c>(
    call: &'c Call<'_>,
    r: &Reflect,
    v: &StateValue,
    id_of: IdOf<'_>,
    var: &str,
    stats: &mut Stats,
) -> Option<Built<'c>> {
    Some(match v {
        StateValue::Undefined => Built::Plain(Value::Undefined),
        StateValue::Real(n) => Built::Plain(Value::Real(*n)),
        StateValue::Whole(n) => Built::Plain(Value::Real(f64::from(*n))),
        StateValue::Real32(n) => Built::Plain(Value::Real(f64::from(*n))),
        StateValue::Int(n) => Built::Plain(Value::Int(*n)),
        StateValue::Bool(b) => Built::Plain(Value::Bool(*b)),
        StateValue::Str(s) => Built::Plain(Value::Str(s.clone())),
        StateValue::Ref(p) if is_instance_ref(*p) => {
            // A raw instance ref of the sender: it names nothing here, or
            // another instance.
            stats.unresolved_here.push((var.to_owned(), "raw", 0, 0));
            return None;
        }
        StateValue::Ref(p) => Built::Plain(Value::Ref(*p)),
        StateValue::Instance(net) => {
            if let Some(id) = id_of(*net) {
                Built::Plain(id)
            } else {
                stats.waiting.push((var.to_owned(), *net));
                Built::Plain(Value::Real(NOONE))
            }
        }
        StateValue::Singleton(obj) => Built::Plain(
            call.call_builtin(
                r.instance_find,
                &[Value::object(u32::from(*obj)), Value::Real(0.0)],
            )
            .unwrap_or(Value::Real(NOONE)),
        ),
        StateValue::Array(items) => {
            let n = Value::Real(items.len() as f64);
            let arr = call.call_builtin_held(r.array_create, &[arg(&n)]).ok()?;
            for (i, it) in items.iter().enumerate() {
                let Some(b) = build(call, r, it, id_of, var, stats) else {
                    continue;
                };
                let idx = Value::Real(i as f64);
                let _ = call.call_builtin_held(r.array_set, &[Arg::Held(&arr), arg(&idx), b.arg()]);
            }
            Built::Held(arr)
        }
        StateValue::Struct(members) => {
            let empty = Value::Str("{}".to_owned());
            let st = call.call_builtin_held(r.json_parse, &[arg(&empty)]).ok()?;
            for (k, m) in members {
                let Some(b) = build(call, r, m, id_of, var, stats) else {
                    continue;
                };
                let key = Value::Str(k.clone());
                let _ = call.call_builtin_held(r.struct_set, &[Arg::Held(&st), arg(&key), b.arg()]);
            }
            Built::Held(st)
        }
        StateValue::Method { func, bound } => {
            // Bound to an instance that is not here: on the sender it runs
            // with that instance as `self`, and here any other `self` is
            // wrong, so it does nothing until a state binds it again.
            let missing = match bound.as_ref() {
                StateValue::Unresolved(o) => Some(*o),
                StateValue::Ref(p) if is_instance_ref(*p) => Some(0),
                StateValue::Instance(net) if id_of(*net).is_none() => Some(0),
                _ => None,
            };
            if let Some(o) = missing {
                stats.unresolved_here.push((var.to_owned(), "method", o, 0));
                let name = Value::Str(NOOP_SCRIPT.to_owned());
                let noop = call.call_builtin(r.asset_get_index, &[name]).ok()?;
                let m = call
                    .call_builtin_held(r.method, &[arg(&Value::Undefined), arg(&noop)])
                    .ok()?;
                return Some(Built::Held(m));
            }
            let b = build(call, r, bound, id_of, var, stats)?;
            let f = method_func(*func);
            let m = call.call_builtin_held(r.method, &[b.arg(), arg(&f)]).ok()?;
            Built::Held(m)
        }
        // Not transferred; a local instance of the sender inside an array or
        // struct (top-level ones are handled in `apply`).
        StateValue::Unsupported(_) | StateValue::Unresolved(_) => return None,
    })
}

/// The function of a method as `method()` takes it back: what
/// `method_get_index` gave. A script reference (a tagged 64-bit value, about
/// 2^56) goes back as that reference: as a real (53-bit mantissa) it loses
/// its low bits and names a neighbouring script.
#[expect(
    clippy::cast_precision_loss,
    reason = "exact: only for a magnitude below 2^32, checked first"
)]
fn method_func(func: i64) -> Value {
    if func.unsigned_abs() >> 32 == 0 {
        Value::Real(func as f64)
    } else {
        Value::Ref(func as u64)
    }
}

/// Variables the world sync writes on a copy every tick (from the snapshot's
/// hull): a repair leaves them alone.
pub const WORLD_OWNED: &[&str] = &["hull", "hullPrevious"];

/// The enemy variables a player's damage and procs read, each with a row:
/// players proc only from their own effects. Every variable that the
/// enemy's initialisation sets and a hit, damage-over-time or death path
/// reads has a row here, in [`STATUS_CARRIED`] or in [`ENEMY_OWN`].
///
/// **Owned**: a player's own status on the enemy. Each game keeps its own on
/// its own enemies or copies; the generic repair never writes the host's
/// onto a copy. A copy's own status is reported only through what it does to
/// the hull. No draw reads these.
pub const STATUS_OWNED: &[(&str, &str)] = &[
    (
        "blitzed",
        "Blitz: `blitzBoost` gives a hit's bonus once per enemy below the player's threshold and \
         sets it; the host's flag on a copy would take the joining player's bonus away",
    ),
    (
        "corrosiveStacks",
        "Corrosion: the joining player's stacks, ticked by its `degenerations`, spread at the \
         death from its copy",
    ),
    (
        "purgeImmunity",
        "Purge: `playerPurge` (the ship's ram) purges once per cooldown and sets it",
    ),
    (
        "satFireHits",
        "Saturation Fire: per satellite id, the hit count `applySatFireHit` multiplies the \
         player's satellites' damage by",
    ),
    (
        "stuckCount",
        "darts lodged in the enemy (`stick`, `dartBurn`): the player's darts' burn",
    ),
    (
        "vulnerabilityCount",
        "the vulnerability mark's timer (`applyVulnerability`, `decrementVulnerability`)",
    ),
    (
        "vulnerabilityMult",
        "the vulnerability mark: `damageTakenModifiers` multiplies every hit on the enemy by it",
    ),
];

/// Status variables the repair carries, and why.
pub const STATUS_CARRIED: &[(&str, &str)] = &[
    (
        "burning",
        "damage over time: carried, since a burn has a look (the game draws it from these: \
         `degenerations`' slag particles); owned, the host's burn would not show on the joining \
         player's copies",
    ),
    ("dotCelestialSurge", "damage over time: as `burning`"),
    ("dotDurationList", "damage over time: as `burning`"),
    ("dotIncendiary", "damage over time: as `burning`"),
    ("dotTimer", "damage over time: as `burning`"),
    ("totalDegen", "damage over time: as `burning`"),
    (
        "damageAccumulatedBurn",
        "damage-number bookkeeping (the floating numbers): no effect on damage or procs",
    ),
    ("damageAccumulatedContinuous", "as `damageAccumulatedBurn`"),
    ("damageAccumulatedSelf", "as `damageAccumulatedBurn`"),
    ("damageFloaterTimer", "as `damageAccumulatedBurn`"),
    (
        "canHitPulse",
        "the hit flash's readiness (`spawnHitPulse`): a look, no effect on damage or procs",
    ),
    (
        "pushed_x",
        "knockback: the copy's place is the host's (snapshots), so a push on the copy moves \
         nothing",
    ),
    ("pushed_y", "as `pushed_x`"),
    (
        "enemySpeed",
        "the enemy's speed (its AI, slows): the host's motion; no proc reads it",
    ),
    ("enemySpeedInverse", "as `enemySpeed`"),
    (
        "victemCount",
        "shatter bookkeeping `projectileHitsSolid` keeps, 0 on enemies: no proc reads it",
    ),
];

/// The enemy's own state that hits and deaths read (its place, size, hull,
/// type properties, AI, rarity, treasure): the same on both games, the host's
/// word; the repair carries it.
pub const ENEMY_OWN: &[&str] = &[
    "alarm",
    "baseMoveSpeed",
    "baseRotSpeed",
    "breakDelay",
    "canDestroy",
    "cantShatter",
    "currentHullOverride",
    "diff",
    "direction",
    "effectRes",
    "enemyAreaDamageTaken",
    "enemyBehavior",
    "enemyDamageModifier",
    "enemyEntry",
    "enemyIsChallenger",
    "enemySingleDamageTaken",
    "enemySizeModifier",
    "enemySpeedFactor",
    "enemyTargetSpeed",
    "enemyTreasureModifier",
    "hardShieldable",
    "hardShielded",
    "hull",
    "hullMax",
    "hullPrevious",
    "ignoreLeaderTarget",
    "image_angle",
    "image_xscale",
    "image_yscale",
    "leader",
    "moveSpeed",
    "onDestroyEffects",
    "rarity",
    "res",
    "rotSpeed",
    "scaler",
    "shotDistribute",
    "sizeBuffer",
    "targMoveSpeed",
    "targRotSpeed",
    "target",
    "targetDir",
    "targetPriority",
    "targetX",
    "targetY",
    "threat",
    "treasure",
    "turn",
    "viewHeight",
    "viewWidth",
    "viewX",
    "viewY",
    "weaknessBonus",
    "wrapBuffer",
    "wrapping",
    "x",
    "y",
];

/// Whether a repair leaves `name` alone on a copy: a player's own status
/// ([`STATUS_OWNED`]).
#[must_use]
pub fn status_owned(name: &str) -> bool {
    STATUS_OWNED.iter().any(|(v, _)| *v == name)
}

/// A copy's first state carries an owned status at its neutral value, not
/// the host's: no status at all (0; `vulnerabilityMult` 1, what `enemyInit`
/// sets), in the host's value's shape (an array element by element, a flag
/// false). An object whose create never sets it (an `oEnemyHardShield`'s
/// `corrosiveStacks`) needs it: `degenerations` stops on an unset variable.
/// `None`: not an owned status.
#[must_use]
pub fn status_neutral(name: &str, host: &StateValue) -> Option<StateValue> {
    fn shape(v: &StateValue, n: f64) -> StateValue {
        match v {
            StateValue::Array(a) => StateValue::Array(a.iter().map(|e| shape(e, n)).collect()),
            StateValue::Bool(_) => StateValue::Bool(n != 0.0),
            _ => StateValue::Real(n),
        }
    }
    status_owned(name).then(|| {
        shape(
            host,
            if name == "vulnerabilityMult" {
                1.0
            } else {
                0.0
            },
        )
    })
}

/// Write `vars`, `builtins` and `alarms` onto instance `id`. Unsupported
/// values are left as the copy has them.
pub fn apply(
    call: &Call<'_>,
    r: &Reflect,
    id: &Value,
    vars: &[(String, StateValue)],
    builtins: &[(u8, StateValue)],
    alarms: Option<&[(u8, i32)]>,
    id_of: IdOf<'_>,
    stats: &mut Stats,
) {
    for (name, v) in vars {
        let (unresolved, target, waiting) = match v {
            StateValue::Unresolved(o) => (true, Some(*o), false),
            StateValue::Ref(p) if is_instance_ref(*p) => {
                stats.unresolved_here.push((name.clone(), "raw", 0, 0));
                (true, None, false)
            }
            StateValue::Instance(net) if id_of(*net).is_none() => {
                // Not made here yet: set when it is (`waiting`).
                stats.waiting.push((name.clone(), *net));
                (true, None, true)
            }
            _ => (false, None, false),
        };
        if waiting {
            // Held: the copy keeps what it has until the instance is made
            // here; only a variable the copy lacks is written, as `noone`.
            let key = Value::Str(name.clone());
            let has = call
                .call_builtin_held(r.get, &[arg(id), arg(&key)])
                .is_ok_and(|h| !matches!(h.value(), Value::Undefined));
            if has {
                stats.held += 1;
            } else if set_var(call, r, id, name, &Value::Real(NOONE)) {
                stats.written += 1;
            }
            continue;
        }
        if unresolved {
            // A local instance of the sender (an effect, a thruster): the copy
            // keeps its own if it made one, else `noone`, so `instance_exists`
            // never meets an unset variable. Object 0 (no live instance on
            // the host): always `noone`.
            let key = Value::Str(name.clone());
            let own = call
                .call_builtin_held(r.get, &[arg(id), arg(&key)])
                .ok()
                .map(|h| h.value());
            let keep = target != Some(0) && own.as_ref().is_some_and(|o| instance_of(call, r, o));
            if let Some(o) = target {
                let how = if keep { "own" } else { "noone" };
                // The copy's own instance's object (an `own` reference is
                // right when it is the host target's object).
                let here = match own.as_ref() {
                    Some(v) if keep => object_index_of(call, v).1,
                    _ => 0,
                };
                stats.unresolved_here.push((name.clone(), how, o, here));
            }
            if !keep && set_var(call, r, id, name, &Value::Real(NOONE)) {
                stats.written += 1;
            }
            continue;
        }
        if is_game_handle_var(name) || matches!(v, StateValue::Ref(p) if is_local_handle(*p)) {
            // Each game's own (a layer, a ds list's index): the copy's stays.
            continue;
        }
        let Some(b) = build(call, r, v, id_of, name, stats) else {
            continue;
        };
        let key = Value::Str(name.clone());
        match call.call_builtin_held(r.set, &[arg(id), arg(&key), b.arg()]) {
            Ok(_) => stats.written += 1,
            Err(_) => stats.failed += 1,
        }
        // Read back what was built in the game: does the variable hold the
        // kind sent?
        let want = match v {
            StateValue::Array(_) => Some(kind::ARRAY),
            StateValue::Struct(_) | StateValue::Method { .. } => Some(kind::OBJECT),
            _ => None,
        };
        if let Some(want) = want {
            let got = call
                .call_builtin_held(r.get, &[arg(id), arg(&key)])
                .map_or(u32::MAX, |h| h.kind());
            if got != want {
                stats.readback_bad.push((name.clone(), got));
            }
        }
    }
    for (i, v) in builtins {
        let Some(name) = BUILTINS.get(usize::from(*i)) else {
            continue;
        };
        let Some(b) = build(call, r, v, id_of, name, stats) else {
            continue;
        };
        let key = Value::Str((*name).to_owned());
        match call.call_builtin_held(r.set, &[arg(id), arg(&key), b.arg()]) {
            Ok(_) => stats.written += 1,
            Err(_) => stats.failed += 1,
        }
    }
    if let Some(alarms) = alarms {
        let _ = call.with(id, |inst, _| set_alarms(&inst, r, alarms));
    }
}

/// `variable_instance_set(id, name, v)`.
#[must_use]
pub fn set_var(call: &Call<'_>, r: &Reflect, id: &Value, name: &str, v: &Value) -> bool {
    call.call_builtin(r.set, &[id.clone(), Value::Str(name.to_owned()), v.clone()])
        .is_ok()
}

fn set_alarms(inst: &Instance<'_>, r: &Reflect, alarms: &[(u8, i32)]) {
    for a in 0..ALARMS {
        let left = alarms
            .iter()
            .find(|(i, _)| *i == a)
            .map_or(-1.0, |(_, v)| f64::from(*v));
        let _ = inst.call_builtin(r.alarm_set, &[f64::from(a).into(), left.into()]);
    }
}

/// The variables (and built-ins) whose values differ between two states.
#[must_use]
pub fn changed(old: &Captured, new: &Captured) -> Captured {
    let prev: HashMap<&str, &StateValue> = old.vars.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let pb: HashMap<u8, &StateValue> = old.builtins.iter().map(|(k, v)| (*k, v)).collect();
    Captured {
        vars: new
            .vars
            .iter()
            .filter(|(k, v)| prev.get(k.as_str()) != Some(&v))
            .cloned()
            .collect(),
        builtins: new
            .builtins
            .iter()
            .filter(|(k, v)| pb.get(k) != Some(&v))
            .cloned()
            .collect(),
        alarms: new.alarms.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layer_is_each_games_own_an_asset_or_instance_is_not() {
        // The BG controller's `backgroundLayerCoils`.
        assert!(is_local_handle(0x0800_0006_0000_0071));
        assert!(!is_local_handle(OBJECT_REF_TAG | 0x25b));
        assert!(!is_local_handle(0x0100_0001_0000_0338)); // a sprite
        assert!(!is_local_handle(0x0100_0002_0000_0010)); // a sound
        assert!(!is_local_handle(0x0100_0005_0001_909b)); // a function
        assert!(!is_local_handle(0x0400_0001_0001_f400)); // an instance
    }

    #[test]
    fn an_owned_status_arrives_neutral_in_the_hosts_shape() {
        let r = StateValue::Real;
        assert_eq!(status_neutral("vulnerabilityMult", &r(1.6)), Some(r(1.0)));
        assert_eq!(status_neutral("corrosiveStacks", &r(4.0)), Some(r(0.0)));
        assert_eq!(
            status_neutral("blitzed", &StateValue::Bool(true)),
            Some(StateValue::Bool(false))
        );
        assert_eq!(
            status_neutral("satFireHits", &StateValue::Array(vec![r(3.0), r(1.0)])),
            Some(StateValue::Array(vec![r(0.0), r(0.0)]))
        );
        assert_eq!(status_neutral("totalDegen", &r(9.0)), None, "carried");
        assert_eq!(status_neutral("hull", &r(9.0)), None);
    }

    #[test]
    fn status_table_has_one_row_per_variable() {
        let mut all: Vec<&str> = STATUS_OWNED
            .iter()
            .chain(STATUS_CARRIED)
            .map(|(v, _)| *v)
            .chain(ENEMY_OWN.iter().copied())
            .collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "a variable in two rows");
        assert!(ENEMY_OWN.windows(2).all(|w| w[0] < w[1]), "sorted");
        // The marks and Corrosion are each game's own; the burn is carried
        // (its look); the hull is the world's.
        for v in [
            "vulnerabilityMult",
            "satFireHits",
            "blitzed",
            "corrosiveStacks",
        ] {
            assert!(status_owned(v), "{v}");
        }
        for v in ["dotDurationList", "totalDegen", "hull", "x"] {
            assert!(!status_owned(v), "{v}");
        }
    }

    #[test]
    fn handles_kept_as_numbers_are_each_games_own_by_name() {
        assert!(
            GAME_HANDLE_VARS.windows(2).all(|w| w[0] < w[1]),
            "sorted, no repeats (binary search)"
        );
        // A ds list's index (enemy blasts, comet shards), the coils' layer.
        assert!(is_game_handle_var("hitList"));
        assert!(is_game_handle_var("backgroundLayerCoils"));
        assert!(is_game_handle_var("activeCollisions"));
        // Numbers that are numbers, and an array (`oEnemySpawner.arr` is
        // `[]`, `@@NewGMLArray@@`, not a handle).
        assert!(!is_game_handle_var("hull"));
        assert!(!is_game_handle_var("arr"));
        assert!(!is_game_handle_var("phaseTimer"));
    }

    #[test]
    fn instance_refs_and_origin() {
        // The Ceratotitan (id 128000) as its pincers' `leader`.
        assert!(is_instance_ref(0x0400_0001_0001_f400));
        assert!(!is_instance_ref(OBJECT_REF_TAG | 0x25b));
        assert!(!is_instance_ref(0x0100_0005_0001_909b)); // a function
        let mut c = Captured::default();
        assert!(!born_by_copy(&c));
        c.builtins.push((0, StateValue::Real(3.0)));
        assert!(!born_by_copy(&c));
        c.builtins
            .push((ORIGIN_BUILTIN, StateValue::Whole(ORIGIN_COPY)));
        assert!(born_by_copy(&c));
        assert!(BUILTINS.get(usize::from(ORIGIN_BUILTIN)).is_none());
        assert!(BUILTINS.get(usize::from(MAKER_BUILTIN)).is_none());
        assert!(BUILTINS.get(usize::from(MAKER_EVENT_BUILTIN)).is_none());
        assert!(!maker_ran_event1(&c));
        c.builtins.push((MAKER_EVENT_BUILTIN, StateValue::Whole(1)));
        assert!(maker_ran_event1(&c));
    }

    #[test]
    #[expect(clippy::cast_precision_loss, reason = "shows the loss on purpose")]
    fn method_functions_go_back_exactly() {
        // Script references, about 2^56, as reals.
        let f = 72_057_615_512_867_079_i64;
        assert_eq!(method_func(f), Value::Ref(72_057_615_512_867_079));
        assert_ne!(f as f64 as i64, f, "a real cannot hold it");
        assert_eq!(method_func(12), Value::Real(12.0));
    }

    #[test]
    fn changed_lists_what_differs() {
        let a = Captured {
            vars: vec![
                ("hull".into(), StateValue::Real(10.0)),
                ("leader".into(), StateValue::Instance(4)),
                (
                    "phases".into(),
                    StateValue::Array(vec![StateValue::Method {
                        func: 7,
                        bound: Box::new(StateValue::Instance(3)),
                    }]),
                ),
            ],
            builtins: vec![(0, StateValue::Ref(0x5))],
            alarms: vec![(0, 30)],
        };
        let mut b = a.clone();
        b.vars[0].1 = StateValue::Real(9.0);
        let d = changed(&a, &b);
        assert_eq!(d.vars.len(), 1);
        assert_eq!(d.vars[0].0, "hull");
        assert!(d.builtins.is_empty());
    }
}
