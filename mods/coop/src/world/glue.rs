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

//! The shared world's hooks and game calls; the decisions are the pure
//! modules'. A GML runtime error inside a mod's call ends the process, so ids
//! are checked with `instance_exists` before id-based calls, only plain values
//! and refs the game gave are passed, a client creates only the host's
//! enemy-family objects and their dependencies, and scripts that need an
//! instance run through `with`. No lock is held across a call into the game.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ndml_net::interp::Vec2;
use ndml_net::msg::{DespawnCause, EnemySpawn, Message, NetId, PeerId, SpawnMarker};
use ndml_sdk::{Builtin, Call, Error, Instance, ModContext, Script, Value, Variable};

use super::WorldSettings;
use super::classify::{self, Class, is_instance, object_index};
use super::client::{self, ClientWorld, Removal, Tick};
use super::entities::{Action, Desired, EntityKey, RemoteEntities};
use super::host::{FirstSight, HostWorld, Seen, TickRead};
use super::lifecycle::{LifeInput, Lifecycle, Loss, Role, Transition};
use super::names::{self, builtin_fn as bf, builtin_var as bv, var};
use super::present::{self, PartnerView};
use super::put_back;
use super::target::Player;
use crate::game::{info_once, warn_once};
use crate::overlay::Line;
use crate::player::sprite_value;
use crate::render::{DrawItem, Gui, View};

/// Area effects: a joining player's ship's per-frame effects on the
/// enemies round it, made on the host round its avatar.
#[path = "area_glue.rs"]
mod area_glue;
/// Corrosion spread on the joining side.
#[path = "corrosion_glue.rs"]
mod corrosion_glue;
/// Beam cover from a partner's Shield Bastion and Arc Barrier drones.
#[path = "cover_glue.rs"]
mod cover_glue;
/// What a world spawn's Destroy event makes stays each game's own.
#[path = "death_fx_glue.rs"]
mod death_fx;
/// What the host's enemies choose of the joining player's things.
#[path = "decoy_glue.rs"]
mod decoy_glue;
/// Dependencies: what replicated enemies refer to that is not an enemy.
#[path = "deps_glue.rs"]
mod deps;
/// Enemies that emerge from the background: ticked after their Step, the
/// joining copy's emerge held behind the host's.
#[path = "emerge_glue.rs"]
mod emerge_glue;
/// Client: references the copies' own code rewrites, kept at the host's value.
#[path = "follow_glue.rs"]
mod follow;
/// Followers a leader's code makes, sent with a no-setup first state.
#[path = "followers_glue.rs"]
mod followers;
/// The Star Eater's lit segments from the host.
#[path = "lit_glue.rs"]
mod lit_glue;
/// The on-death mods (the on-death table): one world effect per death if
/// either player has it; own-build effects for each player who has them.
#[path = "ondeath_glue.rs"]
mod ondeath_glue;
/// Shared pickups: the host's orbs and power-ups on the joining game.
#[path = "orbs_glue.rs"]
mod orbs_glue;
/// Shared enemy shots.
#[path = "shots_glue.rs"]
mod shot_glue;
/// The stand-in bracket (`standin`): on the host, what goes after the
/// joining player finds that player wherever its code names the player.
#[path = "standin_glue.rs"]
mod standin_glue;
pub use shot_glue::ThingLinks;
/// The client's render clock; a child module so it can reach the hooks' state.
#[path = "clock_glue.rs"]
mod clock;
/// Generic state transfer (how every copy is made).
#[path = "gen_glue.rs"]
mod transfer;
/// World view: the host's zoom and background on a client.
#[path = "view_glue.rs"]
mod view;

const ROLE_OFF: u8 = 0;
const ROLE_HOST: u8 = 1;
const ROLE_CLIENT: u8 = 2;

/// What the run tick hands the world each frame (built under the session lock).
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // independent facts about the frame
pub struct WorldInput {
    /// The run's identity (`oController` key, `timeGameStart`); `None` at the menu.
    pub run: Option<(usize, Option<f64>)>,
    /// The run started linked.
    pub coop_run: bool,
    /// A partner is linked.
    pub linked: bool,
    /// This game hosts.
    pub is_host: bool,
    /// This game's peer id.
    pub me: PeerId,
    /// The start gate has released (`Go`).
    pub gate_running: bool,
    /// The last disconnect since the previous frame, if any.
    pub loss: Option<Loss>,
    /// This frame's time.
    pub now: Instant,
    /// The local wire clock (snapshot time).
    pub now_ms: u32,
    /// Host-clock time to show enemies at (client).
    pub render_time: Option<Duration>,
    /// The view (`viewX/Y/Width/Height` globals), logged at a run's start.
    pub view: Option<View>,
    /// The partners in the run.
    pub partners: Vec<PartnerView>,
    /// `oPowers.cloaked` of the local ship.
    pub own_cloaked: bool,
    /// The local ship is down (the session rules): shared pickups wait.
    pub own_down: bool,
    /// Host messages for the world since the last frame.
    pub inbox: Vec<Message>,
    /// Steps per second (`room_speed`).
    pub sps: f64,
    /// Client: the host's clock now (estimate); `None` before the clock syncs.
    pub host_now: Option<Duration>,
}

#[derive(Debug, Clone)]
struct Handles {
    instance_create: Option<Script>,
    create_explosion: Option<Script>,
    exists: Option<Builtin>,
    destroy: Option<Builtin>,
    activate: Option<Builtin>,
    deactivate: Option<Builtin>,
    object_exists: Option<Builtin>,
    is_ancestor: Option<Builtin>,
    asset: Option<Builtin>,
    event_user: Option<Builtin>,
    alarm_get: Option<Builtin>,
    alarm_set: Option<Builtin>,
    var_set: Option<Builtin>,
    sprite_exists: Option<Builtin>,
    object_name: Option<Builtin>,
    object_parent: Option<Builtin>,
    instance_number: Option<Builtin>,
    /// The reflection builtins of the generic state transfer.
    reflect: Option<super::state::Reflect>,
    leader: Option<Variable>,
    /// The effect pool's flag (`oEffectPooled`: in its pool, deactivated
    /// for reuse; `followers_glue.rs`'s "this copy's, live and drawn").
    in_pool: Option<Variable>,
    hull: Option<Variable>,
    hull_max: Option<Variable>,
    hull_previous: Option<Variable>,
    rarity: Option<Variable>,
    target: Option<Variable>,
    target_x: Option<Variable>,
    target_y: Option<Variable>,
    target_hs: Option<Variable>,
    target_vs: Option<Variable>,
    /// The type an enemy prefers to aim at (`chooseTarget`'s first step:
    /// the Tracer's own ball, the Dreadnought's ball), else -4.
    preferred: Option<Variable>,
    wave: Option<Variable>,
    spawn_pos_x: Option<Variable>,
    spawn_pos_y: Option<Variable>,
    scaler: Option<Variable>,
    buffer: Option<Variable>,
    enemy_type: Option<Variable>,
    /// An enemy beam's or charge-up's `shooter` (what a wind-up follows,
    /// `windup.rs`).
    shooter: Option<Variable>,
    /// Shared enemy shots: a shot's turn rate (homing above 0).
    rot_speed: Option<Variable>,
    /// The emerging family's gate (`emerge_glue`).
    emerged: Option<Variable>,
    /// The field's size, `viewWidth` and `viewHeight` (a turn's birth
    /// matched across its wrap, `emerge_glue`).
    field: [Option<Variable>; 2],
}

fn opt<T>(r: Result<T, Error>) -> Option<T> {
    r.map_err(|e| log::warn!("co-op world: {e}; the part that needs it is off"))
        .ok()
}

impl Handles {
    fn resolve(ctx: &ModContext) -> Self {
        let s = |n| opt(ctx.script(n));
        let b = |n| opt(ctx.builtin(n));
        let v = |n| opt(ctx.variable(n));
        Self {
            instance_create: s(names::INSTANCE_CREATE),
            create_explosion: s(names::CREATE_EXPLOSION),
            exists: b(bf::INSTANCE_EXISTS),
            destroy: b(bf::INSTANCE_DESTROY),
            activate: b(bf::INSTANCE_ACTIVATE_OBJECT),
            deactivate: b(bf::INSTANCE_DEACTIVATE_OBJECT),
            object_exists: b(bf::OBJECT_EXISTS),
            is_ancestor: b(bf::OBJECT_IS_ANCESTOR),
            asset: b(bf::ASSET_GET_INDEX),
            event_user: b(bf::EVENT_USER),
            alarm_get: b(bf::ALARM_GET),
            alarm_set: b(bf::ALARM_SET),
            var_set: b(bf::VARIABLE_INSTANCE_SET),
            sprite_exists: b(bf::SPRITE_EXISTS),
            object_name: b(bf::OBJECT_GET_NAME),
            object_parent: b(bf::OBJECT_GET_PARENT),
            instance_number: b(bf::INSTANCE_NUMBER),
            reflect: super::state::Reflect::resolve(ctx),
            leader: v(var::LEADER),
            in_pool: v("inPool"),
            hull: v(crate::names::VAR_HULL),
            hull_max: v(crate::names::VAR_HULL_MAX),
            hull_previous: v(crate::names::VAR_HULL_PREVIOUS),
            rarity: v(var::RARITY),
            target: v(var::TARGET),
            target_x: v(var::TARGET_X),
            target_y: v(var::TARGET_Y),
            target_hs: v(var::TARGET_HSPEED),
            target_vs: v(var::TARGET_VSPEED),
            preferred: v("preferredTargetType"),
            wave: v(var::WAVE),
            spawn_pos_x: v(var::SPAWN_POS_X),
            spawn_pos_y: v(var::SPAWN_POS_Y),
            scaler: v(var::SCALER),
            buffer: v(var::BUFFER),
            enemy_type: v(var::ENEMY_TYPE),
            shooter: v(var::SHOOTER),
            rot_speed: v(super::shots::TURN_RATE),
            emerged: v(emerge_glue::EMERGED),
            field: [
                v(crate::names::GLOBAL_VIEW[2]),
                v(crate::names::GLOBAL_VIEW[3]),
            ],
        }
    }

    /// What the world can't run without.
    fn essential(&self) -> bool {
        self.instance_create.is_some()
            && self.exists.is_some()
            && self.destroy.is_some()
            && self.is_ancestor.is_some()
            && self.object_exists.is_some()
            && self.asset.is_some()
            && self.hull.is_some()
            && self.hull_max.is_some()
            && self.hull_previous.is_some()
    }
}

/// Object indices resolved by name at the first run frame.
#[derive(Debug, Clone)]
struct Objects {
    solid: u32,
    collectable: u32,
    standard: Option<u32>,
    segment: Option<u32>,
    target: Option<u32>,
    /// `oEnemyEmerging` ([`classify::parent::EMERGING`]).
    emerging: Option<u32>,
    waves: Option<u32>,
    player_attack: Option<u32>,
    /// `oPlayer` (no instance active = the game is frozen).
    player: Option<u32>,
    warning: Option<u32>,
    portal: Option<u32>,
    spawner: Option<u32>,
    /// `oSpawner`, the spawn machinery's parent ([`Class::Spawner`]).
    spawner_root: Option<u32>,
    /// `oAsteroid` (asteroid copies die through their own Step).
    asteroid: Option<u32>,
    /// Enemy shot parents.
    shots: Vec<u32>,
    /// Beam charge-ups ([`classify::parent::ENEMY_CHARGES`]).
    charges: Vec<u32>,
    /// `oEnemyAoE` (a scratch area goes silently, `shot_remove`).
    area: Option<u32>,
    /// Roots of what stays each game's own by ancestry
    /// ([`classify::parent::LOCAL_ROOTS`]): never a dependency.
    local_roots: Vec<u32>,
    /// `oEffect` and `oEffectPooled` (the wind-up rule, `windup.rs`).
    effect: Option<u32>,
    pooled: Option<u32>,
}

#[derive(Debug, Clone, Default)]
struct LocalShip {
    key: usize,
    id: Option<Value>,
    pos: Vec2,
    /// px per step.
    vel: Vec2,
    visible: bool,
    depth: Option<f64>,
    seen_frame: u64,
}

#[derive(Debug, Default)]
struct WorldState {
    life: Lifecycle,
    host: HostWorld,
    client: ClientWorld,
    ents: RemoteEntities,
    classes: HashMap<u32, Class>,
    objects: Option<Objects>,
    objects_tried: bool,
    /// Dependencies.
    deps: deps::Deps,
    /// World view.
    view: view::ViewState,
    /// Area effects.
    area: area_glue::AreaGlue,
    /// Beam cover from a partner's Bastion and drones.
    cover: cover_glue::CoverGlue,
    /// The joining player's decoys for the host's enemies.
    decoy: decoy_glue::DecoyGlue,
    /// On-death mods: the players' parts, the host's lends, parts handed over.
    ondeath: ondeath_glue::OnDeathGlue,
    /// The Star Eater's lit segments from the host.
    lit: lit_glue::LitGlue,
    /// Client: copies whose own Step runs on the host's hull now: (key) ->
    /// (their hull before, the host's written for the Step).
    step_swaps: HashMap<usize, (f64, f64)>,
    /// Shared pickups.
    orbs: orbs_glue::OrbsGlue,
    /// Client: the puppet being built (parts created now are its own).
    owner: Option<NetId>,
    /// Client: the copy whose set-up is running (its creates are its own).
    building: Option<usize>,
    me: Option<PeerId>,
    local: LocalShip,
    partners: Vec<PartnerView>,
    gate_running: bool,
    /// The last run step found the game frozen ([`WorldHooks::frozen`]).
    frozen: bool,
    /// Client: emerging copies whose emerge is held in the Step running now
    /// (`emerge_glue`).
    emerge_held: Vec<usize>,
    /// Host: damage reports that arrived while frozen (a deactivated enemy
    /// cannot take a hull write); they wait for the first running frame.
    held_reports: Vec<ndml_net::msg::DamageReport>,
    /// Host: status report entries held the same way (frozen, or the enemy
    /// deactivated), the newest per (peer, enemy).
    held_status: ndml_net::status::StatusHold,
    /// Client: the frame the last freeze ended (for the stray log).
    resumed_at: u64,
    /// Client: enemies made here that no table took (key -> maker object,
    /// frame made, made while frozen): named when they turn up as strays.
    untracked: HashMap<usize, (Option<u32>, u64, bool)>,
    /// Client: strays removed, and the first ones logged in full.
    strays: u64,
    sps: f64,
    frame: u64,
    wave_ms: Option<u32>,
    /// Client: the host's wave timer at its last `WaveState` (ms;
    /// [`names::WAVE_TIMER_OFF`]: not running).
    host_wave_ms: Option<u32>,
    /// Host: telegraphs the spawner made this frame (id, portal), read and
    /// sent at the next run step.
    new_markers: Vec<(Value, bool)>,
    /// The client's render clock.
    repl: clock::ReplState,
    /// Client: copies dying the host's death (key -> frame): asteroids
    /// through their own Step, the others through their Destroy event. Left
    /// alone by the Step hold; their vetoed debris and drops not logged.
    dying: HashMap<usize, u64>,
    /// Client: how many copies' Destroy events are running now (nested when
    /// one destroys another): [`HOST_DEATH_ONLY`](names::HOST_DEATH_ONLY)
    /// and `checkDestroy*` are skipped, what they make is scratch.
    copy_deaths: u32,
    /// Client: copies removed in the last 30 frames (key -> net id, frame),
    /// named in the stray log.
    removed: HashMap<usize, (NetId, u64)>,
    /// Client: the copies' poses this frame, as put in the controller's Step,
    /// for putting them there again at the start of the End Step.
    early_poses: Vec<(Value, Vec2, f32)>,
    /// Client: the pose each copy was given at its End Step tick this frame
    /// (id, key, object, pose, extrapolated, ticked at its own End Step), put
    /// back before the draw if the copy's own code moved it after the tick.
    tick_poses: Vec<(Value, usize, u32, put_back::Pose, bool, bool)>,
    /// Client: last frame's copies by key, for a part riding its leader's
    /// last tick ([`put_back::plan`]); empty after a frame with no draw.
    prev_ticks: HashMap<usize, put_back::Last>,
    /// Client: whether an object is an asteroid (cached).
    asteroids: HashMap<u32, bool>,
    /// Generic state transfer.
    transfer: transfer::GenState,
    /// Client: references followed.
    follow: follow::FollowState,
    /// Followers a leader's code makes (`followers_glue.rs`).
    followers: followers::FollowersState,
    /// Shared enemy shots.
    shots: shot_glue::ShotsState,
}

/// Counters the hot hooks keep without the world lock.
#[derive(Debug, Default)]
struct Meter {
    /// Client: copies whose hull was at or under 0 before their own Step or
    /// Begin Step while the host's lived as shown here, run on the host's
    /// (the first ones are logged).
    held_step: AtomicU64,
    /// Client: copies' Steps run on the host's hull whose event has not
    /// ended yet (see `step_swap_before`).
    step_swaps_live: AtomicU64,
}

/// The shared world as the hooks see it. Owned by `lib.rs`'s shared state.
#[derive(Debug)]
pub struct WorldHooks {
    settings: WorldSettings,
    /// Every essential handle resolved: the shared world runs. Not: it stays
    /// out (one warning at start) and each game plays its own world.
    on: bool,
    role: AtomicU8,
    replicating: AtomicBool,
    st: Mutex<WorldState>,
    h: Handles,
    meter: Meter,
    /// Host: instances whose Destroy event ran since the last run step
    /// (keys), from the event observer. Its own lock, held only to push or
    /// drain: the observer never takes the world's (a Destroy event can run
    /// inside any GML call, the mod's own included).
    destroyed_seen: Mutex<Vec<usize>>,
    /// Shared damage (`crate::damage`): measurement, reports, prediction.
    pub(crate) dmg: Arc<crate::damage::glue::DamageHooks>,
    /// Shared enemy shots' own flags.
    shot: shot_glue::ShotHooks,
    /// Joining game: the host's snapshots taken and the newest stamp, as of
    /// the last run step (`ClientWorld::host_snapshots`), read without the
    /// world's lock for the slow-frames line.
    host_snaps: (AtomicU64, AtomicU32),
    /// The objects that emerge from the background, from the object tree
    /// once the objects are resolved (`emerge_glue`); read by the Step
    /// observer without the world's lock.
    emerging: std::sync::OnceLock<Vec<u32>>,
}

fn read(inst: &Instance<'_>, name: &str) -> Option<f32> {
    inst.builtin_f64(name)
        .filter(|v| v.is_finite())
        .map(|v| v as f32)
}

/// The game's own pick for an enemy, as the `chooseTarget` after-hook sees
/// it ([`WorldHooks::own_pick`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OwnPick {
    /// An instance of the type the enemy prefers (`preferredTargetType`,
    /// or a descendant).
    pub preferred: bool,
    /// A player's thing here: an avatar, or player-side by the mirror's
    /// rule (a ship, a construct, a decoy, a proxy).
    pub player: bool,
}

/// What the `chooseTarget` after-hook does for an enemy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterHook {
    /// Assigned to this game's player: the game's own choice (cloak,
    /// decoys, constructs as in solo play).
    GamesOwn,
    /// Assigned to a remote player, the game's pick its own thing: kept.
    KeepOwnThing,
    /// Assigned to a remote player: pointed at that player's avatar.
    Avatar,
}

/// The pick is the enemy's own thing: the type it prefers, not a player's
/// (the Tracer aiming at its own ball for its combo, the Dreadnought at its
/// ball). A ship, a construct, a decoy, an egg or a proxy never is.
#[must_use]
pub(super) fn keeps_own_pick(pick: Option<OwnPick>) -> bool {
    pick.is_some_and(|p| p.preferred && !p.player)
}

/// The after-hook's rule: an enemy assigned to a remote player keeps the
/// game's pick when it is the enemy's own thing (as the shots' rule keeps
/// an orb feeder steering at its ball), and is pointed at that player's
/// avatar otherwise; one assigned to this game's player is left alone.
#[must_use]
pub(super) fn after_hook(remote: bool, pick: Option<OwnPick>) -> AfterHook {
    if !remote {
        AfterHook::GamesOwn
    } else if keeps_own_pick(pick) {
        AfterHook::KeepOwnThing
    } else {
        AfterHook::Avatar
    }
}

fn object_of(inst: &Instance<'_>) -> Option<u32> {
    inst.builtin_value(bv::OBJECT_INDEX)
        .ok()
        .as_ref()
        .and_then(object_index)
}

fn finite2(x: f32, y: f32) -> bool {
    x.is_finite() && y.is_finite()
}

/// The mirror draws a follower (a hit flash, a glow) on the local instance of the
/// replicated instance it follows: the host's tracked enemies, a client's
/// live copies.
impl crate::mirror::glue::WorldLinks for WorldHooks {
    fn net_of(&self, key: usize, id: &Value) -> Option<NetId> {
        match self.role() {
            ROLE_HOST => self.lock().host.net_id_for(key, id),
            ROLE_CLIENT => self.lock().client.net_id_for(key, id),
            _ => None,
        }
    }

    fn instance_of(&self, net: NetId) -> Option<Value> {
        match self.role() {
            ROLE_HOST => self.lock().host.by_net_id(net).map(|(id, _)| id),
            ROLE_CLIENT => self.lock().client.id_of(net),
            _ => None,
        }
    }

    fn view_shift(&self) -> Option<(f64, f64)> {
        self.ending_view_shift()
    }
}

impl WorldHooks {
    /// Resolve the handles. The hooks are registered by [`register`].
    #[must_use]
    pub fn new(ctx: &ModContext, settings: WorldSettings) -> Self {
        let h = Handles::resolve(ctx);
        let damage_settings = settings.damage.clone();
        let on = h.essential();
        if !on {
            log::warn!(
                "co-op world: unable to find a script, builtin or variable; shared world off (each game plays its own world)"
            );
        }
        Self {
            settings,
            on,
            role: AtomicU8::new(ROLE_OFF),
            replicating: AtomicBool::new(false),
            st: Mutex::new(WorldState {
                sps: 60.0,
                ..WorldState::default()
            }),
            h,
            meter: Meter::default(),
            destroyed_seen: Mutex::new(Vec::new()),
            host_snaps: (AtomicU64::new(0), AtomicU32::new(0)),
            emerging: std::sync::OnceLock::new(),
            dmg: Arc::new(crate::damage::glue::DamageHooks::new(ctx, damage_settings)),
            shot: shot_glue::ShotHooks::default(),
        }
    }

    /// Whether the shared world runs (its essential handles found).
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// The settings in use.
    #[must_use]
    pub fn settings(&self) -> &WorldSettings {
        &self.settings
    }

    /// Before the controller's End Step: the ending's partner ship placed
    /// again from this ship as it will be drawn; how far it moved (the
    /// mirror moves its shifted entries by the same).
    pub fn ending_end_step(&self, call: &Call<'_>) -> Option<(f64, f64)> {
        self.view_ending_end_step(call)
    }

    /// The world lock, tracked ([`crate::game::lock_tracked`]): a hook that
    /// runs inside a game call made while it is held (a Destroy, Create or
    /// user event, a `with`) is named and skipped, never left waiting.
    #[track_caller]
    fn lock(&self) -> crate::game::Tracked<'_, WorldState> {
        crate::game::lock_tracked(&self.st, "world")
    }

    fn role(&self) -> u8 {
        self.role.load(Ordering::Relaxed)
    }

    fn exists(&self, call: &Call<'_>, id: &Value) -> bool {
        let Some(b) = self.h.exists else {
            return false;
        };
        call.call_builtin(b, std::slice::from_ref(id))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Whether the instance `id` is active, deactivated by the game (asleep:
    /// `instance_exists` is false for it, as for a destroyed one) or gone.
    /// A deactivated one is told apart by activating it and deactivating it
    /// again at once (no event of it runs in between: this is the run step).
    pub(super) fn presence(&self, call: &Call<'_>, id: &Value) -> Presence {
        if self.exists(call, id) {
            return Presence::Active;
        }
        let (Some(a), Some(d)) = (self.h.activate, self.h.deactivate) else {
            return Presence::Gone;
        };
        let _ = call.call_builtin(a, std::slice::from_ref(id));
        if !self.exists(call, id) {
            return Presence::Gone;
        }
        let _ = call.call_builtin(d, std::slice::from_ref(id));
        Presence::Asleep
    }

    /// Client, after the game's `activateRegion` (leaving a pause or the
    /// upgrade screen): every copy not asleep with the host's is activated at
    /// once, before any of them steps (the region is this game's view).
    pub(super) fn wake_copies(&self, call: &Call<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let ids = self.lock().client.awake_ids();
        for id in &ids {
            if !self.exists(call, id) {
                self.set_asleep(call, id, false);
            }
        }
        // Copies held born incomplete, and the effects that follow them, stay
        // deactivated if the region brought them back: the next run step
        // decides on them again.
        let held = self.gen_held_now();
        for id in &held {
            if self.exists(call, id) {
                self.set_asleep(call, id, true);
            }
        }
        // A despawn that came during the freeze is carried out now, before
        // anything steps or draws.
        let due = self.lock().client.despawn_waiting();
        for id in &due {
            if self.exists(call, id) {
                let _ = call.with(id, |inst, _| self.client_tick(call, &inst));
            }
        }
    }

    /// Deactivate (`asleep`) or activate a local instance.
    pub(super) fn set_asleep(&self, call: &Call<'_>, id: &Value, asleep: bool) {
        let b = if asleep {
            self.h.deactivate
        } else {
            self.h.activate
        };
        if let Some(b) = b {
            let _ = call.call_builtin(b, std::slice::from_ref(id));
        }
    }

    fn destroy_silently(&self, call: &Call<'_>, id: &Value) {
        // An instance a freeze deactivated is invisible to `instance_exists`
        // and `instance_destroy`: activate it first, or the game's own
        // reactivation brings it back, drawn and never updated.
        if !self.exists(call, id)
            && let Some(a) = self.h.activate
        {
            let _ = call.call_builtin(a, std::slice::from_ref(id));
        }
        if !self.exists(call, id) {
            return;
        }
        if let Some(b) = self.h.destroy
            && let Err(e) = call.call_builtin(b, &[id.clone(), Value::Bool(false)])
        {
            warn_once(
                "world-destroy",
                &format!("co-op world: instance_destroy: {e}"),
            );
        }
    }

    /// The game is frozen: neither `oPlayer` nor `oWaves` is active (pause,
    /// the upgrade screen and the rules' holds deactivate every gameplay
    /// instance). A ship destroyed at the end of a run leaves `oWaves` active.
    pub fn frozen(&self, call: &Call<'_>) -> bool {
        let (player, waves) = {
            let st = self.lock();
            let o = st.objects.as_ref();
            (o.and_then(|o| o.player), o.and_then(|o| o.waves))
        };
        player.is_some_and(|p| self.instances(call, p) == 0)
            && waves.is_none_or(|w| self.instances(call, w) == 0)
    }

    /// The frozen state the last run step saw.
    pub fn was_frozen(&self) -> bool {
        self.lock().frozen
    }

    /// The slow-frames line (`crate::slow_log`): the host's snapshots taken
    /// and the newest stamp, on a joining game that has had one (no lock).
    #[must_use]
    pub fn host_snapshots(&self) -> Option<(u64, u32)> {
        let n = self.host_snaps.0.load(Ordering::Relaxed);
        (self.role() == ROLE_CLIENT && n > 0)
            .then(|| (n, self.host_snaps.1.load(Ordering::Relaxed)))
    }

    /// The slow-frames line: the wave (`global.wave`), the enemies this
    /// game's table holds (the host's tracked ones, a joining game's
    /// copies) and the shared enemy shots live here. One lock, no walk.
    #[must_use]
    pub fn slow_scene(&self, call: &Call<'_>) -> (Option<u32>, usize, usize) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a wave number
        let wave = self
            .h
            .wave
            .and_then(|v| call.global()?.get_f64(v))
            .filter(|w| w.is_finite() && *w >= 0.0)
            .map(|w| w as u32);
        let role = self.role();
        let st = self.lock();
        let (enemies, shots) = match role {
            ROLE_HOST => (st.host.len(), st.shots.host.live.len()),
            ROLE_CLIENT => (st.client.live_count(), st.shots.client.live()),
            _ => (0, 0),
        };
        (wave, enemies, shots)
    }

    /// Sounds: this game's part in the shared world this frame.
    #[must_use]
    pub fn sound_role(&self) -> crate::sound::Role {
        match self.role() {
            ROLE_HOST => crate::sound::Role::Host,
            ROLE_CLIENT => crate::sound::Role::Joining,
            _ => crate::sound::Role::Off,
        }
    }

    /// Sounds, joining: the host-clock ms the world shows the host's enemies
    /// at this frame (the host's world sounds play at it, with the deaths
    /// drawn). `None` before the first frame or while this thread holds the
    /// world lock.
    #[must_use]
    pub fn sound_shown_ms(&self) -> Option<u32> {
        if crate::game::held_at("world").is_some() {
            return None;
        }
        self.lock().repl.shown_ms
    }

    /// Sounds, host: the enemy at instance `key` dies by a partner's
    /// confirmed, still open claim (the partner's game plays its sounds).
    /// `false` while this thread holds the world lock.
    #[must_use]
    pub fn dying_by_partner_claim(&self, key: usize) -> bool {
        if self.role() != ROLE_HOST || crate::game::held_at("world").is_some() {
            return false;
        }
        let net_id = self.lock().host.net_id(key);
        net_id.is_some_and(|n| self.dmg.claimed_now(n))
    }

    /// Sounds, joining: the copy at instance `key` dies by this game's own
    /// claim (a predicted death, [`crate::damage::glue::DamageHooks::dying_now`]).
    #[must_use]
    pub fn dying_by_own_claim(&self, key: usize) -> bool {
        self.role() == ROLE_CLIENT && self.dmg.dying_now(key)
    }

    /// Sounds: the instance `id` (a loop's caller) is destroyed, not merely
    /// deactivated (`Self::presence`; takes no lock). Call it from the run
    /// step only.
    #[must_use]
    pub fn sound_owner_gone(&self, call: &Call<'_>, id: &Value) -> bool {
        matches!(self.presence(call, id), Presence::Gone)
    }

    /// `instance_number(obj)` (0 if it fails).
    fn instances(&self, call: &Call<'_>, obj: u32) -> u32 {
        self.h
            .instance_number
            .and_then(|b| call.call_builtin(b, &[Value::object(obj)]).ok())
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map_or(0, |n| n as u32)
    }

    /// `object_get_name` for log lines (the index if that fails).
    fn object_name(&self, call: &Call<'_>, obj: u32) -> String {
        self.h
            .object_name
            .and_then(|b| call.call_builtin(b, &[Value::object(obj)]).ok())
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| obj.to_string())
    }

    fn asset(&self, call: &Call<'_>, name: &str) -> Option<u32> {
        let v = call.call_builtin(self.h.asset?, &[name.into()]).ok()?;
        object_index(&v)
    }

    /// Resolve object names (once; needs a running game).
    fn ensure_objects(&self, call: &Call<'_>) -> bool {
        {
            let st = self.lock();
            if st.objects.is_some() || st.objects_tried {
                return st.objects.is_some();
            }
        }
        let solid = self.asset(call, classify::parent::SOLID);
        let collectable = self.asset(call, classify::parent::COLLECTABLE);
        let objects = if let (Some(solid), Some(collectable)) = (solid, collectable) {
            let o = Objects {
                solid,
                collectable,
                standard: self.asset(call, classify::parent::ENEMY_STANDARD),
                segment: self.asset(call, classify::parent::SEGMENT),
                target: self.asset(call, classify::parent::TARGET),
                emerging: self.asset(call, classify::parent::EMERGING),
                waves: self.asset(call, names::object::WAVES),
                player_attack: self.asset(call, names::object::PLAYER_ATTACK),
                player: self.asset(call, names::object::PLAYER),
                warning: self.asset(call, names::object::SPAWN_WARNING),
                portal: self.asset(call, names::object::ENEMY_PORTAL),
                spawner: self.asset(call, names::object::ENEMY_SPAWNER),
                spawner_root: self.asset(call, classify::parent::SPAWNER),
                asteroid: self.asset(call, names::object::ASTEROID),
                shots: classify::parent::ENEMY_SHOTS
                    .iter()
                    .filter_map(|n| self.asset(call, n))
                    .collect(),
                charges: classify::parent::ENEMY_CHARGES
                    .iter()
                    .filter_map(|n| self.asset(call, n))
                    .collect(),
                area: self.asset(call, classify::parent::ENEMY_AREA),
                local_roots: classify::parent::LOCAL_ROOTS
                    .iter()
                    .filter_map(|n| self.asset(call, n))
                    .collect(),
                effect: self.asset(call, classify::parent::EFFECT),
                pooled: self.asset(call, classify::parent::EFFECT_POOLED),
            };
            log::info!(
                "co-op world: objects resolved (oSolid {}, oCollectable {}, oPlayerAttack {:?})",
                o.solid,
                o.collectable,
                o.player_attack
            );
            Some(o)
        } else {
            log::warn!("co-op world: unable to find oSolid/oCollectable; shared world off");
            None
        };
        let mut st = self.lock();
        st.objects_tried = true;
        st.objects = objects;
        st.objects.is_some()
    }

    fn ancestor(&self, call: &Call<'_>, child: u32, parent: Option<u32>) -> bool {
        let (Some(p), Some(b)) = (parent, self.h.is_ancestor) else {
            return false;
        };
        child == p
            || call
                .call_builtin(b, &[Value::object(child), Value::object(p)])
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    }

    /// `object_exists(obj)` (false without the builtin).
    pub(super) fn object_known(&self, call: &Call<'_>, obj: u32) -> bool {
        self.h.object_exists.is_some_and(|b| {
            call.call_builtin(b, &[Value::object(obj)])
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
    }

    /// The class of an object the game is using (cached). `check`: first ask
    /// `object_exists` (for indices from the network).
    fn class_of(&self, call: &Call<'_>, obj: u32, check: bool) -> Option<Class> {
        let objs = {
            let st = self.lock();
            if let Some(c) = st.classes.get(&obj) {
                return Some(*c);
            }
            st.objects.clone()?
        };
        if check {
            let ok = call
                .call_builtin(self.h.object_exists?, &[Value::object(obj)])
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !ok {
                return None;
            }
        }
        let class = if Some(obj) == objs.warning {
            Class::Marker { portal: false }
        } else if Some(obj) == objs.portal {
            Class::Marker { portal: true }
        } else if let Some(enemy) = {
            let solid = self.ancestor(call, obj, Some(objs.solid));
            let of = if solid {
                classify::EnemyAncestry {
                    standard: self.ancestor(call, obj, objs.standard),
                    segment: self.ancestor(call, obj, objs.segment),
                    target: self.ancestor(call, obj, objs.target),
                }
            } else {
                classify::EnemyAncestry::default()
            };
            let emerging = !solid && self.ancestor(call, obj, objs.emerging);
            Class::enemy_by_ancestry(solid, emerging, of)
        } {
            enemy
        } else if self.ancestor(call, obj, Some(objs.collectable)) {
            Class::Collectable
        } else if objs
            .shots
            .iter()
            .any(|&p| self.ancestor(call, obj, Some(p)))
        {
            Class::EnemyShot
        } else if objs
            .spawner_root
            .is_some_and(|s| s == obj || self.ancestor(call, obj, Some(s)))
        {
            // By the parent, not the name: every child of `oSpawner`.
            Class::Spawner
        } else {
            Class::Other
        };
        self.lock().classes.insert(obj, class);
        Some(class)
    }

    /// Every object whose instances on a client exist only because of the
    /// host's enemies: `oSolid` and enemy-shot ancestry (the GML guard's
    /// wider scope). Objects are numbered from 0 with no gaps; a run of
    /// missing indices ends the walk.
    fn enemy_side_objects(&self, call: &Call<'_>) -> Vec<i32> {
        let mut out = Vec::new();
        let mut missing = 0;
        for obj in 0..4096u32 {
            match self.class_of(call, obj, true) {
                None => {
                    missing += 1;
                    if missing >= 64 {
                        break;
                    }
                    continue;
                }
                Some(Class::Enemy { .. } | Class::EnemyShot) => {
                    out.extend(i32::try_from(obj));
                }
                Some(_) => {}
            }
            missing = 0;
        }
        out
    }

    /// This frame's role, for the hooks that read it without the lock and
    /// for the shared damage.
    fn publish_role(&self, role: Role, me: PeerId) {
        self.role.store(
            match role {
                Role::Off => ROLE_OFF,
                Role::Host => ROLE_HOST,
                Role::Client => ROLE_CLIENT,
            },
            Ordering::Relaxed,
        );
        self.dmg.set_role(
            match role {
                Role::Off => crate::damage::glue::ROLE_OFF,
                Role::Host => crate::damage::glue::ROLE_HOST,
                Role::Client => crate::damage::glue::ROLE_CLIENT,
            },
            me,
        );
    }

    /// The run tick (`oController_Step_0`, after the session's own work).
    /// Returns the messages to broadcast.
    pub fn run_step(&self, call: &Call<'_>, mut input: WorldInput) -> Vec<Message> {
        if !self.on {
            return Vec::new();
        }
        // Objects are resolved only once a co-op run needs them (a solo run is
        // never touched).
        let ready = !(input.coop_run && input.linked) || self.ensure_objects(call);
        // Last frame's tick poses, if no draw took them (bounded); with no
        // draw, the frame before is no longer the last one.
        {
            let mut st = self.lock();
            if !st.tick_poses.is_empty() {
                st.prev_ticks.clear();
            }
            st.tick_poses.clear();
        }
        let (transition, role, interrupted) = {
            let mut st = self.lock();
            let t = st.life.update(&LifeInput {
                run: input.run,
                coop_run: input.coop_run,
                linked: input.linked,
                is_host: input.is_host,
                enabled: ready,
                loss: input.loss,
            });
            (t, st.life.role(), st.life.interrupted())
        };
        if matches!(transition, Transition::Start(_)) {
            log::info!(
                "co-op world: view (viewX/Y/Width/Height) {:?} at the start (the game wraps; the mod does not)",
                input.view
            );
        }
        self.on_transition(call, transition, input.is_host);
        crate::guard::role_frame(call, role == Role::Client, || self.enemy_side_objects(call));
        self.publish_role(role, input.me);
        if role == Role::Off {
            self.off_step(call);
            return Vec::new();
        }
        let gate_rose = {
            let mut st = self.lock();
            st.me = Some(input.me);
            st.sps = if input.sps.is_finite() && input.sps > 1.0 {
                input.sps
            } else {
                60.0
            };
            st.frame += 1;
            st.partners.clone_from(&input.partners);
            let edge = input.gate_running && !st.gate_running;
            st.gate_running = input.gate_running;
            edge
        };
        let frozen = self.note_frozen(call, role);
        let render_time = self.repl_frame_begin(&input, role);
        let (me, now_ms) = (input.me, input.now_ms);
        let role_u8 = self.role();
        let view = input.view;
        self.shots_inbox(call, &mut input.inbox, role_u8);
        self.area_inbox(&mut input.inbox);
        self.facts_inbox(&mut input.inbox);
        self.lit_inbox(&mut input.inbox);
        let mut out = match role {
            Role::Host => self.host_frame(call, &input, gate_rose),
            Role::Client => self.client_frame(call, input.inbox, render_time),
            Role::Off => Vec::new(),
        };
        if role == Role::Client {
            out.extend(self.area_client(call, me, now_ms));
            self.area_partner(call);
            out.extend(self.orbs_client(call, me, input.own_down));
        } else if role == Role::Host {
            out.extend(self.area_client(call, me, now_ms));
        }
        out.extend(self.shots_run_step(role_u8, view, !interrupted));
        out.extend(self.facts_run_step(call, me));
        out.extend(self.lit_run_step());
        // Frozen (pause, upgrade screen, a rules hold): the game has
        // deactivated every gameplay instance and makes none; the avatar is
        // neither activated, moved, made nor removed until the next running
        // frame (what changed meanwhile is still in the tables then).
        self.entities_step(call, &input.partners, !interrupted && !frozen);
        {
            let mut st = self.lock();
            let frame = st.frame;
            st.dying.retain(|_, f| *f + 30 >= frame);
            st.removed.retain(|_, (_, f)| *f + 30 >= frame);
        }
        let frame = self.lock().frame;
        self.dmg.log_line(frame);
        out
    }

    /// The avatars, when `running`; in the ending the partner's ship drawn
    /// in its formation slot from this game's own ending ship (the avatar
    /// here, the mirror's entries through `WorldLinks::view_shift`).
    fn entities_step(&self, call: &Call<'_>, partners: &[PartnerView], running: bool) {
        let shifted = self
            .view_ending_shift(call)
            .and_then(|_| self.shifted_partners(partners));
        if running {
            self.entities_frame(call, shifted.as_deref().unwrap_or(partners));
        }
        // The stand-in plan, from the avatars just moved.
        self.standin_run_step(call, running);
    }

    fn on_transition(&self, call: &Call<'_>, t: Transition, is_host: bool) {
        match t {
            Transition::None => {}
            Transition::Start(role) => {
                let attack = {
                    let mut st = self.lock();
                    st.host.clear();
                    st.client.clear();
                    st.ents.clear();
                    st.gate_running = false;
                    st.deps.clear();
                    st.view = view::ViewState::default();
                    st.area.clear();
                    st.cover.clear();
                    st.decoy.clear();
                    st.ondeath.clear();
                    st.lit.clear();
                    st.follow = follow::FollowState::default();
                    st.followers = followers::FollowersState::default();
                    st.orbs.clear();
                    st.shots.clear();
                    st.objects.as_ref().and_then(|o| o.player_attack)
                };
                log::info!("co-op world: shared world on as {role:?}");
                if role == Role::Host {
                    self.deps_run_start(call);
                    self.view_session_start();
                } else if role == Role::Client {
                    self.view_client_start(call);
                }
                self.repl_run_start();
                // `roomGame` is persistent: an avatar from an earlier run could
                // still be in it. Nothing else uses `oPlayerAttack`.
                if let Some(obj) = attack {
                    let stale = self.instances(call, obj);
                    if stale > 0 {
                        if let Some(b) = self.h.destroy {
                            let _ = call.call_builtin(b, &[Value::object(obj), Value::Bool(false)]);
                        }
                        log::info!("co-op world: removed {stale} avatars left from an earlier run");
                    }
                }
            }
            Transition::Interrupted => {
                log::info!("co-op world: link lost; world held");
                // By the world's role, not `is_host` (unlinked, the session
                // no longer says this game hosts).
                self.orbs_interrupted();
            }
            Transition::Resumed => {
                log::info!("co-op world: partner back; full resync");
                self.orbs_resumed(call, is_host);
                if is_host {
                    {
                        let mut st = self.lock();
                        st.host.resync();
                        // Every lit state again (changes only go out, and
                        // those sent into the dead link were lost).
                        st.lit.resync_host();
                    }
                    self.gen_resync(call);
                    self.shots_resync();
                    self.director_resync();
                }
            }
            Transition::PartnerLeft => {
                let ids = {
                    let mut st = self.lock();
                    st.host.clear();
                    st.area.clear();
                    st.cover.clear();
                    st.decoy.clear();
                    st.ondeath.clear();
                    st.lit.clear();
                    st.orbs.clear();
                    st.shots.clear();
                    st.ents.clear()
                };
                for id in ids {
                    self.destroy_silently(call, &id);
                }
                log::info!("co-op world: partner gone; host plays on");
            }
            Transition::RunEnded(_) => {
                // The game removed the instances with the run.
                {
                    let mut st = self.lock();
                    st.host.clear();
                    st.client.clear();
                    st.ents.clear();
                    st.gate_running = false;
                    st.shots.clear();
                    st.ondeath.clear();
                }
                log::info!("co-op world: run over");
            }
        }
    }

    /// A run step with a role: the freeze flag, the frame play went on at,
    /// and, joining, every copy's track held at its newest snapshot each
    /// frozen frame ([`ClientWorld::hold_tracks`]). Returns whether frozen.
    fn note_frozen(&self, call: &Call<'_>, role: Role) -> bool {
        let frozen = self.frozen(call);
        let mut st = self.lock();
        if st.frozen && !frozen {
            st.resumed_at = st.frame;
        }
        st.frozen = frozen;
        if frozen && role == Role::Client {
            st.client.hold_tracks();
        }
        frozen
    }

    /// A run step with no role: the freeze flag is kept current.
    fn off_step(&self, call: &Call<'_>) {
        standin_glue::off(call);
        let frozen = self.frozen(call);
        let mut st = self.lock();
        st.frozen = frozen;
    }

    fn set_alarm(&self, call: &Call<'_>, waves: u32, frames: f64) -> bool {
        self.alarm_to(call, waves, frames.max(1.0))
    }

    /// `oWaves.alarm[1] = value` (below 0: off).
    fn alarm_to(&self, call: &Call<'_>, waves: u32, value: f64) -> bool {
        let Some(b) = self.h.alarm_set else {
            return false;
        };
        let mut ok = false;
        let _ = call.with(&Value::object(waves), |inst, _| {
            ok = inst.call_builtin(b, &[1.0.into(), value.into()]).is_ok();
        });
        ok
    }

    fn players(&self, input: &WorldInput) -> Vec<Player> {
        let st = self.lock();
        let mut out = Vec::with_capacity(input.partners.len() + 1);
        out.push(Player {
            peer: input.me,
            pos: st.local.pos,
            targetable: st.frame.saturating_sub(st.local.seen_frame) <= 2
                && st.local.visible
                && !input.own_cloaked,
        });
        for p in &input.partners {
            let Some((pos, _, _)) = p.pose else { continue };
            out.push(Player {
                peer: p.peer,
                pos,
                targetable: p.targetable() && st.ents.target(EntityKey::ship(p.peer)).is_some(),
            });
        }
        out
    }

    fn host_frame(&self, call: &Call<'_>, input: &WorldInput, gate_rose: bool) -> Vec<Message> {
        // Dependencies tick as last frame's `oSolid` ticks did, before the
        // frame turns (so they are in its snapshot and not missed).
        // First what ran its Destroy event since the last frame (before this
        // frame finds it gone): every tracked instance, dependency or enemy;
        // `destroying` ignores the rest.
        let seen = std::mem::take(
            &mut *self
                .destroyed_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if !seen.is_empty() {
            self.host_destroyed_seen(seen);
        }
        // What makers gained in their members' birth frame, read now and
        // sent ahead of those members' spawns (below).
        let maker_out = self.gen_maker_repairs(call);
        self.tick_deps_host(call);
        let players = self.players(input);
        let mut missing = self.lock().host.begin_frame(players);
        if self.lock().frozen {
            // The freeze deactivated everything: not asleep. Asleep is what a
            // running game keeps deactivated (the next running frame finds it).
            missing.clear();
        }
        // Missed by the ticks: destroyed, or deactivated by the game (asleep,
        // not gone: a freeze brings back only the region around the view).
        let mut gone = Vec::new();
        let mut asleep = Vec::new();
        for (n, id) in missing {
            match self.presence(call, &id) {
                Presence::Active => {}
                Presence::Asleep => asleep.push(n),
                Presence::Gone => gone.push(n),
            }
        }
        let wave = self.read_wave(call);
        let made = std::mem::take(&mut self.lock().new_markers);
        let markers: Vec<SpawnMarker> = made
            .iter()
            .filter_map(|(id, portal)| self.read_marker(call, id, *portal))
            .collect();
        let dmg_out = self.host_damage(call, &input.inbox);
        let states = self.gen_host_frame(call);
        let mut st = self.lock();
        for m in markers {
            st.host.marker(m);
        }
        st.host.gone_with(&gone);
        st.host.sleep(&asleep);
        if let Some((w, ms)) = wave {
            st.host.wave(w, ms);
        }
        if gate_rose {
            log::info!(
                "co-op world: run released; sending the world ({} enemies)",
                st.host.len()
            );
            st.host.resync();
        }
        let mut out = dmg_out;
        let msgs = st.host.take_messages(input.now_ms);
        drop(st);
        self.dmg.host_sent(&msgs, input.me);
        out.extend(transfer::order_ahead(maker_out, msgs));
        out.extend(states);
        self.view_inputs_host(call, &input.inbox);
        self.area_host(call, &input.inbox);
        out.extend(self.orbs_host(call, &input.inbox, input.own_down));
        out.extend(self.view_host(call, input.now_ms));
        out
    }

    /// Host: these ran their Destroy event since the last run step (each a
    /// `Destroyed` despawn unless it dies).
    fn host_destroyed_seen(&self, seen: Vec<usize>) {
        let mut st = self.lock();
        for key in seen {
            st.host.destroying(key);
        }
        drop(st);
    }

    /// The status channel on the host (see [`Self::host_damage`]).
    fn host_status(&self, call: &Call<'_>, inbox: &[Message]) {
        let new = crate::damage::glue::DamageHooks::status_entries(inbox);
        let (entries, ids) = {
            let mut st = self.lock();
            if new.is_empty() && st.held_status.is_empty() {
                return;
            }
            let frozen = st.frozen;
            let all = st.held_status.frame(new, frozen);
            if all.is_empty() {
                return;
            }
            let ids: HashMap<NetId, Value> = all
                .iter()
                .filter_map(|(_, e)| st.host.by_net_id(e.net_id).map(|(id, _)| (e.net_id, id)))
                .collect();
            (all, ids)
        };
        let held = self
            .dmg
            .host_status(call, entries, &|n| ids.get(&n).cloned());
        if !held.is_empty() {
            self.lock().held_status.hold(held);
        }
    }

    /// Shared damage on the host: acks for what was applied up to the last
    /// frame (the hulls from the last ticks include it), then this frame's
    /// reports applied to the real enemies.
    fn host_damage(&self, call: &Call<'_>, inbox: &[Message]) -> Vec<Message> {
        let mut out = {
            let st = self.lock();
            self.dmg.host_acks(&|n| st.host.hull_of(n), !st.frozen)
        };
        // The claim rule: every claim answered within the bound.
        let (late, refusals) = {
            let st = self.lock();
            self.dmg.host_overdue(&|n| st.host.hull_of(n))
        };
        if !late.is_empty() {
            let mut st = self.lock();
            for n in late {
                st.host.respawn(n);
            }
        }
        out.extend(refusals);
        // Status effects (knockback, slow, flashes, marks): as damage
        // reports, held while frozen and for a deactivated enemy, applied at
        // the first running frame it is there; never lost.
        if crate::damage::DamageSettings::status_on() {
            self.host_status(call, inbox);
        }
        let mut reports: Vec<ndml_net::msg::DamageReport> = inbox
            .iter()
            .filter_map(|m| match m {
                Message::DamageReport(r) => Some(r.clone()),
                _ => None,
            })
            .collect();
        {
            let mut st = self.lock();
            if st.frozen {
                st.held_reports.append(&mut reports);
                st.held_reports = crate::damage::compact(std::mem::take(&mut st.held_reports));
                return out;
            }
            if !st.held_reports.is_empty() {
                let mut held = std::mem::take(&mut st.held_reports);
                held.append(&mut reports);
                reports = held;
            }
        }
        if reports.is_empty() {
            return out;
        }
        let lookup: HashMap<NetId, (Value, f32, u16)> = {
            let st = self.lock();
            reports
                .iter()
                .flat_map(|r| r.entries.iter())
                .filter_map(|e| {
                    st.host.by_net_id(e.net_id).map(|(id, max)| {
                        (
                            e.net_id,
                            (id, max, st.host.object_of(e.net_id).unwrap_or(0)),
                        )
                    })
                })
                .collect()
        };
        let (killers, again, msgs, held) = self
            .dmg
            .host_apply(call, &reports, &|n| lookup.get(&n).cloned());
        let mut st = self.lock();
        // Entries for enemies the game has deactivated: again next frame.
        if !held.is_empty() {
            st.held_reports.extend(held);
            st.held_reports = crate::damage::compact(std::mem::take(&mut st.held_reports));
        }
        for (n, p, h) in killers {
            st.host.set_damager(n, p, h);
        }
        for n in again {
            st.host.respawn(n);
        }
        out.extend(msgs);
        out
    }

    /// A telegraph the spawner made (its `with` block has filled it in by now:
    /// alarms run before the Step this is called from).
    fn read_marker(&self, call: &Call<'_>, id: &Value, portal: bool) -> Option<SpawnMarker> {
        let h = &self.h;
        let mut out = None;
        let _ = call.with(id, |inst, _| {
            out = if portal {
                let object = h
                    .enemy_type
                    .and_then(|v| inst.get_value(v))
                    .as_ref()
                    .and_then(object_index)
                    .and_then(|o| u16::try_from(o).ok());
                object.map(|object| SpawnMarker {
                    kind: SpawnMarker::PORTAL,
                    x: read(&inst, bv::X).unwrap_or(0.0),
                    y: read(&inst, bv::Y).unwrap_or(0.0),
                    scale: 1.0,
                    object,
                    rarity: h.rarity.and_then(|v| inst.get_f64(v)).unwrap_or(0.0) as f32,
                })
            } else {
                let num =
                    |v: Option<Variable>| v.and_then(|v| inst.get_f64(v)).filter(|n| n.is_finite());
                match (num(h.spawn_pos_x), num(h.spawn_pos_y), num(h.scaler)) {
                    (Some(px), Some(py), Some(scale)) => Some(SpawnMarker {
                        kind: SpawnMarker::WARNING,
                        x: px as f32,
                        y: py as f32,
                        scale: scale as f32,
                        object: 0,
                        rarity: 0.0,
                    }),
                    _ => None,
                }
            };
        });
        out
    }

    /// Client: show a host telegraph with the game's own object.
    fn show_marker(&self, call: &Call<'_>, m: SpawnMarker) {
        let h = &self.h;
        let (warning, portal) = {
            let st = self.lock();
            let o = st.objects.as_ref();
            (o.and_then(|o| o.warning), o.and_then(|o| o.portal))
        };
        let finite = m.x.is_finite() && m.y.is_finite();
        let obj = match m.kind {
            SpawnMarker::WARNING
                if finite
                    && m.scale.is_finite()
                    && m.scale > 0.0
                    && m.scale <= 10.0
                    && h.buffer.is_some()
                    && h.spawn_pos_x.is_some()
                    && h.spawn_pos_y.is_some()
                    && h.scaler.is_some() =>
            {
                warning
            }
            // The portal sizes itself from the announced enemy: it must be
            // an enemy in this game.
            SpawnMarker::PORTAL
                if finite
                    && m.rarity.is_finite()
                    && h.enemy_type.is_some()
                    && h.rarity.is_some()
                    && matches!(
                        self.class_of(call, u32::from(m.object), true),
                        Some(Class::Enemy { .. })
                    ) =>
            {
                portal
            }
            _ => None,
        };
        let (Some(obj), Some(create)) = (obj, h.instance_create) else {
            return;
        };
        let Some(id) = call
            .call_script(
                create,
                &[
                    f64::from(m.x).into(),
                    f64::from(m.y).into(),
                    Value::object(obj),
                ],
            )
            .ok()
            .filter(is_instance)
        else {
            return;
        };
        let _ = call.with(&id, |inst, _| {
            let set = |v: Option<Variable>, x: Value| {
                if let Some(v) = v {
                    let _ = inst.set(v, x);
                }
            };
            if m.kind == SpawnMarker::PORTAL {
                set(h.enemy_type, Value::object(u32::from(m.object)));
                set(h.rarity, f64::from(m.rarity).into());
            } else {
                let s = f64::from(m.scale);
                set(h.buffer, 65.0.into());
                set(h.spawn_pos_x, f64::from(m.x).into());
                set(h.spawn_pos_y, f64::from(m.y).into());
                set(h.scaler, s.into());
                for n in [bv::IMAGE_XSCALE, bv::IMAGE_YSCALE] {
                    if let Some(v) = inst.builtin_f64(n) {
                        let _ = inst.set_builtin_f64(n, v * s);
                    }
                }
            }
        });
    }

    /// `global.wave` every frame, the wave timer (`alarm_get(1)` as
    /// `oWaves`) every 15 frames: [`names::WAVE_TIMER_OFF`] when it is not
    /// running (below 0), the last read when a read fails.
    fn read_wave(&self, call: &Call<'_>) -> Option<(u16, u32)> {
        let wave = call.global()?.get_f64(self.h.wave?)?;
        let (frame, waves, sps, mut ms) = {
            let st = self.lock();
            (
                st.frame,
                st.objects.as_ref().and_then(|o| o.waves),
                st.sps,
                st.wave_ms,
            )
        };
        if frame % 15 == 0
            && let (Some(w), Some(b)) = (waves, self.h.alarm_get)
        {
            let mut frames = None;
            let _ = call.with(&Value::object(w), |inst, _| {
                frames = inst
                    .call_builtin(b, &[1.0.into()])
                    .ok()
                    .and_then(|v| v.as_f64());
            });
            if let Some(f) = frames.filter(|f| f.is_finite()) {
                ms = Some(if f < 0.0 {
                    names::WAVE_TIMER_OFF
                } else {
                    (f / sps * 1000.0).min(f64::from(names::WAVE_TIMER_OFF - 1)) as u32
                });
                self.lock().wave_ms = ms;
            }
        }
        (wave.is_finite() && wave >= 0.0)
            .then(|| (wave.min(f64::from(u16::MAX)) as u16, ms.unwrap_or(0)))
    }

    /// Client, each run frame: copies follow the host's instances' sleep,
    /// and the ones the ticks missed are woken (this game's freeze) or
    /// forgotten (gone here).
    fn client_presence(
        &self,
        call: &Call<'_>,
        sleep: &[(Value, bool)],
        missing: Vec<(NetId, Value)>,
    ) {
        // The host's game deactivated or activated these: so does this one.
        // Not woken in this game's own freeze: its resume wakes them
        // (`wake_copies`).
        let frozen = self.lock().frozen;
        for (id, asleep) in sleep {
            if *asleep || !frozen {
                self.set_asleep(call, id, *asleep);
            }
        }
        // Kept deactivated while the host's is (this game's own freezes
        // bring back the region around its view).
        let asleep = self.lock().client.asleep_ids();
        for id in &asleep {
            if self.exists(call, id) {
                self.set_asleep(call, id, true);
            }
        }
        // Missed by the ticks: gone here, or deactivated by this game (its
        // own freeze brings back only the region around its view): a copy is
        // awake while the host's instance is.
        let mut gone = Vec::new();
        // In a freeze only its first frame has any (running lags the freeze
        // by a frame): the gone leave the table now, the asleep wait for the
        // resume (`wake_copies`).
        for (n, id) in missing {
            match self.presence(call, &id) {
                Presence::Active => {}
                Presence::Asleep if frozen => {}
                Presence::Asleep => {
                    self.set_asleep(call, &id, false);
                }
                Presence::Gone => gone.push(n),
            }
        }
        if !gone.is_empty() {
            self.dmg.client_gone(&gone);
            self.lock().client.gone(&gone);
        }
    }

    /// The frame's drop list, at once, frozen too (each activated first): an
    /// old copy is never stepped or drawn again, and its twin is made later,
    /// at a running frame (no spawns while frozen).
    fn carry_out_drops(&self, call: &Call<'_>, drop: &[Value], resync_drops: usize) {
        if !drop.is_empty() {
            for id in drop {
                self.destroy_silently(call, id);
            }
            let other = drop.len() - resync_drops;
            if resync_drops > 0 {
                log::info!(
                    "co-op world: resync dropped {} old instances{}",
                    resync_drops,
                    if self.lock().frozen {
                        " (in a freeze)"
                    } else {
                        ""
                    }
                );
            }
            if other > 0 {
                log_limited!(
                    log::Level::Info,
                    "world-dropped",
                    "co-op world: dropped {other} instances (remade after a contained error, or made after their despawn)"
                );
            }
        }
        self.lock().client.dropped();
    }

    fn client_frame(
        &self,
        call: &Call<'_>,
        mut inbox: Vec<Message>,
        render_time: Option<Duration>,
    ) -> Vec<Message> {
        self.orbs_client_inbox(&mut inbox);
        self.view_inbox(&mut inbox);
        // The wave director's values before this frame's copies are made
        // (their set-up reads them).
        self.view_director_client(call);
        self.gen_client_inbox(&mut inbox);
        self.dmg.client_messages(&inbox);
        // Kills claimed here before a resync that sent the enemy again: no
        // copy while the claim is open or lately confirmed (the shared
        // damage's word, taken before the world's lock).
        let held = self.dmg.held_spawns();
        let (f, pending) = {
            let mut st = self.lock();
            for m in inbox {
                // A dependency's kind is known from its spawn on: a copy made
                // before it (in the same frame) treats its own of that kind as
                // scratch.
                if let Message::EnemySpawn(s) = &m
                    && s.is_dependency()
                {
                    st.deps.kinds.insert(u32::from(s.object));
                }
                st.client.on_message(m);
            }
            let f = st.client.begin_frame(render_time);
            let (n, stamp) = st.client.host_snapshots();
            self.host_snaps.0.store(n, Ordering::Relaxed);
            self.host_snaps.1.store(stamp, Ordering::Relaxed);
            // Never while frozen: a copy made then would be active behind
            // the pause screen (its own Step running) while everything else
            // is deactivated. `running` alone lags the freeze by a frame.
            let pending = if f.running && !st.frozen {
                // Generic transfer: a copy is made once its state is here
                // (a bare copy's own Step reads what its set-up never wrote).
                let (ready, client) = (&st.transfer, &st.client);
                let mut chosen = client.pending_ready(client::MAX_SPAWNS_PER_FRAME, |n| {
                    !held.contains(&n) && ready.copy_ready(n, client)
                });
                // The enemies they refer to come in the same frame (past the
                // per-frame cap): a fin made before its Glaucus dies.
                let ids: Vec<NetId> = chosen.iter().map(|s| s.net_id).collect();
                let mut with = ready.made_with(&ids, client);
                with.retain(|m| !held.contains(m));
                if !with.is_empty() {
                    chosen.extend(client.pending_ready(usize::MAX, |m| with.contains(&m)));
                }
                ready.makers_first(&mut chosen);
                chosen
            } else {
                Vec::new()
            };
            (f, pending)
        };
        self.carry_out_drops(call, &f.drop, f.resync_drops);
        self.client_presence(call, &f.sleep, f.missing);
        if let Some((w, ms)) = f.wave {
            self.apply_wave(call, w, ms);
        }
        for m in f.markers {
            self.show_marker(call, m);
        }
        for s in pending {
            self.spawn_one(call, s);
        }
        self.gen_client_frame(call);
        self.tick_deps_client(call);
        self.view_client(call);
        self.pose_at_step(call);
        // The host's phase changes the shown time reached, after the poses
        // (a phase's entry acts from the copy's shown place) and before the
        // copies' own Steps.
        self.phase_client(call);
        crate::guard::client_frame(call, &mut self.lock().client);
        let mut out = self.damage_frame();
        out.extend(self.view_inputs_client(call));
        out
    }

    /// Client: put every live copy on this frame's pose now, in the
    /// controller's Step, before the copies' own Steps: what a copy creates
    /// and aims from it (an enemy beam, `oEnemyBeamFront_Step_0`) reads its
    /// pose there. The End Step tick writes the same pose again.
    fn pose_at_step(&self, call: &Call<'_>) {
        let poses = self.lock().client.live_poses();
        Self::put_poses(call, &poses);
        self.lock().early_poses = poses;
    }

    /// Client, the controller's End Step, before the copies' End Steps: every
    /// copy back on this frame's pose. Events run object by object in index
    /// order, so a copy's own Step can turn it after the controller's Step and
    /// a reader of a lower index (a beam charge-up) would see that in its End
    /// Step; from here to the draw a copy is where it is drawn.
    pub(super) fn pose_at_end_step(&self, call: &Call<'_>) {
        let poses = std::mem::take(&mut self.lock().early_poses);
        Self::put_poses(call, &poses);
    }

    /// Client, before the draw (`oBackgroundColor`'s Draw Begin): every copy
    /// whose own code turned it after its End Step tick goes back to the
    /// tick's angle, and a part placed after its tick from a leader that went
    /// back is carried with that leader ([`put_back::plan`]); a part placed
    /// from a leader that did not move rides that leader's last tick.
    pub(super) fn angles_before_draw(&self, call: &Call<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let ticks = std::mem::take(&mut self.lock().tick_poses);
        // A call with nothing ticked since the last one (Draw Begin again in
        // the same frame) leaves last frame's tick poses for the next.
        if ticks.is_empty() {
            return;
        }
        let lv = self.h.leader;
        let mut seen: Vec<put_back::Seen> = Vec::with_capacity(ticks.len());
        let mut leaders: Vec<Option<Value>> = Vec::with_capacity(ticks.len());
        let mut ids: HashMap<usize, &Value> = HashMap::with_capacity(ticks.len());
        for (id, key, obj, tick, guessed, end_step) in &ticks {
            let _ = call.with(id, |inst, _| {
                let now = (
                    read(&inst, bv::X).unwrap_or(tick.0),
                    read(&inst, bv::Y).unwrap_or(tick.1),
                    read(&inst, bv::IMAGE_ANGLE).unwrap_or(tick.2),
                );
                seen.push(put_back::Seen {
                    key: *key,
                    obj: *obj,
                    tick: *tick,
                    now,
                    leader: None,
                    extrapolated: *guessed,
                    at_end_step: *end_step,
                });
                leaders.push(lv.and_then(|v| inst.get_value(v)).filter(is_instance));
                ids.insert(*key, id);
            });
        }
        for (s, l) in seen.iter_mut().zip(&leaders) {
            if let Some(l) = l {
                let _ = call.with(l, |li, _| s.leader = Some(li.key()));
                s.leader = s.leader.filter(|k| ids.contains_key(k));
            }
        }
        let prev = std::mem::take(&mut self.lock().prev_ticks);
        let mut plan = put_back::plan(&seen, &prev);
        for w in &plan.writes {
            let Some(id) = ids.get(&w.key) else { continue };
            let _ = call.with(id, |inst, _| {
                if let Some((x, y)) = w.pos
                    && finite2(x, y)
                {
                    let _ = inst.set_builtin_f64(bv::X, f64::from(x));
                    let _ = inst.set_builtin_f64(bv::Y, f64::from(y));
                }
                if let Some(a) = w.angle
                    && a.is_finite()
                {
                    let _ = inst.set_builtin_f64(bv::IMAGE_ANGLE, f64::from(a));
                }
            });
        }
        self.lock().prev_ticks = std::mem::take(&mut plan.last);
    }

    /// A tick with no velocity from the host (held, first, a new segment):
    /// the copy's own speed, put back to the host's last when it is not
    /// finite or has run away ([`client::held_velocity`]); `None` leaves it.
    fn held_speed(me: &Instance<'_>, last: Option<Vec2>) -> Option<Vec2> {
        let (Some(h), Some(v)) = (me.builtin_f64(bv::HSPEED), me.builtin_f64(bv::VSPEED)) else {
            return None;
        };
        let own = Vec2::new(h as f32, v as f32);
        client::held_velocity(own, last)
    }

    /// The pose a copy's tick gave it, for the put-back before the draw
    /// ([`Self::angles_before_draw`]).
    fn push_tick_pose(&self, me: &Instance<'_>, key: usize, p: &client::PuppetTick) {
        if let Ok(id) = me.builtin_value(bv::ID) {
            let obj = object_of(me).unwrap_or(0);
            let pose = (p.pos.x, p.pos.y, p.angle);
            let mut st = self.lock();
            let end_step = !st.deps.client_ticking;
            st.tick_poses
                .push((id, key, obj, pose, p.extrapolated, end_step));
        }
    }

    fn put_poses(call: &Call<'_>, poses: &[(Value, Vec2, f32)]) {
        for (id, pos, angle) in poses {
            let _ = call.with(id, |inst, _| {
                if !(finite2(pos.x, pos.y) && angle.is_finite()) {
                    return;
                }
                let _ = inst.set_builtin_f64(bv::X, f64::from(pos.x));
                let _ = inst.set_builtin_f64(bv::Y, f64::from(pos.y));
                let _ = inst.set_builtin_f64(bv::IMAGE_ANGLE, f64::from(*angle));
            });
        }
    }

    /// Shared damage's end of the client frame: classify the instances it
    /// asks about (copy, part, other), take the reports.
    fn damage_frame(&self) -> Vec<Message> {
        use crate::damage::glue::Seen;
        let keys = self.dmg.keys_to_classify();
        let classes: HashMap<usize, Seen> = {
            let st = self.lock();
            keys.into_iter()
                .map(|k| {
                    let c = if st.client.net_id(k).is_some() {
                        Seen::Copy
                    } else if st.client.owns(k) {
                        Seen::Part
                    } else {
                        Seen::Other
                    };
                    (k, c)
                })
                .collect()
        };
        let frozen = self.lock().frozen;
        self.dmg
            .client_frame(&|k| classes.get(&k).copied().unwrap_or(Seen::Other), frozen)
    }

    fn apply_wave(&self, call: &Call<'_>, wave: u16, next_ms: u32) {
        if let (Some(g), Some(v)) = (call.global(), self.h.wave)
            && !g.set_f64(v, f64::from(wave))
        {
            warn_once(
                "world-wave",
                "co-op world: global.wave is not a number; not mirrored",
            );
        }
        let off = next_ms == names::WAVE_TIMER_OFF;
        let (waves, sps) = {
            let mut st = self.lock();
            st.host_wave_ms = Some(next_ms);
            (st.objects.as_ref().and_then(|o| o.waves), st.sps)
        };
        if let Some(w) = waves {
            // The timer shows the host's countdown; its alarm stays skipped.
            // Off as the host's is off (a boss fight).
            if off {
                self.alarm_to(call, w, -1.0);
            } else {
                self.set_alarm(call, w, f64::from(next_ms) * sps / 1000.0);
            }
        }
    }

    /// Create a host enemy here: every enemy-family object (an `oSolid`
    /// child) and every dependency is made the same way, created bare and
    /// then given the host's state (the generic transfer, `gen_glue.rs`).
    fn spawn_one(&self, call: &Call<'_>, s: EnemySpawn) {
        let obj = u32::from(s.object);
        // A dependency: not an enemy, but one refers to it.
        let dep = s.is_dependency();
        if dep {
            self.lock().deps.kinds.insert(obj);
        }
        if !dep && !self.class_of(call, obj, false).is_some_and(Class::is_enemy) {
            info_once(
                &format!("world-skip-{obj}"),
                &format!(
                    "co-op world: host enemy object {obj} is not created here: not an enemy in this game"
                ),
            );
            self.lock().client.skipped(s.net_id);
            return;
        }
        let Some(create) = self.h.instance_create else {
            return;
        };
        // Where the host has it, never folded: this game wraps it itself.
        let pos = Vec2::new(s.x, s.y);
        self.lock().owner = Some(s.net_id);
        self.replicating.store(true, Ordering::Relaxed);
        let made = call
            .call_script(
                create,
                &[
                    f64::from(pos.x).into(),
                    f64::from(pos.y).into(),
                    Value::object(obj),
                ],
            )
            .ok()
            .filter(is_instance);
        let mut key = None;
        if let Some(id) = &made {
            let _ = call.with(id, |inst, _| {
                key = Some(inst.key());
            });
        }
        self.replicating.store(false, Ordering::Relaxed);
        let mut st = self.lock();
        st.owner = None;
        if let (Some(id), Some(k)) = (made, key) {
            st.client.spawned(s.net_id, k, id.clone());
            drop(st);
            if dep {
                self.dep_made(s.net_id, obj, id.clone());
            }
            self.gen_client_made(s.net_id, id, s.object);
        } else {
            st.client.skipped(s.net_id);
            drop(st);
            warn_once(
                &format!("world-create-{obj}"),
                &format!("co-op world: creating host enemy object {obj} failed; skipped"),
            );
        }
    }

    fn desired(partners: &[PartnerView]) -> Vec<Desired> {
        // A partner gone quiet (a stall, or a link drop before it times out)
        // keeps its avatar where it was, as drawn, and untargetable; it goes
        // when the partner leaves.
        partners
            .iter()
            .filter_map(|p| {
                let (pos, vel, angle) = p.pose?;
                let (blend, alpha, visible) = present::ship_look(&p.look, p.color);
                Some(Desired {
                    key: EntityKey::ship(p.peer),
                    sprite: p.sprite,
                    frame: f64::from(p.frame),
                    pos,
                    vel,
                    angle,
                    xscale: p.look.xscale,
                    yscale: p.look.yscale,
                    blend,
                    alpha,
                    visible: visible && p.sprite.is_some(),
                    targetable: p.targetable(),
                })
            })
            .collect()
    }

    fn entities_frame(&self, call: &Call<'_>, partners: &[PartnerView]) {
        let desired = Self::desired(partners);
        let (actions, attack, depth) = {
            let mut st = self.lock();
            let running = st.frame.saturating_sub(st.local.seen_frame) <= 2;
            (
                st.ents.plan(&desired, running),
                st.objects.as_ref().and_then(|o| o.player_attack),
                st.local.depth,
            )
        };
        for a in actions {
            match a {
                Action::Create { key, x, y } => {
                    let made = match (attack, self.h.instance_create) {
                        (Some(obj), Some(create)) => call
                            .call_script(create, &[x.into(), y.into(), Value::object(obj)])
                            .ok()
                            .filter(is_instance),
                        _ => None,
                    };
                    if made.is_none() {
                        warn_once(
                            "world-avatar",
                            "co-op world: unable to create the partner's avatar; drawn instead",
                        );
                    } else {
                        log::info!("co-op world: avatar for {} created", key.owner);
                    }
                    self.lock().ents.created(key, made);
                }
                Action::Update {
                    key,
                    id,
                    d,
                    sprite,
                    first,
                } => {
                    let mut update = |inst: Instance<'_>, _: Option<Instance<'_>>| {
                        let set = |n: &str, v: f64| {
                            let _ = inst.set_builtin_f64(n, v);
                        };
                        set(bv::X, f64::from(d.pos.x));
                        set(bv::Y, f64::from(d.pos.y));
                        set(bv::IMAGE_ANGLE, f64::from(d.angle));
                        set(bv::IMAGE_XSCALE, d.xscale);
                        set(bv::IMAGE_YSCALE, d.yscale);
                        set(bv::IMAGE_BLEND, f64::from(d.blend));
                        set(bv::IMAGE_ALPHA, d.alpha);
                        set(bv::IMAGE_INDEX, d.frame);
                        set(bv::VISIBLE, if d.visible { 1.0 } else { 0.0 });
                        if first {
                            set(bv::IMAGE_SPEED, 0.0);
                            if let Some(dp) = depth {
                                set(bv::DEPTH, dp);
                            }
                        }
                    };
                    let mut n = call.with(&id, &mut update).unwrap_or(0);
                    // Not selected: a freeze left it deactivated and the
                    // game's resume region did not bring it back. Activated
                    // here, at once, before this frame's draw. Gone: still 0,
                    // and the plan deals with it.
                    if n == 0
                        && let Some(b) = self.h.activate
                    {
                        let _ = call.call_builtin(b, std::slice::from_ref(&id));
                        n = call.with(&id, &mut update).unwrap_or(0);
                        if n > 0 {
                            info_once(
                                "world-avatar-activate",
                                "co-op world: the avatar was left deactivated by a freeze: re-activated",
                            );
                        }
                    }
                    let applied = if n > 0 {
                        sprite.and_then(|s| self.set_sprite(call, &id, s).then_some(s))
                    } else {
                        None
                    };
                    self.lock().ents.visited(key, n > 0, applied);
                }
                Action::Activate { id } => {
                    if let Some(b) = self.h.activate {
                        let _ = call.call_builtin(b, &[id]);
                    }
                    info_once(
                        "world-avatar-activate",
                        "co-op world: the avatar was missing from the ticks (a freeze deactivated it): re-activated",
                    );
                }
                Action::Destroy { id } => {
                    log::info!("co-op world: avatar removed");
                    self.destroy_silently(call, &id);
                }
            }
        }
    }

    fn set_sprite(&self, call: &Call<'_>, id: &Value, sprite: u32) -> bool {
        let spr = sprite_value(sprite);
        let ok = self
            .h
            .sprite_exists
            .and_then(|b| call.call_builtin(b, std::slice::from_ref(&spr)).ok())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !ok {
            warn_once(
                "world-avatar-sprite",
                &format!("co-op world: the partner's sprite {sprite} does not exist here"),
            );
            return false;
        }
        self.h.var_set.is_some_and(|b| {
            self.exists(call, id)
                && call
                    .call_builtin(b, &[id.clone(), bv::SPRITE_INDEX.into(), spr])
                    .is_ok()
        })
    }

    /// The local ship's step (`oPlayer_Step_0`): position for targeting, id
    /// for target enforcement, and proof the world is running.
    pub fn player_step(&self, me: &Instance<'_>, x: f64, y: f64, hs: f64, vs: f64, visible: bool) {
        if self.role() == ROLE_OFF {
            return;
        }
        let key = me.key();
        let (known, need_depth) = {
            let st = self.lock();
            (
                st.local.key == key && st.local.id.is_some(),
                st.local.depth.is_none(),
            )
        };
        let id = (!known).then(|| me.builtin_value(bv::ID).ok()).flatten();
        let depth = need_depth.then(|| me.builtin_f64(bv::DEPTH)).flatten();
        let mut st = self.lock();
        if let Some(id) = id {
            st.local.key = key;
            st.local.id = Some(id);
        }
        if depth.is_some() {
            st.local.depth = depth;
        }
        st.local.pos = Vec2::new(x as f32, y as f32);
        st.local.vel = Vec2::new(hs as f32, vs as f32);
        st.local.visible = visible;
        st.local.seen_frame = st.frame;
        st.host.player_stepped();
        st.client.player_stepped();
    }

    /// A menu frame: whatever run there was is over.
    pub fn menu_frame(&self) {
        if !self.on {
            return;
        }
        self.role.store(ROLE_OFF, Ordering::Relaxed);
        let mut st = self.lock();
        let t = st.life.update(&LifeInput {
            run: None,
            coop_run: false,
            linked: false,
            is_host: false,
            enabled: true,
            loss: None,
        });
        if matches!(t, Transition::RunEnded(_)) {
            st.host.clear();
            st.client.clear();
            st.ents.clear();
            st.gate_running = false;
            log::info!("co-op world: run over");
        }
    }

    /// Partner labels and markers when every partner is shown as an avatar;
    /// `None` = draw partners with draw calls (no avatars, or one failed).
    /// `screen`: the camera view, as the draw-call path maps partners.
    #[must_use]
    pub fn partner_items(
        &self,
        gui: Gui,
        screen: Option<View>,
        line_h: f64,
    ) -> Option<Vec<DrawItem>> {
        if self.role() == ROLE_OFF {
            return None;
        }
        let st = self.lock();
        let all_live = st
            .partners
            .iter()
            .filter(|p| p.fresh && p.pose.is_some())
            .all(|p| st.ents.live(EntityKey::ship(p.peer)) || !p.look.visible);
        all_live.then(|| {
            present::labels(
                &st.partners,
                screen,
                gui,
                line_h,
                self.settings.show_names,
                self.settings.show_marker,
            )
        })
    }

    /// The draw-call path's items with the name/marker switches and the
    /// partner's colour.
    #[must_use]
    pub fn restyle(&self, items: Vec<DrawItem>) -> Vec<DrawItem> {
        let tint = {
            let st = self.lock();
            match st.partners.as_slice() {
                [p] => p.color,
                _ => None,
            }
        };
        present::restyle(
            items,
            self.settings.show_names,
            self.settings.show_marker,
            tint,
        )
    }

    /// A status line for the run overlay while the world is shared.
    #[must_use]
    pub fn overlay_line(&self) -> Option<Line> {
        let st = self.lock();
        let lost = if st.life.interrupted() {
            " - link lost, holding"
        } else {
            ""
        };
        let text = match st.life.role() {
            Role::Off => return None,
            Role::Host => format!("world: host, {} enemies shared{lost}", st.host.len()),
            Role::Client => {
                let c = st.client.counts();
                // Local spawns left to the host are not shown.
                format!(
                    "world: {} enemies ({} waiting, {} types skipped){lost}",
                    c.live, c.pending, c.skipped
                )
            }
        };
        Some(present::world_line(text))
    }

    fn tick(&self, call: &Call<'_>) {
        let role = self.role();
        if role == ROLE_OFF {
            return;
        }
        let Some(me) = call.instance() else { return };
        if role == ROLE_HOST {
            self.host_tick(&me);
        } else {
            self.client_tick(call, &me);
        }
    }

    /// Host: whether `key`'s Destroy event ran since the last run step.
    fn destroy_pending(&self, key: usize) -> bool {
        self.destroyed_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&key)
    }

    fn host_tick(&self, me: &Instance<'_>) {
        let key = me.key();
        let Some(obj) = object_of(me).and_then(|o| u16::try_from(o).ok()) else {
            return;
        };
        let r = TickRead {
            x: read(me, bv::X).unwrap_or(0.0),
            y: read(me, bv::Y).unwrap_or(0.0),
            angle: read(me, bv::IMAGE_ANGLE).unwrap_or(0.0),
            hull: self.h.hull.and_then(|v| me.get_f64(v)).map(|h| h as f32),
        };
        // Its address's enemy ran its Destroy event since the last run step:
        // another instance here is a new enemy ([`HostWorld::address_reused`]).
        let new_id = self
            .destroy_pending(key)
            .then(|| me.builtin_value(bv::ID).ok())
            .flatten();
        let (seen, reused) = {
            let mut st = self.lock();
            let reused = new_id
                .as_ref()
                .is_some_and(|n| st.host.address_reused(key, n));
            let s = st.host.seen(key, obj);
            match s {
                Seen::Known(id) => st.host.tick(id, r),
                Seen::New => {}
            }
            (s, reused)
        };
        if reused {
            // The old one's Destroy is done with: not the new one's.
            self.destroyed_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|k| *k != key);
        }
        if let Seen::Known(id) = seen {
            self.dmg.host_watch(key, id, r.hull.unwrap_or(0.0));
        }
        if seen != Seen::New {
            return;
        }
        let Ok(id) = me.builtin_value(bv::ID) else {
            return;
        };
        let f = FirstSight {
            id,
            object: obj,
            x: r.x,
            y: r.y,
            angle: r.angle,
            hull: r.hull,
            hull_max: self
                .h
                .hull_max
                .and_then(|v| me.get_f64(v))
                .unwrap_or(f64::from(r.hull.unwrap_or(0.0))) as f32,
            image_index: read(me, bv::IMAGE_INDEX).unwrap_or(0.0),
            dependency: self.lock().deps.host.contains_key(&key),
        };
        let id = f.id.clone();
        let net = self.lock().host.add(key, f);
        self.gen_host_added(net, key, id, obj);
    }

    fn client_tick(&self, call: &Call<'_>, me: &Instance<'_>) {
        let key = me.key();
        if self.gen_scratch_tick(call, key) {
            return;
        }
        let hull = self.h.hull.and_then(|v| me.get_f64(v));
        let t = self.lock().client.tick(key);
        match t {
            Tick::Puppet(p) => {
                if let Some(r) = p.despawn {
                    {
                        let mut st = self.lock();
                        let frame = st.frame;
                        st.removed.insert(key, (p.net_id, frame));
                    }
                    self.dmg.copy_removed(key);
                    self.remove_puppet(call, me, r, p.pos, p.net_id);
                    return;
                }
                if let Some(i) = p.image_index {
                    let _ = me.set_builtin_f64(bv::IMAGE_INDEX, f64::from(i));
                }
                // Put on the host's pose; never a non-finite pose into the
                // game (the copy keeps its last finite one).
                let finite = finite2(p.pos.x, p.pos.y) && p.angle.is_finite();
                if finite {
                    let _ = me.set_builtin_f64(bv::X, f64::from(p.pos.x));
                    let _ = me.set_builtin_f64(bv::Y, f64::from(p.pos.y));
                }
                if finite {
                    let _ = me.set_builtin_f64(bv::IMAGE_ANGLE, f64::from(p.angle));
                    self.push_tick_pose(me, key, &p);
                }
                // The next frame's built-in motion moves it by this. Motion not
                // known this tick: the copy keeps its own, never a zero it would
                // read as stopped; bounded ([`client::held_velocity`]).
                let v = p.vel.or_else(|| Self::held_speed(me, p.last_vel));
                if let Some(v) = v {
                    let _ = me.set_builtin_f64(bv::HSPEED, f64::from(v.x));
                    let _ = me.set_builtin_f64(bv::VSPEED, f64::from(v.y));
                }
                // The host's instance has no hull (a controller, an effect):
                // nothing to damage or write, and never a `hull` made on the
                // copy (it read as 0, a death, wherever hull is tested).
                if let Some(host_hull) = p.hull {
                    let object = self.dmg.settings().audit.then(|| object_of(me)).flatten();
                    let shared = self.dmg.client_tick(
                        p.net_id,
                        key,
                        object,
                        host_hull,
                        hull.map(|h| h as f32),
                    );
                    self.dmg.client_status(me, p.net_id);
                    if shared.is_some_and(|o| o.remove) {
                        self.dmg.remove_ghost(call, me, p.net_id);
                        return;
                    }
                    let target = f64::from(shared.map_or(host_hull, |o| o.write));
                    if hull.is_none_or(|h| (h - target).abs() > 1e-3) {
                        for v in [self.h.hull, self.h.hull_previous].into_iter().flatten() {
                            let _ = me.set_f64(v, target);
                        }
                        self.dmg.own_write(key, hull, target);
                    }
                }
            }
            Tick::Stray => {
                self.log_stray(call, me, key);
                if let (Ok(id), Some(b)) = (me.builtin_value(bv::ID), self.h.destroy) {
                    let obj = object_of(me);
                    let _ = me.call_builtin(b, &[id, Value::Bool(false)]);
                    info_once(&format!("world-stray-{obj:?}"), &{
                        let name =
                            obj.map_or_else(|| "?".to_owned(), |o| self.object_name(call, o));
                        format!(
                            "co-op world: removing enemies that appeared without the host (strays), first {name}"
                        )
                    });
                }
            }
            Tick::Part | Tick::Dropping => {}
        }
    }

    /// A stray at its End Step tick (an enemy here that the host never
    /// announced and no table took): one line each for the first 100, with
    /// its maker, its age, the freeze around it and whether its `hull` was
    /// ever assigned.
    fn log_stray(&self, call: &Call<'_>, me: &Instance<'_>, key: usize) {
        let (info, frame, resumed_at, frozen, n) = {
            let mut st = self.lock();
            st.strays += 1;
            (
                st.untracked.remove(&key),
                st.frame,
                st.resumed_at,
                st.frozen,
                st.strays,
            )
        };
        if n > 100 {
            return;
        }
        let name = object_of(me).map_or_else(|| "?".to_owned(), |o| self.object_name(call, o));
        let hull = self
            .h
            .hull
            .and_then(|v| me.get_f64(v))
            .map_or_else(|| "unset".to_owned(), |h| format!("{h:.1}"));
        let made = match info {
            Some((maker, at, made_frozen)) => format!(
                "maker {}, made {} frames ago{}",
                maker.map_or_else(|| "none".to_owned(), |m| self.object_name(call, m)),
                frame.saturating_sub(at),
                if made_frozen { " while frozen" } else { "" }
            ),
            None => "maker not seen".to_owned(),
        };
        // What the tables know of this instance by its id (keys are
        // addresses): a live copy under another key, a copy removed or
        // being dropped.
        let id = me.builtin_value(bv::ID).ok();
        let known = {
            let st = self.lock();
            let by_id = id.as_ref().and_then(|i| st.client.live_by_id(i));
            match by_id {
                Some((net, k)) => format!(
                    "; the table's live copy {net} has this id at key {k:#x} (this key {key:#x})"
                ),
                None => match st.removed.get(&key) {
                    Some((net, at)) => format!(
                        "; copy {net} removed {} frames ago at this key",
                        frame.saturating_sub(*at)
                    ),
                    None => String::new(),
                },
            }
        };
        log::info!(
            "co-op world: stray {name}: {made}; {} frames since the last freeze ended{}; id {}{known}; hull {hull}",
            frame.saturating_sub(resumed_at),
            if frozen { " (frozen now)" } else { "" },
            id.map_or_else(|| "?".to_owned(), |v| format!("{v:?}"))
        );
    }

    /// A Serpent's user event 1 is about to run on the joining side: logs
    /// (first 20) whose instance it is and what the mod is doing then.
    fn log_chain_build(&self, call: &Call<'_>) {
        static SEEN: AtomicU64 = AtomicU64::new(0);
        if self.role() != ROLE_CLIENT {
            return;
        }
        if SEEN.fetch_add(1, Ordering::Relaxed) >= 20 {
            return;
        }
        let Some(me) = call.instance() else { return };
        let key = me.key();
        let (whose, building, owner, frame, resumed_at, scratch) = {
            let st = self.lock();
            let whose = match st.client.net_id(key) {
                Some(n) => format!("copy {n}"),
                None if st.client.owns(key) => "a part".to_owned(),
                None => "untracked".to_owned(),
            };
            (
                whose,
                st.building == Some(key),
                st.owner,
                st.frame,
                st.resumed_at,
                st.transfer.scratch.iter().any(|s| s.0 == key),
            )
        };
        log::info!(
            "co-op world: Serpent user event 1 on {whose}{}{}{}; replicating {}; frame {frame} ({} since the last freeze ended)",
            if scratch { " (scratch)" } else { "" },
            if building {
                " during its set-up (user event 0)"
            } else {
                ""
            },
            owner.map_or_else(String::new, |o| format!("; making copy {o}")),
            self.replicating.load(Ordering::Relaxed),
            frame.saturating_sub(resumed_at)
        );
    }

    /// The host's despawn, from the puppet's own tick. A kill: the death
    /// effect (`createExplosion` as the enemy), then the copy's own Destroy
    /// event ([`Self::die_like_host`]); a removal (or a dependency):
    /// `instance_destroy(id, false)`, nothing runs.
    fn remove_puppet(
        &self,
        call: &Call<'_>,
        me: &Instance<'_>,
        r: Removal,
        at: Vec2,
        net_id: NetId,
    ) {
        for part in &r.parts {
            self.destroy_silently(call, part);
        }
        let killed = matches!(r.cause, DespawnCause::Killed { .. });
        if killed
            && self.is_asteroid(call, me)
            && let (Some(h), Some(hp)) = (self.h.hull, self.h.hull_previous)
        {
            // Its own Step plays the death (particles, sound, debris) and
            // destroys it; its splits and drops are vetoed quietly (the host
            // sends the splits).
            let _ = me.set_f64(h, 0.0);
            let _ = me.set_f64(hp, 0.0);
            let mut st = self.lock();
            let frame = st.frame;
            st.dying.insert(me.key(), frame);
            return;
        }
        // A dependency (a station core, the Star Eater's legs) is no enemy:
        // `createExplosion` as one raises a GML error; it goes silently.
        let dep = object_of(me).is_some_and(|o| self.is_dep_kind(o));
        if killed
            && !dep
            && let Some(s) = self.h.create_explosion
            && let Err(e) = me.call_script(s, &[const { Value::Undefined }; 4])
        {
            warn_once(
                "world-explosion",
                &format!("co-op world: createExplosion: {e}"),
            );
        }
        // Its own player's corrosion spreads from it (the host spreads its
        // own): before the death plays, while it still has its stacks.
        if killed && !dep {
            self.spread_at_death(me);
            // The world effects the host's word gave this game (Rupture's
            // blast, Spontaneous Generation's swarm: either player's, one
            // per death), as the game's own death path plays them before
            // the Destroy event.
            self.ondeath_at_death(call, me, net_id);
        }
        // Destroyed (a missile or mine that went off on the host, its Destroy
        // event run there without a death): the same, at its own hull. Each
        // cause does what the host's instance did, for copies and dependencies
        // alike: a death or a Destroy event runs the copy's Destroy event (a
        // dependency's removes what it made, such as layers); a removal none.
        let destroyed = matches!(r.cause, DespawnCause::Destroyed);
        if killed || destroyed {
            self.die_like_host(call, me, &r.id, at, killed);
            return;
        }
        if let Some(b) = self.h.destroy {
            let _ = me.call_builtin(b, &[r.id, Value::Bool(false)]);
        }
    }

    /// Client: a copy whose host enemy died dies the same way, at the host's
    /// last position, its Destroy event run under the guard with `hull` 0 for
    /// a kill (death-only Destroy events read it) or its own for `Destroyed`.
    /// Meanwhile [`names::HOST_DEATH_ONLY`] and `checkDestroy*` are skipped,
    /// the enemies it makes are scratch, its drops are vetoed and its hull
    /// changes stay the host's ([`crate::damage::glue::DamageHooks::begin_host_death`]).
    fn die_like_host(
        &self,
        call: &Call<'_>,
        me: &Instance<'_>,
        id: &Value,
        at: Vec2,
        killed: bool,
    ) {
        let _ = me.set_builtin_f64(bv::X, f64::from(at.x));
        let _ = me.set_builtin_f64(bv::Y, f64::from(at.y));
        if killed && let (Some(h), Some(hp)) = (self.h.hull, self.h.hull_previous) {
            let _ = me.set_f64(h, 0.0);
            let _ = me.set_f64(hp, 0.0);
        }
        {
            let mut st = self.lock();
            let frame = st.frame;
            st.dying.insert(me.key(), frame);
            st.copy_deaths += 1;
        }
        self.dmg.begin_host_death();
        if let Some(b) = self.h.destroy {
            let _ = me.call_builtin(b, &[id.clone(), Value::Bool(true)]);
        }
        self.dmg.end_host_death();
        let outer = {
            let mut st = self.lock();
            st.copy_deaths = st.copy_deaths.saturating_sub(1);
            st.copy_deaths == 0
        };
        // The enemies it made go before anyone's Step reads their unset
        // state (as after a predicted death).
        if outer {
            self.gen_scratch_clear(call, 0);
        }
    }

    /// Client, before a [`names::HOST_DEATH_ONLY`] script: skipped while a
    /// copy dies the host's death.
    fn skip_in_copy_death(&self, call: &mut Call<'_>) {
        if self.role() == ROLE_CLIENT && self.lock().copy_deaths > 0 {
            call.skip_original();
            let _ = call.set_return(0.0);
        }
    }

    /// Host, before the builtin `instance_destroy` (`()` = `self`, or
    /// `(target[, execute_event])`): a tracked enemy destroyed with its
    /// Destroy event is despawned as `Destroyed` unless it died (a missile
    /// that went off: its blast runs on the joining side too).
    fn host_destroying(&self, call: &Call<'_>) {
        if self.role() != ROLE_HOST {
            return;
        }
        let event = call.arg_value(1).is_none_or(|v| match v {
            Value::Bool(b) => b,
            other => other.as_f64().is_none_or(|f| f >= 0.5),
        });
        if !event {
            return;
        }
        let mut keys = Vec::new();
        match call.arg_value(0) {
            None => keys.extend(call.instance().map(|i| i.key())),
            Some(target) => {
                let _ = call.with(&target, |inst, _| keys.push(inst.key()));
            }
        }
        if keys.is_empty() {
            return;
        }
        {
            let mut st = self.lock();
            for k in &keys {
                st.host.destroying(*k);
            }
        }
        // Its Destroy event runs on the joining game too: with this state.
        for k in keys {
            self.gen_host_last_now(call, k);
        }
    }

    /// Whether `me` is an asteroid (`oAsteroid` child; cached per object).
    fn is_asteroid(&self, call: &Call<'_>, me: &Instance<'_>) -> bool {
        object_of(me).is_some_and(|obj| self.is_asteroid_object(call, obj))
    }

    /// Whether `obj` is an asteroid (`oAsteroid` child; cached).
    fn is_asteroid_object(&self, call: &Call<'_>, obj: u32) -> bool {
        let parent = {
            let st = self.lock();
            if let Some(&a) = st.asteroids.get(&obj) {
                return a;
            }
            st.objects.as_ref().and_then(|o| o.asteroid)
        };
        let a = self.ancestor(call, obj, parent);
        self.lock().asteroids.insert(obj, a);
        a
    }

    /// Client, before a copy's own Step or Begin Step: a hull below the host's
    /// last (what this game's own code took off since the End Step tick) is
    /// set to the host's for the event; after the event the local drop is
    /// put back on top of whatever the event changed. Until the host's death
    /// is reached on the timeline the copy runs on the host's last hull above
    /// 0 ([`ClientWorld::host_hull_alive`]), so a Step that ends it on
    /// `hull <= 0` (an asteroid's) never ends it first. A copy dying here by
    /// the host's word is left alone.
    fn step_swap_before(&self, me: &Instance<'_>) {
        let Some(h) = self.h.hull else { return };
        let Some(cur) = me.get_f64(h).filter(|v| v.is_finite()) else {
            return;
        };
        let key = me.key();
        let host = {
            let st = self.lock();
            if !st.client.owns(key) || st.dying.contains_key(&key) {
                return;
            }
            match st.client.host_hull_alive(key) {
                Some(host) => f64::from(host),
                None => return,
            }
        };
        if cur >= host - 1e-3 || self.dmg.is_dying(key) {
            return;
        }
        let _ = me.set_f64(h, host);
        self.dmg.own_write(key, Some(cur), host);
        if self.lock().step_swaps.insert(key, (cur, host)).is_none() {
            self.meter.step_swaps_live.fetch_add(1, Ordering::Relaxed);
        }
        if cur <= 0.0 && self.meter.held_step.fetch_add(1, Ordering::Relaxed) < 20 {
            log::info!(
                "co-op world: copy (object {:?}, key {key}) at hull {cur:.1} before its Step ran on the host's {host:.1}",
                object_of(me)
            );
        }
    }

    /// The controller's End Step: a swap whose event never ended (the copy
    /// went in its own Step) is forgotten (keys are reused).
    fn step_swaps_end(&self) {
        if self.meter.step_swaps_live.swap(0, Ordering::Relaxed) > 0 {
            self.lock().step_swaps.clear();
        }
    }

    /// Client, after that event: the local drop back on top of the event's
    /// own change.
    fn step_swap_after(&self, me: &Instance<'_>) {
        if self.meter.step_swaps_live.load(Ordering::Relaxed) == 0 {
            return;
        }
        let key = me.key();
        let Some((cur, host)) = self.lock().step_swaps.remove(&key) else {
            return;
        };
        self.meter.step_swaps_live.fetch_sub(1, Ordering::Relaxed);
        let Some(h) = self.h.hull else { return };
        let Some(now) = me.get_f64(h).filter(|v| v.is_finite()) else {
            return;
        };
        let back = cur + (now - host);
        let _ = me.set_f64(h, back);
        self.dmg.own_write(key, Some(now), back);
    }

    fn choose_target(&self, call: &Call<'_>) {
        let role = self.role();
        if role == ROLE_OFF {
            return;
        }
        self.force_target(call, role);
    }

    fn force_target(&self, call: &Call<'_>, role: u8) {
        let Some(me) = call.instance() else { return };
        let key = me.key();
        // A player's thing (a construct, a decoy): the host says which; the
        // joining game chases the proxy it draws for it (or its own thing).
        if role != ROLE_HOST && self.aim_client(call, &me) {
            return;
        }
        let (enemy, shot) = {
            let st = self.lock();
            // An enemy's assignment, or a shared shot's player (homing).
            if role == ROLE_HOST {
                (
                    st.host.assignment(key),
                    st.shots.host.target_of(key).flatten(),
                )
            } else {
                (
                    st.client.assignment(key),
                    st.shots.client.target_of(key).flatten(),
                )
            }
        };
        // Host: after a partner's avatar, that partner's decoy, egg or
        // construct when the game's rule picks one (`decoy_glue`).
        let mut pick = None;
        if let Some(p) = enemy {
            let remote = Some(p) != self.lock().me;
            let own = if remote {
                self.own_pick(call, &me)
            } else {
                None
            };
            match after_hook(remote, own) {
                AfterHook::KeepOwnThing => Self::standin_target_kept(key),
                AfterHook::GamesOwn | AfterHook::Avatar => {
                    self.force_at(&me, p);
                    if role == ROLE_HOST {
                        pick = self.decoy_pick(call, &me, p);
                    }
                }
            }
        } else if let Some(p) = shot {
            // A shot only when it is after a player (an orb feeder steering at
            // its ball keeps it): on the host its game's choice says so; on a
            // joining screen the host's word (`steer`), not this game's.
            let Ok(id) = me.builtin_value("id") else {
                return;
            };
            let after = if role == ROLE_HOST {
                self.shot_after_player(call, &id)
            } else {
                let s = self.lock().shots.client.steer_of(key);
                s.is_some_and(|(_, s)| s == ndml_net::shots::steer::PLAYER)
            };
            self.shot_aim_now(call, &id, Some(p), after);
            if role == ROLE_HOST && after {
                pick = self.decoy_pick(call, &me, p);
            }
        }
        if role == ROLE_HOST {
            self.aim_host(call, &me, pick);
        }
    }

    /// The game's own pick for the enemy `me`, as `chooseTarget` just wrote
    /// it ([`OwnPick`]): `None` when its `target` names no instance or it
    /// prefers no type. A player's thing by the shots' rule (an avatar, or
    /// player-side by the mirror's: a ship, a construct, a decoy, a proxy).
    fn own_pick(&self, call: &Call<'_>, me: &Instance<'_>) -> Option<OwnPick> {
        let (tv, pv) = (self.h.target?, self.h.preferred?);
        let preferred = me.get_value(pv).as_ref().and_then(object_index)?;
        let t = me.get_value(tv).filter(is_instance)?;
        let mut obj = None;
        let _ = call.with(&t, |inst, _| obj = object_of(&inst));
        let obj = obj?;
        let id = me.builtin_value("id").ok()?;
        Some(OwnPick {
            preferred: self.ancestor(call, obj, Some(preferred)),
            player: self.shot_after_player(call, &id),
        })
    }

    /// Point `me` (an enemy or a shared shot) at player `p`'s avatar when `p`
    /// is not this game's player. This game's own player's: the game's own
    /// choice (cloak, decoys, constructs act as in solo play).
    pub(super) fn force_at(&self, me: &Instance<'_>, p: PeerId) {
        let (forced, sps) = {
            let st = self.lock();
            let remote = Some(p).filter(|p| Some(*p) != st.me);
            (
                remote.and_then(|p| st.ents.target(EntityKey::ship(p))),
                st.sps,
            )
        };
        let Some((id, d)) = forced else {
            // The game's own choice (the host's now) stands, past a bracket's end.
            Self::standin_target_kept(me.key());
            return;
        };
        // Inside an identity bracket for it: the ship, standing at the
        // joining player's place.
        let id = Self::standin_target(me.key(), id);
        let h = &self.h;
        if let Some(v) = h.target
            && me.set(v, id).is_ok()
        {
            let k = 1.0 / sps;
            for (v, val) in [
                (h.target_x, f64::from(d.pos.x)),
                (h.target_y, f64::from(d.pos.y)),
                (h.target_hs, f64::from(d.vel.x) * k),
                (h.target_vs, f64::from(d.vel.y) * k),
            ] {
                if let Some(v) = v {
                    let _ = me.set_f64(v, val);
                }
            }
        }
    }

    /// Before a create (any [`CreatePath`]): the client's veto. Returns what
    /// the after hook needs to record a part.
    fn before_create(&self, call: &mut Call<'_>, path: CreatePath) -> Option<PartNote> {
        let role = self.role();
        if role == ROLE_OFF {
            return None;
        }
        self.create_veto(call, role, path)
    }

    #[allow(clippy::too_many_lines)] // one decision per create, kept in one place
    fn create_veto(&self, call: &mut Call<'_>, role: u8, path: CreatePath) -> Option<PartNote> {
        // The mirror's own proxy (the partner's thing drawn here): never the
        // world's, whatever its object counts as.
        if crate::mirror::glue::making_proxy() {
            return None;
        }
        let obj = match path {
            CreatePath::Script => call.arg_value(2).as_ref().and_then(object_index)?,
            CreatePath::Depth | CreatePath::Layer => {
                call.arg_value(3).as_ref().and_then(object_index)?
            }
            // A copy of the caller, of the caller's object.
            CreatePath::Copy => call.instance().as_ref().and_then(object_of)?,
        };
        let class = self.class_of(call, obj, false)?;
        if path == CreatePath::Copy && class.is_enemy() {
            // `instance_copy` of an enemy (a Serpent building its chain).
            // Host: its first state says so, and the copy's set-up step is
            // left out. Client: it is never the host's, and a veto returns
            // `noone` to code that uses the result, so it is let through and
            // removed at its first tick.
            return Some(if role == ROLE_HOST {
                PartNote::HostCopy
            } else {
                PartNote::Scratch
            });
        }
        if let Class::Marker { portal } = class {
            // Host: the spawner's telegraphs go to the clients (other
            // creators, a puppet's own warnings, stay local).
            if role == ROLE_HOST {
                let caller = call.instance().as_ref().and_then(object_of);
                let spawner = self.lock().objects.as_ref().and_then(|o| o.spawner);
                if caller.is_some() && caller == spawner {
                    return Some(PartNote::Marker(portal));
                }
            }
            return None;
        }
        if class == Class::EnemyShot {
            // Shared enemy shots: the host notes them, a joining game makes
            // its copies' scratch.
            if let Some(note) = self.shot_create(call, role, obj) {
                return Some(note);
            }
        }
        // A beam's charge-up (an effect): the host's with the beams.
        if class == Class::Other
            && let Some(note) = self.shot_create(call, role, obj)
        {
            return Some(note);
        }
        // An enemy's wind-up (`windup.rs`): the host's, a copy's own scratch.
        if class == Class::Other
            && let Some(note) = self.windup_create(call, role, obj)
        {
            return Some(note);
        }
        // Host: what the wave code makes that is not an enemy (comets, black
        // holes, the final boss's controller) is replicated when made; not one
        // placed on a named layer (`instance_create_layer`: the game's
        // presentation, which each game plays itself).
        if role == ROLE_HOST
            && class == Class::Other
            && path != CreatePath::Layer
            && self.world_spawn_before(call, obj)
        {
            return Some(PartNote::World);
        }
        // Client: an object that arrived as a dependency is handled as an
        // enemy: what a copy makes of it is scratch, its own creates vetoed.
        // Not when a telegraph makes it (the client's `oEnemyPortal` needs
        // its effects).
        let dep_kind = role == ROLE_CLIENT
            && class == Class::Other
            && self.is_dep_kind(obj)
            && !call
                .instance()
                .as_ref()
                .and_then(object_of)
                .and_then(|o| self.class_of(call, o, false))
                .is_some_and(|c| matches!(c, Class::Marker { .. }));
        if !class.replicated() && !dep_kind {
            return None;
        }
        if role == ROLE_HOST && class == Class::Collectable {
            // Shared pickups: sent once its maker has set it up.
            return Some(PartNote::Orb);
        }
        let caller = call.instance();
        let caller_key = caller.as_ref().map(Instance::key);
        // Only asked for enemy creates (rare next to shots and effects).
        let caller_enemy = class.is_enemy()
            && caller
                .as_ref()
                .and_then(object_of)
                .and_then(|o| self.class_of(call, o, false))
                .is_some_and(Class::is_enemy);
        if role == ROLE_HOST {
            // Generic transfer: parts, members and missiles are replicated
            // like any enemy. An enemy something else makes (the Star Eater's
            // controller): its maker may be a dependency.
            if class.is_enemy()
                && !caller_enemy
                && let Some(c) = caller.as_ref()
                && let Some(o) = object_of(c).and_then(|o| u16::try_from(o).ok())
                && let Ok(id) = c.builtin_value(bv::ID)
            {
                self.lock().deps.makers_pending.push((c.key(), o, id));
                return Some(PartNote::Made);
            }
            // A member a replicated instance makes: what the maker gains in
            // that frame goes ahead of the member's spawn.
            if class.is_enemy() && caller_key.is_some_and(|k| self.lock().host.is_tracked(k)) {
                return Some(PartNote::Member);
            }
            return None;
        }
        let replicating = self.replicating.load(Ordering::Relaxed);
        let (fresh_owner, owner, caller_owner, by_building, by_scratch, by_death) = {
            let st = self.lock();
            let fresh = caller_key.filter(|k| st.client.is_fresh(*k));
            (
                fresh.and_then(|k| st.client.owner_of(k)),
                st.owner,
                caller_key.and_then(|k| st.client.owner_of(k)),
                st.building.is_some() && st.building == caller_key,
                caller_key.is_some_and(|k| st.transfer.scratch.iter().any(|s| s.0 == k)),
                // A copy's Destroy event (`die_like_host`): the boxes and
                // cars it breaks into, a dweller's spawn.
                st.copy_deaths > 0,
            )
        };
        let by_copy = by_building || caller_owner.is_some() || fresh_owner.is_some() || by_death;
        // Generic transfer: an enemy a copy (or one of these) makes is let
        // through, so the code that made it goes on as in the game (a vetoed
        // create returns `noone`, which crashes or loops those callers), and
        // removed silently at its first tick or after the set-up step (the
        // host's arrives). A dependency copy makes no enemies of its own.
        let by_dep = caller
            .as_ref()
            .and_then(object_of)
            .is_some_and(|o| self.is_dep_kind(o));
        if (by_copy || by_scratch || by_dep) && (class.is_enemy() || dep_kind) {
            return Some(PartNote::Scratch);
        }
        if !replicating && fresh_owner.is_none() {
            call.skip_original();
            if let Err(e) = call.set_return(names::NOONE) {
                warn_once("world-veto", &format!("co-op world: create veto: {e}"));
            }
            {
                // A dying asteroid's debris and drops: expected, not logged.
                let st = self.lock();
                if caller_key.is_some_and(|k| st.dying.contains_key(&k)) {
                    return None;
                }
            }
            let by = call.instance().as_ref().and_then(object_of);
            info_once(&format!("world-veto-{obj}-{by:?}"), &{
                let by = by.map_or_else(|| "?".to_owned(), |b| self.object_name(call, b));
                format!(
                    "co-op world: vetoed a local create of {} (by {by})",
                    self.object_name(call, obj)
                )
            });
            return None;
        }
        if !class.is_enemy() {
            return None;
        }
        let owner = if replicating && caller_enemy {
            owner
        } else {
            fresh_owner
        };
        // No table takes it: noted, so that if it turns up as a stray its
        // maker is named (the game's own code, a copy gone from the tables,
        // ...).
        Some(owner.map_or_else(
            || PartNote::Untracked(caller.as_ref().and_then(object_of)),
            PartNote::Client,
        ))
    }

    fn after_create(&self, call: &Call<'_>, note: Option<PartNote>) {
        let Some(note) = note else { return };
        if let PartNote::Marker(portal) = note {
            if let Some(id) = call.result().filter(is_instance) {
                self.lock().new_markers.push((id, portal));
            }
        } else if let PartNote::SharedShot { scratch } = note {
            self.shot_created(call, scratch);
        } else if let PartNote::Windup { scratch, free } = note {
            self.windup_created(call, scratch, free);
        } else if let PartNote::Made = note {
            self.dep_maker_made(call);
            self.gen_member_made(call);
        } else if let PartNote::Member = note {
            self.gen_member_made(call);
        } else if let PartNote::World = note {
            self.world_spawn_after(call);
        } else if let PartNote::Orb = note {
            if let Some(id) = call.result().filter(is_instance) {
                self.orb_made(id);
            }
        } else {
            self.record_part(call, note);
        }
    }

    fn record_part(&self, call: &Call<'_>, note: PartNote) {
        let Some(id) = call.result().filter(is_instance) else {
            return;
        };
        let mut key = None;
        let _ = call.with(&id, |inst, _| key = Some(inst.key()));
        let Some(key) = key else { return };
        let mut st = self.lock();
        match note {
            PartNote::Client(owner) => st.client.record_part(owner, key, id),
            PartNote::Scratch => {
                let frame = st.frame;
                st.transfer.scratch.push((key, id, frame));
            }
            PartNote::Untracked(maker) => {
                let (frame, frozen) = (st.frame, st.frozen);
                st.untracked.retain(|_, u| frame.saturating_sub(u.1) < 600);
                st.untracked.insert(key, (maker, frame, frozen));
            }
            PartNote::HostCopy => {
                let frame = st.frame;
                st.transfer.copy_born.insert(key, frame);
            }
            PartNote::Marker(_)
            | PartNote::SharedShot { .. }
            | PartNote::Windup { .. }
            | PartNote::Made
            | PartNote::Member
            | PartNote::World
            | PartNote::Orb => {}
        }
    }

    /// Before `checkDestroy*`: the host sends a dying instance's queued first
    /// state now (and its last, ahead of the kill); a client skips it for
    /// puppets and their parts. Returns (skipped, the on-death flags lent
    /// for the path: host).
    fn before_destroy_check(&self, call: &mut Call<'_>) -> (bool, u8) {
        if self.role() == ROLE_HOST {
            // About to die with its first state still queued: sent now.
            if let Some(me) = call.instance()
                && self
                    .h
                    .hull
                    .and_then(|v| me.get_f64(v))
                    .is_some_and(|h| h <= 0.0)
            {
                self.gen_host_first_now(call, me.key());
                // Its copy's Destroy event reads what this one's does.
                self.gen_host_last_now(call, me.key());
                return (false, self.ondeath_lend(call));
            }
            return (false, 0);
        }
        if self.role() != ROLE_CLIENT {
            return (false, 0);
        }
        // A copy dying the host's death (its Destroy event, the Dweller's
        // calls `checkDestroy`): the host ran its death path.
        let owned = self.lock().copy_deaths > 0
            || call.instance().is_some_and(|me| {
                let key = me.key();
                self.lock().client.owns(key) && !self.dmg.is_dying(key)
            });
        if owned {
            call.skip_original();
            let _ = call.set_return(0.0);
        }
        (owned, 0)
    }

    /// After `checkDestroy*`: the host gives back what it lent the path and
    /// sends a kill at once; a client that ran the death path removes the
    /// enemies it made.
    fn after_destroy_check(&self, call: &Call<'_>, (skipped, lent): (bool, u8)) {
        self.ondeath_unlend(call, lent);
        if !skipped && self.role() == ROLE_CLIENT {
            // A copy dying here (a kill this game predicted) runs its death
            // path, which can make enemies (a Serpent link's death splits the
            // chain): they go before anyone's Step reads their unset state.
            // The host's arrive with their state.
            self.gen_scratch_clear(call, 0);
            return;
        }
        if skipped || self.role() != ROLE_HOST {
            return;
        }
        self.report_kill(call);
    }

    fn report_kill(&self, call: &Call<'_>) {
        let Some(me) = call.instance() else { return };
        let dead = self
            .h
            .hull
            .and_then(|v| me.get_f64(v))
            .is_some_and(|h| h <= 0.0);
        if !dead {
            return;
        }
        let key = me.key();
        let Some(id) = self.lock().host.id_of(key) else {
            return;
        };
        if !self.exists(call, &id) {
            let mut st = self.lock();
            st.host.killed(key);
        }
    }

    /// The players' facts the world reads of each other (cover, decoys,
    /// on-death mods).
    fn facts_inbox(&self, inbox: &mut Vec<Message>) {
        self.cover_inbox(inbox);
        self.decoy_inbox(inbox);
        self.ondeath_inbox(inbox);
    }

    /// This game's facts to the partners (cover, decoys, on-death mods).
    fn facts_run_step(&self, call: &Call<'_>, me: PeerId) -> Vec<Message> {
        let mut out = self.cover_run_step(call, Some(me));
        out.extend(self.decoy_run_step(call, Some(me)));
        out.extend(self.ondeath_run_step(call, me));
        out
    }

    fn skip_on_client(&self, call: &mut Call<'_>) {
        if self.role() == ROLE_CLIENT {
            call.skip_original();
        }
    }

    /// Client, before a script in [`COPY_VIEW_DEATHS`] or `destroyOutside`
    /// (the shots' bracket calls it first): not on a replicated copy (the
    /// host's ends it, by the host's view, and its end comes as a despawn).
    /// Keyed on the instance being a copy, whatever its object. Returns
    /// whether it was skipped.
    pub(super) fn skip_view_death(&self, call: &mut Call<'_>) -> bool {
        if self.role() != ROLE_CLIENT {
            return false;
        }
        let Some(me) = call.instance() else {
            return false;
        };
        if !self.lock().client.owns(me.key()) {
            return false;
        }
        call.skip_original();
        true
    }
}

/// Scripts by which an enemy ends itself for being outside this game's view
/// (`viewX`, `viewY`, `viewWidth`, `viewHeight`), besides the game's general
/// one, `destroyOutside`, which no replicated copy runs either (the shots'
/// bracket, `shots_glue.rs`, calls [`WorldHooks::skip_view_death`] first):
/// the two that test the view inline. The joining game's view is not the
/// host's, so on a copy they would end it early or late; the host's end comes
/// as a despawn. A list by name: nothing else in the game marks these two.
pub(super) const COPY_VIEW_DEATHS: [&str; 2] = [
    "gml_Script_starEaterDestroyOutside",
    // The cargo engine's and cars' Step: past the view and moving away, the
    // train ends (`destroyOutside`, `instance_destroy`).
    "gml_Script_destroyTrain",
];

/// Where an instance the ticks missed is (`WorldHooks::presence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Presence {
    Active,
    /// Deactivated by the game: not gone. Leaving a freeze (`activateRegion`)
    /// brings back only the region around the view; the rest stays
    /// deactivated.
    Asleep,
    Gone,
}

/// A part to record after its create.
#[derive(Debug, Clone, Copy)]
enum PartNote {
    /// Client: an instance a puppet's code makes outside a copy's set-up
    /// (noted with its owner).
    Client(NetId),
    /// Not a part: a spawn telegraph to send (`true` = portal).
    Marker(bool),
    /// Shared enemy shots: the host's shot to read at the End Step, or a
    /// joining game's scratch (`scratch`; `false` there: the mod's own).
    SharedShot { scratch: bool },
    /// An enemy's wind-up (`windup.rs`): the host's birth to check at the
    /// frame's end, or a joining game's copy's own (`scratch`); `free`: what
    /// a shared shot's ending made, or its lineage (`windup::free`): the
    /// host's with no follow test.
    Windup { scratch: bool, free: bool },
    /// Client: an enemy a copy's set-up step made (generic transfer).
    Scratch,
    /// Client: an enemy no table takes (its maker's object, if any).
    Untracked(Option<u32>),
    /// Host: an enemy `instance_copy` made.
    HostCopy,
    /// Host: an enemy a non-enemy made (its maker waits in
    /// `deps.makers_pending`).
    Made,
    /// Host: a world spawn, taken after its create.
    World,
    /// Host: an enemy a replicated instance made (generic transfer): its
    /// maker's gains go ahead of its spawn (`gen_maker_repairs`).
    Member,
    /// Host: a collectable (a shared pickup), sent the next run frame.
    Orb,
}

/// How an instance is born: the game's functions that create one. With the
/// loader's builtin hooks every birth is classified at the three builtins;
/// without them, at the script `instance_create` alone (`Script`, which calls
/// `instance_create_depth`: never both, or a create is classified twice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreatePath {
    /// `gml_Script_instance_create(x, y, object)`.
    Script,
    /// `instance_create_depth(x, y, depth, object)`.
    Depth,
    /// `instance_create_layer(x, y, layer, object)`.
    Layer,
    /// `instance_copy(perform_create)`: a copy of the caller.
    Copy,
}

/// The builtins that create instances, with their paths.
const CREATE_BUILTINS: [(&str, CreatePath); 3] = [
    ("instance_create_depth", CreatePath::Depth),
    ("instance_create_layer", CreatePath::Layer),
    ("instance_copy", CreatePath::Copy),
];

fn hook_or_warn(name: &str, r: Result<(), Error>) {
    if let Err(e) = r {
        log::warn!("co-op world: unable to hook {name} ({e})");
    }
}

/// Every birth through one place: the builtins that create instances, or,
/// when the loader cannot hook builtins, the script `instance_create` (then
/// `createSafe`, `instance_copy`, ... are unseen).
fn register_creates(ctx: &ModContext, w: &Arc<WorldHooks>) {
    let mut builtins = 0;
    let mut depth = false;
    for (name, path) in CREATE_BUILTINS {
        let (c1, c2) = (Arc::clone(w), Arc::clone(w));
        match ctx.hook_builtin(
            name,
            move |call| c1.before_create(call, path),
            move |call, note| c2.after_create(call, note),
        ) {
            Ok(()) => {
                builtins += 1;
                depth |= path == CreatePath::Depth;
            }
            Err(e) => log::warn!("co-op world: builtin {name}: {e}"),
        }
    }
    // The script calls `instance_create_depth`: hooked at the script only
    // when that builtin is not (never both).
    if depth {
        log::info!("co-op world: creates classified at the creating builtins ({builtins} hooked)");
    } else {
        log::warn!(
            "co-op world: creates classified at gml_Script_instance_create only (builtin hooks: {builtins} of {})",
            CREATE_BUILTINS.len()
        );
        let (c1, c2) = (Arc::clone(w), Arc::clone(w));
        hook_or_warn(
            names::INSTANCE_CREATE,
            ctx.hook(
                names::INSTANCE_CREATE,
                move |call| c1.before_create(call, CreatePath::Script),
                move |call, note| c2.after_create(call, note),
            ),
        );
    }
}

/// The set-up rule (host): which instances ran user event 0 before their
/// first state. Trusted once it has seen one (a refused or idle observer
/// leaves the `instance_copy` rule in charge).
fn register_setup_observer(ctx: &ModContext, w: &Arc<WorldHooks>) {
    let c = Arc::clone(w);
    match ctx.observe_event(ndml_sdk::EVENT_OTHER, ndml_sdk::user_event(0), move |e| {
        if c.role() == ROLE_HOST
            && let Some(i) = e.instance()
        {
            let mut st = c.lock();
            if e.after {
                st.transfer.in_setup.remove(&i.key());
                return;
            }
            let frame = st.frame;
            st.transfer.setup_seen.insert(i.key(), frame);
            st.transfer.in_setup.insert(i.key());
            st.transfer.observer = true;
        }
    }) {
        Ok(()) => log::info!("co-op world: observing user event 0 (the set-up rule)"),
        Err(e) => log::warn!("co-op world: {e}; the set-up rule stays the instance_copy one"),
    }
    // A maker's user event 1 on a member (the cargo engine's
    // `with (car) event_user(1)`, which makes the car's trails): the
    // member's first state says so and the copy runs it after its set-up.
    // Not when the instance runs it from its own user event 0 (a Serpent
    // link builds its chain there, which the set-up step repeats anyway).
    let c = Arc::clone(w);
    match ctx.observe_event(ndml_sdk::EVENT_OTHER, ndml_sdk::user_event(1), move |e| {
        if !e.after
            && c.role() == ROLE_HOST
            && let (Some(i), Some(o)) = (e.instance(), e.other())
            && o.key() != i.key()
        {
            let mut st = c.lock();
            if !st.transfer.in_setup.contains(&i.key()) {
                let frame = st.frame;
                st.transfer.event1_by_maker.insert(i.key(), frame);
            }
        }
    }) {
        Ok(()) => log::info!("co-op world: observing user event 1 (a maker's on its members)"),
        Err(e) => log::warn!("co-op world: {e}; members' user event 1 stays the host's only"),
    }
}

/// Destroy events on the host: a replicated instance (enemy or dependency)
/// that ends with its Destroy event is despawned as `Destroyed` (`Killed` if
/// it died) and the joining game runs its copy's Destroy event too; one that
/// ends without is `Removed` ([`HostWorld::gone_with`]). The event is watched
/// because YYC code calls `instance_destroy` directly, past the builtin hook
/// ([`WorldHooks::host_destroying`]), as in `oStarEaterController_Step_0`.
fn register_dep_destroy_observer(ctx: &ModContext, w: &Arc<WorldHooks>) {
    /// GameMaker's Destroy event kind.
    const EVENT_DESTROY: i32 = 1;
    let c = Arc::clone(w);
    match ctx.observe_event(EVENT_DESTROY, 0, move |e| {
        if !e.after
            && c.role() == ROLE_HOST
            && let Some(i) = e.instance()
        {
            c.destroyed_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(i.key());
        }
    }) {
        Ok(()) => log::info!("co-op world: observing Destroy events (despawn causes)"),
        Err(e) => log::warn!(
            "co-op world: {e}; a dependency destroyed by compiled code goes silently on the joining side"
        ),
    }
}

/// Client: before every Step and Begin Step that runs code, a copy's hull
/// below the host's is set to the host's while the host's lives as shown here
/// ([`WorldHooks::step_swap_before`]). One hook for all objects, so one whose
/// Step starts its own death (the Star Eater's controller, a boss's phases)
/// is covered unnamed. Not inside a call made under the world lock (a Step
/// the mod runs itself). A copy also never ends itself by this game's view
/// ([`COPY_VIEW_DEATHS`]).
fn register_copy_step_hold(ctx: &ModContext, w: &Arc<WorldHooks>) {
    /// GameMaker's Step event kind; numbers 0 Step, 1 Begin Step, 2 End Step.
    const EVENT_STEP: i32 = 3;
    const END_STEP: i32 = 2;
    let c = Arc::clone(w);
    match ctx.observe_event(EVENT_STEP, -1, move |e| {
        if e.number == END_STEP
            || c.role() != ROLE_CLIENT
            || crate::game::held_at("world").is_some()
        {
            return;
        }
        if let Some(i) = e.instance() {
            // The emerging family's own Step: its emerge held behind the
            // host's when the rules say so (`emerge_glue`).
            let emerging = e.number == 0 && c.emerging_instance(e.instance_object);
            if e.after {
                c.step_swap_after(&i);
                if emerging {
                    c.emerge_hold_after(&i);
                }
            } else {
                c.step_swap_before(&i);
                if emerging {
                    c.emerge_hold_before(&i);
                }
            }
        }
    }) {
        Ok(()) => log::info!(
            "co-op world: copies' own Steps run on the host's hull while the host's live"
        ),
        Err(e) => log::warn!(
            "co-op world: {e}; a copy whose Step starts its own death may die before the host's"
        ),
    }
    for name in COPY_VIEW_DEATHS {
        let c = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook_before(name, move |call| {
                c.skip_view_death(call);
            }),
        );
    }
}

/// Deaths: `checkDestroy*` (the client skips it for copies, the host sends
/// the kill) and, on a client, what a copy's Destroy event must not run
/// ([`names::HOST_DEATH_ONLY`]).
fn register_deaths(ctx: &ModContext, w: &Arc<WorldHooks>) {
    for name in names::CHECK_DESTROY {
        let (c1, c2) = (Arc::clone(w), Arc::clone(w));
        hook_or_warn(
            name,
            ctx.hook(
                name,
                move |call| c1.before_destroy_check(call),
                move |call, before| c2.after_destroy_check(call, before),
            ),
        );
    }
    for name in names::HOST_DEATH_ONLY {
        let c = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook_before(name, move |call| c.skip_in_copy_death(call)),
        );
    }
    let c = Arc::clone(w);
    if let Err(e) = ctx.hook_builtin(
        "instance_destroy",
        move |call| c.host_destroying(call),
        |_, ()| {},
    ) {
        log::warn!(
            "co-op world: builtin instance_destroy: {e}; enemies that go off by themselves are removed silently on the joining side"
        );
    }
}

/// Register the shared world's hooks (none when it is off).
pub fn register(ctx: &ModContext, w: &Arc<WorldHooks>) {
    if !w.on {
        return;
    }
    if w.h.reflect.is_none() {
        log::warn!(
            "co-op world: this game lacks a reflection builtin (variable_instance_get_names and kin); the host's enemies and shots are not made on the joining game"
        );
    }
    let c = Arc::clone(w);
    hook_or_warn(
        names::TICK,
        ctx.hook_after(names::TICK, move |call| c.tick(call)),
    );
    let c = Arc::clone(w);
    hook_or_warn(
        names::ACTIVATE_REGION,
        ctx.hook_after(names::ACTIVATE_REGION, move |call| c.wake_copies(call)),
    );
    let c = Arc::clone(w);
    hook_or_warn(
        names::CONTROLLER_END_STEP,
        ctx.hook_after(names::CONTROLLER_END_STEP, move |call| {
            c.emerging_end_step(call);
            c.shots_end_step(call);
            c.lit_end_step(call);
            c.step_swaps_end();
        }),
    );
    shot_glue::register(ctx, w);
    let c = Arc::clone(w);
    hook_or_warn(
        names::CHOOSE_TARGET,
        ctx.hook_after(names::CHOOSE_TARGET, move |call| c.choose_target(call)),
    );
    register_creates(ctx, w);
    register_setup_observer(ctx, w);
    register_dep_destroy_observer(ctx, w);
    death_fx::register(ctx, w);
    let c = Arc::clone(w);
    hook_or_warn(
        names::CONTROLLER_END_STEP,
        ctx.hook_before(names::CONTROLLER_END_STEP, move |call| {
            c.pose_at_end_step(call);
        }),
    );
    view::resolve(ctx);
    register_view(ctx, w);
    area_glue::register(ctx, w);
    cover_glue::register(ctx, w);
    decoy_glue::register(ctx);
    standin_glue::register(ctx, w);
    lit_glue::register(ctx, w);
    orbs_glue::register(ctx, w);
    corrosion_glue::register(ctx, w);
    ondeath_glue::register(ctx, w);
    register_deaths(ctx, w);
    // Who runs a Serpent's user event 1 on the joining side (it builds the
    // chain with `instance_copy`, which the create hook does not see).
    {
        let c = Arc::clone(w);
        hook_or_warn(
            names::SERPENT_CHAIN,
            ctx.hook_before(names::SERPENT_CHAIN, move |call| c.log_chain_build(call)),
        );
    }
    for name in names::WAVE_ALARMS
        .iter()
        .copied()
        .chain([names::SPAWN_ENEMIES])
    {
        let c = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook_before(name, move |call| c.skip_on_client(call)),
        );
    }
    register_copy_step_hold(ctx, w);
    log::info!(
        "co-op world: hooks on (colour {:?}, names {}, marker {})",
        w.settings.color,
        w.settings.show_names,
        w.settings.show_marker
    );
}

/// Before the draw: copies turned after their tick are put back.
fn register_draw_begin(ctx: &ModContext, w: &Arc<WorldHooks>) {
    let c = Arc::clone(w);
    hook_or_warn(
        names::DRAW_BEGIN,
        ctx.hook_after(names::DRAW_BEGIN, move |call| c.angles_before_draw(call)),
    );
}

/// The world view's hooks: the world calls, the easers, the zoom writers;
/// and the copies' angles put back before the draw.
fn register_view(ctx: &ModContext, w: &Arc<WorldHooks>) {
    register_draw_begin(ctx, w);
    for name in view::CALLS {
        let c = Arc::clone(w);
        let d = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook(
                name,
                move |call| c.view_call(call, name),
                move |call, ()| d.view_call_after(call, name),
            ),
        );
    }
    // What a recorded world call makes (`view::MADE_MARK`).
    for (name, _) in CREATE_BUILTINS {
        let c = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook_builtin(name, |_| {}, move |call, ()| c.view_create_after(call)),
        );
    }
    for name in view::EASERS {
        let c = Arc::clone(w);
        hook_or_warn(name, ctx.hook_before(name, move |call| c.view_write(call)));
    }
    // The host-only functions whose part on this player's objects a joining
    // game repeats on its own (`view::director::OWN_RUNS`): counted here.
    for name in view::director::OWN_RUNS {
        hook_or_warn(
            name,
            ctx.hook_after(name, move |_| view::director::ran(name)),
        );
    }
    // An enemy's phase machine: the host's advance is sent, a copy's own
    // vetoed (`view::phase`).
    {
        let name = view::phase::ADVANCE;
        let (c1, c2) = (Arc::clone(w), Arc::clone(w));
        hook_or_warn(
            name,
            ctx.hook(
                name,
                move |call| c1.phase_before(call),
                move |call, ()| c2.phase_after(call),
            ),
        );
    }
    // An enemy's own phase clock (`view::phase::PHASE_CLOCKS`): the host's
    // run is sent, a copy's own skipped.
    for &(object, alarm, _) in view::phase::PHASE_CLOCKS {
        let name = view::phase::clock_event(object, alarm);
        let (c1, c2) = (Arc::clone(w), Arc::clone(w));
        hook_or_warn(
            &name,
            ctx.hook(
                &name,
                move |call| c1.clock_before(call),
                move |call, ()| c2.clock_after(call),
            ),
        );
    }
    {
        let name = view::ENDING_STEP;
        let c = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook(name, |_| {}, move |call, ()| c.view_formation(call)),
        );
    }
    {
        let name = view::ENDING_BEGIN;
        let c = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook(name, |_| {}, move |call, ()| c.view_ending_began(call)),
        );
    }
    {
        let name = "gml_Script_advanceBGM";
        let c = Arc::clone(w);
        hook_or_warn(
            name,
            ctx.hook_before(name, move |call| c.view_playlist(call)),
        );
    }
    for name in view::ZOOM_WRITERS {
        let (c1, c2) = (Arc::clone(w), Arc::clone(w));
        hook_or_warn(
            name,
            ctx.hook(
                name,
                move |call| c1.zoom_writer_before(call),
                move |call, swapped| c2.zoom_writer_after(call, swapped),
            ),
        );
    }
}

#[cfg(test)]
mod after_hook_tests {
    use super::{AfterHook, OwnPick, after_hook, keeps_own_pick};

    const OWN_BALL: OwnPick = OwnPick {
        preferred: true,
        player: false,
    };

    #[test]
    fn an_enemy_after_the_joining_player_keeps_its_own_ball() {
        assert_eq!(after_hook(true, Some(OWN_BALL)), AfterHook::KeepOwnThing);
        assert!(keeps_own_pick(Some(OWN_BALL)));
    }

    #[test]
    fn the_hosts_ship_construct_or_decoy_is_pointed_at_the_joining_players_avatar() {
        // The host's ship, construct or decoy (Guardian): player-side, not
        // the type the enemy prefers.
        let host_thing = OwnPick {
            preferred: false,
            player: true,
        };
        assert_eq!(after_hook(true, Some(host_thing)), AfterHook::Avatar);
        // Even an enemy whose preferred type a player's thing is: a
        // player's thing is never its own.
        let preferred_player = OwnPick {
            preferred: true,
            player: true,
        };
        assert_eq!(after_hook(true, Some(preferred_player)), AfterHook::Avatar);
        // A world instance it does not prefer (another enemy), no pick, no
        // preferred type: the remote player's avatar.
        let other = OwnPick {
            preferred: false,
            player: false,
        };
        assert_eq!(after_hook(true, Some(other)), AfterHook::Avatar);
        assert_eq!(after_hook(true, None), AfterHook::Avatar);
    }

    #[test]
    fn an_enemy_after_this_games_player_is_left_alone() {
        for pick in [None, Some(OWN_BALL)] {
            assert_eq!(after_hook(false, pick), AfterHook::GamesOwn);
        }
    }
}
