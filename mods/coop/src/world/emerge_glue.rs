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

//! Enemies that emerge from the background in the world hooks (the
//! `oEnemyEmerging` family; the rules: [`super::super::emerge`]).
//!
//! * Which: every object under [`classify::parent::EMERGING`], from the
//!   object tree, found once.
//! * They have no `oSolid`, so their Step has no enemy tick: the
//!   controller's End Step ticks each one as that tick would, on both games.
//! * Joining game: a Step the rules hold runs with the family's gate
//!   (`emerged`) shut and opened again after it.
//!
//! [`classify::parent::EMERGING`]: super::classify::parent::EMERGING

#![forbid(unsafe_code)]

use ndml_sdk::{Call, Instance, Value};

use super::super::emerge;
use super::names::builtin_var as bv;
use super::{ROLE_CLIENT, ROLE_HOST, ROLE_OFF, WorldHooks};

/// The family's own gate: its emerge runs while this is 0.
pub(super) const EMERGED: &str = "emerged";

impl WorldHooks {
    /// The objects that emerge from the background, by the object tree
    /// (found once the objects are resolved; empty before, and in a game
    /// without the family).
    pub(super) fn emerging_family(&self, call: &Call<'_>) -> &[u32] {
        if let Some(v) = self.emerging.get() {
            return v;
        }
        let Some(root) = self.lock().objects.as_ref().map(|o| o.emerging) else {
            return &[];
        };
        let mut found = Vec::new();
        if root.is_some() {
            let mut missing = 0;
            for obj in 0..4096u32 {
                match self.class_of(call, obj, true) {
                    None => {
                        missing += 1;
                        if missing >= 64 {
                            break;
                        }
                        continue;
                    }
                    Some(c) if c.ticked_after_its_step() => found.push(obj),
                    Some(_) => {}
                }
                missing = 0;
            }
        }
        let names: Vec<String> = found
            .iter()
            .map(|&o| format!("{} {o}", self.object_name(call, o)))
            .collect();
        log::info!(
            "co-op world: enemies that emerge from the background (oEnemyEmerging children; replicated, ticked after their Step, the turn the host's): {}",
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        );
        self.lock()
            .client
            .set_emerging(found.iter().filter_map(|&o| u16::try_from(o).ok()));
        let _ = self.emerging.set(found);
        self.emerging.get().map_or(&[], Vec::as_slice)
    }

    /// Whether `obj` emerges from the background (the family found).
    pub(super) fn is_emerging_object(&self, obj: u32) -> bool {
        self.emerging.get().is_some_and(|v| v.contains(&obj))
    }

    /// The controller's End Step: tick every instance of the family as an
    /// `oSolid`'s End Step tick would (after its own Step this frame). Not
    /// while frozen (deactivated, as the dependencies' ticks).
    pub(super) fn emerging_end_step(&self, call: &Call<'_>) {
        let role = self.role();
        if role == ROLE_OFF || self.emerging_family(call).is_empty() {
            return;
        }
        let root = {
            let st = self.lock();
            if st.frozen {
                return;
            }
            st.objects.as_ref().and_then(|o| o.emerging)
        };
        let Some(root) = root else { return };
        let mut ids = Vec::new();
        let _ = call.with(&Value::object(root), |inst, _| {
            if let Ok(id) = inst.builtin_value(bv::ID) {
                ids.push(id);
            }
        });
        // The joining game: the field's size, for a turn's birth across its
        // wrap.
        if role == ROLE_CLIENT
            && !ids.is_empty()
            && let (Some(g), [Some(w), Some(h)]) = (call.global(), self.h.field)
            && let (Some(w), Some(h)) = (g.get_f64(w), g.get_f64(h))
        {
            self.lock().client.set_field(w, h);
        }
        for id in &ids {
            let _ = call.with(id, |inst, _| {
                if role == ROLE_HOST {
                    // Ended in its Step this frame (its turn): no tick; the
                    // next run step finds it gone.
                    if self.destroy_pending(inst.key()) {
                        return;
                    }
                    self.host_tick(&inst);
                } else if role == ROLE_CLIENT {
                    self.client_tick(call, &inst);
                }
            });
        }
    }

    /// The joining game, before a Step of an instance of the family: shut
    /// its emerge's gate for this Step when the rules hold it. Never under
    /// the world's lock across the write.
    pub(super) fn emerge_hold_before(&self, me: &Instance<'_>) {
        let Some(v) = self.h.emerged else { return };
        let key = me.key();
        let step = self.lock().client.emerge_step(key);
        if step != Some(emerge::Step::Hold) || me.get_f64(v) != Some(0.0) {
            return;
        }
        if me.set_f64(v, 1.0) {
            self.lock().emerge_held.push(key);
        }
    }

    /// The joining game, after that Step: the gate open again.
    pub(super) fn emerge_hold_after(&self, me: &Instance<'_>) {
        let key = me.key();
        let held = {
            let mut st = self.lock();
            let before = st.emerge_held.len();
            st.emerge_held.retain(|k| *k != key);
            st.emerge_held.len() != before
        };
        if held && let Some(v) = self.h.emerged {
            let _ = me.set_f64(v, 0.0);
        }
    }

    /// Whether the instance's object is one of the family (the observer's
    /// quick test, no lock).
    pub(super) fn emerging_instance(&self, object: i32) -> bool {
        u32::try_from(object).is_ok_and(|o| self.is_emerging_object(o))
    }
}
