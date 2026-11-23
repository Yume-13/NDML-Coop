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

//! The game's main menu, steered: the "Co-op" row and "page mode".
//!
//! `oMenu_Draw_64` draws eleven fixed rows from the `str*` globals in
//! [`ROW_GLOBALS`] and `confirmMenu` dispatches on `pos`. The Co-op row goes
//! in at row 2, the old rows shift down one (the mod draws Exit at row 11)
//! and `confirmMenu` sees `pos - 1` below it.
//!
//! Page mode reuses `oMenu`: the globals hold the page's rows from row 1,
//! `minPos = 1`, `maxPos = N`, and the game draws and runs the input.

#![forbid(unsafe_code)]

use ndml_net::lobby::{LobbyState, Status};

use super::page::lan_active;
use super::steam_link::{SteamState, SteamView};
use super::text;

/// The eleven row globals in drawing order (`oMenu_Draw_64`).
pub const ROW_GLOBALS: [&str; 11] = [
    "strMenuResumeRun",
    "strMenuStart",
    "strMenuUnlocks",
    "strMenuLearn",
    "strMenuGlossary",
    "strMenuChangelog",
    "strMenuSettings",
    "strMenuScore",
    "strCreditsTitle",
    "strMenuDiscord",
    "strMenuExit",
];
/// Rows the game draws.
pub const ROWS: usize = ROW_GLOBALS.len();
/// The row the mod draws below the game's (and that `maxPos + 1` reaches).
pub const EXTRA_ROW: i64 = 11;
/// Where "Co-op" goes.
pub const INSERT_ROW: i64 = 2;

/// The labels for the Co-op row: what to write into the eleven
/// globals (`None` = leave it) and what the mod draws at row 11.
#[must_use]
pub fn inserted_labels(old: &[String; ROWS], coop: &str) -> ([Option<String>; ROWS], String) {
    let mut out: [Option<String>; ROWS] = Default::default();
    out[2] = Some(coop.to_owned());
    for row in 3..ROWS {
        out[row] = Some(old[row - 1].clone());
    }
    (out, old[ROWS - 1].clone())
}

/// The labels for page mode: row 0 empty, rows 1..=N the page's rows (an
/// empty string for a row the mod draws dimmed itself), the rest empty.
#[must_use]
pub fn page_labels(rows: &[(String, bool)]) -> [String; ROWS] {
    let mut out: [String; ROWS] = Default::default();
    for (i, (label, enabled)) in rows.iter().enumerate().take(ROWS - 1) {
        if *enabled {
            out[i + 1].clone_from(label);
        }
    }
    out
}

/// What to do with a `confirmMenu` call on the main menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Let the game handle it as is.
    Original,
    /// Open the co-op page (skip the original).
    OpenCoop,
    /// Run the original with `pos` set to this, then restore `pos`.
    Shifted(i64),
    /// Skip it (row 11 without the Co-op row: the old store page).
    Swallow,
}

/// Route `confirmMenu` for `pos`. `drawn`: the Co-op row was drawn,
/// `raised`: whether `maxPos` is raised (even without the row).
#[must_use]
pub fn route(drawn: bool, raised: bool, pos: i64) -> Route {
    match pos {
        INSERT_ROW if drawn => Route::OpenCoop,
        p if drawn && (INSERT_ROW + 1..=EXTRA_ROW).contains(&p) => Route::Shifted(p - 1),
        p if raised && p >= EXTRA_ROW => Route::Swallow,
        _ => Route::Original,
    }
}

/// `pos` on the same label when the Co-op row comes (`drawn`) or goes: the
/// rows from Unlocks down move one place (Co-op itself goes to Unlocks).
#[must_use]
pub fn pos_across(pos: f64, drawn: bool) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let row = INSERT_ROW as f64;
    match (drawn, pos) {
        (true, p) if p >= row => p + 1.0,
        (false, p) if p > row => p - 1.0,
        (_, p) => p,
    }
}

/// The `oMenu` values page mode changes, to put back on leaving.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Saved {
    pub pos: f64,
    pub min_pos: f64,
    pub max_pos: f64,
}

/// Selection limits for a page of `n` rows: `(minPos, maxPos)`.
#[must_use]
pub fn page_limits(n: usize) -> (f64, f64) {
    let n = n.clamp(1, ROWS - 1);
    #[allow(clippy::cast_precision_loss)]
    (1.0, n as f64)
}

/// `pos` kept on a page row: the mouse can pick any of the game's eleven
/// rows, so anything outside `1..=n` goes back to `last`.
#[must_use]
pub fn clamp_pos(pos: f64, n: usize, last: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let n = n as f64;
    if pos.is_finite() && pos >= 1.0 && pos <= n {
        pos
    } else {
        last.clamp(1.0, n.max(1.0))
    }
}

/// The menu row that keeps the same page row selected when the rows change,
/// or the nearest valid one.
#[must_use]
pub fn follow_row<T: PartialEq>(old: &[T], new: &[T], pos: f64) -> f64 {
    let kept = row_index(pos, old.len())
        .and_then(|i| old.get(i))
        .and_then(|id| new.iter().position(|n| n == id));
    #[allow(clippy::cast_precision_loss)]
    match kept {
        Some(i) => (i + 1) as f64,
        None => clamp_pos(pos, new.len(), new.len() as f64),
    }
}

/// The page row (0-based) for menu row `pos`.
#[must_use]
pub fn row_index(pos: f64, n: usize) -> Option<usize> {
    let i = pos.round() as i64 - 1;
    usize::try_from(i).ok().filter(|i| *i < n)
}

/// The short state drawn after the "Co-op" row (`fontSmall`, blue), or
/// `None` when idle.
#[must_use]
pub fn row_status(status: Option<&Status>, steam: &SteamView, language: &str) -> Option<String> {
    let name = |raw: &str| text::display_name(raw, language, text::FALLBACK_PARTNER);
    match &steam.state {
        SteamState::Linked | SteamState::InRun => {
            return Some(
                steam
                    .partner
                    .as_deref()
                    .map_or_else(|| "linked".into(), |p| format!("with {}", name(p))),
            );
        }
        SteamState::Hosting => return Some("lobby open".into()),
        SteamState::Creating
        | SteamState::Joining
        | SteamState::InLobby
        | SteamState::Connecting => return Some("connecting...".into()),
        SteamState::PeerLost { .. } => return Some("reconnecting...".into()),
        _ => {}
    }
    let s = status.filter(|s| lan_active(Some(s)))?;
    Some(match s.state {
        LobbyState::Connected => s
            .connected_peers()
            .next()
            .map_or_else(|| "linked".into(), |p| format!("with {}", name(&p.name))),
        LobbyState::Waiting => "lobby open".into(),
        _ => "connecting...".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::lan::tests::{linked, status};
    use crate::ui::steam_link::{NotYet, SteamLink};

    fn labels() -> [String; ROWS] {
        [
            "Resume Run",
            "Game Start",
            "Unlocks",
            "Learn",
            "Glossary",
            "Changelog",
            "Settings",
            "Scores",
            "Credits",
            "Discord",
            "Exit",
        ]
        .map(String::from)
    }

    #[test]
    fn inserted_row_shifts_labels_down() {
        let (w, extra) = inserted_labels(&labels(), "Co-op");
        assert!(
            w[0].is_none() && w[1].is_none(),
            "Resume and Start untouched"
        );
        assert_eq!(w[2].as_deref(), Some("Co-op"));
        assert_eq!(w[3].as_deref(), Some("Unlocks"));
        assert_eq!(w[10].as_deref(), Some("Discord"));
        assert_eq!(extra, "Exit", "the mod draws Exit at row 11");
    }

    #[test]
    fn confirm_routing_keeps_every_original_row() {
        assert_eq!(route(true, true, 0), Route::Original);
        assert_eq!(route(true, true, 1), Route::Original);
        assert_eq!(route(true, true, 2), Route::OpenCoop);
        // Rows 3..11 are the old rows 2..10 (Unlocks .. Exit).
        for pos in 3..=11 {
            assert_eq!(route(true, true, pos), Route::Shifted(pos - 1));
        }
        // A frame without the row: the game's own menu, every row its own.
        for pos in 0..=10 {
            assert_eq!(route(false, false, pos), Route::Original);
        }
        // No row drawn but maxPos still raised: never the store page.
        assert_eq!(route(false, true, 11), Route::Swallow);
        assert_eq!(route(false, true, 4), Route::Original);
    }

    /// When the row comes or goes, the selection stays on its label: Exit
    /// is row 11 with the row and row 10 without it.
    #[test]
    fn the_selection_keeps_its_label_across_the_row() {
        let (with, without) = (inserted_labels(&labels(), "Co-op"), labels());
        let label_with = |pos: f64| {
            let i = pos as usize;
            match i {
                0 | 1 => without[i].clone(),
                11 => with.1.clone(),
                _ => with.0[i].clone().unwrap(),
            }
        };
        for pos in [0.0, 1.0, 3.0, 7.0, 10.0, 11.0] {
            let back = pos_across(pos, false);
            let label = &without[back as usize];
            assert_eq!(*label, label_with(pos), "{pos} -> {back}");
            assert!((pos_across(back, true) - pos).abs() < 1e-9);
        }
        assert!(
            (pos_across(2.0, false) - 2.0).abs() < 1e-9,
            "Co-op: Unlocks"
        );
        assert!(
            pos_across(11.0, false) <= 10.0,
            "never past the game's rows"
        );
    }

    #[test]
    fn page_mode_labels_and_limits() {
        let rows = vec![
            ("Host Game".to_owned(), false),
            ("Ship Colour".to_owned(), true),
            ("Back".to_owned(), true),
        ];
        let l = page_labels(&rows);
        assert_eq!(l[0], "", "row 0 is the heading's place");
        assert_eq!(l[1], "", "a dim row is drawn by the mod");
        assert_eq!(l[2], "Ship Colour");
        assert_eq!(l[3], "Back");
        assert!(l[4..].iter().all(String::is_empty));
        assert_eq!(page_limits(3), (1.0, 3.0));
        assert_eq!(page_limits(40), (1.0, 10.0));
        assert!((clamp_pos(0.0, 3, 2.0) - 2.0).abs() < 1e-9, "row 0: back");
        assert!((clamp_pos(7.0, 3, 9.0) - 3.0).abs() < 1e-9);
        assert!((clamp_pos(2.0, 3, 1.0) - 2.0).abs() < 1e-9);
        assert_eq!(row_index(1.0, 3), Some(0));
        assert_eq!(row_index(3.0, 3), Some(2));
        assert_eq!(row_index(0.0, 3), None);
        assert_eq!(row_index(4.0, 3), None);
    }

    #[test]
    fn selection_follows_its_row_when_rows_change() {
        let old = ["host", "invite", "back"];
        let new = ["host", "retry", "invite", "back"];
        assert!(
            (follow_row(&old, &new, 2.0) - 3.0).abs() < 1e-9,
            "invite kept"
        );
        assert!((follow_row(&new, &old, 3.0) - 2.0).abs() < 1e-9, "and back");
        assert!(
            (follow_row(&new, &old, 2.0) - 2.0).abs() < 1e-9,
            "retry gone: same place"
        );
        assert!((follow_row(&new, &old, 4.0) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn row_status_follows_the_link() {
        let steam = NotYet::default().view();
        assert_eq!(row_status(None, &steam, "english"), None, "idle: nothing");
        assert_eq!(
            row_status(Some(&status(LobbyState::Ready)), &steam, "english"),
            None
        );
        assert_eq!(
            row_status(Some(&linked(40.0)), &steam, "english").as_deref(),
            Some("with Wing")
        );
        assert_eq!(
            row_status(Some(&status(LobbyState::Waiting)), &steam, "english").as_deref(),
            Some("lobby open")
        );
        let mut s = steam.clone();
        s.state = SteamState::Linked;
        s.partner = Some("\u{1F600}".into());
        assert_eq!(
            row_status(None, &s, "english").as_deref(),
            Some("with Player 2")
        );
        s.state = SteamState::PeerLost {
            lost_for_s: Some(3),
        };
        assert_eq!(
            row_status(None, &s, "english").as_deref(),
            Some("reconnecting...")
        );
        s.state = SteamState::Hosting;
        assert_eq!(
            row_status(None, &s, "english").as_deref(),
            Some("lobby open")
        );
    }
}
