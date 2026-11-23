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

//! Copies their own code turned or placed after their End Step tick, put
//! back before the draw: the pure plan.
//!
//! * A copy's angle goes back to its tick's (the host's).
//! * A part placed after its tick from its leader's angle is turned about
//!   its leader by the leader's correction; leaders go first.
//! * A part moved but not turned after its tick, on a leader neither moved
//!   nor turned, rides the leader: its tick offset from the leader's tick a
//!   frame before (`part.tick(N) - leader.tick(N-1)`), carried a frame on,
//!   put on the leader as drawn. Never on an extrapolated or first tick,
//!   across a jump, for a part that ever turned after its tick, or for one
//!   ticked at the run step (see [`Stayed`]).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::hash::BuildHasher;

use ndml_net::interp::angle_delta;

/// A copy turned after its tick by more than this (degrees) is put back.
pub const ANGLE_EPS: f32 = 0.01;
/// A part moved after its tick by more than this (px) was placed by its own
/// code then (only such a part is carried with its leader).
pub const POS_EPS: f32 = 0.01;
/// A tick pose this far from the copy's own a frame before (px) is a jump
/// (a wrap, a blink), not motion: no ride across it.
pub const JUMP_PX: f32 = 200.0;
/// Leader hops followed: the guard against a loop of leaders (a Glaucus's
/// body is a chain of up to 30 segments, its fins and fingers hang off it).
const MAX_DEPTH: usize = 256;

/// A pose: x, y, angle (degrees, the game's).
pub type Pose = (f32, f32, f32);

/// One copy at the draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seen {
    /// Instance key.
    pub key: usize,
    pub obj: u32,
    /// The pose its tick gave it (the host's).
    pub tick: Pose,
    /// Its pose now, before the put-back.
    pub now: Pose,
    /// Its leader, when that is a copy ticked this frame.
    pub leader: Option<usize>,
    /// Its tick pose is the host's last sample carried on, not one between
    /// two of the host's.
    pub extrapolated: bool,
    /// Its tick ran at its End Step (a copy's), not at the run step before
    /// the Steps (a dependency's: a Star Eater leg, placed in its own Step).
    pub at_end_step: bool,
}

/// This frame's plan ([`plan`]).
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The put-backs to write.
    pub writes: Vec<Write>,
    /// Every copy's pose as drawn.
    pub drawn: HashMap<usize, Pose>,
    /// The parts that rode their leader's last tick.
    pub rode: Vec<usize>,
    /// The parts their own code placed after their tick that did not ride,
    /// and why.
    pub stayed: Vec<(usize, Stayed)>,
    /// Every copy's frame, for the next frame's plan.
    pub last: HashMap<usize, Last>,
}

/// Why a part its own code placed after its tick did not ride its leader's
/// last tick ([`plan`]): its own placement, or its leader's carry, stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stayed {
    /// Its leader's own code moved or turned it after its tick.
    LeaderPlaced = 0,
    /// Its own code turned it after its tick (this frame or one before).
    Turned = 1,
    /// Ticked at the run step (a dependency).
    RunStep = 2,
    /// Its tick pose or its leader's is extrapolated, now or last frame.
    Extrapolated = 3,
    /// No tick of it or its leader last frame.
    NoPrev = 4,
    /// It or its leader jumped since last frame.
    Jump = 5,
}

/// What a copy is given: a position, an angle, or both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Write {
    /// Instance key.
    pub key: usize,
    /// Position to write.
    pub pos: Option<(f32, f32)>,
    /// Angle to write (degrees).
    pub angle: Option<f32>,
}

/// `v` turned by `deg` as the game turns (angles anticlockwise on a screen
/// whose y grows down: `lengthdir(l, a)` turned by `d` is `lengthdir(l, a + d)`).
#[must_use]
pub fn turn(v: (f32, f32), deg: f32) -> (f32, f32) {
    let (s, c) = deg.to_radians().sin_cos();
    (v.0 * c + v.1 * s, -v.0 * s + v.1 * c)
}

fn moved(a: Pose, b: Pose) -> f32 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// A copy's last frame, for the next frame's ride ([`plan`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Last {
    /// Its tick pose.
    pub pose: Pose,
    /// That pose was extrapolated.
    pub extrapolated: bool,
    /// Its own code turned it after its tick in that frame or one before
    /// (over the frames it has been seen in a row).
    pub turned: bool,
    /// Its tick offset from its leader's tick the frame before (leader key,
    /// offset); `None` without that tick or when either was extrapolated.
    pub rel: Option<(usize, (f32, f32))>,
}

fn turned_after_tick(c: &Seen) -> bool {
    angle_delta(c.tick.2, c.now.2).abs() > ANGLE_EPS
}

/// The writes for this frame's copies, given their last frames by key
/// (`prev`); the plan's `last` is this frame's, for the next.
#[must_use]
pub fn plan<S: BuildHasher>(copies: &[Seen], prev: &HashMap<usize, Last, S>) -> Plan {
    let by_key: HashMap<usize, &Seen> = copies.iter().map(|c| (c.key, c)).collect();
    let mut drawn: HashMap<usize, Pose> = copies.iter().map(|c| (c.key, c.now)).collect();
    // Every copy whose pose changes: (pose before, pose after).
    let mut changed: HashMap<usize, (Pose, Pose)> = HashMap::new();
    for c in copies {
        if turned_after_tick(c) {
            let after = (c.now.0, c.now.1, c.tick.2);
            drawn.insert(c.key, after);
            changed.insert(c.key, (c.now, after));
        }
    }
    // Leaders before their parts: by the number of leader hops to a copy
    // with no leader among these.
    let depth = |c: &Seen| {
        let mut d = 0;
        let mut at = c.leader;
        while let Some(l) = at {
            d += 1;
            if d > MAX_DEPTH {
                break;
            }
            at = by_key.get(&l).and_then(|l| l.leader);
        }
        d
    };
    let mut order: Vec<(usize, &Seen)> = copies
        .iter()
        .filter(|c| c.leader.is_some())
        .map(|c| (depth(c), c))
        .filter(|(d, _)| *d <= MAX_DEPTH)
        .collect();
    order.sort_by_key(|(d, c)| (*d, c.key));
    let mut rode = Vec::new();
    let mut stayed = Vec::new();
    for (_, c) in order {
        let placed = moved(c.now, c.tick) > POS_EPS;
        if !placed {
            continue;
        }
        if let Some(leader) = c.leader.and_then(|l| by_key.get(&l)) {
            match ride(c, leader, &drawn, prev) {
                Ok(pos) => {
                    let after = (pos.0, pos.1, drawn.get(&c.key).map_or(c.now.2, |p| p.2));
                    drawn.insert(c.key, after);
                    changed.insert(c.key, (c.now, after));
                    rode.push(c.key);
                    continue;
                }
                Err(why) => stayed.push((c.key, why)),
            }
        }
        let Some((lb, la)) = c.leader.and_then(|l| changed.get(&l)).copied() else {
            continue;
        };
        let v = turn((c.now.0 - lb.0, c.now.1 - lb.1), angle_delta(lb.2, la.2));
        let pos = (la.0 + v.0, la.1 + v.1);
        let angle = drawn.get(&c.key).map_or(c.now.2, |p| p.2);
        let after = (pos.0, pos.1, angle);
        drawn.insert(c.key, after);
        changed.insert(c.key, (c.now, after));
    }
    let writes = changed
        .iter()
        .map(|(k, (before, after))| Write {
            key: *k,
            pos: ((after.0 - before.0).hypot(after.1 - before.1) > POS_EPS)
                .then_some((after.0, after.1)),
            angle: (angle_delta(before.2, after.2).abs() > ANGLE_EPS).then_some(after.2),
        })
        .collect();
    let last = copies
        .iter()
        .map(|c| {
            let before = prev.get(&c.key).is_some_and(|l| l.turned);
            let rel = c
                .leader
                .and_then(|k| prev.get(&k).map(|l| (k, l)))
                .filter(|(_, l)| !l.extrapolated && !c.extrapolated)
                .map(|(k, l)| (k, (c.tick.0 - l.pose.0, c.tick.1 - l.pose.1)));
            let l = Last {
                pose: c.tick,
                extrapolated: c.extrapolated,
                turned: before || turned_after_tick(c),
                rel,
            };
            (c.key, l)
        })
        .collect();
    Plan {
        writes,
        drawn,
        rode,
        stayed,
        last,
    }
}

/// Where a part placed after its tick rides its leader (see the module
/// docs), or why its own placement stands.
fn ride<S: BuildHasher>(
    c: &Seen,
    leader: &Seen,
    drawn: &HashMap<usize, Pose>,
    prev: &HashMap<usize, Last, S>,
) -> Result<(f32, f32), Stayed> {
    // A leader placed in its own End Step (a fin): the part is carried instead.
    let leader_placed = moved(leader.now, leader.tick) > POS_EPS
        || angle_delta(leader.tick.2, leader.now.2).abs() > ANGLE_EPS;
    // A part turned after its tick is placed from its leader's angle: its
    // own placement already sits on the leader as drawn.
    let turned = turned_after_tick(c);
    if leader_placed {
        return Err(Stayed::LeaderPlaced);
    }
    if turned {
        return Err(Stayed::Turned);
    }
    if !c.at_end_step {
        return Err(Stayed::RunStep);
    }
    if c.extrapolated || leader.extrapolated {
        return Err(Stayed::Extrapolated);
    }
    let (Some(l), Some(own)) = (prev.get(&leader.key), prev.get(&c.key)) else {
        return Err(Stayed::NoPrev);
    };
    if own.turned {
        return Err(Stayed::Turned);
    }
    if l.extrapolated || own.extrapolated {
        return Err(Stayed::Extrapolated);
    }
    let (lp, cp) = (l.pose, own.pose);
    if moved(leader.tick, lp) > JUMP_PX || moved(c.tick, cp) > JUMP_PX {
        return Err(Stayed::Jump);
    }
    let la = drawn.get(&leader.key).copied().unwrap_or(leader.now);
    let now = (c.tick.0 - lp.0, c.tick.1 - lp.1);
    // The offset a frame on (the host draws the part where it ticks it next);
    // with no offset the frame before, the last one turned by the leader's turn.
    let then = own
        .rel
        .filter(|(k, r)| *k == leader.key && (now.0 - r.0).hypot(now.1 - r.1) <= JUMP_PX)
        .map(|(_, r)| r);
    let v = then.map_or_else(
        || turn(now, angle_delta(lp.2, la.2)),
        |r| (2.0 * now.0 - r.0, 2.0 * now.1 - r.1),
    );
    let pos = (la.0 + v.0, la.1 + v.1);
    if pos.0.is_finite() && pos.1.is_finite() {
        Ok(pos)
    } else {
        Err(Stayed::Jump)
    }
}

/// How far a part is from where it sits on its leader by the host's ticks:
/// its offset from its leader in the leader's frame, against the same from
/// the tick poses (px). `None` for a copy without a leader here.
#[cfg(test)]
fn off_leader(c: &Seen, leader: &Seen, part: Pose, lead: Pose) -> Option<f32> {
    let want = turn(
        (c.tick.0 - leader.tick.0, c.tick.1 - leader.tick.1),
        -leader.tick.2,
    );
    let got = turn((part.0 - lead.0, part.1 - lead.1), -lead.2);
    let d = (got.0 - want.0).hypot(got.1 - want.1);
    d.is_finite().then_some(d)
}

/// A copy's pose with only its angle put back (parts not carried with
/// their leaders): its position now, its angle as drawn.
#[cfg(test)]
fn angles_only(c: &Seen, drawn: Option<&Pose>) -> Pose {
    (c.now.0, c.now.1, drawn.map_or(c.now.2, |p| p.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last(pose: Pose, extrapolated: bool) -> Last {
        Last {
            pose,
            extrapolated,
            turned: false,
            rel: None,
        }
    }

    fn lengthdir(l: f32, a: f32) -> (f32, f32) {
        let r = a.to_radians();
        (l * r.cos(), -l * r.sin())
    }

    fn near(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).hypot(a.1 - b.1) < 1e-3
    }

    #[test]
    fn a_turn_is_the_games_lengthdir_turned() {
        for (l, a, d) in [(10.0, 0.0, 90.0), (73.5, 125.0, -40.0), (3.0, 350.0, 25.0)] {
            assert!(near(turn(lengthdir(l, a), d), lengthdir(l, a + d)));
        }
    }

    /// A fin turned back carries its finger to where the corrected angle
    /// places it; other parts are left alone.
    #[test]
    fn a_finger_goes_with_its_fin() {
        let fin_at = (500.0, 300.0);
        let place = |fin_angle: f32| {
            let tip = lengthdir(109.9, fin_angle);
            let off = lengthdir(73.5, fin_angle - 40.0);
            (fin_at.0 + tip.0 + off.0, fin_at.1 + tip.1 + off.1)
        };
        let (host, local) = (30.0, 52.0);
        let fin = Seen {
            key: 1,
            obj: 479,
            tick: (fin_at.0, fin_at.1, host),
            now: (fin_at.0, fin_at.1, local),
            leader: Some(9),
            extrapolated: false,
            at_end_step: true,
        };
        let p = place(local);
        let h = place(host);
        let finger = Seen {
            key: 2,
            obj: 480,
            tick: (h.0 + 3.0, h.1, host - 40.0),
            now: (p.0, p.1, local - 40.0),
            leader: Some(1),
            extrapolated: false,
            at_end_step: true,
        };
        // A leg on a leader that was not put back: placed after its tick.
        let seg = Seen {
            key: 3,
            obj: 777,
            tick: (100.0, 100.0, 10.0),
            now: (100.0, 100.0, 10.0),
            leader: None,
            extrapolated: false,
            at_end_step: true,
        };
        let leg = Seen {
            key: 4,
            obj: 775,
            tick: (140.0, 100.0, 50.0),
            now: (141.0, 100.0, 50.0),
            leader: Some(3),
            extrapolated: false,
            at_end_step: true,
        };
        // A drone on a turned-back leader that its own code did not move.
        let drone = Seen {
            key: 5,
            obj: 1,
            tick: (520.0, 310.0, 0.0),
            now: (520.0, 310.0, 0.0),
            leader: Some(1),
            extrapolated: false,
            at_end_step: true,
        };
        // The finger listed first: leaders still go first.
        let copies = [finger, leg, drone, fin, seg];
        let Plan {
            writes,
            drawn,
            rode,
            ..
        } = plan(&copies, &HashMap::new());
        assert!(rode.is_empty());
        assert!((drawn[&1].2 - host).abs() < 1e-4);
        assert!(
            near((drawn[&2].0, drawn[&2].1), h),
            "{:?} vs {h:?}",
            drawn[&2]
        );
        assert!((drawn[&2].2 - (host - 40.0)).abs() < 1e-4);
        assert_eq!(drawn[&4], leg.now);
        assert_eq!(drawn[&5], drone.now);
        assert!(writes.iter().all(|w| [1, 2].contains(&w.key)));
        // On its fin as the copy's own code placed both (3 px: the finger's
        // tick pose is 3 px off the host's placement, which stays); off it
        // by the fin's correction with only the angles put back; on it as
        // drawn.
        let own = off_leader(&finger, &fin, finger.now, fin.now).unwrap();
        let angles_only = off_leader(
            &finger,
            &fin,
            angles_only(&finger, drawn.get(&2)),
            angles_only(&fin, drawn.get(&1)),
        )
        .unwrap();
        let after = off_leader(&finger, &fin, drawn[&2], drawn[&1]).unwrap();
        assert!((own - 3.0).abs() < 0.05, "{own}");
        assert!(angles_only > 50.0, "{angles_only}");
        assert!((after - 3.0).abs() < 0.05, "{after}");
    }

    /// A finger on a fin on the 30th segment of a body chain is carried
    /// too: the order comes from the hops, however many.
    #[test]
    fn a_long_chain_still_puts_leaders_first() {
        let mut copies = Vec::new();
        for k in 0..30u16 {
            let x = f32::from(k) * 10.0;
            copies.push(Seen {
                key: usize::from(k),
                obj: 510,
                tick: (x, 0.0, 0.0),
                now: (x, 0.0, 0.0),
                leader: usize::from(k).checked_sub(1),
                extrapolated: false,
                at_end_step: true,
            });
        }
        copies.push(Seen {
            key: 100,
            obj: 479,
            tick: (290.0, 0.0, 30.0),
            now: (290.0, 0.0, 50.0),
            leader: Some(29),
            extrapolated: false,
            at_end_step: true,
        });
        let p = lengthdir(100.0, 50.0);
        copies.push(Seen {
            key: 101,
            obj: 480,
            tick: (290.0, 0.0, 0.0),
            now: (290.0 + p.0, p.1, 0.0),
            leader: Some(100),
            extrapolated: false,
            at_end_step: true,
        });
        copies.reverse();
        let drawn = plan(&copies, &HashMap::new()).drawn;
        let want = lengthdir(100.0, 30.0);
        assert!(near(
            (drawn[&101].0, drawn[&101].1),
            (290.0 + want.0, want.1)
        ));
    }

    /// A drone, its leader and their last frames: the host placed the drone
    /// last frame 300 px out at 40 degrees (`orbit`) from the leader as it
    /// was then; the copy's own orbit is 1400 px out at 200.
    fn drone_case() -> (Seen, Seen, HashMap<usize, Last>, (f32, f32)) {
        let lead_prev = (495.0, 300.0, 0.0);
        let lead = Seen {
            key: 1,
            obj: 382,
            tick: (500.0, 300.0, 10.0),
            now: (500.0, 300.0, 10.0),
            leader: None,
            extrapolated: false,
            at_end_step: true,
        };
        let orbit = lengthdir(300.0, 40.0);
        let host_at = (lead_prev.0 + orbit.0, lead_prev.1 + orbit.1);
        let own_orbit = lengthdir(1400.0, 200.0);
        let drone = Seen {
            key: 2,
            obj: 385,
            tick: (host_at.0, host_at.1, 0.0),
            now: (500.0 + own_orbit.0, 300.0 + own_orbit.1, 0.0),
            leader: Some(1),
            extrapolated: false,
            at_end_step: true,
        };
        let prev: HashMap<usize, Last> = [
            (1, last(lead_prev, false)),
            (2, last((host_at.0 - 3.0, host_at.1, 0.0), false)),
        ]
        .into();
        (lead, drone, prev, orbit)
    }

    /// An orbiting drone rides its leader's last tick; its own placement
    /// stands on an extrapolated tick, with no last tick, or across a jump;
    /// a part on a turned leader is carried, never ridden.
    #[test]
    fn a_drone_rides_its_leaders_last_tick() {
        let (lead, drone, prev, _) = drone_case();
        let lead_prev = prev[&1].pose;
        let host_at = (drone.tick.0, drone.tick.1);
        let p = plan(&[drone, lead], &prev);
        assert_eq!(p.rode, vec![2]);
        let want = lengthdir(300.0, 50.0);
        let got = p.drawn[&2];
        assert!(
            near((got.0, got.1), (500.0 + want.0, 300.0 + want.1)),
            "{got:?}"
        );
        assert!(p.writes.iter().any(|w| w.key == 2 && w.pos.is_some()));
        assert!(!p.writes.iter().any(|w| w.key == 1));
        // Its own placement: extrapolated, no tick last frame, a jump, a
        // guessed tick last frame.
        assert!(p.stayed.is_empty());
        let own = |p: &Plan, why: Stayed| {
            p.rode.is_empty() && p.drawn[&2] == drone.now && p.stayed == vec![(2, why)]
        };
        let guessed = Seen {
            extrapolated: true,
            ..drone
        };
        assert!(own(&plan(&[guessed, lead], &prev), Stayed::Extrapolated));
        let lead_only: HashMap<usize, Last> = [(1, last(lead_prev, false))].into();
        assert!(own(&plan(&[drone, lead], &lead_only), Stayed::NoPrev));
        let wrapped: HashMap<usize, Last> = [
            (1, last((lead_prev.0 + 3000.0, lead_prev.1, 0.0), false)),
            (2, last((host_at.0, host_at.1, 0.0), false)),
        ]
        .into();
        assert!(own(&plan(&[drone, lead], &wrapped), Stayed::Jump));
        let guessed_last: HashMap<usize, Last> = [
            (1, last(lead_prev, true)),
            (2, last((host_at.0, host_at.1, 0.0), false)),
        ]
        .into();
        assert!(own(
            &plan(&[drone, lead], &guessed_last),
            Stayed::Extrapolated
        ));
        // A part its own code also turned after its tick, or one ticked at
        // the run step (a dependency, placed in its own Step): its own.
        let turned = Seen {
            now: (drone.now.0, drone.now.1, 30.0),
            ..drone
        };
        let p = plan(&[turned, lead], &prev);
        assert!(p.rode.is_empty() && (p.drawn[&2].0, p.drawn[&2].1) == (drone.now.0, drone.now.1));
        assert_eq!(p.stayed, vec![(2, Stayed::Turned)]);
        // Turned once, it never rides, in a frame its leader happened not
        // to turn it (a fin); the memory goes on from frame to frame.
        assert!(p.last[&2].turned && !p.last[&1].turned);
        let next = plan(&[drone, lead], &p.last);
        assert_eq!(next.stayed, vec![(2, Stayed::Turned)]);
        assert!(next.last[&2].turned);
        assert!(!plan(&[drone, lead], &prev).last[&2].turned);
        let dependency = Seen {
            at_end_step: false,
            ..drone
        };
        assert!(own(&plan(&[dependency, lead], &prev), Stayed::RunStep));
        // A leader its own code turned after its tick (a fin): carried.
        let fin = Seen {
            now: (500.0, 300.0, 25.0),
            ..lead
        };
        let p = plan(&[drone, fin], &prev);
        assert!(p.rode.is_empty());
        assert_eq!(p.stayed, vec![(2, Stayed::LeaderPlaced)]);
        let carried = turn((drone.now.0 - 500.0, drone.now.1 - 300.0), -15.0);
        assert!(near(
            (p.drawn[&2].0, p.drawn[&2].1),
            (500.0 + carried.0, 300.0 + carried.1)
        ));
    }

    /// With its offset from the leader a frame before known, the drone's
    /// offset is carried a frame on by its last step (the host draws it
    /// where it ticks it next), the leader's turn left out; an offset from
    /// another leader is no step.
    #[test]
    fn a_rider_is_carried_a_frame_on() {
        let (lead, drone, mut prev, orbit) = drone_case();
        let turned = plan(&[drone, lead], &prev).drawn[&2];
        let before = lengthdir(300.0, 36.0);
        prev.get_mut(&2).unwrap().rel = Some((1, before));
        let p = plan(&[drone, lead], &prev);
        let want = (2.0 * orbit.0 - before.0, 2.0 * orbit.1 - before.1);
        let got = p.drawn[&2];
        assert!(
            near((got.0, got.1), (500.0 + want.0, 300.0 + want.1)),
            "{got:?}"
        );
        // The offset it leaves for the next frame.
        let (k, rel) = p.last[&2].rel.unwrap();
        assert!(k == 1 && near(rel, orbit), "{rel:?}");
        prev.get_mut(&2).unwrap().rel = Some((7, before));
        let other = plan(&[drone, lead], &prev).drawn[&2];
        assert!(near((other.0, other.1), (turned.0, turned.1)), "{other:?}");
    }
}
