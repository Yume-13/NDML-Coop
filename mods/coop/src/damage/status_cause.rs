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

//! Which status changes on a copy are the joining player's, by the hull rule
//! ([`super::cause`]): the same brackets and contexts ([`Ctx`]), reading
//! each copy's status variables ([`Vals`]) as well. Every game function that
//! writes a hit enemy's status is called from a bracketed hull writer.
//!
//! A change ([`Change`]): knockback (`pushed_x/y`), a slow (a drop of
//! `enemySpeed`), a flash (`canHitPulse` cleared), a mark (a rise of
//! `vulnerabilityMult`, `vulnerabilityCount`, `satFireHits` or `blitzed`).

#![forbid(unsafe_code)]

use std::collections::HashMap;

use super::cause::Ctx;

/// Changes below this are float noise.
const EPS: f64 = 1e-3;

/// The status variables read in a bracket, in this order.
pub const VARS: [&str; 8] = [
    "pushed_x",
    "pushed_y",
    "enemySpeed",
    "vulnerabilityMult",
    "vulnerabilityCount",
    "satFireHits",
    "blitzed",
    "canHitPulse",
];
pub const PUSH_X: usize = 0;
pub const PUSH_Y: usize = 1;
pub const SPEED: usize = 2;
/// The marks: [`VARS`]`[MARKS]`.
pub const MARKS: std::ops::Range<usize> = 3..7;
pub const PULSE: usize = 7;

/// One copy's status variables (a missing variable reads 0).
pub type Vals = [f64; 8];

/// A copy's status change in one context.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Change {
    pub push_x: f64,
    pub push_y: f64,
    /// `enemySpeed` taken off.
    pub slow: f64,
    pub flashes: u32,
    /// Which marks rose (in [`MARKS`] order).
    pub marks: [bool; 4],
}

impl Change {
    fn of(d: &Vals) -> Self {
        let mut marks = [false; 4];
        for (m, i) in marks.iter_mut().zip(MARKS) {
            *m = d[i] > EPS;
        }
        Self {
            push_x: if d[PUSH_X].abs() > EPS {
                d[PUSH_X]
            } else {
                0.0
            },
            push_y: if d[PUSH_Y].abs() > EPS {
                d[PUSH_Y]
            } else {
                0.0
            },
            slow: if d[SPEED] < -EPS { -d[SPEED] } else { 0.0 },
            flashes: u32::from(d[PULSE] < -0.5),
            marks,
        }
    }

    /// Nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.push_x == 0.0
            && self.push_y == 0.0
            && self.slow == 0.0
            && self.flashes == 0
            && !self.marks.contains(&true)
    }

    fn add(&mut self, o: &Self) {
        self.push_x += o.push_x;
        self.push_y += o.push_y;
        self.slow += o.slow;
        self.flashes += o.flashes;
        for (a, b) in self.marks.iter_mut().zip(o.marks) {
            *a |= b;
        }
    }
}

/// Counts per kind.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Kinds {
    pub pushes: u64,
    /// Sum of the knockback's lengths.
    pub push: f64,
    pub slows: u64,
    pub slow: f64,
    pub flashes: u64,
    pub mark_rises: u64,
}

impl Kinds {
    /// Count one change.
    pub fn add(&mut self, c: &Change) {
        if c.push_x != 0.0 || c.push_y != 0.0 {
            self.pushes += 1;
            self.push += c.push_x.hypot(c.push_y);
        }
        if c.slow > 0.0 {
            self.slows += 1;
            self.slow += c.slow;
        }
        self.flashes += u64::from(c.flashes);
        self.mark_rises += c.marks.iter().filter(|m| **m).count() as u64;
    }

    /// The counts as one line.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "pushes {} ({:.1}), slows {} ({:.2}), flashes {}, mark rises {}",
            self.pushes, self.push, self.slows, self.slow, self.flashes, self.mark_rises
        )
    }

    /// Whether every count here is at most `o`'s.
    #[must_use]
    pub fn within(&self, o: &Self) -> bool {
        self.pushes <= o.pushes
            && self.push <= o.push + EPS
            && self.slows <= o.slows
            && self.slow <= o.slow + EPS
            && self.flashes <= o.flashes
            && self.mark_rises <= o.mark_rises
    }
}

#[derive(Debug, Default)]
struct Frame {
    /// Per copy: the whole change nested brackets made (every context).
    nested: HashMap<usize, Vals>,
    /// Per copy: the mod's own writes inside this bracket.
    own: HashMap<usize, Vals>,
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct StatusCauses {
    stack: Vec<Frame>,
    /// Per copy: counted changes since its last tick.
    pending: HashMap<usize, Change>,
    /// Counted (by source) and host-owned changes.
    pub counted: Kinds,
    pub host: Kinds,
    by_source: HashMap<u16, u64>,
}

fn sub(a: &Vals, b: &Vals) -> Vals {
    let mut d = [0.0; 8];
    for i in 0..8 {
        d[i] = a[i] - b[i];
    }
    d
}

fn add_to(acc: &mut Vals, d: &Vals) {
    for i in 0..8 {
        acc[i] += d[i];
    }
}

impl StatusCauses {
    /// A new game frame (no bracket is open between frames).
    pub fn begin_frame(&mut self) {
        self.stack.clear();
    }

    /// A bracketed call begins.
    pub fn enter(&mut self) {
        self.stack.push(Frame::default());
    }

    /// Whether a bracket is open.
    #[must_use]
    pub fn open(&self) -> bool {
        !self.stack.is_empty()
    }

    /// The mod wrote `delta` to variable `var` of copy `key`.
    pub fn own_write(&mut self, key: usize, var: usize, delta: f64) {
        if let Some(top) = self.stack.last_mut()
            && var < 8
            && delta.abs() > EPS
        {
            top.own.entry(key).or_insert([0.0; 8])[var] += delta;
        }
    }

    /// A bracketed call of `src` in context `ctx` ended: `before` and
    /// `after` are the copies' status variables at its ends.
    pub fn leave(
        &mut self,
        src: u16,
        ctx: Ctx,
        before: &HashMap<usize, Vals>,
        after: &HashMap<usize, Vals>,
    ) {
        let child = self.stack.pop().unwrap_or_default();
        let mut up: Vec<(usize, Vals, Vals)> = Vec::new();
        for (&key, b) in before {
            let Some(a) = after.get(&key) else { continue };
            if !(a.iter().all(|v| v.is_finite()) && b.iter().all(|v| v.is_finite())) {
                continue;
            }
            let total = sub(a, b);
            let own = child.own.get(&key).copied().unwrap_or([0.0; 8]);
            let nested = child.nested.get(&key).copied().unwrap_or([0.0; 8]);
            let mine = sub(&sub(&total, &own), &nested);
            let c = Change::of(&mine);
            if !c.is_empty() {
                if ctx.counts() {
                    self.counted.add(&c);
                    *self.by_source.entry(src).or_default() += 1;
                    self.pending.entry(key).or_default().add(&c);
                } else {
                    self.host.add(&c);
                }
            }
            up.push((key, sub(&total, &own), own));
        }
        if let Some(parent) = self.stack.last_mut() {
            for (k, d, own) in up {
                add_to(parent.nested.entry(k).or_insert([0.0; 8]), &d);
                add_to(parent.own.entry(k).or_insert([0.0; 8]), &own);
            }
        }
    }

    /// A copy's tick: the counted changes since its last one.
    pub fn take(&mut self, key: usize) -> Change {
        self.pending.remove(&key).unwrap_or_default()
    }

    /// An instance left the game (its key may be reused).
    pub fn forget(&mut self, key: usize) {
        self.pending.remove(&key);
    }

    /// Counted changes per source, largest first.
    #[must_use]
    pub fn by_source(&self) -> Vec<(u16, u64)> {
        let mut v: Vec<(u16, u64)> = self.by_source.iter().map(|(k, n)| (*k, *n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COPY: usize = 0x100;

    fn vals(push_x: f64, speed: f64, vuln: f64, pulse: f64) -> Vals {
        [push_x, 0.0, speed, 0.0, vuln, 0.0, 0.0, pulse]
    }

    fn one(key: usize, v: Vals) -> HashMap<usize, Vals> {
        HashMap::from([(key, v)])
    }

    #[test]
    fn a_player_hit_counts_and_a_host_owned_blast_does_not() {
        let mut s = StatusCauses::default();
        // The player's shot: knockback, a flash, a mark.
        s.enter();
        s.leave(
            1,
            Ctx::Player,
            &one(COPY, vals(0.0, 1.0, 0.0, 1.0)),
            &one(COPY, vals(3.0, 1.0, 60.0, 0.0)),
        );
        // An enemy's blast pushing the same copy: the host's.
        s.enter();
        s.leave(
            2,
            Ctx::Host,
            &one(COPY, vals(3.0, 1.0, 60.0, 1.0)),
            &one(COPY, vals(8.0, 1.0, 60.0, 0.0)),
        );
        let c = s.take(COPY);
        assert_eq!((c.push_x, c.flashes), (3.0, 1));
        assert_eq!(c.marks, [false, true, false, false]);
        assert_eq!((s.counted.pushes, s.host.pushes, s.host.flashes), (1, 1, 1));
        assert!(s.take(COPY).is_empty(), "taken once");
        assert_eq!(s.by_source(), vec![(1, 1)]);
    }

    #[test]
    fn nesting_is_exclusive_both_ways() {
        let mut s = StatusCauses::default();
        // A player bracket around a host-owned one: the inner push is the
        // host's, the outer bracket's own slow is the player's.
        s.enter();
        s.enter();
        s.leave(
            2,
            Ctx::Host,
            &one(COPY, vals(0.0, 1.0, 0.0, 0.0)),
            &one(COPY, vals(5.0, 1.0, 0.0, 0.0)),
        );
        s.leave(
            1,
            Ctx::Player,
            &one(COPY, vals(0.0, 1.0, 0.0, 0.0)),
            &one(COPY, vals(5.0, 0.6, 0.0, 0.0)),
        );
        let c = s.take(COPY);
        assert!(c.push_x.abs() < 1e-9);
        assert!((c.slow - 0.4).abs() < 1e-9);
        assert_eq!((s.host.pushes, s.counted.slows), (1, 1));
        // A host bracket around a player one (a stun the player's shot set
        // off inside an enemy's event): the inner slow is the player's.
        s.enter();
        s.enter();
        s.leave(
            3,
            Ctx::Player,
            &one(COPY, vals(0.0, 1.0, 0.0, 0.0)),
            &one(COPY, vals(0.0, 0.5, 0.0, 0.0)),
        );
        s.leave(
            2,
            Ctx::Host,
            &one(COPY, vals(0.0, 1.0, 0.0, 0.0)),
            &one(COPY, vals(2.0, 0.5, 0.0, 0.0)),
        );
        let c = s.take(COPY);
        assert!((c.slow - 0.5).abs() < 1e-9 && c.push_x == 0.0);
        assert_eq!(s.host.pushes, 2);
    }

    #[test]
    fn the_mods_own_writes_and_unbracketed_changes_are_nobodys() {
        let mut s = StatusCauses::default();
        // The copy's tick zeroes its push inside a bracket: not a change.
        s.enter();
        s.own_write(COPY, PUSH_X, -4.0);
        s.leave(
            1,
            Ctx::Player,
            &one(COPY, vals(4.0, 1.0, 0.0, 0.0)),
            &one(COPY, vals(0.0, 1.0, 0.0, 0.0)),
        );
        assert!(s.take(COPY).is_empty());
        assert_eq!(s.counted, Kinds::default());
        // Outside every bracket nothing is recorded at all.
        s.own_write(COPY, PUSH_X, 1.0);
        assert!(!s.open());
        // A copy that is not in both reads (made or gone inside): skipped.
        s.enter();
        s.leave(
            1,
            Ctx::Player,
            &HashMap::new(),
            &one(COPY, vals(9.0, 1.0, 0.0, 0.0)),
        );
        assert!(s.take(COPY).is_empty());
    }

    #[test]
    fn reported_within_counted() {
        let mut counted = Kinds::default();
        let c = Change {
            push_x: 3.0,
            push_y: 4.0,
            slow: 0.5,
            flashes: 1,
            marks: [true, false, false, false],
        };
        counted.add(&c);
        assert!((counted.push - 5.0).abs() < 1e-9);
        let mut reported = Kinds::default();
        assert!(reported.within(&counted));
        reported.add(&c);
        assert!(reported.within(&counted));
        reported.add(&c);
        assert!(!reported.within(&counted));
        assert!(counted.line().starts_with("pushes 1 (5.0)"));
    }
}
