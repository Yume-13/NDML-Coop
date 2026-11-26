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

//! The shared world's life within a run, with three ways for it to stop:
//!
//! 1. **Link interrupted**: nothing is torn down; the host keeps tracking
//!    enemies, the client keeps its puppets at their last pose. If the
//!    partner comes back in the same run the host resyncs the full world.
//! 2. **Partner left for good** (the host only): the host stops replicating.
//!    Past the start gate its run is over too (`state::Core::run_over`) and
//!    the game leaves for the menu. A joining game never owns the world: the
//!    host gone ends its run, so its world stays as for outcome 1 until
//!    outcome 3.
//! 3. **Run ended** (menu, a new run): everything is forgotten.

#![forbid(unsafe_code)]

use ndml_net::endpoint::DisconnectReason;

/// This game's part in the shared world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Not sharing (solo, menu, disabled).
    Off,
    /// Owns the enemies.
    Host,
    /// Shows the host's enemies.
    Client,
}

/// Why a link went away, as far as the world cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    /// It may come back: keep the world.
    Interrupted,
    /// It won't: the host stops sharing.
    Left,
}

impl Loss {
    /// The world's reading of a disconnect.
    #[must_use]
    pub fn of(reason: DisconnectReason) -> Self {
        match reason {
            DisconnectReason::Timeout | DisconnectReason::HandshakeTimeout => Self::Interrupted,
            DisconnectReason::Remote
            | DisconnectReason::Replaced
            | DisconnectReason::Local
            | DisconnectReason::Refused(_) => Self::Left,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Active,
    Interrupted,
    /// Shared world over for this run (the host stopped sharing).
    Done,
}

/// What changed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Nothing changed.
    None,
    /// Sharing starts in this role (fresh state).
    Start(Role),
    /// Outcome 1: the link is gone for now; keep everything.
    Interrupted,
    /// The partner is back in the same run (host: resync everyone).
    Resumed,
    /// Outcome 2 (the host only).
    PartnerLeft,
    /// Outcome 3 (the previous role).
    RunEnded(Role),
}

/// The run as the world sees it each frame.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // independent facts about the frame
pub struct LifeInput {
    /// The run's identity (`oController` key, `timeGameStart`); `None` at the menu.
    pub run: Option<(usize, Option<f64>)>,
    /// The run started linked (the co-op latch).
    pub coop_run: bool,
    /// A partner is linked.
    pub linked: bool,
    /// This game hosts.
    pub is_host: bool,
    /// The world's objects are resolved (or not needed yet).
    pub enabled: bool,
    /// The last disconnect since the previous frame, if any.
    pub loss: Option<Loss>,
}

/// The shared world's state within a run (see the module docs).
#[derive(Debug, Clone)]
pub struct Lifecycle {
    role: Role,
    phase: Phase,
    run: Option<(usize, Option<f64>)>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            role: Role::Off,
            phase: Phase::Idle,
            run: None,
        }
    }
}

impl Lifecycle {
    /// The role the hooks act on (kept while interrupted).
    #[must_use]
    pub fn role(&self) -> Role {
        match self.phase {
            Phase::Active | Phase::Interrupted => self.role,
            Phase::Idle | Phase::Done => Role::Off,
        }
    }

    /// The link is down but the world is kept.
    #[must_use]
    pub fn interrupted(&self) -> bool {
        matches!(self.phase, Phase::Interrupted)
    }

    /// Advance one frame.
    pub fn update(&mut self, i: &LifeInput) -> Transition {
        if i.run != self.run {
            let was = self.role();
            self.run = i.run;
            self.phase = Phase::Idle;
            self.role = Role::Off;
            if was != Role::Off {
                return Transition::RunEnded(was);
            }
        }
        if i.run.is_none() || !i.enabled {
            return Transition::None;
        }
        match self.phase {
            Phase::Idle if i.coop_run && i.linked => {
                self.role = if i.is_host { Role::Host } else { Role::Client };
                self.phase = Phase::Active;
                Transition::Start(self.role)
            }
            Phase::Active if !i.linked => {
                if self.left(i) {
                    self.phase = Phase::Done;
                    Transition::PartnerLeft
                } else {
                    self.phase = Phase::Interrupted;
                    Transition::Interrupted
                }
            }
            Phase::Interrupted => {
                if i.linked {
                    self.phase = Phase::Active;
                    Transition::Resumed
                } else if self.left(i) {
                    self.phase = Phase::Done;
                    Transition::PartnerLeft
                } else {
                    Transition::None
                }
            }
            _ => Transition::None,
        }
    }

    /// The host's partner gone for good (outcome 2). A joining game is never
    /// given a `Left` (its run ends instead).
    fn left(&self, i: &LifeInput) -> bool {
        self.role == Role::Host && i.loss == Some(Loss::Left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(run: Option<usize>, linked: bool, loss: Option<Loss>) -> LifeInput {
        LifeInput {
            run: run.map(|k| (k, Some(1.0))),
            coop_run: true,
            linked,
            is_host: false,
            enabled: true,
            loss,
        }
    }

    fn host(run: Option<usize>, linked: bool, loss: Option<Loss>) -> LifeInput {
        LifeInput {
            is_host: true,
            ..input(run, linked, loss)
        }
    }

    #[test]
    fn three_outcomes() {
        let mut l = Lifecycle::default();
        assert_eq!(l.update(&host(None, true, None)), Transition::None);
        assert_eq!(
            l.update(&host(Some(1), true, None)),
            Transition::Start(Role::Host)
        );
        assert_eq!(l.role(), Role::Host);
        // 1. Interrupted: role kept, waiting however long, then resumed.
        let lost = host(Some(1), false, Some(Loss::Interrupted));
        assert_eq!(l.update(&lost), Transition::Interrupted);
        assert_eq!(l.role(), Role::Host);
        assert!(l.interrupted());
        for _ in 0..10_000 {
            assert_eq!(l.update(&host(Some(1), false, None)), Transition::None);
        }
        assert!(l.interrupted());
        assert_eq!(l.update(&host(Some(1), true, None)), Transition::Resumed);
        // 1 -> 2: gone for good during the wait.
        l.update(&lost);
        assert_eq!(
            l.update(&host(Some(1), false, Some(Loss::Left))),
            Transition::PartnerLeft
        );
        assert_eq!(l.role(), Role::Off);
        assert_eq!(
            l.update(&host(Some(1), true, None)),
            Transition::None,
            "done for this run"
        );
        // 3. Run ended (from Done: nothing to tear down).
        assert_eq!(l.update(&host(None, true, None)), Transition::None);
    }

    #[test]
    fn goodbye_is_left_and_menu_ends_an_active_run() {
        let mut l = Lifecycle::default();
        l.update(&host(Some(1), true, None));
        assert_eq!(
            l.update(&host(Some(1), false, Some(Loss::Left))),
            Transition::PartnerLeft
        );
        let mut l = Lifecycle::default();
        let mut run = host(Some(1), true, None);
        assert_eq!(l.update(&run), Transition::Start(Role::Host));
        assert_eq!(
            l.update(&host(None, true, None)),
            Transition::RunEnded(Role::Host)
        );
        run.run = Some((2, Some(5.0)));
        assert_eq!(l.update(&run), Transition::Start(Role::Host));
        // A solo run never starts.
        let mut l = Lifecycle::default();
        let mut solo = input(Some(3), true, None);
        solo.coop_run = false;
        assert_eq!(l.update(&solo), Transition::None);
        assert_eq!(l.role(), Role::Off);
    }

    #[test]
    fn the_joining_game_keeps_its_world_until_the_run_ends() {
        let mut l = Lifecycle::default();
        assert_eq!(
            l.update(&input(Some(1), true, None)),
            Transition::Start(Role::Client)
        );
        assert_eq!(
            l.update(&input(Some(1), false, Some(Loss::Left))),
            Transition::Interrupted
        );
        assert_eq!(
            l.update(&input(Some(1), false, Some(Loss::Left))),
            Transition::None
        );
        assert_eq!(l.role(), Role::Client);
        assert!(l.interrupted());
        assert_eq!(
            l.update(&input(None, false, None)),
            Transition::RunEnded(Role::Client)
        );
    }

    #[test]
    fn disconnect_reasons() {
        assert_eq!(Loss::of(DisconnectReason::Timeout), Loss::Interrupted);
        assert_eq!(Loss::of(DisconnectReason::Remote), Loss::Left);
        assert_eq!(Loss::of(DisconnectReason::Replaced), Loss::Left);
    }
}
