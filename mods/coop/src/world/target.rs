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

//! Which player each enemy goes after. The host decides and sends it in every
//! snapshot; each game enforces it only for remote players (pointed at their
//! avatar), leaving its own player to the game's `chooseTarget`.
//!
//! Rule: the nearest targetable player (visible, not cloaked, not downed).
//! A target that stops being targetable is dropped at once; otherwise the
//! enemy switches only to a player clearly closer ([`SWITCH_RATIO`]) after
//! holding its target [`MIN_HOLD_FRAMES`].

#![forbid(unsafe_code)]

use ndml_net::interp::Vec2;
use ndml_net::msg::PeerId;

/// Switch only to a player at most this fraction of the current distance.
pub const SWITCH_RATIO: f32 = 0.7;
/// Keep a target at least this many frames before switching to a closer one.
pub const MIN_HOLD_FRAMES: u64 = 90;

/// A player as targeting sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Player {
    /// The player's peer id.
    pub peer: PeerId,
    /// The ship's position.
    pub pos: Vec2,
    /// Visible, not cloaked, not downed.
    pub targetable: bool,
}

/// One enemy's current choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hold {
    current: Option<PeerId>,
    since: u64,
}

impl Hold {
    /// The current target.
    #[must_use]
    pub fn current(&self) -> Option<PeerId> {
        self.current
    }
}

/// The distance the game's own enemy code goes by (`point_distance`, not
/// across the battlefield's wrap).
fn dist(a: Vec2, b: Vec2) -> f32 {
    (b - a).len()
}

/// Update `hold` for an enemy at `at` in frame `frame` and return its target.
pub fn choose(hold: &mut Hold, at: Vec2, players: &[Player], frame: u64) -> Option<PeerId> {
    let nearest = players
        .iter()
        .filter(|p| p.targetable)
        .map(|p| (p.peer, dist(at, p.pos)))
        .min_by(|a, b| a.1.total_cmp(&b.1));
    let current = hold
        .current
        .and_then(|c| players.iter().find(|p| p.peer == c && p.targetable));
    let pick = match (current, nearest) {
        (_, None) => None,
        (None, Some((p, _))) => Some(p),
        (Some(cur), Some((p, d))) => {
            let kept_long = frame.saturating_sub(hold.since) >= MIN_HOLD_FRAMES;
            if p != cur.peer && kept_long && d < dist(at, cur.pos) * SWITCH_RATIO {
                Some(p)
            } else {
                Some(cur.peer)
            }
        }
    };
    if pick != hold.current {
        hold.current = pick;
        hold.since = frame;
    }
    pick
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PeerId = PeerId::HOST;
    const B: PeerId = PeerId(1);

    fn players(a: (f32, bool), b: (f32, bool)) -> Vec<Player> {
        vec![
            Player {
                peer: A,
                pos: Vec2::new(a.0, 0.0),
                targetable: a.1,
            },
            Player {
                peer: B,
                pos: Vec2::new(b.0, 0.0),
                targetable: b.1,
            },
        ]
    }

    #[test]
    fn nearest_first_then_hysteresis() {
        let mut h = Hold::default();
        let at = Vec2::new(0.0, 0.0);
        assert_eq!(
            choose(&mut h, at, &players((100.0, true), (300.0, true)), 0),
            Some(A)
        );
        // B a little closer: stays on A.
        assert_eq!(
            choose(&mut h, at, &players((100.0, true), (90.0, true)), 200),
            Some(A)
        );
        // B much closer but A only just picked: holds.
        let mut h2 = Hold::default();
        choose(&mut h2, at, &players((100.0, true), (300.0, true)), 0);
        assert_eq!(
            choose(&mut h2, at, &players((100.0, true), (10.0, true)), 10),
            Some(A)
        );
        // ... and switches once held long enough.
        assert_eq!(
            choose(&mut h2, at, &players((100.0, true), (10.0, true)), 95),
            Some(B)
        );
        assert_eq!(h2.current(), Some(B));
    }

    #[test]
    fn cloak_drops_at_once_and_returns_gradually() {
        let mut h = Hold::default();
        let at = Vec2::new(0.0, 0.0);
        choose(&mut h, at, &players((50.0, true), (400.0, true)), 0);
        // A cloaks: straight to B.
        assert_eq!(
            choose(&mut h, at, &players((50.0, false), (400.0, true)), 1),
            Some(B)
        );
        // A uncloaks right away: B is kept (hold time).
        assert_eq!(
            choose(&mut h, at, &players((50.0, true), (400.0, true)), 2),
            Some(B)
        );
        // Later A is far closer: back to A.
        assert_eq!(
            choose(&mut h, at, &players((50.0, true), (400.0, true)), 200),
            Some(A)
        );
        // Nobody targetable: nobody (the game's own fallback).
        assert_eq!(
            choose(&mut h, at, &players((50.0, false), (400.0, false)), 201),
            None
        );
        assert_eq!(
            choose(&mut h, at, &players((50.0, false), (400.0, true)), 202),
            Some(B)
        );
    }

    #[test]
    fn distances_are_the_games_own() {
        let mut h = Hold::default();
        // Enemy at x 980: A at 20 is 960 px away (the game does not measure
        // across the wrap), B at 700 is 280.
        let at = Vec2::new(980.0, 0.0);
        assert_eq!(
            choose(&mut h, at, &players((20.0, true), (700.0, true)), 0),
            Some(B)
        );
    }
}
