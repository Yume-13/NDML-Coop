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

//! Upgrades banished for every co-op run: Last Stand.
//!
//! Each game calls the game's own `banishMod(mod)` as `oPowers` once per run,
//! at the run's first tick past the start gate.
//! * `oPowers.banishMods` is made by `oPowers_Create_0`: each run has a new one.
//! * `addModIfValid` refuses a banished mod. The game's banished row shows it.

#![forbid(unsafe_code)]

/// Upgrade objects banished in every co-op run.
pub const SESSION_BANISHED: &[&str] = &["mLastStand"];

/// Once per run: true the first time it is asked with `ready` for a run
/// key, false until the key changes.
#[derive(Debug, Default)]
pub struct RunLatch<K: PartialEq + Copy> {
    done: Option<K>,
}

impl<K: PartialEq + Copy> RunLatch<K> {
    /// Whether to act now for run `key`: `ready` (a co-op run, its gate
    /// past) and not yet done for this key.
    pub fn due(&mut self, key: K, ready: bool) -> bool {
        if !ready || self.done == Some(key) {
            return false;
        }
        self.done = Some(key);
        true
    }

    /// The run ended (menu): the next run is due again even with the same
    /// key.
    pub fn reset(&mut self) {
        self.done = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_per_run_and_only_when_ready() {
        let mut l = RunLatch::default();
        assert!(!l.due(1u32, false), "gate still holding");
        assert!(l.due(1, true));
        assert!(!l.due(1, true), "once");
        assert!(l.due(2, true), "the next run");
        l.reset();
        assert!(l.due(2, true), "after the menu, again");
    }

    #[test]
    fn last_stand_is_banished() {
        assert_eq!(SESSION_BANISHED, &["mLastStand"]);
    }
}
