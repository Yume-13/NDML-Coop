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

//! Enemies that emerge from the background (the `oEnemyEmerging` family,
//! [`super::classify::parent::EMERGING`]). Pure: the joining game's rules
//! for its copies of them.
//!
//! Such an enemy grows out of the background in its own Step and, at its
//! threshold, makes the solid enemy in its place and ends itself: the turn.
//! The joining game's copy is born a frame or two off the host's age, so the
//! turn is the host's.
//!
//! * The copy's emerge never runs ahead of the host's as shown: a Step runs
//!   only while the copy is at least [`MARGIN_FRAMES`] host frames behind,
//!   counted in the host's snapshots ([`Clock`]). A held Step keeps the
//!   family's gate (`emerged`) shut: the copy shows its last look.
//! * The copy goes in the frame the solid enemy's copy is born: the nearest
//!   newborn within [`TURN_PX`] after a despawn that is not a kill takes it
//!   ([`nearest_turned`]). A turned copy no birth takes goes at
//!   [`TURN_BOUND`] past the host's despawn.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::time::Duration;

/// Host frames between two of its snapshots (`host::SNAPSHOT_EVERY`).
pub const FRAMES_PER_SNAPSHOT: u32 = 2;
/// The joining copy's emerge stays at least this many host frames behind
/// the host's as shown (a first state read a frame after the host's first
/// tick, and the copy's birth frame either side of its first sample).
pub const MARGIN_FRAMES: u32 = 2;
/// Snapshot times kept (about 17 s of the host's world).
const KEPT: usize = 512;

/// The host's snapshot times as filed here, oldest first (each distinct).
#[derive(Debug, Clone, Default)]
pub struct SnapTimes(VecDeque<Duration>);

impl SnapTimes {
    /// A snapshot filed at `t` (a part of one already here, or an older one
    /// arriving late, adds nothing).
    pub fn push(&mut self, t: Duration) -> bool {
        if self.0.back().is_some_and(|b| t <= *b) {
            return false;
        }
        self.0.push_back(t);
        if self.0.len() > KEPT {
            self.0.pop_front();
        }
        true
    }

    /// Snapshots after `from` up to `to` (inclusive); `None` when `from` is
    /// older than the oldest kept (the count would be short).
    #[must_use]
    pub fn between(&self, from: Duration, to: Duration) -> Option<u32> {
        if self.0.front().is_none_or(|f| from < *f) {
            return None;
        }
        let n = self.0.iter().filter(|s| **s > from && **s <= to).count();
        Some(u32::try_from(n).unwrap_or(u32::MAX))
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// The joining copy's emerge, counted against the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// The copy's first sample's time (where its first state stands).
    first: Duration,
    /// The host frames the copy's own emerge has run past its first state
    /// (its own Steps let run, raised by an age-stamped repair).
    steps: u32,
}

impl Clock {
    #[must_use]
    pub fn new(first: Duration) -> Self {
        Self { first, steps: 0 }
    }

    /// Whether the copy's emerge may run a Step at the shown time `t` (the
    /// Step is then counted): only while it stays [`MARGIN_FRAMES`] behind
    /// the host frames shown since its first sample.
    pub fn step(&mut self, snaps: &SnapTimes, t: Duration) -> bool {
        let shown = snaps
            .between(self.first, t)
            .map_or(u32::MAX, |n| n.saturating_mul(FRAMES_PER_SNAPSHOT));
        if self.steps.saturating_add(1 + MARGIN_FRAMES) <= shown {
            self.steps += 1;
            true
        } else {
            false
        }
    }

    /// An age-stamped repair (the host's instance `age` frames after its
    /// first tick) was written to the copy: its emerge stands there now.
    pub fn repaired(&mut self, age: u64) {
        self.steps = self.steps.max(u32::try_from(age).unwrap_or(u32::MAX));
    }

    #[must_use]
    pub fn steps(&self) -> u32 {
        self.steps
    }
}

/// What a copy of the family does this Step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Its emerge runs.
    Run,
    /// Its emerge is held this Step (it shows its last look).
    Hold,
}

/// The rule for one Step of the joining copy: held once its despawn is
/// known (`despawn_known`), past its last host sample (`last_sample`), or
/// ahead of the host's shown frames ([`Clock::step`]); with no shown time
/// (no clock yet) it runs.
pub fn step(
    clock: &mut Clock,
    snaps: &SnapTimes,
    t: Option<Duration>,
    last_sample: Option<Duration>,
    despawn_known: bool,
) -> Step {
    if despawn_known {
        return Step::Hold;
    }
    let Some(t) = t else { return Step::Run };
    if last_sample.is_none_or(|l| t > l) {
        return Step::Hold;
    }
    if clock.step(snaps, t) {
        Step::Run
    } else {
        Step::Hold
    }
}

/// A newborn copy this near an emerging copy that has turned (px, across a
/// wrap too: [`turn_distance`]) is what it turned into.
pub const TURN_PX: f32 = 64.0;

/// A turned copy that no birth takes goes this long (shown time) after the
/// newest host snapshot when its despawn came: twice [`super::client`]'s
/// extrapolation cap. The solid enemy's copy is born at most about 70 ms
/// after that time.
pub const TURN_BOUND: Duration = Duration::from_millis(500);

/// The most a wrap moves a position past the field's size: twice the
/// wrapping enemy's own margin (`wrapWithMargin`; the Hive's is 59), with
/// room for larger ones.
pub const WRAP_EXTRA: f32 = 256.0;

/// One axis's distance across the field's wrap: `d` as it is, or, beyond
/// half the field, how far it falls outside one wrap (the field's size plus
/// up to [`WRAP_EXTRA`]).
fn axis_distance(d: f32, size: Option<f32>) -> f32 {
    let d = d.abs();
    match size {
        Some(s) if s > 0.0 && d > s / 2.0 => d.min((s - d).max(d - s - WRAP_EXTRA).max(0.0)),
        _ => d,
    }
}

/// How far a newborn copy at `born` is from where an emerging copy turned
/// (`turned`), across the field's wrap (`field`: its width and height;
/// `None`: as it is).
#[must_use]
pub fn turn_distance(turned: (f32, f32), born: (f32, f32), field: Option<(f32, f32)>) -> f32 {
    let dx = axis_distance(born.0 - turned.0, field.map(|f| f.0));
    let dy = axis_distance(born.1 - turned.1, field.map(|f| f.1));
    dx.hypot(dy)
}

/// The nearest of `pairs` (each a key, a turned copy's place and a
/// newborn copy's first place, both at the newborn's first sample's time)
/// within [`TURN_PX`]; ties go to the first.
#[must_use]
pub fn nearest_turned<K: Copy>(
    pairs: impl IntoIterator<Item = (K, (f32, f32), (f32, f32))>,
    field: Option<(f32, f32)>,
) -> Option<K> {
    let mut best: Option<(K, f32)> = None;
    for (k, at, born) in pairs {
        let d = turn_distance(at, born, field);
        if d <= TURN_PX && best.is_none_or(|(_, b)| d < b) {
            best = Some((k, d));
        }
    }
    best.map(|(k, _)| k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Snapshots every 33 ms from `from` for `n`.
    fn snaps(from: u64, n: u64) -> SnapTimes {
        let mut s = SnapTimes::default();
        for i in 0..n {
            s.push(ms(from + 33 * i));
        }
        s
    }

    #[test]
    fn snapshot_times_count_each_once_in_order() {
        let mut s = SnapTimes::default();
        assert!(s.push(ms(100)));
        assert!(!s.push(ms(100)), "a part of the same snapshot");
        assert!(!s.push(ms(67)), "an older one arriving late");
        assert!(s.push(ms(133)));
        assert_eq!(s.between(ms(100), ms(133)), Some(1));
        assert_eq!(s.between(ms(100), ms(132)), Some(0));
        assert_eq!(s.between(ms(50), ms(133)), None, "older than kept");
    }

    /// The host's emerge turns at its 72nd Step; the joining copy, Stepping
    /// every frame of its game at any phase against the host's, never
    /// reaches it while the host's instance is still shown, and stays within
    /// a few frames of the host's.
    #[test]
    fn the_joining_copys_emerge_never_runs_ahead_of_the_hosts() {
        const TURN: u32 = 72;
        for phase in [0u64, 5, 11, 16] {
            // The host's frames every 16.7 ms; a snapshot every second one.
            let first = ms(1000);
            let mut s = SnapTimes::default();
            let mut clock = Clock::new(first);
            let mut copy = 1u32; // the first state: after the host's first Step
            let mut host_shown_max = 0u32;
            for f in 0..120u64 {
                let t = first + ms(phase) + Duration::from_micros(16_667 * f);
                // Every snapshot up to a little past the shown time is here.
                while s.0.back().is_none_or(|b| *b < t + ms(100)) {
                    let n = s.0.len() as u64;
                    s.push(first + Duration::from_micros(33_333 * n));
                }
                // The host's Steps as shown at `t`: 1 at the first sample.
                let host =
                    1 + u32::try_from(t.checked_sub(first).unwrap().as_micros() / 16_667).unwrap();
                host_shown_max = host_shown_max.max(host);
                if host >= TURN {
                    break; // its turn is on screen: the copy goes now
                }
                if step(&mut clock, &s, Some(t), Some(t + ms(60)), false) == Step::Run {
                    copy += 1;
                }
                // Never ahead; behind once it has run (its first state may
                // stand where the host's first sample does).
                assert!(
                    copy <= host,
                    "phase {phase}: copy {copy} ahead of the host's {host}"
                );
                assert!(
                    copy < host || copy == 1,
                    "phase {phase}: copy {copy} at the host's"
                );
                assert!(
                    copy + 4 >= host || f < 4,
                    "phase {phase}: copy {copy} lags {host}"
                );
            }
            assert!(copy < TURN && copy + 4 >= TURN - 1, "phase {phase}: {copy}");
            assert!(host_shown_max >= TURN);
        }
    }

    /// A host hitch (no snapshots, the host ran no frames) holds the joining
    /// copy.
    #[test]
    fn a_host_hitch_holds_the_joining_copys_emerge() {
        let s = snaps(1000, 4); // to 1099 ms, then nothing
        let mut clock = Clock::new(ms(1000));
        let mut ran = 0;
        for f in 0..40u64 {
            let t = ms(1000) + Duration::from_micros(16_667 * f);
            if step(&mut clock, &s, Some(t), Some(ms(5000)), false) == Step::Run {
                ran += 1;
            }
        }
        // 3 snapshots after the first: 6 host frames; 2 behind.
        assert_eq!(ran, 4);
    }

    #[test]
    fn held_once_its_despawn_is_known_or_past_its_last_sample() {
        let s = snaps(0, 100);
        let mut clock = Clock::new(ms(0));
        let t = Some(ms(1000));
        assert_eq!(step(&mut clock, &s, t, Some(ms(2000)), true), Step::Hold);
        assert_eq!(step(&mut clock, &s, t, Some(ms(999)), false), Step::Hold);
        assert_eq!(step(&mut clock, &s, t, None, false), Step::Hold);
        assert_eq!(step(&mut clock, &s, t, Some(ms(1000)), false), Step::Run);
        assert_eq!(step(&mut clock, &s, None, None, false), Step::Run);
    }

    /// A repair stamped at the host's age moves the copy's count there: it
    /// does not then run on to the host's shown frames from below.
    #[test]
    fn a_repair_moves_the_copys_count_to_its_age() {
        let s = snaps(0, 100);
        let mut clock = Clock::new(ms(0));
        clock.repaired(40);
        assert_eq!(clock.steps(), 40);
        // At 1300 ms the host has shown 2 x 39 = 78 frames: 41 + 2 <= 78.
        assert!(clock.step(&s, ms(1300)));
        clock.repaired(10);
        assert_eq!(clock.steps(), 41, "an older stamp moves nothing back");
        // At 700 ms: 2 x 21 = 42 shown; 42 + 2 > 42.
        let mut c = Clock::new(ms(0));
        c.repaired(40);
        assert!(!c.step(&s, ms(700)));
    }

    /// A newborn copy is near a turned one as it is, or across the field's
    /// wrap (an enemy made past the top edge and wrapped in its first Step);
    /// far is far either way.
    #[test]
    fn a_birth_is_near_a_turn_as_it_is_or_across_a_wrap() {
        let field = Some((2000.0, 1200.0));
        assert!(turn_distance((500.0, 600.0), (530.0, 640.0), field) <= TURN_PX);
        assert!(turn_distance((500.0, 600.0), (600.0, 600.0), field) > TURN_PX);
        // y -60 to 1250: 1310 apart, one wrap of a 1200 field.
        assert!(turn_distance((900.0, -60.0), (905.0, 1250.0), field) <= TURN_PX);
        assert!(turn_distance((-40.0, 300.0), (2080.0, 310.0), field) <= TURN_PX);
        // Half the field apart: no wrap makes that near.
        assert!(turn_distance((900.0, 100.0), (900.0, 700.0), field) > TURN_PX);
        assert!(turn_distance((900.0, -60.0), (905.0, 1250.0), None) > TURN_PX);
    }

    /// Each birth takes the nearest turned copy within reach, or none.
    #[test]
    fn a_birth_takes_the_nearest_turned_copy() {
        let field = Some((2000.0, 1200.0));
        let turned = [(1u16, (500.0, 600.0)), (2, (540.0, 600.0))];
        let to = |born: (f32, f32)| turned.map(|(k, at)| (k, at, born));
        assert_eq!(nearest_turned(to((535.0, 600.0)), field), Some(2));
        assert_eq!(nearest_turned(to((505.0, 600.0)), field), Some(1));
        assert_eq!(nearest_turned(to((900.0, 600.0)), field), None);
        assert_eq!(nearest_turned([] as [(u16, _, _); 0], field), None);
    }
}
