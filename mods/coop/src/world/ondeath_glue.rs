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

//! The game's on-enemy-death mods in co-op, driven by the on-death table
//! ([`fx::ON_DEATH`]): a mod is in force for the run's kills if either
//! player has it, and it does not stack. A copy that dies by the host's word
//! runs its Destroy event, not its death path
//! ([`super::names::HOST_DEATH_ONLY`]).
//!
//! * World effects (kind 1: Rupture's blast, Spontaneous Generation's
//!   swarm): each game sends which its player has ([`OnDeathMsg::Has`]). The
//!   host runs each one once per death or hands it to a partner
//!   ([`fx::world_effect_by`], [`OnDeathMsg::Given`]), whose copy plays it
//!   as it dies by the host's word, with that player's build.
//! * A part the death path calls only when its flag is set (Rupture) is
//!   reached through a lend: while only a partner has it, the host's flag is
//!   1 for the death path's span.
//! * Own-build effects (kind 2: Warpath, Priority Zero) run on each game for
//!   each death, with its own player's build.
//! * Status effects (kind 3): corrosion spreads from each game's own stacks
//!   (`corrosion_glue`). A joining player's Deflagration does not play on a
//!   host kill (its row).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use ndml_net::msg::{Message, NetId, OnDeathMsg, PeerId};
use ndml_sdk::{Call, Instance, ModContext, Script, Value, Variable};

use super::{ROLE_CLIENT, ROLE_HOST, WorldHooks};
use crate::damage::fx;

/// The parts' bits are sent again this often (frames) without a change.
const RESEND_EVERY: u64 = 120;
/// A given part or a part played here is forgotten after this many running
/// frames. A shared hold (a pause, an upgrade round) does not age them.
const KEEP_FRAMES: u64 = 600;
/// The most world effects (the wire's bit set).
const MAX_PARTS: usize = 8;

/// A world effect's handles (by its wire number).
struct PartHandles {
    script: Option<Script>,
    flag: Option<Variable>,
    own_arg0: Option<Variable>,
    lend: bool,
}

static PH: OnceLock<Vec<PartHandles>> = OnceLock::new();
/// `oPowers` (`u32::MAX` until resolved at a run step).
static POWERS: AtomicU32 = AtomicU32::new(u32::MAX);

/// A lend of one part's flag on the host: death paths running with it, and
/// the value to put back after the outermost.
#[derive(Debug, Default, Clone, Copy)]
struct Lend {
    depth: u32,
    saved: f64,
}

/// A part the host's word gave this game for a copy: the host's call's
/// first `argc` arguments, and the running frame ([`OnDeathGlue::tick`]).
#[derive(Debug, Clone, Copy)]
struct Given {
    argc: u8,
    args: [f32; 4],
    frame: u64,
}

/// Per-session state.
#[derive(Debug, Default)]
pub(super) struct OnDeathGlue {
    /// This game's own parts as last read, and the frame they were last sent.
    own: Option<u8>,
    sent_frame: u64,
    /// The partners' last word (their parts).
    partners: BTreeMap<PeerId, u8>,
    /// Host: each part's lend.
    lent: [Lend; MAX_PARTS],
    /// Joining: parts the host's word gave this game ((net id, part)).
    given: HashMap<(NetId, u8), Given>,
    /// Joining: parts a copy played here, at its own kill or by the host's
    /// word ((net id, part) -> frame).
    played: HashMap<(NetId, u8), u64>,
    /// Joining: this game's own call for a given part is running.
    firing: bool,
    counts: [Counts; MAX_PARTS],
    /// Run steps the world ran in (not frozen): the clock of `given` and
    /// `played`.
    run_frames: u64,
}

impl OnDeathGlue {
    /// One run step: the clock advances while the world runs, and what is
    /// older than [`KEEP_FRAMES`] running frames is forgotten. The parts
    /// given and forgotten, by part.
    fn tick(&mut self, running: bool) -> [u64; MAX_PARTS] {
        self.run_frames += u64::from(running);
        let now = self.run_frames;
        let mut expired = [0u64; MAX_PARTS];
        self.given.retain(|key, g| {
            let keep = now.saturating_sub(g.frame) < KEEP_FRAMES;
            if !keep && let Some(e) = expired.get_mut(usize::from(key.1)) {
                *e += 1;
            }
            keep
        });
        self.played
            .retain(|_, f| now.saturating_sub(*f) < KEEP_FRAMES);
        expired
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    /// Host: lends put back at a run step (their death path never came
    /// back).
    stuck_lends: u64,
}

impl OnDeathGlue {
    pub(super) fn clear(&mut self) {
        let lent = self.lent;
        *self = Self::default();
        // A lend is put back by its death path's after-hook, not forgotten.
        self.lent = lent;
    }

    /// The partners that said they have `part`.
    fn with(&self, part: u8) -> Vec<PeerId> {
        self.partners
            .iter()
            .filter(|(_, has)| **has & (1 << part) != 0)
            .map(|(p, _)| *p)
            .collect()
    }
}

/// Resolve the table's handles and hook its world effects.
pub(super) fn register(ctx: &ModContext, w: &Arc<WorldHooks>) {
    let parts: Vec<PartHandles> = fx::world_parts()
        .map(|(_, p, wd)| PartHandles {
            script: ctx.script(&format!("gml_Script_{}", p.script)).ok(),
            flag: ctx.variable(wd.flag).ok(),
            own_arg0: wd.own_arg0.and_then(|v| ctx.variable(v).ok()),
            lend: wd.lend,
        })
        .collect();
    let _ = PH.set(parts);
    for (i, p, _) in fx::world_parts() {
        let name = format!("gml_Script_{}", p.script);
        let c = Arc::clone(w);
        super::hook_or_warn(
            &name,
            ctx.hook_before(&name, move |call| {
                c.world_before(call, i);
            }),
        );
    }
}

impl WorldHooks {
    fn powers(&self, call: &Call<'_>) -> Option<u32> {
        let p = POWERS.load(Ordering::Relaxed);
        if p != u32::MAX {
            return Some(p);
        }
        let p = self.asset(call, "oPowers")?;
        POWERS.store(p, Ordering::Relaxed);
        Some(p)
    }

    /// `oPowers.<var>` (`None`: no `oPowers` now, a freeze).
    fn powers_f64(&self, call: &Call<'_>, var: Option<Variable>) -> Option<f64> {
        let (p, var) = (self.powers(call)?, var?);
        let mut v = None;
        let _ = call.with(&Value::object(p), |inst, _| v = inst.get_f64(var));
        v
    }

    fn powers_set(&self, call: &Call<'_>, var: Option<Variable>, v: f64) -> bool {
        let (Some(p), Some(var)) = (self.powers(call), var) else {
            return false;
        };
        let mut ok = false;
        let _ = call.with(&Value::object(p), |inst, _| ok = inst.set_f64(var, v));
        ok
    }

    /// The parts this game's player has now (`None`: no `oPowers`).
    fn own_parts(&self, call: &Call<'_>, ph: &[PartHandles]) -> Option<u8> {
        let mut bits = 0u8;
        for (i, h) in ph.iter().enumerate() {
            if self.powers_f64(call, h.flag)? > 0.5 {
                bits |= 1 << i;
            }
        }
        Some(bits)
    }

    /// Run step: this game's parts to the partners; a lend left by a death
    /// path that never came back put back; old entries forgotten.
    pub(super) fn ondeath_run_step(&self, call: &Call<'_>, me: PeerId) -> Vec<Message> {
        let Some(ph) = PH.get() else {
            return Vec::new();
        };
        let stuck: Vec<(usize, f64)> = {
            let st = self.lock();
            (st.ondeath.lent.iter().enumerate())
                .filter(|(_, l)| l.depth > 0)
                .map(|(i, l)| (i, l.saved))
                .collect()
        };
        for (i, saved) in stuck {
            if let Some(h) = ph.get(i) {
                self.powers_set(call, h.flag, saved);
            }
            let mut st = self.lock();
            st.ondeath.lent[i].depth = 0;
            st.ondeath.counts[i].stuck_lends += 1;
            if st.ondeath.counts[i].stuck_lends == 1 {
                log::warn!(
                    "co-op on-death: a death path's lend of part {i}'s flag was still out at the run step: put back"
                );
            }
        }
        let has = self.own_parts(call, ph);
        let mut st = self.lock();
        let frame = st.frame;
        let running = !st.frozen;
        let r = &mut st.ondeath;
        let own = has.or(r.own);
        r.tick(running);
        let Some(own) = own else {
            return Vec::new();
        };
        if r.own != Some(own) || frame.saturating_sub(r.sent_frame) >= RESEND_EVERY {
            r.own = Some(own);
            r.sent_frame = frame;
            return vec![Message::OnDeath(OnDeathMsg::Has {
                peer: me,
                parts: own,
            })];
        }
        Vec::new()
    }

    /// Run step: the partners' parts and the host's word for this game.
    pub(super) fn ondeath_inbox(&self, inbox: &mut Vec<Message>) {
        let mut st = self.lock();
        let (me, frame) = (st.me, st.ondeath.run_frames);
        inbox.retain(|m| match m {
            Message::OnDeath(OnDeathMsg::Has { peer, parts }) => {
                if Some(*peer) != me {
                    st.ondeath.partners.insert(*peer, *parts);
                }
                false
            }
            Message::OnDeath(OnDeathMsg::Given {
                net_id,
                by,
                part,
                argc,
                args,
            }) => {
                if Some(*by) == me && usize::from(*part) < MAX_PARTS {
                    st.ondeath.given.insert(
                        (*net_id, *part),
                        Given {
                            argc: *argc,
                            args: *args,
                            frame,
                        },
                    );
                }
                false
            }
            _ => true,
        });
    }

    /// Host, before a death path of an enemy at `hull <= 0`: each part the
    /// path reaches only through its flag is lent while only a partner has
    /// it (the game's own test of the death then reaches the part, which
    /// [`Self::world_before`] hands over). Returns the parts it lent.
    pub(super) fn ondeath_lend(&self, call: &Call<'_>) -> u8 {
        let Some(ph) = PH.get() else { return 0 };
        let mut lent = 0u8;
        for (i, h) in ph.iter().enumerate().filter(|(_, h)| h.lend) {
            let bit = 1u8 << i;
            {
                let mut st = self.lock();
                let l = &mut st.ondeath.lent[i];
                if l.depth > 0 {
                    l.depth += 1;
                    lent |= bit;
                    continue;
                }
                if !st.ondeath.partners.values().any(|has| has & bit != 0) {
                    continue;
                }
            }
            let Some(now) = self.powers_f64(call, h.flag) else {
                continue;
            };
            if now > 0.5 || !self.powers_set(call, h.flag, 1.0) {
                continue;
            }
            let mut st = self.lock();
            st.ondeath.lent[i] = Lend {
                depth: 1,
                saved: now,
            };
            lent |= bit;
        }
        lent
    }

    /// Host, after that death path: each lend put back after the outermost.
    pub(super) fn ondeath_unlend(&self, call: &Call<'_>, lent: u8) {
        if lent == 0 {
            return;
        }
        let Some(ph) = PH.get() else { return };
        for (i, h) in ph.iter().enumerate() {
            if lent & (1 << i) == 0 {
                continue;
            }
            let saved = {
                let mut st = self.lock();
                let l = &mut st.ondeath.lent[i];
                if l.depth == 0 {
                    continue;
                }
                l.depth -= 1;
                (l.depth == 0).then_some(l.saved)
            };
            if let Some(saved) = saved {
                self.powers_set(call, h.flag, saved);
            }
        }
    }

    /// Whether world effect `part` is kept from a player who is down
    /// ([`fx::World::kept_from_the_down`]).
    fn kept_from_the_down(part: u8) -> bool {
        fx::world_parts()
            .nth(usize::from(part))
            .is_some_and(|(_, _, w)| w.kept_from_the_down())
    }

    /// Before world effect `part` (its `self` is the dying enemy). Whether it
    /// runs here.
    fn world_before(&self, call: &mut Call<'_>, part: u8) -> bool {
        let Some(me) = call.instance() else {
            return true;
        };
        match self.role() {
            ROLE_HOST => self.world_host(call, &me, part),
            ROLE_CLIENT => {
                let key = me.key();
                let mut st = self.lock();
                if st.ondeath.firing {
                    return true;
                }
                // A copy dying by the host's word: the host's word says
                // whose it is (played as the copy dies, before this event).
                if st.copy_deaths > 0 {
                    drop(st);
                    call.skip_original();
                    let _ = call.set_return(0.0);
                    return false;
                }
                // Its own kill while its ship is down: a part kept from the
                // down is not played (the host passes this game over for it).
                if st.orbs.own_down() && Self::kept_from_the_down(part) {
                    drop(st);
                    call.skip_original();
                    let _ = call.set_return(0.0);
                    return false;
                }
                // Its own kill (or no copy): it runs; a copy's is marked so
                // that the host's word for it is not played again.
                let has = st.ondeath.own.is_some_and(|o| o & (1 << part) != 0);
                let frame = st.ondeath.run_frames;
                if has && let Some(n) = st.client.net_id(key) {
                    st.ondeath.played.insert((n, part), frame);
                }
                true
            }
            _ => true,
        }
    }

    /// Host: whose this death's part is; the host's own runs, a partner's is
    /// skipped here and handed over. Whether it runs here.
    fn world_host(&self, call: &mut Call<'_>, me: &Instance<'_>, part: u8) -> bool {
        // Not a death (an enemy destroyed without one): the host's, as in a
        // solo game; the joining game plays nothing for it.
        if self
            .h
            .hull
            .and_then(|v| me.get_f64(v))
            .is_none_or(|h| h > 0.0)
        {
            return true;
        }
        let key = me.key();
        let i = usize::from(part);
        let (net, credit, lent, with, host, downs) = {
            let st = self.lock();
            let host = st.me.unwrap_or(PeerId::HOST);
            let mut downs: Vec<PeerId> = st
                .partners
                .iter()
                .filter(|p| p.downed)
                .map(|p| p.peer)
                .collect();
            if st.orbs.own_down() {
                downs.push(host);
            }
            (
                st.host.net_id(key),
                st.host.damager_at(key),
                st.ondeath.lent[i].depth > 0,
                st.ondeath.with(part),
                host,
                downs,
            )
        };
        // The flag now (a lend reads 1: not the host's).
        let host_has = !lent
            && PH
                .get()
                .and_then(|ph| ph.get(i))
                .and_then(|h| self.powers_f64(call, h.flag))
                .is_some_and(|v| v > 0.5);
        // A partner whose confirmed claim is open predicted the death and
        // played it (its part too, if it has it): the kill is its.
        let killer = credit.or_else(|| net.and_then(|n| self.dmg.claimant_now(n)));
        let by = fx::world_effect_by(
            host,
            host_has,
            killer,
            &with,
            Self::kept_from_the_down(part),
            |p| downs.contains(&p),
        );
        // Nobody may play it, though the host has it (the host is down): its
        // own call is skipped, not run.
        if by.is_none() && host_has {
            call.skip_original();
            let _ = call.set_return(0.0);
            return false;
        }
        if by == Some(host) || by.is_none() {
            return true;
        }
        call.skip_original();
        let _ = call.set_return(0.0);
        let (Some(by), Some(net)) = (by, net) else {
            return false;
        };
        let count = call.argc().min(4);
        let mut args = [0.0f32; 4];
        for (k, a) in args.iter_mut().enumerate().take(count) {
            *a = call.arg(k).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        }
        let mut st = self.lock();
        st.host.queue(Message::OnDeath(OnDeathMsg::Given {
            net_id: net,
            by,
            part,
            argc: count as u8,
            args,
        }));
        false
    }

    /// Joining side, a copy dying by the host's word (before its Destroy
    /// event, as the game's own death path plays its parts before it): each
    /// part the host gave this game, unless the copy played it here already.
    pub(super) fn ondeath_at_death(&self, call: &Call<'_>, me: &Instance<'_>, net_id: NetId) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let Some(ph) = PH.get() else { return };
        for (i, h) in ph.iter().enumerate() {
            let part = i as u8;
            let given = {
                let mut st = self.lock();
                let given = st.ondeath.given.remove(&(net_id, part));
                if given.is_some() && st.ondeath.played.contains_key(&(net_id, part)) {
                    continue;
                }
                let Some(g) = given else { continue };
                // Given while this ship was up, played as it is down: kept
                // from the down, it is not played (it would be destroyed).
                if st.orbs.own_down() && Self::kept_from_the_down(part) {
                    continue;
                }
                st.ondeath.firing = true;
                g
            };
            let own0 = h.own_arg0.and_then(|v| self.powers_f64(call, Some(v)));
            let args: Vec<Value> = (given.args.iter().take(usize::from(given.argc)))
                .enumerate()
                .map(|(k, a)| match (k, own0) {
                    (0, Some(o)) => Value::Real(o),
                    _ => Value::Real(f64::from(*a)),
                })
                .collect();
            if let Some(s) = h.script
                && let Err(e) = me.call_script(s, &args)
            {
                crate::game::warn_once(
                    "world-ondeath",
                    &format!("co-op on-death: part {i} on a dying copy: {e}"),
                );
            }
            let mut st = self.lock();
            st.ondeath.firing = false;
            let frame = st.ondeath.run_frames;
            st.ondeath.played.insert((net_id, part), frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A part given before a shared hold longer than the keep still plays
    /// after it: only running frames age it.
    #[test]
    fn a_long_hold_does_not_expire_a_given_part() {
        let mut g = OnDeathGlue::default();
        g.tick(true);
        let at = g.run_frames;
        let given = Given {
            argc: 0,
            args: [0.0; 4],
            frame: at,
        };
        g.given.insert((7, 0), given);
        g.played.insert((8, 0), at);
        // A hold of ten times the keep, then less than the keep running.
        for _ in 0..(KEEP_FRAMES * 10) {
            assert_eq!(g.tick(false), [0; MAX_PARTS]);
        }
        for _ in 0..(KEEP_FRAMES - 2) {
            g.tick(true);
        }
        assert!(g.given.contains_key(&(7, 0)));
        assert!(g.played.contains_key(&(8, 0)));
        // The keep's worth of running frames: forgotten, and counted.
        let mut expired = [0; MAX_PARTS];
        for _ in 0..2 {
            for (e, n) in expired.iter_mut().zip(g.tick(true)) {
                *e += n;
            }
        }
        assert!(g.given.is_empty() && g.played.is_empty());
        assert_eq!(expired[0], 1);
    }
}
