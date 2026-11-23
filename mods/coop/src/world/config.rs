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

//! The shared-world part of the config ([`WorldSettings`]), validated from
//! the flat keys in [`crate::config::Config`] (flat because the loader
//! overlays a settings file on the defaults key by key).

#![forbid(unsafe_code)]

use super::color::parse_color;
use crate::config::Config;

/// The shared world's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldSettings {
    /// This player's colour on the partner's screen; `None` is auto.
    pub color: Option<u32>,
    /// The partner's name above their ship.
    pub show_names: bool,
    /// A marker pointing at the partner.
    pub show_marker: bool,
    /// Shared damage (`crate::damage`).
    pub damage: crate::damage::DamageSettings,
}

impl Config {
    /// Validate the shared-world keys, pushing one warning per changed value.
    pub(crate) fn world_settings(&self, w: &mut Vec<String>) -> WorldSettings {
        let color = parse_color(&self.color).unwrap_or_else(|e| {
            w.push(format!("{e}; using auto"));
            None
        });
        WorldSettings {
            color,
            show_names: self.show_names,
            show_marker: self.show_marker,
            damage: crate::damage::DamageSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Config {
        ndml_sdk::config::parse_defaults(crate::NDML_DEFAULT_CONFIG).unwrap()
    }

    #[test]
    fn shipped_world_defaults() {
        let mut w = Vec::new();
        let s = defaults().world_settings(&mut w);
        assert!(w.is_empty(), "{w:?}");
        assert!(!s.show_names && !s.show_marker, "clean look by default");
        assert_eq!(s.color, None, "auto: no explicit colour");
    }

    /// Unknown keys in a settings file are dropped; its other values count.
    #[test]
    fn unknown_world_keys_are_dropped() {
        let before = "extra_a = false\nextra_b = false\n\
                      extra_c = false\nextra_d = false\n\
                      show_marker = true\n";
        let r = ndml_sdk::config::refresh(before, crate::NDML_DEFAULT_CONFIG).unwrap();
        let dropped: Vec<&str> = r.dropped.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(dropped, ["extra_a", "extra_b", "extra_c", "extra_d"]);
        let c: Config = ndml_sdk::config::parse_defaults(&r.text).unwrap();
        let mut w = Vec::new();
        assert!(c.world_settings(&mut w).show_marker, "the file's own value");
        assert!(w.is_empty(), "{w:?}");
    }

    /// Unknown replication keys are dropped; the rest counts.
    #[test]
    fn unknown_replication_keys_are_dropped() {
        let before = "extra_on = true\nextra_off = false\nextra_mode = \"fast\"\n\
                      extra_gain = 0.5\nextra_px = 10\nextra_ms = 0\n\
                      extra_path = \"draw\"\nshow_names = true\n";
        let r = ndml_sdk::config::refresh(before, crate::NDML_DEFAULT_CONFIG).unwrap();
        let dropped: Vec<&str> = r.dropped.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            dropped,
            [
                "extra_on",
                "extra_off",
                "extra_mode",
                "extra_gain",
                "extra_px",
                "extra_ms",
                "extra_path"
            ]
        );
        let c: Config = ndml_sdk::config::parse_defaults(&r.text).unwrap();
        let mut w = Vec::new();
        assert!(c.world_settings(&mut w).show_names, "the file's own value");
        assert!(w.is_empty(), "{w:?}");
    }

    /// Unknown switches are dropped; the file's other values count.
    #[test]
    fn unknown_switches_are_dropped() {
        const UNKNOWN: [&str; 6] = [
            "switch_a", "switch_b", "switch_c", "switch_d", "switch_e", "switch_f",
        ];
        let mut before: String = UNKNOWN
            .iter()
            .map(|k| {
                if *k == "switch_c" {
                    format!("{k} = \"text\"\n")
                } else {
                    format!("{k} = false\n")
                }
            })
            .collect();
        before.push_str("extra_on = true\nextra_more = true\nshow_names = true\n");
        let r = ndml_sdk::config::refresh(&before, crate::NDML_DEFAULT_CONFIG).unwrap();
        let dropped: Vec<&str> = r.dropped.iter().map(|s| s.key.as_str()).collect();
        let mut want = UNKNOWN.to_vec();
        want.extend(["extra_on", "extra_more"]);
        assert_eq!(dropped, want);
        let c: Config = ndml_sdk::config::parse_defaults(&r.text).unwrap();
        let mut w = Vec::new();
        let s = c.world_settings(&mut w);
        assert!(s.show_names, "the file's own value");
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn bad_world_values_warn_and_fall_back() {
        let mut c = defaults();
        c.color = "teal".into();
        let mut w = Vec::new();
        let s = c.world_settings(&mut w);
        assert_eq!(w.len(), 1, "{w:#?}");
        assert_eq!(s.color, None, "auto: no explicit colour");
        c.color = "none".into();
        let s = c.world_settings(&mut Vec::new());
        assert_eq!(s.color, None);
    }
}
