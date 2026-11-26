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

//! The wave director's values (a child of the world view, `view_glue`):
//! globals only host code writes that a joining game's code reads.
//!
//! A joining game skips the wave director
//! ([`crate::world::names::WAVE_ALARMS`], [`crate::world::names::SPAWN_ENEMIES`])
//! and other host-only code, so a global only that code writes keeps its
//! run-start value there. The enemy scaling `scaleWaves` makes after wave 120
//! is one: the copies' speed and the cost of every enemy hit read it.
//!
//! * Host: [`DIRECTOR`] goes out as the [`DIRECTOR_CALL`] pseudo-call
//!   (reliable; name and value in turn) when a value changes, at a session's
//!   start, after a link comes back and every [`REFRESH_EVERY`] frames.
//! * Joining game: the host's values are written before the frame's copies
//!   are made, and again whenever its own differ.
//! * Own objects: what host-only code does to the player's own objects
//!   ([`OWN_RUNS`]: `scaleWaves` refreshes the shield's radius), the joining
//!   game does to its own, with its own build, once per host run
//!   ([`WorldHooks::own_run`]).

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use ndml_net::msg::{Message, StateValue, WorldCall};
use ndml_sdk::{Builtin, Call, ModContext, Script, Value, Variable};

use super::super::{ROLE_CLIENT, WorldHooks};

/// The host-only functions whose writes on this player's own objects a
/// joining game makes on its own when the host's runs (each one's part is in
/// [`WorldHooks::own_run`]).
pub const OWN_RUNS: [&str; 1] = ["gml_Script_scaleWaves"];
/// Before a function's name in the call's arguments: its run count.
pub const RUN_MARK: &str = "run:";
/// The host's runs of each of [`OWN_RUNS`] (counted by its after hook,
/// [`ran`]).
static RUNS: [AtomicU64; 1] = [AtomicU64::new(0)];

/// After a function of [`OWN_RUNS`] ran here (any role; only the host's
/// counts are sent).
pub fn ran(name: &str) {
    if let Some(i) = OWN_RUNS.iter().position(|n| *n == name) {
        RUNS[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// The globals sent: the enemy factors `scaleWaves` makes, Annihilation's
/// wave offset (the Challenger pick reads it) and the bosses killed
/// (`bossDrops`; the unlock points at the run's end count them).
pub const DIRECTOR: [&str; 5] = [
    "enemySpeedFactor",
    "enemyHullFactor",
    "enemyDamageFactor",
    "annihilationWaveRequirementModifier",
    "bossKills",
];
/// The pseudo-call carrying [`DIRECTOR`] (not a script: a build that does
/// not know it skips it).
pub const DIRECTOR_CALL: &str = "ndml:director";
/// The host sends its values again at least this often (frames), changed or
/// not: a late join or a replay from the middle has no earlier send.
pub const REFRESH_EVERY: u64 = 600;

struct Handles {
    vars: Vec<Option<Variable>>,
    /// `zoomBase`, which `updateShieldRadius` reads.
    zoom_base: Option<Variable>,
    /// `scaleWaves`' own-object part: `updateShieldRadius`, `oPowers`'
    /// `radius` and `radiusPrevious`.
    shield_radius: Option<Script>,
    radius: Option<Variable>,
    radius_previous: Option<Variable>,
    asset: Option<Builtin>,
}

static H: OnceLock<Handles> = OnceLock::new();

/// Resolve the handles (init).
pub(super) fn resolve(ctx: &ModContext) {
    let _ = H.set(Handles {
        vars: DIRECTOR.iter().map(|n| ctx.variable(n).ok()).collect(),
        zoom_base: ctx.variable("zoomBase").ok(),
        shield_radius: ctx.script("gml_Script_updateShieldRadius").ok(),
        radius: ctx.variable("radius").ok(),
        radius_previous: ctx.variable("radiusPrevious").ok(),
        asset: ctx.builtin("asset_get_index").ok(),
    });
}

/// Per-session state (in `ViewState`).
#[derive(Debug, Default)]
pub(super) struct DirectorState {
    /// Host: the values last sent and the frame.
    sent: Option<(Vec<Option<f64>>, u64)>,
    /// Client: the host's newest values (name, value).
    latest: Option<Vec<(String, f64)>>,
    /// Client: the host's newest run counts ([`OWN_RUNS`]), and the counts
    /// this game last made its own part for.
    runs: Vec<(String, u64)>,
    runs_done: HashMap<String, u64>,
    /// Client: the host's time (ms) of the first call that carried each
    /// function's newest count: the host's frame that ran it.
    runs_at: HashMap<String, u32>,
    /// Client: the host's `zoomBase` as each of its views carried it (host
    /// time ms, oldest first), at most [`ZOOM_SEEN`].
    zooms: VecDeque<(u32, f64)>,
    /// Object indices by name (`oPowers`; resolved once).
    objects: HashMap<&'static str, u32>,
}

/// The host views' `zoomBase` kept (one per host frame: about 4 s, past any
/// resend of the reliable call).
pub const ZOOM_SEEN: usize = 256;

/// Whether host time `a` is before `b` (ms, the clock wraps).
#[expect(
    clippy::cast_possible_wrap,
    reason = "the wrapped difference as a signed span"
)]
fn before(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) > 0
}

impl DirectorState {
    /// Client, from each host view: its `zoomBase` at its host time.
    pub(super) fn zoom_seen(&mut self, time_ms: u32, zoom: f64) {
        if !zoom.is_finite() {
            return;
        }
        let at = self
            .zooms
            .iter()
            .rposition(|&(t, _)| !before(time_ms, t))
            .map_or(0, |i| i + 1);
        if at > 0 && self.zooms[at - 1].0 == time_ms {
            return;
        }
        self.zooms.insert(at, (time_ms, zoom));
        while self.zooms.len() > ZOOM_SEEN {
            self.zooms.pop_front();
        }
    }

    /// The host's `zoomBase` in its last view before host time `time_ms`:
    /// what the host's code read in the frame that sent at `time_ms` before
    /// that frame changed it. `None` with no such view kept.
    #[must_use]
    pub fn zoom_before(&self, time_ms: u32) -> Option<f64> {
        self.zooms
            .iter()
            .rev()
            .find(|&&(t, _)| before(t, time_ms))
            .map(|&(_, z)| z)
    }

    /// Client, from a call at host time `time_ms`: its run counts, each new
    /// count with the time of the first call that carried it (a later
    /// resend or refresh keeps it).
    pub(super) fn take_runs(&mut self, runs: Vec<(String, u64)>, time_ms: u32) {
        for (name, count) in &runs {
            let same = self.runs.iter().any(|(n, c)| n == name && c == count);
            if !same || !self.runs_at.contains_key(name) {
                self.runs_at.insert(name.clone(), time_ms);
            }
        }
        self.runs = runs;
    }
}

/// `scaleWaves`' part on this player's `oPowers` (`with (oPowers) { radius
/// = updateShieldRadius(); radiusPrevious = radius }`), on the game or on a
/// model.
pub trait ShieldPart {
    /// The global `zoomBase` now.
    fn zoom_base(&self) -> Option<f64>;
    /// Write the global `zoomBase`.
    fn set_zoom_base(&mut self, zoom: f64);
    /// `oPowers`' `radius` now.
    fn radius(&self) -> Option<f64>;
    /// `updateShieldRadius()` as `oPowers`.
    fn shield_radius(&mut self) -> Option<f64>;
    /// Write `oPowers`' `radius` and `radiusPrevious`.
    fn set_radius(&mut self, radius: f64);
}

/// Make the part with `zoom` as the global `zoomBase` for the call (the
/// host's when its `scaleWaves` ran; put back after), `None`: the one this
/// game has. The radius before and after.
pub fn shield_radius_part(
    part: &mut impl ShieldPart,
    zoom: Option<f64>,
) -> (Option<f64>, Option<f64>) {
    let restore = zoom.and_then(|z| {
        let own = part.zoom_base()?;
        (own.to_bits() != z.to_bits()).then(|| {
            part.set_zoom_base(z);
            own
        })
    });
    let was = part.radius();
    let now = part.shield_radius().filter(|r| r.is_finite());
    if let Some(r) = now {
        part.set_radius(r);
    }
    if let Some(own) = restore {
        part.set_zoom_base(own);
    }
    (was, now)
}

/// [`ShieldPart`] on the game: this player's `oPowers` and the global
/// `zoomBase`.
struct GamePowers<'a, 'c> {
    call: &'a Call<'c>,
    powers: Value,
    zoom_base: Option<Variable>,
    script: Script,
    radius: Variable,
    previous: Variable,
}

impl ShieldPart for GamePowers<'_, '_> {
    fn zoom_base(&self) -> Option<f64> {
        self.zoom_base.and_then(|v| self.call.global()?.get_f64(v))
    }

    fn set_zoom_base(&mut self, zoom: f64) {
        if let (Some(v), Some(g)) = (self.zoom_base, self.call.global()) {
            let _ = g.set_f64(v, zoom);
        }
    }

    fn radius(&self) -> Option<f64> {
        let mut r = None;
        let _ = self
            .call
            .with(&self.powers, |inst, _| r = inst.get_f64(self.radius));
        r
    }

    fn shield_radius(&mut self) -> Option<f64> {
        let mut r = None;
        let _ = self.call.with(&self.powers, |inst, _| {
            r = inst
                .call_script(self.script, &[])
                .ok()
                .and_then(|v| v.as_f64());
        });
        r
    }

    fn set_radius(&mut self, radius: f64) {
        let _ = self.call.with(&self.powers, |inst, _| {
            let _ = inst.set_f64(self.radius, radius);
            let _ = inst.set_f64(self.previous, radius);
        });
    }
}

/// The names the host reads, in order: [`DIRECTOR`], then each of
/// [`OWN_RUNS`] after [`RUN_MARK`] (its run count).
fn sent_names() -> impl Iterator<Item = String> {
    DIRECTOR
        .iter()
        .map(|n| (*n).to_owned())
        .chain(OWN_RUNS.iter().map(|n| format!("{RUN_MARK}{n}")))
}

/// The call's arguments: each value read ([`sent_names`]' order), name and
/// value in turn (one not read is left out).
#[must_use]
pub fn director_args(values: &[Option<f64>]) -> Vec<StateValue> {
    sent_names()
        .zip(values)
        .filter_map(|(n, v)| v.map(|v| [StateValue::Str(n), StateValue::Real(v)]))
        .flatten()
        .collect()
}

/// The run counts in a call's arguments: each of [`OWN_RUNS`] with a whole
/// number; anything else is left out.
#[must_use]
pub fn director_runs(args: &[StateValue]) -> Vec<(String, u64)> {
    args.as_chunks::<2>()
        .0
        .iter()
        .filter_map(|p| {
            let StateValue::Str(n) = &p[0] else {
                return None;
            };
            let name = n.strip_prefix(RUN_MARK)?;
            let v = match p[1].clone().expanded() {
                StateValue::Real(v) => v,
                StateValue::Int(i) => f64::from(i32::try_from(i).ok()?),
                _ => return None,
            };
            let whole = (v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v < 1e15).then_some(v)?;
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a whole number from 0 to 1e15, checked just above"
            )]
            let count = whole as u64;
            OWN_RUNS.contains(&name).then(|| (name.to_owned(), count))
        })
        .collect()
}

/// The functions whose own-object part is due: each run count that differs
/// from the one last made (or was never made).
#[must_use]
pub fn runs_due(runs: &[(String, u64)], done: &HashMap<String, u64>) -> Vec<(String, u64)> {
    runs.iter()
        .filter(|(n, c)| done.get(n) != Some(c))
        .cloned()
        .collect()
}

/// The values in a call's arguments: the names in [`DIRECTOR`] with a
/// number each; anything else is left out.
#[must_use]
pub fn director_values(args: &[StateValue]) -> Vec<(String, f64)> {
    args.as_chunks::<2>()
        .0
        .iter()
        .filter_map(|p| {
            let StateValue::Str(n) = &p[0] else {
                return None;
            };
            let v = match p[1].clone().expanded() {
                StateValue::Real(v) => v,
                StateValue::Int(i) => f64::from(i32::try_from(i).ok()?),
                _ => return None,
            };
            (DIRECTOR.contains(&n.as_str()) && v.is_finite()).then(|| (n.clone(), v))
        })
        .collect()
}

/// Whether the host sends now: nothing sent yet, a value changed (to the
/// bit; one that could not be read counts too), or the refresh is due.
#[must_use]
pub fn director_due(
    sent: Option<&(Vec<Option<f64>>, u64)>,
    now: &[Option<f64>],
    frame: u64,
) -> bool {
    let Some((was, at)) = sent else { return true };
    frame.saturating_sub(*at) >= REFRESH_EVERY
        || was.len() != now.len()
        || was
            .iter()
            .zip(now)
            .any(|(a, b)| a.map(f64::to_bits) != b.map(f64::to_bits))
}

impl WorldHooks {
    /// [`DIRECTOR`]'s values, then the run counts of [`OWN_RUNS`].
    #[expect(clippy::cast_precision_loss, reason = "a run count, far below 2^53")]
    fn director_read(call: &Call<'_>) -> Option<Vec<Option<f64>>> {
        let h = H.get()?;
        let g = call.global()?;
        Some(
            h.vars
                .iter()
                .map(|v| v.and_then(|v| g.get_f64(v)))
                .chain(RUNS.iter().map(|r| Some(r.load(Ordering::Relaxed) as f64)))
                .collect(),
        )
    }

    /// Host, each run frame (with the view): the values when due.
    pub(super) fn director_host(&self, call: &Call<'_>, now_ms: u32) -> Option<Message> {
        let now = Self::director_read(call)?;
        let mut st = self.lock();
        let frame = st.frame;
        if !director_due(st.view.director.sent.as_ref(), &now, frame) {
            return None;
        }
        let args = director_args(&now);
        st.view.director.sent = Some((now, frame));
        Some(Message::WorldCall(WorldCall {
            time_ms: now_ms,
            script: DIRECTOR_CALL.to_owned(),
            args,
        }))
    }

    /// Host, when a link comes back: send again at the next frame (what was
    /// sent into the dead link was lost).
    pub(in crate::world) fn director_resync(&self) {
        self.lock().view.director.sent = None;
    }

    /// Client, from the inbox: the host's newest values.
    pub(super) fn director_take(&self, c: &WorldCall) {
        let values = director_values(&c.args);
        let runs = director_runs(&c.args);
        let mut st = self.lock();
        st.view.director.latest = Some(values);
        st.view.director.take_runs(runs, c.time_ms);
    }

    /// Client, each frame before the frame's copies are made: the host's
    /// values where this game's differ.
    pub(in crate::world) fn view_director_client(&self, call: &Call<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let latest = self.lock().view.director.latest.clone();
        let (Some(latest), Some(h), Some(g)) = (latest, H.get(), call.global()) else {
            return;
        };
        for (n, v) in &latest {
            let Some(var) = DIRECTOR.iter().position(|d| d == n).and_then(|i| h.vars[i]) else {
                continue;
            };
            if g.get_f64(var).map(f64::to_bits) != Some(v.to_bits()) {
                let _ = g.set_f64(var, *v);
            }
        }
        // After the values: the own-object parts read them, and the world
        // view's globals as the host had them when its function ran (each
        // run's host time).
        let due = {
            let st = self.lock();
            runs_due(&st.view.director.runs, &st.view.director.runs_done)
                .into_iter()
                .map(|(n, c)| {
                    let at = st.view.director.runs_at.get(&n).copied();
                    (n, c, at)
                })
                .collect::<Vec<_>>()
        };
        for (name, count, at) in due {
            self.own_run(call, &name, count, at);
            self.lock().view.director.runs_done.insert(name, count);
        }
    }

    /// Client: what the host's `name` did to the host's own objects, done to
    /// this game's own with its own values; `at`: the host's time of the
    /// frame that ran it.
    fn own_run(&self, call: &Call<'_>, name: &str, count: u64, at: Option<u32>) {
        // One arm per function of `OWN_RUNS`.
        if name == OWN_RUNS[0] {
            // The host's tick runs `scaleWaves` before `spawnEnemyWaves`'
            // `refreshZoom`: its radius reads the `zoomBase` from before that
            // frame, which its view of the frame before carried. So does this
            // game's part, whichever of the call and the new view came first.
            let zoom = at.and_then(|t| self.lock().view.director.zoom_before(t));
            self.own_shield_radius(call, count, zoom);
        }
    }

    /// `scaleWaves`' part on this player's objects ([`shield_radius_part`]),
    /// with `zoom` as `zoomBase` for the call.
    fn own_shield_radius(&self, call: &Call<'_>, count: u64, zoom: Option<f64>) {
        let Some(h) = H.get() else { return };
        let (Some(script), Some(radius), Some(previous)) =
            (h.shield_radius, h.radius, h.radius_previous)
        else {
            return;
        };
        let Some(powers) = self.director_object(call, "oPowers") else {
            return;
        };
        let mut part = GamePowers {
            call,
            powers: Value::object(powers),
            zoom_base: h.zoom_base,
            script,
            radius,
            previous,
        };
        let (was, now) = shield_radius_part(&mut part, zoom);
        let _ = (count, was, now);
    }

    /// An object's index by name, resolved once per session.
    fn director_object(&self, call: &Call<'_>, name: &'static str) -> Option<u32> {
        // Its own statement: the guard is dropped before the game call and
        // the second lock below.
        let cached = self.lock().view.director.objects.get(name).copied();
        cached.or_else(|| {
            let h = H.get()?;
            let p = h
                .asset
                .and_then(|b| call.call_builtin(b, &[Value::Str(name.into())]).ok())
                .as_ref()
                .and_then(super::super::classify::object_index)?;
            self.lock().view.director.objects.insert(name, p);
            Some(p)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// `oPowers` and the global `zoomBase` as `updateShieldRadius` gives the
    /// radius without Sanctuary: `mult * mean(1, zoomBase)`.
    struct Model {
        zoom: f64,
        radius: f64,
        mult: f64,
    }

    impl ShieldPart for Model {
        fn zoom_base(&self) -> Option<f64> {
            Some(self.zoom)
        }
        fn set_zoom_base(&mut self, zoom: f64) {
            self.zoom = zoom;
        }
        fn radius(&self) -> Option<f64> {
            Some(self.radius)
        }
        fn shield_radius(&mut self) -> Option<f64> {
            Some(self.mult * f64::midpoint(1.0, self.zoom))
        }
        fn set_radius(&mut self, radius: f64) {
            self.radius = radius;
        }
    }

    /// `refreshZoom`'s base at a wave.
    fn zoom_at(wave: f64) -> f64 {
        1.0 + 0.4 / 240.0 * wave.min(240.0)
    }

    const SCALE: &str = "gml_Script_scaleWaves";

    /// The host's frame at `t` ticks the wave: its `scaleWaves` (run 2)
    /// reads wave 151's `zoomBase`, then its `refreshZoom` makes wave 152's,
    /// which its view of that frame carries. The joining game's part reads
    /// wave 151's whether that view came a frame before the call, with it,
    /// or after, and leaves that game's `zoomBase` as the views left it.
    #[test]
    fn the_shield_part_reads_the_hosts_zoom_from_before_its_run_in_any_order() {
        let t = 9000u32;
        let (old, new) = (zoom_at(151.0), zoom_at(152.0));
        for order in ["view before", "with", "view after"] {
            let mut st = DirectorState::default();
            st.zoom_seen(t - 33, old);
            st.zoom_seen(t - 16, old);
            // The joining game's global: the newest view written (before:
            // the new one already; with or after: still the old one).
            let mut game = Model {
                zoom: old,
                radius: 1.0,
                mult: 1.0,
            };
            if order != "view after" {
                st.zoom_seen(t, new);
            }
            if order == "view before" {
                game.zoom = new;
            }
            st.take_runs(vec![(SCALE.to_owned(), 2)], t);
            let due = runs_due(&st.runs, &st.runs_done);
            assert_eq!(due, vec![(SCALE.to_owned(), 2)], "{order}");
            let zoom = st.zoom_before(st.runs_at[SCALE]);
            assert_eq!(zoom, Some(old), "{order}");
            let before_call = game.zoom;
            let (_, now) = shield_radius_part(&mut game, zoom);
            assert_eq!(now, Some(f64::midpoint(1.0, old)), "{order}");
            assert!(
                (game.radius - f64::midpoint(1.0, old)).abs() < 1e-12,
                "{order}"
            );
            assert_eq!(
                game.zoom.to_bits(),
                before_call.to_bits(),
                "{order}: put back"
            );
            if order == "view after" {
                st.zoom_seen(t, new);
                assert_eq!(st.zoom_before(st.runs_at[SCALE]), Some(old));
            }
        }
    }

    /// With no level-up since the last wave (nothing else refreshed the
    /// radius), the run moves it from the last wave's to this one's; a
    /// refresh call carrying the same count later neither runs it again nor
    /// moves the run's time.
    #[test]
    fn a_run_moves_the_radius_with_no_level_up_first() {
        let mut st = DirectorState::default();
        let mut game = Model {
            zoom: zoom_at(151.0),
            radius: 1.3 * f64::midpoint(1.0, zoom_at(150.0)),
            mult: 1.3,
        };
        st.zoom_seen(4000, zoom_at(150.0));
        st.zoom_seen(9984, zoom_at(151.0));
        st.zoom_seen(10_000, zoom_at(152.0));
        st.take_runs(vec![(SCALE.to_owned(), 7)], 10_000);
        st.take_runs(vec![(SCALE.to_owned(), 7)], 20_000);
        assert_eq!(st.runs_at[SCALE], 10_000, "the first call's time");
        let (was, now) = shield_radius_part(&mut game, st.zoom_before(st.runs_at[SCALE]));
        let want = 1.3 * f64::midpoint(1.0, zoom_at(151.0));
        assert!(was.is_some_and(|w| (w - want).abs() > 1e-4), "{was:?}");
        assert!(now.is_some_and(|r| (r - want).abs() < 1e-12), "{now:?}");
        st.runs_done.insert(SCALE.to_owned(), 7);
        assert!(runs_due(&st.runs, &st.runs_done).is_empty(), "once");
        // A new count at a later call takes that call's time.
        st.take_runs(vec![(SCALE.to_owned(), 8)], 30_000);
        assert_eq!(st.runs_at[SCALE], 30_000);
    }

    #[test]
    fn the_zoom_kept_is_in_host_time_order_and_bounded() {
        let mut st = DirectorState::default();
        st.zoom_seen(200, 2.0);
        st.zoom_seen(100, 1.0); // a late view
        st.zoom_seen(200, 9.0); // the same frame again: kept once
        st.zoom_seen(300, f64::NAN);
        assert_eq!(st.zooms, VecDeque::from(vec![(100, 1.0), (200, 2.0)]));
        assert_eq!(st.zoom_before(200), Some(1.0));
        assert_eq!(st.zoom_before(201), Some(2.0));
        assert_eq!(st.zoom_before(100), None, "nothing before");
        for t in 0..(ZOOM_SEEN as u32 + 10) {
            st.zoom_seen(1000 + t, 1.5);
        }
        assert_eq!(st.zooms.len(), ZOOM_SEEN);
        // The host clock wraps.
        let mut w = DirectorState::default();
        w.zoom_seen(u32::MAX - 10, 1.25);
        w.zoom_seen(5, 1.5);
        assert_eq!(w.zoom_before(5), Some(1.25));
        assert_eq!(w.zoom_before(6), Some(1.5));
    }

    #[test]
    fn the_run_counts_go_by_name_and_each_change_is_made_once() {
        let args = director_args(&[None, None, None, None, None, Some(4.0)]);
        assert_eq!(
            args,
            vec![
                StateValue::Str("run:gml_Script_scaleWaves".into()),
                StateValue::Real(4.0)
            ]
        );
        assert!(director_values(&args).is_empty(), "a count is no value");
        let runs = director_runs(&args);
        assert_eq!(runs, vec![("gml_Script_scaleWaves".to_owned(), 4)]);
        // Unknown functions, fractions and other kinds are left out.
        let odd = vec![
            StateValue::Str("run:gml_Script_somethingNew".into()),
            StateValue::Real(1.0),
            StateValue::Str("run:gml_Script_scaleWaves".into()),
            StateValue::Real(1.5),
            StateValue::Str("enemySpeedFactor".into()),
            StateValue::Real(1.0),
        ];
        assert!(director_runs(&odd).is_empty());
        // Never made: due (a session's first call); the same count again:
        // not; a new count: due once.
        let mut done = HashMap::new();
        assert_eq!(runs_due(&runs, &done), runs);
        done.insert("gml_Script_scaleWaves".to_owned(), 4);
        assert!(runs_due(&runs, &done).is_empty());
        let next = vec![("gml_Script_scaleWaves".to_owned(), 5)];
        assert_eq!(runs_due(&next, &done), next);
    }

    #[test]
    fn the_values_go_by_name_and_come_back() {
        let args = director_args(&[Some(1.5), None, Some(1.25), Some(-10.0), Some(3.0)]);
        assert_eq!(args.len(), 8, "the value not read is left out");
        let back = director_values(&args);
        assert_eq!(
            back,
            vec![
                ("enemySpeedFactor".to_owned(), 1.5),
                ("enemyDamageFactor".to_owned(), 1.25),
                ("annihilationWaveRequirementModifier".to_owned(), -10.0),
                ("bossKills".to_owned(), 3.0),
            ]
        );
        // A name the mod does not know and a value that is no number.
        let odd = vec![
            StateValue::Str("someNewFactor".into()),
            StateValue::Real(2.0),
            StateValue::Str("enemyHullFactor".into()),
            StateValue::Str("x".into()),
            StateValue::Str("enemyHullFactor".into()),
            StateValue::Real(f64::NAN),
        ];
        assert!(director_values(&odd).is_empty());
    }

    #[test]
    fn the_host_sends_at_the_start_on_a_change_and_at_the_refresh() {
        let v = vec![Some(1.0), Some(1.0), Some(1.0), Some(0.0), Some(0.0)];
        assert!(director_due(None, &v, 0), "nothing sent yet");
        let sent = (v.clone(), 100);
        assert!(!director_due(Some(&sent), &v, 101));
        assert!(!director_due(Some(&sent), &v, 100 + REFRESH_EVERY - 1));
        assert!(director_due(Some(&sent), &v, 100 + REFRESH_EVERY));
        // A wave change: the factors move.
        let mut w = v.clone();
        w[0] = Some(1.004_375);
        assert!(director_due(Some(&sent), &w, 101));
        // A value that could not be read, or is read again.
        let mut u = v.clone();
        u[4] = None;
        assert!(director_due(Some(&sent), &u, 101));
    }

    /// One run counter per function the host counts.
    #[test]
    fn every_counted_function_has_its_counter() {
        assert_eq!(RUNS.len(), OWN_RUNS.len());
    }
}
