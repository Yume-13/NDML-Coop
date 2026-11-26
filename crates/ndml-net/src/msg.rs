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

//! The co-op message set.
//!
//! Messages are `postcard`-encoded; the variant index is the wire tag, so
//! **variants are append-only** within a
//! [`PROTOCOL_VERSION`](crate::packet::PROTOCOL_VERSION). The host relays
//! between clients, so messages name their peer explicitly.

#![forbid(unsafe_code)]

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::packet::MAX_MESSAGE;

/// A player in the session. The host is always 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PeerId(pub u8);

impl PeerId {
    /// The host.
    pub const HOST: Self = Self(0);
    /// Largest peer id the session machines can track.
    pub const MAX: u8 = 31;

    /// Whether this is the host.
    #[must_use]
    pub fn is_host(self) -> bool {
        self == Self::HOST
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "P{}", self.0 + 1)
    }
}

/// Host-assigned id of a replicated enemy (or other world object).
pub type NetId = u16;

/// Which channel a message travels on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Resent until acked, delivered once and in order.
    Reliable,
    /// Sent once; older copies of the same stream are dropped on arrival.
    Unreliable,
}

/// Version handshake, sent by each side once connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Version of the co-op mod.
    pub mod_version: u32,
    /// Game build string (both peers must match).
    pub build: String,
    /// Display name.
    pub name: String,
}

/// A ship's state, sent by its owner at 60 Hz (relayed by the host).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    /// Whose ship.
    pub peer: PeerId,
    /// Owner's clock, ms (its interpolation timeline).
    pub time_ms: u32,
    /// Position.
    pub x: f32,
    /// Position.
    pub y: f32,
    /// Velocity, px per second.
    pub vx: f32,
    /// Velocity, px per second.
    pub vy: f32,
    /// Facing, degrees (GML convention).
    pub angle: f32,
    /// Sprite index to draw.
    pub sprite: u16,
    /// Animation frame.
    pub frame: u8,
    /// Currently firing.
    pub firing: bool,
    /// Hull.
    pub hull: f32,
    /// Downed and waiting to respawn.
    pub downed: bool,
    /// Discontinuity segment ([`Segments`](crate::interp::Segments)): not
    /// interpolated across a change.
    pub seg: u8,
}

/// How a ship looks beyond [`PlayerState`]; sent on change.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlayerLook {
    /// Whose ship.
    pub peer: PeerId,
    /// Sprite scale (`image_xscale`).
    pub xscale: f32,
    /// Sprite scale (`image_yscale`).
    pub yscale: f32,
    /// Blend colour (`image_blend`, GameMaker BGR).
    pub blend: u32,
    /// Opacity (`image_alpha`).
    pub alpha: f32,
    /// The ship is drawn at all (`visible`).
    pub visible: bool,
    /// Cloaked (`oPowers.cloaked`): enemies ignore it, draw it faint.
    pub cloaked: bool,
    /// Maximum hull, so `PlayerState::hull` can be shown as a fraction.
    pub hull_max: f32,
}

/// Quantization step for snapshot positions, in pixels.
pub const POS_STEP: f32 = 0.5;
/// Encoded size of one [`EnemyState`].
pub const ENEMY_STATE_BYTES: usize = 11;
/// Most enemies in one [`EnemySnapshot`] part (about 1000 bytes).
pub const ENEMIES_PER_PART: usize = 90;
/// No target ([`EnemyState::target`] on the wire).
const NO_TARGET: u8 = 0xFF;

/// One enemy in a snapshot. Quantized on the wire to 11 bytes:
///
/// ```text
/// net_id:u16  x:i16  y:i16   (POS_STEP px, so ±16383 px)
/// angle:u8    (360/256 deg)
/// hull:u16    (fraction of hull_max, 1/65535)
/// target:u8   (peer id, 0xFF = none)
/// seg:u8      (discontinuity segment)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(from = "[u8; ENEMY_STATE_BYTES]", into = "[u8; ENEMY_STATE_BYTES]")]
pub struct EnemyState {
    /// Which enemy.
    pub net_id: NetId,
    /// Position.
    pub x: f32,
    /// Position.
    pub y: f32,
    /// Facing, degrees in `[0, 360)`.
    pub angle: f32,
    /// `hull / hull_max`, at least 0. Above 1 when the hull rose past its
    /// spawn's base (`buildEnemyPart` scales a part's hull after its create).
    /// The fine snapshot carries it exactly, the coarse encoding clamps it to 1.
    pub hull_frac: f32,
    /// The peer this enemy is attacking (gates its local fire on that peer).
    pub target: Option<PeerId>,
    /// The host's discontinuity segment for this enemy
    /// ([`Segments`](crate::interp::Segments)): not interpolated across a change.
    pub seg: u8,
}

fn quant_pos(v: f32) -> i16 {
    let q = (v / POS_STEP).round();
    if q.is_nan() {
        0
    } else {
        q.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
    }
}

fn quant_angle(deg: f32) -> u8 {
    if !deg.is_finite() {
        return 0;
    }
    let turns = deg.rem_euclid(360.0) / 360.0;
    ((turns * 256.0).round() as u32 % 256) as u8
}

/// The most a hull carried as [`EnemyState::hull_frac`] (a 16-bit fraction
/// of `hull_max`) can be off once decoded: half a step, `hull_max / 131070`.
/// A hull of 31 with a max of 160 decodes as 30.99901.
#[must_use]
pub fn hull_frac_carry(hull_max: f32) -> f32 {
    hull_max.abs() * 0.5 / 65535.0
}

fn quant_frac(f: f32) -> u16 {
    if f.is_nan() {
        0
    } else {
        (f.clamp(0.0, 1.0) * 65535.0).round() as u16
    }
}

impl From<EnemyState> for [u8; ENEMY_STATE_BYTES] {
    fn from(e: EnemyState) -> Self {
        let mut b = [0u8; ENEMY_STATE_BYTES];
        b[0..2].copy_from_slice(&e.net_id.to_le_bytes());
        b[2..4].copy_from_slice(&quant_pos(e.x).to_le_bytes());
        b[4..6].copy_from_slice(&quant_pos(e.y).to_le_bytes());
        b[6] = quant_angle(e.angle);
        b[7..9].copy_from_slice(&quant_frac(e.hull_frac).to_le_bytes());
        b[9] = e.target.map_or(NO_TARGET, |p| p.0);
        b[10] = e.seg;
        b
    }
}

impl From<[u8; ENEMY_STATE_BYTES]> for EnemyState {
    fn from(b: [u8; ENEMY_STATE_BYTES]) -> Self {
        Self {
            net_id: u16::from_le_bytes([b[0], b[1]]),
            x: f32::from(i16::from_le_bytes([b[2], b[3]])) * POS_STEP,
            y: f32::from(i16::from_le_bytes([b[4], b[5]])) * POS_STEP,
            angle: f32::from(b[6]) * (360.0 / 256.0),
            hull_frac: f32::from(u16::from_le_bytes([b[7], b[8]])) / 65535.0,
            target: (b[9] != NO_TARGET).then_some(PeerId(b[9])),
            seg: b[10],
        }
    }
}

impl EnemyState {
    /// This state as the peer will decode it (quantized).
    #[must_use]
    pub fn quantized(self) -> Self {
        Self::from(<[u8; ENEMY_STATE_BYTES]>::from(self))
    }
}

/// One part of a host enemy snapshot (30 Hz, unreliable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnemySnapshot {
    /// Host clock when the snapshot was taken, ms.
    pub time_ms: u32,
    /// Index of this part.
    pub part: u8,
    /// Number of parts in this snapshot.
    pub parts: u8,
    /// The enemies in this part.
    pub enemies: Vec<EnemyState>,
}

impl EnemySnapshot {
    /// Split a full snapshot into parts that each fit one datagram. Always
    /// returns at least one part (an empty snapshot still advances time).
    #[must_use]
    pub fn split(time_ms: u32, enemies: &[EnemyState]) -> Vec<Self> {
        if enemies.is_empty() {
            return vec![Self {
                time_ms,
                part: 0,
                parts: 1,
                enemies: Vec::new(),
            }];
        }
        // Past 255 parts extra enemies are dropped.
        let chunks: Vec<_> = enemies.chunks(ENEMIES_PER_PART).take(255).collect();
        let parts = chunks.len() as u8;
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, c)| Self {
                time_ms,
                part: i as u8,
                parts,
                enemies: c.to_vec(),
            })
            .collect()
    }
}

/// A replicated enemy as the host's game set it up, sent at its first tick
/// (the rest of its state follows as `InstanceStateIx`). Snapshot hull
/// fractions are relative to [`EnemySpawn::hull_max`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EnemySpawn {
    /// Id used by later snapshots and the despawn.
    pub net_id: NetId,
    /// GameMaker object index (both peers run the same build).
    pub object: u16,
    /// Position.
    pub x: f32,
    /// Position.
    pub y: f32,
    /// Facing (`image_angle`), degrees.
    pub angle: f32,
    /// Hull now.
    pub hull: f32,
    /// Maximum hull (after the health multiplier, rarity and size), or the
    /// hull at the first spawn when above it. The base of the snapshot hull
    /// fractions, the same in every later spawn of the enemy. The instance's
    /// own `hullMax` goes with its state.
    pub hull_max: f32,
    /// [`EnemySpawn::DEPENDENCY`], [`EnemySpawn::NO_HULL`] (bits 1 and 2 are
    /// unused).
    pub flags: u8,
}

impl EnemySpawn {
    /// Flag: not an enemy but an instance a replicated enemy depends on (it
    /// refers to it): the client makes it like an enemy.
    pub const DEPENDENCY: u8 = 4;
    /// Flag: the host's instance has no `hull` variable; the client writes
    /// none (the game reads a hull of 0 as dead).
    pub const NO_HULL: u8 = 8;

    /// The host's instance has a `hull` ([`Self::NO_HULL`] unset).
    #[must_use]
    pub fn has_hull(&self) -> bool {
        self.flags & Self::NO_HULL == 0
    }

    /// A dependency ([`Self::DEPENDENCY`]).
    #[must_use]
    pub fn is_dependency(&self) -> bool {
        self.flags & Self::DEPENDENCY != 0
    }

    /// An enemy at full hull with no flags (tests).
    #[must_use]
    pub fn new(net_id: NetId, object: u16, x: f32, y: f32, angle: f32, hull_max: f32) -> Self {
        Self {
            net_id,
            object,
            x,
            y,
            angle,
            hull: hull_max,
            hull_max,
            flags: 0,
        }
    }
}

/// Why an object went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DespawnCause {
    /// Killed (by a player, or by the world when `None`).
    Killed {
        /// Who landed the killing blow.
        by: Option<PeerId>,
    },
    /// Removed without dying (left the arena, wave cleanup).
    Removed,
    /// Destroyed with its Destroy event without dying; the copy runs it too.
    Destroyed,
}

/// Every co-op message. Append new variants at the end only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// Version handshake (both directions).
    Hello(Hello),
    /// Host to a new client: the client's id and the session size.
    Welcome {
        /// The receiver's peer id.
        peer: PeerId,
        /// Number of peers including the host.
        peers: u8,
    },
    /// A ship's state (unreliable, 60 Hz).
    PlayerState(PlayerState),
    /// Part of a host enemy snapshot (unreliable, 30 Hz).
    EnemySnapshot(EnemySnapshot),
    /// Wire tag 4, retired: kept for the tags after it; never sent.
    #[doc(hidden)]
    Retired4,
    /// Enemy gone (host to clients).
    Despawn {
        /// Which enemy.
        net_id: NetId,
        /// Why.
        cause: DespawnCause,
    },
    /// A player's hit on an enemy, reported to the host.
    Damage {
        /// Which enemy.
        net_id: NetId,
        /// Hull removed.
        amount: f32,
        /// Shooter.
        by: PeerId,
    },
    /// An orb or power-up appeared (host to clients); the joining game makes
    /// its own copy.
    OrbSpawn {
        /// Orb id.
        orb_id: u16,
        /// Orb type (the co-op mod: an index into its list of collectables).
        kind: u8,
        /// Position.
        x: f32,
        /// Position.
        y: f32,
        /// XP value (the co-op mod sends 0: a pickup's value is the
        /// collector's build).
        value: f32,
        /// Velocity (`hspeed`, `vspeed`, px per step).
        hs: f32,
        vs: f32,
        /// Steps left (`lifespan`; negative: unknown).
        life: f32,
        /// The orb's `held` (a Medi Charge's: 1 from its Create, 0 once
        /// `spawnScatter` dropped it); `NaN`: the orb has none.
        held: f32,
        /// `spawnScatter`'s magnet factor (its sixth argument; 1 if none).
        scatter: f32,
    },
    /// Client to host: "I picked this up". Host to all: "this peer got it" (first wins).
    OrbCollect {
        /// Orb id.
        orb_id: u16,
        /// Collector.
        peer: PeerId,
    },
    /// Shared XP (host to clients): `oController.xp` and `global.baseLevel`.
    XpState {
        /// XP inside the current level.
        xp: f64,
        /// XP level reached.
        level: u16,
    },
    /// Pause request (to host) or confirmed pause (from host).
    Pause {
        /// Who paused.
        by: PeerId,
    },
    /// Resume request or confirmed resume (the host accepts it from any peer).
    Unpause {
        /// Whose pause ends.
        by: PeerId,
    },
    /// Upgrade screen opens for everyone (host to all, or a client's request).
    UpgradeOpen {
        /// Upgrade round (a counter), so simultaneous opens merge.
        round: u16,
    },
    /// A peer finished choosing (request to host, confirmed by host). The
    /// host accepts a request only for its sender.
    UpgradeDone {
        /// Who finished.
        peer: PeerId,
        /// Round finished.
        round: u16,
        /// Set on the host's relay that releases the round when the resume
        /// count runs ([`crate::session::UPGRADE_COUNT_AFTER`]).
        count: bool,
    },
    /// A peer was downed (report to host; host confirms with the timer).
    Downed {
        /// Who.
        peer: PeerId,
        /// Respawn countdown in ms (0 in a host confirmation: game over).
        respawn_ms: u32,
    },
    /// A downed peer respawns (host to all).
    Respawned {
        /// Who.
        peer: PeerId,
    },
    /// Wave progress (host to clients).
    WaveState {
        /// Wave number.
        wave: u16,
        /// Time until the next wave, ms.
        next_wave_ms: u32,
    },
    /// A peer is in a run and waiting at the start gate.
    RunStart {
        /// Who.
        peer: PeerId,
        /// Hash of game-mode flags; all peers must match.
        mode: u32,
    },
    /// Host: everyone is in, start now.
    Go {
        /// Host clock at the start, ms.
        start_ms: u32,
    },
    /// Host: a peer's game mode does not match; the run is refused.
    RunRefused {
        /// The mismatching peer.
        peer: PeerId,
    },
    /// How a ship looks (scale, colour, visibility), sent on change.
    PlayerLook(PlayerLook),
    /// A replicated enemy (host to clients, reliable).
    EnemySpawn(EnemySpawn),
    /// Host to clients: drop every replicated enemy; the full state follows.
    WorldResync {
        /// How many `EnemySpawn`s follow (for logs and checks).
        enemies: u16,
    },
    /// A player's co-op colour on other screens (`None` = untinted).
    PlayerColor {
        /// Whose colour.
        peer: PeerId,
        /// GameMaker colour (`0xBBGGRR`), or `None`.
        color: Option<u32>,
    },
    /// Host to clients after an [`Message::EnemySpawn`]: its random look.
    EnemyLook {
        /// The enemy.
        net_id: NetId,
        /// `image_index` (sprite subimage) at its first tick.
        image_index: f32,
    },
    /// Host to clients: a spawn telegraph (warning or portal), visual only.
    SpawnMarker(SpawnMarker),
    /// Wire tag 26. Host to all: every player is down, the run is over.
    GameOver,
    /// Wire tag 27, retired: kept for the tags after it; never sent.
    #[doc(hidden)]
    Retired27,
    /// Wire tag 28, retired: kept for the tags after it; never sent.
    #[doc(hidden)]
    Retired28,
    /// Tag 29. Host to clients: variable names of one object, appended at
    /// `base`, sent before the first state that uses them.
    StateNames(StateNames),
    /// Host to clients: [`InstanceState`] with variables by index
    /// into the object's [`Message::StateNames`] list. Tag 30.
    InstanceStateIx(InstanceStateIx),
    /// Tag 31. [`Message::EnemySnapshot`] with finer angles and positions;
    /// see [`Message::normalized`].
    EnemySnapshotFine(EnemySnapshotFine),
    /// Wire tag 32: the generic player-side mirror, moving parts (unreliable;
    /// see [`crate::mirror`]).
    MirrorFrame(crate::mirror::MirrorFrame),
    /// Wire tag 33: the generic mirror, rarely changing parts (reliable).
    MirrorLooks(crate::mirror::MirrorLooks),
    /// Wire tag 34: the generic mirror, what each instance follows (reliable;
    /// see [`crate::mirror::MirrorLeaders`]).
    MirrorLeaders(crate::mirror::MirrorLeaders),
    /// Wire tag 35: the mirror's own drawing ([`crate::mirror::MirrorDraw`]).
    MirrorDraw(crate::mirror::MirrorDraw),
    /// Wire tag 36. A non-owner's cumulative damage per enemy
    /// ([`crate::damage`]), repeated until acknowledged. Unreliable.
    DamageReport(DamageReport),
    /// Wire tag 37. The owner's answer to [`Message::DamageReport`]: what it
    /// has applied from that peer and the enemy's hull afterwards. Unreliable.
    DamageAck(DamageAck),
    /// Wire tag 38. A kill claim left the enemy with hull; an
    /// [`Message::EnemySpawn`] follows. Reliable.
    KillRefused {
        /// The enemy.
        net_id: NetId,
        /// The peer whose claim is refused.
        peer: PeerId,
        /// The owner's hull after applying everything that peer reported.
        hull: f32,
        /// The refused claim's number ([`DamageEntry::claim`]).
        claim: u16,
    },
    /// Wire tag 39: status effects on enemies ([`crate::status`]). Unreliable.
    Status(crate::status::StatusMsg),
    /// Wire tag 40. Reserved: never sent; ignored on receipt.
    ReportInfo(ReportInfo),
    /// Wire tag 41. Reserved: never sent; ignored on receipt.
    ReportMark(ReportMark),
    /// Wire tag 42. Reserved: never sent; ignored on receipt.
    ReportDigest(ReportDigest),
    /// Wire tag 43. Reserved: never sent; ignored on receipt.
    ReportTransfer(ReportTransfer),
    /// Wire tag 44. A ship's changing alpha, scale and blend, sent each frame
    /// while it changes and interpolated like the position. Unreliable.
    PlayerLookFrame(PlayerLookFrame),
    /// Wire tag 45. Host: world-level state (camera zoom, background colours)
    /// every run frame. Unreliable, newest wins.
    WorldView(WorldView),
    /// Wire tag 46. Host: a world-level script ran (`changeColors`); a
    /// joining game runs it with the same arguments. Reliable.
    WorldCall(WorldCall),
    /// Wire tag 47. Host: a replicated instance was deactivated or activated
    /// again; the joining game does the same to its copy.
    Asleep { net_id: NetId, asleep: bool },
    /// Wire tag 48. A joining game's inputs to the shared view
    /// (`oPowers.zoomAdditive`), sent on change.
    WorldInputs(WorldView),
    /// Wire tag 49. Host: the shared score (`global.playerScore` with its
    /// verifier and modulus, which the end screen checks) and the run's kill
    /// counters, on change and every 2 s. Reliable.
    ScoreState {
        score: f64,
        verifier: f64,
        modulus: f64,
        boss_kills: f64,
        star_eater_kills: f64,
    },
    /// Wire tag 50. A joining game's area effects on enemies (Temporal slow,
    /// Vortex pull); the host repeats the calls round its avatar.
    /// Unreliable, newest wins.
    AreaState(AreaState),
    /// Wire tag 51. Host: enemy shots born this frame (shared enemy shots,
    /// see [`crate::shots`]). Reliable.
    ShotFire(crate::shots::ShotFire),
    /// Wire tag 52. Shots that ended: host to clients, or a joining game's
    /// own player's ends to the host. Reliable.
    ShotGone(crate::shots::ShotGone),
    /// Wire tag 53. A shot changed, not ended (reserved: decoded, never sent). Reliable.
    ShotState(crate::shots::ShotState),
    /// Wire tag 54. Host: what shared shots steer at changed (a player or
    /// the world). Reliable.
    ShotSteer(crate::shots::ShotSteer),
    /// Wire tag 55. A joining game to the host: its own ship entered the
    /// Singularity (its copy of the host's black hole caught it). Reliable.
    SingularityEntered(SingularityEntry),
    /// Wire tag 56. Host to clients: a ship entered the Singularity, and every
    /// game catches its own ship as if it had entered too. Reliable.
    SingularityCatch(SingularityEntry),
    /// Wire tag 57. Host: what enemies and shared shots chase changed, where
    /// that is a player's thing (a construct, a mine, a decoy). Reliable.
    AimThings(crate::shots::AimThings),
    /// Wire tag 58. A player's beam cover for other ships (Shield Bastion,
    /// Arc Barrier), on change and every 2 s. Reliable.
    CoverState(CoverState),
    /// Wire tag 59. Host to a joining game on link: the session settings it
    /// uses instead of its own (multipliers, respawn seconds); `plays_on_s` is
    /// the host's fixed plays-on time, only logged. Reliable.
    SessionSettings {
        health_mult: f32,
        damage_mult: f32,
        plays_on_s: f32,
        respawn_s: f32,
    },
    /// Wire tag 60. A player left the co-op run while still linked. A
    /// receiver past its start gate ends its run too; one still at its gate
    /// takes it as the run before's and ignores it. Reliable.
    RunLeft { peer: PeerId },
    /// Wire tag 61. Host: its run settings for both games (`mods`: one bit
    /// per `gameMod*` flag; `daily`), on link and change (`start` false) and
    /// at its run start (`start` true); `on_loadout`: it is on its new game
    /// screen. Reliable.
    RunRules {
        start: bool,
        mods: u16,
        daily: Option<DailyPick>,
        on_loadout: bool,
    },
    /// Wire tag 62. Host: changed lit states of the Star Eater's segments
    /// and head (`redAlpha`, `whiteAlpha`, `whiteFlash`). Reliable.
    LitStates(LitStates),
    /// Wire tag 63. A player's facts for enemy targeting (`chooseTarget`),
    /// on change and every 2 s. Reliable.
    LureState(LureState),
    /// Wire tag 64, retired: kept for the tags after it; never sent.
    #[doc(hidden)]
    Retired64,
    /// Wire tag 65. The sender's session rules as a statement, 4 times a
    /// second; each side adopts what it lacks after a lasting difference.
    /// Unreliable, latest wins.
    RulesState(RulesState),
    /// Wire tag 66. Host to all: a shared pickup (Hyperboost, Nanocharge,
    /// Reroll) was credited to `by`; every other player gets its effect once.
    /// Reliable.
    PickupShared {
        /// The orb's id (as in `OrbSpawn`).
        orb_id: u16,
        /// Its kind (`OrbSpawn.kind`).
        kind: u8,
        /// Who picked it up.
        by: PeerId,
    },
    /// Wire tag 67. The game's on-enemy-death world effects (Rupture,
    /// Spontaneous Generation): which of them each game's player has, and the
    /// host's word that a death's effect is a joining player's to play. An
    /// effect is in force for every death if either player has it, never twice.
    /// Reliable, a `Given` before its enemy's `Despawn`.
    OnDeath(OnDeathMsg),
    /// Wire tag 68. The sender's player is ready on its new game screen
    /// (`ready` true: Start, which reads Ready while linked) or not. Both
    /// directions: a linked run starts once both players are ready. A new link
    /// starts unready on both sides. Reliable, in order with `RunRules`.
    Ready { ready: bool },
    /// Wire tag 69. `peer` is choosing again in `round` after it was done
    /// (Ataraxia, then its own upgrade screen with Space): the round waits for
    /// it. A request to the host, which relays it to all while that round is
    /// open. Reliable, counted with the rules messages.
    UpgradeChoosing {
        /// Who chooses again.
        peer: PeerId,
        /// The open round.
        round: u16,
    },
    /// Wire tag 70. The run's start count on the new game screen (3, 2, 1,
    /// go) once both players are ready. The host starts only after the joining
    /// game's `Ack` of its `Go`, so a Ready taken back on either side before
    /// its go stops the run on both (`coop::loadout`). `round` names the count.
    /// Reliable, in order with `Ready` and `RunRules`.
    StartCount {
        /// The count.
        round: u16,
        /// Its step.
        step: StartStep,
    },
}

/// See [`Message::StartCount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartStep {
    /// Host: both players are ready; the count begins now.
    Begin,
    /// Host: the count is called off (a Ready taken back, the screen left,
    /// no answer to its go): nobody starts.
    Cancel,
    /// Host: its count is over; it starts once the joining game answers.
    Go,
    /// Joining game: the host's go taken while ready. It follows the host's
    /// start and takes no Ready back.
    Ack,
}

/// See [`Message::OnDeath`]. A part is its index among the world effects of
/// the co-op mod's on-death table (`coop::damage::fx::ON_DEATH`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OnDeathMsg {
    /// The parts the sender's player has (bit `1 << part`), on a change and
    /// every 2 s.
    Has { peer: PeerId, parts: u8 },
    /// Host: `part` of `net_id`'s death is `by`'s, played by `by`'s game on
    /// its copy with its own build. `args`: the first `argc` arguments of
    /// the host's call. An argument the table marks as the player's own is
    /// read on the playing game.
    Given {
        net_id: NetId,
        by: PeerId,
        part: u8,
        argc: u8,
        args: [f32; 4],
    },
}

/// The host's daily challenge: `dailyChoice`, `dailyModChoice`,
/// `dailyModChoice2` and `dailyString` (at most [`DailyPick::NAME_MAX`] bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyPick {
    pub choice: i16,
    pub modifier: i16,
    pub modifier2: i16,
    pub name: String,
}

impl DailyPick {
    /// Longest name sent (bytes, cut at a character boundary).
    pub const NAME_MAX: usize = 64;
}

/// See [`Message::RulesState`]. One shape both ways; each field says whose
/// statement it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesState {
    /// The sender.
    pub peer: PeerId,
    /// Rules messages the sender sent on this link (an older statement is old).
    pub seq: u32,
    /// The same messages the sender received from the receiver.
    pub ack: u32,
    /// The sender's count of this link: a new one each time its side of the
    /// link starts over (never 0).
    pub link: u16,
    /// The receiver's `link` the sender last heard (0: none since its own
    /// start over): `seq` counts on that link.
    pub echo: u16,
    /// [`RulesState::GAME_OVER`] (host), [`RulesState::HOLD`] and
    /// [`RulesState::DOWN`] (the sender's own).
    pub flags: u8,
    /// Host: who is on their pause menu (a [`crate::session::PeerSet`] mask).
    pub holders: u32,
    /// Host: the open upgrade round. Joining game: the round its screen is
    /// on (the host's number, or its own proposal). 0: none.
    pub round: u16,
    /// Host: who is done with the open round (mask).
    pub done: u32,
    /// Joining game: the round it chose for (0: none).
    pub chosen: u16,
    /// Host: the last released round (0: none).
    pub released: u16,
    /// Host: the ships down, each with its respawn time left (ms; 0 at a
    /// game over).
    pub downs: Vec<(PeerId, u32)>,
}

impl RulesState {
    pub const GAME_OVER: u8 = 1;
    pub const HOLD: u8 = 2;
    pub const DOWN: u8 = 4;
    /// Host: the last released upgrade round ended with the resume count
    /// ([`Message::UpgradeDone::count`] in the statement).
    pub const COUNTED: u8 = 8;
}

/// See [`Message::LureState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LureState {
    pub peer: PeerId,
    /// `oPowers.decoySignal`: enemies take a closer ally over the ship.
    pub decoy_signal: bool,
    /// `oPowers.cloaked`: enemies do not take the ship.
    pub cloaked: bool,
    /// `oPowers.tacticalBeacon`: mines can be taken.
    pub tactical_beacon: bool,
}

/// See [`Message::LitStates`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LitStates {
    /// The host's clock when read (its End Step).
    pub time_ms: u32,
    pub items: Vec<LitItem>,
}

/// One instance's lit state: alphas in thousandths (0 to 65.535).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LitItem {
    pub net_id: NetId,
    pub red: u16,
    pub white: u16,
    pub flash: bool,
}

impl LitStates {
    /// At most this many items in one message (each about 9 bytes).
    pub const MAX_ITEMS: usize = 120;

    /// `items` in messages of at most [`Self::MAX_ITEMS`].
    #[must_use]
    pub fn split(time_ms: u32, items: &[LitItem]) -> Vec<Self> {
        items
            .chunks(Self::MAX_ITEMS)
            .map(|c| Self {
                time_ms,
                items: c.to_vec(),
            })
            .collect()
    }
}

/// A ship that entered the Singularity ([`Message::SingularityEntered`],
/// [`Message::SingularityCatch`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SingularityEntry {
    /// The player whose ship entered.
    pub by: PeerId,
    /// The black hole's centre on the sender's screen (the receiver closes
    /// the one nearest to it).
    pub x: f32,
    pub y: f32,
    /// What the entering ship's drain left of its hull and shield (each a
    /// fraction, 1 = nothing lost): the other ships lose the same share.
    pub hull: f32,
    pub shield: f32,
    /// The sender's network clock when it was caught, ms (a message older
    /// than the rule's limit on arrival is dropped).
    pub time_ms: u32,
}

/// See [`Message::CoverState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverState {
    pub peer: PeerId,
    /// `oPowers.shield > 0`.
    pub shield_up: bool,
    /// `oPowers.shieldedDrones` (the shield then guards constructs; the
    /// Bastion's beam cover is off).
    pub shielded_drones: bool,
    /// `oPowers.arcBarrier`.
    pub arc_barrier: bool,
}

/// See [`Message::AreaState`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AreaState {
    pub peer: PeerId,
    /// The sender's clock, ms.
    pub time_ms: u32,
    /// A hash of the names the values are for (the sender's list): a
    /// receiver with another list ignores them.
    pub names: u32,
    /// The values, in the list's order (`NaN`: unset).
    pub values: Vec<f32>,
    /// Where this frame's calls were made from (none: nothing acts).
    pub centers: Vec<AreaCenter>,
}

/// One call's centre, from the sender's ship (room pixels), and whether the
/// caller is of the kind the game gives its own multiplier (a turret).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AreaCenter {
    pub dx: f32,
    pub dy: f32,
    pub turret: bool,
    /// Which call: 0 the Temporal shield's slow (`applyTemporal`), 1 a
    /// Vortex shot's pull (`vortexGravity`).
    pub kind: u8,
    /// The caller's size the call reads (`image_xscale`; a Vortex's, folded
    /// for the number of Vortexes the sender had); 0 for a slow.
    pub scale: f32,
}

/// See [`Message::WorldView`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldView {
    /// The host's clock, ms.
    pub time_ms: u32,
    /// A hash of the names the values are for (the sender's list): a
    /// receiver with another list ignores them.
    pub names: u32,
    /// The values, in the list's order (`NaN`: unset).
    pub values: Vec<f64>,
}

/// See [`Message::WorldCall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldCall {
    /// The host's clock, ms.
    pub time_ms: u32,
    /// The script (`gml_Script_changeColors`).
    pub script: String,
    /// Its arguments (numbers, booleans, text, `undefined`).
    pub args: Vec<StateValue>,
}

/// Payload of [`Message::ReportInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportInfo {
    /// The sender.
    pub peer: PeerId,
    /// [`ReportInfo::HELLO`] or [`ReportInfo::BUILD`].
    pub kind: u8,
    /// `key=value` lines, at most [`ReportInfo::MAX_TEXT`] bytes.
    pub text: String,
}

impl ReportInfo {
    /// Kind 0.
    pub const HELLO: u8 = 0;
    /// Kind 1.
    pub const BUILD: u8 = 1;
    /// Longest `text`.
    pub const MAX_TEXT: usize = 900;
}

/// Payload of [`Message::ReportMark`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportMark {
    /// The sender.
    pub peer: PeerId,
    /// The mark's number.
    pub id: u16,
    /// [`ReportMark::KEY`], [`ReportMark::EVENT`] or [`ReportMark::NOTE`].
    pub kind: u8,
    /// The sender's clock (wire ms).
    pub time_ms: u32,
    /// Text, at most [`ReportMark::MAX_NOTE`] bytes.
    pub note: String,
}

impl ReportMark {
    pub const KEY: u8 = 0;
    pub const EVENT: u8 = 1;
    pub const NOTE: u8 = 2;
    pub const MAX_NOTE: usize = 120;
}

/// Payload of [`Message::ReportDigest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportDigest {
    /// The sender.
    pub peer: PeerId,
    /// The sender's clock (wire ms).
    pub time_ms: u32,
    /// `ReportDigest::IN_RUN | ...`.
    pub flags: u8,
    /// Wave number.
    pub wave: u16,
    /// Enemy count.
    pub enemies: u16,
    /// Order-free hash of the enemies' net ids.
    pub ids_hash: u32,
    /// A frame time (ms).
    pub frame_max_ms: u16,
}

impl ReportDigest {
    pub const IN_RUN: u8 = 1;
    pub const GAMEPLAY: u8 = 2;
    pub const PAUSED: u8 = 4;
    pub const GAME_OVER: u8 = 8;
    pub const DOWNED: u8 = 16;
    pub const UPGRADE: u8 = 32;
}

/// Payload of [`Message::ReportTransfer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportTransfer {
    /// File number `file` is `name` and holds `len` bytes.
    Offer {
        file: u8,
        epoch: u16,
        name: String,
        len: u32,
    },
    /// The first `have` bytes of `file` are held.
    Have { file: u8, epoch: u16, have: u32 },
    /// `raw_len` bytes of `file` from `offset`, raw-deflate compressed into
    /// `data` (at most [`ReportTransfer::MAX_DATA`] bytes).
    Chunk {
        file: u8,
        epoch: u16,
        offset: u32,
        raw_len: u32,
        data: Vec<u8>,
    },
    /// A request for the data.
    Request,
}

impl ReportTransfer {
    /// Largest compressed chunk.
    pub const MAX_DATA: usize = 1000;
}

/// See [`Message::PlayerLookFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlayerLookFrame {
    /// Whose ship.
    pub peer: PeerId,
    /// Owner's clock, ms (as [`PlayerState::time_ms`]).
    pub time_ms: u32,
    /// `image_xscale`.
    pub xscale: f32,
    /// `image_yscale`.
    pub yscale: f32,
    /// `image_blend` (BGR).
    pub blend: u32,
    /// `image_alpha` × 255.
    pub alpha: u8,
}

/// One enemy in a [`DamageReport`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DamageEntry {
    /// The enemy.
    pub net_id: NetId,
    /// Hull this peer has removed from it in total (since its spawn).
    pub total: f32,
    /// This peer's open kill claim number (0 none), raised per new claim and
    /// repeated until the owner answers it.
    pub claim: u16,
}

/// See [`Message::DamageReport`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DamageReport {
    /// The shooter.
    pub by: PeerId,
    /// At most [`DamageReport::MAX_ENTRIES`].
    pub entries: Vec<DamageEntry>,
}

impl DamageReport {
    /// Entries per message (10 bytes or less each).
    pub const MAX_ENTRIES: usize = 100;
}

/// One enemy in a [`DamageAck`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AckEntry {
    /// The enemy.
    pub net_id: NetId,
    /// Of the peer's reported total, how much the owner has applied.
    pub applied: f32,
    /// The owner's hull after that.
    pub hull: f32,
    /// The highest claim number the owner has answered (0 none).
    pub judged: u16,
}

/// See [`Message::DamageAck`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DamageAck {
    /// The peer whose reports are acknowledged.
    pub peer: PeerId,
    /// The owner's frame counter (newer acks replace older ones).
    pub frame: u32,
    /// At most [`DamageAck::MAX_ENTRIES`].
    pub entries: Vec<AckEntry>,
}

impl DamageAck {
    /// Entries per message (14 bytes or less each).
    pub const MAX_ENTRIES: usize = 75;
}

/// Position step of [`EnemySnapshotFine`], px (so +-8191 px).
pub const FINE_POS_STEP: f32 = 0.25;
/// Encoded size of one enemy in an [`EnemySnapshotFine`].
pub const FINE_STATE_BYTES: usize = 12;

/// One enemy of an [`EnemySnapshotFine`] on the wire:
///
/// ```text
/// net_id:u16  x:i16  y:i16   (FINE_POS_STEP px)
/// angle:u16   (360/65536 deg)
/// hull:u16    target:u8      seg:u8 (as in EnemyState)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(from = "[u8; FINE_STATE_BYTES]", into = "[u8; FINE_STATE_BYTES]")]
pub struct FineState(pub EnemyState);

impl From<FineState> for [u8; FINE_STATE_BYTES] {
    // The narrowing casts take clamped or wrapped values.
    fn from(f: FineState) -> Self {
        let e = f.0;
        let pos = |v: f32| -> i16 {
            let q = (v / FINE_POS_STEP).round();
            if q.is_nan() {
                0
            } else {
                q.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
            }
        };
        let angle = if e.angle.is_finite() {
            ((e.angle.rem_euclid(360.0) / 360.0 * 65536.0).round() as u32 % 65536) as u16
        } else {
            0
        };
        let mut b = [0u8; FINE_STATE_BYTES];
        b[0..2].copy_from_slice(&e.net_id.to_le_bytes());
        b[2..4].copy_from_slice(&pos(e.x).to_le_bytes());
        b[4..6].copy_from_slice(&pos(e.y).to_le_bytes());
        b[6..8].copy_from_slice(&angle.to_le_bytes());
        b[8..10].copy_from_slice(&quant_frac(e.hull_frac).to_le_bytes());
        b[10] = e.target.map_or(NO_TARGET, |p| p.0);
        b[11] = e.seg;
        b
    }
}

impl From<[u8; FINE_STATE_BYTES]> for FineState {
    fn from(b: [u8; FINE_STATE_BYTES]) -> Self {
        Self(EnemyState {
            net_id: u16::from_le_bytes([b[0], b[1]]),
            x: f32::from(i16::from_le_bytes([b[2], b[3]])) * FINE_POS_STEP,
            y: f32::from(i16::from_le_bytes([b[4], b[5]])) * FINE_POS_STEP,
            angle: f32::from(u16::from_le_bytes([b[6], b[7]])) * (360.0 / 65536.0),
            hull_frac: f32::from(u16::from_le_bytes([b[8], b[9]])) / 65535.0,
            target: (b[10] != NO_TARGET).then_some(PeerId(b[10])),
            seg: b[11],
        })
    }
}

/// An enemy of an [`EnemySnapshotFine`] whose `hull_frac` is over 1, which the
/// 16-bit fraction cannot hold: its fraction as an `f32`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HullOver {
    pub net_id: NetId,
    pub hull_frac: f32,
}

/// The bytes of one part's enemies at most. A [`HullOver`] takes room from them.
const FINE_PART_BYTES: usize = ENEMIES_PER_PART * FINE_STATE_BYTES;
/// A [`HullOver`] on the wire at most (a 3-byte varint and an `f32`).
const HULL_OVER_BYTES: usize = 7;

/// Whether a fraction is over 1 by more than the 16-bit fraction's half step.
fn hull_over(frac: f32) -> bool {
    frac > 1.0 + 0.5 / 65535.0
}

/// See [`Message::EnemySnapshotFine`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnemySnapshotFine {
    pub time_ms: u32,
    pub part: u8,
    pub parts: u8,
    pub enemies: Vec<FineState>,
    /// The enemies of this part whose hull is above their spawn's base, at
    /// full precision (their [`FineState`] holds 1).
    pub over: Vec<HullOver>,
}

impl EnemySnapshotFine {
    /// [`EnemySnapshot::split`] in the fine encoding. A part holds at most
    /// [`ENEMIES_PER_PART`] enemies, and no more bytes than that many take.
    #[must_use]
    pub fn split(time_ms: u32, enemies: &[EnemyState]) -> Vec<Self> {
        let mut chunks: Vec<(Vec<FineState>, Vec<HullOver>)> = Vec::new();
        let mut bytes = FINE_PART_BYTES;
        for e in enemies {
            let cost = FINE_STATE_BYTES
                + if hull_over(e.hull_frac) {
                    HULL_OVER_BYTES
                } else {
                    0
                };
            if bytes + cost > FINE_PART_BYTES
                || chunks.last().is_none_or(|c| c.0.len() >= ENEMIES_PER_PART)
            {
                if chunks.len() == 255 {
                    // Past 255 parts extra enemies are dropped (as in
                    // EnemySnapshot::split).
                    break;
                }
                chunks.push((Vec::new(), Vec::new()));
                bytes = 0;
            }
            bytes += cost;
            let Some(c) = chunks.last_mut() else { break };
            c.0.push(FineState(*e));
            if hull_over(e.hull_frac) {
                c.1.push(HullOver {
                    net_id: e.net_id,
                    hull_frac: e.hull_frac,
                });
            }
        }
        if chunks.is_empty() {
            chunks.push((Vec::new(), Vec::new()));
        }
        let parts = chunks.len() as u8;
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, (enemies, over))| Self {
                time_ms,
                part: i as u8,
                parts,
                enemies,
                over,
            })
            .collect()
    }

    /// The same snapshot as an [`EnemySnapshot`], values at full precision.
    #[must_use]
    pub fn into_snapshot(self) -> EnemySnapshot {
        let over = self.over;
        EnemySnapshot {
            time_ms: self.time_ms,
            part: self.part,
            parts: self.parts,
            enemies: self
                .enemies
                .into_iter()
                .map(|f| {
                    let mut e = f.0;
                    if let Some(o) = over.iter().find(|o| o.net_id == e.net_id) {
                        e.hull_frac = o.hull_frac;
                    }
                    e
                })
                .collect(),
        }
    }
}

impl Message {
    /// [`Message::EnemySnapshotFine`] as the [`Message::EnemySnapshot`] it
    /// stands for; anything else unchanged.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self {
            Self::EnemySnapshotFine(f) => Self::EnemySnapshot(f.into_snapshot()),
            other => other,
        }
    }
}

/// New variable names of one object (see [`Message::StateNames`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateNames {
    /// The object.
    pub object: u16,
    /// Index of the first name in the object's list.
    pub base: u16,
    pub names: Vec<String>,
}

impl StateNames {
    /// Split new names into datagram-sized messages.
    #[must_use]
    pub fn split(object: u16, base: u16, names: Vec<String>) -> Vec<Self> {
        let budget = MAX_MESSAGE - 32;
        let mut out = vec![Self {
            object,
            base,
            names: Vec::new(),
        }];
        let mut used = 16;
        let mut next = base;
        for n in names {
            let one = n.len() + 2;
            if used + one > budget {
                out.push(Self {
                    object,
                    base: next,
                    names: Vec::new(),
                });
                used = 16;
            }
            used += one;
            next = next.saturating_add(1);
            if let Some(m) = out.last_mut() {
                m.names.push(n);
            }
        }
        out
    }
}

/// A part of an instance's state with variables by index (see
/// [`Message::InstanceStateIx`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceStateIx {
    pub net_id: NetId,
    /// The object (its name list).
    pub object: u16,
    pub seq: u16,
    pub part: u8,
    pub parts: u8,
    /// Instance variables: index into the object's names, value.
    pub vars: Vec<(u16, StateValue)>,
    pub builtins: Vec<(u8, StateValue)>,
    pub alarms: Vec<(u8, i32)>,
}

impl InstanceStateIx {
    /// Split a state into parts within [`MAX_MESSAGE`]: alarms and built-ins
    /// first, the built-ins past the first part's room next, then the
    /// variables. A receiver takes a state's built-ins from all its parts. An
    /// oversized variable or built-in becomes `Unsupported(255)`.
    #[must_use]
    pub fn split(
        net_id: NetId,
        object: u16,
        seq: u16,
        vars: Vec<(u16, StateValue)>,
        builtins: Vec<(u8, StateValue)>,
        alarms: Vec<(u8, i32)>,
    ) -> Vec<Self> {
        let budget = MAX_MESSAGE - 32;
        let empty = |part: usize| Self {
            net_id,
            object,
            seq,
            part: u8::try_from(part).unwrap_or(u8::MAX),
            parts: 0,
            vars: Vec::new(),
            builtins: Vec::new(),
            alarms: Vec::new(),
        };
        let mut parts = vec![Self { alarms, ..empty(0) }];
        let mut used = postcard::to_allocvec(&Message::InstanceStateIx(parts[0].clone()))
            .map_or(usize::MAX, |b| b.len());
        for (i, value) in builtins {
            let one = postcard::to_allocvec(&(i, &value)).map_or(usize::MAX, |b| b.len());
            let (value, one) = if one > budget - 16 {
                (StateValue::Unsupported(255), 4)
            } else {
                (value, one)
            };
            if used + one > budget {
                parts.push(empty(parts.len()));
                used = 24;
            }
            used += one;
            if let Some(p) = parts.last_mut() {
                p.builtins.push((i, value));
            }
        }
        for (ix, value) in vars {
            let one = postcard::to_allocvec(&(ix, &value)).map_or(usize::MAX, |b| b.len());
            let (value, one) = if one > budget - 16 {
                (StateValue::Unsupported(255), 4)
            } else {
                (value, one)
            };
            if used + one > budget {
                parts.push(empty(parts.len()));
                used = 24;
            }
            used += one;
            if let Some(p) = parts.last_mut() {
                p.vars.push((ix, value));
            }
        }
        let n = u8::try_from(parts.len()).unwrap_or(u8::MAX);
        for p in &mut parts {
            p.parts = n;
        }
        parts
    }
}

impl StateValue {
    /// The same value in fewer bytes ([`Self::Whole`], [`Self::Real32`]; deep).
    #[must_use]
    #[allow(clippy::float_cmp)] // checked: whole, or exact in f32
    pub fn compact(self) -> Self {
        match self {
            Self::Real(x) if x.fract() == 0.0 && x.abs() < 2_147_483_648.0 => Self::Whole(x as i32),
            Self::Real(x) if f64::from(x as f32) == x => Self::Real32(x as f32),
            Self::Array(v) => Self::Array(v.into_iter().map(Self::compact).collect()),
            Self::Struct(v) => Self::Struct(v.into_iter().map(|(k, v)| (k, v.compact())).collect()),
            Self::Method { func, bound } => Self::Method {
                func,
                bound: Box::new(bound.compact()),
            },
            other => other,
        }
    }

    /// [`Self::compact`] undone: [`Self::Whole`] and [`Self::Real32`] as
    /// [`Self::Real`] (deep).
    #[must_use]
    pub fn expanded(self) -> Self {
        match self {
            Self::Whole(n) => Self::Real(f64::from(n)),
            Self::Real32(x) => Self::Real(f64::from(x)),
            Self::Array(v) => Self::Array(v.into_iter().map(Self::expanded).collect()),
            Self::Struct(v) => {
                Self::Struct(v.into_iter().map(|(k, v)| (k, v.expanded())).collect())
            }
            Self::Method { func, bound } => Self::Method {
                func,
                bound: Box::new(bound.expanded()),
            },
            other => other,
        }
    }
}

/// One variable's value in an [`InstanceState`], with no knowledge of the
/// object it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StateValue {
    /// `undefined`.
    Undefined,
    /// A number.
    Real(f64),
    /// An `int32`/`int64`.
    Int(i64),
    /// A boolean.
    Bool(bool),
    /// A string.
    Str(String),
    /// A ref that is not an instance (sprite, sound, object, ...): the raw
    /// payload, the same on both machines (same build).
    Ref(u64),
    /// Another replicated instance, by its net id.
    Instance(NetId),
    /// An instance that is not replicated but is the only instance of its
    /// object (the player, a controller): the receiver's own one.
    Singleton(u16),
    /// An instance that is neither (its object): `noone` on the receiver.
    Unresolved(u16),
    /// An array, element by element.
    Array(Vec<StateValue>),
    /// A struct, member by member.
    Struct(Vec<(String, StateValue)>),
    /// A method: the function's index and what it is bound to.
    Method {
        /// `method_get_index`.
        func: i64,
        /// `method_get_self` (an instance, a struct, or undefined).
        bound: Box<StateValue>,
    },
    /// Not transferred: the runner's type tag, or 255 when the value was too
    /// deep or too large.
    Unsupported(u8),
    /// A number that is whole (sent compact; see [`StateValue::compact`]).
    Whole(i32),
    /// A number an `f32` holds exactly (sent compact).
    Real32(f32),
}

/// A part of an instance's state (see [`Message::InstanceStateIx`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceState {
    /// Which instance.
    pub net_id: NetId,
    /// 0: at creation; then one more per repair.
    pub seq: u16,
    /// This part's index.
    pub part: u8,
    /// Number of parts of this `seq`.
    pub parts: u8,
    /// Instance variables by name.
    pub vars: Vec<(String, StateValue)>,
    /// Built-in variables: index into the receiver's list of names (the
    /// co-op mod's `BUILTINS`), value.
    pub builtins: Vec<(u8, StateValue)>,
    /// Alarms that are running: index, steps left.
    pub alarms: Vec<(u8, i32)>,
}

/// A spawn telegraph (see [`Message::SpawnMarker`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpawnMarker {
    /// [`SpawnMarker::WARNING`] (`oSpawnWarning`) or [`SpawnMarker::PORTAL`]
    /// (`oEnemyPortal`).
    pub kind: u8,
    /// Where the enemy will appear (a warning clamps itself to the screen).
    pub x: f32,
    pub y: f32,
    /// Warning: its size (`scaler`, the spawner's `enemyWarning`).
    pub scale: f32,
    /// Portal: the enemy object it announces (`enemyType`, sets its size).
    pub object: u16,
    /// Portal: the enemy's rarity (sets its colour).
    pub rarity: f32,
}

impl SpawnMarker {
    pub const WARNING: u8 = 0;
    pub const PORTAL: u8 = 1;
}

/// Encoding or decoding failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsgError {
    /// postcard error (malformed bytes, unknown variant, ...).
    Codec(String),
    /// Encoded message is larger than one datagram allows.
    TooLarge(usize),
}

impl fmt::Display for MsgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codec(e) => write!(f, "message codec error: {e}"),
            Self::TooLarge(n) => {
                write!(
                    f,
                    "message is {n} bytes, more than {MAX_MESSAGE} fit in a datagram"
                )
            }
        }
    }
}

impl std::error::Error for MsgError {}

impl Message {
    /// The channel this message must travel on.
    #[must_use]
    pub fn channel(&self) -> Channel {
        match self {
            Self::PlayerState(_)
            | Self::EnemySnapshot(_)
            | Self::EnemySnapshotFine(_)
            | Self::MirrorFrame(_)
            | Self::MirrorDraw(
                crate::mirror::MirrorDraw::Vars { .. }
                | crate::mirror::MirrorDraw::Ticks { .. }
                | crate::mirror::MirrorDraw::Edits { .. },
            )
            | Self::DamageReport(_)
            | Self::Status(_)
            | Self::DamageAck(_)
            | Self::PlayerLookFrame(_)
            | Self::WorldView(_)
            | Self::AreaState(_)
            | Self::ReportDigest(_)
            | Self::RulesState(_) => Channel::Unreliable,
            _ => Channel::Reliable,
        }
    }

    /// For unreliable messages, the stream they belong to: a newer packet on the
    /// same stream makes older arrivals stale.
    #[must_use]
    pub fn stream(&self) -> Option<u16> {
        match self {
            Self::PlayerState(p) => Some(0x100 | u16::from(p.peer.0)),
            Self::EnemySnapshot(s) => Some(0x200 | u16::from(s.part)),
            // The same stream as the coarse form: either supersedes the other.
            Self::EnemySnapshotFine(s) => Some(0x200 | u16::from(s.part)),
            Self::MirrorFrame(f) => Some(0x8000 | (u16::from(f.peer.0) << 8) | u16::from(f.part)),
            Self::MirrorDraw(crate::mirror::MirrorDraw::Vars { peer, part, .. }) => {
                Some(0xC000 | (u16::from(peer.0) << 8) | u16::from(*part))
            }
            // `Ticks` and `Edits` have no stream: the viewer orders them itself.
            Self::PlayerLookFrame(f) => Some(0x300 | u16::from(f.peer.0)),
            Self::WorldView(_) => Some(0x500),
            Self::AreaState(a) => Some(0x600 | u16::from(a.peer.0)),
            Self::ReportDigest(d) => Some(0x400 | u16::from(d.peer.0)),
            Self::RulesState(r) => Some(0x700 | u16::from(r.peer.0)),
            _ => None,
        }
    }

    /// Encode, refusing messages that cannot fit a datagram.
    pub fn encode(&self) -> Result<Vec<u8>, MsgError> {
        let bytes = postcard::to_allocvec(self).map_err(|e| MsgError::Codec(e.to_string()))?;
        if bytes.len() > MAX_MESSAGE {
            return Err(MsgError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    /// Decode one message (trailing bytes are an error).
    pub fn decode(bytes: &[u8]) -> Result<Self, MsgError> {
        let (msg, rest) =
            postcard::take_from_bytes(bytes).map_err(|e| MsgError::Codec(e.to_string()))?;
        if rest.is_empty() {
            Ok(msg)
        } else {
            Err(MsgError::Codec(format!("{} trailing bytes", rest.len())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enemy(i: u16) -> EnemyState {
        EnemyState {
            seg: 0,
            net_id: i,
            x: f32::from(i) * 3.25 - 100.0,
            y: 540.5,
            angle: f32::from(i % 360),
            hull_frac: 0.5,
            target: if i.is_multiple_of(2) {
                Some(PeerId(1))
            } else {
                None
            },
        }
    }

    #[allow(clippy::too_many_lines)] // one entry per message type
    fn all_messages() -> Vec<Message> {
        let p = PeerId(1);
        vec![
            Message::Asleep {
                net_id: 7,
                asleep: true,
            },
            Message::WorldInputs(WorldView {
                time_ms: 5,
                names: 9,
                values: vec![0.25],
            }),
            Message::ScoreState {
                score: 12_480.0,
                verifier: 7.0,
                modulus: 13.0,
                boss_kills: 3.0,
                star_eater_kills: 1.0,
            },
            Message::AreaState(AreaState {
                peer: p,
                time_ms: 11,
                names: 0x1234,
                values: vec![6.0, 1.0, 0.35],
                centers: vec![
                    AreaCenter {
                        dx: 0.0,
                        dy: 0.0,
                        turret: false,
                        kind: 0,
                        scale: 0.0,
                    },
                    AreaCenter {
                        dx: -40.5,
                        dy: 12.0,
                        turret: true,
                        kind: 1,
                        scale: 0.75,
                    },
                ],
            }),
            Message::ShotFire(crate::shots::ShotFire {
                shots: vec![crate::shots::FiredShot {
                    id: 70_000,
                    object: 762,
                    time_ms: 12_345,
                    vars: vec![(3, StateValue::Whole(5)), (4, StateValue::Instance(9))],
                    builtins: vec![(9, StateValue::Real(100.25))],
                    alarms: vec![(0, 12)],
                    target: Some(PeerId(1)),
                    steer: crate::shots::steer::PLAYER,
                    places: vec![(4, 402, 100.5, -20.0)],
                    place_fills: vec![(0, 7, StateValue::Real(55.0))],
                    replaces: true,
                    links: vec![(5, 70_001)],
                }],
            }),
            Message::ShotGone(crate::shots::ShotGone {
                ends: vec![crate::shots::ShotEnd {
                    id: 70_000,
                    time_ms: 12_400,
                    cause: crate::shots::cause::PLAYER,
                }],
            }),
            Message::ShotState(crate::shots::ShotState {
                id: 70_000,
                time_ms: 12_500,
                vars: vec![(3, StateValue::Real32(2.5))],
                builtins: Vec::new(),
                alarms: Vec::new(),
            }),
            Message::ShotSteer(crate::shots::ShotSteer {
                changes: vec![crate::shots::SteerChange {
                    id: 70_000,
                    time_ms: 12_600,
                    steer: crate::shots::steer::WORLD,
                }],
            }),
            Message::SingularityEntered(SingularityEntry {
                by: PeerId(1),
                x: 1200.5,
                y: -40.25,
                hull: 0.162,
                shield: 0.5,
                time_ms: 172_500,
            }),
            Message::SingularityCatch(SingularityEntry {
                by: PeerId::HOST,
                x: 10.0,
                y: 20.0,
                hull: 1.0,
                shield: 0.162,
                time_ms: 4,
            }),
            Message::CoverState(CoverState {
                peer: PeerId(1),
                shield_up: true,
                shielded_drones: false,
                arc_barrier: true,
            }),
            Message::LureState(LureState {
                peer: PeerId(1),
                decoy_signal: true,
                cloaked: false,
                tactical_beacon: false,
            }),
            Message::LitStates(LitStates {
                time_ms: 12_800,
                items: vec![LitItem {
                    net_id: 311,
                    red: 1000,
                    white: 250,
                    flash: true,
                }],
            }),
            Message::AimThings(crate::shots::AimThings {
                changes: vec![crate::shots::AimChange {
                    subject: crate::shots::AimSubject::Enemy(41),
                    time_ms: 12_700,
                    thing: Some(crate::shots::ThingRef {
                        owner: PeerId(0),
                        key: 513,
                    }),
                }],
            }),
            Message::SessionSettings {
                health_mult: 1.8,
                damage_mult: 1.0,
                plays_on_s: 15.0,
                respawn_s: 45.0,
            },
            Message::RunLeft { peer: p },
            Message::RunRules {
                start: true,
                mods: 0b0000_0100_0010,
                daily: Some(DailyPick {
                    choice: 7,
                    modifier: 3,
                    modifier2: -1,
                    name: "Boss Rush".into(),
                }),
                on_loadout: true,
            },
            Message::RunRules {
                start: false,
                mods: 0,
                daily: None,
                on_loadout: false,
            },
            Message::PickupShared {
                orb_id: 513,
                kind: 8,
                by: p,
            },
            Message::OnDeath(OnDeathMsg::Has { peer: p, parts: 3 }),
            Message::OnDeath(OnDeathMsg::Given {
                net_id: 65_000,
                by: p,
                part: 1,
                argc: 4,
                args: [12.0, -0.025, 0.25, 8.0],
            }),
            Message::Ready { ready: true },
            Message::Ready { ready: false },
            Message::StartCount {
                round: 7,
                step: StartStep::Begin,
            },
            Message::StartCount {
                round: 65_535,
                step: StartStep::Ack,
            },
            Message::RulesState(RulesState {
                peer: p,
                seq: 70_000,
                ack: 69_999,
                link: 65_000,
                echo: 3,
                flags: RulesState::HOLD | RulesState::DOWN,
                holders: 3,
                round: 4,
                done: 1,
                chosen: 4,
                released: 3,
                downs: vec![(p, 12_000), (PeerId::HOST, 0)],
            }),
            Message::MirrorLeaders(crate::mirror::MirrorLeaders {
                peer: p,
                leaders: vec![crate::mirror::MirrorLeader::new(3, Some(9), 1.5, -2.0)],
                time_ms: 77,
            }),
            Message::WorldView(WorldView {
                time_ms: 9,
                names: 0xABCD,
                values: vec![1.25, -1.0, 16_777_215.0],
            }),
            Message::WorldCall(WorldCall {
                time_ms: 9,
                script: "gml_Script_changeColors".into(),
                args: vec![
                    StateValue::Real(3.0),
                    StateValue::Bool(true),
                    StateValue::Undefined,
                ],
            }),
            Message::PlayerLookFrame(PlayerLookFrame {
                peer: p,
                time_ms: 7,
                xscale: 1.0,
                yscale: 1.0,
                blend: 0xFF_FFFF,
                alpha: 64,
            }),
            Message::Hello(Hello {
                mod_version: 3,
                build: "1.2.38.4".into(),
                name: "Sam".into(),
            }),
            Message::Welcome { peer: p, peers: 2 },
            Message::PlayerState(PlayerState {
                seg: 0,
                peer: p,
                time_ms: 99_999,
                x: 1.5,
                y: -2.25,
                vx: 100.0,
                vy: -3.0,
                angle: 271.0,
                sprite: 512,
                frame: 3,
                firing: true,
                hull: 77.5,
                downed: false,
            }),
            Message::EnemySnapshot(EnemySnapshot {
                time_ms: 1234,
                part: 1,
                parts: 2,
                enemies: (0..5).map(|i| enemy(i).quantized()).collect(),
            }),
            Message::Despawn {
                net_id: 7,
                cause: DespawnCause::Killed { by: Some(p) },
            },
            Message::Despawn {
                net_id: 8,
                cause: DespawnCause::Removed,
            },
            Message::Damage {
                net_id: 7,
                amount: 12.5,
                by: p,
            },
            Message::OrbSpawn {
                orb_id: 3,
                kind: 1,
                x: 5.0,
                y: 6.0,
                value: 2.0,
                hs: 1.5,
                vs: -0.5,
                life: 600.0,
                held: 0.0,
                scatter: 2.0,
            },
            Message::OrbCollect { orb_id: 3, peer: p },
            Message::DamageReport(DamageReport {
                by: p,
                entries: vec![
                    DamageEntry {
                        net_id: 7,
                        total: 120.5,
                        claim: 0,
                    },
                    DamageEntry {
                        net_id: 900,
                        total: 3.0,
                        claim: 2,
                    },
                ],
            }),
            Message::DamageAck(DamageAck {
                peer: p,
                frame: 77,
                entries: vec![AckEntry {
                    net_id: 7,
                    applied: 100.0,
                    hull: 20.5,
                    judged: 1,
                }],
            }),
            Message::KillRefused {
                net_id: 900,
                peer: p,
                hull: 4.0,
                claim: 2,
            },
            Message::XpState {
                xp: 1234.5,
                level: 9,
            },
            Message::Pause { by: p },
            Message::Unpause { by: p },
            Message::UpgradeOpen { round: 4 },
            Message::UpgradeDone {
                peer: p,
                round: 4,
                count: true,
            },
            Message::Downed {
                peer: p,
                respawn_ms: 45_000,
            },
            Message::Respawned { peer: p },
            Message::WaveState {
                wave: 12,
                next_wave_ms: 20_000,
            },
            Message::RunStart {
                peer: p,
                mode: 0xABCD,
            },
            Message::Go { start_ms: 777 },
            Message::RunRefused { peer: p },
            Message::PlayerLook(PlayerLook {
                peer: p,
                xscale: 1.25,
                yscale: -1.0,
                blend: 0x00FF_FFFF,
                alpha: 0.5,
                visible: true,
                cloaked: false,
                hull_max: 120.0,
            }),
            Message::EnemySpawn(EnemySpawn {
                net_id: 65_535,
                object: 0x325,
                x: -12.5,
                y: 4000.25,
                angle: 359.5,
                hull: 30.0,
                hull_max: 45.0,
                flags: EnemySpawn::DEPENDENCY,
            }),
            Message::WorldResync { enemies: 140 },
            Message::PlayerColor {
                peer: p,
                color: Some(0x00DC_3CFF),
            },
            Message::PlayerColor {
                peer: PeerId::HOST,
                color: None,
            },
            Message::EnemyLook {
                net_id: 900,
                image_index: 3.0,
            },
            Message::SpawnMarker(SpawnMarker {
                kind: SpawnMarker::PORTAL,
                x: -65.0,
                y: 1300.5,
                scale: 0.75,
                object: 0x325,
                rarity: 1.0,
            }),
            Message::GameOver,
        ]
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one assertion per wire tag
    fn variant_tags_are_append_only() {
        // The wire tag is the variant index: existing messages keep theirs.
        let tag = |m: &Message| m.encode().unwrap()[0];
        assert_eq!(
            tag(&Message::Welcome {
                peer: PeerId(1),
                peers: 2
            }),
            1
        );
        assert_eq!(
            tag(&Message::WaveState {
                wave: 1,
                next_wave_ms: 2
            }),
            16
        );
        let all = all_messages();
        let look = all
            .iter()
            .find(|m| matches!(m, Message::PlayerLook(_)))
            .unwrap();
        assert_eq!(tag(look), 20);
        let spawn = all
            .iter()
            .find(|m| matches!(m, Message::EnemySpawn(_)))
            .unwrap();
        assert_eq!(tag(spawn), 21);
        assert_eq!(tag(&Message::WorldResync { enemies: 0 }), 22);
        assert_eq!(
            tag(&Message::PlayerColor {
                peer: PeerId(1),
                color: None
            }),
            23
        );
        assert_eq!(
            tag(&Message::EnemyLook {
                net_id: 1,
                image_index: 2.0
            }),
            24
        );
        let marker = all
            .iter()
            .find(|m| matches!(m, Message::SpawnMarker(_)))
            .unwrap();
        assert_eq!(tag(marker), 25);
        assert_eq!(tag(&Message::Retired27), 27, "retired");
        assert_eq!(tag(&Message::Retired4), 4, "retired");
        assert_eq!(
            tag(&Message::Retired28),
            28,
            "the generic state by name, retired"
        );
        let names = Message::StateNames(StateNames {
            object: 1,
            base: 0,
            names: Vec::new(),
        });
        assert_eq!(tag(&names), 29);
        let ix = Message::InstanceStateIx(InstanceStateIx {
            net_id: 1,
            object: 1,
            seq: 0,
            part: 0,
            parts: 1,
            vars: Vec::new(),
            builtins: Vec::new(),
            alarms: Vec::new(),
        });
        assert_eq!(tag(&ix), 30);
        let fine = Message::EnemySnapshotFine(EnemySnapshotFine {
            time_ms: 0,
            part: 0,
            parts: 1,
            enemies: Vec::new(),
            over: Vec::new(),
        });
        assert_eq!(tag(&fine), 31);
        assert_eq!(
            tag(&Message::MirrorLooks(crate::mirror::MirrorLooks {
                peer: PeerId(0),
                looks: vec![]
            })),
            33
        );
        assert_eq!(
            tag(&Message::MirrorLeaders(crate::mirror::MirrorLeaders {
                peer: PeerId(0),
                leaders: vec![],
                time_ms: 0
            })),
            34
        );
        assert_eq!(tag(&Message::GameOver), 26);
        assert_eq!(tag(&Message::ReportTransfer(ReportTransfer::Request)), 43);
        assert_eq!(
            tag(&Message::WorldView(WorldView {
                time_ms: 0,
                names: 0,
                values: Vec::new(),
            })),
            45
        );
        assert_eq!(
            tag(&Message::PlayerLookFrame(PlayerLookFrame {
                peer: PeerId(1),
                time_ms: 0,
                xscale: 1.0,
                yscale: 1.0,
                blend: 0,
                alpha: 255
            })),
            44
        );
        assert_eq!(
            tag(&Message::MirrorDraw(crate::mirror::MirrorDraw::Drawn {
                peer: PeerId(0),
                objects: vec![]
            })),
            35
        );
        assert_eq!(
            tag(&Message::Asleep {
                net_id: 1,
                asleep: false
            }),
            47
        );
        assert_eq!(
            tag(&Message::WorldInputs(WorldView {
                time_ms: 0,
                names: 0,
                values: Vec::new()
            })),
            48
        );
        assert_eq!(
            tag(&Message::ScoreState {
                score: 0.0,
                verifier: 0.0,
                modulus: 13.0,
                boss_kills: 0.0,
                star_eater_kills: 0.0
            }),
            49
        );
        assert_eq!(
            tag(&Message::AreaState(AreaState {
                peer: PeerId(1),
                time_ms: 0,
                names: 0,
                values: Vec::new(),
                centers: Vec::new()
            })),
            50
        );
        assert_eq!(
            tag(&Message::ShotFire(crate::shots::ShotFire {
                shots: Vec::new()
            })),
            51
        );
        assert_eq!(
            tag(&Message::ShotGone(crate::shots::ShotGone {
                ends: Vec::new()
            })),
            52
        );
        assert_eq!(
            tag(&Message::ShotState(crate::shots::ShotState {
                id: 1,
                time_ms: 0,
                vars: Vec::new(),
                builtins: Vec::new(),
                alarms: Vec::new(),
            })),
            53
        );
        assert_eq!(
            tag(&Message::ShotSteer(crate::shots::ShotSteer {
                changes: Vec::new()
            })),
            54
        );
        let entry = SingularityEntry {
            by: PeerId(1),
            x: 0.0,
            y: 0.0,
            hull: 1.0,
            shield: 1.0,
            time_ms: 0,
        };
        assert_eq!(tag(&Message::SingularityEntered(entry)), 55);
        assert_eq!(tag(&Message::SingularityCatch(entry)), 56);
        assert_eq!(
            tag(&Message::AimThings(crate::shots::AimThings {
                changes: Vec::new()
            })),
            57
        );
        assert_eq!(
            tag(&Message::CoverState(CoverState {
                peer: PeerId(0),
                shield_up: false,
                shielded_drones: false,
                arc_barrier: false,
            })),
            58
        );
        assert_eq!(
            tag(&Message::SessionSettings {
                health_mult: 1.8,
                damage_mult: 1.0,
                plays_on_s: 0.0,
                respawn_s: 45.0,
            }),
            59
        );
        assert_eq!(tag(&Message::RunLeft { peer: PeerId(1) }), 60);
        assert_eq!(
            tag(&Message::RunRules {
                start: false,
                mods: 0,
                daily: None,
                on_loadout: false,
            }),
            61
        );
        assert_eq!(tag(&Message::Retired64), 64, "retired");
        let choosing = Message::UpgradeChoosing {
            peer: PeerId(1),
            round: 3,
        };
        assert_eq!(tag(&choosing), 69);
        assert_eq!(choosing.channel(), Channel::Reliable);
        assert_eq!(
            Message::decode(&choosing.encode().unwrap()).unwrap(),
            choosing
        );
        assert_eq!(
            tag(&Message::PickupShared {
                orb_id: 1,
                kind: 6,
                by: PeerId(1),
            }),
            66
        );
        assert_eq!(
            tag(&Message::OnDeath(OnDeathMsg::Has {
                peer: PeerId(1),
                parts: 1,
            })),
            67
        );
        assert_eq!(tag(&Message::Ready { ready: true }), 68);
        assert_eq!(Message::Ready { ready: false }.channel(), Channel::Reliable);
        for step in [
            StartStep::Begin,
            StartStep::Cancel,
            StartStep::Go,
            StartStep::Ack,
        ] {
            let m = Message::StartCount { round: 2, step };
            assert_eq!(tag(&m), 70);
            assert_eq!(m.channel(), Channel::Reliable);
            assert_eq!(Message::decode(&m.encode().unwrap()).unwrap(), m);
        }
        let rules = all.iter().find(|m| matches!(m, Message::RulesState(_)));
        assert_eq!(rules.map(&tag), Some(65));
        assert_eq!(
            tag(&Message::LureState(LureState {
                peer: PeerId(1),
                decoy_signal: false,
                cloaked: false,
                tactical_beacon: false,
            })),
            63
        );
        assert_eq!(
            tag(&Message::LitStates(LitStates {
                time_ms: 0,
                items: Vec::new(),
            })),
            62
        );
        let items: Vec<LitItem> = (0..250)
            .map(|n| LitItem {
                net_id: n,
                red: 0,
                white: 0,
                flash: false,
            })
            .collect();
        let parts = LitStates::split(5, &items);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.time_ms == 5));
        for p in parts {
            assert!(Message::LitStates(p).encode().is_ok());
        }
        let report = all
            .iter()
            .find(|m| matches!(m, Message::DamageReport(_)))
            .unwrap();
        assert_eq!(tag(report), 36, "the damage tags start at 36");
        let ack = all
            .iter()
            .find(|m| matches!(m, Message::DamageAck(_)))
            .unwrap();
        assert_eq!(tag(ack), 37);
        assert_eq!(
            tag(&Message::KillRefused {
                net_id: 1,
                peer: PeerId(1),
                hull: 1.0,
                claim: 1,
            }),
            38
        );
    }

    #[test]
    fn tags_39_to_43_fit() {
        let tag = |m: &Message| m.encode().unwrap()[0];
        assert_eq!(
            tag(&Message::Status(crate::status::StatusMsg::Report {
                by: PeerId(1),
                entries: Vec::new(),
            })),
            39
        );
        let info = Message::ReportInfo(ReportInfo {
            peer: PeerId(1),
            kind: ReportInfo::HELLO,
            text: "x".repeat(ReportInfo::MAX_TEXT),
        });
        assert_eq!(tag(&info), 40);
        let mark = Message::ReportMark(ReportMark {
            peer: PeerId(1),
            id: 7,
            kind: ReportMark::KEY,
            time_ms: u32::MAX,
            note: "n".repeat(ReportMark::MAX_NOTE),
        });
        assert_eq!(tag(&mark), 41);
        let digest = Message::ReportDigest(ReportDigest {
            peer: PeerId(1),
            time_ms: u32::MAX,
            flags: 0xFF,
            wave: u16::MAX,
            enemies: u16::MAX,
            ids_hash: u32::MAX,
            frame_max_ms: u16::MAX,
        });
        assert_eq!(tag(&digest), 42);
        assert_eq!(digest.channel(), Channel::Unreliable);
        assert_eq!(digest.stream(), Some(0x401), "not the look frames' 0x300");
        let chunk = Message::ReportTransfer(ReportTransfer::Chunk {
            file: 3,
            epoch: u16::MAX,
            offset: u32::MAX,
            raw_len: u32::MAX,
            data: vec![0xAB; ReportTransfer::MAX_DATA],
        });
        assert_eq!(tag(&chunk), 43);
        assert_eq!(chunk.channel(), Channel::Reliable);
        for m in [info, mark, digest, chunk] {
            assert_eq!(Message::decode(&m.encode().unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn full_damage_messages_fit_a_datagram() {
        let entries = (0..DamageReport::MAX_ENTRIES)
            .map(|i| DamageEntry {
                net_id: u16::MAX - i as u16,
                total: 1.0e6,
                claim: u16::MAX,
            })
            .collect();
        let r = Message::DamageReport(DamageReport {
            by: PeerId(3),
            entries,
        });
        assert!(r.encode().is_ok());
        let entries = (0..DamageAck::MAX_ENTRIES)
            .map(|i| AckEntry {
                net_id: u16::MAX - i as u16,
                applied: 1.0e6,
                hull: -5.0,
                judged: u16::MAX,
            })
            .collect();
        let a = Message::DamageAck(DamageAck {
            peer: PeerId(3),
            frame: u32::MAX,
            entries,
        });
        assert!(a.encode().is_ok());
        assert_eq!(r.channel(), Channel::Unreliable);
        assert_eq!(a.channel(), Channel::Unreliable);
    }

    #[test]
    fn fine_snapshots_keep_slow_turns() {
        let e = EnemyState {
            seg: 0,
            net_id: 9,
            x: 1000.3,
            y: -20.1,
            angle: 123.456,
            hull_frac: 0.5,
            target: None,
        };
        let m = Message::EnemySnapshotFine(EnemySnapshotFine::split(5, &[e]).remove(0));
        let bytes = m.encode().unwrap();
        let back = Message::decode(&bytes).unwrap().normalized();
        let Message::EnemySnapshot(s) = back else {
            unreachable!("normalized to EnemySnapshot")
        };
        let d = s.enemies[0];
        assert!((d.angle - 123.456).abs() < 0.006, "{}", d.angle);
        assert!((d.x - 1000.3).abs() <= 0.125 && (d.y + 20.1).abs() <= 0.125);
        // The coarse encoding moves this angle by up to 0.7 degrees.
        assert!((e.quantized().angle - 123.456).abs() > 0.1);
    }

    /// A hull five times its spawn's base crosses the wire exactly beside ones
    /// at or under the base. Every part fits a message, also with all of them
    /// above the base.
    #[test]
    fn a_hull_above_its_base_crosses_the_fine_snapshot_exactly() {
        let base = 400.0_f32;
        let at = |net_id: u16, hull: f32| EnemyState {
            hull_frac: hull / base,
            ..enemy(net_id)
        };
        let sent = [at(1, 2000.0), at(2, 200.0), at(3, base)];
        let parts = EnemySnapshotFine::split(5, &sent);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].over.len(), 1, "only the one above its base");
        let bytes = Message::EnemySnapshotFine(parts[0].clone())
            .encode()
            .unwrap();
        let Message::EnemySnapshot(s) = Message::decode(&bytes).unwrap().normalized() else {
            unreachable!("normalized to EnemySnapshot")
        };
        for (e, d) in sent.iter().zip(&s.enemies) {
            let (h, got) = (e.hull_frac * base, d.hull_frac * base);
            assert!(
                (h - got).abs() <= hull_frac_carry(base),
                "{h} decoded {got}"
            );
        }
        let many: Vec<_> = (0..1000).map(|i| at(i, 5.0 * base)).collect();
        let parts = EnemySnapshotFine::split(9, &many);
        let mut seen = 0;
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(
                (usize::from(p.part), usize::from(p.parts)),
                (i, parts.len())
            );
            assert_eq!(p.over.len(), p.enemies.len());
            let n = Message::EnemySnapshotFine(p.clone())
                .encode()
                .unwrap()
                .len();
            assert!(n <= MAX_MESSAGE, "part {i} is {n} bytes");
            seen += p.enemies.len();
        }
        assert_eq!(seen, 1000);
        assert_eq!(
            EnemySnapshotFine::split(1, &[]).len(),
            1,
            "an empty one still goes"
        );
    }

    #[test]
    fn compact_values_round_trip_and_shrink() {
        let v = StateValue::Array(vec![
            StateValue::Real(3.0),
            StateValue::Real(0.5),
            StateValue::Real(0.1),
            StateValue::Real(-7.0),
        ]);
        let c = v.clone().compact();
        assert_eq!(
            c,
            StateValue::Array(vec![
                StateValue::Whole(3),
                StateValue::Real32(0.5),
                StateValue::Real(0.1),
                StateValue::Whole(-7),
            ])
        );
        assert_eq!(c.clone().expanded(), v);
        let size = |v: &StateValue| postcard::to_allocvec(v).unwrap().len();
        assert!(size(&c) < size(&v));
        let parts = InstanceStateIx::split(
            3,
            9,
            0,
            (0..300)
                .map(|i| (i, StateValue::Whole(i32::from(i))))
                .collect(),
            Vec::new(),
            Vec::new(),
        );
        for p in &parts {
            let m = Message::InstanceStateIx(p.clone());
            assert_eq!(Message::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert_eq!(parts.iter().map(|p| p.vars.len()).sum::<usize>(), 300);
        // Built-ins past one datagram go on in the next parts, in order.
        let big = |i: u8| {
            (
                i,
                StateValue::Array(
                    (0..60)
                        .map(|k| StateValue::Real(f64::from(k) + 0.1))
                        .collect(),
                ),
            )
        };
        let builtins: Vec<(u8, StateValue)> = (0..6).map(big).collect();
        let parts = InstanceStateIx::split(3, 9, 0, Vec::new(), builtins.clone(), vec![(0, 5)]);
        assert!(parts.len() > 1, "{} parts", parts.len());
        for p in &parts {
            assert!(Message::InstanceStateIx(p.clone()).encode().is_ok());
            assert_eq!(usize::from(p.parts), parts.len());
        }
        let back: Vec<(u8, StateValue)> = parts.iter().flat_map(|p| p.builtins.clone()).collect();
        assert_eq!(back, builtins);
        assert_eq!(parts[0].alarms, vec![(0, 5)]);
        let names = StateNames::split(9, 4, (0..500).map(|i| format!("name{i}")).collect());
        assert!(names.len() > 1);
        assert_eq!(
            usize::from(names[1].base),
            4 + names[0].names.len(),
            "each part starts where the last ended"
        );
    }

    #[test]
    fn enemy_spawn_flags() {
        let mut s = EnemySpawn::new(3, 0xf8, 1.0, 2.0, 3.0, 40.0);
        assert!((s.hull - 40.0).abs() < f32::EPSILON);
        assert!(!s.is_dependency() && s.has_hull());
        s.flags |= EnemySpawn::DEPENDENCY;
        assert!(s.is_dependency());
        let back = Message::decode(&Message::EnemySpawn(s).encode().unwrap()).unwrap();
        assert!(matches!(back, Message::EnemySpawn(b) if b.is_dependency()));
        // Small enough to send dozens in one frame at a wave start.
        let n = Message::EnemySpawn(s).encode().unwrap().len();
        assert!(n <= 32, "{n} bytes");
    }

    #[test]
    fn every_message_round_trips() {
        for m in all_messages() {
            let bytes = m.encode().unwrap();
            assert_eq!(Message::decode(&bytes).unwrap(), m, "{bytes:?}");
        }
    }

    #[test]
    fn decode_rejects_garbage_without_panicking() {
        assert!(Message::decode(&[]).is_err());
        assert!(Message::decode(&[200]).is_err(), "unknown variant");
        let mut b = Message::Pause { by: PeerId(1) }.encode().unwrap();
        b.push(0);
        assert!(Message::decode(&b).is_err(), "trailing byte");
        let b = Message::XpState { xp: 1.0, level: 1 }.encode().unwrap();
        assert!(Message::decode(&b[..b.len() - 1]).is_err(), "truncated");
    }

    #[test]
    fn enemy_state_quantization_is_eleven_bytes_and_close() {
        let e = EnemyState {
            seg: 0,
            net_id: 513,
            x: 1234.3,
            y: -77.8,
            angle: 359.0,
            hull_frac: 0.333,
            target: Some(PeerId(2)),
        };
        let bytes = postcard::to_allocvec(&e).unwrap();
        assert_eq!(bytes.len(), ENEMY_STATE_BYTES);
        let q: EnemyState = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(q.net_id, 513);
        assert!((q.x - e.x).abs() <= POS_STEP / 2.0);
        assert!((q.y - e.y).abs() <= POS_STEP / 2.0);
        // 359 deg is within half a step (0.7 deg).
        assert!((q.angle - 359.0).abs() < 0.71, "{}", q.angle);
        assert!((q.hull_frac - e.hull_frac).abs() < 1e-4);
        assert_eq!(q.target, Some(PeerId(2)));
        // Out of range and NaN inputs clamp instead of wrapping.
        let wild = EnemyState {
            x: 1e9,
            y: f32::NAN,
            angle: -90.0,
            hull_frac: 7.0,
            target: None,
            ..e
        }
        .quantized();
        assert!(wild.x > 16_000.0);
        assert!(wild.y.abs() < f32::EPSILON);
        assert!((wild.angle - 270.0).abs() < 1.5);
        assert!((wild.hull_frac - 1.0).abs() < f32::EPSILON);
        assert_eq!(wild.target, None);
    }

    #[test]
    fn a_hull_carried_as_a_fraction_is_off_by_at_most_half_a_step() {
        // 31 of 160 and 13 of 80 decode as 30.99901 and 12.99947: within half
        // a step under the host's.
        for (hull, max, shown) in [(31.0_f32, 160.0_f32, 30.99901_f32), (13.0, 80.0, 12.99947)] {
            let q = EnemyState {
                seg: 0,
                net_id: 1,
                x: 0.0,
                y: 0.0,
                angle: 0.0,
                hull_frac: hull / max,
                target: None,
            }
            .quantized();
            let decoded = q.hull_frac * max;
            assert!((decoded - shown).abs() < 1e-4, "{decoded}");
            assert!(hull - decoded > 0.0);
            assert!(
                hull - decoded <= hull_frac_carry(max),
                "{} > {}",
                hull - decoded,
                hull_frac_carry(max)
            );
        }
    }

    #[test]
    fn snapshot_split_respects_the_size_cap() {
        let enemies: Vec<_> = (0..1000).map(enemy).collect();
        let parts = EnemySnapshot::split(4242, &enemies);
        assert_eq!(parts.len(), 1000_usize.div_ceil(ENEMIES_PER_PART));
        let mut seen = 0;
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(usize::from(p.part), i);
            assert_eq!(usize::from(p.parts), parts.len());
            let m = Message::EnemySnapshot(p.clone());
            let n = m.encode().unwrap().len();
            assert!(n <= MAX_MESSAGE, "part {i} is {n} bytes");
            // About 11 bytes per enemy plus a small header.
            assert!(n <= p.enemies.len() * ENEMY_STATE_BYTES + 12, "{n}");
            seen += p.enemies.len();
        }
        assert_eq!(seen, 1000);
        // 150 enemies (a heavy wave) is two datagrams, about 1.65 KB.
        let total: usize = EnemySnapshot::split(1, &enemies[..150])
            .into_iter()
            .map(|p| Message::EnemySnapshot(p).encode().unwrap().len())
            .sum();
        assert!((1650..1680).contains(&total), "{total}");
        let empty = EnemySnapshot::split(9, &[]);
        assert_eq!(empty.len(), 1);
        assert!(empty[0].enemies.is_empty());
    }

    #[test]
    fn channels_and_streams() {
        for m in all_messages() {
            let streamed = matches!(
                m,
                Message::PlayerState(_)
                    | Message::EnemySnapshot(_)
                    | Message::EnemySnapshotFine(_)
                    | Message::MirrorFrame(_)
                    | Message::PlayerLookFrame(_)
                    | Message::WorldView(_)
                    | Message::AreaState(_)
                    | Message::RulesState(_)
            );
            // Damage reports and acks are cumulative: any order will do.
            let unreliable =
                streamed || matches!(m, Message::DamageReport(_) | Message::DamageAck(_));
            assert_eq!(m.channel() == Channel::Unreliable, unreliable);
            assert_eq!(m.stream().is_some(), streamed);
        }
        assert_eq!(PeerId::HOST.to_string(), "P1");
    }

    #[test]
    fn player_state_is_small() {
        // 60 Hz per player: keep it under 40 bytes even with a large clock.
        let ps = PlayerState {
            seg: 0,
            peer: PeerId(3),
            time_ms: u32::MAX,
            x: 1.0,
            y: 2.0,
            vx: 3.0,
            vy: 4.0,
            angle: 5.0,
            sprite: u16::MAX,
            frame: 255,
            firing: true,
            hull: 6.0,
            downed: true,
        };
        let n = Message::PlayerState(ps).encode().unwrap().len();
        assert!(n <= 40, "{n} bytes");
    }
}
