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

//! Area effects: what a ship does each frame to the enemies around it. They
//! are made on the host round the player's avatar, with that player's
//! values, by the game's own code.
//!
//! * Temporal shield (`applyTemporal`, from the ship or its shielded
//!   riders): eases `enemySpeed` of everything in a circle. The joining side
//!   sends its calls' centres and its [`VALUES`]; the host runs the script as
//!   the avatar at each centre with those values on `oPowers`
//!   ([`call_values`]).
//! * Vortex pull (`vortexGravity`, `vortexGravity2`): moves every `oSolid`
//!   toward the shot. The joining side skips its own (its enemies are the
//!   host's poses) and sends the shot's place and size; the host runs
//!   `vortexGravity` as the avatar. A released Vortex's division by the
//!   Vortex count is folded into the size (`image_xscale / sqrt(n)`).

#![forbid(unsafe_code)]

use ndml_net::msg::{AreaCenter, AreaState};

/// The script.
pub const APPLY_TEMPORAL: &str = "gml_Script_applyTemporal";
/// The charging Vortex's pull (no count).
pub const VORTEX_GRAVITY: &str = "gml_Script_vortexGravity";
/// The released Vortex's pull (over `instance_number(oVortex)`).
pub const VORTEX_GRAVITY2: &str = "gml_Script_vortexGravity2";
/// [`AreaCenter::kind`]: the Temporal shield's slow.
pub const KIND_TEMPORAL: u8 = 0;
/// [`AreaCenter::kind`]: a Vortex's pull (`vortexGravity` on the host).
pub const KIND_VORTEX: u8 = 1;

/// What a write listed in [`SCAN`] touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    /// Enemies (or enemy shots) round the caller: the rule's family, made on
    /// the host round the avatar.
    Enemies,
    /// The player's own objects, its ship, `oPowers` or its own effects:
    /// each game's own (the reason).
    Own(&'static str),
}

/// Every function the ship's, riders', shields' and `oPowers`' Steps reach
/// (four calls deep) that writes the instance a `with` block runs on, and
/// what it writes.
pub const SCAN: &[(&str, Writes)] = &[
    ("gml_Script_applyTemporal", Writes::Enemies),
    (
        "gml_Object_oBlade_Step_2",
        Writes::Own(
            "its own damaging burst (`spawnBurstGrowDamagingNoExclusion`; hits are the damage channel's)",
        ),
    ),
    (
        "gml_Object_oPowers_Step_2",
        Writes::Own("the ship's own look (`with (oPlayer)`)"),
    ),
    (
        "gml_Object_oShieldSiphon_Step_0",
        Writes::Own("its own `oSiphonParticle` effects"),
    ),
    (
        "gml_Script_chargeWeapons",
        Writes::Own("`chargeTime` of the ship and `oPowers`"),
    ),
    (
        "gml_Script_damageReduction",
        Writes::Own("the player's own constructs' and swords' `multDamageReduction`"),
    ),
    (
        "gml_Script_droneBattalionPositions",
        Writes::Own("the player's own drones' formation"),
    ),
    (
        "gml_Script_droneOrbitPositions",
        Writes::Own("the player's own drones' orbit"),
    ),
    (
        "gml_Script_droneRampartPositions",
        Writes::Own("the player's own drones' formation"),
    ),
    (
        "gml_Script_promoteCommander",
        Writes::Own("the player's own drones' `isCommander`"),
    ),
    (
        "gml_Script_ricochetShotBlade",
        Writes::Own("a shot it makes (`instance_create`)"),
    ),
    (
        "gml_Script_shoot3Tracking",
        Writes::Own("a shot it makes (`instance_create`)"),
    ),
    (
        "gml_Script_spawnArmorStack",
        Writes::Own("an armour stack it makes (pooled)"),
    ),
    (
        "gml_Script_spawnExplosionSlags",
        Writes::Own("slag effects it makes (pooled)"),
    ),
    (
        "gml_Script_spawnFireBurst",
        Writes::Own("a fire burst it makes; its hits are the damage channel's"),
    ),
    (
        "gml_Script_splinterShotBlade",
        Writes::Own("a shot it makes (`instance_create`)"),
    ),
    (
        "gml_Script_destroyConstruct",
        Writes::Own("the player's own constructs and their debris"),
    ),
    (
        "gml_Script_hitCounterPulse",
        Writes::Own("its own damaging burst (`spawnBurstGrowDamaging`)"),
    ),
    (
        "gml_Script_logActivity",
        Writes::Own("the debug log (`oLog`)"),
    ),
    (
        "gml_Script_mediChargeReduceSelfDamage",
        Writes::Own("the player's own allies' hull"),
    ),
    (
        "gml_Script_recenterAllSwordsOnWielder",
        Writes::Own("the player's own swords"),
    ),
    (
        "gml_Script_refreshDrones",
        Writes::Own("the player's own constructs, blades, swords, segments"),
    ),
    (
        "gml_Script_setWarpathBonus",
        Writes::Own("`oPowers`' Warpath stats"),
    ),
    (
        "gml_Script_shareDamage",
        Writes::Own("the player's own super construct's hull"),
    ),
    (
        "gml_Script_triggerSelfDestruction",
        Writes::Own("its own fire burst (`spawnFireBurst`)"),
    ),
    (
        "gml_Script_triggerShieldSoft",
        Writes::Own("`oPowers.shieldCooldownTimer`"),
    ),
    (
        "gml_Script_playerThrust",
        Writes::Own("the ship's own thruster effects"),
    ),
    (
        "gml_Script_pressBlink",
        Writes::Own("the ship's own blink: its reticle, trails and drone trails"),
    ),
    (
        "gml_Script_wrapPlayer",
        Writes::Own("the ship's own followers moved with it at the arena's edge"),
    ),
];

/// The `oPowers` variables `applyTemporal` reads (and its helper for the
/// radius), then the one the Vortex's pull reads, in the wire's order.
pub const VALUES: [&str; 14] = [
    "shieldType",
    "shield",
    "sanctuary",
    "sanctuaryShieldEnable",
    "constructEffectMult",
    "turretPointDefenseMult",
    "sanctuaryRadius",
    "temporalRadius",
    "radius",
    "slowValue",
    "effect",
    "temporalSolidMod",
    "temporalMod",
    // The Vortex's pull (push when set).
    "polarInversion",
];
/// Index of `shieldType` in [`VALUES`].
pub const SHIELD_TYPE: usize = 0;
/// Index of `polarInversion` in [`VALUES`].
pub const POLAR_INVERSION: usize = 13;
const SANCTUARY: usize = 2;
const SANCTUARY_ENABLE: usize = 3;
const CONSTRUCT_MULT: usize = 4;
const TURRET_MULT: usize = 5;
const SANCTUARY_RADIUS: usize = 6;
const TEMPORAL_RADIUS: usize = 7;
const RADIUS: usize = 8;

/// `shieldType` of the Temporal shield: `applyTemporal` does nothing for
/// another (`shieldType == 6`, then `shield > 0`).
pub const TEMPORAL: f32 = 6.0;
/// A joining game sends its state at least this often while it acts
/// (frames), and the host forgets one older than [`EXPIRE_FRAMES`].
pub const RESEND_FRAMES: u64 = 10;
/// A changed state (riders orbiting, a value) is sent at most this often
/// (frames), to bound the traffic. The stop goes at once.
pub const CENTERS_EVERY: u64 = 3;
/// The host forgets a partner's state older than this (frames).
pub const EXPIRE_FRAMES: u64 = 45;

/// A hash of [`VALUES`] (both sides must use the same list).
#[must_use]
pub fn names_hash() -> u32 {
    VALUES.iter().fold(0x811C_9DC5u32, |h, n| {
        n.bytes()
            .chain([0])
            .fold(h, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
    })
}

/// One watched call: where the caller was, whether it is the ship, a
/// turret; which call ([`KIND_TEMPORAL`], [`KIND_VORTEX`]) and, for a
/// Vortex, its `image_xscale` and whether it is a released one (whose pull
/// the game divides by the number of Vortexes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seen {
    /// The caller's x.
    pub x: f64,
    /// The caller's y.
    pub y: f64,
    /// The caller is the ship.
    pub ship: bool,
    /// The caller is a turret.
    pub turret: bool,
    /// [`KIND_TEMPORAL`] or [`KIND_VORTEX`].
    pub kind: u8,
    /// A Vortex's `image_xscale`.
    pub scale: f32,
    /// A released Vortex.
    pub released: bool,
}

impl Seen {
    /// A Temporal call.
    #[must_use]
    pub fn temporal(x: f64, y: f64, ship: bool, turret: bool) -> Self {
        Self {
            x,
            y,
            ship,
            turret,
            kind: KIND_TEMPORAL,
            scale: 0.0,
            released: false,
        }
    }

    /// A Vortex's pull.
    #[must_use]
    pub fn vortex(x: f64, y: f64, scale: f32, released: bool) -> Self {
        Self {
            x,
            y,
            ship: false,
            turret: false,
            kind: KIND_VORTEX,
            scale,
            released,
        }
    }
}

/// The centres of the calls watched since the last tick, from the ship at
/// `ship` (none without a ship). Temporal calls only while `temporal` (the
/// game's call does nothing otherwise): the ship's own call first, at most
/// one, then the riders' (with Shielded Constructs only the riders call).
/// With Sanctuary on, every Temporal call is made round `sanctuary` (the
/// `oSanctuaryController`'s place). Then each Vortex's pull at its place, a
/// released one's size divided by the square root of the released Vortexes
/// seen (the game divides `sqr(41 * image_xscale)` by their number).
#[must_use]
pub fn centers(
    seen: &[Seen],
    ship: Option<(f64, f64)>,
    sanctuary: Option<(f64, f64)>,
    temporal: bool,
) -> Vec<AreaCenter> {
    let Some((sx, sy)) = ship else {
        return Vec::new();
    };
    let at = |x: f64, y: f64, turret: bool, kind: u8, scale: f32| AreaCenter {
        dx: (x - sx).round() as f32,
        dy: (y - sy).round() as f32,
        turret,
        kind,
        scale,
    };
    let slow = |s: &Seen| {
        let (x, y) = sanctuary.unwrap_or((s.x, s.y));
        at(x, y, s.turret, KIND_TEMPORAL, 0.0)
    };
    let mut out = Vec::new();
    if temporal {
        let slows = || seen.iter().filter(|s| s.kind == KIND_TEMPORAL);
        out.extend(slows().rev().find(|s| s.ship).map(slow));
        out.extend(slows().filter(|s| !s.ship).map(slow));
    }
    let vortexes = || seen.iter().filter(|s| s.kind == KIND_VORTEX);
    let released = vortexes().filter(|s| s.released).count();
    let fold = if released > 1 {
        // A count, at most the 256 calls a frame keeps: exact in f32.
        f32::from(u16::try_from(released).unwrap_or(u16::MAX)).sqrt()
    } else {
        1.0
    };
    out.extend(vortexes().map(|s| {
        let scale = if s.released { s.scale / fold } else { s.scale };
        at(s.x, s.y, false, KIND_VORTEX, scale)
    }));
    out
}

/// Joining side: when to send.
#[derive(Debug, Default)]
pub struct AreaSend {
    last: Option<(Vec<f32>, Vec<AreaCenter>, u64)>,
}

impl AreaSend {
    /// This tick's state, if it is to be sent: while there are centres
    /// (the Temporal shield's calls, a Vortex), when it changed or every
    /// [`RESEND_FRAMES`]; once with no centres when it stops.
    pub fn due(
        &mut self,
        values: Vec<f32>,
        centers: Vec<AreaCenter>,
        frame: u64,
    ) -> Option<(Vec<f32>, Vec<AreaCenter>)> {
        let acts = !centers.is_empty();
        if !acts {
            // Stopped: say so once.
            return match self.last.take() {
                Some((_, c, _)) if !c.is_empty() => Some((values, Vec::new())),
                _ => None,
            };
        }
        let send = self.last.as_ref().is_none_or(|(v, c, at)| {
            let age = frame.saturating_sub(*at);
            let changed = !same(v, &values) || *c != centers;
            (changed && age >= CENTERS_EVERY) || age >= RESEND_FRAMES
        });
        if !send {
            return None;
        }
        self.last = Some((values.clone(), centers.clone(), frame));
        Some((values, centers))
    }

    /// Forget the last state sent.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

fn same(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()))
}

/// Host: the newest state of each partner.
#[derive(Debug, Default)]
pub struct AreaRecv {
    states: Vec<(AreaState, u64)>,
}

impl AreaRecv {
    /// Keep the newest state per peer (one with another value list is
    /// ignored).
    pub fn take(&mut self, s: AreaState, frame: u64) {
        if s.names != names_hash() || s.values.len() != VALUES.len() {
            return;
        }
        self.states.retain(|(o, _)| o.peer != s.peer);
        self.states.push((s, frame));
    }

    /// The states that act this frame (older ones forgotten).
    pub fn live(&mut self, frame: u64) -> Vec<AreaState> {
        self.states
            .retain(|(_, at)| frame.saturating_sub(*at) <= EXPIRE_FRAMES);
        self.states
            .iter()
            .filter(|(s, _)| !s.centers.is_empty())
            .map(|(s, _)| s.clone())
            .collect()
    }

    /// Forget every state.
    pub fn clear(&mut self) {
        self.states.clear();
    }
}

/// Host: the values to put on `oPowers` for a Vortex's pull (its
/// `polarInversion`), and the avatar's `image_xscale` (the shot's, folded
/// for the sender's count); `None` without them or for a size of 0 or less
/// (the game's pull would be nothing).
#[must_use]
pub fn vortex_values(values: &[f32], c: &AreaCenter) -> Option<(Vec<(usize, f64)>, f64)> {
    if values.len() != VALUES.len() || c.kind != KIND_VORTEX {
        return None;
    }
    let polar = *values.get(POLAR_INVERSION)?;
    if polar.is_nan() || !c.scale.is_finite() || c.scale <= 0.0 {
        return None;
    }
    Some((
        vec![(POLAR_INVERSION, f64::from(polar))],
        f64::from(c.scale),
    ))
}

/// Host: the values to put on `oPowers` (index into [`VALUES`], value) for
/// one call round the avatar, or `None` when the call would do nothing on
/// the partner's game or a value is missing there (`NaN`, never made up).
///
/// The avatar is never a turret and has no `oSanctuaryController`, so those
/// branches are folded in: a turret's `turretPointDefenseMult` multiplies
/// `constructEffectMult`; with Sanctuary on, `temporalRadius` is set so the
/// circle is `sanctuaryRadius` and `sanctuary` goes to 0 ([`centers`] puts
/// the call at the Sanctuary's place).
#[must_use]
pub fn call_values(values: &[f32], turret: bool) -> Option<Vec<(usize, f64)>> {
    if values.len() != VALUES.len() || values.iter().any(|v| v.is_nan()) {
        return None;
    }
    let mut v: Vec<f64> = values.iter().map(|x| f64::from(*x)).collect();
    if turret {
        v[CONSTRUCT_MULT] *= v[TURRET_MULT];
    }
    if v[SANCTUARY] != 0.0 {
        if v[SANCTUARY_ENABLE] == 0.0 {
            return None;
        }
        let per = v[RADIUS] * v[CONSTRUCT_MULT];
        if per == 0.0 {
            return None;
        }
        v[TEMPORAL_RADIUS] = v[SANCTUARY_RADIUS] / per;
        v[SANCTUARY] = 0.0;
    }
    Some(v.into_iter().enumerate().collect())
}

/// The circle's radius the game uses for these values and caller
/// (`temporalRadius * radius * constructEffectMult`, or `sanctuaryRadius`).
#[cfg(test)]
fn radius(values: &[f32], turret: bool) -> Option<f64> {
    let v = call_values(values, turret)?;
    Some(v[TEMPORAL_RADIUS].1 * v[RADIUS].1 * v[CONSTRUCT_MULT].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::msg::PeerId;

    fn vals() -> Vec<f32> {
        // shieldType, shield, sanctuary, enable, construct, turret, sanct r,
        // temporal r, radius, slow, effect, solid mod, temporal mod, polar
        vec![
            6.0, 1.0, 0.0, 1.0, 1.0, 1.5, 300.0, 1.2, 100.0, 0.02, 1.0, 1.0, 0.5, 0.0,
        ]
    }

    fn seen(x: f64, y: f64, ship: bool, turret: bool) -> Seen {
        Seen::temporal(x, y, ship, turret)
    }

    #[test]
    fn centres_are_from_the_ship_and_its_own_call_comes_first() {
        let s = [
            seen(130.0, 40.0, false, true),
            seen(100.0, 50.0, true, false),
            seen(90.4, 60.6, false, false),
        ];
        let ship = Some((100.0, 50.0));
        let c = centers(&s, ship, None, true);
        assert_eq!(c.len(), 3);
        assert_eq!((c[0].dx, c[0].dy, c[0].turret), (0.0, 0.0, false));
        assert_eq!((c[1].dx, c[1].dy, c[1].turret), (30.0, -10.0, true));
        assert_eq!((c[2].dx, c[2].dy), (-10.0, 11.0));
        // Riders alone (Shielded Constructs: the ship does not call).
        let r = centers(&s[2..], ship, None, true);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].dx, r[0].dy), (-10.0, 11.0));
        // No ship: nothing (the host centres on its avatar).
        assert!(centers(&s, None, None, true).is_empty());
        // Sanctuary: every call round its place.
        let c = centers(&s, ship, Some((400.0, 50.0)), true);
        assert!(c.iter().all(|c| (c.dx, c.dy) == (300.0, 0.0)));
        // Another shield: the game's calls do nothing, none sent.
        assert!(centers(&s, ship, None, false).is_empty());
    }

    #[test]
    fn a_vortex_pull_goes_at_its_place_with_a_released_ones_count_folded_in() {
        let ship = Some((100.0, 50.0));
        let s = [
            seen(100.0, 50.0, true, false),
            Seen::vortex(300.0, 50.0, 1.0, false),
            Seen::vortex(100.0, 250.0, 2.0, true),
            Seen::vortex(100.0, -150.0, 2.0, true),
        ];
        // Not Temporal: the pulls alone.
        let c = centers(&s, ship, Some((0.0, 0.0)), false);
        assert_eq!(c.len(), 3);
        assert!(c.iter().all(|c| c.kind == KIND_VORTEX && !c.turret));
        assert_eq!((c[0].dx, c[0].dy, c[0].scale), (200.0, 0.0, 1.0));
        // Two released: each size over sqrt(2), so sqr(41 * size) is halved
        // as the game's division by instance_number(oVortex) halves it; the
        // Sanctuary does not move a pull.
        let half = 2.0 / 2f32.sqrt();
        assert_eq!((c[1].dx, c[1].dy), (0.0, 200.0));
        assert!((c[1].scale - half).abs() < 1e-6 && (c[2].scale - half).abs() < 1e-6);
        let sq = |s: f32| (41.0 * s).powi(2);
        assert!((sq(c[1].scale) - sq(2.0) / 2.0).abs() < 1e-2);
        // Temporal too: the slow first.
        let t = centers(&s, ship, None, true);
        assert_eq!(t.len(), 4);
        assert_eq!(t[0].kind, KIND_TEMPORAL);
        // The host's values: polarInversion only, and the size.
        let mut v = vals();
        v[POLAR_INVERSION] = 1.0;
        let (on, scale) = vortex_values(&v, &c[0]).unwrap();
        assert_eq!(on, vec![(POLAR_INVERSION, 1.0)]);
        assert!((scale - 1.0).abs() < 1e-9);
        assert!(vortex_values(&v, &t[0]).is_none(), "not a pull");
        v[POLAR_INVERSION] = f32::NAN;
        assert!(vortex_values(&v, &c[0]).is_none(), "never made up");
        let none = AreaCenter { scale: 0.0, ..c[0] };
        assert!(vortex_values(&vals(), &none).is_none(), "no size, no pull");
    }

    #[test]
    fn sent_while_temporal_on_change_and_resend_and_once_when_it_stops() {
        let mut a = AreaSend::default();
        let c = vec![AreaCenter {
            dx: 0.0,
            dy: 0.0,
            turret: false,
            kind: KIND_TEMPORAL,
            scale: 0.0,
        }];
        assert!(a.due(vals(), c.clone(), 1).is_some());
        assert!(a.due(vals(), c.clone(), 2).is_none());
        assert!(a.due(vals(), c.clone(), 1 + RESEND_FRAMES).is_some());
        // A change: at most every CENTERS_EVERY frames.
        let moved = vec![AreaCenter {
            dx: 5.0,
            dy: 0.0,
            turret: false,
            kind: KIND_TEMPORAL,
            scale: 0.0,
        }];
        assert!(a.due(vals(), moved.clone(), 2 + RESEND_FRAMES).is_none());
        assert!(
            a.due(vals(), moved.clone(), 1 + RESEND_FRAMES + CENTERS_EVERY)
                .is_some()
        );
        // A value change the same.
        let mut v = vals();
        v[1] = 0.0;
        let at = 1 + RESEND_FRAMES + CENTERS_EVERY;
        assert!(a.due(v.clone(), c.clone(), at + 1).is_none());
        assert!(a.due(v, c.clone(), at + CENTERS_EVERY).is_some());
        // Another shield (no centres): one empty state, then nothing.
        let mut other = vals();
        other[SHIELD_TYPE] = 2.0;
        let stop = a.due(other.clone(), Vec::new(), 30).unwrap();
        assert!(stop.1.is_empty());
        assert!(a.due(other.clone(), Vec::new(), 31).is_none());
        // Never acting: never sent.
        let mut b = AreaSend::default();
        assert!(b.due(other, Vec::new(), 1).is_none());
    }

    #[test]
    fn the_host_keeps_the_newest_per_peer_and_forgets_stale_ones() {
        let mut r = AreaRecv::default();
        let st = |peer: u8, n: usize| AreaState {
            peer: PeerId(peer),
            time_ms: 0,
            names: names_hash(),
            values: vals(),
            centers: vec![
                AreaCenter {
                    dx: 0.0,
                    dy: 0.0,
                    turret: false,
                    kind: KIND_TEMPORAL,
                    scale: 0.0
                };
                n
            ],
        };
        r.take(st(1, 1), 10);
        r.take(st(1, 2), 11);
        assert_eq!(r.live(12).len(), 1);
        assert_eq!(r.live(12)[0].centers.len(), 2);
        r.take(st(1, 0), 13);
        assert!(r.live(14).is_empty(), "an empty state stops it");
        r.take(st(1, 1), 20);
        assert_eq!(r.live(20 + EXPIRE_FRAMES).len(), 1);
        assert!(r.live(21 + EXPIRE_FRAMES).is_empty());
        let mut other = st(1, 1);
        other.names ^= 1;
        r.take(other, 30);
        assert!(r.live(30).is_empty(), "another value list is ignored");
    }

    #[test]
    fn the_avatar_gets_the_partners_values_with_the_turret_and_sanctuary_folded_in() {
        let plain = call_values(&vals(), false).unwrap();
        assert_eq!(plain.len(), VALUES.len());
        assert!((radius(&vals(), false).unwrap() - 120.0).abs() < 1e-3);
        // A turret: constructEffectMult times turretPointDefenseMult.
        let turret = call_values(&vals(), true).unwrap();
        assert!((turret[CONSTRUCT_MULT].1 - 1.5).abs() < 1e-9);
        assert!((radius(&vals(), true).unwrap() - 180.0).abs() < 1e-3);
        // Sanctuary: the circle is sanctuaryRadius, sanctuary off for the call.
        let mut sanct = vals();
        sanct[SANCTUARY] = 1.0;
        let folded = call_values(&sanct, false).unwrap();
        assert!(folded[SANCTUARY].1.abs() < f64::EPSILON);
        assert!((radius(&sanct, false).unwrap() - 300.0).abs() < 1e-3);
        // Sanctuary with its shield off: the game does nothing.
        sanct[SANCTUARY_ENABLE] = 0.0;
        assert!(call_values(&sanct, false).is_none());
        // A value the partner did not have is never made up.
        let mut unset = vals();
        unset[3] = f32::NAN;
        assert!(call_values(&unset, false).is_none());
    }

    #[test]
    fn only_apply_temporal_writes_enemies() {
        // The family is the table's `Enemies` rows: applyTemporal alone.
        let family: Vec<&str> = SCAN
            .iter()
            .filter(|(_, w)| *w == Writes::Enemies)
            .map(|(f, _)| *f)
            .collect();
        assert_eq!(family, vec![APPLY_TEMPORAL]);
    }
}
