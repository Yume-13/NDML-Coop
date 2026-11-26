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

//! The stand-in bracket: on the host, the joining player is "the player" for
//! what is after the joining player. The pure plan; `standin_glue.rs` is the
//! game side.
//!
//! Enemy code that does not go through `target` finds the host's ship: it
//! reads `oPlayer.x/y`, calls a builtin with `oPlayer` (`distance_to_object`,
//! `place_meeting`), runs `with (oPlayer)`, or tests
//! `target.object_index == oPlayer`, which the avatar (an `oPlayerAttack`)
//! fails.
//!
//! * An instance after the joining player runs its [`BRACKETED`] events with
//!   the host's ship at the avatar's place, `visible` 1 and `oPowers.cloaked`
//!   0. After the event everything is put back as it was.
//! * An aim ([`Kind::Aim`]) follows the assignment. A touch ([`Kind::Touch`])
//!   keeps the nearest ship, as the game's builtins would with two ships.
//! * Inside a bracket, another instance's event, a Destroy and a death script
//!   ([`DEATH_SCRIPTS`]) run with the ship at home. Collision events and Draw
//!   are never bracketed.
//! * [`NOT_BRACKETED`] objects write to a player's thing: they act on each
//!   screen's own ship.
//! * `oPowers` is one instance: only `cloaked` is swapped.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use ndml_net::msg::PeerId;
use ndml_sdk::Value;

/// What a lookup the bracket answers decides about "the player", and so
/// which ship answers it for an instance after the joining player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An aim, a heading, a range gate for an attack or the identity test:
    /// the instance's attack. Follows the assignment: the avatar's place
    /// whenever the instance is after the joining player.
    Aim,
    /// A contact or under-a-radius test against the player (or a lookup not
    /// placed by kind): whoever touches it sets it off. The nearest ship.
    Touch,
}

/// Objects whose own events write to a player's thing: never bracketed,
/// with what they do. An instance of a descendant is not bracketed either.
pub const NOT_BRACKETED: &[(&str, &str)] = &[
    (
        "oBlackHole",
        "the Singularity's catch and pull act on the ship it finds (singularity.rs)",
    ),
    (
        "oCrucibleBurningHoop",
        "pushes the ship, its segments and constructs it touches",
    ),
    (
        "oDreadnoughtBall",
        "pushes the ship, its segments and constructs it touches",
    ),
    ("oGlaucusMaelstrom", "pulls the ship it finds"),
    ("oMineEnemySingularity", "pulls the ship toward itself"),
    (
        "oStarEaterController",
        "the ending (beginGameEnding) acts on every player thing",
    ),
    ("oStarEaterVortex", "pulls the ship it finds"),
    ("oStationCore", "pushes the ship away from the core"),
    (
        "oWaves",
        "the ending (beginGameEnding) acts on every player thing",
    ),
];

/// What a bracketed event needs of the stand-in, from the kind of its
/// lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// The ship's place (by-name reads, `with`, aim): `target` keeps the
    /// avatar.
    Pose,
    /// The identity test (`target.object_index == oPlayer`): the place, and
    /// a `target` naming the avatar names the ship during the event.
    Identity,
    /// Only proximity tests on the player object (`distance_to_object`,
    /// `place_meeting`: a ball's burst): the place, for whoever the instance
    /// is after, so the nearer ship sets it off.
    Proximity,
}

/// The events that reach a lookup the bracket answers: (object, event, need,
/// kind). The kind is [`Kind::Touch`] when the event reaches a contact or
/// under-a-radius test, or a lookup not placed by kind; [`Kind::Aim`]
/// otherwise. A row covers the object's descendants, less [`NOT_BRACKETED`]
/// (so `oStarEaterVortex`'s `Alarm_4` never is). A Step or alarm row is that
/// event alone ([`event_of`]). A user event (`Other_10`) holds the methods
/// the phases run from Step and alarms, so it covers every kind.
/// `oEnemySpawner` is never assigned, so it is never bracketed in play.
pub const BRACKETED: &[(&str, &str, Need, Kind)] = &[
    ("oBulwarkBarrier", "Step_0", Need::Identity, Kind::Aim),
    ("oCargoEngine", "Other_11", Need::Pose, Kind::Aim),
    ("oCeratotitan", "Other_10", Need::Pose, Kind::Aim),
    ("oConstructorTurret", "Step_0", Need::Pose, Kind::Aim),
    ("oCrucible", "Other_10", Need::Pose, Kind::Aim),
    ("oDreadnoughtMissile", "Step_0", Need::Identity, Kind::Aim),
    ("oDweller", "Other_10", Need::Pose, Kind::Aim),
    ("oDwellerChitinMissile", "Step_0", Need::Identity, Kind::Aim),
    ("oDwellerDrone", "Other_10", Need::Identity, Kind::Aim),
    ("oEnemySpawner", "Alarm_1", Need::Pose, Kind::Aim),
    ("oGyrogun", "Other_10", Need::Pose, Kind::Aim),
    ("oJuggernaut", "Alarm_3", Need::Identity, Kind::Aim),
    ("oMineDrone", "Alarm_0", Need::Pose, Kind::Aim),
    ("oSeraphTeslaMine", "Alarm_1", Need::Pose, Kind::Aim),
    ("oSeraphZoneDrone", "Alarm_1", Need::Pose, Kind::Aim),
    ("oSerpent", "Step_0", Need::Pose, Kind::Aim),
    ("oShredderMine", "Other_10", Need::Identity, Kind::Aim),
    ("oSpitfireBall", "Step_0", Need::Proximity, Kind::Touch),
    ("oStarEater", "Other_10", Need::Pose, Kind::Touch),
    ("oStarEaterSegment", "Step_0", Need::Pose, Kind::Touch),
    ("oStarEaterVortex", "Alarm_4", Need::Pose, Kind::Aim),
    ("oTracerBall", "Step_0", Need::Proximity, Kind::Touch),
];

/// The scripts a death runs: inside a bracket they run with the ship at
/// home. Their kill procs (`procEffectsOnDestroy`) read and write the host's
/// ship and constructs.
pub const DEATH_SCRIPTS: &[&str] = &[
    "gml_Script_bossDrops",
    "gml_Script_checkDestroy",
    "gml_Script_checkDestroyNoExplosion",
    "gml_Script_checkDestroyQuiet",
    "gml_Script_checkDestroyReduced",
    "gml_Script_createExplosion",
    "gml_Script_enemyDestroyed",
];

/// How a bracketed event uses the stand-in: its [`Need`]s and [`Kind`]s
/// merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    /// `target` names the ship during the event ([`Need::Identity`]).
    pub retarget: bool,
    /// Bracketed whoever the instance is after ([`Need::Proximity`], every
    /// need of the event).
    pub nearest: bool,
    /// The stand-in follows the assignment: the avatar's place whenever the
    /// instance is after the joining player ([`Kind::Aim`], every kind of the
    /// event). Otherwise only while the avatar is the nearer.
    pub assigned: bool,
}

impl Use {
    #[must_use]
    pub fn of(need: Need, kind: Kind) -> Self {
        Self {
            retarget: need == Need::Identity,
            nearest: need == Need::Proximity,
            assigned: kind == Kind::Aim && need != Need::Proximity,
        }
    }

    /// Two needs of one event. The identity test wins `target`. `nearest`
    /// holds only when every need is a proximity test, and `assigned` only
    /// when every kind is an aim: one swap serves the whole event.
    #[must_use]
    pub fn merge(self, o: Self) -> Self {
        Self {
            retarget: self.retarget || o.retarget,
            nearest: self.nearest && o.nearest,
            assigned: self.assigned && o.assigned,
        }
    }
}

/// Which of an object's events are bracketed, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Events {
    /// Every observed kind (Step, alarms, user events).
    All(Use),
    /// These (kind, number) only.
    Only(Vec<((i32, i32), Use)>),
}

impl Events {
    /// How event (kind, number) is bracketed, if it is.
    #[must_use]
    pub fn get(&self, kind: i32, number: i32) -> Option<Use> {
        match self {
            Self::All(u) => Some(*u),
            Self::Only(v) => v.iter().find(|(e, _)| *e == (kind, number)).map(|x| x.1),
        }
    }

    /// Add a [`BRACKETED`] event.
    pub fn add(&mut self, event: &str, need: Need, kind: Kind) {
        let u = Use::of(need, kind);
        match (event_of(event), &mut *self) {
            (Some(e), Self::Only(v)) => match v.iter_mut().find(|(x, _)| *x == e) {
                Some(x) => x.1 = x.1.merge(u),
                None => v.push((e, u)),
            },
            (None, Self::Only(v)) => {
                let all = v.iter().fold(u, |a, (_, b)| a.merge(*b));
                *self = Self::All(all);
            }
            (_, Self::All(a)) => *a = a.merge(u),
        }
    }
}

/// A Step or alarm event's (kind, number) (`Step_0` to `Step_2`: kind 3;
/// `Alarm_N`: kind 2); anything else `None` ([`Events::All`]).
#[must_use]
pub fn event_of(name: &str) -> Option<(i32, i32)> {
    let (kind, n) = name.split_once('_')?;
    let n: i32 = n.parse().ok()?;
    match kind {
        "Step" if (0..=2).contains(&n) => Some((3, n)),
        "Alarm" if (0..=11).contains(&n) => Some((2, n)),
        _ => None,
    }
}

/// The bracketable events by object: every object (`objects`) under a
/// [`BRACKETED`] root (`wanted`: (root, event, need, kind)), unless it is under a
/// writer (`writers`, [`NOT_BRACKETED`]); `under(i, r)`: object `i` is `r`
/// or descends from it.
pub fn bracketable(
    objects: impl IntoIterator<Item = u32>,
    under: impl Fn(u32, u32) -> bool,
    wanted: &[(u32, &str, Need, Kind)],
    writers: &[u32],
) -> HashMap<i32, Events> {
    let mut out: HashMap<i32, Events> = HashMap::new();
    for i in objects {
        let Ok(key) = i32::try_from(i) else { continue };
        if writers.iter().any(|&w| under(i, w)) {
            continue;
        }
        for &(r, e, need, kind) in wanted {
            if under(i, r) {
                out.entry(key)
                    .or_insert_with(|| Events::Only(Vec::new()))
                    .add(e, need, kind);
            }
        }
    }
    out
}

/// Where the ship stands in for a partner: the partner's avatar's place and
/// id.
#[derive(Debug, Clone, PartialEq)]
pub struct StandIn {
    pub x: f64,
    pub y: f64,
    pub avatar: Value,
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// What a frame's bracket set is: who each bracketed instance is after,
/// and where that partner stands. Rebuilt at the host's run step.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    after: HashMap<usize, PeerId>,
    stand: HashMap<PeerId, StandIn>,
    /// The host's ship (the instance moved).
    pub ship: Option<Value>,
}

impl Plan {
    /// The plan from the host's assignments (`enemies`: (key, player)) and
    /// shared shots' targets (`shots`), for the partners that have a
    /// stand-in place (`stand`; `me`, the host, never has one).
    pub fn new(
        me: PeerId,
        ship: Option<Value>,
        stand: impl IntoIterator<Item = (PeerId, StandIn)>,
        enemies: impl IntoIterator<Item = (usize, PeerId)>,
        shots: impl IntoIterator<Item = (usize, PeerId)>,
    ) -> Self {
        let stand: HashMap<PeerId, StandIn> = stand.into_iter().filter(|(p, _)| *p != me).collect();
        let after = enemies
            .into_iter()
            .chain(shots)
            .filter(|(_, p)| stand.contains_key(p))
            .collect();
        Self { after, stand, ship }
    }

    /// Nothing to bracket: no partner with a place, or no ship.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stand.is_empty() || self.ship.is_none()
    }

    /// How many instances are after a partner.
    #[must_use]
    pub fn len(&self) -> usize {
        self.after.len()
    }

    /// Whether the instance at `key` may be bracketed for an event used as
    /// `u` (the place test comes after, in [`Plan::choose`]): it is after a
    /// partner with a place, or the event is bracketed whoever it is after.
    #[must_use]
    pub fn wants(&self, key: usize, u: Use) -> bool {
        !self.is_empty() && (u.nearest || self.for_key(key).is_some())
    }

    /// The partner an instance is after and where that partner stands.
    #[must_use]
    pub fn for_key(&self, key: usize) -> Option<(PeerId, &StandIn)> {
        let p = *self.after.get(&key)?;
        self.stand.get(&p).map(|s| (p, s))
    }

    /// Where the ship stands for the instance at `key` (at `inst`) with the
    /// host's ship at `ship_at`, for an event used as `u`. An aim
    /// ([`Use::assigned`]) takes the partner it is after; the assignment's
    /// hysteresis is the debounce. A touch takes that partner, or for
    /// [`Use::nearest`] the partner nearest it, only while that partner is
    /// nearer than the host's ship. A partner has a place only while
    /// targetable.
    #[must_use]
    pub fn choose(
        &self,
        key: usize,
        u: Use,
        inst: (f64, f64),
        ship_at: (f64, f64),
    ) -> Option<&StandIn> {
        let partner = match self.for_key(key) {
            Some((_, s)) if u.assigned => return Some(s),
            Some((_, s)) => s,
            None if u.nearest => self
                .stand
                .values()
                .min_by(|a, b| dist((a.x, a.y), inst).total_cmp(&dist((b.x, b.y), inst)))?,
            None => return None,
        };
        (dist((partner.x, partner.y), inst) < dist(ship_at, inst)).then_some(partner)
    }
}

/// What a bracket saved of the host's ship and `oPowers` (put back as is).
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub x: f64,
    pub y: f64,
    /// The ship's speeds, to tell whether the event moved it.
    pub hspeed: f64,
    pub vspeed: f64,
    /// `visible` when the bracket changed it.
    pub visible: Option<f64>,
    /// `oPowers.cloaked` when the bracket changed it (its value as it was).
    pub cloaked: Option<Value>,
}

/// One open bracket.
#[derive(Debug, Clone, PartialEq)]
pub struct Active {
    pub key: usize,
    /// The instance's id (its `target` is written through it).
    pub inst: Value,
    /// The instance's object (for the log).
    pub object: i32,
    /// The host's ship moved and `oPowers`'s index: the restore's, whatever
    /// the plan says by then.
    pub ship: Value,
    pub powers: Option<u32>,
    pub saved: Saved,
    /// Where the ship stands (and the avatar).
    pub at: StandIn,
    pub retarget: bool,
    /// `target` names the ship during the event: (avatar, ship), put back
    /// if it still names the ship.
    pub target: Option<(Value, Value)>,
}

/// The ship and the instance as the restore finds them.
#[derive(Debug, Clone, PartialEq)]
pub struct Now {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub hspeed: Option<f64>,
    pub vspeed: Option<f64>,
    /// The instance's `target`.
    pub target: Option<Value>,
}

/// What the restore writes.
#[derive(Debug, Clone, PartialEq)]
pub struct Writes {
    pub x: f64,
    pub y: f64,
    /// The speeds, when the event changed them.
    pub speeds: Option<(f64, f64)>,
    pub visible: Option<f64>,
    pub cloaked: Option<Value>,
    /// The instance's `target` back to the avatar.
    pub target: Option<Value>,
    /// The event itself moved the ship or changed its speeds.
    pub moved: bool,
}

fn same_bits(now: Option<f64>, want: f64) -> bool {
    now.is_some_and(|v| v.to_bits() == want.to_bits())
}

/// The restore of `a`, bit for bit: the place and what the swap changed;
/// the speeds only if the event changed them; `target` back to the avatar
/// only while it still names the ship (an event that chose another keeps
/// its choice).
#[must_use]
pub fn restore_writes(a: &Active, now: &Now, same: impl Fn(&Value, &Value) -> bool) -> Writes {
    let speeds_kept =
        same_bits(now.hspeed, a.saved.hspeed) && same_bits(now.vspeed, a.saved.vspeed);
    Writes {
        x: a.saved.x,
        y: a.saved.y,
        speeds: (!speeds_kept).then_some((a.saved.hspeed, a.saved.vspeed)),
        visible: a.saved.visible,
        cloaked: a.saved.cloaked.clone(),
        target: a.target.as_ref().and_then(|(avatar, ship)| {
            now.target
                .as_ref()
                .is_some_and(|t| same(t, ship))
                .then(|| avatar.clone())
        }),
        moved: !same_bits(now.x, a.at.x) || !same_bits(now.y, a.at.y) || !speeds_kept,
    }
}

/// What the observer does at an event's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Begin {
    /// Inside an open bracket, the ship standing in: counted, nothing changed.
    Nested,
    /// Not bracketed.
    Pass,
    /// Open a bracket for this event.
    Swap,
    /// Inside an open bracket, another instance's event, a Destroy or a
    /// death script: the ship goes home for its length.
    Home,
}

/// What the observer does at an event's end.
#[derive(Debug, Clone, PartialEq)]
pub enum End {
    Nothing,
    /// The outermost event ended: put back.
    Close(Box<Active>),
    /// The event that sent the ship home ended: stand it in again.
    Resume,
}

/// The bracket's state across events.
#[derive(Debug, Default)]
pub struct Bracket {
    active: Option<Active>,
    /// Observed events (and death scripts) open inside the active one.
    depth: u32,
    /// The depth of the event that sent the ship home.
    home_from: Option<u32>,
}

impl Bracket {
    /// An observed event of the instance at `key` starts (`home`: a Destroy
    /// or a death script); `wanted`: its instance is bracketed.
    pub fn begin(&mut self, key: usize, wanted: bool, home: bool) -> Begin {
        let Some(a) = &self.active else {
            return if wanted && !home {
                Begin::Swap
            } else {
                Begin::Pass
            };
        };
        self.depth += 1;
        if self.home_from.is_none() && (home || a.key != key) {
            self.home_from = Some(self.depth);
            return Begin::Home;
        }
        Begin::Nested
    }

    /// The swap was made: the bracket is open.
    pub fn open(&mut self, a: Active) {
        self.active = Some(a);
        self.depth = 0;
        self.home_from = None;
    }

    /// An observed event of the instance at `key` ended.
    pub fn end(&mut self, key: usize) -> End {
        if self.depth > 0 {
            let resume = self.home_from == Some(self.depth);
            if resume {
                self.home_from = None;
            }
            self.depth -= 1;
            return if resume { End::Resume } else { End::Nothing };
        }
        if self.active.as_ref().is_some_and(|a| a.key == key) {
            return self
                .active
                .take()
                .map_or(End::Nothing, |a| End::Close(Box::new(a)));
        }
        End::Nothing
    }

    /// The ship is home (an event inside sent it there).
    #[must_use]
    pub fn is_home(&self) -> bool {
        self.home_from.is_some()
    }

    /// An open bracket left over (an event whose end was never seen): taken
    /// to be put back.
    pub fn take_stale(&mut self) -> Option<Active> {
        self.depth = 0;
        let home = self.home_from.take().is_some();
        let a = self.active.take();
        // Home: the ship is where it was; nothing to put back.
        if home { None } else { a }
    }

    /// The open bracket for `key`.
    #[must_use]
    pub fn open_for(&self, key: usize) -> Option<&Active> {
        self.active.as_ref().filter(|a| a.key == key)
    }

    /// [`Bracket::open_for`], to record a target written meanwhile.
    pub fn open_for_mut(&mut self, key: usize) -> Option<&mut Active> {
        self.active.as_mut().filter(|a| a.key == key)
    }

    /// The open bracket (to stand the ship in again at a resume).
    pub fn active_mut(&mut self) -> Option<&mut Active> {
        self.active.as_mut()
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> PeerId {
        PeerId(n)
    }

    fn stand() -> StandIn {
        StandIn {
            x: 100.0,
            y: 200.0,
            avatar: Value::Real(100_001.0),
        }
    }

    fn same(a: &Value, b: &Value) -> bool {
        a == b
    }

    fn active(key: usize) -> Active {
        Active {
            key,
            inst: Value::Real(100_007.0),
            object: 7,
            ship: Value::Real(100_000.0),
            powers: Some(3),
            saved: Saved {
                x: 0.1 + 0.2,
                y: -3.0e-310,
                hspeed: 1.0 / 3.0,
                vspeed: -0.0,
                visible: Some(0.0),
                cloaked: Some(Value::Bool(true)),
            },
            at: stand(),
            retarget: true,
            target: Some((Value::Real(100_001.0), Value::Real(100_000.0))),
        }
    }

    /// An aim by the ship's place (a dash's heading, a beam's range gate).
    const POSE: Use = Use {
        retarget: false,
        nearest: false,
        assigned: true,
    };
    /// The identity test (a Juggernaut's charge, a mine's blast).
    const IDENT: Use = Use {
        retarget: true,
        nearest: false,
        assigned: true,
    };
    /// An event that reaches a contact test (the Star Eater segment's touch).
    const TOUCH: Use = Use {
        retarget: false,
        nearest: false,
        assigned: false,
    };
    /// Only proximity tests (a ball's burst), whoever it is after.
    const NEAR: Use = Use {
        retarget: false,
        nearest: true,
        assigned: false,
    };

    /// The host and a plan where 1 is after the partner, 2 after the host;
    /// the partner stands at (100, 200).
    fn plan_with_partner() -> Plan {
        Plan::new(
            peer(0),
            Some(Value::Real(100_000.0)),
            [(peer(1), stand())],
            [(1, peer(1)), (2, peer(0))],
            [],
        )
    }

    #[test]
    fn tables_are_sorted_and_unique() {
        for w in BRACKETED.windows(2) {
            assert!(
                (w[0].0, w[0].1) < (w[1].0, w[1].1),
                "{:?} before {:?}",
                w[0],
                w[1]
            );
        }
        for w in NOT_BRACKETED.windows(2) {
            assert!(w[0].0 < w[1].0, "{} before {}", w[0].0, w[1].0);
        }
        for w in DEATH_SCRIPTS.windows(2) {
            assert!(w[0] < w[1], "{} before {}", w[0], w[1]);
        }
    }

    /// The Star Eater keeps the nearest ship: each of its bracketed events
    /// reaches a touch (the segment's contact, the chomp), and the vortex is
    /// never bracketed.
    #[test]
    fn the_star_eater_keeps_the_nearest_ship() {
        for (o, e, _, k) in BRACKETED {
            if o.starts_with("oStarEater") && !NOT_BRACKETED.iter().any(|w| w.0 == *o) {
                assert_eq!(*k, Kind::Touch, "{o} {e}: the Star Eater changes");
            }
        }
    }

    #[test]
    fn the_plan_takes_what_is_after_a_partner_with_a_place() {
        let host = peer(0);
        let p = Plan::new(
            host,
            Some(Value::Real(100_000.0)),
            [(peer(1), stand()), (host, stand())],
            [(1, peer(1)), (2, host), (3, peer(2))],
            [(4, peer(1)), (5, host)],
        );
        assert_eq!(p.len(), 2, "the enemy and the shot after the partner");
        assert!(p.for_key(1).is_some());
        assert!(p.for_key(4).is_some());
        assert!(p.for_key(2).is_none(), "the host's own: the game's own");
        assert!(p.for_key(3).is_none(), "a peer with no stand-in place");
        assert!(p.for_key(5).is_none());
        assert!(!p.is_empty());
        let none = Plan::new(host, None, [(peer(1), stand())], [(1, peer(1))], []);
        assert!(none.is_empty(), "no ship to move");
        let alone = Plan::new(host, Some(Value::Real(1.0)), [], [(1, peer(1))], []);
        assert!(alone.is_empty(), "no partner with a place");
    }

    #[test]
    fn an_aim_after_the_partner_follows_the_assignment_even_with_the_host_nearer() {
        let p = plan_with_partner();
        let ship_far = (2000.0, 200.0);
        let ship_near = (150.0, 200.0);
        for u in [POSE, IDENT] {
            assert!(p.wants(1, u));
            assert!(p.choose(1, u, (110.0, 200.0), ship_far).is_some());
            assert!(
                p.choose(1, u, (160.0, 200.0), ship_near).is_some(),
                "after the partner, the host's ship nearer: still the partner's place ({u:?})"
            );
            assert!(
                p.choose(1, u, (125.0, 200.0), ship_near).is_some(),
                "a tie: the partner's place, no flip at equal distances"
            );
        }
    }

    #[test]
    fn a_touch_after_the_partner_keeps_the_nearest_ship() {
        let p = plan_with_partner();
        let ship_far = (2000.0, 200.0);
        let ship_near = (150.0, 200.0);
        assert!(p.wants(1, TOUCH));
        assert!(p.choose(1, TOUCH, (110.0, 200.0), ship_far).is_some());
        assert!(
            p.choose(1, TOUCH, (160.0, 200.0), ship_near).is_none(),
            "after the partner, the host touching it: the host's ship (the host sets it off)"
        );
        assert!(
            p.choose(1, TOUCH, (125.0, 200.0), ship_near).is_none(),
            "a tie: the host's ship (the game's own)"
        );
    }

    #[test]
    fn after_the_host_or_with_the_partner_cloaked_or_down_nothing_stands_in() {
        let p = plan_with_partner();
        let ship_far = (2000.0, 200.0);
        for u in [POSE, IDENT, TOUCH] {
            assert!(!p.wants(2, u), "the host's own enemy ({u:?})");
            assert!(
                p.choose(2, u, (110.0, 200.0), ship_far).is_none(),
                "after the host, the partner the nearer: the host's ship ({u:?})"
            );
        }
        // Cloaked, down or the avatar gone: the glue gives the partner no
        // place (`Entities::target` is only a targetable partner's).
        let hidden = Plan::new(
            peer(0),
            Some(Value::Real(100_000.0)),
            [],
            [(1, peer(1))],
            [],
        );
        for u in [POSE, IDENT, TOUCH, NEAR] {
            assert!(!hidden.wants(1, u));
            assert!(hidden.choose(1, u, (110.0, 200.0), ship_far).is_none());
        }
    }

    #[test]
    fn a_ball_whoever_it_is_after_answers_the_nearer_ship() {
        let p = plan_with_partner();
        let ship_far = (2000.0, 200.0);
        let ship_near = (150.0, 200.0);
        // A proximity event (a ball) whoever it is after: the nearer ship.
        assert!(p.wants(2, NEAR) && p.wants(9, NEAR));
        assert!(
            p.choose(2, NEAR, (110.0, 200.0), ship_far).is_some(),
            "the host's ball, the partner nearer: the partner sets it off"
        );
        assert!(p.choose(9, NEAR, (160.0, 200.0), ship_near).is_none());
        assert!(
            p.choose(1, NEAR, (160.0, 200.0), ship_near).is_none(),
            "the partner's ball, the host flies into it: the host's"
        );
    }

    #[test]
    fn nesting_pairs_and_other_instances_deaths_run_at_home() {
        let mut b = Bracket::default();
        assert_eq!(b.begin(9, false, false), Begin::Pass);
        assert_eq!(b.end(9), End::Nothing);
        assert_eq!(
            b.begin(1, true, true),
            Begin::Pass,
            "a death outside a bracket"
        );
        assert_eq!(b.end(1), End::Nothing);
        assert_eq!(b.begin(1, true, false), Begin::Swap);
        b.open(active(1));
        assert!(b.open_for(1).is_some() && b.open_for(2).is_none());
        // Its own nested event (an inherited event, a user event): no swap.
        assert_eq!(b.begin(1, true, false), Begin::Nested);
        // Another instance's event inside it: home, then back.
        assert_eq!(b.begin(2, true, false), Begin::Home);
        assert!(b.is_home());
        // Inside that, more: nothing more changes.
        assert_eq!(b.begin(3, false, true), Begin::Nested);
        assert_eq!(b.end(3), End::Nothing);
        assert_eq!(b.end(2), End::Resume);
        assert!(!b.is_home());
        assert_eq!(b.end(1), End::Nothing, "its own nested event");
        // Its death script, then its own Destroy: home each time.
        assert_eq!(b.begin(1, false, true), Begin::Home);
        assert_eq!(b.begin(1, false, true), Begin::Nested);
        assert_eq!(b.end(1), End::Nothing);
        assert_eq!(b.end(1), End::Resume);
        assert_eq!(b.end(1), End::Close(Box::new(active(1))));
        assert!(!b.is_open());
        assert_eq!(b.end(1), End::Nothing);
        // A mismatched end at depth 0 (another key): nothing closes.
        b.begin(1, true, false);
        b.open(active(1));
        assert_eq!(b.end(5), End::Nothing);
        assert!(b.is_open());
    }

    #[test]
    fn a_stale_bracket_is_taken_unless_the_ship_is_home() {
        let mut b = Bracket::default();
        b.begin(5, true, false);
        b.open(active(5));
        b.begin(5, false, false);
        assert_eq!(b.take_stale(), Some(active(5)));
        assert_eq!(b.begin(6, false, false), Begin::Pass);
        // Left open while home: the ship is already where it was.
        b.begin(5, true, false);
        b.open(active(5));
        assert_eq!(b.begin(6, false, false), Begin::Home);
        assert_eq!(b.take_stale(), None);
        assert!(!b.is_open() && !b.is_home());
    }

    #[test]
    fn the_restore_puts_back_what_the_swap_changed() {
        let a = active(1);
        let untouched = Now {
            x: Some(100.0),
            y: Some(200.0),
            hspeed: Some(1.0 / 3.0),
            vspeed: Some(-0.0),
            target: Some(Value::Real(100_000.0)),
        };
        let w = restore_writes(&a, &untouched, same);
        assert_eq!(w.x.to_bits(), a.saved.x.to_bits());
        assert_eq!(w.y.to_bits(), a.saved.y.to_bits());
        assert_eq!(w.speeds, None, "speeds untouched: not written");
        assert_eq!(w.visible, Some(0.0));
        assert_eq!(w.cloaked, Some(Value::Bool(true)), "its value and type");
        assert_eq!(
            w.target,
            Some(Value::Real(100_001.0)),
            "the ship back to the avatar"
        );
        assert!(!w.moved);
        // The event moved the ship and changed its speed: put back, and reported as moved.
        let moved = Now {
            x: Some(101.0),
            hspeed: Some(0.0),
            ..untouched.clone()
        };
        let w = restore_writes(&a, &moved, same);
        assert!(w.moved);
        assert_eq!(w.speeds, Some((1.0 / 3.0, -0.0)));
        // -0.0 and 0.0 differ by bit: a speed written to 0.0 is put back.
        let zeroed = Now {
            vspeed: Some(0.0),
            ..untouched.clone()
        };
        assert!(restore_writes(&a, &zeroed, same).speeds.is_some());
        // The event chose another target: kept.
        let retargeted = Now {
            target: Some(Value::Real(5.0)),
            ..untouched.clone()
        };
        assert_eq!(restore_writes(&a, &retargeted, same).target, None);
        // No target written (a Pose event): none restored.
        let pose = Active {
            retarget: false,
            target: None,
            ..active(1)
        };
        assert_eq!(restore_writes(&pose, &untouched, same).target, None);
        // Nothing the swap left alone is written.
        let plain = Active {
            saved: Saved {
                visible: None,
                cloaked: None,
                ..active(1).saved
            },
            ..active(1)
        };
        let w = restore_writes(&plain, &untouched, same);
        assert_eq!((w.visible, w.cloaked), (None, None));
    }

    #[test]
    fn descendants_are_bracketed_and_writers_excluded() {
        // 10 <- 11 <- 12 (12 writes: under writer 12), 20 alone, 30 <- 31.
        let parent = |i: u32| match i {
            11 => Some(10),
            12 => Some(11),
            31 => Some(30),
            _ => None,
        };
        let under = |i: u32, r: u32| {
            let mut o = Some(i);
            while let Some(x) = o {
                if x == r {
                    return true;
                }
                o = parent(x);
            }
            false
        };
        let wanted = [
            (10, "Step_0", Need::Pose, Kind::Aim),
            (20, "Other_10", Need::Identity, Kind::Aim),
            (30, "Step_0", Need::Proximity, Kind::Touch),
            (30, "Alarm_1", Need::Pose, Kind::Aim),
        ];
        let m = bracketable([10, 11, 12, 20, 21, 30, 31], under, &wanted, &[12]);
        assert_eq!(m.get(&10), m.get(&11), "a descendant as its root");
        assert!(
            m.get(&11)
                .is_some_and(|e| e.get(3, 0) == Some(POSE) && e.get(3, 1).is_none())
        );
        assert!(!m.contains_key(&12), "under a writer: never");
        assert!(!m.contains_key(&21), "not under a root");
        let all = m.get(&20).expect("user event: every kind");
        assert_eq!(all.get(2, 7), Some(IDENT));
        let near = m.get(&31).expect("descendant of 30");
        assert_eq!(near.get(3, 0), Some(NEAR));
        assert_eq!(near.get(2, 1), Some(POSE));
        // An All over a proximity event: not whoever it is after, and its
        // touch keeps the nearest ship for every kind.
        let mut e = Events::Only(Vec::new());
        e.add("Step_0", Need::Proximity, Kind::Touch);
        e.add("Other_10", Need::Pose, Kind::Aim);
        assert_eq!(e, Events::All(TOUCH));
        // An aim and a touch in one event: the nearest ship.
        let mut e = Events::Only(Vec::new());
        e.add("Step_0", Need::Pose, Kind::Aim);
        e.add("Step_0", Need::Pose, Kind::Touch);
        assert_eq!(e.get(3, 0), Some(TOUCH));
        assert_eq!(event_of("Alarm_3"), Some((2, 3)));
        assert_eq!(event_of("Step_2"), Some((3, 2)));
        assert_eq!(event_of("Other_10"), None);
    }
}
