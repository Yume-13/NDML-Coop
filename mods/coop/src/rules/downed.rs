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

//! What a down does to what follows the ship: it is gone until the respawn,
//! as in a solo death, while the hidden ship stays.
//!
//! At the down, every instance of a loadout object (`refreshLoadout`) whose
//! variables refer to the ship or to an instance already taken is deactivated,
//! and so are the `oConstructDrone`s (the ship's Destroy event zeroes their
//! hull). Shots, mines and allies play on. The pause and upgrade screens
//! reactivate the room, so the run tick deactivates them again
//! ([`Downed::hold`]); around the game's own rebuilds they come back and the
//! rule runs again after. The revive reactivates everything and waits a frame
//! before its `refreshLoadout`.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ndml_sdk::abi::OBJECT_REF_TAG;
use ndml_sdk::{Builtin, Call, Instance, ModContext, Value};

use crate::world::state::{self, Reflect, is_instance_ref};

/// The object the ship's Destroy event zeroes the hull of
/// (`oPlayer_Destroy_0`: `with (oConstructDrone) hull = 0`).
const DIES_WITH_SHIP: &str = "oConstructDrone";
/// `all`, for `with`.
const ALL: f64 = -3.0;

/// What the down deactivated.
#[derive(Debug, Default)]
struct Gone {
    /// (instance id, object).
    ids: Vec<(Value, u32)>,
    /// Times they were deactivated again after the game reactivated them.
    again: u32,
}

/// Hooks and state (see the module docs).
#[derive(Debug)]
pub struct Downed {
    reflect: Option<Reflect>,
    asset_get_index: Option<Builtin>,
    object_get_name: Option<Builtin>,
    deactivate: Option<Builtin>,
    activate: Option<Builtin>,
    /// `refreshLoadout` calls in progress.
    refresh_depth: AtomicU32,
    /// Objects of the instances `refreshLoadout` made (run time).
    loadout_made: Mutex<HashSet<u32>>,
    gone: Mutex<Option<Gone>>,
    /// The game's own rebuilds of what follows the ship in progress
    /// (`refreshLoadout`, `refreshDefenses`).
    rebuild_depth: AtomicU32,
    /// At the outermost rebuild's start while down: whether `with` saw the
    /// reactivated instances at once.
    rebuild_seen: AtomicBool,
    /// While down, the game made an instance of an object that follows the
    /// ship (construct assembly refills the drones); the next run tick
    /// applies the rule to it.
    fresh: AtomicBool,
    /// [`DIES_WITH_SHIP`]'s object index (looked up at the down).
    dies_obj: Mutex<Option<u32>>,
}

impl Downed {
    #[must_use]
    pub fn resolve(ctx: &ModContext) -> Self {
        let b = |n: &str| ctx.builtin(n).ok();
        Self {
            reflect: Reflect::resolve(ctx),
            asset_get_index: b("asset_get_index"),
            object_get_name: b("object_get_name"),
            deactivate: b("instance_deactivate_object"),
            activate: b("instance_activate_object"),
            refresh_depth: AtomicU32::new(0),
            loadout_made: Mutex::new(HashSet::new()),
            gone: Mutex::new(None),
            rebuild_depth: AtomicU32::new(0),
            rebuild_seen: AtomicBool::new(false),
            fresh: AtomicBool::new(false),
            dies_obj: Mutex::new(None),
        }
    }

    /// The run ended (the menu): a down of that run is over with it. A game
    /// over otherwise leaves `gone` set, and the next run's first
    /// `refreshLoadout` would be taken for a rebuild while down.
    pub fn end_run(&self) {
        let was = lock(&self.gone).take();
        self.fresh.store(false, Ordering::Relaxed);
        self.rebuild_seen.store(false, Ordering::Relaxed);
        *lock(&self.dies_obj) = None;
        if let Some(g) = was {
            log::info!(
                "co-op rules: down: the run ended with the ship down; {} instances of it forgotten",
                g.ids.len()
            );
        }
    }

    /// Whether a down holds what follows the ship.
    #[must_use]
    pub fn is_down(&self) -> bool {
        lock(&self.gone).is_some()
    }

    /// `refreshLoadout` starts (hooked before it).
    pub fn refresh_enter(&self) {
        self.refresh_depth.fetch_add(1, Ordering::Relaxed);
    }

    /// `refreshLoadout` ends (hooked after it).
    pub fn refresh_exit(&self) {
        let _ = self
            .refresh_depth
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |d| d.checked_sub(1));
    }

    /// After `instance_create_depth`/`_layer`: inside `refreshLoadout`, the
    /// new instance's object is one the loadout makes.
    /// While down (outside the game's rebuilds, which [`Self::rebuild_exit`]
    /// covers), an instance of an object that follows the ship marks the
    /// next run tick to apply the rule again.
    pub fn after_create(&self, call: &Call<'_>) {
        let loadout = self.refresh_depth.load(Ordering::Relaxed) > 0;
        let down = self.rebuild_depth.load(Ordering::Relaxed) == 0 && lock(&self.gone).is_some();
        if !loadout && !down {
            return;
        }
        let Some(id) = call.result() else { return };
        let mut obj = None;
        let _ = call.with(&id, |inst, _| obj = object_of(&inst));
        let Some(o) = obj else { return };
        if loadout {
            lock(&self.loadout_made).insert(o);
        }
        if down && (lock(&self.loadout_made).contains(&o) || *lock(&self.dies_obj) == Some(o)) {
            self.fresh.store(true, Ordering::Relaxed);
        }
    }

    /// Before `refreshLoadout` and `refreshDefenses`: while down, what the down
    /// took comes back for the rebuild. A level-up's `refreshDefenses` replaces
    /// the chain from the ship's links; a deactivated old chain would stay and
    /// return as a second tail at the respawn.
    pub fn rebuild_enter(&self, call: &Call<'_>) {
        if self.rebuild_depth.fetch_add(1, Ordering::Relaxed) > 0 {
            return;
        }
        let ids: Vec<Value> = match lock(&self.gone).as_ref() {
            Some(g) => g.ids.iter().map(|(id, _)| id.clone()).collect(),
            None => return,
        };
        let Some(activate) = self.activate else {
            return;
        };
        for id in &ids {
            if let Err(e) = call.call_builtin(activate, std::slice::from_ref(id)) {
                log::warn!("co-op rules: down: instance_activate_object({id:?}): {e}");
            }
        }
        let seen = ids.iter().filter(|id| active(call, id)).count();
        self.rebuild_seen
            .store(seen == ids.len(), Ordering::Relaxed);
        log::info!(
            "co-op rules: down: the game rebuilds what follows the ship; {} reactivated for it, {seen} active at once",
            ids.len()
        );
    }

    /// The rebuild is over (hooked after it): while down, the rule again.
    pub fn rebuild_exit(&self, call: &Call<'_>) {
        let was = self
            .rebuild_depth
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |d| d.checked_sub(1))
            .unwrap_or(0);
        if was != 1 {
            return;
        }
        let Some(deactivate) = self.deactivate else {
            return;
        };
        let Some(old) = lock(&self.gone).take() else {
            return;
        };
        // What the rebuild destroyed is no longer found; when the
        // reactivation was not seen at once, everything is kept.
        let seen = self.rebuild_seen.load(Ordering::Relaxed);
        let mut gone = Gone {
            ids: Vec::new(),
            again: old.again,
        };
        let mut ended = 0;
        for (id, o) in old.ids {
            if !seen || active(call, &id) {
                gone.ids.push((id, o));
            } else {
                ended += 1;
            }
        }
        for (id, _) in &gone.ids {
            let _ = call.call_builtin(deactivate, std::slice::from_ref(id));
        }
        let kept = gone.ids.len();
        *lock(&self.gone) = Some(gone);
        let new = self.take_new(call);
        log::info!(
            "co-op rules: down: after the rebuild {kept} deactivated again, {ended} ended by it, new: {new}"
        );
    }

    /// What follows the ship now: the loadout's instances its links reach,
    /// and the drones. `None` (logged) when the rule cannot be applied.
    fn reach(&self, call: &Call<'_>) -> Option<Vec<(Value, u32)>> {
        let r = self.reflect?;
        let Some((ship_id, ship_obj)) = self.ship(call) else {
            log::warn!("co-op rules: down: no ship found; what follows it is left as it is");
            return None;
        };
        let made = lock(&self.loadout_made).clone();
        if made.is_empty() {
            log::warn!(
                "co-op rules: down: no refreshLoadout seen this run; what follows the ship is left as it is"
            );
            return None;
        }
        // The loadout's instances and the instances each one refers to.
        let mut objs: Vec<(Value, u32, u32)> = Vec::new();
        let _ = call.with(&Value::Real(ALL), |inst, _| {
            if let (Some(o), Ok(id)) = (object_of(&inst), inst.builtin_value("id"))
                && o != ship_obj
                && made.contains(&o)
                && let Some(n) = id_of(&id)
            {
                objs.push((id, n, o));
            }
        });
        let mut refs: Vec<(Value, u32, u32, HashSet<u32>)> = Vec::new();
        for (id, n, o) in objs {
            match state::raw_vars(call, &r, &id) {
                Ok(vars) => {
                    let to: HashSet<u32> = vars
                        .iter()
                        .filter_map(|(_, v)| refers(v, ship_obj, ship_id))
                        .collect();
                    refs.push((id, n, o, to));
                }
                Err((what, arg, e)) => {
                    log::warn!("co-op rules: down: {what}({id:?}, {arg}) on object {o}: {e}");
                }
            }
        }
        // Follow the links from the ship.
        let mut taken: HashSet<u32> = HashSet::from([ship_id]);
        loop {
            let before = taken.len();
            for (_, n, _, to) in &refs {
                if !taken.contains(n) && to.iter().any(|t| taken.contains(t)) {
                    taken.insert(*n);
                }
            }
            if taken.len() == before {
                break;
            }
        }
        let mut out = Vec::new();
        for (id, n, o, _) in refs {
            if taken.contains(&n) {
                out.push((id, o));
            }
        }
        // The Destroy event's line about the drones.
        if let Some(obj) = self
            .asset_get_index
            .and_then(|b| call.call_builtin(b, &[DIES_WITH_SHIP.into()]).ok())
            .filter(|v| crate::world::classify::object_index(v).is_some())
        {
            *lock(&self.dies_obj) = crate::world::classify::object_index(&obj);
            let _ = call.with(&obj, |inst, _| {
                if let (Some(o), Ok(id)) = (object_of(&inst), inst.builtin_value("id")) {
                    out.push((id, o));
                }
            });
        }
        Some(out)
    }

    /// The ship is active (not deactivated by a freeze), so [`Self::down`]
    /// can follow its links.
    pub fn ship_reachable(&self, call: &Call<'_>) -> bool {
        self.ship(call).is_some()
    }

    /// The ship went down (`oController`'s run tick, outside every lock).
    pub fn down(&self, call: &Call<'_>) {
        let Some(deactivate) = self.deactivate else {
            return;
        };
        let Some(ids) = self.reach(call) else { return };
        let gone = Gone { ids, again: 0 };
        let mut per: HashMap<u32, u32> = HashMap::new();
        let mut failed = 0;
        for (id, o) in &gone.ids {
            match call.call_builtin(deactivate, std::slice::from_ref(id)) {
                Ok(_) => *per.entry(*o).or_default() += 1,
                Err(e) => {
                    failed += 1;
                    log::warn!("co-op rules: down: instance_deactivate_object({id:?}): {e}");
                }
            }
        }
        log::info!(
            "co-op rules: down: gone until the respawn (following the ship's links; {DIES_WITH_SHIP} by the Destroy event's line): {}{}",
            self.list(call, &per),
            if failed > 0 {
                format!("; {failed} failed")
            } else {
                String::new()
            }
        );
        *lock(&self.gone) = Some(gone);
    }

    /// While down, every run tick: the game's pause and upgrade screen
    /// reactivate the whole room when they end; deactivate the taken ones again.
    pub fn hold(&self, call: &Call<'_>) {
        let Some(deactivate) = self.deactivate else {
            return;
        };
        let ids: Vec<Value> = match lock(&self.gone).as_ref() {
            Some(g) => g.ids.iter().map(|(id, _)| id.clone()).collect(),
            None => return,
        };
        let mut again = 0;
        for id in ids {
            if active(call, &id) && call.call_builtin(deactivate, &[id]).is_ok() {
                again += 1;
            }
        }
        if again > 0
            && let Some(g) = lock(&self.gone).as_mut()
        {
            g.again += 1;
        }
        if self.fresh.swap(false, Ordering::Relaxed) {
            let new = self.take_new(call);
            if new != "none" {
                log::info!("co-op rules: down: made while down, deactivated too: {new}");
            }
        }
    }

    /// While down: the rule again; what it reaches that is not gone yet is
    /// deactivated and added. The objects found, for the log.
    fn take_new(&self, call: &Call<'_>) -> String {
        let Some(deactivate) = self.deactivate else {
            return "none".to_owned();
        };
        // Only more of what the down took: a reassembled ally can be reached
        // by its links while the one there at the down was not, and allies
        // play on.
        let (known, mut kinds): (HashSet<u32>, HashSet<u32>) = match lock(&self.gone).as_ref() {
            Some(g) => (
                g.ids.iter().filter_map(|(id, _)| id_of(id)).collect(),
                g.ids.iter().map(|(_, o)| *o).collect(),
            ),
            None => return "none".to_owned(),
        };
        let Some(reached) = self.reach(call) else {
            return "none".to_owned();
        };
        kinds.extend(*lock(&self.dies_obj));
        let mut per: HashMap<u32, u32> = HashMap::new();
        let mut add = Vec::new();
        for (id, o) in reached {
            if kinds.contains(&o)
                && id_of(&id).is_some_and(|n| !known.contains(&n))
                && call
                    .call_builtin(deactivate, std::slice::from_ref(&id))
                    .is_ok()
            {
                *per.entry(o).or_default() += 1;
                add.push((id, o));
            }
        }
        if let Some(g) = lock(&self.gone).as_mut() {
            g.ids.extend(add);
        }
        self.list(call, &per)
    }

    /// The revive: everything comes back. True when something was
    /// reactivated: the revive then waits a frame, so that its
    /// `refreshLoadout` finds it active whenever an activation is seen.
    pub fn back(&self, call: &Call<'_>) -> bool {
        let Some(gone) = lock(&self.gone).take() else {
            return false;
        };
        let Some(activate) = self.activate else {
            return false;
        };
        let mut per: HashMap<u32, u32> = HashMap::new();
        for (id, o) in &gone.ids {
            match call.call_builtin(activate, std::slice::from_ref(id)) {
                Ok(_) => *per.entry(*o).or_default() += 1,
                Err(e) => log::warn!("co-op rules: back: instance_activate_object({id:?}): {e}"),
            }
        }
        log::info!(
            "co-op rules: back: reactivated, refreshLoadout next frame: {} (deactivated again after {} unfreezes)",
            self.list(call, &per),
            gone.again
        );
        !gone.ids.is_empty()
    }

    /// After the revive's `refreshLoadout`: what follows the ship now (log).
    pub fn rebuilt(&self, call: &Call<'_>) {
        let (Some(r), Some((ship_id, ship_obj))) = (self.reflect, self.ship(call)) else {
            return;
        };
        let made = lock(&self.loadout_made).clone();
        let mut objs: Vec<(Value, u32)> = Vec::new();
        let _ = call.with(&Value::Real(ALL), |inst, _| {
            if let (Some(o), Ok(id)) = (object_of(&inst), inst.builtin_value("id"))
                && o != ship_obj
                && made.contains(&o)
            {
                objs.push((id, o));
            }
        });
        let mut per: HashMap<u32, u32> = HashMap::new();
        for (id, o) in objs {
            let linked = state::raw_vars(call, &r, &id)
                .unwrap_or_default()
                .iter()
                .any(|(_, v)| refers(v, ship_obj, ship_id) == Some(ship_id));
            if linked {
                *per.entry(o).or_default() += 1;
            }
        }
        log::info!(
            "co-op rules: back: after refreshLoadout, referring to the ship: {}",
            self.list(call, &per)
        );
    }

    /// False while the game is frozen (pause or upgrade screen): the ship is
    /// deactivated then, while `oPowers` steps on. True when that cannot be
    /// told.
    pub fn ship_active(&self, call: &Call<'_>) -> bool {
        let Some(obj) = self
            .asset_get_index
            .and_then(|b| call.call_builtin(b, &["oPlayer".into()]).ok())
        else {
            return true;
        };
        active(call, &obj)
    }

    /// The ship's instance id and object.
    fn ship(&self, call: &Call<'_>) -> Option<(u32, u32)> {
        let obj = call
            .call_builtin(self.asset_get_index?, &["oPlayer".into()])
            .ok()?;
        let mut found = None;
        let _ = call.with(&obj, |inst, _| {
            if found.is_none() {
                found = inst
                    .builtin_value("id")
                    .ok()
                    .as_ref()
                    .and_then(id_of)
                    .zip(object_of(&inst));
            }
        });
        found
    }

    fn list(&self, call: &Call<'_>, m: &HashMap<u32, u32>) -> String {
        if m.is_empty() {
            return "none".to_owned();
        }
        let mut v: Vec<String> = m
            .iter()
            .map(|(o, n)| {
                let name = self
                    .object_get_name
                    .and_then(|b| call.call_builtin(b, &[f64::from(*o).into()]).ok())
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| o.to_string());
                format!("{name} x{n}")
            })
            .collect();
        v.sort();
        v.join(", ")
    }
}

/// Whether `with` reaches the instance (it exists and is active).
fn active(call: &Call<'_>, id: &Value) -> bool {
    let mut found = false;
    let _ = call.with(id, |_, _| found = true);
    found
}

/// An instance's id from its `id` value (a ref or a number).
fn id_of(v: &Value) -> Option<u32> {
    match v {
        Value::Ref(p) if is_instance_ref(*p) => Some((p & 0xFFFF_FFFF) as u32),
        Value::Real(n) if *n >= 100_000.0 && n.fract() == 0.0 => Some(*n as u32),
        Value::Int(n) if *n >= 100_000 => u32::try_from(*n).ok(),
        _ => None,
    }
}

/// The instance `v` names, if any: an instance (by ref or id), or the ship
/// when it names the ship's object (which has that one instance).
fn refers(v: &Value, ship_obj: u32, ship_id: u32) -> Option<u32> {
    match v {
        Value::Ref(p) if *p & 0xFFFF_FFFF_0000_0000 == OBJECT_REF_TAG => {
            ((p & 0xFFFF_FFFF) as u32 == ship_obj).then_some(ship_id)
        }
        other => id_of(other),
    }
}

/// The instance's object index (a number or an object ref, by runner).
fn object_of(inst: &Instance<'_>) -> Option<u32> {
    inst.builtin_value("object_index")
        .ok()
        .as_ref()
        .and_then(crate::world::classify::object_index)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unresolved() -> Downed {
        Downed {
            reflect: None,
            asset_get_index: None,
            object_get_name: None,
            deactivate: None,
            activate: None,
            refresh_depth: AtomicU32::new(0),
            loadout_made: Mutex::new(HashSet::new()),
            gone: Mutex::new(None),
            rebuild_depth: AtomicU32::new(0),
            rebuild_seen: AtomicBool::new(false),
            fresh: AtomicBool::new(false),
            dies_obj: Mutex::new(None),
        }
    }

    /// A game over leaves the ship down; the next run must not start
    /// down (its first `refreshLoadout` would be a rebuild while down).
    #[test]
    fn a_run_end_forgets_the_down() {
        let d = unresolved();
        *lock(&d.gone) = Some(Gone {
            ids: vec![(Value::Real(100_001.0), 640), (Value::Real(100_002.0), 641)],
            again: 2,
        });
        d.fresh.store(true, Ordering::Relaxed);
        *lock(&d.dies_obj) = Some(77);
        assert!(d.is_down());
        d.end_run();
        assert!(!d.is_down(), "the next run starts up");
        assert!(!d.fresh.load(Ordering::Relaxed));
        assert_eq!(*lock(&d.dies_obj), None);
        d.end_run();
        assert!(!d.is_down(), "again at the menu: nothing");
    }

    #[test]
    fn links_name_instances_and_the_ship_by_its_object() {
        let inst = Value::Ref(state::INSTANCE_REF_TAG | 0x0001_871B);
        assert_eq!(refers(&inst, 603, 100_094), Some(100_123));
        assert_eq!(refers(&Value::Real(100_094.0), 603, 100_094), Some(100_094));
        assert_eq!(
            refers(&Value::Ref(OBJECT_REF_TAG | 0x025B), 603, 100_094),
            Some(100_094)
        );
        assert_eq!(
            refers(&Value::Ref(OBJECT_REF_TAG | 0x025C), 603, 100_094),
            None
        );
        assert_eq!(refers(&Value::Real(12.0), 603, 100_094), None);
        assert_eq!(refers(&Value::Real(-4.0), 603, 100_094), None);
    }
}
