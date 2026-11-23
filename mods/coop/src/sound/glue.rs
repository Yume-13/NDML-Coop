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

//! Sounds in co-op: the hooks and game calls ([`super`] has the rule).
//!
//! Hooks the audio builtins (each play routed by [`super::route`]), instance
//! creation (what a player-side thing makes is player-side), deaths, the
//! on-screen play wrappers, the skipped wave events and the run and menu
//! ticks. The mirror calls `frame_sounds`, `on_frame` and `shown`; the
//! world answers who owns a caller through [`WorldHooks`] without waiting on
//! its locks.

#![forbid(unsafe_code)]

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ndml_net::PeerId;
use ndml_net::mirror::{FrameSound, FrameSounds, GAIN_ONE};
use ndml_sdk::{Builtin, Call, Error, Instance, ModContext, Script, Value, Variable};

use super::{
    BLIP, Facts, Kind, MusicAct, MusicDue, Receiver, Role, Route, Sender, loop_change, route,
};
use crate::world::glue::WorldHooks;

/// The priority partner own sounds play at (the game plays everything at 1).
pub const OWN_PRIORITY: f64 = 0.9375;
/// The priority the host's world sounds play at on a joining game.
pub const WORLD_PRIORITY: f64 = 1.0625;

/// A sound asset ref (`REF type 2 #i`).
const SOUND_REF: u64 = 0x0100_0002_0000_0000;
const REF_CLASS: u64 = 0xFFFF_FFFF_0000_0000;
/// How often the tracked instances and handles are pruned.
const PRUNE_EVERY: Duration = Duration::from_secs(10);
/// A partner loop that ended here starts again no sooner than this.
const RESTART_AFTER: Duration = Duration::from_millis(500);
/// This game's own instances kept per asset (for stops by asset).
const OWN_LIVE_MAX: usize = 32;
/// Partner one-shot instances kept per asset.
const PARTNER_MAX: usize = 4;
/// Joining: loops whose caller is gone are looked for this often.
const OWNERS_EVERY: Duration = Duration::from_millis(250);

thread_local! {
    /// Set while this module plays, stops or checks a sound itself.
    static OWN_CALL: Cell<bool> = const { Cell::new(false) };
    /// Depth of `createExplosion` and `instance_destroy` (a death, never a hit).
    static DEATH: Cell<u32> = const { Cell::new(0) };
    /// Depth of the wave events a joining game skips.
    static WAVE: Cell<u32> = const { Cell::new(0) };
    /// Depth of the game's on-screen play wrappers.
    static SCREEN: Cell<u32> = const { Cell::new(0) };
    /// Host: inside [`SKIPPED_MUSIC`].
    static WAVE_MUSIC: Cell<WaveMusic> = const { Cell::new((0, None)) };
}

/// [`WAVE_MUSIC`]: depth, the newest fade (gain, ms) no start followed.
type WaveMusic = (u32, Option<(f64, f64)>);

/// Code a joining game skips that makes music calls.
const SKIPPED_MUSIC: [&str; 3] = [
    "gml_Script_spawnEnemyWaves",
    "gml_Script_advanceBGM",
    "gml_Script_spawnBossRush",
];
const FADE_BGM: &str = "gml_Script_fadeBGM";
const PLAY_BGM: &str = "gml_Script_playBGM";
const PLAY_BGM_LOOP: &str = "gml_Script_playBGMLoop";
const BOSS_MUSIC_END: &str = "gml_Script_bossMusicEnd";

/// Set while a co-op run with a partner is on (armed at the run tick,
/// disarmed at the run's end or in the menu); disarmed, every hook returns.
static ARMED: AtomicBool = AtomicBool::new(false);

fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

fn enter(c: &'static std::thread::LocalKey<Cell<u32>>) {
    if armed() {
        c.with(|d| d.set(d.get() + 1));
    }
}

fn leave(c: &'static std::thread::LocalKey<Cell<u32>>) {
    c.with(|d| d.set(d.get().saturating_sub(1)));
}

fn inside(c: &'static std::thread::LocalKey<Cell<u32>>) -> bool {
    c.with(Cell::get) > 0
}

/// Run `f` with [`OWN_CALL`] set (this module's own sound calls).
fn own_call<T>(f: impl FnOnce() -> T) -> T {
    OWN_CALL.with(|o| o.set(true));
    let r = f();
    OWN_CALL.with(|o| o.set(false));
    r
}

#[derive(Clone, Copy)]
struct Api {
    play: Builtin,
    stop: Builtin,
    gain: Builtin,
    get_gain: Builtin,
    pitch: Builtin,
    playing: Builtin,
    exists: Builtin,
    name: Builtin,
    parent: Builtin,
    asset: Builtin,
    sound_name: Option<Builtin>,
    sfx: Option<Variable>,
    /// Joining: the music calls the host's marks ask for.
    boss_music_end: Option<Script>,
    fade_bgm: Option<Script>,
    play_bgm: Option<Script>,
    play_bgm_loop: Option<Script>,
    /// `global.bgmPlaying`: the track `playBGM`/`playBGMLoop` play.
    bgm: Option<Variable>,
}

/// One of this game's own sounds held at gain 0 on a joining game.
struct Held {
    handle: Value,
    /// The gain the game asked for (kept back while held).
    gain: Option<f64>,
    /// Let through (a blip).
    open: bool,
}

#[derive(Default)]
struct Counts {
    reported_own: u64,
    reported_world: u64,
    started_own: u64,
    started_world: u64,
    held: u64,
    /// Joining: own loops stopped because their caller was gone.
    orphans: u64,
    /// Host: music marks made (`bossMusicEnd`, wave fades).
    music_marks: u64,
    /// Joining: music calls made for the host's marks.
    music_calls: u64,
}

#[derive(Default)]
struct State {
    /// `partner_sound_volume`.
    volume: f64,
    tx: Sender,
    rx: HashMap<u8, Receiver>,
    shown: HashMap<u8, u32>,
    /// Partner loops playing here: (as tuned, handle, started).
    loops: BTreeMap<(Kind, u16), (FrameSound, Value, Instant)>,
    /// Partner one-shots playing here by asset, newest last.
    partner: HashMap<u16, Vec<(Kind, Value)>>,
    /// This game's own instances by asset (any caller), newest last.
    own_live: HashMap<u16, Vec<Value>>,
    /// Joining: this game's own world sounds held at gain 0, by handle.
    held: HashMap<u64, Held>,
    /// Joining: own loops by a non-player caller, by handle: (handle, the
    /// caller's id). A copy the world removes leaves its loop playing.
    owned: HashMap<u64, (Value, Value)>,
    owners_checked: Option<Instant>,
    /// Joining: no host frame for [`BLIP`] at the last run tick.
    blip: bool,
    /// The asset just stopped by asset (a stop-first play follows).
    last_stop: Option<u16>,
    /// Object index -> class by name.
    classes: HashMap<u32, Class>,
    /// Object index -> the host's world (the world's answers, cached).
    world_objects: HashMap<u32, bool>,
    /// Instances a player-side thing made: key -> (object, id).
    lineage: HashMap<usize, (u32, Value)>,
    pruned: Option<Instant>,
    /// `oWaves`, `oPlayerAttack` (resolved at the first run tick).
    objects: Option<(Value, Value)>,
    counts: Counts,
    /// Sound asset -> a music track (`m` and a capital: `mFreefall`).
    music: HashMap<u16, bool>,
    /// This run's host sends music marks (latched until [`State::next_run`]).
    host_marks: bool,
}

impl State {
    /// A partner's first part arrived.
    fn on_frame(&mut self, peer: u8, time_ms: u32, sounds: Option<FrameSounds>, now: Instant) {
        self.host_marks |= sounds.as_ref().is_some_and(|s| s.music.is_some());
        self.rx
            .entry(peer)
            .or_default()
            .on_frame(time_ms, sounds, now);
    }

    /// The state for the next run: keeps the volume, the caches and the music count.
    fn next_run(&mut self) -> Self {
        Self {
            volume: self.volume,
            classes: std::mem::take(&mut self.classes),
            world_objects: std::mem::take(&mut self.world_objects),
            objects: self.objects.take(),
            music: std::mem::take(&mut self.music),
            tx: self.tx.next_run(),
            ..Self::default()
        }
    }

    /// A partner instance of `asset` plays here (or may, until pruned).
    fn partner_has(&self, asset: u16) -> bool {
        self.loops.keys().any(|(_, a)| *a == asset)
            || self.partner.get(&asset).is_some_and(|v| !v.is_empty())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Player,
    Collectable,
    Other,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static API: Mutex<Option<Api>> = Mutex::new(None);
static WORLD: OnceLock<Arc<WorldHooks>> = OnceLock::new();

fn state() -> MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn api() -> Option<Api> {
    *API.lock().unwrap_or_else(PoisonError::into_inner)
}

fn role() -> Role {
    WORLD.get().map_or(Role::Off, |w| w.sound_role())
}

fn asset_of(v: &Value) -> Option<u16> {
    match v {
        Value::Ref(p) if p & REF_CLASS == SOUND_REF => u16::try_from(p & 0xFFFF_FFFF)
            .ok()
            .filter(|a| a & FrameSound::STOP_FIRST == 0),
        _ => None,
    }
}

fn handle_of(v: &Value) -> Option<u64> {
    match v {
        Value::Real(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
        Value::Int(n) if *n >= 0 => Some(*n as u64),
        _ => None,
    }
}

fn sound_ref(asset: u16) -> Value {
    Value::Ref(SOUND_REF | u64::from(asset))
}

fn truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true)) || v.as_f64().is_some_and(|n| n > 0.5)
}

fn object_of(inst: &Instance<'_>) -> Option<u32> {
    match inst.builtin_value("object_index").ok()? {
        Value::Ref(p) => u32::try_from(p & 0xFFFF_FFFF).ok(),
        other => other.as_f64().filter(|n| *n >= 0.0).map(|n| n as u32),
    }
}

/// This game's `sfxVolume` (1 when unknown).
fn sfx_volume(call: &Call<'_>, a: &Api) -> f64 {
    a.sfx
        .and_then(|v| call.global()?.get_f64(v))
        .filter(|v| v.is_finite())
        .unwrap_or(1.0)
}

/// Below this `sfxVolume` the owning game's gains say nothing; the asset's
/// own gain is reported instead.
const MIN_OWNER_SFX: f64 = 0.05;

/// A gain the owning game set, without its own `sfxVolume` (`None`: too low to tell).
fn owner_gain(raw: f64, sfx: f64) -> Option<u8> {
    (sfx >= MIN_OWNER_SFX).then(|| FrameSound::step(raw / sfx))
}
/// An object's class by its own and its ancestors' names (cached).
fn class_of(call: &Call<'_>, a: &Api, st: &mut State, object: u32) -> Class {
    if let Some(c) = st.classes.get(&object) {
        return *c;
    }
    let mut c = Class::Other;
    let mut cur = object;
    for _ in 0..16 {
        let name = call
            .call_builtin(a.name, &[Value::object(cur)])
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        if name.starts_with("oPlayer") || name == "oPowers" {
            c = Class::Player;
            break;
        }
        if name == "oCollectable" {
            c = Class::Collectable;
            break;
        }
        let Some(p) = call
            .call_builtin(a.parent, &[Value::object(cur)])
            .ok()
            .and_then(|p| crate::world::classify::object_index(&p))
            .filter(|p| *p != cur && *p <= 0xFFFF)
        else {
            break;
        };
        cur = p;
    }
    st.classes.insert(object, c);
    c
}

/// Whether a sound asset is a music track (`m` and a capital: `mFreefall`; cached).
fn music(call: &Call<'_>, a: &Api, st: &mut State, asset: u16) -> bool {
    if let Some(m) = st.music.get(&asset) {
        return *m;
    }
    let m = a
        .sound_name
        .and_then(|b| call.call_builtin(b, &[sound_ref(asset)]).ok())
        .and_then(|v| v.as_str().map(is_music_name))
        .unwrap_or(false);
    st.music.insert(asset, m);
    m
}

fn is_music_name(name: &str) -> bool {
    let mut c = name.chars();
    c.next() == Some('m') && c.next().is_some_and(|x| x.is_ascii_uppercase())
}

/// Whether object `o` is the host's world: the world's answer, the cached
/// one, else by its names.
fn world_object(call: &Call<'_>, a: &Api, st: &mut State, w: &WorldHooks, o: u32) -> bool {
    if let Some(b) = w.sound_world_object(call, o) {
        st.world_objects.insert(o, b);
        return b;
    }
    if let Some(b) = st.world_objects.get(&o) {
        return *b;
    }
    world_by_names(call, a, o)
}

/// The world's rule by names alone: an `oSolid` child not in
/// [`crate::world::classify::LOCAL`], or an enemy shot or charge-up child.
fn world_by_names(call: &Call<'_>, a: &Api, object: u32) -> bool {
    use crate::world::classify::{LOCAL, parent};
    let mut cur = object;
    for depth in 0..16 {
        let name = call
            .call_builtin(a.name, &[Value::object(cur)])
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        if depth == 0 && LOCAL.contains(&name.as_str()) {
            return false;
        }
        if name == parent::SOLID
            || parent::ENEMY_SHOTS.contains(&name.as_str())
            || parent::ENEMY_CHARGES.contains(&name.as_str())
        {
            return true;
        }
        let Some(p) = call
            .call_builtin(a.parent, &[Value::object(cur)])
            .ok()
            .and_then(|p| crate::world::classify::object_index(&p))
            .filter(|p| *p != cur && *p <= 0xFFFF)
        else {
            return false;
        };
        cur = p;
    }
    false
}

/// Whether an instance is player-side: its family, or made by one.
fn player_side(call: &Call<'_>, a: &Api, st: &mut State, inst: &Instance<'_>) -> bool {
    let Some(o) = object_of(inst) else {
        return false;
    };
    match class_of(call, a, st, o) {
        Class::Player => true,
        Class::Collectable => false,
        Class::Other => st.lineage.get(&inst.key()).is_some_and(|(lo, _)| *lo == o),
    }
}

/// What is known about the sound the caller just started.
fn facts(call: &Call<'_>, a: &Api, st: &mut State, role: Role, now: Instant) -> Facts {
    let mut f = Facts {
        on_screen: inside(&SCREEN),
        ..Facts::default()
    };
    let Some(me) = call.instance() else {
        return f;
    };
    let o = object_of(&me);
    // A skipped wave event's own sound comes from `oWaves` itself.
    let waves = st
        .objects
        .as_ref()
        .and_then(|(w, _)| crate::world::classify::object_index(w));
    f.world = inside(&WAVE) && o.is_some() && o == waves;
    f.collectable = o.is_some_and(|o| class_of(call, a, st, o) == Class::Collectable);
    if f.collectable {
        return f;
    }
    // A player-side caller, or (outside deaths) one hit by a player-side `other`.
    f.player_side = player_side(call, a, st, &me)
        || (!inside(&DEATH)
            && call
                .other()
                .filter(|x| x.key() != me.key())
                .is_some_and(|x| player_side(call, a, st, &x)));
    if f.player_side || role == Role::Off {
        return f;
    }
    let Some(w) = WORLD.get() else { return f };
    f.world = f.world || o.is_some_and(|o| world_object(call, a, st, w, o));
    if f.world {
        match role {
            Role::Host => f.dying_partner_claim = w.dying_by_partner_claim(me.key()),
            Role::Joining => {
                f.dying_own_claim = w.dying_by_own_claim(me.key());
                f.blip = st.rx.values().all(|r| r.quiet(now, BLIP));
            }
            Role::Off => {}
        }
    }
    f
}

/// Register the hooks (`volume`: this player's `partner_sound_volume`).
pub fn register(ctx: &ModContext, volume: f64, world: &Arc<WorldHooks>) {
    let resolve = || -> Result<Api, Error> {
        Ok(Api {
            play: ctx.builtin("audio_play_sound")?,
            stop: ctx.builtin("audio_stop_sound")?,
            gain: ctx.builtin("audio_sound_gain")?,
            get_gain: ctx.builtin("audio_sound_get_gain")?,
            pitch: ctx.builtin("audio_sound_pitch")?,
            playing: ctx.builtin("audio_is_playing")?,
            exists: ctx.builtin("instance_exists")?,
            name: ctx.builtin("object_get_name")?,
            parent: ctx.builtin("object_get_parent")?,
            asset: ctx.builtin("asset_get_index")?,
            sound_name: ctx.builtin("audio_get_name").ok(),
            sfx: ctx.variable("sfxVolume").ok(),
            boss_music_end: ctx.script(BOSS_MUSIC_END).ok(),
            fade_bgm: ctx.script(FADE_BGM).ok(),
            play_bgm: ctx.script(PLAY_BGM).ok(),
            play_bgm_loop: ctx.script(PLAY_BGM_LOOP).ok(),
            bgm: ctx.variable("bgmPlaying").ok(),
        })
    };
    let a = match resolve() {
        Ok(a) => a,
        Err(e) => {
            log::warn!("co-op sounds: unable to start ({e})");
            return;
        }
    };
    let _ = WORLD.set(Arc::clone(world));
    *API.lock().unwrap_or_else(PoisonError::into_inner) = Some(a);
    *state() = Some(State {
        volume,
        ..State::default()
    });
    let mut ok = true;
    ok &= hook(ctx, "audio_play_sound", |_| {}, after_play);
    ok &= hook(ctx, "audio_stop_sound", before_stop, |_| {});
    ok &= hook(ctx, "audio_stop_all", before_stop_all, |_| {});
    ok &= hook(ctx, "audio_sound_gain", |c| before_tune(c, true), |_| {});
    ok &= hook(ctx, "audio_sound_pitch", |c| before_tune(c, false), |_| {});
    ok &= hook(ctx, "audio_is_playing", |_| {}, after_is_playing);
    for b in [
        "instance_create_depth",
        "instance_create_layer",
        "instance_copy",
    ] {
        ok &= hook(ctx, b, |_| {}, after_create);
    }
    ok &= hook(
        ctx,
        "instance_destroy",
        |_| enter(&DEATH),
        |_| leave(&DEATH),
    );
    let depth = |name: &str, c: &'static std::thread::LocalKey<Cell<u32>>| {
        ctx.hook(name, move |_| enter(c), move |_, ()| leave(c))
            .map_err(|e| log::warn!("co-op sounds: {name}: {e}"))
            .is_ok()
    };
    ok &= depth(crate::world::names::CREATE_EXPLOSION, &DEATH);
    ok &= depth("gml_Script_playOnScreen", &SCREEN);
    ok &= depth("gml_Script_playVariantsOnScreen", &SCREEN);
    for name in crate::world::names::WAVE_ALARMS
        .iter()
        .chain([&crate::world::names::SPAWN_ENEMIES])
    {
        ok &= depth(name, &WAVE);
    }
    ok &= hook_music(ctx);
    ok &= ctx
        .hook_after(crate::names::CONTROLLER_STEP, run_tick)
        .is_ok();
    ok &= ctx.hook_before(crate::names::MENU_STEP, menu_tick).is_ok();
    log::info!(
        "co-op sounds: off until a co-op run with a partner (partner_sound_volume {volume}){}",
        if ok { "" } else { " (some hooks missing)" }
    );
}

/// A fade's arguments for a mark: gain in 1/64, ms.
fn fade_mark((gain, ms): (f64, f64)) -> (u8, u16) {
    (FrameSound::step(gain), ms.clamp(0.0, 65_535.0) as u16)
}

/// A track (`bgmPlaying`) as its sound index.
fn track_of(v: &Value) -> Option<u16> {
    asset_of(v).or_else(|| {
        v.as_f64()
            .filter(|n| (0.0..65_535.0).contains(n))
            .map(|n| n as u16)
    })
}

/// Host: marks every `bossMusicEnd`, and inside [`SKIPPED_MUSIC`] each track
/// start (with the fade before it) and each fade no start follows.
fn hook_music(ctx: &ModContext) -> bool {
    let warn = |name: &str, r: Result<(), Error>| {
        r.map_err(|e| log::warn!("co-op sounds: {name}: {e}"))
            .is_ok()
    };
    let mut ok = true;
    for name in SKIPPED_MUSIC {
        ok &= warn(
            name,
            ctx.hook(
                name,
                |_| {
                    if armed() {
                        WAVE_MUSIC.with(|w| {
                            let (d, f) = w.get();
                            w.set(if d == 0 { (1, None) } else { (d + 1, f) });
                        });
                    }
                },
                |_, ()| {
                    let (d, fade) = WAVE_MUSIC.with(Cell::get);
                    if d == 0 {
                        return;
                    }
                    let d = d - 1;
                    WAVE_MUSIC.with(|w| w.set((d, if d == 0 { None } else { fade })));
                    if d > 0 || role() != Role::Host {
                        return;
                    }
                    if let Some(f) = fade
                        && let Some(st) = state().as_mut()
                    {
                        let (gain, ms) = fade_mark(f);
                        st.tx.wave_faded(gain, ms);
                        st.counts.music_marks += 1;
                    }
                },
            ),
        );
    }
    ok &= warn(
        FADE_BGM,
        ctx.hook_before(FADE_BGM, |call| {
            let (d, _) = WAVE_MUSIC.with(Cell::get);
            if d == 0 {
                return;
            }
            let arg = |i| call.arg_value(i).and_then(|v| v.as_f64());
            if let (Some(g), Some(ms)) = (arg(0), arg(1)) {
                WAVE_MUSIC.with(|w| w.set((d, Some((g, ms)))));
            }
        }),
    );
    for (name, looped) in [(PLAY_BGM, false), (PLAY_BGM_LOOP, true)] {
        ok &= warn(
            name,
            ctx.hook_before(name, move |call| {
                let (d, fade) = WAVE_MUSIC.with(Cell::get);
                if d == 0 || OWN_CALL.with(Cell::get) || role() != Role::Host {
                    return;
                }
                WAVE_MUSIC.with(|w| w.set((d, None)));
                let track = api()
                    .and_then(|a| a.bgm)
                    .and_then(|v| call.global()?.get_value(v))
                    .as_ref()
                    .and_then(track_of);
                if let (Some(track), Some(st)) = (track, state().as_mut()) {
                    st.tx.music_started(track, looped, fade.map(fade_mark));
                    st.counts.music_marks += 1;
                }
            }),
        );
    }
    ok &= warn(
        BOSS_MUSIC_END,
        ctx.hook_before(BOSS_MUSIC_END, |_| {
            if !armed() || OWN_CALL.with(Cell::get) || role() != Role::Host {
                return;
            }
            if let Some(st) = state().as_mut() {
                st.tx.boss_music_ended();
                st.counts.music_marks += 1;
            }
        }),
    );
    ok
}

/// Joining: this game follows the host's music marks, so the world view's
/// own music switch stands aside.
pub(crate) fn follows_host_music() -> bool {
    armed() && role() == Role::Joining && state().as_ref().is_some_and(|st| st.host_marks)
}

/// Joining, outside a freeze: make the music calls the host's marks ask for,
/// once each in the host's order (a start sets `bgmPlaying` and calls
/// `playBGM` or `playBGMLoop` as the host did).
fn music_tick(call: &Call<'_>, a: &Api, role: Role, frozen: bool) {
    if role != Role::Joining || frozen {
        return;
    }
    let due: MusicDue = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        let due: MusicDue = st.rx.values_mut().flat_map(Receiver::take_music).collect();
        st.counts.music_calls += due.len() as u64;
        due
    };
    let fade = |gain: u8, ms: u16| {
        if let Some(s) = a.fade_bgm {
            let args = [
                Value::Real(FrameSound::value(gain)),
                Value::Real(f64::from(ms)),
            ];
            let _ = own_call(|| call.call_script(s, &args));
        }
    };
    for act in due {
        match act {
            MusicAct::BossEnd => {
                if let Some(s) = a.boss_music_end {
                    let r = own_call(|| call.call_script(s, &[]));
                    log::info!(
                        "co-op sounds: the host's boss music ended: bossMusicEnd here ({:?})",
                        r.map(|_| ())
                    );
                }
            }
            MusicAct::Fade(gain, ms) => {
                fade(gain, ms);
                log::info!(
                    "co-op sounds: the host faded its track: fadeBGM({}, {ms}) here",
                    FrameSound::value(gain)
                );
            }
            MusicAct::Start {
                track,
                looped,
                fade: before,
            } => {
                if let Some((gain, ms)) = before {
                    fade(gain, ms);
                }
                let (Some(var), Some(g)) = (a.bgm, call.global()) else {
                    continue;
                };
                // The ref's high bits as this game's own value has them.
                let high = match g.get_value(var) {
                    Some(Value::Ref(p)) => p & REF_CLASS,
                    _ => SOUND_REF,
                };
                let _ = g.set(var, Value::Ref(high | u64::from(track)));
                let play = if looped { a.play_bgm_loop } else { a.play_bgm };
                let r = play.map(|s| own_call(|| call.call_script(s, &[])));
                log::info!(
                    "co-op sounds: the host started track {track} ({}): played here ({:?})",
                    if looped { "playBGMLoop" } else { "playBGM" },
                    r.map(|r| r.map(|_| ()))
                );
            }
        }
    }
}

fn hook(
    ctx: &ModContext,
    name: &str,
    before: impl Fn(&mut Call<'_>) + Send + Sync + 'static,
    after: impl Fn(&mut Call<'_>) + Send + Sync + 'static,
) -> bool {
    match ctx.hook_builtin(name, before, move |c, ()| after(c)) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("co-op sounds: {name}: {e}");
            false
        }
    }
}

fn after_play(call: &mut Call<'_>) {
    if !armed() || OWN_CALL.with(Cell::get) {
        return;
    }
    let (Some(a), Some(asset)) = (api(), call.arg_value(0).as_ref().and_then(asset_of)) else {
        return;
    };
    let looped = call.arg_value(2).is_some_and(|v| truthy(&v));
    let Some(handle) = call.result() else { return };
    let Some(h) = handle_of(&handle) else { return };
    let role = role();
    let now = Instant::now();
    // The asset's own gain until the game sets the instance's.
    let base = call
        .call_builtin(a.get_gain, &[sound_ref(asset)])
        .ok()
        .and_then(|v| v.as_f64())
        .map_or(GAIN_ONE, FrameSound::step);
    let hold = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        let stop_first = st.last_stop.take() == Some(asset);
        let own = st.own_live.entry(asset).or_default();
        own.retain(|x| handle_of(x) != Some(h));
        own.push(handle.clone());
        if own.len() > OWN_LIVE_MAX {
            own.remove(0);
        }
        // A handle used again is a new sound.
        st.held.remove(&h);
        st.owned.remove(&h);
        // Music is each game's own.
        if music(call, &a, st, asset) {
            return;
        }
        if looped
            && role == Role::Joining
            && let Some(me) = call.instance()
            && !player_side(call, &a, st, &me)
            && let Ok(id) = me.builtin_value("id")
        {
            st.owned.insert(h, (handle.clone(), id));
        }
        let f = facts(call, &a, st, role, now);
        let sound = FrameSound {
            asset: if stop_first {
                asset | FrameSound::STOP_FIRST
            } else {
                asset
            },
            gain: base,
            pitch: GAIN_ONE,
        };
        let report = |st: &mut State, kind: Kind| {
            if looped {
                st.tx.loop_started(kind, h, sound);
            } else {
                st.tx.shot(kind, h, sound);
            }
        };
        match route(role, &f) {
            Route::Native => false,
            Route::ReportOwn => {
                st.counts.reported_own += 1;
                report(st, Kind::Own);
                false
            }
            Route::ReportWorld => {
                st.counts.reported_world += 1;
                report(st, Kind::World);
                false
            }
            Route::Mute | Route::Open => {
                let open = route(role, &f) == Route::Open;
                st.held.insert(
                    h,
                    Held {
                        handle: handle.clone(),
                        gain: None,
                        open,
                    },
                );
                st.counts.held += u64::from(!open);
                !open
            }
        }
    };
    if hold {
        own_call(|| {
            let _ = call.call_builtin(a.gain, &[handle, Value::Real(0.0), Value::Real(0.0)]);
        });
    }
}

fn before_stop(call: &mut Call<'_>) {
    if !armed() || OWN_CALL.with(Cell::get) {
        return;
    }
    let (Some(a), Some(v)) = (api(), call.arg_value(0)) else {
        return;
    };
    // A stop by asset while a partner instance plays here: own instances only.
    let own_only = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        if let Some(asset) = asset_of(&v) {
            st.last_stop = Some(asset);
            st.tx.stopped(None, Some(asset));
            let own = st.own_live.remove(&asset).unwrap_or_default();
            for x in &own {
                if let Some(h) = handle_of(x) {
                    st.held.remove(&h);
                    st.owned.remove(&h);
                }
            }
            st.partner_has(asset).then_some(own)
        } else if let Some(h) = handle_of(&v) {
            st.tx.stopped(Some(h), None);
            for hs in st.own_live.values_mut() {
                hs.retain(|x| handle_of(x) != Some(h));
            }
            st.held.remove(&h);
            st.owned.remove(&h);
            None
        } else {
            None
        }
    };
    if let Some(own) = own_only {
        call.skip_original();
        stop_handles(call, &a, own.into_iter());
    }
}

fn before_stop_all(_call: &mut Call<'_>) {
    if !armed() {
        return;
    }
    let mut g = state();
    let Some(st) = g.as_mut() else { return };
    st.tx.stopped(None, None);
    st.own_live.clear();
    st.loops.clear();
    st.partner.clear();
    st.held.clear();
    st.owned.clear();
    st.last_stop = None;
}

fn before_tune(call: &mut Call<'_>, gain: bool) {
    if !armed() || OWN_CALL.with(Cell::get) {
        return;
    }
    let (Some(a), Some(handle), Some(value)) = (
        api(),
        call.arg_value(0).as_ref().and_then(handle_of),
        call.arg_value(1).and_then(|v| v.as_f64()),
    ) else {
        return;
    };
    let step = if gain {
        owner_gain(value, sfx_volume(call, &a))
    } else {
        Some(FrameSound::step(value))
    };
    let held = {
        let mut guard = state();
        let Some(st) = guard.as_mut() else { return };
        if gain {
            st.tx.set(handle, step, None);
        } else {
            st.tx.set(handle, None, step);
        }
        match st.held.get_mut(&handle) {
            Some(held) if gain => {
                held.gain = Some(value);
                !held.open
            }
            _ => false,
        }
    };
    if held {
        call.skip_original();
    }
}

/// `audio_is_playing(asset)` while partner instances play: this game's own
/// only (so the game's `playLoop` still starts its loop).
fn after_is_playing(call: &mut Call<'_>) {
    if !armed() || OWN_CALL.with(Cell::get) {
        return;
    }
    let (Some(a), Some(asset)) = (api(), call.arg_value(0).as_ref().and_then(asset_of)) else {
        return;
    };
    if !call.result().is_some_and(|v| truthy(&v)) {
        return;
    }
    let own = {
        let g = state();
        let Some(st) = g.as_ref() else { return };
        if !st.partner_has(asset) {
            return;
        }
        st.own_live.get(&asset).cloned().unwrap_or_default()
    };
    let any = own_call(|| {
        own.iter().any(|h| {
            call.call_builtin(a.playing, std::slice::from_ref(h))
                .is_ok_and(|v| truthy(&v))
        })
    });
    let _ = call.set_return(Value::Bool(any));
}

fn after_create(call: &mut Call<'_>) {
    if !armed() {
        return;
    }
    let Some(a) = api() else { return };
    let Some(made) = call.result() else { return };
    let Some(me) = call.instance() else { return };
    let mut g = state();
    let Some(st) = g.as_mut() else { return };
    if !player_side(call, &a, st, &me) {
        return;
    }
    let mut found = None;
    let _ = call.with(&made, |inst, _| {
        if found.is_none() {
            found = object_of(&inst).map(|o| (inst.key(), o));
        }
    });
    if let Some((key, o)) = found {
        st.lineage.insert(key, (o, made));
    }
}

/// Sending side: this frame's sounds for the first part of the mirror frame.
#[allow(clippy::cast_precision_loss)] // sound handles count from 400001: far below 2^52
pub(crate) fn frame_sounds(call: &Call<'_>) -> Option<FrameSounds> {
    if !armed() {
        return None;
    }
    let a = api()?;
    let loops: Vec<u64> = state().as_ref()?.tx.live_loops();
    // Live loops: still playing, at their gain now.
    let sfx = sfx_volume(call, &a);
    let mut gone = Vec::new();
    let mut gains = Vec::new();
    own_call(|| {
        for h in &loops {
            let v = Value::Real(*h as f64);
            let playing = call
                .call_builtin(a.playing, std::slice::from_ref(&v))
                .is_ok_and(|x| truthy(&x));
            if !playing {
                gone.push(*h);
                continue;
            }
            if let Some(step) = call
                .call_builtin(a.get_gain, std::slice::from_ref(&v))
                .ok()
                .and_then(|x| x.as_f64())
                .and_then(|g| owner_gain(g, sfx))
            {
                gains.push((*h, step));
            }
        }
    });
    let host = role() == Role::Host;
    let mut g = state();
    let st = g.as_mut()?;
    for h in gone {
        st.tx.stopped(Some(h), None);
    }
    for (h, step) in gains {
        st.tx.set(h, Some(step), None);
    }
    let deaf = st.volume <= 0.0;
    let send_own = !st.rx.values().any(|r| r.deaf);
    st.tx.frame(deaf, send_own, host)
}

/// Viewer: a partner's mirror frame part arrived.
pub(crate) fn on_frame(peer: PeerId, time_ms: u32, part: u8, sounds: Option<FrameSounds>) {
    if part != 0 || !armed() {
        return;
    }
    if let Some(st) = state().as_mut() {
        st.on_frame(peer.0, time_ms, sounds, Instant::now());
    }
}

/// Viewer: the partner's sender time the mirror presents this frame.
pub(crate) fn shown(t: u32) {
    if !armed() {
        return;
    }
    let mut g = state();
    if let Some(st) = g.as_mut() {
        let peers: Vec<u8> = st.rx.keys().copied().collect();
        for p in peers {
            st.shown.insert(p, t);
        }
    }
}

fn menu_tick(call: &mut Call<'_>) {
    if ARMED.swap(false, Ordering::Relaxed) {
        disarm(call);
    }
}

/// The co-op run is over: partner sounds stop, the counts are logged, the
/// state resets.
fn disarm(call: &Call<'_>) {
    let Some(a) = api() else { return };
    let handles: Vec<Value> = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        let c = &st.counts;
        log::info!(
            "co-op sounds: disarmed (the co-op run is over): reported {} own and {} world, started {} partner own and {} host world, held {}, {} own loops stopped with their caller gone, music marks {} sent and {} calls made",
            c.reported_own,
            c.reported_world,
            c.started_own,
            c.started_world,
            c.held,
            c.orphans,
            c.music_marks,
            c.music_calls
        );
        let loops = std::mem::take(&mut st.loops).into_values().map(|l| l.1);
        let partner = std::mem::take(&mut st.partner)
            .into_values()
            .flatten()
            .map(|x| x.1);
        let out = loops.chain(partner).collect();
        *st = st.next_run();
        out
    };
    stop_handles(call, &a, handles.into_iter());
}

fn stop_handles(call: &Call<'_>, a: &Api, hs: impl Iterator<Item = Value>) {
    own_call(|| {
        for h in hs {
            let _ = call.call_builtin(a.stop, &[h]);
        }
    });
}

fn set_gain(call: &Call<'_>, a: &Api, h: &Value, g: f64) {
    own_call(|| {
        let _ = call.call_builtin(a.gain, &[h.clone(), Value::Real(g), Value::Real(0.0)]);
    });
}

/// Play one partner sound (`scale`: this game's volume for its list).
fn play_one(
    call: &Call<'_>,
    a: &Api,
    attack: &Value,
    kind: Kind,
    s: FrameSound,
    looped: bool,
    scale: f64,
) -> Option<Value> {
    let priority = match kind {
        Kind::Own => OWN_PRIORITY,
        Kind::World => WORLD_PRIORITY,
    };
    let run = |c: &dyn Fn(Builtin, &[Value]) -> Option<Value>| {
        let h = c(
            a.play,
            &[
                sound_ref(s.index()),
                Value::Real(priority),
                Value::Bool(looped),
            ],
        )?;
        let _ = c(
            a.gain,
            &[
                h.clone(),
                Value::Real(FrameSound::value(s.gain) * scale),
                Value::Real(0.0),
            ],
        );
        if s.pitch != GAIN_ONE {
            let _ = c(
                a.pitch,
                &[h.clone(), Value::Real(FrameSound::value(s.pitch))],
            );
        }
        Some(h)
    };
    own_call(|| {
        let mut out = None;
        // A partner own sound plays as its avatar if any, else as the controller.
        let mut done = false;
        if kind == Kind::Own {
            let _ = call.with(attack, |inst, _| {
                if !done {
                    done = true;
                    out = run(&|b, args| inst.call_builtin(b, args).ok());
                }
            });
        }
        if !done {
            out = run(&|b, args| call.call_builtin(b, args).ok());
        }
        out
    })
}

/// What the receivers present this tick.
#[derive(Default)]
struct Due {
    shots: Vec<(Kind, FrameSound)>,
    own_loops: Option<Vec<FrameSound>>,
    world_loops: Option<Vec<FrameSound>>,
}

/// The run tick: the blip, the partner's due sounds, its loop sets.
fn run_tick(call: &mut Call<'_>) {
    if role() == Role::Off {
        if ARMED.swap(false, Ordering::Relaxed) {
            disarm(call);
        }
        return;
    }
    if !ARMED.swap(true, Ordering::Relaxed) {
        log::info!("co-op sounds: armed (a co-op run with a partner)");
    }
    let Some(a) = api() else { return };
    let now = Instant::now();
    let (waves, attack) = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        if st.objects.is_none() {
            let get = |n: &str| {
                call.call_builtin(a.asset, &[Value::Str(n.to_owned())])
                    .unwrap_or(Value::Undefined)
            };
            st.objects = Some((get("oWaves"), get("oPlayerAttack")));
        }
        st.objects
            .clone()
            .unwrap_or((Value::Undefined, Value::Undefined))
    };
    let frozen = !call
        .call_builtin(a.exists, std::slice::from_ref(&waves))
        .is_ok_and(|v| truthy(&v));
    let role = role();
    let sfx = sfx_volume(call, &a);
    blip_tick(call, &a, role, now);
    stop_orphans(call, &a, frozen, now);
    music_tick(call, &a, role, frozen);
    // The host's world sounds play at the time the world shows its enemies.
    let world_t = if role == Role::Joining {
        WORLD.get().and_then(|w| w.sound_shown_ms())
    } else {
        None
    };
    let Some((volume, due)) = present(frozen, world_t, now) else {
        return;
    };
    for (kind, s) in due.shots {
        let scale = match kind {
            Kind::Own => sfx * volume,
            Kind::World => sfx,
        };
        if s.stop_first() {
            let last: Vec<Value> = state()
                .as_mut()
                .and_then(|st| {
                    let v = st.partner.get_mut(&s.index())?;
                    let (stop, keep): (Vec<_>, Vec<_>) = v.drain(..).partition(|x| x.0 == kind);
                    *v = keep;
                    Some(stop.into_iter().map(|x| x.1).collect())
                })
                .unwrap_or_default();
            stop_handles(call, &a, last.into_iter());
        }
        if let Some(h) = play_one(call, &a, &attack, kind, s, false, scale)
            && let Some(st) = state().as_mut()
        {
            let v = st.partner.entry(s.index()).or_default();
            v.push((kind, h));
            if v.len() > PARTNER_MAX {
                v.remove(0);
            }
            match kind {
                Kind::Own => st.counts.started_own += 1,
                Kind::World => st.counts.started_world += 1,
            }
        }
    }
    for (kind, want, scale) in [
        (Kind::Own, due.own_loops, sfx * volume),
        (Kind::World, due.world_loops, sfx),
    ] {
        if let Some(want) = want {
            follow_loops(call, &a, &attack, kind, &want, scale, now);
        }
    }
    restart_loops(call, &a, &attack, sfx, volume, now);
    prune(call, &a, now);
}

/// Joining: at a blip's start held world sounds play; at its end they are held again.
fn blip_tick(call: &Call<'_>, a: &Api, role: Role, now: Instant) {
    let change: Vec<(Value, f64)> = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        let blip = role == Role::Joining && st.rx.values().all(|r| r.quiet(now, BLIP));
        if blip == st.blip {
            return;
        }
        st.blip = blip;
        st.held
            .values_mut()
            .filter(|m| m.open != blip)
            .filter_map(|m| {
                m.open = blip;
                if blip {
                    m.gain.map(|g| (m.handle.clone(), g))
                } else {
                    Some((m.handle.clone(), 0.0))
                }
            })
            .collect()
    };
    for (h, g) in change {
        set_gain(call, a, &h, g);
    }
}

/// Present every receiver at its shown time (`None`: not registered).
fn present(frozen: bool, world_t: Option<u32>, now: Instant) -> Option<(f64, Due)> {
    let mut g = state();
    let st = g.as_mut()?;
    let volume = st.volume;
    let mut due = Due::default();
    if frozen {
        for r in st.rx.values_mut() {
            r.drop_queue();
        }
        due.own_loops = Some(Vec::new());
        due.world_loops = Some(Vec::new());
        return Some((volume, due));
    }
    let mut silent = true;
    let shown = st.shown.clone();
    for (peer, r) in &mut st.rx {
        if r.silent(now) {
            r.drop_queue();
            continue;
        }
        silent = false;
        let Some(t) = shown.get(peer) else { continue };
        let p = r.present(*t, world_t.unwrap_or(*t), now);
        if volume > 0.0 {
            due.shots.extend(p.own.into_iter().map(|s| (Kind::Own, s)));
        }
        due.shots
            .extend(p.world.into_iter().map(|s| (Kind::World, s)));
        if let Some(l) = p.own_loops {
            due.own_loops.get_or_insert_with(Vec::new).extend(l);
        }
        if let Some(l) = p.world_loops {
            due.world_loops.get_or_insert_with(Vec::new).extend(l);
        }
    }
    if silent {
        due.own_loops = Some(Vec::new());
        due.world_loops = Some(Vec::new());
    }
    if volume <= 0.0 {
        due.own_loops = Some(Vec::new());
    }
    Some((volume, due))
}

/// Keep exactly `want` of `kind`'s partner loops playing.
fn follow_loops(
    call: &Call<'_>,
    a: &Api,
    attack: &Value,
    kind: Kind,
    want: &[FrameSound],
    scale: f64,
    now: Instant,
) {
    let playing: BTreeMap<u16, FrameSound> = state().as_ref().map_or_default(|st| {
        st.loops
            .iter()
            .filter(|((k, _), _)| *k == kind)
            .map(|((_, asset), v)| (*asset, v.0))
            .collect()
    });
    let c = loop_change(&playing, want);
    for asset in &c.stop {
        let h = state()
            .as_mut()
            .and_then(|st| st.loops.remove(&(kind, *asset)));
        if let Some((_, h, _)) = h {
            stop_handles(call, a, std::iter::once(h));
        }
    }
    for s in c.start {
        if let Some(h) = play_one(call, a, attack, kind, s, true, scale)
            && let Some(st) = state().as_mut()
        {
            st.loops.insert((kind, s.index()), (s, h, now));
        }
    }
    for s in c.retune {
        let h = state().as_mut().and_then(|st| {
            st.loops.get_mut(&(kind, s.index())).map(|l| {
                l.0 = s;
                l.1.clone()
            })
        });
        if let Some(h) = h {
            set_gain(call, a, &h, FrameSound::value(s.gain) * scale);
            own_call(|| {
                let _ = call.call_builtin(a.pitch, &[h, Value::Real(FrameSound::value(s.pitch))]);
            });
        }
    }
}

/// A partner loop that ended here (not by this module): again, later.
fn restart_loops(call: &Call<'_>, a: &Api, attack: &Value, sfx: f64, volume: f64, now: Instant) {
    let check: Vec<((Kind, u16), FrameSound, Value, Instant)> =
        state().as_ref().map_or_default(|st| {
            st.loops
                .iter()
                .map(|(k, v)| (*k, v.0, v.1.clone(), v.2))
                .collect()
        });
    for (key, s, h, at) in check {
        let alive = own_call(|| {
            call.call_builtin(a.playing, std::slice::from_ref(&h))
                .is_ok_and(|v| truthy(&v))
        });
        if alive || now.duration_since(at) < RESTART_AFTER {
            continue;
        }
        let scale = match key.0 {
            Kind::Own => sfx * volume,
            Kind::World => sfx,
        };
        if let Some(h) = play_one(call, a, attack, key.0, s, true, scale)
            && let Some(st) = state().as_mut()
        {
            st.loops.insert(key, (s, h, now));
        }
    }
}

/// Joining, every [`OWNERS_EVERY`]: stop own loops whose caller is gone.
fn stop_orphans(call: &Call<'_>, a: &Api, frozen: bool, now: Instant) {
    let Some(w) = WORLD.get() else { return };
    let owned: Vec<(u64, Value)> = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        if frozen
            || st.owned.is_empty()
            || st
                .owners_checked
                .is_some_and(|t| now.duration_since(t) < OWNERS_EVERY)
        {
            return;
        }
        st.owners_checked = Some(now);
        st.owned.iter().map(|(h, (_, o))| (*h, o.clone())).collect()
    };
    // Destroyed, not deactivated.
    let gone: Vec<u64> = owned
        .into_iter()
        .filter(|(_, o)| w.sound_owner_gone(call, o))
        .map(|x| x.0)
        .collect();
    if gone.is_empty() {
        return;
    }
    let stop: Vec<Value> = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        st.counts.orphans += gone.len() as u64;
        gone.iter()
            .filter_map(|h| {
                st.held.remove(h);
                for hs in st.own_live.values_mut() {
                    hs.retain(|x| handle_of(x) != Some(*h));
                }
                st.owned.remove(h).map(|x| x.0)
            })
            .collect()
    };
    stop_handles(call, a, stop.into_iter());
}

/// Every [`PRUNE_EVERY`]: drop gone lineage and handles no longer playing.
fn prune(call: &Call<'_>, a: &Api, now: Instant) {
    let (lineage, own, held, partner) = {
        let mut g = state();
        let Some(st) = g.as_mut() else { return };
        if st
            .pruned
            .is_some_and(|t| now.duration_since(t) < PRUNE_EVERY)
        {
            return;
        }
        st.pruned = Some(now);
        let lineage: Vec<(usize, Value)> = st
            .lineage
            .iter()
            .map(|(k, (_, id))| (*k, id.clone()))
            .collect();
        let own: Vec<Value> = st.own_live.values().flatten().cloned().collect();
        let held: Vec<Value> = st
            .held
            .values()
            .map(|m| &m.handle)
            .chain(st.owned.values().map(|x| &x.0))
            .cloned()
            .collect();
        let partner: Vec<Value> = st.partner.values().flatten().map(|x| x.1.clone()).collect();
        (lineage, own, held, partner)
    };
    let gone_lineage: Vec<usize> = lineage
        .into_iter()
        .filter(|(_, id)| {
            !call
                .call_builtin(a.exists, std::slice::from_ref(id))
                .is_ok_and(|v| truthy(&v))
        })
        .map(|(k, _)| k)
        .collect();
    let playing = |v: &Value| {
        own_call(|| {
            call.call_builtin(a.playing, std::slice::from_ref(v))
                .is_ok_and(|x| truthy(&x))
        })
    };
    let ended: std::collections::HashSet<u64> = own
        .iter()
        .chain(held.iter())
        .chain(partner.iter())
        .filter(|v| !playing(v))
        .filter_map(handle_of)
        .collect();
    let mut g = state();
    let Some(st) = g.as_mut() else { return };
    for k in gone_lineage {
        st.lineage.remove(&k);
    }
    let live = |v: &Value| handle_of(v).is_none_or(|h| !ended.contains(&h));
    for hs in st.own_live.values_mut() {
        hs.retain(live);
    }
    st.own_live.retain(|_, hs| !hs.is_empty());
    st.held.retain(|h, _| !ended.contains(h));
    st.owned.retain(|h, _| !ended.contains(h));
    for v in st.partner.values_mut() {
        v.retain(|x| live(&x.1));
    }
    st.partner.retain(|_, v| !v.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosts_marks_latch_for_the_run_from_its_first_frame() {
        let now = Instant::now();
        let mut host = Sender::default();
        let mut st = State {
            volume: 0.5,
            ..State::default()
        };
        // A partner frame without marks (a joining game's) sets nothing.
        let mut join = Sender::default();
        join.boss_music_ended();
        st.on_frame(2, 0, join.frame(true, true, false), now);
        assert!(!st.host_marks);
        // The host's first frame carries them, before any music call.
        st.on_frame(1, 0, host.frame(false, true, true), now);
        assert!(st.host_marks);
        // Frames without marks and new receivers: still latched.
        for t in 1..=10 {
            st.on_frame(1, t * 16, None, now);
        }
        st.rx.clear();
        assert!(st.host_marks);
        // The run's end clears it and keeps the host's count and the volume.
        host.boss_music_ended();
        st.tx = host;
        let next = st.next_run();
        assert!(!next.host_marks);
        assert!(next.rx.is_empty());
        assert!((next.volume - 0.5).abs() < f64::EPSILON);
        let mut tx = next.tx;
        let m = tx.frame(false, true, true).and_then(|f| f.music);
        assert_eq!(m.map(|m| m.boss_end), Some(1));
    }

    #[test]
    fn music_tracks_by_name() {
        assert!(is_music_name("mFreefall"));
        assert!(is_music_name("mNovaDriftTitle"));
        assert!(!is_music_name("minePlaced2"));
        assert!(!is_music_name("m"));
        assert!(!is_music_name("destroyMedium"));
    }

    #[test]
    fn a_player_who_mutes_their_effects_does_not_mute_their_ship() {
        // The owning game's gain without its own volume.
        assert_eq!(owner_gain(0.25, 0.5), Some(FrameSound::step(0.5)));
        // Effects off or nearly: nothing to tell.
        assert_eq!(owner_gain(0.0, 0.0), None);
        assert_eq!(owner_gain(0.001, 0.01), None);
        // At the threshold a sound at full gain is 1.0, not 20.
        assert_eq!(owner_gain(MIN_OWNER_SFX, MIN_OWNER_SFX), Some(GAIN_ONE));
    }
}
