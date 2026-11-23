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

//! Co-op colours: each player's chosen colour (`color`, or the co-op page's
//! Ship Colour row) is sent to partners as `PlayerColor`; `present` decides
//! how it is drawn. Parsing, the page's choices and [`ColorSync`].

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use ndml_net::msg::{Message, PeerId};

use crate::net::Out;
use crate::render::rgb;

/// The game's six ship palettes (`refreshEffects`): name (the mod's own),
/// `col` (ship blend) and `colJet` (thrusters, trails), `0xBBGGRR`.
pub const SHIP_PALETTES: [(&str, u32, u32); 6] = [
    ("blue", 0x00FF_F2CF, 0x00FF_A000),
    ("gold", 0x00D5_F1FF, 0x0052_B6FC),
    ("red", 0x00DC_D2FF, 0x003D_00FE),
    ("green", 0x00E8_FFC0, 0x00CD_F538),
    ("violet", 0x00FF_DCFF, 0x00ED_4670),
    ("purple", 0x00FF_C8FF, 0x0094_256E),
];

/// Extra named colours (not the game's own).
pub const EXTRA_COLORS: &[(&str, (u8, u8, u8))] = &[
    ("cyan", (64, 220, 255)),
    ("magenta", (255, 70, 220)),
    ("amber", (255, 190, 64)),
    ("orange", (255, 140, 50)),
    ("pink", (255, 150, 200)),
    ("white", (255, 255, 255)),
];

/// `"auto"` / `"none"` (no explicit colour: the partner uses your ship's own
/// colours), one of the game's palettes (its `colJet`), an extra name, or
/// `#RRGGBB` / `RRGGBB` / `0xRRGGBB` (red first, as people write hex).
pub fn parse_color(text: &str) -> Result<Option<u32>, String> {
    let t = text.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    if let Some(&(_, _, jet)) = SHIP_PALETTES
        .iter()
        .find(|(n, ..)| n.eq_ignore_ascii_case(t))
    {
        return Ok(Some(jet));
    }
    if let Some(&(_, (r, g, b))) = EXTRA_COLORS.iter().find(|(n, _)| n.eq_ignore_ascii_case(t)) {
        return Ok(Some(rgb(r, g, b)));
    }
    let hex = t
        .strip_prefix('#')
        .or_else(|| t.strip_prefix("0x"))
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if hex.len() == 6
        && hex.chars().all(|c| c.is_ascii_hexdigit())
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        let [_, r, g, b] = v.to_be_bytes();
        return Ok(Some(rgb(r, g, b)));
    }
    let names: Vec<&str> = SHIP_PALETTES
        .iter()
        .map(|(n, ..)| *n)
        .chain(EXTRA_COLORS.iter().map(|(n, _)| *n))
        .collect();
    Err(format!(
        "color {text:?} is not \"auto\", one of {}, or a hex colour like \"#40C0FF\"",
        names.join(", ")
    ))
}

/// One value of the page's Ship Colour row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorChoice {
    /// What goes into the config file (`auto`, `gold`, `#40C0FF`, ...).
    pub key: String,
    /// What the row shows.
    pub label: String,
    /// The colour sent to the partner.
    pub value: Option<u32>,
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map_or_default(|f| f.to_ascii_uppercase().to_string() + c.as_str())
}

/// The Ship Colour row's values (Auto, then the game's palettes, then the
/// configured value if it is something else) and the index of `current`.
#[must_use]
pub fn page_choices(current: &str) -> (Vec<ColorChoice>, usize) {
    let mut out = vec![ColorChoice {
        key: "auto".into(),
        label: "Auto".into(),
        value: None,
    }];
    out.extend(SHIP_PALETTES.iter().map(|&(n, _, jet)| ColorChoice {
        key: n.into(),
        label: title_case(n),
        value: Some(jet),
    }));
    let t = current.trim();
    let found = if t.is_empty() || t.eq_ignore_ascii_case("none") {
        Some(0)
    } else {
        out.iter().position(|c| c.key.eq_ignore_ascii_case(t))
    };
    let idx = match (found, parse_color(t)) {
        (Some(i), _) => i,
        (None, Ok(value)) => {
            out.push(ColorChoice {
                key: t.to_owned(),
                label: title_case(&crate::ui::text::truncate(t, 12)),
                value,
            });
            out.len() - 1
        }
        (None, Err(_)) => 0,
    };
    (out, idx)
}

/// Own colour going out, the partners' coming in.
#[derive(Debug, Clone, Default)]
pub struct ColorSync {
    mine: Option<u32>,
    told: BTreeSet<PeerId>,
    theirs: BTreeMap<PeerId, Option<u32>>,
}

impl ColorSync {
    /// Own colour from the config.
    #[must_use]
    pub fn new(mine: Option<u32>) -> Self {
        Self {
            mine,
            ..Self::default()
        }
    }

    /// The player picked another colour: every partner is told again.
    pub fn set_mine(&mut self, mine: Option<u32>) {
        if self.mine != mine {
            self.mine = mine;
            self.told.clear();
        }
    }

    /// Own colour.
    #[must_use]
    pub fn mine(&self) -> Option<u32> {
        self.mine
    }

    /// Messages telling every linked partner not told yet. Partners no longer
    /// linked are forgotten, so a reconnect tells them again.
    pub fn outs(&mut self, me: PeerId, linked: &[PeerId]) -> Vec<Out> {
        self.told.retain(|p| linked.contains(p));
        self.theirs.retain(|p, _| linked.contains(p));
        let mut outs = Vec::new();
        for &p in linked {
            if self.told.insert(p) {
                outs.push(Out::Send(
                    p,
                    Message::PlayerColor {
                        peer: me,
                        color: self.mine,
                    },
                ));
            }
        }
        outs
    }

    /// A `PlayerColor` arrived about `peer` (the owner; a relaying host keeps
    /// the owner's id).
    pub fn on_color(&mut self, peer: PeerId, color: Option<u32>) {
        self.theirs.insert(peer, color.map(|c| c & 0x00FF_FFFF));
    }

    /// A partner's colour (`None`: none chosen, or not heard yet).
    #[must_use]
    pub fn partner(&self, peer: PeerId) -> Option<u32> {
        self.theirs.get(&peer).copied().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_names_hex_and_auto() {
        assert_eq!(parse_color("none"), Ok(None));
        assert_eq!(parse_color(" AUTO "), Ok(None));
        assert_eq!(parse_color(""), Ok(None));
        assert_eq!(
            parse_color("Gold"),
            Ok(Some(0x0052_B6FC)),
            "the game's colJet"
        );
        assert_eq!(parse_color("cyan"), Ok(Some(rgb(64, 220, 255))));
        assert_eq!(parse_color("white"), Ok(Some(0x00FF_FFFF)));
        // Hex is red-first as written; GameMaker stores blue-first.
        assert_eq!(parse_color("#FF0000"), Ok(Some(0x0000_00FF)));
        assert_eq!(parse_color("0000ff"), Ok(Some(0x00FF_0000)));
        assert_eq!(parse_color("0x00FF00"), Ok(Some(0x0000_FF00)));
        for bad in ["#FFF", "teal", "#GG0000", "#12345678", "0x"] {
            let e = parse_color(bad).unwrap_err();
            assert!(e.contains("gold") && e.contains("hex"), "{e}");
        }
        assert!(
            SHIP_PALETTES
                .iter()
                .all(|(n, ..)| parse_color(n).unwrap().is_some())
        );
    }

    /// The config's `# options:` for `color` list exactly these colours.
    #[test]
    fn the_shipped_colour_options_are_these_colours() {
        let text = include_str!("../../config.toml");
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.starts_with("# Your colour on your partner's screen"))
            .take_while(|l| !l.starts_with("color = "))
            .skip_while(|l| l.trim() != "# options:")
            .skip(1)
            .map(|l| l.trim_start_matches('#').trim())
            .filter(|l| !l.ends_with(':'))
            .collect();
        let hex = |r: u8, g: u8, b: u8| format!("#{r:02x}{g:02x}{b:02x}");
        let mut want = vec!["auto".to_owned()];
        want.extend(SHIP_PALETTES.iter().map(|&(n, _, jet)| {
            let [_, b, g, r] = jet.to_be_bytes();
            format!("{n} {}", hex(r, g, b))
        }));
        want.extend(
            EXTRA_COLORS
                .iter()
                .map(|&(n, (r, g, b))| format!("{n} {}", hex(r, g, b))),
        );
        want.push("#rrggbb".into());
        assert_eq!(block, want);
        for entry in &block {
            let name = entry.split(' ').next().unwrap();
            if name != "#rrggbb" {
                assert!(parse_color(name).is_ok(), "{name}");
            }
        }
    }

    #[test]
    fn page_choices_cover_the_palettes_and_a_custom_value() {
        let (c, i) = page_choices("auto");
        assert_eq!((c.len(), i), (7, 0));
        assert_eq!(c[0].label, "Auto");
        assert_eq!(c[2].label, "Gold");
        assert_eq!(page_choices("Violet").1, 5);
        assert_eq!(page_choices("none").1, 0);
        let (c, i) = page_choices("#40C0FF");
        assert_eq!((c.len(), i), (8, 7));
        assert_eq!(c[7].key, "#40C0FF");
        assert_eq!(c[7].value, parse_color("#40C0FF").unwrap());
        let (c, i) = page_choices("cyan");
        assert_eq!(c[i].label, "Cyan");
        assert_eq!(page_choices("teal").1, 0, "unparsable: Auto");
    }

    #[test]
    fn colours_go_to_each_partner_once_and_again_after_a_change() {
        let mut c = ColorSync::new(Some(7));
        let host = PeerId::HOST;
        let me = PeerId(1);
        let outs = c.outs(me, &[host]);
        assert_eq!(
            outs,
            vec![Out::Send(
                host,
                Message::PlayerColor {
                    peer: me,
                    color: Some(7)
                }
            )]
        );
        assert!(c.outs(me, &[host]).is_empty(), "once per link");
        c.set_mine(Some(7));
        assert!(c.outs(me, &[host]).is_empty(), "unchanged: not resent");
        c.set_mine(Some(8));
        assert_eq!(c.mine(), Some(8));
        assert_eq!(c.outs(me, &[host]).len(), 1, "a new colour is resent");
        c.on_color(host, Some(0x0123_4567));
        assert_eq!(c.partner(host), Some(0x0023_4567), "masked to 24 bits");
        c.on_color(host, None);
        assert_eq!(c.partner(host), None);
        assert!(c.outs(me, &[]).is_empty());
        assert_eq!(c.outs(me, &[host]).len(), 1, "told again after a relink");
    }
}
