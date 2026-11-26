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

//! The mirror's own drawing: a proxy whose object has Draw code is drawn by that code
//! (`event_perform_object`) with the variables it reads, sent by the owner.
//!
//! Owner: an observer on Draw learns which objects run Draw code; a trial runs it on scratch
//! painter instances to find the variables it needs (safe, or not); each send tick sends those
//! values ([`MirrorDraw`]). Viewer: the painter's Draw is hooked; proxies draw with the owner's
//! code when safe, else with `draw_self`; code that fails too often keeps the default draw for
//! the run ([`FAIL_LIMIT`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use ndml_net::Message;
use ndml_net::mirror::{KeyVars, MirrorDraw};
use ndml_net::mirror_vars::{ArrayEdit, DrawSender, values_of};
use ndml_net::msg::{PeerId, StateValue};
use ndml_sdk::{Builtin, Call, ModContext, Value};

use super::{MirrorHooks, id_key};
use crate::world::state::{self, Reflect};

/// The proxies' object: only a Draw event, no parent, unused by game code.
pub const PAINTER: &str = "oAchiApproachGlow";
const PAINTER_DRAW: &str = "gml_Object_oAchiApproachGlow_Draw_0";
/// GML `ev_draw`.
const EV_DRAW: f64 = 8.0;
/// Variables one trial may add before giving up.
const TRIAL_VARS: usize = 24;
/// Trials per send tick.
const TRIALS_PER_TICK: usize = 2;
/// Trials per code object, each on another live instance; the variables are
/// the union of the clean trials.
const TRIALS_EACH: u32 = 3;
/// Send ticks between a code object's trials.
const TRIAL_SPACING: u32 = 15;
/// Send ticks after its first trial when a code object is settled anyway.
const TRIAL_SETTLE: u32 = 90;
/// Send ticks after settling when a live code object is tried again (also
/// after every freeze: the build may have changed).
const RETRY_EVERY: u32 = 300;
/// Built-in variables a trial copies from its model (the default draw's).
const POSE: [&str; 9] = [
    "x",
    "y",
    "image_angle",
    "image_index",
    "image_xscale",
    "image_yscale",
    "image_alpha",
    "image_blend",
    "depth",
];
/// Failures (and more than one in [`FAIL_PER`] draws) after which a code
/// object keeps the default draw for the run.
const FAIL_LIMIT: u32 = 8;
const FAIL_PER: u32 = 100;

/// Builtins the drawing needs.
#[derive(Debug, Clone, Copy)]
pub(super) struct DrawH {
    perform: Option<Builtin>,
    draw_self: Option<Builtin>,
    reflect: Option<Reflect>,
    /// GPU state a Draw stopped by an error leaves behind is put back.
    get_state: Option<Builtin>,
    set_state: Option<Builtin>,
    map_destroy: Option<Builtin>,
    blendmode: Option<Builtin>,
    fog: Option<Builtin>,
    shader_reset: Option<Builtin>,
}

impl DrawH {
    pub(super) fn resolve(ctx: &ModContext) -> Self {
        Self {
            perform: ctx.builtin("event_perform_object").ok(),
            draw_self: ctx.builtin("draw_self").ok(),
            reflect: Reflect::resolve(ctx),
            get_state: ctx.builtin("gpu_get_state").ok(),
            set_state: ctx.builtin("gpu_set_state").ok(),
            map_destroy: ctx.builtin("ds_map_destroy").ok(),
            blendmode: ctx.builtin("gpu_set_blendmode").ok(),
            fog: ctx.builtin("gpu_set_fog").ok(),
            shader_reset: ctx.builtin("shader_reset").ok(),
        }
    }

    /// The GPU state (a `ds_map`), to put back with [`Self::restore`].
    fn save(&self, call: &Call<'_>) -> Option<Value> {
        call.call_builtin(self.get_state?, &[]).ok()
    }

    fn restore(&self, call: &Call<'_>, saved: Option<Value>) {
        let Some(map) = saved else { return };
        if let Some(s) = self.set_state {
            let _ = call.call_builtin(s, std::slice::from_ref(&map));
        }
        if let Some(d) = self.map_destroy {
            let _ = call.call_builtin(d, &[map]);
        }
    }

    /// After a Draw stopped by an error: normal blending, no fog, no shader.
    fn reset(&self, call: &Call<'_>) {
        if let Some(b) = self.blendmode {
            let _ = call.call_builtin(b, &[0.0.into()]);
        }
        if let Some(f) = self.fog {
            let _ = call.call_builtin(f, &[Value::Bool(false), 0.0.into(), 0.0.into(), 1.0.into()]);
        }
        if let Some(s) = self.shader_reset {
            let _ = call.call_builtin(s, &[]);
        }
    }
}

/// A trial's outcome for one code object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Trial {
    pub safe: bool,
    pub names: Vec<String>,
    /// Why not safe (the runner's message, shortened).
    pub why: String,
}

/// Owner state.
#[derive(Debug, Default)]
pub(super) struct OwnerDraw {
    /// Objects announced as drawing with code (instance object).
    announced: HashSet<u32>,
    /// Instance object → code object, as announced.
    code_of: HashMap<u32, u32>,
    trials: HashMap<u32, Trial>,
    /// Code objects being tried.
    trying: HashMap<u32, Trying>,
    /// Per code object: the variable count and settledness last sent.
    names_sent: HashMap<u32, (usize, bool)>,
    /// The draw values as sent (`MirrorDraw::Edits`).
    values: DrawSender,
    tick: u32,
    /// Other objects' variables sent (`MirrorDraw::Reads`), as sent.
    reads_sent: HashMap<String, StateValue>,
    /// Per settled code object: the send tick it settled at.
    settled_at: HashMap<u32, u32>,
    /// The game was frozen since the last send tick.
    was_frozen: bool,
    /// Per code object: the result last logged.
    logged: HashMap<u32, Trial>,
}

impl OwnerDraw {
    /// Whether `c`'s settled result differs from the one last logged
    /// (recording it), so a retry that settles the same is not logged.
    fn result_is_new(&mut self, c: u32, t: &Trial) -> bool {
        if self.logged.get(&c) == Some(t) {
            return false;
        }
        self.logged.insert(c, t.clone());
        true
    }

    /// The game is frozen this frame (no send tick runs).
    pub(super) fn frozen(&mut self) {
        self.was_frozen = true;
    }

    /// Try settled code object `c` again; its result stands meanwhile.
    fn retry(&mut self, c: u32) {
        if let Some(t) = self.trials.remove(&c) {
            self.trying.insert(
                c,
                Trying {
                    done: 0,
                    first: self.tick,
                    next: self.tick,
                    merged: t,
                    again: true,
                },
            );
        }
    }

    /// Settled code objects of `live` due another trial: all after a freeze,
    /// else those settled [`RETRY_EVERY`] send ticks ago.
    fn retries_due(&mut self, live: &HashSet<u32>) -> Vec<u32> {
        let thawed = std::mem::take(&mut self.was_frozen);
        let mut due: Vec<u32> = self
            .trials
            .keys()
            .filter(|c| live.contains(c))
            .filter(|c| {
                thawed
                    || self
                        .settled_at
                        .get(c)
                        .is_none_or(|t| self.tick >= t + RETRY_EVERY)
            })
            .copied()
            .collect();
        due.sort_unstable();
        due
    }
    /// `c`'s result as it stands and whether it is settled; with
    /// `provisional`, the union so far once a trial ran clean.
    fn current(&self, c: u32, provisional: bool) -> Option<(&Trial, bool)> {
        self.trials.get(&c).map(|t| (t, true)).or_else(|| {
            self.trying
                .get(&c)
                .filter(|t| (provisional || t.again) && t.merged.safe)
                .map(|t| (&t.merged, false))
        })
    }
}

/// A code object between its first trial and its settled result.
#[derive(Debug)]
struct Trying {
    done: u32,
    first: u32,
    next: u32,
    merged: Trial,
    /// Tried again after it had settled.
    again: bool,
}

impl Trying {
    /// Fold a trial in: safe if any ran clean; variables in first-found order.
    fn add(&mut self, t: Trial) {
        self.done += 1;
        if t.safe {
            if !self.merged.safe {
                self.merged.safe = true;
                self.merged.names.clear();
                self.merged.why.clear();
            }
            for n in t.names {
                if !self.merged.names.contains(&n) {
                    self.merged.names.push(n);
                }
            }
        } else if !self.merged.safe {
            self.merged.why = t.why;
        }
    }
}

/// Viewer state.
#[derive(Debug, Default)]
pub(super) struct ViewerDraw {
    /// Instance object → code object (the owner's announcement).
    drawn: HashMap<u16, u16>,
    /// Code object -> (safe, variables, settled).
    names: HashMap<u16, (bool, Vec<String>, bool)>,
    vals: HashMap<u16, Vec<Option<StateValue>>>,
    dirty: HashSet<u16>,
    /// Code objects whose code failed here [`FAIL_LIMIT`] times, and why.
    failed: HashMap<u16, String>,
    /// Failures and draws by its code per code object so far.
    fails: HashMap<u16, u32>,
    runs: HashMap<u16, u32>,
    /// Key → the proxy instance (id key) its values were last set on.
    applied: HashMap<u16, u64>,
    /// Proxy instance (id key) → (key, object).
    pub(super) proxy_of: HashMap<u64, (u16, u16)>,
    /// Values not yet due: key -> owner time -> pieces ([`Self::advance`]).
    incoming: HashMap<u16, BTreeMap<u32, Tick>>,
    /// The viewer's running render time ([`Self::advance`]): the shown
    /// time's steps from one drawn frame to the next, each counted up to
    /// [`RUN_STEP_MS`]. Then the last shown time it stepped from, and per key
    /// the running time its newest values came at.
    run_ms: u32,
    last_shown: Option<u32>,
    came_at: HashMap<u16, u32>,
    /// Ticks applied, dropped incomplete, keys started over, dropped late.
    pub(super) ticks: [u64; 4],
    /// Per key: the owner time of the newest tick applied.
    applied_at: HashMap<u16, u32>,
    /// The owner's values of other objects' variables (`"object.variable"`).
    reads: HashMap<String, StateValue>,
}

/// One key's values of one owner time, as its pieces arrive.
#[derive(Debug, Default)]
struct Tick {
    pieces: Vec<Option<KeyVars<ArrayEdit>>>,
    /// Time since the key's previous send (0: never; `None`: not given).
    after: Option<u32>,
}

impl Tick {
    fn complete(&self) -> bool {
        self.pieces.iter().all(Option::is_some)
    }
}

/// Draw code that reads another object's variables: code object, and the
/// (object, variable) pairs it reads. The owner sends those values.
pub(super) const FOREIGN_READS: &[(&str, &[(&str, &str)])] = &[
    ("oConstructAlly", &[("oPowers", "constructNearby")]),
    ("oConstructSwarm", &[("oPowers", "constructNearby")]),
    (
        "oConstructTurret",
        &[("oPowers", "constructNearby"), ("oPowers", "pointDefense")],
    ),
    (
        "oSanctuaryIndicator",
        &[
            ("oPowers", "sanctuaryColBG"),
            ("oPowers", "sanctuaryColBright"),
        ],
    ),
];

/// Ticks kept per key while waiting for their time (about 2 s of sends).
const TICKS_KEPT: usize = 64;
/// A key with nothing drawn this long after its newest values is forgotten
/// (running render time: a hold does not count).
const INCOMING_STALE_MS: u32 = 3000;
/// The most render time one drawn frame adds to the running time: more is a
/// hold or a stall the viewer did not run through.
const RUN_STEP_MS: u32 = 250;

impl ViewerDraw {
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn on_message(&mut self, m: MirrorDraw) {
        match m {
            MirrorDraw::Drawn { objects, .. } => {
                for (o, c) in objects {
                    self.drawn.insert(o, c);
                }
            }
            MirrorDraw::Names {
                object,
                safe,
                names,
                settled,
                ..
            } => {
                // More variables: the failures so far may have been those.
                if self
                    .names
                    .get(&object)
                    .is_none_or(|(_, n, _)| n.len() != names.len())
                {
                    self.fails.remove(&object);
                    self.failed.remove(&object);
                }
                self.names.insert(object, (safe, names, settled));
            }
            MirrorDraw::Reads { vals, .. } => {
                for (k, v) in &vals {
                    log::info!("co-op mirror draw: the owner's {k} = {v:?}");
                }
                self.reads.extend(vals);
            }
            // Values in any form.
            m @ (MirrorDraw::Vars { .. } | MirrorDraw::Ticks { .. } | MirrorDraw::Edits { .. }) => {
                if let Ok((_, time_ms, keys)) = values_of(m) {
                    self.on_ticks(time_ms, keys.into_iter());
                }
            }
        }
    }

    /// One part's values of owner time `time_ms`.
    fn on_ticks(
        &mut self,
        time_ms: u32,
        keys: impl Iterator<Item = (Option<u32>, KeyVars<ArrayEdit>)>,
    ) {
        for (after, k) in keys {
            let (key, i, n) = (k.key, usize::from(k.piece), usize::from(k.pieces));
            self.came_at.insert(key, self.run_ms);
            let ticks = self.incoming.entry(key).or_default();
            let tick = ticks.entry(time_ms).or_insert_with(|| Tick {
                pieces: vec![None; n.max(1)],
                after,
            });
            if tick.pieces.len() == n
                && let Some(p) = tick.pieces.get_mut(i)
            {
                *p = Some(k);
            }
            // Too many waiting: apply the oldest now, in order.
            let mut over = Vec::new();
            while ticks.len() > TICKS_KEPT {
                over.extend(ticks.pop_first());
            }
            for (time, t) in over {
                self.apply_tick(key, time, t);
            }
        }
    }

    /// Apply every tick of `key` up to `at`, oldest first. An incomplete tick
    /// waits while it is the newest, else it is dropped.
    fn advance_key(&mut self, key: u16, at: u32) {
        let Some(ticks) = self.incoming.get_mut(&key) else {
            return;
        };
        let mut due = Vec::new();
        while let Some((&time, tick)) = ticks.first_key_value() {
            if time > at || (!tick.complete() && ticks.len() == 1) {
                break;
            }
            due.extend(ticks.pop_first());
        }
        for (time, t) in due {
            self.apply_tick(key, time, t);
        }
    }

    /// Bring drawn keys' values to their poses' times; forget stale keys.
    pub(super) fn advance(&mut self, shown: impl IntoIterator<Item = (u16, u32)>, t: u32) {
        let step = self
            .last_shown
            .map_or(0, |l| t.saturating_sub(l).min(RUN_STEP_MS));
        self.run_ms = self.run_ms.saturating_add(step);
        self.last_shown = Some(t);
        for (key, at) in shown {
            self.advance_key(key, at);
        }
        let now = self.run_ms;
        let came_at = &self.came_at;
        self.incoming.retain(|key, _| {
            came_at
                .get(key)
                .is_some_and(|c| c.saturating_add(INCOMING_STALE_MS) >= now)
        });
        let incoming = &self.incoming;
        self.came_at.retain(|key, _| incoming.contains_key(key));
    }

    /// Apply `key`'s values of owner time `time`. After a missed send the
    /// values start over and the key draws plain until all have come again.
    fn apply_tick(&mut self, key: u16, time: u32, tick: Tick) {
        if !tick.complete() {
            self.ticks[1] += 1;
            return;
        }
        let newest = self.applied_at.get(&key).copied();
        if newest.is_some_and(|n| time <= n) {
            self.ticks[3] += 1;
            return;
        }
        self.ticks[0] += 1;
        self.applied_at.insert(key, time);
        let v = self.vals.entry(key).or_default();
        if let Some(after) = tick.after {
            let previous = (after > 0).then(|| time.wrapping_sub(after));
            if previous != newest {
                v.clear();
                // `after` 0: a reused key, nothing was lost.
                if after > 0 {
                    self.ticks[2] += 1;
                }
            }
        }
        for k in tick.pieces.into_iter().flatten() {
            for (slot, val) in k.vals {
                let i = usize::from(slot);
                if v.len() <= i {
                    v.resize(i + 1, None);
                }
                v[i] = Some(val);
            }
            // A change to an array not held here empties the slot until
            // the next full value.
            for d in k.deltas {
                let i = usize::from(d.slot);
                if v.len() <= i {
                    v.resize(i + 1, None);
                }
                let new = match &v[i] {
                    Some(StateValue::Array(old)) => d.apply(old),
                    _ => None,
                };
                v[i] = new.map(StateValue::Array);
            }
        }
        self.dirty.insert(key);
    }

    /// Whether proxies of `object` are drawn by the owner's code here.
    pub(super) fn runs_code(&self, object: u16) -> bool {
        self.drawn.get(&object).is_some_and(|c| {
            !self.failed.contains_key(c) && self.names.get(c).is_some_and(|(safe, ..)| *safe)
        })
    }

    pub(super) fn forget(&mut self, key: u16) {
        self.incoming.remove(&key);
        self.came_at.remove(&key);
        self.vals.remove(&key);
        self.dirty.remove(&key);
        self.applied.remove(&key);
        self.applied_at.remove(&key);
    }

    /// Whether every one of `n` slots of `key` holds a value.
    fn ready(&self, key: u16, n: usize) -> bool {
        n == 0
            || self
                .vals
                .get(&key)
                .is_some_and(|v| v.len() >= n && v.iter().take(n).all(Option::is_some))
    }
}

/// The observer's map, apart from the state lock (it runs inside events).
#[derive(Debug, Default)]
pub(super) struct Observed {
    pub(super) code_of: Mutex<HashMap<u32, u32>>,
}

/// The instance's own variables each code object's Draw reads on any path
/// (its `Draw_0`, the scripts it calls three deep, the ancestor's Draw an
/// `event_inherited` runs): every trial of that code keeps those, besides
/// what it finds. A trial sees only the path its instance takes. An entry
/// also covers the object's children without a Draw of their own.
pub(super) const PATH_READS: &[(&str, &[&str])] = &[
    ("oBoltThrowerSlice", &["blend"]),
    ("oBurstGrowDamaging", &["blend"]),
    ("oBurstGrowDynamic", &["blend"]),
    ("oBurstGrowPos", &["blend"]),
    ("oBurstQuick", &["blend"]),
    ("oBurstQuickNoDamage", &["blend"]),
    ("oBurstRetaliate", &["blendAdd"]),
    ("oChain", &["blend"]),
    ("oCharge", &["blend"]),
    ("oConstructAlly", &["col", "nearby"]),
    ("oConstructSwarm", &["col", "nearby", "rarity"]),
    ("oConstructTurret", &["col", "nearby"]),
    ("oEnemyAoEReflectedObjectClone", &["object_index_original"]),
    (
        "oEnemyBlastReflectedObjectClone",
        &["object_index_original"],
    ),
    (
        "oEnemyProjectileReflectedObjectClone",
        &["object_index_original"],
    ),
    ("oFlash", &["colorWhite"]),
    ("oFollowingGraphic", &["blend"]),
    ("oFollowingGraphicPulsating", &["blend"]),
    ("oFollowingGraphicPulsing", &["blend"]),
    (
        "oPlayerTrails",
        &[
            "DrawTriangleNode",
            "NodeCount",
            "TrailD",
            "TrailLength",
            "TrailT",
            "TrailTime",
            "TrailX",
            "TrailY",
            "trailAlpha0",
            "trailAlpha1",
            "trailColor0",
            "trailColor1",
            "trailSprite",
            "trailWidth0",
            "trailWidth1",
        ],
    ),
    ("oPlayerVisibility", &["blend"]),
    (
        "oRailTrail",
        &[
            "DrawTriangleNode",
            "NodeCount",
            "TrailD",
            "TrailLength",
            "TrailT",
            "TrailTime",
            "TrailX",
            "TrailY",
            "trailAlpha0",
            "trailAlpha1",
            "trailColor0",
            "trailColor1",
            "trailSprite",
            "trailWidth0",
            "trailWidth1",
        ],
    ),
    (
        "oRailgun",
        &[
            "DrawTriangleNode",
            "NodeCount",
            "TrailD",
            "TrailLength",
            "TrailT",
            "TrailTime",
            "TrailX",
            "TrailY",
        ],
    ),
    (
        "oRainbowEffect",
        &[
            "col",
            "flashAlpha",
            "flashAlpha2",
            "flashAlpha3",
            "flashScale",
            "flashScale2",
            "flashScale3",
            "flashScale4",
            "graphic",
        ],
    ),
    ("oSanctuaryIndicator", &["shieldSize"]),
    (
        "oSwordObject",
        &[
            "ColAdd",
            "ColMake",
            "ColMult",
            "ColSplit",
            "attackDone",
            "charging",
            "controllerFinishIdle",
            "controllerStartIdle",
            "debugColor",
            "debugPos",
            "firing",
            "getController",
            "goalX",
            "goalY",
            "hasSwapped",
            "leftSide",
            "mgmtSwordIndex",
            "powered",
            "recovering",
            "relativeToObjective",
            "startLeftSide",
        ],
    ),
    ("oSwordSpark", &["blend"]),
    (
        "oTrail",
        &[
            "DrawTriangleNode",
            "NodeCount",
            "TrailD",
            "TrailLength",
            "TrailT",
            "TrailTime",
            "TrailX",
            "TrailY",
            "trailAlpha0",
            "trailAlpha1",
            "trailColor0",
            "trailColor1",
            "trailSprite",
            "trailWidth0",
            "trailWidth1",
        ],
    ),
    (
        "oTrailRay",
        &[
            "DrawTriangleNode",
            "NodeCount",
            "TrailD",
            "TrailLength",
            "TrailT",
            "TrailTime",
            "TrailX",
            "TrailY",
            "trailAlpha0",
            "trailAlpha1",
            "trailColor0",
            "trailColor1",
            "trailWidth0",
            "trailWidth1",
        ],
    ),
    (
        "oUnlockable",
        &[
            "alpha",
            "col",
            "fade",
            "flashAlpha",
            "flashAlpha2",
            "flashAlpha3",
            "flashScale",
            "flashScale2",
            "flashScale3",
            "flashScale4",
            "graphic",
            "graphicBack",
            "hueCount",
            "scale",
            "textDesc",
            "textPosX",
            "textPosY",
            "useFullDesc",
        ],
    ),
    ("oVortex", &["blend"]),
    ("oVortexCharge", &["blend"]),
    ("oVortexTempest", &["blend"]),
    (
        "obj_Shockwave",
        &[
            "wave_additive",
            "wave_alpha",
            "wave_blend",
            "wave_max_radius",
            "wave_push",
            "wave_radius",
            "wave_segment",
            "wave_speed",
            "wave_surf",
            "wave_surf_h",
            "wave_surf_t",
            "wave_surf_w",
            "wave_update",
            "wave_vb_inside",
            "wave_vb_outside",
            "wave_view",
            "wave_width",
        ],
    ),
];

/// `found` plus the [`PATH_READS`] the model has, in the model's order.
fn with_path_reads(
    mut found: Vec<(String, StateValue)>,
    all: &[(String, StateValue)],
    path: &[&str],
) -> Vec<(String, StateValue)> {
    for (n, v) in all {
        if path.contains(&n.as_str()) && !found.iter().any(|(f, _)| f == n) {
            found.push((n.clone(), v.clone()));
        }
    }
    found
}

/// `Variable <object>.<name>(...) not set before reading it`: (object, name).
#[must_use]
pub fn missing_var(text: &str) -> Option<(String, String)> {
    let i = text.find("Variable ")? + "Variable ".len();
    let rest = &text[i..];
    let end = rest.find(['(', ' '])?;
    let full = &rest[..end];
    if !text[i + end..].contains("not set before reading") {
        return None;
    }
    let (obj, name) = full.rsplit_once('.')?;
    Some((obj.to_owned(), name.to_owned()))
}

/// The runner's message on one line, its "ERROR" words lowered (expected).
fn short(text: &str) -> String {
    let t = text.trim().replace('\n', " | ");
    let t = t
        .strip_prefix("ERROR in action number 1 | ")
        .unwrap_or(&t)
        .replace(" ERROR ", " error ");
    t.chars().take(160).collect()
}

impl MirrorHooks {
    /// The Draw observer (owner) and the painter's Draw hook (viewer).
    pub(super) fn register_draw(ctx: &ModContext, me: &Arc<Self>) {
        let m = Arc::clone(me);
        let r = ctx.observe_event(8, 0, move |e| {
            if e.after {
                return;
            }
            let (Ok(o), Ok(c)) = (u32::try_from(e.instance_object), u32::try_from(e.object)) else {
                return;
            };
            let mut map = m
                .observed
                .code_of
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            map.entry(o).or_insert(c);
        });
        if let Err(e) = r {
            log::warn!("co-op mirror: draw observer: {e}; partners' things keep the default draw");
        }
        let m = Arc::clone(me);
        if let Err(e) = ctx.hook_before(PAINTER_DRAW, move |call| m.painter_draw(call)) {
            log::warn!(
                "co-op mirror: {PAINTER_DRAW} hook: {e}; partners' things keep the default draw"
            );
        }
    }

    /// Owner, after the frame's reads: announce, try, send values. `ids`:
    /// (instance key, object, id) of every keyed instance this frame.
    pub(super) fn owner_draw(
        &self,
        call: &Call<'_>,
        me: PeerId,
        now_ms: u32,
        ids: &[(usize, u32, Value)],
    ) -> Vec<Message> {
        crate::guard::confirm(call);
        if !self.settings.draw || !crate::guard::calls_contained() {
            return Vec::new();
        }
        let observed = self
            .observed
            .code_of
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut out = Vec::new();
        // Announce objects seen drawing with code.
        let (painter, attack) = {
            let st = self.lock();
            (st.painter, st.attack)
        };
        let new: Vec<(u32, u32)> = {
            let mut st = self.lock();
            let mut new = Vec::new();
            for (_, o, _) in ids {
                // Screen UI (`oGuiObject` and below) is the owner's own HUD.
                if Some(*o) == painter || Some(*o) == attack || st.gui.contains(o) {
                    continue;
                }
                if let Some(&c) = observed.get(o)
                    && st.draw_owner.announced.insert(*o)
                {
                    st.draw_owner.code_of.insert(*o, c);
                    new.push((*o, c));
                }
            }
            new
        };
        if !new.is_empty() {
            out.push(Message::MirrorDraw(MirrorDraw::Drawn {
                peer: me,
                objects: new.iter().map(|&(o, c)| (o as u16, c as u16)).collect(),
            }));
        }
        // Settled code objects due another trial.
        {
            let mut st = self.lock();
            let o = &mut st.draw_owner;
            let live: HashSet<u32> = ids
                .iter()
                .filter_map(|(_, obj, _)| o.code_of.get(obj).copied())
                .collect();
            for c in o.retries_due(&live) {
                o.retry(c);
            }
        }
        // Trials for unsettled code objects, each on another live instance.
        let todo: Vec<(u32, Value)> = {
            let st = self.lock();
            let o = &st.draw_owner;
            let mut models: HashMap<u32, Vec<&Value>> = HashMap::new();
            for (_, obj, id) in ids {
                if let Some(&c) = o.code_of.get(obj)
                    && !o.trials.contains_key(&c)
                    && o.trying.get(&c).is_none_or(|t| t.next <= o.tick)
                {
                    models.entry(c).or_default().push(id);
                }
            }
            let mut todo: Vec<(u32, Value)> = models
                .into_iter()
                .map(|(c, m)| {
                    let done = o.trying.get(&c).map_or(0, |t| t.done) as usize;
                    (c, m[(done * 7) % m.len()].clone())
                })
                .collect();
            todo.sort_by_key(|(c, _)| *c);
            todo.truncate(TRIALS_PER_TICK);
            todo
        };
        for (c, model) in todo {
            let t = self.trial(call, c, &model);
            let settled = {
                let mut st = self.lock();
                let tick = st.draw_owner.tick;
                let tr = st.draw_owner.trying.entry(c).or_insert_with(|| Trying {
                    done: 0,
                    first: tick,
                    next: tick,
                    merged: Trial {
                        safe: false,
                        names: Vec::new(),
                        why: String::new(),
                    },
                    again: false,
                });
                tr.add(t);
                tr.next = tick + TRIAL_SPACING;
                if tr.done >= TRIALS_EACH {
                    st.draw_owner.trying.remove(&c).map(|t| t.merged)
                } else {
                    None
                }
            };
            if let Some(t) = settled {
                self.settle(call, c, t);
            }
        }
        // Code objects whose later trials found no instance: settled now.
        let late: Vec<(u32, Trial)> = {
            let mut st = self.lock();
            let tick = st.draw_owner.tick;
            let due: Vec<u32> = st
                .draw_owner
                .trying
                .iter()
                .filter(|(_, t)| tick >= t.first + TRIAL_SETTLE)
                .map(|(c, _)| *c)
                .collect();
            due.into_iter()
                .filter_map(|c| st.draw_owner.trying.remove(&c).map(|t| (c, t.merged)))
                .collect()
        };
        for (c, t) in late {
            self.settle(call, c, t);
        }
        // Names: sent when the variable count or settledness changes.
        {
            let mut st = self.lock();
            let o = &st.draw_owner;
            let foreign = &st.foreign;
            let codes: Vec<u32> = o.trials.keys().chain(o.trying.keys()).copied().collect();
            let ready: Vec<(u32, Trial, bool)> = codes
                .into_iter()
                .filter_map(|c| {
                    let (t, settled) = o.current(c, !foreign.contains_key(&c))?;
                    (o.names_sent.get(&c) != Some(&(t.names.len(), settled)))
                        .then(|| (c, t.clone(), settled))
                })
                .collect();
            for (c, t, settled) in ready {
                st.draw_owner.names_sent.insert(c, (t.names.len(), settled));
                out.push(Message::MirrorDraw(MirrorDraw::Names {
                    peer: me,
                    object: c as u16,
                    safe: t.safe,
                    names: t.names,
                    settled,
                }));
            }
        }
        out.extend(self.owner_reads(call, me));
        out.extend(self.owner_vars(call, me, now_ms, ids));
        out
    }

    /// Find the variables of `model` that `code`'s Draw needs: those named by
    /// "variable not set" errors, else all of them reduced by halving groups.
    fn trial(&self, call: &Call<'_>, code: u32, model: &Value) -> Trial {
        let fail = |why: &str| Trial {
            safe: false,
            names: Vec::new(),
            why: why.to_owned(),
        };
        if !crate::guard::calls_contained() {
            return fail("the guard is off");
        }
        let (Some(r), Some(painter)) = (self.dh.reflect, self.lock().painter) else {
            return fail("builtins missing");
        };
        let painter_name = self
            .lock()
            .names
            .get(&painter)
            .cloned()
            .unwrap_or_else(|| PAINTER.to_owned());
        let mut stats = state::Stats::default();
        let all = state::capture(call, &r, model, &|_, _| None, &mut stats).vars;
        let path: &[&str] = self.path_reads_of(call, code);
        let gpu = self.dh.save(call);
        let mut runs = 0u32;
        let mut run = |vars: &[(String, StateValue)]| {
            runs += 1;
            self.trial_run(call, code, model, vars)
        };
        let mut needed = with_path_reads(Vec::new(), &all, path);
        let named = loop {
            let text = match run(&needed) {
                Ok(()) => break Ok(()),
                Err(t) => t,
            };
            match missing_var(&text) {
                Some((obj, name))
                    if obj == painter_name
                        && needed.len() < TRIAL_VARS
                        && !needed.iter().any(|(n, _)| *n == name) =>
                {
                    match all.iter().find(|(n, _)| *n == name) {
                        Some(v) => needed.push(v.clone()),
                        None => break Err(format!("reads {name}, which the instance lacks")),
                    }
                }
                Some((obj, name)) if obj != painter_name => {
                    break Err(format!("reads {obj}.{name}"));
                }
                _ => break Err(text),
            }
        };
        let result = match named {
            Ok(()) => Ok(needed),
            Err(first) => match run(&all) {
                Err(t) => Err(if missing_var(&first).is_some() {
                    first
                } else {
                    t
                }),
                Ok(()) => {
                    // Drop groups the code runs without, halving the size.
                    let mut keep = all.clone();
                    let mut chunk = keep.len().div_ceil(4).max(1);
                    loop {
                        let mut i = 0;
                        while i < keep.len() {
                            let end = (i + chunk).min(keep.len());
                            let mut without = keep.clone();
                            without.drain(i..end);
                            if run(&without).is_ok() {
                                keep = without;
                            } else {
                                i = end;
                            }
                        }
                        if chunk == 1 {
                            break;
                        }
                        chunk = chunk.div_ceil(2);
                    }
                    Ok(with_path_reads(keep, &all, path))
                }
            },
        };
        self.dh.restore(call, gpu);
        log::debug!("co-op mirror draw: trial of object {code}: {runs} runs");
        match result {
            Ok(vars) => Trial {
                safe: true,
                names: vars.into_iter().map(|(n, _)| n).collect(),
                why: String::new(),
            },
            Err(why) => Trial {
                safe: false,
                names: Vec::new(),
                why: short(&why),
            },
        }
    }

    /// One run of `code`'s Draw on a scratch painter posed as `model` with
    /// `vars`; `Err` holds the runner's message.
    fn trial_run(
        &self,
        call: &Call<'_>,
        code: u32,
        model: &Value,
        vars: &[(String, StateValue)],
    ) -> Result<(), String> {
        let (Some(perform), Some(r), Some(create), Some(painter)) = (
            self.dh.perform,
            self.dh.reflect,
            self.h.create,
            self.lock().painter,
        ) else {
            return Err("builtins missing".to_owned());
        };
        let Some(scratch) = call
            .call_script(
                create,
                &[
                    (-10_000.0).into(),
                    (-10_000.0).into(),
                    Value::object(painter),
                ],
            )
            .ok()
            .filter(crate::world::classify::is_instance)
        else {
            return Err("no scratch instance".to_owned());
        };
        let mut pose: Vec<(&str, f64)> = Vec::new();
        let mut sprite = None;
        let _ = call.with(model, |inst, _| {
            sprite = inst.builtin_value("sprite_index").ok();
            for n in POSE {
                if let Some(v) = inst.builtin_f64(n) {
                    pose.push((n, v));
                }
            }
        });
        let _ = call.with(&scratch, |inst, _| {
            for (n, v) in &pose {
                let _ = inst.set_builtin_f64(n, *v);
            }
        });
        if let Some(spr) = sprite {
            let _ = state::set_var(call, &r, &scratch, "sprite_index", &spr);
        }
        if !vars.is_empty() {
            let mut stats = state::Stats::default();
            state::apply(call, &r, &scratch, vars, &[], None, &|_| None, &mut stats);
        }
        crate::guard::set_aside(call);
        let mut ran = None;
        // A trial may fail: that is what it finds out.
        let expect = call.guard_expect();
        let _ = call.with(&scratch, |inst, _| {
            ran = Some(
                inst.call_builtin(perform, &[Value::object(code), EV_DRAW.into(), 0.0.into()]),
            );
        });
        drop(expect);
        let errors = crate::guard::take_calls(call);
        if let Some(d) = self.h.destroy {
            let _ = call.call_builtin(d, &[scratch, Value::Bool(false)]);
        }
        match (ran, errors.first()) {
            (None, _) => Err("the scratch instance is gone".to_owned()),
            (Some(_), Some(text)) => Err(text.clone()),
            (Some(Err(e)), None) => Err(format!("{e:?}")),
            (Some(Ok(_)), None) => Ok(()),
        }
    }

    /// The draw variables of every keyed instance with safe code, as
    /// [`DrawSender`] sends them.
    fn owner_vars(
        &self,
        call: &Call<'_>,
        me: PeerId,
        now_ms: u32,
        ids: &[(usize, u32, Value)],
    ) -> Vec<Message> {
        let Some(r) = self.dh.reflect else {
            return Vec::new();
        };
        let (work, keys) = {
            let mut st = self.lock();
            st.draw_owner.tick += 1;
            let mut work = Vec::new();
            for (inst, o, id) in ids {
                let Some(c) = st.draw_owner.code_of.get(o) else {
                    continue;
                };
                let Some((t, _)) = st.draw_owner.current(*c, !st.foreign.contains_key(c)) else {
                    continue;
                };
                if !t.safe || t.names.is_empty() {
                    continue;
                }
                let Some(key) = st.sender.key_of(*inst) else {
                    continue;
                };
                work.push((key, id.clone(), t.names.clone()));
            }
            let keys: HashMap<usize, u16> = ids
                .iter()
                .filter_map(|(i, _, _)| st.sender.key_of(*i).map(|k| (*i, k)))
                .collect();
            (work, keys)
        };
        let net_of = |i: usize, _: &Value| keys.get(&i).copied();
        let mut stats = state::Stats::default();
        let mut read: Vec<(u16, Vec<Option<StateValue>>)> = Vec::with_capacity(work.len());
        for (key, id, names) in work {
            let vals = names
                .iter()
                .map(|n| state::read_var(call, &r, &id, n, &net_of, &mut stats))
                .collect();
            read.push((key, vals));
        }
        let mut st = self.lock();
        st.draw_owner
            .values
            .tick(me, now_ms, read)
            .into_iter()
            .map(Message::MirrorDraw)
            .collect()
    }

    /// The painter's Draw (viewer): the proxies draw with the owner's code or
    /// `draw_self`; anything else runs the painter's own Draw.
    fn painter_draw(&self, call: &mut Call<'_>) {
        crate::guard::confirm(call);
        let Some(inst) = call.instance() else { return };
        let Some(idk) = inst.builtin_value("id").ok().as_ref().and_then(id_key) else {
            return;
        };
        let (key, code, vars, names, id) = {
            let mut st = self.lock();
            let Some(&(key, object)) = st.draw_view.proxy_of.get(&idk) else {
                return;
            };
            let Some(id) = st.proxies.get(&key).and_then(|p| p.id.clone()) else {
                return;
            };
            let v = &mut st.draw_view;
            // The code runs only with the guard containing the mod's calls.
            let code = v.drawn.get(&object).copied().filter(|c| {
                crate::guard::calls_contained()
                    && !v.failed.contains_key(c)
                    && v.names.get(c).is_some_and(|(safe, ..)| *safe)
            });
            let names = code
                .and_then(|c| v.names.get(&c))
                .map_or_default(|(_, n, _)| n.clone());
            // Only once every value it reads is here.
            let code = code.filter(|_| v.ready(key, names.len()));
            // Set all values when they changed or the proxy is new.
            let fresh = v.applied.get(&key) != Some(&idk);
            let vars = if code.is_some() && (v.dirty.remove(&key) || fresh) {
                v.applied.insert(key, idk);
                v.vals.get(&key).cloned()
            } else {
                None
            };
            (key, code, vars, names, id)
        };
        call.skip_original();
        let slot;
        if let (Some(c), Some(perform), Some(r)) = (code, self.dh.perform, self.dh.reflect) {
            if let Some(vals) = vars {
                let list: Vec<(String, StateValue)> = names
                    .iter()
                    .zip(vals)
                    .filter_map(|(n, v)| v.map(|v| (n.clone(), v)))
                    .collect();
                let proxies: HashMap<u16, Value> = {
                    let st = self.lock();
                    st.proxies
                        .iter()
                        .filter_map(|(k, p)| p.id.clone().map(|id| (*k, id)))
                        .collect()
                };
                let id_of = |k: u16| proxies.get(&k).cloned();
                let mut stats = state::Stats::default();
                state::apply(call, &r, &id, &list, &[], None, &id_of, &mut stats);
            }
            let own = self.foreign_swap(call, u32::from(c), None);
            crate::guard::set_aside(call);
            // Counted below (the 1 % rule).
            let expect = call.guard_expect();
            let ran = inst.call_builtin(
                perform,
                &[Value::object(u32::from(c)), EV_DRAW.into(), 0.0.into()],
            );
            drop(expect);
            self.foreign_swap(call, u32::from(c), Some(own));
            // An error inside the event abandons it and the call returns.
            let errors = crate::guard::take_calls(call);
            match (ran, errors.first()) {
                (Ok(_), None) => {
                    *self.lock().draw_view.runs.entry(c).or_default() += 1;
                    slot = 0;
                }
                (ran, first) => {
                    self.dh.reset(call);
                    let why = first.map_or_else(|| format!("{:?}", ran.err()), |t| short(t));
                    // Wait for the next full set of values.
                    let mut st = self.lock();
                    let v = &mut st.draw_view;
                    v.vals.remove(&key);
                    v.applied.remove(&key);
                    let ok = v.runs.get(&c).copied().unwrap_or(0);
                    // Failures count only once the trials are settled.
                    let settled = v.names.get(&c).is_some_and(|(.., s)| *s);
                    let n = v.fails.entry(c).or_default();
                    *n += u32::from(settled);
                    if *n >= FAIL_LIMIT && *n * FAIL_PER > ok {
                        log::warn!(
                            "co-op mirror draw: object {c}'s code failed {n} times on proxies in {ok} draws (last key {key}); default draw from now on: {why}"
                        );
                        v.failed.insert(c, why);
                    } else {
                        log_limited!(
                            log::Level::Info,
                            "mirror-draw-proxy-failed",
                            "co-op mirror draw: object {c}'s code failed on a proxy (key {key}; {n} failures in {ok} draws): {why}"
                        );
                    }
                    slot = 2;
                }
            }
        } else {
            slot = 1;
        }
        if slot != 0
            && let Some(ds) = self.dh.draw_self
        {
            let _ = inst.call_builtin(ds, &[]);
        }
    }

    /// Record and log a code object's settled result.
    fn settle(&self, call: &Call<'_>, c: u32, t: Trial) {
        // The code's object may be an ancestor the mirror never saw.
        let known = self.lock().names.get(&c).cloned();
        let name = known
            .or_else(|| {
                let n = self.h.name?;
                call.call_builtin(n, &[Value::object(c)])
                    .ok()?
                    .as_str()
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        let mut st = self.lock();
        let line = format!(
            "co-op mirror draw: {name} ({c}) {} {}{}",
            if t.safe {
                "draws with its own code, reading"
            } else {
                "is not drawn with its own code"
            },
            if t.names.is_empty() {
                "nothing of its own".to_owned()
            } else {
                t.names.join(", ")
            },
            if t.why.is_empty() {
                String::new()
            } else {
                format!(": {}", t.why)
            }
        );
        // A retry with the same result is not logged again.
        if st.draw_owner.result_is_new(c, &t) {
            log::info!("{line}");
        }
        let tick = st.draw_owner.tick;
        st.draw_owner.settled_at.insert(c, tick);
        st.draw_owner.trials.insert(c, t);
    }

    /// The first instance of object `o`.
    fn first_of(&self, call: &Call<'_>, o: u32) -> Option<Value> {
        call.call_builtin(self.h.find?, &[Value::object(o), 0.0.into()])
            .ok()
            .filter(crate::world::classify::is_instance)
    }

    /// Owner: the other objects' variables its announced Draw code reads,
    /// sent when one changed.
    fn owner_reads(&self, call: &Call<'_>, me: PeerId) -> Vec<Message> {
        let Some(r) = self.dh.reflect else {
            return Vec::new();
        };
        let wanted: Vec<(u32, &'static str, &'static str)> = {
            let st = self.lock();
            let mut w: Vec<_> = st
                .draw_owner
                .code_of
                .values()
                .filter_map(|c| st.foreign.get(c))
                .flatten()
                .copied()
                .collect();
            w.sort_unstable();
            w.dedup();
            w
        };
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut stats = state::Stats::default();
        let mut now = Vec::new();
        for (o, oname, var) in wanted {
            let Some(id) = self.first_of(call, o) else {
                continue;
            };
            if let Some(v) = state::read_var(call, &r, &id, var, &|_, _| None, &mut stats) {
                now.push((format!("{oname}.{var}"), v));
            }
        }
        let mut st = self.lock();
        let changed: Vec<(String, StateValue)> = now
            .into_iter()
            .filter(|(k, v)| st.draw_owner.reads_sent.get(k) != Some(v))
            .collect();
        if changed.is_empty() {
            return Vec::new();
        }
        st.draw_owner.reads_sent.extend(changed.iter().cloned());
        // Code reading a changed value may take a new path: try it again.
        let keys: HashSet<&str> = changed.iter().map(|(k, _)| k.as_str()).collect();
        let st = &mut *st;
        let again: Vec<u32> = st
            .foreign
            .iter()
            .filter(|(c, reads)| {
                st.draw_owner.trials.contains_key(c)
                    && reads
                        .iter()
                        .any(|(_, o, v)| keys.contains(format!("{o}.{v}").as_str()))
            })
            .map(|(c, _)| *c)
            .collect();
        for c in again {
            st.draw_owner.retry(c);
        }
        vec![Message::MirrorDraw(MirrorDraw::Reads {
            peer: me,
            vals: changed,
        })]
    }

    /// Viewer: swap in the owner's values of `code`'s foreign reads, returning
    /// the viewer's own; with `restore`, put those back.
    fn foreign_swap(
        &self,
        call: &Call<'_>,
        code: u32,
        restore: Option<Vec<(Value, String, StateValue)>>,
    ) -> Vec<(Value, String, StateValue)> {
        let Some(r) = self.dh.reflect else {
            return Vec::new();
        };
        let mut stats = state::Stats::default();
        if let Some(own) = restore {
            for (id, var, v) in own {
                state::apply(call, &r, &id, &[(var, v)], &[], None, &|_| None, &mut stats);
            }
            return Vec::new();
        }
        let todo: Vec<(u32, &'static str, StateValue)> = {
            let st = self.lock();
            let Some(reads) = st.foreign.get(&code) else {
                return Vec::new();
            };
            reads
                .iter()
                .filter_map(|&(o, oname, var)| {
                    let v = st.draw_view.reads.get(&format!("{oname}.{var}"))?;
                    Some((o, var, v.clone()))
                })
                .collect()
        };
        let mut own = Vec::new();
        for (o, var, v) in todo {
            let Some(id) = self.first_of(call, o) else {
                continue;
            };
            let Some(mine) = state::read_var(call, &r, &id, var, &|_, _| None, &mut stats) else {
                continue;
            };
            state::apply(
                call,
                &r,
                &id,
                &[(var.to_owned(), v)],
                &[],
                None,
                &|_| None,
                &mut stats,
            );
            own.push((id, var.to_owned(), mine));
        }
        own
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::mirror::ArrayDelta;

    fn arr(from: u32) -> Vec<StateValue> {
        (from..from + 40)
            .map(|i| StateValue::Real(f64::from(i)))
            .collect()
    }

    fn vars(time_ms: u32, keys: Vec<KeyVars>) -> MirrorDraw {
        MirrorDraw::Vars {
            peer: PeerId(1),
            part: 0,
            time_ms,
            keys,
        }
    }

    fn piece(key: u16, piece: u8, pieces: u8, slot: u8, v: Vec<StateValue>) -> KeyVars {
        KeyVars {
            piece,
            pieces,
            ..KeyVars::whole(key, vec![(slot, StateValue::Array(v))], Vec::new())
        }
    }

    #[test]
    fn more_variables_from_a_later_trial_clear_the_failures() {
        let names = |n: &[&str], settled: bool| MirrorDraw::Names {
            peer: PeerId(1),
            object: 808,
            safe: true,
            names: n.iter().map(|x| (*x).to_owned()).collect(),
            settled,
        };
        let mut v = ViewerDraw::default();
        v.on_message(names(&["NodeCount"], false));
        v.fails.insert(808, 5);
        v.failed.insert(808, "STRING argument is unset".into());
        // The same set again (settled): the failures stand.
        v.on_message(names(&["NodeCount"], true));
        assert_eq!(v.fails.get(&808), Some(&5));
        // More variables: a fresh start.
        v.on_message(names(&["NodeCount", "TrailX"], true));
        assert!(v.fails.is_empty() && v.failed.is_empty());
        assert!(v.names[&808].2, "settled");
    }

    #[test]
    fn a_retry_that_settles_as_before_is_no_event_line() {
        let trail = Trial {
            safe: true,
            names: vec!["NodeCount".into()],
            why: String::new(),
        };
        let mut o = OwnerDraw::default();
        assert!(o.result_is_new(230, &trail), "the first result");
        assert!(!o.result_is_new(230, &trail), "the same after a retry");
        assert!(o.result_is_new(240, &trail), "another code object");
        let grown = Trial {
            names: vec!["NodeCount".into(), "TrailX".into()],
            ..trail.clone()
        };
        assert!(o.result_is_new(230, &grown), "more variables");
        assert!(!o.result_is_new(230, &grown));
        assert!(o.result_is_new(230, &trail), "back to fewer is new too");
    }

    #[test]
    fn a_settled_code_is_tried_again_after_a_freeze_and_after_a_while() {
        let trail = Trial {
            safe: true,
            names: vec!["NodeCount".into()],
            why: String::new(),
        };
        let mut o = OwnerDraw {
            tick: 100,
            ..OwnerDraw::default()
        };
        o.trials.insert(230, trail.clone());
        o.settled_at.insert(230, 100);
        o.trials.insert(240, trail.clone());
        o.settled_at.insert(240, 100);
        let live: HashSet<u32> = [230].into();
        o.tick = 101;
        assert!(o.retries_due(&live).is_empty());
        // The upgrade screen (new gear): every live settled code again.
        o.frozen();
        assert_eq!(o.retries_due(&live), [230]);
        o.retry(230);
        assert!(!o.trials.contains_key(&230));
        let t = &o.trying[&230];
        assert!(t.again && t.merged == trail && t.done == 0);
        // Its result stands meanwhile, unsettled.
        assert_eq!(
            o.current(230, false).map(|(t, s)| (t.names.len(), s)),
            Some((1, false))
        );
        // The freeze is taken once; a while later, the rest (if live).
        assert!(o.retries_due(&live).is_empty());
        o.tick = 100 + RETRY_EVERY;
        assert!(o.retries_due(&live).is_empty(), "240 has no live instance");
        assert_eq!(o.retries_due(&[230, 240].into()), [240]);
    }

    #[test]
    fn trials_merge_to_the_union_of_the_clean_ones() {
        let t = |safe: bool, names: &[&str], why: &str| Trial {
            safe,
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            why: why.to_owned(),
        };
        let mut tr = Trying {
            done: 0,
            first: 0,
            next: 0,
            merged: t(false, &[], ""),
            again: false,
        };
        // A young trail: its code reads only three variables.
        tr.add(t(true, &["NodeCount", "trailAlpha0", "trailAlpha1"], ""));
        // An older one also reads its arrays; the first three keep their slots.
        tr.add(t(true, &["TrailX", "NodeCount", "TrailY"], ""));
        // A trial that failed on its model changes nothing.
        tr.add(t(false, &[], "reads TrailD, which the instance lacks"));
        assert_eq!(tr.done, 3);
        assert!(tr.merged.safe && tr.merged.why.is_empty());
        assert_eq!(
            tr.merged.names,
            [
                "NodeCount",
                "trailAlpha0",
                "trailAlpha1",
                "TrailX",
                "TrailY"
            ]
        );
        // Failed first, then clean: clean.
        let mut tr = Trying {
            done: 0,
            first: 0,
            next: 0,
            merged: t(false, &[], ""),
            again: false,
        };
        tr.add(t(false, &[], "bool argument is unset"));
        assert!(!tr.merged.safe && tr.merged.why == "bool argument is unset");
        tr.add(t(true, &["col"], ""));
        assert!(tr.merged.safe && tr.merged.names == ["col"] && tr.merged.why.is_empty());
    }

    #[test]
    fn the_owners_reads_of_other_objects_are_kept_and_go_reliably() {
        let mut v = ViewerDraw::default();
        let m = MirrorDraw::Reads {
            peer: PeerId(1),
            vals: vec![("oPowers.constructNearby".into(), StateValue::Real(1.0))],
        };
        assert_eq!(
            Message::MirrorDraw(m.clone()).channel(),
            ndml_net::msg::Channel::Reliable
        );
        v.on_message(m);
        v.on_message(MirrorDraw::Reads {
            peer: PeerId(1),
            vals: vec![("oPowers.pointDefense".into(), StateValue::Real(0.0))],
        });
        assert_eq!(
            v.reads.get("oPowers.constructNearby"),
            Some(&StateValue::Real(1.0))
        );
        assert_eq!(v.reads.len(), 2);
        // Every code object in the table reads something.
        assert!(FOREIGN_READS.iter().all(|(_, r)| !r.is_empty()));
    }

    #[test]
    fn a_keys_values_apply_whole_and_at_their_time() {
        let mut v = ViewerDraw::default();
        // Tick 100: two pieces (x and y points), the second arriving last.
        v.on_message(vars(100, vec![piece(7, 0, 2, 0, arr(0))]));
        v.advance([(7, 100)], 100);
        assert!(!v.ready(7, 2), "half a tick is not applied");
        v.on_message(vars(100, vec![piece(7, 1, 2, 1, arr(0))]));
        // Tick 116 is here, but the pose drawn is still that of 100.
        let d = ArrayDelta::between(0, &arr(0), &arr(1)).unwrap();
        v.on_message(vars(116, vec![KeyVars::whole(7, Vec::new(), vec![d])]));
        v.advance([(7, 100)], 100);
        assert!(v.ready(7, 2));
        assert_eq!(v.vals[&7][0], Some(StateValue::Array(arr(0))));
        v.advance([(7, 116)], 116);
        assert_eq!(v.vals[&7][0], Some(StateValue::Array(arr(1))));
        // Tick 133 lost a piece: it waits while it is the newest, and is
        // dropped once tick 150 is in; the values stay those of 116, both
        // arrays from one moment.
        v.on_message(vars(133, vec![piece(7, 1, 2, 1, arr(2))]));
        v.advance([(7, 140)], 140);
        assert_eq!(v.ticks, [2, 0, 0, 0]);
        v.on_message(vars(150, vec![piece(7, 1, 2, 1, arr(3))]));
        v.advance([(7, 140)], 140);
        assert_eq!(v.vals[&7][0], Some(StateValue::Array(arr(1))));
        assert_eq!(v.vals[&7][1], Some(StateValue::Array(arr(0))));
        assert_eq!(v.ticks, [2, 1, 0, 0]);
        // A change to an array this viewer does not hold (tick 150 lost):
        // the slot is emptied, the key not ready until a full value.
        let d = ArrayDelta::between(1, &arr(5), &arr(6)).unwrap();
        v.on_message(vars(166, vec![KeyVars::whole(7, Vec::new(), vec![d])]));
        v.on_message(vars(183, vec![KeyVars::whole(7, Vec::new(), Vec::new())]));
        v.advance([(7, 170)], 170);
        assert_eq!(v.vals[&7][1], None);
        assert!(!v.ready(7, 2));
        // A key not drawn for a while (running render time) is forgotten.
        v.on_message(vars(150, vec![piece(8, 0, 1, 0, arr(0))]));
        let mut t = 170;
        while t <= 170 + INCOMING_STALE_MS + 100 {
            t += 100;
            v.advance([], t);
        }
        assert!(!v.incoming.contains_key(&8));
    }

    /// A hold (a pause, an upgrade screen) longer than `INCOMING_STALE_MS`
    /// with more than `TICKS_KEPT` of a key's values waiting does not cost
    /// the key its values: the hold's time is not running render time.
    #[test]
    fn a_hold_does_not_cost_a_key_its_values() {
        let tick = |time_ms, after, vals: Vec<(u8, StateValue)>| MirrorDraw::Edits {
            peer: PeerId(1),
            part: 0,
            time_ms,
            keys: vec![(after, KeyVars::whole(7, vals, Vec::new()))],
        };
        let n = |x: f64| StateValue::Real(x);
        let mut v = ViewerDraw::default();
        v.on_message(tick(100, 0, vec![(0, n(4.0)), (1, n(40.0)), (2, n(9.0))]));
        v.advance([(7, 100)], 100);
        // 70 more sends, every 16 ms, the pose drawn still that of 100 when
        // the hold begins: the oldest over `TICKS_KEPT` go in order, the
        // rest wait.
        let pending = TICKS_KEPT as u32 + 6;
        let mut time = 100;
        for i in 0..pending {
            time += 16;
            v.on_message(tick(time, 16, vec![(1, n(41.0 + f64::from(i)))]));
        }
        assert_eq!(v.ticks, [7, 0, 0, 0], "the 6 over the cap applied in order");
        // The hold: 5 s with no frame drawn; the first frame after it does
        // not show the key yet (its new poses still on the way).
        let after_hold = time + 5000;
        v.advance([], after_hold);
        // The key's first send after the hold names its last one before it.
        v.on_message(tick(
            after_hold + 50,
            after_hold + 50 - time,
            vec![(1, n(99.0))],
        ));
        v.advance([(7, after_hold + 50)], after_hold + 66);
        assert_eq!(v.ticks[2], 0, "no key started over at the hold");
        assert_eq!(v.ticks[0], 1 + u64::from(pending) + 1);
        assert!(v.ready(7, 3));
        assert_eq!(v.vals[&7][1], Some(n(99.0)));
    }

    #[test]
    fn a_key_whose_send_was_lost_starts_over() {
        let tick = |time_ms, after, vals: Vec<(u8, StateValue)>| MirrorDraw::Edits {
            peer: PeerId(1),
            part: 0,
            time_ms,
            keys: vec![(after, KeyVars::whole(7, vals, Vec::new()))],
        };
        let n = |x: f64| StateValue::Real(x);
        let mut v = ViewerDraw::default();
        // A trail: node count (slot 0), points (slot 1), colour (slot 2).
        v.on_message(tick(100, 0, vec![(0, n(4.0)), (1, n(40.0)), (2, n(9.0))]));
        v.advance([(7, 100)], 100);
        assert!(v.ready(7, 3));
        // Tick 116 (count 5) never arrives; 133 changes only the points.
        v.on_message(tick(133, 17, vec![(1, n(50.0))]));
        v.advance([(7, 133)], 133);
        assert_eq!(v.ticks, [2, 0, 1, 0]);
        // Not drawn with the count of 100 and the points of 133.
        assert!(!v.ready(7, 3));
        assert_eq!(v.vals[&7][1], Some(n(50.0)));
        // Late, older than what is applied: dropped.
        v.on_message(tick(116, 16, vec![(0, n(5.0)), (1, n(45.0))]));
        v.advance([(7, 140)], 140);
        assert_eq!(v.ticks[3], 1);
        // The full send: every value of one tick again.
        v.on_message(tick(150, 17, vec![(0, n(5.0)), (1, n(55.0)), (2, n(9.0))]));
        v.advance([(7, 150)], 150);
        assert!(v.ready(7, 3));
        assert_eq!(v.ticks, [3, 0, 1, 1]);
    }

    #[test]
    fn a_trials_variables_keep_the_codes_path_reads_the_model_has() {
        let v = |n: &str| (n.to_owned(), StateValue::Real(1.0));
        let all = [v("NodeCount"), v("TrailX"), v("trailAlpha0"), v("other")];
        let path = ["NodeCount", "TrailX", "TrailY"];
        let got = with_path_reads(vec![v("trailAlpha0")], &all, &path);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        // Found first; the path's that the model has (not TrailY), once.
        assert_eq!(names, ["trailAlpha0", "NodeCount", "TrailX"]);
        let again = with_path_reads(got, &all, &path);
        assert_eq!(again.len(), 3);
        assert!(PATH_READS.iter().all(|(_, r)| !r.is_empty()));
        assert!(PATH_READS.iter().any(|(c, _)| *c == "oTrail"));
    }

    #[test]
    fn missing_variables_are_read_from_the_runner_message() {
        let t = "ERROR in action number 1\nof Draw Event for object oAchiApproachGlow:\nVariable oAchiApproachGlow.blend(100041, -2147483648) not set before reading it.\n at gml_Object_oBurstGrowPos_Draw_0 (line 1)";
        assert_eq!(
            missing_var(t),
            Some(("oAchiApproachGlow".into(), "blend".into()))
        );
        let other = "Variable oPowers.constructNearby(100, -1) not set before reading it.";
        assert_eq!(missing_var(other).unwrap().0, "oPowers");
        assert_eq!(missing_var("DoAdd :2: illegal arguments"), None);
    }
}
