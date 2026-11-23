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

//! Which local hull changes on a copy are the joining player's (pure part;
//! the hooks are in [`super::glue`]). Only those cross the network; every
//! other local change to a copy's hull is the host's and is put back at once.
//! Each hull writer ([`super::glue::AUDIT_SITES`]) is bracketed, its context
//! decided from the call's `self` and `other`, its changes taken from an
//! `oSolid` sweep before and after:
//!
//! * [`Ctx::Player`]: `self` or `other` is player-side (the mirror's rule).
//! * [`Ctx::Chained`]: `self` is not a measured copy and took a player-side
//!   hit this frame or the last (a part passing damage to its leader).
//! * [`Ctx::Status`]: a status tick on a copy the local player has hit.
//! * [`Ctx::Host`]: everything else.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

/// Changes below this are float noise.
const EPS: f64 = 1e-3;

/// Why a bracketed call's changes count as the player's, or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctx {
    Player,
    Chained,
    Status,
    Host,
}

impl Ctx {
    /// Whether changes made in this context are the joining player's.
    #[must_use]
    pub fn counts(self) -> bool {
        self != Self::Host
    }

    fn index(self) -> usize {
        match self {
            Self::Player => 0,
            Self::Chained => 1,
            Self::Status => 2,
            Self::Host => 3,
        }
    }
}

/// Per source function: hull lowered on copies in each context (`Host`:
/// put back), and how many changes were put back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceRow {
    pub lowered: [f64; 4],
    pub restored_events: u64,
}

#[derive(Debug, Default)]
struct Bracket {
    /// Per instance: change made by nested brackets that counted.
    counted: HashMap<usize, f64>,
    /// Per instance: the mod's own writes inside this bracket.
    own: HashMap<usize, f64>,
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct Causes {
    stack: Vec<Bracket>,
    frame: u64,
    /// Instance key -> last frame a counted change lowered its hull.
    hit: HashMap<usize, u64>,
    /// Copies the local player has damaged (status ticks count on these).
    touched: HashSet<usize>,
    rows: HashMap<u16, SourceRow>,
    /// Per instance: hull the counted contexts took off since its last
    /// measurement (and the frame of the last), for [`Self::take_counted`].
    counted: HashMap<usize, (f64, u64)>,
    /// Drops the measurement saw that no counted context explains (not
    /// reported: a cause outside every bracket is not the player's).
    unexplained: (u64, f64),
}

impl Causes {
    /// An empty attribution.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A new game frame (the taint window is this frame and the last).
    pub fn begin_frame(&mut self, frame: u64) {
        self.frame = frame;
        self.stack.clear();
        self.hit.retain(|_, f| frame.saturating_sub(*f) <= 1);
        self.counted.retain(|_, c| frame.saturating_sub(c.1) <= 2);
    }

    /// Whether `key` took a counted hit this frame or the last.
    #[must_use]
    pub fn hit_lately(&self, key: usize) -> bool {
        self.hit
            .get(&key)
            .is_some_and(|f| self.frame.saturating_sub(*f) <= 1)
    }

    /// Whether the local player has damaged `key` before.
    #[must_use]
    pub fn touched(&self, key: usize) -> bool {
        self.touched.contains(&key)
    }

    /// The local player's status reached `key` without a hull change (a
    /// corrosion spread): its status ticks there are the player's.
    pub fn touch(&mut self, key: usize) {
        self.touched.insert(key);
    }

    /// An instance left the game (its key may be reused).
    pub fn forget(&mut self, key: usize) {
        self.hit.remove(&key);
        self.touched.remove(&key);
        self.counted.remove(&key);
    }

    /// A bracketed call begins.
    pub fn enter(&mut self) {
        self.stack.push(Bracket::default());
    }

    /// The mod wrote `delta` to `key`'s hull (not a game cause).
    pub fn own_write(&mut self, key: usize, delta: f64) {
        if delta.abs() > EPS
            && let Some(top) = self.stack.last_mut()
        {
            *top.own.entry(key).or_insert(0.0) += delta;
        }
    }

    /// A bracketed call of `src` in context `ctx` ended. `is_copy`: whether
    /// an instance is a measured copy. Returns the hulls to put back
    /// (`Host` changes on copies), as (key, hull).
    pub fn leave(
        &mut self,
        src: u16,
        ctx: Ctx,
        before: &HashMap<usize, f64>,
        after: &HashMap<usize, f64>,
        is_copy: &dyn Fn(usize) -> bool,
    ) -> Vec<(usize, f64)> {
        let child = self.stack.pop().unwrap_or_default();
        let mut restore = Vec::new();
        let mut up_counted: Vec<(usize, f64)> = Vec::new();
        let mut up_own: Vec<(usize, f64)> = Vec::new();
        for (&key, &b) in before {
            let Some(&a) = after.get(&key) else { continue };
            if !(a.is_finite() && b.is_finite()) {
                continue;
            }
            let own = child.own.get(&key).copied().unwrap_or(0.0);
            let nested = child.counted.get(&key).copied().unwrap_or(0.0);
            let mine = a - b - own - nested;
            if own.abs() > EPS {
                up_own.push((key, own));
            }
            if ctx.counts() {
                if mine < -EPS {
                    self.hit.insert(key, self.frame);
                    let c = self.counted.entry(key).or_insert((0.0, self.frame));
                    c.0 += -mine;
                    c.1 = self.frame;
                    if is_copy(key) {
                        self.touched.insert(key);
                        self.row(src).lowered[ctx.index()] += -mine;
                    }
                }
                let total = mine + nested;
                if total.abs() > EPS {
                    up_counted.push((key, total));
                }
                continue;
            }
            if nested.abs() > EPS {
                up_counted.push((key, nested));
            }
            if mine.abs() <= EPS || !is_copy(key) {
                continue;
            }
            let r = self.row(src);
            r.restored_events += 1;
            if mine < 0.0 {
                r.lowered[Ctx::Host.index()] += -mine;
            }
            restore.push((key, a - mine));
        }
        if let Some(parent) = self.stack.last_mut() {
            for (k, d) in up_counted {
                *parent.counted.entry(k).or_insert(0.0) += d;
            }
            for (k, d) in up_own {
                *parent.own.entry(k).or_insert(0.0) += d;
            }
        }
        restore
    }

    fn row(&mut self, src: u16) -> &mut SourceRow {
        self.rows.entry(src).or_default()
    }

    /// A copy's measurement: the hull counted contexts took off it since the
    /// last one (the rest of `drop` is counted as unexplained).
    pub fn take_counted(&mut self, key: usize, drop: f64) -> f64 {
        let counted = self.counted.remove(&key).map_or(0.0, |c| c.0);
        if drop > counted + EPS {
            self.unexplained.0 += 1;
            self.unexplained.1 += drop - counted;
        }
        counted
    }

    /// Drops no counted context explains: (events, hull).
    #[must_use]
    pub fn unexplained(&self) -> (u64, f64) {
        self.unexplained
    }

    /// Totals: hull lowered on copies per context (player, chained, status,
    /// host-owned and put back) and the changes put back.
    #[must_use]
    pub fn totals(&self) -> ([f64; 4], u64) {
        let mut t = [0.0; 4];
        let mut n = 0;
        for r in self.rows.values() {
            for (a, b) in t.iter_mut().zip(r.lowered) {
                *a += b;
            }
            n += r.restored_events;
        }
        (t, n)
    }

    /// Per source, sorted by host-owned hull put back (largest first).
    #[must_use]
    pub fn rows(&self) -> Vec<(u16, SourceRow)> {
        let mut v: Vec<(u16, SourceRow)> = self.rows.iter().map(|(k, r)| (*k, r.clone())).collect();
        v.sort_by(|a, b| {
            b.1.lowered[3]
                .total_cmp(&a.1.lowered[3])
                .then(a.0.cmp(&b.0))
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: &[(usize, f64)]) -> HashMap<usize, f64> {
        v.iter().copied().collect()
    }

    #[test]
    fn host_causes_on_copies_are_put_back_and_player_hits_stay() {
        let mut c = Causes::new();
        c.begin_frame(10);
        let copy = |k: usize| k == 1 || k == 2;
        // A hazard the copy's own code made lowers copy 1 and a local
        // missile (3, not a copy).
        c.enter();
        let r = c.leave(
            0,
            Ctx::Host,
            &m(&[(1, 100.0), (3, 10.0)]),
            &m(&[(1, 60.0), (3, 0.0)]),
            &copy,
        );
        assert_eq!(r, vec![(1, 100.0)]);
        // A player shot lowers copy 2: kept, and the copy is touched.
        c.enter();
        let r = c.leave(1, Ctx::Player, &m(&[(2, 50.0)]), &m(&[(2, 40.0)]), &copy);
        assert!(r.is_empty());
        assert!(c.touched(2) && c.hit_lately(2) && !c.touched(1));
        let (t, n) = c.totals();
        assert!((t[0] - 10.0).abs() < 1e-9 && (t[3] - 40.0).abs() < 1e-9 && n == 1);
    }

    #[test]
    fn a_counted_nested_call_survives_its_host_owned_parent() {
        let mut c = Causes::new();
        c.begin_frame(5);
        let copy = |k: usize| k == 1;
        // The copy's own Step (host-owned) calls a status tick that counts
        // (-5), and itself lowers the hull by 20 more.
        c.enter();
        c.enter();
        let r = c.leave(1, Ctx::Status, &m(&[(1, 100.0)]), &m(&[(1, 95.0)]), &copy);
        assert!(r.is_empty());
        let r = c.leave(0, Ctx::Host, &m(&[(1, 100.0)]), &m(&[(1, 75.0)]), &copy);
        assert_eq!(r, vec![(1, 95.0)]);
    }

    #[test]
    fn the_mods_own_writes_are_not_a_cause_and_parts_are_not_put_back() {
        let mut c = Causes::new();
        c.begin_frame(1);
        let copy = |k: usize| k == 1;
        c.enter();
        c.own_write(1, -30.0);
        let r = c.leave(
            0,
            Ctx::Host,
            &m(&[(1, 100.0), (7, 9.0)]),
            &m(&[(1, 70.0), (7, 1.0)]),
            &copy,
        );
        assert!(
            r.is_empty(),
            "the mod's write stays, the part (7) is the game's"
        );
    }

    #[test]
    fn only_counted_hull_is_measured() {
        let mut c = Causes::new();
        c.begin_frame(1);
        c.enter();
        let _ = c.leave(0, Ctx::Player, &m(&[(1, 100.0)]), &m(&[(1, 90.0)]), &|k| {
            k == 1
        });
        // The copy measured 25 down: 10 the player's, 15 from nowhere.
        assert!((c.take_counted(1, 25.0) - 10.0).abs() < 1e-9);
        assert_eq!(c.unexplained().0, 1);
        assert!((c.unexplained().1 - 15.0).abs() < 1e-9);
        assert!(c.take_counted(1, 0.0).abs() < 1e-9, "taken once");
    }

    #[test]
    fn the_taint_lasts_this_frame_and_the_next() {
        let mut c = Causes::new();
        c.begin_frame(1);
        c.enter();
        let _ = c.leave(0, Ctx::Player, &m(&[(9, 10.0)]), &m(&[(9, 5.0)]), &|_| {
            false
        });
        assert!(
            c.hit_lately(9) && !c.touched(9),
            "a part: tainted, never touched"
        );
        c.begin_frame(2);
        assert!(c.hit_lately(9));
        c.begin_frame(3);
        assert!(!c.hit_lately(9));
    }
}

/// The copies whose corrosion stacks a spread raised: `before` and `after`
/// are (key, `corrosiveStacks`) read round the game's `corrosiveSpread`.
#[must_use]
pub fn risen(before: &[(usize, f64)], after: &[(usize, f64)]) -> Vec<usize> {
    let was: HashMap<usize, f64> = before.iter().copied().collect();
    after
        .iter()
        .filter(|(k, v)| *v > was.get(k).copied().unwrap_or(0.0) + EPS)
        .map(|(k, _)| *k)
        .collect()
}

#[cfg(test)]
mod spread_tests {
    use super::*;

    #[test]
    fn a_spread_marks_the_copies_it_raised_and_their_ticks_count() {
        let before = [(1, 0.0), (2, 3.0), (3, 2.0)];
        let after = [(1, 2.0), (2, 3.0), (3, 1.0), (4, 2.0)];
        // Raised: 1 (0 -> 2) and 4 (new in the list); 2 unchanged, 3 fell.
        assert_eq!(risen(&before, &after), vec![1, 4]);
        let mut c = Causes::default();
        assert!(!c.touched(1));
        for k in risen(&before, &after) {
            c.touch(k);
        }
        assert!(c.touched(1) && c.touched(4) && !c.touched(2));
        c.forget(1);
        assert!(!c.touched(1), "a gone copy is forgotten");
    }
}
