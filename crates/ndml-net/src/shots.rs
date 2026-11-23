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

//! Shared enemy shots on the wire. The host owns which enemy shots exist and
//! sends each shot's birth ([`ShotFire`]) and end ([`ShotGone`]); a joining
//! game makes the shot at its shown time and reports the ends its own player
//! caused. All reliable, batched per frame, split to fit a datagram.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use crate::msg::{PeerId, StateValue};
use crate::packet::MAX_MESSAGE;

/// Why a shot ended ([`ShotEnd::cause`]).
pub mod cause {
    /// The host's world: asteroids, `destroyOutside`, enemy-side code.
    pub const WORLD: u8 = 0;
    /// The sender's player: its ship, shield, shots, constructs, blades.
    pub const PLAYER: u8 = 1;
    /// The shot's own timed end (an alarm or step that ends it).
    pub const OWN: u8 = 2;
    /// No known site ended it.
    pub const UNKNOWN: u8 = 3;
    /// It became something else (`instance_change`).
    pub const CHANGED: u8 = 4;
    /// Host: a joining player's end, forwarded to the other joining games.
    pub const PEER: u8 = 5;

    /// A short name for log lines.
    #[must_use]
    pub fn name(c: u8) -> &'static str {
        match c {
            WORLD => "world",
            PLAYER => "player",
            OWN => "own",
            UNKNOWN => "unknown",
            CHANGED => "changed",
            PEER => "peer",
            _ => "?",
        }
    }
}

/// What a turning shot steers at on the host; other screens follow it.
pub mod steer {
    /// Not known yet (no target, or not turning).
    pub const UNKNOWN: u8 = 0;
    /// A player's thing: the shot is after its player.
    pub const PLAYER: u8 = 1;
    /// Something of the world: left to each game.
    pub const WORLD: u8 = 2;
}

/// One shot's birth: what [`ShotFire`] carries per shot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FiredShot {
    /// The host's id for the shot (a per-run counter).
    pub id: u32,
    pub object: u16,
    /// The host's clock when the state was read (ms).
    pub time_ms: u32,
    /// Instance variables by index into the object's names.
    pub vars: Vec<(u16, StateValue)>,
    /// Built-in variables (the generic transfer's index).
    pub builtins: Vec<(u8, StateValue)>,
    /// Alarms left (index, steps).
    pub alarms: Vec<(u8, i32)>,
    /// The player the shot goes after: a homing shot targets that player's
    /// ship on their screen and their avatar on every other one.
    pub target: Option<PeerId>,
    /// What it steers at on the host when read ([`steer`]).
    pub steer: u8,
    /// References to an enemy attack the receiver has no copy of by id:
    /// variable index, object and host position; the nearest instance is used.
    pub places: Vec<(u16, u16, f32, f32)>,
    /// The referenced attacks' variables on the host: (place index, variable
    /// index, value); written only where the receiver's instance lacks one.
    pub place_fills: Vec<(u8, u16, StateValue)>,
    /// An enemy attack each game made itself, shared from now on: the receiver
    /// replaces its own nearest instance with this one.
    pub replaces: bool,
}

/// Host to clients: shots born in a frame (wire tag 51).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotFire {
    pub shots: Vec<FiredShot>,
}

/// One shot's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotEnd {
    pub id: u32,
    /// The host's clock at the end (ms; a joining game's estimate).
    pub time_ms: u32,
    /// [`cause`].
    pub cause: u8,
}

/// Shots that ended (wire tag 52): host to clients, or a joining game to the
/// host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotGone {
    pub ends: Vec<ShotEnd>,
}

/// A shot's new state (wire tag 53). Reserved: decoded, never sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotState {
    pub id: u32,
    pub time_ms: u32,
    pub vars: Vec<(u16, StateValue)>,
    pub builtins: Vec<(u8, StateValue)>,
    pub alarms: Vec<(u8, i32)>,
}

/// A change of what a shared shot steers at on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerChange {
    pub id: u32,
    /// The host's clock at the change (ms): applied at that shown time.
    pub time_ms: u32,
    /// [`steer`].
    pub steer: u8,
}

/// Host to clients: steering changes this frame (wire tag 54). Reliable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotSteer {
    pub changes: Vec<SteerChange>,
}

impl ShotSteer {
    /// The changes in parts that each fit a datagram, in order.
    #[must_use]
    pub fn split(changes: &[SteerChange]) -> Vec<Self> {
        // A change is at most 5 + 5 + 1 bytes.
        let per = (MAX_MESSAGE - SLACK) / 11;
        changes
            .chunks(per)
            .map(|c| Self {
                changes: c.to_vec(),
            })
            .collect()
    }
}

/// A player's thing other than the ship: its owner and its mirror key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThingRef {
    pub owner: PeerId,
    pub key: u16,
}

/// What chases a player's thing: an enemy (its copies) or a shared shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AimSubject {
    Enemy(crate::msg::NetId),
    Shot(u32),
}

/// What an enemy or a shared shot chases on the host changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AimChange {
    pub subject: AimSubject,
    /// The host's clock at the change (ms).
    pub time_ms: u32,
    /// The player's thing it chases now; `None`: the usual rules.
    pub thing: Option<ThingRef>,
}

/// Host to clients: chase targets that are a player's thing, changed this
/// frame (wire tag 57). Reliable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AimThings {
    pub changes: Vec<AimChange>,
}

impl AimThings {
    /// The changes in parts that each fit a datagram, in order.
    #[must_use]
    pub fn split(changes: &[AimChange]) -> Vec<Self> {
        // A change is at most 1 + 5 + 5 + 1 + 1 + 3 bytes.
        let per = (MAX_MESSAGE - SLACK) / 16;
        changes
            .chunks(per)
            .map(|c| Self {
                changes: c.to_vec(),
            })
            .collect()
    }
}

/// Bytes kept free in a part for the message tag and the list's length.
const SLACK: usize = 16;

fn size<T: Serialize>(v: &T) -> usize {
    postcard::to_allocvec(v).map_or(usize::MAX, |b| b.len())
}

impl ShotFire {
    /// The shots in parts that fit a datagram each; shots too big alone are
    /// returned second.
    #[must_use]
    pub fn split(shots: Vec<FiredShot>) -> (Vec<Self>, Vec<FiredShot>) {
        let budget = MAX_MESSAGE - SLACK;
        let mut parts: Vec<Self> = Vec::new();
        let mut too_big = Vec::new();
        let mut used = 0;
        for s in shots {
            let n = size(&s);
            if n > budget {
                too_big.push(s);
                continue;
            }
            match parts.last_mut() {
                Some(p) if used + n <= budget => p.shots.push(s),
                _ => {
                    parts.push(Self { shots: vec![s] });
                    used = 0;
                }
            }
            used += n;
        }
        (parts, too_big)
    }
}

impl ShotGone {
    /// The ends in parts that each fit a datagram, in order.
    #[must_use]
    pub fn split(ends: &[ShotEnd]) -> Vec<Self> {
        // An end is at most 5 + 5 + 1 bytes.
        let per = (MAX_MESSAGE - SLACK) / 11;
        ends.chunks(per)
            .map(|c| Self { ends: c.to_vec() })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::Message;

    fn shot(id: u32, vars: usize) -> FiredShot {
        FiredShot {
            id,
            object: 762,
            time_ms: 123_456,
            vars: (0..vars)
                .map(|i| {
                    let i = u16::try_from(i).unwrap();
                    (i, StateValue::Real(0.1 * f64::from(i)))
                })
                .collect(),
            builtins: vec![(9, StateValue::Real32(100.5)), (18, StateValue::Whole(90))],
            alarms: vec![(0, 30)],
            target: Some(PeerId(1)),
            steer: steer::PLAYER,
            places: vec![(4, 402, 100.5, -20.0)],
            place_fills: vec![(0, 7, StateValue::Real(55.0))],
            replaces: true,
        }
    }

    #[test]
    fn fires_split_to_datagrams_in_order() {
        let shots: Vec<FiredShot> = (0..40).map(|i| shot(i, 37)).collect();
        let (parts, big) = ShotFire::split(shots.clone());
        assert!(big.is_empty());
        assert!(
            parts.len() > 1,
            "40 shots of 37 variables need several parts"
        );
        for p in &parts {
            let m = Message::ShotFire(p.clone());
            assert!(m.encode().is_ok(), "every part fits a datagram");
        }
        let back: Vec<FiredShot> = parts.into_iter().flat_map(|p| p.shots).collect();
        assert_eq!(back, shots);
    }

    #[test]
    fn a_shot_too_big_alone_is_left_out() {
        let (parts, big) = ShotFire::split(vec![shot(1, 2), shot(2, 400), shot(3, 2)]);
        assert_eq!(big.len(), 1);
        assert_eq!(big[0].id, 2);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].shots.len(), 2);
    }

    #[test]
    fn steer_changes_split_and_round_trip() {
        let changes: Vec<SteerChange> = (0..1000u32)
            .map(|i| SteerChange {
                id: u32::MAX - i,
                time_ms: u32::MAX - i,
                steer: steer::WORLD,
            })
            .collect();
        let parts = ShotSteer::split(&changes);
        assert!(parts.len() > 1);
        let mut back = Vec::new();
        for p in parts {
            let m = Message::ShotSteer(p);
            let bytes = m.encode().unwrap();
            assert!(bytes.len() <= MAX_MESSAGE);
            let Message::ShotSteer(p) = Message::decode(&bytes).unwrap() else {
                unreachable!("a ShotSteer decodes as one");
            };
            back.extend(p.changes);
        }
        assert_eq!(back, changes);
    }

    #[test]
    fn aim_changes_split_and_round_trip() {
        let changes: Vec<AimChange> = (0..1000u32)
            .map(|i| AimChange {
                subject: if i % 2 == 0 {
                    AimSubject::Shot(u32::MAX - i)
                } else {
                    AimSubject::Enemy(u16::MAX)
                },
                time_ms: u32::MAX - i,
                thing: (i % 3 != 0).then_some(ThingRef {
                    owner: PeerId(u8::MAX),
                    key: u16::MAX,
                }),
            })
            .collect();
        let parts = AimThings::split(&changes);
        assert!(parts.len() > 1);
        let mut back = Vec::new();
        for p in parts {
            let m = Message::AimThings(p);
            let bytes = m.encode().unwrap();
            assert!(bytes.len() <= MAX_MESSAGE);
            let Message::AimThings(p) = Message::decode(&bytes).unwrap() else {
                unreachable!("an AimThings decodes as one");
            };
            back.extend(p.changes);
        }
        assert_eq!(back, changes);
    }

    #[test]
    fn ends_split_and_round_trip() {
        let ends: Vec<ShotEnd> = (0..300)
            .map(|i| ShotEnd {
                id: u32::MAX - i,
                time_ms: u32::MAX,
                cause: cause::PLAYER,
            })
            .collect();
        let parts = ShotGone::split(&ends);
        let mut back = Vec::new();
        for p in parts {
            let bytes = Message::ShotGone(p).encode().expect("fits a datagram");
            let Message::ShotGone(g) = Message::decode(&bytes).unwrap() else {
                unreachable!("a ShotGone decodes as one");
            };
            back.extend(g.ends);
        }
        assert_eq!(back, ends);
        assert!(ShotGone::split(&[]).is_empty());
    }

    #[test]
    fn cause_names() {
        for c in 0..=5 {
            assert_ne!(cause::name(c), "?");
        }
        assert_eq!(cause::name(200), "?");
    }
}
