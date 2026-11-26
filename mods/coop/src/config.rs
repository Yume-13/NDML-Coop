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

//! `ndml/config/ndmod_coop.toml`: the raw file ([`Config`]) and the validated,
//! clamped settings the mod runs with ([`Settings`]). Every value out of range
//! is clamped (or replaced by its default) with a warning that names it.

#![forbid(unsafe_code)]

use std::time::Duration;

use ndml_net::lobby::files::clean_name;
use serde::Deserialize;

/// `player_name` (any case) for an automatic name: a pick from
/// [`crate::callsigns`] on codes and LAN sessions.
pub const RANDOM: &str = ndml_net::lobby::files::AUTO_NAME;

/// The longest a partner's ship keeps moving along its last velocity when
/// packets are late (the remote ship's and the mirror's coast).
pub const EXTRAPOLATION: Duration = Duration::from_millis(250);

/// Read `ndmod_coop.toml` over the shipped defaults, through the SDK's
/// settings refresh as every mod does.
pub fn load(ctx: &ndml_sdk::ModContext) -> Result<Config, ndml_sdk::Error> {
    ctx.load_config()
}

/// The file as written. See the shipped `config.toml` for each field.
#[allow(clippy::struct_excessive_bools)] // flat config keys, one switch each
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub player_name: String,
    pub port: i64,
    pub internet_lookup: bool,
    pub enemy_health_mult: f64,
    pub enemy_damage_mult: f64,
    pub respawn_seconds: f64,
    pub overlay: bool,
    // The shared world: see `world::config`.
    pub color: String,
    pub show_names: bool,
    pub show_marker: bool,
    /// The partner's sounds here, a fraction of their normal gain (0 = off:
    /// none played, none asked for): see `sound`.
    #[serde(default = "half")]
    pub partner_sound_volume: f64,
    // The co-op UI: see `ui::settings`.
    pub hud_corner: crate::ui::layout::HudCorner,
    pub show_traffic: bool,
    pub deck_keyboard: bool,
    pub partner_tint: crate::world::present::PartnerTint,
    // Steam connector: see `steam::settings`.
    pub connector: String,
    pub steam_lobby: String,
    // Session rules: see `rules::settings`.
    pub pause_on_focus_loss: bool,
    // This player's own wild mods in a co-op run: see `wild`.
    #[serde(default)]
    pub wild_mods: crate::wild::WildMods,
}

/// The default when the key is missing.
fn half() -> f64 {
    0.5
}

/// Validated settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The name on codes and LAN sessions: the typed one, or this launch's
    /// automatic pick (a Steam session goes by the Steam name).
    pub name: String,
    /// `Some` when `name` is automatic (`player_name = "Random"`): the
    /// launch's second pick ([`crate::callsigns`]).
    pub name_alt: Option<String>,
    pub port: u16,
    /// Ask the shipped STUN servers for this game's internet address on the
    /// connection-code route (`internet_lookup`); off: none is ever asked.
    pub internet_lookup: bool,
    pub health_mult: f64,
    pub damage_mult: f64,
    pub respawn: Duration,
    pub overlay: bool,
    /// The shared world.
    pub world: crate::world::WorldSettings,
    /// The co-op UI.
    pub ui: crate::ui::settings::UiSettings,
    /// The Steam connector.
    pub steam: crate::steam::SteamSettings,
    /// The player-side mirror.
    pub mirror: crate::mirror::MirrorSettings,
    /// The session rules.
    pub rules: crate::rules::RulesSettings,
    /// `partner_sound_volume`, in [0, 1].
    pub partner_sound_volume: f64,
    /// `wild_mods`: this player's own wild mods in a co-op run, the Co-op page's
    /// Wild Mods row (`wild`).
    pub wild_mods: crate::wild::WildMods,
}

/// Clamp `v` into `[lo, hi]`; NaN/inf become `default`. Warns when it changed.
fn clamp_f(name: &str, v: f64, lo: f64, hi: f64, default: f64, warn: &mut Vec<String>) -> f64 {
    if !v.is_finite() {
        warn.push(format!("{name} = {v} is not a number; using {default}"));
        return default;
    }
    let c = v.clamp(lo, hi);
    if (c - v).abs() > f64::EPSILON {
        warn.push(format!("{name} = {v} is outside [{lo}, {hi}]; using {c}"));
    }
    c
}

impl Config {
    /// Validate and clamp. Returns the settings and one warning per value
    /// that was changed.
    #[must_use]
    #[allow(clippy::too_many_lines)] // one block per setting, in file order
    pub fn validate(&self) -> (Settings, Vec<String>) {
        let mut w = Vec::new();
        let auto = || {
            let (a, b) = crate::callsigns::launch_picks();
            (a.to_owned(), Some(b.to_owned()))
        };
        let (name, name_alt) = if self.player_name.trim().eq_ignore_ascii_case(RANDOM) {
            auto()
        } else if let Some(n) = clean_name(&self.player_name) {
            (n, None)
        } else {
            w.push(format!(
                "player_name {:?} is empty or unusable; using {RANDOM} (a name picked at random)",
                self.player_name
            ));
            auto()
        };
        let port = u16::try_from(self.port).unwrap_or_else(|_| {
            w.push(format!(
                "port = {} is not a UDP port; using {}",
                self.port,
                ndml_net::lobby::DEFAULT_PORT
            ));
            ndml_net::lobby::DEFAULT_PORT
        });
        let health_mult = clamp_f(
            "enemy_health_mult",
            self.enemy_health_mult,
            0.1,
            10.0,
            1.8,
            &mut w,
        );
        let damage_mult = clamp_f(
            "enemy_damage_mult",
            self.enemy_damage_mult,
            0.1,
            10.0,
            1.0,
            &mut w,
        );
        let respawn = Duration::from_secs_f64(clamp_f(
            "respawn_seconds",
            self.respawn_seconds,
            5.0,
            600.0,
            45.0,
            &mut w,
        ));
        let world = self.world_settings(&mut w);
        let steam = self.steam(&mut w);
        let rules = crate::rules::settings::validate(&crate::rules::settings::Raw {
            pause_on_focus_loss: self.pause_on_focus_loss,
            respawn,
        });
        let settings = Settings {
            name,
            name_alt,
            port,
            internet_lookup: self.internet_lookup,
            health_mult,
            damage_mult,
            respawn,
            overlay: self.overlay,
            world,
            ui: self.ui_settings(),
            steam,
            mirror: crate::mirror::MirrorSettings {
                draw: true,
                carry_ms: u32::try_from(EXTRAPOLATION.as_millis()).unwrap_or(u32::MAX),
            },
            rules,
            partner_sound_volume: clamp_f(
                "partner_sound_volume",
                self.partner_sound_volume,
                0.0,
                1.0,
                0.5,
                &mut w,
            ),
            wild_mods: self.wild_mods,
        };
        (settings, w)
    }

    fn steam(&self, w: &mut Vec<String>) -> crate::steam::SteamSettings {
        crate::steam::settings::validate(
            &crate::steam::settings::Raw {
                connector: &self.connector,
                lobby: &self.steam_lobby,
            },
            w,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Config {
        ndml_sdk::config::parse_defaults(crate::NDML_DEFAULT_CONFIG).unwrap()
    }

    /// A file that holds `host_plays_on_seconds` loads. The refresh drops the
    /// key with its notice and no warning.
    #[test]
    fn a_file_with_host_plays_on_seconds_loads() {
        let file = format!(
            "{}\nhost_plays_on_seconds = 40\n",
            crate::NDML_DEFAULT_CONFIG
        );
        let r = ndml_sdk::config::refresh(&file, crate::NDML_DEFAULT_CONFIG).unwrap();
        assert_eq!(
            r.notice().as_deref(),
            Some("config refreshed: host_plays_on_seconds dropped (was 40)")
        );
        let c: Config = ndml_sdk::config::parse_defaults(&r.text).unwrap();
        assert_eq!(c.pause_on_focus_loss, defaults().pause_on_focus_loss);
        assert!(!crate::NDML_DEFAULT_CONFIG.contains("host_plays_on"));
    }

    /// A file that holds `upgrade_wait_seconds` loads. The key is dropped.
    #[test]
    fn a_file_with_upgrade_wait_seconds_loads() {
        let file = format!(
            "{}\nupgrade_wait_seconds = 60\n",
            crate::NDML_DEFAULT_CONFIG
        );
        let r = ndml_sdk::config::refresh(&file, crate::NDML_DEFAULT_CONFIG).unwrap();
        assert_eq!(
            r.notice().as_deref(),
            Some("config refreshed: upgrade_wait_seconds dropped (was 60)")
        );
        let _: Config = ndml_sdk::config::parse_defaults(&r.text).unwrap();
        assert!(!crate::NDML_DEFAULT_CONFIG.contains("upgrade_wait"));
    }

    /// The shipped `config.toml` lists exactly the keys `Config` reads: a
    /// settings refresh drops what the shipped file lacks, so a key read but
    /// not shipped would vanish from every player's file.
    #[test]
    fn shipped_config_lists_every_key() {
        ndml_sdk::config::check_shipped_keys::<Config>(crate::NDML_DEFAULT_CONFIG, &[]).unwrap();
    }

    /// A file with unknown keys through the SDK's refresh: they go, a mistyped value falls back
    /// to its default, the rest counts; the original is kept once as `.bak`; a second load is a
    /// no-op.
    #[test]
    fn a_file_keeps_its_values_and_loses_only_unknown_keys() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/coop-unknown-config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ndmod_coop.toml");
        let bak = ndml_sdk::config::backup_path(&path);
        let _ = std::fs::remove_file(&bak);
        let before = "player_name = \"Ada\"\nport = 7800\npause_on_focus_loss = \"yes\"\n\
                      extra_count = 5\nextra_on = true\nextra_off = false\n\
                      extra_seconds = 30\nextra_list = []\n\
                      extra_key = \"F10\"\nextra_style = \"drawn\"\n";
        std::fs::write(&path, before).unwrap();

        let c: Config = ndml_sdk::config::load(&path, crate::NDML_DEFAULT_CONFIG).unwrap();
        assert_eq!(c.player_name, "Ada");
        assert_eq!(c.port, 7800);
        assert!(c.pause_on_focus_loss, "mistyped: the default");
        assert!(c.internet_lookup, "not in the file: the shipped value");

        let now = std::fs::read_to_string(&path).unwrap();
        let has = |k: &str| now.lines().any(|l| l.starts_with(&format!("{k} = ")));
        assert!(now.contains("player_name = \"Ada\"") && now.contains("port = 7800"));
        for k in [
            "extra_count",
            "extra_on",
            "extra_off",
            "extra_seconds",
            "extra_list",
            "extra_key",
            "extra_style",
        ] {
            assert!(!has(k), "{k} unknown: dropped");
        }
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), before);

        let _: Config = ndml_sdk::config::load(&path, crate::NDML_DEFAULT_CONFIG).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            now,
            "a second load"
        );
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), before);
    }

    /// Unknown keys, even set off, load without a warning and are dropped.
    #[test]
    fn unknown_switches_load_quietly() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/coop-unknown-switches");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ndmod_coop.toml");
        let _ = std::fs::remove_file(ndml_sdk::config::backup_path(&path));
        let keys = ["switch_a", "switch_b", "switch_c", "switch_d", "switch_e"];
        let before = keys.join(" = false\n") + " = false\n";
        std::fs::write(&path, before).unwrap();
        let c: Config = ndml_sdk::config::load(&path, crate::NDML_DEFAULT_CONFIG).unwrap();
        let (_, warnings) = c.validate();
        assert!(warnings.is_empty(), "{warnings:?}");
        let now = std::fs::read_to_string(&path).unwrap();
        for k in keys {
            assert!(
                !now.lines().any(|l| l.starts_with(&format!("{k} = "))),
                "{k} dropped"
            );
        }
    }

    #[test]
    fn shipped_defaults_are_valid_and_quiet() {
        let (s, warnings) = defaults().validate();
        assert!(warnings.is_empty(), "{warnings:?}");
        let (pick, alt) = crate::callsigns::launch_picks();
        assert_eq!(s.name, pick, "\"Random\": this launch's pick");
        assert_eq!(s.name_alt.as_deref(), Some(alt));
        assert_eq!(s.port, 7777);
        assert!(s.internet_lookup);
        assert!((s.health_mult - 1.8).abs() < 1e-12);
        assert!((s.damage_mult - 1.0).abs() < 1e-12);
        assert_eq!(s.respawn, Duration::from_secs(45));
        assert!(s.overlay);
    }

    /// "Random" in any case and spacing is automatic; any other name
    /// ("Pilot" included) is the player's own.
    #[test]
    fn random_is_automatic_and_any_other_name_is_typed() {
        let (pick, alt) = crate::callsigns::launch_picks();
        for v in ["Random", " random ", "RANDOM"] {
            let mut c = defaults();
            c.player_name = v.into();
            let (s, w) = c.validate();
            assert!(w.is_empty(), "{w:?}");
            assert_eq!(
                (s.name.as_str(), s.name_alt.as_deref()),
                (pick, Some(alt)),
                "{v}"
            );
        }
        for (v, want) in [
            ("Pilot", "Pilot"),
            ("  Night Owl ", "Night Owl"),
            ("Randomizer", "Randomizer"),
        ] {
            let mut c = defaults();
            c.player_name = v.into();
            let (s, w) = c.validate();
            assert!(w.is_empty(), "{w:?}");
            assert_eq!(
                (s.name.as_str(), s.name_alt.as_deref()),
                (want, None),
                "{v}"
            );
        }
    }

    #[test]
    fn out_of_range_values_are_clamped_with_warnings() {
        let mut c = defaults();
        c.player_name = "   ".into();
        c.port = 70_000;
        c.enemy_health_mult = 50.0;
        c.enemy_damage_mult = f64::NAN;
        c.respawn_seconds = 1.0;
        let (s, w) = c.validate();
        assert_eq!(
            s.name,
            crate::callsigns::launch_picks().0,
            "unusable: Random"
        );
        assert!(s.name_alt.is_some());
        assert_eq!(s.port, 7777);
        assert!((s.health_mult - 10.0).abs() < 1e-12);
        assert!((s.damage_mult - 1.0).abs() < 1e-12);
        assert_eq!(s.respawn, Duration::from_secs(5));
        // name, port, health, damage, respawn.
        assert_eq!(w.len(), 5, "{w:#?}");
        assert!(w.iter().any(|m| m.contains("enemy_health_mult")));
    }

    /// Every serde default equals the shipped value, so a config read without the SDK's overlay
    /// (a test, a tool) matches the shipped file; a switch shipped off stays off.
    #[test]
    fn code_defaults_for_missing_keys_match_the_shipped_file() {
        let shipped: toml::Table = crate::NDML_DEFAULT_CONFIG.parse().unwrap();
        let full = defaults();
        let mut differ = Vec::new();
        for key in shipped.keys() {
            let mut t = shipped.clone();
            t.remove(key);
            // A key with no code default does not parse without it.
            if let Ok(c) = toml::Value::Table(t).try_into::<Config>()
                && c != full
            {
                differ.push(key.clone());
            }
        }
        assert!(
            differ.is_empty(),
            "code default differs from the shipped value: {differ:?}"
        );
    }
}
