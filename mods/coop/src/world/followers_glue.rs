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

//! Followers a leader's code makes: effects (a glow, a following graphic)
//! that another instance's code made for a replicated instance sent without
//! its set-up, so nothing on the joining game makes them.
//!
//! * Host, at such an instance's first state: the non-replicated instances
//!   whose `leader` it is go with that state (object, variables, drawing
//!   built-ins, alarms, and the leader's variables naming them).
//! * Client, as that state is applied: the followers the copy lacks are made
//!   at it, given their state and named in the copy's variables.
//!
//! On the wire each is one pseudo built-in ([`FOLLOWER_BUILTIN`]); a receiver
//! that does not know it skips it.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ndml_net::msg::{InstanceStateIx, Message, StateValue};
use ndml_sdk::{Call, Value};

use super::super::classify::{Class, is_instance, object_index};
use super::super::names::builtin_var as bv;
use super::super::state::{self, BUILTINS, Captured, Stats};
use super::WorldHooks;

/// Pseudo built-in index on the wire (not in [`BUILTINS`]): one follower in a
/// first state ([`encode`]); may repeat. Shares 252 with
/// [`state::MAKER_EVENT_BUILTIN`], which goes only on states sent with their
/// set-up; followers only on states sent without.
pub const FOLLOWER_BUILTIN: u8 = 252;

/// The built-ins a follower carries: how it is drawn (it places itself on
/// its leader).
const FOLLOWER_BUILTINS: &[&str] = &[
    "sprite_index",
    "image_index",
    "image_speed",
    "image_xscale",
    "image_yscale",
    "image_angle",
    "image_alpha",
    "image_blend",
    "depth",
    "visible",
];

/// Encoded bytes the followers of one first state may take (the first part
/// carries every built-in and must fit one message). The rest are left out.
const FOLLOWER_BYTES: usize = 640;

/// Followers one leader may take.
const FOLLOWERS_MAX: usize = 8;

/// One follower as sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Follower {
    /// Object index.
    pub object: u16,
    /// Its variables, drawing built-ins and alarms.
    pub state: Captured,
    /// The leader's variables that refer to it.
    pub refs: Vec<String>,
}

/// One follower as a pseudo built-in value: `[object, {vars}, [[ix, v]...],
/// [[alarm, left]...], [ref names]]`.
#[must_use]
pub fn encode(f: &Follower) -> StateValue {
    let whole = |n: i64| StateValue::Int(n);
    StateValue::Array(vec![
        whole(i64::from(f.object)),
        StateValue::Struct(f.state.vars.clone()),
        StateValue::Array(
            f.state
                .builtins
                .iter()
                .map(|(i, v)| StateValue::Array(vec![whole(i64::from(*i)), v.clone()]))
                .collect(),
        ),
        StateValue::Array(
            f.state
                .alarms
                .iter()
                .map(|(a, n)| StateValue::Array(vec![whole(i64::from(*a)), whole(i64::from(*n))]))
                .collect(),
        ),
        StateValue::Array(f.refs.iter().map(|r| StateValue::Str(r.clone())).collect()),
    ])
}

fn int_of(v: &StateValue) -> Option<i64> {
    match v.clone().expanded() {
        StateValue::Int(n) => Some(n),
        StateValue::Real(x) if x.fract() == 0.0 => Some(x as i64),
        _ => None,
    }
}

fn pair(v: &StateValue) -> Option<(i64, &StateValue)> {
    match v {
        StateValue::Array(p) if p.len() == 2 => Some((int_of(&p[0])?, &p[1])),
        _ => None,
    }
}

/// [`encode`] undone; `None` for anything malformed.
#[must_use]
pub fn decode(v: &StateValue) -> Option<Follower> {
    let StateValue::Array(parts) = v else {
        return None;
    };
    let [object, vars, builtins, alarms, refs] = parts.as_slice() else {
        return None;
    };
    let object = u16::try_from(int_of(object)?).ok()?;
    let StateValue::Struct(vars) = vars else {
        return None;
    };
    let (StateValue::Array(builtins), StateValue::Array(alarms), StateValue::Array(refs)) =
        (builtins, alarms, refs)
    else {
        return None;
    };
    let builtins = builtins
        .iter()
        .map(|b| pair(b).and_then(|(i, v)| Some((u8::try_from(i).ok()?, v.clone()))))
        .collect::<Option<Vec<_>>>()?;
    let alarms = alarms
        .iter()
        .map(|a| {
            pair(a)
                .and_then(|(i, n)| Some((u8::try_from(i).ok()?, i32::try_from(int_of(n)?).ok()?)))
        })
        .collect::<Option<Vec<_>>>()?;
    let refs = refs
        .iter()
        .map(|r| match r {
            StateValue::Str(s) => Some(s.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Follower {
        object,
        state: Captured {
            vars: vars.clone(),
            builtins,
            alarms,
        },
        refs,
    })
}

/// The followers a first state carries.
#[must_use]
pub fn followers_in(c: &Captured) -> Vec<Follower> {
    c.builtins
        .iter()
        .filter(|(i, _)| *i == FOLLOWER_BUILTIN)
        .filter_map(|(_, v)| decode(v))
        .collect()
}

/// Encoded size of a pseudo built-in value (compact, as sent).
fn wire_bytes(v: &StateValue) -> usize {
    Message::InstanceStateIx(InstanceStateIx {
        net_id: 0,
        object: 0,
        seq: 0,
        part: 0,
        parts: 1,
        vars: Vec::new(),
        builtins: vec![(FOLLOWER_BUILTIN, v.clone().compact())],
        alarms: Vec::new(),
    })
    .encode()
    .map_or(usize::MAX, |b| b.len())
}

/// The followers that fit [`FOLLOWER_BYTES`], in order, as pseudo built-ins;
/// and how many were left out.
fn fitting(followers: &[Follower]) -> (Vec<(u8, StateValue)>, u64) {
    let mut out = Vec::new();
    let mut used = 0usize;
    let mut left = 0u64;
    for f in followers {
        let v = encode(f);
        let n = wire_bytes(&v);
        if out.len() >= FOLLOWERS_MAX || used.saturating_add(n) > FOLLOWER_BYTES {
            left += 1;
            continue;
        }
        used += n;
        out.push((FOLLOWER_BUILTIN, v));
    }
    (out, left)
}

/// An instance id as a number (refs or numbers), for comparing references.
fn id_number(v: &Value) -> Option<i64> {
    match v {
        Value::Ref(p) => Some(i64::from((*p & 0xFFFF_FFFF) as u32)),
        other => other.as_f64().map(|x| x as i64),
    }
}

/// Host: this frame's not-replicated instances by the leader they follow.
#[derive(Debug, Default)]
struct Scan {
    frame: u64,
    /// Leader id -> (follower id, object).
    by_leader: HashMap<i64, Vec<(Value, u32)>>,
}

/// Host: the scan cache.
#[derive(Debug, Default)]
pub(super) struct FollowersState {
    scan: Option<Scan>,
}

impl WorldHooks {
    /// Host: the not-replicated followers by leader, from one `with (all)`
    /// pass a frame (kept for the frame).
    fn followers_scan(&self, call: &Call<'_>) -> HashMap<i64, Vec<(Value, u32)>> {
        let frame = self.lock().frame;
        if let Some(s) = self.lock().followers.scan.as_ref()
            && s.frame == frame
        {
            return s.by_leader.clone();
        }
        let mut by_leader: HashMap<i64, Vec<(Value, u32)>> = HashMap::new();
        if let Some(leader) = self.h.leader {
            let mut found: Vec<(i64, usize, Value, u32)> = Vec::new();
            let _ = call.with(&Value::Real(-3.0), |inst, _| {
                let Some(l) = inst
                    .get_value(leader)
                    .filter(is_instance)
                    .as_ref()
                    .and_then(id_number)
                else {
                    return;
                };
                let Some(o) = inst
                    .builtin_value(bv::OBJECT_INDEX)
                    .ok()
                    .as_ref()
                    .and_then(object_index)
                else {
                    return;
                };
                let Ok(id) = inst.builtin_value(bv::ID) else {
                    return;
                };
                found.push((l, inst.key(), id, o));
            });
            for (l, key, id, o) in found {
                if self.lock().host.net_id_for(key, &id).is_some() {
                    continue;
                }
                by_leader.entry(l).or_default().push((id, o));
            }
        }
        let mut st = self.lock();
        st.followers.scan = Some(Scan {
            frame,
            by_leader: by_leader.clone(),
        });
        by_leader
    }

    /// Host, at the first state of an instance sent without its set-up
    /// (`first`, of instance `id`): its followers its leader's code made, as
    /// [`FOLLOWER_BUILTIN`] entries.
    pub(super) fn followers_attach(&self, call: &Call<'_>, id: &Value, first: &mut Captured) {
        let Some(r) = self.h.reflect.as_ref() else {
            return;
        };
        let Some(me) = id_number(id) else { return };
        let mine = self.followers_scan(call).remove(&me).unwrap_or_default();
        let mut followers = Vec::new();
        for (fid, o) in mine {
            if !matches!(self.class_of(call, o, false), Some(Class::Other)) {
                continue;
            }
            let Ok(object) = u16::try_from(o) else {
                continue;
            };
            if !self.exists(call, &fid) {
                continue;
            }
            // Named by the leader and replicated as a dependency (the Star
            // Eater's legs): the client gets the host's instead.
            let fnum = id_number(&fid);
            let named = first
                .vars
                .iter()
                .filter(|(_, v)| matches!(v, StateValue::Unresolved(_) | StateValue::Instance(_)))
                .any(|(n, _)| {
                    state::get_var(call, r, id, n)
                        .as_ref()
                        .and_then(id_number)
                        .is_some_and(|n| Some(n) == fnum)
                });
            if named && self.dep_eligible(call, object) {
                continue;
            }
            let net_of = |k: usize, v: &Value| self.lock().host.net_id_for(k, v);
            let mut s = state::capture(call, r, &fid, &net_of, &mut Stats::default());
            s.builtins.retain(|(i, _)| {
                BUILTINS
                    .get(usize::from(*i))
                    .is_some_and(|n| FOLLOWER_BUILTINS.contains(n))
            });
            // The leader's variables that hold this follower (`Unresolved`).
            let refs = first
                .vars
                .iter()
                .filter(|(_, v)| matches!(v, StateValue::Unresolved(u) if *u == object))
                .filter(|(n, _)| {
                    state::get_var(call, r, id, n)
                        .as_ref()
                        .and_then(id_number)
                        .is_some_and(|n| Some(n) == fnum)
                })
                .map(|(n, _)| n.clone())
                .collect();
            followers.push(Follower {
                object,
                state: s,
                refs,
            });
        }
        // Referred to first (a glow the leader names), then by object.
        followers.sort_by_key(|f| (f.refs.is_empty(), f.object));
        let (entries, _) = fitting(&followers);
        first.builtins.extend(entries);
    }

    /// Client, as state `seq` of copy `copy` is applied: on the first, the
    /// followers it carries that the copy lacks, made and set up.
    pub(super) fn followers_make(&self, call: &Call<'_>, copy: &Value, seq: u16, first: &Captured) {
        let followers = if seq == 0 {
            followers_in(first)
        } else {
            Vec::new()
        };
        if followers.is_empty() {
            return;
        }
        let (Some(reflect), Some(create), Some(leader)) = (
            self.h.reflect.as_ref(),
            self.h.instance_create,
            self.h.leader,
        ) else {
            return;
        };
        let Some(me) = id_number(copy) else { return };
        let mut pos = None;
        let _ = call.with(copy, |inst, _| {
            pos = inst.builtin_f64(bv::X).zip(inst.builtin_f64(bv::Y));
        });
        let Some((at_x, at_y)) = pos else { return };
        // Per object: how many the copy has already (its own Create made them).
        let mut have: HashMap<u16, usize> = HashMap::new();
        for f in &followers {
            if have.contains_key(&f.object) {
                continue;
            }
            let mut count = 0usize;
            let _ = call.with(&Value::object(u32::from(f.object)), |inst, _| {
                if inst
                    .get_value(leader)
                    .filter(is_instance)
                    .as_ref()
                    .and_then(id_number)
                    == Some(me)
                {
                    count += 1;
                }
            });
            have.insert(f.object, count);
        }
        for f in &followers {
            if let Some(left) = have.get_mut(&f.object)
                && *left > 0
            {
                *left -= 1;
                continue;
            }
            // An object that came as a dependency: the host's arrives.
            if self.is_dep_kind(u32::from(f.object)) {
                continue;
            }
            let fid = call
                .call_script(
                    create,
                    &[at_x.into(), at_y.into(), Value::object(u32::from(f.object))],
                )
                .ok()
                .filter(is_instance);
            let Some(fid) = fid else {
                continue;
            };
            let id_of = |net| self.lock().client.id_of(net);
            state::apply(
                call,
                reflect,
                &fid,
                &f.state.vars,
                &f.state.builtins,
                Some(&f.state.alarms),
                &id_of,
                &mut Stats::default(),
            );
            for name in &f.refs {
                let _ = state::set_var(call, reflect, copy, name, &fid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glow() -> Follower {
        Follower {
            object: 473,
            state: Captured {
                vars: vec![
                    ("leader".into(), StateValue::Instance(83)),
                    ("pooled".into(), StateValue::Bool(false)),
                ],
                builtins: vec![
                    (0, StateValue::Ref(0x0100_0000_0000_04d2)),
                    (7, StateValue::Real(0.4)),
                ],
                alarms: vec![(4, 12)],
            },
            refs: vec!["enemyGlow".into()],
        }
    }

    #[test]
    fn a_follower_reads_back_as_sent_and_compact() {
        let f = glow();
        assert_eq!(decode(&encode(&f)), Some(f.clone()));
        // As it travels: compact, then expanded by the receiver.
        let wire = encode(&f).compact().expanded();
        let back = decode(&wire).unwrap();
        assert_eq!(back.object, 473);
        assert_eq!(back.refs, vec!["enemyGlow".to_owned()]);
        assert_eq!(back.state.alarms, vec![(4, 12)]);
    }

    #[test]
    fn a_first_state_carries_its_followers_and_nothing_else_reads_as_one() {
        let c = Captured {
            builtins: vec![
                (9, StateValue::Real(10.0)),
                (state::ORIGIN_BUILTIN, StateValue::Whole(state::ORIGIN_COPY)),
                (FOLLOWER_BUILTIN, encode(&glow())),
                (FOLLOWER_BUILTIN, StateValue::Str("bad".into())),
            ],
            ..Captured::default()
        };
        assert_eq!(followers_in(&c), vec![glow()]);
        assert!(BUILTINS.get(usize::from(FOLLOWER_BUILTIN)).is_none());
    }

    #[test]
    fn followers_past_the_byte_budget_are_left_out() {
        let mut big = glow();
        big.state
            .vars
            .push(("name".into(), StateValue::Str("x".repeat(300))));
        let all = vec![big.clone(), big.clone(), big, glow()];
        let (sent, left) = fitting(&all);
        assert_eq!(sent.len() + left as usize, 4);
        assert!(left >= 1);
        let bytes: usize = sent.iter().map(|(_, v)| wire_bytes(v)).sum();
        assert!(bytes <= FOLLOWER_BYTES);
        // The small one still goes after the big ones are cut.
        assert!(sent.iter().any(|(_, v)| decode(v) == Some(glow())));
    }
}
