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

//! The game's UI style as data: font roles, colour tokens, alignment, and
//! [`Op`], the display list every co-op screen is built as.

#![forbid(unsafe_code)]

use crate::render::ShipDraw;

/// A font role, held in a game global. The handles change on a window resize
/// and are -1 until loaded, so the glue reads them every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Font {
    /// "PAUSED", "SETTINGS". Capitals only: never a player name.
    Title,
    /// Settings tabs, "CHOOSE MOD". Title typeface: never a player name.
    Heading,
    /// Main-menu rows.
    Large,
    /// "WAVE 12", the score.
    XLarge,
    /// The `[ NAME ]` high-score entry.
    XXLarge,
    /// Settings rows, the run timer.
    Medium,
    /// "Wave: 2", tutorial hints, descriptions.
    Small,
    /// The build number.
    SmallAlt,
}

impl Font {
    /// Every role.
    pub const ALL: [Self; 8] = [
        Self::Title,
        Self::Heading,
        Self::Large,
        Self::XLarge,
        Self::XXLarge,
        Self::Medium,
        Self::Small,
        Self::SmallAlt,
    ];

    /// The global that holds this font's handle.
    #[must_use]
    pub const fn global(self) -> &'static str {
        match self {
            Self::Title => "fontTitle",
            Self::Heading => "fontHeading",
            Self::Large => "fontLarge",
            Self::XLarge => "fontXLarge",
            Self::XXLarge => "fontXXLarge",
            Self::Medium => "fontMedium",
            Self::Small => "fontSmall",
            Self::SmallAlt => "fontSmallAlt",
        }
    }

    /// Index into [`Font::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// The game's colours (`0xBBGGRR`).
pub mod color {
    /// All text.
    pub const WHITE: u32 = 0x00FF_FFFF;
    /// Headings, values, positive/highlight (84, 184, 255).
    pub const BLUE: u32 = 0x00FF_B854;
    /// Errors and "bad" values (250, 50, 105): failures only.
    pub const NEGATIVE: u32 = 0x0069_32FA;
    /// Unavailable (the main menu draws white at 0.3 alpha instead).
    pub const GREY: u32 = 0x0080_8080;
    /// The dim backdrop (at 0.5 alpha, as `oDimmer`).
    pub const BLACK: u32 = 0;
    /// The resume count's edge glow (255, 140, 0), a warning orange.
    pub const COUNT_EDGE: u32 = 0x0000_8CFF;
}

/// Alpha factors the game uses.
pub mod alpha {
    /// Rows the player can't use right now (× the page alpha).
    pub const DISABLED: f64 = 0.3;
    /// Everything behind a confirmation (× the page alpha).
    pub const BEHIND_DIALOG: f64 = 0.35;
    /// In-run small print (× `oController.timeAlpha`).
    pub const SMALL_PRINT: f64 = 0.65;
    /// The full-screen dim.
    pub const DIM: f64 = 0.5;
    /// Partner label and marker.
    pub const LABEL: f64 = 0.8;
}

/// Horizontal alignment (`fa_left` 0, `fa_center` 1, `fa_right` 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// Vertical alignment (`fa_top` 0, `fa_middle` 1, `fa_bottom` 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

impl HAlign {
    /// The GML constant.
    #[must_use]
    pub const fn gml(self) -> f64 {
        match self {
            Self::Left => 0.0,
            Self::Center => 1.0,
            Self::Right => 2.0,
        }
    }
}

impl VAlign {
    /// The GML constant.
    #[must_use]
    pub const fn gml(self) -> f64 {
        match self {
            Self::Top => 0.0,
            Self::Middle => 1.0,
            Self::Bottom => 2.0,
        }
    }
}

/// One line of text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextOp {
    pub font: Font,
    pub x: f64,
    pub y: f64,
    pub text: String,
    pub color: u32,
    pub alpha: f64,
    pub h: HAlign,
    pub v: VAlign,
    /// 1 = `draw_text`; anything else `draw_text_transformed`.
    pub scale: f64,
}

impl TextOp {
    /// White, left/top, full alpha, scale 1.
    #[must_use]
    pub fn new(font: Font, x: f64, y: f64, text: impl Into<String>) -> Self {
        Self {
            font,
            x,
            y,
            text: text.into(),
            color: color::WHITE,
            alpha: 1.0,
            h: HAlign::Left,
            v: VAlign::Top,
            scale: 1.0,
        }
    }

    #[must_use]
    pub fn color(mut self, c: u32) -> Self {
        self.color = c;
        self
    }

    #[must_use]
    pub fn alpha(mut self, a: f64) -> Self {
        self.alpha = a;
        self
    }

    #[must_use]
    pub fn align(mut self, h: HAlign, v: VAlign) -> Self {
        self.h = h;
        self.v = v;
        self
    }

    #[must_use]
    pub fn scale(mut self, s: f64) -> Self {
        self.scale = s;
        self
    }
}

/// One drawing operation on the GUI layer.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Text(TextOp),
    /// The game's hint-bar backdrop (`drawTextBox`), sized to `text` in
    /// `fontSmall`; a [`Op::Text`] follows it. `y` is the sprite's top.
    TextBox {
        x: f64,
        y: f64,
        text: String,
        alpha: f64,
    },
    /// A filled rectangle (fallback backings, the drawn dialog's boxes).
    Rect {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: u32,
        alpha: f64,
        outline: bool,
    },
    /// Black over the whole GUI (as `oDimmer`).
    Dim {
        alpha: f64,
    },
    /// A filled triangle (the partner marker).
    Triangle {
        pts: [(f64, f64); 3],
        color: u32,
        alpha: f64,
    },
    /// The partner's ship on the draw-call path (`sprite_exists` first).
    Sprite(ShipDraw),
    /// The game's `sGUIGradient` over `0..w` and `y..y + h`, as Settings draws it.
    Backdrop {
        y: f64,
        w: f64,
        h: f64,
        alpha: f64,
    },
}

impl From<TextOp> for Op {
    fn from(t: TextOp) -> Self {
        Self::Text(t)
    }
}

/// Text sizes, measured by the glue with the game's fonts (a fake in tests).
pub trait Measure {
    /// Width of `text` in `font`, GUI px.
    fn width(&self, font: Font, text: &str) -> f64;
    /// Line height of `font`, GUI px.
    fn line(&self, font: Font) -> f64;
    /// Height of `sTutorialBarWide` in sprite pixels, if it can be read.
    fn text_box_sprite_height(&self) -> Option<f64> {
        None
    }
}

/// The frame the screens are laid out in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// GUI size.
    pub w: f64,
    pub h: f64,
    /// `global.guiScale` (1 ≈ a 1080p layout).
    pub gs: f64,
}

impl Frame {
    /// A frame with a sane scale (`gs` from the global, or from the height).
    #[must_use]
    pub fn new(w: f64, h: f64, gs: Option<f64>) -> Self {
        let gs = gs
            .filter(|g| g.is_finite() && *g > 0.05 && *g < 20.0)
            .unwrap_or((h / 1080.0).clamp(0.25, 8.0));
        Self { w, h, gs }
    }
}

/// Word-wrap `text` to `max_w` px in `font` (long words are cut).
#[must_use]
pub fn wrap(m: &dyn Measure, font: Font, text: &str, max_w: f64) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if m.width(font, &candidate) <= max_w || line.is_empty() {
                if line.is_empty() && m.width(font, word) > max_w {
                    let mut part = String::new();
                    for c in word.chars() {
                        part.push(c);
                        if m.width(font, &part) > max_w && part.chars().count() > 1 {
                            part.pop();
                            out.push(std::mem::take(&mut part));
                            part.push(c);
                        }
                    }
                    line = part;
                } else {
                    line = candidate;
                }
            } else {
                out.push(std::mem::replace(&mut line, word.to_owned()));
            }
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Half a line height per character; line heights by role.
    #[derive(Debug)]
    pub struct Fake;

    impl Measure for Fake {
        fn width(&self, font: Font, text: &str) -> f64 {
            f64::from(u32::try_from(text.chars().count()).unwrap_or(u32::MAX))
                * self.line(font)
                * 0.5
        }
        fn line(&self, font: Font) -> f64 {
            match font {
                Font::Title => 60.0,
                Font::XXLarge => 50.0,
                Font::Heading | Font::XLarge => 32.0,
                Font::Large => 28.0,
                Font::Medium => 24.0,
                Font::Small => 19.0,
                Font::SmallAlt => 14.0,
            }
        }
    }

    #[test]
    fn tokens_are_bgr() {
        assert_eq!(color::BLUE, crate::render::rgb(84, 184, 255));
        assert_eq!(color::NEGATIVE, crate::render::rgb(250, 50, 105));
        assert_eq!(Font::ALL.len(), 8);
        for (i, f) in Font::ALL.iter().enumerate() {
            assert_eq!(f.index(), i);
            assert!(f.global().starts_with("font"));
        }
    }

    #[test]
    fn wrap_fits_and_keeps_words() {
        let lines = wrap(&Fake, Font::Small, "one two three four five six", 9.5 * 7.0);
        for l in &lines {
            assert!(Fake.width(Font::Small, l) <= 9.5 * 7.0 + 1e-9, "{l}");
        }
        assert_eq!(lines.join(" "), "one two three four five six");
        let cut = wrap(&Fake, Font::Small, "abcdefghijklmnop", 9.5 * 5.0);
        assert!(
            cut.len() >= 3 && cut.concat() == "abcdefghijklmnop",
            "{cut:?}"
        );
        assert_eq!(wrap(&Fake, Font::Small, "a\nb", 1000.0), vec!["a", "b"]);
    }

    #[test]
    fn frame_scale_falls_back_to_height() {
        assert!((Frame::new(3840.0, 2160.0, None).gs - 2.0).abs() < 1e-9);
        assert!((Frame::new(3840.0, 2160.0, Some(1.5)).gs - 1.5).abs() < 1e-9);
        assert!((Frame::new(1920.0, 1080.0, Some(f64::NAN)).gs - 1.0).abs() < 1e-9);
    }
}
