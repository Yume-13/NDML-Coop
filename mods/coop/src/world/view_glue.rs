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

//! World view: the host's world-level state that is on no enemy (camera zoom,
//! background colours and instances, music), shown the same on a joining
//! game. The battlefield is the camera's view, so both games need the same
//! zoom; the background comes from `changeColors` and fades in `colorFade`.
//!
//! * **State**: the host sends [`GLOBALS`] every run frame (unreliable, newest
//!   wins); a joining game writes the targets every frame so its own camera
//!   and colour fade ease the same way, and snaps `zoomFactor` past [`SNAP`].
//! * **Calls**: the scripts in [`CALLS`] run on the host are replayed on a
//!   joining game with the same arguments and inputs ([`CALL_INPUTS`]); its
//!   own calls of them are skipped.
//! * **Scheme**: when the host's colour scheme differs from the one the
//!   joining game built its backdrop with for [`REBUILD_AFTER`] frames,
//!   `changeColors(host scheme)` runs once.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, PoisonError};

use ndml_net::msg::{Message, PeerId, StateValue, WorldCall, WorldView};
use ndml_sdk::{Builtin, Call, ModContext, Script, Value, Variable};

use super::{PartnerView, ROLE_CLIENT, ROLE_HOST, WorldHooks};

/// The world-level globals sent (the order is the wire's).
pub const GLOBALS: [&str; 17] = [
    "zoomTarget",
    "zoomBase",
    "slowZoom",
    "zoomRate",
    "zoomStiffness",
    "zoomRateLimit",
    "zoomFactor",
    "tr",
    "tg",
    "tb",
    "gtr",
    "gtg",
    "gtb",
    "backgroundGlowColor",
    "color",
    // Whether ships wrap at the battlefield's edges (`wrapPlayer`): the
    // Crucible turns it off for its fight (`oCrucible_Other_10`, back on in
    // `oCrucible_CleanUp_0`).
    "wrapEnabled",
    // The track playing (`playBGMLoop` plays `global.bgmPlaying`): sent as
    // its sound index, never written; a joining game switches to it (see
    // [`MUSIC_SETTLE`]).
    "bgmPlaying",
];
/// Index of `bgmPlaying` in [`GLOBALS`].
const BGM: usize = 16;
/// Running frames the host's track must differ from this game's before it
/// switches (the host's own switch is a call or two in one frame; the view
/// is unreliable).
pub const MUSIC_SETTLE: u64 = 30;
/// The fade the wave director gives the track it replaces (`spawnEnemyWaves`:
/// `fadeBGM(0, 3000)`, then `bgmPlaying = ...`, `playBGMLoop()`).
const MUSIC_FADE_MS: f64 = 3000.0;
/// The ending's formation: per joining player (P2, P3, P4), its ship's offset
/// from where the ending puts it, in the ship's own frame (pixels to its
/// right, pixels behind it). The host's ship keeps the game's place.
pub const FORMATION_SLOTS: [(f64, f64); 3] = [(110.0, 30.0), (-110.0, 30.0), (0.0, 140.0)];
/// The ending's step that places the ship each frame:
/// `oAchiEndingDebug_Step_0` writes `oPlayer.x/y/direction/image_angle`
/// from the sequence's `oAchiEndingPlayer` (and a `sin(current_time)`
/// wobble), absolutely, so an offset added after it never accumulates.
pub const ENDING_STEP: &str = "gml_Object_oAchiEndingDebug_Step_0";
/// The ending's start: `beginGameEnding` puts `oPlayer` at its own view's
/// centre and makes `oAchiEndingDebug`, whose step puts `oPlayer` on the
/// sequence's ship ([`ENDING_SHIP`]) once that exists.
pub const ENDING_BEGIN: &str = "gml_Script_beginGameEnding";
/// The sequence's ship, which `oAchiEndingDebug_Step_0` puts `oPlayer` on.
pub const ENDING_SHIP: &str = "oAchiEndingPlayer";

/// The ending's shift: the shifted partner's peer, dx, dy.
type EndingShift = (u16, f64, f64);

/// The offset of formation slot `slot` for a ship facing `angle` degrees
/// (GML: counter-clockwise, y down).
#[must_use]
pub fn formation_offset(slot: usize, angle: f64) -> Option<(f64, f64)> {
    let (right, behind) = *FORMATION_SLOTS.get(slot)?;
    let a = angle.to_radians();
    // Forward (cos a, -sin a); right of it (sin a, cos a).
    let (fx, fy) = (a.cos(), -a.sin());
    let (rx, ry) = (a.sin(), a.cos());
    Some((rx * right - fx * behind, ry * right - fy * behind))
}

/// Where `joining` is from `host` in the frame of a ship at `host` facing
/// `angle` degrees: pixels to its right, pixels behind it (the inverse of
/// [`formation_offset`]).
#[cfg(test)]
#[must_use]
pub fn in_ship_frame(host: (f64, f64), joining: (f64, f64), angle: f64) -> (f64, f64) {
    let a = angle.to_radians();
    let (dx, dy) = (joining.0 - host.0, joining.1 - host.1);
    (dx * a.sin() + dy * a.cos(), dy * a.sin() - dx * a.cos())
}

/// A sound's ref payload without its index: ref kind 2 (sound 231 is
/// `0x01000002000000e7`), used when this game's own `bgmPlaying` is not a
/// sound ref to take the high bits from.
const SOUND_REF: u64 = 0x0100_0002_0000_0000;
/// Index of `zoomFactor` (the eased zoom: snapped, not written every frame).
const ZOOM_FACTOR: usize = 6;
/// Index of `zoomRate`, the easing's velocity: sent, never written, so a
/// joining game's `zoomFactor` eases to the shared target with its own
/// velocity instead of keeping an offset.
const ZOOM_RATE: usize = 3;
/// Index of `color` (the scheme). Written like the targets: between changes
/// only background code, debug keys and the unsaved run data read it.
const COLOR: usize = 14;
/// `zoomFactor` is written when off by more than this fraction of the
/// host's.
pub const SNAP: f64 = 0.02;
/// `zoomFactor` is also written whatever the difference at the first view
/// state of a run and after this many frames without one (a resume, a
/// reconnect): easing from a stale value is visible.
pub const SNAP_GAP_FRAMES: u64 = 30;
/// World-level scripts replayed with their arguments: the wave director's
/// (`spawnEnemyWaves`) world-level steps. `changeColors` (the sector's
/// background); `closeBlackHoles` (`with (oBlackHole) fade = 1`: every black
/// hole shrinks and goes, at each new wave). A joining game's own wave
/// director runs on its own clock, so its own call would close the host's
/// black holes early.
pub const CALLS: [&str; 2] = ["gml_Script_changeColors", "gml_Script_closeBlackHoles"];
/// Per call in [`CALLS`]: the globals its result depends on besides its
/// arguments. `changeColors` with no argument advances the scheme from
/// `color` (the final boss's background from `wave` and
/// `dailySerpentineDrift`); `closeBlackHoles`: none.
pub const CALL_INPUTS: [&[&str]; 2] = [&["color", "wave", "dailySerpentineDrift"], &[]];
/// In a [`WorldCall`]'s arguments: after this text come the inputs, name and
/// value in turn (a joining game that does not know it would pass them on
/// as extra arguments, which the scripts ignore).
pub const INPUTS_MARK: &str = "@inputs";
/// Running frames the host's scheme must differ from the one this game built
/// with, with no host call waiting, before the backdrop is rebuilt (a call
/// on its way, reliable, would rebuild it anyway; the view is unreliable and
/// can arrive before it).
pub const REBUILD_AFTER: u64 = 60;
/// The call whose newest one is sent again at a session's start (the run's
/// own background); the others act only on what exists when they run.
const CALL_AT_START: &str = "gml_Script_changeColors";
/// The game's easing steps the host's view is written just before.
pub const EASERS: [&str; 2] = ["gml_Script_cameraZoom", "gml_Script_colorFade"];
/// The background bodies kept together with the host's ([`WorldHooks::view_bodies_host`]):
/// the planets' bodies and the final boss's lead-up coils
/// (`oStarEaterBGObject`, an `oCelestialBody` child the boss controller's
/// Step destroys at the defeat's whiteout).
const SYNCED_BODIES: [&str; 4] = [
    "oCelestialBody",
    "oCelestialBodyAdditive",
    "oCelestialBodyFlicker",
    "oStarEaterBGObject",
];
/// The pseudo-call carrying the host's body list (`sprite, x, y` each); not
/// a script, so a build that does not know it skips it.
const BODY_LIST: &str = "ndml:bodies";
/// Frames between the host's body lists.
const BODY_LIST_EVERY: u64 = 60;

/// Each game's own inputs to the shared view, on `oPowers` (the order is the
/// wire's): `zoomAdditive`, the battlefield size Farsight adds, which every
/// writer of the zoom target adds.
pub const INPUTS: [&str; 1] = ["zoomAdditive"];
/// The zoom writers that add [`INPUTS`]: on the host they run with the larger
/// of the two games' values (its own put back after).
pub const ZOOM_WRITERS: [&str; 3] = [
    "gml_Script_refreshZoom",
    "gml_Script_setBossZoom",
    "gml_Script_setZoom",
];
/// A joining game sends its inputs at least this often (frames).
const INPUTS_EVERY: u64 = 120;

/// A hash of [`INPUTS`].
#[must_use]
pub fn inputs_hash() -> u32 {
    INPUTS.iter().fold(0x811C_9DC5u32, |h, n| {
        n.bytes()
            .chain([0])
            .fold(h, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
    })
}

struct ViewHandles {
    vars: Vec<Option<Variable>>,
    /// [`INPUTS`] (instance variables of `oPowers`).
    inputs: Vec<Option<Variable>>,
    refresh_zoom: Option<Script>,
    calls: Vec<Option<Script>>,
    /// [`CALL_INPUTS`], by call.
    inputs_of: Vec<Vec<Option<Variable>>>,
    fade_bgm: Option<Script>,
    play_bgm_loop: Option<Script>,
    asset: Option<Builtin>,
    destroy: Option<Builtin>,
}

static VH: OnceLock<ViewHandles> = OnceLock::new();
/// The newest world call this game made (any role).
static LAST_CALL: Mutex<Option<WorldCall>> = Mutex::new(None);

/// A hash of [`GLOBALS`] (both sides must use the same list).
#[must_use]
pub fn names_hash() -> u32 {
    GLOBALS.iter().fold(0x811C_9DC5u32, |h, n| {
        n.bytes()
            .chain([0])
            .fold(h, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
    })
}

/// Per-session state.
#[derive(Debug, Default)]
pub(super) struct ViewState {
    /// Client: the newest view from the host.
    latest: Option<WorldView>,
    /// Client: calls from the host still to run.
    calls: Vec<WorldCall>,
    /// Host: calls made this frame, to send.
    out_calls: Vec<WorldCall>,
    /// Client: a host call is running now (its own are skipped otherwise).
    replaying: bool,
    /// Host: the clock of the last frame (for the calls' stamp).
    now_ms: u32,
    /// `oPowers`' object index (resolved once).
    powers: Option<u32>,
    /// Client: the inputs last sent, and when (frame).
    inputs_sent: Option<(Vec<f64>, u64)>,
    /// Host: the partner's inputs, and the shared values last applied.
    partner_inputs: Option<Vec<f64>>,
    shared_applied: Option<Vec<f64>>,
    /// Host: own values put aside while a zoom writer runs (a stack).
    swapped: Vec<Vec<f64>>,
    /// Client: the frame of the last view write (`None` before the run's
    /// first).
    written_at: Option<u64>,
    /// Client: the scheme this game's backdrop was last built with (its own
    /// at the session's start, then each call run here).
    built: Option<f64>,
    /// Client: running frames the host's scheme has differed from `built`.
    differs_for: u64,
    /// Client: running frames the host's track has differed from this
    /// game's.
    music_differs_for: u64,
    /// Client: the formation line was logged this session.
    formation_logged: bool,
    /// The ending is on (from `beginGameEnding` while `oAchiEndingDebug`
    /// exists), and this frame's shift of the partner's drawn ship (peer,
    /// dx, dy: [`WorldHooks::view_ending_shift`]).
    ending_on: bool,
    ending_shift: Option<(u16, f64, f64)>,
    /// This ship's place the shift was taken from (the run step).
    ending_own: Option<(f64, f64)>,
    /// The body sync ([`SYNCED_BODIES`]): the objects (resolved once); on a
    /// joining game the host's latest list not yet applied, its counts per
    /// sprite last time, lists in a row each sprite had more bodies here, and
    /// the bodies placed on the host's.
    body_objects: Option<Vec<u32>>,
    host_bodies: Option<Vec<(i64, f64, f64)>>,
    host_body_counts: HashMap<i64, usize>,
    body_extra: HashMap<i64, u32>,
    bodies_placed: HashSet<usize>,
}

/// A track (`bgmPlaying`) as its sound index: a ref's low 32 bits, or a
/// number (-1 or `undefined` before any).
fn track_index(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Ref(p)) => f64::from((p & 0xFFFF_FFFF) as u32),
        Some(other) => other.as_f64().unwrap_or(f64::NAN),
        None => f64::NAN,
    }
}

/// A call's arguments and its inputs (split at [`INPUTS_MARK`]).
#[must_use]
pub fn split_inputs(args: &[StateValue]) -> (&[StateValue], Vec<(&str, &StateValue)>) {
    let Some(at) = args
        .iter()
        .position(|a| matches!(a, StateValue::Str(s) if s == INPUTS_MARK))
    else {
        return (args, Vec::new());
    };
    let inputs = args[at + 1..]
        .as_chunks::<2>()
        .0
        .iter()
        .filter_map(|p| match &p[0] {
            StateValue::Str(n) => Some((n.as_str(), &p[1])),
            _ => None,
        })
        .collect();
    (&args[..at], inputs)
}

/// Resolve the handles (init).
pub(super) fn resolve(ctx: &ModContext) {
    let _ = VH.set(ViewHandles {
        vars: GLOBALS.iter().map(|n| ctx.variable(n).ok()).collect(),
        inputs: INPUTS.iter().map(|n| ctx.variable(n).ok()).collect(),
        refresh_zoom: ctx.script("gml_Script_refreshZoom").ok(),
        calls: CALLS.iter().map(|n| ctx.script(n).ok()).collect(),
        inputs_of: CALL_INPUTS
            .iter()
            .map(|l| l.iter().map(|n| ctx.variable(n).ok()).collect())
            .collect(),
        fade_bgm: ctx.script("gml_Script_fadeBGM").ok(),
        play_bgm_loop: ctx.script("gml_Script_playBGMLoop").ok(),
        asset: ctx.builtin("asset_get_index").ok(),
        destroy: ctx.builtin("instance_destroy").ok(),
    });
}

fn to_state(v: &Value) -> StateValue {
    match v {
        Value::Bool(b) => StateValue::Bool(*b),
        Value::Str(s) => StateValue::Str(s.clone()),
        Value::Int(n) => StateValue::Int(*n),
        other => other
            .as_f64()
            .map_or(StateValue::Undefined, StateValue::Real),
    }
}

fn to_value(v: &StateValue) -> Value {
    match v {
        StateValue::Bool(b) => Value::Bool(*b),
        StateValue::Str(s) => Value::Str(s.clone()),
        StateValue::Int(n) => Value::Int(*n),
        StateValue::Real(n) => Value::Real(*n),
        StateValue::Whole(n) => Value::Real(f64::from(*n)),
        StateValue::Real32(n) => Value::Real(f64::from(*n)),
        _ => Value::Undefined,
    }
}

impl WorldHooks {
    fn view_read(call: &Call<'_>) -> Option<Vec<f64>> {
        let h = VH.get()?;
        let g = call.global()?;
        Some(
            h.vars
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    if i == BGM {
                        track_index(v.and_then(|v| g.get_value(v)).as_ref())
                    } else {
                        v.and_then(|v| g.get_f64(v)).unwrap_or(f64::NAN)
                    }
                })
                .collect(),
        )
    }

    /// Host, each run frame: the view and the calls made since the last.
    pub(super) fn view_host(&self, call: &Call<'_>, now_ms: u32) -> Vec<Message> {
        self.lock().view.now_ms = now_ms;
        let mut out: Vec<Message> = std::mem::take(&mut self.lock().view.out_calls)
            .into_iter()
            .map(Message::WorldCall)
            .collect();
        out.extend(self.view_bodies_host(call, now_ms));
        if let Some(values) = Self::view_read(call) {
            out.push(Message::WorldView(WorldView {
                time_ms: now_ms,
                names: names_hash(),
                values,
            }));
        }
        out
    }

    /// This game's background bodies ([`SYNCED_BODIES`], exact objects):
    /// sprite, x, y and the instance.
    fn body_list(&self, call: &Call<'_>) -> Vec<(i64, f64, f64, Value)> {
        let Some(h) = VH.get() else {
            return Vec::new();
        };
        let objects = self.lock().view.body_objects.clone();
        let objects = objects.unwrap_or_else(|| {
            let o: Vec<u32> = SYNCED_BODIES
                .iter()
                .filter_map(|n| {
                    h.asset
                        .and_then(|b| call.call_builtin(b, &[Value::Str((*n).to_owned())]).ok())
                        .as_ref()
                        .and_then(super::classify::object_index)
                })
                .collect();
            self.lock().view.body_objects = Some(o.clone());
            o
        });
        let mut out = Vec::new();
        for o in objects {
            let _ = call.with(&Value::object(o), |inst, _| {
                let own = inst
                    .builtin_value("object_index")
                    .ok()
                    .as_ref()
                    .and_then(super::classify::object_index)
                    == Some(o);
                let at = |n: &str| inst.builtin_value(n).ok().and_then(|v| v.as_f64());
                if own
                    && let (Some(sp), Some(x), Some(y), Ok(id)) =
                        (sprite_of(&inst), at("x"), at("y"), inst.builtin_value("id"))
                {
                    out.push((sp, x, y, id));
                }
            });
        }
        out
    }

    /// Host, every [`BODY_LIST_EVERY`] run frames: its background bodies
    /// (sprite, x, y each) as the [`BODY_LIST`] pseudo-call.
    fn view_bodies_host(&self, call: &Call<'_>, now_ms: u32) -> Option<Message> {
        let frame = self.lock().frame;
        if !frame.is_multiple_of(BODY_LIST_EVERY) || self.lock().frozen {
            return None;
        }
        let args = self
            .body_list(call)
            .into_iter()
            .flat_map(|(s, x, y, _)| {
                [
                    StateValue::Real(f64::from(i32::try_from(s).unwrap_or(-1))),
                    StateValue::Real(x),
                    StateValue::Real(y),
                ]
            })
            .collect();
        Some(Message::WorldCall(WorldCall {
            time_ms: now_ms,
            script: BODY_LIST.to_owned(),
            args,
        }))
    }

    /// Joining game, on the host's body list: a sprite this game has more
    /// bodies of than the host is cut back to the host's count (the farthest
    /// first) after two lists in a row or a fall in the host's count; a body
    /// is put on the nearest of the host's once. Not in a freeze.
    fn view_bodies_client(&self, call: &Call<'_>) {
        let host = {
            let mut st = self.lock();
            if st.frozen {
                return;
            }
            st.view.host_bodies.take()
        };
        let Some(host) = host else { return };
        let mine = self.body_list(call);
        let mut by_sprite: HashMap<i64, Vec<(f64, f64)>> = HashMap::new();
        for (s, x, y) in &host {
            by_sprite.entry(*s).or_default().push((*x, *y));
        }
        let mut own: HashMap<i64, Vec<(f64, f64, Value)>> = HashMap::new();
        for (s, x, y, id) in mine {
            own.entry(s).or_default().push((x, y, id));
        }
        let mut remove = Vec::new();
        let mut place = Vec::new();
        {
            let mut st = self.lock();
            let v = &mut st.view;
            for (s, list) in &own {
                let theirs = by_sprite.get(s).map_or(&[][..], Vec::as_slice);
                let near = |x: f64, y: f64| {
                    theirs
                        .iter()
                        .map(|(hx, hy)| (hx - x).hypot(hy - y))
                        .fold(f64::INFINITY, f64::min)
                };
                if list.len() > theirs.len() {
                    let fell = v
                        .host_body_counts
                        .get(s)
                        .is_some_and(|&was| theirs.len() < was);
                    let seen = v.body_extra.entry(*s).or_insert(0);
                    *seen += 1;
                    if *seen >= 2 || fell {
                        let mut far: Vec<&(f64, f64, Value)> = list.iter().collect();
                        far.sort_by(|a, b| near(b.0, b.1).total_cmp(&near(a.0, a.1)));
                        remove.extend(
                            far.into_iter()
                                .take(list.len() - theirs.len())
                                .map(|b| b.2.clone()),
                        );
                        v.body_extra.remove(s);
                    }
                } else {
                    v.body_extra.remove(s);
                }
                // Places: each of the host's taken by the nearest unplaced one.
                let mut free: Vec<&(f64, f64, Value)> =
                    list.iter().filter(|b| !remove.contains(&b.2)).collect();
                for (hx, hy) in theirs {
                    let Some(i) = free
                        .iter()
                        .enumerate()
                        .min_by(|a, b| {
                            (a.1.0 - hx)
                                .hypot(a.1.1 - hy)
                                .total_cmp(&(b.1.0 - hx).hypot(b.1.1 - hy))
                        })
                        .map(|(i, _)| i)
                    else {
                        break;
                    };
                    let b = free.swap_remove(i);
                    place.push((b.2.clone(), *hx, *hy));
                }
            }
            v.host_body_counts = by_sprite.iter().map(|(s, l)| (*s, l.len())).collect();
        }
        let destroy = VH.get().and_then(|h| h.destroy);
        for id in remove {
            if let Some(b) = destroy {
                let _ = call.call_builtin(b, &[id, Value::Bool(false)]);
            }
        }
        for (id, x, y) in place {
            let _ = call.with(&id, |inst, _| {
                let key = inst.key();
                if self.lock().view.bodies_placed.insert(key) {
                    let _ = inst.set_builtin_f64("x", x);
                    let _ = inst.set_builtin_f64("y", y);
                }
            });
        }
    }

    /// Client: take the view messages out of the inbox.
    pub(super) fn view_inbox(&self, inbox: &mut Vec<Message>) {
        let mut st = self.lock();
        inbox.retain(|m| match m {
            Message::WorldView(v) => {
                if v.names == names_hash()
                    && st
                        .view
                        .latest
                        .as_ref()
                        .is_none_or(|l| l.time_ms <= v.time_ms)
                {
                    st.view.latest = Some(v.clone());
                }
                false
            }
            Message::WorldCall(c) if c.script == BODY_LIST => {
                st.view.host_bodies = Some(
                    c.args
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .filter_map(|b| {
                            let n = |v: &StateValue| match v.clone().expanded() {
                                StateValue::Real(f) => Some(f),
                                StateValue::Int(i) => i32::try_from(i).ok().map(f64::from),
                                _ => None,
                            };
                            Some((n(&b[0])? as i64, n(&b[1])?, n(&b[2])?))
                        })
                        .collect(),
                );
                false
            }
            Message::WorldCall(c) => {
                st.view.calls.push(c.clone());
                false
            }
            _ => true,
        });
    }

    /// Client, each run frame: run the host's calls, write its view.
    pub(super) fn view_client(&self, call: &Call<'_>) {
        self.view_bodies_client(call);
        let Some(h) = VH.get() else { return };
        // Not in this game's freeze: a call then acts on a deactivated world
        // (`closeBlackHoles` finds no black hole, `changeColors` makes its
        // background behind the pause screen); they wait, in order, for the
        // first running frame.
        let (calls, latest) = {
            let mut st = self.lock();
            let calls = if st.frozen {
                Vec::new()
            } else {
                std::mem::take(&mut st.view.calls)
            };
            (calls, st.view.latest.clone())
        };
        let ran = !calls.is_empty();
        for c in calls {
            let Some(ix) = CALLS.iter().position(|n| *n == c.script) else {
                continue;
            };
            let Some(script) = h.calls[ix] else { continue };
            let (args, inputs) = split_inputs(&c.args);
            // The host's inputs, as its call read them; after the call, one
            // it left as set goes back to this game's own (an input it only
            // read: `wave` from an older call sent again at a session's
            // start), one it changed keeps the call's result (`color`).
            let mut set: Vec<(Variable, Option<Value>, Value)> = Vec::new();
            if let Some(g) = call.global() {
                for (n, v) in &inputs {
                    let var = CALL_INPUTS[ix]
                        .iter()
                        .position(|x| x == n)
                        .and_then(|i| h.inputs_of[ix][i]);
                    if let (Some(var), false) = (var, matches!(v, StateValue::Undefined)) {
                        let own = g.get_value(var);
                        let value = to_value(v);
                        if g.set(var, value.clone()).is_ok() {
                            set.push((var, own, value));
                        }
                    }
                }
            }
            let args: Vec<Value> = args.iter().map(to_value).collect();
            let before = Self::scheme(call);
            self.lock().view.replaying = true;
            let r = call.call_script(script, &args);
            // A call that changed the scheme built a backdrop with it.
            let after = Self::scheme(call);
            let built = after.filter(|a| before.is_none_or(|b| (b - a).abs() >= 0.5));
            if let Some(g) = call.global() {
                for (var, own, value) in set {
                    if let Some(own) = own
                        && g.get_value(var).is_some_and(|now| now == value)
                    {
                        let _ = g.set(var, own);
                    }
                }
            }
            let mut st = self.lock();
            st.view.replaying = false;
            if ix == 0 && built.is_some() {
                st.view.built = built;
            }
            drop(st);
            if let Err(e) = r {
                log::warn!("co-op world view: the host's {}: {e}", c.script);
            }
        }
        self.view_rebuild(call, latest.as_ref(), ran);
        self.view_music(call, latest.as_ref());
        self.view_write(call);
    }

    /// Client, each running frame: when the host's track has differed from
    /// this game's for [`MUSIC_SETTLE`] frames, switch as the wave director
    /// does (`fadeBGM(0, 3000)`, `bgmPlaying` = the track, `playBGMLoop()`):
    /// at this player's own volume and music setting (`playBGMLoop` reads
    /// `musicVolumeBGM`), once per host change.
    fn view_music(&self, call: &Call<'_>, latest: Option<&WorldView>) {
        let host = latest
            .and_then(|v| v.values.get(BGM).copied())
            .filter(|t| t.is_finite() && *t >= 0.0);
        let (Some(host), Some(h), Some(g)) = (host, VH.get(), call.global()) else {
            return;
        };
        let Some(var) = h.vars[BGM] else { return };
        // The sound module plays the host's music calls themselves (the
        // tracks its playlist and wave code start, played once or looped as
        // there): this switch, which loops whatever `bgmPlaying` the host
        // shows, would play a track the host plays once on and on, and a
        // track the host names but never starts.
        let frozen = self.lock().frozen;
        if frozen || crate::sound::glue::follows_host_music() {
            // Settle again from 0 if the switch takes over later.
            self.lock().view.music_differs_for = 0;
            return;
        }
        let own_value = g.get_value(var);
        let own = track_index(own_value.as_ref());
        if (own - host).abs() < 0.5 {
            self.lock().view.music_differs_for = 0;
            return;
        }
        let n = {
            let mut st = self.lock();
            st.view.music_differs_for += 1;
            st.view.music_differs_for
        };
        if n < MUSIC_SETTLE {
            return;
        }
        let high = match own_value {
            Some(Value::Ref(p)) => p & !0xFFFF_FFFF,
            _ => SOUND_REF,
        };
        let track = Value::Ref(high | host as u64);
        if let Some(fade) = h.fade_bgm {
            let _ = call.call_script(fade, &[Value::Real(0.0), Value::Real(MUSIC_FADE_MS)]);
        }
        let _ = g.set(var, track);
        let r = h.play_bgm_loop.map(|p| call.call_script(p, &[]));
        self.lock().view.music_differs_for = 0;
        log::info!(
            "co-op world view: music switched to the host's track {host} (was {own}): {:?}",
            r.map(|r| r.map(|_| ()))
        );
    }

    /// The ending's draw shift for the partner's ship (peer, dx, dy), refreshed
    /// each run step while the ending is on: drawn at this ship's place less
    /// its slot plus the partner's, so both screens show the formation whatever
    /// the sequences' start times; only the place shifts. `None` outside the
    /// ending. Used by [`Self::shifted_partners`] and `WorldLinks::view_shift`.
    pub(super) fn view_ending_shift(&self, call: &Call<'_>) -> Option<(u16, f64, f64)> {
        if !self.lock().view.ending_on {
            return None;
        }
        let on = self
            .asset(call, "oAchiEndingDebug")
            .is_some_and(|o| self.instances(call, o) > 0);
        let now = on.then(|| self.ending_shift_now(call)).flatten();
        let mut st = self.lock();
        st.view.ending_on = on;
        st.view.ending_shift = now.map(|(s, _)| s);
        st.view.ending_own = now.map(|(_, o)| o);
        st.view.ending_shift
    }

    /// The shift, and this ship's place it was taken from.
    fn ending_shift_now(&self, call: &Call<'_>) -> Option<(EndingShift, (f64, f64))> {
        let me = self.lock().me?;
        let player = self.asset(call, "oPlayer")?;
        let mut own = None;
        let _ = call.with(&Value::object(player), |inst, _| {
            own = match (
                inst.builtin_f64("x"),
                inst.builtin_f64("y"),
                inst.builtin_f64("image_angle"),
            ) {
                (Some(x), Some(y), Some(a)) => Some((x, y, a)),
                _ => None,
            };
        });
        let (x, y, a) = own?;
        let (p, px, py, _) = self.ending_partner()?;
        let slot = |peer: u16| {
            usize::from(peer)
                .checked_sub(1)
                .and_then(|s| formation_offset(s, a))
                .unwrap_or((0.0, 0.0))
        };
        let (mx, my) = slot(u16::from(me.0));
        let (ox, oy) = slot(p);
        Some(((p, x - mx + ox - px, y - my + oy - py), (x, y)))
    }

    /// Before the controller's End Step, while the ending is on: this ship
    /// as it will be drawn (after every Step and the built-in motion), and
    /// the partner's drawn ship and its mirrored entries moved with it by
    /// what this ship moved since the run step took the shift from it, so
    /// the two keep their slots exactly at any speed. Returns that move
    /// (`None`: none) for the mirror ([`crate::mirror`]) to move its
    /// shifted entries by.
    pub(super) fn view_ending_end_step(&self, call: &Call<'_>) -> Option<(f64, f64)> {
        if self.role() == 0 {
            return None;
        }
        let (peer, own_then) = {
            let st = self.lock();
            if !st.view.ending_on {
                return None;
            }
            (st.view.ending_shift.map(|s| s.0), st.view.ending_own)
        };
        let player = self.asset(call, "oPlayer")?;
        let mut own = None;
        let _ = call.with(&Value::object(player), |inst, _| {
            own = match (
                inst.builtin_f64("x"),
                inst.builtin_f64("y"),
                inst.builtin_f64("image_angle"),
            ) {
                (Some(x), Some(y), Some(a)) => Some((x, y, a)),
                _ => None,
            };
        });
        match (peer, own_then, own) {
            (Some(peer), Some((tx, ty)), Some((x, y, _))) => {
                let (dx, dy) = (x - tx, y - ty);
                (dx.abs() > 0.01 || dy.abs() > 0.01).then(|| {
                    let id = {
                        let mut st = self.lock();
                        if let Some(s) = st.view.ending_shift.as_mut() {
                            s.1 += dx;
                            s.2 += dy;
                        }
                        st.view.ending_own = Some((x, y));
                        u8::try_from(peer)
                            .ok()
                            .and_then(|p| st.ents.nudge(PeerId(p), dx as f32, dy as f32))
                    };
                    if let Some(id) = id {
                        let _ = call.with(&id, |inst, _| {
                            if let (Some(ax), Some(ay)) =
                                (inst.builtin_f64("x"), inst.builtin_f64("y"))
                            {
                                let _ = inst.set_builtin_f64("x", ax + dx);
                                let _ = inst.set_builtin_f64("y", ay + dy);
                            }
                        });
                    }
                    (dx, dy)
                })
            }
            _ => None,
        }
    }

    /// `partners` with the ending's shift on the shifted partner's place.
    pub(super) fn shifted_partners(&self, partners: &[PartnerView]) -> Option<Vec<PartnerView>> {
        let (peer, dx, dy) = self.lock().view.ending_shift?;
        Some(
            partners
                .iter()
                .cloned()
                .map(|mut p| {
                    if u16::from(p.peer.0) == peer
                        && let Some((pos, _, _)) = p.pose.as_mut()
                    {
                        pos.x += dx as f32;
                        pos.y += dy as f32;
                    }
                    p
                })
                .collect(),
        )
    }

    /// The mirror's shift of the partner's entries this frame (pairs: the
    /// one partner's).
    pub(super) fn ending_view_shift(&self) -> Option<(f64, f64)> {
        self.lock().view.ending_shift.map(|(_, dx, dy)| (dx, dy))
    }

    /// The partner whose ship the ending's formation is placed against: the
    /// host's on a joining game, the first joining ship's on the host
    /// (position and facing as this game has it from its poses: the
    /// formation's base, never shifted).
    fn ending_partner(&self) -> Option<(u16, f64, f64, f64)> {
        let client = self.role() == ROLE_CLIENT;
        self.lock()
            .partners
            .iter()
            .filter(|p| !client || p.peer.0 == 0)
            .find_map(|p| {
                p.pose.map(|(pos, _, a)| {
                    (
                        u16::from(p.peer.0),
                        f64::from(pos.x),
                        f64::from(pos.y),
                        f64::from(a),
                    )
                })
            })
    }

    /// After `beginGameEnding` ([`ENDING_BEGIN`]): where it put this ship,
    /// and where this game draws the partner's (both sides, with the wall
    /// clock).
    pub(super) fn view_ending_began(&self, call: &Call<'_>) {
        if self.role() == 0 {
            return;
        }
        self.lock().view.ending_on = true;
        let Some(player) = self.asset(call, "oPlayer") else {
            return;
        };
        let mut own = None;
        let _ = call.with(&Value::object(player), |inst, _| {
            own = inst.builtin_f64("x").zip(inst.builtin_f64("y"));
        });
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let partner = self.ending_partner();
        log::info!(
            "co-op world view: the ending began (unix ms {unix}): this ship put at {}, the partner's drawn at {}",
            own.map_or_else(|| "-".to_owned(), |(x, y)| format!("({x:.0}, {y:.0})")),
            partner.map_or_else(
                || "-".to_owned(),
                |(p, x, y, _)| format!("({x:.0}, {y:.0}) (P{})", p + 1)
            )
        );
    }

    /// After the ending's step ([`ENDING_STEP`]) on a joining game: moves this
    /// ship to its formation slot ([`FORMATION_SLOTS`]; P2 takes the first),
    /// relative to where the step put it once the sequence's ship exists, else
    /// to the host's ship as drawn here. The host's ship is never moved.
    pub(super) fn view_formation(&self, call: &Call<'_>) {
        if self.role() == 0 {
            return;
        }
        let client = self.role() == ROLE_CLIENT;
        let Some(me) = self.lock().me else { return };
        let slot = usize::from(me.0).checked_sub(1).filter(|_| client);
        let Some(player) = self.asset(call, "oPlayer") else {
            return;
        };
        let sequence = self
            .asset(call, ENDING_SHIP)
            .is_some_and(|o| self.instances(call, o) > 0);
        let partner = self.ending_partner();
        let mut moved = 0u32;
        let _ = call.with(&Value::object(player), |inst, _| {
            let (Some(x), Some(y), Some(a)) = (
                inst.builtin_f64("x"),
                inst.builtin_f64("y"),
                inst.builtin_f64("image_angle"),
            ) else {
                return;
            };
            let Some((dx, dy)) = slot.and_then(|s| formation_offset(s, a)) else {
                return;
            };
            let base = if sequence {
                Some((x, y))
            } else {
                partner.map(|(_, px, py, _)| (px, py))
            };
            let Some((bx, by)) = base else { return };
            let _ = inst.set_builtin_f64("x", bx + dx);
            let _ = inst.set_builtin_f64("y", by + dy);
            moved += 1;
        });
        if moved > 0 {
            let mut st = self.lock();
            if !st.view.formation_logged {
                st.view.formation_logged = true;
                drop(st);
                log::info!(
                    "co-op world view: the ending's formation: this ship in slot {} (P{}), from the {}",
                    slot.unwrap_or(0),
                    me.0 + 1,
                    if sequence {
                        "sequence's ship"
                    } else {
                        "host's ship (the lead-in)"
                    }
                );
            }
        }
    }

    /// Before `advanceBGM` (the playlist's next song, `oController`'s
    /// `Alarm_0` and `Create`, a debug key): a joining game in a shared run
    /// skips its own (the host's track arrives; its own playlist would
    /// switch away from it). Outside a shared run, the menu, the UI: its own.
    pub(super) fn view_playlist(&self, call: &mut Call<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        call.skip_original();
        let _ = call.set_return(0.0);
    }

    /// This game's `global.color` (the scheme).
    fn scheme(call: &Call<'_>) -> Option<f64> {
        let v = VH.get()?.vars[COLOR]?;
        call.global()?.get_f64(v).filter(|c| c.is_finite())
    }

    /// Client, at the session's start (before any view write): the scheme
    /// this game's backdrop was built with is its own `color` now.
    pub(super) fn view_client_start(&self, call: &Call<'_>) {
        let built = Self::scheme(call);
        self.lock().view.built = built;
    }

    /// Client, each running frame after the host's calls: when the host's
    /// scheme has differed from the one this backdrop was built with for
    /// [`REBUILD_AFTER`] frames and no host call waits, `changeColors(host
    /// scheme)` from the built one (the game's own transition), once.
    fn view_rebuild(&self, call: &Call<'_>, latest: Option<&WorldView>, ran: bool) {
        let host = latest
            .and_then(|v| v.values.get(COLOR).copied())
            .filter(|c| c.is_finite());
        let (Some(host), Some(h), Some(g)) = (host, VH.get(), call.global()) else {
            return;
        };
        let (built, waiting, frozen) = {
            let st = self.lock();
            (st.view.built, !st.view.calls.is_empty(), st.frozen)
        };
        if frozen {
            return;
        }
        let Some(built) = built else {
            // No build seen and none known: take the host's as built.
            self.lock().view.built = Some(host);
            return;
        };
        if ran || waiting || (built - host).abs() < 0.5 {
            self.lock().view.differs_for = 0;
            return;
        }
        let n = {
            let mut st = self.lock();
            st.view.differs_for += 1;
            st.view.differs_for
        };
        if n < REBUILD_AFTER {
            return;
        }
        let (Some(script), Some(color)) = (h.calls[0], h.vars[COLOR]) else {
            return;
        };
        // From the built scheme, so the call sees a change and runs the
        // game's own swap (it does nothing for the scheme it already has).
        let _ = g.set_f64(color, built);
        self.lock().view.replaying = true;
        let r = call.call_script(script, &[Value::Real(host)]);
        let now = Self::scheme(call);
        {
            let mut st = self.lock();
            st.view.replaying = false;
            st.view.differs_for = 0;
            st.view.built = Some(host);
        }
        log::info!(
            "co-op world view: backdrop rebuilt for the host's scheme {host} (built with {built}; now {now:?}): {:?}",
            r.as_ref().map(|_| ())
        );
    }

    /// Client: write the host's view globals. Called at the run step and
    /// again just before the game's own `cameraZoom` and `colorFade`, so
    /// whatever this game's code set earlier in the frame (`refreshZoom` from
    /// its own input included), the camera and colours ease toward the
    /// host's.
    pub(super) fn view_write(&self, call: &Call<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let Some(h) = VH.get() else { return };
        let (latest, frame, stale) = {
            let st = self.lock();
            let frame = st.frame;
            let stale = st
                .view
                .written_at
                .is_none_or(|w| frame.saturating_sub(w) > SNAP_GAP_FRAMES);
            (st.view.latest.clone(), frame, stale)
        };
        if let (Some(v), Some(g)) = (latest, call.global()) {
            for (i, (var, value)) in h.vars.iter().zip(&v.values).enumerate() {
                let Some(var) = var else { continue };
                if !value.is_finite() || i == ZOOM_RATE || i == BGM {
                    continue;
                }
                if i == ZOOM_FACTOR {
                    let own = g.get_f64(*var).unwrap_or(f64::NAN);
                    if !stale
                        && own.is_finite()
                        && (own - value).abs() <= SNAP * value.abs().max(1e-6)
                    {
                        continue;
                    }
                }
                let _ = g.set_f64(*var, *value);
            }
            let mut st = self.lock();
            st.view.written_at = Some(frame);
        }
    }

    /// Before a script in [`CALLS`]: the host records it; a client skips its
    /// own (the host's arrive), unless it is running the host's.
    pub(super) fn view_call(&self, call: &mut Call<'_>, script: &'static str) {
        match self.role() {
            ROLE_CLIENT => {
                let st = self.lock();
                if !st.view.replaying {
                    drop(st);
                    call.skip_original();
                    let _ = call.set_return(0.0);
                }
            }
            role => {
                // Any other role: the newest call is kept (a session that
                // starts later sends it first), and a host sends it now.
                let mut args: Vec<StateValue> = (0..call.argc())
                    .map(|i| {
                        call.arg_value(i)
                            .as_ref()
                            .map_or(StateValue::Undefined, to_state)
                    })
                    .collect();
                // The globals the call reads, as they are now (before it).
                let ix = CALLS.iter().position(|n| *n == script);
                if let (Some(ix), Some(h), Some(g)) = (ix, VH.get(), call.global()) {
                    let names = CALL_INPUTS[ix];
                    if !names.is_empty() {
                        args.push(StateValue::Str(INPUTS_MARK.to_owned()));
                        for (n, v) in names.iter().zip(&h.inputs_of[ix]) {
                            args.push(StateValue::Str((*n).to_owned()));
                            args.push(
                                v.and_then(|v| g.get_value(v))
                                    .as_ref()
                                    .map_or(StateValue::Undefined, to_state),
                            );
                        }
                    }
                }
                let mut st = self.lock();
                let made = WorldCall {
                    time_ms: st.view.now_ms,
                    script: script.to_owned(),
                    args,
                };
                if role == ROLE_HOST {
                    st.view.out_calls.push(made.clone());
                }
                if script == CALL_AT_START {
                    *LAST_CALL.lock().unwrap_or_else(PoisonError::into_inner) = Some(made);
                }
            }
        }
    }

    /// Host, at the session's start: the newest world call made before it
    /// (the run's own background), so a joining game starts from it too.
    pub(super) fn view_session_start(&self) {
        if let Some(c) = LAST_CALL
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            self.lock().view.out_calls.push(c);
        }
    }

    /// This game's [`INPUTS`] (`oPowers`' variables; `None` without one).
    fn inputs_read(&self, call: &Call<'_>) -> Option<Vec<f64>> {
        let h = VH.get()?;
        let powers = self.view_powers(call)?;
        let mut out = None;
        let _ = call.with(&Value::object(powers), |inst, _| {
            out = Some(
                h.inputs
                    .iter()
                    .map(|v| v.and_then(|v| inst.get_f64(v)).unwrap_or(0.0))
                    .collect::<Vec<f64>>(),
            );
        });
        out
    }

    fn inputs_write(&self, call: &Call<'_>, values: &[f64]) {
        let (Some(h), Some(powers)) = (VH.get(), self.view_powers(call)) else {
            return;
        };
        let _ = call.with(&Value::object(powers), |inst, _| {
            for (v, x) in h.inputs.iter().zip(values) {
                if let Some(v) = v {
                    let _ = inst.set_f64(*v, *x);
                }
            }
        });
    }

    fn view_powers(&self, call: &Call<'_>) -> Option<u32> {
        if let Some(p) = self.lock().view.powers {
            return Some(p);
        }
        let p = VH
            .get()?
            .asset
            .and_then(|b| call.call_builtin(b, &[Value::Str("oPowers".into())]).ok())
            .as_ref()
            .and_then(super::classify::object_index)?;
        self.lock().view.powers = Some(p);
        Some(p)
    }

    /// Client, each run frame: this game's own inputs to the shared view, to
    /// the host when they change (and every [`INPUTS_EVERY`] frames).
    pub(super) fn view_inputs_client(&self, call: &Call<'_>) -> Vec<Message> {
        if self.role() != ROLE_CLIENT {
            return Vec::new();
        }
        let Some(values) = self.inputs_read(call) else {
            return Vec::new();
        };
        let mut st = self.lock();
        let frame = st.frame;
        let due = st
            .view
            .inputs_sent
            .as_ref()
            .is_none_or(|(v, at)| *v != values || frame.saturating_sub(*at) >= INPUTS_EVERY);
        if !due {
            return Vec::new();
        }
        st.view.inputs_sent = Some((values.clone(), frame));
        let time_ms = st.view.now_ms;
        drop(st);
        vec![Message::WorldInputs(WorldView {
            time_ms,
            names: inputs_hash(),
            values,
        })]
    }

    /// Host, each run frame: the partner's inputs. When the shared values
    /// (the larger of each) change, the zoom is refreshed as the upgrade
    /// itself does (`refreshZoom(true)`, run with the shared values).
    pub(super) fn view_inputs_host(&self, call: &Call<'_>, inbox: &[Message]) {
        let newest = inbox.iter().rev().find_map(|m| match m {
            Message::WorldInputs(v) if v.names == inputs_hash() => Some(v.values.clone()),
            _ => None,
        });
        if let Some(v) = newest {
            self.lock().view.partner_inputs = Some(v);
        }
        let Some(own) = self.inputs_read(call) else {
            return;
        };
        let Some(shared) = self.shared_inputs(&own) else {
            return;
        };
        let changed = self
            .lock()
            .view
            .shared_applied
            .as_ref()
            .is_none_or(|a| *a != shared);
        if !changed {
            return;
        }
        let first = self.lock().view.shared_applied.is_none();
        self.lock().view.shared_applied = Some(shared.clone());
        // Nothing to redo while the partner adds nothing over the host's.
        if first && shared == own {
            return;
        }
        if let Some(s) = VH.get().and_then(|h| h.refresh_zoom) {
            let r = call.call_script(s, &[Value::Bool(true)]);
            log::info!(
                "co-op world view: shared view inputs {shared:?} (own {own:?}): zoom refreshed ({r:?})"
            );
        }
    }

    /// The shared inputs: the larger of `own` and the partner's, each.
    fn shared_inputs(&self, own: &[f64]) -> Option<Vec<f64>> {
        let st = self.lock();
        let partner = st.view.partner_inputs.as_ref()?;
        Some(
            own.iter()
                .zip(partner.iter().chain(std::iter::repeat(&f64::NEG_INFINITY)))
                .map(|(a, b)| if b.is_finite() { a.max(*b) } else { *a })
                .collect(),
        )
    }

    /// Host, before a zoom writer ([`ZOOM_WRITERS`]): it runs with the shared
    /// inputs; the host's go back after ([`Self::zoom_writer_after`]).
    pub(super) fn zoom_writer_before(&self, call: &Call<'_>) -> bool {
        if self.role() != ROLE_HOST {
            return false;
        }
        let Some(own) = self.inputs_read(call) else {
            return false;
        };
        let Some(shared) = self.shared_inputs(&own) else {
            return false;
        };
        if shared == own {
            return false;
        }
        self.inputs_write(call, &shared);
        self.lock().view.swapped.push(own);
        true
    }

    /// Host, after a zoom writer: its own inputs back.
    pub(super) fn zoom_writer_after(&self, call: &Call<'_>, swapped: bool) {
        if !swapped {
            return;
        }
        let own = self.lock().view.swapped.pop();
        if let Some(own) = own {
            self.inputs_write(call, &own);
        }
    }
}

/// An instance's `sprite_index` as a number.
fn sprite_of(inst: &ndml_sdk::Instance<'_>) -> Option<i64> {
    match inst.builtin_value("sprite_index") {
        Ok(Value::Ref(p)) => Some(i64::from((p & 0xFFFF_FFFF) as u32 as i32)),
        Ok(v) => v.as_f64().map(|n| n as i64),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_s_inputs_travel_after_its_arguments() {
        let s = |t: &str| StateValue::Str(t.to_owned());
        let args = vec![
            StateValue::Undefined,
            StateValue::Bool(false),
            s(INPUTS_MARK),
            s("color"),
            StateValue::Real(2.0),
            s("wave"),
            StateValue::Real(61.0),
        ];
        let (a, inputs) = split_inputs(&args);
        assert_eq!(a, &args[..2]);
        assert_eq!(
            inputs,
            vec![
                ("color", &StateValue::Real(2.0)),
                ("wave", &StateValue::Real(61.0))
            ]
        );
        // A call without inputs: all arguments.
        let (a, inputs) = split_inputs(&args[..2]);
        assert_eq!(a.len(), 2);
        assert!(inputs.is_empty());
    }

    #[test]
    fn a_slot_measured_in_the_ship_frame_is_the_slot() {
        for angle in [0.0, 37.0, 90.0, 200.0] {
            for (slot, &(r, b)) in FORMATION_SLOTS.iter().enumerate() {
                let (dx, dy) = formation_offset(slot, angle).unwrap_or_default();
                let (right, behind) = in_ship_frame((50.0, -20.0), (50.0 + dx, -20.0 + dy), angle);
                assert!((right - r).abs() < 1e-9 && (behind - b).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn the_formation_slot_is_right_of_the_ship_and_behind_it() {
        // Facing up (90): right is +x, behind is +y (down the screen).
        let (dx, dy) = formation_offset(0, 90.0).unwrap_or_default();
        assert!(
            (dx - 110.0).abs() < 1e-9 && (dy - 30.0).abs() < 1e-9,
            "{dx} {dy}"
        );
        // Facing right (0): right is +y (down), behind is -x.
        let (dx, dy) = formation_offset(0, 0.0).unwrap_or_default();
        assert!(
            (dx + 30.0).abs() < 1e-9 && (dy - 110.0).abs() < 1e-9,
            "{dx} {dy}"
        );
        // P3 mirrors P2; there is no fifth player's slot.
        let (dx, _) = formation_offset(1, 90.0).unwrap_or_default();
        assert!((dx + 110.0).abs() < 1e-9);
        assert!(formation_offset(3, 0.0).is_none());
    }

    #[test]
    fn a_track_is_its_sound_index() {
        assert_eq!(GLOBALS[BGM], "bgmPlaying");
        assert!((track_index(Some(&Value::Ref(0x0100_0002_0000_00e7))) - 231.0).abs() < 1e-9);
        assert!((track_index(Some(&Value::Real(-1.0))) + 1.0).abs() < 1e-9);
        assert!(track_index(None).is_nan());
        assert_eq!(SOUND_REF | 0xe7, 0x0100_0002_0000_00e7);
    }

    #[test]
    fn every_replayed_call_names_its_inputs() {
        assert_eq!(CALLS.len(), CALL_INPUTS.len());
        assert!(CALL_INPUTS[0].contains(&"color"));
        assert_eq!(GLOBALS.iter().position(|n| *n == "color"), Some(COLOR));
    }
}
