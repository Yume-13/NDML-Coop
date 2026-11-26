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

//! Positions: turns the page and run views into [`Op`]s in the game's
//! layout. Pure; sizes come from a [`Measure`].

#![forbid(unsafe_code)]
// Geometry reads best with short names (f: frame, g: menu, m: measure).
#![allow(clippy::many_single_char_names)]

use super::menu::{EXTRA_ROW, INSERT_ROW};
use super::page::{LineKind, PageView};
use super::run::{Bar, RunUi};
use super::style::{Font, Frame, HAlign, Measure, Op, TextOp, VAlign, alpha, color};
use crate::render::DrawItem;

/// Row 0 is three pitches below `vPos` in `oMenu_Draw_64`.
pub const ROW_BASE: f64 = 3.0;
/// `oMenu_Draw_64` subtracts 4 from every row's y.
pub const ROW_DY: f64 = -4.0;

/// `oMenu`'s layout and state, read every frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuGeom {
    /// Rows' left edge.
    pub h_pos: f64,
    /// `oMenu.vPos` (row 0 is [`ROW_BASE`] pitches below it).
    pub v_pos: f64,
    /// Row pitch.
    pub ll: f64,
    /// The menu's alpha.
    pub alpha: f64,
    /// The YES/NO dialog (`newGameCheck`) is open.
    pub dialog: bool,
}

impl MenuGeom {
    /// The centre line of row `i`: `vPos + ll · (i + 3) - 4`.
    #[must_use]
    pub fn row_y(&self, i: f64) -> f64 {
        self.v_pos + self.ll * (i + ROW_BASE) + ROW_DY
    }

    /// The row whose band contains GUI `y` (row `i` spans `row_y(i) ± ll/2`).
    #[must_use]
    pub fn row_at(&self, y: f64) -> f64 {
        ((y - self.v_pos - ROW_DY) / self.ll - ROW_BASE + 0.5).floor()
    }

    fn page_alpha(&self) -> f64 {
        let a = self.alpha.clamp(0.0, 1.0);
        if self.dialog {
            a * alpha::BEHIND_DIALOG
        } else {
            a
        }
    }
}

/// The alpha for what the mod draws over the main menu: `oMenu.alpha`, faded
/// with the menu's fade from black (`oMenu.blackAlpha`).
#[must_use]
pub fn faded(alpha: f64, black: Option<f64>) -> f64 {
    let a = alpha.clamp(0.0, 1.0);
    match black {
        Some(b) if b.is_finite() => a * (1.0 - b.clamp(0.0, 1.0)),
        _ => a,
    }
}

/// `hud_corner`: where the link line goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HudCorner {
    /// Bottom right, mirroring the run timer.
    #[default]
    Right,
    /// Bottom left, above "Wave:" (shares the stack with boss kill times).
    Left,
}

/// The main menu's extras with the Co-op row: the row the mod draws at row
/// 11 (the old Exit) and the short link state after the "Co-op" label.
#[must_use]
pub fn menu_extras(
    g: &MenuGeom,
    row11: &str,
    coop_label: &str,
    status: Option<&str>,
    f: Frame,
    m: &dyn Measure,
) -> Vec<Op> {
    let a = g.page_alpha();
    #[allow(clippy::cast_precision_loss)]
    let y11 = g.row_y(EXTRA_ROW as f64);
    let mut out = vec![
        TextOp::new(Font::Large, g.h_pos, y11, row11)
            .alpha(a)
            .align(HAlign::Left, VAlign::Middle)
            .into(),
    ];
    if let Some(s) = status {
        #[allow(clippy::cast_precision_loss)]
        let y = g.row_y(INSERT_ROW as f64);
        let x = g.h_pos + m.width(Font::Large, coop_label) + 24.0 * f.gs;
        out.push(
            TextOp::new(Font::Small, x, y, s)
                .color(color::BLUE)
                .alpha(a)
                .align(HAlign::Left, VAlign::Middle)
                .into(),
        );
    }
    out
}

/// The page's secondary text wraps before this fraction of the width.
pub const COLUMN_RIGHT: f64 = 0.42;
/// Its `sGUIGradient` backdrop fades out at this fraction of the width.
pub const BACKDROP_RIGHT: f64 = 0.5;

/// What the mod draws for the page on top of the game's menu (the game draws
/// the enabled rows, the beam and its YES/NO dialog).
#[must_use]
pub fn page_ops(v: &PageView, g: &MenuGeom, f: Frame, m: &dyn Measure) -> Vec<Op> {
    if let Some(e) = &v.entry {
        return entry_ops(e, f, m);
    }
    let a = g.page_alpha();
    let mut out: Vec<Op> = vec![
        TextOp::new(Font::Heading, g.h_pos, g.row_y(0.0), &v.title)
            .color(color::BLUE)
            .alpha(a)
            .align(HAlign::Left, VAlign::Middle)
            .into(),
    ];
    let label_w = v
        .rows
        .iter()
        .map(|r| m.width(Font::Large, &r.label))
        .fold(0.0, f64::max);
    let value_x = g.h_pos + label_w + 48.0 * f.gs;
    let mut value_end = value_x;
    for (i, r) in v.rows.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let y = g.row_y((i + 1) as f64);
        let ra = if r.enabled { a } else { a * alpha::DISABLED };
        // The game draws rows 0..=10 only. Row 11 is the mod's, as Exit.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // EXTRA_ROW, a small row
        let mod_row = i + 1 >= EXTRA_ROW as usize;
        if !r.enabled || mod_row {
            out.push(
                TextOp::new(Font::Large, g.h_pos, y, &r.label)
                    .alpha(ra)
                    .align(HAlign::Left, VAlign::Middle)
                    .into(),
            );
        }
        if let Some(val) = &r.value {
            value_end = value_end.max(value_x + m.width(Font::Large, val));
            out.push(
                TextOp::new(Font::Large, value_x, y, val)
                    .color(color::BLUE)
                    .alpha(ra)
                    .align(HAlign::Left, VAlign::Middle)
                    .into(),
            );
        }
    }
    // The game's YES/NO dialog sits where the column is.
    if !g.dialog {
        #[allow(clippy::cast_precision_loss)]
        let below = v.rows.len() as f64;
        out.extend(column_ops(&v.column, below, value_end, a, g, f, m));
    }
    out
}

/// The page's secondary text under row `last_row`, in the menu's column over
/// a `sGUIGradient` backdrop, as Settings draws its descriptions.
fn column_ops(
    column: &[super::page::ColLine],
    last_row: f64,
    value_end: f64,
    a: f64,
    g: &MenuGeom,
    f: Frame,
    m: &dyn Measure,
) -> Vec<Op> {
    let col_x = g.h_pos;
    let col_w = (f.w * COLUMN_RIGHT - col_x).max(value_end - col_x);
    let first = g.row_y(last_row + 1.5) - g.ll / 2.0;
    let mut y = first;
    let bottom = f.h - 48.0 * f.gs;
    let mut out: Vec<Op> = Vec::new();
    'lines: for l in column {
        let (font, la, text) = match l.kind {
            LineKind::Gap => {
                y += m.line(Font::Medium) * 0.5;
                continue;
            }
            LineKind::Heading => (Font::Heading, a, super::text::upper(&l.text)),
            LineKind::Body | LineKind::Code => (Font::Medium, a, l.text.clone()),
            LineKind::RoomCode => (Font::XLarge, a, l.text.clone()),
            LineKind::Small => (Font::Small, a * alpha::SMALL_PRINT, l.text.clone()),
        };
        let lines = if matches!(l.kind, LineKind::Code | LineKind::RoomCode) {
            vec![text]
        } else {
            super::style::wrap(m, font, &text, col_w)
        };
        for part in lines {
            if y + m.line(font) > bottom {
                break 'lines;
            }
            out.push(
                TextOp::new(font, col_x, y, part)
                    .color(l.color())
                    .alpha(la)
                    .into(),
            );
            y += m.line(font) * 1.1;
        }
    }
    if y > first {
        // As Settings: at `top - ll / 4`, 0.5 × the page alpha, under the text.
        out.insert(
            0,
            Op::Backdrop {
                y: first - g.ll / 4.0,
                w: f.w * BACKDROP_RIGHT,
                h: y - first + g.ll / 2.0,
                alpha: a * alpha::DIM,
            },
        );
    }
    out
}

/// The high-score entry look: prompt in `fontMedium`, `[ text ]` in
/// `fontXLarge`, centred at a quarter of the height.
fn entry_ops(e: &super::page::EntryView, f: Frame, m: &dyn Measure) -> Vec<Op> {
    let cx = f.w / 2.0;
    let mut y = f.h * 0.25;
    let mut out: Vec<Op> = vec![
        TextOp::new(Font::Medium, cx, y, &e.prompt)
            .align(HAlign::Center, VAlign::Top)
            .into(),
    ];
    y += m.line(Font::Medium) * 1.6;
    // Long text shrinks to fit the screen.
    let w = m.width(Font::XLarge, &e.boxed);
    let scale = if w > f.w * 0.9 { f.w * 0.9 / w } else { 1.0 };
    out.push(
        TextOp::new(Font::XLarge, cx, y, &e.boxed)
            .align(HAlign::Center, VAlign::Top)
            .scale(scale)
            .into(),
    );
    y += m.line(Font::XLarge) * scale * 1.4;
    if let Some((t, tone)) = &e.check {
        out.push(
            TextOp::new(Font::Small, cx, y, t)
                .color(tone.color(u64::MAX))
                .align(HAlign::Center, VAlign::Top)
                .into(),
        );
    }
    y += m.line(Font::Small) * 1.6;
    out.push(
        TextOp::new(Font::Small, cx, y, &e.hint)
            .alpha(alpha::SMALL_PRINT)
            .align(HAlign::Center, VAlign::Top)
            .into(),
    );
    out
}

/// How the run pieces are drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunStyle {
    pub hud_corner: HudCorner,
    /// `oController.timeAlpha` (the HUD's fade-in).
    pub time_alpha: f64,
    /// A real wave alert is on screen: the co-op alert goes one line higher.
    pub wave_alert: bool,
    /// Seconds, for the pulse.
    pub t: f64,
    /// The pause screen is up: bars go beside or under its menu rows.
    pub paused: bool,
    /// The high-score table after a run is up (`oHighscoresDynamic`).
    pub table: bool,
    /// The pause menu's rows as read from the game (bars beside them);
    /// without it, one bar goes under the rows.
    pub pause_rows: Option<PauseRows>,
}

/// The pause menu's rows (`pauseMenuPositions`): centre, row 1's line, fade.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PauseRows {
    pub x: f64,
    pub first_y: f64,
    pub alpha: f64,
}

/// Bars beside the pause rows: left edge this far right of the rows' centre
/// (× `guiScale`, clear of labels and beam), and this far from the right edge.
pub const PAUSE_SIDE_GAP: f64 = 240.0;
pub const PAUSE_SIDE_MARGIN: f64 = 24.0;
/// Their spacing (× `guiScale`) and most bars (the stats start below the third).
pub const PAUSE_SIDE_STEP: f64 = 60.0;
pub const PAUSE_SIDE_BARS: f64 = 3.0;

/// `drawTextBox` draws `sTutorialBarWide` at this scale × `guiScale`.
pub const TEXTBOX_SCALE: f64 = 0.5;
/// `sTutorialBarWide`'s height, used when `sprite_get_height` can't be read.
pub const TEXTBOX_SPRITE_H: f64 = 116.0;

/// The hint bar's height on screen, GUI px.
#[must_use]
pub fn text_box_height(f: Frame, m: &dyn Measure) -> f64 {
    let h = m
        .text_box_sprite_height()
        .filter(|h| h.is_finite() && *h > 0.0)
        .unwrap_or(TEXTBOX_SPRITE_H);
    h * TEXTBOX_SCALE * f.gs
}

/// A hint bar's width on screen: `max(string_width(text), 500 × guiScale)`
/// plus its rounded ends.
#[must_use]
pub fn bar_width(text: &str, f: Frame, m: &dyn Measure) -> f64 {
    m.width(Font::Small, text).max(500.0 * f.gs) + 2.0 * BAR_END * f.gs
}

/// A hint bar's rounded end beyond its text width (× `guiScale`).
pub const BAR_END: f64 = 30.0;

/// The centre of a bar `width` wide beside the pause rows, kept on screen.
fn side_x(r: PauseRows, width: f64, f: Frame) -> f64 {
    let lo = r.x + PAUSE_SIDE_GAP * f.gs + width / 2.0;
    let hi = f.w - PAUSE_SIDE_MARGIN * f.gs - width / 2.0;
    lo.min(hi)
}

/// The centre of the bar under the pause menu's rows (× `guiScale`).
pub const PAUSE_BAR_Y: f64 = 299.0;
/// In a run, the lowest bar's centre above the bottom, clear of the hull and
/// shield bars (× `guiScale`).
pub const RUN_BAR_RISE: f64 = 143.0;
/// The alert's top and the bar's centre on the high-score table, × `guiScale`
/// above the bottom: in the gap between its statistics and its credits.
/// `oHighscoresDynamic_Draw_64` draws the credits from `h - 278 × guiScale`.
pub const TABLE_ALERT_TOP: f64 = 418.0;
/// See [`TABLE_ALERT_TOP`].
pub const TABLE_BAR_RISE: f64 = 338.0;

fn bar_ops(b: &Bar, (x, y): (f64, f64), style: RunStyle, f: Frame, m: &dyn Measure) -> Vec<Op> {
    let a = if b.pulse {
        0.75 + 0.25 * (style.t * 2.5).sin()
    } else {
        1.0
    };
    // As the game's own bars: the text `fa_middle` at the rounded middle of
    // the sprite's frame.
    let text_y = y.round();
    let h = text_box_height(f, m);
    vec![
        Op::TextBox {
            x,
            y: text_y - h / 2.0,
            text: b.text.clone(),
            alpha: a,
        },
        TextOp::new(Font::Small, x, text_y, &b.text)
            .alpha(a)
            .align(HAlign::Center, VAlign::Middle)
            .into(),
    ]
}

/// The hint bars: beside the pause rows (one under them when they could
/// not be read), or above the HUD in play.
fn pause_bars(ui: &RunUi, style: RunStyle, f: Frame, m: &dyn Measure, out: &mut Vec<Group>) {
    let rows = style.pause_rows.filter(|_| style.paused);
    let (mut bottom, mut centre) = (0.0_f64, 0.0_f64);
    for b in &ui.bars {
        let at = if b.centre {
            centre += 1.0;
            (f.w / 2.0, f.h / 2.0 + 60.0 * f.gs * (centre - 1.0))
        } else {
            bottom += 1.0;
            if rows.is_some() && bottom > PAUSE_SIDE_BARS {
                continue; // beside the rows only: the stats are below
            }
            match rows {
                Some(r) => (
                    side_x(r, bar_width(&b.text, f, m), f),
                    r.first_y + PAUSE_SIDE_STEP * f.gs * (bottom - 1.0),
                ),
                // Rows unread: only one bar fits above the stats.
                None if style.paused && bottom > 1.0 => continue,
                None if style.paused => (f.w / 2.0, PAUSE_BAR_Y * f.gs),
                None => (
                    f.w / 2.0,
                    f.h - if style.table {
                        TABLE_BAR_RISE
                    } else {
                        RUN_BAR_RISE
                    } * f.gs
                        - 60.0 * f.gs * (bottom - 1.0),
                ),
            }
        };
        out.push(Group {
            part: format!("bar {}", b.text),
            ops: bar_ops(b, at, style, f, m),
        });
    }
}

/// One piece of the run UI with its operations, by name ("bar `<text>`",
/// "alert `<text>`", "count `<label> <number>`", "card `<title>`", "hud").
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub part: String,
    pub ops: Vec<Op>,
}

/// Every in-run piece for this frame.
#[must_use]
pub fn run_ops(ui: &RunUi, style: RunStyle, f: Frame, m: &dyn Measure) -> Vec<Op> {
    run_groups(ui, style, f, m)
        .into_iter()
        .flat_map(|g| g.ops)
        .collect()
}

/// [`run_ops`] piece by piece, in drawing order.
#[must_use]
pub fn run_groups(ui: &RunUi, style: RunStyle, f: Frame, m: &dyn Measure) -> Vec<Group> {
    let mut groups = Vec::new();
    if let Some(c) = &ui.card {
        let mut out = Vec::new();
        out.push(Op::Dim { alpha: alpha::DIM });
        let cx = f.w / 2.0;
        let mut y = f.h * 0.4;
        out.push(
            TextOp::new(Font::Title, cx, y, &c.title)
                .align(HAlign::Center, VAlign::Middle)
                .into(),
        );
        y += m.line(Font::Title) * 0.9;
        out.push(
            TextOp::new(Font::Medium, cx, y, &c.subtitle)
                .align(HAlign::Center, VAlign::Middle)
                .into(),
        );
        if let Some(n) = &c.note {
            y += m.line(Font::Medium) * 1.3;
            out.push(
                TextOp::new(Font::Small, cx, y, n)
                    .alpha(alpha::SMALL_PRINT)
                    .align(HAlign::Center, VAlign::Middle)
                    .into(),
            );
        }
        groups.push(Group {
            part: format!("card {}", c.title),
            ops: out,
        });
    }
    pause_bars(ui, style, f, m, &mut groups);
    if let Some(c) = &ui.count {
        let mut out = Vec::new();
        count_ops(c, f, m, &mut out);
        groups.push(Group {
            part: format!("count {} {}", c.label, c.number),
            ops: out,
        });
    }
    // Never over the pause screen's text: there the bars say it.
    if let Some(a) = ui.alert.as_ref().filter(|_| !style.paused) {
        let lift = if style.wave_alert {
            m.line(Font::XLarge)
        } else {
            0.0
        };
        let top = if style.table { TABLE_ALERT_TOP } else { 275.0 };
        groups.push(Group {
            part: format!("alert {}", a.text),
            ops: vec![
                TextOp::new(Font::XLarge, f.w / 2.0, f.h - top * f.gs - lift, &a.text)
                    .alpha(a.alpha)
                    .scale(a.scale)
                    .align(HAlign::Center, VAlign::Top)
                    .into(),
            ],
        });
    }
    if let Some(h) = &ui.hud {
        let mut out = Vec::new();
        hud_ops(h, style, f, m, &mut out);
        groups.push(Group {
            part: "hud".into(),
            ops: out,
        });
    }
    groups
}

/// Rings of the count's edge glow, from the very edge inwards.
const EDGE_RINGS: u32 = 12;
/// The glow's depth, as a share of the GUI's short side.
const EDGE_DEPTH: f64 = 0.07;

/// A count: the edge glow, then its number and word on the panel or the strip.
fn count_ops(c: &super::run::CountShow, f: Frame, m: &dyn Measure, out: &mut Vec<Op>) {
    let fresh = (1.0 - c.phase).clamp(0.0, 1.0);
    if c.dim {
        count_dim_ops(f, out);
    }
    if c.edge {
        edge_ops(fresh, f, out);
    }
    if !c.panel {
        respawn_alert_ops(c, fresh, f, m, out);
        return;
    }
    count_panel_ops(c, fresh, f, m, out);
}

/// The count's edge glow: [`EDGE_RINGS`] rings of the four sides.
fn edge_ops(fresh: f64, f: Frame, out: &mut Vec<Op>) {
    let peak = 0.45 + 0.3 * fresh * fresh;
    let ring = f.w.min(f.h) * EDGE_DEPTH / f64::from(EDGE_RINGS);
    for i in 0..EDGE_RINGS {
        let k = 1.0 - f64::from(i) / f64::from(EDGE_RINGS);
        let a = peak * k * k;
        let o = f64::from(i) * ring;
        let rect = |x1: f64, y1: f64, x2: f64, y2: f64| Op::Rect {
            x1,
            y1,
            x2,
            y2,
            color: color::COUNT_EDGE,
            alpha: a,
            outline: false,
        };
        out.push(rect(o, o, f.w - o, o + ring));
        out.push(rect(o, f.h - o - ring, f.w - o, f.h - o));
        out.push(rect(o, o + ring, o + ring, f.h - o - ring));
        out.push(rect(f.w - o - ring, o + ring, f.w - o, f.h - o - ring));
    }
}

/// The resume and start counts' number and word on the middle panel.
fn count_panel_ops(
    c: &super::run::CountShow,
    fresh: f64,
    f: Frame,
    m: &dyn Measure,
    out: &mut Vec<Op>,
) {
    let num = c.number.to_string();
    let lh = m.line(Font::Title);
    let nh = m.line(Font::XXLarge) * COUNT_SCALE;
    let cx = f.w / 2.0;
    let cy = f.h * 0.47;
    let top = cy - nh * 0.5 - lh * 1.3;
    let bottom = cy + nh * 0.55;
    let half = (m
        .width(Font::Title, c.label)
        .max(m.width(Font::XXLarge, &num) * COUNT_SCALE)
        * 0.5
        + lh)
        .max((bottom - top) * 0.6);
    out.push(Op::Rect {
        x1: cx - half,
        y1: top,
        x2: cx + half,
        y2: bottom,
        color: color::BLACK,
        alpha: COUNT_PANEL_ALPHA,
        outline: false,
    });
    out.push(
        TextOp::new(Font::Title, cx, top + lh * 0.25, c.label)
            .align(HAlign::Center, VAlign::Top)
            .into(),
    );
    out.push(
        TextOp::new(Font::XXLarge, cx, cy, num)
            .scale(COUNT_SCALE * (1.0 + 0.25 * fresh.powi(3)))
            .align(HAlign::Center, VAlign::Middle)
            .into(),
    );
}

/// The pause screen dimmed under its rows while the resume count shows.
fn count_dim_ops(f: Frame, out: &mut Vec<Op>) {
    let soft = f.h * COUNT_DIM_SOFT / f64::from(COUNT_DIM_STEPS);
    let from = f.h * COUNT_DIM_FROM;
    for i in 0..COUNT_DIM_STEPS {
        let y = from + f64::from(i) * soft;
        out.push(Op::Rect {
            x1: 0.0,
            y1: y,
            x2: f.w,
            y2: y + soft,
            color: color::BLACK,
            alpha: COUNT_DIM_ALPHA * f64::from(i + 1) / f64::from(COUNT_DIM_STEPS + 1),
            outline: false,
        });
    }
    out.push(Op::Rect {
        x1: 0.0,
        y1: from + f.h * COUNT_DIM_SOFT,
        x2: f.w,
        y2: f.h,
        color: color::BLACK,
        alpha: COUNT_DIM_ALPHA,
        outline: false,
    });
}

/// The respawn alert: BACK IN and the number on an upper strip, outlined.
fn respawn_alert_ops(
    c: &super::run::CountShow,
    fresh: f64,
    f: Frame,
    m: &dyn Measure,
    out: &mut Vec<Op>,
) {
    let num = c.number.to_string();
    let nh = m.line(Font::XXLarge) * ALERT_SCALE;
    let lw = m.width(Font::Title, c.label);
    let nw = m.width(Font::XXLarge, &num) * ALERT_SCALE;
    let gap = m.line(Font::Title);
    let total = lw + gap + nw;
    let cx = f.w / 2.0;
    let top = f.h * ALERT_TOP;
    let cy = top + nh * 0.55;
    let pad = gap;
    out.push(Op::Rect {
        x1: cx - total / 2.0 - pad,
        y1: top,
        x2: cx + total / 2.0 + pad,
        y2: top + nh * 1.1,
        color: color::BLACK,
        alpha: ALERT_STRIP_ALPHA,
        outline: false,
    });
    let label =
        TextOp::new(Font::Title, cx - total / 2.0, cy, c.label).align(HAlign::Left, VAlign::Middle);
    let number = TextOp::new(Font::XXLarge, cx + total / 2.0 - nw / 2.0, cy, num)
        .scale(ALERT_SCALE * (1.0 + 0.2 * fresh.powi(3)))
        .align(HAlign::Center, VAlign::Middle);
    let o = (2.0 * f.gs).max(1.0);
    for t in [label, number] {
        for (dx, dy) in [(-o, 0.0), (o, 0.0), (0.0, -o), (0.0, o)] {
            let mut shade = t.clone();
            shade.x += dx;
            shade.y += dy;
            shade.color = color::BLACK;
            shade.alpha = 0.85;
            out.push(shade.into());
        }
        out.push(t.into());
    }
}

/// The count's number: the high-score entry's font, this many times over.
const COUNT_SCALE: f64 = 2.5;
/// The panel behind the count (the game's dimmer is 0.5).
const COUNT_PANEL_ALPHA: f64 = 0.8;
/// The pause screen dimmed under its rows while the count shows: from this
/// share of the height down, at this alpha.
const COUNT_DIM_FROM: f64 = 0.21;
const COUNT_DIM_ALPHA: f64 = 0.5;
/// The dim's top edge: faded in over this share of the height, in steps.
const COUNT_DIM_SOFT: f64 = 0.05;
const COUNT_DIM_STEPS: u32 = 8;
/// The respawn alert's number (the high-score entry's font, scaled).
const ALERT_SCALE: f64 = 1.5;
/// The respawn alert's strip: its top as a share of the height, and its alpha.
/// The game's LEVEL UP banner reaches 15 % of the height.
const ALERT_TOP: f64 = 0.19;
const ALERT_STRIP_ALPHA: f64 = 0.35;

/// P3: the HUD line, and the traffic line one line above it (`show_traffic`).
fn hud_ops(h: &super::run::Hud, style: RunStyle, f: Frame, m: &dyn Measure, out: &mut Vec<Op>) {
    let a = hud_alpha(h, style.time_alpha, style.paused);
    let detail_color = if h.bad { color::NEGATIVE } else { color::WHITE };
    let gap = 16.0 * f.gs;
    let above = m.line(Font::Small);
    match style.hud_corner {
        HudCorner::Right => {
            let x = f.w - 18.0 * f.gs;
            let y = f.h - 48.0 * f.gs;
            let mut name_x = x;
            if let Some(d) = &h.detail {
                out.push(
                    TextOp::new(Font::Small, x, y, d)
                        .color(detail_color)
                        .alpha(a)
                        .align(HAlign::Right, VAlign::Top)
                        .into(),
                );
                name_x = x - m.width(Font::Small, d) - gap;
            }
            out.push(
                TextOp::new(Font::Small, name_x, y, &h.name)
                    .alpha(a)
                    .align(HAlign::Right, VAlign::Top)
                    .into(),
            );
            if let Some(t) = &h.traffic {
                out.push(
                    TextOp::new(Font::Small, x, y - above, t)
                        .alpha(a)
                        .align(HAlign::Right, VAlign::Top)
                        .into(),
                );
            }
        }
        HudCorner::Left => {
            let x = 18.0 * f.gs;
            let y = f.h - 108.0 * f.gs;
            out.push(TextOp::new(Font::Small, x, y, &h.name).alpha(a).into());
            if let Some(d) = &h.detail {
                out.push(
                    TextOp::new(Font::Small, x + m.width(Font::Small, &h.name) + gap, y, d)
                        .color(detail_color)
                        .alpha(a)
                        .into(),
                );
            }
            if let Some(t) = &h.traffic {
                out.push(TextOp::new(Font::Small, x, y - above, t).alpha(a).into());
            }
        }
    }
}

/// The HUD line's alpha. In play the ordinary line follows the game's run
/// timer (`timeAlpha`, which stays 0 unless the speedrun timer option is
/// on); a warning and the pause screen show it regardless.
fn hud_alpha(h: &super::run::Hud, time_alpha: f64, paused: bool) -> f64 {
    alpha::SMALL_PRINT
        * if h.bad || paused {
            1.0
        } else {
            time_alpha.clamp(0.0, 1.0)
        }
}

/// The partner's ship, label and marker as ops (labels in `fontSmall` at 0.8
/// alpha). On the pause screen labels and markers are left out so they do
/// not cover the pause menu; the ship stays.
#[must_use]
pub fn partner_ops(items: Vec<DrawItem>, paused: bool) -> Vec<Op> {
    items
        .into_iter()
        .filter(|i| !(paused && matches!(i, DrawItem::Text { .. } | DrawItem::Triangle { .. })))
        .map(|i| match i {
            DrawItem::Text {
                x, y, text, color, ..
            } => TextOp::new(Font::Small, x, y, text)
                .color(color)
                .alpha(alpha::LABEL)
                .align(HAlign::Center, VAlign::Top)
                .into(),
            DrawItem::Triangle { pts, color, .. } => Op::Triangle {
                pts,
                color,
                alpha: alpha::LABEL,
            },
            DrawItem::Sprite(s) => Op::Sprite(s),
            DrawItem::Rect {
                x1,
                y1,
                x2,
                y2,
                color,
                alpha,
                outline,
            } => Op::Rect {
                x1,
                y1,
                x2,
                y2,
                color,
                alpha,
                outline,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::page::{ColLine, EntryView, PageId, RowId, RowView, Tone};

    #[test]
    fn no_partner_label_or_marker_over_the_pause_screen() {
        let items = || {
            vec![
                DrawItem::Text {
                    x: 10.0,
                    y: 10.0,
                    text: "Ace".into(),
                    color: 0xFF_FFFF,
                    scale: 1.0,
                    align: crate::render::Align::Center,
                },
                DrawItem::Triangle {
                    pts: [(0.0, 0.0), (4.0, 0.0), (2.0, 3.0)],
                    color: 0xFF_FFFF,
                    alpha: 1.0,
                },
                DrawItem::Rect {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 4.0,
                    y2: 4.0,
                    color: 0,
                    alpha: 1.0,
                    outline: false,
                },
            ]
        };
        assert_eq!(partner_ops(items(), false).len(), 3);
        let paused = partner_ops(items(), true);
        assert_eq!(paused.len(), 1, "only the ship's own drawing stays");
        assert!(matches!(paused[0], Op::Rect { .. }));
    }
    use crate::ui::run::{AlertView, Card, Hud};
    use crate::ui::style::tests::Fake;

    const F: Frame = Frame {
        w: 1920.0,
        h: 1080.0,
        gs: 1.0,
    };
    const G: MenuGeom = MenuGeom {
        h_pos: 120.0,
        v_pos: 300.0,
        ll: 60.0,
        alpha: 1.0,
        dialog: false,
    };

    fn texts(ops: &[Op]) -> Vec<&TextOp> {
        ops.iter()
            .filter_map(|o| match o {
                Op::Text(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    fn view() -> PageView {
        PageView {
            page: PageId::Main,
            title: "CO-OP".into(),
            rows: vec![
                RowView {
                    id: RowId::HostGame,
                    label: "Host Game".into(),
                    value: None,
                    enabled: false,
                },
                RowView {
                    id: RowId::ShipColour,
                    label: "Ship Colour".into(),
                    value: Some("Gold".into()),
                    enabled: true,
                },
                RowView {
                    id: RowId::Back,
                    label: "Back".into(),
                    value: None,
                    enabled: true,
                },
            ],
            column: vec![
                ColLine::heading("Steam"),
                ColLine::body(&"word ".repeat(80), Tone::Info),
                ColLine::gap(),
                ColLine::small("small print", Tone::Info),
            ],
            entry: None,
            confirm: None,
        }
    }

    #[test]
    fn menu_extras_draw_row_11_and_the_status() {
        let ops = menu_extras(&G, "Exit", "Co-op", Some("with Sam"), F, &Fake);
        let t = texts(&ops);
        assert_eq!(t[0].text, "Exit");
        assert!(
            (t[0].y - (300.0 + 60.0 * 14.0 - 4.0)).abs() < 1e-9,
            "row 11 = vPos + ll * (11 + 3) - 4"
        );
        assert!((G.row_at(t[0].y) - 11.0).abs() < 1e-9);
        assert!((G.row_at(t[0].y + 29.0) - 11.0).abs() < 1e-9);
        assert!((G.row_at(t[0].y + 31.0) - 12.0).abs() < 1e-9);
        assert_eq!(
            (t[0].font, t[0].h, t[0].v),
            (Font::Large, HAlign::Left, VAlign::Middle)
        );
        assert_eq!(t[1].color, color::BLUE);
        assert_eq!(t[1].font, Font::Small);
        assert!((t[1].y - G.row_y(2.0)).abs() < 1e-9, "on the Co-op row (2)");
        assert!(t[1].x > 120.0 + Fake.width(Font::Large, "Co-op"));
        let dim = MenuGeom { dialog: true, ..G };
        let ops = menu_extras(&dim, "Exit", "Co-op", None, F, &Fake);
        let t = texts(&ops);
        assert_eq!(t.len(), 1);
        assert!((t[0].alpha - alpha::BEHIND_DIALOG).abs() < 1e-9);
    }

    #[test]
    fn a_page_of_eleven_rows_draws_its_last_on_row_11() {
        let mut v = view();
        let row = |label: &str| RowView {
            id: RowId::WildMods,
            label: label.into(),
            value: None,
            enabled: true,
        };
        v.rows = (1..=11).map(|i| row(&format!("Row {i}"))).collect();
        v.rows[10].value = Some("Sync".into());
        let ops = page_ops(&v, &G, F, &Fake);
        let t = texts(&ops);
        let last = t.iter().find(|x| x.text == "Row 11").expect("row 11 drawn");
        assert!((last.y - G.row_y(11.0)).abs() < 1e-9);
        assert!((last.alpha - 1.0).abs() < 1e-9, "enabled: full alpha");
        assert!(
            !t.iter().any(|x| x.text == "Row 10"),
            "rows 1..=10 are the game's"
        );
        assert!(t.iter().any(|x| x.text == "Sync"), "its value too");
    }

    #[test]
    fn extras_follow_the_menu_fade_from_black() {
        assert!((faded(1.0, None) - 1.0).abs() < 1e-12);
        assert!((faded(0.8, Some(1.0))).abs() < 1e-12, "fully black");
        assert!((faded(1.0, Some(0.25)) - 0.75).abs() < 1e-12);
        assert!((faded(1.0, Some(f64::NAN)) - 1.0).abs() < 1e-12);
        let g = MenuGeom {
            alpha: faded(1.0, Some(0.6)),
            ..G
        };
        let ops = menu_extras(&g, "Exit", "Co-op", None, F, &Fake);
        assert!((texts(&ops)[0].alpha - 0.4).abs() < 1e-12);
    }

    #[test]
    fn page_ops_native_draws_only_heading_dim_rows_values_and_column() {
        let ops = page_ops(&view(), &G, F, &Fake);
        let t = texts(&ops);
        assert_eq!(t[0].text, "CO-OP");
        assert_eq!((t[0].font, t[0].color), (Font::Heading, color::BLUE));
        let host = t.iter().find(|x| x.text == "Host Game").unwrap();
        assert!(
            (host.alpha - alpha::DISABLED).abs() < 1e-9,
            "dim row drawn by the mod"
        );
        assert!(
            !t.iter().any(|x| x.text == "Back"),
            "the game draws enabled rows"
        );
        let gold = t.iter().find(|x| x.text == "Gold").unwrap();
        assert_eq!((gold.font, gold.color), (Font::Large, color::BLUE));
        assert!(gold.x > 120.0 + Fake.width(Font::Large, "Ship Colour"));
        let col: Vec<_> = t.iter().filter(|x| x.text.starts_with("word")).collect();
        assert!(col.len() > 1, "wrapped");
        for c in &col {
            assert!((c.x - 120.0).abs() < 1e-9, "in the menu's column");
            assert!(c.y > G.row_y(3.0) + G.ll / 2.0, "under the last row");
            assert!(c.x + Fake.width(Font::Medium, &c.text) <= F.w * COLUMN_RIGHT + 1e-9);
        }
        let backdrop = ops
            .iter()
            .find_map(|o| match o {
                Op::Backdrop { y, h, alpha, .. } => Some((*y, *h, *alpha)),
                _ => None,
            })
            .unwrap();
        let last = col.iter().map(|c| c.y).fold(0.0, f64::max);
        assert!(backdrop.0 < col[0].y && backdrop.0 + backdrop.1 > last);
        assert!((backdrop.2 - 0.5).abs() < 1e-9, "0.5 x the page alpha");
        assert!(t.iter().any(|x| x.text == "STEAM"), "headings upper case");
        let small = t.iter().find(|x| x.text == "small print").unwrap();
        assert!((small.alpha - alpha::SMALL_PRINT).abs() < 1e-9);
        // Behind the game's YES/NO dialog the column steps aside; the rows
        // and values stay, dimmed.
        let dialog = MenuGeom { dialog: true, ..G };
        let ops = page_ops(&view(), &dialog, F, &Fake);
        let t = texts(&ops);
        assert!(
            !t.iter()
                .any(|x| x.text == "STEAM" || x.text.starts_with("word"))
        );
        assert!(t.iter().any(|x| x.text == "Gold"));
        assert!(
            !t.iter().any(|x| x.text == "Back"),
            "the game draws the question and its rows"
        );
    }

    #[test]
    fn entry_replaces_the_page() {
        let mut v = view();
        v.entry = Some(EntryView {
            prompt: "Enter the host's address (ip:port)".into(),
            boxed: "[ 192.168.1.2_ ]".into(),
            check: Some(("Host at 192.168.1.2:7777".into(), Tone::Good)),
            hint: "Enter: done".into(),
        });
        let ops = page_ops(&v, &G, F, &Fake);
        let t = texts(&ops);
        assert_eq!(t.len(), 4);
        assert!((t[0].y - 270.0).abs() < 1e-9 && t[0].h == HAlign::Center);
        assert_eq!(t[1].font, Font::XLarge);
        assert_eq!(t[2].color, color::BLUE);
        // A very long entry shrinks.
        v.entry.as_mut().unwrap().boxed = format!("[ {} ]", "x".repeat(200));
        let t2 = page_ops(&v, &G, F, &Fake);
        assert!(texts(&t2)[1].scale < 1.0);
    }

    #[test]
    fn the_count_stands_on_a_panel_in_the_middle() {
        let f = Frame {
            w: 1920.0,
            h: 1080.0,
            gs: 1.0,
        };
        let c = crate::ui::run::CountShow {
            number: 3,
            label: "RESUMING",
            phase: 0.0,
            edge: true,
            panel: true,
            dim: true,
        };
        let mut out = Vec::new();
        count_ops(&c, f, &Fake, &mut out);
        // The dim (its soft edge and the rest), 12 rings of 4 sides, the
        // panel, the word, the number.
        let dim = COUNT_DIM_STEPS as usize + 1;
        assert_eq!(out.len(), dim + 12 * 4 + 3);
        let panel = &out[dim + 48];
        assert!(
            matches!(panel, Op::Rect { color: color::BLACK, x1, x2, .. } if *x1 < 960.0 && *x2 > 960.0)
        );
        // The start count on the new game screen: the same without the dim.
        let mut start = Vec::new();
        count_ops(
            &crate::ui::run::CountShow {
                label: "STARTING",
                dim: false,
                ..c.clone()
            },
            f,
            &Fake,
            &mut start,
        );
        assert_eq!(start.len(), 12 * 4 + 3);
        assert_eq!(start[48], out[dim + 48], "the same panel");
        let mut out = Vec::new();
        count_ops(
            &crate::ui::run::CountShow {
                panel: false,
                dim: false,
                ..c
            },
            f,
            &Fake,
            &mut out,
        );
        // The respawn alert: the glow, a strip, two texts in 4 outline shades.
        assert_eq!(out.len(), 12 * 4 + 1 + 2 * 5, "the edge, no panel, no dim");
        assert_eq!(out[..48], start[..48], "the same edge");
        assert!(
            out.iter().all(|o| !matches!(o, Op::Rect { color: color::BLACK, alpha, .. } if (*alpha - COUNT_PANEL_ALPHA).abs() < 1e-9)),
            "no middle panel"
        );
        let Op::Rect { y1, y2, .. } = &out[48] else {
            panic!("{:?}", out[48])
        };
        assert!(
            *y1 >= f.h * 0.14 && *y2 < f.h * 0.35,
            "the upper part, clear of the middle"
        );
    }

    #[test]
    fn run_ops_place_each_primitive() {
        let ui = RunUi {
            count: None,
            pause: None,
            bars: vec![
                Bar {
                    text: "one".into(),
                    pulse: false,
                    centre: false,
                },
                Bar {
                    text: "two".into(),
                    pulse: true,
                    centre: false,
                },
            ],
            hud: Some(Hud {
                name: "Sam".into(),
                detail: Some("210 ms".into()),
                bad: true,
                traffic: None,
            }),
            card: Some(Card {
                title: "WAITING".into(),
                subtitle: "Waiting for Sam...".into(),
                note: None,
            }),
            alert: Some(AlertView {
                text: "SAM LEFT".into(),
                scale: 0.5,
                alpha: 1.0,
            }),
        };
        let style = RunStyle {
            hud_corner: HudCorner::Right,
            time_alpha: 1.0,
            wave_alert: false,
            t: 0.0,
            paused: false,
            table: false,
            pause_rows: None,
        };
        let ops = run_ops(&ui, style, F, &Fake);
        assert!(matches!(ops[0], Op::Dim { alpha } if (alpha - 0.5).abs() < 1e-9));
        let boxes: Vec<_> = ops
            .iter()
            .filter_map(|o| match o {
                Op::TextBox { y, .. } => Some(*y),
                _ => None,
            })
            .collect();
        assert_eq!(
            boxes,
            vec![1080.0 - 143.0 - 29.0, 1080.0 - 203.0 - 29.0],
            "stacked 60 px up; the text's line on the middle of the bar's frame"
        );
        let t = texts(&ops);
        let one = t.iter().find(|x| x.text == "one").unwrap();
        assert!((one.y - (1080.0 - 143.0)).abs() < 1e-9 && one.v == VAlign::Middle);
        // On the pause screen bars go under its menu rows.
        let p = run_ops(
            &ui,
            RunStyle {
                paused: true,
                ..style
            },
            F,
            &Fake,
        );
        let one = texts(&p).into_iter().find(|x| x.text == "one").unwrap();
        assert!((one.y - PAUSE_BAR_Y).abs() < 1e-9);
        let alert = t.iter().find(|x| x.text == "SAM LEFT").unwrap();
        assert!((alert.y - (1080.0 - 275.0)).abs() < 1e-9 && (alert.scale - 0.5).abs() < 1e-9);
        let ms = t.iter().find(|x| x.text == "210 ms").unwrap();
        assert_eq!((ms.color, ms.h), (color::NEGATIVE, HAlign::Right));
        assert!((ms.alpha - 0.65).abs() < 1e-9);
        let name = t.iter().find(|x| x.text == "Sam").unwrap();
        assert!(name.x < ms.x);
        // Wave alert up: the co-op alert goes higher; left corner.
        let style = RunStyle {
            hud_corner: HudCorner::Left,
            wave_alert: true,
            ..style
        };
        let ops = run_ops(&ui, style, F, &Fake);
        let t = texts(&ops);
        assert!(t.iter().find(|x| x.text == "SAM LEFT").unwrap().y < 1080.0 - 275.0);
        assert!(t.iter().find(|x| x.text == "Sam").unwrap().x < 100.0);
    }

    const WAIT_BAR: &str = "Lost the link to Sam. Leave Co-op Run returns to the menu.";

    /// The pause screen of a frozen game: two bars, an alert.
    fn waiting_pause_ui() -> RunUi {
        let pause = crate::ui::run::PauseText {
            row0: Some("Waiting for Sam...".into()),
            row0_blocked: true,
            row0_choose: false,
            quit: Some(("Leave Co-op Run".into(), "?".into())),
        };
        RunUi {
            pause: Some(pause),
            bars: vec![
                Bar {
                    text: WAIT_BAR.into(),
                    pulse: true,
                    centre: false,
                },
                Bar {
                    text: "Sam is paused too. Play resumes when you have both resumed.".into(),
                    pulse: false,
                    centre: false,
                },
            ],
            alert: Some(AlertView {
                text: "CONNECTION LOST".into(),
                scale: 1.0,
                alpha: 1.0,
            }),
            ..RunUi::default()
        }
    }

    /// Bars beside the pause rows stay clear of them, the screen's edge and
    /// the stats (from 330 × `guiScale`); no alert over the pause screen.
    #[test]
    fn the_pause_rows_put_the_bars_beside_them() {
        let ui = waiting_pause_ui();
        // The GUI is at least 2286 × `guiScale` wide whatever the shape.
        for (w, h) in [(2286.0, 1286.0), (2286.0, 1715.0), (3000.0, 1286.0)] {
            let gs = 1.0;
            let f = Frame { w, h, gs };
            let rows = PauseRows {
                x: w / 2.0,
                first_y: 117.0 * gs,
                alpha: 0.8,
            };
            let style = RunStyle {
                hud_corner: HudCorner::Right,
                time_alpha: 1.0,
                wave_alert: false,
                t: 0.0,
                paused: true,
                table: false,
                pause_rows: Some(rows),
            };
            let ops = run_ops(&ui, style, f, &Fake);
            let t = texts(&ops);
            assert!(t.iter().all(|x| x.text != "CONNECTION LOST"), "no alert");
            for b in &ui.bars {
                let x = t.iter().find(|x| x.text == b.text).unwrap();
                let half = bar_width(&b.text, f, &Fake) / 2.0;
                let left = x.x - half;
                assert!(
                    left >= rows.x + 130.0 * gs,
                    "{w}: clear of the rows ({left})"
                );
                assert!(x.x + half <= w + 1e-9, "{w}: on screen");
                assert!(x.y + 29.0 * gs <= 330.0 * gs, "{w}: above the stats");
            }
        }
    }

    /// Bars stay off the stats; with the rows unread, one bar under them.
    #[test]
    fn four_pause_rows_keep_the_bars_off_the_stats() {
        let four = waiting_pause_ui();
        let style = RunStyle {
            hud_corner: HudCorner::Right,
            time_alpha: 1.0,
            wave_alert: false,
            t: 0.0,
            paused: true,
            table: false,
            pause_rows: Some(PauseRows {
                x: 1143.0,
                first_y: 117.0,
                alpha: 1.0,
            }),
        };
        let f = Frame {
            w: 2286.0,
            h: 1286.0,
            gs: 1.0,
        };
        let ops = run_ops(&four, style, f, &Fake);
        let t = texts(&ops);
        for b in &four.bars {
            let x = t.iter().find(|x| x.text == b.text).unwrap();
            assert!(x.x - bar_width(&b.text, f, &Fake) / 2.0 >= 1143.0 + 130.0);
            assert!(x.y + 29.0 <= 330.0, "above the stats");
        }
        // The rows unread: one bar under them, no second on the stats.
        let ops = run_ops(
            &four,
            RunStyle {
                table: false,
                pause_rows: None,
                ..style
            },
            F,
            &Fake,
        );
        let t = texts(&ops);
        let one = t.iter().find(|x| x.text == WAIT_BAR).unwrap();
        assert!((one.y - PAUSE_BAR_Y).abs() < 1e-9 && (one.x - 960.0).abs() < 1e-9);
        assert!(
            t.iter().all(|x| x.text != four.bars[1].text),
            "one bar only"
        );
    }

    /// With the run timer hidden (`timeAlpha` 0) only a warning shows in play.
    #[test]
    fn a_link_warning_shows_without_the_games_timer() {
        let style = RunStyle {
            hud_corner: HudCorner::Right,
            time_alpha: 0.0,
            wave_alert: false,
            t: 0.0,
            paused: false,
            table: false,
            pause_rows: None,
        };
        let line = |hud: Hud| {
            let ui = RunUi {
                hud: Some(hud),
                ..RunUi::default()
            };
            texts(&run_ops(&ui, style, F, &Fake))
                .into_iter()
                .map(|x| (x.text.clone(), x.alpha))
                .collect::<Vec<_>>()
        };
        let ordinary = line(Hud {
            name: "Sam".into(),
            detail: Some("42 ms".into()),
            bad: false,
            traffic: None,
        });
        assert!(ordinary.iter().all(|(_, a)| *a == 0.0), "{ordinary:?}");
        // On the pause screen the ordinary line shows whatever the timer.
        let ui = RunUi {
            hud: Some(Hud {
                name: "Sam".into(),
                detail: Some("42 ms".into()),
                bad: false,
                traffic: None,
            }),
            ..RunUi::default()
        };
        let ops = run_ops(
            &ui,
            RunStyle {
                paused: true,
                ..style
            },
            F,
            &Fake,
        );
        let paused: Vec<f64> = texts(&ops).iter().map(|x| x.alpha).collect();
        assert!(
            paused.len() == 2 && paused.iter().all(|a| (a - 0.65).abs() < 1e-9),
            "{paused:?}"
        );
        for warn in [
            crate::ui::run::hud_no_signal("Sam"),
            crate::ui::run::hud_reconnecting("Sam"),
        ] {
            let got = line(warn);
            assert_eq!(got.len(), 2);
            assert!(got.iter().all(|(_, a)| (*a - 0.65).abs() < 1e-9), "{got:?}");
        }
    }

    /// One `fontSmall` line above, in the same corner, font and alpha.
    #[test]
    fn the_traffic_line_sits_above_the_hud_line_in_both_corners() {
        let mut h = crate::ui::run::hud("Sam", Some(42.0), Some(0.0));
        let traffic = crate::ui::run::traffic_text(48.4, 7.25);
        assert_eq!(traffic, "48 KB/s up  7.2 KB/s down");
        h.traffic = Some(traffic.clone());
        let ui = RunUi {
            hud: Some(h),
            ..RunUi::default()
        };
        for corner in [HudCorner::Right, HudCorner::Left] {
            let style = RunStyle {
                hud_corner: corner,
                time_alpha: 1.0,
                wave_alert: false,
                t: 0.0,
                paused: false,
                table: false,
                pause_rows: None,
            };
            let ops = run_ops(&ui, style, F, &Fake);
            let t = texts(&ops);
            assert_eq!(t.len(), 3, "{corner:?}");
            let line = t.iter().find(|x| x.text == traffic).unwrap();
            let name = t.iter().find(|x| x.text == "Sam").unwrap();
            let ms = t.iter().find(|x| x.text == "42 ms").unwrap();
            assert!(
                (name.y - line.y - Fake.line(Font::Small)).abs() < 1e-9,
                "{corner:?}: one line above"
            );
            assert_eq!(line.h, ms.h, "{corner:?}: the same alignment");
            assert!((line.alpha - name.alpha).abs() < 1e-9);
            assert_eq!(line.font, Font::Small);
            match corner {
                HudCorner::Right => assert!((line.x - ms.x).abs() < 1e-9, "right edge"),
                HudCorner::Left => assert!((line.x - name.x).abs() < 1e-9, "left edge"),
            }
        }
        // `show_traffic` off: the same HUD line, nothing above it.
        let mut ui = RunUi {
            hud: Some(crate::ui::run::hud("Sam", Some(42.0), Some(0.0))),
            ..RunUi::default()
        };
        let off = crate::ui::run::Switches::default();
        crate::ui::run::add_traffic(&mut ui, off, Some((48.0, 21.0)));
        assert_eq!(ui.hud.as_ref().unwrap().traffic, None);
        let on = crate::ui::run::Switches {
            traffic: true,
            ..off
        };
        crate::ui::run::add_traffic(&mut ui, on, None);
        assert_eq!(
            ui.hud.as_ref().unwrap().traffic,
            None,
            "no numbers unlinked"
        );
        crate::ui::run::add_traffic(&mut ui, on, Some((48.0, 21.0)));
        assert_eq!(
            ui.hud.as_ref().unwrap().traffic.as_deref(),
            Some("48 KB/s up  21 KB/s down")
        );
        let mut none = RunUi::default();
        crate::ui::run::add_traffic(&mut none, on, Some((1.0, 1.0)));
        assert!(none.hud.is_none(), "only with a HUD line");
    }

    /// The fake, with the game reporting its own bar sprite height.
    struct WithBar(f64);

    impl Measure for WithBar {
        fn width(&self, font: Font, text: &str) -> f64 {
            Fake.width(font, text)
        }
        fn line(&self, font: Font) -> f64 {
            Fake.line(font)
        }
        fn text_box_sprite_height(&self) -> Option<f64> {
            Some(self.0)
        }
    }

    #[test]
    fn bar_text_sits_on_the_middle_of_the_sprite() {
        let ui = RunUi {
            bars: vec![Bar {
                text: "Sam is back.".into(),
                pulse: false,
                centre: false,
            }],
            ..RunUi::default()
        };
        let style = RunStyle {
            hud_corner: HudCorner::Right,
            time_alpha: 1.0,
            wave_alert: false,
            t: 0.0,
            paused: false,
            table: false,
            pause_rows: None,
        };
        let f = Frame { gs: 1.68, ..F };
        for (m, h) in [
            (&WithBar(140.0) as &dyn Measure, 140.0),
            (&Fake as &dyn Measure, TEXTBOX_SPRITE_H),
        ] {
            let ops = run_ops(&ui, style, f, m);
            let top = ops
                .iter()
                .find_map(|o| match o {
                    Op::TextBox { y, .. } => Some(*y),
                    _ => None,
                })
                .unwrap();
            let t = texts(&ops)[0];
            let frame = h * TEXTBOX_SCALE * f.gs;
            assert!(
                (t.y - (top + frame / 2.0)).abs() < 1e-9,
                "the line's centre on the frame's middle"
            );
            assert!((t.y - t.y.round()).abs() < 1e-9, "rounded like the game");
            assert_eq!(t.v, VAlign::Middle);
        }
    }

    #[test]
    fn partner_labels_use_small_print() {
        let ops = partner_ops(
            vec![DrawItem::Text {
                x: 1.0,
                y: 2.0,
                text: "Sam".into(),
                color: 7,
                scale: 1.0,
                align: crate::render::Align::Center,
            }],
            false,
        );
        let t = texts(&ops);
        assert_eq!((t[0].font, t[0].color), (Font::Small, 7));
        assert!((t[0].alpha - 0.8).abs() < 1e-9);
    }

    /// The partner's Ready goes into the gap above the credits (`h - 278`).
    #[test]
    fn on_the_high_score_table_the_notice_clears_the_credits() {
        let ui = RunUi {
            bars: vec![super::super::run::ready_bar("ready")],
            alert: Some(AlertView {
                text: "SAM IS READY".into(),
                scale: 1.0,
                alpha: 1.0,
            }),
            ..RunUi::default()
        };
        let style = RunStyle {
            hud_corner: HudCorner::Right,
            time_alpha: 1.0,
            wave_alert: false,
            t: 0.0,
            paused: false,
            table: false,
            pause_rows: None,
        };
        let at = |table| {
            let ops = run_ops(&ui, RunStyle { table, ..style }, F, &Fake);
            let t = texts(&ops);
            let y = |s: &str| t.iter().find(|x| x.text == s).unwrap().y;
            (y("SAM IS READY"), y("ready"))
        };
        let credits = F.h - 278.0;
        let (alert, bar) = at(false);
        assert!(alert >= credits - 5.0, "elsewhere: the usual places");
        assert!((bar - (F.h - RUN_BAR_RISE)).abs() < 1.0);
        let (alert, bar) = at(true);
        let line = Fake.line(Font::XLarge);
        assert!((alert - (F.h - TABLE_ALERT_TOP)).abs() < 1e-9);
        assert!(alert + line < bar - 20.0, "the alert above the bar");
        assert!(bar + 30.0 < credits, "the bar clear of the credits");
        assert!(alert > F.h - 444.0, "below the statistics");
    }
}
