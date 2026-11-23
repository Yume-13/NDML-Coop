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

//! Shared damage: hooks and game calls (see the [module docs](super)). The
//! world glue calls in at the client's tick, its death checks, the client and
//! host frames and the host's despawns. No lock is held across a call into
//! the game; ids are checked with `instance_exists` first.

#![forbid(unsafe_code)]

pub mod effects;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use ndml_net::msg::{DamageReport, DespawnCause, Message, NetId, PeerId};
use ndml_sdk::{Builtin, Call, Instance, ModContext, Script, Value, Variable};

use super::audit::{Audit, Flush};
use super::cause::{Causes, Ctx};
use super::{ClientDamage, DamageSettings, HOST_STUCK_FRAMES, HostDamage, TickOut, Verdict};
use crate::game::{info_once, warn_once};

pub const ROLE_OFF: u8 = 0;
pub const ROLE_HOST: u8 = 1;
pub const ROLE_CLIENT: u8 = 2;

const CHECK_DESTROY: &str = "gml_Script_checkDestroy";
const ENEMY_DESTROYED: &str = "gml_Script_enemyDestroyed";
const ASTEROID_STEPS: [&str; 2] = [
    "gml_Object_oAsteroid_Step_0",
    "gml_Object_oAsteroidSmall_Step_0",
];
const SOLID: &str = "oSolid";
/// How long a destroyed enemy's net id is remembered for the "destroyed a
/// second time" check (well under the net id wrap).
const DESTROYED_TWICE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// Every game function that writes `hull` where the written instance can be
/// an enemy (its own instance, a `with` target, `other` in a collision, or an
/// unresolved id). Each is bracketed.
pub const AUDIT_SITES: &[&str] = &[
    "gml_Object_oAsteroid_Collision_oEnemyBeamReflected",
    "gml_Object_oAsteroid_Collision_oEnemyBlastReflected",
    "gml_Object_oAsteroid_Collision_oEnemyProjectileReflectedObjectClone",
    "gml_Object_oBlackHole_Collision_oSolid",
    "gml_Object_oBlade_Step_2",
    "gml_Object_oBoltThrowerJawLeft_Destroy_0",
    "gml_Object_oBoltThrowerJawRight_Destroy_0",
    "gml_Object_oBoxLarge_Alarm_1",
    "gml_Object_oBoxSmall_Alarm_1",
    "gml_Object_oCelestialDrone_Step_0",
    "gml_Object_oCometShard_Collision_oSolid",
    "gml_Object_oComet_Collision_oSolid",
    "gml_Object_oConstructAlly_Alarm_4",
    "gml_Object_oConstructDrone_Alarm_0",
    "gml_Object_oConstructMine_Destroy_0",
    "gml_Object_oConstructorDrone_Alarm_1",
    "gml_Object_oConstructorDrone_Step_0",
    "gml_Object_oController_KeyPress_75",
    "gml_Object_oCosmicEgg_Collision_oSolid",
    "gml_Object_oCrucibleDrone_Step_2",
    "gml_Object_oDummy_Other_10",
    "gml_Object_oDweller_Destroy_0",
    "gml_Object_oEclipseChasis_Create_0",
    "gml_Object_oEclipseChasis_Step_0",
    "gml_Object_oEclipsePanel_Create_0",
    "gml_Object_oEclipsePanel_Step_0",
    "gml_Object_oEclipseShield_Create_0",
    "gml_Object_oEnemyHardShield_Create_0",
    "gml_Object_oEnemySegmentFin_Step_2",
    "gml_Object_oEnemySegmentFinger_Step_2",
    "gml_Object_oEnemySegment_Destroy_0",
    "gml_Object_oEnemySegment_Step_2",
    "gml_Object_oEnemySpawner_Alarm_1",
    "gml_Object_oHammerhead_Alarm_0",
    "gml_Object_oHostDrone_Step_0",
    "gml_Object_oHost_Alarm_0",
    "gml_Object_oJuggernaut_Other_10",
    "gml_Object_oMegaMine_Alarm_0",
    "gml_Object_oMegaMine_Collision_oPlayer",
    "gml_Object_oMegaMine_Collision_oShieldBastion",
    "gml_Object_oMegaMine_Destroy_0",
    "gml_Object_oMineEnemySingularity_Alarm_0",
    "gml_Object_oMineEnemy_Alarm_0",
    "gml_Object_oMineInterceptor_Alarm_0",
    "gml_Object_oMineShield_Collision_oProjectileEffect",
    "gml_Object_oMineShield_Create_0",
    "gml_Object_oPlayerConstruct_Collision_oEnemyExplosive",
    "gml_Object_oSENullWave_Collision_oSolid",
    "gml_Object_oSeraphDrone_Step_0",
    "gml_Object_oSeraphTeslaMine_Collision_oPlayer",
    "gml_Object_oSeraphTeslaMine_Collision_oPlayerSegment",
    "gml_Object_oSeraphTeslaMine_Collision_oShieldBastion",
    "gml_Object_oSeraphZoneDrone_Step_0",
    "gml_Object_oSeraph_Alarm_1",
    "gml_Object_oSeraph_Alarm_4",
    "gml_Object_oSeraph_Alarm_7",
    "gml_Object_oSnapperJawLeft_Step_0",
    "gml_Object_oSnapperJawRight_Step_0",
    "gml_Object_oSnapperTentacleBase_Step_0",
    "gml_Object_oSnapperTentacle_Step_0",
    "gml_Object_oStarEaterController_Alarm_0",
    "gml_Object_oStarEaterController_Alarm_1",
    "gml_Object_oStarEaterController_Alarm_2",
    "gml_Object_oStarEaterController_Alarm_3",
    "gml_Object_oStarEaterController_Step_0",
    "gml_Object_oStarEaterSegment_Create_0",
    "gml_Object_oStarEaterSegment_Step_0",
    "gml_Object_oStarEater_Create_0",
    "gml_Object_oStationCore_Step_0",
    "gml_Object_oStationDrone_Step_0",
    "gml_Object_oStationNode_Step_0",
    "gml_Object_oStationWall_Step_0",
    "gml_Object_oSwordController_Step_1",
    "gml_Object_oSwordObject_Collision_oEnemyExplosive",
    "gml_Object_oSwordObject_Collision_oSolid",
    "gml_Object_oTriangleMine_Alarm_0",
    "gml_Object_oTriangleMine_Collision_oPlayer",
    "gml_Object_oTriangleMine_Collision_oShieldBastion",
    "gml_Object_oVanguardMine_Alarm_0",
    "gml_Object_oWarbringerBroadside1_Step_2",
    "gml_Object_oWarbringerBroadside2_Step_2",
    "gml_Object_oWarbringerChasis_Step_2",
    "gml_Object_oWarbringerLaser_Step_2",
    "gml_Object_oWarbringerThruster1_Step_2",
    "gml_Object_oWarbringerThruster2_Step_2",
    "gml_Object_oWarbringerWing1_Step_2",
    "gml_Object_oWarbringerWing2_Step_2",
    "gml_Object_oWaves_Alarm_1",
    "gml_Script_allyBehavior",
    "gml_Script_allyGuardian",
    "gml_Script_allyWingman",
    "gml_Script_applyCommanderStats",
    "gml_Script_applyRarity",
    "gml_Script_bastionDamageSolid",
    "gml_Script_buildEnemyPart",
    "gml_Script_burstHitsSolid",
    "gml_Script_burstRetaliateHitsSolid",
    "gml_Script_dartBurn",
    "gml_Script_degenerations",
    "gml_Script_destroyConstruct",
    "gml_Script_droneProjectileHitsSolid",
    "gml_Script_dronePursuitWavy",
    "gml_Script_enemyBlastHitsSolid",
    "gml_Script_enemyBurnHitsSolid",
    "gml_Script_enemyCreateHardShield",
    "gml_Script_enemyFireProjectiles",
    "gml_Script_enemyHealingAura",
    "gml_Script_enemyInit",
    "gml_Script_enemyProjectileHitsSolid",
    "gml_Script_enemyProximityExplode",
    "gml_Script_enemyReduceDwellerHull",
    "gml_Script_fullKill@gml_Object_oSwordController_Step_1",
    "gml_Script_geminiDOTHullConstruct",
    "gml_Script_haloHitsSolid",
    "gml_Script_inheritRarity",
    "gml_Script_playerHitsSolid",
    "gml_Script_playerSegmentHitsSolid",
    "gml_Script_projectileHitsSolid",
    "gml_Script_reduceLeaderHealth",
    "gml_Script_reflectBeams",
    "gml_Script_refreshDefenses",
    "gml_Script_refreshDrones",
    "gml_Script_refreshSwordStats",
    "gml_Script_restoreRun_misc",
    "gml_Script_ricochetShotBlade",
    "gml_Script_scaleEnemyStats",
    "gml_Script_scorchingWakeHitsSolid",
    "gml_Script_segmentLeaderDamage",
    "gml_Script_setChampion",
    "gml_Script_setElite",
    "gml_Script_shootSpecialCases3",
    "gml_Script_siegeWeaponrySelfDamageConstruct",
    "gml_Script_siphonHitsSolid",
    "gml_Script_splinterShotBlade",
    "gml_Script_starEaterDamage",
    "gml_Script_swarmConstructDefensive",
    "gml_Script_terminateCull",
    "gml_Script_terminateStationOmega",
    "gml_Script_thermalLanceHitsSolid",
    "gml_Script_triggerRetribution",
    "gml_Script_venomStrikeHitsSolid",
    "gml_Script_vortexChargeHitsSolid",
    "gml_Script_vortexHitsSolid",
];

/// How the shared world classifies an instance the audit saw damaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// A live copy (measured at its tick).
    Copy,
    /// A copy's part.
    Part,
    /// Neither (a local missile, a stray, or gone).
    Other,
}

#[derive(Debug, Default)]
struct Observed {
    /// `enemyDestroyed` calls, and per net id the count and time of the
    /// first, forgotten after [`DESTROYED_TWICE_WINDOW`] (net ids wrap).
    destroyed: u64,
    destroyed_by_net: HashMap<NetId, (u32, std::time::Instant)>,
}

#[derive(Debug, Default)]
struct State {
    client: ClientDamage,
    host: HostDamage,
    audit: Audit,
    /// Which local changes to copies are the joining player's.
    causes: Causes,
    me: Option<PeerId>,
    frame: u64,
    /// Copies the client world reported at their last tick (key -> net id).
    copies: HashMap<usize, NetId>,
    obs: Observed,
    /// Enemies the host has killed or removed: the frame its word came.
    host_gone: HashMap<NetId, u64>,
    /// Damage on copies that left before their measurement and before the
    /// host's word, until that word comes or 600 frames pass.
    gone_watch: HashMap<NetId, GoneWatch>,
    /// What became of that damage: (host killed it, sent again, no word).
    gone_after: [f64; 3],
    gone_logged: u32,
    /// Host: kill credit and the kill-effects counts.
    kills: super::fx::HostKills,
    /// Host: the death paths running now, innermost last, each with the
    /// partner its death is credited to.
    deaths: Vec<Option<PeerId>>,
    /// Host: `enemyDestroyed` running on this instance (its map entry goes
    /// after it, so the paths inside it still find it).
    destroying: Vec<usize>,
    /// The status channel (joining side, host side) and its counts.
    status_rep: ndml_net::status::StatusReporter,
    status_led: ndml_net::status::StatusLedger,
    status_counts: effects::StatusCounts,
    /// Joining side: which status changes on copies the player's side made
    /// (the cause rule's brackets, [`super::status_cause`]).
    status_causes: super::status_cause::StatusCauses,
}

/// Gone damage waiting for the host's word: (frame, damage, by source,
/// object).
type GoneWatch = (u64, f64, Vec<(u16, f64)>, u32);

/// A report that took an enemy to 0: (net id, the peer, the hull it left).
pub type Killer = (NetId, PeerId, f32);

/// The player-side test of an instance.
pub type PlayerSideFn = dyn Fn(&Call<'_>, &Instance<'_>) -> bool + Send + Sync;

/// Whether an instance is player-side (`self`/`other` of a bracketed call).
pub struct PlayerSide(pub Box<PlayerSideFn>);

impl std::fmt::Debug for PlayerSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlayerSide")
    }
}

/// The kill credit of the host enemy at an instance key: the world's
/// `last_damager`, as a kill's `Despawn` names it.
pub type KillCreditFn = dyn Fn(usize) -> Option<PeerId> + Send + Sync;

/// See [`KillCreditFn`].
pub struct KillCredit(pub Box<KillCreditFn>);

impl std::fmt::Debug for KillCredit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KillCredit")
    }
}

/// Status ticks (burns, corrosion, darts), run in the enemy's own Step.
const STATUS_SITES: &[&str] = &["gml_Script_degenerations", "gml_Script_dartBurn"];

/// A bracket's state from its before-hook to its after-hook.
#[derive(Debug)]
pub struct Bracket {
    before: HashMap<usize, f64>,
    /// Copies' status variables (with the status channel on).
    before_fx: HashMap<usize, super::status_cause::Vals>,
    ctx: Ctx,
    audited: bool,
}

/// See the [module docs](super).
#[derive(Debug)]
pub struct DamageHooks {
    settings: DamageSettings,
    role: AtomicU8,
    st: Mutex<State>,
    hull: Option<Variable>,
    check_destroy: Option<Script>,
    exists: Option<Builtin>,
    destroy: Option<Builtin>,
    asset: Option<Builtin>,
    /// `oSolid` (`u32::MAX` until resolved).
    solid: AtomicU32,
    /// Whether an instance is player-side (the mirror's rule), set once.
    player_side: OnceLock<PlayerSide>,
    /// Host: the world's kill credit (kill effects), set once.
    kill_credit: OnceLock<KillCredit>,
    /// Client: copies' Destroy events running now (the world's
    /// `die_like_host`): every hull change is the host's.
    host_deaths: AtomicU32,
    /// Kill effects and the status channel's variables and scripts.
    fx: effects::FxHandles,
}

impl DamageHooks {
    /// Resolve handles; hooks are registered by [`register`].
    #[must_use]
    pub fn new(ctx: &ModContext, settings: DamageSettings) -> Self {
        let v = |n: &str| ctx.variable(n).ok();
        Self {
            settings,
            role: AtomicU8::new(ROLE_OFF),
            st: Mutex::new(State::default()),
            hull: v(crate::names::VAR_HULL),
            check_destroy: ctx.script(CHECK_DESTROY).ok(),
            exists: ctx.builtin("instance_exists").ok(),
            destroy: ctx.builtin("instance_destroy").ok(),
            asset: ctx.builtin("asset_get_index").ok(),
            solid: AtomicU32::new(u32::MAX),
            player_side: OnceLock::new(),
            kill_credit: OnceLock::new(),
            host_deaths: AtomicU32::new(0),
            fx: effects::FxHandles::new(ctx),
        }
    }

    /// Client: a copy starts dying the host's death; until
    /// [`Self::end_host_death`] every bracketed hull change is the host's.
    pub fn begin_host_death(&self) {
        self.host_deaths.fetch_add(1, Ordering::Relaxed);
    }

    /// See [`Self::begin_host_death`].
    pub fn end_host_death(&self) {
        let _ = self
            .host_deaths
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }

    /// The switches.
    #[must_use]
    pub fn settings(&self) -> &DamageSettings {
        &self.settings
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::game::lock_or_report(&self.st, "damage")
    }

    /// The shared world's role this frame (`ROLE_*`).
    pub fn set_role(&self, role: u8, me: PeerId) {
        let old = self.role.swap(role, Ordering::Relaxed);
        let mut st = self.lock();
        st.me = Some(me);
        if old != role {
            st.client.clear();
            st.host.clear();
            st.copies.clear();
        }
    }

    fn role(&self) -> u8 {
        self.role.load(Ordering::Relaxed)
    }

    /// Set the kill credit (host; see [`KillCredit`]).
    pub fn set_kill_credit(&self, f: KillCredit) {
        let _ = self.kill_credit.set(f);
    }

    /// The player-side rule (the mirror's), set once at start.
    pub fn set_player_side(&self, f: PlayerSide) {
        let _ = self.player_side.set(f);
    }

    fn sharing(&self) -> bool {
        self.role() != ROLE_OFF
    }

    fn auditing(&self) -> bool {
        self.settings.audit && self.role() == ROLE_CLIENT
    }

    fn object(&self, call: &Call<'_>, slot: &AtomicU32, name: &str) -> Option<u32> {
        let v = slot.load(Ordering::Relaxed);
        if v != u32::MAX {
            return Some(v);
        }
        let i = self
            .asset
            .and_then(|b| call.call_builtin(b, &[Value::from(name)]).ok())
            .and_then(|v| crate::world::classify::object_index(&v))?;
        slot.store(i, Ordering::Relaxed);
        Some(i)
    }

    fn exists(&self, call: &Call<'_>, id: &Value) -> bool {
        self.exists
            .and_then(|b| call.call_builtin(b, std::slice::from_ref(id)).ok())
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Hull of every `oSolid` and, with `fx`, each copy's status variables.
    fn sweep(
        &self,
        call: &Call<'_>,
        fx: bool,
    ) -> (
        HashMap<usize, f64>,
        HashMap<usize, super::status_cause::Vals>,
    ) {
        let mut out = HashMap::new();
        let mut vals = HashMap::new();
        let (Some(solid), Some(hull)) = (self.object(call, &self.solid, SOLID), self.hull) else {
            return (out, vals);
        };
        let copies: std::collections::HashSet<usize> = if fx {
            self.lock().copies.keys().copied().collect()
        } else {
            std::collections::HashSet::new()
        };
        let _ = call.with(&Value::object(solid), |inst, _| {
            let key = inst.key();
            if let Some(h) = inst.get_f64(hull) {
                out.insert(key, h);
            }
            if copies.contains(&key) {
                vals.insert(key, self.fx.vals(&inst));
            }
        });
        (out, vals)
    }

    /// Host messages before the client world sees them.
    pub fn client_messages(&self, inbox: &[Message]) {
        let Some(me) = self.lock().me else { return };
        self.client_status_messages(me, inbox);
        let mut st = self.lock();
        for m in inbox {
            st.client.on_message(me, m);
            match m {
                Message::Despawn { net_id, cause } => {
                    let frame = st.frame;
                    st.host_gone.insert(*net_id, frame);
                    if let Some((at, d, parts, object)) = st.gone_watch.remove(net_id) {
                        st.gone_after[0] += d;
                        st.audit.gone_was_overkill(&parts, object);
                        let lag = st.frame.saturating_sub(at);
                        log_limited!(
                            log::Level::Info,
                            "damage-gone-despawn",
                            "shared damage: gone damage {d:.1} on {net_id}: the host's despawn ({cause:?}) came {lag} frames after it left here"
                        );
                    }
                }
                Message::EnemySpawn(s) => {
                    st.host_gone.remove(&s.net_id);
                    if let Some((at, d, _, _)) = st.gone_watch.remove(&s.net_id) {
                        st.gone_after[1] += d;
                        let lag = st.frame.saturating_sub(at);
                        log_limited!(
                            log::Level::Info,
                            "damage-gone-sent-again",
                            "shared damage: gone damage {d:.1} on {}: the host sent the enemy again {lag} frames after it left here (lost)",
                            s.net_id
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// A copy's tick (End Step, before the world writes). `now_hull` was
    /// read from the copy; `host_hull` is the host's. `None` when sharing
    /// is off (the world writes the host's hull).
    pub fn client_tick(
        &self,
        net_id: NetId,
        key: usize,
        _object: Option<u32>,
        host_hull: f32,
        now_hull: Option<f32>,
    ) -> Option<TickOut> {
        let mut st = self.lock();
        // Only what a player-side context took off is measured.
        let written = st.client.written(net_id);
        let raw = written
            .zip(now_hull)
            .map_or(0.0, |(w, h)| f64::from(w - h).max(0.0));
        let counted = st.causes.take_counted(key, raw);
        let measured_now = match (written, now_hull) {
            (Some(w), Some(_)) if raw > counted + 1e-3 => Some(w - counted as f32),
            _ => now_hull,
        };
        let out = st.client.tick(net_id, key, host_hull, measured_now);
        st.copies.insert(key, net_id);
        if self.auditing() {
            // The audit counts above 0: the drop down to 0 at most.
            let drop = match (written, measured_now) {
                (Some(w), Some(h)) if out.drop > 0.0 => (w.max(0.0) - h.max(0.0)).max(0.0),
                _ => out.drop,
            };
            let _ = st.audit.measure(key, f64::from(drop), out.overkill);
        }
        Some(out)
    }

    /// The world created a copy of `net_id` (instance `key`) and wrote
    /// `hull` to it: the measurement's first baseline, so damage before its
    /// first tick counts.
    pub fn spawned(&self, net_id: NetId, key: usize, hull: f32) {
        let auditing = self.auditing();
        let mut st = self.lock();
        st.client.baseline(net_id, hull);
        st.copies.insert(key, net_id);
        // Hull changes from the copy's creation and set-up are replaced by
        // the host's hull.
        if auditing {
            let _ = st.audit.flush(key, Flush::BeforeState);
        }
        // A copy sent again (after a refused kill) may die again.
        st.obs.destroyed_by_net.remove(&net_id);
    }

    /// The world wrote `new` over `old` on `key` (not a game cause: the cause
    /// rule and the audit take it out).
    pub fn own_write(&self, key: usize, old: Option<f64>, new: f64) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let Some(o) = old else { return };
        let mut st = self.lock();
        st.causes.own_write(key, new - o);
        if self.auditing() {
            st.audit.own_write(key, o, new);
        }
    }

    /// Client: copies the local player's corrosion reached by a spread (no
    /// hull change marks them): their status ticks are the player's.
    pub fn mark_touched(&self, keys: &[usize]) {
        if self.role() != ROLE_CLIENT || keys.is_empty() {
            return;
        }
        let mut st = self.lock();
        for &k in keys {
            st.causes.touch(k);
        }
    }

    /// Whether the copy at `key` is dying here (its death path runs).
    #[must_use]
    pub fn is_dying(&self, key: usize) -> bool {
        self.role() == ROLE_CLIENT && self.lock().client.is_dying(key)
    }

    /// [`Self::is_dying`] without waiting (sounds, `crate::sound`): `false`
    /// while the damage lock is held (a sound inside a game call the damage
    /// code makes, which would otherwise wait on itself).
    #[must_use]
    pub fn dying_now(&self, key: usize) -> bool {
        self.role() == ROLE_CLIENT && self.st.try_lock().is_ok_and(|s| s.client.is_dying(key))
    }

    /// Host, sounds (`crate::sound`): a peer's claim on `net_id` is
    /// confirmed and still open ([`HostDamage::confirmed_claim`]), without
    /// waiting (`false` while the damage lock is held).
    #[must_use]
    pub fn claimed_now(&self, net_id: NetId) -> bool {
        self.role() == ROLE_HOST
            && self
                .st
                .try_lock()
                .is_ok_and(|s| s.host.confirmed_claim(net_id).is_some())
    }

    /// Client: net ids whose kill claim is still open (the copy died here
    /// and may be sent again after a refusal).
    #[must_use]
    pub fn open_claims(&self) -> Vec<NetId> {
        if self.role() != ROLE_CLIENT {
            return Vec::new();
        }
        self.lock().client.open_claims()
    }

    /// Client: enemies whose copy must not be made now (kills claimed here
    /// before a resync that sent the enemy again; see
    /// [`super::ClientDamage::held_spawns`]).
    #[must_use]
    pub fn held_spawns(&self) -> Vec<NetId> {
        if self.role() != ROLE_CLIENT {
            return Vec::new();
        }
        self.lock().client.held_spawns()
    }

    /// A copy the host despawned that is still here [`super::GHOST_FRAMES`]
    /// later: `checkDestroy` as the enemy, then `instance_destroy` if needed.
    pub fn remove_ghost(&self, call: &Call<'_>, me: &Instance<'_>, net_id: NetId) {
        let Ok(id) = me.builtin_value("id") else {
            return;
        };
        if let Some(s) = self.check_destroy
            && let Err(e) = me.call_script(s, &[])
        {
            warn_once(
                "damage-ghost",
                &format!("shared damage: removing a despawned copy: {e}"),
            );
        }
        if self.exists(call, &id)
            && let Some(b) = self.destroy
        {
            let _ = me.call_builtin(b, &[id.clone(), Value::Bool(true)]);
        }
        if !self.exists(call, &id) {
            self.left(me.key(), Some(net_id));
            info_once(
                "damage-ghost-removed",
                "shared damage: a copy the host despawned was still here after the ghost bound; removed",
            );
        }
    }

    /// After a `checkDestroy*` or asteroid Step ran on the client: a dying
    /// copy that is gone now died here.
    pub fn after_death_path(&self, call: &Call<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let Some(me) = call.instance() else { return };
        let key = me.key();
        let Some(net_id) = self.lock().client.dying_net_id(key) else {
            return;
        };
        let Ok(id) = me.builtin_value("id") else {
            return;
        };
        if !self.exists(call, &id) {
            self.left(key, Some(net_id));
        }
    }

    /// A copy left the game here (`net_id` when known); closes its audit
    /// window before its address can be reused.
    fn left(&self, key: usize, net_id: Option<NetId>) {
        let mut st = self.lock();
        let net_id = net_id.or_else(|| st.copies.get(&key).copied());
        let dying = st.client.is_dying(key);
        let host_gone = net_id.is_some_and(|n| st.host_gone.contains_key(&n));
        // The old instance of a refused claim's copy.
        let refused = net_id.is_some_and(|n| st.client.refused_lately(n));
        if let Some(n) = net_id {
            st.client.gone(n);
        }
        if self.auditing() {
            let how = if dying || host_gone || refused {
                Flush::AfterKill
            } else {
                Flush::Gone
            };
            let (parts, object) = st.audit.flush(key, how);
            let d: f64 = parts.iter().map(|p| p.1).sum();
            if how == Flush::Gone
                && d > 0.0
                && let Some(n) = net_id
            {
                let frame = st.frame;
                st.gone_watch.insert(n, (frame, d, parts, object));
                if st.gone_logged < 20 {
                    st.gone_logged += 1;
                    log::info!(
                        "shared damage: {d:.1} damage on copy {n} counted gone: it left here before its measurement and before the host's word"
                    );
                }
            }
        }
        st.causes.forget(key);
        st.status_causes.forget(key);
        st.copies.remove(&key);
    }

    /// The world removes the copy at `key` now (the host's despawn).
    pub fn copy_removed(&self, key: usize) {
        self.left(key, None);
    }

    /// Copies the world found gone at the start of a frame.
    pub fn client_gone(&self, ids: &[NetId]) {
        let keys: Vec<(usize, NetId)> = {
            let st = self.lock();
            st.copies
                .iter()
                .filter(|(_, n)| ids.contains(n))
                .map(|(&k, &n)| (k, n))
                .collect()
        };
        for (k, n) in keys {
            self.left(k, Some(n));
        }
        let mut st = self.lock();
        for &n in ids {
            st.client.gone(n);
        }
    }

    /// Instances the world must classify for [`Self::client_frame`].
    #[must_use]
    pub fn keys_to_classify(&self) -> Vec<usize> {
        let st = self.lock();
        let mut keys = st.audit.pending_keys();
        keys.extend(st.copies.keys().copied());
        keys
    }

    /// End of the client's run tick: the reports to send. `frozen`: the
    /// claims' answer clock and the copies' timers do not age.
    pub fn client_frame(&self, classify: &dyn Fn(usize) -> Seen, frozen: bool) -> Vec<Message> {
        let mut st = self.lock();
        st.frame += 1;
        let frame = st.frame;
        st.causes.begin_frame(frame);
        st.status_causes.begin_frame();
        if !frozen {
            for n in st.client.begin_frame() {
                log_limited!(
                    log::Level::Info,
                    "damage-claim-unanswered",
                    "shared damage: claim on {n} unanswered by the host; given up"
                );
            }
        }
        if self.auditing() {
            for key in st.audit.pending_keys() {
                let how = match classify(key) {
                    Seen::Copy => continue,
                    Seen::Part => Flush::Part,
                    Seen::Other => match st.copies.get(&key) {
                        Some(n) if st.host_gone.contains_key(n) || st.client.refused_lately(*n) => {
                            Flush::AfterKill
                        }
                        Some(_) => Flush::Gone,
                        None => Flush::Local,
                    },
                };
                st.audit.flush(key, how);
            }
        }
        st.copies.retain(|&k, _| classify(k) == Seen::Copy);
        let live: std::collections::HashSet<NetId> = st.copies.values().copied().collect();
        st.host_gone
            .retain(|n, at| super::host_gone_kept(live.contains(n), *at, frame));
        let Some(me) = st.me else {
            return Vec::new();
        };
        let mut out = st.client.take_reports(me);
        drop(st);
        out.extend(self.client_status_reports(me));
        out
    }

    /// Apply peers' reports to the real enemies (`lookup`: a net id's
    /// instance id, max hull and object). Returns (killers, net ids to send
    /// again, messages, entries held back for a deactivated enemy).
    pub fn host_apply(
        &self,
        call: &Call<'_>,
        reports: &[DamageReport],
        lookup: &dyn Fn(NetId) -> Option<(Value, f32, u16)>,
    ) -> (Vec<Killer>, Vec<NetId>, Vec<Message>, Vec<DamageReport>) {
        let (mut killers, mut again, mut out, mut held) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        if !self.sharing() || self.role() != ROLE_HOST {
            return (killers, again, out, held);
        }
        for r in reports {
            let (now, later): (Vec<_>, Vec<_>) =
                r.entries.iter().copied().partition(|e| {
                    lookup(e.net_id).is_none_or(|(id, _, _)| self.exists(call, &id))
                });
            if !later.is_empty() {
                held.push(DamageReport {
                    by: r.by,
                    entries: later,
                });
            }
            if now.is_empty() {
                continue;
            }
            let r = &DamageReport {
                by: r.by,
                entries: now,
            };
            let applies = self.lock().host.on_report(r);
            for a in applies {
                let target = lookup(a.net_id);
                let mut read = None;
                if let Some((id, _, _)) = &target
                    && self.exists(call, id)
                    && let Some(hv) = self.hull
                {
                    let delta = f64::from(a.delta);
                    let _ = call.with(id, |inst, _| {
                        if let Some(before) = inst.get_f64(hv) {
                            if delta > 0.0 {
                                let _ = inst.set_f64(hv, before - delta);
                            }
                            read = inst.get_f64(hv).map(|after| (before, after));
                        }
                    });
                }
                let hull_max = target.as_ref().map_or(0.0, |t| t.1);
                let object = target.as_ref().map_or(0, |t| t.2);
                let after = read.map(|r| r.1);
                {
                    let mut st = self.lock();
                    if DamageSettings::kill_fx_on() {
                        st.kills.claimed(a.net_id, a.claim);
                    }
                    st.host.applied(&a, read, hull_max, object);
                    if let Some(h) = after.filter(|h| *h <= 0.0) {
                        killers.push((a.net_id, a.by, h as f32));
                    }
                    if a.claim > 0 && st.host.judge(&a, after) == Verdict::Refuse {
                        again.push(a.net_id);
                        out.push(Message::KillRefused {
                            net_id: a.net_id,
                            peer: a.by,
                            hull: after.unwrap_or(0.0) as f32,
                            claim: a.claim,
                        });
                    }
                }
            }
        }
        (killers, again, out, held)
    }

    /// The host's acks this frame, and its stuck-at-0 watch.
    /// `running`: the world runs (the stuck watch counts running frames).
    pub fn host_acks(&self, hull_of: &dyn Fn(NetId) -> Option<f32>, running: bool) -> Vec<Message> {
        if !self.sharing() || self.role() != ROLE_HOST {
            return Vec::new();
        }
        let mut out = {
            let mut st = self.lock();
            if running {
                st.kills.frame();
            }
            st.host.acks(hull_of, running)
        };
        out.extend(self.host_status_acks());
        out
    }

    /// The claim rule's bound: confirmed claims whose enemy still lives
    /// [`super::REFUSE_AFTER_FRAMES`] after are refused now (`hull_of`: the
    /// enemy's hull at its last tick). Returns (net ids to send again,
    /// messages).
    pub fn host_overdue(
        &self,
        hull_of: &dyn Fn(NetId) -> Option<f32>,
    ) -> (Vec<NetId>, Vec<Message>) {
        if !self.sharing() || self.role() != ROLE_HOST {
            return (Vec::new(), Vec::new());
        }
        let late = self.lock().host.overdue(hull_of);
        let mut again = Vec::new();
        let mut out = Vec::new();
        for (net_id, peer, hull, claim) in late {
            log_limited!(
                log::Level::Info,
                "damage-claim-refused",
                "shared damage: claim on {net_id} by {peer:?} refused: still alive at hull {hull:.1} after the bound"
            );
            again.push(net_id);
            out.push(Message::KillRefused {
                net_id,
                peer,
                hull,
                claim,
            });
        }
        (again, out)
    }

    /// The host world's outgoing messages: despawns close accounts.
    pub fn host_sent(&self, msgs: &[Message], me: PeerId) {
        if !self.sharing() || self.role() != ROLE_HOST {
            return;
        }
        let mut st = self.lock();
        for m in msgs {
            if let Message::Despawn { net_id, cause } = m {
                let (killed, by_peer) = match cause {
                    DespawnCause::Killed { by } => (true, by.is_some_and(|p| p != me)),
                    DespawnCause::Removed | DespawnCause::Destroyed => (false, false),
                };
                st.host.gone(*net_id, killed, by_peer);
                if killed {
                    st.kills.died(*net_id, by_peer);
                } else {
                    st.kills.gone(*net_id);
                }
                st.status_led.forget(*net_id);
            }
        }
    }

    /// A host enemy's tick (stuck-at-0 watch; key for the death count).
    pub fn host_watch(&self, key: usize, net_id: NetId, hull: f32) {
        if self.sharing() && self.role() == ROLE_HOST {
            let mut st = self.lock();
            st.copies.insert(key, net_id);
            if let Some(s) = st.host.watch(net_id, hull) {
                log_limited!(
                    log::Level::Info,
                    "damage-stuck",
                    "shared damage: stuck: {} (object {}) at hull {:.1} for {HOST_STUCK_FRAMES} running frames after a peer's damage took it to 0 (peer damage applied {:.1})",
                    s.net_id,
                    s.object,
                    s.hull,
                    s.applied
                );
            }
        }
    }

    /// `enemyDestroyed` (an enemy's Destroy event): a second one for a net id
    /// is logged.
    fn on_destroyed(&self, call: &Call<'_>) {
        if !self.sharing() {
            return;
        }
        let Some(me) = call.instance() else { return };
        let key = me.key();
        let host = self.role() == ROLE_HOST;
        let mut st = self.lock();
        st.obs.destroyed += 1;
        let net = st.copies.get(&key).copied();
        if host {
            st.destroying.push(key);
        }
        if let Some(n) = net {
            let now = std::time::Instant::now();
            let fresh = |at: std::time::Instant| now.duration_since(at) < DESTROYED_TWICE_WINDOW;
            if st.obs.destroyed.is_multiple_of(1024) {
                st.obs.destroyed_by_net.retain(|_, (_, at)| fresh(*at));
            }
            let c = st.obs.destroyed_by_net.entry(n).or_insert((0, now));
            if !fresh(c.1) {
                *c = (0, now);
            }
            c.0 += 1;
            if c.0 == 2 {
                log_limited!(
                    log::Level::Info,
                    "damage-destroyed-twice",
                    "shared damage: enemy {n} destroyed a second time (instance {key:#x})"
                );
            }
        }
    }

    /// After `enemyDestroyed` (host): the address no longer names that enemy.
    fn after_destroyed(&self) {
        let mut st = self.lock();
        if let Some(key) = st.destroying.pop() {
            st.copies.remove(&key);
        }
    }

    /// The context of a bracketed call (see [`super::cause`]).
    fn context(&self, call: &Call<'_>, status: bool) -> Ctx {
        if self.host_deaths.load(Ordering::Relaxed) > 0 {
            return Ctx::Host;
        }
        let side = self.player_side.get();
        let me = call.instance();
        let other = call.other();
        let player = |i: &Option<Instance<'_>>| match (side, i) {
            (Some(f), Some(i)) => (f.0)(call, i),
            _ => false,
        };
        if player(&me) || player(&other) {
            return Ctx::Player;
        }
        let Some(key) = me.as_ref().map(Instance::key) else {
            return Ctx::Host;
        };
        let st = self.lock();
        if !st.copies.contains_key(&key) && st.causes.hit_lately(key) {
            return Ctx::Chained;
        }
        if status && st.causes.touched(key) {
            return Ctx::Status;
        }
        Ctx::Host
    }

    fn bracket_before(&self, call: &Call<'_>, status: bool) -> Option<Bracket> {
        if self.role() != ROLE_CLIENT {
            return None;
        }
        let ctx = self.context(call, status);
        let fx = DamageSettings::status_on();
        let (before, before_fx) = self.sweep(call, fx);
        let audited = self.auditing();
        let mut st = self.lock();
        st.causes.enter();
        if fx {
            st.status_causes.enter();
        }
        if audited {
            st.audit.enter();
        }
        Some(Bracket {
            before,
            before_fx,
            ctx,
            audited,
        })
    }

    fn bracket_after(&self, call: &Call<'_>, src: u16, b: Option<Bracket>) {
        let Some(b) = b else { return };
        let fx = DamageSettings::status_on();
        let (mut after, after_fx) = self.sweep(call, fx);
        let restore = {
            let mut st = self.lock();
            let st = &mut *st;
            if fx {
                st.status_causes.leave(src, b.ctx, &b.before_fx, &after_fx);
            }
            let copies = &st.copies;
            st.causes
                .leave(src, b.ctx, &b.before, &after, &|k| copies.contains_key(&k))
        };
        if !restore.is_empty() {
            self.put_back(call, &restore);
            for (k, h) in restore {
                after.insert(k, h);
            }
        }
        if !b.audited {
            return;
        }
        let changed = self.lock().audit.leave(src, &b.before, &after);
        if changed.is_empty() {
            return;
        }
        let Some(solid) = self.object(call, &self.solid, SOLID) else {
            return;
        };
        let mut objs = Vec::new();
        let _ = call.with(&Value::object(solid), |inst, _| {
            if changed.contains(&inst.key())
                && let Some(o) = inst
                    .builtin_value("object_index")
                    .ok()
                    .as_ref()
                    .and_then(crate::world::classify::object_index)
            {
                objs.push((inst.key(), o));
            }
        });
        let mut st = self.lock();
        for (k, o) in objs {
            st.audit.set_object(k, o);
        }
    }

    /// Put copies' hulls back (the host's causes, [`super::cause`]).
    fn put_back(&self, call: &Call<'_>, hulls: &[(usize, f64)]) {
        let (Some(solid), Some(hull)) = (self.object(call, &self.solid, SOLID), self.hull) else {
            return;
        };
        let want: HashMap<usize, f64> = hulls.iter().copied().collect();
        let _ = call.with(&Value::object(solid), |inst, _| {
            if let Some(h) = want.get(&inst.key()) {
                let _ = inst.set_f64(hull, *h);
            }
        });
    }

    /// Every 600 frames: the audit table (with the audit on).
    pub fn log_line(&self, frame: u64) {
        if !frame.is_multiple_of(600) {
            return;
        }
        let mut st = self.lock();
        if self.role() == ROLE_CLIENT && self.settings.audit {
            // Gone damage the host has not spoken of in 600 frames.
            let now = st.frame;
            let stale: Vec<NetId> = st
                .gone_watch
                .iter()
                .filter(|(_, g)| now.saturating_sub(g.0) > 600)
                .map(|(n, _)| *n)
                .collect();
            for n in stale {
                if let Some((_, d, _, _)) = st.gone_watch.remove(&n) {
                    st.gone_after[2] += d;
                }
            }
            let [k, b, w] = st.gone_after;
            log::info!(
                "damage audit gone, what became of it: host killed after {k:.1} (moved to overkill), sent again (lost) {b:.1}, no word {w:.1}, open {:.1}",
                st.gone_watch.values().map(|g| g.1).sum::<f64>()
            );
            log::info!("{}", st.audit.line());
            for l in st.audit.table() {
                log::info!("{l}");
            }
            for l in st.audit.by_object() {
                log::info!("{l}");
            }
        }
    }
}

/// Register the `enemyDestroyed` hook, the client's asteroid death watch and
/// the hull-write brackets. Returns how many sites were hooked.
#[allow(clippy::many_single_char_names)] // hook closures: a, b per site
pub fn register(ctx: &ModContext, d: &Arc<DamageHooks>) -> usize {
    let mut n = 0;
    let mut ok = |name: &str, r: Result<(), ndml_sdk::Error>| match r {
        Ok(()) => n += 1,
        Err(e) => log::warn!("shared damage: {name}: {e}"),
    };
    for name in ASTEROID_STEPS {
        let a = Arc::clone(d);
        ok(
            name,
            ctx.hook_after(name, move |call| a.after_death_path(call)),
        );
    }
    let (a, b) = (Arc::clone(d), Arc::clone(d));
    ok(
        ENEMY_DESTROYED,
        ctx.hook(
            ENEMY_DESTROYED,
            move |call| a.on_destroyed(call),
            move |_, ()| b.after_destroyed(),
        ),
    );
    // Every hull writer is bracketed for the cause rule.
    {
        let mut st = d.lock();
        for &name in AUDIT_SITES {
            let src = st.audit.register(name);
            let status = STATUS_SITES.contains(&name);
            let (a, b) = (Arc::clone(d), Arc::clone(d));
            match ctx.hook(
                name,
                move |call| a.bracket_before(call, status),
                move |call, before| b.bracket_after(call, src, before),
            ) {
                Ok(()) => n += 1,
                Err(e) => log::warn!("shared damage bracket: {name}: {e}"),
            }
        }
    }
    n += effects::register(ctx, d);
    log::info!(
        "shared damage: on (audit {}, kill effects {}, status {}; {n} hook sites)",
        d.settings.audit,
        DamageSettings::kill_fx_on(),
        DamageSettings::status_on()
    );
    n
}
