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

//! The UI's config switches. The page is reached through the Co-op row in
//! the game's main menu and drawn by the game's menu, its questions in the
//! game's YES/NO dialog; the in-run notes are the game's hint bars.

#![forbid(unsafe_code)]

use super::layout::HudCorner;
use super::run::Switches;
use crate::world::present::PartnerTint;

/// See the module docs; one field per config key.
#[derive(Debug, Clone, PartialEq)]
pub struct UiSettings {
    pub hud_corner: HudCorner,
    /// `show_traffic`: KB/s up and down above the HUD line.
    pub show_traffic: bool,
    /// The Steam Deck's on-screen keyboard for text entry.
    pub deck_keyboard: bool,
    pub tint: PartnerTint,
}

impl UiSettings {
    /// The run selector's switches. `overlay = false` turns off the HUD line
    /// and the traffic line.
    #[must_use]
    pub fn switches(&self, overlay: bool) -> Switches {
        Switches {
            hud: overlay,
            traffic: overlay && self.show_traffic,
        }
    }
}

impl crate::config::Config {
    /// The UI keys (all enums or switches: nothing to clamp).
    pub(crate) fn ui_settings(&self) -> UiSettings {
        UiSettings {
            hud_corner: self.hud_corner,
            show_traffic: self.show_traffic,
            deck_keyboard: self.deck_keyboard,
            tint: self.partner_tint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_are_the_native_ui() {
        let c: crate::config::Config =
            ndml_sdk::config::parse_defaults(crate::NDML_DEFAULT_CONFIG).unwrap();
        let s = c.ui_settings();
        assert_eq!(s.tint, PartnerTint::Off);
        assert_eq!(s.hud_corner, HudCorner::Right);
        assert!(!s.switches(false).hud);
        assert!(!s.show_traffic, "the traffic line is off by default");
        assert!(!s.switches(true).traffic);
    }

    #[test]
    fn show_traffic_needs_the_hud_line() {
        let text =
            crate::NDML_DEFAULT_CONFIG.replace("show_traffic = false", "show_traffic = true");
        let c: crate::config::Config = ndml_sdk::config::parse_defaults(&text).unwrap();
        let s = c.ui_settings();
        assert!(s.switches(true).traffic);
        assert!(!s.switches(false).traffic, "overlay = false hides both");
    }

    #[test]
    fn every_option_parses() {
        let text = crate::NDML_DEFAULT_CONFIG
            .replace("partner_tint = \"off\"", "partner_tint = \"chosen\"")
            .replace("hud_corner = \"right\"", "hud_corner = \"left\"");
        let c: crate::config::Config = ndml_sdk::config::parse_defaults(&text).unwrap();
        let s = c.ui_settings();
        assert_eq!(s.tint, PartnerTint::Chosen);
        assert_eq!(s.hud_corner, HudCorner::Left);
    }
}
