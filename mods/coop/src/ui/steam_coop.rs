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

//! The adapter from the Steam connector ([`crate::steam::SteamCoop`]) to the
//! page's seam ([`super::steam_link`]). [`adapt`] is pure: every connector
//! state becomes a [`SteamState`] with one ASCII sentence in the game's voice.
//! [`CoopSteam`] takes only the Steam lock (lock order: co-op, then Steam).

#![forbid(unsafe_code)]

use std::sync::Arc;

use super::steam_link::{
    SteamAction, SteamLink, SteamMember, SteamOffers, SteamState, SteamView, Visibility,
};
use crate::steam::view::{self as cv, Refusal, SteamError, SteamPage};

/// Shown while Steam is not up.
pub const UNAVAILABLE: &str =
    "Steam is not running, or this copy of the game has no Steam. Use Codes and LAN.";

/// Longest raw persona name kept (the page cuts to `text::NAME_MAX`).
const RAW_NAME_MAX: usize = 64;

/// A connector sentence made drawable: printable ASCII only (anything else
/// becomes `?`), control characters dropped, trimmed.
#[must_use]
pub fn ascii(text: &str) -> String {
    text.trim()
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect()
}

/// A persona name for the screen: control characters dropped, length
/// capped. Glyph filtering for the active language happens at draw time
/// (`text::display_name`).
#[must_use]
pub fn clean_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(RAW_NAME_MAX)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A refusal in the game's voice.
#[must_use]
pub fn refusal_text(r: &Refusal) -> String {
    match r {
        Refusal::VersionMismatch { what } => format!(
            "Versions differ ({}). Both players need the same game and co-op mod version.",
            ascii(what)
        ),
        Refusal::NotCoop => "That lobby is not a co-op lobby. The host needs the co-op mod.".into(),
        Refusal::HostInRun => {
            "The host is in a run. Join from the main menu before it starts.".into()
        }
        Refusal::Full => "The host's game is full.".into(),
        Refusal::Other(s) => ascii(s),
    }
}

/// An error in the game's voice.
#[must_use]
pub fn error_text(e: &SteamError) -> String {
    match e {
        SteamError::CreateFailed => {
            "Steam could not open a lobby. Check that Steam is online, then try again.".into()
        }
        SteamError::JoinFailed => {
            "Steam could not put you in that lobby. It may be closed or full.".into()
        }
        SteamError::LobbyEmpty => "Nobody is hosting that lobby any more.".into(),
        SteamError::BadCode(why) => format!("Room code: {}.", ascii(why)),
        SteamError::NoLobbyForCode(code) => format!(
            "No lobby has the room code {}. Check it with the host.",
            ascii(code)
        ),
        SteamError::HandshakeTimeout => {
            "The host did not answer. Try again, or ask for a new invite.".into()
        }
        SteamError::Link(s) => format!("The link failed: {}", ascii(s)),
        SteamError::Call(what) => format!("Steam refused a request ({}).", ascii(what)),
    }
}

fn state(v: &cv::SteamView) -> SteamState {
    if !v.available {
        return SteamState::Unavailable {
            reason: UNAVAILABLE.into(),
        };
    }
    match v.page {
        SteamPage::Unavailable => SteamState::Unavailable {
            reason: UNAVAILABLE.into(),
        },
        SteamPage::Idle => SteamState::Idle,
        SteamPage::Creating => SteamState::Creating,
        SteamPage::Hosting => SteamState::Hosting,
        SteamPage::Joining => SteamState::Joining,
        SteamPage::InLobby => SteamState::InLobby,
        SteamPage::Connecting => SteamState::Connecting,
        SteamPage::Linked => SteamState::Linked,
        SteamPage::InRun => SteamState::InRun,
        SteamPage::PeerLost => SteamState::PeerLost {
            lost_for_s: v.lost_for_s,
        },
        SteamPage::Refused => SteamState::Refused {
            reason: v
                .refusal
                .as_ref()
                .map_or_else(|| "The host turned you away.".into(), refusal_text),
        },
        SteamPage::Error => SteamState::Error {
            message: v.error.as_ref().map_or_else(
                || "Something went wrong with Steam. Try again.".into(),
                error_text,
            ),
        },
        SteamPage::Left => SteamState::Left,
    }
}

fn visibility(v: cv::Visibility) -> Visibility {
    match v {
        cv::Visibility::Friends => Visibility::Friends,
        cv::Visibility::InviteOnly => Visibility::InviteOnly,
        cv::Visibility::RoomCode => Visibility::RoomCode,
    }
}

/// The connector's view as the page's.
#[must_use]
pub fn adapt(v: &cv::SteamView) -> SteamView {
    let members = v
        .members
        .iter()
        .map(|m| SteamMember {
            name: clean_name(&m.name),
            me: m.me,
            host: m.host,
            linked: m.linked,
            rtt_ms: if m.linked && !m.me { v.rtt_ms } else { None },
        })
        .collect();
    let o = v.offers;
    SteamView {
        state: state(v),
        members,
        max_members: u32::from(v.max_members),
        is_host: v.role == Some(cv::SteamRole::Host),
        partner: v.partner.as_deref().map(clean_name),
        rtt_ms: v.rtt_ms,
        visibility: visibility(v.visibility),
        room_code: v.room_code.as_deref().map(ascii),
        notice: v.notice.as_deref().map(ascii),
        invites_arrive: v.invites_arrive,
        offers: SteamOffers {
            host: o.host,
            invite: o.invite,
            join_code: o.join_code,
            leave: o.leave,
            retry: o.retry,
            visibility: o.visibility,
        },
    }
}

/// The page's action as the connector's.
#[must_use]
pub fn action(a: SteamAction) -> cv::SteamAction {
    match a {
        SteamAction::Host => cv::SteamAction::Host,
        SteamAction::SetVisibility(v) => cv::SteamAction::SetVisibility(match v {
            Visibility::Friends => cv::Visibility::Friends,
            Visibility::InviteOnly => cv::Visibility::InviteOnly,
            Visibility::RoomCode => cv::Visibility::RoomCode,
        }),
        SteamAction::OpenInvite => cv::SteamAction::OpenInvite,
        SteamAction::JoinByCode(c) => cv::SteamAction::JoinCode(c),
        SteamAction::Leave => cv::SteamAction::Leave,
        SteamAction::Retry => cv::SteamAction::Retry,
    }
}

/// The page's [`SteamLink`] over the real connector.
#[derive(Debug)]
pub struct CoopSteam(pub Arc<crate::steam::SteamCoop>);

impl SteamLink for CoopSteam {
    fn view(&self) -> SteamView {
        adapt(&self.0.view())
    }

    fn act(&mut self, a: SteamAction) -> Result<(), String> {
        // Queued for the next pump tick; failures come back in the view.
        self.0.act(action(a));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv::{MemberView, Offers, SteamRole};

    fn up(page: SteamPage) -> cv::SteamView {
        cv::SteamView {
            available: true,
            page,
            max_members: 4,
            invites_arrive: true,
            ..cv::SteamView::default()
        }
    }

    /// The sentence a refused or failed page carries (empty otherwise).
    fn sentence(v: &cv::SteamView) -> String {
        match adapt(v).state {
            SteamState::Refused { reason } => reason,
            SteamState::Error { message } => message,
            _ => String::new(),
        }
    }

    #[test]
    fn every_page_maps_one_to_one() {
        let cases = [
            (SteamPage::Idle, SteamState::Idle),
            (SteamPage::Creating, SteamState::Creating),
            (SteamPage::Hosting, SteamState::Hosting),
            (SteamPage::Joining, SteamState::Joining),
            (SteamPage::InLobby, SteamState::InLobby),
            (SteamPage::Connecting, SteamState::Connecting),
            (SteamPage::Linked, SteamState::Linked),
            (SteamPage::InRun, SteamState::InRun),
            (SteamPage::Left, SteamState::Left),
            (
                SteamPage::PeerLost,
                SteamState::PeerLost { lost_for_s: None },
            ),
        ];
        for (page, want) in cases {
            assert_eq!(adapt(&up(page)).state, want, "{page:?}");
        }
        let unavailable = SteamState::Unavailable {
            reason: UNAVAILABLE.into(),
        };
        assert_eq!(adapt(&up(SteamPage::Unavailable)).state, unavailable);
        // `available = false` wins over any page.
        let mut down = up(SteamPage::Hosting);
        down.available = false;
        assert_eq!(adapt(&down).state, unavailable);
        assert!(!adapt(&down).available());
    }

    #[test]
    fn peer_lost_carries_how_long() {
        let mut v = up(SteamPage::PeerLost);
        v.lost_for_s = Some(42);
        assert_eq!(
            adapt(&v).state,
            SteamState::PeerLost {
                lost_for_s: Some(42)
            }
        );
        assert!(adapt(&v).reconnecting());
    }

    #[test]
    fn every_refusal_reads_well() {
        let all = [
            Refusal::VersionMismatch {
                what: "game build".into(),
            },
            Refusal::NotCoop,
            Refusal::HostInRun,
            Refusal::Full,
            Refusal::Other("Host says no \u{2014} sorry".into()),
        ];
        for r in all {
            let mut v = up(SteamPage::Refused);
            v.refusal = Some(r.clone());
            assert!(
                matches!(adapt(&v).state, SteamState::Refused { .. }),
                "{r:?}"
            );
            let reason = sentence(&v);
            assert!(reason.is_ascii() && !reason.is_empty(), "{reason}");
            assert_eq!(reason, refusal_text(&r));
        }
        assert!(
            refusal_text(&Refusal::VersionMismatch {
                what: "game build".into()
            })
            .contains("(game build)")
        );
        assert_eq!(
            refusal_text(&Refusal::Other("Host says no \u{2014} sorry".into())),
            "Host says no ? sorry"
        );
        // A refused page without the detail still says something.
        assert!(!sentence(&up(SteamPage::Refused)).is_empty());
    }

    #[test]
    fn every_error_reads_well() {
        let all = [
            SteamError::CreateFailed,
            SteamError::JoinFailed,
            SteamError::LobbyEmpty,
            SteamError::BadCode("5 characters".into()),
            SteamError::NoLobbyForCode("K7QX2".into()),
            SteamError::HandshakeTimeout,
            SteamError::Link("socket closed".into()),
            SteamError::Call("steam_lobby_create".into()),
        ];
        for e in all {
            let mut v = up(SteamPage::Error);
            v.error = Some(e.clone());
            assert!(matches!(adapt(&v).state, SteamState::Error { .. }), "{e:?}");
            let message = sentence(&v);
            assert!(message.is_ascii() && !message.is_empty(), "{message}");
            assert_eq!(message, error_text(&e));
        }
        assert!(error_text(&SteamError::NoLobbyForCode("K7QX2".into())).contains("K7QX2"));
        assert!(error_text(&SteamError::BadCode("5 characters".into())).contains("5 characters"));
        assert!(!sentence(&up(SteamPage::Error)).is_empty());
    }

    #[test]
    fn hosting_with_a_room_code_and_members() {
        let mut v = up(SteamPage::Linked);
        v.role = Some(SteamRole::Host);
        v.visibility = cv::Visibility::RoomCode;
        v.room_code = Some("K7QX2".into());
        v.partner = Some("Sam\u{7}".into());
        v.rtt_ms = Some(41.0);
        v.members = vec![
            MemberView {
                name: "Ace".into(),
                me: true,
                host: true,
                linked: false,
            },
            MemberView {
                name: "Sam\u{7}\n".into(),
                me: false,
                host: false,
                linked: true,
            },
        ];
        v.notice = Some("A player left the lobby.".into());
        v.offers = Offers {
            invite: true,
            leave: true,
            ..Offers::default()
        };
        let a = adapt(&v);
        assert!(a.is_host);
        assert_eq!(a.visibility, Visibility::RoomCode);
        assert_eq!(a.room_code.as_deref(), Some("K7QX2"));
        assert_eq!(a.partner.as_deref(), Some("Sam"));
        assert_eq!(a.max_members, 4);
        assert_eq!(a.members.len(), 2);
        assert_eq!(a.members[1].name, "Sam", "control characters dropped");
        assert_eq!(a.members[1].rtt_ms, Some(41.0), "the linked partner's ms");
        assert_eq!(a.members[0].rtt_ms, None, "never on the local member");
        assert!(a.members[0].me && a.members[0].host);
        assert_eq!(a.notice.as_deref(), Some("A player left the lobby."));
        assert!(a.offers.invite && a.offers.leave && !a.offers.host);
        assert!(a.invites_arrive);
    }

    #[test]
    fn names_are_capped_and_kept_for_the_page_to_filter() {
        let long: String = "x".repeat(200);
        assert_eq!(clean_name(&long).chars().count(), RAW_NAME_MAX);
        // Non-Latin names stay: `text::display_name` filters them per language.
        assert_eq!(clean_name(" \u{30B5}\u{30E0} "), "\u{30B5}\u{30E0}");
    }

    #[test]
    fn visibilities_and_actions_round_trip() {
        for (c, p) in [
            (cv::Visibility::Friends, Visibility::Friends),
            (cv::Visibility::InviteOnly, Visibility::InviteOnly),
            (cv::Visibility::RoomCode, Visibility::RoomCode),
        ] {
            let mut v = up(SteamPage::Idle);
            v.visibility = c;
            assert_eq!(adapt(&v).visibility, p);
            assert_eq!(
                action(SteamAction::SetVisibility(p)),
                cv::SteamAction::SetVisibility(c)
            );
        }
        assert_eq!(action(SteamAction::Host), cv::SteamAction::Host);
        assert_eq!(action(SteamAction::OpenInvite), cv::SteamAction::OpenInvite);
        assert_eq!(
            action(SteamAction::JoinByCode("K7QX2".into())),
            cv::SteamAction::JoinCode("K7QX2".into())
        );
        assert_eq!(action(SteamAction::Leave), cv::SteamAction::Leave);
        assert_eq!(action(SteamAction::Retry), cv::SteamAction::Retry);
    }

    #[test]
    fn joiner_is_not_host() {
        let mut v = up(SteamPage::Linked);
        v.role = Some(SteamRole::Join);
        assert!(!adapt(&v).is_host);
        assert!(!adapt(&up(SteamPage::Idle)).is_host);
    }
}
