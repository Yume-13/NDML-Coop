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
//! parsing (the glue resolves names and ancestry). [`PARTS`] and [`LOCAL`]
//! name the `oSolid` children that are their maker's parts or each game's
//! own missiles.

#![forbid(unsafe_code)]

use ndml_sdk::Value;
use ndml_sdk::abi::OBJECT_REF_TAG;

/// Parts made by their leader at any time, not only while it is built
/// (`oEnemySegment` children count too).
pub const PARTS: &[&str] = &[
    "oSnapperJawLeft",
    "oSnapperJawRight",
    "oSnapperTentacle",
    "oSnapperTentacleBase",
    "oEnemyHardShield",
];

/// Enemy missiles: like shots, each game makes its own from its copy of the
/// shooter.
pub const LOCAL: &[&str] = &[
    "oConstructorMissile",
    "oDreadnoughtMissile",
    "oDwellerChitinMissile",
    "oInterceptorMissile",
    "oJuggMissile",
    "oJuggMissileHoming",
    "oWarbringerMissile",
];

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
    /// Enemy shots, blasts and beams.
    pub const ENEMY_SHOTS: [&str; 3] = ["oEnemyProjectile", "oEnemyBlast", "oEnemyBeam"];
    /// Beam charge-ups (`oEffect` children), shared and counted as beams.
    pub const ENEMY_CHARGES: [&str; 6] = [
        "oEnemyBeamCharge",
        "oEnemyBeamChargeBlue",
        "oEnemyBeamChargeGreen",
        "oEnemyBeamChargeLenDir",
        "oEnemyBeamChargePulse",
        "oEnemyBeamChargeScaled",
    ];
    /// Enemy areas (an `oEnemyBeam` child).
    pub const ENEMY_AREA: &str = "oEnemyAoE";
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
    /// An `oSolid` child (enemy, part, asteroid, mine).
    Enemy {
        /// An `oEnemyStandard` child.
        standard: bool,
        /// An `oEnemySegment` child (never created standalone).
        segment: bool,
        /// An `oTarget` child.
        target: bool,
        /// Part of whoever creates it, whenever it is made: a segment, or in
        /// [`PARTS`]. Never replicated; a puppet builds its own.
        part: bool,
        /// An attack each game makes for itself ([`LOCAL`]): never replicated;
        /// a puppet's own is let through and left to the game.
        local: bool,
    },
    /// An `oCollectable` child.
    Collectable,
    /// An enemy shot, blast or beam ([`parent::ENEMY_SHOTS`] children).
    EnemyShot,
    /// A spawn telegraph (`oSpawnWarning`, or `oEnemyPortal` when `portal`).
    Marker { portal: bool },
    /// Anything else (shots, effects, controllers, the avatar).
    Other,
}

impl Class {
    /// The client vetoes local creation of these.
    #[must_use]
    pub fn replicated(self) -> bool {
        matches!(self, Self::Enemy { .. } | Self::Collectable)
    }

    /// An `oSolid` child.
    #[must_use]
    pub fn is_enemy(self) -> bool {
        matches!(self, Self::Enemy { .. })
    }

    /// Part of its creator (see [`Class::Enemy`]).
    #[must_use]
    pub fn is_part(self) -> bool {
        matches!(self, Self::Enemy { part: true, .. })
    }

    /// An attack each game makes for itself (see [`Class::Enemy`]).
    #[must_use]
    pub fn is_local(self) -> bool {
        matches!(self, Self::Enemy { local: true, .. })
    }
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
        part: false,
        local: false,
    };

    #[test]
    fn classes() {
        for n in PARTS.iter().chain(LOCAL) {
            assert!(n.starts_with('o'), "{n}");
        }
        assert!(Class::Collectable.replicated() && !Class::Collectable.is_enemy());
        assert!(ENEMY.replicated() && ENEMY.is_enemy());
        assert!(!Class::Other.replicated());
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
