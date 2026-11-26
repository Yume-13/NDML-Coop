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

//! Saving the page's choices (ship colour, partner labels) into the config
//! file in place: only the value on the `key = ...` line changes. Runs on the
//! game thread when a choice is made, never per frame.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::path::Path;

use ndml_sdk::config::{read_text, refuse_read_only, write_atomic};

/// `text` with the top-level `key`'s value replaced by `value` (already in
/// TOML form: `"gold"`, `true`). Appends the key when it is missing.
#[must_use]
pub fn set_value(text: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut done = false;
    let mut in_table = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_table = true;
        }
        let is_key = !in_table
            && !done
            && trimmed
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='));
        if is_key {
            let indent = &line[..line.len() - trimmed.len()];
            let eol = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            let _ = write!(out, "{indent}{key} = {value}{eol}");
            done = true;
        } else {
            out.push_str(line);
        }
    }
    if !done {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        let _ = writeln!(out, "{key} = {value}");
    }
    out
}

/// A TOML string literal.
#[must_use]
pub fn toml_str(s: &str) -> String {
    let escaped: String = s
        .chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

/// Apply `changes` to the config file at `path`, atomically. A file that
/// can't be read or is read-only is an error and nothing is written; with no
/// file yet, the changes go into `shipped` (the mod's defaults).
pub fn save(path: &Path, shipped: &str, changes: &[(&str, String)]) -> Result<(), String> {
    // An error names the file from the game folder on, not in full.
    let shown = crate::game::shown_path(path);
    let named = |e: String| e.replace(&path.display().to_string(), &shown);
    let mut text = read_text(path)
        .map_err(named)?
        .unwrap_or_else(|| shipped.to_owned());
    for (k, v) in changes {
        text = set_value(&text, k, v);
    }
    refuse_read_only(path).map_err(named)?;
    write_atomic(path, &text).map_err(|e| format!("unable to write {shown} ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# Your co-op colour.\ncolor = \"auto\"\r\nshow_names = false # inline\ncolor_x = 1\n[table]\ncolor = \"nope\"\n";

    #[test]
    fn replaces_only_the_value() {
        let t = set_value(FILE, "color", &toml_str("gold"));
        assert!(
            t.contains("# Your co-op colour.\ncolor = \"gold\"\r\n"),
            "{t}"
        );
        assert!(t.contains("color_x = 1"), "a longer key is not touched");
        assert!(
            t.contains("[table]\ncolor = \"nope\""),
            "tables are not touched"
        );
        let t = set_value(&t, "show_names", "true");
        assert!(t.contains("show_names = true\n"));
        let t = set_value("a = 1", "show_marker", "true");
        assert_eq!(t, "a = 1\nshow_marker = true\n");
        assert_eq!(toml_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn saves_atomically_in_place() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/coop-prefs");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ndmod_coop.toml");
        std::fs::write(&path, FILE).unwrap();
        save(
            &path,
            "",
            &[("color", toml_str("red")), ("show_marker", "true".into())],
        )
        .unwrap();
        let t = std::fs::read_to_string(&path).unwrap();
        assert!(t.contains("color = \"red\"") && t.ends_with("show_marker = true\n"));
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn a_file_that_cannot_be_read_is_never_replaced() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/coop-prefs-unreadable")
            .join("default");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let change = [("color", toml_str("red"))];

        // Not UTF-8: refused, the bytes kept.
        let path = dir.join("ndmod_coop.toml");
        let bytes = b"color = \"auto\"\nport = 7800\n\xff\xfe\n";
        std::fs::write(&path, bytes).unwrap();
        let e = save(&path, crate::NDML_DEFAULT_CONFIG, &change).unwrap_err();
        assert!(e.contains("ndmod_coop.toml"), "{e}");
        assert!(!e.contains("test-tmp"), "never the folders above: {e}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        // A read that fails (a folder in the file's place): refused.
        let locked = dir.join("locked.toml");
        std::fs::create_dir_all(&locked).unwrap();
        assert!(save(&locked, crate::NDML_DEFAULT_CONFIG, &change).is_err());
        assert!(locked.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no temp file");

        // No file: the shipped defaults with the change.
        let fresh = dir.join("fresh.toml");
        save(&fresh, crate::NDML_DEFAULT_CONFIG, &change).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fresh).unwrap(),
            set_value(crate::NDML_DEFAULT_CONFIG, "color", &toml_str("red"))
        );
        let c: crate::config::Config =
            ndml_sdk::config::load(&fresh, crate::NDML_DEFAULT_CONFIG).unwrap();
        assert_eq!(c.color, "red");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Choices saved into a file the launcher wrote survive its next refresh.
    #[test]
    fn choices_saved_after_the_launcher_survive_its_next_refresh() {
        use ndml_sdk::config::{FileUpdate, update_file};
        let shipped = crate::NDML_DEFAULT_CONFIG;
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/coop-prefs-after-launcher")
            .join("default");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ndmod_coop.toml");
        let _ = std::fs::remove_file(&path);

        // A new install, then an old file the launcher refreshes.
        for start in [None, Some("color = \"gold\"\nport = 7800\n")] {
            if let Some(old) = start {
                std::fs::write(&path, old).unwrap();
            }
            let first = update_file(&path, shipped, true).unwrap();
            assert!(
                matches!(first, FileUpdate::Created | FileUpdate::Refreshed { .. }),
                "{first:?}"
            );
            save(
                &path,
                shipped,
                &[
                    ("color", toml_str("red")),
                    ("show_names", "false".into()),
                    ("show_marker", "true".into()),
                ],
            )
            .unwrap();
            let saved = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                update_file(&path, shipped, true).unwrap(),
                FileUpdate::Unchanged
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
            let c: crate::config::Config = ndml_sdk::config::load(&path, shipped).unwrap();
            assert_eq!(c.color, "red");
            assert!(!c.show_names && c.show_marker);
            if start.is_some() {
                assert_eq!(c.port, 7800, "the old file's value kept");
            }
        }
    }
}
