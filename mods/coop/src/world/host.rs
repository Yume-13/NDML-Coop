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

//! The host's side of the shared world (pure). The glue reports each enemy's
//! tick (`decrementVulnerability`, once per frame per `oSolid`); this module
//! assigns net ids, picks targets, detects despawns and builds the messages.
//! An enemy is announced at its first tick, after its maker set it up; parts
//! are replicated like any enemy. A kill is sent as `Killed` at once, anything
//! else that stops ticking and fails `instance_exists` as `Removed`.
//! Snapshots go out at 30 Hz; [`HostWorld::resync`] resends every live enemy
//! under the same net ids.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};

use ndml_net::interp::{Segments, Vec2};
use ndml_net::msg::{
    DespawnCause, EnemySnapshotFine, EnemySpawn, EnemyState, Message, NetId, PeerId, SpawnMarker,
};
use ndml_sdk::Value;

use super::target::{self, Hold, Player};

/// Snapshots go out every this many frames (30 Hz at 60 fps).
pub const SNAPSHOT_EVERY: u64 = 2;
/// Frames between checks of an asleep (deactivated) enemy: woken by a tick,
/// but gone only when a check finds no instance at all.
pub const ASLEEP_CHECK_EVERY: u64 = 30;
/// `WaveState` is repeated this often (frames) even without a change.
pub const WAVE_REFRESH: u64 = 300;

/// What the glue read from an enemy at its first tick.
#[derive(Debug, Clone, PartialEq)]
pub struct FirstSight {
    /// Its instance id (`builtin_value("id")`), for `instance_exists`.
    pub id: Value,
    pub object: u16,
    pub x: f32,
    pub y: f32,
    pub angle: f32,
    /// `None`: the object has no `hull` (sent with [`EnemySpawn::NO_HULL`]).
    pub hull: Option<f32>,
    pub hull_max: f32,
    /// `image_index` (an asteroid's shape), sent as `EnemyLook` when not 0.
    pub image_index: f32,
    /// Not an enemy: an instance a replicated enemy depends on.
    pub dependency: bool,
}

/// What the glue reads every tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TickRead {
    pub x: f32,
    pub y: f32,
    pub angle: f32,
    /// `None`: the object has no `hull` (a controller, a background): never
    /// read as 0, which a despawn's cause would take for a death.
    pub hull: Option<f32>,
}

/// What a tick's instance is to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// Tracked under this id.
    Known(NetId),
    /// Not seen before (or its address now holds another object).
    New,
}

#[derive(Debug, Clone)]
struct HostEnemy {
    key: usize,
    id: Value,
    spawn: EnemySpawn,
    pos: Vec2,
    angle: f32,
    hull: f32,
    last_tick: u64,
    hold: Hold,
    target: Option<PeerId>,
    image_index: f32,
    /// Its discontinuity segment: the game moved it by more than its motion
    /// (a wrap, a blink). Sent with each snapshot.
    segs: Segments,
    /// Deactivated by the game (not gone): no snapshots until it ticks again.
    asleep: bool,
    /// The peer whose damage report took it to 0, so its kill names them;
    /// cleared when the host's own hit or the world changes the hull after
    /// that.
    last_damager: Option<PeerId>,
    /// The hull that report left.
    damager_hull: f32,
    /// The hull its last tick read; `None` before one, or for an object with
    /// no `hull`. What decides an inferred death ([`HostWorld::gone_with`]).
    hull_read: Option<f32>,
}

/// See the module docs.
#[derive(Debug, Clone)]
pub struct HostWorld {
    next_id: NetId,
    by_key: HashMap<usize, NetId>,
    enemies: BTreeMap<NetId, HostEnemy>,
    frame: u64,
    last_tick_frame: Option<u64>,
    /// The frame before this one if anything ticked in it, as
    /// [`Self::begin_frame`] found it: whether this frame is due a snapshot
    /// (dependencies taken later in the run step do not change it).
    ticked_before: Option<u64>,
    player_seen: bool,
    players: Vec<Player>,
    /// Addresses a new instance took before the run step found their old
    /// enemy gone ([`Self::address_reused`]), this run.
    reused_total: u64,
    outbox: Vec<Message>,
    wave: Option<(u16, u32)>,
    wave_sent: Option<(u16, u64)>,
    /// Enemies that ran `instance_destroy` with their Destroy event and are
    /// not dead (a missile that went off): sent as `Destroyed`, not `Removed`.
    destroyed: HashSet<NetId>,
}

impl Default for HostWorld {
    fn default() -> Self {
        Self {
            next_id: 1,
            by_key: HashMap::new(),
            enemies: BTreeMap::new(),
            destroyed: HashSet::new(),
            frame: 0,
            last_tick_frame: None,
            ticked_before: None,
            player_seen: false,
            players: Vec::new(),
            reused_total: 0,
            outbox: Vec::new(),
            wave: None,
            wave_sent: None,
        }
    }
}

fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

impl HostWorld {
    /// Live replicated enemies.
    #[must_use]
    pub fn len(&self) -> usize {
        self.enemies.len()
    }

    /// No live replicated enemies.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.enemies.is_empty()
    }

    /// What `key` (with object index `object`) is.
    #[must_use]
    pub fn seen(&self, key: usize, object: u16) -> Seen {
        if let Some(&id) = self.by_key.get(&key) {
            return match self.enemies.get(&id) {
                Some(e) if e.spawn.object == object => Seen::Known(id),
                _ => Seen::New,
            };
        }
        Seen::New
    }

    /// The net id of the tracked enemy at `key`.
    #[must_use]
    pub fn net_id(&self, key: usize) -> Option<NetId> {
        self.by_key.get(&key).copied()
    }

    /// The net id of the tracked enemy at `key` only when it is the instance
    /// `id`: a new instance at a destroyed enemy's address resolves to
    /// nothing until its first tick.
    #[must_use]
    pub fn net_id_for(&self, key: usize, id: &Value) -> Option<NetId> {
        let net = *self.by_key.get(&key)?;
        let e = self.enemies.get(&net)?;
        (crate::ids::id_number(&e.id).is_some()
            && crate::ids::id_number(&e.id) == crate::ids::id_number(id))
        .then_some(net)
    }

    /// Whether `key` is a tracked (already ticked) enemy.
    #[must_use]
    pub fn is_tracked(&self, key: usize) -> bool {
        self.by_key.contains_key(&key)
    }

    /// The instance id of a tracked enemy.
    #[must_use]
    pub fn id_of(&self, key: usize) -> Option<Value> {
        let id = self.by_key.get(&key)?;
        self.enemies.get(id).map(|e| e.id.clone())
    }

    /// Shared damage (`crate::damage`): a tracked enemy's instance id and max
    /// hull, by net id.
    #[must_use]
    pub fn by_net_id(&self, net_id: NetId) -> Option<(Value, f32)> {
        self.enemies
            .get(&net_id)
            .map(|e| (e.id.clone(), e.spawn.hull_max))
    }

    /// Shared damage: a tracked enemy's object.
    #[must_use]
    pub fn object_of(&self, net_id: NetId) -> Option<u16> {
        self.enemies.get(&net_id).map(|e| e.spawn.object)
    }

    /// Shared damage: a tracked enemy's hull at its last tick.
    #[must_use]
    pub fn hull_of(&self, net_id: NetId) -> Option<f32> {
        self.enemies.get(&net_id).map(|e| e.hull)
    }

    /// Shared damage: the kill credit of the enemy at instance `key` (the
    /// peer its kill would name now).
    #[must_use]
    pub fn damager_at(&self, key: usize) -> Option<PeerId> {
        let id = self.by_key.get(&key)?;
        self.enemies.get(id).and_then(|e| e.last_damager)
    }

    /// Shared damage: `peer`'s report hit this enemy (its kill names them).
    pub fn set_damager(&mut self, net_id: NetId, peer: PeerId, hull: f32) {
        if let Some(e) = self.enemies.get_mut(&net_id) {
            e.last_damager = Some(peer);
            e.damager_hull = hull;
        }
    }

    /// Shared damage: send this enemy again as it is now (a refused kill
    /// claim: the claimer's copy is gone and comes back from this).
    pub fn respawn(&mut self, net_id: NetId) {
        if let Some(e) = self.enemies.get(&net_id) {
            let s = EnemySpawn {
                x: e.pos.x,
                y: e.pos.y,
                angle: e.angle,
                hull: e.hull,
                ..e.spawn
            };
            let index = e.image_index;
            self.outbox.push(Message::EnemySpawn(s));
            self.push_look(net_id, index);
        }
    }

    fn take_id(&mut self) -> NetId {
        // Skip 0 and ids still in use (after a wrap).
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.enemies.contains_key(&id) {
                return id;
            }
        }
    }

    fn remove(&mut self, id: NetId, cause: DespawnCause) {
        let cause = if self.destroyed.remove(&id) && cause == DespawnCause::Removed {
            DespawnCause::Destroyed
        } else {
            cause
        };
        if let Some(e) = self.enemies.remove(&id) {
            if self.by_key.get(&e.key) == Some(&id) {
                self.by_key.remove(&e.key);
            }
            self.outbox.push(Message::Despawn { net_id: id, cause });
        }
    }

    /// First tick of an enemy: track it and announce it.
    pub fn add(&mut self, key: usize, f: FirstSight) -> NetId {
        // Its address held another object until now: that one is gone.
        if let Some(&old) = self.by_key.get(&key) {
            self.remove(old, DespawnCause::Removed);
        }
        let net_id = self.take_id();
        let spawn = EnemySpawn {
            net_id,
            object: f.object,
            x: finite(f.x),
            y: finite(f.y),
            angle: finite(f.angle),
            hull: finite(f.hull.unwrap_or(0.0)),
            hull_max: if f.hull_max.is_finite() && f.hull_max > 0.0 {
                f.hull_max
            } else {
                f.hull.unwrap_or(0.0).max(1.0)
            },
            flags: if f.dependency {
                EnemySpawn::DEPENDENCY
            } else {
                0
            } | if f.hull.is_none() {
                EnemySpawn::NO_HULL
            } else {
                0
            },
        };
        let pos = Vec2::new(spawn.x, spawn.y);
        let mut hold = Hold::default();
        let target = self.pick(&mut hold, pos);
        self.enemies.insert(
            net_id,
            HostEnemy {
                key,
                id: f.id,
                spawn,
                pos,
                angle: spawn.angle,
                hull: spawn.hull,
                last_tick: self.frame,
                hold,
                target,
                image_index: finite(f.image_index),
                last_damager: None,
                damager_hull: 0.0,
                hull_read: f.hull.map(finite),
                segs: {
                    let mut g = Segments::default();
                    g.observe(pos);
                    g
                },
                asleep: false,
            },
        );
        self.by_key.insert(key, net_id);
        self.last_tick_frame = Some(self.frame);
        self.outbox.push(Message::EnemySpawn(spawn));
        self.push_look(net_id, finite(f.image_index));
        net_id
    }

    fn pick(&self, hold: &mut Hold, pos: Vec2) -> Option<PeerId> {
        target::choose(hold, pos, &self.players, self.frame)
    }

    /// The player a shot at `pos` goes after when its maker has none: the
    /// enemies' rule with no history (the nearest targetable player).
    #[must_use]
    pub fn nearest_player(&self, pos: Vec2) -> Option<PeerId> {
        self.pick(&mut Hold::default(), pos)
    }

    /// A tracked enemy ticked.
    pub fn tick(&mut self, id: NetId, r: TickRead) {
        let frame = self.frame;
        let Some(mut e) = self.enemies.remove(&id) else {
            return;
        };
        e.pos = Vec2::new(finite(r.x), finite(r.y));
        e.segs.observe(e.pos);
        if e.asleep {
            // Activated again (a freeze's region, `instance_activate_*`).
            e.asleep = false;
            self.outbox.push(Message::Asleep {
                net_id: id,
                asleep: false,
            });
        }
        e.angle = finite(r.angle);
        // The wire's hull stays 0 for an object without one; the cause
        // decision reads `hull_read`.
        e.hull = finite(r.hull.unwrap_or(0.0));
        e.hull_read = r.hull.map(finite);
        if e.last_damager.is_some() && (e.hull - e.damager_hull).abs() > 1e-3 {
            // The host's own hit or the world changed it since the report.
            e.last_damager = None;
        }
        e.last_tick = frame;
        e.target = self.pick(&mut e.hold, e.pos);
        self.enemies.insert(id, e);
        self.last_tick_frame = Some(frame);
    }

    /// The local ship stepped (the world is running).
    pub fn player_stepped(&mut self) {
        self.player_seen = true;
    }

    /// `checkDestroy*` destroyed the enemy at `key`: send the kill now.
    pub fn killed(&mut self, key: usize) -> bool {
        let Some(&id) = self.by_key.get(&key) else {
            return false;
        };
        let by = self.enemies.get(&id).and_then(|e| e.last_damager);
        self.remove(id, DespawnCause::Killed { by });
        true
    }

    /// Start a frame (the run tick, before this frame's enemy ticks). Returns
    /// the enemies that missed the last frame's ticks while the world ran
    /// (asleep ones every [`ASLEEP_CHECK_EVERY`] frames), for the glue to check.
    pub fn begin_frame(&mut self, players: Vec<Player>) -> Vec<(NetId, Value)> {
        let prev = self.frame;
        self.frame += 1;
        self.players = players;
        self.ticked_before = self.last_tick_frame.filter(|f| *f == prev);
        let ran = self.ticked_before.is_some() || self.player_seen;
        self.player_seen = false;
        if !ran {
            return Vec::new();
        }
        self.enemies
            .iter()
            .filter(|(_, e)| {
                e.last_tick < prev && (!e.asleep || prev.is_multiple_of(ASLEEP_CHECK_EVERY))
            })
            .map(|(&id, e)| (id, e.id.clone()))
            .collect()
    }

    /// The game destroys the enemy at `key` with its Destroy event (the
    /// builtin `instance_destroy`, event on): unless it dies (`Killed`), its
    /// despawn is `Destroyed`. True when `key` is a tracked enemy.
    pub fn destroying(&mut self, key: usize) -> bool {
        let Some(&id) = self.by_key.get(&key) else {
            return false;
        };
        self.destroyed.insert(id);
        true
    }

    /// An instance `new_id` ticks at `key`, whose tracked enemy ran its
    /// Destroy event since the last run step: if it is another instance (the
    /// address reused), the old enemy goes now (`Destroyed`) and `key` is
    /// free. True when it was another one.
    pub fn address_reused(&mut self, key: usize, new_id: &Value) -> bool {
        let Some(&old) = self.by_key.get(&key) else {
            return false;
        };
        if self.enemies.get(&old).is_none_or(|e| e.id == *new_id) {
            // The old instance itself, ticking after its own Destroy event
            // in the same code (GML goes on after `instance_destroy`).
            return false;
        }
        self.destroyed.insert(old);
        self.remove(old, DespawnCause::Removed);
        self.reused_total += 1;
        true
    }

    /// Addresses reused before the run step this run ([`Self::address_reused`]).
    #[must_use]
    pub fn reused_count(&self) -> u64 {
        self.reused_total
    }

    /// Net ids start at `first`, so a run crosses the 16-bit wrap. Only
    /// before any id was taken.
    #[cfg(test)]
    pub fn start_ids_at(&mut self, first: NetId) {
        if self.next_id == 1 && self.enemies.is_empty() {
            self.next_id = first.max(1);
        }
    }

    /// These were deactivated by the game (a freeze left them outside the
    /// region it brought back): kept, and the joining game deactivates its
    /// copies too. They go on when they tick again.
    pub fn sleep(&mut self, ids: &[NetId]) {
        for &id in ids {
            if let Some(e) = self.enemies.get_mut(&id)
                && !e.asleep
            {
                e.asleep = true;
                self.outbox.push(Message::Asleep {
                    net_id: id,
                    asleep: true,
                });
            }
        }
    }

    /// These failed `instance_exists`: removed without dying.
    pub fn gone(&mut self, ids: &[NetId]) {
        for &id in ids {
            self.remove(id, DespawnCause::Removed);
        }
    }

    /// These failed `instance_exists`; each is sent as what happened to it:
    /// `Killed` when its last tick read a `hull` at or below 0 (an enemy that
    /// destroys itself in its own Step, past `checkDestroy*`; no `hull` is not
    /// 0), so the copy takes the game's death path and its Destroy event;
    /// `Destroyed` when its Destroy event ran otherwise; `Removed` when it ran
    /// none.
    pub fn gone_with(&mut self, ids: &[NetId]) {
        for &id in ids {
            let dead = self
                .enemies
                .get(&id)
                .filter(|e| e.hull_read.is_some_and(|h| h <= 0.0));
            let cause = match dead {
                Some(e) => DespawnCause::Killed { by: e.last_damager },
                None => DespawnCause::Removed,
            };
            // `remove` makes a `Removed` with its event seen `Destroyed`.
            self.remove(id, cause);
        }
    }

    /// The wave as the game shows it (`global.wave`, frames to the next wave
    /// converted to ms). Queues `WaveState` on a change, and every
    /// [`WAVE_REFRESH`] frames to keep the client's timer close.
    pub fn wave(&mut self, wave: u16, next_wave_ms: u32) {
        self.wave = Some((wave, next_wave_ms));
        let due = self
            .wave_sent
            .is_none_or(|(w, at)| w != wave || self.frame.saturating_sub(at) >= WAVE_REFRESH);
        if due {
            self.wave_sent = Some((wave, self.frame));
            self.outbox.push(Message::WaveState { wave, next_wave_ms });
        }
    }

    /// The full state for peers that have nothing: see the module docs.
    pub fn resync(&mut self) {
        let enemies = u16::try_from(self.enemies.len()).unwrap_or(u16::MAX);
        self.outbox.push(Message::WorldResync { enemies });
        let looks: Vec<(NetId, f32)> = self
            .enemies
            .iter()
            .map(|(&id, e)| (id, e.image_index))
            .collect();
        for e in self.enemies.values() {
            self.outbox.push(Message::EnemySpawn(EnemySpawn {
                x: e.pos.x,
                y: e.pos.y,
                angle: e.angle,
                hull: e.hull,
                ..e.spawn
            }));
        }
        for (id, index) in looks {
            self.push_look(id, index);
        }
        if let Some((wave, next_wave_ms)) = self.wave {
            self.wave_sent = Some((wave, self.frame));
            self.outbox.push(Message::WaveState { wave, next_wave_ms });
        }
    }

    /// Messages to broadcast now: spawns, despawns, waves, and a snapshot
    /// every [`SNAPSHOT_EVERY`] frames while enemies tick. `time_ms` is the
    /// host's wire clock.
    pub fn take_messages(&mut self, time_ms: u32) -> Vec<Message> {
        let mut out = std::mem::take(&mut self.outbox);
        let prev = self.frame.saturating_sub(1);
        if self.frame.is_multiple_of(SNAPSHOT_EVERY) && self.ticked_before == Some(prev) {
            let states: Vec<EnemyState> = self
                .enemies
                .iter()
                .filter(|(_, e)| e.last_tick == prev)
                .map(|(&net_id, e)| EnemyState {
                    seg: e.segs.seg(),
                    net_id,
                    x: e.pos.x,
                    y: e.pos.y,
                    angle: e.angle,
                    hull_frac: e.hull / e.spawn.hull_max,
                    target: e.target,
                })
                .collect();
            if !states.is_empty() {
                out.extend(
                    EnemySnapshotFine::split(time_ms, &states)
                        .into_iter()
                        .map(Message::EnemySnapshotFine),
                );
            }
        }
        out
    }

    fn push_look(&mut self, net_id: NetId, image_index: f32) {
        if image_index != 0.0 {
            self.outbox.push(Message::EnemyLook {
                net_id,
                image_index,
            });
        }
    }

    /// The game placed a spawn telegraph: pass it on.
    pub fn marker(&mut self, m: SpawnMarker) {
        self.outbox.push(Message::SpawnMarker(m));
    }

    /// The player the enemy at `key` is assigned to.
    #[must_use]
    pub fn assignment(&self, key: usize) -> Option<PeerId> {
        let id = self.by_key.get(&key)?;
        self.enemies.get(id)?.target
    }

    /// Forget everything (run ended, or the partner left for good).
    pub fn clear(&mut self) {
        *self = Self {
            next_id: self.next_id,
            ..Self::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::msg::EnemySnapshot;

    fn sight(object: u16, x: f32) -> FirstSight {
        FirstSight {
            id: Value::Real(100_000.0 + f64::from(x)),
            object,
            x,
            y: 10.0,
            angle: 90.0,
            hull: Some(60.0),
            hull_max: 120.0,
            image_index: 0.0,
            dependency: false,
        }
    }

    fn read(x: f32, hull: f32) -> TickRead {
        TickRead {
            x,
            y: 10.0,
            angle: 45.0,
            hull: Some(hull),
        }
    }

    fn players() -> Vec<Player> {
        vec![
            Player {
                peer: PeerId::HOST,
                pos: Vec2::new(0.0, 0.0),
                targetable: true,
            },
            Player {
                peer: PeerId(1),
                pos: Vec2::new(1000.0, 0.0),
                targetable: true,
            },
        ]
    }

    #[test]
    fn an_instance_without_hull_is_sent_so() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        h.add(
            1,
            FirstSight {
                hull: None,
                hull_max: 0.0,
                ..sight(0x302, 0.0)
            },
        );
        h.add(2, sight(0x325, 0.0));
        let flags: Vec<bool> = h
            .take_messages(0)
            .into_iter()
            .filter_map(|m| match m {
                Message::EnemySpawn(s) => Some(s.has_hull()),
                _ => None,
            })
            .collect();
        assert_eq!(flags, vec![false, true]);
    }

    #[test]
    fn spawn_snapshot_and_targets() {
        let mut h = HostWorld::default();
        assert!(h.begin_frame(players()).is_empty());
        assert_eq!(h.seen(7, 0x325), Seen::New);
        let id = h.add(7, sight(0x325, 900.0));
        assert_eq!(h.seen(7, 0x325), Seen::Known(id));
        assert_eq!(
            h.seen(7, 0x214),
            Seen::New,
            "another object at that address"
        );
        let msgs = h.take_messages(0);
        let Some(Message::EnemySpawn(s)) = msgs.first() else {
            unreachable!("{msgs:?}")
        };
        assert_eq!((s.net_id, s.object), (id, 0x325));
        assert!(!s.is_dependency());
        assert!((s.hull_max - 120.0).abs() < 1e-6);
        assert_eq!(h.assignment(7), Some(PeerId(1)), "nearest player");
        // Next frame (2, even): ticked in frame 1, snapshot goes out.
        assert!(h.begin_frame(players()).is_empty());
        let msgs = normalized(h.take_messages(33));
        let Some(Message::EnemySnapshot(snap)) = msgs.first() else {
            unreachable!("{msgs:?}")
        };
        assert_eq!(snap.enemies.len(), 1);
        assert!((snap.enemies[0].hull_frac - 0.5).abs() < 1e-6);
        assert_eq!(snap.enemies[0].target, Some(PeerId(1)));
        h.tick(id, read(905.0, 30.0));
        h.begin_frame(players());
        assert!(h.take_messages(50).is_empty(), "odd frame: no snapshot");
    }

    /// A dependency taken in the run step keeps the frame's snapshot of what
    /// ticked the frame before; its own pose goes in its spawn.
    #[test]
    fn a_dependency_taken_in_the_run_step_keeps_the_frames_snapshot() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x325, 100.0));
        let _ = h.take_messages(0);
        // Frame 2 (even): `a` ticked in frame 1; a leg is taken now.
        h.begin_frame(players());
        let leg = h.add(2, sight(0x326, 200.0));
        let msgs = normalized(h.take_messages(33));
        let snaps: Vec<&EnemySnapshot> = msgs
            .iter()
            .filter_map(|m| match m {
                Message::EnemySnapshot(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(snaps.len(), 1, "{msgs:?}");
        let ids: Vec<NetId> = snaps[0].enemies.iter().map(|e| e.net_id).collect();
        assert_eq!(ids, vec![a], "what ticked the frame before");
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Message::EnemySpawn(s) if s.net_id == leg)),
            "the leg's spawn"
        );
    }

    /// A frame after one where nothing ticked, with nothing added, makes no
    /// snapshot (a freeze stays quiet).
    #[test]
    fn a_frame_after_one_with_no_ticks_makes_no_snapshot() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x325, 100.0));
        let _ = h.take_messages(0);
        h.begin_frame(players());
        assert!(!h.take_messages(33).is_empty(), "frame 2: ticked in 1");
        // Frame 2 ran no tick (frozen): frames 3 and 4 send nothing.
        h.begin_frame(players());
        assert!(h.take_messages(50).is_empty());
        h.begin_frame(players());
        assert!(
            h.take_messages(66).is_empty(),
            "frame 4: nothing ticked in 3"
        );
        // It ticks again in frames 4 and 5: frame 5 is odd, frame 6 sends.
        h.tick(a, read(110.0, 60.0));
        h.begin_frame(players());
        assert!(h.take_messages(83).is_empty(), "odd frame");
        h.tick(a, read(120.0, 60.0));
        h.begin_frame(players());
        assert!(
            h.take_messages(100)
                .iter()
                .any(|m| matches!(m, Message::EnemySnapshotFine(_))),
            "frame 6: ticked in 5"
        );
    }

    #[test]
    fn self_destroyed_dead_enemies_count_as_kills() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0xf8, 10.0));
        let b = h.add(2, sight(0xf8, 20.0));
        h.tick(a, read(10.0, -5.0)); // shot to death, then gone in its own Step
        h.tick(b, read(20.0, 40.0)); // wiped by the wave
        let _ = h.take_messages(0);
        h.gone_with(&[a, b]);
        let causes: Vec<_> = h
            .take_messages(0)
            .into_iter()
            .filter_map(|m| match m {
                Message::Despawn { net_id, cause } => Some((net_id, cause)),
                _ => None,
            })
            .collect();
        assert_eq!(
            causes,
            vec![
                (a, DespawnCause::Killed { by: None }),
                (b, DespawnCause::Removed),
            ]
        );
        assert!(h.is_empty());
    }

    #[test]
    fn a_destroy_with_its_event_is_destroyed_unless_a_kill() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x127, 0.0));
        let b = h.add(2, sight(0x127, 0.0));
        let c = h.add(3, sight(0x127, 0.0));
        h.take_messages(0);
        assert!(h.destroying(1));
        assert!(h.destroying(2));
        assert!(!h.destroying(9), "not tracked");
        assert!(h.killed(2), "a kill stays a kill");
        h.gone(&[a, c]);
        let causes: Vec<_> = h
            .take_messages(0)
            .into_iter()
            .filter_map(|m| match m {
                Message::Despawn { net_id, cause } => Some((net_id, cause)),
                _ => None,
            })
            .collect();
        assert_eq!(
            causes,
            vec![
                (b, DespawnCause::Killed { by: None }),
                (a, DespawnCause::Destroyed),
                (c, DespawnCause::Removed),
            ]
        );
    }

    /// Snapshots as receivers read them (the host sends the fine encoding).
    fn normalized(msgs: Vec<Message>) -> Vec<Message> {
        msgs.into_iter().map(Message::normalized).collect()
    }

    fn despawns(h: &mut HostWorld) -> Vec<(NetId, DespawnCause)> {
        h.take_messages(0)
            .into_iter()
            .filter_map(|m| match m {
                Message::Despawn { net_id, cause } => Some((net_id, cause)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_new_instance_at_a_destroyed_enemys_address_is_a_new_enemy() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x127, 0.0));
        h.take_messages(0);
        // The old instance ticking after its own Destroy event: still it.
        assert!(!h.address_reused(1, &Value::Real(100_000.0)));
        assert!(!h.address_reused(9, &Value::Real(100_005.0)), "not tracked");
        assert_eq!(h.seen(1, 0x127), Seen::Known(a));
        // Another instance of the same object at that address.
        assert!(h.address_reused(1, &Value::Real(100_005.0)));
        assert_eq!(despawns(&mut h), vec![(a, DespawnCause::Destroyed)]);
        assert_eq!(h.seen(1, 0x127), Seen::New);
        let b = h.add(1, sight(0x127, 5.0));
        assert_ne!(a, b);
        assert_eq!(h.seen(1, 0x127), Seen::Known(b));
        assert_eq!(h.reused_count(), 1);
        // The new one's own Destroy and removal later: its own cause.
        h.gone(&[b]);
        assert_eq!(despawns(&mut h), vec![(b, DespawnCause::Removed)]);
    }

    /// An address answers for a net id only for the instance tracked there,
    /// whether its id is a ref or a number.
    #[test]
    fn an_address_answers_only_for_the_instance_tracked_there() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x127, 0.0));
        assert_eq!(h.net_id_for(1, &Value::Real(100_000.0)), Some(a));
        assert_eq!(h.net_id_for(1, &Value::Ref(100_000)), Some(a), "a ref");
        assert_eq!(h.net_id(1), Some(a), "the address alone");
        assert_eq!(
            h.net_id_for(1, &Value::Real(100_005.0)),
            None,
            "another instance"
        );
        assert_eq!(h.net_id_for(2, &Value::Real(100_000.0)), None, "untracked");
        assert_eq!(h.net_id_for(1, &Value::Undefined), None);
    }

    #[test]
    fn net_ids_cross_the_wrap_skipping_ids_in_use_on_both_sides() {
        let mut h = HostWorld::default();
        h.start_ids_at(65_533);
        h.begin_frame(players());
        let ids: Vec<NetId> = (1..=5).map(|k| h.add(k, sight(0x127, 0.0))).collect();
        assert_eq!(ids, vec![65_533, 65_534, 65_535, 1, 2], "0 is never an id");
        // Alive across the wrap: 65,534 and 2; the rest gone.
        h.gone(&[65_533, 65_535, 1]);
        h.next_id = 65_534;
        let more: Vec<NetId> = (6..=8).map(|k| h.add(k, sight(0x127, 0.0))).collect();
        assert_eq!(more, vec![65_535, 1, 3], "ids in use are skipped");
        // Too late once ids were taken; a new run keeps counting.
        h.start_ids_at(100);
        h.clear();
        h.start_ids_at(100);
        assert_eq!(h.add(1, sight(0x127, 0.0)), 4);
    }

    #[test]
    fn the_cause_is_what_happened_no_hull_is_no_death() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let none = |x| TickRead {
            x,
            y: 0.0,
            angle: 0.0,
            hull: None,
        };
        // 1: no hull, its Destroy ran (the Star Eater's BG controller).
        // 2: no hull, no event. 3: dead, its Destroy ran (a death).
        // 4: dead, no event (an object with no Destroy: oAsteroidSmall).
        // 5: alive, its Destroy ran.
        // 6: never ticked since its spawn (its first sight read hull 60).
        let ids: Vec<NetId> = (1..=6).map(|k| h.add(k, sight(0x127, 0.0))).collect();
        h.tick(ids[0], none(0.0));
        h.tick(ids[1], none(0.0));
        h.tick(ids[2], read(0.0, 0.0));
        h.tick(ids[3], read(0.0, -3.0));
        h.tick(ids[4], read(0.0, 40.0));
        h.take_messages(0);
        for k in [1, 3, 5, 6] {
            assert!(h.destroying(k));
        }
        h.gone_with(&ids);
        let causes: Vec<_> = h
            .take_messages(0)
            .into_iter()
            .filter_map(|m| match m {
                Message::Despawn { cause, .. } => Some(cause),
                _ => None,
            })
            .collect();
        assert_eq!(
            causes,
            vec![
                DespawnCause::Destroyed,
                DespawnCause::Removed,
                DespawnCause::Killed { by: None },
                DespawnCause::Killed { by: None },
                DespawnCause::Destroyed,
                DespawnCause::Destroyed,
            ]
        );
    }

    #[test]
    fn a_dead_hull_is_a_death_and_no_hull_none() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x127, 0.0));
        let b = h.add(2, sight(0x127, 0.0));
        h.tick(a, read(0.0, 0.0));
        h.tick(
            b,
            TickRead {
                x: 0.0,
                y: 0.0,
                angle: 0.0,
                hull: None,
            },
        );
        h.take_messages(0);
        h.gone_with(&[a, b]);
        let causes: Vec<_> = h
            .take_messages(0)
            .into_iter()
            .filter_map(|m| match m {
                Message::Despawn { cause, .. } => Some(cause),
                _ => None,
            })
            .collect();
        assert_eq!(
            causes,
            vec![DespawnCause::Killed { by: None }, DespawnCause::Removed]
        );
    }

    #[test]
    fn kills_and_removals() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x127, 0.0));
        let b = h.add(3, sight(0x325, 50.0));
        h.take_messages(0);
        // Frame 2: both tick; then 3 is killed by checkDestroy.
        assert!(h.begin_frame(players()).is_empty());
        h.tick(a, read(1.0, 60.0));
        assert!(h.killed(3));
        assert!(!h.killed(3), "only once");
        let msgs = h.take_messages(0);
        assert!(msgs.contains(&Message::Despawn {
            net_id: b,
            cause: DespawnCause::Killed { by: None }
        }));
        // Frame 3: `a` stops ticking; world ran (player stepped).
        h.player_stepped();
        let missing = h.begin_frame(players());
        assert!(missing.is_empty(), "a ticked in frame 2");
        h.player_stepped();
        let missing = h.begin_frame(players());
        assert_eq!(missing.iter().map(|m| m.0).collect::<Vec<_>>(), vec![a]);
        h.gone(&[a]);
        assert!(h.take_messages(0).contains(&Message::Despawn {
            net_id: a,
            cause: DespawnCause::Removed
        }));
        assert!(h.is_empty());
    }

    #[test]
    fn a_partners_kill_credit_ends_when_the_hosts_own_change_lands() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x325, 0.0));
        let b = h.add(2, sight(0x325, 50.0));
        h.take_messages(0);
        // Both reports took the hull to 0 (peer 2).
        h.set_damager(a, PeerId(2), -3.0);
        h.set_damager(b, PeerId(2), -1.0);
        h.begin_frame(players());
        h.tick(a, read(0.0, -3.0));
        h.tick(b, read(50.0, 20.0)); // the host's game changed it since
        assert!(h.killed(1) && h.killed(2));
        let msgs = h.take_messages(0);
        assert!(msgs.contains(&Message::Despawn {
            net_id: a,
            cause: DespawnCause::Killed {
                by: Some(PeerId(2))
            }
        }));
        assert!(msgs.contains(&Message::Despawn {
            net_id: b,
            cause: DespawnCause::Killed { by: None }
        }));
    }

    #[test]
    fn the_kill_credit_survives_a_freeze_and_a_refusal() {
        // Kill effects read this credit when a death path begins.
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x325, 0.0));
        h.take_messages(0);
        h.set_damager(a, PeerId(2), 0.0);
        assert_eq!(h.damager_at(1), Some(PeerId(2)));
        // A freeze: no ticks, no frames run; the credit stays.
        for _ in 0..50 {
            h.begin_frame(players());
        }
        assert_eq!(h.damager_at(1), Some(PeerId(2)));
        // The claim is refused after the bound (the game kept it alive at
        // 0): sent again, the hull unchanged; still the partner's.
        h.respawn(a);
        h.begin_frame(players());
        h.tick(a, read(0.0, 0.0));
        assert_eq!(h.damager_at(1), Some(PeerId(2)));
        // The game's own kill later names the partner.
        assert!(h.killed(1));
        assert!(h.take_messages(0).contains(&Message::Despawn {
            net_id: a,
            cause: DespawnCause::Killed {
                by: Some(PeerId(2))
            }
        }));
        assert_eq!(h.damager_at(1), None);
        // A hull rise above 0 (a heal) ends it, as it ends `refused_zero`.
        let b = h.add(3, sight(0x325, 10.0));
        h.set_damager(b, PeerId(2), 0.0);
        h.begin_frame(players());
        h.tick(b, read(10.0, 4.0));
        assert_eq!(h.damager_at(3), None);
    }

    #[test]
    fn frozen_world_sweeps_nothing() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        h.add(1, sight(0x325, 0.0));
        h.begin_frame(players());
        for _ in 0..10 {
            // Paused: no ticks, no player step.
            assert!(h.begin_frame(players()).is_empty());
        }
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn resync_carries_current_state_and_keeps_ids() {
        let mut h = HostWorld::default();
        h.begin_frame(players());
        let a = h.add(1, sight(0x325, 0.0));
        h.wave(3, 20_000);
        h.tick(a, read(77.0, 12.0));
        h.take_messages(0);
        h.resync();
        let msgs = h.take_messages(1);
        assert_eq!(msgs[0], Message::WorldResync { enemies: 1 });
        let Message::EnemySpawn(s) = msgs[1] else {
            unreachable!("{msgs:?}")
        };
        assert_eq!(s.net_id, a, "ids unchanged");
        assert!((s.x - 77.0).abs() < 1e-6 && (s.hull - 12.0).abs() < 1e-6);
        assert!((s.hull_max - 120.0).abs() < 1e-6);
        assert_eq!(
            msgs[2],
            Message::WaveState {
                wave: 3,
                next_wave_ms: 20_000
            }
        );
    }

    #[test]
    fn waves_on_change_and_refresh() {
        let mut h = HostWorld::default();
        h.wave(1, 5000);
        h.wave(1, 4900);
        assert_eq!(h.take_messages(0).len(), 1);
        h.wave(2, 30_000);
        assert_eq!(h.take_messages(0).len(), 1);
        for _ in 0..WAVE_REFRESH {
            h.begin_frame(Vec::new());
        }
        h.wave(2, 1000);
        assert_eq!(h.take_messages(0).len(), 1, "refreshed");
    }

    #[test]
    fn ids_skip_zero_and_live_ones_and_survive_clear() {
        let mut h = HostWorld {
            next_id: u16::MAX,
            ..HostWorld::default()
        };
        let a = h.add(1, sight(1, 0.0));
        let b = h.add(2, sight(1, 0.0));
        assert_eq!((a, b), (u16::MAX, 1));
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.add(3, sight(1, 0.0)), 2, "a cleared world keeps counting");
    }
}
