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

//! The link counts alone ([`Link`]) on random schedules: delays, stalls,
//! lost and duplicated statements, and start overs on one or both sides.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ndml_net::msg::{PeerId, RulesState};
use ndml_net::rng::Rng;

use super::super::Link;

/// A tick of the model is a frame.
fn at(tick: u64) -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now) + Duration::from_micros(tick * 16_667)
}

/// The self-check's window in the model, above the lane's longest delay.
const MODEL_WINDOW: Duration = Duration::from_millis(7_500);

/// A counted rules message on its way: when it was sent (the event clock).
#[derive(Debug, Clone, Copy)]
struct Msg {
    due: u64,
    sent_at: u64,
}

/// A statement on its way: to which side, when built.
#[derive(Debug, Clone)]
struct Stmt {
    due: u64,
    to: usize,
    built: u64,
    s: RulesState,
}

struct Side {
    link: Link,
    last: u16,
    /// The newest build time of a statement adopted on this link.
    adopted_built: Option<u64>,
    /// Statements that were current on this link so far (an adoption needs
    /// one before it: the grace).
    current_seen: u32,
}

impl Side {
    fn new() -> Self {
        let mut s = Self {
            link: Link::default(),
            last: 0,
            adopted_built: None,
            current_seen: 0,
        };
        s.restart();
        s
    }

    fn restart(&mut self) {
        self.last = self.last.wrapping_add(1).max(1);
        self.link = Link::new(self.last);
        self.adopted_built = None;
        self.current_seen = 0;
    }
}

struct World {
    rng: Rng,
    side: [Side; 2],
    /// The ordered lane from side i.
    lane: [Vec<Msg>; 2],
    stmts: Vec<Stmt>,
    clock: u64,
    tick: u64,
    /// Sends are lost until this tick (a link that is down).
    down_until: u64,
    /// A start over of this side due at this tick.
    restart_at: [Option<u64>; 2],
    adoptions: u32,
    skipped: u32,
    trace: Vec<String>,
    /// Sends as few as a game's (one in about 4 s a side) instead of
    /// several a second, and the self-check on.
    sparse: bool,
}

impl World {
    fn new(seed: u64) -> Self {
        Self {
            sparse: false,
            rng: Rng::new(seed),
            side: [Side::new(), Side::new()],
            lane: [Vec::new(), Vec::new()],
            stmts: Vec::new(),
            clock: 0,
            tick: 0,
            down_until: 0,
            restart_at: [None, None],
            adoptions: 0,
            skipped: 0,
            trace: Vec::new(),
        }
    }

    fn now(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn restart(&mut self, i: usize) {
        self.side[i].restart();
        self.trace.push(format!(
            "t{} restart {i} -> {:?}",
            self.tick, self.side[i].link
        ));
    }

    /// A start over of side `i` in `gap` ticks (or sooner, if one is due
    /// already): its tick.
    fn later(&mut self, i: usize, gap: u64) -> u64 {
        let at = self.restart_at[i].map_or(self.tick + gap, |t| t.min(self.tick + gap));
        self.restart_at[i] = Some(at);
        at
    }

    fn send(&mut self, i: usize) {
        if self.tick < self.down_until {
            return;
        }
        self.side[i].link.sent_one();
        let delay = if self.rng.chance(0.05) {
            self.rng.below(300)
        } else {
            self.rng.below(8)
        };
        let last = self.lane[i].last().map_or(0, |m| m.due);
        let sent_at = self.now();
        self.trace
            .push(format!("t{} {i} sends {sent_at}", self.tick));
        self.lane[i].push(Msg {
            due: (self.tick + delay).max(last),
            sent_at,
        });
    }

    fn state(&mut self, i: usize) {
        if self.tick < self.down_until || self.rng.chance(0.2) {
            return;
        }
        let mut s = blank();
        self.side[i].link.stamp(&mut s);
        let built = self.now();
        let copies = if self.rng.chance(0.1) { 2 } else { 1 };
        for _ in 0..copies {
            let delay = if self.rng.chance(0.1) {
                self.rng.below(400)
            } else {
                self.rng.below(20)
            };
            self.stmts.push(Stmt {
                due: self.tick + delay,
                to: 1 - i,
                built,
                s: s.clone(),
            });
        }
    }

    /// Side `to` takes the next message from the other if it is due.
    fn arrive(&mut self, to: usize, all: bool) -> bool {
        let from = 1 - to;
        let Some(m) = self.lane[from].first().copied() else {
            return false;
        };
        if !all && m.due > self.tick {
            return false;
        }
        self.lane[from].remove(0);
        self.trace
            .push(format!("t{} {to} gets msg sent {}", self.tick, m.sent_at));
        let side = &mut self.side[to];
        if !side.link.arrived() {
            self.skipped += 1;
            // Only a message sent before the statement adopted.
            assert!(
                side.adopted_built.is_some_and(|b| m.sent_at < b),
                "skipped a message sent at {} after the adopted statement ({:?})",
                m.sent_at,
                side.adopted_built
            );
        }
        true
    }

    fn hear(&mut self, st: &Stmt, adopt: bool) {
        let side = &mut self.side[st.to];
        let before = side.link.clone();
        if !side.link.heard(&st.s) {
            assert_eq!(side.link, before, "a stale statement changed nothing");
            return;
        }
        // `sent` moves down only on a link new to this side.
        if side.link.sent < before.sent {
            assert_ne!(
                st.s.link, before.seen,
                "sent lowered by a known link's statement"
            );
            assert_eq!(side.link.sent, st.s.ack);
        }
        if side.link.current(&st.s) {
            // Sparse: the self-check never fires.
            if self.sparse {
                let fired = side.link.lost_check(&st.s, at(self.tick), MODEL_WINDOW);
                assert_eq!(
                    fired,
                    None,
                    "the self-check fired with nothing lost\n{}",
                    self.trace[self.trace.len().saturating_sub(60)..].join("\n")
                );
            }
            let side = &mut self.side[st.to];
            side.current_seen += 1;
            // Never on the first statement after a start over.
            if adopt && side.current_seen > 1 {
                side.link.adopt(&st.s);
                side.adopted_built = side.adopted_built.max(Some(st.built));
                self.adoptions += 1;
            }
        }
        let now = format!("{:?}", self.side[st.to].link);
        self.trace.push(format!(
            "t{} {} hears seq {} ack {} link {} echo {} built {}: {now}",
            self.tick, st.to, st.s.seq, st.s.ack, st.s.link, st.s.echo, st.built
        ));
    }

    /// No side states more sent than the partner will have received.
    fn check(&self) {
        for x in 0..2 {
            let y = 1 - x;
            let (sx, sy) = (&self.side[x].link, &self.side[y].link);
            if sx.seen == sy.id {
                let flying = u32::try_from(self.lane[x].len()).unwrap_or(u32::MAX);
                assert!(
                    sx.sent + sy.skip <= sy.got + flying,
                    "side {x} states {} sent, side {y} got {} (+{} to skip) with {flying} on the way\n{}",
                    sx.sent,
                    sy.got,
                    sy.skip,
                    self.trace[self.trace.len().saturating_sub(60)..].join("\n")
                );
            }
        }
    }

    fn step(&mut self) {
        self.tick += 1;
        for i in 0..2 {
            if self.restart_at[i].is_some_and(|t| t <= self.tick) {
                self.restart_at[i] = None;
                self.restart(i);
            }
        }
        let r = self.rng.below(1000);
        if r < 4 {
            // The link wait on one side: the lane carries on.
            let i = usize::from(self.rng.chance(0.5));
            self.restart(i);
        } else if r < 7 {
            // Lost: what was in flight is gone, each side starts over, and
            // nothing gets through meanwhile.
            self.lane = [Vec::new(), Vec::new()];
            self.trace.push(format!("t{} LOST", self.tick));
            self.stmts.clear();
            let i = usize::from(self.rng.chance(0.5));
            self.restart(i);
            let gap = 1 + self.rng.below(60);
            let at = self.later(1 - i, gap);
            self.down_until = self.down_until.max(at + 1);
        } else if r < 9 {
            // A run's end on both, a moment apart; the lane carries on.
            let i = usize::from(self.rng.chance(0.5));
            self.restart(i);
            let gap = self.rng.below(90);
            self.later(1 - i, gap);
        }
        for i in 0..2 {
            if self.rng.chance(if self.sparse { 0.004 } else { 0.08 }) {
                self.send(i);
            }
            if self.tick % 15 == u64::try_from(i).unwrap_or(0) * 7 {
                self.state(i);
            }
        }
        for to in 0..2 {
            while self.arrive(to, false) {}
        }
        let tick = self.tick;
        let due: Vec<Stmt> = self.stmts.extract_if(.., |s| s.due <= tick).collect();
        for st in due {
            let adopt = self.rng.chance(0.3);
            self.hear(&st, adopt);
        }
        self.check();
    }

    /// Nothing more sent or started over; everything in flight arrives.
    fn settle(&mut self) {
        for i in 0..2 {
            if self.restart_at[i].take().is_some() {
                self.restart(i);
            }
        }
        for to in 0..2 {
            while self.arrive(to, true) {}
        }
        self.stmts.clear();
        self.check();
    }

    /// One statement from `i`, delivered at once.
    fn exchange(&mut self, i: usize) {
        let mut s = blank();
        self.side[i].link.stamp(&mut s);
        let built = self.now();
        self.hear(
            &Stmt {
                due: 0,
                to: 1 - i,
                built,
                s,
            },
            false,
        );
    }
}

fn blank() -> RulesState {
    RulesState {
        peer: PeerId::HOST,
        seq: 0,
        ack: 0,
        link: 0,
        echo: 0,
        flags: 0,
        holders: 0,
        round: 0,
        done: 0,
        chosen: 0,
        released: 0,
        downs: Vec::new(),
    }
}

#[test]
fn counts_hold_on_random_schedules() {
    let (mut adoptions, mut skipped) = (0, 0);
    for seed in 1..=3000 {
        let mut w = World::new(seed);
        for _ in 0..600 {
            w.step();
        }
        w.settle();
        // Nothing in flight, one statement each way: the counts agree
        // on both links, and the next statements are current.
        w.exchange(0);
        w.exchange(1);
        for x in 0..2 {
            let (sx, sy) = (&w.side[x].link, &w.side[1 - x].link);
            assert_eq!(
                sx.sent, sy.got,
                "seed {seed}: side {x}'s sent against the other's got"
            );
            assert_eq!(sy.skip, 0, "seed {seed}: nothing left to skip");
        }
        w.exchange(0);
        for x in 0..2 {
            let mut s = blank();
            w.side[x].link.stamp(&mut s);
            assert!(
                w.side[1 - x].link.current(&s),
                "seed {seed}: side {x}'s statement is current"
            );
        }
        adoptions += w.adoptions;
        skipped += w.skipped;
    }
    // The schedules did exercise adoptions and drops.
    assert!(
        adoptions > 1000 && skipped > 100,
        "{adoptions} adoptions, {skipped} skipped"
    );
}

/// This side starts over, the partner does not, and the partner's statement
/// arrives before messages it counts: the counts still end equal.
#[test]
fn a_start_over_here_with_the_partners_statement_ahead_of_its_messages() {
    let mut me = Side::new();
    let mut it = Side::new();
    // Consistent so far: it sent 10, 8 arrived, 2 on their way.
    for _ in 0..10 {
        it.link.sent_one();
    }
    for _ in 0..8 {
        assert!(me.link.arrived());
    }
    let mut s = blank();
    me.link.stamp(&mut s);
    assert!(it.link.heard(&s));
    me.restart();
    // Its statement first: it counts on this side's old link.
    let mut s = blank();
    it.link.stamp(&mut s);
    assert!(me.link.heard(&s));
    assert!(!me.link.current(&s), "counts on this side's old link");
    assert_eq!(me.link.got, 0, "got is not taken from the statement");
    // This side's statement goes before the two arrive: its sent takes 0.
    let mut ours = blank();
    me.link.stamp(&mut ours);
    assert!(it.link.heard(&ours));
    assert_eq!(it.link.sent, 0);
    // Then the two arrive: this got runs ahead of its sent, and its
    // statements read as old (nothing adopted from them).
    assert!(me.link.arrived() && me.link.arrived());
    let mut s = blank();
    it.link.stamp(&mut s);
    assert!(me.link.heard(&s));
    assert!(!me.link.current(&s), "behind what arrived: old");
    // The next statement's ack raises its sent: equal again.
    let mut ours = blank();
    me.link.stamp(&mut ours);
    assert!(it.link.heard(&ours));
    assert_eq!((it.link.sent, me.link.got), (2, 2));
    let mut s = blank();
    it.link.stamp(&mut s);
    assert!(me.link.heard(&s) && me.link.current(&s));
    // Its next message is counted, never skipped.
    it.link.sent_one();
    assert!(me.link.arrived());
    assert_eq!(it.link.sent, me.link.got);
}

/// A statement from the partner's earlier link, late, changes nothing.
#[test]
fn a_statement_of_an_earlier_link_is_ignored() {
    let mut me = Side::new();
    let mut it = Side::new();
    let mut old = blank();
    it.link.sent_one();
    it.link.stamp(&mut old);
    it.restart();
    let mut new = blank();
    it.link.stamp(&mut new);
    assert!(me.link.heard(&new));
    let before = me.link.clone();
    assert!(!me.link.heard(&old));
    assert_eq!(me.link, before);
}

/// One rules message lost with the connection up: after the self-check's
/// window it is written off and later messages count again.
#[test]
fn a_message_lost_with_the_connection_up_is_written_off_after_the_window() {
    let mut me = Side::new();
    let mut it = Side::new();
    let window = Duration::from_secs(30);
    let stmt = |l: &Link| {
        let mut s = blank();
        l.stamp(&mut s);
        s
    };
    // Linked both ways.
    assert!(it.link.heard(&stmt(&me.link)));
    assert!(me.link.heard(&stmt(&it.link)));
    // Three sent; the second never arrives.
    for _ in 0..3 {
        it.link.sent_one();
    }
    assert!(me.link.arrived() && me.link.arrived());
    let mut t = 0;
    let s = stmt(&it.link);
    assert!(me.link.heard(&s) && me.link.current(&s));
    assert_eq!(me.link.lost_check(&s, at(t), window), None);
    // The rules differ: adopted after the grace; the next real one is
    // dropped as covered.
    t += 180;
    me.link.adopt(&s);
    it.link.sent_one();
    assert!(!me.link.arrived(), "a real message dropped");
    assert_eq!(me.link.got + 1, it.link.sent, "the offset stays");
    // Statements four a second: under the window nothing changes.
    while t + 15 < 30 * 60 {
        t += 15;
        let s = stmt(&it.link);
        assert!(me.link.heard(&s) && me.link.current(&s));
        assert!(it.link.heard(&stmt(&me.link)));
        assert_eq!(me.link.lost_check(&s, at(t), window), None, "t{t}");
    }
    // Past it: one written off; the counts agree.
    t += 15;
    let s = stmt(&it.link);
    assert!(me.link.heard(&s));
    assert_eq!(me.link.lost_check(&s, at(t), window), Some(1));
    assert_eq!((me.link.got, me.link.skip), (it.link.sent, 0));
    assert!(it.link.heard(&stmt(&me.link)));
    // Later messages count; nothing more is written off.
    for _ in 0..3 {
        it.link.sent_one();
        assert!(me.link.arrived());
    }
    for k in 1..=200 {
        let s = stmt(&it.link);
        assert!(me.link.heard(&s) && me.link.current(&s));
        assert_eq!(me.link.lost_check(&s, at(t + 15 * k), window), None);
    }
    assert_eq!(me.link.got, it.link.sent);
}

/// A stall shorter than the window writes nothing off.
#[test]
fn a_stall_under_the_window_writes_nothing_off() {
    let mut me = Side::new();
    let mut it = Side::new();
    let window = Duration::from_secs(30);
    let mut s = blank();
    me.link.stamp(&mut s);
    assert!(it.link.heard(&s));
    it.link.sent_one();
    it.link.sent_one();
    // 25 s of statements with both still on their way.
    for k in 0..100 {
        let mut s = blank();
        it.link.stamp(&mut s);
        assert!(me.link.heard(&s) && me.link.current(&s));
        assert_eq!(me.link.lost_check(&s, at(15 * k), window), None);
    }
    assert!(me.link.arrived() && me.link.arrived());
    let mut s = blank();
    it.link.stamp(&mut s);
    assert!(me.link.heard(&s));
    assert_eq!(me.link.lost_check(&s, at(1500), window), None);
    // A new stall starts its own window.
    it.link.sent_one();
    for k in 0..100 {
        let mut s = blank();
        it.link.stamp(&mut s);
        assert!(me.link.heard(&s));
        assert_eq!(me.link.lost_check(&s, at(1515 + 15 * k), window), None);
    }
    assert_eq!(me.link.got, 2);
}

/// With messages as few as in a game the self-check never fires.
#[test]
fn the_self_check_stays_quiet_on_sparse_random_schedules() {
    for seed in 1..=1000 {
        let mut w = World::new(seed);
        w.sparse = true;
        for _ in 0..3600 {
            w.step();
        }
        w.settle();
        w.exchange(0);
        w.exchange(1);
        for x in 0..2 {
            let (sx, sy) = (&w.side[x].link, &w.side[1 - x].link);
            assert_eq!(sx.sent, sy.got, "seed {seed}: side {x}");
            assert_eq!(sy.skip, 0, "seed {seed}");
        }
    }
}
