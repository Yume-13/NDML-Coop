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

//! Dependencies: non-enemy instances a replicated enemy refers to and needs
//! (a station's nodes and walls destroy themselves without their core).
//!
//! * Host: an instance a replicated enemy's state refers to, or the
//!   non-enemy that made it, is a dependency when its object is not local by
//!   ancestry or player family and it was made after the run started. It is
//!   then tracked like an enemy (spawn flagged [`EnemySpawn::DEPENDENCY`])
//!   and ticked by the run step. A maker is named in the first state
//!   ([`MAKER_BUILTIN`]) so the client makes it first.
//! * Client: a dependency spawn is made like an enemy and ticked by the run
//!   step; its local creates of that kind are vetoed.
//!
//! [`EnemySpawn::DEPENDENCY`]: ndml_net::msg::EnemySpawn::DEPENDENCY
//! [`MAKER_BUILTIN`]: super::super::state::MAKER_BUILTIN

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use ndml_net::msg::NetId;
use ndml_sdk::{Call, Value};

use super::{Class, WorldHooks};

/// GML `all` (a `with` target).
const ALL: f64 = -3.0;

/// Dependency bookkeeping (both roles).
#[derive(Debug, Default)]
pub(super) struct Deps {
    /// Host: dependencies by instance key (their ids), ticked by the run step.
    pub host: HashMap<usize, Value>,
    /// Host: the highest instance id when the run started (`None`: not read
    /// yet, no dependency is taken).
    pub watermark: Option<f64>,
    /// Host: objects judged (eligible or not by ancestry).
    eligible: HashMap<u16, bool>,
    /// Host: the non-enemy making an enemy now (a stack: creates nest).
    pub makers_pending: Vec<(usize, u16, Value)>,
    /// Host: the enemies non-enemies made, by key: (maker key, object, id,
    /// frame).
    makers: HashMap<usize, (usize, u16, Value, u64)>,
    /// Host: instances the wave code made (world spawns, by key): what they
    /// make is a world spawn too.
    pub world: HashSet<usize>,
    /// Host: world spawns being made now (creates nest), and the ones made
    /// inside another's create, taken after it.
    world_depth: u32,
    world_pending: Vec<(usize, u16, Value)>,
    /// Host: `oSpawner` (its family stays each game's own), once looked up.
    spawner_root: Option<u32>,
    spawner_looked_up: bool,
    /// Client: objects that arrived as dependencies.
    pub kinds: HashSet<u32>,
    /// Client: dependency copies (net id, id), ticked by the run step.
    pub client: Vec<(NetId, Value)>,
    /// Client: they are being ticked now (at the run step), so none rides
    /// its leader's last tick (`put_back::plan`).
    pub client_ticking: bool,
}

impl Deps {
    /// A new run.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// An instance id as a number (ids are refs or numbers).
fn id_number(v: &Value) -> Option<f64> {
    match v {
        Value::Ref(p) => Some(f64::from((*p & 0xFFFF_FFFF) as u32)),
        other => other.as_f64(),
    }
}

impl WorldHooks {
    /// Host, at the run's start: the highest instance id now.
    pub(super) fn deps_run_start(&self, call: &Call<'_>) {
        let mut top: Option<f64> = None;
        let n = call.with(&Value::Real(ALL), |inst, _| {
            if let Some(id) = inst.builtin_value("id").ok().as_ref().and_then(id_number) {
                top = Some(top.map_or(id, |t| t.max(id)));
            }
        });
        log::info!(
            "co-op world: dependencies: instances made after id {top:?} may be replicated ({n:?} instances now)"
        );
        self.lock().deps.watermark = top;
    }

    /// Whether instances of `obj` may be dependencies (by ancestry; cached).
    pub(super) fn dep_eligible(&self, call: &Call<'_>, obj: u16) -> bool {
        if let Some(e) = self.lock().deps.eligible.get(&obj) {
            return *e;
        }
        let roots = self
            .lock()
            .objects
            .as_ref()
            .map_or_default(|o| o.local_roots.clone());
        let o = u32::from(obj);
        let e = self.class_of(call, o, true) == Some(Class::Other)
            && !roots.iter().any(|&r| self.ancestor(call, o, Some(r)))
            && !self.player_family(call, o);
        self.lock().deps.eligible.insert(obj, e);
        e
    }

    /// Whether the instance `id` is player-side by the mirror's rule (no
    /// mirror linked: not).
    fn mirror_player_side(&self, call: &Call<'_>, id: &Value) -> bool {
        let Some(side) = self.shot.side.get() else {
            return false;
        };
        let mut player = false;
        let _ = call.with(id, |inst, _| player = (side.0)(call, &inst));
        player
    }

    /// Whether `obj` is a player family member by name
    /// ([`super::classify::is_player_family_name`]); `true` when unknown.
    fn player_family(&self, call: &Call<'_>, obj: u32) -> bool {
        let Some(parent) = self.h.object_parent else {
            return true;
        };
        let mut chain = Vec::new();
        let mut cur = Some(obj);
        while let Some(o) = cur {
            if chain.len() > 16 {
                break;
            }
            chain.push(self.object_name(call, o));
            cur = call
                .call_builtin(parent, &[Value::object(o)])
                .ok()
                .and_then(|p| super::object_index(&p))
                // `object_get_parent` gives -100 (no parent) as a number.
                .filter(|&p| p != o && p <= 0xFFFF);
        }
        let refs: Vec<&str> = chain.iter().map(String::as_str).collect();
        super::classify::is_player_family_name(&refs)
    }

    /// Host, after a create whose maker `deps.makers_pending` holds: note
    /// the new enemy's maker.
    pub(super) fn dep_maker_made(&self, call: &Call<'_>) {
        let Some(maker) = self.lock().deps.makers_pending.pop() else {
            return;
        };
        let Some(id) = call.result().filter(super::is_instance) else {
            return;
        };
        let mut key = None;
        let _ = call.with(&id, |inst, _| key = Some(inst.key()));
        let Some(key) = key else { return };
        let mut st = self.lock();
        let frame = st.frame;
        st.deps
            .makers
            .retain(|_, m| frame.saturating_sub(m.3) < 600);
        st.deps
            .makers
            .insert(key, (maker.0, maker.1, maker.2, frame));
    }

    /// Host: the non-enemy that made the enemy at `key`, if one did (taken).
    pub(super) fn take_maker(&self, key: usize) -> Option<(usize, u16, Value)> {
        self.lock()
            .deps
            .makers
            .remove(&key)
            .map(|m| (m.0, m.1, m.2))
    }

    /// Host: take the dependencies among `refs` (what a capture found not
    /// replicated). Returns how many were taken now.
    pub(super) fn take_deps(&self, call: &Call<'_>, refs: &[(usize, u16, Value)]) -> usize {
        if refs.is_empty() {
            return 0;
        }
        let Some(mark) = self.lock().deps.watermark else {
            return 0;
        };
        let mut taken = 0;
        for (key, obj, id) in refs {
            if self.lock().host.is_tracked(*key) || !self.exists(call, id) {
                continue;
            }
            if !self.dep_eligible(call, *obj) {
                continue;
            }
            if id_number(id).is_none_or(|n| n <= mark) {
                continue;
            }
            // Player-side by the mirror's rule: the mirror draws it on the
            // partner's screen already.
            if self.mirror_player_side(call, id) {
                continue;
            }
            self.lock().deps.host.insert(*key, id.clone());
            // As its own tick would: tracked, spawn and first state queued.
            let _ = call.with(id, |inst, _| self.host_tick(&inst));
            if self.lock().host.is_tracked(*key) {
                taken += 1;
            } else {
                self.lock().deps.host.remove(key);
            }
        }
        taken
    }

    /// Host, at the run step before `begin_frame`: tick each dependency (no
    /// `oSolid` tick of its own); not while frozen. Untracked ones are forgotten.
    pub(super) fn tick_deps_host(&self, call: &Call<'_>) {
        if self.lock().frozen {
            return;
        }
        let deps: Vec<(usize, Value)> = self
            .lock()
            .deps
            .host
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        for (_, id) in &deps {
            let _ = call.with(id, |inst, _| self.host_tick(&inst));
        }
        let mut st = self.lock();
        for (key, _) in deps {
            if !st.host.is_tracked(key) {
                st.deps.host.remove(&key);
            }
        }
        // Untracked world spawns are forgotten too: a key is an address the
        // game reuses for later instances.
        let st = &mut *st;
        let host = &st.host;
        st.deps.world.retain(|k| host.is_tracked(*k));
    }

    /// Client: a dependency copy was made.
    pub(super) fn dep_made(&self, net: NetId, obj: u32, id: Value) {
        let mut st = self.lock();
        st.deps.kinds.insert(obj);
        st.deps.client.push((net, id));
    }

    /// Client, at the run step: tick each dependency copy as its `oSolid`
    /// tick would; not while frozen. Unknown ones are forgotten.
    pub(super) fn tick_deps_client(&self, call: &Call<'_>) {
        let deps = {
            let st = self.lock();
            if st.frozen {
                return;
            }
            st.deps.client.clone()
        };
        if deps.is_empty() {
            return;
        }
        self.lock().deps.client_ticking = true;
        for (_, id) in &deps {
            let _ = call.with(id, |inst, _| self.client_tick(call, &inst));
        }
        let mut st = self.lock();
        st.deps.client_ticking = false;
        let st = &mut *st;
        let client = &st.client;
        st.deps.client.retain(|(n, _)| client.is_known(*n));
    }

    /// Client: whether `obj` arrived as a dependency.
    pub(super) fn is_dep_kind(&self, obj: u32) -> bool {
        self.lock().deps.kinds.contains(&obj)
    }

    /// Host, before a create of a non-enemy `obj`: whether it is a world
    /// spawn, made by the wave director (`oWaves`), by a world spawn or
    /// inside one's create, and replicable (not local, not a spawner).
    pub(super) fn world_spawn_before(&self, call: &Call<'_>, obj: u32) -> bool {
        if self.lock().deps.watermark.is_none() {
            return false;
        }
        let Ok(o) = u16::try_from(obj) else {
            return false;
        };
        let waves = self.lock().objects.as_ref().and_then(|o| o.waves);
        let nested = self.lock().deps.world_depth > 0;
        let by_world = nested
            || call.instance().is_some_and(|c| {
                self.lock().deps.world.contains(&c.key())
                    || (waves.is_some() && super::object_of(&c) == waves)
            });
        if !by_world || !self.dep_eligible(call, o) || self.is_spawner(call, obj) {
            return false;
        }
        if super::death_fx::dying_world_spawn() {
            return false;
        }
        self.lock().deps.world_depth += 1;
        true
    }

    fn is_spawner(&self, call: &Call<'_>, obj: u32) -> bool {
        let looked_up = self.lock().deps.spawner_looked_up;
        if !looked_up {
            let r = self.asset(call, "oSpawner");
            let mut st = self.lock();
            st.deps.spawner_root = r;
            st.deps.spawner_looked_up = true;
        }
        let root = self.lock().deps.spawner_root;
        self.ancestor(call, obj, root)
    }

    /// Host, after a world spawn's create: taken as a dependency (one made
    /// inside another's create after that one, so its maker comes first).
    pub(super) fn world_spawn_after(&self, call: &Call<'_>) {
        let depth = {
            let mut st = self.lock();
            st.deps.world_depth = st.deps.world_depth.saturating_sub(1);
            st.deps.world_depth
        };
        let made = call
            .result()
            .filter(super::classify::is_instance)
            .and_then(|id| {
                let mut found = None;
                let _ = call.with(&id, |inst, _| {
                    found = inst
                        .builtin_value("object_index")
                        .ok()
                        .as_ref()
                        .and_then(super::classify::object_index)
                        .and_then(|o| u16::try_from(o).ok())
                        .map(|o| (inst.key(), o));
                });
                found.map(|(k, o)| (k, o, id))
            });
        if depth > 0 {
            if let Some(m) = made {
                self.lock().deps.world_pending.push(m);
            }
            return;
        }
        let pending = std::mem::take(&mut self.lock().deps.world_pending);
        for (key, obj, id) in made.into_iter().chain(pending) {
            self.lock().deps.world.insert(key);
            self.take_deps(call, &[(key, obj, id)]);
        }
    }
}
