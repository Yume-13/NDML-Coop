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

//! The co-op page's interface to the Steam connector: a read-only
//! [`SteamView`] and the [`SteamAction`]s it can ask for. Plain data, safe
//! under the co-op lock; actions run on the next pump tick.

#![forbid(unsafe_code)]

/// Which page the co-op UI should show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SteamPage {
    /// Steam isn't running (or this is a non-Steam build): show the UDP page.
    #[default]
    Unavailable,
    /// Steam works, no lobby: offer Host and Join by room code.
    Idle,
    Creating,
    /// The lobby is up and nobody has joined yet: offer Invite.
    Hosting,
    /// Looking up a room code or entering someone's lobby.
    Joining,
    /// In the host's lobby, checking versions before connecting.
    InLobby,
    /// A member is in the lobby; the game connection is being set up.
    Connecting,
    /// Connected to the partner, on the menu.
    Linked,
    InRun,
    /// The connection dropped; it keeps trying (see [`SteamView::lost_for_s`]).
    PeerLost,
    /// The host turned this player away (see [`SteamView::refusal`]).
    Refused,
    /// Something failed (see [`SteamView::error`]); Retry is offered.
    Error,
    /// The session ended because the other side left.
    Left,
}

impl SteamPage {
    /// The key used in `status.toml` (`[steam] page`).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Idle => "idle",
            Self::Creating => "creating",
            Self::Hosting => "hosting",
            Self::Joining => "joining",
            Self::InLobby => "in_lobby",
            Self::Connecting => "connecting",
            Self::Linked => "linked",
            Self::InRun => "in_run",
            Self::PeerLost => "peer_lost",
            Self::Refused => "refused",
            Self::Error => "error",
            Self::Left => "left",
        }
    }
}

/// Who may join a lobby hosted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    /// Friends join from their friends list ("Join Game"); invites work.
    #[default]
    Friends,
    /// Only people invited through the Steam overlay.
    InviteOnly,
    /// A short room code; invites still work, the friends list hides it.
    RoomCode,
}

impl Visibility {
    /// The key used in the config and `status.toml`.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Friends => "friends",
            Self::InviteOnly => "invite_only",
            Self::RoomCode => "code",
        }
    }
}

/// The local part in the Steam session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteamRole {
    /// Hosting (the local lobby and world).
    Host,
    /// Joined someone's lobby.
    Join,
}

/// Why the host (or its lobby) turned this player away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Different co-op mod version or game build.
    VersionMismatch {
        /// What differs, in words (`"game build"` or `"co-op mod version"`).
        what: String,
    },
    /// The lobby isn't a co-op lobby (no `ndml` data).
    NotCoop,
    /// The host is in a run; join from the main menu.
    HostInRun,
    Full,
    /// Something else, in the host's words.
    Other(String),
}

impl Refusal {
    /// The sentence for the player.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::VersionMismatch { what } => format!(
                "Version mismatch ({what}): both players need the same game build and co-op \
                 mod version."
            ),
            Self::NotCoop => "That lobby is not a co-op lobby (the host may not have the co-op \
                              mod)."
                .into(),
            Self::HostInRun => {
                "Your partner is in a run right now. Join when they are back in the main menu."
                    .into()
            }
            Self::Full => "The host's session is full.".into(),
            Self::Other(s) => s.clone(),
        }
    }

    /// A short machine-readable key.
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Self::VersionMismatch { .. } => "version_mismatch",
            Self::NotCoop => "not_coop",
            Self::HostInRun => "run_in_progress",
            Self::Full => "full",
            Self::Other(_) => "other",
        }
    }
}

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamError {
    /// Steam returned an error creating the lobby (or no lobby in time).
    CreateFailed,
    /// Steam could not put this player in the lobby.
    JoinFailed,
    /// The lobby has nobody else in it (the host left before the join).
    LobbyEmpty,
    /// The room code is malformed (the text says how).
    BadCode(String),
    NoLobbyForCode(String),
    /// The host never answered the game connection.
    HandshakeTimeout,
    /// The game connection failed another way (the network's words).
    Link(String),
    /// A Steam call failed (which one).
    Call(String),
}

impl SteamError {
    /// The sentence for the player.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::CreateFailed => "Steam could not create the lobby. Check that you are online \
                                   in Steam and try again."
                .into(),
            Self::JoinFailed => "Steam could not put you in that lobby. It may be closed or full; \
                                 ask the host to invite you again."
                .into(),
            Self::LobbyEmpty => "Nobody is hosting in that lobby any more.".into(),
            Self::BadCode(why) => format!("Room code: {why}."),
            Self::NoLobbyForCode(code) => {
                format!("No lobby has the room code {code}. Check it with the host.")
            }
            Self::HandshakeTimeout => "The host did not answer. Steam could not connect you; \
                                       try again, or ask the host to invite you again."
                .into(),
            Self::Link(s) => s.clone(),
            Self::Call(what) => format!("Steam refused a request ({what})."),
        }
    }

    /// A short machine-readable key.
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Self::CreateFailed => "create_failed",
            Self::JoinFailed => "join_failed",
            Self::LobbyEmpty => "lobby_empty",
            Self::BadCode(_) => "bad_code",
            Self::NoLobbyForCode(_) => "no_lobby_for_code",
            Self::HandshakeTimeout => "handshake_timeout",
            Self::Link(_) => "link",
            Self::Call(_) => "call",
        }
    }
}

/// One lobby member, by persona name only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    /// Steam persona name as plain text (`"?"` until Steam has it).
    pub name: String,
    /// This player.
    pub me: bool,
    /// The co-op session's host.
    pub host: bool,
    /// Connected to this player in the game session.
    pub linked: bool,
}

/// What the UI may offer right now (buttons to enable).
#[allow(clippy::struct_excessive_bools)] // one flag per button
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Offers {
    pub host: bool,
    /// [`SteamAction::OpenInvite`] (the Steam overlay must be enabled).
    pub invite: bool,
    pub join_code: bool,
    pub leave: bool,
    pub retry: bool,
    /// [`SteamAction::SetVisibility`] (also while hosting).
    pub visibility: bool,
}

/// The Steam connector at a glance.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SteamView {
    /// Steam is running and logged in (else the page is `Unavailable`).
    pub available: bool,
    pub page: SteamPage,
    /// Host or joiner, while in a lobby.
    pub role: Option<SteamRole>,
    /// The visibility a lobby hosted here has (or will have).
    pub visibility: Visibility,
    /// Lobby members (this player included), host first.
    pub members: Vec<MemberView>,
    pub max_members: u8,
    /// The partner's persona name, while there is one.
    pub partner: Option<String>,
    /// Round trip to the partner, while linked.
    pub rtt_ms: Option<f64>,
    /// Seconds since the connection dropped ([`SteamPage::PeerLost`]).
    pub lost_for_s: Option<u64>,
    /// The room code to read out, while hosting with [`Visibility::RoomCode`].
    pub room_code: Option<String>,
    pub refusal: Option<Refusal>,
    /// With [`SteamPage::Error`] (also kept as a hint on other pages).
    pub error: Option<SteamError>,
    /// A short one-off message (an invite received, a member left, ...).
    pub notice: Option<String>,
    /// Invites accepted while the game runs arrive (the listener works).
    pub invites_arrive: bool,
    pub offers: Offers,
}

impl SteamView {
    /// The main message of the page in one sentence (for a minimal UI).
    #[must_use]
    pub fn headline(&self) -> String {
        let partner = self.partner.as_deref().unwrap_or("your partner");
        match self.page {
            SteamPage::Unavailable => "Steam is not available: use connection codes.".into(),
            SteamPage::Idle => "Host a lobby and invite a friend, or join with a room code.".into(),
            SteamPage::Creating => "Creating the lobby...".into(),
            SteamPage::Hosting => match &self.room_code {
                Some(c) => {
                    format!("Lobby open. Room code {c}. Invite a friend or read the code out.")
                }
                None => "Lobby open. Invite a friend through Steam.".into(),
            },
            SteamPage::Joining => "Joining the lobby...".into(),
            SteamPage::InLobby => "In the lobby, checking versions...".into(),
            SteamPage::Connecting => format!("Connecting to {partner}..."),
            SteamPage::Linked => match self.rtt_ms {
                Some(r) => format!("Linked with {partner} ({r:.0} ms). Start a run together."),
                None => format!("Linked with {partner}. Start a run together."),
            },
            SteamPage::InRun => format!("Playing with {partner}."),
            SteamPage::PeerLost => format!(
                "Connection to {partner} lost{}. Reconnecting...",
                self.lost_for_s.map_or_default(|s| format!(" {s} s ago"))
            ),
            SteamPage::Refused => self
                .refusal
                .as_ref()
                .map_or_else(|| "Refused by the host.".into(), Refusal::message),
            SteamPage::Error => self
                .error
                .as_ref()
                .map_or_else(|| "Something went wrong.".into(), SteamError::message),
            SteamPage::Left => format!("{partner} left the session."),
        }
    }
}

/// What the UI can ask the Steam connector to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamAction {
    /// Create a lobby (with the current [`Visibility`]) and host.
    Host,
    /// Change who may join (also while hosting).
    SetVisibility(Visibility),
    /// Shorthand: invite-only on, or back to friends.
    SetInviteOnly(bool),
    /// Open the Steam overlay's invite dialog (while in a lobby).
    OpenInvite,
    /// Join the lobby with this room code (any case, dashes and spaces ok).
    JoinCode(String),
    /// Leave the lobby and the session.
    Leave,
    /// Repeat the last attempt after an error, refusal or a partner leaving.
    Retry,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_messages() {
        let pages = [
            SteamPage::Unavailable,
            SteamPage::Idle,
            SteamPage::Creating,
            SteamPage::Hosting,
            SteamPage::Joining,
            SteamPage::InLobby,
            SteamPage::Connecting,
            SteamPage::Linked,
            SteamPage::InRun,
            SteamPage::PeerLost,
            SteamPage::Refused,
            SteamPage::Error,
            SteamPage::Left,
        ];
        let keys: std::collections::BTreeSet<_> = pages.iter().map(|p| p.key()).collect();
        assert_eq!(keys.len(), pages.len());
        for p in pages {
            let v = SteamView {
                page: p,
                ..SteamView::default()
            };
            assert!(!v.headline().is_empty());
        }
        assert!(
            Refusal::VersionMismatch {
                what: "game build".into()
            }
            .message()
            .contains("game build")
        );
        assert!(
            SteamError::NoLobbyForCode("K7QX2".into())
                .message()
                .contains("K7QX2")
        );
        assert_eq!(Visibility::default().key(), "friends");
        let v = SteamView {
            page: SteamPage::Hosting,
            room_code: Some("K7QX2".into()),
            ..SteamView::default()
        };
        assert!(v.headline().contains("K7QX2"));
        let v = SteamView {
            page: SteamPage::Linked,
            partner: Some("Wing".into()),
            rtt_ms: Some(41.6),
            ..SteamView::default()
        };
        assert_eq!(
            v.headline(),
            "Linked with Wing (42 ms). Start a run together."
        );
    }
}
