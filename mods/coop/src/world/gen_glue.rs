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

//! Generic state transfer in the world hooks (how every copy is made; see
//! [`super::super::state`]): the host captures each replicated instance's
//! whole state the frame after its first tick and sends it; a client applies
//! it to the copy right after creating it. Repairs follow: at once when an
//! instance gains variables or changes a reference, and periodically for what
//! changed in the rest. A repair carries the instance's age when read, and
//! the copy takes it at that age; a state for a deactivated copy waits.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use ndml_net::msg::{InstanceState, InstanceStateIx, Message, NetId, StateNames, StateValue};
use ndml_sdk::{Call, Value};

use super::super::state::{self, Captured, Stats};
use super::WorldHooks;

/// A reference to a copy that does not exist yet is given up after this
/// many running frames (`GenState::run_frames`).
const WAIT_FRAMES: u64 = 600;
/// Host: periodic repairs captured per frame at most (each is a full
/// capture). The safety net: what changes in a variable the copy has.
const REPAIRS_PER_FRAME: usize = 3;
/// Host: change repairs captured per frame at most, ahead of the periodic
/// ones; the rest the next frame, the longest waiting first.
const CHANGES_PER_FRAME: usize = 64;
/// Host: reference variables the change test reads per frame at most, the
/// instances taken in turn.
const REF_READS_PER_FRAME: u64 = 1500;
/// Host: first states captured per frame at most; the rest the next frame,
/// in birth order (a maker's first state goes before or with its members').
const FIRST_STATES_PER_FRAME: usize = 16;
/// Client: frames a copy waits for the maker its first state names; then it
/// is made without it.
pub(super) const COPY_MAKER_WAIT_FRAMES: u64 = 30;

#[derive(Debug)]
struct HostGen {
    key: usize,
    id: Value,
    object: u16,
    born: u64,
    /// Its first state as sent (a resync sends it again).
    first: Captured,
    /// The capture of the last periodic repair (or the first state), with
    /// change repairs folded in: what a repair is judged against.
    last: Captured,
    seq: u16,
    sent_at: u64,
    /// Narrow repairs: the numbers that changed in the last repair window
    /// (`None` before the first).
    moved: Option<HashSet<String>>,
    /// Variables the instance held at the last capture
    /// ([`state::names_count`]): another count is a change repair.
    count: usize,
    /// A change seen and not captured yet (budget): since this frame.
    change_since: Option<u64>,
    /// The variables that held an instance reference (or `noone`) at a
    /// capture, and what they held at the last: another value is a change.
    refs: Arc<Vec<String>>,
    ref_sig: Vec<u64>,
}

#[derive(Debug)]
struct Made {
    id: Value,
    object: u16,
    born: u64,
    /// Made again for the same enemy (a refused kill): its repairs go at
    /// once (the age they were read at is the first copy's).
    again: bool,
    /// A first state was applied to this copy.
    first_done: bool,
}

/// A reference change to send this frame: (net, id, object, born, the
/// watched references).
type RefDue = (NetId, Value, u16, u64, Arc<Vec<String>>);

/// One live host instance as the frame's pass reads it.
struct Live {
    net: NetId,
    key: usize,
    id: Value,
    object: u16,
    born: u64,
    sent_at: u64,
    count: usize,
    change_since: Option<u64>,
    refs: Arc<Vec<String>>,
    ref_sig: Vec<u64>,
}

/// Generic transfer state (both roles).
#[derive(Debug, Default)]
pub(super) struct GenState {
    /// Host: first ticks to capture (net id, key, id, object, frame).
    host_new: Vec<(NetId, usize, Value, u16, u64)>,
    /// Host: members born since the last run step, by a tracked maker (maker
    /// key, member key): the maker's gains go ahead of them.
    pub(super) members_born: Vec<(usize, usize)>,
    /// Host: instances `instance_copy` made (key, frame): their first state
    /// says so ([`state::ORIGIN_BUILTIN`]).
    pub(super) copy_born: HashMap<usize, u64>,
    /// Host: instances that ran user event 0 (the set-up step), seen by the
    /// loader's event observer (key, frame).
    pub(super) setup_seen: HashMap<usize, u64>,
    /// Host: instances whose user event 1 another instance ran (key,
    /// frame), outside their own user event 0
    /// ([`state::MAKER_EVENT_BUILTIN`]).
    pub(super) event1_by_maker: HashMap<usize, u64>,
    /// Host: instances inside their own user event 0 now (its user event 1
    /// from there is the set-up's own: a Serpent link's chain).
    pub(super) in_setup: HashSet<usize>,
    /// Client: first states held until their maker's is written (net id,
    /// running frame first held: `run_frames`).
    pub(super) held_for_maker: HashMap<NetId, u64>,
    /// Client: run step frames outside a freeze: what waits on the joining
    /// side counts these, not wall frames.
    run_frames: u64,
    /// Host: the observer runs (else the `instance_copy` rule decides).
    pub(super) observer: bool,
    host_live: HashMap<NetId, HostGen>,
    /// Host: where the reference test starts this frame (in turn).
    ref_cursor: usize,
    /// Host: (object, variable) logged as not a reference.
    ref_numbers_seen: HashSet<(u16, String)>,
    /// Client: parts of a state being collected.
    parts: HashMap<(NetId, u16), Vec<InstanceState>>,
    /// Client: complete states not yet applied.
    ready: Vec<(NetId, u16, Captured)>,
    /// Client: copies made (to apply states to).
    made: HashMap<NetId, Made>,
    /// Client: each live enemy's first state, kept after it was applied: a
    /// copy made again for the same enemy (a refused kill) gets it too.
    kept: HashMap<NetId, Captured>,
    /// Client: references to copies not made yet: (referrer, variable,
    /// target, since frame).
    waiting: Vec<(NetId, String, NetId, u64)>,
    /// Client: enemies that copies made (key, id, frame): let through so the code
    /// that made them goes on as in the game, and removed silently after the
    /// set-up step or at their first tick (the host's arrive).
    pub(super) scratch: Vec<(usize, Value, u64)>,
    /// Host, compact: each object's variable names and their indices.
    host_names: HashMap<u16, HashMap<String, u16>>,
    /// Client, compact: each object's variable names by index.
    client_names: HashMap<u16, Vec<String>>,
    /// Host, paced: messages waiting to go (message, encoded bytes).
    outq: VecDeque<(Message, usize)>,
}

impl GenState {
    /// Host, the partner back (a resync): the name tables start again and
    /// each live instance's first state goes again; returns them (net, id,
    /// object, first) with the repair base cleared.
    pub(super) fn resync_plan(&mut self) -> Vec<(NetId, Value, u16, Captured)> {
        self.host_names.clear();
        let live = self.host_live.iter_mut();
        live.map(|(&n, g)| {
            g.last = Captured::default();
            g.moved = None;
            (n, g.id.clone(), g.object, g.first.clone())
        })
        .collect()
    }

    /// Client, each run step: copies the table no longer knows lose their
    /// `made` entry and kept first state, except one that died here on a kill
    /// claim not settled yet (`open`): a refusal remakes it from the kept
    /// state.
    pub(super) fn forget_gone(
        &mut self,
        client: &crate::world::client::ClientWorld,
        open: &[NetId],
    ) {
        let here = |n: &NetId| client.is_known(*n) || open.contains(n);
        self.kept.retain(|n, _| here(n));
        self.made.retain(|n, _| here(n));
    }

    /// Client: the first state of `net` is here (complete, not applied yet).
    pub(super) fn has_state(&self, net: NetId) -> bool {
        self.kept.contains_key(&net) || self.ready.iter().any(|r| r.0 == net && r.1 == 0)
    }

    /// Client: the maker the first state of `net` names
    /// ([`state::MAKER_BUILTIN`]), if any.
    pub(super) fn first_maker(&self, net: NetId) -> Option<NetId> {
        let c = self.kept.get(&net).or_else(|| {
            self.ready
                .iter()
                .find(|r| r.0 == net && r.1 == 0)
                .map(|r| &r.2)
        })?;
        c.builtins.iter().find_map(|(i, v)| match v {
            StateValue::Instance(m) if *i == state::MAKER_BUILTIN && *m != net => Some(*m),
            _ => None,
        })
    }

    /// Client: order a frame's copies so that a maker is made (and its state
    /// written) before what it made (the Star Eater copy's set-up reads its
    /// controller's `centerX`). Stable otherwise.
    pub(super) fn makers_first(&self, chosen: &mut [ndml_net::msg::EnemySpawn]) {
        let ids: Vec<NetId> = chosen.iter().map(|s| s.net_id).collect();
        let depth = |mut n: NetId| {
            let mut d = 0u8;
            while d < 8 {
                match self.first_maker(n) {
                    Some(m) if ids.contains(&m) => {
                        d += 1;
                        n = m;
                    }
                    _ => break,
                }
            }
            d
        };
        chosen.sort_by_key(|s| depth(s.net_id));
    }

    /// Client: the other enemies the first state of `net` refers to (deep).
    pub(super) fn first_refs(&self, net: NetId) -> Vec<NetId> {
        fn walk(v: &StateValue, out: &mut Vec<NetId>) {
            match v {
                StateValue::Instance(n) => out.push(*n),
                StateValue::Array(items) => items.iter().for_each(|i| walk(i, out)),
                StateValue::Struct(m) => m.iter().for_each(|(_, i)| walk(i, out)),
                StateValue::Method { bound, .. } => walk(bound, out),
                _ => {}
            }
        }
        let c = self.kept.get(&net).or_else(|| {
            self.ready
                .iter()
                .find(|r| r.0 == net && r.1 == 0)
                .map(|r| &r.2)
        });
        let mut out = Vec::new();
        if let Some(c) = c {
            c.vars.iter().for_each(|(_, v)| walk(v, &mut out));
            c.builtins
                .iter()
                .filter(|(i, _)| *i == state::MAKER_BUILTIN)
                .for_each(|(_, v)| walk(v, &mut out));
        }
        out.retain(|n| *n != net);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Client: a copy of `net` may be made: its first state is here, and so
    /// is that of every enemy it refers to still to be made
    /// ([`Self::made_with`]), so a part never starts before its leader. The
    /// maker its first state names is made first or in the same frame, up to
    /// [`COPY_MAKER_WAIT_FRAMES`]: a member made before it would run without
    /// its set-up.
    pub(super) fn copy_ready(
        &self,
        net: NetId,
        client: &crate::world::client::ClientWorld,
    ) -> bool {
        self.has_state(net)
            && self
                .first_refs(net)
                .iter()
                .all(|m| !client.is_pending(*m) || self.has_state(*m))
            && self.first_maker(net).is_none_or(|m| {
                self.maker_ready(m, client)
                    || client
                        .waited(net)
                        .is_none_or(|w| w >= COPY_MAKER_WAIT_FRAMES)
            })
    }

    /// Client: a maker `m` is made, or can be made this frame with its
    /// members ([`Self::made_with`]).
    fn maker_ready(&self, m: NetId, client: &crate::world::client::ClientWorld) -> bool {
        client.is_known(m) && (!client.is_pending(m) || (self.has_state(m) && client.is_due(m)))
    }

    /// Client: what must be made in the same frame as `chosen`: the enemies
    /// still to be made that their first states refer to, and theirs.
    pub(super) fn made_with(
        &self,
        chosen: &[NetId],
        client: &crate::world::client::ClientWorld,
    ) -> HashSet<NetId> {
        let mut want: HashSet<NetId> = HashSet::new();
        let mut todo: Vec<NetId> = chosen.to_vec();
        while let Some(n) = todo.pop() {
            for m in self.first_refs(n) {
                if client.is_pending(m)
                    && self.has_state(m)
                    && !chosen.contains(&m)
                    && want.insert(m)
                {
                    todo.push(m);
                }
            }
        }
        want
    }
}

impl WorldHooks {
    fn gen_wanted(&self) -> bool {
        self.h.reflect.is_some()
    }

    /// Host: an instance was seen for the first time (tracked as `net`).
    pub(super) fn gen_host_added(&self, net: NetId, key: usize, id: Value, object: u16) {
        if !self.gen_wanted() {
            return;
        }
        let mut st = self.lock();
        let frame = st.frame;
        st.transfer.host_new.push((net, key, id, object, frame));
    }

    /// Host, the partner back: every live instance's first state again (a
    /// copy the resync makes takes its set-up as the first time), then its
    /// whole state now as a repair with no age (applied at once).
    pub(super) fn gen_resync(&self, call: &Call<'_>) {
        let plan = self.lock().transfer.resync_plan();
        for (net, id, object, first) in plan {
            self.send(net, object, 0, &first);
            let now = self.host_capture(call, &id, &mut Stats::default());
            let Some(c) = now.filter(|_| self.exists(call, &id)) else {
                continue;
            };
            let seq = {
                let mut st = self.lock();
                let frame = st.frame;
                let Some(g) = st.transfer.host_live.get_mut(&net) else {
                    continue;
                };
                g.seq = g.seq.wrapping_add(1).max(1);
                (g.last, g.sent_at) = (c.clone(), frame);
                g.seq
            };
            self.send(net, object, seq, &c);
        }
    }

    fn host_capture(&self, call: &Call<'_>, id: &Value, stats: &mut Stats) -> Option<Captured> {
        let r = self.h.reflect.as_ref()?;
        let net_of = |k: usize, v: &Value| self.lock().host.net_id_for(k, v);
        Some(state::capture(call, r, id, &net_of, stats))
    }

    fn send(&self, net: NetId, object: u16, seq: u16, c: &Captured) {
        // Variables by index into per-object name lists sent once (`StateNames`),
        // numbers in fewer bytes.
        let mut st = self.lock();
        let table = st.transfer.host_names.entry(object).or_default();
        let base = u16::try_from(table.len()).unwrap_or(u16::MAX);
        let mut new = Vec::new();
        let mut vars = Vec::with_capacity(c.vars.len());
        for (k, v) in &c.vars {
            let ix = if let Some(ix) = table.get(k) {
                *ix
            } else {
                let ix = u16::try_from(table.len()).unwrap_or(u16::MAX);
                table.insert(k.clone(), ix);
                new.push(k.clone());
                ix
            };
            vars.push((ix, v.clone().compact()));
        }
        drop(st);
        let mut msgs: Vec<Message> = if new.is_empty() {
            Vec::new()
        } else {
            StateNames::split(object, base, new)
                .into_iter()
                .map(Message::StateNames)
                .collect()
        };
        let compact = |v: &[(u8, StateValue)]| -> Vec<(u8, StateValue)> {
            v.iter().map(|(k, v)| (*k, v.clone().compact())).collect()
        };
        msgs.extend(
            InstanceStateIx::split(
                net,
                object,
                seq,
                vars,
                compact(&c.builtins),
                c.alarms.clone(),
            )
            .into_iter()
            .map(Message::InstanceStateIx),
        );
        let mut st = self.lock();
        for m in msgs {
            let n = m.encode().map_or(0, |b| b.len());
            st.transfer.outq.push_back((m, n));
        }
    }

    /// Host: one new instance's first state, captured and sent (with its
    /// dependencies first), and its repair base kept.
    #[allow(clippy::too_many_lines)] // one instance's capture, set-up rule and base: kept together
    fn gen_host_first(
        &self,
        call: &Call<'_>,
        frame: u64,
        (net, key, id, object, born): (NetId, usize, Value, u16, u64),
    ) {
        if !self.exists(call, &id) {
            return;
        }
        let mut stats = Stats::default();
        let Some(mut c) = self.host_capture(call, &id, &mut stats) else {
            return;
        };
        // Dependencies it refers to (and the non-enemy that made it, if
        // that is still there) are replicated first, and its state read
        // again so that it names them.
        let maker = self.take_maker(key).filter(|m| self.exists(call, &m.2));
        let mut candidates = stats.not_replicated.clone();
        candidates.extend(maker.clone());
        if self.take_deps(call, &candidates) > 0 {
            stats = Stats::default();
            let Some(again) = self.host_capture(call, &id, &mut stats) else {
                return;
            };
            c = again;
        }
        // Whether the copy runs the set-up step: with the event observer,
        // exactly when the host's instance ran user event 0 before its
        // first state; without it, the `instance_copy` rule (a copy of
        // an instance that already ran it).
        let (by_copy, ran, observed) = {
            let mut st = self.lock();
            let t = &mut st.transfer;
            t.copy_born.retain(|_, f| frame.saturating_sub(*f) < 600);
            t.setup_seen.retain(|_, f| frame.saturating_sub(*f) < 600);
            t.event1_by_maker
                .retain(|_, f| frame.saturating_sub(*f) < 600);
            (
                t.copy_born.remove(&key).is_some(),
                t.setup_seen.remove(&key).is_some(),
                t.observer,
            )
        };
        let no_setup = if observed { !ran } else { by_copy };
        let mut first = c.clone();
        if no_setup {
            first
                .builtins
                .push((state::ORIGIN_BUILTIN, StateValue::Whole(state::ORIGIN_COPY)));
            self.followers_attach(call, &id, &mut first);
        }
        // Its maker ran its user event 1 (a car's trails): the copy
        // runs it after its set-up.
        if !no_setup && self.lock().transfer.event1_by_maker.remove(&key).is_some() {
            first
                .builtins
                .push((state::MAKER_EVENT_BUILTIN, StateValue::Whole(1)));
        }
        // Its maker, when that is replicated: the copy waits for it.
        if let Some(m) = maker
            .as_ref()
            .and_then(|m| self.lock().host.net_id_for(m.0, &m.2))
        {
            first
                .builtins
                .push((state::MAKER_BUILTIN, StateValue::Instance(m)));
        }
        self.send(net, object, 0, &first);
        let sent = first;
        let count = self
            .h
            .reflect
            .as_ref()
            .and_then(|r| state::names_count(call, r, &id))
            .unwrap_or(c.vars.len());
        let (refs, ref_sig) = self.gen_ref_watch(call, &id, &[], &c);
        let mut st = self.lock();
        st.transfer.host_live.insert(
            net,
            HostGen {
                key,
                id,
                object,
                born,
                first: sent,
                last: c,
                seq: 0,
                sent_at: frame,
                moved: None,
                count,
                change_since: None,
                refs,
                ref_sig,
            },
        );
    }

    /// Host: an instance about to die (`checkDestroy*`, hull at 0) whose
    /// first state still waits in the queue has it captured and sent now, so
    /// its copy is made on the joining game to die there with its explosion.
    pub(super) fn gen_host_first_now(&self, call: &Call<'_>, key: usize) {
        if !self.gen_wanted() {
            return;
        }
        let (entry, frame) = {
            let mut st = self.lock();
            if st.frozen {
                return;
            }
            let at = st.transfer.host_new.iter().position(|e| e.1 == key);
            let frame = st.frame;
            (at.map(|i| st.transfer.host_new.remove(i)), frame)
        };
        if let Some(e) = entry {
            self.gen_host_first(call, frame, e);
        }
    }

    /// Host, at the run step: capture the instances first seen last frame and
    /// send repairs. Returns the messages.
    #[allow(clippy::too_many_lines)] // capture and repairs: one pass over the live instances
    pub(super) fn gen_host_frame(&self, call: &Call<'_>) -> Vec<Message> {
        let mut out = Vec::new();
        if !self.gen_wanted() {
            return out;
        }
        {
            // Frozen: the game has deactivated its instances, which read back
            // wrong; nothing is captured until the next running frame.
            let st = self.lock();
            if st.frozen {
                // The guard must be gone before `gen_flush` locks again.
                drop(st);
                self.gen_flush(&mut out);
                return out;
            }
        }
        let (new, frame) = {
            let mut st = self.lock();
            let frame = st.frame;
            let queue = std::mem::take(&mut st.transfer.host_new);
            let (now, left) = first_states_due(queue, frame, FIRST_STATES_PER_FRAME, |e| e.4);
            st.transfer.host_new = left;
            (now, frame)
        };
        for (net, key, id, object, born) in new {
            self.gen_host_first(call, frame, (net, key, id, object, born));
        }
        // Forget the gone; change repairs (an instance gained variables this
        // frame), then periodic repairs.
        let live: Vec<Live> = {
            let st = self.lock();
            st.transfer
                .host_live
                .iter()
                .map(|(n, g)| Live {
                    net: *n,
                    key: g.key,
                    id: g.id.clone(),
                    object: g.object,
                    born: g.born,
                    sent_at: g.sent_at,
                    count: g.count,
                    change_since: g.change_since,
                    refs: Arc::clone(&g.refs),
                    ref_sig: g.ref_sig.clone(),
                })
                .collect()
        };
        let mut ref_reads = 0u64;
        // Where this frame's reference reads start (the instances in turn).
        let ref_start = self.lock().transfer.ref_cursor % live.len().max(1);
        let mut ref_instances = 0usize;
        let n_live = live.len();
        let mut ref_done: Vec<RefDue> = Vec::new();
        let mut repairs: Vec<(NetId, Value, u16, u64, u64)> = Vec::new();
        let mut changes: Vec<(NetId, Value, u16, u64, u64)> = Vec::new();
        let period = (super::super::repl::REPAIR_EVERY.as_secs_f64() * 60.0).max(1.0) as u64;
        // The reference test takes the instances in turn from `ref_start`
        // until its budget is spent (the rest wait for a later frame).
        let mut live = live;
        live.sort_unstable_by_key(|l| l.net);
        live.rotate_left(ref_start.min(n_live));
        for (pos, l) in live.into_iter().enumerate() {
            let Live {
                net,
                key,
                id,
                object,
                born,
                sent_at,
                ..
            } = l;
            let tracked = self.lock().host.net_id_for(key, &id) == Some(net);
            if !tracked {
                self.lock().transfer.host_live.remove(&net);
                continue;
            }
            // The change test: a count of its variables, every frame; a
            // deactivated instance waits until it is active again.
            let gained = self.h.reflect.as_ref().is_some_and(|r| {
                self.exists(call, &id)
                    && state::names_count(call, r, &id).is_some_and(|n| n != l.count)
            });
            // And its references: a handful of variables per instance.
            let reref = !gained
                && !l.refs.is_empty()
                && ref_reads < REF_READS_PER_FRAME
                && self.h.reflect.as_ref().is_some_and(|r| {
                    ref_reads += l.refs.len() as u64;
                    ref_instances = pos + 1;
                    self.exists(call, &id)
                        && state::ref_signature(call, r, &id, &l.refs) != l.ref_sig
                });
            if reref {
                // Only the references go, read singly (`gen_ref_repair`); a
                // full change repair only when one names an instance not
                // replicated yet.
                ref_done.push((net, id.clone(), object, born, Arc::clone(&l.refs)));
                continue;
            }
            if gained {
                // Read the frame after the change is seen, as a first state
                // is read the frame after the first tick: what the same step
                // made and the new variables name (a segment's new legs) is
                // replicated by then, not sent as unresolved.
                match l.change_since {
                    Some(since) => changes.push((net, id, object, born, since)),
                    None => {
                        if let Some(g) = self.lock().transfer.host_live.get_mut(&net) {
                            g.change_since = Some(frame);
                        }
                    }
                }
            } else if frame.saturating_sub(sent_at) >= period {
                repairs.push((net, id, object, born, sent_at));
            }
        }
        // Next frame goes on after the last instance read (0 read: all of
        // them had their turn, start over).
        {
            let mut st = self.lock();
            let t = &mut st.transfer;
            t.ref_cursor = if ref_instances == 0 {
                0
            } else {
                ref_start + ref_instances
            };
        }
        for (net, id, object, born, refs) in ref_done {
            if !self.gen_ref_repair(call, frame, net, &id, object, born, &refs) {
                changes.push((net, id, object, born, frame));
            }
        }
        // Changes first, the longest waiting first; each instance's repairs
        // go out in the order they were read (one queue), so a burst is
        // paced without reordering any instance's own.
        changes.sort_by_key(|r| (r.4, r.0));
        repairs.sort_by_key(|r| r.4);
        let due = changes
            .into_iter()
            .take(CHANGES_PER_FRAME)
            .map(|c| (true, c))
            .chain(
                repairs
                    .into_iter()
                    .take(REPAIRS_PER_FRAME)
                    .map(|r| (false, r)),
            );
        for (change, (net, id, object, born, _)) in due {
            self.gen_repair(call, frame, change, net, &id, object, born);
        }
        self.gen_flush(&mut out);
        out
    }

    /// Host: one repair of `net`, read now. A change repair (`change`: the
    /// instance gained variables) sends the new ones and what the narrow rule
    /// would send with them, and leaves the window's history as it is; a
    /// periodic one judges the window and starts the next. Either is stamped
    /// with the instance's age ([`state::AGE_BUILTIN`]).
    #[allow(clippy::too_many_arguments)] // one repair's facts; a struct would only rename them
    fn gen_repair(
        &self,
        call: &Call<'_>,
        frame: u64,
        change: bool,
        net: NetId,
        id: &Value,
        object: u16,
        born: u64,
    ) {
        // Not active (deactivated on its own): read back wrong; later.
        if !self.exists(call, id) {
            return;
        }
        let mut stats = Stats::default();
        let Some(mut c) = self.host_capture(call, id, &mut stats) else {
            return;
        };
        // A dependency it refers to from now on is replicated first and the
        // state read again, so that this repair names it.
        if self.take_deps(call, &stats.not_replicated) > 0 {
            stats = Stats::default();
            let Some(again) = self.host_capture(call, id, &mut stats) else {
                return;
            };
            c = again;
        }
        let count = self
            .h
            .reflect
            .as_ref()
            .and_then(|r| state::names_count(call, r, id))
            .unwrap_or(c.vars.len());
        let had = self
            .lock()
            .transfer
            .host_live
            .get(&net)
            .map_or_default(|g| Arc::clone(&g.refs));
        let (refs, ref_sig) = self.gen_ref_watch(call, id, &had, &c);
        let (mut diff, seq) = {
            let mut st = self.lock();
            let Some(g) = st.transfer.host_live.get_mut(&net) else {
                return;
            };
            let mut d = state::changed(&g.last, &c);
            if change {
                change_repair(&mut d, g.moved.as_ref(), &g.last, &refs);
                merge_vars(&mut g.last, &d);
            } else {
                narrow_repair(&mut d, &mut g.moved, &g.last);
                g.last = c.clone();
                g.sent_at = frame;
            }
            g.count = count;
            g.change_since = None;
            g.refs = refs;
            g.ref_sig = ref_sig;
            if d.vars.is_empty() && d.builtins.is_empty() {
                return;
            }
            g.seq = g.seq.wrapping_add(1).max(1);
            (d, g.seq)
        };
        let age = frame.saturating_sub(born);
        diff.builtins.push((
            state::AGE_BUILTIN,
            StateValue::Whole(i32::try_from(age).unwrap_or(i32::MAX)),
        ));
        self.send(net, object, seq, &diff);
    }

    /// Host: a reference of `net` changed: send the watched references that
    /// differ from the last state sent, read singly (no full capture). False
    /// when one names an instance that is not replicated (a new dependency:
    /// the full change repair takes it first).
    #[allow(clippy::too_many_arguments)] // one repair's facts, as gen_repair
    fn gen_ref_repair(
        &self,
        call: &Call<'_>,
        frame: u64,
        net: NetId,
        id: &Value,
        object: u16,
        born: u64,
        refs: &[String],
    ) -> bool {
        let Some(r) = self.h.reflect.as_ref() else {
            return true;
        };
        let net_of = |k: usize, v: &Value| self.lock().host.net_id_for(k, v);
        let mut stats = Stats::default();
        let mut now = Vec::with_capacity(refs.len());
        for name in refs {
            if let Some(v) = state::read_var(call, r, id, name, &net_of, &mut stats) {
                now.push((name.clone(), v));
            }
        }
        if !stats.not_replicated.is_empty() {
            return false;
        }
        // A watched variable that holds anything but an instance or `noone`
        // is not a reference (`noone` at the capture was a number's start
        // value): it leaves the watch, and its changes go with the repairs
        // as any number's.
        let numbers: Vec<String> = now
            .iter()
            .filter(|(_, v)| !state::is_ref_value(v))
            .map(|(k, _)| k.clone())
            .collect();
        let refs: Vec<String> = refs
            .iter()
            .filter(|n| !numbers.contains(n))
            .cloned()
            .collect();
        now.retain(|(k, _)| !numbers.contains(k));
        if !numbers.is_empty() {
            self.ref_numbers(call, net, object, &numbers, &refs);
        }
        let sig = state::ref_signature(call, r, id, &refs);
        let (diff, seq) = {
            let mut st = self.lock();
            let Some(g) = st.transfer.host_live.get_mut(&net) else {
                return true;
            };
            g.ref_sig = sig;
            let changed: Vec<(String, StateValue)> = now
                .into_iter()
                .filter(|(k, v)| !g.last.vars.iter().any(|(n, old)| n == k && old == v))
                .collect();
            if changed.is_empty() {
                return true;
            }
            let Some(g) = st.transfer.host_live.get_mut(&net) else {
                return true;
            };
            let d = Captured {
                vars: changed,
                ..Captured::default()
            };
            merge_vars(&mut g.last, &d);
            g.seq = g.seq.wrapping_add(1).max(1);
            (d, g.seq)
        };
        let mut diff = diff;
        let age = frame.saturating_sub(born);
        diff.builtins.push((
            state::AGE_BUILTIN,
            StateValue::Whole(i32::try_from(age).unwrap_or(i32::MAX)),
        ));
        self.send(net, object, seq, &diff);
        true
    }

    /// Host, after the create of a replicated member by a tracked instance:
    /// noted for [`Self::gen_maker_repairs`].
    pub(super) fn gen_member_made(&self, call: &Call<'_>) {
        let Some(id) = call.result().filter(super::classify::is_instance) else {
            return;
        };
        let Some(maker) = call.instance().map(|i| i.key()) else {
            return;
        };
        let mut member = None;
        let _ = call.with(&id, |inst, _| member = Some(inst.key()));
        if let Some(member) = member {
            self.lock().transfer.members_born.push((maker, member));
        }
    }

    /// Host, at the run step, before this frame's spawns go out: each maker
    /// that made members since the last run step sends the variables it
    /// gained since its last state (read singly), ahead of its members'
    /// spawns and outside the change budget, once per frame (a Seraph sets
    /// `orbitSpeed` and `dronePhase` on itself in the frame it makes its
    /// drones). Returns the messages, the maker's queued repairs first.
    pub(super) fn gen_maker_repairs(&self, call: &Call<'_>) -> Vec<Message> {
        let born = std::mem::take(&mut self.lock().transfer.members_born);
        if born.is_empty() || self.lock().frozen {
            return Vec::new();
        }
        let Some(r) = self.h.reflect.as_ref() else {
            return Vec::new();
        };
        let frame = self.lock().frame;
        let mut ahead: HashSet<NetId> = HashSet::new();
        for (maker, _) in makers_once(&born) {
            // By address alone until the next tick: the repair goes out only
            // while the tracked instance, checked by its id below, exists.
            let Some(net) = self.lock().host.net_id(maker) else {
                continue;
            };
            let Some((id, object, born_at)) = self
                .lock()
                .transfer
                .host_live
                .get(&net)
                .map(|g| (g.id.clone(), g.object, g.born))
            else {
                // Its first state is not out yet: it carries everything.
                continue;
            };
            if !self.exists(call, &id) {
                continue;
            }
            let Some(names) = state::var_names(call, r, &id) else {
                continue;
            };
            let gained = self
                .lock()
                .transfer
                .host_live
                .get(&net)
                .map_or_default(|g| gained_names(&g.last, &names));
            if gained.is_empty() {
                continue;
            }
            let net_of = |k: usize, v: &Value| self.lock().host.net_id_for(k, v);
            let mut stats = Stats::default();
            let mut now = Vec::with_capacity(gained.len());
            for name in &gained {
                if let Some(v) = state::read_var(call, r, &id, name, &net_of, &mut stats) {
                    now.push((name.clone(), v));
                }
            }
            let (mut diff, seq) = {
                let mut st = self.lock();
                let Some(g) = st.transfer.host_live.get_mut(&net) else {
                    continue;
                };
                let d = Captured {
                    vars: now,
                    ..Captured::default()
                };
                merge_vars(&mut g.last, &d);
                // Seen: the change test does not send them again.
                g.count = names.len();
                g.change_since = None;
                g.seq = g.seq.wrapping_add(1).max(1);
                let seq = g.seq;
                (d, seq)
            };
            diff.builtins.push((
                state::AGE_BUILTIN,
                StateValue::Whole(i32::try_from(frame.saturating_sub(born_at)).unwrap_or(i32::MAX)),
            ));
            self.send(net, object, seq, &diff);
            ahead.insert(net);
        }
        if ahead.is_empty() {
            return Vec::new();
        }
        take_ahead(&mut self.lock().transfer.outq, &ahead)
    }

    /// Host: `numbers` (watched as references, holding something else now)
    /// leave `net`'s watch; each (object, variable) logged once.
    fn ref_numbers(
        &self,
        call: &Call<'_>,
        net: NetId,
        object: u16,
        numbers: &[String],
        keep: &[String],
    ) {
        let fresh: Vec<String> = {
            let mut st = self.lock();
            if let Some(g) = st.transfer.host_live.get_mut(&net) {
                g.refs = Arc::new(keep.to_vec());
            }
            numbers
                .iter()
                .filter(|n| st.transfer.ref_numbers_seen.insert((object, (*n).clone())))
                .cloned()
                .collect()
        };
        if !fresh.is_empty() {
            let name = self.object_name(call, u32::from(object));
            log::info!(
                "co-op world: not references (held noone at a capture, a number or other value now; no longer read each frame): {}",
                fresh
                    .iter()
                    .map(|n| format!("{name}.{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    /// Host: the references to watch after capture `c` (those `had`, and
    /// those `c` holds) and what they hold now.
    fn gen_ref_watch(
        &self,
        call: &Call<'_>,
        id: &Value,
        had: &[String],
        c: &Captured,
    ) -> (Arc<Vec<String>>, Vec<u64>) {
        let Some(r) = self.h.reflect.as_ref() else {
            return (Arc::default(), Vec::new());
        };
        // What it held and what holds an instance or `noone` now, less any
        // that this capture shows holding something else (not references).
        let mut names: Vec<String> = had
            .iter()
            .filter(|n| {
                c.vars
                    .iter()
                    .find(|(k, _)| k == *n)
                    .is_none_or(|(_, v)| state::is_ref_value(v))
            })
            .cloned()
            .collect();
        for n in state::ref_names(c) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        let sig = state::ref_signature(call, r, id, &names);
        (Arc::new(names), sig)
    }

    /// Host: queued state messages out, oldest first, at most the pace per
    /// frame ([`super::super::repl::PACE_BYTES`]).
    fn gen_flush(&self, out: &mut Vec<Message>) {
        let pace = super::super::repl::PACE_BYTES;
        let mut st = self.lock();
        let mut sent = 0usize;
        while let Some((_, n)) = st.transfer.outq.front() {
            if pace > 0 && sent > 0 && sent + n > pace {
                break;
            }
            let Some((m, n)) = st.transfer.outq.pop_front() else {
                break;
            };
            sent += n;
            out.push(m);
        }
    }

    /// Client: take the state parts (`InstanceStateIx`, expanded into an
    /// [`InstanceState`]) and name tables out of the world's inbox.
    pub(super) fn gen_client_inbox(&self, inbox: &mut Vec<Message>) {
        let mut st = self.lock();
        inbox.retain(|m| {
            let expanded;
            let p = match m {
                Message::StateNames(n) => {
                    let list = st.transfer.client_names.entry(n.object).or_default();
                    let base = usize::from(n.base);
                    if list.len() < base {
                        list.resize(base, String::new());
                    }
                    for (i, name) in n.names.iter().enumerate() {
                        if base + i < list.len() {
                            list[base + i].clone_from(name);
                        } else {
                            list.push(name.clone());
                        }
                    }
                    return false;
                }
                Message::InstanceStateIx(p) => {
                    let names = st.transfer.client_names.get(&p.object);
                    let vars = p
                        .vars
                        .iter()
                        .filter_map(|(ix, v)| {
                            let n = names
                                .and_then(|l| l.get(usize::from(*ix)))
                                .filter(|n| !n.is_empty());
                            Some((n?.clone(), v.clone().expanded()))
                        })
                        .collect();
                    expanded = InstanceState {
                        net_id: p.net_id,
                        seq: p.seq,
                        part: p.part,
                        parts: p.parts,
                        vars,
                        builtins: p
                            .builtins
                            .iter()
                            .map(|(k, v)| (*k, v.clone().expanded()))
                            .collect(),
                        alarms: p.alarms.clone(),
                    };
                    &expanded
                }
                _ => return true,
            };
            let k = (p.net_id, p.seq);
            let v = st.transfer.parts.entry(k).or_default();
            if !v.iter().any(|q| q.part == p.part) {
                v.push(p.clone());
            }
            if v.len() >= usize::from(p.parts.max(1)) {
                let mut parts = st.transfer.parts.remove(&k).unwrap_or_default();
                parts.sort_by_key(|q| q.part);
                let mut c = Captured::default();
                for q in parts {
                    c.vars.extend(q.vars);
                    c.builtins.extend(q.builtins);
                    c.alarms.extend(q.alarms);
                }
                st.transfer.ready.push((k.0, k.1, c));
            }
            false
        });
    }

    /// Client: a copy of `net` was made as `id`.
    pub(super) fn gen_client_made(&self, net: NetId, id: Value, object: u16) {
        if !self.gen_wanted() {
            return;
        }
        {
            let mut st = self.lock();
            let frame = st.frame;
            let again = st.transfer.kept.contains_key(&net);
            st.transfer.made.insert(
                net,
                Made {
                    id,
                    object,
                    born: frame,
                    again,
                    first_done: false,
                },
            );
            // Made again (a refused kill): its first state once more.
            if !st.transfer.ready.iter().any(|r| r.0 == net && r.1 == 0)
                && let Some(c) = st.transfer.kept.get(&net).cloned()
            {
                st.transfer.ready.push((net, 0, c));
            }
        }
        // Applied in `gen_client_frame`, once every copy due this frame
        // exists (their references to each other resolve then).
    }

    /// Client: a repair as it is applied, and the kept first state brought
    /// up to date with it.
    fn gen_repair_prepare(&self, net: NetId, c: &mut Captured) {
        // A repair never rebuilds a method: its method values are dropped
        // (see `narrow_repair`).
        c.vars
            .retain(|v| !matches!(v.1, ndml_net::msg::StateValue::Method { .. }));
        // Nor the hull: the world writes it every tick from the snapshot.
        c.vars
            .retain(|v| !state::WORLD_OWNED.contains(&v.0.as_str()));
        // Nor what the copy holds for its own player (its corrosion).
        c.vars
            .retain(|v| !state::COPY_OWNED.contains(&v.0.as_str()));
        // The kept state follows every repair (a narrow repair only carries
        // what changed).
        if let Some(k) = self.lock().transfer.kept.get_mut(&net) {
            merge_vars(k, c);
        }
    }

    /// Client: the states ready to apply now (first states and repairs of
    /// copies made here), in order; the rest wait. A repair waits until its
    /// copy is as old as the host's instance was when read
    /// ([`state::AGE_BUILTIN`]), later ones of the same copy behind it; a
    /// copy made again (a refused kill) takes them at once.
    fn gen_take_ready(&self) -> Vec<(NetId, u16, Captured, Value)> {
        let mut st = self.lock();
        let st = &mut *st;
        let frame = st.frame;
        let mut ready = std::mem::take(&mut st.transfer.ready);
        // Each copy's states in their order; arrival order otherwise.
        ready.sort_by_key(|r| (r.0, r.1));
        let mut keep = Vec::new();
        let mut todo = Vec::new();
        let mut held: HashSet<NetId> = HashSet::new();
        for (net, seq, c) in ready {
            match st.transfer.made.get(&net) {
                Some(m) => {
                    let copy_age = frame.saturating_sub(m.born);
                    let stamp = (seq > 0 && !m.again)
                        .then(|| state::stamped_age(&c))
                        .flatten();
                    if held.contains(&net) || stamp.is_some_and(|a| copy_age < a) {
                        held.insert(net);
                        keep.push((net, seq, c));
                        continue;
                    }
                    todo.push((net, seq, c, m.id.clone()));
                }
                None if st.client.is_known(net) => keep.push((net, seq, c)),
                None => {}
            }
        }
        st.transfer.ready = keep;
        sort_makers_first(&mut todo);
        todo
    }

    #[allow(clippy::too_many_lines)] // one pass in order
    fn gen_apply_ready(&self, call: &Call<'_>, open: &[NetId]) {
        let Some(r) = self.h.reflect else { return };
        let todo = self.gen_take_ready();
        // A copy whose state goes back keeps its later ones behind it.
        let mut blocked: HashSet<NetId> = HashSet::new();
        for (net, seq, mut c, id) in todo {
            if blocked.contains(&net) {
                self.lock().transfer.ready.push((net, seq, c));
                continue;
            }
            if !self.exists(call, &id) {
                // Deactivated (a freeze, or asleep outside the active region),
                // not gone: kept for when it is active again.
                if self.lock().client.is_known(net) {
                    self.lock().transfer.ready.push((net, seq, c));
                    blocked.insert(net);
                } else if seq > 0 && open.contains(&net) {
                    // Died here on a kill claim not settled yet: folded into
                    // its kept first state, which a copy made again gets.
                    self.gen_repair_prepare(net, &mut c);
                }
                continue;
            }
            if seq == 0 && self.gen_hold_for_maker(net, &c) {
                self.lock().transfer.ready.push((net, seq, c));
                blocked.insert(net);
                continue;
            }
            // A copy that had its first state (made from the kept one at a
            // resync) skips the resync's copy of it: the same state, and its
            // set-up must not run twice; the repair after it brings the current state.
            let mut made_pose = None;
            if seq == 0 {
                let mut st = self.lock();
                let made = st.transfer.made.get_mut(&net);
                let again = made.as_ref().is_some_and(|m| m.again);
                let done = made.map(|m| std::mem::replace(&mut m.first_done, true));
                if done == Some(true) {
                    continue;
                }
                st.transfer.kept.insert(net, c.clone());
                drop(st);
                // Made again (a refused kill claim, a resync): placed where
                // its timeline has it now, not at the kept state's old place.
                if again {
                    made_pose = copy_pose(call, &id);
                }
            }
            if seq > 0 {
                self.gen_repair_prepare(net, &mut c);
            }
            let mut stats = Stats::default();
            let id_of = |n: NetId| self.lock().client.id_of(n);
            let setup = seq == 0 && !state::born_by_copy(&c);
            if setup {
                // The game's own set-up step (user event 0, as
                // `oEnemySpawner_Alarm_1` runs it), with the host's state
                // written before and after; the enemies it makes are let
                // through and removed after it.
                state::apply(
                    call,
                    &r,
                    &id,
                    &c.vars,
                    &c.builtins,
                    None,
                    &id_of,
                    &mut Stats::default(),
                );
                if let Some(eu) = self.h.event_user {
                    let event1 = state::maker_ran_event1(&c);
                    let _ = call.with(&id, |inst, _| {
                        self.lock().building = Some(inst.key());
                        let _ = inst.call_builtin(eu, &[0.0.into()]);
                        // As its maker did on the host (a car's trails).
                        if event1 {
                            let _ = inst.call_builtin(eu, &[1.0.into()]);
                        }
                        self.lock().building = None;
                    });
                    self.gen_scratch_clear(call, 0);
                }
            }
            state::apply(
                call,
                &r,
                &id,
                &c.vars,
                &c.builtins,
                (seq == 0 || !c.alarms.is_empty()).then_some(c.alarms.as_slice()),
                &id_of,
                &mut stats,
            );
            if let Some(p) = made_pose {
                set_copy_pose(call, &id, p);
            }
            self.followers_make(call, &id, seq, &c);
            if !stats.readback_bad.is_empty() {
                let obj = self.lock().transfer.made.get(&net).map_or(0, |m| m.object);
                crate::game::info_once(
                    &format!("generic-readback-{obj}"),
                    &format!(
                        "co-op world (generic): object {obj}: written values that read back as another kind: {:?}",
                        stats.readback_bad
                    ),
                );
            }
            if seq == 0
                && let Some(v) = self.h.hull
            {
                // The first state set the copy's hull: the shared-damage
                // measurement starts from it.
                let mut hull = None;
                let _ = call.with(&id, |inst, _| {
                    hull = inst.get_f64(v).map(|h| (inst.key(), h));
                });
                if let Some((key, h)) = hull.filter(|(_, h)| h.is_finite()) {
                    self.dmg.spawned(net, key, h as f32);
                }
            }
            self.gen_applied_refs(net, &c);
            let mut st = self.lock();
            let since = st.transfer.run_frames;
            for (var, target) in stats.waiting {
                st.transfer.waiting.push((net, var, target, since));
            }
        }
    }

    /// Client: the references state `c` wrote on the copy of `net`: kept for
    /// `follow_applied`, and a variable it set no longer waits for an older
    /// target (arriving later, that would undo the newer value).
    fn gen_applied_refs(&self, net: NetId, c: &Captured) {
        let object = self.lock().transfer.made.get(&net).map_or(0, |m| m.object);
        self.follow_applied(net, object, &c.vars);
        self.lock()
            .transfer
            .waiting
            .retain(|w| w.0 != net || !c.vars.iter().any(|(k, _)| *k == w.1));
    }

    /// Client: remove enemies that copies made, `min_age` frames old or more
    /// (0: all).
    pub(super) fn gen_scratch_clear(&self, call: &Call<'_>, min_age: u64) {
        let gone: Vec<Value> = {
            let mut st = self.lock();
            let frame = st.frame;
            let (old, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut st.transfer.scratch)
                .into_iter()
                .partition(|s| frame.saturating_sub(s.2) >= min_age);
            st.transfer.scratch = keep;
            old.into_iter().map(|s| s.1).collect()
        };
        for id in gone {
            self.destroy_silently(call, &id);
        }
    }

    /// Client, at an enemy's tick: an enemy a copy made goes now (before its
    /// first Step). True when it was one.
    pub(super) fn gen_scratch_tick(&self, call: &Call<'_>, key: usize) -> bool {
        let made = {
            let mut st = self.lock();
            let Some(i) = st.transfer.scratch.iter().position(|s| s.0 == key) else {
                return false;
            };
            st.transfer.scratch.swap_remove(i).1
        };
        self.destroy_silently(call, &made);
        true
    }

    /// Client: copies no longer here are forgotten, except one that died
    /// here on a kill claim not settled yet ([`GenState::forget_gone`]);
    /// returns those claims.
    fn gen_forget_gone(&self) -> Vec<NetId> {
        let open = self.dmg.open_claims();
        let mut st = self.lock();
        let st = &mut *st;
        st.transfer.forget_gone(&st.client, &open);
        open
    }

    /// Client, at the run step: states that completed and references whose
    /// target now exists.
    pub(super) fn gen_client_frame(&self, call: &Call<'_>) {
        if !self.gen_wanted() {
            return;
        }
        let frozen = {
            let mut st = self.lock();
            if !st.frozen {
                st.transfer.run_frames += 1;
            }
            st.frozen
        };
        // Enemies that copies made and never ticked (destroyed by their maker,
        // or not ticking yet) go after two frames; not in a freeze (a
        // deactivated one is not destroyed, and was forgotten).
        if !frozen {
            self.gen_scratch_clear(call, 2);
        }
        let open = self.gen_forget_gone();
        self.gen_apply_ready(call, &open);
        // References whose target is here now; not in a freeze (the copies
        // are deactivated: a write to one is lost), the first running frame
        // after it sets them.
        if let Some(r) = self.h.reflect
            && !frozen
        {
            let set = {
                let mut st = self.lock();
                let run = st.transfer.run_frames;
                let waiting = std::mem::take(&mut st.transfer.waiting);
                let mut set = Vec::new();
                let mut keep = Vec::new();
                for w in waiting {
                    match (st.client.id_of(w.0), st.client.id_of(w.2)) {
                        (Some(a), Some(b)) => set.push((a, b, w)),
                        _ if run.saturating_sub(w.3) > WAIT_FRAMES => {}
                        _ => keep.push(w),
                    }
                }
                st.transfer.waiting = keep;
                drop(st);
                set
            };
            for (a, b, w) in set {
                // The referrer asleep (deactivated outside the active
                // region): later, when it is active.
                if !self.exists(call, &a) {
                    self.lock().transfer.waiting.push(w);
                    continue;
                }
                let _ = state::set_var(call, &r, &a, &w.1, &b);
            }
            self.follow_frame(call);
        }
    }
}

/// Narrow a periodic repair to what the receiving copy cannot work out itself:
/// variables that changed after being still for the last window (`moved` keeps
/// this window's), plus any variable the copy's last state (`last`) lacked;
/// never built-ins, alarms or methods. The test looks at a variable's history
/// only, so variables one game step sets together go together.
fn narrow_repair(d: &mut Captured, moved: &mut Option<HashSet<String>>, last: &Captured) {
    let had: HashSet<&str> = last.vars.iter().map(|(k, _)| k.as_str()).collect();
    let now: HashSet<String> = d.vars.iter().map(|(k, _)| k.clone()).collect();
    let before = moved.replace(now);
    // Methods are code the instance assigns as it runs; rebuilding one with
    // `method()` can throw inside the runner, so a repair never carries them.
    d.vars.retain(|(k, v)| {
        !matches!(v, StateValue::Method { .. })
            && (!had.contains(k.as_str()) || before.as_ref().is_none_or(|b| !b.contains(k)))
    });
    d.builtins.clear();
    d.alarms.clear();
}

/// Narrow a change repair: the new names always, a watched reference that
/// changed (`refs`), and the other changed variables the narrow rule would
/// send now; never a method, built-ins or alarms. `moved` is only read.
fn change_repair(
    d: &mut Captured,
    moved: Option<&HashSet<String>>,
    last: &Captured,
    refs: &[String],
) {
    let had: HashSet<&str> = last.vars.iter().map(|(k, _)| k.as_str()).collect();
    d.vars.retain(|(k, v)| {
        !matches!(v, StateValue::Method { .. })
            && (!had.contains(k.as_str())
                || refs.contains(k)
                || moved.is_none_or(|m| !m.contains(k)))
    });
    d.builtins.clear();
    d.alarms.clear();
}

/// A copy's place and facing as it is now (x, y, image angle).
fn copy_pose(call: &Call<'_>, id: &Value) -> Option<(f64, f64, f64)> {
    let mut out = None;
    let _ = call.with(id, |inst, _| {
        let get = |n: &str| inst.builtin_value(n).ok().and_then(|v| v.as_f64());
        out = get("x")
            .zip(get("y"))
            .map(|(x, y)| (x, y, get("image_angle").unwrap_or(0.0)));
    });
    out
}

/// Put a copy back at `p` ([`copy_pose`]), its previous place with it (no
/// motion read from the jump).
fn set_copy_pose(call: &Call<'_>, id: &Value, (x, y, angle): (f64, f64, f64)) {
    let _ = call.with(id, |inst, _| {
        for (n, v) in [
            ("x", x),
            ("y", y),
            ("xprevious", x),
            ("yprevious", y),
            ("image_angle", angle),
        ] {
            let _ = inst.set_builtin_f64(n, v);
        }
    });
}

/// Host: each maker once, with its members, in the order first made.
fn makers_once(born: &[(usize, usize)]) -> Vec<(usize, Vec<usize>)> {
    let mut out: Vec<(usize, Vec<usize>)> = Vec::new();
    for &(maker, member) in born {
        match out.iter_mut().find(|(m, _)| *m == maker) {
            Some((_, members)) => members.push(member),
            None => out.push((maker, vec![member])),
        }
    }
    out
}

/// Host: the names an instance has now that its last state sent lacked.
fn gained_names(last: &Captured, now: &[String]) -> Vec<String> {
    let had: HashSet<&str> = last.vars.iter().map(|(k, _)| k.as_str()).collect();
    now.iter()
        .filter(|n| !had.contains(n.as_str()))
        .cloned()
        .collect()
}

/// Host: out of the paced queue, in queue order, every message that must go
/// with a forced maker repair: those makers' states (their earlier repairs
/// first, so no copy sees its states out of order) and every name table (a
/// state may use names an earlier queued table brings). The rest stay.
fn take_ahead(q: &mut VecDeque<(Message, usize)>, nets: &HashSet<NetId>) -> Vec<Message> {
    let mut out = Vec::new();
    let mut keep = VecDeque::with_capacity(q.len());
    for (m, n) in q.drain(..) {
        let go = match &m {
            Message::StateNames(_) => true,
            Message::InstanceStateIx(s) => nets.contains(&s.net_id),
            _ => false,
        };
        if go {
            out.push(m);
        } else {
            keep.push_back((m, n));
        }
    }
    *q = keep;
    out
}

/// Host: a frame's maker repairs (with what they took ahead) before the
/// host's own messages, the members' spawns among them.
pub(super) fn order_ahead(ahead: Vec<Message>, msgs: Vec<Message>) -> Vec<Message> {
    let mut out = ahead;
    out.extend(msgs);
    out
}

/// Client: fold a repair's variables into a kept state (the latest value of
/// each; names the repair brings are added).
fn merge_vars(kept: &mut Captured, repair: &Captured) {
    for (k, v) in &repair.vars {
        match kept.vars.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = v.clone(),
            None => kept.vars.push((k.clone(), v.clone())),
        }
    }
}

/// First states (`seq` 0) before repairs, and a maker's first state before
/// what it made.
fn sort_makers_first(todo: &mut [(NetId, u16, Captured, Value)]) {
    // First states before repairs (an older kept state applied last would
    // undo a repair); a maker's first state before what it made (the copy's
    // set-up step reads its maker).
    let makers: HashMap<NetId, NetId> = todo
        .iter()
        .filter(|t| t.1 == 0)
        .filter_map(|t| {
            t.2.builtins.iter().find_map(|(i, v)| match v {
                StateValue::Instance(m) if *i == state::MAKER_BUILTIN && *m != t.0 => {
                    Some((t.0, *m))
                }
                _ => None,
            })
        })
        .collect();
    let firsts: Vec<NetId> = todo.iter().filter(|t| t.1 == 0).map(|t| t.0).collect();
    let depth = |mut n: NetId| {
        let mut d = 0u8;
        while d < 8 {
            match makers.get(&n) {
                Some(m) if firsts.contains(m) => {
                    d += 1;
                    n = *m;
                }
                _ => break,
            }
        }
        d
    };
    todo.sort_by_key(|t| (t.1 != 0, depth(t.0)));
}

/// Frames a copy's first state waits for its maker's (then it goes anyway).
const MAKER_WAIT_FRAMES: u64 = 120;

/// Host: the first states to capture this frame out of `queue` (in birth
/// order): those born before `frame`, at most `budget`, oldest first; and
/// what stays queued, in order (the ones over the budget ahead of those born
/// this frame).
fn first_states_due<T>(
    queue: Vec<T>,
    frame: u64,
    budget: usize,
    born: impl Fn(&T) -> u64,
) -> (Vec<T>, Vec<T>) {
    let (mut now, later): (Vec<T>, Vec<T>) = queue.into_iter().partition(|e| born(e) < frame);
    let rest = now.split_off(now.len().min(budget));
    (now, rest.into_iter().chain(later).collect())
}

impl WorldHooks {
    /// Host: `vars` by index into `object`'s names list (compacted), and the
    /// `StateNames` for names new to the list, which must go first. The
    /// shared enemy shots' births use the same lists as the states.
    pub(super) fn gen_index_vars(
        &self,
        object: u16,
        vars: &[(String, StateValue)],
    ) -> (Vec<Message>, Vec<(u16, StateValue)>) {
        let mut st = self.lock();
        let table = st.transfer.host_names.entry(object).or_default();
        let base = u16::try_from(table.len()).unwrap_or(u16::MAX);
        let mut new = Vec::new();
        let mut out = Vec::with_capacity(vars.len());
        for (k, v) in vars {
            let ix = if let Some(ix) = table.get(k) {
                *ix
            } else {
                let ix = u16::try_from(table.len()).unwrap_or(u16::MAX);
                table.insert(k.clone(), ix);
                new.push(k.clone());
                ix
            };
            out.push((ix, v.clone().compact()));
        }
        drop(st);
        let names = if new.is_empty() {
            Vec::new()
        } else {
            StateNames::split(object, base, new)
                .into_iter()
                .map(Message::StateNames)
                .collect()
        };
        (names, out)
    }

    /// Client: variables by index back to names (expanded); those whose name
    /// is not known are left out.
    pub(super) fn gen_named_vars(
        &self,
        object: u16,
        vars: &[(u16, StateValue)],
    ) -> Vec<(String, StateValue)> {
        let st = self.lock();
        let names = st.transfer.client_names.get(&object);
        vars.iter()
            .filter_map(|(ix, v)| {
                let n = names
                    .and_then(|l| l.get(usize::from(*ix)))
                    .filter(|n| !n.is_empty());
                Some((n?.clone(), v.clone().expanded()))
            })
            .collect()
    }

    /// Client: whether the first state of `net` waits: it names a maker this
    /// game knows whose first state is not written yet (the copy's set-up
    /// step reads its maker). At most [`MAKER_WAIT_FRAMES`].
    fn gen_hold_for_maker(&self, net: NetId, c: &Captured) -> bool {
        let Some(m) = c.builtins.iter().find_map(|(i, v)| match v {
            StateValue::Instance(m) if *i == state::MAKER_BUILTIN && *m != net => Some(*m),
            _ => None,
        }) else {
            return false;
        };
        let mut st = self.lock();
        if !st.client.is_known(m) || st.transfer.kept.contains_key(&m) {
            st.transfer.held_for_maker.remove(&net);
            return false;
        }
        let run = st.transfer.run_frames;
        let since = *st.transfer.held_for_maker.entry(net).or_insert(run);
        if run.saturating_sub(since) >= MAKER_WAIT_FRAMES {
            st.transfer.held_for_maker.remove(&net);
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_states_oldest_first_within_the_budget() {
        // (key, born): a maker (1) born before its members (2, 3), a later
        // spawn (4), one born this frame (9, frame 10).
        let q = vec![(1, 7), (2, 7), (3, 7), (4, 8), (9, 10)];
        let (now, left) = first_states_due(q, 10, 2, |e| e.1);
        assert_eq!(
            now,
            vec![(1, 7), (2, 7)],
            "the maker first, then its members"
        );
        assert_eq!(
            left,
            vec![(3, 7), (4, 8), (9, 10)],
            "the rest in birth order"
        );
        let (now, left) = first_states_due(left, 11, 16, |e| e.1);
        assert_eq!(now.len(), 3);
        assert!(left.is_empty());
    }

    #[test]
    fn a_maker_once_a_frame_with_what_it_gained() {
        // A Seraph (key 4) makes three drones, a Juggernaut (9) one shot.
        let born = [(4, 51), (9, 70), (4, 52), (4, 53)];
        assert_eq!(
            makers_once(&born),
            vec![(4, vec![51, 52, 53]), (9, vec![70])]
        );
        let last = cap(&[
            ("hull", StateValue::Real(5.0)),
            ("phase", StateValue::Real(1.0)),
        ]);
        let now: Vec<String> = ["hull", "phase", "orbitSpeed", "dronePhase"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(gained_names(&last, &now), vec!["orbitSpeed", "dronePhase"]);
        assert!(gained_names(&last, &now[..2]).is_empty());
    }

    #[test]
    fn a_makers_repair_goes_before_its_members_spawn() {
        let state = |net: NetId, seq: u16| {
            InstanceStateIx::split(net, 689, seq, vec![], vec![], vec![])
                .into_iter()
                .map(|s| (Message::InstanceStateIx(s), 10))
                .collect::<Vec<_>>()
        };
        let names = StateNames::split(689, 0, vec!["orbitSpeed".into()])
            .into_iter()
            .map(|s| (Message::StateNames(s), 10));
        // Queued (paced): another enemy's state, the maker's earlier repair,
        // a name table, then the maker's forced repair.
        let mut q: VecDeque<(Message, usize)> = VecDeque::new();
        q.extend(state(7, 3));
        q.extend(state(4, 4));
        q.extend(names);
        q.extend(state(4, 5));
        let ahead = take_ahead(&mut q, &HashSet::from([4]));
        let spawn = Message::EnemySpawn(ndml_net::msg::EnemySpawn {
            net_id: 5,
            object: 691,
            x: 0.0,
            y: 0.0,
            angle: 0.0,
            hull: 1.0,
            hull_max: 1.0,
            flags: 0,
        });
        let out = order_ahead(ahead, vec![spawn]);
        let kinds: Vec<String> = out
            .iter()
            .map(|m| match m {
                Message::InstanceStateIx(s) => format!("state {} {}", s.net_id, s.seq),
                Message::StateNames(_) => "names".to_owned(),
                Message::EnemySpawn(s) => format!("spawn {}", s.net_id),
                _ => "other".to_owned(),
            })
            .collect();
        assert_eq!(kinds, ["state 4 4", "names", "state 4 5", "spawn 5"]);
        // The other enemy's state stays paced, in its place.
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn first_refs_finds_every_enemy_a_state_names() {
        // A Glaucus fin: its leader, its dependency, a method bound to
        // itself, an array of segments.
        let mut g = GenState::default();
        let c = cap(&[
            ("leader", StateValue::Instance(131)),
            ("dependency", StateValue::Instance(127)),
            (
                "me",
                StateValue::Method {
                    func: 7,
                    bound: Box::new(StateValue::Instance(93)),
                },
            ),
            (
                "segs",
                StateValue::Array(vec![StateValue::Instance(131), StateValue::Instance(137)]),
            ),
            ("hull", StateValue::Real(5.0)),
        ]);
        g.ready.push((93, 0, c));
        assert_eq!(g.first_refs(93), vec![127, 131, 137]);
        assert!(g.has_state(93));
        assert!(g.first_refs(94).is_empty());
    }

    #[test]
    fn a_copy_dead_on_an_open_claim_keeps_its_made_entry_and_kept_state() {
        use crate::world::client::ClientWorld;
        use ndml_net::msg::EnemySpawn;
        let made = || Made {
            id: Value::Bool(false),
            object: 853,
            born: 0,
            again: false,
            first_done: true,
        };
        // 5 is in the table; 6 died here on a kill claim still open; 7 is
        // gone with nothing open.
        let mut c = ClientWorld::default();
        c.on_message(Message::EnemySpawn(EnemySpawn {
            net_id: 5,
            object: 853,
            x: 0.0,
            y: 0.0,
            angle: 0.0,
            hull: 1.0,
            hull_max: 1.0,
            flags: 0,
        }));
        let mut g = GenState::default();
        for n in [5, 6, 7] {
            g.made.insert(n, made());
            g.kept.insert(n, cap(&[]));
        }
        let keys = |g: &GenState| {
            let mut m: Vec<NetId> = g.made.keys().copied().collect();
            let mut k: Vec<NetId> = g.kept.keys().copied().collect();
            m.sort_unstable();
            k.sort_unstable();
            (m, k)
        };
        // The repair fold (`gen_apply_ready`) needs 6's entry: kept.
        g.forget_gone(&c, &[6]);
        assert_eq!(keys(&g), (vec![5, 6], vec![5, 6]));
        // Its claim settled (confirmed): forgotten too.
        g.forget_gone(&c, &[]);
        assert_eq!(keys(&g), (vec![5], vec![5]));
    }

    fn cap(vars: &[(&str, StateValue)]) -> Captured {
        Captured {
            vars: vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            builtins: vec![(0, StateValue::Real(1.0))],
            alarms: vec![(1, 30)],
        }
    }

    #[test]
    fn a_copy_waits_for_the_maker_its_first_state_names_up_to_the_bound() {
        use crate::world::client::ClientWorld;
        use ndml_net::msg::EnemySpawn;
        let spawn = |net_id, object| {
            Message::EnemySpawn(EnemySpawn {
                net_id,
                object,
                x: 10.0,
                y: 20.0,
                angle: 0.0,
                hull: 50.0,
                hull_max: 50.0,
                flags: 0,
            })
        };
        // A Warden (net 12) whose first state names its spawner (17),
        // whose own spawn has not come.
        let mut g = GenState::default();
        let mut first = cap(&[]);
        first
            .builtins
            .push((state::MAKER_BUILTIN, StateValue::Instance(17)));
        g.kept.insert(12, first);
        let mut c = ClientWorld::default();
        c.on_message(spawn(12, 853));
        c.player_stepped();
        c.begin_frame(None);
        assert!(!g.copy_ready(12, &c), "waits for its maker");
        // The spawner's spawn and first state come: made with it.
        c.on_message(spawn(17, 482));
        assert!(!g.copy_ready(12, &c), "the maker's state too");
        g.kept.insert(17, cap(&[]));
        assert!(g.copy_ready(12, &c));
        // Timeline births: a maker here with its state but not due yet holds
        // its member back too.
        let mut c = ClientWorld::default();
        c.set_timing(true, true);
        c.on_message(spawn(12, 853));
        c.on_message(spawn(17, 482));
        c.player_stepped();
        c.begin_frame(Some(std::time::Duration::from_secs(5)));
        assert!(!c.is_due(17), "no snapshot yet");
        assert!(!g.copy_ready(12, &c), "its maker is not due");
        // A maker that never comes: made without it at the bound.
        let mut g = GenState::default();
        let mut first = cap(&[]);
        first
            .builtins
            .push((state::MAKER_BUILTIN, StateValue::Instance(999)));
        g.kept.insert(12, first);
        let mut c = ClientWorld::default();
        c.on_message(spawn(12, 853));
        for _ in 0..COPY_MAKER_WAIT_FRAMES {
            assert!(!g.copy_ready(12, &c));
            c.player_stepped();
            c.begin_frame(None);
        }
        assert!(g.copy_ready(12, &c), "the bound");
    }

    #[test]
    fn a_resync_plans_every_first_state_as_sent_and_new_name_tables() {
        let mut g = GenState::default();
        let first = cap(&[("isBoss", StateValue::Real(1.0))]);
        g.host_live.insert(
            7,
            HostGen {
                key: 70,
                id: Value::object(1),
                object: 3,
                born: 10,
                first: first.clone(),
                last: cap(&[("phase", StateValue::Real(4.0))]),
                seq: 4,
                sent_at: 10,
                moved: Some(HashSet::new()),
                count: 0,
                change_since: None,
                refs: Arc::new(Vec::new()),
                ref_sig: Vec::new(),
            },
        );
        g.host_names
            .entry(3)
            .or_default()
            .insert("isBoss".into(), 0);
        let plan = g.resync_plan();
        assert_eq!(plan.len(), 1);
        assert_eq!((plan[0].0, plan[0].2), (7, 3));
        assert_eq!(plan[0].3, first, "its first state as it went out");
        assert!(g.host_names.is_empty(), "names are sent again from 0");
        let h = &g.host_live[&7];
        assert!(
            h.last.vars.is_empty() && h.moved.is_none(),
            "the next repair is whole"
        );
    }

    #[test]
    fn a_kept_first_state_follows_the_repairs() {
        // A copy made again from its kept state must hold what later
        // repairs brought (a narrow repair never sends an unchanged name
        // again): changed values replaced, new names added.
        let mut kept = cap(&[
            ("phase", StateValue::Real(0.0)),
            ("isBoss", StateValue::Real(0.0)),
        ]);
        merge_vars(
            &mut kept,
            &cap(&[
                ("phase", StateValue::Real(2.0)),
                ("healingTarget", StateValue::Instance(7)),
            ]),
        );
        let names: Vec<&str> = kept.vars.iter().map(|v| v.0.as_str()).collect();
        assert_eq!(names, vec!["phase", "isBoss", "healingTarget"]);
        assert_eq!(kept.vars[0].1, StateValue::Real(2.0));
    }

    #[test]
    fn narrow_repairs_skip_timers_and_keep_links_and_late_flags() {
        let mut moved = None;
        // First window: the changed number and the reference go; the method
        // never does.
        let mut d = cap(&[
            ("timer", StateValue::Real(5.0)),
            ("leader", StateValue::Instance(3)),
            (
                "argumentDefaultParse",
                StateValue::Method {
                    func: 1,
                    bound: Box::new(StateValue::Instance(3)),
                },
            ),
        ]);
        let last = cap(&[("timer", StateValue::Real(6.0))]);
        narrow_repair(&mut d, &mut moved, &last);
        let names: Vec<&str> = d.vars.iter().map(|v| v.0.as_str()).collect();
        assert_eq!(
            names,
            ["timer", "leader"],
            "first window: every changed number"
        );
        assert!(d.builtins.is_empty() && d.alarms.is_empty());
        // Second window: the timer moved again (kept back); a flag that was
        // still last window goes.
        let mut d = cap(&[
            ("timer", StateValue::Real(4.0)),
            ("canFire", StateValue::Real(1.0)),
        ]);
        let last = cap(&[
            ("timer", StateValue::Real(5.0)),
            ("canFire", StateValue::Real(0.0)),
        ]);
        narrow_repair(&mut d, &mut moved, &last);
        let names: Vec<&str> = d.vars.iter().map(|v| v.0.as_str()).collect();
        assert_eq!(names, ["canFire"]);
    }

    #[test]
    fn a_variable_new_since_the_last_state_always_goes() {
        // The drone flashed (canFlash false, flashTimer set in the same step)
        // while flashTimer was moving; it still goes, the copy never had it.
        let mut moved = Some(["flashTimer".to_owned()].into_iter().collect());
        let last = cap(&[("canFlash", StateValue::Bool(true))]);
        let mut d = cap(&[
            ("canFlash", StateValue::Bool(false)),
            ("flashTimer", StateValue::Real(20.0)),
        ]);
        narrow_repair(&mut d, &mut moved, &last);
        let names: Vec<&str> = d.vars.iter().map(|v| v.0.as_str()).collect();
        assert_eq!(names, ["canFlash", "flashTimer"]);
    }

    #[test]
    fn variables_set_in_one_step_go_or_stay_together() {
        // A Tracer swapping weapons every few seconds: `shotType` (an object
        // reference) and `shotDamage` (a number) change in the same windows.
        let ball = StateValue::Ref(0x0100_0001_0000_0326);
        let shot = StateValue::Ref(0x0100_0001_0000_0327);
        let pair = |r: &StateValue, d: f64| {
            cap(&[("shotType", r.clone()), ("shotDamage", StateValue::Real(d))])
        };
        let mut moved = None;
        let mut sent = Vec::new();
        let mut last = pair(&shot, 12.0);
        for (r, dmg) in [
            (&ball, 0.0),
            (&shot, 12.0),
            (&shot, 12.0),
            (&ball, 0.0),
            (&shot, 12.0),
        ] {
            let now = pair(r, dmg);
            let mut d = state::changed(&last, &now);
            narrow_repair(&mut d, &mut moved, &last);
            let names: Vec<String> = d.vars.iter().map(|v| v.0.clone()).collect();
            assert!(
                names.is_empty() || names.len() == 2,
                "a swap's pair split: {names:?}"
            );
            sent.push(names.len());
            last = now;
        }
        // First window: both go; changed again next window: both stay; still,
        // then changed: both go; changed again: both stay.
        assert_eq!(sent, [2, 0, 0, 2, 0]);
    }

    #[test]
    fn a_change_repair_brings_new_names_and_what_was_still() {
        // A leg after its controller's Step: three new names; `timer` moved
        // last window (kept back), `phase` was still (goes with them); the
        // history is not touched.
        let moved: Option<HashSet<String>> = Some(["timer".to_owned()].into_iter().collect());
        let last = cap(&[
            ("timer", StateValue::Real(3.0)),
            ("phase", StateValue::Real(0.0)),
        ]);
        let now = cap(&[
            ("timer", StateValue::Real(2.0)),
            ("phase", StateValue::Real(1.0)),
            ("defeatSequence", StateValue::Bool(false)),
            ("destroySoon", StateValue::Bool(false)),
            (
                "helper",
                StateValue::Method {
                    func: 1,
                    bound: Box::new(StateValue::Undefined),
                },
            ),
        ]);
        let mut d = state::changed(&last, &now);
        change_repair(&mut d, moved.as_ref(), &last, &[]);
        let names: Vec<&str> = d.vars.iter().map(|v| v.0.as_str()).collect();
        assert_eq!(names, ["phase", "defeatSequence", "destroySoon"]);
        assert!(d.builtins.is_empty() && d.alarms.is_empty());
        // No window yet: every changed variable goes.
        let mut d = state::changed(&last, &now);
        change_repair(&mut d, None, &last, &[]);
        assert_eq!(d.vars.len(), 4);
    }

    #[test]
    fn a_changed_reference_goes_however_often_it_moved() {
        // A serpent link's `leader` after a cut: it moved last window too
        // (an earlier cut), and still goes; `timer`, also moved, stays.
        let moved: Option<HashSet<String>> = Some(
            ["timer".to_owned(), "leader".to_owned()]
                .into_iter()
                .collect(),
        );
        let last = cap(&[
            ("timer", StateValue::Real(3.0)),
            ("leader", StateValue::Instance(7)),
        ]);
        let now = cap(&[
            ("timer", StateValue::Real(2.0)),
            ("leader", StateValue::Instance(9)),
        ]);
        let mut d = state::changed(&last, &now);
        change_repair(&mut d, moved.as_ref(), &last, &["leader".to_owned()]);
        let names: Vec<&str> = d.vars.iter().map(|v| v.0.as_str()).collect();
        assert_eq!(names, ["leader"]);
        assert_eq!(state::ref_names(&now), ["leader"]);
        let noone = cap(&[
            ("child", StateValue::Whole(-4)),
            ("hp", StateValue::Real(4.0)),
        ]);
        assert_eq!(state::ref_names(&noone), ["child"]);
    }

    #[test]
    fn a_repair_says_its_age_through_the_compact_form() {
        let mut c = cap(&[("phase", StateValue::Real(1.0))]);
        assert_eq!(state::stamped_age(&c), None);
        c.builtins
            .push((state::AGE_BUILTIN, StateValue::Whole(61).compact()));
        assert_eq!(state::stamped_age(&c), Some(61));
        c.builtins = c
            .builtins
            .into_iter()
            .map(|(i, v)| (i, v.expanded()))
            .collect();
        assert_eq!(state::stamped_age(&c), Some(61));
    }
}
