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

//! Session rules: shared pause, the shared upgrade screen, shared experience
//! and score, and the downed state with respawn. The host sequences them
//! (`ndml_net::session`); they are a no-op outside a linked co-op run past
//! the start gate.
//!
//! * [`machine`]: the rules for one run (pure): messages, hook decisions,
//!   one game action per frame, what to show.
//! * [`glue`]: the hooks and game calls.
//! * [`xp`], [`score`], [`orbs`]: the shared experience, score and pickups.
//! * [`downed`]: what a down does to what follows the ship.
//! * [`countdown`], [`protect`], [`banish`]: the 3, 2, 1 count, the respawn's
//!   protection, and Last Stand banished in every co-op run.
//!
//! In short:
//! * *Pause.* A player's pause pauses everyone; play resumes when everyone
//!   has resumed. A player who resumes first waits on the pause screen.
//! * *Upgrades.* Opening the upgrade screen opens everyone's. A player who
//!   has chosen, or has nothing to choose, waits. The round ends when
//!   everyone is done, or for a partner gone (a lost link, a leave).
//! * *Downed.* A ship that would die while a partner is linked goes down
//!   instead (hidden, invulnerable, no control) and comes back after the
//!   respawn time (game time) by the game's own fly-in (`oPlayer_Alarm_3`).
//!   Last Stand's revive comes first. When nobody is left standing, every
//!   game dies through its own death path.

#![forbid(unsafe_code)]

pub mod banish;
pub mod countdown;
pub mod downed;
pub mod glue;
pub mod machine;
pub mod orbs;
pub mod protect;
pub mod score;
pub mod settings;
pub mod xp;

pub use machine::Rules;
pub use settings::RulesSettings;
