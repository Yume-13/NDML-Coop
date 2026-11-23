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

//! GML identifiers the shared world uses (Nova Drift 1.2.38.4).

#![forbid(unsafe_code)]

/// The per-enemy End Step tick: last call of `oEnemyStandard_Step_2` and of
/// every segment, asteroid, mine and Warbringer part. Runs exactly once per
/// frame for every `oSolid` instance, `self` = the enemy.
pub const TICK: &str = "gml_Script_decrementVulnerability";
/// `oBackgroundColor`'s Draw Begin: after every End Step, before any Draw;
/// called several times a frame, in every room, active through freezes.
/// Copies' poses are put back there (`angles_before_draw`).
pub const DRAW_BEGIN: &str = "gml_Object_oBackgroundColor_Draw_72";
/// The controller's End Step: after every instance's Step and before every
/// copy's and every wind-up's End Step (events run in object index order;
/// the controller is 377: copies are posed there).
pub const CONTROLLER_END_STEP: &str = "gml_Object_oController_Step_2";
/// Enemy (and homing shot) target choice; copies the target's position into
/// `targetX/Y/Hspeed/Vspeed`. Overwriting those after it works.
pub const CHOOSE_TARGET: &str = "gml_Script_chooseTarget";
/// `instance_create(x, y, obj)` (502 callers): the client's veto, part
/// tracking on both sides, and the client's host-ordered spawns.
pub const INSTANCE_CREATE: &str = "gml_Script_instance_create";
/// Leaving a freeze: activates the region around the view.
pub const ACTIVATE_REGION: &str = "gml_Script_activateRegion";
/// Enemy deaths: `if hull <= 0` explosion, drops, `instance_destroy`.
pub const CHECK_DESTROY: [&str; 3] = [
    "gml_Script_checkDestroy",
    "gml_Script_checkDestroyReduced",
    "gml_Script_checkDestroyNoExplosion",
];
/// What a death runs that belongs to the host's game (boss drops and run
/// save, achievements, on-kill procs). Skipped on a client while a copy dies.
pub const HOST_DEATH_ONLY: [&str; 4] = [
    "gml_Script_bossDrops",
    "gml_Script_getCheevo",
    "gml_Script_procEffectsOnDestroy",
    "gml_Script_spontaneousGeneration",
];
/// The wave director's alarms (1 wave tick, 2 threat, 3 heist train, 4 black
/// holes and comets, 7 Hostile Universe, 8 Serpentine Drift). Skipped on a
/// client; skipped alarms don't re-arm themselves.
pub const WAVE_ALARMS: [&str; 6] = [
    "gml_Object_oWaves_Alarm_1",
    "gml_Object_oWaves_Alarm_2",
    "gml_Object_oWaves_Alarm_3",
    "gml_Object_oWaves_Alarm_4",
    "gml_Object_oWaves_Alarm_7",
    "gml_Object_oWaves_Alarm_8",
];
/// Creates an `oEnemySpawner` (also called by black holes): skipped on a client.
pub const SPAWN_ENEMIES: &str = "gml_Script_spawnEnemies";
/// The death effect alone (`createExplosion(undefined x4)` as the enemy).
pub const CREATE_EXPLOSION: &str = "gml_Script_createExplosion";
/// A Serpent's user event 1: builds its chain with `instance_copy`; logged
/// on the joining side (which runs it).
pub const SERPENT_CHAIN: &str = "gml_Object_oSerpent_Other_11";

/// Asteroid Steps, which destroy an asteroid once `hull <= 0`; a client
/// holds its copies' hull above 0 here.
pub const ASTEROID_STEPS: [&str; 2] = [
    "gml_Object_oAsteroid_Step_0",
    "gml_Object_oAsteroidSmall_Step_0",
];
/// Instance variables.
pub mod var {
    pub const RARITY: &str = "rarity";
    /// A formation member's parent (`oHostDrone`: its `oHost`).
    pub const LEADER: &str = "leader";
    pub const TARGET: &str = "target";
    pub const TARGET_X: &str = "targetX";
    pub const TARGET_Y: &str = "targetY";
    pub const TARGET_HSPEED: &str = "targetHspeed";
    pub const TARGET_VSPEED: &str = "targetVspeed";
    /// `global.wave` (read through the global instance by `oWaves_Alarm_1`).
    pub const WAVE: &str = "wave";
    /// `oSpawnWarning`: where the enemy will appear, its size, and the edge
    /// margin its Step reads (its Create does not set it: 65 as the spawner).
    pub const SPAWN_POS_X: &str = "spawnPosX";
    pub const SPAWN_POS_Y: &str = "spawnPosY";
    pub const SCALER: &str = "scaler";
    pub const BUFFER: &str = "buffer";
    /// `oEnemyPortal`: the announced enemy object (size) and its rarity (colour).
    pub const ENEMY_TYPE: &str = "enemyType";
}

/// Built-in variables (through the runner's getters and setters).
pub mod builtin_var {
    pub const OBJECT_INDEX: &str = "object_index";
    pub const ID: &str = "id";
    pub const X: &str = "x";
    pub const Y: &str = "y";
    pub const IMAGE_ANGLE: &str = "image_angle";
    pub const IMAGE_XSCALE: &str = "image_xscale";
    pub const IMAGE_YSCALE: &str = "image_yscale";
    pub const IMAGE_BLEND: &str = "image_blend";
    pub const IMAGE_ALPHA: &str = "image_alpha";
    pub const IMAGE_INDEX: &str = "image_index";
    pub const IMAGE_SPEED: &str = "image_speed";
    pub const VISIBLE: &str = "visible";
    pub const SPEED: &str = "speed";
    pub const HSPEED: &str = "hspeed";
    pub const VSPEED: &str = "vspeed";
    pub const DEPTH: &str = "depth";
    pub const SPRITE_INDEX: &str = "sprite_index";
}

/// Builtin functions.
pub mod builtin_fn {
    pub const INSTANCE_EXISTS: &str = "instance_exists";
    pub const INSTANCE_DESTROY: &str = "instance_destroy";
    pub const INSTANCE_ACTIVATE_OBJECT: &str = "instance_activate_object";
    pub const INSTANCE_DEACTIVATE_OBJECT: &str = "instance_deactivate_object";
    pub const OBJECT_EXISTS: &str = "object_exists";
    pub const OBJECT_IS_ANCESTOR: &str = "object_is_ancestor";
    pub const ASSET_GET_INDEX: &str = "asset_get_index";
    pub const EVENT_USER: &str = "event_user";
    pub const ALARM_GET: &str = "alarm_get";
    pub const ALARM_SET: &str = "alarm_set";
    pub const VARIABLE_INSTANCE_SET: &str = "variable_instance_set";
    pub const SPRITE_EXISTS: &str = "sprite_exists";
    pub const OBJECT_GET_NAME: &str = "object_get_name";
    pub const OBJECT_GET_PARENT: &str = "object_get_parent";
    pub const INSTANCE_NUMBER: &str = "instance_number";
}

/// Objects resolved by name with `asset_get_index`.
pub mod object {
    /// The wave director (alarms; `alarm_get/set(1)` is the wave timer).
    pub const WAVES: &str = "oWaves";
    /// Inert: no events in its chain, never referenced. Remote entities (the
    /// partner's ship) are made of it.
    pub const PLAYER_ATTACK: &str = "oPlayerAttack";
    pub const PLAYER: &str = "oPlayer";
    /// The spawn warning `oEnemySpawner_Alarm_0` places (its Step fades it in,
    /// grows it, pins it to the screen edge; it deletes itself).
    pub const SPAWN_WARNING: &str = "oSpawnWarning";
    /// The warp-gate telegraph (`enemyEntry == 1`): effects only, deletes itself.
    pub const ENEMY_PORTAL: &str = "oEnemyPortal";
    /// Made by `spawnEnemies`; its `Alarm_0` places the telegraphs above.
    pub const ENEMY_SPAWNER: &str = "oEnemySpawner";
    /// Asteroids: they destroy themselves in their own Step once `hull <= 0`.
    pub const ASTEROID: &str = "oAsteroid";
}

/// GML `noone`.
pub const NOONE: f64 = -4.0;
/// `WaveState::next_wave_ms` for a stopped wave timer (`oWaves.alarm[1]` < 0,
/// boss fights). A joining game stops its own too: a wave tick at the run's
/// cap ends the run with no boss check.
pub const WAVE_TIMER_OFF: u32 = u32::MAX;
