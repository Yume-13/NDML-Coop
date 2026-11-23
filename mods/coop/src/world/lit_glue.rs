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

//! The Star Eater's lit segments from the host (its lightning warning).
//!
//! A segment lights through `starEaterMakeLightning`, which ramps `redAlpha`
//! and `whiteAlpha` to 1, discharges and fades; the Draw events read only
//! those two.
//!
//! * Host, at the controller's End Step: the changed `redAlpha`,
//!   `whiteAlpha` and `whiteFlash` of each tracked [`LIT_OBJECTS`] instance
//!   go out in `LitStates`.
//! * Joining game: `starEaterMakeLightning` does not run on a copy; the
//!   host's values at the shown time are written on every copy at the
//!   controller's End Step, before anything draws.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

use ndml_net::msg::{LitItem, LitStates, Message, NetId};
use ndml_sdk::{Call, ModContext, Value, Variable};

use super::{ROLE_CLIENT, ROLE_HOST, WorldHooks, hook_or_warn};

/// The objects whose lit state is the host's.
pub const LIT_OBJECTS: [&str; 2] = ["oStarEaterSegment", "oStarEater"];
/// The script that ramps and discharges a segment.
pub const MAKE_LIGHTNING: &str = "gml_Script_starEaterMakeLightning";

struct Handles {
    red: Variable,
    white: Variable,
    flash: Variable,
}

static LH: OnceLock<Handles> = OnceLock::new();

/// Thousandths, clamped to `u16`.
#[must_use]
pub fn milli(v: f64) -> u16 {
    if v.is_finite() {
        (v * 1000.0).round().clamp(0.0, f64::from(u16::MAX)) as u16
    } else {
        0
    }
}

/// One instance's lit values (thousandths, flag).
pub type Lit = (u16, u16, bool);

/// Both roles' lit state (inside the world's lock).
#[derive(Debug, Default)]
pub(super) struct LitGlue {
    objects: Option<Vec<u32>>,
    /// Host: the values last sent, by net id.
    sent: HashMap<NetId, Lit>,
    /// Host: to send at this read's time.
    out: Vec<LitItem>,
    out_time: u32,
    /// Client: the host's values waiting for the shown time, by net id.
    pending: HashMap<NetId, VecDeque<(u32, Lit)>>,
    /// Client: the host's values now, by net id.
    host: HashMap<NetId, Lit>,
}

/// White at or above this (thousandths) is the peak a discharge leaves from.
pub const PEAK: u16 = 990;

impl LitGlue {
    /// Client: a batch of the host's, queued per instance.
    pub(super) fn queue(&mut self, b: &LitStates) {
        for it in &b.items {
            self.pending
                .entry(it.net_id)
                .or_default()
                .push_back((b.time_ms, (it.red, it.white, it.flash)));
        }
    }

    /// Client: the host's values due at `shown`. A frame that would pass an
    /// instance through its white peak stops there, so the peak is drawn.
    pub(super) fn take_due(&mut self, shown: u32) {
        let host = &mut self.host;
        self.pending.retain(|net, q| {
            while q.front().is_some_and(|&(t, _)| t <= shown) {
                let Some((_, lit)) = q.pop_front() else { break };
                let was = host.insert(*net, lit);
                let rising = lit.1 >= PEAK && was.is_none_or(|w| w.1 < PEAK);
                if rising && q.front().is_some_and(|&(t, _)| t <= shown) {
                    break;
                }
            }
            !q.is_empty()
        });
    }

    /// Host, after a resync: every instance's values go again at the next
    /// read (only changes are sent).
    pub(super) fn resync_host(&mut self) {
        self.sent.clear();
    }

    /// Joining game, a resync: the host's values from before it are dropped.
    pub(super) fn resync_client(&mut self) {
        self.pending.clear();
        self.host.clear();
    }

    /// Joining game: drop the values of net ids the table no longer knows
    /// (16-bit net ids are reused in a long session).
    pub(super) fn forget_gone(&mut self, known: impl Fn(NetId) -> bool) {
        self.host.retain(|n, _| known(*n));
    }

    /// Forget everything but the resolved objects.
    pub(super) fn clear(&mut self) {
        let objects = self.objects.take();
        *self = Self {
            objects,
            ..Self::default()
        };
    }
}

/// Resolve the handles and hook `starEaterMakeLightning`.
pub(super) fn register(ctx: &ModContext, w: &std::sync::Arc<WorldHooks>) {
    if let (Ok(red), Ok(white), Ok(flash)) = (
        ctx.variable("redAlpha"),
        ctx.variable("whiteAlpha"),
        ctx.variable("whiteFlash"),
    ) {
        let _ = LH.set(Handles { red, white, flash });
    }
    let c = std::sync::Arc::clone(w);
    hook_or_warn(
        MAKE_LIGHTNING,
        ctx.hook_before(MAKE_LIGHTNING, move |call| c.lit_make_before(call)),
    );
}

impl WorldHooks {
    fn lit_on(&self) -> bool {
        self.shots_on() && LH.get().is_some()
    }

    fn lit_objects(&self, call: &Call<'_>) -> Vec<u32> {
        if let Some(o) = &self.lock().lit.objects {
            return o.clone();
        }
        let o: Vec<u32> = LIT_OBJECTS
            .iter()
            .filter_map(|n| self.asset(call, n))
            .collect();
        self.lock().lit.objects = Some(o.clone());
        o
    }

    /// Client, before `starEaterMakeLightning`: not on a copy.
    fn lit_make_before(&self, call: &mut Call<'_>) {
        if self.role() != ROLE_CLIENT || !self.lit_on() {
            return;
        }
        let Some(me) = call.instance() else { return };
        let key = me.key();
        let copy = self.lock().client.owns(key);
        if copy {
            call.skip_original();
        }
    }

    /// The controller's End Step: the host reads and queues the changes; a
    /// joining game writes the host's values on its copies.
    pub(super) fn lit_end_step(&self, call: &Call<'_>) {
        let role = self.role();
        if !(role == ROLE_HOST || role == ROLE_CLIENT) || !self.lit_on() {
            return;
        }
        let Some(h) = LH.get() else { return };
        let objects = self.lit_objects(call);
        // Client: the host's values due at the shown time.
        if role == ROLE_CLIENT {
            let mut st = self.lock();
            if let Some(shown) = st.repl.shown_ms {
                st.lit.take_due(shown);
            }
            let st = &mut *st;
            let client = &st.client;
            st.lit.forget_gone(|n| client.is_known(n));
        }
        let mut seen = Vec::new();
        for &o in &objects {
            let _ = call.with(&Value::object(o), |inst, _| {
                let key = inst.key();
                let (net, want) = {
                    let st = self.lock();
                    if role == ROLE_HOST {
                        (st.host.net_id(key), None)
                    } else {
                        let n = st.client.net_id(key);
                        (n, n.and_then(|n| st.lit.host.get(&n).copied()))
                    }
                };
                let Some(net) = net else { return };
                if let Some((r, w, f)) = want {
                    let _ = inst.set_f64(h.red, f64::from(r) / 1000.0);
                    let _ = inst.set_f64(h.white, f64::from(w) / 1000.0);
                    let _ = inst.set_f64(h.flash, f64::from(u8::from(f)));
                }
                let lit = (
                    milli(inst.get_f64(h.red).unwrap_or(0.0)),
                    milli(inst.get_f64(h.white).unwrap_or(0.0)),
                    inst.get_value(h.flash).is_some_and(|v| {
                        v.as_bool().unwrap_or(false) || v.as_f64().is_some_and(|x| x > 0.5)
                    }),
                );
                seen.push((net, lit));
            });
        }
        let mut st = self.lock();
        let now = st.repl.now_ms;
        let live: HashSet<NetId> = seen.iter().map(|(n, _)| *n).collect();
        for (net, lit) in seen {
            if role == ROLE_HOST && st.lit.sent.insert(net, lit) != Some(lit) {
                st.lit.out.push(LitItem {
                    net_id: net,
                    red: lit.0,
                    white: lit.1,
                    flash: lit.2,
                });
                st.lit.out_time = now;
            }
        }
        st.lit.sent.retain(|n, _| live.contains(n));
    }

    /// Host, the run step: the changes as `LitStates`.
    pub(super) fn lit_run_step(&self) -> Vec<Message> {
        if self.role() != ROLE_HOST || !self.lit_on() {
            return Vec::new();
        }
        let mut st = self.lock();
        if st.lit.out.is_empty() {
            return Vec::new();
        }
        let items = std::mem::take(&mut st.lit.out);
        let parts = LitStates::split(st.lit.out_time, &items);
        parts.into_iter().map(Message::LitStates).collect()
    }

    /// Client: the host's `LitStates`, queued for the shown time.
    pub(super) fn lit_inbox(&self, inbox: &mut Vec<Message>) {
        if !inbox
            .iter()
            .any(|m| matches!(m, Message::LitStates(_) | Message::WorldResync { .. }))
        {
            return;
        }
        let mut st = self.lock();
        inbox.retain(|m| match m {
            Message::LitStates(l) => {
                st.lit.queue(l);
                false
            }
            // In the inbox's order; kept for the world.
            Message::WorldResync { .. } => {
                st.lit.resync_client();
                true
            }
            _ => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{LitGlue, milli};
    use ndml_net::msg::{LitItem, LitStates};

    fn batch(t: u32, net: u16, white: u16) -> LitStates {
        LitStates {
            time_ms: t,
            items: vec![LitItem {
                net_id: net,
                red: 1000,
                white,
                flash: true,
            }],
        }
    }

    #[test]
    fn a_white_peak_due_with_later_values_is_drawn_for_a_frame() {
        let mut g = LitGlue::default();
        for (t, w) in [(10, 500), (27, 1000), (44, 600), (61, 200)] {
            g.queue(&batch(t, 7, w));
        }
        g.queue(&batch(44, 8, 300));
        // Three of the host's frames due at once: the peak is shown.
        g.take_due(50);
        assert_eq!(g.host[&7].1, 1000);
        assert_eq!(g.host[&8].1, 300);
        // The next frame goes on from there.
        g.take_due(50);
        assert_eq!(g.host[&7].1, 600);
        g.take_due(70);
        assert_eq!(g.host[&7].1, 200);
        assert!(g.pending.is_empty());
        // A peak held over from frame to frame is not held again.
        g.queue(&batch(80, 7, 1000));
        g.queue(&batch(90, 7, 1000));
        g.queue(&batch(95, 7, 0));
        g.take_due(90);
        assert_eq!(g.host[&7].1, 1000);
        g.take_due(100);
        assert_eq!(g.host[&7].1, 0);
    }

    #[test]
    fn a_gone_enemys_values_are_forgotten() {
        let mut g = LitGlue::default();
        g.queue(&batch(10, 7, 500));
        g.queue(&batch(10, 8, 300));
        g.take_due(20);
        g.forget_gone(|n| n == 7);
        assert_eq!(g.host.keys().copied().collect::<Vec<_>>(), vec![7]);
    }

    #[test]
    fn alphas_in_thousandths() {
        assert_eq!(milli(1.0), 1000);
        assert_eq!(milli(0.2504), 250);
        assert_eq!(milli(-0.3), 0);
        assert_eq!(milli(f64::NAN), 0);
        assert_eq!(milli(1e9), u16::MAX);
    }
}
