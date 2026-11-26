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
//! * A shared shot whose maker's code dresses it after the create (the
//!   Crucible's `spawnEdges` keeps a graphic in the burn's `follower`)
//!   carries its followers in its one state ([`WorldHooks::shot_followers`]).
//!   Only a follower the shot names in a variable goes.
//!
//! On the wire each is one pseudo built-in ([`FOLLOWER_BUILTIN`]) in the
//! first state (or the shot's built-ins); a receiver that does not know it
//! skips it.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ndml_net::msg::{InstanceStateIx, Message, StateValue};
use ndml_net::shots::FiredShot;
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

/// Whether an instance naming a copy as its `leader` counts as one of the
/// copy's followers in the top-up: it follows this copy, is live (not back
/// in its pool: `oEffectPooled`'s `inPool`; a deactivated one is not seen
/// at all) and drawn (`visible`). A glow taken from the pool and given
/// back keeps its `leader`.
#[must_use]
pub fn counts_as_mine(follows: bool, in_pool: bool, drawn: bool) -> bool {
    follows && !in_pool && drawn
}

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

/// Encoded bytes the followers of one first state may take: a bound on the
/// first state's size, not a datagram's (built-ins past one message go on in
/// the state's next parts, `InstanceStateIx::split`). A follower that does
/// not fit is left out.
const FOLLOWER_BYTES: usize = 2400;

/// Followers one leader may take (a leader with more is cut).
const FOLLOWERS_MAX: usize = 16;

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

/// A shared shot given the followers that fit the room its datagram has
/// left ([`FiredShot::room`]), as compact pseudo built-ins. How many went
/// and how many were left out.
pub(super) fn dress(
    shot: &mut FiredShot,
    followers: &[Follower],
    world: u64,
    t: &mut FollowersState,
) -> (usize, u64) {
    let (entries, left) = fitting(followers, shot.room());
    let sent = entries.len();
    shot.builtins
        .extend(entries.into_iter().map(|(i, v)| (i, v.compact())));
    let _ = (world, t);
    (sent, left)
}

/// The followers that fit `bytes` ([`FOLLOWER_BYTES`] for a first state), in
/// order, as pseudo built-ins; and how many were left out.
fn fitting(followers: &[Follower], bytes: usize) -> (Vec<(u8, StateValue)>, u64) {
    let mut out = Vec::new();
    let mut used = 0usize;
    let mut left = 0u64;
    for f in followers {
        let v = encode(f);
        let n = wire_bytes(&v);
        if out.len() >= FOLLOWERS_MAX || used.saturating_add(n) > bytes {
            left += 1;
            continue;
        }
        used += n;
        out.push((FOLLOWER_BUILTIN, v));
    }
    (out, left)
}

/// An instance id as a number (refs or numbers), for comparing references.
pub(super) fn id_number(v: &Value) -> Option<i64> {
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
        self.followers_rescan(call, frame)
    }

    /// [`Self::followers_scan`]'s pass, kept for `frame`.
    fn followers_rescan(&self, call: &Call<'_>, frame: u64) -> HashMap<i64, Vec<(Value, u32)>> {
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

    /// Client: the local effects ([`Class::Other`], not replicated) whose
    /// `leader` is one of `leaders`, each with the id number of the leader it
    /// follows. They are held with a held copy
    /// ([`super::client::ClientWorld::update_held`]): an effect whose leader
    /// is deactivated takes it for gone. One `with (all)` pass, and one
    /// `with (object)` pass for each of `objects`.
    pub(super) fn followers_of(
        &self,
        call: &Call<'_>,
        leaders: &[Value],
        objects: &[u16],
    ) -> Vec<(i64, Value)> {
        let Some(leader) = self.h.leader else {
            return Vec::new();
        };
        let want: Vec<i64> = leaders.iter().filter_map(id_number).collect();
        if want.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<(i64, Value, u32)> = Vec::new();
        let mut look = |inst: &ndml_sdk::Instance<'_>| {
            let Some(l) = inst
                .get_value(leader)
                .filter(is_instance)
                .as_ref()
                .and_then(id_number)
            else {
                return;
            };
            if !want.contains(&l) {
                return;
            }
            let (Ok(id), Some(o)) = (
                inst.builtin_value(bv::ID),
                inst.builtin_value(bv::OBJECT_INDEX)
                    .ok()
                    .as_ref()
                    .and_then(object_index),
            ) else {
                return;
            };
            if !found.iter().any(|(_, g, _)| id_number(g) == id_number(&id)) {
                found.push((l, id, o));
            }
        };
        let _ = call.with(&Value::Real(-3.0), |inst, _| look(&inst));
        for &o in objects {
            let _ = call.with(&Value::object(u32::from(o)), |inst, _| look(&inst));
        }
        found
            .into_iter()
            .filter(|(_, _, o)| {
                self.class_of(call, *o, false)
                    .is_some_and(Class::may_go_with_an_enemy)
            })
            .map(|(l, id, _)| (l, id))
            .collect()
    }

    /// Host, at the first state of an instance sent without its set-up
    /// (`first`, of instance `id`): its followers its leader's code made, as
    /// [`FOLLOWER_BUILTIN`] entries.
    pub(super) fn followers_attach(&self, call: &Call<'_>, id: &Value, first: &mut Captured) {
        let Some(me) = id_number(id) else { return };
        let mine = self.followers_scan(call).remove(&me).unwrap_or_default();
        let (followers, world) = self.followers_built(call, id, &first.vars, mine);
        let (entries, _) = fitting(&followers, FOLLOWER_BYTES);
        let _ = world;
        first.builtins.extend(entries);
    }

    /// Host, at a shared shot's read (`vars` of shot `id`): its followers its
    /// maker's code made, when the shot names one (module docs); empty
    /// otherwise, with no pass over the instances; and how many were left to
    /// the world. The caller fits them to what the shot's datagram has left
    /// ([`dress`]).
    pub(super) fn shot_followers(
        &self,
        call: &Call<'_>,
        id: &Value,
        vars: &[(String, StateValue)],
    ) -> (Vec<Follower>, u64) {
        let (Some(r), Some(leader)) = (self.h.reflect.as_ref(), self.h.leader) else {
            return (Vec::new(), 0);
        };
        let Some(me) = id_number(id) else {
            return (Vec::new(), 0);
        };
        // The effects it names that follow it.
        let mut named = Vec::new();
        for (n, v) in vars {
            let StateValue::Unresolved(o) = v else {
                continue;
            };
            if *o == 0
                || !matches!(
                    self.class_of(call, u32::from(*o), false),
                    Some(Class::Other)
                )
            {
                continue;
            }
            let Some(fid) = state::get_var(call, r, id, n).filter(is_instance) else {
                continue;
            };
            let mut follows = false;
            let _ = call.with(&fid, |inst, _| {
                follows = inst
                    .get_value(leader)
                    .filter(is_instance)
                    .as_ref()
                    .and_then(id_number)
                    == Some(me);
            });
            if follows && let Some(f) = id_number(&fid) {
                named.push(f);
            }
        }
        if named.is_empty() {
            return (Vec::new(), 0);
        }
        // This frame's pass, again if it ran before the shot was dressed (a
        // first state earlier in the frame).
        let mut mine = self.followers_scan(call).remove(&me).unwrap_or_default();
        let has_all = |m: &[(Value, u32)]| {
            named
                .iter()
                .all(|f| m.iter().any(|(v, _)| id_number(v) == Some(*f)))
        };
        if !has_all(&mine) {
            let frame = self.lock().frame;
            mine = self
                .followers_rescan(call, frame)
                .remove(&me)
                .unwrap_or_default();
        }
        self.followers_built(call, id, vars, mine)
    }

    /// Host: the followers in `mine` (instance id, object) of leader `id`
    /// (whose state's variables are `vars`), each with its state and the
    /// leader's variables that name it, named first; and how many were left
    /// to the world as dependencies.
    fn followers_built(
        &self,
        call: &Call<'_>,
        id: &Value,
        vars: &[(String, StateValue)],
        mine: Vec<(Value, u32)>,
    ) -> (Vec<Follower>, u64) {
        let Some(r) = self.h.reflect.as_ref() else {
            return (Vec::new(), 0);
        };
        let mut followers = Vec::new();
        let mut world = 0u64;
        for (fid, o) in mine {
            if !self
                .class_of(call, o, false)
                .is_some_and(Class::may_go_with_an_enemy)
            {
                continue;
            }
            let Ok(object) = u16::try_from(o) else {
                continue;
            };
            if !self.exists(call, &fid) {
                continue;
            }
            // Named by the leader and taken by the world as a dependency
            // copy of its own (the Star Eater's segments name their legs,
            // `leg1` to `leg4`): the client gets the host's, and a follower
            // made there would be refused by the world's create rule.
            let fnum = id_number(&fid);
            let named = vars
                .iter()
                .filter(|(_, v)| matches!(v, StateValue::Unresolved(_) | StateValue::Instance(_)))
                .any(|(n, _)| {
                    state::get_var(call, r, id, n)
                        .as_ref()
                        .and_then(id_number)
                        .is_some_and(|n| Some(n) == fnum)
                });
            if named && self.dep_eligible(call, object) {
                world += 1;
                continue;
            }
            let net_of = |k: usize, v: &Value| self.lock().host.net_id_for(k, v);
            let mut s = state::capture(call, r, &fid, &net_of, &mut Stats::default());
            s.builtins.retain(|(i, _)| {
                BUILTINS
                    .get(usize::from(*i))
                    .is_some_and(|n| FOLLOWER_BUILTINS.contains(n))
            });
            // The leader's variables that hold this follower (sent as a
            // local instance of the host: `Unresolved` of its object).
            let refs = vars
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
        (followers, world)
    }

    /// Client: per follower object, how many instances already count as
    /// `copy`'s (number `me`): only this copy's, live and drawn
    /// ([`counts_as_mine`]; a glow its set-up took from the pool and the
    /// pool took back still names it as `leader`).
    fn followers_had(
        &self,
        call: &Call<'_>,
        copy: &Value,
        me: i64,
        followers: &[Follower],
    ) -> HashMap<u16, usize> {
        let (leader, in_pool) = (self.h.leader, self.h.in_pool);
        let mut have: HashMap<u16, usize> = HashMap::new();
        let _ = copy;
        for f in followers {
            if have.contains_key(&f.object) {
                continue;
            }
            let mut count = 0usize;
            let _ = call.with(&Value::object(u32::from(f.object)), |inst, _| {
                let follows = leader
                    .and_then(|l| inst.get_value(l))
                    .filter(is_instance)
                    .as_ref()
                    .and_then(id_number)
                    == Some(me);
                if !follows {
                    return;
                }
                let pooled = crate::game::truthy(&inst, in_pool).unwrap_or(false);
                let drawn = inst
                    .builtin_value(bv::VISIBLE)
                    .ok()
                    .and_then(|v| v.as_f64())
                    .is_none_or(|v| v > 0.5);
                let counted = counts_as_mine(follows, pooled, drawn);
                count += usize::from(counted);
            });
            have.insert(f.object, count);
        }
        have
    }

    /// Client, as state `seq` of copy `copy` is applied (`first`): when it is
    /// the first, the followers it carries that the copy lacks, made and set
    /// up.
    pub(super) fn followers_make(&self, call: &Call<'_>, copy: &Value, seq: u16, first: &Captured) {
        let followers = if seq == 0 {
            followers_in(first)
        } else {
            Vec::new()
        };
        if followers.is_empty() {
            return;
        }
        let (Some(reflect), Some(create), Some(_)) = (
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
        // Per object: how many the copy has already (its own code made
        // them), so only the rest are made.
        let mut have = self.followers_had(call, copy, me, &followers);
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
            // It followed the host's instance: here it follows the copy (a
            // shot has no net id, so its `leader` came as the host's local).
            let _ = state::set_var(call, reflect, &fid, super::super::names::var::LEADER, copy);
            for name in &f.refs {
                let _ = state::set_var(call, reflect, copy, name, &fid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The top-up counts a follower as the copy's only when it follows
    /// this copy, is live and is drawn: a glow back in its pool or hidden
    /// is made again.
    #[test]
    fn a_follower_counts_only_when_this_copys_live_and_drawn() {
        assert!(counts_as_mine(true, false, true));
        assert!(!counts_as_mine(true, true, true), "back in its pool");
        assert!(!counts_as_mine(true, false, false), "not drawn");
        assert!(!counts_as_mine(false, false, true), "another leader's");
    }

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
            .push(("name".into(), StateValue::Str("x".repeat(900))));
        let all = vec![big.clone(), big.clone(), big.clone(), big, glow()];
        let (sent, left) = fitting(&all, FOLLOWER_BYTES);
        assert_eq!(sent.len() + left as usize, 5);
        assert!(left >= 1);
        let bytes: usize = sent.iter().map(|(_, v)| wire_bytes(v)).sum();
        assert!(bytes <= FOLLOWER_BYTES);
        // The small one still goes after the big ones are cut.
        assert!(sent.iter().any(|(_, v)| decode(v) == Some(glow())));
    }

    /// A shared shot (a Crucible burn and the graphic it keeps in
    /// `follower`) takes its followers as far as its datagram has room: it
    /// still goes in a part alone, and the receiver reads them back from its
    /// built-ins as from a first state's.
    #[test]
    fn a_shared_shot_is_dressed_within_its_datagram() {
        use ndml_net::shots::ShotFire;
        let mut shot = FiredShot {
            id: 7,
            object: 383,
            time_ms: 1000,
            vars: (0..30u16).map(|i| (i, StateValue::Real(0.5))).collect(),
            builtins: vec![(0, StateValue::Real(100.0)), (1, StateValue::Real(50.0))],
            alarms: vec![(0, 30)],
            target: None,
            steer: 0,
            places: Vec::new(),
            place_fills: Vec::new(),
            replaces: true,
            links: Vec::new(),
        };
        let mut big = glow();
        big.state
            .vars
            .push(("name".into(), StateValue::Str("x".repeat(1200))));
        let mut t = FollowersState::default();
        let (sent, left) = dress(&mut shot, &[glow(), big, glow()], 0, &mut t);
        assert_eq!((sent, left), (2, 1));
        let (parts, too_big) = ShotFire::split(vec![shot.clone()]);
        assert!(too_big.is_empty() && parts.len() == 1);
        let back = Captured {
            builtins: shot
                .builtins
                .iter()
                .map(|(i, v)| (*i, v.clone().expanded()))
                .collect(),
            ..Captured::default()
        };
        assert_eq!(followers_in(&back), vec![glow(), glow()]);
    }
}
