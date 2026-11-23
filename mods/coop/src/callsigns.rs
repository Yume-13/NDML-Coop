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

//! Automatic player names for `player_name = "Random"` (codes and LAN). Two
//! are picked per launch: the name, and an alternative for a joining game
//! whose host has the first (`LobbyConfig::name_alt`).

#![forbid(unsafe_code)]

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Pilot call signs: short, plain words.
pub const CALL_SIGNS: &[&str] = &[
    "Pilot", "Ace", "Comet", "Vector", "Kestrel", "Heron", "Raven", "Hawk", "Lark", "Wren",
    "Finch", "Tern", "Petrel", "Condor", "Talon", "Quasar", "Meteor", "Photon", "Flare", "Blaze",
    "Ember", "Cinder", "Spark", "Dart", "Arrow", "Ranger", "Scout", "Rookie", "Saber", "Glider",
    "Kite", "Beacon", "Vortex", "Shard", "Tempest", "Cyclone", "Gale", "Breeze", "Zephyr",
    "Halcyon", "Sable", "Frost", "Dusk", "Flint", "Quartz", "Cobalt", "Onyx", "Indigo", "Rigel",
    "Orion", "Starling", "Gannet",
];

/// This launch's two different picks, kept until the game closes.
#[must_use]
pub fn launch_picks() -> (&'static str, &'static str) {
    static PICKS: OnceLock<(usize, usize)> = OnceLock::new();
    let &(a, b) = PICKS.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let seed = u64::from(nanos) ^ (u64::from(std::process::id()) << 32);
        picks_from(seed)
    });
    (CALL_SIGNS[a], CALL_SIGNS[b])
}

/// Two different indices into [`CALL_SIGNS`] from `seed` (splitmix64).
fn picks_from(seed: u64) -> (usize, usize) {
    let mut s = seed;
    let mut next = || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let n = CALL_SIGNS.len() as u64;
    let a = next() % n;
    let b = (a + 1 + next() % (n - 1)) % n;
    // Both are below the list's length, so the casts are exact.
    (a as usize, b as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_net::lobby::files::{MAX_NAME_CHARS, clean_name};

    /// Every call sign is a clean, short, unique name, never "Random".
    #[test]
    fn every_call_sign_is_a_clean_short_name_of_its_own() {
        assert!(CALL_SIGNS.len() >= 30);
        let mut seen = std::collections::HashSet::new();
        for &n in CALL_SIGNS {
            assert_eq!(clean_name(n).as_deref(), Some(n), "{n}");
            assert!(
                n.chars().count() <= MAX_NAME_CHARS.min(crate::ui::text::NAME_MAX),
                "{n}"
            );
            assert!(
                n.chars().count() <= 10,
                "short enough for the ship label: {n}"
            );
            assert!(n.chars().all(|c| c.is_ascii_alphabetic()), "{n}");
            assert!(seen.insert(n.to_lowercase()), "twice: {n}");
            assert!(!n.eq_ignore_ascii_case(crate::config::RANDOM));
        }
    }

    #[test]
    fn the_two_picks_differ_for_any_seed_and_cover_the_list() {
        let mut used = std::collections::HashSet::new();
        for seed in 0..5_000u64 {
            let (a, b) = picks_from(seed.wrapping_mul(0x1234_5678_9ABC_DEF1));
            assert_ne!(a, b);
            assert!(a < CALL_SIGNS.len() && b < CALL_SIGNS.len());
            used.insert(a);
        }
        assert_eq!(used.len(), CALL_SIGNS.len(), "every name can come up");
        let (a, b) = launch_picks();
        assert_ne!(a, b);
        assert_eq!(launch_picks(), (a, b), "kept for the launch");
    }
}
