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

//! A small seeded PRNG (`SplitMix64`) for the conditioner and tests, plus
//! [`random_u64`] for nonces and STUN transaction ids.
//!
//! Not cryptographic. Nonces only separate sessions and reject stray packets.

#![forbid(unsafe_code)]

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

/// `SplitMix64`: tiny, fast, and good enough for simulation.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// A generator with a fixed seed (same seed, same sequence).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Next 32 random bits.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        // 53 random mantissa bits.
        #[allow(clippy::cast_precision_loss)] // exact: value < 2^53
        let v = (self.next_u64() >> 11) as f64;
        v * (1.0 / 9_007_199_254_740_992.0) // 2^-53
    }

    /// Uniform in `[lo, hi)`.
    pub fn range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }

    /// Uniform integer in `[0, n)`; 0 when `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True with probability `p` (clamped to `[0, 1]`).
    pub fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

/// 64 bits of OS-seeded randomness from std's `RandomState` (no dependency, no
/// unsafe). Used for session nonces and STUN transaction ids.
#[must_use]
pub fn random_u64() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u64(0x6e64_6d6c);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn floats_stay_in_range_and_look_uniform() {
        let mut r = Rng::new(1);
        let n = 20_000;
        let mut sum = 0.0;
        for _ in 0..n {
            let v = r.next_f64();
            assert!((0.0..1.0).contains(&v));
            sum += v;
        }
        let mean = sum / f64::from(n);
        assert!((mean - 0.5).abs() < 0.01, "mean {mean}");
        assert_eq!(r.below(0), 0);
    }

    #[test]
    fn os_randomness_differs() {
        assert_ne!(random_u64(), random_u64());
    }
}
