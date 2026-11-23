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

//! The client's side of the shared world (pure): which local instances are
//! puppets of host enemies, their interpolation tracks, and the spawns and
//! despawns still to carry out. Puppets keep their own AI, animation and
//! collisions; each tick writes the host's interpolated pose and hull. Parts a
//! puppet builds while it is created belong to it and die with it; other
//! ticking `oSolid` instances are strays the glue removes. A resync drops
//! every puppet and rebuilds the world under the host's net ids.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use ndml_net::interp::{Sample, Track, Vec2};
use ndml_net::msg::{DespawnCause, EnemySnapshot, EnemySpawn, Message, NetId, PeerId, SpawnMarker};
use ndml_net::time;
use ndml_sdk::Value;

mod contain; // the GML error guard's table edits (`crate::guard`)

/// Longest a puppet keeps moving along its last velocity without snapshots.
pub const EXTRAPOLATION: Duration = Duration::from_millis(250);

/// The host sends a snapshot every second step of its 60 Hz game.
const SNAPSHOT_MS: f64 = 1000.0 / 30.0;

/// A spawn that has had no snapshot for this many frames is created anyway, at
/// its spawn pose (timeline births).
pub const BIRTH_WAIT_FRAMES: u64 = 30;

/// A spawn the host killed before its copy was made waits this many frames
/// for its first state, then goes unshown.
pub const DIE_WAIT_FRAMES: u64 = 120;

/// Whether `e` is a spawn the host killed before its copy was made: it is
/// made anyway, to die at once with its death effect.
fn killed_unmade(e: &Entry) -> bool {
    e.state == State::Pending && matches!(e.despawn, Some(DespawnCause::Killed { .. }))
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Pending,
    Live {
        key: usize,
        id: Value,
        last_tick: u64,
        /// The position written last tick (velocity for the next).
        last_pos: Option<Vec2>,
        /// The last velocity written from the host's motion: what a held
        /// tick falls back on when the copy's own speed runs away
        /// ([`held_velocity`]).
        last_vel: Option<Vec2>,
    },
    /// Not created here (no recipe); its messages are ignored quietly.
    Skipped,
}

#[derive(Debug, Clone)]
struct Entry {
    spawn: EnemySpawn,
    track: Track,
    hull_frac: f32,
    target: Option<PeerId>,
    state: State,
    despawn: Option<DespawnCause>,
    /// `EnemyLook`: the host's `image_index`, still to be written.
    look: Option<f32>,
    /// The host-clock time it is to go (timeline deaths).
    despawn_at: Option<Duration>,
    /// Frame the spawn arrived (a spawn with no snapshot is created anyway
    /// after [`BIRTH_WAIT_FRAMES`]).
    since: u64,
    /// The segment of the pose written last (no velocity across a change).
    shown_seg: Option<u8>,
    /// The host's instance is deactivated (`Message::Asleep`).
    asleep: bool,
}

#[derive(Debug, Clone)]
struct Part {
    owner: NetId,
    id: Value,
    last_tick: u64,
}

/// A puppet's writes for this tick.
#[derive(Debug, Clone, PartialEq)]
pub struct PuppetTick {
    /// The host enemy's net id.
    pub net_id: NetId,
    /// Position to write (as the host has it, never folded).
    pub pos: Vec2,
    /// Facing to write.
    pub angle: f32,
    /// The pose is the host's last sample carried on (extrapolated, or held
    /// at the extrapolation cap), not one between two of the host's.
    pub extrapolated: bool,
    /// How far it moved since last tick (px per frame), written as
    /// `hspeed`/`vspeed`. `None` when the motion is not known: the copy's own
    /// velocity stays, since a zero tells the game's code it stopped.
    pub vel: Option<Vec2>,
    /// The last velocity the host's motion gave it ([`held_velocity`]).
    pub last_vel: Option<Vec2>,
    /// The host's hull to write (also into `hullPrevious`); `None` when the
    /// host's instance has none.
    pub hull: Option<f32>,
    /// `image_index` to write once (the host's random look).
    pub image_index: Option<f32>,
    /// Remove it now (death effect first when killed).
    pub despawn: Option<Removal>,
}

/// A puppet to take out of the game.
#[derive(Debug, Clone, PartialEq)]
pub struct Removal {
    /// Why the host's instance went.
    pub cause: DespawnCause,
    /// The puppet's instance id.
    pub id: Value,
    /// Its parts' ids (destroyed first, after `instance_exists`).
    pub parts: Vec<Value>,
}

/// A held copy's own speed beyond this many times the host's last speed
/// ([`held_velocity`]) has run away.
pub const HELD_SPEED_FACTOR: f32 = 2.0;
/// ... or beyond the host's last speed and this many px per frame.
pub const HELD_SPEED_MARGIN: f32 = 20.0;

/// The velocity for a tick without one from the host: `None` leaves the
/// copy's own. A segment chain feeds on its own speed (`segmentFollowLeader`),
/// so a speed that is not finite, or beyond max([`HELD_SPEED_FACTOR`] x the
/// host's, the host's + [`HELD_SPEED_MARGIN`]), goes back to the host's last.
#[must_use]
pub fn held_velocity(own: Vec2, last: Option<Vec2>) -> Option<Vec2> {
    let last = last.filter(|v| v.x.is_finite() && v.y.is_finite())?;
    let host = last.len();
    let bound = (HELD_SPEED_FACTOR * host).max(host + HELD_SPEED_MARGIN);
    let finite = own.x.is_finite() && own.y.is_finite();
    (!finite || own.len() > bound).then_some(last)
}

/// A burst of due spawns is made within this many frames, whatever the
/// per-frame cap ([`spawn_quota`]).
pub const SPAWN_DRAIN_FRAMES: usize = 2;

/// Most host enemies made here in one frame before [`SPAWN_DRAIN_FRAMES`]
/// raises it.
pub const MAX_SPAWNS_PER_FRAME: usize = 8;

/// How many of `due` waiting copies to make this frame: `cap`, or more so a
/// burst waits no longer than [`SPAWN_DRAIN_FRAMES`].
#[must_use]
pub fn spawn_quota(cap: usize, due: usize) -> usize {
    cap.max(due.div_ceil(SPAWN_DRAIN_FRAMES))
}

/// What a ticking instance is to the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Tick {
    /// A puppet of a host enemy, with its writes.
    Puppet(PuppetTick),
    /// A part a puppet built.
    Part,
    /// Waiting to be dropped (after a resync): leave it alone.
    Dropping,
    /// Not the host's: remove it.
    Stray,
}

/// Per-frame work for the glue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    /// The world ran last frame (spawns and sweeps only then).
    pub running: bool,
    /// Instances to destroy at once, frozen or not (after a resync, a remake,
    /// a spawn made after its despawn).
    pub drop: Vec<Value>,
    /// How many of [`drop`](Self::drop) a resync put there.
    pub resync_drops: usize,
    /// Puppets that missed the last frame's ticks: check `instance_exists`
    /// and report the gone ones with [`ClientWorld::gone`].
    pub missing: Vec<(NetId, Value)>,
    /// A new `WaveState` to show.
    pub wave: Option<(u16, u32)>,
    /// Spawn telegraphs to show.
    pub markers: Vec<SpawnMarker>,
    /// Copies to deactivate (true) or activate again (false), as the host's.
    pub sleep: Vec<(Value, bool)>,
}

/// Counts for the overlay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Puppets in the game.
    pub live: usize,
    /// Spawns not made yet.
    pub pending: usize,
    /// Spawns not made here (no recipe).
    pub skipped: usize,
    /// Puppets that failed `instance_exists`.
    pub lost: u32,
    /// Strays found.
    pub strays: u32,
    /// Resyncs.
    pub resyncs: u32,
    /// Spawns the host killed before their copy was made, made to die.
    pub died_unmade: u32,
}

/// See the module docs.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // one switch per mechanism
pub struct ClientWorld {
    entries: BTreeMap<NetId, Entry>,
    by_key: HashMap<usize, NetId>,
    parts: HashMap<usize, Part>,
    fresh: HashSet<usize>,
    dropping: HashMap<usize, Value>,
    to_drop: Vec<Value>,
    resync_drops: usize,
    frame: u64,
    last_tick_frame: Option<u64>,
    player_seen: bool,
    render_time: Option<Duration>,
    wave: Option<(u16, u32)>,
    markers: Vec<SpawnMarker>,
    counts: Counts,
    /// Copies to (de)activate at the next frame ([`Frame::sleep`]).
    sleep: Vec<(Value, bool)>,
    /// Snapshot times put on whole snapshot intervals (the host's wall-clock
    /// stamps jitter by up to a step).
    regular: bool,
    /// Births and deaths when the render time reaches them (timeline).
    timeline: bool,
    /// Angles between snapshots linearly (else cubic).
    linear_angle: bool,
    /// The newest snapshot: host ms as sent, and its regular time (ms).
    snap_clock: Option<(u32, f64)>,
    /// The newest snapshot time (where a death that arrives now is placed).
    newest: Option<Duration>,
}

impl ClientWorld {
    /// An enemy snapshot part: a sample for each copy it names.
    fn on_snapshot(&mut self, snap: EnemySnapshot) {
        let t = self.snapshot_time(snap.time_ms);
        if self.newest.is_none_or(|n| t > n) {
            self.newest = Some(t);
        }
        for st in snap.enemies {
            let Some(e) = self.entries.get_mut(&st.net_id) else {
                continue; // spawn not here yet, or gone
            };
            let kept = e.track.push(Sample {
                time: t,
                pos: Vec2::new(st.x, st.y),
                vel: None,
                angle: st.angle,
                seg: st.seg,
            });
            if kept {
                e.hull_frac = st.hull_frac;
                e.target = st.target;
            }
        }
    }

    /// A message from the host.
    pub fn on_message(&mut self, msg: Message) {
        match msg.normalized() {
            Message::EnemySpawn(s) => self.on_spawn(s),
            Message::EnemySnapshot(snap) => self.on_snapshot(snap),
            Message::Asleep { net_id, asleep } => {
                if let Some(e) = self.entries.get_mut(&net_id) {
                    e.asleep = asleep;
                    if let State::Live { id, .. } = &e.state {
                        self.sleep.push((id.clone(), asleep));
                    }
                }
            }
            Message::Despawn { net_id, cause } => {
                let (timeline, newest) = (self.timeline, self.newest);
                let live = match self.entries.get_mut(&net_id) {
                    Some(e) if matches!(e.state, State::Live { .. }) => {
                        e.despawn = Some(cause);
                        e.despawn_at = newest.filter(|_| timeline);
                        true
                    }
                    // Timeline births: one born and gone unseen is still shown.
                    Some(e)
                        if timeline
                            && e.state == State::Pending
                            && e.track.first().is_some()
                            && newest.is_some() =>
                    {
                        e.despawn = Some(cause);
                        e.despawn_at = newest;
                        true
                    }
                    // Killed before its copy was made: made to die at once.
                    Some(e)
                        if e.state == State::Pending
                            && matches!(cause, DespawnCause::Killed { .. }) =>
                    {
                        e.despawn = Some(cause);
                        e.despawn_at = None;
                        true
                    }
                    _ => false,
                };
                if !live {
                    self.entries.remove(&net_id);
                }
            }
            Message::WaveState { wave, next_wave_ms } => self.wave = Some((wave, next_wave_ms)),
            Message::WorldResync { .. } => self.resync(),
            Message::EnemyLook {
                net_id,
                image_index,
            } => {
                if let Some(e) = self.entries.get_mut(&net_id)
                    && image_index.is_finite()
                {
                    e.look = Some(image_index);
                }
            }
            // A frozen client would show a pile of stale ones later.
            Message::SpawnMarker(m) if self.markers.len() < 64 => self.markers.push(m),
            _ => {}
        }
    }

    fn on_spawn(&mut self, s: EnemySpawn) {
        match self.entries.get_mut(&s.net_id) {
            // A repeat of a known puppet (same object): keep it.
            Some(e) if e.spawn.object == s.object => {}
            _ => {
                if let Some(old) = self.entries.remove(&s.net_id) {
                    self.drop_entry(old);
                }
                self.entries.insert(
                    s.net_id,
                    Entry {
                        spawn: s,
                        track: {
                            let mut t = Track::new(EXTRAPOLATION);
                            t.set_linear_angle(self.linear_angle);
                            t
                        },
                        hull_frac: if s.hull_max > 0.0 {
                            s.hull / s.hull_max
                        } else {
                            1.0
                        },
                        target: None,
                        state: State::Pending,
                        despawn: None,
                        look: None,
                        despawn_at: None,
                        since: self.frame,
                        shown_seg: None,
                        asleep: false,
                    },
                );
            }
        }
    }

    fn drop_entry(&mut self, e: Entry) {
        if let State::Live { key, id, .. } = e.state {
            if self.by_key.get(&key) == Some(&e.spawn.net_id) {
                self.by_key.remove(&key);
            }
            self.dropping.insert(key, id.clone());
            self.to_drop.push(id);
        }
    }

    /// Drop every puppet and part (they go to the drop list); keep running.
    pub fn resync(&mut self) {
        self.counts.resyncs += 1;
        let before = self.to_drop.len();
        let entries = std::mem::take(&mut self.entries);
        for e in entries.into_values() {
            self.drop_entry(e);
        }
        for (key, p) in self.parts.drain() {
            self.dropping.insert(key, p.id.clone());
            self.to_drop.push(p.id);
        }
        self.resync_drops += self.to_drop.len() - before;
        self.by_key.clear();
        self.fresh.clear();
    }

    /// Start a frame (the run tick). `render_time`: host-clock time to show
    /// enemies at (`None` before the clock syncs: newest snapshot).
    pub fn begin_frame(&mut self, render_time: Option<Duration>) -> Frame {
        let prev = self.frame;
        self.frame += 1;
        self.render_time = render_time;
        // Timeline births: a spawn whose whole life has passed unshown.
        if self.timeline
            && let Some(t) = render_time
        {
            let frame = self.frame;
            self.entries.retain(|_, e| {
                if killed_unmade(e) {
                    return frame < e.since + DIE_WAIT_FRAMES;
                }
                !(e.state == State::Pending && e.despawn_at.is_some_and(|d| t >= d))
            });
        }
        let running = self.last_tick_frame == Some(prev) || self.player_seen;
        self.player_seen = false;
        // The drop list goes out every frame, frozen too ("running" lags a
        // freeze by a frame).
        let mut f = Frame {
            running,
            wave: self.wave.take(),
            drop: std::mem::take(&mut self.to_drop),
            resync_drops: std::mem::take(&mut self.resync_drops),
            ..Frame::default()
        };
        if !running {
            return f;
        }
        f.markers = std::mem::take(&mut self.markers);
        f.sleep = std::mem::take(&mut self.sleep);
        f.missing = self
            .entries
            .iter()
            .filter_map(|(&n, e)| match &e.state {
                State::Live { last_tick, id, .. } if *last_tick < prev && !e.asleep => {
                    Some((n, id.clone()))
                }
                _ => None,
            })
            .collect();
        self.parts.retain(|_, p| p.last_tick + 1 >= prev);
        f
    }

    /// The live copy whose instance is `id`, and the key (address) the table
    /// holds for it (the stray log: a known instance at another address).
    #[must_use]
    pub fn live_by_id(&self, id: &Value) -> Option<(NetId, usize)> {
        self.entries.iter().find_map(|(&n, e)| match &e.state {
            State::Live { id: i, key, .. } if i == id => Some((n, *key)),
            _ => None,
        })
    }

    /// The drop list was carried out. Instances queued since (none in the
    /// glue's order) stay the host's ([`Tick::Dropping`]) until theirs is.
    pub fn dropped(&mut self) {
        if self.to_drop.is_empty() {
            self.dropping.clear();
        }
    }

    /// The local ship stepped (the world runs even with no enemies).
    pub fn player_stepped(&mut self) {
        self.player_seen = true;
    }

    /// A frozen frame of this game (pause, upgrade screen, a hold): every
    /// track holds its newest pose ([`Track::stop`]) until a newer snapshot,
    /// as the host's enemies stand still in the shared freeze.
    pub fn hold_tracks(&mut self) {
        for e in self.entries.values_mut() {
            e.track.stop();
        }
    }

    /// Interpolate copies' angles linearly instead of on the cubic.
    pub fn set_linear_angle(&mut self, on: bool) {
        self.linear_angle = on;
    }

    /// Every live copy's id and pose at this frame's render time (the pose
    /// its End Step tick will write), for writing it early.
    #[must_use]
    pub fn live_poses(&self) -> Vec<(Value, Vec2, f32)> {
        self.entries
            .values()
            .filter(|e| e.despawn.is_none())
            .filter_map(|e| match &e.state {
                State::Live { id, .. } => self.pose_of(e).map(|(p, a)| (id.clone(), p, a)),
                _ => None,
            })
            .collect()
    }

    /// Regular snapshot times and timeline births and deaths.
    pub fn set_timing(&mut self, regular: bool, timeline: bool) {
        self.regular = regular;
        self.timeline = timeline;
    }

    /// The time to file a snapshot under: with `regular`, whole snapshot
    /// intervals after the previous one, drifting 5 % per snapshot towards
    /// the stamp, snapping to it past 100 ms.
    fn snapshot_time(&mut self, raw: u32) -> Duration {
        if !self.regular {
            return time::from_wire_ms(raw);
        }
        let raw_ms = f64::from(raw);
        let reg = match self.snap_clock {
            Some((last_raw, last_reg)) if last_raw == raw => last_reg,
            Some((last_raw, last_reg)) => {
                let d = raw_ms - f64::from(last_raw);
                if d <= 0.0 {
                    // An older part arriving late: same offset as the newest.
                    return Duration::from_secs_f64(
                        (raw_ms + last_reg - f64::from(last_raw)).max(0.0) / 1000.0,
                    );
                }
                // Whole intervals from the regular clock, so stamp jitter does
                // not add up.
                let r =
                    last_reg + ((raw_ms - last_reg) / SNAPSHOT_MS).round().max(1.0) * SNAPSHOT_MS;
                if (raw_ms - r).abs() > 100.0 {
                    raw_ms
                } else {
                    r + (raw_ms - r) * 0.05
                }
            }
            None => raw_ms,
        };
        self.snap_clock = Some((raw, reg));
        Duration::from_secs_f64(reg.max(0.0) / 1000.0)
    }

    /// Whether a pending spawn is due: its first snapshot is on screen time
    /// (timeline births), or it has waited [`BIRTH_WAIT_FRAMES`] without one.
    fn due(&self, e: &Entry) -> bool {
        if !self.timeline {
            return true;
        }
        let Some(t) = self.render_time else {
            return true;
        };
        if killed_unmade(e) {
            return e.track.first().is_none_or(|first| t >= first.time);
        }
        match e.track.first() {
            Some(first) => t >= first.time && e.despawn_at.is_none_or(|d| t < d),
            None => self.frame >= e.since + BIRTH_WAIT_FRAMES,
        }
    }

    /// The pose to show `e` at now.
    fn pose_of(&self, e: &Entry) -> Option<(Vec2, f32)> {
        let last = e.track.latest()?;
        let t = self.render_time.unwrap_or(last.time);
        let p = e.track.sample(t)?;
        Some((p.pos, p.angle))
    }

    /// The net id of the puppet at `key` (not its parts).
    #[must_use]
    pub fn net_id(&self, key: usize) -> Option<NetId> {
        self.by_key.get(&key).copied()
    }

    /// The net id of the live copy at `key` when it is the instance `id` (as
    /// [`super::host::HostWorld::net_id_for`]).
    #[must_use]
    pub fn net_id_for(&self, key: usize, id: &Value) -> Option<NetId> {
        let net = *self.by_key.get(&key)?;
        match &self.entries.get(&net)?.state {
            State::Live { id: made, .. }
                if crate::ids::id_number(made).is_some()
                    && crate::ids::id_number(made) == crate::ids::id_number(id) =>
            {
                Some(net)
            }
            _ => None,
        }
    }

    /// Whether `net_id` is a host enemy this client still knows.
    #[must_use]
    pub fn is_known(&self, net_id: NetId) -> bool {
        self.entries.contains_key(&net_id)
    }

    /// Whether `net_id` waits to be made and is due now (timeline births).
    #[must_use]
    pub fn is_due(&self, net_id: NetId) -> bool {
        self.entries
            .get(&net_id)
            .is_some_and(|e| e.state == State::Pending && self.due(e))
    }

    /// The frame `net_id`'s spawn came (`None`: not known).
    #[must_use]
    pub fn known_since(&self, net_id: NetId) -> Option<u64> {
        self.entries.get(&net_id).map(|e| e.since)
    }

    /// Frames `net_id`'s spawn has waited since it came (`None`: not known).
    #[must_use]
    pub fn waited(&self, net_id: NetId) -> Option<u64> {
        self.known_since(net_id)
            .map(|s| self.frame.saturating_sub(s))
    }

    /// Whether `net_id` waits to be made (known, not live, not skipped).
    #[must_use]
    pub fn is_pending(&self, net_id: NetId) -> bool {
        self.entries
            .get(&net_id)
            .is_some_and(|e| e.state == State::Pending)
    }

    /// Every live copy's net id and instance id.
    #[must_use]
    pub fn live_pairs(&self) -> Vec<(NetId, Value)> {
        self.entries
            .iter()
            .filter_map(|(n, e)| match &e.state {
                State::Live { id, .. } => Some((*n, id.clone())),
                _ => None,
            })
            .collect()
    }

    /// The instance id of the live copy of `net_id`.
    #[must_use]
    pub fn id_of(&self, net_id: NetId) -> Option<Value> {
        match &self.entries.get(&net_id)?.state {
            State::Live { id, .. } => Some(id.clone()),
            _ => None,
        }
    }

    /// Spawns to carry out now, oldest first.
    #[must_use]
    pub fn pending(&self, max: usize) -> Vec<EnemySpawn> {
        self.pending_ready(max, |_| true)
    }

    /// [`Self::pending`], only the spawns `ready` accepts (the others wait).
    /// At least `max` of them, more for a burst: [`spawn_quota`].
    #[must_use]
    pub fn pending_ready(&self, max: usize, ready: impl Fn(NetId) -> bool) -> Vec<EnemySpawn> {
        let due: Vec<&Entry> = self
            .entries
            .values()
            .filter(|e| e.state == State::Pending && self.due(e) && ready(e.spawn.net_id))
            .collect();
        let quota = spawn_quota(max, due.len());
        due.into_iter()
            .take(quota)
            .map(|e| {
                let mut s = e.spawn;
                // Timeline births: made where its timeline has it now.
                if self.timeline
                    && let Some((pos, angle)) = self.pose_of(e)
                {
                    (s.x, s.y, s.angle) = (pos.x, pos.y, angle);
                }
                s
            })
            .collect()
    }

    /// A spawn was created as instance `id` at `key`.
    pub fn spawned(&mut self, net_id: NetId, key: usize, id: Value) {
        let Some(e) = self.entries.get_mut(&net_id) else {
            // Despawned meanwhile: it has to go again.
            self.dropping.insert(key, id.clone());
            self.to_drop.push(id);
            return;
        };
        if killed_unmade(e) {
            self.counts.died_unmade += 1;
        }
        e.state = State::Live {
            key,
            id,
            last_tick: self.frame,
            last_pos: None,
            last_vel: None,
        };
        self.by_key.insert(key, net_id);
        self.fresh.insert(key);
    }

    /// A spawn is not created here (no recipe, or the create failed).
    pub fn skipped(&mut self, net_id: NetId) {
        if let Some(e) = self.entries.get_mut(&net_id) {
            e.state = State::Skipped;
        }
    }

    /// An `oSolid` was created at `key` while `owner` was being built (or by
    /// one of its fresh parts).
    pub fn record_part(&mut self, owner: NetId, key: usize, id: Value) {
        if self.by_key.contains_key(&key) {
            return;
        }
        self.parts.insert(
            key,
            Part {
                owner,
                id,
                last_tick: self.frame,
            },
        );
        self.fresh.insert(key);
    }

    /// The puppet that owns `key` (itself, or as its part).
    #[must_use]
    pub fn owner_of(&self, key: usize) -> Option<NetId> {
        self.by_key
            .get(&key)
            .copied()
            .or_else(|| self.parts.get(&key).map(|p| p.owner))
    }

    /// Created by a host order and not ticked yet (its creates are let through).
    #[must_use]
    pub fn is_fresh(&self, key: usize) -> bool {
        self.fresh.contains(&key)
    }

    /// Whether the host decides this instance's death (puppet, part, dropping).
    #[must_use]
    pub fn owns(&self, key: usize) -> bool {
        self.by_key.contains_key(&key)
            || self.parts.contains_key(&key)
            || self.dropping.contains_key(&key)
    }

    /// The host's last hull for the copy at `key` while the host's instance
    /// lives (above 0, no despawn); `None` otherwise.
    #[must_use]
    pub fn host_hull_alive(&self, key: usize) -> Option<f32> {
        let e = self.entries.get(self.by_key.get(&key)?)?;
        (e.spawn.has_hull() && e.hull_frac > 0.0 && e.despawn.is_none())
            .then(|| e.hull_frac.min(1.0) * e.spawn.hull_max)
    }

    /// An `oSolid` at `key` ticked.
    #[allow(clippy::too_many_lines)] // one decision per tick, kept in one place
    pub fn tick(&mut self, key: usize) -> Tick {
        self.last_tick_frame = Some(self.frame);
        self.fresh.remove(&key);
        if let Some(p) = self.parts.get_mut(&key) {
            p.last_tick = self.frame;
            return Tick::Part;
        }
        if self.dropping.contains_key(&key) {
            return Tick::Dropping;
        }
        let Some(&net_id) = self.by_key.get(&key) else {
            self.counts.strays += 1;
            return Tick::Stray;
        };
        let frame = self.frame;
        let render_time = self.render_time;
        let linear_angle = self.linear_angle;
        let Some(e) = self.entries.get_mut(&net_id) else {
            self.by_key.remove(&key);
            return Tick::Stray;
        };
        e.track.set_linear_angle(linear_angle);
        // Timeline deaths: it goes when the render time reaches its death.
        let due = e
            .despawn_at
            .is_none_or(|d| render_time.is_none_or(|t| t >= d));
        if let Some(cause) = e.despawn.filter(|_| due) {
            let State::Live { id, last_pos, .. } = e.state.clone() else {
                return Tick::Dropping;
            };
            let at = last_pos.unwrap_or(Vec2::new(e.spawn.x, e.spawn.y));
            let (x, y, angle) = (at.x, at.y, e.spawn.angle);
            self.entries.remove(&net_id);
            self.by_key.remove(&key);
            let mut parts = Vec::new();
            self.parts.retain(|_, p| {
                let mine = p.owner == net_id;
                if mine {
                    parts.push(p.id.clone());
                }
                !mine
            });
            return Tick::Puppet(PuppetTick {
                net_id,
                pos: Vec2::new(x, y),
                angle,
                extrapolated: false,
                vel: None,
                last_vel: None,
                hull: None,
                image_index: None,
                despawn: Some(Removal { cause, id, parts }),
            });
        }
        let pose = e
            .track
            .latest()
            .and_then(|last| e.track.sample(render_time.unwrap_or(last.time)));
        let extrapolated = pose.is_some_and(|p| p.extrapolated);
        let (pos, angle, seg) = pose.map_or_else(
            || (Vec2::new(e.spawn.x, e.spawn.y), e.spawn.angle, None),
            |p| (p.pos, p.angle, Some(p.seg)),
        );
        let same_seg = seg == e.shown_seg;
        e.shown_seg = seg;
        // No sample yet, or none within the extrapolation cap: the pose
        // stands still here whatever the host's instance does.
        let held = pose.is_none() || render_time.is_some_and(|t| e.track.holds_at(t));
        if let Some(t) = render_time {
            e.track.prune_before(t);
        }
        let host_hull = e.hull_frac.clamp(0.0, 1.0) * e.spawn.hull_max;
        let mut vel = None;
        let mut host_vel = None;
        if let State::Live {
            last_tick,
            last_pos,
            last_vel,
            ..
        } = &mut e.state
        {
            if let Some(lp) = *last_pos {
                // Across a new segment (the host's game moved it) the step
                // is no motion, and a held pose says nothing of it.
                vel = (same_seg && !held)
                    .then(|| pos - lp)
                    .filter(|v| v.x.is_finite() && v.y.is_finite());
            }
            if vel.is_some() {
                *last_vel = vel;
            }
            host_vel = *last_vel;
            *last_pos = Some(pos);
            *last_tick = frame;
        }
        Tick::Puppet(PuppetTick {
            net_id,
            pos,
            angle,
            extrapolated,
            vel,
            last_vel: host_vel,
            hull: e.spawn.has_hull().then_some(host_hull),
            image_index: e.look.take(),
            despawn: None,
        })
    }

    /// The instances of the live copies not asleep with the host's.
    #[must_use]
    pub fn awake_ids(&self) -> Vec<Value> {
        self.entries
            .values()
            .filter(|e| !e.asleep)
            .filter_map(|e| match &e.state {
                State::Live { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    /// The live copies whose despawn came while they were frozen.
    #[must_use]
    pub fn despawn_waiting(&self) -> Vec<Value> {
        self.entries
            .values()
            .filter(|e| e.despawn.is_some())
            .filter_map(|e| match &e.state {
                State::Live { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Copies whose host instance is asleep (deactivated there).
    #[must_use]
    pub fn asleep_ids(&self) -> Vec<Value> {
        self.entries
            .values()
            .filter(|e| e.asleep)
            .filter_map(|e| match &e.state {
                State::Live { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    /// These puppets failed `instance_exists` (gone locally): forget them.
    pub fn gone(&mut self, ids: &[NetId]) {
        for id in ids {
            if let Some(e) = self.entries.remove(id) {
                if let State::Live { key, .. } = e.state {
                    self.by_key.remove(&key);
                }
                self.counts.lost += 1;
            }
        }
    }

    /// The player the puppet at `key` goes after, per the host.
    #[must_use]
    pub fn assignment(&self, key: usize) -> Option<PeerId> {
        self.entries.get(self.by_key.get(&key)?)?.target
    }

    /// Counts for the overlay.
    #[must_use]
    pub fn counts(&self) -> Counts {
        let mut c = self.counts;
        for e in self.entries.values() {
            match e.state {
                State::Pending => c.pending += 1,
                State::Live { .. } => c.live += 1,
                State::Skipped => c.skipped += 1,
            }
        }
        c
    }

    /// Forget everything without touching the game (run ended, or the world
    /// handed over); settings are kept.
    pub fn clear(&mut self) {
        let (regular, timeline, linear_angle) = (self.regular, self.timeline, self.linear_angle);
        *self = Self {
            regular,
            timeline,
            linear_angle,
            ..Self::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::msg::{EnemySnapshot, EnemyState};

    /// A held copy keeps its own speed while it is within the bound, and
    /// gets the host's last velocity back (never a zero) when its own is
    /// not finite or has run away; with no host velocity yet it is left.
    #[test]
    fn a_held_copy_runs_away_no_further_than_the_bound() {
        let host = Some(Vec2::new(-4.7, 9.1)); // 10.2 px a frame
        assert_eq!(held_velocity(Vec2::new(-5.0, 9.5), host), None);
        assert_eq!(held_velocity(Vec2::new(0.0, 30.0), host), None);
        assert_eq!(held_velocity(Vec2::new(0.0, 31.0), host), host);
        assert_eq!(held_velocity(Vec2::new(f32::NAN, 1.0), host), host);
        assert_eq!(held_velocity(Vec2::new(1.0, f32::INFINITY), host), host);
        assert_eq!(held_velocity(Vec2::new(1200.0, 800.0), host), host);
        // A slow host: the margin, not the factor, is the bound.
        let slow = Some(Vec2::new(0.5, 0.0));
        assert_eq!(held_velocity(Vec2::new(20.0, 0.0), slow), None);
        assert_eq!(held_velocity(Vec2::new(21.0, 0.0), slow), slow);
        // No host velocity: nothing to fall back on.
        assert_eq!(held_velocity(Vec2::new(f32::NAN, 0.0), None), None);
        let nan = Some(Vec2::new(f32::NAN, 0.0));
        assert_eq!(held_velocity(Vec2::new(f32::NAN, 0.0), nan), None);
    }

    /// A chain fed as `segmentFollowLeader` feeds it (each step every
    /// segment adds a share of its speed to its leader's and its
    /// follower's), held for 150 s: on its own it runs to inf and NaN; with
    /// each held tick bounded it stays within a step's feed of the bound.
    #[test]
    fn a_held_chain_does_not_feed_on_itself_past_the_bound() {
        const SEGMENTS: usize = 30;
        const FRAMES: usize = 150 * 60;
        const SHARE: f32 = 0.06;
        let host = Vec2::new(-4.7, 9.1);
        let step = |s: &mut [Vec2]| {
            for i in 0..s.len() {
                let give = s[i] * SHARE;
                if i > 0 {
                    s[i - 1] = s[i - 1] + give;
                }
                if i + 1 < s.len() {
                    s[i + 1] = s[i + 1] + give;
                }
            }
        };
        let mut free = [host; SEGMENTS];
        let mut held = [host; SEGMENTS];
        let mut most = 0.0f32;
        for _ in 0..FRAMES {
            step(&mut free);
            step(&mut held);
            for v in &mut held {
                if let Some(back) = held_velocity(*v, Some(host)) {
                    *v = back;
                }
                most = most.max(v.len());
            }
        }
        assert!(
            free.iter().any(|v| v.len() >= 1e30 || v.len().is_nan()),
            "the toy chain should run away on its own"
        );
        let bound = (HELD_SPEED_FACTOR * host.len()).max(host.len() + HELD_SPEED_MARGIN);
        assert!(
            most <= bound * (1.0 + 2.0 * SHARE) + 1e-3,
            "{most} vs {bound}"
        );
        assert!(held.iter().all(|v| v.x.is_finite() && v.y.is_finite()));
    }

    fn spawn(net_id: NetId, object: u16) -> Message {
        Message::EnemySpawn(EnemySpawn {
            net_id,
            object,
            x: 100.0,
            y: 200.0,
            angle: 10.0,
            hull: 50.0,
            hull_max: 100.0,
            flags: 0,
        })
    }

    fn snap(ms: u32, net_id: NetId, x: f32, frac: f32, target: Option<PeerId>) -> Message {
        Message::EnemySnapshot(EnemySnapshot {
            time_ms: ms,
            part: 0,
            parts: 1,
            enemies: vec![
                EnemyState {
                    seg: 0,
                    net_id,
                    x,
                    y: 200.0,
                    angle: 20.0,
                    hull_frac: frac,
                    target,
                }
                .quantized(),
            ],
        })
    }

    fn id(n: u32) -> Value {
        Value::Real(f64::from(100_000 + n))
    }

    fn puppet(t: Tick) -> Option<PuppetTick> {
        match t {
            Tick::Puppet(p) => Some(p),
            _ => None,
        }
    }

    /// A world with net id 1 live as instance key 11.
    fn live_world() -> ClientWorld {
        let mut c = ClientWorld::default();
        c.on_message(spawn(1, 0x325));
        c.player_stepped();
        let f = c.begin_frame(None);
        assert!(f.running);
        assert_eq!(c.pending(8).len(), 1);
        c.spawned(1, 11, id(1));
        c
    }

    /// The live copy's address answers only for that copy's instance.
    #[test]
    fn a_copys_address_answers_only_for_its_instance() {
        let c = live_world();
        assert_eq!(c.net_id_for(11, &id(1)), Some(1));
        assert_eq!(c.net_id_for(11, &Value::Ref(100_001)), Some(1), "a ref");
        assert_eq!(c.net_id_for(11, &id(2)), None, "another instance there");
        assert_eq!(c.net_id_for(12, &id(1)), None);
    }

    /// A copy its own code destroyed in the last running frame (no tick
    /// there) is among the freeze's first frame's missed copies, so it can
    /// leave the table then; a later frozen frame lists none.
    #[test]
    fn a_freezes_first_frame_lists_what_the_last_running_frame_missed() {
        let mut c = ClientWorld::default();
        c.on_message(spawn(1, 0x325));
        c.on_message(spawn(2, 0x325));
        c.player_stepped();
        assert!(c.begin_frame(None).running);
        c.spawned(1, 11, id(1));
        c.spawned(2, 12, id(2));
        // A running frame: both tick.
        c.player_stepped();
        assert!(c.begin_frame(None).running);
        let _ = c.tick(11);
        let _ = c.tick(12);
        // The last running frame: copy 2 destroyed in its own Step.
        c.player_stepped();
        assert!(c.begin_frame(None).running);
        let _ = c.tick(11);
        // The freeze's first frame: still "running" (it lags a frame).
        let f = c.begin_frame(None);
        assert!(f.running);
        let missed: Vec<NetId> = f.missing.iter().map(|m| m.0).collect();
        assert_eq!(missed, vec![2]);
        // A later frozen frame: none.
        let f = c.begin_frame(None);
        assert!(!f.running && f.missing.is_empty());
    }

    #[test]
    fn a_burst_is_made_within_two_frames() {
        assert_eq!(spawn_quota(8, 3), 8);
        assert_eq!(spawn_quota(8, 16), 8);
        assert_eq!(spawn_quota(8, 70), 35);
        assert_eq!(spawn_quota(usize::MAX, 5), usize::MAX);
        let mut c = ClientWorld::default();
        for n in 1..=30 {
            c.on_message(spawn(n, 0x325));
        }
        c.player_stepped();
        assert!(c.begin_frame(None).running);
        let p = c.pending(8);
        assert_eq!(p.len(), 15, "half the burst this frame");
        assert_eq!(p[0].net_id, 1, "oldest first");
    }

    #[test]
    fn a_copy_of_an_instance_without_hull_is_given_none() {
        let mut c = ClientWorld::default();
        let Message::EnemySpawn(mut s) = spawn(1, 0x302) else {
            unreachable!()
        };
        s.hull = 0.0;
        s.flags |= EnemySpawn::NO_HULL;
        c.on_message(Message::EnemySpawn(s));
        c.player_stepped();
        assert!(c.begin_frame(None).running);
        assert_eq!(c.pending(8).len(), 1);
        c.spawned(1, 11, id(1));
        c.on_message(snap(1000, 1, 300.0, 0.0, None));
        let p = puppet(c.tick(11)).unwrap();
        assert_eq!(p.hull, None, "no hull written, the copy has none");
    }

    #[test]
    fn spawn_tick_interpolate_and_hull() {
        let mut c = live_world();
        assert!(c.pending(8).is_empty());
        assert!(c.is_fresh(11));
        // No snapshot yet: held at the spawn pose, spawn hull.
        let p = puppet(c.tick(11)).unwrap();
        assert!(!c.is_fresh(11), "first tick ends the fresh window");
        assert!((p.pos.x - 100.0).abs() < 1e-3 && (p.hull.unwrap() - 50.0).abs() < 1e-3);
        c.on_message(snap(1000, 1, 300.0, 0.25, Some(PeerId(1))));
        c.on_message(snap(1100, 1, 400.0, 0.25, Some(PeerId(1))));
        c.begin_frame(Some(Duration::from_millis(1050)));
        let p = puppet(c.tick(11)).unwrap();
        assert!((p.pos.x - 350.0).abs() < 1.0, "{}", p.pos.x);
        assert!((p.hull.unwrap() - 25.0).abs() < 0.01);
        assert_eq!(c.assignment(11), Some(PeerId(1)));
        assert_eq!(c.counts().live, 1);
    }

    #[test]
    fn after_a_freeze_a_copy_stands_at_the_hosts_last_pose_until_its_next() {
        let mut c = live_world();
        c.on_message(snap(1000, 1, 300.0, 1.0, None));
        c.on_message(snap(1100, 1, 400.0, 1.0, None));
        // Frozen frames (both games: the shared pause), then the resume
        // before the host's first snapshot after it.
        c.hold_tracks();
        c.begin_frame(Some(Duration::from_secs(3)));
        let p = puppet(c.tick(11)).unwrap();
        assert!(
            (p.pos.x - 400.0).abs() < 1.0,
            "not 250 ms ahead: {}",
            p.pos.x
        );
        c.begin_frame(Some(Duration::from_millis(3017)));
        let p = puppet(c.tick(11)).unwrap();
        assert!((p.pos.x - 400.0).abs() < 1.0 && p.vel.is_none(), "{p:?}");
        // The host's snapshots after its resume: it goes on from there.
        c.on_message(snap(3100, 1, 400.0, 1.0, None));
        c.on_message(snap(3133, 1, 410.0, 1.0, None));
        c.begin_frame(Some(Duration::from_millis(3117)));
        let p = puppet(c.tick(11)).unwrap();
        assert!(p.pos.x > 400.5 && p.pos.x < 410.0, "{}", p.pos.x);
    }

    #[test]
    fn the_hosts_hull_says_whether_a_copy_may_die_by_its_own_step() {
        let mut c = live_world();
        c.record_part(1, 12, id(2));
        let near = |v: Option<f32>, want: f32| v.is_some_and(|h| (h - want).abs() < 1e-3);
        assert!(near(c.host_hull_alive(11), 50.0), "the spawn hull");
        assert_eq!(c.host_hull_alive(12), None, "a part: no hull of the host's");
        assert_eq!(c.host_hull_alive(99), None);
        c.on_message(snap(1000, 1, 300.0, 0.0, None));
        c.begin_frame(Some(Duration::from_secs(1)));
        let _ = c.tick(11);
        assert_eq!(c.host_hull_alive(11), None, "the host's hull at 0");
        c.on_message(snap(1100, 1, 300.0, 0.25, None));
        c.begin_frame(Some(Duration::from_millis(1100)));
        let _ = c.tick(11);
        assert!(
            near(c.host_hull_alive(11), 25.0),
            "{:?}",
            c.host_hull_alive(11)
        );
        c.on_message(Message::Despawn {
            net_id: 1,
            cause: DespawnCause::Killed { by: None },
        });
        assert_eq!(c.host_hull_alive(11), None, "the host's despawn");
    }

    #[test]
    fn despawn_removes_on_the_next_tick_with_parts() {
        let mut c = live_world();
        c.record_part(1, 12, id(2));
        assert_eq!(c.owner_of(12), Some(1));
        assert!(c.owns(12) && c.owns(11) && !c.owns(99));
        assert_eq!(c.tick(12), Tick::Part);
        c.on_message(Message::Despawn {
            net_id: 1,
            cause: DespawnCause::Killed { by: None },
        });
        let p = puppet(c.tick(11)).unwrap();
        let r = p.despawn.unwrap();
        assert_eq!(r.id, id(1));
        assert_eq!(r.parts, vec![id(2)]);
        assert!(matches!(r.cause, DespawnCause::Killed { .. }));
        assert!(!c.owns(11) && !c.owns(12));
        assert_eq!(c.tick(11), Tick::Stray, "forgotten");
        // Despawn of something never created: just forgotten.
        c.on_message(spawn(5, 0x214));
        c.on_message(Message::Despawn {
            net_id: 5,
            cause: DespawnCause::Removed,
        });
        assert!(c.pending(8).is_empty());
        // Killed before it was made: made anyway, to die at its first tick.
        c.on_message(spawn(6, 0x214));
        c.on_message(Message::Despawn {
            net_id: 6,
            cause: DespawnCause::Killed { by: None },
        });
        assert_eq!(c.pending(8).len(), 1, "made to die");
        c.spawned(6, 16, id(6));
        assert_eq!(c.counts().died_unmade, 1);
        let r = puppet(c.tick(16)).unwrap().despawn.unwrap();
        assert!(matches!(r.cause, DespawnCause::Killed { .. }));
    }

    #[test]
    fn velocity_look_and_markers() {
        let mut c = live_world();
        c.on_message(Message::EnemyLook {
            net_id: 1,
            image_index: 3.0,
        });
        c.on_message(Message::SpawnMarker(SpawnMarker {
            kind: SpawnMarker::WARNING,
            x: 1.0,
            y: 2.0,
            scale: 0.75,
            object: 0,
            rarity: 0.0,
        }));
        let p = puppet(c.tick(11)).unwrap();
        assert_eq!(p.image_index, Some(3.0));
        assert_eq!(p.vel, None, "no previous position yet");
        let p = puppet(c.tick(11)).unwrap();
        assert_eq!(p.image_index, None, "written once");
        c.player_stepped();
        let f = c.begin_frame(None);
        assert_eq!(f.markers.len(), 1);
        assert!(c.begin_frame(None).markers.is_empty());
    }

    /// A copy is given a velocity only when its motion is known: never a zero
    /// for "not known", which the game's own code reads as stopped (a speed of
    /// 0 makes a black hole act as a Scion's stationary hole without its
    /// Scion, which closes).
    #[test]
    fn a_copys_velocity_is_written_only_when_its_motion_is_known() {
        let at = Duration::from_millis;
        let mut c = live_world();
        let vel = |c: &mut ClientWorld, t: u64| {
            c.player_stepped();
            c.begin_frame(Some(at(t)));
            puppet(c.tick(11)).unwrap().vel
        };
        assert_eq!(vel(&mut c, 990), None, "no sample yet");
        c.on_message(snap(1000, 1, 100.0, 1.0, None));
        c.on_message(snap(1033, 1, 133.0, 1.0, None));
        assert_eq!(vel(&mut c, 1000), None, "from the spawn pose to the first");
        let v = vel(&mut c, 1016).expect("moving between two samples");
        assert!(v.x > 10.0, "{v:?}");
        assert!(vel(&mut c, 1100).is_some(), "extrapolated, within the cap");
        // The link stalls: from the cap on the pose is held, motion unknown.
        assert_eq!(vel(&mut c, 1283), None);
        assert_eq!(vel(&mut c, 1300), None);
        // A newer sample moves it again.
        c.on_message(snap(1316, 1, 160.0, 1.0, None));
        c.on_message(snap(1350, 1, 190.0, 1.0, None));
        assert!(vel(&mut c, 1320).is_some());
    }

    #[test]
    fn strays_skips_and_lost_puppets() {
        let mut c = live_world();
        assert_eq!(c.tick(77), Tick::Stray);
        c.on_message(spawn(2, 0x1fa));
        c.skipped(2);
        c.on_message(snap(10, 2, 1.0, 1.0, None)); // ignored quietly
        assert_eq!(c.counts().skipped, 1);
        assert!(c.pending(8).is_empty());
        // Puppet 1 stops ticking while the world runs.
        c.tick(11);
        c.begin_frame(None);
        c.player_stepped();
        let f = c.begin_frame(None);
        assert_eq!(f.missing, vec![(1, id(1))]);
        c.gone(&[1]);
        assert_eq!(c.counts().lost, 1);
        assert_eq!(c.counts().live, 0);
        assert!(c.counts().strays >= 1);
    }

    #[test]
    fn frozen_frames_neither_spawn_nor_sweep() {
        let mut c = ClientWorld::default();
        c.on_message(spawn(1, 0x325));
        let f = c.begin_frame(None);
        assert!(!f.running && f.missing.is_empty());
        c.on_message(Message::WaveState {
            wave: 4,
            next_wave_ms: 9000,
        });
        assert_eq!(c.begin_frame(None).wave, Some((4, 9000)));
        assert_eq!(c.begin_frame(None).wave, None, "once");
    }

    #[test]
    fn resync_drops_everything_and_rebuilds_under_the_same_ids() {
        let mut c = live_world();
        c.record_part(1, 12, id(2));
        c.on_message(spawn(3, 0x214)); // pending
        c.on_message(Message::WorldResync { enemies: 2 });
        assert_eq!(c.counts().live + c.counts().pending, 0);
        // Old instances tick meanwhile: left alone, still the host's call.
        assert_eq!(c.tick(11), Tick::Dropping);
        assert_eq!(c.tick(12), Tick::Dropping);
        assert!(c.owns(11));
        // The full state follows.
        c.on_message(spawn(1, 0x325));
        c.on_message(spawn(3, 0x214));
        let f = c.begin_frame(None);
        assert!(f.running);
        let mut drop = f.drop.clone();
        drop.sort_by(|a, b| a.as_f64().partial_cmp(&b.as_f64()).unwrap());
        assert_eq!(drop, vec![id(1), id(2)]);
        c.dropped();
        assert!(!c.owns(11) && !c.owns(12));
        let p = c.pending(8);
        assert_eq!(p.iter().map(|s| s.net_id).collect::<Vec<_>>(), vec![1, 3]);
        c.spawned(1, 21, id(21));
        c.spawned(3, 23, id(23));
        assert_eq!(c.counts().live, 2);
        assert_eq!(c.counts().resyncs, 1);
        assert!(puppet(c.tick(21)).is_some());
    }

    #[test]
    fn a_resync_in_a_freeze_drops_the_old_copies_in_the_freeze() {
        let mut c = live_world();
        c.record_part(1, 12, id(2));
        // Frozen: nothing ticks, so the next frames do not run.
        let _ = c.begin_frame(None);
        assert!(!c.begin_frame(None).running);
        c.on_message(Message::WorldResync { enemies: 1 });
        // Until the frame's drop the old instances are the host's (deactivated
        // by the freeze, they do not tick).
        assert!(c.owns(11) && c.owns(12));
        let f = c.begin_frame(None);
        assert!(!f.running, "still frozen");
        let mut drop = f.drop.clone();
        drop.sort_by(|a, b| a.as_f64().partial_cmp(&b.as_f64()).unwrap());
        assert_eq!(drop, vec![id(1), id(2)], "dropped in the freeze");
        assert_eq!(f.resync_drops, 2);
        c.dropped();
        assert!(!c.owns(11) && !c.owns(12));
        // The twins come only with a running frame (spawns never in a freeze).
        c.on_message(spawn(1, 0x325));
        c.player_stepped();
        let f = c.begin_frame(None);
        assert!(f.running && f.drop.is_empty() && f.resync_drops == 0);
        assert_eq!(c.pending(8).len(), 1);
    }

    #[test]
    fn a_remake_after_an_error_is_dropped_but_not_counted_as_a_resync() {
        let mut c = live_world();
        assert!(c.redo_after_error(1, true));
        let f = c.begin_frame(None);
        assert_eq!((f.drop.len(), f.resync_drops), (1, 0));
    }

    #[test]
    fn repeated_spawn_keeps_the_puppet_and_a_late_spawn_is_dropped() {
        let mut c = live_world();
        c.on_message(spawn(1, 0x325));
        assert!(c.pending(8).is_empty(), "same object: kept");
        c.on_message(spawn(1, 0x214));
        assert_eq!(
            c.pending(8).len(),
            1,
            "another object under the id: rebuilt"
        );
        assert_eq!(c.tick(11), Tick::Dropping);
        // A create that finishes after its despawn arrived.
        c.on_message(spawn(9, 0x325));
        c.on_message(Message::Despawn {
            net_id: 9,
            cause: DespawnCause::Removed,
        });
        c.spawned(9, 19, id(19));
        assert_eq!(c.tick(19), Tick::Dropping);
    }

    #[test]
    fn snapshot_times_are_put_on_whole_intervals() {
        let mut c = ClientWorld::default();
        c.set_timing(true, false);
        // Host stamps 33 ms apart on average but 17 or 50 apart in turn.
        let stamps = [1000u32, 1017, 1067, 1084, 1134, 1151, 1201];
        let t: Vec<f64> = stamps
            .iter()
            .map(|s| c.snapshot_time(*s).as_secs_f64() * 1000.0)
            .collect();
        for w in t.windows(2) {
            let d = w[1] - w[0];
            assert!((d - SNAPSHOT_MS).abs() < 3.0, "{t:?}");
        }
        // The same stamp twice (parts of one snapshot) files the same time.
        assert!((c.snapshot_time(1201).as_secs_f64() * 1000.0 - t[6]).abs() < 1e-9);
        // A long gap: whole intervals again, near the stamp.
        let j = c.snapshot_time(5000).as_secs_f64() * 1000.0;
        assert!((j - 5000.0).abs() < 20.0, "{j}");
        // Off: the stamp as sent.
        c.set_timing(false, false);
        assert_eq!(c.snapshot_time(5017), Duration::from_millis(5017));
    }

    #[test]
    fn timeline_births_and_deaths() {
        let mut c = ClientWorld::default();
        c.set_timing(false, true);
        c.on_message(spawn(1, 0x325));
        c.on_message(snap(1000, 1, 300.0, 1.0, None));
        c.on_message(snap(1033, 1, 310.0, 1.0, None));
        c.player_stepped();
        // Render time before the first snapshot: not yet.
        c.begin_frame(Some(Duration::from_millis(990)));
        assert!(c.pending(8).is_empty());
        c.player_stepped();
        c.begin_frame(Some(Duration::from_millis(1010)));
        let p = c.pending(8);
        assert_eq!(p.len(), 1);
        assert!(
            (p[0].x - 303.0).abs() < 0.6,
            "made where its timeline is: {}",
            p[0].x
        );
        c.spawned(1, 11, id(1));
        // Its death arrives with the 1033 snapshot newest: it goes at 1033.
        c.on_message(Message::Despawn {
            net_id: 1,
            cause: DespawnCause::Removed,
        });
        c.player_stepped();
        c.begin_frame(Some(Duration::from_millis(1020)));
        assert!(puppet(c.tick(11)).unwrap().despawn.is_none(), "not yet");
        c.player_stepped();
        c.begin_frame(Some(Duration::from_millis(1034)));
        assert!(puppet(c.tick(11)).unwrap().despawn.is_some());
        // Born and gone before the render time reached it: never made.
        c.on_message(spawn(2, 0x325));
        c.on_message(snap(2000, 2, 300.0, 1.0, None));
        c.on_message(Message::Despawn {
            net_id: 2,
            cause: DespawnCause::Removed,
        });
        c.player_stepped();
        c.begin_frame(Some(Duration::from_millis(1990)));
        assert!(c.pending(8).is_empty());
        assert_eq!(c.counts().pending, 1, "kept for its moment");
        c.player_stepped();
        c.begin_frame(Some(Duration::from_millis(2001)));
        assert_eq!(c.counts().pending, 0, "its life passed");
        // No snapshot at all: made after the wait.
        c.on_message(spawn(3, 0x325));
        for _ in 0..BIRTH_WAIT_FRAMES {
            c.player_stepped();
            c.begin_frame(Some(Duration::from_millis(2100)));
        }
        assert_eq!(c.pending(8).len(), 1);
    }
}
