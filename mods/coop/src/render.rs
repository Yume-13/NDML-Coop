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

//! Drawing: a display list ([`DrawItem`]) the pure code builds and the glue
//! turns into builtin calls in a GUI Draw event.
//!
//! The partner's ship is normally an avatar instance the game draws
//! (`world::entities`); without one the mod draws it with `draw_sprite_ext`
//! ([`plan_partner`], world position mapped to GUI coordinates).

#![forbid(unsafe_code)]

use ndml_net::interp::Vec2;

/// GameMaker colours are `0xBBGGRR`.
#[must_use]
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Palette (retro: cyan to magenta on near-black).
pub mod color {
    use super::rgb;
    pub const WHITE: u32 = rgb(235, 240, 255);
    pub const TEXT: u32 = rgb(200, 210, 230);
    pub const DIM: u32 = rgb(120, 130, 155);
    pub const FAINT: u32 = rgb(70, 78, 100);
    pub const CYAN: u32 = rgb(0, 230, 255);
    pub const MAGENTA: u32 = rgb(255, 60, 220);
    pub const GREEN: u32 = rgb(90, 255, 130);
    pub const AMBER: u32 = rgb(255, 190, 60);
    pub const RED: u32 = rgb(255, 85, 85);
    pub const PANEL: u32 = rgb(8, 10, 22);
    /// Partner label and marker.
    pub const PARTNER: u32 = MAGENTA;
}

/// Horizontal text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
}

/// A sprite to draw (all GUI coordinates).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShipDraw {
    pub sprite: u32,
    pub frame: f64,
    pub x: f64,
    pub y: f64,
    pub xscale: f64,
    pub yscale: f64,
    pub angle: f64,
    pub blend: u32,
    pub alpha: f64,
}

/// One drawing operation on the GUI layer.
#[derive(Debug, Clone, PartialEq)]
pub enum DrawItem {
    /// `draw_rectangle`.
    Rect {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: u32,
        alpha: f64,
        outline: bool,
    },
    /// `draw_text` (scale 1) or `draw_text_transformed`.
    Text {
        x: f64,
        y: f64,
        text: String,
        color: u32,
        scale: f64,
        align: Align,
    },
    /// `draw_triangle` (filled).
    Triangle {
        pts: [(f64, f64); 3],
        color: u32,
        alpha: f64,
    },
    /// `draw_sprite_ext` (checked with `sprite_exists` first).
    Sprite(ShipDraw),
}

/// The world rectangle the camera shows (the wrap-around arena).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The GUI layer size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gui {
    pub w: f64,
    pub h: f64,
}

impl View {
    /// World point to GUI point, plus the GUI-per-world scale.
    #[must_use]
    pub fn to_gui(&self, gui: Gui, x: f64, y: f64) -> (f64, f64, f64, f64) {
        let ok = self.w > 1.0 && self.h > 1.0 && gui.w > 1.0 && gui.h > 1.0;
        if !ok {
            return (x, y, 1.0, 1.0);
        }
        let (sx, sy) = (gui.w / self.w, gui.h / self.h);
        ((x - self.x) * sx, (y - self.y) * sy, sx, sy)
    }
}

/// How a partner should look (from its `PlayerLook`, or defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LookParams {
    pub xscale: f64,
    pub yscale: f64,
    pub blend: u32,
    pub alpha: f64,
    pub visible: bool,
    pub cloaked: bool,
}

impl Default for LookParams {
    fn default() -> Self {
        Self {
            xscale: 1.0,
            yscale: 1.0,
            blend: 0x00FF_FFFF,
            alpha: 1.0,
            visible: true,
            cloaked: false,
        }
    }
}

/// Everything about one partner ship this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct PartnerFrame<'a> {
    pub pos: Vec2,
    pub angle: f32,
    /// Sprite to draw (`None`: unknown, draw only the marker and label).
    pub sprite: Option<u32>,
    pub frame: u8,
    pub look: LookParams,
    pub name: &'a str,
}

/// Margin that keeps labels and markers on screen, GUI px.
const EDGE: f64 = 14.0;

/// The draw items for one partner: the ship (unless hidden), a name label and
/// a marker, both kept on screen.
#[must_use]
pub fn plan_partner(
    p: &PartnerFrame<'_>,
    view: Option<View>,
    gui: Gui,
    line_h: f64,
) -> Vec<DrawItem> {
    let lh = if line_h.is_finite() && line_h > 4.0 {
        line_h
    } else {
        16.0
    };
    // Marker half-size, from the font size.
    let m = (lh * 0.22).clamp(5.0, 24.0);
    let (gx, gy, sx, sy) = view.map_or((f64::from(p.pos.x), f64::from(p.pos.y), 1.0, 1.0), |v| {
        v.to_gui(gui, f64::from(p.pos.x), f64::from(p.pos.y))
    });
    let mut out = Vec::with_capacity(4);
    let on_screen = (0.0..=gui.w).contains(&gx) && (0.0..=gui.h).contains(&gy);
    if let (Some(sprite), true, true) = (p.sprite, p.look.visible, on_screen) {
        // The cloak is already in `image_alpha`: no extra fade.
        let alpha = p.look.alpha.clamp(0.0, 1.0);
        out.push(DrawItem::Sprite(ShipDraw {
            sprite,
            frame: f64::from(p.frame),
            x: gx,
            y: gy,
            xscale: p.look.xscale * sx,
            yscale: p.look.yscale * sy,
            angle: f64::from(p.angle),
            blend: p.look.blend,
            alpha,
        }));
    }
    let lift = 40.0 * sy.max(0.5) + m;
    let mx = gx.clamp(EDGE, (gui.w - EDGE).max(EDGE));
    let my = (gy - lift).clamp(EDGE, (gui.h - EDGE).max(EDGE));
    let tri = if on_screen {
        [(mx - m, my - m), (mx + m, my - m), (mx, my + m * 0.7)]
    } else {
        // At the edge, pointing toward the ship.
        let (dx, dy) = (gx - mx, gy - my);
        let len = dx.hypot(dy).max(1e-6);
        let (ux, uy) = (dx / len, dy / len);
        let (tx, ty) = (mx + ux * m * 1.2, my + uy * m * 1.2);
        let (bx, by) = (mx - ux * m * 0.7, my - uy * m * 0.7);
        [
            (tx, ty),
            (bx - uy * m, by + ux * m),
            (bx + uy * m, by - ux * m),
        ]
    };
    out.push(DrawItem::Triangle {
        pts: tri,
        color: color::PARTNER,
        alpha: 0.9,
    });
    let half = lh * 3.0;
    let label_x = gx.clamp(EDGE + half, (gui.w - EDGE - half).max(EDGE + half));
    let label_y = (my - m - lh).clamp(EDGE, (gui.h - EDGE - lh).max(EDGE));
    out.push(DrawItem::Text {
        x: label_x,
        y: label_y,
        text: p.name.to_owned(),
        color: color::PARTNER,
        scale: 1.0,
        align: Align::Center,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUI: Gui = Gui {
        w: 1920.0,
        h: 1080.0,
    };

    fn frame(x: f32, y: f32) -> PartnerFrame<'static> {
        PartnerFrame {
            pos: Vec2::new(x, y),
            angle: 45.0,
            sprite: Some(824),
            frame: 2,
            look: LookParams::default(),
            name: "Wing",
        }
    }

    fn sprite(items: &[DrawItem]) -> Option<ShipDraw> {
        items.iter().find_map(|i| match i {
            DrawItem::Sprite(s) => Some(*s),
            _ => None,
        })
    }

    fn label(items: &[DrawItem]) -> (f64, f64) {
        items
            .iter()
            .find_map(|i| match i {
                DrawItem::Text { x, y, .. } => Some((*x, *y)),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn colours_are_bgr() {
        assert_eq!(rgb(0xFF, 0, 0), 0x0000_00FF);
        assert_eq!(rgb(0, 0, 0xFF), 0x00FF_0000);
    }

    #[test]
    fn world_maps_to_gui_with_the_view_offset_and_scale() {
        let view = View {
            x: 100.0,
            y: 50.0,
            w: 960.0,
            h: 540.0,
        };
        let items = plan_partner(&frame(580.0, 320.0), Some(view), GUI, 16.0);
        let s = sprite(&items).unwrap();
        assert!((s.x - 960.0).abs() < 1e-9 && (s.y - 540.0).abs() < 1e-9);
        assert!((s.xscale - 2.0).abs() < 1e-9, "scaled with the view");
        assert_eq!(s.sprite, 824);
        assert!((s.frame - 2.0).abs() < 1e-9);
        // No usable view: world coordinates as they are.
        let bad = View { w: 0.0, ..view };
        let s = sprite(&plan_partner(&frame(5.0, 6.0), Some(bad), GUI, 16.0)).unwrap();
        assert!((s.x - 5.0).abs() < 1e-9);
    }

    #[test]
    fn label_and_marker_stay_on_screen_at_the_edges() {
        let view = View {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        for (x, y) in [(0.0, 0.0), (1919.0, 1079.0), (1919.0, 2.0), (3.0, 1079.0)] {
            let items = plan_partner(&frame(x, y), Some(view), GUI, 16.0);
            let (lx, ly) = label(&items);
            assert!(
                (0.0..=GUI.w).contains(&lx) && (0.0..=GUI.h).contains(&ly),
                "{lx},{ly}"
            );
            for i in &items {
                if let DrawItem::Triangle { pts, .. } = i {
                    for (px, py) in pts {
                        assert!(
                            (-1.0..=GUI.w + 1.0).contains(px) && (-1.0..=GUI.h + 1.0).contains(py),
                            "marker off screen at {px},{py} for ship {x},{y}"
                        );
                    }
                }
            }
        }
        // Off screen (view and GUI disagree): no sprite, but marker and label.
        let items = plan_partner(&frame(5000.0, 500.0), Some(view), GUI, 16.0);
        assert!(sprite(&items).is_none());
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn label_sits_above_the_ship_at_4k_font_sizes() {
        let gui = Gui {
            w: 3840.0,
            h: 2160.0,
        };
        let view = View {
            x: 0.0,
            y: 0.0,
            w: 2965.0,
            h: 1668.0,
        };
        let items = plan_partner(&frame(1500.0, 800.0), Some(view), gui, 70.0);
        let s = sprite(&items).unwrap();
        let (_, ly) = label(&items);
        assert!(
            ly + 70.0 < s.y - 40.0,
            "label bottom {} vs ship {}",
            ly + 70.0,
            s.y
        );
    }

    #[test]
    fn hidden_and_cloaked_partners() {
        let mut f = frame(500.0, 500.0);
        f.look.visible = false;
        assert!(sprite(&plan_partner(&f, None, GUI, 16.0)).is_none());
        f.look.visible = true;
        f.look.cloaked = true;
        f.look.alpha = 0.3;
        let s = sprite(&plan_partner(&f, None, GUI, 16.0)).unwrap();
        assert!(
            (s.alpha - 0.3).abs() < 1e-9,
            "the game's cloak alpha, no extra fade"
        );
        f.sprite = None;
        assert!(sprite(&plan_partner(&f, None, GUI, 16.0)).is_none());
    }
}
