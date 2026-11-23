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

//! How partners look on screen: the avatar's tint and opacity, and the
//! optional name label and edge marker (placed with the camera view).
//!
//! * Ship colour ([`PartnerTint`]): by default a partner keeps the colours
//!   of its own screen and its chosen colour goes only on label and marker.
//! * A cloak is the game's own `image_alpha`, carried in `PlayerLook` and
//!   used as is.

#![forbid(unsafe_code)]

use ndml_net::interp::Vec2;
use ndml_net::msg::{PeerId, PlayerLook};

use super::color::SHIP_PALETTES;
use crate::overlay::Line;
use crate::render::{DrawItem, Gui, LookParams, PartnerFrame, View, color, plan_partner};

/// A partner as the world sees it this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct PartnerView {
    /// The partner's peer id.
    pub peer: PeerId,
    /// The partner's display name.
    pub name: String,
    /// Heard from within the last second.
    pub fresh: bool,
    /// Downed (never targeted).
    pub downed: bool,
    /// Interpolated pose: position, velocity (px/s), facing.
    pub pose: Option<(Vec2, Vec2, f32)>,
    /// Ship sprite index, if known.
    pub sprite: Option<u32>,
    /// Sprite frame.
    pub frame: u8,
    /// Scale, blend, alpha and visibility.
    pub look: LookParams,
    /// Their co-op colour.
    pub color: Option<u32>,
}

impl PartnerView {
    /// Enemies may go after this partner (before the avatar check).
    #[must_use]
    pub fn targetable(&self) -> bool {
        self.fresh && self.pose.is_some() && self.look.visible && !self.look.cloaked && !self.downed
    }
}

/// `LookParams` from a `PlayerLook`.
#[must_use]
pub fn look_params(l: &PlayerLook) -> LookParams {
    LookParams {
        xscale: f64::from(l.xscale),
        yscale: f64::from(l.yscale),
        blend: l.blend,
        alpha: f64::from(l.alpha),
        visible: l.visible,
        cloaked: l.cloaked,
    }
}

/// `(image_blend, image_alpha, visible)` for a partner's avatar.
#[must_use]
pub fn ship_look(look: &LookParams, tint: Option<u32>) -> (u32, f64, bool) {
    let alpha = look.alpha.clamp(0.0, 1.0);
    (tint.unwrap_or(look.blend), alpha, look.visible)
}

fn keep(item: &DrawItem, names: bool, marker: bool) -> bool {
    match item {
        DrawItem::Text { .. } => names,
        DrawItem::Triangle { .. } => marker,
        _ => true,
    }
}

fn recolor(item: DrawItem, tint: Option<u32>) -> DrawItem {
    let c = tint.unwrap_or(color::PARTNER);
    match item {
        DrawItem::Text {
            x,
            y,
            text,
            scale,
            align,
            ..
        } => DrawItem::Text {
            x,
            y,
            text,
            color: c,
            scale,
            align,
        },
        DrawItem::Triangle { pts, alpha, .. } => DrawItem::Triangle {
            pts,
            color: c,
            alpha,
        },
        DrawItem::Sprite(mut s) => {
            if let Some(t) = tint {
                s.blend = t;
            }
            DrawItem::Sprite(s)
        }
        other @ DrawItem::Rect { .. } => other,
    }
}

/// Labels and markers for partners shown as avatars (the game draws the ships).
/// `screen` is the camera view (`view_get_camera(0)`), not the arena globals,
/// which do not match the GUI placement.
#[must_use]
pub fn labels(
    partners: &[PartnerView],
    screen: Option<View>,
    gui: Gui,
    line_h: f64,
    names: bool,
    marker: bool,
) -> Vec<DrawItem> {
    if !names && !marker {
        return Vec::new();
    }
    let mut out = Vec::new();
    // A downed partner (the session rules) has no ship to label.
    for p in partners.iter().filter(|p| p.fresh && !p.downed) {
        let Some((pos, _, angle)) = p.pose else {
            continue;
        };
        let items = plan_partner(
            &PartnerFrame {
                pos,
                angle,
                sprite: None,
                frame: p.frame,
                look: p.look,
                name: &p.name,
            },
            screen,
            gui,
            line_h,
        );
        out.extend(
            items
                .into_iter()
                .filter(|i| keep(i, names, marker))
                .map(|i| recolor(i, p.color)),
        );
    }
    out
}

/// The draw-call path's items with names/marker switched and the partner's
/// colour applied (one partner: its colour; several: labels only filtered).
#[must_use]
pub fn restyle(
    items: Vec<DrawItem>,
    names: bool,
    marker: bool,
    tint: Option<u32>,
) -> Vec<DrawItem> {
    items
        .into_iter()
        .filter(|i| keep(i, names, marker))
        .map(|i| recolor(i, tint))
        .collect()
}

/// How partners' ships are coloured on this screen (`partner_tint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartnerTint {
    /// No tint: the partner's own replicated `image_blend`, as on their
    /// screen, even when both match; their chosen colour goes only on
    /// the label and marker.
    #[default]
    Off,
    /// The partner's real, replicated `image_blend` (a pale wash, as the
    /// game colours every ship), swapped to the next game palette when both
    /// match; their chosen colour goes only on the label and marker.
    Native,
    /// The partner's chosen colour pushed into `image_blend`.
    Chosen,
}

/// The blend to draw a partner with in [`PartnerTint::Native`]: theirs, or,
/// when it equals the local ship's, the next of the game's palettes.
#[must_use]
pub fn native_blend(partner: u32, own: Option<u32>) -> u32 {
    let p = partner & 0x00FF_FFFF;
    if own.map(|o| o & 0x00FF_FFFF) != Some(p) {
        return p;
    }
    let n = SHIP_PALETTES.len();
    let start = SHIP_PALETTES
        .iter()
        .position(|&(_, col, _)| col == p)
        .map_or(0, |i| i + 1);
    (0..n)
        .map(|k| SHIP_PALETTES[(start + k) % n].1)
        .find(|&c| c != p)
        .unwrap_or(p)
}

/// The label and marker colour: the partner's chosen colour, else the
/// `colJet` of the palette they are drawn with, else the game's blue.
#[must_use]
pub fn accent(blend: u32, chosen: Option<u32>) -> u32 {
    chosen
        .or_else(|| {
            SHIP_PALETTES
                .iter()
                .find(|&&(_, col, _)| col == blend & 0x00FF_FFFF)
                .map(|&(_, _, jet)| jet)
        })
        .unwrap_or(crate::ui::style::color::BLUE)
}

/// Apply `tint` to the partners the world will draw (in place: their
/// `look.blend` and `color`, which `ship_look` reads) and return copies for
/// the labels, whose `color` is the accent. `own_blend`: the local ship's.
#[must_use]
pub fn style_partners(
    partners: &mut [PartnerView],
    tint: PartnerTint,
    own_blend: Option<u32>,
) -> Vec<PartnerView> {
    let mut labels = Vec::with_capacity(partners.len());
    for p in partners.iter_mut() {
        let chosen = p.color;
        let ship = match tint {
            PartnerTint::Off => {
                p.color = None;
                p.look.blend
            }
            PartnerTint::Native => {
                p.look.blend = native_blend(p.look.blend, own_blend);
                p.color = None;
                p.look.blend
            }
            PartnerTint::Chosen => chosen.unwrap_or(p.look.blend),
        };
        let mut l = p.clone();
        l.color = Some(accent(ship, chosen));
        labels.push(l);
    }
    labels
}

/// The draw-call path's items with the label switches, the ship blend
/// (`None` = as drawn) and the accent on the label and marker.
#[must_use]
pub fn restyle_split(
    items: Vec<DrawItem>,
    names: bool,
    marker: bool,
    ship_blend: Option<u32>,
    accent: u32,
) -> Vec<DrawItem> {
    items
        .into_iter()
        .filter(|i| keep(i, names, marker))
        .map(|i| match i {
            DrawItem::Sprite(mut s) => {
                if let Some(b) = ship_blend {
                    s.blend = b;
                }
                DrawItem::Sprite(s)
            }
            other => recolor(other, Some(accent)),
        })
        .collect()
}

/// A line for the run overlay.
#[must_use]
pub fn world_line(text: String) -> Line {
    (text, color::DIM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partner() -> PartnerView {
        PartnerView {
            peer: PeerId::HOST,
            name: "Ace".into(),
            fresh: true,
            downed: false,
            pose: Some((Vec2::new(500.0, 400.0), Vec2::default(), 0.0)),
            sprite: Some(5),
            frame: 0,
            look: LookParams::default(),
            color: Some(0x0012_3456),
        }
    }

    const GUI: Gui = Gui {
        w: 1920.0,
        h: 1080.0,
    };

    #[test]
    fn labels_follow_the_switches_and_the_colour() {
        let p = [partner()];
        assert!(
            labels(&p, None, GUI, 18.0, false, false).is_empty(),
            "clean by default"
        );
        let only_names = labels(&p, None, GUI, 18.0, true, false);
        assert_eq!(only_names.len(), 1);
        assert!(matches!(&only_names[0], DrawItem::Text { color, .. } if *color == 0x0012_3456));
        let both = labels(&p, None, GUI, 18.0, true, true);
        assert_eq!(both.len(), 2);
        assert!(
            both.iter().all(|i| !matches!(i, DrawItem::Sprite(_))),
            "the game draws the ship"
        );
        let mut untinted = partner();
        untinted.color = None;
        let l = labels(&[untinted], None, GUI, 18.0, false, true);
        assert!(matches!(&l[0], DrawItem::Triangle { color, .. } if *color == color::PARTNER));
        let mut down = partner();
        down.downed = true;
        assert!(
            labels(&[down], None, GUI, 18.0, true, true).is_empty(),
            "no label or marker while down"
        );
    }

    #[test]
    fn restyle_tints_the_drawn_ship() {
        let items = plan_partner(
            &PartnerFrame {
                pos: Vec2::new(10.0, 10.0),
                angle: 0.0,
                sprite: Some(3),
                frame: 0,
                look: LookParams::default(),
                name: "x",
            },
            None,
            GUI,
            18.0,
        );
        let r = restyle(items, false, false, Some(77));
        assert_eq!(r.len(), 1);
        assert!(matches!(&r[0], DrawItem::Sprite(s) if s.blend == 77));
    }

    #[test]
    fn native_blend_swaps_only_on_a_match() {
        let blue = SHIP_PALETTES[0].1;
        let gold = SHIP_PALETTES[1].1;
        let purple = SHIP_PALETTES[5].1;
        assert_eq!(native_blend(gold, Some(blue)), gold, "different: as is");
        assert_eq!(native_blend(blue, Some(blue)), gold, "same: next palette");
        assert_eq!(native_blend(purple, Some(purple)), blue, "wraps");
        assert_eq!(
            native_blend(0x0012_3456, Some(0x0012_3456)),
            blue,
            "custom skin"
        );
        assert_eq!(native_blend(blue, None), blue);
        assert_eq!(
            accent(gold, None),
            SHIP_PALETTES[1].2,
            "the palette's colJet"
        );
        assert_eq!(accent(gold, Some(9)), 9, "the chosen colour wins");
        assert_eq!(accent(0x0012_3456, None), crate::ui::style::color::BLUE);
    }

    #[test]
    fn styles_off_native_and_chosen() {
        let blue = SHIP_PALETTES[0].1;
        let mut p = partner();
        p.look.blend = blue;
        p.color = Some(0x0012_3456);
        let mut native = [p.clone()];
        let labels = style_partners(&mut native, PartnerTint::Native, Some(blue));
        assert_eq!(native[0].look.blend, SHIP_PALETTES[1].1, "swapped if equal");
        assert_eq!(native[0].color, None, "ship_look keeps the blend");
        assert_eq!(
            ship_look(&native[0].look, native[0].color).0,
            SHIP_PALETTES[1].1
        );
        assert_eq!(
            labels[0].color,
            Some(0x0012_3456),
            "label in the chosen colour"
        );
        let mut chosen = [p.clone()];
        let labels = style_partners(&mut chosen, PartnerTint::Chosen, Some(blue));
        assert_eq!(ship_look(&chosen[0].look, chosen[0].color).0, 0x0012_3456);
        assert_eq!(labels[0].color, Some(0x0012_3456));
        let mut off = [p.clone()];
        let labels = style_partners(&mut off, PartnerTint::Off, Some(blue));
        assert_eq!(
            off[0].look.blend, blue,
            "as on their screen, even when both match"
        );
        assert_eq!(ship_look(&off[0].look, off[0].color).0, blue);
        assert_eq!(
            labels[0].color,
            Some(0x0012_3456),
            "label in the chosen colour"
        );
        let mut auto = [PartnerView { color: None, ..p }];
        let labels = style_partners(&mut auto, PartnerTint::Native, None);
        assert_eq!(
            labels[0].color,
            Some(SHIP_PALETTES[0].2),
            "colJet of their palette"
        );
    }

    #[test]
    fn restyle_split_colours_ship_and_label_apart() {
        let items = plan_partner(
            &PartnerFrame {
                pos: Vec2::new(10.0, 10.0),
                angle: 0.0,
                sprite: Some(3),
                frame: 0,
                look: LookParams::default(),
                name: "x",
            },
            None,
            GUI,
            18.0,
        );
        let r = restyle_split(items, true, true, Some(55), 66);
        assert!(
            r.iter()
                .any(|i| matches!(i, DrawItem::Sprite(s) if s.blend == 55))
        );
        assert!(
            r.iter()
                .any(|i| matches!(i, DrawItem::Text { color, .. } if *color == 66))
        );
        assert!(
            r.iter()
                .any(|i| matches!(i, DrawItem::Triangle { color, .. } if *color == 66))
        );
    }

    #[test]
    fn cloak_and_tint() {
        let mut look = LookParams::default();
        assert_eq!(ship_look(&look, Some(9)), (9, 1.0, true));
        assert_eq!(ship_look(&look, None).0, look.blend);
        look.cloaked = true;
        assert!(
            (ship_look(&look, None).1 - 1.0).abs() < 1e-9,
            "no extra fade: the game's alpha carries the cloak"
        );
        look.alpha = 0.3;
        assert!(
            (ship_look(&look, None).1 - 0.3).abs() < 1e-9,
            "the game's own fade kept"
        );
        let mut p = partner();
        assert!(p.targetable());
        p.look.cloaked = true;
        assert!(!p.targetable());
        p.look.cloaked = false;
        p.look.visible = false;
        assert!(!p.targetable());
    }
}
