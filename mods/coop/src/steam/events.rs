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

//! Async Steam results (`Other_69`), decoded from the `async_load` map of
//! GMEXT-Steamworks 1.6.9. Only the fields the connector uses are read.

#![forbid(unsafe_code)]

use ndml_sdk::Value;

/// One result the connector cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamEvent {
    /// `lobby_created`: `success` (no I/O failure and result OK) and the id.
    LobbyCreated {
        ok: bool,
        /// 0 on failure.
        lobby: u64,
    },
    /// `lobby_joined`: `success` only means "no I/O failure".
    LobbyJoined { ok: bool, lobby: u64 },
    /// `lobby_join_requested`: an invite or "Join Game" accepted in-game.
    JoinRequested { lobby: u64 },
    /// `lobby_chat_update`: someone entered or left (`change_flags`).
    MemberChange {
        /// Entered (1) or left / disconnected / kicked / banned (2, 4, 8, 16).
        flags: u32,
    },
    /// `lobby_list`: a lobby search finished (also visible by polling).
    LobbyList,
}

/// The `event_type` key.
pub const EVENT_TYPE: &str = "event_type";

/// The keys to read for an `event_type` (empty: not decoded).
#[must_use]
pub fn keys_for(event_type: &str) -> &'static [&'static str] {
    match event_type {
        "lobby_created" | "lobby_joined" => &["success", "lobby_id"],
        "lobby_join_requested" => &["lobby_id"],
        "lobby_chat_update" => &["change_flags"],
        _ => &[],
    }
}

/// Whether `event_type` is decoded here.
#[must_use]
pub fn wanted(event_type: &str) -> bool {
    matches!(
        event_type,
        "lobby_created"
            | "lobby_joined"
            | "lobby_join_requested"
            | "lobby_chat_update"
            | "lobby_list"
    )
}

/// A Steam or lobby id from an int64, a number or a digit string (0 if none).
#[must_use]
pub fn id_of(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Int(i)) => *i as u64,
        Some(Value::Real(r)) if r.is_finite() && *r > 0.0 => *r as u64,
        Some(Value::Str(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn truth(v: Option<&Value>) -> bool {
    v.and_then(Value::as_bool).unwrap_or(false)
}

/// Decode one event from its type and a lookup of its fields.
#[must_use]
pub fn parse(event_type: &str, get: &dyn Fn(&str) -> Option<Value>) -> Option<SteamEvent> {
    Some(match event_type {
        "lobby_created" => SteamEvent::LobbyCreated {
            ok: truth(get("success").as_ref()),
            lobby: id_of(get("lobby_id").as_ref()),
        },
        "lobby_joined" => SteamEvent::LobbyJoined {
            ok: truth(get("success").as_ref()),
            lobby: id_of(get("lobby_id").as_ref()),
        },
        "lobby_join_requested" => SteamEvent::JoinRequested {
            lobby: id_of(get("lobby_id").as_ref()),
        },
        "lobby_chat_update" => SteamEvent::MemberChange {
            flags: get("change_flags")
                .and_then(|v| v.as_f64())
                .map_or(0, |f| f.max(0.0) as u32),
        },
        "lobby_list" => SteamEvent::LobbyList,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields<'a>(pairs: &'a [(&'a str, Value)]) -> impl Fn(&str) -> Option<Value> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn decodes_the_events_we_use() {
        let f = [
            ("success", Value::Bool(true)),
            ("lobby_id", Value::Int(0x0186_0000_0000_0042)),
            ("result", Value::Real(1.0)),
        ];
        assert_eq!(
            parse("lobby_created", &fields(&f)),
            Some(SteamEvent::LobbyCreated {
                ok: true,
                lobby: 0x0186_0000_0000_0042
            })
        );
        let f = [("success", Value::Real(0.0))];
        assert_eq!(
            parse("lobby_joined", &fields(&f)),
            Some(SteamEvent::LobbyJoined {
                ok: false,
                lobby: 0
            })
        );
        let f = [("lobby_id", Value::Str("12345".into()))];
        assert_eq!(
            parse("lobby_join_requested", &fields(&f)),
            Some(SteamEvent::JoinRequested { lobby: 12345 })
        );
        let f = [("change_flags", Value::Real(2.0))];
        assert_eq!(
            parse("lobby_chat_update", &fields(&f)),
            Some(SteamEvent::MemberChange { flags: 2 })
        );
        assert_eq!(
            parse("lobby_list", &fields(&[])),
            Some(SteamEvent::LobbyList)
        );
        assert_eq!(parse("leaderboard_download", &fields(&[])), None);
        assert!(wanted("lobby_list") && !wanted("user_stats_received"));
        assert_eq!(keys_for("lobby_join_requested"), &["lobby_id"]);
        assert!(keys_for("leaderboard_download").is_empty());
    }

    #[test]
    fn ids_in_every_shape() {
        assert_eq!(id_of(Some(&Value::Int(-1))), u64::MAX);
        assert_eq!(id_of(Some(&Value::Real(42.0))), 42);
        assert_eq!(id_of(Some(&Value::Real(f64::NAN))), 0);
        assert_eq!(id_of(Some(&Value::Str(" 7 ".into()))), 7);
        assert_eq!(id_of(Some(&Value::Undefined)), 0);
        assert_eq!(id_of(None), 0);
    }
}
