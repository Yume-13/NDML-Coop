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

//! Sounds in co-op: a sound belongs to whoever owns the thing that makes it.
//!
//! * The partner's own things are inert proxies here, so the owning game reports what its
//!   player-side code played (`own` list, [`Route::ReportOwn`]); the viewer plays it at
//!   `partner_sound_volume`.
//! * The host reports its world's sounds (`world` list, [`Route::ReportWorld`]); the joining
//!   game's copies run out of step and are held at gain 0 ([`Route::Mute`]), except a death it
//!   predicted, a hit by its own player-side thing, and while no host frame came for [`BLIP`]
//!   ([`Route::Open`]).
//! * Everything else (local missiles, menus, music, pickups, effects, a sound inside one
//!   player's view) is each game's own.
//!
//! [`Sender`] numbers one-shots per list and sends each for [`RIDE_FRAMES`] frames; live loops
//! go as sets. [`Receiver`] plays a one-shot once, when the mirror presents the frame that first
//! carried it, and keeps exactly the frame's loop sets playing until frames stop for [`SILENCE`].
//! Host music calls in code the joining game skips (`bossMusicEnd`, `fadeBGM`, track starts) go as
//! [`MusicMarks`], made once there with its own settings ([`MusicDue`]).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use ndml_net::mirror::{
    FrameSound, FrameSounds, LOOPS_PER_FRAME, MusicMarks, SHOTS_PER_FRAME, SoundList,
    WORLD_SHOTS_PER_FRAME,
};

pub mod glue;

/// Frames a one-shot rides (unreliable frames: a lost one loses nothing).
pub const RIDE_FRAMES: u8 = 4;
/// No frame from the partner for this long: its loops stop.
pub const SILENCE: Duration = Duration::from_millis(500);
/// A live loop set is sent again every this many frames.
pub const LOOP_REFRESH: u32 = 15;
/// A frame presented this many ms late plays none of its one-shots (its
/// loops still apply).
pub const LATE: u32 = 100;
/// No host frame for this long: the joining game's own world sounds play.
pub const BLIP: Duration = Duration::from_millis(250);
/// Partner own one-shots started a second at most (a flood guard).
pub const MAX_SHOTS_PER_S: u32 = 120;
/// Host world one-shots started a second at most.
pub const MAX_WORLD_SHOTS_PER_S: u32 = 60;
/// A [`FrameSound::STOP_FIRST`] one-shot of an asset started within this
/// long replaces that instance, so it is not counted against the caps.
pub const REPLACES_WITHIN: Duration = Duration::from_millis(250);
/// A sequence number this far behind the last played is a new count.
const SEQ_RESTART: u32 = 1 << 20;

/// Which list a sound rides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// The partner's own things (played at `partner_sound_volume`).
    Own,
    /// The host's world (played at full volume).
    World,
}

/// This game's part in the shared world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// No shared world (solo, the menu, the world off).
    Off,
    Host,
    Joining,
}

/// What is known about a sound this game just started.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // independent facts about one call
pub struct Facts {
    /// The caller is a pickup (each player's own).
    pub collectable: bool,
    /// The caller is player-side, or hit by a player-side `other`.
    pub player_side: bool,
    /// The caller is the host's world, or a wave event the joining game skips.
    pub world: bool,
    /// Played by `playOnScreen` / `playVariantsOnScreen` (this player's view).
    pub on_screen: bool,
    /// Joining: the caller is a copy dying by this game's own claim.
    pub dying_own_claim: bool,
    /// Host: the caller dies by the partner's claim.
    pub dying_partner_claim: bool,
    /// Joining: no host frame for [`BLIP`].
    pub blip: bool,
}

/// What this game does with a sound it just started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Plays as the game started it; nothing reported.
    Native,
    /// Plays; reported in the `own` list.
    ReportOwn,
    /// Plays; reported in the `world` list (the host).
    ReportWorld,
    /// Held at gain 0 (the joining game: the host reports it).
    Mute,
    /// Plays now (a blip), held at gain 0 once host frames come again.
    Open,
}

/// The rule for one sound.
#[must_use]
pub fn route(role: Role, f: &Facts) -> Route {
    if f.collectable {
        return Route::Native;
    }
    if f.player_side {
        return Route::ReportOwn;
    }
    if !f.world || f.on_screen {
        return Route::Native;
    }
    match role {
        Role::Off => Route::Native,
        Role::Host if f.dying_partner_claim => Route::Native,
        Role::Host => Route::ReportWorld,
        Role::Joining if f.dying_own_claim => Route::Native,
        Role::Joining if f.blip => Route::Open,
        Role::Joining => Route::Mute,
    }
}

/// One list's one-shots and loops on the sending side.
#[derive(Debug, Default)]
struct Track {
    seq: u32,
    /// (number, sound, frames left), oldest first.
    recent: VecDeque<(u32, FrameSound, u8)>,
    /// Live loops by handle.
    loops: BTreeMap<u64, FrameSound>,
    /// The loop set last sent, the frames it still rides, and the frames
    /// since it last went out.
    sent_loops: Vec<FrameSound>,
    loops_ride: u8,
    loops_age: u32,
}

/// The loop set this list sends: one per asset, the loudest
/// [`LOOPS_PER_FRAME`] (ties by asset), sorted by asset.
fn loop_set(loops: &BTreeMap<u64, FrameSound>) -> Vec<FrameSound> {
    let mut all: Vec<FrameSound> = loops
        .values()
        .map(|s| FrameSound {
            asset: s.index(),
            ..*s
        })
        .collect();
    all.sort_by_key(|s| (std::cmp::Reverse(s.gain), s.asset));
    let mut out: Vec<FrameSound> = Vec::new();
    for s in all {
        if out.len() < LOOPS_PER_FRAME && !out.iter().any(|l| l.asset == s.asset) {
            out.push(s);
        }
    }
    out.sort_by_key(|s| s.asset);
    out
}

/// Whether a loop set differs from the one sent by an asset or by more than
/// a step of gain or pitch.
fn loops_changed(sent: &[FrameSound], now: &[FrameSound]) -> bool {
    sent.len() != now.len()
        || sent.iter().zip(now).any(|(a, b)| {
            a.asset != b.asset || a.gain.abs_diff(b.gain) > 1 || a.pitch.abs_diff(b.pitch) > 1
        })
}

impl Track {
    fn shot(&mut self, sound: FrameSound, cap: usize) -> u32 {
        // A stop-first play of an asset already played this frame replaces it.
        if sound.stop_first()
            && let Some(r) = self
                .recent
                .iter_mut()
                .find(|r| r.2 == RIDE_FRAMES && r.1.index() == sound.index())
        {
            r.1 = sound;
            return r.0;
        }
        self.seq = self.seq.wrapping_add(1);
        self.recent.push_back((self.seq, sound, RIDE_FRAMES));
        while self.recent.len() > cap * 2 {
            self.recent.pop_front();
        }
        self.seq
    }

    /// This frame's list (`send`: else only aged): the newest `cap`
    /// one-shots, consecutive numbers ending at the newest.
    fn take(&mut self, cap: usize, send: bool) -> SoundList {
        let mut shots: Vec<(u32, FrameSound)> = Vec::new();
        for r in &mut self.recent {
            if r.2 > 0 {
                r.2 -= 1;
                shots.push((r.0, r.1));
            }
        }
        self.recent.retain(|r| r.2 > 0);
        if !send {
            // The count as it stands: a list numbered 0 reads as a restart.
            return SoundList {
                seq: self.seq,
                ..SoundList::default()
            };
        }
        let start = shots.len().saturating_sub(cap);
        let shots = &shots[start..];
        // The loop set goes on a change, and while not empty every LOOP_REFRESH frames.
        let now = loop_set(&self.loops);
        if loops_changed(&self.sent_loops, &now) {
            self.sent_loops = now;
            self.loops_ride = RIDE_FRAMES;
        }
        self.loops_age += 1;
        let refresh = !self.sent_loops.is_empty() && self.loops_age >= LOOP_REFRESH;
        let loops = (self.loops_ride > 0 || refresh).then(|| {
            self.loops_ride = self.loops_ride.saturating_sub(1);
            self.loops_age = 0;
            self.sent_loops.clone()
        });
        SoundList {
            seq: shots.last().map_or(self.seq, |s| s.0),
            shots: shots.iter().map(|s| s.1).collect(),
            loops,
        }
    }
}

/// Sending side: what this game reported, for its next frames.
#[derive(Debug, Default)]
pub struct Sender {
    own: Track,
    world: Track,
    /// One-shots started this frame by handle (gain and pitch follow the play
    /// call): list and number.
    shot_handles: HashMap<u64, (Kind, u32)>,
    /// Host: its marks for music calls the joining game skips.
    music: Option<MusicMarks>,
    music_n: u8,
    music_ride: u8,
    music_age: u32,
}

impl Sender {
    fn track(&mut self, kind: Kind) -> &mut Track {
        match kind {
            Kind::Own => &mut self.own,
            Kind::World => &mut self.world,
        }
    }

    /// A reported one-shot.
    pub fn shot(&mut self, kind: Kind, handle: u64, sound: FrameSound) {
        let cap = match kind {
            Kind::Own => SHOTS_PER_FRAME,
            Kind::World => WORLD_SHOTS_PER_FRAME,
        };
        let seq = self.track(kind).shot(sound, cap);
        // A replaced shot's handle no longer tunes it.
        self.shot_handles.retain(|_, v| *v != (kind, seq));
        self.shot_handles.insert(handle, (kind, seq));
    }

    /// A reported loop.
    pub fn loop_started(&mut self, kind: Kind, handle: u64, sound: FrameSound) {
        self.track(kind).loops.insert(handle, sound);
    }

    /// The handles of every reported loop still listed.
    #[must_use]
    pub fn live_loops(&self) -> Vec<u64> {
        self.own
            .loops
            .keys()
            .chain(self.world.loops.keys())
            .copied()
            .collect()
    }

    /// `audio_sound_gain` / `audio_sound_pitch` on a handle of this frame or a live loop.
    pub fn set(&mut self, handle: u64, gain: Option<u8>, pitch: Option<u8>) {
        let apply = |s: &mut FrameSound| {
            if let Some(g) = gain {
                s.gain = g;
            }
            if let Some(p) = pitch {
                s.pitch = p;
            }
        };
        if let Some(&(kind, seq)) = self.shot_handles.get(&handle)
            && let Some(r) = self.track(kind).recent.iter_mut().find(|r| r.0 == seq)
        {
            apply(&mut r.1);
        }
        for t in [&mut self.own, &mut self.world] {
            if let Some(l) = t.loops.get_mut(&handle) {
                apply(l);
            }
        }
    }

    /// A stop by handle (`Some(h)`), by asset, or of everything.
    pub fn stopped(&mut self, handle: Option<u64>, asset: Option<u16>) {
        for t in [&mut self.own, &mut self.world] {
            match (handle, asset) {
                (Some(h), _) => {
                    t.loops.remove(&h);
                }
                (None, Some(a)) => t.loops.retain(|_, s| s.index() != a),
                (None, None) => t.loops.clear(),
            }
        }
    }

    /// The sounds for this frame's first part (ages the one-shots). `deaf`:
    /// partner own sounds unwanted; `send_own`: the partner wants local ones;
    /// `world`: this game is the host.
    pub fn frame(&mut self, deaf: bool, send_own: bool, world: bool) -> Option<FrameSounds> {
        self.shot_handles.clear();
        let own = self.own.take(SHOTS_PER_FRAME, send_own);
        let host = world;
        let world = self.world.take(WORLD_SHOTS_PER_FRAME, world);
        // The marks go out from the host's first frame (the joining game's baseline).
        if host && self.music.is_none() {
            self.music = Some(MusicMarks::default());
            self.music_ride = RIDE_FRAMES;
        }
        self.music_age += 1;
        let music = self
            .music
            .filter(|_| host && (self.music_ride > 0 || self.music_age >= LOOP_REFRESH));
        if music.is_some() {
            self.music_ride = self.music_ride.saturating_sub(1);
            self.music_age = 0;
        }
        if own.is_empty() && world.is_empty() && music.is_none() && !deaf {
            return None;
        }
        Some(FrameSounds {
            deaf,
            own,
            world,
            music,
        })
    }

    /// A fresh sender for the next run that keeps the music count and marks,
    /// so the next run's calls are always ahead of what a viewer has seen.
    #[must_use]
    pub fn next_run(&self) -> Self {
        Self {
            music: self.music,
            music_n: self.music_n,
            music_ride: if self.music.is_some() { RIDE_FRAMES } else { 0 },
            ..Self::default()
        }
    }

    /// The next call's number on the one count of all kinds (its order).
    fn music_changed(&mut self, f: impl FnOnce(&mut MusicMarks, u8)) {
        self.music_n = self.music_n.wrapping_add(1);
        let n = self.music_n;
        let m = self.music.get_or_insert_with(MusicMarks::default);
        m.newest = n;
        f(m, n);
        self.music_ride = RIDE_FRAMES;
    }

    /// Host: `bossMusicEnd` ran (the boss's drops, or the wave code).
    pub fn boss_music_ended(&mut self) {
        self.music_changed(|m, n| m.boss_end = n);
    }

    /// Host: the skipped code faded the track (`gain` in 1/64).
    pub fn wave_faded(&mut self, gain: u8, ms: u16) {
        self.music_changed(|m, n| {
            m.fade = n;
            m.fade_gain = gain;
            m.fade_ms = ms;
        });
    }

    /// Host: the skipped code started `track` (`looped`: `playBGMLoop`,
    /// else `playBGM`), after `fade` if it made one.
    pub fn music_started(&mut self, track: u16, looped: bool, fade: Option<(u8, u16)>) {
        self.music_changed(|m, n| {
            m.start = n;
            m.start_track = track;
            m.start_looped = looped;
            m.start_fade = fade;
        });
    }
}

/// A music call the host's marks ask of a joining game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicAct {
    /// `bossMusicEnd()`.
    BossEnd,
    /// `fadeBGM(gain, ms)` (gain in 1/64).
    Fade(u8, u16),
    /// `fadeBGM` first when `fade`, then `bgmPlaying = track` and
    /// `playBGMLoop()` (`looped`) or `playBGM()`.
    Start {
        track: u16,
        looped: bool,
        fade: Option<(u8, u16)>,
    },
}

/// The calls due, in the host's order.
pub type MusicDue = Vec<MusicAct>;

/// Whether music count `n` comes after `last`.
fn count_ahead(n: u8, last: u8) -> bool {
    let d = n.wrapping_sub(last);
    d != 0 && d < 128
}

/// What the viewer does at a presented frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Present {
    /// One-shots to start now (capped), by list.
    pub own: Vec<FrameSound>,
    pub world: Vec<FrameSound>,
    /// The loop sets to keep playing (`None`: no frame presented, keep).
    pub own_loops: Option<Vec<FrameSound>>,
    pub world_loops: Option<Vec<FrameSound>>,
}

/// Shots started in the current second, against a cap.
#[derive(Debug, Default)]
struct Window(Option<(Instant, u32)>);

impl Window {
    fn take(&mut self, now: Instant, cap: u32) -> bool {
        match &mut self.0 {
            Some((t, n)) if now.duration_since(*t) < Duration::from_secs(1) => {
                if *n >= cap {
                    return false;
                }
                *n += 1;
                true
            }
            s => {
                *s = Some((now, 1));
                true
            }
        }
    }
}

/// One list's frames on the viewer's side, presented at its own time.
#[derive(Debug, Default)]
struct Lane {
    /// The last one-shot number played.
    seq: Option<u32>,
    /// Frames not yet presented: (sender ms, the list).
    queue: VecDeque<(u32, SoundList)>,
    /// The newest sender time taken; a frame at or before it is dropped.
    newest: Option<u32>,
    /// The next presented frame follows a gap: its count is the baseline.
    resync: bool,
    window: Window,
    /// When each asset was last started from this list.
    started: HashMap<u16, Instant>,
}

/// A sender time this far behind the newest is a restarted sender clock.
const CLOCK_RESTART_MS: u32 = 10_000;

impl Lane {
    fn push(&mut self, time_ms: u32, list: SoundList) {
        if let Some(n) = self.newest
            && time_ms <= n
            && n - time_ms < CLOCK_RESTART_MS
        {
            return;
        }
        self.newest = Some(time_ms);
        self.queue.push_back((time_ms, list));
        while self.queue.len() > 120 {
            self.queue.pop_front();
        }
    }

    /// Every queued frame up to sender time `t`: the one-shots to start and
    /// the loop set (`None`: no frame presented carried one).
    fn present(
        &mut self,
        t: u32,
        now: Instant,
        cap: u32,
    ) -> (Vec<FrameSound>, Option<Vec<FrameSound>>) {
        let (mut shots, mut loops) = (Vec::new(), None);
        while self.queue.front().is_some_and(|f| f.0 <= t) {
            let Some((at, list)) = self.queue.pop_front() else {
                break;
            };
            if let Some(l) = list.loops {
                loops = Some(l);
            }
            let late = t - at > LATE;
            let n = list.shots.len() as u32;
            // The number before this list's first one-shot.
            let before = list.seq.wrapping_sub(n);
            if std::mem::take(&mut self.resync) {
                self.seq = Some(list.seq);
                continue;
            }
            if let Some(l) = self.seq {
                let behind = l.wrapping_sub(list.seq);
                if behind != 0 && behind < SEQ_RESTART {
                    // Lists come in order: a count behind is one that started over.
                    self.seq = Some(before);
                } else {
                    // Numbers that never came: the last played moves up to them.
                    let skipped = before.wrapping_sub(l);
                    if skipped < SEQ_RESTART {
                        self.seq = Some(before);
                    }
                }
            }
            for (i, shot) in list.shots.iter().enumerate() {
                let seq = before.wrapping_add(i as u32 + 1);
                if self.seq.is_some_and(|l| !is_new(seq, l)) {
                    continue;
                }
                self.seq = Some(seq);
                if late {
                    continue;
                }
                let replaces = shot.stop_first()
                    && self
                        .started
                        .get(&shot.index())
                        .is_some_and(|t| now.duration_since(*t) < REPLACES_WITHIN);
                if replaces || self.window.take(now, cap) {
                    self.started.insert(shot.index(), now);
                    shots.push(*shot);
                }
            }
        }
        (shots, loops)
    }

    /// Forget the queue (a freeze or silence); nothing in it plays later.
    fn drop_queue(&mut self) {
        for (_, list) in std::mem::take(&mut self.queue) {
            let n = list.shots.len() as u32;
            if n == 0 {
                continue;
            }
            let first = list.seq.wrapping_sub(n - 1);
            let mut new = 0;
            for (i, _) in list.shots.iter().enumerate() {
                if self
                    .seq
                    .is_none_or(|l| is_new(first.wrapping_add(i as u32), l))
                {
                    new += 1;
                }
            }
            if new > 0 {
                self.seq = Some(list.seq);
            }
        }
    }
}

/// Viewer side, per partner.
#[derive(Debug, Default)]
pub struct Receiver {
    own: Lane,
    world: Lane,
    /// The partner wants no own sounds from here.
    pub deaf: bool,
    last_frame: Option<Instant>,
    /// The host's music marks seen (the first is the baseline; kept through
    /// silence).
    music_seen: Option<MusicMarks>,
    music_due: MusicDue,
}

impl Receiver {
    /// A first part arrived (`sounds` as it carried them).
    pub fn on_frame(&mut self, time_ms: u32, sounds: Option<FrameSounds>, now: Instant) {
        let gap = self.last_frame.map(|t| now.duration_since(t));
        if gap.is_none_or(|g| g > BLIP) {
            self.own.resync = true;
            self.world.resync = true;
        }
        if gap.is_none_or(|g| g > SILENCE) {
            // A restarted sender clock is taken after silence.
            self.own.newest = None;
            self.world.newest = None;
        }
        self.last_frame = Some(now);
        // A frame without sounds carries no list (it would read as numbered 0).
        let Some(s) = sounds else {
            self.deaf = false;
            return;
        };
        self.deaf = s.deaf;
        if let Some(m) = s.music {
            self.music(m);
        }
        self.own.push(time_ms, s.own);
        self.world.push(time_ms, s.world);
    }

    fn music(&mut self, m: MusicMarks) {
        let Some(seen) = self.music_seen.as_mut() else {
            self.music_seen = Some(m);
            return;
        };
        // Each kind's newest call not seen yet, in the host's order.
        let mut new: Vec<(u8, MusicAct)> = Vec::new();
        if count_ahead(m.boss_end, seen.boss_end) {
            seen.boss_end = m.boss_end;
            new.push((m.boss_end, MusicAct::BossEnd));
        }
        if count_ahead(m.fade, seen.fade) {
            seen.fade = m.fade;
            new.push((m.fade, MusicAct::Fade(m.fade_gain, m.fade_ms)));
        }
        if count_ahead(m.start, seen.start) {
            seen.start = m.start;
            new.push((
                m.start,
                MusicAct::Start {
                    track: m.start_track,
                    looped: m.start_looped,
                    fade: m.start_fade,
                },
            ));
        }
        new.sort_by_key(|(n, _)| std::cmp::Reverse(m.newest.wrapping_sub(*n)));
        self.music_due.extend(new.into_iter().map(|x| x.1));
    }

    /// The music calls due (each once), cleared.
    pub fn take_music(&mut self) -> MusicDue {
        std::mem::take(&mut self.music_due)
    }

    /// Whether no frame has come for `quiet`.
    #[must_use]
    pub fn quiet(&self, now: Instant, quiet: Duration) -> bool {
        self.last_frame
            .is_none_or(|t| now.duration_since(t) > quiet)
    }

    /// Whether frames have stopped coming ([`SILENCE`]).
    #[must_use]
    pub fn silent(&self, now: Instant) -> bool {
        self.quiet(now, SILENCE)
    }

    /// Present the own list up to `t_own` (the mirror's time) and the world
    /// list up to `t_world` (the time the world shows the host's enemies at).
    pub fn present(&mut self, t_own: u32, t_world: u32, now: Instant) -> Present {
        let (own, own_loops) = self.own.present(t_own, now, MAX_SHOTS_PER_S);
        let (world, world_loops) = self.world.present(t_world, now, MAX_WORLD_SHOTS_PER_S);
        Present {
            own,
            world,
            own_loops,
            world_loops,
        }
    }

    /// Forget everything queued.
    pub fn drop_queue(&mut self) {
        self.own.drop_queue();
        self.world.drop_queue();
    }
}

/// Whether one-shot number `seq` comes after `last` (or the count restarted).
#[must_use]
pub fn is_new(seq: u32, last: u32) -> bool {
    let ahead = seq.wrapping_sub(last);
    let behind = last.wrapping_sub(seq);
    ahead != 0 && (ahead < SEQ_RESTART || behind >= SEQ_RESTART)
}

/// A loop set change: assets to stop, sounds to start or retune.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LoopChange {
    pub stop: Vec<u16>,
    pub start: Vec<FrameSound>,
    pub retune: Vec<FrameSound>,
}

/// From the loops playing (by asset, as last tuned) to `want`.
#[must_use]
pub fn loop_change(playing: &BTreeMap<u16, FrameSound>, want: &[FrameSound]) -> LoopChange {
    let mut c = LoopChange::default();
    for a in playing.keys() {
        if !want.iter().any(|w| w.index() == *a) {
            c.stop.push(*a);
        }
    }
    for w in want {
        match playing.get(&w.index()) {
            None => c.start.push(*w),
            Some(p) if p.gain.abs_diff(w.gain) > 1 || p.pitch.abs_diff(w.pitch) > 1 => {
                c.retune.push(*w);
            }
            Some(_) => {}
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::mirror::GAIN_ONE;

    fn s(asset: u16) -> FrameSound {
        FrameSound {
            asset,
            gain: GAIN_ONE,
            pitch: GAIN_ONE,
        }
    }

    /// A frame whose world list carries no one-shots, at count `seq`.
    fn empty(seq: u32) -> FrameSounds {
        FrameSounds {
            world: SoundList {
                seq,
                ..SoundList::default()
            },
            ..FrameSounds::default()
        }
    }

    /// Feed and present one frame per call at 33 ms steps.
    fn step(tx: &mut Sender, rx: &mut Receiver, f: u32, now: Instant) -> Present {
        rx.on_frame(f * 33, tx.frame(false, true, true), now);
        rx.present(f * 33, f * 33, now)
    }

    #[test]
    fn a_quiet_sender_sends_nothing_and_a_deaf_one_says_so() {
        let mut tx = Sender::default();
        // The host's first frames carry its music marks (the baseline).
        for _ in 0..RIDE_FRAMES {
            assert!(
                tx.frame(false, true, true)
                    .is_some_and(|f| f.music.is_some())
            );
        }
        assert_eq!(tx.frame(false, true, true), None);
        let d = tx.frame(true, true, true).unwrap();
        assert!(d.deaf && d.own.is_empty() && d.world.is_empty());
        // The partner deaf: no own sounds go, the host's world still does.
        tx.shot(Kind::Own, 1, s(5));
        assert_eq!(tx.frame(false, false, true), None);
        tx.shot(Kind::Own, 2, s(5));
        tx.shot(Kind::World, 3, s(6));
        let f = tx.frame(false, false, true).unwrap();
        assert!(f.own.is_empty());
        assert_eq!(f.world.shots, vec![s(6)]);
        // Not the host: no world list.
        let mut tx = Sender::default();
        tx.shot(Kind::World, 4, s(6));
        assert_eq!(tx.frame(false, true, false), None);
    }

    #[test]
    fn a_one_shot_rides_a_few_frames_and_plays_once() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let now = Instant::now();
        // The first frame after a gap plays no backlog: open the stream.
        step(&mut tx, &mut rx, 0, now);
        tx.shot(Kind::Own, 10, s(7));
        tx.set(10, Some(32), None);
        let mut played = Vec::new();
        for f in 1..7u32 {
            let p = step(&mut tx, &mut rx, f, now);
            played.extend(p.own);
        }
        assert_eq!(played, vec![FrameSound { gain: 32, ..s(7) }]);
    }

    #[test]
    fn the_lists_count_apart() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let now = Instant::now();
        step(&mut tx, &mut rx, 0, now);
        tx.shot(Kind::World, 1, s(1));
        tx.shot(Kind::World, 2, s(2));
        tx.shot(Kind::Own, 3, s(3));
        let p = step(&mut tx, &mut rx, 1, now);
        assert_eq!(p.world, vec![s(1), s(2)]);
        assert_eq!(p.own, vec![s(3)]);
        tx.shot(Kind::Own, 4, s(4));
        let p = step(&mut tx, &mut rx, 2, now);
        assert!(p.world.is_empty());
        assert_eq!(p.own, vec![s(4)]);
    }

    #[test]
    fn a_lost_frame_loses_nothing_and_a_duplicate_replays_nothing() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let now = Instant::now();
        step(&mut tx, &mut rx, 0, now);
        tx.shot(Kind::World, 1, s(1));
        let first = tx.frame(false, true, true);
        tx.shot(Kind::World, 2, s(2));
        let second = tx.frame(false, true, true);
        // The first frame is lost; the second carries both.
        rx.on_frame(66, second.clone(), now);
        assert_eq!(rx.present(66, 66, now).world, vec![s(1), s(2)]);
        // The first comes late (dropped as older) and the second again.
        rx.on_frame(33, first, now);
        rx.on_frame(99, second, now);
        assert!(rx.present(99, 99, now).world.is_empty());
    }

    #[test]
    fn no_burst_of_old_sounds_after_a_gap() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let t0 = Instant::now();
        step(&mut tx, &mut rx, 0, t0);
        // Frames stop coming; the host goes on playing.
        tx.shot(Kind::World, 1, s(1));
        tx.shot(Kind::World, 2, s(2));
        let _lost = tx.frame(false, true, true);
        tx.shot(Kind::World, 3, s(3));
        let back = t0 + BLIP + Duration::from_millis(10);
        rx.on_frame(500, tx.frame(false, true, true), back);
        assert!(rx.present(500, 500, back).world.is_empty());
        // What comes after the return plays.
        tx.shot(Kind::World, 4, s(4));
        rx.on_frame(533, tx.frame(false, true, true), back);
        assert_eq!(rx.present(533, 533, back).world, vec![s(4)]);
    }

    #[test]
    fn each_list_plays_at_its_own_time() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let now = Instant::now();
        step(&mut tx, &mut rx, 0, now);
        tx.shot(Kind::Own, 1, s(1));
        tx.shot(Kind::World, 2, s(2));
        rx.on_frame(100, tx.frame(false, true, true), now);
        // The mirror draws 100, the world shows 60: the world's sound waits.
        let p = rx.present(100, 60, now);
        assert_eq!((p.own, p.world), (vec![s(1)], Vec::new()));
        let p = rx.present(133, 100, now);
        assert_eq!((p.own, p.world), (Vec::new(), vec![s(2)]));
    }

    #[test]
    fn loops_follow_the_sets_and_stop_at_silence() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let now = Instant::now();
        tx.loop_started(Kind::Own, 100, s(444));
        tx.loop_started(Kind::World, 101, s(57));
        let p = step(&mut tx, &mut rx, 0, now);
        assert_eq!(p.own_loops, Some(vec![s(444)]));
        assert_eq!(p.world_loops, Some(vec![s(57)]));
        tx.stopped(None, Some(444));
        let p = step(&mut tx, &mut rx, 1, now);
        assert_eq!(p.own_loops, Some(Vec::new()));
        // The world set's change still rides.
        assert_eq!(p.world_loops, Some(vec![s(57)]));
        tx.stopped(Some(101), None);
        let p = step(&mut tx, &mut rx, 2, now);
        assert_eq!(p.world_loops, Some(Vec::new()));
        assert!(!rx.silent(now));
        assert!(rx.silent(now + SILENCE + Duration::from_millis(1)));
    }

    #[test]
    fn a_loop_set_goes_on_change_and_every_few_frames() {
        let mut tx = Sender::default();
        tx.loop_started(Kind::World, 1, s(57));
        let sets: Vec<bool> = (0..40)
            .map(|_| {
                tx.frame(false, true, true)
                    .is_some_and(|f| f.world.loops.is_some())
            })
            .collect();
        // The change rides RIDE_FRAMES frames, then a refresh every LOOP_REFRESH.
        let n = sets.iter().filter(|x| **x).count();
        assert!(sets[..usize::from(RIDE_FRAMES)].iter().all(|x| *x));
        assert_eq!(n, usize::from(RIDE_FRAMES) + 36 / LOOP_REFRESH as usize);
        // Nothing playing and nothing changed: nothing sent.
        tx.stopped(None, None);
        for _ in 0..usize::from(RIDE_FRAMES) {
            assert_eq!(
                tx.frame(false, true, true).unwrap().world.loops,
                Some(Vec::new())
            );
        }
        assert_eq!(tx.frame(false, true, true), None);
    }

    #[test]
    fn the_loudest_loops_go_in_a_stable_order() {
        let mut loops = BTreeMap::new();
        for (h, a, g) in [(9u64, 30u16, 10u8), (1, 20, 200), (5, 10, 64), (7, 20, 5)] {
            loops.insert(h, FrameSound { gain: g, ..s(a) });
        }
        for h in 100..110u64 {
            loops.insert(
                h,
                FrameSound {
                    gain: 1,
                    ..s(h as u16)
                },
            );
        }
        let set = loop_set(&loops);
        assert_eq!(set.len(), LOOPS_PER_FRAME);
        // Sorted by asset, the loudest instance of an asset, quiet ones out.
        assert!(set.windows(2).all(|w| w[0].asset < w[1].asset));
        assert!(set.contains(&FrameSound { gain: 200, ..s(20) }));
        assert!(set.contains(&FrameSound { gain: 64, ..s(10) }));
        assert!(set.contains(&FrameSound { gain: 10, ..s(30) }));
    }

    #[test]
    fn a_frame_duplicated_or_reordered_after_the_queue_drains_changes_nothing() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let now = Instant::now();
        tx.loop_started(Kind::World, 1, s(57));
        let old = tx.frame(false, true, true);
        rx.on_frame(0, old.clone(), now);
        assert_eq!(rx.present(0, 0, now).world_loops, Some(vec![s(57)]));
        tx.stopped(None, None);
        rx.on_frame(33, tx.frame(false, true, true), now);
        assert_eq!(rx.present(33, 33, now).world_loops, Some(Vec::new()));
        // The queue is empty; the old frame comes again, and late.
        rx.on_frame(0, old.clone(), now);
        rx.on_frame(33, old, now);
        let p = rx.present(66, 66, now);
        assert_eq!(p.world_loops, None);
        assert!(p.world.is_empty());
    }

    #[test]
    fn a_count_that_starts_over_small_is_taken() {
        let mut rx = Receiver::default();
        let t0 = Instant::now();
        let list = |seq, a| FrameSounds {
            world: SoundList {
                seq,
                shots: vec![s(a)],
                loops: None,
            },
            ..FrameSounds::default()
        };
        // The first list is the baseline (a frame without sounds is none).
        rx.on_frame(0, None, t0);
        rx.on_frame(5, Some(empty(4999)), t0);
        rx.present(5, 5, t0);
        rx.on_frame(10, Some(list(5000, 1)), t0);
        assert_eq!(rx.present(10, 10, t0).world, vec![s(1)]);
        // The sending game went back to its menu: a gap and a new count.
        let back = t0 + SILENCE + Duration::from_millis(1);
        rx.on_frame(5, Some(list(1, 2)), back);
        assert!(rx.present(5, 5, back).world.is_empty());
        rx.on_frame(38, Some(list(2, 3)), back);
        assert_eq!(rx.present(38, 38, back).world, vec![s(3)]);
    }

    #[test]
    fn the_hosts_music_calls_are_made_once_each_even_across_a_cut() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let t0 = Instant::now();
        // The host's first frame carries the marks before any call (the baseline).
        let first = tx.frame(false, true, true);
        assert_eq!(
            first.as_ref().and_then(|f| f.music),
            Some(MusicMarks::default())
        );
        rx.on_frame(0, first.clone(), t0);
        assert!(rx.take_music().is_empty());
        // The first call is made.
        tx.boss_music_ended();
        rx.on_frame(16, tx.frame(false, true, true), t0);
        assert_eq!(rx.take_music(), vec![MusicAct::BossEnd]);
        // In the host's order: a fade, then a boss end.
        tx.wave_faded(0, 3000);
        tx.boss_music_ended();
        let f = tx.frame(false, true, true);
        assert!(f.as_ref().is_some_and(|f| f.music.is_some()));
        rx.on_frame(33, f.clone(), t0);
        rx.on_frame(66, f, t0);
        assert_eq!(
            rx.take_music(),
            vec![MusicAct::Fade(0, 3000), MusicAct::BossEnd]
        );
        // An older frame (reordered) asks for nothing.
        rx.on_frame(20, first, t0);
        assert!(rx.take_music().is_empty());
        // A track started, then another with its fade: the newest start wins.
        tx.music_started(285, false, None);
        rx.on_frame(80, tx.frame(false, true, true), t0);
        assert_eq!(
            rx.take_music(),
            vec![MusicAct::Start {
                track: 285,
                looped: false,
                fade: None
            }]
        );
        tx.music_started(271, false, None);
        tx.wave_faded(0, 3000);
        tx.music_started(231, true, Some((0, 3000)));
        rx.on_frame(96, tx.frame(false, true, true), t0);
        assert_eq!(
            rx.take_music(),
            vec![
                MusicAct::Fade(0, 3000),
                MusicAct::Start {
                    track: 231,
                    looped: true,
                    fade: Some((0, 3000))
                }
            ]
        );
        // A cut: two boss ends while no frame came; the refresh asks for one.
        tx.boss_music_ended();
        tx.boss_music_ended();
        // Every frame of the ride and the refresh wait is lost.
        for _ in 0..u32::from(RIDE_FRAMES) + LOOP_REFRESH - 1 {
            let _ = tx.frame(false, true, true);
        }
        let back = t0 + Duration::from_secs(25);
        let refresh = tx.frame(false, true, true);
        assert!(refresh.as_ref().is_some_and(|f| f.music.is_some()));
        rx.on_frame(30_000, refresh, back);
        assert_eq!(rx.take_music(), vec![MusicAct::BossEnd]);
        // A joining game sends none.
        let mut join = Sender::default();
        join.boss_music_ended();
        assert!(
            join.frame(false, true, false)
                .is_none_or(|f| f.music.is_none())
        );
    }

    #[test]
    fn a_track_the_host_started_during_a_cut_is_played_at_the_return() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let t0 = Instant::now();
        rx.on_frame(0, tx.frame(false, true, true), t0);
        tx.music_started(285, false, None);
        rx.on_frame(16, tx.frame(false, true, true), t0);
        assert_eq!(rx.take_music().len(), 1);
        // The host plays on alone (a boss end, two starts); every frame is lost.
        tx.boss_music_ended();
        tx.music_started(271, false, None);
        tx.music_started(231, true, Some((0, 3000)));
        for _ in 0..u32::from(RIDE_FRAMES) + LOOP_REFRESH - 1 {
            let _ = tx.frame(false, true, true);
        }
        // The first refresh after the return: the boss end, then the newest start.
        let back = t0 + Duration::from_secs(20);
        rx.on_frame(20_000, tx.frame(false, true, true), back);
        assert_eq!(
            rx.take_music(),
            vec![
                MusicAct::BossEnd,
                MusicAct::Start {
                    track: 231,
                    looped: true,
                    fade: Some((0, 3000))
                }
            ]
        );
        for i in 1..=LOOP_REFRESH {
            rx.on_frame(20_000 + i * 16, tx.frame(false, true, true), back);
        }
        assert!(rx.take_music().is_empty());
    }

    #[test]
    fn a_second_run_makes_its_music_calls_whatever_the_viewer_kept() {
        let t0 = Instant::now();
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        rx.on_frame(0, tx.frame(false, true, true), t0);
        for _ in 0..9 {
            tx.boss_music_ended();
        }
        tx.music_started(231, true, Some((0, 3000)));
        let last = tx.frame(false, true, true);
        rx.on_frame(16, last.clone(), t0);
        assert_eq!(rx.take_music().len(), 2);
        // Run end: the host's sender for the next run keeps its count.
        let mut tx = tx.next_run();
        let first = tx.frame(false, true, true);
        assert!(
            first.as_ref().is_some_and(|f| f.music.is_some()),
            "the marks go out from the next run's first frame"
        );
        let at = t0 + Duration::from_secs(60);
        // (a) a viewer reset at run end: run 2's first frame is its baseline.
        let mut fresh = Receiver::default();
        fresh.on_frame(0, first.clone(), at);
        assert!(fresh.take_music().is_empty());
        // (b) the same with a late frame of run 1 taken as the baseline.
        let mut late = Receiver::default();
        late.on_frame(16, last, at);
        late.on_frame(0, first.clone(), at);
        assert!(late.take_music().is_empty());
        // (c) a viewer not reset between the runs.
        rx.on_frame(0, first, at);
        assert!(rx.take_music().is_empty());
        tx.boss_music_ended();
        let f = tx.frame(false, true, true);
        for r in [&mut fresh, &mut late, &mut rx] {
            r.on_frame(16, f.clone(), at + Duration::from_secs(1));
            assert_eq!(r.take_music(), vec![MusicAct::BossEnd]);
        }
    }

    #[test]
    fn a_stall_delivered_in_one_burst_plays_no_burst() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let now = Instant::now();
        step(&mut tx, &mut rx, 0, now);
        // 300 ms of frames held back, then delivered at once (no resync).
        let mut frames = Vec::new();
        for f in 1..=9u32 {
            tx.shot(Kind::World, u64::from(f), s(f as u16));
            frames.push((f * 33, tx.frame(false, true, true)));
        }
        for (t, fr) in frames {
            rx.on_frame(t, fr, now);
        }
        let p = rx.present(9 * 33, 9 * 33, now);
        // Only what is within LATE of the shown time plays.
        assert!(p.world.len() <= 4, "{:?}", p.world);
        assert!(p.world.contains(&s(9)));
    }

    #[test]
    fn the_caps_hold_each_list() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let now = Instant::now();
        step(&mut tx, &mut rx, 0, now);
        let (mut own, mut world) = (0, 0);
        for f in 1..61u32 {
            for k in 0..8u64 {
                tx.shot(Kind::Own, u64::from(f) * 100 + k, s(3));
                tx.shot(Kind::World, u64::from(f) * 100 + 50 + k, s(4));
            }
            let p = step(&mut tx, &mut rx, f, now);
            own += p.own.len();
            world += p.world.len();
        }
        assert_eq!(own as u32, MAX_SHOTS_PER_S);
        assert_eq!(world as u32, MAX_WORLD_SHOTS_PER_S);
    }

    #[test]
    fn a_stop_first_burst_replaces_one_voice_and_is_not_capped() {
        // Three stop-first variants, each played every frame: 180 a second.
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let t0 = Instant::now();
        step(&mut tx, &mut rx, 0, t0);
        let laser = |a: u16| FrameSound {
            asset: a | FrameSound::STOP_FIRST,
            ..s(a)
        };
        let mut played = 0;
        for f in 1..61u32 {
            for v in 0..3u16 {
                tx.shot(
                    Kind::World,
                    u64::from(f) * 10 + u64::from(v),
                    laser(173 + v),
                );
            }
            let now = t0 + Duration::from_millis(u64::from(f) * 16);
            played += step(&mut tx, &mut rx, f, now).world.len();
        }
        assert_eq!(played, 180);
        // Plain one-shots (each a new voice) still meet the cap.
        let after = t0 + Duration::from_secs(5);
        let mut plain = 0;
        for f in 61..121u32 {
            for k in 0..3u64 {
                tx.shot(Kind::World, u64::from(f) * 10 + k, s(4));
            }
            plain += step(&mut tx, &mut rx, f, after).world.len();
        }
        assert_eq!(plain as u32, MAX_WORLD_SHOTS_PER_S);
    }

    #[test]
    fn a_volley_in_one_frame_leaves_room_for_the_other_sounds() {
        // One plain sound, then 96 stop-first plays of three assets in one frame.
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let t0 = Instant::now();
        step(&mut tx, &mut rx, 0, t0);
        let laser = |a: u16, gain: u8| FrameSound {
            asset: a | FrameSound::STOP_FIRST,
            gain,
            ..s(a)
        };
        tx.shot(Kind::World, 1, s(9));
        for k in 0..96u16 {
            tx.shot(Kind::World, 100 + u64::from(k), laser(173 + k % 3, 10));
        }
        // The last play's handle tunes the asset's one entry.
        tx.set(193, Some(20), None);
        let p = step(&mut tx, &mut rx, 1, t0 + Duration::from_millis(16));
        let mut got: Vec<(u16, u8)> = p.world.iter().map(|x| (x.index(), x.gain)).collect();
        got.sort_unstable();
        assert_eq!(got, vec![(9, s(9).gain), (173, 20), (174, 10), (175, 10)]);
        // The next frame's plays are new shots again.
        tx.shot(Kind::World, 300, laser(173, 10));
        let p = step(&mut tx, &mut rx, 2, t0 + Duration::from_millis(33));
        assert_eq!(p.world.len(), 1);
    }

    #[test]
    fn a_new_count_after_a_restart_plays() {
        let mut rx = Receiver::default();
        let now = Instant::now();
        let list = |seq, a| FrameSounds {
            world: SoundList {
                seq,
                shots: vec![s(a)],
                loops: None,
            },
            ..FrameSounds::default()
        };
        rx.on_frame(5, Some(empty(4_999_999)), now);
        rx.present(5, 5, now);
        rx.on_frame(10, Some(list(5_000_000, 1)), now);
        assert_eq!(rx.present(10, 10, now).world.len(), 1);
        rx.on_frame(20, Some(list(1, 2)), now);
        assert_eq!(rx.present(20, 20, now).world, vec![s(2)]);
    }

    #[test]
    fn a_small_count_that_starts_over_without_a_gap_plays() {
        // A new run's count from 1 while this viewer kept the last run's (700).
        let mut rx = Receiver::default();
        let now = Instant::now();
        let list = |seq, a| FrameSounds {
            world: SoundList {
                seq,
                shots: vec![s(a)],
                loops: None,
            },
            ..FrameSounds::default()
        };
        rx.on_frame(5, Some(empty(699)), now);
        rx.present(5, 5, now);
        rx.on_frame(10, Some(list(700, 1)), now);
        assert_eq!(rx.present(10, 10, now).world, vec![s(1)]);
        rx.on_frame(20, Some(list(1, 2)), now);
        assert_eq!(rx.present(20, 20, now).world, vec![s(2)]);
        rx.on_frame(30, Some(list(2, 3)), now);
        assert_eq!(rx.present(30, 30, now).world, vec![s(3)]);
    }

    #[test]
    fn empty_lists_past_a_gap_in_the_count_move_the_count_up() {
        // Empty lists at a count 640 past the last played.
        let mut rx = Receiver::default();
        let now = Instant::now();
        rx.on_frame(5, Some(empty(10)), now);
        rx.present(5, 5, now);
        for f in 1..=30u32 {
            rx.on_frame(5 + f * 33, Some(empty(650)), now);
            rx.present(5 + f * 33, 5 + f * 33, now);
        }
        // The next one-shot plays.
        let one = FrameSounds {
            world: SoundList {
                seq: 651,
                shots: vec![s(4)],
                loops: None,
            },
            ..FrameSounds::default()
        };
        rx.on_frame(2_000, Some(one), now);
        assert_eq!(rx.present(2_000, 2_000, now).world, vec![s(4)]);
    }

    #[test]
    fn a_dropped_queue_is_not_played_after_the_freeze() {
        let mut tx = Sender::default();
        let mut rx = Receiver::default();
        let t0 = Instant::now();
        step(&mut tx, &mut rx, 0, t0);
        tx.shot(Kind::World, 1, s(4));
        tx.shot(Kind::Own, 2, s(5));
        rx.on_frame(33, tx.frame(false, true, true), t0);
        rx.drop_queue();
        // After the freeze the riding copies are not played.
        let p = step(&mut tx, &mut rx, 2, t0);
        assert!(p.world.is_empty() && p.own.is_empty());
    }

    #[test]
    fn stop_first_rides_with_the_sound_and_loops_list_plain_assets() {
        let mut tx = Sender::default();
        let flagged = FrameSound {
            asset: 9 | FrameSound::STOP_FIRST,
            ..s(9)
        };
        tx.shot(Kind::World, 1, flagged);
        tx.loop_started(Kind::World, 2, flagged);
        let f = tx.frame(false, true, true).unwrap();
        assert!(f.world.shots[0].stop_first());
        assert_eq!(f.world.loops, Some(vec![s(9)]));
    }

    #[test]
    fn loop_changes() {
        let mut playing = BTreeMap::new();
        playing.insert(1, s(1));
        playing.insert(2, s(2));
        let want = [s(2), FrameSound { gain: 10, ..s(3) }];
        let c = loop_change(&playing, &want);
        assert_eq!(c.stop, vec![1]);
        assert_eq!(c.start, vec![FrameSound { gain: 10, ..s(3) }]);
        assert!(c.retune.is_empty());
        let c = loop_change(&playing, &[FrameSound { gain: 10, ..s(2) }, s(1)]);
        assert_eq!(c.retune, vec![FrameSound { gain: 10, ..s(2) }]);
    }

    #[test]
    fn the_rule() {
        let f = |w: bool| Facts {
            world: w,
            ..Facts::default()
        };
        // Solo stays vanilla.
        assert_eq!(route(Role::Off, &f(true)), Route::Native);
        assert_eq!(
            route(
                Role::Off,
                &Facts {
                    player_side: true,
                    ..f(false)
                }
            ),
            Route::ReportOwn
        );
        // Pickups are each player's; not-world things each game's own.
        let pick = Facts {
            collectable: true,
            player_side: true,
            ..f(true)
        };
        assert_eq!(route(Role::Joining, &pick), Route::Native);
        assert_eq!(route(Role::Joining, &f(false)), Route::Native);
        // The world: the host reports, the joining game holds its own.
        assert_eq!(route(Role::Host, &f(true)), Route::ReportWorld);
        assert_eq!(route(Role::Joining, &f(true)), Route::Mute);
        // A hit by the joining player's own thing: its own, reported.
        let hit = Facts {
            player_side: true,
            ..f(true)
        };
        assert_eq!(route(Role::Joining, &hit), Route::ReportOwn);
        // A predicted death stays native there; the host reports none.
        let mine = Facts {
            dying_own_claim: true,
            ..f(true)
        };
        assert_eq!(route(Role::Joining, &mine), Route::Native);
        let theirs = Facts {
            dying_partner_claim: true,
            ..f(true)
        };
        assert_eq!(route(Role::Host, &theirs), Route::Native);
        // A blip lets the joining game's own through.
        let blip = Facts {
            blip: true,
            ..f(true)
        };
        assert_eq!(route(Role::Joining, &blip), Route::Open);
        // What each player's own view decides stays each game's.
        let seen = Facts {
            on_screen: true,
            ..f(true)
        };
        assert_eq!(route(Role::Host, &seen), Route::Native);
        assert_eq!(route(Role::Joining, &seen), Route::Native);
    }
}
