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

//! Which game objects the shared world deals with: names, classes and value
//! parsing (the glue resolves names and ancestry). A client makes every
//! enemy-family object the host replicates the same way: `instance_create`
//! alone, then the host's whole state (`gen_glue.rs`).

#![forbid(unsafe_code)]

use ndml_sdk::Value;
use ndml_sdk::abi::OBJECT_REF_TAG;

/// Parents the glue resolves by name.
pub mod parent {
    /// Every enemy, segment, asteroid and mine.
    pub const SOLID: &str = "oSolid";
    /// Orbs and power-ups.
    pub const COLLECTABLE: &str = "oCollectable";
    /// Data-driven enemies (user event 0 fills their phases).
    pub const ENEMY_STANDARD: &str = "oEnemyStandard";
    /// Chain segments (built by their leader).
    pub const SEGMENT: &str = "oEnemySegment";
    /// Asteroids, mines, missiles.
    pub const TARGET: &str = "oTarget";
    /// Enemies that emerge from the background (`oHiveEmerge`,
    /// `oEclipseEmerging`): not an `oSolid` (no collision, no hit). Each
    /// grows out of the background in its own Step, then makes the solid
    /// enemy in its place and ends itself ([`super::Class::Enemy`]'s
    /// `emerging`).
    pub const EMERGING: &str = "oEnemyEmerging";
    /// Enemy shots, blasts and beams.
    pub const ENEMY_SHOTS: [&str; 3] = ["oEnemyProjectile", "oEnemyBlast", "oEnemyBeam"];
    /// Beam charge-ups (`oEffect` children, and their children), shared and
    /// counted as beams. They go by this list, not by the wind-up rule
    /// (`world::windup`): the host owns each one whoever makes it. Nothing
    /// in the game marks them but their names (`chargeUpObject` holds them
    /// and other wind-ups alike).
    pub const ENEMY_CHARGES: [&str; 6] = [
        "oEnemyBeamCharge",
        "oEnemyBeamChargeBlue",
        "oEnemyBeamChargeGreen",
        "oEnemyBeamChargeLenDir",
        "oEnemyBeamChargePulse",
        "oEnemyBeamChargeScaled",
    ];
    /// Effects (the one [`LOCAL_ROOTS`] root a wind-up may be under, and
    /// the pooled ones under it that are never one: `world::windup`).
    pub const EFFECT: &str = "oEffect";
    pub const EFFECT_POOLED: &str = "oEffectPooled";
    /// Enemy areas (an `oEnemyBeam` child).
    pub const ENEMY_AREA: &str = "oEnemyAoE";
    /// The spawn machinery ([`super::Class::Spawner`]): `oEnemySpawner` and
    /// every other child.
    pub const SPAWNER: &str = "oSpawner";
    /// What stays each game's own by ancestry: never a dependency.
    pub const LOCAL_ROOTS: [&str; 13] = [
        "oEffect",
        "oTrail",
        "oScreenEffect",
        "oGuiObject",
        "oBackground",
        "oPlayer",
        "oPlayerProjectile",
        "oPlayerShield",
        "oPlayerConstruct",
        "oPlayerBurn",
        "oPlayerBlast",
        "oPlayerAttack",
        "oUpgrades",
    ];
}

/// Whether an object (its own and its ancestors' names) is a player's: a
/// name starting with `oPlayer`, the mirror's rule. Such objects stay each
/// game's own; the partner draws them from the mirror.
#[must_use]
pub fn is_player_family_name(chain: &[&str]) -> bool {
    chain.iter().any(|n| n.starts_with("oPlayer"))
}

/// What an object is, from its ancestry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// An `oSolid` child (enemy, part, asteroid, mine), or an enemy that
    /// emerges from the background (`emerging`).
    Enemy {
        /// An `oEnemyStandard` child.
        standard: bool,
        /// An `oEnemySegment` child (never created standalone).
        segment: bool,
        /// An `oTarget` child.
        target: bool,
        /// A [`parent::EMERGING`] child: replicated like an enemy but not an
        /// `oSolid`, so nothing hits, targets or counts it on either game,
        /// and its Step has no enemy tick of its own: the world ticks it after
        /// its Step ([`Self::ticked_after_its_step`]). Its end is the host's
        /// (`world::emerge`).
        emerging: bool,
    },
    /// An `oCollectable` child.
    Collectable,
    /// An enemy shot, blast or beam ([`parent::ENEMY_SHOTS`] children).
    EnemyShot,
    /// A spawn telegraph (`oSpawnWarning`, or `oEnemyPortal` when `portal`).
    Marker { portal: bool },
    /// The host's spawn machinery (an `oSpawner` child: `oEnemySpawner`,
    /// made by `spawnEnemies` for the wave director, a black hole or an
    /// enemy that summons: a Host, Gyrogun, Bolt Thrower, Warden, Saucer).
    /// Never a dependency, a follower, a world spawn or a part on a joining
    /// game: all a spawner does reaches it another way (its telegraph as a
    /// `SpawnMarker`, the enemy its `Alarm_1` makes as that enemy's own
    /// spawn).
    Spawner,
    /// Anything else (shots, effects, controllers, the avatar).
    Other,
}

impl Class {
    /// The client vetoes local creation of these.
    #[must_use]
    pub fn replicated(self) -> bool {
        matches!(self, Self::Enemy { .. } | Self::Collectable)
    }

    /// An `oSolid` child, or an enemy that emerges from the background.
    #[must_use]
    pub fn is_enemy(self) -> bool {
        matches!(self, Self::Enemy { .. })
    }

    /// An enemy whose Step has no enemy tick of its own: the world ticks it
    /// after its Step, every frame, on both games (the emerging ones).
    #[must_use]
    pub fn ticked_after_its_step(self) -> bool {
        matches!(self, Self::Enemy { emerging: true, .. })
    }

    /// The class of an object from its ancestry: an `oSolid` child, else an
    /// [`parent::EMERGING`] child, is an enemy; `None` for the rest (the glue
    /// tells those apart). `standard`, `segment` and `target` are the
    /// `oSolid` child's own ancestry.
    #[must_use]
    pub fn enemy_by_ancestry(solid: bool, emerging: bool, of: EnemyAncestry) -> Option<Self> {
        if solid {
            Some(Self::Enemy {
                standard: of.standard,
                segment: of.segment,
                target: of.target,
                emerging: false,
            })
        } else if emerging {
            Some(Self::Enemy {
                standard: false,
                segment: false,
                target: false,
                emerging: true,
            })
        } else {
            None
        }
    }

    /// May an instance of this class be a dependency, a follower a leader's
    /// code made, or a part of a replicated enemy: only [`Self::Other`]
    /// (the rest is replicated in its own right, a telegraph, or the
    /// host's spawn machinery, [`Self::Spawner`]).
    #[must_use]
    pub fn may_go_with_an_enemy(self) -> bool {
        self == Self::Other
    }
}

/// An `oSolid` child's further ancestry ([`Class::enemy_by_ancestry`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one per ancestor asked
pub struct EnemyAncestry {
    pub standard: bool,
    pub segment: bool,
    pub target: bool,
}

/// The object index in a value the game gives for an object: an object ref
/// (`OBJECT_REF_TAG | index`) or a plain number. `None` for other refs,
/// negatives (`-1` = no such asset) and non-numbers.
#[must_use]
pub fn object_index(v: &Value) -> Option<u32> {
    const TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;
    match *v {
        Value::Ref(p) if p & TAG_MASK == OBJECT_REF_TAG => Some((p & 0xFFFF_FFFF) as u32),
        Value::Ref(_) => None,
        _ => {
            let f = v.as_f64()?;
            (f.is_finite() && f >= 0.0 && f < f64::from(u32::MAX)).then_some(f as u32)
        }
    }
}

/// Whether a value looks like a live instance id as the game returns them (a
/// ref, or a number from 100000 up). `noone` (-4) and failures are not.
#[must_use]
pub fn is_instance(v: &Value) -> bool {
    match v {
        Value::Ref(p) => *p & 0xFFFF_FFFF_0000_0000 != OBJECT_REF_TAG,
        other => other.as_f64().is_some_and(|n| n >= 100_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_families_stay_local_whatever_their_root() {
        assert!(is_player_family_name(&[
            "oDroneBullet",
            "oPlayerDroneProjectile"
        ]));
        assert!(is_player_family_name(&["oBlaster", "oPlayerProjectile"]));
        assert!(is_player_family_name(&["oPlayer"]));
        assert!(!is_player_family_name(&[
            "oStarEaterLeg",
            "oEnemySegment",
            "oSolid"
        ]));
        assert!(!is_player_family_name(&["oSpikeShard"]));
        assert!(!is_player_family_name(&[]));
    }

    const ENEMY: Class = Class::Enemy {
        standard: true,
        segment: false,
        target: false,
        emerging: false,
    };

    #[test]
    fn classes() {
        assert!(Class::Collectable.replicated() && !Class::Collectable.is_enemy());
        assert!(ENEMY.replicated() && ENEMY.is_enemy());
        assert!(!Class::Other.replicated());
    }

    /// The emerging family (an `oEnemyEmerging` child, no `oSolid`) is a
    /// replicated enemy: the joining game never makes one itself, never
    /// takes one for a dependency, follower or part, and ticks it after its
    /// Step (no enemy tick of its own); an `oSolid` child is ticked by its
    /// own.
    #[test]
    fn the_emerging_family_is_a_replicated_enemy_ticked_after_its_step() {
        let of = EnemyAncestry {
            standard: true,
            ..EnemyAncestry::default()
        };
        let emerging = Class::enemy_by_ancestry(false, true, of).unwrap();
        assert!(emerging.replicated() && emerging.is_enemy());
        assert!(emerging.ticked_after_its_step());
        assert!(!emerging.may_go_with_an_enemy());
        assert_eq!(
            emerging,
            Class::Enemy {
                standard: false,
                segment: false,
                target: false,
                emerging: true,
            }
        );
        let solid = Class::enemy_by_ancestry(true, false, of).unwrap();
        assert_eq!(solid, ENEMY);
        assert!(!solid.ticked_after_its_step());
        // An `oSolid` child is the `oSolid` kind whatever else it is.
        assert_eq!(Class::enemy_by_ancestry(true, true, of), Some(ENEMY));
        assert_eq!(Class::enemy_by_ancestry(false, false, of), None);
        assert_eq!(parent::EMERGING, "oEnemyEmerging");
    }

    /// The spawn machinery is the host's alone: not replicated, never with
    /// an enemy (dependency, follower, part); an effect may be.
    #[test]
    fn a_spawner_goes_with_nothing() {
        assert!(!Class::Spawner.replicated() && !Class::Spawner.is_enemy());
        assert!(!Class::Spawner.may_go_with_an_enemy());
        assert!(Class::Other.may_go_with_an_enemy());
        assert!(!ENEMY.may_go_with_an_enemy());
        assert!(!Class::Marker { portal: false }.may_go_with_an_enemy());
        assert_eq!(parent::SPAWNER, "oSpawner");
    }

    #[test]
    fn values() {
        assert_eq!(object_index(&Value::object(0x325)), Some(0x325));
        assert_eq!(object_index(&Value::Real(605.0)), Some(605));
        assert_eq!(object_index(&Value::Real(-1.0)), None);
        assert_eq!(
            object_index(&Value::Ref(0x0100_0001_0000_0005)),
            None,
            "a sprite"
        );
        assert_eq!(object_index(&Value::Undefined), None);
        assert!(is_instance(&Value::Real(100_123.0)));
        assert!(is_instance(&Value::Ref(0x0100_0002_0001_86A3)));
        assert!(!is_instance(&Value::Real(-4.0)));
        assert!(!is_instance(&Value::object(3)));
        assert!(!is_instance(&Value::Undefined));
    }
}
