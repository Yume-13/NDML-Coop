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

//! The hooks of [`crate::wild`]: `oUpgrade_Create_0` and `setRerolls` see
//! `gameModWildMetamorphosis` off during the call, then the run's value is
//! put back. [`restore_pending`] puts it back if an after-hook never ran.

#![forbid(unsafe_code)]

use std::sync::Arc;

use ndml_sdk::{Call, ModContext, Value, Variable};

use crate::Shared;
use crate::game::{var, warn_once};
use crate::names;
use crate::wild;

/// Resolved at init.
#[derive(Debug, Clone, Copy)]
struct Handles {
    /// `global.gameModWildMetamorphosis`.
    flag: Option<Variable>,
}

/// Install the hooks.
pub(crate) fn register(ctx: &ModContext, shared: &Arc<Shared>) {
    let flag = var(ctx, names::GLOBAL_WILD);
    let _ = FLAG.set(flag);
    let h = Handles { flag };
    for name in [names::UPGRADE_CREATE, names::SET_REROLLS] {
        let (s1, s2) = (Arc::clone(shared), Arc::clone(shared));
        let r = ctx.hook(
            name,
            move |call| before(call, &s1, h),
            move |call, old| after(call, &s2, h, old),
        );
        if let Err(e) = r {
            log::warn!("co-op: unable to hook {name} ({e}); wild_mods = \"off\" may not apply");
        }
    }
}

/// Switch the flag off for this event if the setting applies; returns the
/// run's value for the after-hook.
fn before(call: &mut Call<'_>, s: &Shared, h: Handles) -> Option<Value> {
    let var = h.flag?;
    let (setting, coop) = {
        let c = s.lock();
        (c.settings.wild_mods, c.core.linked() || c.core.coop_run())
    };
    let g = call.global()?;
    let old = g.get_value(var);
    let off = wild::off_value(setting, coop, old.as_ref())?;
    let old = old?;
    match g.set(var, off) {
        Ok(()) => {
            s.wild.set(old.clone());
            Some(old)
        }
        Err(e) => {
            warn_once(
                "wild-off",
                &format!("co-op: wild_mods: unable to switch the flag ({e})"),
            );
            None
        }
    }
}

fn after(call: &mut Call<'_>, s: &Shared, h: Handles, old: Option<Value>) {
    if let Some(old) = old {
        // The saved value is put back; the pending copy is spent.
        let _ = s.wild.take();
        put_back(call, h.flag, old);
    }
}

fn put_back(call: &Call<'_>, flag: Option<Variable>, old: Value) {
    let (Some(g), Some(var)) = (call.global(), flag) else {
        return;
    };
    if let Err(e) = g.set(var, old) {
        warn_once(
            "wild-back",
            &format!("co-op: wild_mods: unable to put the flag back ({e})"),
        );
    }
}

/// Put back a value an after-hook did not (run and menu ticks).
pub(crate) fn restore_pending(call: &Call<'_>, s: &Shared) {
    if let Some(old) = s.wild.take() {
        log::warn!("co-op: wild_mods: Wild Metamorphosis put back after an upgrade screen");
        put_back(call, FLAG.get().copied().flatten(), old);
    }
}

/// The flag, for [`restore_pending`] (`register` sets it).
static FLAG: std::sync::OnceLock<Option<Variable>> = std::sync::OnceLock::new();
