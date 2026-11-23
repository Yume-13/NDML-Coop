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

//! Text: player names made safe for the game's fonts, the typed-text feed,
//! and the co-op strings that are not built from a status.
//!
//! The game's Latin body fonts cover printable ASCII and Latin-1 only (Russian
//! and the CJK languages load their own typefaces), so every string the mod
//! writes is ASCII and [`display_name`] filters player names for the active
//! language.

#![forbid(unsafe_code)]

/// The name shown when nothing printable is left of a partner's name.
pub const FALLBACK_PARTNER: &str = "Player 2";
/// Longest name shown, in characters (including the `...`).
pub const NAME_MAX: usize = 16;

/// Which characters the active body font can draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyphs {
    /// ASCII and Latin-1 (English, French, Spanish, Portuguese, German, Polish).
    Latin,
    /// Plus Cyrillic (Russian: Roboto Condensed).
    Cyrillic,
    /// Plus CJK, kana and Hangul (the CJK fonts).
    Cjk,
}

/// The glyph set for `global.language` (`english`, `russian`, `schinese`, ...).
#[must_use]
pub fn glyphs_for(language: &str) -> Glyphs {
    let l = language.trim().to_ascii_lowercase();
    match l.as_str() {
        "russian" => Glyphs::Cyrillic,
        "schinese" | "tchinese" | "japanese" | "korean" | "koreana" | "chinese" => Glyphs::Cjk,
        _ => Glyphs::Latin,
    }
}

fn drawable(c: char, g: Glyphs) -> bool {
    let latin = (' '..='~').contains(&c) || ('\u{A0}'..='\u{FF}').contains(&c);
    match g {
        Glyphs::Latin => latin,
        Glyphs::Cyrillic => latin || ('\u{400}'..='\u{4FF}').contains(&c),
        Glyphs::Cjk => {
            latin
                || ('\u{3000}'..='\u{9FFF}').contains(&c)
                || ('\u{AC00}'..='\u{D7AF}').contains(&c)
                || ('\u{FF00}'..='\u{FFEF}').contains(&c)
        }
    }
}

/// A partner's name as it can be drawn: characters the font lacks become
/// `?`, nothing readable left gives `fallback`, longer than [`NAME_MAX`]
/// is cut with `...`.
#[must_use]
pub fn display_name(raw: &str, language: &str, fallback: &str) -> String {
    let g = glyphs_for(language);
    let mut readable = false;
    let s: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if drawable(c, g) {
                readable |= c.is_alphanumeric();
                c
            } else {
                '?'
            }
        })
        .collect();
    if !readable {
        return fallback.to_owned();
    }
    truncate(&s, NAME_MAX)
}

/// `text` cut to at most `max` characters, ending in `...` when cut.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(3).max(1);
    let head: String = text.chars().take(keep).collect();
    format!("{}...", head.trim_end())
}

/// Upper case the way the game's `string_upper` does it for alerts (ASCII
/// letters only, so a name keeps its accents).
#[must_use]
pub fn upper(text: &str) -> String {
    text.to_ascii_uppercase()
}

/// Turns successive values of `keyboard_string` into newly typed text: what
/// follows the common prefix. Deletions are ignored (Backspace is a key).
#[derive(Debug, Clone, Default)]
pub struct TextFeed {
    prev: String,
}

impl TextFeed {
    /// New text since the last call (printable characters only).
    pub fn feed(&mut self, current: &str) -> String {
        let common = self
            .prev
            .chars()
            .zip(current.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let added: String = current
            .chars()
            .skip(common)
            .filter(|c| !c.is_control())
            .collect();
        current.clone_into(&mut self.prev);
        added
    }

    /// Remember `current` without taking anything from it.
    pub fn sync(&mut self, current: &str) {
        current.clone_into(&mut self.prev);
    }
}

/// Fixed co-op strings (English).
pub mod en {
    pub const COOP_ROW: &str = "Co-op";
    pub const COOP_TITLE: &str = "CO-OP";
    pub const LAN_TITLE: &str = "CODES AND LAN";
    pub const BACK: &str = "Back";
    pub const OFF: &str = "Off";
    pub const NOT_YET: &str =
        "Steam co-op is not available yet. Use Codes and LAN to play together.";
    pub const ENTRY_HINT: &str = "Enter: done     Esc: cancel     Ctrl+V: paste";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_keep_what_the_font_draws() {
        assert_eq!(display_name("Sam", "english", "P2"), "Sam");
        assert_eq!(display_name("  José  ", "english", "P2"), "José");
        assert_eq!(display_name("Ünal Øst", "german", "P2"), "Ünal Øst");
        assert_eq!(
            display_name("サム", "english", FALLBACK_PARTNER),
            "Player 2"
        );
        assert_eq!(display_name("サム", "japanese", "P2"), "サム");
        assert_eq!(display_name("Сэм", "english", "P2"), "P2");
        assert_eq!(display_name("Сэм", "russian", "P2"), "Сэм");
        assert_eq!(display_name("😀Sam😀", "english", "P2"), "?Sam?");
        assert_eq!(
            display_name("😀😀", "schinese", "P2"),
            "P2",
            "no emoji in CJK fonts"
        );
        assert_eq!(display_name("", "english", "P2"), "P2");
        assert_eq!(display_name("???", "english", "P2"), "P2");
        assert_eq!(display_name("a\u{7}b", "english", "P2"), "ab");
    }

    #[test]
    fn long_names_are_cut_with_dots() {
        let n = display_name("Commander Shepard of the Normandy", "english", "P2");
        assert_eq!(n.chars().count(), NAME_MAX);
        assert!(n.ends_with("...") && n.starts_with("Commander"), "{n}");
        assert_eq!(truncate("short", 16), "short");
        assert!(n.is_ascii());
    }

    #[test]
    fn language_glyph_sets() {
        assert_eq!(glyphs_for("English"), Glyphs::Latin);
        assert_eq!(glyphs_for("polish"), Glyphs::Latin);
        assert_eq!(glyphs_for("russian"), Glyphs::Cyrillic);
        assert_eq!(glyphs_for("tchinese"), Glyphs::Cjk);
        assert_eq!(glyphs_for(""), Glyphs::Latin);
    }

    #[test]
    fn text_feed_takes_only_new_characters() {
        let mut f = TextFeed::default();
        assert_eq!(f.feed("ab"), "ab");
        assert_eq!(f.feed("abc"), "c");
        assert_eq!(f.feed("ab"), "", "engine backspace is not text");
        assert_eq!(f.feed("abX"), "X");
        assert_eq!(f.feed("Y"), "Y", "cleared by io_clear, then typed");
        assert_eq!(f.feed("Y\u{8}"), "", "control characters dropped");
        f.sync("typed while closed");
        assert_eq!(f.feed("typed while closed"), "");
    }

    #[test]
    fn fixed_strings_are_ascii() {
        for s in [en::NOT_YET, en::ENTRY_HINT, en::COOP_ROW, en::LAN_TITLE] {
            assert!(s.is_ascii());
        }
    }
}
