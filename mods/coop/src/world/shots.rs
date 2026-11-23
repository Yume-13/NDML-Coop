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

//! Shared enemy shots, the pure part (the game side is `shots_glue`).
//!
//! The host owns which shots exist: it sends a shot's state the frame it is
//! made (`ShotFire`) and its end (`ShotGone`); a joining game makes it at its
//! shown time, and the copies' own creates of the family are scratch there
//! (removed at the frame's end, before anything draws them). Ends fall in
//! classes ([`END_SITES`]): the joining player's (reported), the world's
//! (vetoed there: the host decides) and the shot's own (each game its own).
//! The host keeps a shot its `destroyOutside` would end while it is in the
//! joining player's view ([`keep_for_view`]).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use ndml_net::msg::PeerId;
use ndml_net::shots::{FiredShot, ShotEnd, SteerChange, cause, steer};
use ndml_sdk::Value;

/// How a function that can end a shared shot is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndClass {
    /// The local player's things (ship, shield, segments, constructs,
    /// blades, swords, shots): a joining game reports the end.
    Player,
    /// The world or enemy-side code (asteroids, the shot leaving the view,
    /// a boss's own mechanics): vetoed for a shared shot on a joining game;
    /// the host's game decides.
    World,
    /// The mirror's player-side rule decides at run time (`self` or `other`
    /// player-side: [`Self::Player`], else [`Self::World`]): black holes are
    /// made by players and by the world.
    ByRule,
    /// The shot's own event ends it (a timer, its step): each game runs its
    /// own, nothing is reported.
    Own,
    /// A player's thing's own event that ends the shots it finds itself
    /// (the railgun's Step clears the shots on its line): no shot is `self`
    /// or `other`, so every shared shot gone across it is a player end
    /// (reported by a joining game, as [`Self::Player`]).
    PlayerSweep,
}

/// Every game function that can end an `oEnemyProjectile`-family shot in
/// 1.2.38.4 (collision events with a family member on either side, the
/// family's own destroying events, functions that destroy a family object),
/// classified. Sorted by name (binary search).
pub const END_SITES: &[(&str, EndClass)] = &[
    ("gml_Object_oArcTail_Step_0", EndClass::Own),
    (
        "gml_Object_oAsteroid_Collision_oEnemyProjectile",
        EndClass::World,
    ),
    ("gml_Object_oBeamcasterShot_Step_0", EndClass::Own),
    (
        "gml_Object_oBlackHole_Collision_oEnemyProjectile",
        EndClass::ByRule,
    ),
    (
        "gml_Object_oBladeChild_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oBlade_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oBlaster_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oBoltThrowerShot_Step_0", EndClass::Own),
    ("gml_Object_oCelestialShot_Step_0", EndClass::Own),
    ("gml_Object_oCelestialStrike_Alarm_2", EndClass::Own),
    ("gml_Object_oCeratotitanProjectile_Alarm_2", EndClass::Own),
    (
        "gml_Object_oConstructMine_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oConstructSwarm_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oCosmicEgg_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oDart_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oDreadnoughtBallShot_Step_0", EndClass::Own),
    (
        "gml_Object_oDreadnoughtBall_Collision_oDreadnoughtOrbFeeder",
        EndClass::World,
    ),
    ("gml_Object_oDreadnoughtOrbFeeder_Step_0", EndClass::Own),
    ("gml_Object_oDreadnoughtVulcan_Destroy_0", EndClass::Own),
    ("gml_Object_oDreadnoughtVulcan_Step_0", EndClass::Own),
    ("gml_Object_oDwellerGuardShot_Step_0", EndClass::Own),
    ("gml_Object_oEclipseBolt_Alarm_4", EndClass::Own),
    ("gml_Object_oEclipseOrb_Alarm_0", EndClass::Own),
    ("gml_Object_oEclipseOrb_Alarm_4", EndClass::Own),
    ("gml_Object_oEclipseShot_Step_0", EndClass::Own),
    ("gml_Object_oEclipseTorrent_Step_0", EndClass::Own),
    (
        "gml_Object_oEnemyProjectile_Collision_oAsteroid",
        EndClass::World,
    ),
    ("gml_Object_oEnemyProjectile_Other_23", EndClass::Own),
    (
        "gml_Object_oFlak_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oGlaucusProjectile_Alarm_2", EndClass::Own),
    (
        "gml_Object_oGrenade_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oGyrogunShot_Step_0", EndClass::Own),
    (
        "gml_Object_oHelixOrb_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oJuggernautShot_Destroy_0", EndClass::Own),
    ("gml_Object_oJuggernautShot_Step_0", EndClass::Own),
    ("gml_Object_oMineDroneShot_Destroy_0", EndClass::Own),
    ("gml_Object_oMyrmidonBomb_Alarm_2", EndClass::Own),
    ("gml_Object_oPlasmaShot_Alarm_4", EndClass::Own),
    (
        "gml_Object_oPlayerConstruct_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oPlayerSegment_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oPlayer_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oPulsarBarrage_Step_0", EndClass::Own),
    ("gml_Object_oPulseShotgunHoming_Step_0", EndClass::Own),
    ("gml_Object_oPulseShotgun_Step_0", EndClass::Own),
    (
        "gml_Object_oPulse_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oRailgun_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oRailgun_Step_0", EndClass::PlayerSweep),
    (
        "gml_Object_oSalvo_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oSaucerShotMega_Destroy_0", EndClass::Own),
    ("gml_Object_oSaucerShot_Destroy_0", EndClass::Own),
    ("gml_Object_oSaucerShot_Step_0", EndClass::Own),
    ("gml_Object_oScionBolt_Alarm_4", EndClass::Own),
    ("gml_Object_oScionLanceShot_Step_0", EndClass::Own),
    ("gml_Object_oSeraphBall_Alarm_4", EndClass::Own),
    ("gml_Object_oSeraphShot_Step_0", EndClass::Own),
    (
        "gml_Object_oShieldBastion_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oSpikeMini_Step_0", EndClass::Own),
    ("gml_Object_oSpikeTriple_Alarm_0", EndClass::Own),
    ("gml_Object_oSpike_Step_0", EndClass::Own),
    ("gml_Object_oSpit_Step_0", EndClass::Own),
    ("gml_Object_oSpitfireProjectile_Alarm_2", EndClass::Own),
    ("gml_Object_oSpitfireProjectile_Step_0", EndClass::Own),
    (
        "gml_Object_oSplitShot_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oStationDroneShot_Alarm_0", EndClass::Own),
    ("gml_Object_oStationDroneShot_Alarm_4", EndClass::Own),
    ("gml_Object_oStationDroneShot_Step_0", EndClass::Own),
    (
        "gml_Object_oSwordObject_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oSwordSpark_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oThermalLance_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oTorrent_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    (
        "gml_Object_oTracerBall_Collision_oTracerShot",
        EndClass::World,
    ),
    ("gml_Object_oTracerShot_Step_0", EndClass::Own),
    (
        "gml_Object_oTurretBullet_Collision_oEnemyProjectile",
        EndClass::Player,
    ),
    ("gml_Object_oVanguardShotLarge_Step_0", EndClass::Own),
    ("gml_Object_oVanguardShot_Step_0", EndClass::Own),
    ("gml_Script_destroyOutside", EndClass::World),
];

/// `destroyOutside`: ends its `self` when it is more than its argument
/// (default [`OUTSIDE_MARGIN`]) outside the view.
pub const DESTROY_OUTSIDE: &str = "gml_Script_destroyOutside";
/// `destroyOutside`'s default margin (px).
pub const OUTSIDE_MARGIN: f64 = 128.0;
/// The turn rate every `projectileHoming*` script turns a shot by: above 0,
/// the shot homes, else it flies straight.
pub const TURN_RATE: &str = "rotSpeed";
/// A joining view rect older than this is stale: the host's own rule.
pub const VIEW_FRESH: Duration = Duration::from_millis(500);
/// A joining game sends its view rect every this many frames.
pub const VIEW_EVERY: u64 = 4;
/// Host: shots read per frame at most (a full read each); the rest the next
/// frame, read then and stamped then.
pub const READS_PER_FRAME: usize = 64;
/// Client: a shot whose state names an enemy whose copy is still to be
/// made waits for it at most this many frames, then is made with `noone`.
pub const WAIT_FRAMES: u64 = 30;

/// The class of a function by name.
#[must_use]
pub fn end_class(name: &str) -> Option<EndClass> {
    END_SITES
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|i| END_SITES[i].1)
}

/// What a shot found at a site is to this game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A shared shot (the host's, or a joining game's made one).
    Shared,
    /// A joining game: a copy's own create, removed at the frame's end.
    Scratch,
}

/// What a site does with a shot of `kind` on a joining game (the host runs
/// every site).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Run the game's code; an end it makes is attributed to the class.
    Run,
    /// Do not run it for this shot (the world's end on a shared shot: the
    /// host decides; anything on a scratch shot: it is not there).
    Veto,
}

/// A joining game's action at a site of `class` (`ByRule` already decided).
/// `linked`: the host is there to decide (else the game's own ends run).
/// `own_party`: the shot's other party is an enemy attack (a Dreadnought's
/// ball taking an orb feeder), whose own state goes on only through the
/// site, so it runs here too.
#[must_use]
pub fn client_action(class: EndClass, kind: Kind, linked: bool, own_party: bool) -> Action {
    match (kind, class) {
        (Kind::Scratch, _) => Action::Veto,
        (Kind::Shared, EndClass::World) if linked && !own_party => Action::Veto,
        _ => Action::Run,
    }
}

/// The wire cause of an end made at a site of `class`.
#[must_use]
pub fn cause_of(class: EndClass) -> u8 {
    match class {
        EndClass::Player | EndClass::PlayerSweep => cause::PLAYER,
        EndClass::World | EndClass::ByRule => cause::WORLD,
        EndClass::Own => cause::OWN,
    }
}

/// A view rectangle (`viewX`, `viewY`, `viewWidth`, `viewHeight`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

impl Rect {
    /// Whether (`x`, `y`) is inside, the edges moved out by `margin`.
    #[must_use]
    pub fn contains(&self, x: f64, y: f64, margin: f64) -> bool {
        x >= self.x - margin
            && x <= self.x + self.w + margin
            && y >= self.y - margin
            && y <= self.y + self.h + margin
    }
}

/// Host, before its `destroyOutside` on a shared shot: keep the shot (skip
/// the end) while it is inside the joining player's view rect with the
/// function's own margin plus one shot step. `rect`: the last rect received
/// and its age; a stale one (or none) leaves the host's own rule.
#[must_use]
pub fn keep_for_view(
    rect: Option<(Rect, Duration)>,
    x: f64,
    y: f64,
    margin: f64,
    step: f64,
) -> bool {
    let Some((r, age)) = rect else { return false };
    let margin = if margin.is_finite() {
        margin
    } else {
        OUTSIDE_MARGIN
    };
    let step = if step.is_finite() { step.abs() } else { 0.0 };
    age <= VIEW_FRESH && r.contains(x, y, margin + step)
}

/// A hash of the view rect's names: a `WorldInputs` with it carries the
/// joining view, not the view inputs (each side ignores the other's list).
#[must_use]
pub fn view_names_hash() -> u32 {
    names_hash(&crate::names::GLOBAL_VIEW)
}

/// FNV-1a over the names, each ended by a 0 byte (the view inputs' hash).
fn names_hash(names: &[&str]) -> u32 {
    names.iter().fold(0x811C_9DC5u32, |h, n| {
        n.bytes()
            .chain(std::iter::once(0))
            .fold(h, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
    })
}

/// Every collision event with the `oEnemyBeam` family on either side (the
/// beam is `other` in the first ten, `self` in the rest): on a joining game a
/// scratch beam touches nothing.
pub const BEAM_COLLISIONS: [&str; 18] = [
    "gml_Object_oAsteroid_Collision_oEnemyBeam",
    "gml_Object_oBlade_Collision_oEnemyBeam",
    "gml_Object_oConstructMine_Collision_oEnemyBeam",
    "gml_Object_oConstructSwarm_Collision_oEnemyBeam",
    "gml_Object_oCosmicEgg_Collision_oEnemyBeam",
    "gml_Object_oPlayer_Collision_oEnemyBeam",
    "gml_Object_oPlayerConstruct_Collision_oEnemyBeam",
    "gml_Object_oPlayerSegment_Collision_oEnemyBeam",
    "gml_Object_oShieldBastion_Collision_oEnemyBeam",
    "gml_Object_oSwordObject_Collision_oEnemyBeam",
    "gml_Object_oEnemyBeamFront_Collision_oPlayer",
    "gml_Object_oEnemyBeamFront_Collision_oPlayerConstruct",
    "gml_Object_oEnemyBeamLenDir_Collision_oPlayer",
    "gml_Object_oEnemyBeamLenDir_Collision_oPlayerConstruct",
    "gml_Object_oEnemyBeamWarbringer_Collision_oPlayer",
    "gml_Object_oEnemyBeamWarbringer_Collision_oPlayerConstruct",
    "gml_Object_oScionLance_Collision_oPlayer",
    "gml_Object_oScionLance_Collision_oPlayerConstruct",
];

/// The shared kinds, by root (`classify::parent::ENEMY_SHOTS` order).
pub const KINDS: [&str; 3] = ["shots", "blasts", "beams"];
/// A place reference ([`FiredShot::places`]): variable index, object, x, y.
pub type Place = (u16, u16, f32, f32);
/// A place's attack variable ([`FiredShot::place_fills`]): the place's
/// position, the variable's index into the attack object's names, the value.
pub type PlaceFill = (u8, u16, ndml_net::msg::StateValue);

/// A place reference takes the receiver's instance this close (px).
pub const PLACE_NEAR_PX: f32 = 300.0;

/// A family shot the host made, to read at the End Step.
#[derive(Debug, Clone)]
pub struct Born {
    /// Instance key.
    pub key: usize,
    /// Instance id.
    pub value: Value,
    /// Object index.
    pub object: u32,
    /// The frame it was made in.
    pub frame: u64,
    /// Its maker's player ([`FiredShot::target`]): an enemy's assignment,
    /// or a shared shot's own target (a split shot's children). `None`: the
    /// maker has none (the nearest targetable player at the read).
    pub target: Option<PeerId>,
    /// End Step reads to let pass before it is read: 1 for a beam, chain
    /// link or charge-up the host owns, whose first alarm sets its shape.
    pub wait: u8,
    /// For a host beam: the host's clock at its birth, sent as its time so
    /// it shows in step with its maker; 0 otherwise (the read's time).
    pub born_ms: u32,
}

/// One live shared shot on the host.
#[derive(Debug, Clone)]
pub struct HostShot {
    /// Shot id on the wire.
    pub id: u32,
    /// Instance id.
    pub value: Value,
    /// Object index.
    pub object: u32,
    /// The player it goes after ([`FiredShot::target`]).
    pub target: Option<PeerId>,
    /// What it steers at here ([`steer`]); a change is sent.
    pub steer: u8,
}

/// The host's tables.
#[derive(Debug, Default)]
pub struct HostShots {
    next: u32,
    /// Family shots made since the last read.
    pub born: Vec<Born>,
    /// Live shared shots by key.
    pub live: HashMap<usize, HostShot>,
    /// The joining view (rect, when it arrived).
    pub view: Option<(Rect, Instant)>,
    /// To send at the next run step.
    pub fires: Vec<FiredShot>,
    /// Ends to send at the next run step.
    pub ends: Vec<ShotEnd>,
    /// Steer changes to send at the next run step.
    pub steers: Vec<SteerChange>,
}

impl HostShots {
    /// What the live shot at `key` steers at now: queue a change.
    pub fn steer_now(&mut self, key: usize, now_ms: u32, s: u8) {
        if let Some(shot) = self.live.get_mut(&key)
            && shot.steer != s
        {
            shot.steer = s;
            self.steers.push(SteerChange {
                id: shot.id,
                time_ms: now_ms,
                steer: s,
            });
        }
    }

    /// The next shot id (never 0).
    pub fn next_id(&mut self) -> u32 {
        self.next = self.next.wrapping_add(1).max(1);
        self.next
    }

    /// The shared shot at `key`.
    #[must_use]
    pub fn shared(&self, key: usize) -> bool {
        self.live.contains_key(&key) || self.born.iter().any(|b| b.key == key)
    }

    /// The player the live shared shot at `key` goes after.
    #[must_use]
    pub fn target_of(&self, key: usize) -> Option<Option<PeerId>> {
        self.live.get(&key).map(|s| s.target)
    }

    /// The joining view and its age at `now`.
    #[must_use]
    pub fn view_at(&self, now: Instant) -> Option<(Rect, Duration)> {
        self.view
            .map(|(r, at)| (r, now.saturating_duration_since(at)))
    }

    /// A shot ended: queue its end.
    pub fn end(&mut self, key: usize, time_ms: u32, cause: u8) -> Option<u32> {
        let s = self.live.remove(&key)?;
        self.ends.push(ShotEnd {
            id: s.id,
            time_ms,
            cause,
        });
        Some(s.id)
    }

    /// A joining game's end of `id`: the instance to remove here, if the
    /// shot is still live (else it crossed with this game's own end).
    pub fn report(&mut self, end: ShotEnd) -> Option<Value> {
        let key = self
            .live
            .iter()
            .find(|(_, s)| s.id == end.id)
            .map(|(k, _)| *k);
        let key = key?;
        let s = self.live.remove(&key)?;
        // Forwarded to the other joining games (the reporter finds it gone).
        self.ends.push(ShotEnd {
            id: end.id,
            time_ms: end.time_ms,
            cause: cause::PEER,
        });
        Some(s.value)
    }

    /// The partner back after a lost link (its resync removes every shot
    /// copy): each live shared shot is born again, sent afresh under a new id
    /// at the next running read, and nothing queued for the dead link goes
    /// out. Returns how many.
    pub fn resync(&mut self, frame: u64) -> usize {
        self.fires.clear();
        self.ends.clear();
        self.steers.clear();
        let mut again: Vec<Born> = self
            .live
            .drain()
            .map(|(key, s)| Born {
                key,
                value: s.value,
                object: s.object,
                frame,
                target: s.target,
                wait: 0,
                born_ms: 0,
            })
            .collect();
        again.sort_by_key(|b| b.key);
        let n = again.len();
        again.append(&mut self.born);
        self.born = again;
        n
    }

    /// Forget everything (a run starts or ends).
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// One shot a joining game made.
#[derive(Debug, Clone)]
pub struct MadeShot {
    /// Shot id on the wire.
    pub id: u32,
    /// Instance key.
    pub key: usize,
    /// Instance id.
    pub value: Value,
    /// Object index.
    pub object: u32,
    /// The player it goes after ([`FiredShot::target`]).
    pub target: Option<PeerId>,
    /// What it steers at on the host ([`steer`]), at the shown time.
    pub steer: u8,
    /// Gone here (shown time, ms) without a report: kept until the host's end.
    pub gone_at: Option<u32>,
}

/// A shot waiting for its shown time (or for a copy it names).
#[derive(Debug, Clone)]
pub struct Due {
    /// The host's shot.
    pub shot: FiredShot,
    /// Frames it has waited for a copy.
    pub waited: u64,
}

/// A joining game's tables.
#[derive(Debug, Default)]
pub struct ClientShots {
    /// Births by (time, id).
    pub due: BTreeMap<(u32, u32), Due>,
    /// Host ends by (time, id), applied at their shown time.
    pub host_ends: BTreeMap<(u32, u32), u8>,
    /// The host's steer changes by (time, id), applied at their shown time.
    pub steer_due: BTreeMap<(u32, u32), u8>,
    /// Shots made here, by id; and id by key.
    pub made: HashMap<u32, MadeShot>,
    /// Shot id by instance key.
    pub by_key: HashMap<usize, u32>,
    /// Ids ended here by this game's own player (reported).
    pub reported: HashSet<u32>,
    /// Scratch shots (key, id) made this frame.
    pub scratch: Vec<(usize, Value)>,
    /// The keys of [`Self::scratch`].
    pub scratch_keys: HashSet<usize>,
    /// The view rect last sent (frame).
    pub view_sent: u64,
    /// Reports to send at the next run step.
    pub reports: Vec<ShotEnd>,
}

impl ClientShots {
    /// Messages in: births wait for their shown time, ends for theirs. An
    /// end for a shot not made yet means it is never made.
    pub fn fire(&mut self, shot: FiredShot) {
        self.due
            .insert((shot.time_ms, shot.id), Due { shot, waited: 0 });
    }

    /// A host end (see [`Self::fire`]).
    pub fn host_end(&mut self, end: ShotEnd) {
        let due = self.due.keys().find(|(_, id)| *id == end.id).copied();
        if let Some(k) = due {
            self.due.remove(&k);
            return;
        }
        if self.reported.remove(&end.id) {
            // Own report, forwarded back, or the host's end crossing it.
            return;
        }
        self.host_ends.insert((end.time_ms, end.id), end.cause);
    }

    /// A host steer change: a shot not made yet is born with it (its birth's
    /// time is earlier), a made one takes it at its shown time.
    pub fn steer_change(&mut self, c: SteerChange) {
        if let Some(d) = self.due.values_mut().find(|d| d.shot.id == c.id) {
            d.shot.steer = c.steer;
            return;
        }
        self.steer_due.insert((c.time_ms, c.id), c.steer);
    }

    /// The steer changes due at `shown`, applied to the made shots.
    pub fn apply_steers(&mut self, shown: u32) {
        let later = self.steer_due.split_off(&(shown.saturating_add(1), 0));
        let due = std::mem::replace(&mut self.steer_due, later);
        for ((_, id), s) in due {
            if let Some(m) = self.made.get_mut(&id) {
                m.steer = s;
            }
        }
    }

    /// The births due at `shown` (oldest first), taken out.
    pub fn take_due(&mut self, shown: u32) -> Vec<Due> {
        let later = self.due.split_off(&(shown.saturating_add(1), 0));
        let due = std::mem::replace(&mut self.due, later);
        due.into_values().collect()
    }

    /// Put back a birth that waits for a copy.
    pub fn wait(&mut self, mut d: Due) {
        d.waited += 1;
        self.due.insert((d.shot.time_ms, d.shot.id), d);
    }

    /// The host ends due at `shown`, taken out: (id, time, cause).
    pub fn take_ends(&mut self, shown: u32) -> Vec<(u32, u32, u8)> {
        let later = self.host_ends.split_off(&(shown.saturating_add(1), 0));
        let due = std::mem::replace(&mut self.host_ends, later);
        due.into_iter().map(|((t, id), c)| (id, t, c)).collect()
    }

    /// A shot was made here.
    pub fn made(&mut self, id: u32, key: usize, value: Value, object: u32) {
        self.made_for(id, key, value, object, None, steer::UNKNOWN);
    }

    /// A shot was made here, going after `target`.
    pub fn made_for(
        &mut self,
        id: u32,
        key: usize,
        value: Value,
        object: u32,
        target: Option<PeerId>,
        steer: u8,
    ) {
        self.by_key.insert(key, id);
        self.made.insert(
            id,
            MadeShot {
                id,
                key,
                value,
                object,
                target,
                steer,
                gone_at: None,
            },
        );
    }

    /// The made shot at `key`: its player and what it steers at on the host.
    #[must_use]
    pub fn steer_of(&self, key: usize) -> Option<(Option<PeerId>, u8)> {
        self.shared(key).map(|m| (m.target, m.steer))
    }

    /// The player the made shot at `key` goes after.
    #[must_use]
    pub fn target_of(&self, key: usize) -> Option<Option<PeerId>> {
        self.shared(key).map(|m| m.target)
    }

    /// The made shot at `key`.
    #[must_use]
    pub fn shared(&self, key: usize) -> Option<&MadeShot> {
        self.by_key
            .get(&key)
            .and_then(|id| self.made.get(id))
            .filter(|m| m.gone_at.is_none())
    }

    /// A made shot is gone here (`shown`, ms), ended by `cause` (`None`: no
    /// known site). The player's ends are reported (`host_ms`: the host's
    /// clock now, estimated); the rest wait for the host's end.
    pub fn gone_here(&mut self, id: u32, shown: u32, host_ms: u32, cause: Option<u8>) {
        let Some(m) = self.made.get_mut(&id) else {
            return;
        };
        self.by_key.remove(&m.key);
        match cause {
            Some(c @ (cause::PLAYER | cause::CHANGED)) => {
                self.made.remove(&id);
                self.reported.insert(id);
                self.reports.push(ShotEnd {
                    id,
                    time_ms: host_ms,
                    cause: c,
                });
            }
            _ => m.gone_at = Some(shown),
        }
    }

    /// The host's end of a made shot: the instance to remove (if still here).
    pub fn apply_end(&mut self, id: u32) -> Option<Value> {
        let m = self.made.remove(&id)?;
        self.by_key.remove(&m.key);
        if m.gone_at.is_some() {
            return None;
        }
        Some(m.value)
    }

    /// Forget everything (a run starts or ends, the host left).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// A resync (the host back after a lost link): every copy made here
    /// goes (the instances to remove are returned) and nothing queued from
    /// before is made, ended or steered; the host sends its live shots again
    /// ([`HostShots::resync`]).
    pub fn resync(&mut self) -> Vec<Value> {
        let out = self
            .made
            .values()
            .filter(|m| m.gone_at.is_none())
            .map(|m| m.value.clone())
            .collect();
        let view_sent = self.view_sent;
        *self = Self {
            view_sent,
            ..Self::default()
        };
        out
    }

    /// Shots made here and live.
    #[must_use]
    pub fn live(&self) -> usize {
        self.by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_unique() {
        for w in END_SITES.windows(2) {
            assert!(w[0].0 < w[1].0, "{} before {}", w[0].0, w[1].0);
        }
        for (n, _) in END_SITES {
            assert!(
                n.starts_with("gml_Object_") || n.starts_with("gml_Script_"),
                "{n}"
            );
            assert!(end_class(n).is_some());
        }
        assert_eq!(end_class("gml_Object_oPlayer_Step_0"), None);
    }

    /// The family's own events that end a shot through a script
    /// (`sizeDestroy`, `degenerateProj`, `sizeDesroyThreshhold`; not
    /// `destroyOutside`), in 1.2.38.4.
    const OWN_VIA_SCRIPT: [&str; 25] = [
        "oArcTail_Step_0",
        "oBeamcasterShot_Step_0",
        "oBoltThrowerShot_Step_0",
        "oCelestialShot_Step_0",
        "oDreadnoughtBallShot_Step_0",
        "oDreadnoughtOrbFeeder_Step_0",
        "oDreadnoughtVulcan_Step_0",
        "oDwellerGuardShot_Step_0",
        "oEclipseShot_Step_0",
        "oEclipseTorrent_Step_0",
        "oGyrogunShot_Step_0",
        "oJuggernautShot_Step_0",
        "oPulsarBarrage_Step_0",
        "oPulseShotgunHoming_Step_0",
        "oPulseShotgun_Step_0",
        "oSaucerShot_Step_0",
        "oScionLanceShot_Step_0",
        "oSeraphShot_Step_0",
        "oSpikeMini_Step_0",
        "oSpike_Step_0",
        "oSpit_Step_0",
        "oStationDroneShot_Step_0",
        "oTracerShot_Step_0",
        "oVanguardShotLarge_Step_0",
        "oVanguardShot_Step_0",
    ];

    #[test]
    fn the_hosts_steer_word_reaches_a_shot_at_its_shown_time() {
        let mut h = HostShots::default();
        h.live.insert(
            9,
            HostShot {
                id: 4,
                value: Value::Real(9.0),
                object: 7,
                target: Some(PeerId(0)),
                steer: steer::UNKNOWN,
            },
        );
        h.steer_now(9, 100, steer::PLAYER);
        h.steer_now(9, 110, steer::PLAYER);
        assert_eq!(h.steers.len(), 1, "a change only");
        let mut c = ClientShots::default();
        // Not made yet: born with it.
        c.fire(fired(5, 1000));
        c.steer_change(SteerChange {
            id: 5,
            time_ms: 1010,
            steer: steer::WORLD,
        });
        let d = c.take_due(1000);
        assert_eq!(d[0].shot.steer, steer::WORLD);
        // Made: at its shown time.
        c.made_for(4, 40, Value::Real(1.0), 7, Some(PeerId(0)), steer::UNKNOWN);
        c.steer_change(h.steers[0]);
        c.apply_steers(99);
        assert_eq!(c.steer_of(40), Some((Some(PeerId(0)), steer::UNKNOWN)));
        c.apply_steers(100);
        assert_eq!(c.steer_of(40), Some((Some(PeerId(0)), steer::PLAYER)));
    }

    #[test]
    fn a_shared_shot_keeps_its_player_on_both_sides() {
        let mut c = ClientShots::default();
        c.made_for(3, 30, Value::Real(1.0), 7, Some(PeerId(1)), steer::PLAYER);
        c.made(4, 40, Value::Real(2.0), 7);
        assert_eq!(c.target_of(30), Some(Some(PeerId(1))));
        assert_eq!(c.target_of(40), Some(None), "shared, no player");
        assert_eq!(c.target_of(50), None, "not shared");
        c.gone_here(3, 100, 100, Some(cause::PLAYER));
        assert_eq!(c.target_of(30), None, "gone");
        let mut h = HostShots::default();
        h.live.insert(
            9,
            HostShot {
                id: 1,
                value: Value::Real(9.0),
                object: 7,
                target: Some(PeerId(0)),
                steer: 0,
            },
        );
        assert_eq!(h.target_of(9), Some(Some(PeerId(0))));
        h.end(9, 10, cause::WORLD);
        assert_eq!(h.target_of(9), None);
    }

    /// Every collision event with a family member on either side, and the
    /// family's own events that destroy (1.2.38.4).
    #[test]
    fn the_table_covers_every_family_event() {
        const COLLISIONS: [&str; 28] = [
            "oAsteroid_Collision_oEnemyProjectile",
            "oBlackHole_Collision_oEnemyProjectile",
            "oBlade_Collision_oEnemyProjectile",
            "oBladeChild_Collision_oEnemyProjectile",
            "oBlaster_Collision_oEnemyProjectile",
            "oConstructMine_Collision_oEnemyProjectile",
            "oConstructSwarm_Collision_oEnemyProjectile",
            "oCosmicEgg_Collision_oEnemyProjectile",
            "oDart_Collision_oEnemyProjectile",
            "oDreadnoughtBall_Collision_oDreadnoughtOrbFeeder",
            "oEnemyProjectile_Collision_oAsteroid",
            "oFlak_Collision_oEnemyProjectile",
            "oGrenade_Collision_oEnemyProjectile",
            "oHelixOrb_Collision_oEnemyProjectile",
            "oPlayer_Collision_oEnemyProjectile",
            "oPlayerConstruct_Collision_oEnemyProjectile",
            "oPlayerSegment_Collision_oEnemyProjectile",
            "oPulse_Collision_oEnemyProjectile",
            "oRailgun_Collision_oEnemyProjectile",
            "oSalvo_Collision_oEnemyProjectile",
            "oShieldBastion_Collision_oEnemyProjectile",
            "oSplitShot_Collision_oEnemyProjectile",
            "oSwordObject_Collision_oEnemyProjectile",
            "oSwordSpark_Collision_oEnemyProjectile",
            "oThermalLance_Collision_oEnemyProjectile",
            "oTorrent_Collision_oEnemyProjectile",
            "oTracerBall_Collision_oTracerShot",
            "oTurretBullet_Collision_oEnemyProjectile",
        ];
        const OWN: [&str; 21] = [
            "oCelestialStrike_Alarm_2",
            "oCeratotitanProjectile_Alarm_2",
            "oDreadnoughtVulcan_Destroy_0",
            "oEclipseBolt_Alarm_4",
            "oEclipseOrb_Alarm_0",
            "oEclipseOrb_Alarm_4",
            "oEnemyProjectile_Other_23",
            "oGlaucusProjectile_Alarm_2",
            "oJuggernautShot_Destroy_0",
            "oMineDroneShot_Destroy_0",
            "oMyrmidonBomb_Alarm_2",
            "oPlasmaShot_Alarm_4",
            "oSaucerShotMega_Destroy_0",
            "oSaucerShot_Destroy_0",
            "oScionBolt_Alarm_4",
            "oSeraphBall_Alarm_4",
            "oSpikeTriple_Alarm_0",
            "oSpitfireProjectile_Alarm_2",
            "oSpitfireProjectile_Step_0",
            "oStationDroneShot_Alarm_0",
            "oStationDroneShot_Alarm_4",
        ];
        for c in COLLISIONS {
            let name = format!("gml_Object_{c}");
            let class = end_class(&name);
            assert!(class.is_some(), "{name} is not classified");
            // A player-side object colliding with the family is the
            // player's; the world's are asteroids and enemy-side objects.
            let player_obj = !(c.starts_with("oAsteroid")
                || c.starts_with("oEnemyProjectile")
                || c.starts_with("oBlackHole")
                || c.starts_with("oDreadnoughtBall")
                || c.starts_with("oTracerBall"));
            assert_eq!(class == Some(EndClass::Player), player_obj, "{name}");
        }
        for o in OWN.iter().chain(OWN_VIA_SCRIPT.iter()) {
            assert_eq!(
                end_class(&format!("gml_Object_{o}")),
                Some(EndClass::Own),
                "{o}"
            );
        }
        assert_eq!(end_class(DESTROY_OUTSIDE), Some(EndClass::World));
        assert_eq!(
            end_class("gml_Object_oPlayer_Collision_oEnemyProjectile"),
            Some(EndClass::Player)
        );
        assert_eq!(
            end_class("gml_Object_oRailgun_Step_0"),
            Some(EndClass::PlayerSweep)
        );
        assert_eq!(cause_of(EndClass::PlayerSweep), cause::PLAYER);
        // + 2: destroyOutside, rail clearing.
        let n = COLLISIONS.len() + OWN.len() + OWN_VIA_SCRIPT.len() + 2;
        assert_eq!(END_SITES.len(), n, "nothing outside these lists");
    }

    #[test]
    fn a_joining_game_vetoes_world_ends_of_shared_shots_and_every_site_of_scratch() {
        use Action::{Run, Veto};
        for class in [EndClass::Player, EndClass::World, EndClass::Own] {
            for own_party in [false, true] {
                assert_eq!(client_action(class, Kind::Scratch, true, own_party), Veto);
            }
        }
        assert_eq!(
            client_action(EndClass::World, Kind::Shared, true, false),
            Veto
        );
        assert_eq!(
            client_action(EndClass::World, Kind::Shared, false, false),
            Run,
            "no host to decide: the game's own"
        );
        assert_eq!(
            client_action(EndClass::World, Kind::Shared, true, true),
            Run,
            "an attack each game makes itself takes its side of the site here too"
        );
        assert_eq!(
            client_action(EndClass::Player, Kind::Shared, true, false),
            Run
        );
        assert_eq!(client_action(EndClass::Own, Kind::Shared, true, false), Run);
        assert_eq!(cause_of(EndClass::Player), cause::PLAYER);
        assert_eq!(cause_of(EndClass::Own), cause::OWN);
        assert_eq!(cause_of(EndClass::World), cause::WORLD);
    }

    #[test]
    fn the_host_keeps_a_shot_inside_the_joining_view() {
        let r = Rect {
            x: 1000.0,
            y: 500.0,
            w: 1280.0,
            h: 720.0,
        };
        let fresh = Some((r, Duration::from_millis(100)));
        assert!(keep_for_view(fresh, 1500.0, 800.0, 128.0, 6.0));
        // Just outside the rect but inside the margin plus a step.
        assert!(keep_for_view(fresh, 1000.0 - 130.0, 800.0, 128.0, 6.0));
        assert!(!keep_for_view(fresh, 1000.0 - 140.0, 800.0, 128.0, 6.0));
        assert!(!keep_for_view(fresh, 3000.0, 800.0, 128.0, 6.0));
        let stale = Some((r, Duration::from_secs(2)));
        assert!(
            !keep_for_view(stale, 1500.0, 800.0, 128.0, 6.0),
            "stale: the host's rule"
        );
        assert!(!keep_for_view(None, 1500.0, 800.0, 128.0, 6.0));
        assert!(keep_for_view(fresh, 1500.0, 800.0, f64::NAN, f64::NAN));
    }

    #[test]
    fn view_names_differ_from_the_view_inputs() {
        // The view inputs' list (`view_glue::INPUTS`).
        assert_ne!(view_names_hash(), names_hash(&["zoomAdditive"]));
    }

    fn fired(id: u32, t: u32) -> FiredShot {
        FiredShot {
            id,
            object: 7,
            time_ms: t,
            vars: Vec::new(),
            builtins: Vec::new(),
            alarms: Vec::new(),
            target: None,
            steer: 0,
            places: Vec::new(),
            place_fills: Vec::new(),
            replaces: false,
        }
    }

    #[test]
    fn births_wait_for_their_shown_time_and_an_early_end_cancels_one() {
        let mut c = ClientShots::default();
        c.fire(fired(2, 1100));
        c.fire(fired(1, 1000));
        c.fire(fired(3, 1300));
        c.host_end(ShotEnd {
            id: 3,
            time_ms: 1310,
            cause: cause::WORLD,
        });
        assert!(c.take_due(990).is_empty());
        let d: Vec<u32> = c.take_due(1100).iter().map(|d| d.shot.id).collect();
        assert_eq!(d, vec![1, 2], "oldest first");
        assert!(c.take_due(5000).is_empty(), "3 is never made");
    }

    #[test]
    fn host_ends_apply_at_their_shown_time() {
        let mut c = ClientShots::default();
        c.made(1, 11, Value::Real(100_001.0), 7);
        c.host_end(ShotEnd {
            id: 1,
            time_ms: 2000,
            cause: cause::PLAYER,
        });
        assert!(c.take_ends(1999).is_empty());
        let e = c.take_ends(2016);
        assert_eq!(e, vec![(1, 2000, cause::PLAYER)]);
        let v = c.apply_end(1);
        assert!(v.is_some());
        assert_eq!(c.live(), 0);
    }

    #[test]
    fn a_players_end_is_reported_once_and_the_echo_is_dropped() {
        let mut c = ClientShots::default();
        c.made(5, 50, Value::Real(100_005.0), 7);
        assert!(c.shared(50).is_some());
        c.gone_here(5, 3000, 3100, Some(cause::PLAYER));
        assert_eq!(c.reports.len(), 1);
        assert_eq!(c.reports[0].time_ms, 3100);
        assert!(c.shared(50).is_none());
        c.host_end(ShotEnd {
            id: 5,
            time_ms: 3150,
            cause: cause::PEER,
        });
        assert!(c.take_ends(u32::MAX).is_empty(), "the echo is dropped");
    }

    #[test]
    fn a_shot_gone_here_waits_for_the_hosts_end() {
        let mut c = ClientShots::default();
        c.made(6, 60, Value::Real(1.0), 7);
        c.made(7, 70, Value::Real(2.0), 7);
        // 6: ended by its own timer.
        c.gone_here(6, 4000, 4100, Some(cause::OWN));
        // 7: no known site.
        c.gone_here(7, 4000, 4100, None);
        assert!(c.reports.is_empty(), "neither is reported");
        assert!(c.apply_end(6).is_none());
        assert!(c.apply_end(7).is_none());
    }

    #[test]
    fn the_host_queues_ends_and_takes_reports_once() {
        let mut h = HostShots::default();
        let a = h.next_id();
        let b = h.next_id();
        assert!(a != 0 && b != a);
        h.live.insert(
            1,
            HostShot {
                id: a,
                value: Value::Real(1.0),
                object: 7,
                target: None,
                steer: 0,
            },
        );
        h.live.insert(
            2,
            HostShot {
                id: b,
                value: Value::Real(2.0),
                object: 7,
                target: None,
                steer: 0,
            },
        );
        assert!(h.shared(1));
        assert_eq!(h.end(1, 500, cause::WORLD), Some(a));
        assert_eq!(h.end(1, 500, cause::WORLD), None, "once");
        let r = h.report(ShotEnd {
            id: b,
            time_ms: 480,
            cause: cause::PLAYER,
        });
        assert!(r.is_some());
        let r = h.report(ShotEnd {
            id: a,
            time_ms: 490,
            cause: cause::PLAYER,
        });
        assert!(r.is_none());
        assert_eq!(h.ends.len(), 2, "the host's end and the forwarded report");
        assert!(h.live.is_empty());
    }

    #[test]
    fn a_resync_sends_every_live_shot_again_and_drops_what_the_dead_link_held() {
        let mut h = HostShots::default();
        let a = h.next_id();
        h.live.insert(
            5,
            HostShot {
                id: a,
                value: Value::Real(5.0),
                object: 7,
                target: Some(PeerId(1)),
                steer: 0,
            },
        );
        h.fires.push(fired(a, 100));
        h.ends.push(ShotEnd {
            id: 99,
            time_ms: 90,
            cause: cause::WORLD,
        });
        assert_eq!(h.resync(40), 1);
        assert!(h.live.is_empty() && h.fires.is_empty() && h.ends.is_empty());
        let b = &h.born[0];
        assert_eq!(
            (b.key, b.frame, b.target, b.wait),
            (5, 40, Some(PeerId(1)), 0)
        );
        assert!(h.shared(5), "still the host's: read again at the next read");

        let mut c = ClientShots::default();
        c.made(1, 11, Value::Real(1.0), 7);
        c.made(2, 12, Value::Real(2.0), 7);
        c.gone_here(2, 50, 50, Some(cause::OWN));
        c.fire(fired(3, 2000));
        c.host_end(ShotEnd {
            id: 1,
            time_ms: 900,
            cause: cause::WORLD,
        });
        let stale = c.resync();
        assert_eq!(
            stale,
            vec![Value::Real(1.0)],
            "the live copy, not one gone here"
        );
        assert_eq!(c.live(), 0);
        assert!(c.take_due(u32::MAX).is_empty() && c.take_ends(u32::MAX).is_empty());
    }
}
