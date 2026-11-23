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

//! Kill effects and the status channel: hooks and game calls (see
//! [`crate::damage::fx`] and [`ndml_net::status`]). Both are off
//! ([`crate::damage::DamageSettings::kill_fx_on`], `status_on`).
//!
//! * **Kill effects** (host): a death path of an enemy at `hull <= 0` pushes
//!   a context with its kill credit; a killer-build part
//!   ([`fx::KILLER_BUILD`]) inside a death credited to a partner is skipped.
//! * **Status channel**: the joining side reports the status changes its
//!   player made on copies ([`crate::damage::status_cause`]); the host
//!   applies each increase to the real enemy (held while frozen or
//!   deactivated) and acknowledges it.

#![forbid(unsafe_code)]

use std::sync::Arc;

use ndml_net::msg::{Message, NetId, PeerId};
use ndml_net::status::{Marks, StatusEntry, StatusMsg};
use ndml_sdk::{Call, Instance, ModContext, Script, Value, Variable};

use super::{DamageHooks, ROLE_CLIENT, ROLE_HOST};
use crate::damage::status_cause::{self, Vals};
use crate::damage::{DamageSettings, fx};

/// The death paths whose context the killer-build parts are judged in.
const DEATH_PATHS: [&str; 5] = [
    "gml_Script_checkDestroy",
    "gml_Script_checkDestroyReduced",
    "gml_Script_checkDestroyNoExplosion",
    "gml_Script_checkDestroyQuiet",
    "gml_Script_enemyDestroyed",
];
const SPAWN_HIT_PULSE: &str = "gml_Script_spawnHitPulse";
/// `stunOther`'s floor on `enemySpeed`.
const SPEED_FLOOR: f64 = 0.25;

/// A status report entry with its reporter (held while it cannot apply).
pub type HeldStatus = (PeerId, StatusEntry);

/// Variables and scripts the effects need (resolved at start-up).
#[derive(Debug, Default)]
pub struct FxHandles {
    pushed_x: Option<Variable>,
    pushed_y: Option<Variable>,
    enemy_speed: Option<Variable>,
    vuln_mult: Option<Variable>,
    vuln_count: Option<Variable>,
    sat_hits: Option<Variable>,
    blitzed: Option<Variable>,
    can_hit_pulse: Option<Variable>,
    hit_pulse: Option<Script>,
}

impl FxHandles {
    #[must_use]
    pub fn new(ctx: &ModContext) -> Self {
        let v = |n: &str| ctx.variable(n).ok();
        let [px, py, speed, vm, vc, sat, blitz, pulse] = status_cause::VARS;
        Self {
            pushed_x: v(px),
            pushed_y: v(py),
            enemy_speed: v(speed),
            vuln_mult: v(vm),
            vuln_count: v(vc),
            sat_hits: v(sat),
            blitzed: v(blitz),
            can_hit_pulse: v(pulse),
            hit_pulse: ctx.script(SPAWN_HIT_PULSE).ok(),
        }
    }

    /// A copy's status variables, in [`status_cause::VARS`] order.
    pub(super) fn vals(&self, me: &Instance<'_>) -> Vals {
        let r = |v: Option<Variable>| v.and_then(|v| me.get_f64(v)).unwrap_or(0.0);
        [
            r(self.pushed_x),
            r(self.pushed_y),
            r(self.enemy_speed),
            r(self.vuln_mult),
            r(self.vuln_count),
            r(self.sat_hits),
            r(self.blitzed),
            r(self.can_hit_pulse),
        ]
    }

    fn marks(&self, me: &Instance<'_>) -> Marks {
        let f = |v: Option<Variable>| v.and_then(|v| me.get_f64(v)).unwrap_or(0.0) as f32;
        Marks {
            vulnerability_mult: f(self.vuln_mult),
            vulnerability_count: f(self.vuln_count),
            sat_fire_hits: f(self.sat_hits),
            blitzed: f(self.blitzed),
        }
    }
}

/// The status channel's count of entries for an enemy the world does not track.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusCounts {
    pub missing: u64,
}

impl DamageHooks {
    fn kill_fx(&self) -> bool {
        DamageSettings::kill_fx_on() && self.role() == ROLE_HOST
    }

    fn status_on(&self, role: u8) -> bool {
        DamageSettings::status_on() && self.role() == role
    }

    /// Before a death path: a context when the enemy is at `hull <= 0`.
    fn death_before(&self, call: &Call<'_>) -> bool {
        if !self.kill_fx() {
            return false;
        }
        let (Some(me), Some(hv)) = (call.instance(), self.hull) else {
            return false;
        };
        if me.get_f64(hv).is_none_or(|h| h > 0.0) {
            return false;
        }
        // The world's credit, under its lock and not this one (never both).
        let credited = self.kill_credit.get().and_then(|f| (f.0)(me.key()));
        self.lock().deaths.push(credited);
        true
    }

    fn death_after(&self, pushed: bool) {
        if pushed {
            self.lock().deaths.pop();
        }
    }

    /// Before a killer-build part: skipped inside a partner's kill.
    fn killer_part(&self, call: &mut Call<'_>) {
        if !self.kill_fx() {
            return;
        }
        let mut st = self.lock();
        if st.deaths.last().copied().flatten().is_some() {
            st.kills.skipped();
            drop(st);
            call.skip_original();
        }
    }

    /// A copy's tick: the status changes the player's side made on it since
    /// the last tick go to the reporter; its knockback is taken off (the
    /// copy's pose is the host's, whose game moves it).
    pub fn client_status(&self, me: &Instance<'_>, net_id: NetId) {
        if !self.status_on(ROLE_CLIENT) {
            return;
        }
        let h = &self.fx;
        let key = me.key();
        let mut zeroed = Vec::new();
        for (i, v) in [
            (status_cause::PUSH_X, h.pushed_x),
            (status_cause::PUSH_Y, h.pushed_y),
        ] {
            if let Some(v) = v
                && let Some(was) = me.get_f64(v).filter(|p| *p != 0.0)
                && me.set_f64(v, 0.0)
            {
                zeroed.push((i, -was));
            }
        }
        let now = h.marks(me);
        let mut st = self.lock();
        for (i, d) in zeroed {
            st.status_causes.own_write(key, i, d);
        }
        let mut c = st.status_causes.take(key);
        // A slow is never reported: the only one is the Temporal shield's
        // area, which the host makes round the avatar (`crate::world::area`);
        // reporting it too would slow those enemies twice.
        c.slow = 0.0;
        if c.is_empty() {
            return;
        }
        st.status_rep.push(net_id, c.push_x as f32, c.push_y as f32);
        for _ in 0..c.flashes {
            st.status_rep.flash(net_id);
        }
        // Only the marks this side raised, at their values now; 0 for the
        // others (merged by maximum on the host: a 0 changes nothing).
        let pick = |rose: bool, v: f32| if rose { v } else { 0.0 };
        st.status_rep.marks(
            net_id,
            Marks {
                vulnerability_mult: pick(c.marks[0], now.vulnerability_mult),
                vulnerability_count: pick(c.marks[1], now.vulnerability_count),
                sat_fire_hits: pick(c.marks[2], now.sat_fire_hits),
                blitzed: pick(c.marks[3], now.blitzed),
            },
        );
    }

    /// Joining side: the host's acks.
    pub(super) fn client_status_messages(&self, me: PeerId, inbox: &[Message]) {
        if !DamageSettings::status_on() {
            return;
        }
        let mut st = self.lock();
        for m in inbox {
            match m {
                Message::Status(StatusMsg::Ack {
                    peer,
                    frame,
                    entries,
                }) => st.status_rep.on_ack(me, *peer, *frame, entries),
                Message::Despawn { net_id, .. } => st.status_rep.forget(*net_id),
                _ => {}
            }
        }
    }

    /// Joining side: the reports due this frame.
    pub(super) fn client_status_reports(&self, me: PeerId) -> Vec<Message> {
        if !self.status_on(ROLE_CLIENT) {
            return Vec::new();
        }
        let mut st = self.lock();
        let frame = st.frame;
        st.status_rep
            .take_reports(me, frame)
            .into_iter()
            .map(Message::Status)
            .collect()
    }

    /// The status report entries in `inbox`, with their reporters.
    #[must_use]
    pub fn status_entries(inbox: &[Message]) -> Vec<HeldStatus> {
        inbox
            .iter()
            .filter_map(|m| match m {
                Message::Status(StatusMsg::Report { by, entries }) => Some((*by, entries)),
                _ => None,
            })
            .flat_map(|(by, entries)| entries.iter().map(move |e| (by, *e)))
            .collect()
    }

    /// Host, a running frame: apply peers' status entries to the real
    /// enemies (`lookup`: a tracked net id's instance id). Returns the
    /// entries for deactivated enemies, for the caller to hand in again.
    pub fn host_status(
        &self,
        call: &Call<'_>,
        entries: Vec<HeldStatus>,
        lookup: &dyn Fn(NetId) -> Option<Value>,
    ) -> Vec<HeldStatus> {
        let mut held = Vec::new();
        if !self.status_on(ROLE_HOST) {
            return held;
        }
        for (by, e) in entries {
            let Some(id) = lookup(e.net_id) else {
                // Not tracked: acknowledged with nothing applied, so the
                // reporter stops resending.
                let mut st = self.lock();
                if !st.status_led.report(by, &e).is_empty() {
                    st.status_counts.missing += 1;
                    if st.status_counts.missing <= 5 {
                        log::info!(
                            "co-op effects: status for {} by {by:?}: not an enemy here (answered, not applied)",
                            e.net_id
                        );
                    }
                }
                continue;
            };
            if !self.exists(call, &id) {
                held.push((by, e));
                continue;
            }
            let a = self.lock().status_led.report(by, &e);
            if a.is_empty() {
                continue;
            }
            let h = &self.fx;
            let _ = call.with(&id, |inst, _| {
                let add = |v: Option<Variable>, d: f32| {
                    if let Some(v) = v
                        && d != 0.0
                    {
                        let now = inst.get_f64(v).unwrap_or(0.0);
                        let _ = inst.set_f64(v, now + f64::from(d));
                    }
                };
                add(h.pushed_x, a.push_x);
                add(h.pushed_y, a.push_y);
                if a.slow > 0.0
                    && let Some(v) = h.enemy_speed
                    && let Some(s) = inst.get_f64(v)
                {
                    let _ = inst.set_f64(v, (s - f64::from(a.slow)).max(SPEED_FLOOR.min(s)));
                }
                if let Some(m) = a.marks {
                    let merged = h.marks(&inst).max(m);
                    for (v, x) in [
                        (h.vuln_mult, merged.vulnerability_mult),
                        (h.vuln_count, merged.vulnerability_count),
                        (h.sat_hits, merged.sat_fire_hits),
                        (h.blitzed, merged.blitzed),
                    ] {
                        if let Some(v) = v {
                            let _ = inst.set_f64(v, f64::from(x));
                        }
                    }
                }
                if a.flashes > 0
                    && let Some(s) = h.hit_pulse
                {
                    let _ = inst.call_script(s, &[]);
                }
            });
        }
        held
    }

    /// Host: the status acks this frame (every account that changed lately:
    /// applied, or answered for an id that is not an enemy here; a despawn
    /// forgets its accounts).
    pub(super) fn host_status_acks(&self) -> Vec<Message> {
        if !self.status_on(ROLE_HOST) {
            return Vec::new();
        }
        self.lock()
            .status_led
            .acks(|_| true)
            .into_iter()
            .map(Message::Status)
            .collect()
    }
}

/// Register the effects' hooks. Returns how many sites were hooked.
pub fn register(ctx: &ModContext, d: &Arc<DamageHooks>) -> usize {
    let mut n = 0;
    let mut ok = |name: &str, r: Result<(), ndml_sdk::Error>| match r {
        Ok(()) => n += 1,
        Err(e) => log::warn!("co-op effects: {name}: {e}"),
    };
    if DamageSettings::kill_fx_on() {
        for name in DEATH_PATHS {
            let (a, b) = (Arc::clone(d), Arc::clone(d));
            ok(
                name,
                ctx.hook(
                    name,
                    move |call| a.death_before(call),
                    move |_call, pushed| b.death_after(pushed),
                ),
            );
        }
        for (part, _) in fx::KILLER_BUILD {
            let name = format!("gml_Script_{part}");
            let a = Arc::clone(d);
            ok(
                &name,
                ctx.hook_before(&name, move |call| a.killer_part(call)),
            );
        }
    }
    // No stun bracket: the game never calls `stun` or `stunOther`.
    n
}
