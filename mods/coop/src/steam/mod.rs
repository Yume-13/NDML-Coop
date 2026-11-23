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

//! The Steam connector: the two games meet in a Steam lobby and exchange the
//! netcode's datagrams as P2P packets through the game's Steamworks extension.
//!
//! The network thread's lobby agent runs over [`ndml_net::steam::SteamMux`]
//! (Steam ids as synthetic addresses). The game thread owns every Steam call:
//! a pump in `oGamepad_Step_1` moves datagrams and drives the lobby flow, and
//! `oGuiCursor_Draw_75` flushes. A hidden `oHighscoresDynamic` instance
//! receives async Steam events (invites); polling alone still works.
//!
//! [`view`] is the UI interface; [`lobby`], [`pump`], [`events`],
//! [`roomcode`] and [`text`] are pure; `glue.rs` makes the game calls.

#![forbid(unsafe_code)]

pub mod events;
mod glue;
pub mod lobby;
pub mod names;
pub mod pump;
pub mod roomcode;
pub mod settings;
pub mod text;
pub mod view;

use std::sync::Arc;

use ndml_sdk::{Error, ModContext};

pub use glue::SteamCoop;
pub use settings::{ConnectorChoice, SteamSettings};
pub use view::{SteamAction, SteamPage, SteamView, Visibility};

fn hook_or_warn(name: &str, r: Result<(), Error>) -> bool {
    match r {
        Ok(()) => true,
        Err(e) => {
            log::warn!("co-op steam: unable to hook {name} ({e})");
            false
        }
    }
}

/// Register the connector's hooks. Returns how many event sites it took.
pub fn register(ctx: &ModContext, s: &Arc<SteamCoop>) -> usize {
    let mut n = 0;
    let c = Arc::clone(s);
    if hook_or_warn(
        names::PUMP_EVENT,
        ctx.hook_before(names::PUMP_EVENT, move |call| c.pump(call)),
    ) {
        n += 1;
    } else {
        log::warn!("co-op steam: unable to start the Steam pump");
    }
    let c = Arc::clone(s);
    n += usize::from(hook_or_warn(
        names::FLUSH_EVENT,
        ctx.hook_after(names::FLUSH_EVENT, move |call| c.flush(call)),
    ));
    let c = Arc::clone(s);
    n += usize::from(hook_or_warn(
        names::LISTENER_CREATE,
        ctx.hook_before(names::LISTENER_CREATE, move |call| c.listener_create(call)),
    ));
    for name in names::LISTENER_EVENTS {
        let c = Arc::clone(s);
        n += usize::from(hook_or_warn(
            name,
            ctx.hook_before(name, move |call| c.listener_event(call)),
        ));
    }
    let c = Arc::clone(s);
    n += usize::from(hook_or_warn(
        names::LISTENER_ASYNC,
        ctx.hook_before(names::LISTENER_ASYNC, move |call| c.listener_async(call)),
    ));
    for name in names::LEADERBOARD_GUARDS {
        let c = Arc::clone(s);
        n += usize::from(hook_or_warn(
            name,
            ctx.hook_before(name, move |call| c.guard(call)),
        ));
    }
    n
}
