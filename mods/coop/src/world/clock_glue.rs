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

//! The joining game's render clock ([`crate::world::repl::StepClock`]), which
//! times every copy's pose and the timeline's births and deaths, and this
//! frame's clocks. A child of `glue` so it can use the hooks' state.

#![forbid(unsafe_code)]

use std::time::Duration;

use super::super::lifecycle::Role;
use super::WorldHooks;

/// The per-run clock state (inside the world's lock).
#[derive(Debug, Default)]
pub(super) struct ReplState {
    /// This game's wire clock at the run step (ms).
    pub(super) now_ms: u32,
    /// The time shown this frame: the host's clock on the host, the render
    /// time on a joining game (ms).
    pub(super) shown_ms: Option<u32>,
    /// The host's clock now (a joining game's estimate; ms).
    pub(super) host_now_ms: Option<u32>,
    /// Client render clock.
    step: crate::world::repl::StepClock,
}

fn ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

impl WorldHooks {
    /// Run step, before the role's frame: this frame's times. Returns the
    /// render time the client shows enemies at.
    pub(super) fn repl_frame_begin(
        &self,
        input: &super::WorldInput,
        role: Role,
    ) -> Option<Duration> {
        let mut render = input.render_time;
        let mut st = self.lock();
        if role != Role::Host {
            render = st.repl.step.advance(render);
        }
        st.repl.now_ms = input.now_ms;
        if role == Role::Host {
            st.repl.shown_ms = Some(input.now_ms);
            st.repl.host_now_ms = Some(input.now_ms);
        } else {
            st.repl.shown_ms = render.map(ms);
            st.repl.host_now_ms = input.host_now.map(ms);
            st.client.set_linear_angle(false);
            st.client.set_timing(true, true);
        }
        render
    }

    /// A shared world starts: the clocks start over.
    pub(super) fn repl_run_start(&self) {
        self.lock().repl = ReplState::default();
    }
}
