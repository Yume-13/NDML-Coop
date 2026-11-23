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

//! What a world spawn's Destroy event makes stays each game's own: the
//! joining game runs its copy's Destroy event too, and a replicated death
//! effect (the station core's kaleidoscope) can read host-only assets.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ndml_sdk::ModContext;

use super::{ROLE_HOST, WorldHooks};

/// GameMaker's Destroy event kind.
const EVENT_DESTROY: i32 = 1;

/// Host: world spawns whose Destroy event is running now (they nest: a
/// Destroy can destroy another). Game thread only.
static DYING: AtomicU32 = AtomicU32::new(0);
/// Creates held back since the outermost such Destroy event began.
static HELD: AtomicU32 = AtomicU32::new(0);

/// Whether a world spawn's Destroy event is running now (asked by the
/// world-spawn rule at each create; a yes is counted for the log).
pub(super) fn dying_world_spawn() -> bool {
    let dying = DYING.load(Ordering::Relaxed) > 0;
    if dying {
        HELD.fetch_add(1, Ordering::Relaxed);
    }
    dying
}

/// Watch Destroy events of world spawns on the host.
pub(super) fn register(ctx: &ModContext, w: &Arc<WorldHooks>) {
    let c = Arc::clone(w);
    match ctx.observe_event(EVENT_DESTROY, 0, move |e| {
        if c.role() != ROLE_HOST {
            DYING.store(0, Ordering::Relaxed);
            return;
        }
        let Some(i) = e.instance() else { return };
        if !c.lock().deps.world.contains(&i.key()) {
            return;
        }
        if e.after {
            let was = DYING
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_sub(1))
                })
                .unwrap_or(0);
            let held = if was <= 1 {
                HELD.swap(0, Ordering::Relaxed)
            } else {
                0
            };
            if held > 0 {
                let (id, object) = (e.id, e.instance_object);
                log::info!(
                    "co-op world: {held} create(s) in the Destroy event of world spawn {id} (object {object}) not taken: each game makes its own"
                );
            }
            // Gone: a later instance at the same address is not a world
            // spawn (`deps.world` is keyed by address; also pruned where a
            // taken dependency is forgotten, `tick_deps_host`).
            c.lock().deps.world.remove(&i.key());
        } else {
            DYING.fetch_add(1, Ordering::Relaxed);
        }
    }) {
        Ok(()) => log::info!(
            "co-op world: observing world spawns' Destroy events (what they make stays local)"
        ),
        Err(e) => log::warn!(
            "co-op world: {e}; what a world spawn's Destroy event makes is replicated"
        ),
    }
}
