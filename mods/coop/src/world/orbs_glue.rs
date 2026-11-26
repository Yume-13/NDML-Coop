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

//! Shared pickups' game side (the rule is in [`crate::rules::orbs`]).
//!
//! * Host: each collectable made goes out as an `OrbSpawn` the next run
//!   frame (kind, place, velocity, `lifespan`, `held`, the `spawnScatter`
//!   magnet factor). Its ship's touch is its collect; a partner's
//!   `OrbCollect` is judged by [`HostOrbs::collect`] and, when credited, the
//!   orb's own pickup runs on the host. Every outcome, and an orb running
//!   out, is broadcast as `OrbCollect`.
//! * Joining side: each `OrbSpawn` becomes a local copy that moves, is
//!   pulled and runs out by itself; touching it runs its own pickup and sends
//!   `OrbCollect` once ([`ClientOrbs::touched`]). Another player's collect
//!   removes the copy.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use ndml_net::msg::{Message, PeerId};
use ndml_sdk::{Builtin, Call, ModContext, Value, Variable};

use super::names::builtin_var as bv;
use super::{ROLE_CLIENT, ROLE_HOST, WorldHooks};
use crate::rules::orbs::{
    self, ClientOrbs, Ended, HeldSpawns, HostOrbs, HostResync, OrbVerdict, SharedPickups,
};
use crate::world::classify::object_index;
use crate::world::lifecycle::Role;

/// Frames between the host's checks for orbs gone by themselves.
const GONE_EVERY: u64 = 10;
/// `ev_collision`.
const EV_COLLISION: f64 = 4.0;
/// Copies made per frame at most (a boss's loot comes in one frame).
const MAX_MADE_PER_FRAME: usize = 64;
/// The script that scatters a drop's collectables (its sixth argument is
/// the magnet factor, 1 when left out).
const SPAWN_SCATTER: &str = "gml_Script_spawnScatter";

struct Handles {
    asset: Option<Builtin>,
    perform: Option<Builtin>,
    lifespan: Option<Variable>,
    /// `oPowers.hyperCharge` (a Serpentine orb's pickup charges it).
    hyper: Option<Variable>,
    alarm_get: Option<Builtin>,
    alarm_set: Option<Builtin>,
    held: Option<Variable>,
    mag: Option<Variable>,
    mag_dist: Option<Variable>,
}

static OH: OnceLock<Handles> = OnceLock::new();

/// A spawn as sent.
#[derive(Debug, Clone, Copy)]
struct Spawn {
    orb_id: u16,
    kind: u8,
    x: f32,
    y: f32,
    hs: f32,
    vs: f32,
    life: f32,
    held: f32,
    scatter: f32,
}

/// Per-session state.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // independent facts: looked up, crediting, applying, down
pub(super) struct OrbsGlue {
    /// Object index per [`orbs::KINDS`] entry, and `oPlayer`.
    objects: Option<(Vec<Option<u32>>, u32)>,
    tried: bool,
    next_id: u16,
    /// Host: made since the last run frame, with its `spawnScatter` factor
    /// (1 outside one).
    made: Vec<(Value, f32)>,
    /// Host: `spawnScatter` calls running (a stack), their factors.
    scatter: Vec<f32>,
    /// Host: orb id -> (instance id, key, kind, `spawnScatter` factor).
    live: HashMap<u16, (Value, usize, u8, f32)>,
    by_key: HashMap<usize, u16>,
    host: HostOrbs,
    /// Host: a partner's pickup being run: the collision hook leaves it alone.
    crediting: bool,
    /// Messages from hooks, sent with the next run frame.
    out: Vec<Message>,
    spawns: HeldSpawns<Spawn>,
    collects: Vec<(u16, PeerId)>,
    copies: HashMap<u16, (Value, usize)>,
    copy_keys: HashMap<usize, u16>,
    client: ClientOrbs,
    /// The shared pickups this game gets ([`SharedPickups`]).
    shared: SharedPickups,
    /// Joining side: `PickupShared` since the last run frame.
    shared_in: Vec<(u16, u8, PeerId)>,
    /// A shared pickup being run here ([`orbs::tables_see`]).
    applying: bool,
    /// This game's ship is down, from the run frame.
    own_down: bool,
    /// Host: what a lost link may have taken, for the return ([`HostResync`]).
    resync: HostResync,
    /// Host: the return's messages, sent with the next run frame.
    resync_out: Vec<Message>,
}

impl OrbsGlue {
    /// This game's ship is down, as the last run frame had it (the on-death
    /// world effects read it too).
    pub(super) fn own_down(&self) -> bool {
        self.own_down
    }

    /// The world's notice of a lost link: a host opens the window of what to
    /// send again at the return. Goes by the world's role, which stays the
    /// host's while the link is down (the session's `is_host` does not).
    fn note_interrupted(&mut self, world_role: Role, frame: u64) {
        if world_role == Role::Host {
            self.resync.interrupted(frame);
        }
    }

    /// Forget everything but the resolved objects.
    pub(super) fn clear(&mut self) {
        let objects = self.objects.take();
        let tried = self.tried;
        *self = Self::default();
        self.objects = objects;
        self.tried = tried;
    }
}

/// Resolve the handles and watch every collectable's pickup.
pub(super) fn register(ctx: &ModContext, w: &std::sync::Arc<WorldHooks>) {
    let _ = OH.set(Handles {
        asset: ctx.builtin("asset_get_index").ok(),
        perform: ctx.builtin("event_perform").ok(),
        lifespan: ctx.variable("lifespan").ok(),
        hyper: ctx.variable("hyperCharge").ok(),
        alarm_get: ctx.builtin("alarm_get").ok(),
        alarm_set: ctx.builtin("alarm_set").ok(),
        held: ctx.variable("held").ok(),
        mag: ctx.variable("mag").ok(),
        mag_dist: ctx.variable("magDist").ok(),
    });
    let c = std::sync::Arc::clone(w);
    super::hook_or_warn(
        SPAWN_SCATTER,
        ctx.hook_before(SPAWN_SCATTER, move |call| c.scatter_before(call)),
    );
    let c = std::sync::Arc::clone(w);
    super::hook_or_warn(
        SPAWN_SCATTER,
        ctx.hook_after(SPAWN_SCATTER, move |_| c.scatter_after()),
    );
    for kind in orbs::KINDS {
        let name = orbs::pickup_event(kind);
        let c = std::sync::Arc::clone(w);
        super::hook_or_warn(
            &name,
            ctx.hook_before(&name, move |call| c.orb_touched(call)),
        );
    }
}

fn f(v: Option<f64>) -> f32 {
    v.map_or(0.0, |v| v as f32)
}

impl WorldHooks {
    fn orb_objects(&self, call: &Call<'_>) -> Option<(Vec<Option<u32>>, u32)> {
        {
            let st = self.lock();
            if st.orbs.objects.is_some() || st.orbs.tried {
                return st.orbs.objects.clone();
            }
        }
        let h = OH.get()?;
        let asset = |n: &str| {
            h.asset
                .and_then(|b| call.call_builtin(b, &[Value::Str(n.into())]).ok())
                .as_ref()
                .and_then(object_index)
        };
        let kinds: Vec<Option<u32>> = orbs::KINDS.iter().map(|k| asset(k)).collect();
        let o = asset("oPlayer").map(|p| (kinds, p));
        let mut st = self.lock();
        st.orbs.tried = true;
        st.orbs.objects.clone_from(&o);
        match &o {
            None => log::warn!("co-op orbs: unable to find oPlayer; shared pickups off"),
            Some((k, _)) => {
                let missing: Vec<&str> = orbs::KINDS
                    .iter()
                    .zip(k)
                    .filter(|(_, o)| o.is_none())
                    .map(|(n, _)| *n)
                    .collect();
                if !missing.is_empty() {
                    log::warn!("co-op orbs: not in this game: {missing:?}");
                }
            }
        }
        o
    }

    /// Host, after a collectable's create (any create path): noted with
    /// the factor of the `spawnScatter` call making it, if any.
    pub(super) fn orb_made(&self, id: Value) {
        let mut st = self.lock();
        if !orbs::tables_see(st.orbs.applying) {
            return;
        }
        let scatter = st.orbs.scatter.last().copied().unwrap_or(1.0);
        st.orbs.made.push((id, scatter));
    }

    /// Host, before `spawnScatter`: its factor (argument 6; 1 when left out
    /// or not a number).
    fn scatter_before(&self, call: &Call<'_>) {
        if self.role() != ROLE_HOST {
            return;
        }
        let factor = call
            .arg_value(5)
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite())
            .map_or(1.0, |v| v as f32);
        self.lock().orbs.scatter.push(factor);
    }

    /// Host, after `spawnScatter`.
    fn scatter_after(&self) {
        if self.role() == ROLE_HOST {
            self.lock().orbs.scatter.pop();
        }
    }

    /// Before a collectable's `Collision_oPlayer`: the host's ship collects
    /// (host), or this game's ship collects a copy (joining side). The
    /// pickup runs either way.
    fn orb_touched(&self, call: &mut Call<'_>) {
        let role = self.role();
        let Some(me) = call.instance() else { return };
        let key = me.key();
        let mut st = self.lock();
        if !orbs::tables_see(st.orbs.applying) {
            return;
        }
        if role == ROLE_HOST {
            if st.orbs.crediting {
                return;
            }
            if let Some(orb_id) = st.orbs.by_key.remove(&key) {
                let kind = st.orbs.live.remove(&orb_id).map(|(_, _, k, _)| k);
                if let Some(m) = st.orbs.host.own(orb_id) {
                    st.orbs.out.push(m);
                    st.orbs.shared.own(orb_id);
                    if let Some(kind) = kind.filter(|&k| orbs::host_shares(k, None)) {
                        st.orbs.out.push(Message::PickupShared {
                            orb_id,
                            kind,
                            by: PeerId::HOST,
                        });
                    }
                }
            }
        } else if role == ROLE_CLIENT
            && let Some(orb_id) = st.orbs.copy_keys.remove(&key)
        {
            st.orbs.copies.remove(&orb_id);
            st.orbs.shared.own(orb_id);
            let me_peer = st.me.unwrap_or(PeerId::HOST);
            if let Some(m) = st.orbs.client.touched(orb_id, me_peer) {
                st.orbs.out.push(m);
            }
        }
    }

    /// Joining side: the orb messages of this frame's inbox, taken out.
    pub(super) fn orbs_client_inbox(&self, inbox: &mut Vec<Message>) {
        let mut st = self.lock();
        inbox.retain(|m| match *m {
            Message::OrbSpawn {
                orb_id,
                kind,
                x,
                y,
                hs,
                vs,
                life,
                held,
                scatter,
                ..
            } => {
                let has_copy = st.orbs.copies.contains_key(&orb_id);
                let is_held = st.orbs.spawns.holds(orb_id);
                if !st.orbs.client.wants_copy(orb_id, has_copy, is_held) {
                    return false;
                }
                st.orbs.spawns.push(
                    orb_id,
                    Spawn {
                        orb_id,
                        kind,
                        x,
                        y,
                        hs,
                        vs,
                        life,
                        held,
                        scatter,
                    },
                );
                false
            }
            Message::OrbCollect { orb_id, peer } => {
                st.orbs.collects.push((orb_id, peer));
                false
            }
            Message::PickupShared { orb_id, kind, by } => {
                st.orbs.shared_in.push((orb_id, kind, by));
                false
            }
            _ => true,
        });
    }

    /// Host, each run frame: new orbs out, partners' collects answered,
    /// orbs gone by themselves broadcast.
    #[allow(clippy::too_many_lines)] // one pass over the frame's orbs
    pub(super) fn orbs_host(
        &self,
        call: &Call<'_>,
        inbox: &[Message],
        own_down: bool,
    ) -> Vec<Message> {
        let Some((kinds, player)) = self.orb_objects(call) else {
            return Vec::new();
        };
        let (frame, frozen, made) = {
            let mut st = self.lock();
            st.orbs.host.frame();
            st.orbs.own_down = own_down;
            let playing = !st.frozen && !own_down;
            st.orbs.shared.frame(playing);
            let made = if st.frozen {
                Vec::new()
            } else {
                std::mem::take(&mut st.orbs.made)
            };
            (st.frame, st.frozen, made)
        };
        for (id, scatter) in made {
            // Gone in its first frame (made and picked up at once).
            let Some((key, obj, x, y, hs, vs, life, held)) = read_orb(call, &id) else {
                continue;
            };
            let kind = obj.and_then(|o| kinds.iter().position(|k| *k == Some(o)));
            let mut st = self.lock();
            let Some(kind) = kind.and_then(|k| u8::try_from(k).ok()) else {
                continue;
            };
            st.orbs.next_id = st.orbs.next_id.wrapping_add(1).max(1);
            let orb_id = st.orbs.next_id;
            if let Some((_, old_key, _, _)) = st.orbs.live.insert(orb_id, (id, key, kind, scatter))
            {
                st.orbs.by_key.remove(&old_key);
            }
            st.orbs.by_key.insert(key, orb_id);
            st.orbs.host.spawned(orb_id, kind);
            st.orbs.out.push(Message::OrbSpawn {
                orb_id,
                kind,
                x,
                y,
                value: 0.0,
                hs,
                vs,
                life,
                held,
                scatter,
            });
        }
        for m in inbox {
            let Message::OrbCollect { orb_id, peer } = *m else {
                continue;
            };
            if peer == PeerId::HOST {
                continue;
            }
            let (verdict, kind) = {
                let mut st = self.lock();
                (
                    st.orbs.host.collect(orb_id, peer),
                    st.orbs.host.kind(orb_id),
                )
            };
            let answer = match verdict {
                OrbVerdict::Credit => {
                    self.orb_credit(call, orb_id, player);
                    peer
                }
                OrbVerdict::Taken { by, .. } => by,
                OrbVerdict::Expired | OrbVerdict::Unknown => PeerId::HOST,
            };
            let mut st = self.lock();
            st.orbs.out.push(Message::OrbCollect {
                orb_id,
                peer: answer,
            });
            // A partner's pickup of a shared kind: the effect for every other
            // player too.
            if let Some(kind) = kind.filter(|&k| orbs::host_shares(k, Some(verdict))) {
                st.orbs.out.push(Message::PickupShared {
                    orb_id,
                    kind,
                    by: peer,
                });
                st.orbs.shared.offer(orb_id, kind, peer, PeerId::HOST);
            }
        }
        self.apply_shared(call, &kinds, player);
        // Gone by themselves (run out): the copies go too.
        if !frozen && frame % GONE_EVERY == 0 {
            let live: Vec<(u16, Value)> = self
                .lock()
                .orbs
                .live
                .iter()
                .map(|(n, (id, _, _, _))| (*n, id.clone()))
                .collect();
            for (orb_id, id) in live {
                if self.presence(call, &id) == super::Presence::Gone {
                    let mut st = self.lock();
                    if let Some((_, key, _, _)) = st.orbs.live.remove(&orb_id) {
                        st.orbs.by_key.remove(&key);
                    }
                    st.orbs.host.gone(orb_id);
                    st.orbs.out.push(Message::OrbCollect {
                        orb_id,
                        peer: PeerId::HOST,
                    });
                }
            }
        }
        let mut st = self.lock();
        let mut out = std::mem::take(&mut st.orbs.out);
        for e in out.iter().filter_map(Ended::of) {
            let _ = st.orbs.resync.record(frame, e);
        }
        out.append(&mut st.orbs.resync_out);
        out
    }

    /// At the world's notice of a lost link ([`OrbsGlue::note_interrupted`]).
    pub(super) fn orbs_interrupted(&self) {
        let mut st = self.lock();
        let (role, frame) = (st.life.role(), st.frame);
        st.orbs.note_interrupted(role, frame);
    }

    /// The link back. Host: the collects and shared pickups the dead link
    /// may have taken ([`HostResync`]), then every live orb as it is now.
    /// Joining side: its unanswered collects sent again, before it can touch
    /// anything new; spawns it already has are skipped
    /// ([`ClientOrbs::wants_copy`]).
    pub(super) fn orbs_resumed(&self, call: &Call<'_>, is_host: bool) {
        if !is_host {
            let mut st = self.lock();
            let me = st.me.unwrap_or(PeerId::HOST);
            let again = st.orbs.client.resend(me);
            st.orbs.out.extend(again);
            return;
        }
        let (live, ended) = {
            let mut st = self.lock();
            let live: Vec<(u16, Value, u8, f32)> = st
                .orbs
                .live
                .iter()
                .map(|(n, (id, _, k, sc))| (*n, id.clone(), *k, *sc))
                .collect();
            (live, st.orbs.resync.resumed())
        };
        let mut msgs: Vec<Message> = ended.iter().map(|e| e.message()).collect();
        for (orb_id, id, kind, scatter) in live {
            if let Some((_, _, x, y, hs, vs, life, held)) = read_orb(call, &id) {
                msgs.push(Message::OrbSpawn {
                    orb_id,
                    kind,
                    x,
                    y,
                    value: 0.0,
                    hs,
                    vs,
                    life,
                    held,
                    scatter,
                });
            }
        }
        self.lock().orbs.resync_out.extend(msgs);
    }

    /// Host: a partner's collect credited. A bounty orb's own pickup runs as
    /// the orb (score, XP, flash, sound; it destroys itself); a power-up's
    /// orb is removed (its effect was the collector's).
    fn orb_credit(&self, call: &Call<'_>, orb_id: u16, player: u32) {
        let entry = {
            let mut st = self.lock();
            let e = st.orbs.live.remove(&orb_id);
            if let Some((_, key, _, _)) = &e {
                st.orbs.by_key.remove(key);
            }
            e
        };
        let Some((id, _, kind, _)) = entry else {
            return;
        };
        // A freeze or the region left it deactivated: bring it back first.
        if self.presence(call, &id) == super::Presence::Asleep
            && let Some(a) = self.h.activate
        {
            let _ = call.call_builtin(a, std::slice::from_ref(&id));
        }
        let perform = OH.get().and_then(|h| h.perform);
        if orbs::scored(kind)
            && let Some(p) = perform
        {
            // What the pickup gives the collecting player's build was given
            // on the partner's game; the host's own is put back.
            let kept = Self::collector_state(call, kind);
            self.lock().orbs.crediting = true;
            let mut ran = false;
            let _ = call.with(&id, |i, _| {
                ran = i
                    .call_builtin(p, &[Value::Real(EV_COLLISION), Value::object(player)])
                    .is_ok();
            });
            self.lock().orbs.crediting = false;
            Self::collector_restore(call, kept);
            if ran {
                // Its pickup destroys it; if not, it goes now.
                self.destroy_silently(call, &id);
                return;
            }
        }
        self.destroy_silently(call, &id);
    }

    /// Host, before running a partner's Serpentine pickup: `oPowers`'
    /// `hyperCharge` and alarm 5 (the pickup sets both); `None` for other kinds.
    fn collector_state(call: &Call<'_>, kind: u8) -> Option<(Value, Option<f64>, f64)> {
        if orbs::KINDS.get(usize::from(kind)) != Some(&"oBountySerpentine") {
            return None;
        }
        let h = OH.get()?;
        let (hyper, asset) = (h.hyper?, h.asset?);
        let powers = call
            .call_builtin(asset, &[Value::Str("oPowers".into())])
            .ok()?;
        let mut out = None;
        let _ = call.with(&powers, |p, _| {
            let alarm = h
                .alarm_get
                .and_then(|b| p.call_builtin(b, &[Value::Real(5.0)]).ok())
                .and_then(|v| v.as_f64())
                .unwrap_or(-1.0);
            out = Some((p.get_f64(hyper), alarm));
        });
        out.map(|(v, a)| (powers, v, a))
    }

    /// Host, after it: the host's own put back.
    fn collector_restore(call: &Call<'_>, kept: Option<(Value, Option<f64>, f64)>) {
        let (Some((powers, hyper, alarm)), Some(h)) = (kept, OH.get()) else {
            return;
        };
        let _ = call.with(&powers, |p, _| {
            if let (Some(var), Some(v)) = (h.hyper, hyper) {
                let _ = p.set_f64(var, v);
            }
            if let Some(b) = h.alarm_set {
                let _ = p.call_builtin(b, &[Value::Real(5.0), Value::Real(alarm)]);
            }
        });
    }

    /// Joining side, each run frame (after the inbox): copies made for the
    /// host's new orbs, removed for another player's collect; this game's
    /// collects to the host.
    #[allow(clippy::too_many_lines)] // one pass over the frame's copies
    pub(super) fn orbs_client(&self, call: &Call<'_>, me: PeerId, own_down: bool) -> Vec<Message> {
        if self.role() != ROLE_CLIENT {
            return Vec::new();
        }
        let Some((kinds, player)) = self.orb_objects(call) else {
            return Vec::new();
        };
        let (frame, collects, spawns) = {
            let mut st = self.lock();
            st.orbs.client.frame();
            st.orbs.own_down = own_down;
            let playing = !st.frozen && !own_down;
            st.orbs.shared.frame(playing);
            // After this game's own collects of the frame, so a contested
            // shared pickup is known as its own.
            for (orb_id, kind, by) in std::mem::take(&mut st.orbs.shared_in) {
                st.orbs.shared.offer(orb_id, kind, by, me);
            }
            let collects = std::mem::take(&mut st.orbs.collects);
            // An orb gone while its spawn is held is never made; nothing is
            // made while frozen.
            for &(orb_id, _) in &collects {
                st.orbs.spawns.gone(orb_id);
            }
            let frozen = st.frozen;
            let spawns = st.orbs.spawns.take(frozen, MAX_MADE_PER_FRAME);
            (st.frame, collects, spawns)
        };
        for (orb_id, by) in collects {
            let remove = self.lock().orbs.client.on_collect(orb_id, by, me);
            if !remove {
                continue;
            }
            let copy = {
                let mut st = self.lock();
                let c = st.orbs.copies.remove(&orb_id);
                if let Some((_, key)) = &c {
                    st.orbs.copy_keys.remove(key);
                }
                c
            };
            if let Some((id, _)) = copy {
                self.destroy_silently(call, &id);
            }
        }
        let h = OH.get();
        let lifespan = h.and_then(|h| h.lifespan);
        let held_var = h.and_then(|h| h.held);
        let magnet = [h.and_then(|h| h.mag), h.and_then(|h| h.mag_dist)];
        let create = self.h.instance_create;
        for s in spawns {
            let Some(obj) = kinds.get(usize::from(s.kind)).copied().flatten() else {
                continue;
            };
            let Some(create) = create else {
                return Vec::new();
            };
            self.replicating
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let made = call
                .call_script(
                    create,
                    &[
                        f64::from(s.x).into(),
                        f64::from(s.y).into(),
                        Value::object(obj),
                    ],
                )
                .ok()
                .filter(super::is_instance);
            self.replicating
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let mut key = None;
            if let Some(id) = &made {
                let _ = call.with(id, |i, _| {
                    key = Some(i.key());
                    let _ = i.set_builtin_f64(bv::HSPEED, f64::from(s.hs));
                    let _ = i.set_builtin_f64(bv::VSPEED, f64::from(s.vs));
                    if s.life >= 0.0
                        && let Some(v) = lifespan
                    {
                        let _ = i.set_f64(v, f64::from(s.life));
                    }
                    if s.held.is_finite()
                        && let Some(v) = held_var
                    {
                        let _ = i.set_f64(v, f64::from(s.held));
                    }
                    // As `spawnScatter` does: this copy's own magnet times the factor.
                    if s.scatter.is_finite() && (s.scatter - 1.0).abs() > f32::EPSILON {
                        for v in magnet.into_iter().flatten() {
                            if let Some(m) = i.get_f64(v) {
                                let _ = i.set_f64(v, m * f64::from(s.scatter));
                            }
                        }
                    }
                });
            }
            if let (Some(id), Some(k)) = (made, key) {
                let mut st = self.lock();
                st.orbs.copies.insert(s.orb_id, (id, k));
                st.orbs.copy_keys.insert(k, s.orb_id);
            }
        }
        // Copies gone by themselves (run out here first).
        if frame % GONE_EVERY == 0 {
            let copies: Vec<(u16, Value)> = self
                .lock()
                .orbs
                .copies
                .iter()
                .map(|(n, (id, _))| (*n, id.clone()))
                .collect();
            for (orb_id, id) in copies {
                if self.presence(call, &id) == super::Presence::Gone {
                    let mut st = self.lock();
                    if let Some((_, key)) = st.orbs.copies.remove(&orb_id) {
                        st.orbs.copy_keys.remove(&key);
                    }
                }
            }
        }
        self.apply_shared(call, &kinds, player);
        std::mem::take(&mut self.lock().orbs.out)
    }

    /// Each run frame, both roles: the next queued shared pickup applied to
    /// this game's ship once it can use it (not frozen, not down). An
    /// instance of its kind is made at the ship, its `Collision_oPlayer`
    /// performed and the rest removed in this call: never drawn, never in
    /// the orb tables or on the wire ([`orbs::tables_see`]).
    fn apply_shared(&self, call: &Call<'_>, kinds: &[Option<u32>], player: u32) {
        let next = {
            let mut st = self.lock();
            let running = !st.frozen && !st.orbs.own_down;
            st.orbs.shared.next(running)
        };
        let Some(kind) = next else { return };
        let mut at = None;
        let _ = call.with(&Value::object(player), |i, _| {
            at = at.or(Some((i.builtin_f64(bv::X), i.builtin_f64(bv::Y))));
        });
        let Some((Some(x), Some(y))) = at else {
            self.lock().orbs.shared.put_back(kind);
            return;
        };
        let (Some(obj), Some(create), Some(perform)) = (
            kinds.get(usize::from(kind)).copied().flatten(),
            self.h.instance_create,
            OH.get().and_then(|h| h.perform),
        ) else {
            return;
        };
        self.lock().orbs.applying = true;
        // Past the joining game's create veto, as a copy is.
        self.replicating
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let made = call
            .call_script(create, &[x.into(), y.into(), Value::object(obj)])
            .ok()
            .filter(super::is_instance);
        self.replicating
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(id) = &made {
            let _ = call.with(id, |i, _| {
                let _ =
                    i.call_builtin(perform, &[Value::Real(EV_COLLISION), Value::object(player)]);
            });
            self.destroy_silently(call, id);
        }
        self.lock().orbs.applying = false;
    }
}

/// An orb as `OrbSpawn` sends it: its key, object, place, speed, timer and
/// `held`.
type OrbRead = (usize, Option<u32>, f32, f32, f32, f32, f32, f32);

/// Host: read an orb (`None`: gone).
fn read_orb(call: &Call<'_>, id: &Value) -> Option<OrbRead> {
    let lifespan = OH.get().and_then(|h| h.lifespan);
    let held_var = OH.get().and_then(|h| h.held);
    let mut read = None;
    let _ = call.with(id, |i, _| {
        let obj = i
            .builtin_value(bv::OBJECT_INDEX)
            .ok()
            .as_ref()
            .and_then(object_index);
        read = Some((
            i.key(),
            obj,
            f(i.builtin_f64(bv::X)),
            f(i.builtin_f64(bv::Y)),
            f(i.builtin_f64(bv::HSPEED)),
            f(i.builtin_f64(bv::VSPEED)),
            lifespan
                .and_then(|v| i.get_f64(v))
                .map_or(-1.0, |v| v as f32),
            held_var
                .and_then(|v| i.get_f64(v))
                .map_or(f32::NAN, |v| v as f32),
        ));
    });
    read
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::lifecycle::{LifeInput, Lifecycle, Loss, Transition};

    /// The host's lost-link notice opens the resync window although the
    /// session, unlinked, no longer says this game hosts.
    #[test]
    fn the_hosts_lost_link_opens_the_resync_window() {
        let session = |linked: bool, loss| LifeInput {
            run: Some((1, Some(1.0))),
            coop_run: true,
            linked,
            // `Core::is_host`: `linked() && me().is_host()`.
            is_host: linked,
            enabled: true,
            loss,
        };
        let mut life = Lifecycle::default();
        assert_eq!(
            life.update(&session(true, None)),
            Transition::Start(Role::Host)
        );
        let mut orbs = OrbsGlue::default();
        let lost = Ended::Collect {
            orb_id: 3,
            peer: PeerId::HOST,
        };
        orbs.resync.record(1000, lost);
        let t = life.update(&session(false, Some(Loss::Interrupted)));
        assert_eq!(t, Transition::Interrupted);
        orbs.note_interrupted(life.role(), 1200);
        assert_eq!(orbs.resync.resumed(), vec![lost], "the window opened");
        // A joining game opens nothing.
        let mut orbs = OrbsGlue::default();
        orbs.resync.record(1000, lost);
        orbs.note_interrupted(Role::Client, 1200);
        assert!(orbs.resync.resumed().is_empty());
    }
}
