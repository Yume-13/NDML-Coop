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
    /// How long a downed ship waits (game time).
    pub respawn: Duration,
}

impl Default for RulesSettings {
    fn default() -> Self {
        Self {
            pause_on_focus_loss: true,
            respawn: Duration::from_secs(45),
        }
    }
}

/// The raw keys, as `config.rs` read them.
#[derive(Debug, Clone, Copy)]
pub struct Raw {
    pub pause_on_focus_loss: bool,
    /// `respawn_seconds`, already validated.
    pub respawn: Duration,
}

/// The settings from the raw keys.
#[must_use]
pub fn validate(r: &Raw) -> RulesSettings {
    RulesSettings {
        pause_on_focus_loss: r.pause_on_focus_loss,
        respawn: r.respawn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> Raw {
        Raw {
            pause_on_focus_loss: true,
            respawn: Duration::from_secs(45),
        }
    }

    #[test]
    fn defaults_validate() {
        assert_eq!(validate(&raw()), RulesSettings::default());
    }
}
