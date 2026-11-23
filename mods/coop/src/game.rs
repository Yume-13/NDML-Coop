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

//! Game handles (resolved at init, each optional) and the glue helpers every
//! hook uses: reading values, calling builtins, locks, warning once.
//!
//! A GML runtime error inside a mod's call ends the process: drawing builtins
//! get only values it built, sprites are checked with `sprite_exists` first.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ndml_sdk::{Builtin, Call, Instance, ModContext, Script, Value, Variable};

use crate::names::{self, builtin_fn as bf};
use crate::player::sprite_value;

/// Lock `m`; when it is already held, log it once per `name` (with a
/// backtrace) before waiting: on the game's thread that wait never ends.
pub fn lock_or_report<'a, T>(m: &'a Mutex<T>, name: &str) -> std::sync::MutexGuard<'a, T> {
    match m.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            warn_once(
                &format!("lock-held-{name}"),
                &format!(
                    "co-op: the {name} lock is already held (taken again on the game's thread: it hangs here); from:
{}",
                    std::backtrace::Backtrace::force_capture()
                ),
            );
            m.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }
}

thread_local! {
    /// The tracked locks this thread holds: name and where each was taken.
    static HELD: RefCell<Vec<(&'static str, &'static Location<'static>)>> =
        const { RefCell::new(Vec::new()) };
}

/// A tracked lock's guard: while it lives, this thread is recorded as the
/// holder, with the call site that took it ([`lock_tracked`]).
pub struct Tracked<'a, T> {
    guard: MutexGuard<'a, T>,
    name: &'static str,
}

impl<T> std::fmt::Debug for Tracked<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tracked({})", self.name)
    }
}

impl<T> Deref for Tracked<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for Tracked<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for Tracked<'_, T> {
    fn drop(&mut self) {
        HELD.with(|h| {
            let mut h = h.borrow_mut();
            if let Some(i) = h.iter().rposition(|(n, _)| *n == self.name) {
                h.remove(i);
            }
        });
    }
}

/// Where this thread took the tracked lock `name`, if it holds it.
#[must_use]
pub fn held_at(name: &str) -> Option<&'static Location<'static>> {
    HELD.with(|h| {
        h.borrow()
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|(_, at)| *at)
    })
}

/// Lock `m` as the tracked lock `name`. Taken again on a thread that holds
/// it (a hook inside a game call made under the lock), this logs both call
/// sites and unwinds, ending that hook alone instead of hanging the game.
#[track_caller]
pub fn lock_tracked<'a, T>(m: &'a Mutex<T>, name: &'static str) -> Tracked<'a, T> {
    let here = Location::caller();
    if let Some(at) = held_at(name) {
        let msg = format!(
            "co-op: the {name} lock taken again at {here} while this thread holds it since {at} \
             (a hook inside a game call made under the lock): that hook is skipped"
        );
        log_limited!(log::Level::Error, &format!("lock {name} {here}"), "{msg}");
        std::panic::resume_unwind(Box::new(msg));
    }
    let guard = lock_or_report(m, name);
    HELD.with(|h| h.borrow_mut().push((name, here)));
    Tracked { guard, name }
}

/// A problem's line that can repeat at play rate, at most this often per
/// site after its first.
pub const LIMIT_EVERY: Duration = Duration::from_secs(10);

/// One site's occurrences and its log lines (`log_limited!`).
#[derive(Debug, Default)]
pub struct SiteCount {
    /// Occurrences so far.
    pub n: u32,
    /// The last line logged for it.
    logged_at: Option<Instant>,
    /// Occurrences since that line not logged.
    quiet: u32,
}

impl SiteCount {
    /// Count one at `now`: `Some(not logged since the last line)` when a
    /// line is due (the first, or [`LIMIT_EVERY`] after the last).
    pub fn count(&mut self, now: Instant) -> Option<u32> {
        self.n += 1;
        if self
            .logged_at
            .is_some_and(|t| now.saturating_duration_since(t) < LIMIT_EVERY)
        {
            self.quiet += 1;
            return None;
        }
        self.logged_at = Some(now);
        Some(std::mem::take(&mut self.quiet))
    }
}

/// `log_limited!`'s count for `site`: `Some(not logged since the last
/// line)` when its line is due.
pub fn limit_due(site: &str) -> Option<u32> {
    static SITES: Mutex<BTreeMap<String, SiteCount>> = Mutex::new(BTreeMap::new());
    SITES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(site.to_owned())
        .or_default()
        .count(Instant::now())
}

/// Log `msg` once per `key` for the whole session.
pub fn warn_once(key: &str, msg: &str) {
    static SEEN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let first = SEEN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key.to_owned());
    if first {
        log::warn!("{msg}");
    }
}

/// Info once per `key`.
pub fn info_once(key: &str, msg: &str) {
    static SEEN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let first = SEEN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key.to_owned());
    if first {
        log::info!("{msg}");
    }
}

pub(crate) fn var(ctx: &ModContext, name: &str) -> Option<Variable> {
    ctx.variable(name)
        .map_err(|e| log::warn!("co-op: unable to find a variable ({e})"))
        .ok()
}

pub(crate) fn script(ctx: &ModContext, name: &str) -> Option<Script> {
    ctx.script(name)
        .map_err(|e| log::warn!("co-op: unable to find a script ({e})"))
        .ok()
}

pub(crate) fn builtin(ctx: &ModContext, name: &str) -> Option<Builtin> {
    ctx.builtin(name)
        .map_err(|e| log::warn!("co-op: unable to find a builtin ({name}: {e})"))
        .ok()
}

/// Everything resolved at init.
#[derive(Debug, Clone)]
pub struct Handles {
    pub pause: Option<Variable>,
    pub gameplay: Option<Variable>,
    pub game_over: Option<Variable>,
    pub time_game_start: Option<Variable>,
    pub hull: Option<Variable>,
    pub hull_max: Option<Variable>,
    pub hull_previous: Option<Variable>,
    pub shield: Option<Variable>,
    pub cloaked: Option<Variable>,
    pub view: [Option<Variable>; 4],
    pub mode: Vec<(&'static str, Option<Variable>)>,
    pub enable_leaderboards: Option<Variable>,
    pub hide_gui: Option<Variable>,
    pub game_ending: Option<Variable>,
    pub press_pause: Option<Script>,
    pub press_unpause: Option<Script>,
    pub press_quit: Option<Script>,
    pub b: Builtins,
}

/// Builtin functions by name.
#[derive(Debug, Clone)]
pub struct Builtins {
    pub draw_sprite_ext: Option<Builtin>,
    pub draw_text: Option<Builtin>,
    pub draw_text_transformed: Option<Builtin>,
    pub draw_rectangle: Option<Builtin>,
    pub draw_triangle: Option<Builtin>,
    pub draw_set_color: Option<Builtin>,
    pub draw_get_color: Option<Builtin>,
    pub draw_set_alpha: Option<Builtin>,
    pub draw_get_alpha: Option<Builtin>,
    pub draw_set_halign: Option<Builtin>,
    pub draw_get_halign: Option<Builtin>,
    pub draw_set_valign: Option<Builtin>,
    pub draw_get_valign: Option<Builtin>,
    pub string_height: Option<Builtin>,
    pub string_width: Option<Builtin>,
    pub sprite_exists: Option<Builtin>,
    pub gui_width: Option<Builtin>,
    pub gui_height: Option<Builtin>,
    pub clipboard_set: Option<Builtin>,
    pub clipboard_get: Option<Builtin>,
    pub clipboard_has: Option<Builtin>,
    pub io_clear: Option<Builtin>,
    pub view_get_camera: Option<Builtin>,
    pub camera_view: [Option<Builtin>; 4],
}

impl Handles {
    /// Resolve everything (missing items are logged and left `None`).
    #[must_use]
    pub fn resolve(ctx: &ModContext) -> Self {
        let b = |n| builtin(ctx, n);
        Self {
            pause: var(ctx, names::VAR_PAUSE),
            gameplay: var(ctx, names::VAR_GAMEPLAY),
            game_over: var(ctx, names::VAR_GAME_OVER),
            time_game_start: var(ctx, names::VAR_TIME_GAME_START),
            hull: var(ctx, names::VAR_HULL),
            hull_max: var(ctx, names::VAR_HULL_MAX),
            hull_previous: var(ctx, names::VAR_HULL_PREVIOUS),
            shield: var(ctx, names::VAR_SHIELD),
            cloaked: var(ctx, names::VAR_CLOAKED),
            view: names::GLOBAL_VIEW.map(|n| var(ctx, n)),
            mode: names::GLOBAL_MODE
                .iter()
                .map(|n| (*n, var(ctx, n)))
                .collect(),
            enable_leaderboards: var(ctx, names::GLOBAL_ENABLE_LEADERBOARDS),
            hide_gui: var(ctx, names::GLOBAL_HIDE_GUI),
            game_ending: var(ctx, names::GLOBAL_GAME_ENDING),
            press_pause: script(ctx, names::PRESS_PAUSE),
            press_unpause: script(ctx, names::PRESS_UNPAUSE),
            press_quit: script(ctx, names::PRESS_QUIT),
            b: Builtins {
                draw_sprite_ext: b(bf::DRAW_SPRITE_EXT),
                draw_text: b(bf::DRAW_TEXT),
                draw_text_transformed: b(bf::DRAW_TEXT_TRANSFORMED),
                draw_rectangle: b(bf::DRAW_RECTANGLE),
                draw_triangle: b(bf::DRAW_TRIANGLE),
                draw_set_color: b(bf::DRAW_SET_COLOR),
                draw_get_color: b(bf::DRAW_GET_COLOR),
                draw_set_alpha: b(bf::DRAW_SET_ALPHA),
                draw_get_alpha: b(bf::DRAW_GET_ALPHA),
                draw_set_halign: b(bf::DRAW_SET_HALIGN),
                draw_get_halign: b(bf::DRAW_GET_HALIGN),
                draw_set_valign: b(bf::DRAW_SET_VALIGN),
                draw_get_valign: b(bf::DRAW_GET_VALIGN),
                string_height: b(bf::STRING_HEIGHT),
                string_width: b(bf::STRING_WIDTH),
                sprite_exists: b(bf::SPRITE_EXISTS),
                gui_width: b(bf::GUI_WIDTH),
                gui_height: b(bf::GUI_HEIGHT),
                clipboard_set: b(bf::CLIPBOARD_SET),
                clipboard_get: b(bf::CLIPBOARD_GET),
                clipboard_has: b(bf::CLIPBOARD_HAS),
                io_clear: b(bf::IO_CLEAR),
                view_get_camera: b(bf::VIEW_GET_CAMERA),
                camera_view: [bf::CAMERA_X, bf::CAMERA_Y, bf::CAMERA_W, bf::CAMERA_H].map(b),
            },
        }
    }
}

/// An instance variable as a number.
#[must_use]
pub fn num(inst: &Instance<'_>, v: Option<Variable>) -> Option<f64> {
    inst.get_f64(v?)
}

/// An instance variable as a GML truth value.
#[must_use]
pub fn truthy(inst: &Instance<'_>, v: Option<Variable>) -> Option<bool> {
    num(inst, v).map(|x| x > 0.5)
}

/// Call a builtin; a failure is logged once per builtin and gives `None`.
#[must_use = "a failed call gives None"]
pub fn call_b(call: &Call<'_>, b: Option<Builtin>, args: &[Value]) -> Option<Value> {
    let b = b?;
    match call.call_builtin(b, args) {
        Ok(v) => Some(v),
        Err(e) => {
            warn_once(b.name(), &format!("co-op: {e}"));
            None
        }
    }
}

/// The GUI size, or `None` if the builtins are unavailable.
#[must_use]
pub fn gui_size(call: &Call<'_>, b: &Builtins) -> Option<crate::render::Gui> {
    let w = call_b(call, b.gui_width, &[])?.as_f64()?;
    let h = call_b(call, b.gui_height, &[])?.as_f64()?;
    (w > 1.0 && h > 1.0).then_some(crate::render::Gui { w, h })
}

/// The visible world rectangle: camera 0 of view 0 (`view_get_camera(0)`,
/// `camera_get_view_x/y/width/height`). `None` if there is no camera (an id
/// below 0) or a call fails; only a real camera id is passed on.
#[must_use]
pub fn camera_view(call: &Call<'_>, b: &Builtins) -> Option<crate::render::View> {
    let cam = call_b(call, b.view_get_camera, &[0.0.into()])?;
    let usable = match &cam {
        Value::Ref(_) => true,
        other => other.as_f64().is_some_and(|c| c >= 0.0),
    };
    if !usable {
        return None;
    }
    let mut rect = [0.0; 4];
    for (slot, getter) in rect.iter_mut().zip(b.camera_view) {
        *slot = call_b(call, getter, std::slice::from_ref(&cam))?.as_f64()?;
    }
    let [left, top, width, height] = rect;
    (width > 1.0 && height > 1.0).then_some(crate::render::View {
        x: left,
        y: top,
        w: width,
        h: height,
    })
}

/// Draw a partner ship (draw-call path) after checking `sprite_exists`.
#[must_use = "None if a call failed"]
pub fn draw_sprite(call: &Call<'_>, b: &Builtins, s: &crate::render::ShipDraw) -> Option<()> {
    let spr = sprite_value(s.sprite);
    let exists = call_b(call, b.sprite_exists, std::slice::from_ref(&spr))?
        .as_bool()
        .unwrap_or(false);
    if !exists {
        warn_once(
            "partner-sprite",
            &format!(
                "co-op: the partner's sprite {} does not exist here",
                s.sprite
            ),
        );
        return Some(());
    }
    call_b(
        call,
        b.draw_sprite_ext,
        &[
            spr,
            s.frame.into(),
            s.x.into(),
            s.y.into(),
            s.xscale.into(),
            s.yscale.into(),
            s.angle.into(),
            f64::from(s.blend).into(),
            s.alpha.clamp(0.0, 1.0).into(),
        ],
    )?;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hook the game calls while the caller holds the lock.
    fn hook_inside_game_call(m: &Mutex<u32>) {
        let mut st = lock_tracked(m, "test-world");
        *st += 1;
    }

    #[test]
    fn a_second_take_on_the_holding_thread_names_both_sites_instead_of_waiting() {
        let m = Mutex::new(0);
        let outer = lock_tracked(&m, "test-world");
        let held = held_at("test-world").expect("recorded");
        assert!(held.file().ends_with("game.rs"));
        let err =
            std::panic::catch_unwind(|| hook_inside_game_call(&m)).expect_err("must not wait");
        let msg = err.downcast_ref::<String>().expect("a message");
        assert!(msg.contains("test-world lock taken again at"), "{msg}");
        assert!(msg.contains(&format!("since {held}")), "{msg}");
        // The holder is untouched: its guard still works, nothing poisoned.
        drop(outer);
        assert!(!m.is_poisoned());
        assert!(held_at("test-world").is_none());
        hook_inside_game_call(&m);
        assert_eq!(*lock_tracked(&m, "test-world"), 1);
    }

    #[test]
    fn another_thread_holding_the_lock_is_not_this_threads() {
        let m = Mutex::new(0);
        let _g = lock_tracked(&m, "test-other");
        std::thread::scope(|s| {
            s.spawn(|| assert!(held_at("test-other").is_none()));
        });
    }

    /// `log_limited!`'s count: a first line, then one a `LIMIT_EVERY` with
    /// the count left out; each site apart.
    #[test]
    fn a_problem_repeating_at_play_rate_is_limited_per_site() {
        assert_eq!(limit_due("test-limit-a"), Some(0));
        for _ in 0..5 {
            assert_eq!(limit_due("test-limit-a"), None);
        }
        assert_eq!(limit_due("test-limit-b"), Some(0), "another site");
        let t0 = Instant::now();
        let mut site = SiteCount::default();
        let due: Vec<Option<u32>> = [0, 1, 9, 10, 11, 25]
            .map(|s| site.count(t0 + Duration::from_secs(s)))
            .into();
        assert_eq!(due, [Some(0), None, None, Some(2), None, Some(1)]);
        assert_eq!(site.n, 6);
    }
}
