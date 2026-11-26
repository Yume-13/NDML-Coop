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

//! GML error guard for the co-op mod: the loader's guard contains a GML runtime error (a
//! `try`/`catch` around the event body) instead of the game's fatal "Code Error" dialog.
//!
//! Scope, on the joining side while the role is client: the puppets the client made for the
//! host and their parts (every event kind); for every event but Draw also what those make and
//! every enemy-side instance. The host contains only the mod's calls; the game's own instances
//! are never contained.
//!
//! After a contained error a puppet is destroyed (no Destroy event) and made again from the
//! host's state; a second error within [`STRIKE_WINDOW`] leaves it out. Errors are logged
//! limited per site ([`crate::game::LIMIT_EVERY`]), with a totals line at most every 10 s.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ndml_net::msg::NetId;
use ndml_sdk::{Builtin, Call, GmlError, ModContext, Value};

use crate::game::SiteCount;
use crate::world::client::ClientWorld;

/// A second error from the same puppet within this long leaves it out.
pub const STRIKE_WINDOW: Duration = Duration::from_secs(30);
/// The totals line, at most this often (and only when they changed).
const TOTALS_EVERY: Duration = Duration::from_secs(10);
/// Event kind Draw (left out of the wider scope).
const EVENT_DRAW: u32 = 8;

#[derive(Clone, Copy)]
struct Handles {
    exists: Builtin,
    destroy: Builtin,
}

static HANDLES: OnceLock<Handles> = OnceLock::new();

/// The guard asked for at init is not running (known only after boot:
/// [`confirm`]).
static NOT_LIVE: AtomicBool = AtomicBool::new(false);

/// Errors [`take_calls`] did not take, for the frame handlers.
static PENDING: Mutex<Vec<GmlError>> = Mutex::new(Vec::new());

/// Whether the mod's calls are contained (a GML error in a call returns
/// `Err` instead of ending the process).
#[must_use]
pub fn calls_contained() -> bool {
    HANDLES.get().is_some() && !NOT_LIVE.load(Ordering::Relaxed)
}

/// Once, at the first use after boot: ask the loader whether the call guard
/// runs. If not, nothing that relies on contained calls runs.
pub fn confirm(call: &Call<'_>) {
    static ONCE: Once = Once::new();
    if HANDLES.get().is_none() {
        return;
    }
    ONCE.call_once(|| {
        if let Err(status) = call.guard_calls(true) {
            NOT_LIVE.store(true, Ordering::Relaxed);
            log::warn!(
                "co-op guard: the loader's GML error guard is not running ({status:?}); the mod's calls are not contained: mirror draw off, the partner's things draw as their sprites"
            );
        }
    });
}

/// Keep everything caught so far for the frame handlers, before a call
/// whose own errors [`take_calls`] then takes.
pub fn set_aside(call: &Call<'_>) {
    PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .extend(call.guard_take());
}

/// Take the runner messages of errors caught inside an "errors expected"
/// scope ([`Call::guard_expect`]; call [`set_aside`] first). Every other
/// error stays for the frame handlers.
pub fn take_calls(call: &Call<'_>) -> Vec<String> {
    let (expected, rest): (Vec<GmlError>, Vec<GmlError>) =
        call.guard_take().into_iter().partition(|e| e.expected);
    PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .extend(rest);
    expected.into_iter().map(|e| e.text).collect()
}

/// What the guard caught: what [`take_calls`] left, then the rest.
fn take_all(call: &Call<'_>) -> Vec<GmlError> {
    let mut v = std::mem::take(&mut *PENDING.lock().unwrap_or_else(PoisonError::into_inner));
    v.extend(call.guard_take());
    v
}

/// Contained errors so far, by why they were contained.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    /// In a puppet's (or its part's) event.
    pub copies: u32,
    /// In what a puppet made (the loader's offspring).
    pub offspring: u32,
    /// In an enemy-side instance, by its object.
    pub objects: u32,
    /// In a call of the mod (or an event run inside one).
    pub calls: u32,
    /// Puppets removed and made again.
    pub remade: u32,
    /// Puppets left out after a second error.
    pub left_out: u32,
}

impl Totals {
    /// Contained errors of any kind.
    #[must_use]
    pub fn contained(&self) -> u32 {
        self.copies + self.offspring + self.objects + self.calls
    }
}

#[derive(Debug, Default)]
struct State {
    last: HashMap<NetId, Instant>,
    totals: Totals,
    /// Errors per site (object, event, line, missing), for the log's limit.
    sites: HashMap<String, SiteCount>,
    reported: Totals,
    reported_at: Option<Instant>,
    /// What the loader has now (`None`: never set).
    scope_on: Option<bool>,
}

/// The log's line for an error (`due`: [`SiteCount::count`]); `None`: not
/// logged.
fn site_line(line: &str, due: Option<u32>) -> Option<String> {
    due.map(|q| {
        if q == 0 {
            format!("co-op guard: {line}")
        } else {
            format!("co-op guard: {line} ({q} more at this site not logged before it)")
        }
    })
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Ask the loader for both guards. Init only.
pub fn init(ctx: &ModContext) {
    let handles = (|| {
        Some(Handles {
            exists: ctx.builtin("instance_exists").ok()?,
            destroy: ctx.builtin("instance_destroy").ok()?,
        })
    })();
    let Some(h) = handles else {
        log::warn!("co-op guard: builtins missing; the guard stays off");
        return;
    };
    match ctx.guard(true, true) {
        Ok(()) => {
            let _ = HANDLES.set(h);
            *STATE.lock().unwrap_or_else(PoisonError::into_inner) = Some(State::default());
            log::info!(
                "co-op guard: asked for the loader's GML error guard (puppets' events, on the joining side also what they make and enemy-side objects and the mod's calls)"
            );
        }
        Err(e) => log::warn!("co-op guard: {e}; the guard stays off"),
    }
}

/// Every run tick: the wider scope on while the role is client (`objects`:
/// the enemy-side object indices, asked for once). Off the client, count and
/// log what the guard caught.
pub fn role_frame(call: &Call<'_>, client: bool, objects: impl FnOnce() -> Vec<i32>) {
    static OBJECTS: OnceLock<Vec<i32>> = OnceLock::new();
    confirm(call);
    if !calls_contained() {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(s) = g.as_mut() else { return };
    let want = client;
    if s.scope_on != Some(want) {
        let ok = if want {
            let objs = OBJECTS.get_or_init(objects);
            call.guard_scope(true, objs, !(1u32 << EVENT_DRAW))
        } else {
            call.guard_scope(false, &[], 0)
        };
        if ok && want {
            log::info!(
                "co-op guard: wider scope on (what puppets make, and {} enemy-side objects; every event but Draw)",
                OBJECTS.get().map_or(0, Vec::len)
            );
        } else if ok {
            log::info!("co-op guard: wider scope off (puppets only)");
        } else if want {
            log::warn!("co-op guard: this loader has no wider scope; puppets only");
        }
        s.scope_on = Some(want);
    }
    if !client {
        let now = Instant::now();
        for e in take_all(call) {
            count(s, &e, now);
        }
        totals_line(s, now);
    }
}

/// The client's end of frame: handle what the guard caught, mark the
/// instances the client made.
pub fn client_frame(call: &Call<'_>, client: &mut ClientWorld) {
    let Some(h) = HANDLES.get() else { return };
    let mut g = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(s) = g.as_mut() else { return };
    let now = Instant::now();
    let mut handled: Vec<i32> = Vec::new();
    for e in take_all(call) {
        let shown = count(s, &e, now);
        if e.in_call || handled.contains(&e.instance) {
            continue;
        }
        handled.push(e.instance);
        let Some(net) = client.net_of_instance(e.instance) else {
            // A part, an offspring or an enemy-side instance: it goes.
            remove_now(call, h, e.instance);
            continue;
        };
        let again = s
            .last
            .get(&net)
            .is_none_or(|t| now.duration_since(*t) > STRIKE_WINDOW);
        s.last.insert(net, now);
        client.redo_after_error(net, again);
        remove_now(call, h, e.instance);
        if again {
            s.totals.remade += 1;
        } else {
            s.totals.left_out += 1;
        }
        if !shown {
            continue;
        }
        log::info!(
            "co-op guard: puppet {net} (instance {}) {} (remade {}, left out {})",
            e.instance,
            if again {
                "removed, to be made again from the host's state"
            } else {
                "had a second error within 30 s: left out"
            },
            s.totals.remade,
            s.totals.left_out
        );
    }
    totals_line(s, now);
    drop(g);
    call.guard_mark(&client.guarded_instances());
}

/// Count one caught error and log it (limited per site); whether it was logged.
fn count(s: &mut State, e: &GmlError, now: Instant) -> bool {
    let why = if e.in_call {
        s.totals.calls += 1;
        "a call of the mod"
    } else if e.marked {
        s.totals.copies += 1;
        "a puppet or its part"
    } else if e.offspring {
        s.totals.offspring += 1;
        "made by a puppet"
    } else if e.by_object {
        s.totals.objects += 1;
        "an enemy-side object"
    } else {
        s.totals.calls += 1;
        "an event run inside a call of the mod"
    };
    let key = Site::parse(&e.text).key();
    let site = s.sites.entry(key.clone()).or_default();
    let due = site.count(now);
    let n = site.n;
    let line = format!(
        "contained {key} ({why}; instance {}; #{n} at this site, {} in all): {}",
        e.instance,
        s.totals.contained(),
        e.text.trim().replace('\n', " | ")
    );
    let shown = site_line(&line, due);
    if let Some(l) = &shown {
        log::warn!("{l}");
    }
    shown.is_some()
}

/// The totals line when they changed, at most every [`TOTALS_EVERY`].
fn totals_line(s: &mut State, now: Instant) {
    if s.totals == s.reported || s.reported_at.is_some_and(|t| now - t < TOTALS_EVERY) {
        return;
    }
    let t = s.totals;
    let text = format!(
        "contained {} (puppets {}, made by puppets {}, enemy-side objects {}, calls {}); puppets remade {}, left out {}",
        t.contained(),
        t.copies,
        t.offspring,
        t.objects,
        t.calls,
        t.remade,
        t.left_out
    );
    log::warn!("co-op guard: totals: {text}");
    s.reported = t;
    s.reported_at = Some(now);
}

/// Where a runner error was raised, read from its message: `ERROR in action
/// number 1\nof <event> for object <object>:\n<error>\n at gml_Object_<..>
/// (line <n>) ...`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Site {
    /// The instance's object (`for object X:`).
    pub object: String,
    /// The code that raised it (`gml_Object_<object>_<Event>_<n>`, a parent's
    /// for an inherited event, or a script), else the runner's event name.
    pub event: String,
    /// The line in that code.
    pub line: Option<u32>,
    /// The variable an unset read names (`Variable <..>.<name>(`).
    pub missing: Option<String>,
}

impl Site {
    /// Parse what can be found; the rest stays empty.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let flat = text.replace('\n', " ");
        let object = after(&flat, "for object ")
            .and_then(|r| r.split(':').next())
            .map_or_default(|o| o.trim().to_owned());
        let code = flat
            .split_whitespace()
            .find(|w| w.starts_with("gml_Object_") || w.starts_with("gml_Script_"))
            .map(|w| w.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_'));
        let event = code.map_or_else(
            || {
                after(&flat, "of ")
                    .and_then(|r| r.split(" for object").next())
                    .map_or_default(|e| e.trim().to_owned())
            },
            str::to_owned,
        );
        let line = after(&flat, "(line ")
            .and_then(|r| r.split(')').next())
            .and_then(|n| n.trim().parse().ok());
        let missing = after(&flat, "Variable ")
            .and_then(|r| r.split('(').next())
            .and_then(|v| v.rsplit('.').next())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_alphanumeric() || c == '_'));
        Self {
            object,
            event,
            line,
            missing,
        }
    }

    /// `<object> <event> line <n> missing <name>` (the parts found).
    #[must_use]
    pub fn key(&self) -> String {
        let or_q = |s: &str| {
            if s.is_empty() {
                "?".to_owned()
            } else {
                s.to_owned()
            }
        };
        let mut k = format!("{} {}", or_q(&self.object), or_q(&self.event));
        if let Some(l) = self.line {
            let _ = write!(k, " line {l}");
        }
        if let Some(m) = &self.missing {
            let _ = write!(k, " missing {m}");
        }
        k
    }
}

fn after<'a>(s: &'a str, pat: &str) -> Option<&'a str> {
    s.find(pat).map(|i| &s[i + pat.len()..])
}

/// `instance_destroy(id, false)` if it still exists (no Destroy event: the
/// instance's own code is what failed).
fn remove_now(call: &Call<'_>, h: &Handles, instance: i32) {
    let id =
        Value::Ref((0x0400_0001u64 << 32) | u64::from(u32::from_ne_bytes(instance.to_ne_bytes())));
    let exists = call
        .call_builtin(h.exists, std::slice::from_ref(&id))
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if exists && let Err(e) = call.call_builtin(h.destroy, &[id, false.into()]) {
        log::warn!("co-op guard: instance_destroy({instance}): {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_are_read_from_the_runner_message() {
        let t = "ERROR in action number 1\nof  Alarm Event for alarm 0\nfor object oTracerShot:\n\n\
                 Variable oTracerShot.enemySizeModifier(100734, -2147483648) not set before reading it.\n \
                 at gml_Object_oTracerShot_Alarm_0 (line 3) - w = 2 * enemySizeModifier\n";
        let s = Site::parse(t);
        assert_eq!(s.object, "oTracerShot");
        assert_eq!(s.event, "gml_Object_oTracerShot_Alarm_0");
        assert_eq!(s.line, Some(3));
        assert_eq!(s.missing.as_deref(), Some("enemySizeModifier"));
        assert_eq!(
            s.key(),
            "oTracerShot gml_Object_oTracerShot_Alarm_0 line 3 missing enemySizeModifier"
        );
        // Another error, no stack line: the runner's event name, nothing missing.
        let s = Site::parse(
            "ERROR in action number 1\nof Step Event2 for object oEnemyBeamChargeGreen:\nREAL argument is unset",
        );
        assert_eq!(s.object, "oEnemyBeamChargeGreen");
        assert_eq!(s.event, "Step Event2");
        assert_eq!((s.line, s.missing), (None, None));
        assert_eq!(Site::parse("").key(), "? ?");
    }

    #[test]
    fn totals_add_up() {
        let t = Totals {
            copies: 1,
            offspring: 2,
            objects: 3,
            calls: 4,
            remade: 9,
            left_out: 9,
        };
        assert_eq!(t.contained(), 10);
    }

    /// An error repeating every frame: its site's first line, then one a
    /// [`crate::game::LIMIT_EVERY`] with the count left out.
    #[test]
    fn a_site_repeating_every_frame_is_logged_once_per_window() {
        let t0 = Instant::now();
        let frame = Duration::from_millis(16);
        let mut site = SiteCount::default();
        let mut quiet = Vec::new();
        for f in 0..1251u32 {
            let due = site.count(t0 + frame * f);
            if let Some(l) = site_line("x", due) {
                quiet.push(l);
            }
        }
        assert_eq!(site.n, 1251);
        // 0 s, 10.0 s (frame 625), 20.0 s (frame 1250).
        assert_eq!(
            quiet,
            [
                "co-op guard: x",
                "co-op guard: x (624 more at this site not logged before it)",
                "co-op guard: x (624 more at this site not logged before it)",
            ]
        );
    }
}
