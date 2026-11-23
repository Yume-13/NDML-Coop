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

//! The generic player-side mirror: the owner (the game whose instances they
//! are) sends every player-side instance's sprite and pose; the viewer draws
//! each as an inert `oPlayerAttack` proxy.
//!
//! Player-side: objects whose own or an ancestor's name starts with `oPlayer`
//! (except `oPlayer` and `oPlayerAttack`), and by lineage anything a create
//! function makes from player-side code. Never `oGuiObject` descendants.
//!
//! This file is the pure part; the hooks are in [`glue`].

#![forbid(unsafe_code)]
// Poses are f64 in the game; the wire quantizes (see `ndml_net::mirror`).
#![allow(clippy::cast_precision_loss)]
// Short names follow the formulas (s = samples, t = time).
#![allow(clippy::many_single_char_names)]

pub mod glue;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use ndml_net::interp::{Segments, Vec2};
use ndml_net::mirror::{MirrorEntry, MirrorLeader, MirrorLook};

/// Viewer: display delay behind the newest frame before the clocks sync, ms.
pub const DELAY_MS: u32 = 100;

/// The mirror's settings (fixed in [`crate::config`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MirrorSettings {
    /// Owner: send what instances' own Draw code needs (`MirrorDraw`).
    pub draw: bool,
    /// Viewer: how long motion goes on past a track's newest sample, ms (the
    /// ship's coast, so its parts stay on the hull).
    pub carry_ms: u32,
}

/// Owner frames go about every this many game frames (see [`send_due`]).
pub const SEND_EVERY: u64 = 2;

/// Minimum gap between owner frames, ms. Matches the ship states' 14 ms floor
/// at paired steps, so riders stay in step with the ship.
pub const SEND_GAP_MS: u32 = 28;

/// Whether an owner frame is due at `now_ms` after one at `last_ms`.
#[must_use]
pub fn send_due(last_ms: Option<u32>, now_ms: u32) -> bool {
    last_ms.is_none_or(|l| {
        let gap = now_ms.wrapping_sub(l);
        // A clock that went back (a resync): send, and go on from it.
        gap >= SEND_GAP_MS || gap > u32::MAX / 2
    })
}

/// Whether an object (own name first, then ancestors) is a sent player family.
#[must_use]
pub fn is_player_family(chain: &[&str]) -> bool {
    let Some(own) = chain.first() else {
        return false;
    };
    if *own == "oPlayer" || *own == "oPlayerAttack" {
        return false;
    }
    chain
        .iter()
        .any(|n| n.starts_with("oPlayer") && *n != "oPlayerAttack")
}

/// Whether an object (same chain) descends from `oGuiObject`: the owner's
/// HUD, never sent.
#[must_use]
pub fn is_screen_ui(chain: &[&str]) -> bool {
    chain.contains(&"oGuiObject")
}

/// The ship's aiming and locating aids (object, what it shows). They follow
/// the local `oPlayer` and would read as the viewer's own aim, so they are
/// never sent.
pub const LOCAL_AIDS: &[(&str, &str)] = &[
    (
        "oBlinkReticle",
        "where a blink or warp lands (Warp shield, blink and strafe mods)",
    ),
    (
        "oScope",
        "each gun's firing direction, just ahead of the ship",
    ),
    ("oPointer", "where the ship is when it is outside the view"),
    ("oFinder", "where the ship is when it is outside the view"),
];

/// Whether an object (same chain) is or descends from a [`LOCAL_AIDS`] entry
/// (still player-side, never sent).
#[must_use]
pub fn is_local_aid(chain: &[&str]) -> bool {
    chain
        .iter()
        .any(|n| LOCAL_AIDS.iter().any(|(aid, _)| aid == n))
}

/// Tag `made` (instance address, object) when player-side code made it,
/// untag it otherwise: the tag follows the instance's life, not its address.
#[allow(clippy::implicit_hasher)] // the mirror's own set
pub fn retag(lineage: &mut HashSet<(usize, u32)>, made: (usize, u32), player_side: bool) {
    if player_side {
        lineage.insert(made);
    } else {
        lineage.remove(&made);
    }
}

/// The lineage entry a `reflect` call makes, if any. `reflect` turns the
/// enemy shot (self) into its clone with `instance_change` (no Create event),
/// so the clone is player-side when the reflector (other) is and self's
/// object changed.
#[must_use]
pub fn reflect_mark(
    before: Option<(usize, u32)>,
    after: Option<(usize, u32)>,
    reflector_player_side: bool,
) -> Option<(usize, u32)> {
    let (b, a) = (before?, after?);
    (reflector_player_side && b.0 == a.0 && b.1 != a.1).then_some(a)
}

/// What the owner read from one instance this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Read {
    pub inst: usize,
    /// The instance id (0 when unread): with `inst`, one life of an address.
    pub id: u64,
    pub object: u32,
    pub sprite: i32,
    pub x: f64,
    pub y: f64,
    pub angle: f64,
    pub frame: f64,
    pub xscale: f64,
    pub yscale: f64,
    pub alpha: f64,
    pub blend: u32,
    pub depth: f64,
    pub visible: bool,
}

/// The owner side: stable 16-bit keys per instance life and look changes.
#[derive(Debug, Default)]
pub struct Sender {
    next_key: u16,
    /// Address -> (key, object, instance id, segments).
    keys: HashMap<usize, (u16, u32, u64, Segments)>,
    looks: HashMap<u16, MirrorLook>,
    /// The keys of `keys` (last frame's instances): a new key skips them.
    live: HashSet<u16>,
}

impl Sender {
    /// Entries and changed looks for this frame's reads; unread instances are
    /// forgotten. A key is one life of an address (same object and id).
    pub fn frame(&mut self, reads: &[Read]) -> (Vec<MirrorEntry>, Vec<MirrorLook>) {
        let mut entries = Vec::with_capacity(reads.len());
        let mut changed = Vec::new();
        let mut alive = HashMap::with_capacity(reads.len());
        for r in reads {
            let (key, mut segs) = match self.keys.get(&r.inst) {
                Some(&(k, o, id, g)) if o == r.object && id == r.id => (k, g),
                _ => {
                    // The count wraps at 65,536: skip keys still in use.
                    let mut k = self.next_key;
                    for _ in 0..u16::MAX {
                        if !self.live.contains(&k) {
                            break;
                        }
                        k = k.wrapping_add(1);
                    }
                    self.next_key = k.wrapping_add(1);
                    (k, Segments::default())
                }
            };
            // A jump beyond its motion (wrap, blink) starts a new segment.
            let seg = segs.observe(Vec2::new(r.x as f32, r.y as f32));
            alive.insert(r.inst, (key, r.object, r.id, segs));
            entries.push(MirrorEntry {
                seg,
                ..MirrorEntry::quantize(
                    key, r.x, r.y, r.angle, r.frame, r.xscale, r.yscale, r.alpha,
                )
            });
            let look = MirrorLook {
                key,
                object: r.object.min(u32::from(u16::MAX)) as u16,
                sprite: r.sprite,
                depth: r.depth.round() as i32,
                blend: r.blend,
                visible: r.visible,
            };
            if self.looks.get(&key) != Some(&look) {
                self.looks.insert(key, look);
                changed.push(look);
            }
        }
        let live: HashSet<u16> = alive.values().map(|v| v.0).collect();
        self.looks.retain(|k, _| live.contains(k));
        self.keys = alive;
        self.live = live;
        (entries, changed)
    }

    /// Instances currently keyed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// The key of an instance (by [`Read::inst`]), if it is keyed.
    #[must_use]
    pub fn key_of(&self, inst: usize) -> Option<u16> {
        self.keys.get(&inst).map(|&(k, ..)| k)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// One drawn instance on the viewer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shown {
    pub key: u16,
    /// Owner time of the pose drawn (earlier than the render time if late).
    pub at: u32,
    pub look: MirrorLook,
    pub x: f64,
    pub y: f64,
    pub angle: f64,
    pub frame: f64,
    pub xscale: f64,
    pub yscale: f64,
    pub alpha: f64,
}

#[derive(Debug, Clone, Default)]
struct Track {
    samples: VecDeque<(u32, MirrorEntry)>,
    /// Playback began (it had a look when the render time reached it).
    started: bool,
    /// The render time passed its first sample while it had no look.
    waited: bool,
    /// How far behind the render time it plays, ms; shrinks as it catches up.
    late: f64,
}

/// The viewer side: a partner's instances by key, on the owner's clock.
#[derive(Debug, Default)]
pub struct Receiver {
    /// Carry past the newest sample, ms (`None`: [`GONE_AFTER_MS`]).
    pub carry_ms: Option<u32>,
    tracks: HashMap<u16, Track>,
    /// Looks by key. Sent only on change, so a look outlives its track and
    /// goes only when a complete frame shows its instance gone.
    looks: HashMap<u16, MirrorLook>,
    newest: Option<u32>,
    /// Recent frame times: (parts received, parts).
    frames: BTreeMap<u32, (u8, u8)>,
    /// The render time of the last [`Receiver::shown`].
    last_t: Option<u32>,
    /// [`Self::newest`] at the last call, and how many calls it held still.
    quiet: (Option<u32>, u32),
    /// The last freeze the render time stepped over: (before, after).
    jump: Option<(u32, u32)>,
}

/// Samples kept per instance at most.
const KEEP: usize = 64;
/// A track with no look yet is kept this long past its newest sample, ms.
pub const LOOK_WAIT_MS: u32 = 1000;
/// A track whose look came late plays at most this far behind, ms.
pub const MAX_LATE_MS: u32 = 600;
/// A late track catches up by this share of elapsed time (1.1x speed).
const CATCH_UP: f64 = 0.1;
/// Shown past its newest sample for at most this long, ms.
pub const GONE_AFTER_MS: u32 = 120;
/// Shown frames a track is held past [`GONE_AFTER_MS`] while the owner sends nothing newer:
/// a freeze, or a link cut up to its 10 s timeout.
pub const QUIET_HOLD_FRAMES: u32 = 720;
/// A render time step longer than this (ms) is a shared freeze, not play.
pub const FREEZE_GAP_MS: u32 = 250;
/// Frame times remembered for the end-of-life test.
const FRAMES_KEPT: usize = 64;
/// Looks kept beyond the live tracks before the spare ones are forgotten.
const LOOKS_SPARE: usize = 4096;

impl Receiver {
    /// One part of a frame (`parts` in all).
    pub fn on_frame(&mut self, time_ms: u32, parts: u8, entries: &[MirrorEntry]) {
        self.newest = Some(self.newest.map_or(time_ms, |n| n.max(time_ms)));
        let f = self.frames.entry(time_ms).or_insert((0, parts));
        f.0 = f.0.saturating_add(1);
        while self.frames.len() > FRAMES_KEPT {
            self.frames.pop_first();
        }
        for e in entries {
            let t = self.tracks.entry(e.key).or_default();
            if t.samples.back().is_some_and(|(s, _)| *s >= time_ms) {
                continue;
            }
            t.samples.push_back((time_ms, *e));
            while t.samples.len() > KEEP {
                t.samples.pop_front();
            }
        }
    }

    pub fn on_looks(&mut self, looks: &[MirrorLook]) {
        for l in looks {
            self.looks.insert(l.key, *l);
        }
    }

    /// The newest owner time received.
    #[must_use]
    pub fn newest(&self) -> Option<u32> {
        self.newest
    }

    /// Everything to draw at owner time `t`, dropping what is gone. A track
    /// whose look came late plays from its first sample on its own clock and
    /// catches up.
    pub fn shown(&mut self, t: u32) -> Vec<Shown> {
        let prev_t = self.last_t;
        let dt = prev_t.map_or(0, |p| t.saturating_sub(p));
        self.last_t = Some(t);
        if let Some(p) = prev_t.filter(|_| dt > FREEZE_GAP_MS) {
            self.jump = Some((p, t));
        }
        let jump = self.jump;
        if self.quiet.0 == self.newest {
            self.quiet.1 = self.quiet.1.saturating_add(1);
        } else {
            self.quiet = (self.newest, 0);
        }
        let (newest, quiet) = (self.newest, self.quiet.1 < QUIET_HOLD_FRAMES);
        let frames = &self.frames;
        // Gone: a complete frame after its last sample, up to `at`, lacks it.
        let ended = |last: u32, at: u32| {
            last < at
                && frames
                    .range(last + 1..=at)
                    .any(|(_, (got, parts))| got >= parts)
        };
        let looks = &mut self.looks;
        self.tracks.retain(|key, tr| {
            let (Some(&(first, _)), Some(&(s, _))) = (tr.samples.front(), tr.samples.back()) else {
                return true;
            };
            if tr.started {
                tr.late = (tr.late - f64::from(dt) * CATCH_UP).max(0.0);
            } else {
                if !looks.contains_key(key) {
                    // Waiting for its look: kept, samples and all.
                    tr.waited |= t >= first;
                    return t <= s.saturating_add(LOOK_WAIT_MS);
                }
                if t < first {
                    return true;
                }
                tr.started = true;
                // A whole life between two frames plays late, so it is seen.
                let missed = prev_t.is_some_and(|p| first > p) && s < t;
                // Never from before a freeze the render time stepped over.
                let late = if tr.waited || missed {
                    let since_resume = jump
                        .filter(|&(_, to)| first < to)
                        .map_or(u32::MAX, |(_, to)| t.saturating_sub(to));
                    (t - first).min(MAX_LATE_MS).min(since_resume)
                } else {
                    0
                };
                tr.late = f64::from(late);
            }
            let at = t.saturating_sub(tr.late.round() as u32);
            if ended(s, at) {
                looks.remove(key);
                return false;
            }
            // Held while the owner is quiet and this track is near its newest frame.
            at <= s.saturating_add(GONE_AFTER_MS)
                || (quiet && newest.is_none_or(|n| n <= s.saturating_add(GONE_AFTER_MS)))
        });
        if self.looks.len() > self.tracks.len() + LOOKS_SPARE {
            let tracks = &self.tracks;
            self.looks.retain(|k, _| tracks.contains_key(k));
        }
        let mut out = Vec::new();
        let carry = self.carry_ms.unwrap_or(GONE_AFTER_MS);
        for (&key, tr) in &mut self.tracks {
            if !tr.started {
                continue;
            }
            let Some(&look) = self.looks.get(&key) else {
                continue;
            };
            let at = t.saturating_sub(tr.late.round() as u32);
            // Played samples are not needed again (one before `at` stays).
            while tr.samples.len() > 2 && tr.samples[1].0 <= at {
                tr.samples.pop_front();
            }
            if let Some(s) = sample_at(&tr.samples, at, jump, carry) {
                out.push(Shown { key, at, look, ..s });
            }
        }
        out.sort_by_key(|s| s.key);
        out
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.looks.clear();
        self.newest = None;
        self.frames.clear();
        self.last_t = None;
    }

    /// A new run here: drop the last run's poses; keep the looks (sent once).
    pub fn new_run(&mut self) {
        self.tracks.clear();
        self.frames.clear();
        self.last_t = None;
        self.quiet = (None, 0);
        self.jump = None;
    }
}

/// Below this `image_alpha` an instance is faint.
pub const FAINT_ALPHA: f64 = 0.1;

/// Owner: whether an instance may have a leader this frame: only while
/// visible (no zero scale, not faint).
#[must_use]
pub fn followable(alpha: f64, xscale: f64, yscale: f64) -> bool {
    xscale != 0.0 && yscale != 0.0 && alpha >= FAINT_ALPHA
}

/// Owner: the leaders to send this frame (`follows`: entry index -> net id,
/// offset). Sent on a new leader, an offset move of `resend` half pixels, or
/// a stop; `sent` keeps the last sent per key.
#[allow(clippy::implicit_hasher)] // the mirror's own maps
pub fn leader_changes(
    sent: &mut HashMap<u16, MirrorLeader>,
    entries: &[MirrorEntry],
    follows: &HashMap<usize, (u16, f64, f64)>,
    resend: i16,
) -> Vec<MirrorLeader> {
    let mut out = Vec::new();
    let mut alive = HashMap::with_capacity(sent.len());
    for (i, e) in entries.iter().enumerate() {
        let now = follows
            .get(&i)
            .map(|&(net, dx, dy)| MirrorLeader::new(e.key, Some(net), dx, dy));
        let before = sent.get(&e.key).copied();
        match (now, before) {
            (Some(n), Some(b))
                if n.net == b.net
                    && (n.dx - b.dx).abs() < resend
                    && (n.dy - b.dy).abs() < resend =>
            {
                alive.insert(e.key, b);
            }
            (Some(n), _) => {
                out.push(n);
                alive.insert(e.key, n);
            }
            (None, Some(_)) => out.push(MirrorLeader::new(e.key, None, 0.0, 0.0)),
            (None, None) => {}
        }
    }
    *sent = alive;
    out
}

/// Viewer: take the leader changes (time, key, leader) due now out of
/// `changes`: at or before the key's shown time (`at_of`, else `t`). A key's
/// first leader is due at once.
#[allow(clippy::implicit_hasher)] // the mirror's own maps
pub fn take_due_leaders(
    changes: &mut Vec<(u32, u16, Option<MirrorLeader>)>,
    current: &HashMap<u16, MirrorLeader>,
    at_of: &HashMap<u16, u32>,
    t: u32,
) -> Vec<(u32, u16, Option<MirrorLeader>)> {
    let mut due = Vec::new();
    let mut later = Vec::new();
    // Keys with a leader now, or with a change waiting before this one.
    let mut led: std::collections::HashSet<u16> = current.keys().copied().collect();
    for c in std::mem::take(changes) {
        let first = c.2.is_some() && !led.contains(&c.1);
        if first || c.0 <= at_of.get(&c.1).copied().unwrap_or(t) {
            if c.2.is_some() {
                led.insert(c.1);
            }
            due.push(c);
        } else {
            led.insert(c.1);
            later.push(c);
        }
    }
    *changes = later;
    due
}

/// A leader change this much older than the current one means a clock resync.
pub const LEADER_CLOCK_BACK_MS: u32 = 10_000;

/// Viewer: apply `due` changes to `current` in time order. A change older
/// than the key's leader (`at`) is skipped; returns the changes taken and
/// the number skipped.
#[allow(clippy::implicit_hasher)] // the mirror's own maps
pub fn take_leader_changes(
    mut due: Vec<(u32, u16, Option<MirrorLeader>)>,
    current: &mut HashMap<u16, MirrorLeader>,
    at: &mut HashMap<u16, u32>,
) -> (Vec<(u32, u16, MirrorLeader)>, u64) {
    due.sort_by_key(|c| c.0);
    let (mut taken, mut older) = (Vec::new(), 0);
    for (t, key, l) in due {
        let new = l.unwrap_or_else(|| MirrorLeader::new(key, None, 0.0, 0.0));
        if let Some(&prev) = at.get(&key) {
            let back = prev.saturating_sub(t) > LEADER_CLOCK_BACK_MS;
            if !back && t < prev {
                older += 1;
                continue;
            }
            if !back && current.get(&key) == Some(&new) {
                continue;
            }
        }
        at.insert(key, t);
        current.insert(key, new);
        taken.push((t, key, new));
    }
    (taken, older)
}

/// Interpolate (or extrapolate from the last two samples) at `t`.
fn sample_at(
    s: &VecDeque<(u32, MirrorEntry)>,
    t: u32,
    jump: Option<(u32, u32)>,
    carry: u32,
) -> Option<Shown> {
    let pose = |e: &MirrorEntry| Shown {
        key: e.key,
        at: t,
        look: MirrorLook {
            key: e.key,
            object: 0,
            sprite: -1,
            depth: 0,
            blend: 0,
            visible: true,
        },
        x: e.x(),
        y: e.y(),
        angle: e.angle_deg(),
        frame: f64::from(e.frame),
        xscale: e.xscale(),
        yscale: e.yscale(),
        alpha: e.alpha(),
    };
    let (a, b, after) = match s.iter().position(|(ts, _)| *ts >= t) {
        Some(0) => return s.front().map(|(_, e)| pose(e)),
        Some(i) => (s[i - 1], s[i], s.get(i + 1).copied()),
        None if s.len() >= 2 => (s[s.len() - 2], s[s.len() - 1], None),
        None => return s.back().map(|(_, e)| pose(e)),
    };
    let (ta, tb) = (f64::from(a.0), f64::from(b.0));
    if tb <= ta {
        return Some(pose(&b.1));
    }
    // Across a shared freeze the instance holds its last pose before it.
    if let Some((from, to)) = jump {
        if t > b.0 && b.0 < to && to - b.0 > FREEZE_GAP_MS {
            return Some(pose(&b.1));
        }
        if t < b.0 && b.0 - a.0 > FREEZE_GAP_MS && a.0 < to && from < b.0 {
            return Some(pose(&a.1));
        }
    }
    // Extrapolate at most `carry` past the newest sample, then hold.
    let f = (f64::from(t).min(tb + f64::from(carry)) - ta) / (tb - ta);
    let (pa, pb) = (pose(&a.1), pose(&b.1));
    // A new segment is a jump: hold, unless the next move repeats it (a fast
    // shot's first move is marked as a jump).
    if a.1.seg != b.1.seg
        && !after.is_some_and(|c| {
            c.1.seg == b.1.seg && repeats(&pa, &pb, &pose(&c.1), ta, tb, f64::from(c.0))
        })
    {
        return Some(if f < 1.0 { pa } else { pb });
    }
    let lerp = |x: f64, y: f64| x + (y - x) * f;
    // Looks extrapolate too, but a scale never crosses zero.
    let scale = |x: f64, y: f64| {
        let v = lerp(x, y);
        if y >= 0.0 { v.max(0.0) } else { v.min(0.0) }
    };
    let mut da = (pb.angle - pa.angle).rem_euclid(360.0);
    if da > 180.0 {
        da -= 360.0;
    }
    Some(Shown {
        x: lerp(pa.x, pb.x),
        y: lerp(pa.y, pb.y),
        angle: (pa.angle + da * f).rem_euclid(360.0),
        frame: if f < 1.0 { pa.frame } else { pb.frame },
        xscale: scale(pa.xscale, pb.xscale),
        yscale: scale(pa.yscale, pb.yscale),
        alpha: lerp(pa.alpha, pb.alpha).clamp(0.0, 1.0),
        ..pb
    })
}

/// Whether move `b` → `c` continues `a` → `b` within a quarter of its
/// velocity plus 0.1 px/ms.
fn repeats(a: &Shown, b: &Shown, c: &Shown, ta: f64, tb: f64, tc: f64) -> bool {
    if tb <= ta || tc <= tb {
        return false;
    }
    let (v1x, v1y) = ((b.x - a.x) / (tb - ta), (b.y - a.y) / (tb - ta));
    let (v2x, v2y) = ((c.x - b.x) / (tc - tb), (c.y - b.y) / (tc - tb));
    (v2x - v1x).hypot(v2y - v1y) <= 0.25 * v1x.hypot(v1y) + 0.1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The steps at which owner frames go, from the steps' times.
    fn sent(times: &[u32]) -> Vec<u32> {
        let mut last = None;
        times
            .iter()
            .copied()
            .filter(|&t| {
                let due = send_due(last, t);
                if due {
                    last = Some(t);
                }
                due
            })
            .collect()
    }

    #[test]
    fn owner_frames_go_at_the_first_step_of_each_pair_and_every_other_even_step() {
        // Steps in pairs 2 ms apart with a 31 ms gap: the first of each pair,
        // where the ship's states (a 14 ms floor) go too.
        assert_eq!(sent(&[0, 2, 33, 35, 66, 68, 99]), vec![0, 33, 66, 99]);
        // Even steps: every other one.
        assert_eq!(sent(&[0, 17, 33, 50, 67, 83, 100]), vec![0, 33, 67, 100]);
        // A clock that went back sends at once.
        assert_eq!(sent(&[1000, 1033, 20, 50]), vec![1000, 1033, 20, 50]);
    }

    #[test]
    fn a_shot_reflected_by_the_player_is_marked_as_its_clone() {
        // The shot (key 7, an enemy projectile, 300) becomes the clone (477)
        // off the ship: marked under its new object.
        assert_eq!(
            reflect_mark(Some((7, 300)), Some((7, 477)), true),
            Some((7, 477))
        );
        // Reflected by something not player-side: not the player's.
        assert_eq!(reflect_mark(Some((7, 300)), Some((7, 477)), false), None);
        // Nothing changed (the call returned early): nothing to mark.
        assert_eq!(reflect_mark(Some((7, 300)), Some((7, 300)), true), None);
        // Another instance after the call, or none: not this shot's change.
        assert_eq!(reflect_mark(Some((7, 300)), Some((8, 477)), true), None);
        assert_eq!(reflect_mark(Some((7, 300)), None, true), None);
        assert_eq!(reflect_mark(None, Some((7, 477)), true), None);
    }

    fn read(inst: usize, object: u32, x: f64) -> Read {
        Read {
            inst,
            id: 100_000 + inst as u64,
            object,
            sprite: 5,
            x,
            y: 10.0,
            angle: 90.0,
            frame: 0.0,
            xscale: 1.0,
            yscale: 1.0,
            alpha: 1.0,
            blend: 0xFF_FFFF,
            depth: -99.0,
            visible: true,
        }
    }

    #[test]
    fn families_are_found_by_name_and_ancestry() {
        assert!(is_player_family(&["oBlaster", "oPlayerProjectile"]));
        assert!(is_player_family(&[
            "oConstructAllyStandard",
            "oConstructAlly",
            "oPlayerAdvancedConstruct",
            "oPlayerConstruct"
        ]));
        assert!(is_player_family(&["oPlayerBlast"]));
        assert!(!is_player_family(&["oPlayer"]));
        assert!(!is_player_family(&["oPlayerAttack"]));
        assert!(!is_player_family(&["oBurstRetaliate", "oPlayerAttack"]));
        assert!(!is_player_family(&["oRedShot", "oEnemyProjectile"]));
        assert!(!is_player_family(&[]));
    }

    #[test]
    fn keys_are_stable_and_looks_sent_on_change() {
        let mut s = Sender::default();
        let (e, l) = s.frame(&[read(1, 10, 0.0), read(2, 11, 5.0)]);
        assert_eq!((e.len(), l.len()), (2, 2));
        let k1 = e[0].key;
        let (e, l) = s.frame(&[read(1, 10, 1.0), read(2, 11, 6.0)]);
        assert_eq!((e[0].key, l.len()), (k1, 0));
        let mut r = read(1, 10, 2.0);
        r.sprite = 6;
        let (_, l) = s.frame(&[r]);
        assert_eq!(l.len(), 1);
        assert_eq!(s.len(), 1);
        // Same address, another object: a new key.
        let (e, l) = s.frame(&[read(1, 12, 0.0)]);
        assert_ne!(e[0].key, k1);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn keys_coming_round_skip_those_still_in_use() {
        let mut s = Sender::default();
        let (e, _) = s.frame(&[read(1, 10, 0.0), read(2, 11, 0.0)]);
        let (a, b) = (e[0].key, e[1].key);
        // 65,536 keys later the count is back at the first one.
        s.next_key = a;
        let (e, _) = s.frame(&[read(1, 10, 1.0), read(2, 11, 1.0), read(3, 12, 0.0)]);
        assert_eq!((e[0].key, e[1].key), (a, b));
        assert!(e[2].key != a && e[2].key != b, "{}", e[2].key);
        // The others ended: their keys are free again, the live one is not.
        let (e, _) = s.frame(&[read(3, 12, 1.0)]);
        let c = e[0].key;
        s.next_key = c;
        let (e, _) = s.frame(&[read(3, 12, 2.0), read(4, 13, 0.0)]);
        assert_eq!(e[0].key, c);
        assert_eq!(e[1].key, c.wrapping_add(1));
        // A new life at a live key's address, with the count at that key:
        // it skips it and the keys in use.
        s.next_key = c;
        let mut again = read(3, 12, 3.0);
        again.id += 1;
        let (e, _) = s.frame(&[again, read(4, 13, 1.0)]);
        assert_ne!(e[0].key, c, "a new life");
        assert_ne!(e[0].key, c.wrapping_add(1), "in use");
        assert_eq!(e[1].key, c.wrapping_add(1));
    }

    /// An instance made at the address of another between two owner frames
    /// gets its own key: one key is one life.
    #[test]
    fn two_lives_at_one_address_get_two_keys() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let first = read(7, 309, 100.0);
        let mut keys = Vec::new();
        for i in 0..5 {
            let mut p = first;
            p.x += f64::from(i) * 10.0;
            let (e, l) = s.frame(&[p]);
            r.on_looks(&l);
            r.on_frame(1000 + 33 * i, 1, &e);
            keys.push(e[0].key);
        }
        assert!(keys.windows(2).all(|k| k[0] == k[1]), "one life, one key");
        let mut next = read(7, 309, 900.0);
        next.id = first.id + 50;
        let (e, l) = s.frame(&[next]);
        r.on_looks(&l);
        r.on_frame(1165, 1, &e);
        assert_ne!(e[0].key, keys[0], "another life, another key");
        // The old key ended with the complete frame: only the new life is
        // shown, at its own place.
        let at = r.shown(1165);
        assert_eq!(at.len(), 1);
        assert_eq!(at[0].key, e[0].key);
        assert!((at[0].x - 900.0).abs() < 1.0, "{}", at[0].x);
    }

    #[test]
    fn viewer_interpolates_extrapolates_and_drops() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let (e, l) = s.frame(&[read(1, 10, 0.0)]);
        r.on_looks(&l);
        r.on_frame(1000, 1, &e);
        let (e, _) = s.frame(&[read(1, 10, 10.0)]);
        r.on_frame(1033, 1, &e);
        assert!(r.shown(990).is_empty(), "not before its first sample");
        let mid = r.shown(1016);
        assert_eq!(mid.len(), 1);
        assert!((mid[0].x - 4.8).abs() < 0.6, "{}", mid[0].x);
        let ahead = r.shown(1066);
        assert!(
            (ahead[0].x - 20.0).abs() < 0.6,
            "extrapolated {}",
            ahead[0].x
        );
        // Past its newest sample with nothing newer from the owner (a
        // freeze, both games resuming a link delay apart): kept for a while.
        assert_eq!(r.shown(1033 + GONE_AFTER_MS + 1).len(), 1);
        assert_eq!(r.shown(60_000).len(), 1, "a long pause's jump");
        // The owner speaks again without it (an incomplete frame, so not an
        // end): gone by the timeout.
        r.on_frame(60_100, 2, &[]);
        assert!(r.shown(60_101).is_empty());
        assert!(r.is_empty());
    }

    #[test]
    fn a_silent_owner_ends_its_tracks_after_the_quiet_hold() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let (e, l) = s.frame(&[read(1, 10, 0.0)]);
        r.on_looks(&l);
        r.on_frame(1000, 1, &e);
        assert_eq!(r.shown(1000).len(), 1);
        for i in 0..QUIET_HOLD_FRAMES {
            let _ = r.shown(1200 + i);
        }
        assert!(r.shown(5000).is_empty());
    }

    #[test]
    fn a_short_link_cut_keeps_the_partners_instances_where_they_were() {
        // A ship's glow (key 1) at 30 Hz frames, then a 3 s cut of the link
        // while this game runs at 60 frames a second: shown throughout, held
        // at its last place, and on again with the first frame after the cut.
        let mut s = Sender::default();
        let mut r = Receiver::default();
        feed(&mut r, &mut s, &[(1000, 0.0), (1033, 10.0), (1066, 20.0)]);
        let mut t = 1066;
        let mut last = None;
        for _ in 0..180 {
            let shown = r.shown(t);
            assert_eq!(shown.len(), 1, "dropped {} ms into the cut", t - 1066);
            last = Some(shown[0].x);
            t += 17;
        }
        let held = last.unwrap_or_default();
        let reach = 20.0 + 10.0 * f64::from(GONE_AFTER_MS) / 33.0;
        assert!((held - reach).abs() < 0.6, "held at {held}");
        feed(&mut r, &mut s, &[(4100, 300.0), (4133, 310.0)]);
        let back = r.shown(4133);
        assert_eq!(back.len(), 1, "the same key goes on");
        assert!((back[0].x - 310.0).abs() < 0.6, "{}", back[0].x);
    }

    #[test]
    fn a_cut_after_a_frame_that_came_in_part_still_holds() {
        // Frames in two parts: the glow (key 0) in part 0, a shot (key 1)
        // in part 1. The last frame before the cut came without part 0.
        let mut s = Sender::default();
        let mut r = Receiver::default();
        for (t, x) in [(1000u32, 0.0), (1033, 10.0)] {
            let (e, l) = s.frame(&[read(1, 10, x), read(2, 11, x)]);
            r.on_looks(&l);
            r.on_frame(t, 2, &e[..1]);
            r.on_frame(t, 2, &e[1..]);
        }
        let (e, _) = s.frame(&[read(1, 10, 20.0), read(2, 11, 20.0)]);
        r.on_frame(1066, 2, &e[1..]);
        let mut t = 1066;
        for _ in 0..150 {
            assert!(
                r.shown(t).iter().any(|d| d.key == 0),
                "the glow dropped {} ms into the cut",
                t - 1066
            );
            t += 17;
        }
    }

    #[test]
    fn a_late_look_plays_a_short_life_in_full_late() {
        // A burst alive 200 ms (frames at 33 ms); its look comes 300 ms
        // after its first frame (resent behind a lost reliable message).
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let mut look = Vec::new();
        for i in 0..7u32 {
            let (e, l) = s.frame(&[read(1, 10, f64::from(i) * 10.0)]);
            look.extend(l);
            r.on_frame(1000 + i * 33, 1, &e);
        }
        // Gone on the owner: complete frames without it.
        for i in 7..12u32 {
            let (e, _) = s.frame(&[]);
            r.on_frame(1000 + i * 33, 1, &e);
        }
        // The render time runs on without a look: nothing drawn, kept.
        let mut t = 1000;
        while t < 1300 {
            assert!(r.shown(t).is_empty());
            t += 16;
        }
        r.on_looks(&look);
        let mut xs = Vec::new();
        for _ in 0..40 {
            for d in r.shown(t) {
                assert!(d.at < t, "played on its own clock");
                xs.push(d.x);
            }
            t += 16;
        }
        assert!(
            xs.first().is_some_and(|x| *x < 5.0),
            "from its start: {xs:?}"
        );
        assert!(xs.last().is_some_and(|x| *x > 55.0), "to its end: {xs:?}");
        assert!(xs.len() >= 11, "{} frames drawn", xs.len());
        assert!(r.is_empty(), "gone after its end");
    }

    #[test]
    fn a_look_outlives_a_silence_but_not_an_end() {
        // A freeze: no frames for seconds, then the same instance again.
        // Its look was sent once and is not sent again.
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let (e, l) = s.frame(&[read(1, 10, 0.0), read(2, 11, 0.0)]);
        r.on_looks(&l);
        r.on_frame(1000, 1, &e);
        assert_eq!(r.shown(1000).len(), 2);
        assert_eq!(
            r.shown(4000).len(),
            2,
            "kept through the silence (nothing newer came)"
        );
        let (e, l) = s.frame(&[read(1, 10, 5.0), read(2, 11, 5.0)]);
        assert!(l.is_empty(), "unchanged looks are not sent again");
        r.on_frame(4100, 1, &e);
        assert_eq!(r.shown(4100).len(), 2, "drawn again with the looks it had");
        // Instance 2 ends: a complete frame without it.
        let (e, _) = s.frame(&[read(1, 10, 6.0)]);
        r.on_frame(4133, 1, &e);
        assert_eq!(r.shown(4133).len(), 1);
        assert_eq!(r.looks.len(), 1, "the ended instance's look is gone");
    }

    #[test]
    fn looks_go_on_past_the_newest_sample_but_never_flip() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let mut a = read(1, 10, 0.0);
        a.xscale = 0.2;
        let (e, l) = s.frame(&[a]);
        r.on_looks(&l);
        r.on_frame(1000, 1, &e);
        let mut b = read(1, 10, 10.0);
        b.xscale = 0.0;
        b.alpha = 0.5;
        let (e, _) = s.frame(&[b]);
        r.on_frame(1033, 1, &e);
        let ahead = r.shown(1066);
        assert!((ahead[0].x - 20.0).abs() < 0.6, "motion extrapolates");
        assert!(
            ahead[0].xscale.abs() < 0.01,
            "scale stops at zero: {}",
            ahead[0].xscale
        );
        assert!(ahead[0].alpha.abs() < 0.01, "alpha goes on to 0");
    }

    #[test]
    fn a_fast_shot_is_interpolated_from_its_first_move() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        for (i, t) in [1000u32, 1033, 1066, 1100].into_iter().enumerate() {
            let (e, l) = s.frame(&[read(1, 10, 142.0 * i as f64)]);
            r.on_looks(&l);
            r.on_frame(t, 1, &e);
        }
        // Between the first two samples (its first move a new segment).
        let mid = r.shown(1016);
        assert!((mid[0].x - 69.0).abs() < 5.0, "{}", mid[0].x);
        // A wrap is still held: a move the next one does not repeat.
        let mut s = Sender::default();
        let mut r = Receiver::default();
        for (t, x) in [(1000u32, 0.0), (1033, 10.0), (1066, 2310.0), (1100, 2320.0)] {
            let (e, l) = s.frame(&[read(1, 10, x)]);
            r.on_looks(&l);
            r.on_frame(t, 1, &e);
        }
        let mid = r.shown(1050);
        assert!(
            (mid[0].x - 10.0).abs() < 0.6,
            "held, not drawn across: {}",
            mid[0].x
        );
    }

    #[test]
    fn a_key_whose_life_fell_between_two_frames_is_still_shown() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let (e, l) = s.frame(&[read(1, 10, 0.0)]);
        r.on_looks(&l);
        r.on_frame(1000, 1, &e);
        assert_eq!(r.shown(1000).len(), 1);
        // A short shot (key 2) lives 1033..1066 while the viewer stalls
        // from 1000 to 1150; key 1 goes on.
        let (e, l) = s.frame(&[read(1, 10, 10.0), read(2, 10, 50.0)]);
        r.on_looks(&l);
        r.on_frame(1033, 1, &e);
        let (e, _) = s.frame(&[read(1, 10, 20.0), read(2, 10, 60.0)]);
        r.on_frame(1066, 1, &e);
        for t in [1100, 1133] {
            let (e, _) = s.frame(&[read(1, 10, 30.0)]);
            r.on_frame(t, 1, &e);
        }
        // Keys are numbered from 0: the long one 0, the short shot 1.
        let shown = r.shown(1150);
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert_eq!(
            (shown[1].key, shown[1].at),
            (1, 1033),
            "from its first sample"
        );
    }

    #[test]
    fn a_track_held_through_the_owners_silence_holds_its_pose() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let mut a = read(1, 10, 0.0);
        a.alpha = 0.25;
        let (e, l) = s.frame(&[a]);
        r.on_looks(&l);
        r.on_frame(1000, 1, &e);
        let mut b = read(1, 10, 10.0);
        b.alpha = 0.275;
        let (e, _) = s.frame(&[b]);
        r.on_frame(1033, 1, &e);
        // A freeze: the render time jumps 1 s past the newest sample before
        // the owner's next frame; the track is kept (quiet hold).
        let held = r.shown(2033);
        assert_eq!(held.len(), 1);
        let reach = 1.0 + f64::from(GONE_AFTER_MS) / 33.0;
        assert!(
            (held[0].x - 10.0 * reach).abs() < 0.6,
            "motion stops after GONE_AFTER_MS: {}",
            held[0].x
        );
        assert!(
            held[0].alpha < 0.4,
            "alpha not run on to 1: {}",
            held[0].alpha
        );
    }

    #[test]
    fn parts_coast_as_long_as_the_hull_through_a_silence() {
        // The ship's coast (`config::EXTRAPOLATION`) 250 ms: a part moving 10 px
        // a frame (33 ms) goes on 250 ms past its newest sample, then holds.
        let mut s = Sender::default();
        let mut r = Receiver {
            carry_ms: Some(250),
            ..Receiver::default()
        };
        feed(&mut r, &mut s, &[(1000, 0.0), (1033, 10.0)]);
        let mut t = 1033;
        let mut x = 0.0;
        for _ in 0..60 {
            let d = r.shown(t);
            assert_eq!(d.len(), 1, "held through the silence");
            x = d[0].x;
            t += 17;
        }
        let reach = 10.0 + 10.0 * 250.0 / 33.0;
        assert!((x - reach).abs() < 0.6, "coasted to {x}, not {reach}");
    }

    #[test]
    fn a_new_run_drops_the_held_poses_and_keeps_the_looks() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        feed(&mut r, &mut s, &[(1000, 0.0), (1033, 10.0)]);
        // The partner went quiet; its pose is held.
        assert_eq!(r.shown(1500).len(), 1);
        r.new_run();
        assert!(r.shown(1517).is_empty(), "nothing of the last run drawn");
        // The same key's next frame draws again with the look sent before.
        let (e, l) = s.frame(&[read(1, 10, 20.0)]);
        assert!(l.is_empty(), "the look is not sent again");
        r.on_frame(1600, 1, &e);
        assert_eq!(r.shown(1600).len(), 1);
    }

    /// Owner frames of one instance (1, object 10): (owner time, x) each.
    fn feed(r: &mut Receiver, s: &mut Sender, xs: &[(u32, f64)]) {
        for &(t, x) in xs {
            let (e, l) = s.frame(&[read(1, 10, x)]);
            r.on_looks(&l);
            r.on_frame(t, 1, &e);
        }
    }

    #[test]
    fn a_shared_freeze_holds_the_pose_it_stopped_at() {
        // The owner's frames stop at 1066 and come back at 6066; the viewer
        // draws nothing in between, its render time steps from 1066 to 6050.
        let mut s = Sender::default();
        let mut r = Receiver::default();
        feed(&mut r, &mut s, &[(1000, 0.0), (1033, 100.0), (1066, 200.0)]);
        assert!((r.shown(1066)[0].x - 200.0).abs() < 0.6);
        // Resumed before the owner's first frame after the freeze is here:
        // where it stopped, not carried on.
        let resumed = r.shown(6050);
        assert!((resumed[0].x - 200.0).abs() < 0.6, "{}", resumed[0].x);
        // The owner's frames again (one step after its resume): still where
        // it stopped until then, no crawl across the 5 s gap, then on.
        feed(&mut r, &mut s, &[(6066, 300.0), (6100, 400.0)]);
        let before = r.shown(6060);
        assert!((before[0].x - 200.0).abs() < 0.6, "{}", before[0].x);
        let on = r.shown(6083);
        assert!((on[0].x - 350.0).abs() < 3.0, "{}", on[0].x);
    }

    #[test]
    fn a_shot_born_before_a_freeze_starts_at_the_resume() {
        // Born 16 ms after the viewer's last frame before the freeze (never
        // drawn before it); after the freeze it plays from the resume, not
        // 600 ms back into the freeze's span.
        let mut s = Sender::default();
        let mut r = Receiver::default();
        feed(&mut r, &mut s, &[(1000, 0.0)]);
        assert_eq!(r.shown(1000).len(), 1);
        let (e, l) = s.frame(&[read(1, 10, 0.0), read(2, 10, 500.0)]);
        r.on_looks(&l);
        r.on_frame(1016, 1, &e);
        let (e, _) = s.frame(&[read(1, 10, 0.0), read(2, 10, 600.0)]);
        r.on_frame(1049, 1, &e);
        let shown = r.shown(6050);
        let shot = shown.iter().find(|d| d.key == 1).expect("shown");
        assert_eq!(shot.at, 6050, "on time");
        assert!((shot.x - 600.0).abs() < 0.6, "where it stopped: {}", shot.x);
    }

    #[test]
    fn a_complete_frame_without_an_instance_ends_it() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let (e, l) = s.frame(&[read(1, 10, 0.0), read(2, 10, 0.0)]);
        r.on_looks(&l);
        r.on_frame(1000, 1, &e);
        let (e, _) = s.frame(&[read(1, 10, 10.0), read(2, 10, 10.0)]);
        r.on_frame(1033, 1, &e);
        let (e, _) = s.frame(&[read(1, 10, 20.0)]);
        // Two parts, only the first arrived: not proof yet.
        r.on_frame(1066, 2, &e);
        assert_eq!(r.shown(1070).len(), 2);
        let (e, _) = s.frame(&[read(1, 10, 30.0)]);
        r.on_frame(1100, 1, &e);
        assert_eq!(r.shown(1080).len(), 2, "the complete frame is later");
        let at = r.shown(1100);
        assert_eq!(at.len(), 1);
        assert_eq!(at[0].key, 0);
    }

    #[test]
    fn screen_ui_is_found_by_ancestry() {
        assert!(is_screen_ui(&[
            "oTutorialGeneric",
            "oTutorial",
            "oGuiObject"
        ]));
        assert!(!is_screen_ui(&["oBurstGrow", "oEffect"]));
    }

    /// The ship's aiming and locating aids are never sent; the ship's
    /// visible effects, its families and its constructs still are (ancestry
    /// chains as the game has them).
    #[test]
    fn the_ships_aiming_aids_are_never_sent() {
        for aid in [
            &["oBlinkReticle", "oEffect"][..],
            &["oScope", "oEffect"],
            &["oPointer", "oEffect"],
            &["oFinder", "oEffect"],
        ] {
            assert!(is_local_aid(aid), "{aid:?}");
            assert!(!is_screen_ui(aid), "{aid:?}");
            assert!(!is_player_family(aid), "{aid:?}: sent by lineage only");
        }
        // A child of an aid is an aid.
        assert!(is_local_aid(&["oScopeChild", "oScope", "oEffect"]));
        assert!(!is_local_aid(&["oTutorial", "oGuiObject"]));
        // Ship-attached things the partner sees: effects that also follow
        // the ship, the ship's families, its constructs.
        for shown in [
            &["oBurstThrust", "oEffect"][..],
            &["oArmorStack", "oEffectPooled", "oEffect"],
            &["oWarpathStack", "oEffectPooled", "oEffect"],
            &["oCalibrate", "oEffect"],
            &["oAegis"],
            &["oPlayerEffectWhite", "oEffect"],
            &["oPlayerGlow"],
            &["oShieldWarpIndicator", "oPlayerShieldEffect"],
            &[
                "oConstructTurret",
                "oPlayerAdvancedConstruct",
                "oPlayerConstruct",
            ],
        ] {
            assert!(!is_local_aid(shown), "{shown:?}");
        }
        // Every aid has its reason.
        assert!(
            LOCAL_AIDS
                .iter()
                .all(|(n, why)| n.starts_with('o') && !why.is_empty())
        );
    }

    /// A pooled effect reused at scale 0 next to an enemy follows nothing;
    /// a visible instance does.
    #[test]
    fn only_a_visible_instance_follows() {
        assert!(!followable(0.33, 0.0, 0.0), "scale 0");
        assert!(!followable(1.0, 0.5, 0.0), "one scale 0");
        assert!(!followable(0.05, 1.0, 1.0), "faint");
        assert!(followable(FAINT_ALPHA, 1.0, 1.0), "at the cut");
        assert!(followable(0.8, 0.25, 0.25), "visible");
        assert!(followable(0.8, -1.0, 1.0), "mirrored sprite");
        // A follower withdrawn this way is sent with no leader once.
        let mut sent = HashMap::new();
        let e = |a: f64| MirrorEntry::quantize(7, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, a);
        let follows: HashMap<usize, (u16, f64, f64)> = [(0, (40, 10.0, 0.0))].into();
        assert_eq!(leader_changes(&mut sent, &[e(1.0)], &follows, 2).len(), 1);
        let none = HashMap::new();
        let out = leader_changes(&mut sent, &[e(0.05)], &none, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].net, None);
    }

    /// The lineage follows an instance's life, not its address.
    #[test]
    fn a_new_life_at_an_address_is_tagged_by_its_own_maker() {
        let mut lin = HashSet::new();
        let burst = (0x1000, 309);
        retag(&mut lin, burst, true);
        assert!(lin.contains(&burst), "the player's burst");
        // An effect the player's burst made in its Create (nested: player
        // side) stays tagged beside it.
        retag(&mut lin, (0x2000, 303), true);
        // The same address and object made again by enemy code: not the
        // player's.
        retag(&mut lin, burst, false);
        assert!(!lin.contains(&burst), "the enemy's burst");
        assert!(
            lin.contains(&(0x2000, 303)),
            "the other effect is untouched"
        );
        // Made again by the player: tagged again.
        retag(&mut lin, burst, true);
        assert!(lin.contains(&burst));
        // Another object at that address is its own entry.
        retag(&mut lin, (0x1000, 541), false);
        assert!(lin.contains(&burst));
    }

    #[test]
    fn jumps_are_held_not_interpolated() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let (e, l) = s.frame(&[read(1, 10, 2000.0)]);
        r.on_looks(&l);
        r.on_frame(0, 1, &e);
        let (e, _) = s.frame(&[read(1, 10, 10.0)]);
        r.on_frame(100, 1, &e);
        assert!((r.shown(50)[0].x - 2000.0).abs() < 0.6);
        assert!((r.shown(130)[0].x - 10.0).abs() < 0.6, "no extrapolation");
    }

    #[test]
    fn angles_take_the_short_way() {
        let mut s = Sender::default();
        let mut r = Receiver::default();
        let mut a = read(1, 10, 0.0);
        a.angle = 350.0;
        let (e, l) = s.frame(&[a]);
        r.on_looks(&l);
        r.on_frame(0, 1, &e);
        a.angle = 10.0;
        let (e, _) = s.frame(&[a]);
        r.on_frame(100, 1, &e);
        let m = r.shown(50);
        assert!(m[0].angle < 1.0 || m[0].angle > 359.0, "{}", m[0].angle);
    }

    #[test]
    fn a_late_key_takes_its_leader_changes_at_its_own_time() {
        let l = |net| Some(MirrorLeader::new(7, Some(net), 0.0, 0.0));
        // Key 7: 268 until 1000, 371 at 1100, `-` at 1130; the key plays
        // 460 ms late (its sample at 700 while `t` is 1160).
        let mut ch = vec![
            (1000, 7, l(268)),
            (1100, 7, l(371)),
            (1130, 7, None),
            (1100, 9, None),
        ];
        let at = HashMap::from([(7u16, 700u32)]);
        let due = take_due_leaders(&mut ch, &HashMap::from([(7u16, l(1).unwrap())]), &at, 1160);
        // Key 9 is not shown: by `t`. Key 7: nothing yet.
        assert_eq!(due.iter().map(|c| c.1).collect::<Vec<_>>(), vec![9]);
        assert_eq!(ch.len(), 3);
        // Its own time reaches 1000: 268 only; then 1130: both others.
        let due = take_due_leaders(
            &mut ch,
            &HashMap::new(),
            &HashMap::from([(7u16, 1000u32)]),
            1460,
        );
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].2.and_then(|x| x.net), Some(268));
        let due = take_due_leaders(
            &mut ch,
            &HashMap::new(),
            &HashMap::from([(7u16, 1130u32)]),
            1590,
        );
        assert_eq!(due.len(), 2);
        assert!(due[1].2.is_none() && ch.is_empty());
        // A key's first leader is due at once, even stamped after its
        // first sample; a second change for it still waits for its time.
        let mut ch = vec![(500, 3, l(211)), (520, 3, l(212))];
        let due = take_due_leaders(
            &mut ch,
            &HashMap::new(),
            &HashMap::from([(3u16, 450u32)]),
            900,
        );
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].2.and_then(|x| x.net), Some(211));
        assert_eq!(ch.len(), 1);
    }

    #[test]
    fn a_late_reliable_copy_does_not_undo_a_newer_change() {
        let l = |net| Some(MirrorLeader::new(7, Some(net), 0.0, 0.0));
        let (mut cur, mut at) = (HashMap::new(), HashMap::new());
        // On 36 from 74167, put on 28 at 74434; the frames carry 28 at 74434
        // (and 74467, 74500); its reliable copy comes 300 ms late, after a
        // third change to 40 at 74600 is in.
        let (taken, older) = take_leader_changes(
            vec![(74167, 7, l(36)), (74467, 7, l(28)), (74434, 7, l(28))],
            &mut cur,
            &mut at,
        );
        assert_eq!(
            taken.iter().map(|c| (c.0, c.2.net)).collect::<Vec<_>>(),
            vec![(74167, Some(36)), (74434, Some(28))]
        );
        assert_eq!(older, 0);
        let (taken, older) = take_leader_changes(
            vec![(74600, 7, l(40)), (74500, 7, l(28)), (74434, 7, l(28))],
            &mut cur,
            &mut at,
        );
        assert_eq!(taken.len(), 1);
        assert_eq!(cur[&7].net, Some(40));
        assert_eq!(
            older, 0,
            "28 at 74434/74500 sorts before 40: no change, not older"
        );
        let (taken, older) = take_leader_changes(vec![(74434, 7, l(28))], &mut cur, &mut at);
        assert!(taken.is_empty());
        assert_eq!((older, cur[&7].net), (1, Some(40)));
        // A stop, then a clock that went back (a resync): taken.
        let (taken, _) = take_leader_changes(vec![(74700, 7, None)], &mut cur, &mut at);
        assert_eq!(taken[0].2.net, None);
        let (taken, _) = take_leader_changes(vec![(500, 7, l(9))], &mut cur, &mut at);
        assert_eq!((taken.len(), cur[&7].net), (1, Some(9)));
    }

    #[test]
    fn leaders_are_sent_on_change_and_when_they_stop() {
        let mut s = Sender::default();
        let mut sent = HashMap::new();
        let (e, _) = s.frame(&[read(1, 10, 0.0), read(2, 10, 5.0)]);
        // The first follows net 7, 3 px right of it; the second nothing.
        let f: HashMap<usize, (u16, f64, f64)> = [(0, (7, 3.0, 0.0))].into();
        let out = leader_changes(&mut sent, &e, &f, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].key, out[0].net, out[0].offset()),
            (e[0].key, Some(7), (3.0, 0.0))
        );
        // Same leader, offset within a pixel: nothing; moved: sent again.
        let f2: HashMap<usize, (u16, f64, f64)> = [(0, (7, 3.4, 0.0))].into();
        assert!(leader_changes(&mut sent, &e, &f2, 2).is_empty());
        let f3: HashMap<usize, (u16, f64, f64)> = [(0, (7, 9.0, 0.0))].into();
        assert_eq!(leader_changes(&mut sent, &e, &f3, 2).len(), 1);
        // Another leader (a pooled flash reused on another enemy): sent.
        let f4: HashMap<usize, (u16, f64, f64)> = [(0, (8, 9.0, 0.0))].into();
        assert_eq!(leader_changes(&mut sent, &e, &f4, 2)[0].net, Some(8));
        // Stops following: sent once with no leader, then nothing.
        let out = leader_changes(&mut sent, &e, &HashMap::new(), 2);
        assert_eq!((out.len(), out[0].net), (1, None));
        assert!(leader_changes(&mut sent, &e, &HashMap::new(), 2).is_empty());
        // A key gone from the frame is forgotten.
        leader_changes(&mut sent, &e, &f, 2);
        leader_changes(&mut sent, &e[1..], &HashMap::new(), 2);
        assert!(sent.is_empty());
    }
}
