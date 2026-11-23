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

//! Corrosion spread on the joining side. The game's `corrosiveSpread` gives
//! a dying enemy's `corrosiveStacks` to the enemies near it; the joining
//! player's stacks live on its copies, whose deaths are the host's. So:
//!
//! * a dying copy with stacks runs `corrosiveSpread` as itself first;
//! * the copies a spread raised are marked as hit by the local player, so
//!   their corrosion ticks count as that player's damage.

#![forbid(unsafe_code)]

use std::sync::OnceLock;

use ndml_sdk::{Call, Instance, ModContext, Script, Variable};

use super::{ROLE_CLIENT, WorldHooks};
use crate::damage::cause;

/// The game's spread.
pub const CORROSIVE_SPREAD: &str = "gml_Script_corrosiveSpread";

struct Handles {
    spread: Option<Script>,
    stacks: Option<Variable>,
}

static CH: OnceLock<Handles> = OnceLock::new();

/// Resolve the handles and watch the spread.
pub(super) fn register(ctx: &ModContext, w: &std::sync::Arc<WorldHooks>) {
    let _ = CH.set(Handles {
        spread: ctx.script(CORROSIVE_SPREAD).ok(),
        stacks: ctx.variable("corrosiveStacks").ok(),
    });
    let (c1, c2) = (std::sync::Arc::clone(w), std::sync::Arc::clone(w));
    super::hook_or_warn(
        CORROSIVE_SPREAD,
        ctx.hook(
            CORROSIVE_SPREAD,
            move |call| c1.spread_before(call),
            move |call, before| c2.spread_after(call, &before),
        ),
    );
}

impl WorldHooks {
    /// The live copies' stacks (joining side; empty otherwise).
    fn copy_stacks(&self, call: &Call<'_>) -> Vec<(usize, f64)> {
        let Some(var) = CH.get().and_then(|h| h.stacks) else {
            return Vec::new();
        };
        let ids = self.lock().client.live_pairs();
        let mut out = Vec::with_capacity(ids.len());
        for (_, id) in ids {
            let _ = call.with(&id, |i, _| {
                out.push((i.key(), i.get_f64(var).unwrap_or(0.0)));
            });
        }
        out
    }

    fn spread_before(&self, call: &mut Call<'_>) -> Vec<(usize, f64)> {
        if self.role() != ROLE_CLIENT {
            return Vec::new();
        }
        self.copy_stacks(call)
    }

    fn spread_after(&self, call: &mut Call<'_>, before: &[(usize, f64)]) {
        if self.role() != ROLE_CLIENT || before.is_empty() {
            return;
        }
        let after = self.copy_stacks(call);
        let raised = cause::risen(before, &after);
        self.dmg.mark_touched(&raised);
    }

    /// Joining side, before a killed copy's death plays: its own stacks
    /// spread by the game's `corrosiveSpread` as itself.
    pub(super) fn spread_at_death(&self, me: &Instance<'_>) {
        if self.role() != ROLE_CLIENT {
            return;
        }
        let Some(h) = CH.get() else { return };
        let (Some(spread), Some(var)) = (h.spread, h.stacks) else {
            return;
        };
        if !me.get_f64(var).is_some_and(|s| s > 0.0) {
            return;
        }
        if let Err(e) = me.call_script(spread, &[]) {
            crate::game::warn_once(
                "world-spread",
                &format!("co-op world: corrosiveSpread on a dying copy: {e}"),
            );
        }
    }
}
