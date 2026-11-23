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

//! Time conventions.
//!
//! Every pure component takes the current time as a [`Duration`] since an
//! arbitrary, per-process epoch (the network thread uses `Instant::now() -
//! epoch`, tests use a simulated counter). Nothing in the protocol logic reads a
//! wall clock.
//!
//! On the wire, send times are milliseconds since the sender's epoch in a `u32`
//! ([`to_wire_ms`]), which wraps after about 49 days. Sessions are far shorter, so
//! wrapping is not handled.

#![forbid(unsafe_code)]

use std::time::Duration;

/// Milliseconds since the sender's epoch, as carried in packet headers and
/// snapshot timestamps. Saturates instead of wrapping.
#[must_use]
pub fn to_wire_ms(t: Duration) -> u32 {
    u32::try_from(t.as_millis()).unwrap_or(u32::MAX)
}

/// Inverse of [`to_wire_ms`].
#[must_use]
pub fn from_wire_ms(ms: u32) -> Duration {
    Duration::from_millis(u64::from(ms))
}

/// Signed seconds between two times (`b - a`), for filters that need negatives.
#[must_use]
pub fn signed_secs(a: Duration, b: Duration) -> f64 {
    b.as_secs_f64() - a.as_secs_f64()
}

/// Duration from non-negative seconds; negatives and NaN clamp to zero.
#[must_use]
pub fn secs(s: f64) -> Duration {
    if s.is_finite() && s > 0.0 {
        Duration::from_secs_f64(s)
    } else {
        Duration::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_ms_round_trips_and_saturates() {
        let t = Duration::from_millis(123_456);
        assert_eq!(from_wire_ms(to_wire_ms(t)), t);
        assert_eq!(to_wire_ms(Duration::from_secs(u64::MAX / 2)), u32::MAX);
        assert_eq!(secs(-1.0), Duration::ZERO);
        assert_eq!(secs(f64::NAN), Duration::ZERO);
    }
}
