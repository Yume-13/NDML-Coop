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

//! The Steam connector's config keys, validated (see the shipped
//! `config.toml`).

#![forbid(unsafe_code)]

use std::time::Duration;

use ndml_net::endpoint::Rejoin;

use super::view::Visibility;

/// `connector`: which connector the mod sets up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectorChoice {
    /// Steam when it is running, UDP codes otherwise.
    #[default]
    Auto,
    /// Steam (UDP still works on its page).
    Steam,
    /// UDP only: no Steam hooks at all.
    Udp,
}

/// Validated Steam settings.
#[derive(Debug, Clone, PartialEq)]
pub struct SteamSettings {
    pub connector: ConnectorChoice,
    /// Visibility of a lobby hosted here.
    pub visibility: Visibility,
    /// How long a lost partner may come back outside a run (no limit in one).
    pub rejoin: Rejoin,
}

impl Default for SteamSettings {
    fn default() -> Self {
        Self {
            connector: ConnectorChoice::Auto,
            visibility: Visibility::Friends,
            rejoin: LOBBY_REJOIN,
        }
    }
}

/// Most lobby members. The session is 2 players; the spare room lets a
/// restarted game rejoin while Steam still lists its old membership.
pub const LOBBY_SIZE: u8 = 4;

/// Outside a run, how long a timed-out partner may come back.
pub const LOBBY_REJOIN: Rejoin = Rejoin::Within(Duration::from_secs(120));

impl SteamSettings {
    /// Whether the Steam connector is set up at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.connector != ConnectorChoice::Udp
    }
}

/// The raw keys, as read from the file.
#[derive(Debug, Clone, Copy)]
pub struct Raw<'a> {
    pub connector: &'a str,
    pub lobby: &'a str,
}

/// Validate, with one warning per value that was replaced.
#[must_use]
pub fn validate(raw: &Raw<'_>, w: &mut Vec<String>) -> SteamSettings {
    let d = SteamSettings::default();
    let connector = match raw.connector.trim().to_ascii_lowercase().as_str() {
        "auto" => ConnectorChoice::Auto,
        "steam" => ConnectorChoice::Steam,
        "udp" => ConnectorChoice::Udp,
        other => {
            w.push(format!(
                "connector = {other:?} is not auto, steam or udp; using auto"
            ));
            ConnectorChoice::Auto
        }
    };
    let visibility = match raw.lobby.trim().to_ascii_lowercase().as_str() {
        "friends" => Visibility::Friends,
        "invite_only" => Visibility::InviteOnly,
        "code" => Visibility::RoomCode,
        other => {
            w.push(format!(
                "steam_lobby = {other:?} is not friends, invite_only or code; using friends"
            ));
            Visibility::Friends
        }
    };
    let rejoin = d.rejoin;
    SteamSettings {
        connector,
        visibility,
        rejoin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> Raw<'static> {
        Raw {
            connector: "auto",
            lobby: "friends",
        }
    }

    #[test]
    fn defaults_and_bad_values() {
        let mut w = Vec::new();
        assert_eq!(validate(&raw(), &mut w), SteamSettings::default());
        assert!(w.is_empty());
        let bad = Raw {
            connector: "carrier pigeon",
            lobby: "public",
        };
        let s = validate(&bad, &mut w);
        assert_eq!(w.len(), 2, "{w:?}");
        assert_eq!(s.connector, ConnectorChoice::Auto);
        assert_eq!(s.visibility, Visibility::Friends);
        assert_eq!(s.rejoin, LOBBY_REJOIN);
        let s = validate(
            &Raw {
                connector: " UDP ",
                lobby: "code",
            },
            &mut Vec::new(),
        );
        assert!(!s.enabled());
        assert_eq!(s.visibility, Visibility::RoomCode);
        assert_eq!(s.rejoin, Rejoin::Within(Duration::from_secs(120)));
    }
}
