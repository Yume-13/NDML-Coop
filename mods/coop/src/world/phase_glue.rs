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

//! An enemy's phase machine is the host's word (a child of the world view,
//! `view_glue`).
//!
//! An enemy with a `phases` table runs it from `oEnemyStandard_Step_0`, which
//! counts `phaseTimer` down and at 0 calls `enemyPhaseAdvance`; its phase
//! actions (`enemyChargeUp`, `enemyEscape`, `starEaterChomp`) call it too. A
//! copy running it alone would go by its own dice, screen and target.
//!
//! * Host: after each `enemyPhaseAdvance` of a replicated instance, the new
//!   `phase` and `phaseTimer` go out with the host's time ([`PHASE_CALL`],
//!   reliable).
//! * Joining game: a copy's own advance is vetoed ([`veto`]). At the word's
//!   shown time the copy runs `enemyPhaseAdvance(phase)` itself, so the entry
//!   is its own, then takes the host's `phaseTimer`.
//! * An enemy's own phase clock ([`PHASE_CLOCKS`]), an alarm event that
//!   writes `phase`, follows the same rule: the copy runs the event once
//!   (`event_perform`) at the word's shown time.
//! * Repairs leave out what a word carries while the host's last word
//!   carries the phase captured ([`repair_leaves`]). Other code's direct
//!   writes of `phase` go with the repairs.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use ndml_net::msg::{NetId, StateValue, WorldCall};
use ndml_sdk::{Builtin, Call, ModContext, Script, Value, Variable};

use super::super::{ROLE_CLIENT, ROLE_HOST, WorldHooks};

/// The pseudo-call carrying one host phase change: net id, its count, the
/// phase, the timer.
pub const PHASE_CALL: &str = "ndml:phase";
/// The machine's advance.
pub const ADVANCE: &str = "gml_Script_enemyPhaseAdvance";
/// Words kept waiting per copy at most (a copy that never comes).
const PENDING_MAX: usize = 16;

/// One host phase change: the net id, its count on the host (in order,
/// from 1), the phase and timer after it, and the host's time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhaseWord {
    pub net: NetId,
    pub seq: u32,
    pub phase: f64,
    pub timer: f64,
    pub at_ms: u32,
}

/// The call's arguments.
#[must_use]
pub fn phase_args(w: &PhaseWord) -> Vec<StateValue> {
    vec![
        StateValue::Real(f64::from(w.net)),
        StateValue::Real(f64::from(w.seq)),
        StateValue::Real(w.phase),
        StateValue::Real(w.timer),
    ]
    .into_iter()
    .map(StateValue::compact)
    .collect()
}

/// A phase word from a call (`None`: another call, or not four numbers).
#[must_use]
pub fn phase_word(c: &WorldCall) -> Option<PhaseWord> {
    if c.script != PHASE_CALL {
        return None;
    }
    let n = |i: usize| match c.args.get(i)?.clone().expanded() {
        StateValue::Real(v) if v.is_finite() => Some(v),
        StateValue::Int(i) => Some(f64::from(i32::try_from(i).ok()?)),
        _ => None,
    };
    let whole = |v: f64, max: f64| (v >= 0.0 && v <= max && v.fract() == 0.0).then_some(v);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "whole numbers within the type, checked just above"
    )]
    Some(PhaseWord {
        net: whole(n(0)?, f64::from(u16::MAX))? as NetId,
        seq: whole(n(1)?, f64::from(u32::MAX))? as u32,
        phase: n(2)?,
        timer: n(3)?,
        at_ms: c.time_ms,
    })
}

/// Whether host time `a` is at or before `b` (ms, the clock wraps).
#[expect(
    clippy::cast_possible_wrap,
    reason = "the wrapped difference as a signed span"
)]
fn at_or_before(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) >= 0
}

/// Host: the words of this frame, each net's count and last word.
#[derive(Debug, Default)]
pub struct PhaseHost {
    seq: HashMap<NetId, u32>,
    last: HashMap<NetId, f64>,
    out: Vec<PhaseWord>,
}

impl PhaseHost {
    /// The host's `net` advanced to `phase` with `timer` at `now_ms`.
    pub fn changed(&mut self, net: NetId, phase: f64, timer: f64, now_ms: u32) {
        let seq = self.seq.entry(net).or_insert(0);
        *seq = seq.wrapping_add(1).max(1);
        self.last.insert(net, phase);
        self.out.push(PhaseWord {
            net,
            seq: *seq,
            phase,
            timer,
            at_ms: now_ms,
        });
    }

    /// This frame's words, to send.
    pub fn take(&mut self) -> Vec<PhaseWord> {
        std::mem::take(&mut self.out)
    }

    /// The phase the host's last word for `net` carried.
    #[must_use]
    pub fn last_phase(&self, net: NetId) -> Option<f64> {
        self.last.get(&net).copied()
    }
}

/// Joining game: the host's words waiting for their shown time, by copy
/// and count, and the count each copy last took.
#[derive(Debug, Default)]
pub struct PhaseClient {
    pending: HashMap<NetId, BTreeMap<u32, PhaseWord>>,
    taken: HashMap<NetId, u32>,
}

impl PhaseClient {
    /// A word arrived: kept unless this copy took it, or a later one,
    /// already (a repeat, or one overtaken).
    pub fn arrived(&mut self, w: PhaseWord) {
        if self.taken.get(&w.net).is_some_and(|t| w.seq <= *t) {
            return;
        }
        let q = self.pending.entry(w.net).or_default();
        q.insert(w.seq, w);
        while q.len() > PENDING_MAX {
            q.pop_first();
        }
    }

    /// The words whose host time the shown time `shown_ms` reached, each
    /// copy's in order; taken here (each once).
    pub fn due(&mut self, shown_ms: Option<u32>) -> Vec<PhaseWord> {
        let Some(shown) = shown_ms else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (net, q) in &mut self.pending {
            while let Some((&seq, w)) = q.first_key_value() {
                if !at_or_before(w.at_ms, shown) {
                    break;
                }
                out.push(*w);
                q.remove(&seq);
                self.taken.insert(*net, seq);
            }
        }
        self.pending.retain(|_, q| !q.is_empty());
        out.sort_by_key(|w| (w.at_ms, w.net, w.seq));
        out
    }

    /// A due word its copy could not take yet (deactivated here: this game's
    /// freeze, the birth-order hold): due again at the next frame, before
    /// any later one of that copy.
    pub fn again(&mut self, w: PhaseWord) {
        if self.taken.get(&w.net).is_some_and(|t| *t >= w.seq) {
            self.taken.insert(w.net, w.seq.saturating_sub(1));
        }
        let q = self.pending.entry(w.net).or_default();
        q.insert(w.seq, w);
        while q.len() > PENDING_MAX {
            q.pop_first();
        }
    }

    /// Words waiting.
    #[cfg(test)]
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.pending.values().map(BTreeMap::len).sum()
    }
}

/// Whether an `enemyPhaseAdvance` is vetoed: on a joining game, on a copy of
/// a host instance, unless it is the host's word being applied. The host's
/// own (its stand-in bracket's events included) always run.
#[must_use]
pub fn veto(role: u8, is_copy: bool, applying: bool) -> bool {
    role == ROLE_CLIENT && is_copy && !applying
}

/// What a due word does to its copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// The copy takes it.
    Take,
    /// Kept for the next frame: the copy is here but deactivated (this
    /// game's freeze, the birth-order hold). Repairs do not carry the phase.
    Wait,
    /// Dropped: no live copy here (not made yet: its first state carries
    /// the host's phase; or gone), or one this game's prediction killed (it
    /// dies by its own code, never by a phase).
    Drop,
}

/// The decision for a due word: `copy_live` (a copy of it in the table),
/// `active` (that copy active here), `dying` (this game's prediction killed
/// it).
#[must_use]
pub fn apply(copy_live: bool, active: bool, dying: bool) -> Apply {
    match (copy_live, active, dying) {
        (false, _, _) | (true, true, true) => Apply::Drop,
        (true, false, _) => Apply::Wait,
        (true, true, false) => Apply::Take,
    }
}

/// The variables the phase word carries for a machine user, left out of its
/// repairs while the host's last word carries the phase now captured.
pub const CARRIED: [&str; 3] = ["phase", "phaseTimer", "phaseDuration"];

/// An enemy's own phase clock: the object, the alarm, and what that event
/// writes on its instance (an alarm event that writes `phase` on its
/// instance, re-arms that alarm, and calls no GML script and no builtin but
/// pure ones). No Star Eater object is one.
pub const PHASE_CLOCKS: &[(&str, u8, &[&str])] = &[
    (
        "oCelestial",
        0,
        &[
            "bombAlt",
            "bombDist",
            "bombEndPositionX",
            "bombEndPositionY",
            "bombIncrement",
            "bombPhase",
            "bombProgression",
            "bombStartPosition",
            "bombStartPositionX",
            "bombStartPositionY",
            "canFire",
            "clockwise",
            "dronePhase",
            "phase",
            "shots",
            "targOrbitShape",
            "targOrbitSpeed",
            "targRot",
            "targSpeed",
        ],
    ),
    (
        "oStationCore",
        5,
        &[
            "intro",
            "phase",
            "phaseCount",
            "phaseOrbs",
            "phaseOrbs2",
            "phaseVulcan",
            "vulcanCount",
        ],
    ),
];

/// The phase clock's event function.
#[must_use]
pub fn clock_event(object: &str, alarm: u8) -> String {
    format!("gml_Object_{object}_Alarm_{alarm}")
}

/// `ev_alarm`, `event_perform`'s event type.
const EV_ALARM: f64 = 2.0;

/// What a due word does to a phase clock's copy (`copy_phase`: its phase
/// now): its event runs, or (the copy already in the word's phase: its first
/// state carried the host's run) the word is taken without a run.
#[must_use]
pub fn clock_runs(copy_phase: Option<f64>, word_phase: f64) -> bool {
    copy_phase.map(f64::to_bits) != Some(word_phase.to_bits())
}

/// What a repair leaves out (`has_phases`: a machine user; `clock`: a phase
/// clock's writes): [`CARRIED`] for a machine user, the clock's writes for a
/// phase clock, while the host's last word carries the phase captured;
/// nothing otherwise.
#[must_use]
pub fn repair_leaves(
    has_phases: bool,
    clock: Option<&'static [&'static str]>,
    captured_phase: Option<f64>,
    last_word: Option<f64>,
) -> &'static [&'static str] {
    let carried =
        matches!((captured_phase, last_word), (Some(c), Some(w)) if c.to_bits() == w.to_bits());
    match (carried, clock) {
        (true, Some(writes)) => writes,
        (true, None) if has_phases => &CARRIED,
        _ => &[],
    }
}

/// The phase words' counts.
#[derive(Debug, Default, Clone, Copy)]
pub struct PhaseCounts {
    pub sent: u64,
    pub applied: u64,
    pub vetoed: u64,
    pub dropped: u64,
    /// Phase clocks: words taken by a run of the event, words taken
    /// without one (the copy already in that phase), runs put to the word's
    /// phase after.
    pub clock_runs: u64,
    pub clock_taken: u64,
    pub clock_put: u64,
}

/// Per-session state (in `ViewState`).
#[derive(Debug, Default)]
pub(super) struct PhaseState {
    pub(super) host: PhaseHost,
    pub(super) client: PhaseClient,
    pub(super) counts: PhaseCounts,
}

struct Handles {
    advance: Option<Script>,
    phase: Option<Variable>,
    timer: Option<Variable>,
    duration: Option<Variable>,
    /// `event_perform` (a phase clock's run at the host's word).
    perform: Option<Builtin>,
}

static H: OnceLock<Handles> = OnceLock::new();

/// The phase clocks' object indexes, alarms and writes (resolved at the
/// first frame that needs them; an object this game lacks is left out).
static CLOCKS: OnceLock<Vec<(u32, u8, &'static [&'static str])>> = OnceLock::new();

/// This game is applying the host's word now (its own advance runs).
static APPLYING: AtomicBool = AtomicBool::new(false);

/// Resolve the handles (init).
pub(super) fn resolve(ctx: &ModContext) {
    let _ = H.set(Handles {
        advance: ctx.script(ADVANCE).ok(),
        phase: ctx.variable("phase").ok(),
        timer: ctx.variable("phaseTimer").ok(),
        duration: ctx.variable("phaseDuration").ok(),
        perform: ctx.builtin(crate::names::rules_fn::EVENT_PERFORM).ok(),
    });
}

impl WorldHooks {
    /// Before `enemyPhaseAdvance`: a copy's own is vetoed ([`veto`]).
    pub(in crate::world) fn phase_before(&self, call: &mut Call<'_>) {
        let role = self.role();
        if role != ROLE_CLIENT {
            return;
        }
        let Some(me) = call.instance() else { return };
        let Ok(id) = me.builtin_value(crate::world::names::builtin_var::ID) else {
            return;
        };
        let key = me.key();
        let net = self.lock().client.net_id_for(key, &id);
        if veto(role, net.is_some(), APPLYING.load(Ordering::Relaxed)) {
            call.skip_original();
            let _ = call.set_return(0.0);
            let mut st = self.lock();
            st.view.phase.counts.vetoed += 1;
        }
    }

    /// After `enemyPhaseAdvance` (or a phase clock's event) on the host: the
    /// word for a replicated instance.
    pub(in crate::world) fn phase_after(&self, call: &Call<'_>) {
        if self.role() != ROLE_HOST {
            return;
        }
        let (Some(h), Some(me)) = (H.get(), call.instance()) else {
            return;
        };
        let Some(pv) = h.phase else {
            return;
        };
        let Ok(id) = me.builtin_value(crate::world::names::builtin_var::ID) else {
            return;
        };
        let Some(phase) = me.get_f64(pv) else {
            return;
        };
        // A phase clock's object has no `phaseTimer`: 0.
        let timer = h.timer.and_then(|v| me.get_f64(v)).unwrap_or(0.0);
        let key = me.key();
        let mut st = self.lock();
        let Some(net) = st.host.net_id_for(key, &id) else {
            return;
        };
        let now = st.repl.now_ms;
        st.view.phase.host.changed(net, phase, timer, now);
        st.view.phase.counts.sent += 1;
    }

    /// Host, with the view: this frame's words as calls.
    pub(super) fn phase_host(&self) -> Vec<WorldCall> {
        self.lock()
            .view
            .phase
            .host
            .take()
            .iter()
            .map(|w| WorldCall {
                time_ms: w.at_ms,
                script: PHASE_CALL.to_owned(),
                args: phase_args(w),
            })
            .collect()
    }

    /// Client, from the inbox: a host word.
    pub(super) fn phase_take(&self, c: &WorldCall) {
        if let Some(w) = phase_word(c) {
            self.lock().view.phase.client.arrived(w);
        }
    }

    /// Client, each frame after the copies are made: the words the shown
    /// time reached, each applied to its copy once.
    pub(in crate::world) fn phase_client(&self, call: &Call<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let Some(h) = H.get() else { return };
        let clocks = self.clocks(call);
        let (due, shown) = {
            let mut st = self.lock();
            let shown = st.repl.shown_ms;
            let due = st.view.phase.client.due(shown);
            let due = due
                .into_iter()
                .map(|w| (w, st.client.id_of(w.net)))
                .collect::<Vec<_>>();
            (due, shown.unwrap_or(0))
        };
        let _ = shown;
        for (w, id) in due {
            let Some(id) = id else {
                let mut st = self.lock();
                st.view.phase.counts.dropped += 1;
                continue;
            };
            if apply(true, self.exists(call, &id), false) == Apply::Wait {
                self.lock().view.phase.client.again(w);
                continue;
            }
            let mut applied = false;
            let _ = call.with(&id, |inst, _| {
                if apply(true, true, self.dmg.is_dying(inst.key())) == Apply::Drop {
                    return;
                }
                let object = inst
                    .builtin_value("object_index")
                    .ok()
                    .as_ref()
                    .and_then(crate::world::classify::object_index);
                let clock = object.and_then(|o| clocks.iter().find(|c| c.0 == o));
                if let Some(&(_, alarm, _)) = clock {
                    applied = self.clock_apply(&inst, h, alarm, w.phase);
                    return;
                }
                let (Some(advance), Some(tv)) = (h.advance, h.timer) else {
                    return;
                };
                APPLYING.store(true, Ordering::Relaxed);
                let _ = inst.call_script(advance, &[Value::Real(w.phase)]);
                APPLYING.store(false, Ordering::Relaxed);
                let _ = inst.set_f64(tv, w.timer);
                if let Some(d) = h.duration {
                    let _ = inst.set_f64(d, w.timer);
                }
                applied = true;
            });
            let mut st = self.lock();
            if applied {
                st.view.phase.counts.applied += 1;
            } else {
                st.view.phase.counts.dropped += 1;
            }
        }
    }

    /// Host repair of `net`, an `object` (`gen_glue`): the variables left
    /// out ([`repair_leaves`]).
    pub(in crate::world) fn phase_repair_leaves(
        &self,
        call: &Call<'_>,
        net: NetId,
        object: u32,
        has_phases: bool,
        phase: Option<f64>,
    ) -> &'static [&'static str] {
        let clock = self
            .clocks(call)
            .iter()
            .find(|c| c.0 == object)
            .map(|c| c.2);
        let last = self.lock().view.phase.host.last_phase(net);
        repair_leaves(has_phases, clock, phase, last)
    }

    /// The phase clocks this game has ([`PHASE_CLOCKS`] by object index).
    fn clocks(&self, call: &Call<'_>) -> &'static [(u32, u8, &'static [&'static str])] {
        if let Some(c) = CLOCKS.get() {
            return c;
        }
        let found: Vec<_> = PHASE_CLOCKS
            .iter()
            .filter_map(|&(name, alarm, writes)| Some((self.asset(call, name)?, alarm, writes)))
            .collect();
        // Before the objects are there (the menus) nothing is kept.
        if found.is_empty() {
            return &[];
        }
        CLOCKS.get_or_init(|| found)
    }

    /// Before a phase clock's event ([`PHASE_CLOCKS`]): a copy's own run is
    /// skipped on a joining game ([`veto`]; its alarm then stays off until
    /// the host's word runs the event).
    pub(in crate::world) fn clock_before(&self, call: &mut Call<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let Some(me) = call.instance() else { return };
        let Ok(id) = me.builtin_value(crate::world::names::builtin_var::ID) else {
            return;
        };
        let key = me.key();
        let net = self.lock().client.net_id_for(key, &id);
        if veto(ROLE_CLIENT, net.is_some(), APPLYING.load(Ordering::Relaxed)) {
            call.skip_original();
            let mut st = self.lock();
            st.view.phase.counts.vetoed += 1;
        }
    }

    /// After a phase clock's event: the host's word ([`Self::phase_after`]).
    pub(in crate::world) fn clock_after(&self, call: &Call<'_>) {
        self.phase_after(call);
    }

    /// A due word for a phase clock's copy `inst`: its event `alarm` runs
    /// once now as the host's did ([`clock_runs`]), and its phase is the
    /// word's after. Whether the word was taken.
    fn clock_apply(
        &self,
        inst: &ndml_sdk::Instance<'_>,
        h: &Handles,
        alarm: u8,
        phase: f64,
    ) -> bool {
        let (Some(perform), Some(pv)) = (h.perform, h.phase) else {
            return false;
        };
        let run = clock_runs(inst.get_f64(pv), phase);
        if run {
            APPLYING.store(true, Ordering::Relaxed);
            let _ = inst.call_builtin(perform, &[EV_ALARM.into(), f64::from(alarm).into()]);
            APPLYING.store(false, Ordering::Relaxed);
        }
        let put = inst.get_f64(pv).map(f64::to_bits) != Some(phase.to_bits());
        if put {
            let _ = inst.set_f64(pv, phase);
        }
        let mut st = self.lock();
        let c = &mut st.view.phase.counts;
        if run {
            c.clock_runs += 1;
        } else {
            c.clock_taken += 1;
        }
        c.clock_put += u64::from(put);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(net: NetId, seq: u32, phase: f64, at_ms: u32) -> PhaseWord {
        PhaseWord {
            net,
            seq,
            phase,
            timer: 60.0,
            at_ms,
        }
    }

    #[test]
    fn a_word_goes_and_comes_back() {
        let w = PhaseWord {
            net: 42,
            seq: 7,
            phase: 3.0,
            timer: 120.5,
            at_ms: 60_000,
        };
        let c = WorldCall {
            time_ms: w.at_ms,
            script: PHASE_CALL.to_owned(),
            args: phase_args(&w),
        };
        assert_eq!(phase_word(&c), Some(w));
        let other = WorldCall {
            script: "ndml:director".to_owned(),
            ..c.clone()
        };
        assert_eq!(phase_word(&other), None);
        let short = WorldCall {
            args: c.args[..3].to_vec(),
            ..c
        };
        assert_eq!(phase_word(&short), None);
    }

    /// A copy never advances on its own; the host's own always runs (its
    /// stand-in bracket's events with it: the bracket is never what moves
    /// a phase, the host's advance is).
    #[test]
    fn a_copy_never_advances_alone() {
        assert!(veto(ROLE_CLIENT, true, false), "the copy's own advance");
        assert!(!veto(ROLE_CLIENT, true, true), "the host's word applied");
        assert!(
            !veto(ROLE_CLIENT, false, false),
            "the joining game's own instance"
        );
        assert!(
            !veto(ROLE_HOST, true, false),
            "the host's (bracketed or not)"
        );
        assert!(!veto(ROLE_HOST, false, false));
    }

    /// A host change is applied once, at its shown time, with its timer;
    /// two changes in flight each at their own time, in order.
    #[test]
    fn a_host_change_is_applied_once_at_its_shown_time() {
        let mut c = PhaseClient::default();
        c.arrived(word(5, 1, 1.0, 1000));
        c.arrived(word(5, 2, 2.0, 1100));
        assert!(c.due(Some(990)).is_empty(), "not shown yet");
        assert!(c.due(None).is_empty(), "no shown time");
        let first = c.due(Some(1000));
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].phase, first[0].timer), (1.0, 60.0));
        assert!(c.due(Some(1050)).is_empty(), "once");
        assert_eq!(
            c.due(Some(1200)).iter().map(|w| w.seq).collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(c.waiting(), 0);
    }

    /// Out of order and repeated: each count is taken once, never one older
    /// than the last taken; the clock may wrap.
    #[test]
    fn out_of_order_and_repeated_words_are_taken_once_in_order() {
        let mut c = PhaseClient::default();
        c.arrived(word(5, 3, 3.0, 1200));
        c.arrived(word(5, 2, 2.0, 1100));
        c.arrived(word(5, 2, 2.0, 1100));
        let got: Vec<u32> = c.due(Some(1300)).iter().map(|w| w.seq).collect();
        assert_eq!(got, [2, 3]);
        c.arrived(word(5, 3, 3.0, 1200));
        c.arrived(word(5, 1, 1.0, 1000));
        assert!(c.due(Some(1300)).is_empty(), "a repeat and an old one");
        let mut w = PhaseClient::default();
        w.arrived(word(9, 1, 1.0, u32::MAX - 5));
        assert_eq!(w.due(Some(10)).len(), 1, "across the wrap");
    }

    /// A word that comes due while its copy is deactivated here (this game
    /// paused) waits for the copy instead of being dropped, and comes due
    /// again before the host's next word.
    #[test]
    fn a_word_for_a_deactivated_copy_waits_for_it() {
        assert_eq!(apply(true, false, false), Apply::Wait, "frozen or held");
        assert_eq!(apply(true, false, true), Apply::Wait);
        let mut c = PhaseClient::default();
        c.arrived(word(41, 1, 1.0, 120_000));
        let due = c.due(Some(120_100));
        assert_eq!(due.len(), 1);
        c.again(due[0]);
        c.arrived(word(41, 1, 1.0, 120_000));
        assert_eq!(c.waiting(), 1, "a resend is the same word");
        c.arrived(word(41, 2, 2.0, 130_000));
        let due = c.due(Some(128_000));
        assert_eq!(
            due.iter().map(|w| (w.seq, w.phase)).collect::<Vec<_>>(),
            vec![(1, 1.0)],
            "due again after the resume, the next one at its own time"
        );
        let due = c.due(Some(130_050));
        assert_eq!(due.iter().map(|w| w.seq).collect::<Vec<_>>(), vec![2]);
        assert!(c.due(Some(140_000)).is_empty(), "each taken once");
    }

    /// An enemy this game predicted dead takes no word (it dies by its own
    /// code); one with no live copy here neither. Waiting words are bounded.
    #[test]
    fn a_copy_predicted_dead_takes_no_word() {
        assert_eq!(apply(true, true, false), Apply::Take);
        assert_eq!(apply(true, true, true), Apply::Drop, "predicted dead here");
        assert_eq!(apply(false, false, false), Apply::Drop, "no live copy");
        let mut c = PhaseClient::default();
        for s in 1..=(PENDING_MAX as u32 + 4) {
            c.arrived(word(6, s, 1.0, 5000));
        }
        assert_eq!(c.waiting(), PENDING_MAX, "bounded");
    }

    #[test]
    fn the_host_counts_each_nets_changes() {
        let mut h = PhaseHost::default();
        h.changed(5, 1.0, 80.0, 100);
        h.changed(6, 0.0, 30.0, 100);
        h.changed(5, 2.0, 60.0, 116);
        let out = h.take();
        assert_eq!(
            out.iter().map(|w| (w.net, w.seq)).collect::<Vec<_>>(),
            [(5, 1), (6, 1), (5, 2)]
        );
        assert!(h.take().is_empty());
        assert_eq!(h.last_phase(5), Some(2.0));
        assert_eq!(h.last_phase(7), None);
    }

    /// A repair of a machine user leaves the phase out while the host's
    /// last word carries it; a phase other code wrote goes with the repair.
    /// A phase clock's repair leaves out everything its event writes, on the
    /// same condition.
    #[test]
    fn a_repair_leaves_out_what_the_word_carries() {
        assert_eq!(repair_leaves(true, None, Some(2.0), Some(2.0)), CARRIED);
        assert!(
            repair_leaves(true, None, Some(3.0), Some(2.0)).is_empty(),
            "a direct write"
        );
        assert!(
            repair_leaves(true, None, Some(2.0), None).is_empty(),
            "no word yet"
        );
        assert!(
            repair_leaves(false, None, Some(2.0), Some(2.0)).is_empty(),
            "no machine"
        );
        let celestial = PHASE_CLOCKS[0].2;
        assert_eq!(
            repair_leaves(false, Some(celestial), Some(1.0), Some(1.0)),
            celestial
        );
        assert!(repair_leaves(false, Some(celestial), Some(2.0), Some(1.0)).is_empty());
    }

    /// A phase clock's word runs its event unless the copy is in that phase
    /// already (its first state carried the host's run: a second run would
    /// move it on, the Celestial's `phase += 1`).
    #[test]
    fn a_clock_word_runs_the_event_once_from_the_phase_before() {
        assert!(clock_runs(Some(1.0), 2.0));
        assert!(clock_runs(Some(3.0), 1.0), "the wrap of the cycle");
        assert!(!clock_runs(Some(2.0), 2.0), "already in it: taken");
        assert!(clock_runs(None, 2.0), "no phase read: run");
        assert_eq!(
            clock_event("oCelestial", 0),
            "gml_Object_oCelestial_Alarm_0"
        );
    }

    /// Each phase clock writes `phase`, not a machine's `phases`, and none is
    /// a Star Eater object.
    #[test]
    fn phase_clocks_write_phase_and_none_is_the_star_eaters() {
        for (object, _, writes) in PHASE_CLOCKS {
            assert!(!object.starts_with("oStarEater"), "{object}");
            assert!(writes.contains(&"phase"), "{object}");
            assert!(!writes.contains(&"phases"), "{object}: a machine user");
        }
    }
}
