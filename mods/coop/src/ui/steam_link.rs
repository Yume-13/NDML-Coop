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

//! The one seam between the co-op page and the Steam connector: a read-only
//! [`SteamView`] and the [`SteamAction`]s the page asks for.
//! [`super::steam_coop::CoopSteam`] adapts the connector to [`SteamLink`];
//! [`NotYet`] stands in when there is none (`connector = "udp"`).
//!
//! Actions are queued: Steam calls run on the game thread from the
//! connector's pump. Persona names are shown on screen and never logged.

#![forbid(unsafe_code)]

/// Where the Steam connector is (the connector's `SteamPage`, one to one).
#[derive(Debug, Clone, PartialEq)]
pub enum SteamState {
    /// No Steam (off, not running or not logged in); `reason` is shown.
    Unavailable { reason: String },
    /// Steam is up, no lobby.
    Idle,
    /// The lobby is being created.
    Creating,
    /// The lobby is open (room code in [`SteamView::room_code`]).
    Hosting,
    /// Looking up a room code or entering someone's lobby.
    Joining,
    /// In the host's lobby, checking versions before connecting.
    InLobby,
    /// The game connection with the partner is being set up.
    Connecting,
    /// Linked, on the menu.
    Linked,
    /// Linked and in a run.
    InRun,
    /// The link dropped and the connector is reconnecting (seconds since).
    PeerLost { lost_for_s: Option<u64> },
    /// The host turned this player away (the sentence to show).
    Refused { reason: String },
    /// Something failed (the sentence to show).
    Error { message: String },
    /// The session ended because the other side left.
    Left,
}

/// Who may join a lobby hosted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    /// Friends see the lobby ("Join Game") and invites work.
    #[default]
    Friends,
    /// Only people invited through the Steam overlay.
    InviteOnly,
    /// A short room code to read out; invites still work.
    RoomCode,
}

impl Visibility {
    const ALL: [Self; 3] = [Self::Friends, Self::InviteOnly, Self::RoomCode];

    /// The value shown on the "Who Can Join" row.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Friends => "Friends",
            Self::InviteOnly => "Invite Only",
            Self::RoomCode => "Room Code",
        }
    }

    /// One sentence on what the choice means.
    #[must_use]
    pub fn explain(self) -> &'static str {
        match self {
            Self::Friends => "Friends can also join with Join Game on your Steam profile.",
            Self::InviteOnly => "Invite only: friends need an invite from you.",
            Self::RoomCode => "Room code: your lobby gets a code to read out. Invites still work.",
        }
    }

    /// The next choice left (`dir < 0`) or right.
    #[must_use]
    pub fn step(self, dir: i32) -> Self {
        let i = Self::ALL.iter().position(|v| *v == self).unwrap_or(0);
        let n = Self::ALL.len();
        Self::ALL[if dir < 0 {
            (i + n - 1) % n
        } else {
            (i + 1) % n
        }]
    }
}

/// A lobby member (this player is in the list too).
#[derive(Debug, Clone, PartialEq)]
pub struct SteamMember {
    /// Persona name, control characters removed (screen only, never logged).
    pub name: String,
    /// This player.
    pub me: bool,
    /// The session's host.
    pub host: bool,
    /// Connected to this player in the game session.
    pub linked: bool,
    /// Round trip, for the linked partner.
    pub rtt_ms: Option<f64>,
}

/// What the connector says makes sense now (one flag per row).
#[allow(clippy::struct_excessive_bools)] // one flag per button
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SteamOffers {
    pub host: bool,
    /// Needs the Steam overlay.
    pub invite: bool,
    pub join_code: bool,
    pub leave: bool,
    /// After an error, a refusal, or the partner leaving.
    pub retry: bool,
    pub visibility: bool,
}

/// What the page shows about Steam.
#[derive(Debug, Clone, PartialEq)]
pub struct SteamView {
    pub state: SteamState,
    /// Lobby members, host first (empty outside a lobby).
    pub members: Vec<SteamMember>,
    /// Lobby capacity.
    pub max_members: u32,
    /// This game hosts (its lobby, its world).
    pub is_host: bool,
    /// The partner's name, while there is one.
    pub partner: Option<String>,
    /// Round trip to the partner, while linked.
    pub rtt_ms: Option<f64>,
    /// Who may join a lobby this game hosts (or will host).
    pub visibility: Visibility,
    /// The room code to read out ([`Visibility::RoomCode`], hosting).
    pub room_code: Option<String>,
    /// A one-off message from the connector.
    pub notice: Option<String>,
    /// Invites accepted while the game runs reach it (the async listener).
    pub invites_arrive: bool,
    pub offers: SteamOffers,
}

impl SteamView {
    /// A view with no Steam, saying why.
    #[must_use]
    pub fn unavailable(reason: &str) -> Self {
        Self {
            state: SteamState::Unavailable {
                reason: reason.to_owned(),
            },
            members: Vec::new(),
            max_members: 2,
            is_host: false,
            partner: None,
            rtt_ms: None,
            visibility: Visibility::Friends,
            room_code: None,
            notice: None,
            invites_arrive: false,
            offers: SteamOffers::default(),
        }
    }

    /// Steam can be used at all.
    #[must_use]
    pub fn available(&self) -> bool {
        !matches!(self.state, SteamState::Unavailable { .. })
    }

    /// In a lobby or on the way into one (the "Leave Lobby" states).
    #[must_use]
    pub fn in_lobby(&self) -> bool {
        matches!(
            self.state,
            SteamState::Creating
                | SteamState::Hosting
                | SteamState::Joining
                | SteamState::InLobby
                | SteamState::Connecting
                | SteamState::Linked
                | SteamState::InRun
                | SteamState::PeerLost { .. }
        )
    }

    /// A partner is linked, being linked, or being reconnected.
    #[must_use]
    pub fn with_partner(&self) -> bool {
        matches!(
            self.state,
            SteamState::Connecting
                | SteamState::Linked
                | SteamState::InRun
                | SteamState::PeerLost { .. }
        )
    }

    /// The connector is reconnecting to a lost partner.
    #[must_use]
    pub fn reconnecting(&self) -> bool {
        matches!(self.state, SteamState::PeerLost { .. })
    }
}

/// What the page asks the connector to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamAction {
    /// Create a lobby with the current [`Visibility`].
    Host,
    /// Change who may join (before or while hosting).
    SetVisibility(Visibility),
    /// The Steam overlay's invite dialog.
    OpenInvite,
    /// Find and join the lobby with this room code.
    JoinByCode(String),
    /// Leave the lobby and the session.
    Leave,
    /// Repeat the last attempt (after an error, a refusal, the partner leaving).
    Retry,
}

/// The connector as the page sees it.
pub trait SteamLink: Send + std::fmt::Debug {
    /// The state to show this frame.
    fn view(&self) -> SteamView;
    /// Queue an action; `Err` is shown on the page as is.
    fn act(&mut self, action: SteamAction) -> Result<(), String>;
}

/// No connector: Steam is off for this session, and says why.
#[derive(Debug, Clone)]
pub struct NotYet {
    reason: String,
}

impl NotYet {
    /// `connector = "udp"`.
    #[must_use]
    pub fn udp() -> Self {
        Self {
            reason: "Steam co-op is off in the co-op settings (connector = udp).".into(),
        }
    }
}

impl Default for NotYet {
    fn default() -> Self {
        Self {
            reason: super::text::en::NOT_YET.into(),
        }
    }
}

impl SteamLink for NotYet {
    fn view(&self) -> SteamView {
        SteamView::unavailable(&self.reason)
    }

    fn act(&mut self, _action: SteamAction) -> Result<(), String> {
        Err(self.reason.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_why() {
        let mut s = NotYet::default();
        let v = s.view();
        assert!(!v.available() && !v.in_lobby() && !v.with_partner());
        assert!(
            s.act(SteamAction::OpenInvite)
                .unwrap_err()
                .contains("not available yet")
        );
        let mut u = NotYet::udp();
        assert!(
            u.act(SteamAction::Host)
                .unwrap_err()
                .contains("connector = udp")
        );
    }

    #[test]
    fn state_groups() {
        let view = |state| SteamView {
            state,
            ..SteamView::unavailable("")
        };
        assert!(view(SteamState::Hosting).in_lobby());
        assert!(!view(SteamState::Idle).in_lobby());
        assert!(!view(SteamState::Left).in_lobby());
        let lost = view(SteamState::PeerLost {
            lost_for_s: Some(3),
        });
        assert!(lost.in_lobby() && lost.with_partner() && lost.reconnecting());
        assert!(!view(SteamState::Hosting).with_partner());
        assert!(view(SteamState::InRun).with_partner());
    }

    #[test]
    fn visibility_cycles_both_ways() {
        assert_eq!(Visibility::Friends.step(1), Visibility::InviteOnly);
        assert_eq!(Visibility::RoomCode.step(1), Visibility::Friends);
        assert_eq!(Visibility::Friends.step(-1), Visibility::RoomCode);
        for v in Visibility::ALL {
            assert!(v.label().is_ascii() && v.explain().is_ascii());
        }
    }
}
