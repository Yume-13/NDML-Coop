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

//! The session rules' settings (`ndml/config/ndmod_coop.toml`).

#![forbid(unsafe_code)]

use std::time::Duration;

/// Validated settings for the session rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesSettings {
    /// The game's pause on focus loss counts as that player pausing.
    pub pause_on_focus_loss: bool,
    /// After this long a player who has chosen may stop waiting (`None`: never).
    pub upgrade_wait: Option<Duration>,
    /// How long a downed ship waits (game time).
    pub respawn: Duration,
}

impl Default for RulesSettings {
    fn default() -> Self {
        Self {
            pause_on_focus_loss: true,
            upgrade_wait: Some(Duration::from_secs(60)),
            respawn: Duration::from_secs(45),
        }
    }
}

/// The raw keys, as `config.rs` read them.
#[derive(Debug, Clone, Copy)]
pub struct Raw {
    pub pause_on_focus_loss: bool,
    pub upgrade_wait_seconds: f64,
    pub respawn: Duration,
}

/// A wait in seconds: 0 = never, otherwise clamped to 10..3600 with a warning.
fn wait(name: &str, v: f64, default: f64, w: &mut Vec<String>) -> Option<Duration> {
    if !v.is_finite() || v < 0.0 {
        w.push(format!(
            "{name} = {v} is not a number of seconds; using {default}"
        ));
        return Some(Duration::from_secs_f64(default));
    }
    if v == 0.0 {
        return None;
    }
    let c = v.clamp(10.0, 3600.0);
    if (c - v).abs() > f64::EPSILON {
        w.push(format!(
            "{name} = {v} is outside [10, 3600] (0 = never); using {c}"
        ));
    }
    Some(Duration::from_secs_f64(c))
}

/// Validate the raw keys; one warning per value changed.
#[must_use]
pub fn validate(r: &Raw, w: &mut Vec<String>) -> RulesSettings {
    RulesSettings {
        pause_on_focus_loss: r.pause_on_focus_loss,
        upgrade_wait: wait("upgrade_wait_seconds", r.upgrade_wait_seconds, 60.0, w),
        respawn: r.respawn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> Raw {
        Raw {
            pause_on_focus_loss: true,
            upgrade_wait_seconds: 60.0,
            respawn: Duration::from_secs(45),
        }
    }

    #[test]
    fn defaults_validate_quietly() {
        let mut w = Vec::new();
        assert_eq!(validate(&raw(), &mut w), RulesSettings::default());
        assert!(w.is_empty());
    }

    #[test]
    fn waits_zero_means_never_and_clamp() {
        let mut w = Vec::new();
        let s = validate(
            &Raw {
                upgrade_wait_seconds: 0.0,
                ..raw()
            },
            &mut w,
        );
        assert_eq!(s.upgrade_wait, None);
        assert!(w.is_empty());
        let s = validate(
            &Raw {
                upgrade_wait_seconds: 2.0,
                ..raw()
            },
            &mut w,
        );
        assert_eq!(s.upgrade_wait, Some(Duration::from_secs(10)));
        assert_eq!(w.len(), 1);
        let s = validate(
            &Raw {
                upgrade_wait_seconds: f64::NAN,
                ..raw()
            },
            &mut w,
        );
        assert_eq!(s.upgrade_wait, Some(Duration::from_secs(60)));
        assert_eq!(w.len(), 2);
    }
}
