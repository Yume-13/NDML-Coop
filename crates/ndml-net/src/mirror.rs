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

//! The generic player-side mirror: every instance a player's game shows for
//! that player, drawn on the partner's screen with no effect there.
//!
//! Wire tags 32 to 34: [`MirrorFrame`] (unreliable, 30 Hz: moving parts),
//! [`MirrorLooks`] (reliable: rarely changing parts) and [`MirrorLeaders`]
//! (reliable: what an instance follows). Keys are the owner's 16-bit counters.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use crate::msg::PeerId;
use crate::packet::MAX_MESSAGE;

/// One mirrored instance's moving parts, quantized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorEntry {
    /// The owner's key for the instance.
    pub key: u16,
    /// Position in half pixels (±16,383 px).
    pub x: i16,
    pub y: i16,
    /// `image_angle`, 65,536 steps per turn.
    pub angle: u16,
    /// `floor(image_index)` modulo 256.
    pub frame: u8,
    /// `image_xscale` / `image_yscale` in 1/256 (±128).
    pub xscale: i16,
    pub yscale: i16,
    /// `image_alpha` × 255.
    pub alpha: u8,
    /// The owner's segment ([`Segments`](crate::interp::Segments)).
    pub seg: u8,
}

/// Worst-case encoded size of an entry (postcard varints).
pub const ENTRY_MAX_BYTES: usize = 3 + 3 + 3 + 3 + 1 + 3 + 3 + 1 + 1;

impl MirrorEntry {
    #[must_use]
    #[allow(clippy::too_many_arguments)] // one argument per drawn field
    pub fn quantize(
        key: u16,
        x: f64,
        y: f64,
        angle: f64,
        frame: f64,
        xscale: f64,
        yscale: f64,
        alpha: f64,
    ) -> Self {
        let half = |v: f64| (v * 2.0).round().clamp(-32767.0, 32767.0) as i16;
        let scale = |v: f64| (v * 256.0).round().clamp(-32767.0, 32767.0) as i16;
        let turn = angle.rem_euclid(360.0) / 360.0 * 65536.0;
        Self {
            key,
            x: half(x),
            y: half(y),
            angle: (turn.round() as u32 % 65536) as u16,
            frame: (frame.max(0.0).floor() as u64 % 256) as u8,
            xscale: scale(xscale),
            yscale: scale(yscale),
            alpha: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
            seg: 0,
        }
    }

    #[must_use]
    pub fn x(&self) -> f64 {
        f64::from(self.x) / 2.0
    }
    #[must_use]
    pub fn y(&self) -> f64 {
        f64::from(self.y) / 2.0
    }
    #[must_use]
    pub fn angle_deg(&self) -> f64 {
        f64::from(self.angle) / 65536.0 * 360.0
    }
    #[must_use]
    pub fn xscale(&self) -> f64 {
        f64::from(self.xscale) / 256.0
    }
    #[must_use]
    pub fn yscale(&self) -> f64 {
        f64::from(self.yscale) / 256.0
    }
    #[must_use]
    pub fn alpha(&self) -> f64 {
        f64::from(self.alpha) / 255.0
    }
}

/// One part of a mirror frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorFrame {
    /// Whose instances.
    pub peer: PeerId,
    /// The owner's clock, ms.
    pub time_ms: u32,
    pub part: u8,
    pub parts: u8,
    pub entries: Vec<MirrorEntry>,
    /// Leaders of this part's keys that changed now or just before (also sent
    /// reliably): a pose and the leader it is drawn on arrive together.
    pub leaders: Vec<MirrorLeader>,
    /// The owner's player-side sounds, in the first part only.
    pub sounds: Option<FrameSounds>,
}

/// The sounds riding a mirror frame, played when it is drawn. Assets are the
/// game's sound indices; gain (without `sfxVolume`) and pitch in 1/64.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FrameSounds {
    /// The owner wants none of the partner's own sounds: no `own` list comes.
    pub deaf: bool,
    /// What the owner's own things played, played at the partner volume.
    pub own: SoundList,
    /// The host only: what its world played, played at full volume.
    pub world: SoundList,
    /// The host only: its music calls from code the joining game skips.
    pub music: Option<MusicMarks>,
}

/// The host's music calls from code a joining game skips, each kind as the
/// number of its newest call: the joining game makes each call it has not
/// seen, in order, with its own music settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MusicMarks {
    /// The number of the newest call of any kind.
    pub newest: u8,
    /// `bossMusicEnd`.
    pub boss_end: u8,
    /// A `fadeBGM` that starts no track: its gain (1/64) and time (ms).
    pub fade: u8,
    pub fade_gain: u8,
    pub fade_ms: u16,
    /// A track started (`playBGM` or `playBGMLoop`), with its fade.
    pub start: u8,
    pub start_track: u16,
    pub start_looped: bool,
    pub start_fade: Option<(u8, u16)>,
}

/// One-shots and loops of one kind.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SoundList {
    /// The owner's count of one-shots up to the last in `shots`; the viewer
    /// plays only numbers it has not played.
    pub seq: u32,
    /// The newest one-shots, oldest first.
    pub shots: Vec<FrameSound>,
    /// Every loop of this kind playing now (the loudest [`LOOPS_PER_FRAME`]);
    /// `None`: the set the viewer has stands.
    pub loops: Option<Vec<FrameSound>>,
}

impl SoundList {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shots.is_empty() && self.loops.is_none()
    }
}

/// One sound in a [`SoundList`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSound {
    /// The sound asset; [`FrameSound::STOP_FIRST`]: the asset's other
    /// instances were stopped first.
    pub asset: u16,
    pub gain: u8,
    pub pitch: u8,
}

/// [`FrameSound`] gain and pitch of 1.0.
pub const GAIN_ONE: u8 = 64;
/// Own one-shots a frame carries.
pub const SHOTS_PER_FRAME: usize = 8;
/// World one-shots a frame carries (a serpent's links die together).
pub const WORLD_SHOTS_PER_FRAME: usize = 24;
/// Loops of each kind a frame carries.
pub const LOOPS_PER_FRAME: usize = 8;
/// Worst-case encoded size of a [`FrameSounds`].
pub const SOUNDS_MAX_BYTES: usize = 1
    + 1
    + 2 * (5 + 1 + 1 + 1)
    + (SHOTS_PER_FRAME + WORLD_SHOTS_PER_FRAME + 2 * LOOPS_PER_FRAME) * 5
    + (1 + 1 + 3 + 3 + 1 + 3 + 1 + 1 + 1 + 3);

impl FrameSound {
    /// The flag bit in `asset` (sound indices stay below it).
    pub const STOP_FIRST: u16 = 0x8000;

    /// A gain or pitch in 1/64 (0 to 255/64).
    #[must_use]
    pub fn step(v: f64) -> u8 {
        if v.is_nan() {
            return 0;
        }
        (v * f64::from(GAIN_ONE)).round().clamp(0.0, 255.0) as u8
    }

    #[must_use]
    pub fn value(step: u8) -> f64 {
        f64::from(step) / f64::from(GAIN_ONE)
    }

    #[must_use]
    pub fn index(&self) -> u16 {
        self.asset & !Self::STOP_FIRST
    }

    /// Whether the owner stopped the asset's other instances first.
    #[must_use]
    pub fn stop_first(&self) -> bool {
        self.asset & Self::STOP_FIRST != 0
    }
}

/// Entries per part, so a part always fits one datagram.
pub const ENTRIES_PER_PART: usize = (MAX_MESSAGE - 16 - 1) / ENTRY_MAX_BYTES;

/// The room `sounds` takes in the first part beyond the one byte of `None`.
#[must_use]
pub fn sounds_room(sounds: Option<&FrameSounds>) -> usize {
    sounds.map_or(0, |s| {
        postcard::to_allocvec(&Some(s)).map_or(SOUNDS_MAX_BYTES, |b| b.len().saturating_sub(1))
    })
}

impl MirrorFrame {
    /// Split one frame into parts (at least one), each with the leaders of
    /// its keys; other leaders and `sounds` go with the first part.
    #[must_use]
    pub fn split(
        peer: PeerId,
        time_ms: u32,
        entries: &[MirrorEntry],
        leaders: &[MirrorLeader],
        sounds: Option<FrameSounds>,
    ) -> Vec<Self> {
        let mut of_key: std::collections::HashMap<u16, Vec<MirrorLeader>> =
            std::collections::HashMap::new();
        for l in leaders {
            of_key.entry(l.key).or_default().push(*l);
        }
        // The part budget; the first part's less the sounds' room.
        let budget = ENTRIES_PER_PART * ENTRY_MAX_BYTES;
        let first_budget = budget.saturating_sub(sounds_room(sounds.as_ref()));
        let mut parts: Vec<(Vec<MirrorEntry>, Vec<MirrorLeader>)> = vec![(Vec::new(), Vec::new())];
        let mut size = 0;
        for e in entries {
            let ls = of_key.remove(&e.key).unwrap_or_default();
            let n = ENTRY_MAX_BYTES + ls.len() * LEADER_MAX_BYTES;
            let cap = if parts.len() == 1 {
                first_budget
            } else {
                budget
            };
            if size + n > cap && !parts.last().is_some_and(|p| p.0.is_empty()) {
                parts.push((Vec::new(), Vec::new()));
                size = 0;
            }
            size += n;
            if let Some(p) = parts.last_mut() {
                p.0.push(*e);
                p.1.extend(ls);
            }
        }
        // Leaders of keys not in this frame: the first part, as room allows.
        let mut rest: Vec<MirrorLeader> = of_key.into_values().flatten().collect();
        rest.sort_by_key(|l| l.key);
        let first_size: usize =
            parts[0].0.len() * ENTRY_MAX_BYTES + parts[0].1.len() * LEADER_MAX_BYTES;
        let room = first_budget.saturating_sub(first_size) / LEADER_MAX_BYTES;
        rest.truncate(room);
        parts[0].1.extend(rest);
        let n = parts.len().min(255) as u8;
        let mut sounds = sounds;
        parts
            .into_iter()
            .take(255)
            .enumerate()
            .map(|(i, (entries, leaders))| Self {
                peer,
                time_ms,
                part: i as u8,
                parts: n,
                entries,
                leaders,
                sounds: sounds.take(),
            })
            .collect()
    }
}

/// The rarely changing parts of one mirrored instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorLook {
    pub key: u16,
    /// Object index (the owner's game; for statistics and custom drawing).
    pub object: u16,
    /// `sprite_index` (-1 = none).
    pub sprite: i32,
    /// `depth`, rounded.
    pub depth: i32,
    /// `image_blend` (`0xBBGGRR`).
    pub blend: u32,
    pub visible: bool,
}

/// Looks that changed (reliable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorLooks {
    pub peer: PeerId,
    pub looks: Vec<MirrorLook>,
}

/// Worst-case encoded size of a look.
pub const LOOK_MAX_BYTES: usize = 3 + 3 + 5 + 5 + 5 + 1;
/// Looks per message.
pub const LOOKS_PER_MESSAGE: usize = (MAX_MESSAGE - 16) / LOOK_MAX_BYTES;

impl MirrorLooks {
    /// Split into messages that fit one datagram each.
    #[must_use]
    pub fn split(peer: PeerId, looks: &[MirrorLook]) -> Vec<Self> {
        looks
            .chunks(LOOKS_PER_MESSAGE)
            .map(|c| Self {
                peer,
                looks: c.to_vec(),
            })
            .collect()
    }
}

/// The mirror's own drawing (wire tag 35): an instance whose object has Draw
/// code is drawn on the viewer by that code run on its proxy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MirrorDraw {
    /// Reliable: objects whose instances run Draw code, with the object
    /// whose code runs (itself or an ancestor).
    Drawn {
        peer: PeerId,
        objects: Vec<(u16, u16)>,
    },
    /// Reliable: the variables `object`'s Draw code reads, in slot order (new
    /// ones at the end); `safe = false`: not run on the viewer; `settled`:
    /// its trials are done.
    Names {
        peer: PeerId,
        object: u16,
        safe: bool,
        names: Vec<String>,
        settled: bool,
    },
    /// Unreliable: changed values of those variables per key, all of them
    /// every second, read at `time_ms` on the owner's clock.
    Vars {
        peer: PeerId,
        part: u8,
        time_ms: u32,
        keys: Vec<KeyVars>,
    },
    /// Reliable, on change: other objects' variables some Draw code reads, as
    /// the owner's game has them; set around that code's run on a proxy.
    Reads {
        peer: PeerId,
        vals: Vec<(String, crate::msg::StateValue)>,
    },
    /// Unreliable, on no stream, in place of `Vars`: each entry also says how
    /// long before `time_ms` its key was last sent (0: never). A viewer that
    /// did not apply that send drops the key's values until each comes again.
    Ticks {
        peer: PeerId,
        part: u8,
        time_ms: u32,
        keys: Vec<(u32, KeyVars)>,
    },
    /// Unreliable, on no stream, in place of `Ticks`: numbers in their smallest
    /// exact encoding and arrays as an
    /// [`ArrayEdit`](crate::mirror_vars::ArrayEdit) of the one sent before.
    Edits {
        peer: PeerId,
        part: u8,
        time_ms: u32,
        keys: Vec<(u32, KeyVars<crate::mirror_vars::ArrayEdit>)>,
    },
}

/// One instance's draw variables: (slot, value), and array changes (`D`). A
/// key too large for one part goes as `pieces` entries of one `time_ms`,
/// applied only once every piece is in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyVars<D = ArrayDelta> {
    pub key: u16,
    pub piece: u8,
    pub pieces: u8,
    pub vals: Vec<(u8, crate::msg::StateValue)>,
    pub deltas: Vec<D>,
}

impl<D> KeyVars<D> {
    /// A key's values in one piece.
    #[must_use]
    pub fn whole(key: u16, vals: Vec<(u8, crate::msg::StateValue)>, deltas: Vec<D>) -> Self {
        Self {
            key,
            piece: 0,
            pieces: 1,
            vals,
            deltas,
        }
    }

    /// The same values with each array change converted.
    #[must_use]
    pub fn map_deltas<E>(self, f: impl FnMut(D) -> E) -> KeyVars<E> {
        KeyVars {
            key: self.key,
            piece: self.piece,
            pieces: self.pieces,
            vals: self.vals,
            deltas: self.deltas.into_iter().map(f).collect(),
        }
    }
}

/// An array as a change to the one sent before it (`from_hash`): without its
/// first `drop` elements, `patches` applied, `tail` appended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArrayDelta {
    pub slot: u8,
    pub from_hash: u32,
    /// Length of the new array.
    pub len: u16,
    pub drop: u16,
    pub patches: Vec<(u16, crate::msg::StateValue)>,
    pub tail: Vec<crate::msg::StateValue>,
}

type Patches = Vec<(u16, crate::msg::StateValue)>;

/// Elements a delta may drop from the front.
const DELTA_MAX_DROP: usize = 16;

/// FNV-1a over the array's encoding.
#[must_use]
pub fn array_hash(items: &[crate::msg::StateValue]) -> u32 {
    let bytes = postcard::to_allocvec(items).unwrap_or_default();
    bytes.iter().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(*b)).wrapping_mul(0x0100_0193)
    })
}

impl ArrayDelta {
    /// The change from `old` to `new`, when it is smaller than `new` itself.
    #[must_use]
    pub fn between(
        slot: u8,
        old: &[crate::msg::StateValue],
        new: &[crate::msg::StateValue],
    ) -> Option<Self> {
        let mut best: Option<(usize, usize, Patches)> = None;
        for drop in 0..=old.len().min(DELTA_MAX_DROP) {
            let kept = (old.len() - drop).min(new.len());
            let patches: Patches = (0..kept)
                .filter(|&i| old[drop + i] != new[i])
                .map(|i| (u16::try_from(i).unwrap_or(u16::MAX), new[i].clone()))
                .collect();
            let cost = patches.len() + new.len() - kept;
            if best.as_ref().is_none_or(|(c, _, _)| cost < *c) && cost * 2 < new.len().max(1) {
                best = Some((cost, drop, patches));
                if cost <= 1 {
                    break;
                }
            }
        }
        let (_, drop, patches) = best?;
        let kept = (old.len() - drop).min(new.len());
        Some(Self {
            slot,
            from_hash: array_hash(old),
            len: u16::try_from(new.len()).ok()?,
            drop: u16::try_from(drop).ok()?,
            patches,
            tail: new[kept..].to_vec(),
        })
    }

    /// The new array, if `old` is the one the delta was made from.
    #[must_use]
    pub fn apply(&self, old: &[crate::msg::StateValue]) -> Option<Vec<crate::msg::StateValue>> {
        if array_hash(old) != self.from_hash || usize::from(self.drop) > old.len() {
            return None;
        }
        let mut v = old[usize::from(self.drop)..].to_vec();
        v.truncate(usize::from(self.len));
        for (i, val) in &self.patches {
            *v.get_mut(usize::from(*i))? = val.clone();
        }
        v.extend(self.tail.iter().cloned());
        (v.len() == usize::from(self.len)).then_some(v)
    }
}

/// Bytes of `Vars` content per message part.
pub const VARS_PART_BYTES: usize = MAX_MESSAGE - 64;

impl MirrorDraw {
    /// Whose instances.
    #[must_use]
    pub fn peer(&self) -> PeerId {
        match self {
            Self::Drawn { peer, .. }
            | Self::Names { peer, .. }
            | Self::Vars { peer, .. }
            | Self::Reads { peer, .. }
            | Self::Ticks { peer, .. }
            | Self::Edits { peer, .. } => *peer,
        }
    }

    /// Split per-key values into `Ticks` parts; see [`split_keys`].
    #[must_use]
    pub fn split_ticks(
        peer: PeerId,
        time_ms: u32,
        keys: Vec<(u32, KeyVars)>,
    ) -> (Vec<Self>, usize) {
        let (parts, dropped) = split_keys(keys);
        let parts = parts
            .into_iter()
            .enumerate()
            .map(|(i, keys)| Self::Ticks {
                peer,
                part: i as u8,
                time_ms,
                keys,
            })
            .collect();
        (parts, dropped)
    }

    /// Split per-key values into `Edits` parts; see [`split_keys`].
    #[must_use]
    pub fn split_edits(
        peer: PeerId,
        time_ms: u32,
        keys: Vec<(u32, KeyVars<crate::mirror_vars::ArrayEdit>)>,
    ) -> (Vec<Self>, usize) {
        let (parts, dropped) = split_keys(keys);
        let parts = parts
            .into_iter()
            .enumerate()
            .map(|(i, keys)| Self::Edits {
                peer,
                part: i as u8,
                time_ms,
                keys,
            })
            .collect();
        (parts, dropped)
    }
}

/// Per-key values split into parts, each key with its time since last sent.
pub type Parts<D> = Vec<Vec<(u32, KeyVars<D>)>>;

/// Per-key values as at most 255 parts that fit a datagram each; a key too
/// large for one part goes as numbered pieces. Returns the parts and the
/// number of values, pieces or keys left out.
#[must_use]
pub fn split_keys<D: Serialize + Clone>(keys: Vec<(u32, KeyVars<D>)>) -> (Parts<D>, usize) {
    // The time since goes beside each piece: a varint of at most 5 bytes.
    let size_of =
        |k: &KeyVars<D>| postcard::to_allocvec(k).map_or(usize::MAX, |b| b.len().saturating_add(5));
    let mut out = Vec::new();
    let mut cur: Vec<(u32, KeyVars<D>)> = Vec::new();
    let mut size = 0;
    let mut dropped = 0;
    let mut pieces: Vec<(u32, KeyVars<D>)> = Vec::new();
    for (after, k) in keys {
        if size_of(&k) <= VARS_PART_BYTES {
            pieces.push((after, k));
            continue;
        }
        let key = k.key;
        let first = pieces.len();
        let mut piece = KeyVars::whole(key, Vec::new(), Vec::new());
        let singles = k
            .vals
            .into_iter()
            .map(|v| KeyVars::whole(key, vec![v], Vec::new()))
            .chain(
                k.deltas
                    .into_iter()
                    .map(|d| KeyVars::whole(key, Vec::new(), vec![d])),
            );
        for one in singles {
            if size_of(&one) > VARS_PART_BYTES {
                dropped += 1;
                continue;
            }
            let mut grown = piece.clone();
            grown.vals.extend(one.vals.iter().cloned());
            grown.deltas.extend(one.deltas.iter().cloned());
            if size_of(&grown) > VARS_PART_BYTES {
                pieces.push((after, std::mem::replace(&mut piece, one)));
            } else {
                piece = grown;
            }
        }
        if !piece.vals.is_empty() || !piece.deltas.is_empty() {
            pieces.push((after, piece));
        }
        let Ok(n) = u8::try_from(pieces.len() - first) else {
            dropped += pieces.len() - first;
            pieces.truncate(first);
            continue;
        };
        for (i, (_, p)) in pieces[first..].iter_mut().enumerate() {
            p.piece = i as u8;
            p.pieces = n;
        }
    }
    for k in pieces {
        let n = size_of(&k.1);
        if size + n > VARS_PART_BYTES && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            size = 0;
        }
        size += n;
        cur.push(k);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    dropped += out.iter().skip(255).map(Vec::len).sum::<usize>();
    out.truncate(255);
    (out, dropped)
}

/// What a mirrored instance follows: a replicated enemy by net id, and the
/// offset from it (half pixels). The viewer draws the instance on its own
/// copy of that enemy, and not at all while it has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorLeader {
    pub key: u16,
    /// `None`: follows nothing replicated (drawn where the owner has it).
    pub net: Option<u16>,
    pub dx: i16,
    pub dy: i16,
}

impl MirrorLeader {
    /// Offset in half pixels, clamped.
    #[must_use]
    pub fn new(key: u16, net: Option<u16>, dx: f64, dy: f64) -> Self {
        let q = |v: f64| (v * 2.0).round().clamp(-32767.0, 32767.0) as i16;
        Self {
            key,
            net,
            dx: q(dx),
            dy: q(dy),
        }
    }

    /// The offset in pixels.
    #[must_use]
    pub fn offset(&self) -> (f64, f64) {
        (f64::from(self.dx) / 2.0, f64::from(self.dy) / 2.0)
    }
}

/// Leaders that changed (reliable, wire tag 34, sent before the frame's looks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MirrorLeaders {
    pub peer: PeerId,
    pub leaders: Vec<MirrorLeader>,
    /// The owner frame they changed at (its clock, ms).
    pub time_ms: u32,
}

/// Worst-case encoded size of a leader.
pub const LEADER_MAX_BYTES: usize = 3 + 4 + 3 + 3;
/// Leaders per message.
pub const LEADERS_PER_MESSAGE: usize = (MAX_MESSAGE - 16) / LEADER_MAX_BYTES;

impl MirrorLeaders {
    /// Split into messages that fit one datagram each.
    #[must_use]
    pub fn split(peer: PeerId, time_ms: u32, leaders: &[MirrorLeader]) -> Vec<Self> {
        leaders
            .chunks(LEADERS_PER_MESSAGE)
            .map(|c| Self {
                peer,
                leaders: c.to_vec(),
                time_ms,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::Message;

    const WORST_MUSIC: MusicMarks = MusicMarks {
        newest: u8::MAX,
        boss_end: u8::MAX,
        fade: u8::MAX,
        fade_gain: u8::MAX,
        fade_ms: u16::MAX,
        start: u8::MAX,
        start_track: u16::MAX,
        start_looped: true,
        start_fade: Some((u8::MAX, u16::MAX)),
    };

    #[test]
    fn sounds_ride_the_first_part_and_round_trip() {
        let e = MirrorEntry::quantize(1, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0);
        // A frame without sounds: `None` takes one byte.
        let mut f = MirrorFrame::split(PeerId(1), 77, &[e], &[], None).remove(0);
        assert_eq!(f.sounds, None);
        let none = Message::MirrorFrame(f.clone()).encode().unwrap();
        assert_eq!(
            Message::decode(&none).unwrap(),
            Message::MirrorFrame(f.clone())
        );
        // Worst case: still one datagram.
        let s = FrameSound {
            asset: u16::MAX,
            gain: 255,
            pitch: 255,
        };
        f.sounds = Some(FrameSounds {
            deaf: true,
            own: SoundList {
                seq: u32::MAX,
                shots: vec![s; SHOTS_PER_FRAME],
                loops: Some(vec![s; LOOPS_PER_FRAME]),
            },
            world: SoundList {
                seq: u32::MAX,
                shots: vec![s; WORLD_SHOTS_PER_FRAME],
                loops: Some(vec![s; LOOPS_PER_FRAME]),
            },
            music: Some(WORST_MUSIC),
        });
        let bytes = Message::MirrorFrame(f.clone()).encode().unwrap();
        assert_eq!(
            Message::decode(&bytes).unwrap(),
            Message::MirrorFrame(f.clone())
        );
        assert!(bytes.len() - none.len() <= SOUNDS_MAX_BYTES);
        let worst = MirrorEntry {
            seg: 255,
            key: u16::MAX,
            x: i16::MIN + 1,
            y: i16::MAX,
            angle: u16::MAX,
            frame: 255,
            xscale: i16::MIN + 1,
            yscale: i16::MAX,
            alpha: 255,
        };
        // With the worst sounds the first part makes room for them.
        let full = MirrorFrame::split(
            PeerId(255),
            u32::MAX,
            &vec![worst; ENTRIES_PER_PART],
            &[],
            None,
        );
        assert_eq!(full.len(), 1);
        assert!(Message::MirrorFrame(full[0].clone()).encode().is_ok());
        let with = MirrorFrame::split(
            PeerId(255),
            u32::MAX,
            &vec![worst; ENTRIES_PER_PART],
            &[],
            f.sounds.clone(),
        );
        assert_eq!(with.len(), 2);
        assert_eq!(with[0].sounds, f.sounds);
        for p in &with {
            assert!(Message::MirrorFrame(p.clone()).encode().is_ok());
        }
        // Parts after the first carry no sounds and keep their whole room.
        let many = MirrorFrame::split(
            PeerId(2),
            5,
            &vec![worst; ENTRIES_PER_PART * 2],
            &[],
            f.sounds.clone(),
        );
        assert!(many[1..].iter().all(|p| p.sounds.is_none()));
        assert_eq!(many[1].entries.len(), ENTRIES_PER_PART);
        // A deaf-only frame's sounds take less than one entry's room.
        let quiet = FrameSounds::default();
        assert!(sounds_room(Some(&quiet)) < ENTRY_MAX_BYTES);
        let one = MirrorFrame::split(
            PeerId(2),
            5,
            &vec![worst; ENTRIES_PER_PART - 1],
            &[],
            Some(quiet),
        );
        assert_eq!(one.len(), 1);
        assert!(Message::MirrorFrame(one[0].clone()).encode().is_ok());
    }

    #[test]
    fn a_first_part_full_of_sounds_and_carried_leaders_is_one_datagram() {
        let s = FrameSound {
            asset: u16::MAX,
            gain: 255,
            pitch: 255,
        };
        let sounds = FrameSounds {
            deaf: true,
            own: SoundList {
                seq: u32::MAX,
                shots: vec![s; SHOTS_PER_FRAME],
                loops: Some(vec![s; LOOPS_PER_FRAME]),
            },
            world: SoundList {
                seq: u32::MAX,
                shots: vec![s; WORLD_SHOTS_PER_FRAME],
                loops: Some(vec![s; LOOPS_PER_FRAME]),
            },
            music: Some(WORST_MUSIC),
        };
        let e = MirrorEntry {
            seg: 255,
            key: u16::MAX,
            x: i16::MIN + 1,
            y: i16::MAX,
            angle: u16::MAX,
            frame: 255,
            xscale: i16::MIN + 1,
            yscale: i16::MAX,
            alpha: 255,
        };
        let keyed: Vec<MirrorEntry> = (0..ENTRIES_PER_PART * 2)
            .map(|i| MirrorEntry {
                key: u16::MAX - i as u16,
                ..e
            })
            .collect();
        // Leaders of keys in the frame and of keys gone.
        let riders: Vec<MirrorLeader> = keyed
            .iter()
            .map(|e| MirrorLeader::new(e.key, Some(u16::MAX), -1e9, 1e9))
            .chain((0..40).map(|k| MirrorLeader::new(k, Some(u16::MAX), -1e9, 1e9)))
            .collect();
        let parts = MirrorFrame::split(PeerId(3), u32::MAX, &keyed, &riders, Some(sounds.clone()));
        assert_eq!(parts[0].sounds.as_ref(), Some(&sounds));
        for p in &parts {
            let bytes = Message::MirrorFrame(p.clone()).encode().unwrap();
            assert!(
                bytes.len() <= MAX_MESSAGE,
                "{} > {MAX_MESSAGE}",
                bytes.len()
            );
        }
    }

    #[test]
    fn a_frame_of_the_old_layout_is_refused_cleanly() {
        // A layout without `sounds`: the same fields, nothing after.
        #[derive(Serialize)]
        struct OldFrame {
            peer: PeerId,
            time_ms: u32,
            part: u8,
            parts: u8,
            entries: Vec<MirrorEntry>,
            leaders: Vec<MirrorLeader>,
        }
        let old = OldFrame {
            peer: PeerId(1),
            time_ms: 9,
            part: 0,
            parts: 1,
            entries: Vec::new(),
            leaders: Vec::new(),
        };
        let new = MirrorFrame::split(PeerId(1), 9, &[], &[], None).remove(0);
        let new_bytes = Message::MirrorFrame(new).encode().unwrap();
        let tag_len = new_bytes.len() - postcard::to_allocvec(&old).unwrap().len() - 1;
        let mut old_bytes = new_bytes[..tag_len].to_vec();
        old_bytes.extend(postcard::to_allocvec(&old).unwrap());
        assert!(
            Message::decode(&old_bytes).is_err(),
            "an old frame must not decode"
        );
        // A new frame in an old decoder: trailing bytes.
        let mut extra = new_bytes.clone();
        extra.push(0);
        assert!(Message::decode(&extra).is_err());
    }

    #[test]
    fn steps_clamp() {
        assert_eq!(FrameSound::step(1.0), GAIN_ONE);
        assert_eq!(FrameSound::step(-1.0), 0);
        assert_eq!(FrameSound::step(100.0), 255);
        assert_eq!(FrameSound::step(f64::NAN), 0);
        assert!((FrameSound::value(32) - 0.5).abs() < 1e-12);
        let f = FrameSound {
            asset: 0x01bc | FrameSound::STOP_FIRST, // sound index 444
            gain: 0,
            pitch: 0,
        };
        assert!(f.stop_first() && f.index() == 444);
    }

    #[test]
    fn quantize_round_trips_within_a_step() {
        let e = MirrorEntry::quantize(7, 1234.3, -56.8, 359.99, 3.7, 0.4, -1.5, 0.5);
        assert!((e.x() - 1234.5).abs() < 0.26);
        assert!((e.y() + 56.8).abs() < 0.26);
        assert!(e.angle_deg() < 0.01 || e.angle_deg() > 359.98);
        assert_eq!(e.frame, 3);
        assert!((e.xscale() - 0.4).abs() < 0.004 && (e.yscale() + 1.5).abs() < 0.004);
        assert_eq!(e.alpha, 128);
        let far = MirrorEntry::quantize(0, 1e9, -1e9, -90.0, -1.0, 1e9, 0.0, 9.0);
        assert_eq!(
            (far.x, far.y, far.frame, far.alpha),
            (32767, -32767, 0, 255)
        );
        assert!((far.angle_deg() - 270.0).abs() < 0.01);
    }

    #[test]
    fn worst_case_parts_fit_a_datagram() {
        let e = MirrorEntry {
            seg: 0,
            key: u16::MAX,
            x: i16::MIN + 1,
            y: i16::MAX,
            angle: u16::MAX,
            frame: 255,
            xscale: i16::MIN + 1,
            yscale: i16::MAX,
            alpha: 255,
        };
        let entries = vec![e; ENTRIES_PER_PART * 2 + 3];
        let parts = MirrorFrame::split(PeerId(3), u32::MAX, &entries, &[], None);
        assert_eq!(parts.len(), 3);
        for p in parts {
            assert!(Message::MirrorFrame(p).encode().is_ok());
        }
        // Every entry with a worst-case leader: each leader in its key's part.
        let keyed: Vec<MirrorEntry> = (0..ENTRIES_PER_PART * 2)
            .map(|i| MirrorEntry {
                key: u16::MAX - i as u16,
                ..e
            })
            .collect();
        let riders: Vec<MirrorLeader> = keyed
            .iter()
            .map(|e| MirrorLeader::new(e.key, Some(u16::MAX), -1e9, 1e9))
            .chain([MirrorLeader::new(5, None, 0.0, 0.0)])
            .collect();
        let parts = MirrorFrame::split(PeerId(3), u32::MAX, &keyed, &riders, None);
        assert!(parts.len() > 2);
        assert_eq!(
            parts.iter().map(|p| p.leaders.len()).sum::<usize>(),
            riders.len()
        );
        for p in &parts {
            for l in &p.leaders {
                assert!(l.key == 5 || p.entries.iter().any(|e| e.key == l.key));
            }
            assert!(Message::MirrorFrame(p.clone()).encode().is_ok());
        }
        assert!(parts[0].leaders.iter().any(|l| l.key == 5));
        let look = MirrorLook {
            key: u16::MAX,
            object: u16::MAX,
            sprite: i32::MIN,
            depth: i32::MIN,
            blend: u32::MAX,
            visible: true,
        };
        let looks = vec![look; LOOKS_PER_MESSAGE + 1];
        let msgs = MirrorLooks::split(PeerId(1), &looks);
        assert_eq!(msgs.len(), 2);
        for m in msgs {
            assert!(Message::MirrorLooks(m).encode().is_ok());
        }
        assert_eq!(MirrorFrame::split(PeerId(0), 5, &[], &[], None).len(), 1);
        let leader = MirrorLeader::new(u16::MAX, Some(u16::MAX), -1e9, 1e9);
        assert_eq!((leader.dx, leader.dy), (-32767, 32767));
        let leaders = vec![leader; LEADERS_PER_MESSAGE + 1];
        let msgs = MirrorLeaders::split(PeerId(1), u32::MAX, &leaders);
        assert_eq!(msgs.len(), 2);
        for m in msgs {
            assert!(Message::MirrorLeaders(m).encode().is_ok());
        }
        assert_eq!(
            MirrorLeader::new(1, None, 3.26, -0.74).offset(),
            (3.5, -0.5)
        );
    }

    /// Parts past 255 are left out and their keys counted as dropped.
    #[test]
    fn vars_parts_past_255_are_counted_as_dropped() {
        use crate::msg::StateValue;
        // A real is 9 bytes: just over half a part each, so one key a part.
        let reals = VARS_PART_BYTES / 2 / 9 + 1;
        let keys: Vec<(u32, KeyVars)> = (0..300u16)
            .map(|k| {
                let v = vec![(0, StateValue::Array(vec![StateValue::Real(1.5); reals]))];
                (0, KeyVars::whole(k, v, Vec::new()))
            })
            .collect();
        let (parts, dropped) = split_keys(keys);
        assert!(parts.iter().all(|p| p.len() == 1));
        assert_eq!(parts.len(), 255);
        assert_eq!(dropped, 300 - 255);
    }

    #[test]
    fn vars_parts_fit_a_datagram_and_drop_only_what_cannot() {
        use crate::msg::StateValue;
        let trail = |key| {
            KeyVars::whole(
                key,
                vec![
                    (0, StateValue::Array(vec![StateValue::Real(1234.567); 40])),
                    (1, StateValue::Str("x".repeat(20))),
                ],
                Vec::new(),
            )
        };
        let huge = KeyVars::whole(
            9,
            vec![(0, StateValue::Array(vec![StateValue::Real(1.5); 400]))],
            Vec::new(),
        );
        let mut keys: Vec<KeyVars> = (0..12).map(trail).collect();
        keys.push(huge);
        // Too large as a whole, each slot fits: split across parts.
        keys.push(KeyVars::whole(
            20,
            (0..4)
                .map(|s| (s, StateValue::Array(vec![StateValue::Real(2.5); 90])))
                .collect(),
            Vec::new(),
        ));
        let keys = keys.into_iter().map(|k| (u32::from(k.key), k)).collect();
        let (parts, dropped) = MirrorDraw::split_ticks(PeerId(1), 777, keys);
        assert_eq!(dropped, 1);
        assert!(parts.len() > 1);
        let mut seen = 0;
        let mut pieces_of_20 = Vec::new();
        for (i, p) in parts.into_iter().enumerate() {
            if let MirrorDraw::Ticks {
                part,
                keys,
                time_ms,
                ..
            } = &p
            {
                assert_eq!(usize::from(*part), i);
                assert_eq!(*time_ms, 777);
                seen += keys.len();
                for (after, k) in keys {
                    // Every piece says when its key was last sent.
                    assert_eq!(*after, u32::from(k.key));
                    if k.key == 20 {
                        pieces_of_20.push((k.piece, k.pieces));
                    } else {
                        assert_eq!((k.piece, k.pieces), (0, 1));
                    }
                }
            }
            let m = Message::MirrorDraw(p);
            assert!(m.encode().is_ok());
            assert_eq!(m.channel(), crate::msg::Channel::Unreliable);
            // Ordered by the viewer, not newest-wins.
            assert_eq!(m.stream(), None);
        }
        assert!(seen >= 14, "{seen}");
        // Many small keys: still a datagram per part.
        let small = (0..600u16)
            .map(|k| {
                let v = vec![(0, StateValue::Real(1.5))];
                (u32::MAX, KeyVars::whole(k, v, Vec::new()))
            })
            .collect();
        let (parts, dropped) = MirrorDraw::split_ticks(PeerId(1), 777, small);
        assert_eq!(dropped, 0);
        for p in parts {
            assert!(Message::MirrorDraw(p).encode().is_ok());
        }
        // Key 20's pieces: numbered, each knowing how many there are.
        let n = pieces_of_20.len();
        assert!(n > 1);
        let want: Vec<(u8, u8)> = (0..n).map(|i| (i as u8, n as u8)).collect();
        assert_eq!(pieces_of_20, want);
        let names = Message::MirrorDraw(MirrorDraw::Names {
            peer: PeerId(0),
            object: 3,
            safe: true,
            names: vec!["blend".into()],
            settled: true,
        });
        assert_eq!(names.channel(), crate::msg::Channel::Reliable);
        assert_eq!(names.encode().unwrap()[0], 35);
    }

    #[test]
    fn a_trail_array_goes_as_its_change() {
        use crate::msg::StateValue;
        let pts = |from: u32, n: u32| -> Vec<StateValue> {
            (from..from + n)
                .map(|i| StateValue::Real(f64::from(i) * 1.5))
                .collect()
        };
        // Oldest point gone, a new one at the end, the newest one moved.
        let old = pts(0, 40);
        let mut new = pts(1, 40);
        new[20] = StateValue::Real(-1.0);
        let d = ArrayDelta::between(3, &old, &new).unwrap();
        assert_eq!((d.drop, d.patches.len(), d.tail.len()), (1, 1, 1));
        assert_eq!(d.apply(&old).unwrap(), new);
        // Shorter, and from an array the viewer does not hold.
        let short = pts(5, 30);
        let d = ArrayDelta::between(0, &old, &short).unwrap();
        assert_eq!(d.apply(&old).unwrap(), short);
        assert!(d.apply(&new).is_none());
        // Every element changed: no delta.
        assert!(ArrayDelta::between(0, &old, &pts(100, 40)).is_none());
        let k = KeyVars::whole(
            1,
            Vec::new(),
            vec![ArrayDelta::between(0, &old, &new).unwrap()],
        );
        let full = KeyVars::whole(1, vec![(0, StateValue::Array(new))], Vec::new());
        let size = |k: &KeyVars| postcard::to_allocvec(k).unwrap().len();
        assert!(size(&k) * 10 < size(&full), "{} {}", size(&k), size(&full));
    }
}
