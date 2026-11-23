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

//! The core GML identifiers; subsystems keep their own.

#![forbid(unsafe_code)]

/// Main menu step (not its input, which is `Step_2`). The menu tick.
pub const MENU_STEP: &str = "gml_Object_oMenu_Step_0";
/// Main menu GUI draw: the rows from the `str*` globals.
pub const MENU_DRAW_GUI: &str = "gml_Object_oMenu_Draw_64";
/// Run controller step; keeps running while paused. The run tick.
pub const CONTROLLER_STEP: &str = "gml_Object_oController_Step_0";
/// Run controller GUI draw (also while paused): the pause screen and the run UI.
pub const CONTROLLER_DRAW_GUI: &str = "gml_Object_oController_Draw_64";
/// The local ship's step (`self` = `oPlayer`).
pub const PLAYER_STEP: &str = "gml_Object_oPlayer_Step_0";
/// `oPowers` step: hull, shield and flags live there.
pub const POWERS_STEP: &str = "gml_Object_oPowers_Step_0";
/// End screen setup: every `steam_upload_score` call is inside its
/// `global.enableLeaderboards == 1` block.
pub const DRAW_END_SCORE_CREATE: &str = "gml_Object_oDrawEndScore_Create_0";
/// The game's teardown before the menu (`restartGame`): destroys `oLog`'s
/// `waveLog`, then the `oLog`.
pub const DESTROY_DATA_STRUCTURES: &str = "gml_Script_destroyDataStructures";
/// `oLog`'s `CleanUp`: `ds_grid_destroy(waveLog)` (a second time inside the
/// teardown).
pub const LOG_CLEANUP: &str = "gml_Object_oLog_CleanUp_0";

/// Writes the resume file `continue*.sav` (every wave, pause, upgrade).
pub const TRIGGER_SAVE_RUN: &str = "gml_Script_triggerSaveRun";
/// Deletes the resume file (at game over).
pub const DELETE_RUN_FILES: &str = "gml_Script_deleteRunFiles";
/// `updateScore(exp, points, popup, _)`: adds points to the score and XP to
/// `oController.xp`.
pub const UPDATE_SCORE: &str = "gml_Script_updateScore";
/// `enemyInit(hull, ...)`: sets `hull = hullMax = argument0`, `hullPrevious = hull`.
pub const ENEMY_INIT: &str = "gml_Script_enemyInit";
/// Damage an enemy deals to the player or a construct; the mod scales it.
pub const ENEMY_DAMAGE_SCALING: &str = "gml_Script_enemyDamageScaling";
/// Defers `pressPauseDeferred` (run in `oController`'s `Draw_75`). Call as `oController`.
pub const PRESS_PAUSE: &str = "gml_Script_pressPause";
/// Defers `pressUnpauseDeferred` the same way.
pub const PRESS_UNPAUSE: &str = "gml_Script_pressUnpause";
/// The pause menu's quit (Leave Co-op Run); needs `pause == 1`.
pub const PRESS_QUIT: &str = "gml_Script_pressQuit";

/// `oController`: 1 while paused.
pub const VAR_PAUSE: &str = "pause";
/// `oController`: live input (false while paused, upgrading, at the end screen).
pub const VAR_GAMEPLAY: &str = "gameplay";
/// `oController`: set by `beginEndScoreTransition`.
pub const VAR_GAME_OVER: &str = "gameOver";
/// `oController`: `current_time` at its Create; a new value means a new run.
pub const VAR_TIME_GAME_START: &str = "timeGameStart";
/// `oPowers` (player) and every enemy: current hull.
pub const VAR_HULL: &str = "hull";
/// `oPowers` (player) and every enemy: maximum hull.
pub const VAR_HULL_MAX: &str = "hullMax";
/// Enemies: last frame's hull; any hull write must set it too.
pub const VAR_HULL_PREVIOUS: &str = "hullPrevious";
/// `oPowers`: shield.
pub const VAR_SHIELD: &str = "shield";
/// `oPowers`: cloaked (enemies don't target the ship).
pub const VAR_CLOAKED: &str = "cloaked";

/// The wrap-around play area.
pub const GLOBAL_VIEW: [&str; 4] = ["viewX", "viewY", "viewWidth", "viewHeight"];

/// Gates every leaderboard upload in `oDrawEndScore_Create_0`; set to 0
/// around that event in a co-op run, then restored.
pub const GLOBAL_ENABLE_LEADERBOARDS: &str = "enableLeaderboards";

/// The game hides its own HUD (the ending cutscene).
pub const GLOBAL_HIDE_GUI: &str = "hideGUI";
/// The ending has begun.
pub const GLOBAL_GAME_ENDING: &str = "gameEnding";

/// Game-mode globals hashed into `RunStart::mode`; both peers must match.
pub const GLOBAL_MODE: [&str; 15] = [
    "gameModAnnihilation",
    "gameModDailyChallenge",
    "gameModDangerZone",
    "gameModDraft",
    "gameModEliteEnemy",
    "gameModEndless",
    "gameModEnemyTerritory",
    "gameModHostileUniverse",
    "gameModMayhem",
    "gameModNemesis",
    "gameModPractice",
    "gameModWildMetamorphosis",
    "baseEnemyHullFactor",
    "baseEnemyDamageFactor",
    "baseEnemySpeedFactor",
];

/// The run settings (`loadout.rs`): the loadout screen's start, the daily
/// reset, the settings writes, the screen's step, back and GUI draw.
pub const CONFIRM_LOADOUT: &str = "gml_Script_confirmLoadout";
pub const RESET_DAILY: &str = "gml_Script_resetDailyChallenges";
/// Settings' reset progress: writes default mode picks.
pub const RESET_PROGRESS: &str = "gml_Script_resetProgress";
/// A resumed run's globals put back from its file (`oLoadout_Alarm_2`).
pub const RESUME_RUN: &str = "gml_Script_resumeRun";
pub const INI_WRITE_BOOL: &str = "gml_Script_ini_write_bool";
pub const LOADOUT_STEP: &str = "gml_Object_oLoadout_Step_0";
pub const LEAVE_LOADOUT: &str = "gml_Script_leaveLoadout";
pub const LOADOUT_DRAW_GUI: &str = "gml_Object_oLoadout_Draw_64";
/// `oLoadout`'s own: the selected row (0 = Start), the fade a start sets to
/// 1, the daily's three picks and its name.
pub const VAR_LOADOUT_POS: &str = "pos";
pub const VAR_LOADOUT_FADE: &str = "fade";
pub const VAR_DAILY_CHOICE: &str = "dailyChoice";
pub const VAR_DAILY_MOD: &str = "dailyModChoice";
pub const VAR_DAILY_MOD2: &str = "dailyModChoice2";
pub const VAR_DAILY_STRING: &str = "dailyString";
/// The loadout screen's Start label (read only by `oLoadout_Draw_64`).
pub const GLOBAL_LOADOUT_START: &str = "strLoadoutStart";
/// A resumed run (the loadout's Start then resumes instead).
pub const GLOBAL_CHOSE_RESUME_RUN: &str = "choseResumeRun";
/// Each of the first 12 [`GLOBAL_MODE`] flags' title: the game's localized
/// string (a global) and the English one.
pub const MODE_TITLES: [(&str, &str); 12] = [
    ("strUnlocksTitleAnnihilation", "Annihilation"),
    ("strUnlocksTitleDailyChallenge", "Daily Challenge"),
    ("strUnlocksTitleDanger", "Danger Zone"),
    ("strUnlocksTitleDraft", "Draft"),
    ("strUnlocksTitleEliteEnemy", "Elite Enemy"),
    ("strLoadoutEndless", "Endless"),
    ("strUnlocksTitleEnemyTerritory", "Enemy Territory"),
    ("strUnlocksTitleHostileUniverse", "Hostile Universe"),
    ("strUnlocksTitleMayhem", "Mayhem"),
    ("strUnlocksTitleNemesis", "Nemesis"),
    ("strLoadoutPractice", "Practice"),
    ("strUnlocksTitleWildMetamorphosis", "Wild Metamorphosis"),
];

/// `wild_mods` (`wild.rs`): the flag, the upgrade screen, the rerolls, and
/// the screen's wild slots and offers.
pub const GLOBAL_WILD: &str = "gameModWildMetamorphosis";
pub const UPGRADE_CREATE: &str = "gml_Object_oUpgrade_Create_0";
pub const SET_REROLLS: &str = "gml_Script_setRerolls";

/// Built-in variables read from `oPlayer` each step.
pub mod builtin_var {
    pub const X: &str = "x";
    pub const Y: &str = "y";
    pub const HSPEED: &str = "hspeed";
    pub const VSPEED: &str = "vspeed";
    pub const IMAGE_ANGLE: &str = "image_angle";
    pub const SPRITE_INDEX: &str = "sprite_index";
    pub const IMAGE_INDEX: &str = "image_index";
    pub const IMAGE_XSCALE: &str = "image_xscale";
    pub const IMAGE_YSCALE: &str = "image_yscale";
    pub const IMAGE_BLEND: &str = "image_blend";
    pub const IMAGE_ALPHA: &str = "image_alpha";
    pub const VISIBLE: &str = "visible";
    /// Steps per second (ship speeds are px per step).
    pub const ROOM_SPEED: &str = "room_speed";
    /// Text typed since it was last cleared (the page's text entry).
    pub const KEYBOARD_STRING: &str = "keyboard_string";
}

/// Builtin functions called by name.
pub mod builtin_fn {
    pub const DRAW_SPRITE_EXT: &str = "draw_sprite_ext";
    pub const DRAW_TEXT: &str = "draw_text";
    pub const DRAW_TEXT_TRANSFORMED: &str = "draw_text_transformed";
    pub const DRAW_RECTANGLE: &str = "draw_rectangle";
    pub const DRAW_TRIANGLE: &str = "draw_triangle";
    pub const DRAW_SET_COLOR: &str = "draw_set_color";
    pub const DRAW_GET_COLOR: &str = "draw_get_color";
    pub const DRAW_SET_ALPHA: &str = "draw_set_alpha";
    pub const DRAW_GET_ALPHA: &str = "draw_get_alpha";
    pub const DRAW_SET_HALIGN: &str = "draw_set_halign";
    pub const DRAW_GET_HALIGN: &str = "draw_get_halign";
    pub const DRAW_SET_VALIGN: &str = "draw_set_valign";
    pub const DRAW_GET_VALIGN: &str = "draw_get_valign";
    pub const STRING_HEIGHT: &str = "string_height";
    pub const STRING_WIDTH: &str = "string_width";
    pub const SPRITE_EXISTS: &str = "sprite_exists";
    pub const GUI_WIDTH: &str = "display_get_gui_width";
    pub const GUI_HEIGHT: &str = "display_get_gui_height";
    pub const CLIPBOARD_SET: &str = "clipboard_set_text";
    pub const CLIPBOARD_GET: &str = "clipboard_get_text";
    pub const CLIPBOARD_HAS: &str = "clipboard_has_text";
    /// The camera of view 0 and its rectangle (changes with the zoom).
    pub const VIEW_GET_CAMERA: &str = "view_get_camera";
    pub const CAMERA_X: &str = "camera_get_view_x";
    pub const CAMERA_Y: &str = "camera_get_view_y";
    pub const CAMERA_W: &str = "camera_get_view_width";
    pub const CAMERA_H: &str = "camera_get_view_height";
    /// Clears keyboard and mouse state for the rest of the step.
    pub const IO_CLEAR: &str = "io_clear";
}

/// Main menu create: the only writer of `minPos`/`maxPos`.
pub const MENU_CREATE: &str = "gml_Object_oMenu_Create_0";
/// Main menu End Step: all of the menu's input.
pub const MENU_END_STEP: &str = "gml_Object_oMenu_Step_2";
/// Main menu confirm: `switch (pos)`. Runs as `oMenu`.
pub const CONFIRM_MENU: &str = "gml_Script_confirmMenu";
/// Pause menu confirm (as `oController`, row in `pos`): row 0 resumes.
pub const CONFIRM_PAUSE: &str = "gml_Script_confirmPause";

/// Stores `pressPauseDeferred`: the pause key, focus loss, the Steam overlay.
pub const HOOK_PRESS_PAUSE: &str = "gml_Script_pressPause";
/// Stores `pressUnpauseDeferred`: from `confirmPause` and from Esc in
/// `pauseMenuStep`, so blocking here covers both ways out of a pause.
pub const HOOK_PRESS_UNPAUSE: &str = "gml_Script_pressUnpause";
/// Run when the upgrade screen opens, each chained screen included.
pub const GOTO_UPGRADE_DEFERRED: &str = "gml_Script_gotoUpgradeDeferred";
/// Run when the last screen of a chain closes; skipping it holds the world frozen.
pub const LEAVE_UPGRADE_DEFERRED: &str = "gml_Script_leaveUpgradeDeferred";
/// `self` = `oPlayer`, from `oPowers_Step_2`: the ship dies when its hull is
/// out and it is not invulnerable, unless `oPowers.lastStand` revives.
pub const CHECK_DESTROY_PLAYER: &str = "gml_Script_checkDestroyPlayer";
/// Opens the upgrade screen (as `oController`; needs `global.levelup >= 1`).
pub const GOTO_UPGRADE: &str = "gml_Script_gotoUpgrade";
/// Leaves the upgrade screen: the next pending screen, or `leaveUpgradeDeferred`.
pub const LEAVE_UPGRADE: &str = "gml_Script_leaveUpgrade";
/// Restores the loadout after a revive. Must run as `oPowers`.
pub const REFRESH_LOADOUT: &str = "gml_Script_refreshLoadout";

/// The Game End event that runs when the game closes during a run.
pub const GAME_END_IN_RUN: &str = "gml_Object_oSaveRunFileManager_Other_3";
/// Rebuilds the Leviathan chain.
pub const REFRESH_DEFENSES: &str = "gml_Script_refreshDefenses";

/// Variables the session rules read or write.
pub mod rules_var {
    /// `oController`: XP inside the current level.
    pub const XP: &str = "xp";
    /// `oController`: XP to the next level.
    pub const MAX_EXP: &str = "maxexp";
    /// Global: XP levels reached.
    pub const BASE_LEVEL: &str = "baseLevel";
    /// Global: pending upgrade screens.
    pub const LEVELUP: &str = "levelup";
    /// Global: the run's score.
    pub const PLAYER_SCORE: &str = "playerScore";
    /// Global: points not yet moved into the score.
    pub const SCORE_VERIFIER: &str = "scoreVerifier";
    /// Global: the score's step (13; 239 past 1,000,000).
    pub const SCORE_MODULUS: &str = "scoreVerifierModulus";
    /// Global: the run's boss kills (`bossDrops`; the end screen's account EXP).
    pub const BOSS_KILLS: &str = "bossKills";
    /// Global: the run's Star Eater kills (the end screen's account EXP).
    pub const STAR_EATER_KILLS: &str = "starEaterKills";
    /// Global, an account flag: the Star Eater was ever killed.
    pub const FINAL_BOSS_DEFEATED: &str = "finalBossDefeated";
    /// `oPowers`: damage and the death check are off while above 0.
    pub const INVULNERABLE_TIME: &str = "invulnerableTime";
    /// `oPlayer`: projectile hits are off while above 0.
    pub const IFRAMES: &str = "iFrames";
    /// `oPowers`: input (and the upgrade screen) on.
    pub const PLAYER_CONTROL: &str = "playerControl";
    /// `oPowers`: firing allowed.
    pub const CAN_FIRE: &str = "canFire";
    /// `oPowers`: shield maximum.
    pub const SHIELD_MAX: &str = "shieldMax";
    /// `oPowers`: the other death condition.
    pub const HULL_RESERVED: &str = "hullReserved";
    /// `oPowers`: Last Stand will revive the ship.
    pub const LAST_STAND: &str = "lastStand";
    /// Global: the death check runs at all (a debug switch, normally true).
    pub const ALLOW_PLAYER_DEATH: &str = "allowPlayerDeath";
}

/// Objects the session rules reach by name (`asset_get_index`, checked).
pub mod rules_obj {
    pub const POWERS: &str = "oPowers";
}

/// Builtins only the session rules call.
pub mod rules_fn {
    /// False while the game window is in the background.
    pub const WINDOW_HAS_FOCUS: &str = "window_has_focus";
    pub const ASSET_GET_INDEX: &str = "asset_get_index";
    pub const KEYBOARD_CHECK_PRESSED: &str = "keyboard_check_pressed";
}
