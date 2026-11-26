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

//! The co-op pages: rows, what each row does, the confirmation question, the
//! text entry and the right-hand column. Pure. The glue shows them through
//! the game's main menu ("page mode"); the page answers confirm, left/right
//! and back with [`Effect`]s.
//!
//! **Co-op** holds the Steam rows, colour, labels, wild mods, "Codes and LAN" and back;
//! **Codes and LAN** holds the lobby's code and address requests. Steam goes
//! through [`super::steam_link`] only.

#![forbid(unsafe_code)]

use ndml_net::lobby::{Connector, LobbyRequest, LobbyState, RoleChoice, Status};

use super::lan::{
    AddrCheck, CodeCheck, Field, LanEffect, LanForm, bad_link, check_code, check_direct,
    link_lines, push_limited,
};
use super::steam_link::{SteamAction, SteamState, SteamView};
use super::style::color;
use super::text::{self, en};
use crate::wild::WildMods;
use crate::world::color::ColorChoice;

/// Players in a co-op session, as the page shows it ("PLAYERS n/2"). The
/// lobby agent admits one partner and the start gate waits for two.
pub const SESSION_PLAYERS: usize = 2;

/// Frames a message stays (60 per second).
pub const MESSAGE_FRAMES: u64 = 60 * 8;
/// Frames a warning takes to fade from the negative colour to white.
pub const WARN_FADE_FRAMES: u64 = 60;

/// How a line or message reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    /// Done / positive: the game's blue.
    Good,
    /// Can't do that (yet): starts in the negative colour, fades to white.
    Warn,
    /// Failed: the negative colour.
    Bad,
}

fn lerp_rgb(from: u32, to: u32, t: f64) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |shift: u32| {
        let lo = f64::from((from >> shift) & 0xFF);
        let hi = f64::from((to >> shift) & 0xFF);
        // In [0, 255] by construction.
        let v = (lo + (hi - lo) * t).round() as u32;
        v << shift
    };
    ch(0) | ch(8) | ch(16)
}

impl Tone {
    /// The colour `age` frames after the message appeared.
    #[must_use]
    pub fn color(self, age: u64) -> u32 {
        match self {
            Self::Info => color::WHITE,
            Self::Good => color::BLUE,
            Self::Bad => color::NEGATIVE,
            Self::Warn => {
                #[allow(clippy::cast_precision_loss)]
                let t = age as f64 / WARN_FADE_FRAMES as f64;
                lerp_rgb(color::NEGATIVE, color::WHITE, t)
            }
        }
    }
}

/// How a column line is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// `fontHeading`, blue, upper case.
    Heading,
    /// `fontMedium`.
    Body,
    /// A connection code: `fontMedium`, never wrapped.
    Code,
    /// A Steam room code to read out: `fontLarge`, never wrapped.
    RoomCode,
    /// `fontSmall` at 0.65 alpha.
    Small,
    /// Half a line of space.
    Gap,
}

/// One line of the right-hand column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColLine {
    pub kind: LineKind,
    pub text: String,
    pub tone: Tone,
    /// Frames since a message appeared (for [`Tone::Warn`]'s fade).
    pub age: u64,
}

impl ColLine {
    #[must_use]
    pub fn heading(text: &str) -> Self {
        Self::of(LineKind::Heading, text, Tone::Good)
    }
    #[must_use]
    pub fn body(text: &str, tone: Tone) -> Self {
        Self::of(LineKind::Body, text, tone)
    }
    #[must_use]
    pub fn code(text: &str) -> Self {
        Self::of(LineKind::Code, text, Tone::Info)
    }
    #[must_use]
    pub fn room_code(text: &str) -> Self {
        Self::of(LineKind::RoomCode, text, Tone::Info)
    }
    #[must_use]
    pub fn small(text: &str, tone: Tone) -> Self {
        Self::of(LineKind::Small, text, tone)
    }
    #[must_use]
    pub fn gap() -> Self {
        Self::of(LineKind::Gap, "", Tone::Info)
    }
    fn of(kind: LineKind, text: &str, tone: Tone) -> Self {
        Self {
            kind,
            text: text.to_owned(),
            tone,
            age: 0,
        }
    }
    /// The colour to draw it in.
    #[must_use]
    pub fn color(&self) -> u32 {
        self.tone.color(self.age)
    }
}

/// Which page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageId {
    Main,
    Lan,
}

/// A row's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowId {
    /// "Host Game", or "Leave Lobby" while in a lobby.
    HostGame,
    /// "Try Again" (only while the connector offers a retry).
    Retry,
    InviteFriend,
    JoinFriend,
    RoomCode,
    /// "Who Can Join": Friends, Invite Only, Room Code.
    WhoCanJoin,
    ShipColour,
    PartnerLabels,
    /// "Wild Mods": Sync, Force On, Force Off (this player's own, `crate::wild`).
    WildMods,
    CodesAndLan,
    Back,
    CopyCode,
    PasteCode,
    PartnerCode,
    Address,
    Role,
    Connect,
    Name,
}

/// The Co-op page's rows for this Steam state: at most 11, menu rows 1..=11.
/// The game draws rows 0..=10 and the mod draws row 11, as the plain menu's
/// Exit.
#[must_use]
pub fn main_rows(steam: &SteamView) -> Vec<RowId> {
    let mut rows = vec![RowId::HostGame];
    if steam.offers.retry {
        rows.push(RowId::Retry);
    }
    rows.extend([
        RowId::InviteFriend,
        RowId::JoinFriend,
        RowId::RoomCode,
        RowId::WhoCanJoin,
        RowId::ShipColour,
        RowId::PartnerLabels,
        RowId::WildMods,
        RowId::CodesAndLan,
        RowId::Back,
    ]);
    rows
}

const LAN_ROWS: [RowId; 8] = [
    RowId::CopyCode,
    RowId::PasteCode,
    RowId::PartnerCode,
    RowId::Address,
    RowId::Role,
    RowId::Connect,
    RowId::Name,
    RowId::Back,
];

impl PageId {
    /// The rows, top to bottom (menu rows 1..=N).
    #[must_use]
    pub fn rows(self, steam: &SteamView) -> Vec<RowId> {
        match self {
            Self::Main => main_rows(steam),
            Self::Lan => LAN_ROWS.to_vec(),
        }
    }
}

/// A row as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub id: RowId,
    pub label: String,
    /// The value column (blue), for rows that have one.
    pub value: Option<String>,
    /// Drawn at 0.3 alpha when false (still selectable; confirm explains).
    pub enabled: bool,
}

/// Partner name labels and edge markers (`show_names`, `show_marker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelMode {
    #[default]
    Off,
    Name,
    Marker,
    Both,
}

impl LabelMode {
    /// From the two config switches.
    #[must_use]
    pub fn from_flags(names: bool, marker: bool) -> Self {
        match (names, marker) {
            (false, false) => Self::Off,
            (true, false) => Self::Name,
            (false, true) => Self::Marker,
            (true, true) => Self::Both,
        }
    }
    #[must_use]
    pub fn names(self) -> bool {
        matches!(self, Self::Name | Self::Both)
    }
    #[must_use]
    pub fn marker(self) -> bool {
        matches!(self, Self::Marker | Self::Both)
    }
    fn label(self) -> &'static str {
        match self {
            Self::Off => en::OFF,
            Self::Name => "Name",
            Self::Marker => "Marker",
            Self::Both => "Name and Marker",
        }
    }
    fn step(self, dir: i32) -> Self {
        const ALL: [LabelMode; 4] = [
            LabelMode::Off,
            LabelMode::Name,
            LabelMode::Marker,
            LabelMode::Both,
        ];
        let i = ALL.iter().position(|m| *m == self).unwrap_or(0);
        ALL[cycle(i, ALL.len(), dir)]
    }
}

fn cycle(i: usize, n: usize, dir: i32) -> usize {
    if n == 0 {
        return 0;
    }
    if dir < 0 {
        (i + n - 1) % n
    } else {
        (i + 1) % n
    }
}

/// Something for the glue to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Hand this choice to the lobby agent.
    Lobby(LobbyRequest),
    /// `clipboard_set_text`.
    Copy(String),
    /// Read the clipboard and call [`Page::paste`].
    Paste,
    /// Hand this to the Steam connector, then [`Page::steam_result`].
    Steam(SteamAction),
    /// This player's co-op colour changed (send it, save it).
    SetColor(ColorChoice),
    /// The label switches changed (use them, save them).
    SetLabels(LabelMode),
    /// The Wild Mods row changed (use it, save it).
    SetWild(WildMods),
    /// The page closed: give the menu back.
    Closed,
    /// Switched between the pages (the glue resets the selection).
    Switched(PageId),
}

/// What a yes to the confirmation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    Disconnect,
    LeaveLobby,
}

/// An open question (YES/NO, NO selected first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub question: String,
    pub action: ConfirmAction,
}

/// A text field being typed into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub field: Field,
    pub text: String,
}

/// What the entry screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryView {
    pub prompt: String,
    /// `"[ text ]"`.
    pub boxed: String,
    /// Live validation, if the field has any.
    pub check: Option<(String, Tone)>,
    pub hint: String,
}

/// Everything the glue needs to draw the page this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageView {
    pub page: PageId,
    pub title: String,
    pub rows: Vec<RowView>,
    pub column: Vec<ColLine>,
    pub entry: Option<EntryView>,
    pub confirm: Option<String>,
}

/// The co-op pages' state.
#[derive(Debug, Clone)]
pub struct Page {
    /// The page shown, if any.
    pub open: Option<PageId>,
    /// The Codes and LAN form.
    pub lan: LanForm,
    entry: Option<Entry>,
    confirm: Option<Confirm>,
    colors: Vec<ColorChoice>,
    color: usize,
    labels: LabelMode,
    /// The Wild Mods row (`wild_mods`).
    wild: WildMods,
    message: Option<(String, Tone, u64)>,
    tick: u64,
    lang: String,
    /// The host's run settings in words, on a joining game (read only).
    host_run: Option<String>,
}

impl Page {
    /// A closed page with the configured name, colour choices and labels.
    #[must_use]
    pub fn new(name: &str, colors: (Vec<ColorChoice>, usize), labels: LabelMode) -> Self {
        Self {
            open: None,
            lan: LanForm::new(name),
            entry: None,
            confirm: None,
            color: colors.1.min(colors.0.len().saturating_sub(1)),
            colors: colors.0,
            labels,
            wild: WildMods::default(),
            message: None,
            tick: 0,
            lang: String::new(),
            host_run: None,
        }
    }

    /// Once per menu frame. `language`: `global.language`.
    pub fn refresh(&mut self, status: Option<&Status>, language: &str) {
        self.tick += 1;
        self.lan.refresh(status);
        if self.lang != language {
            language.clone_into(&mut self.lang);
        }
    }

    /// The host's run settings in words for the main page, or none.
    pub fn set_host_run(&mut self, line: Option<String>) {
        self.host_run = line;
    }

    fn say(&mut self, text: impl Into<String>, tone: Tone) {
        self.message = Some((text.into(), tone, self.tick));
    }

    /// The page's own message (the Codes page uses the form's).
    fn message_line(&self) -> Option<ColLine> {
        let (text, tone, age) = if let Some(PageId::Lan) = self.open {
            self.lan.message()?
        } else {
            let (t, tone, at) = self.message.as_ref()?;
            let age = self.tick.saturating_sub(*at);
            if age >= MESSAGE_FRAMES {
                return None;
            }
            (t.as_str(), *tone, age)
        };
        let mut l = ColLine::body(text, tone);
        l.age = age;
        Some(l)
    }

    /// Show a page (from the Co-op row).
    pub fn show(&mut self, page: PageId) {
        self.open = Some(page);
        self.entry = None;
        self.confirm = None;
    }

    /// Hide everything.
    pub fn close(&mut self) {
        self.open = None;
        self.entry = None;
        self.confirm = None;
    }

    /// The labels mode in effect.
    #[must_use]
    pub fn labels(&self) -> LabelMode {
        self.labels
    }

    /// The Wild Mods row as configured (`wild_mods`).
    pub fn set_wild(&mut self, wild: WildMods) {
        self.wild = wild;
    }

    /// The Wild Mods row's value.
    #[must_use]
    pub fn wild(&self) -> WildMods {
        self.wild
    }

    /// The colour chosen.
    #[must_use]
    pub fn color(&self) -> Option<&ColorChoice> {
        self.colors.get(self.color)
    }

    /// The text entry, if one is open.
    #[must_use]
    pub fn entry(&self) -> Option<&Entry> {
        self.entry.as_ref()
    }

    /// The open question, if any.
    #[must_use]
    pub fn confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    /// The rows of the open page.
    #[must_use]
    pub fn rows(&self, steam: &SteamView, status: Option<&Status>) -> Vec<RowView> {
        let Some(page) = self.open else {
            return Vec::new();
        };
        page.rows(steam)
            .into_iter()
            .map(|id| self.row(id, steam, status))
            .collect()
    }

    /// The row ids of the open page (empty when closed).
    #[must_use]
    pub fn row_ids(&self, steam: &SteamView) -> Vec<RowId> {
        self.open.map_or_default(|p| p.rows(steam))
    }

    fn row(&self, id: RowId, steam: &SteamView, status: Option<&Status>) -> RowView {
        let field_value = |s: &str| {
            Some(if s.trim().is_empty() {
                "None".to_owned()
            } else {
                text::truncate(s.trim(), 24)
            })
        };
        let (label, value, enabled) = match id {
            RowId::HostGame if steam.in_lobby() => ("Leave Lobby", None, steam.offers.leave),
            RowId::HostGame => ("Host Game", None, steam.offers.host),
            RowId::Retry => ("Try Again", None, steam.offers.retry),
            RowId::InviteFriend => ("Invite Friend", None, steam.offers.invite),
            RowId::JoinFriend => ("Join Friend", None, steam.available() && !steam.in_lobby()),
            RowId::RoomCode => ("Join by Room Code...", None, steam.offers.join_code),
            RowId::WhoCanJoin => (
                "Who Can Join",
                Some(steam.visibility.label().to_owned()),
                steam.offers.visibility,
            ),
            RowId::ShipColour => ("Label Colour", self.color().map(|c| c.label.clone()), true),
            RowId::PartnerLabels => ("Partner Labels", Some(self.labels.label().to_owned()), true),
            RowId::WildMods => ("Wild Mods", Some(self.wild.label().to_owned()), true),
            RowId::CodesAndLan => ("Codes and LAN", None, true),
            RowId::Back => (en::BACK, None, true),
            RowId::CopyCode => (
                "Copy My Code",
                None,
                status.is_some_and(|s| s.code.is_some() || s.lan_code.is_some()),
            ),
            RowId::PasteCode => ("Paste Partner Code", None, true),
            RowId::PartnerCode => ("Partner Code...", field_value(&self.lan.code), true),
            RowId::Address => ("Host Address...", field_value(&self.lan.direct), true),
            RowId::Role => (
                "Role",
                Some(
                    match self.lan.shown_role(status) {
                        RoleChoice::Join => "Join",
                        RoleChoice::Host | RoleChoice::None => "Host",
                    }
                    .to_owned(),
                ),
                true,
            ),
            RowId::Connect if lan_active(status) => ("Disconnect", None, true),
            RowId::Connect => ("Connect", None, true),
            RowId::Name => (
                "Name...",
                Some(text::truncate(self.lan.shown_name(), 24)),
                true,
            ),
        };
        RowView {
            id,
            label: label.to_owned(),
            value,
            enabled,
        }
    }

    fn row_at(&self, index: usize, steam: &SteamView) -> Option<RowId> {
        self.open?.rows(steam).get(index).copied()
    }

    fn steam_unavailable(&mut self, steam: &SteamView) -> bool {
        let SteamState::Unavailable { reason } = &steam.state else {
            return false;
        };
        let text = if reason.contains("Codes and LAN") || reason.contains("not available yet") {
            reason.clone()
        } else {
            format!("{reason} Use Codes and LAN to play together.")
        };
        self.say(text, Tone::Warn);
        true
    }

    /// The partner's name for a sentence.
    fn partner_name(&self, steam: &SteamView) -> String {
        steam.partner.as_deref().map_or_else(
            || "your partner".into(),
            |p| text::display_name(p, &self.lang, text::FALLBACK_PARTNER),
        )
    }

    fn lan_effects(effects: Vec<LanEffect>) -> Vec<Effect> {
        effects
            .into_iter()
            .map(|e| match e {
                LanEffect::Request(r) => Effect::Lobby(r),
                LanEffect::Copy(c) => Effect::Copy(c),
            })
            .collect()
    }

    fn start_entry(&mut self, field: Field) {
        let text = match field {
            Field::RoomCode => String::new(),
            f => self.lan.field(f).to_owned(),
        };
        self.entry = Some(Entry { field, text });
    }

    /// Confirm on row `index` (0 = menu row 1).
    pub fn confirm_row(
        &mut self,
        index: usize,
        steam: &SteamView,
        status: Option<&Status>,
    ) -> Vec<Effect> {
        if self.entry.is_some() || self.confirm.is_some() {
            return Vec::new();
        }
        let Some(id) = self.row_at(index, steam) else {
            return Vec::new();
        };
        match id {
            RowId::HostGame
            | RowId::Retry
            | RowId::InviteFriend
            | RowId::JoinFriend
            | RowId::RoomCode => self.confirm_steam(id, steam),
            RowId::WhoCanJoin
            | RowId::ShipColour
            | RowId::PartnerLabels
            | RowId::WildMods
            | RowId::Role => self.shift_row(index, 1, steam, status),
            RowId::CodesAndLan => {
                self.show(PageId::Lan);
                vec![Effect::Switched(PageId::Lan)]
            }
            RowId::Back => self.back(),
            RowId::CopyCode => Self::lan_effects(self.lan.copy_code(status)),
            RowId::PasteCode => vec![Effect::Paste],
            RowId::PartnerCode => {
                self.start_entry(Field::Code);
                Vec::new()
            }
            RowId::Address => {
                self.start_entry(Field::Direct);
                Vec::new()
            }
            RowId::Name => {
                self.start_entry(Field::Name);
                Vec::new()
            }
            RowId::Connect => match status.map(|s| s.state) {
                Some(LobbyState::Connected) => {
                    let who = status.and_then(|s| s.connected_peers().next()).map_or_else(
                        || "your partner".into(),
                        |p| text::display_name(&p.name, &self.lang, "your partner"),
                    );
                    self.confirm = Some(Confirm {
                        question: format!("Disconnect from {who}?"),
                        action: ConfirmAction::Disconnect,
                    });
                    Vec::new()
                }
                Some(LobbyState::Connecting | LobbyState::Waiting) => {
                    Self::lan_effects(self.lan.disconnect())
                }
                _ => Self::lan_effects(self.lan.connect(status)),
            },
        }
    }

    /// Confirm on one of the Steam rows.
    fn confirm_steam(&mut self, id: RowId, steam: &SteamView) -> Vec<Effect> {
        match id {
            RowId::HostGame => {
                if self.steam_unavailable(steam) {
                    return Vec::new();
                }
                if steam.in_lobby() {
                    if steam.with_partner() {
                        self.confirm = Some(Confirm {
                            question: if steam.reconnecting() {
                                "Stop reconnecting and leave?".into()
                            } else {
                                "Leave the lobby?".into()
                            },
                            action: ConfirmAction::LeaveLobby,
                        });
                        return Vec::new();
                    }
                    return vec![Effect::Steam(SteamAction::Leave)];
                }
                if steam.offers.host {
                    vec![Effect::Steam(SteamAction::Host)]
                } else {
                    self.say("Steam is busy. Try again in a moment.", Tone::Warn);
                    Vec::new()
                }
            }
            RowId::Retry => {
                if steam.offers.retry {
                    vec![Effect::Steam(SteamAction::Retry)]
                } else {
                    Vec::new()
                }
            }
            RowId::InviteFriend => {
                if self.steam_unavailable(steam) {
                    return Vec::new();
                }
                if steam.offers.invite {
                    vec![Effect::Steam(SteamAction::OpenInvite)]
                } else if steam.in_lobby() {
                    self.say(
                        "The Steam overlay is off, so invites can't be sent. Set Who Can Join to \
                         Room Code instead.",
                        Tone::Warn,
                    );
                    Vec::new()
                } else {
                    self.say("Host a game first, then invite a friend.", Tone::Warn);
                    Vec::new()
                }
            }
            RowId::JoinFriend => {
                if self.steam_unavailable(steam) {
                    return Vec::new();
                }
                if steam.in_lobby() {
                    self.say("Leave the lobby first to join a friend.", Tone::Warn);
                } else if steam.invites_arrive {
                    self.say(
                        "Accept a friend's invite in Steam, or choose Join Game on their Steam \
                         profile.",
                        Tone::Info,
                    );
                } else {
                    self.say(
                        "Invites can't reach a running game here: join by room code, or start \
                         the game from the invite.",
                        Tone::Warn,
                    );
                }
                Vec::new()
            }
            RowId::RoomCode => {
                if self.steam_unavailable(steam) {
                    return Vec::new();
                }
                if steam.offers.join_code {
                    self.start_entry(Field::RoomCode);
                } else {
                    self.say(
                        "Leave the lobby first to join with a room code.",
                        Tone::Warn,
                    );
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Left (`dir < 0`) or right on row `index`.
    pub fn shift_row(
        &mut self,
        index: usize,
        dir: i32,
        steam: &SteamView,
        status: Option<&Status>,
    ) -> Vec<Effect> {
        if self.entry.is_some() || self.confirm.is_some() {
            return Vec::new();
        }
        match self.row_at(index, steam) {
            Some(RowId::ShipColour) => {
                self.color = cycle(self.color, self.colors.len(), dir);
                self.color()
                    .cloned()
                    .map(Effect::SetColor)
                    .into_iter()
                    .collect()
            }
            Some(RowId::PartnerLabels) => {
                self.labels = self.labels.step(dir);
                vec![Effect::SetLabels(self.labels)]
            }
            Some(RowId::WildMods) => {
                self.wild = self.wild.step(dir);
                self.say(self.wild.explain(), Tone::Info);
                vec![Effect::SetWild(self.wild)]
            }
            Some(RowId::WhoCanJoin) => {
                if self.steam_unavailable(steam) {
                    return Vec::new();
                }
                if steam.offers.visibility {
                    let next = steam.visibility.step(dir);
                    self.say(next.explain(), Tone::Info);
                    vec![Effect::Steam(SteamAction::SetVisibility(next))]
                } else {
                    self.say(
                        "Who can join is set until someone joins. Leave the lobby to change it.",
                        Tone::Warn,
                    );
                    Vec::new()
                }
            }
            Some(RowId::Role) => {
                let next = match self.lan.shown_role(status) {
                    RoleChoice::Join => RoleChoice::Host,
                    RoleChoice::Host | RoleChoice::None => RoleChoice::Join,
                };
                self.lan.set_role(next);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Whether row `index` reacts to left/right (for the move sound).
    #[must_use]
    pub fn row_shifts(&self, index: usize, steam: &SteamView) -> bool {
        matches!(
            self.row_at(index, steam),
            Some(
                RowId::ShipColour
                    | RowId::PartnerLabels
                    | RowId::WildMods
                    | RowId::WhoCanJoin
                    | RowId::Role
            )
        )
    }

    /// Back: closes the entry or question, then the Codes page, then the page.
    pub fn back(&mut self) -> Vec<Effect> {
        if self.entry.take().is_some() || self.confirm.take().is_some() {
            return Vec::new();
        }
        match self.open {
            Some(PageId::Lan) => {
                self.show(PageId::Main);
                vec![Effect::Switched(PageId::Main)]
            }
            Some(PageId::Main) => {
                self.close();
                vec![Effect::Closed]
            }
            None => Vec::new(),
        }
    }

    /// The answer to the open question.
    pub fn answer(&mut self, yes: bool) -> Vec<Effect> {
        let Some(c) = self.confirm.take() else {
            return Vec::new();
        };
        if !yes {
            return Vec::new();
        }
        match c.action {
            ConfirmAction::Disconnect => Self::lan_effects(self.lan.disconnect()),
            ConfirmAction::LeaveLobby => vec![Effect::Steam(SteamAction::Leave)],
        }
    }

    /// Typed text for the entry.
    pub fn entry_text(&mut self, typed: &str) {
        if let Some(e) = &mut self.entry {
            push_limited(&mut e.text, typed, e.field.max());
        }
    }

    pub fn entry_backspace(&mut self) {
        if let Some(e) = &mut self.entry {
            e.text.pop();
        }
    }

    pub fn entry_clear(&mut self) {
        if let Some(e) = &mut self.entry {
            e.text.clear();
        }
    }

    /// Esc in the entry: drop what was typed.
    pub fn entry_cancel(&mut self) {
        self.entry = None;
    }

    /// Enter in the entry: keep it.
    pub fn entry_done(&mut self) -> Vec<Effect> {
        let Some(e) = self.entry.take() else {
            return Vec::new();
        };
        match e.field {
            Field::RoomCode => {
                let code = e.text.trim().to_ascii_uppercase();
                if code.is_empty() {
                    Vec::new()
                } else {
                    self.say(format!("Looking for room {code}..."), Tone::Info);
                    vec![Effect::Steam(SteamAction::JoinByCode(code))]
                }
            }
            f => {
                self.lan.set_field(f, &e.text);
                Vec::new()
            }
        }
    }

    /// Clipboard text after [`Effect::Paste`] (or Ctrl+V in an entry).
    pub fn paste(&mut self, clip: Option<&str>, status: Option<&Status>) {
        if let Some(e) = &mut self.entry {
            if let Some(t) = clip {
                push_limited(&mut e.text, t.trim(), e.field.max());
            }
            return;
        }
        self.lan.paste(clip, status);
    }

    /// What the Steam connector said to an action.
    pub fn steam_result(&mut self, r: Result<(), String>) {
        if let Err(e) = r {
            self.say(e, Tone::Warn);
        }
    }

    fn entry_view(&self, status: Option<&Status>) -> Option<EntryView> {
        let e = self.entry.as_ref()?;
        let prompt = match e.field {
            Field::Code => "Enter your partner's code",
            Field::Direct => "Enter the host's address (ip:port)",
            Field::Name => "Enter your name",
            Field::RoomCode => "Enter the room code",
        };
        let check = match e.field {
            Field::Code => match check_code(&e.text, status) {
                CodeCheck::Empty => None,
                CodeCheck::Typing(n) => Some((
                    format!("{n}/{} characters", super::lan::CODE_CHARS),
                    Tone::Info,
                )),
                CodeCheck::Valid(c) => Some((format!("Valid: partner at {}", c.addr), Tone::Good)),
                CodeCheck::Own => Some(("That is your own code.".into(), Tone::Warn)),
                CodeCheck::Invalid(err) => Some((err, Tone::Bad)),
            },
            Field::Direct => match check_direct(&e.text) {
                AddrCheck::Empty => None,
                AddrCheck::Valid(a) => Some((format!("Host at {a}"), Tone::Good)),
                AddrCheck::Invalid(err) => Some((err, Tone::Bad)),
            },
            Field::Name | Field::RoomCode => None,
        };
        Some(EntryView {
            prompt: prompt.into(),
            boxed: format!("[ {}_ ]", e.text),
            check,
            hint: en::ENTRY_HINT.into(),
        })
    }

    /// Everything to draw this frame.
    #[must_use]
    pub fn view(&self, steam: &SteamView, status: Option<&Status>) -> Option<PageView> {
        let page = self.open?;
        let mut column = match page {
            PageId::Main => self.steam_column(steam, status),
            PageId::Lan => self.lan.column(status),
        };
        if let (PageId::Main, Some(line)) = (page, &self.host_run) {
            column.push(ColLine::gap());
            column.push(ColLine::small(line, Tone::Info));
        }
        if let Some(m) = self.message_line() {
            column.push(ColLine::gap());
            column.push(m);
        }
        Some(PageView {
            page,
            title: match page {
                PageId::Main => en::COOP_TITLE,
                PageId::Lan => en::LAN_TITLE,
            }
            .into(),
            rows: self.rows(steam, status),
            column,
            entry: self.entry_view(status),
            confirm: self.confirm.as_ref().map(|c| c.question.clone()),
        })
    }

    /// The heading and what the Steam state means, in words.
    fn steam_state_lines(&self, steam: &SteamView) -> Vec<ColLine> {
        let mut out = vec![ColLine::heading("STEAM")];
        let small = |t: &str| ColLine::small(t, Tone::Info);
        let partner = self.partner_name(steam);
        let try_again = "Choose Try Again, or Host Game.";
        match &steam.state {
            SteamState::Unavailable { reason } => {
                out.push(ColLine::body(reason, Tone::Info));
                out.push(small(
                    "Codes and LAN works over the internet and on a local network.",
                ));
            }
            SteamState::Idle => {
                out.push(ColLine::body(
                    "Host a game, then invite a friend from the Steam overlay.",
                    Tone::Info,
                ));
                out.push(small(steam.visibility.explain()));
            }
            SteamState::Creating => out.push(ColLine::body("Opening a lobby...", Tone::Info)),
            SteamState::Hosting => {
                out.push(ColLine::body(
                    if steam.offers.invite {
                        "Lobby open. Invite a friend from the Steam overlay."
                    } else {
                        "Lobby open."
                    },
                    Tone::Info,
                ));
                out.push(small(steam.visibility.explain()));
            }
            SteamState::Joining => out.push(ColLine::body("Joining the lobby...", Tone::Info)),
            SteamState::InLobby => out.push(ColLine::body(
                "In the lobby. Checking versions with the host...",
                Tone::Info,
            )),
            SteamState::Connecting => out.push(ColLine::body(
                &format!("Connecting to {partner}..."),
                Tone::Info,
            )),
            SteamState::Linked => out.push(ColLine::body(
                &format!("Linked with {partner}. Start a run: you wait for each other."),
                Tone::Good,
            )),
            SteamState::InRun => {
                out.push(ColLine::body(
                    &format!("Playing with {partner}."),
                    Tone::Good,
                ));
            }
            SteamState::PeerLost { lost_for_s } => {
                let ago = lost_for_s.map_or_default(|s| format!(" {s} s ago"));
                out.push(ColLine::body(
                    &super::text::lost_link(&partner, &ago),
                    Tone::Bad,
                ));
                out.push(small(
                    "Steam keeps trying to reconnect. Leave Lobby stops it.",
                ));
            }
            SteamState::Refused { reason } => {
                out.push(ColLine::body(reason, Tone::Bad));
                out.push(small(try_again));
            }
            SteamState::Error { message } => {
                out.push(ColLine::body(message, Tone::Bad));
                out.push(small(try_again));
            }
            SteamState::Left => {
                out.push(ColLine::body(
                    &super::text::partner_left(&partner),
                    Tone::Info,
                ));
                out.push(small(try_again));
            }
        }
        out
    }

    fn steam_column(&self, steam: &SteamView, status: Option<&Status>) -> Vec<ColLine> {
        let mut out = self.steam_state_lines(steam);
        let small = |t: &str| ColLine::small(t, Tone::Info);
        if let Some(code) = steam.room_code.as_deref().filter(|c| !c.is_empty()) {
            out.push(ColLine::gap());
            out.push(ColLine::heading("ROOM CODE"));
            out.push(ColLine::room_code(code));
        }
        if steam.in_lobby() && !steam.members.is_empty() {
            out.push(ColLine::gap());
            // The session's size, not the Steam lobby's (`steam::settings::LOBBY_SIZE`).
            // A refused third member can sit in the Steam lobby for a moment.
            out.push(ColLine::heading(&format!(
                "PLAYERS {}/{SESSION_PLAYERS}",
                steam.members.len().min(SESSION_PLAYERS),
            )));
            for m in &steam.members {
                out.push(self.member_line(m, steam));
            }
        }
        if let Some(n) = &steam.notice {
            out.push(ColLine::small(n, Tone::Info));
        }
        if steam.available() && !steam.invites_arrive && !steam.in_lobby() {
            out.push(small(
                "Invites can't reach a running game here: join by room code.",
            ));
        }
        // A link made on the Codes page shows here too (not a Steam session).
        if let Some(s) = status.filter(|s| lan_active(Some(s)) && s.connector != Connector::Steam) {
            out.push(ColLine::gap());
            let mut lines = link_lines(s);
            if let Some(h) = lines.first_mut() {
                h.text = "CODES AND LAN".into();
            }
            out.extend(lines.into_iter().take(3));
        }
        out
    }

    /// One member of the Steam lobby: name, "(host)", "- you", round trip or state.
    fn member_line(&self, m: &super::steam_link::SteamMember, steam: &SteamView) -> ColLine {
        let fallback = if m.me { "You" } else { text::FALLBACK_PARTNER };
        let mut t = text::display_name(&m.name, &self.lang, fallback);
        if m.host {
            t.push_str(" (host)");
        }
        if m.me {
            t.push_str(" - you");
            return ColLine::body(&t, Tone::Info);
        }
        match (m.linked, m.rtt_ms) {
            (true, Some(ms)) => {
                let tone = if bad_link(ms, 0.0) {
                    Tone::Bad
                } else {
                    Tone::Info
                };
                ColLine::body(&format!("{t}   {ms:.0} ms"), tone)
            }
            (true, None) => ColLine::body(&t, Tone::Info),
            (false, _) if steam.reconnecting() => {
                ColLine::body(&format!("{t}   reconnecting..."), Tone::Bad)
            }
            (false, _) => ColLine::body(&format!("{t}   connecting..."), Tone::Info),
        }
    }
}

/// The lobby is linked or trying to link.
#[must_use]
pub fn lan_active(status: Option<&Status>) -> bool {
    status.is_some_and(|s| {
        matches!(
            s.state,
            LobbyState::Connected | LobbyState::Connecting | LobbyState::Waiting
        )
    })
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use crate::ui::lan::tests::{linked, partner, status};
    use crate::ui::steam_link::{NotYet, SteamLink, SteamMember, SteamOffers, Visibility};

    fn page() -> Page {
        let mut p = Page::new(
            "Ace",
            crate::world::color::page_choices("auto"),
            LabelMode::Off,
        );
        p.show(PageId::Main);
        p
    }

    fn steam() -> SteamView {
        NotYet::default().view()
    }

    fn idx(page: PageId, id: RowId) -> usize {
        idx_in(page, &steam(), id)
    }

    fn idx_in(page: PageId, s: &SteamView, id: RowId) -> usize {
        page.rows(s).iter().position(|r| *r == id).unwrap()
    }

    fn col_text(v: &PageView) -> String {
        v.column
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Steam up, in `state`, with the offers the connector makes there.
    fn up(state: SteamState) -> SteamView {
        let idle = matches!(
            state,
            SteamState::Idle
                | SteamState::Error { .. }
                | SteamState::Refused { .. }
                | SteamState::Left
        );
        let hosting = state == SteamState::Hosting;
        let mut v = SteamView {
            state,
            max_members: 4,
            invites_arrive: true,
            ..SteamView::unavailable("")
        };
        let in_lobby = v.in_lobby();
        v.offers = SteamOffers {
            host: idle,
            invite: in_lobby,
            join_code: idle || hosting,
            leave: !idle,
            retry: idle && v.state != SteamState::Idle,
            visibility: idle || hosting,
        };
        v
    }

    fn member(name: &str, me: bool, host: bool, linked: bool, rtt: Option<f64>) -> SteamMember {
        SteamMember {
            name: name.into(),
            me,
            host,
            linked,
            rtt_ms: rtt,
        }
    }

    #[test]
    fn rows_fit_the_menu_and_end_with_back() {
        for s in [steam(), up(SteamState::Idle), up(SteamState::Left)] {
            for p in [PageId::Main, PageId::Lan] {
                let rows = p.rows(&s);
                assert!(rows.len() <= 11, "rows 1..=11 only");
                assert_eq!(*rows.last().unwrap(), RowId::Back);
            }
        }
        assert!(!main_rows(&up(SteamState::Idle)).contains(&RowId::Retry));
        assert_eq!(main_rows(&up(SteamState::Left))[1], RowId::Retry);
    }

    #[test]
    fn wild_mods_row_cycles_this_players_own_three_values() {
        for state in [SteamState::Idle, SteamState::Linked, SteamState::Left] {
            let s = up(state);
            let mut p = page();
            p.set_wild(WildMods::Sync);
            let i = idx_in(PageId::Main, &s, RowId::WildMods);
            assert!(p.row_shifts(i, &s));
            let row = &p.rows(&s, None)[i];
            assert_eq!(row.label, "Wild Mods");
            assert_eq!(row.value.as_deref(), Some("Sync"));
            assert!(row.enabled, "each player's own: the joining player's too");
            assert_eq!(
                p.shift_row(i, 1, &s, None),
                vec![Effect::SetWild(WildMods::On)]
            );
            assert_eq!(p.rows(&s, None)[i].value.as_deref(), Some("Force On"));
            let t = col_text(&p.view(&s, None).unwrap());
            assert!(t.contains("halved"), "says what it means: {t}");
            assert_eq!(
                p.confirm_row(i, &s, None),
                vec![Effect::SetWild(WildMods::Off)],
                "confirm cycles too"
            );
            assert_eq!(
                p.shift_row(i, -1, &s, None),
                vec![Effect::SetWild(WildMods::On)]
            );
            assert_eq!(p.wild(), WildMods::On);
        }
    }

    #[test]
    fn players_heading_shows_the_two_player_session() {
        let p = page();
        let mut h = up(SteamState::Linked);
        h.is_host = true;
        h.members = vec![
            member("Ace", true, true, true, None),
            member("Sam", false, false, true, Some(40.0)),
        ];
        let t = col_text(&p.view(&h, None).unwrap());
        assert!(t.contains("PLAYERS 2/2"), "{t}");
        assert!(
            !t.contains("/4"),
            "the Steam lobby's room is not shown: {t}"
        );
        // A refused third member waiting in the Steam lobby for a moment.
        h.members.push(member("Kim", false, false, false, None));
        let t = col_text(&p.view(&h, None).unwrap());
        assert!(t.contains("PLAYERS 2/2"), "{t}");
    }

    #[test]
    fn steam_rows_are_dim_and_explain_without_a_connector() {
        let mut p = page();
        let s = steam();
        let rows = p.rows(&s, None);
        let host = &rows[idx(PageId::Main, RowId::HostGame)];
        assert!(!host.enabled && host.label == "Host Game");
        assert!(rows[idx(PageId::Main, RowId::ShipColour)].enabled);
        assert!(
            p.confirm_row(idx(PageId::Main, RowId::HostGame), &s, None)
                .is_empty()
        );
        let v = p.view(&s, None).unwrap();
        let t = col_text(&v);
        assert!(t.contains("not available yet"), "{t}");
        let msg = v.column.last().unwrap();
        assert_eq!(msg.tone, Tone::Warn);
        assert_eq!(msg.color(), color::NEGATIVE, "starts in the warning colour");
        for _ in 0..WARN_FADE_FRAMES {
            p.refresh(None, "english");
        }
        assert_eq!(
            p.view(&s, None).unwrap().column.last().unwrap().color(),
            color::WHITE,
            "and fades to white"
        );
        // connector = "udp" says so.
        let mut p = page();
        let u = NotYet::udp().view();
        p.confirm_row(idx(PageId::Main, RowId::RoomCode), &u, None);
        let t = col_text(&p.view(&u, None).unwrap());
        assert!(
            t.contains("connector = udp") && t.contains("Codes and LAN"),
            "{t}"
        );
        // Steam not running (the adapter's text) is not repeated.
        let mut p = page();
        let n = SteamView::unavailable(crate::ui::steam_coop::UNAVAILABLE);
        p.confirm_row(idx(PageId::Main, RowId::HostGame), &n, None);
        let t = col_text(&p.view(&n, None).unwrap());
        assert_eq!(t.matches("Use Codes and LAN").count(), 2, "{t}");
    }

    #[test]
    fn steam_actions_when_available() {
        let mut p = page();
        let idle = up(SteamState::Idle);
        assert_eq!(
            p.confirm_row(idx_in(PageId::Main, &idle, RowId::HostGame), &idle, None),
            vec![Effect::Steam(SteamAction::Host)]
        );
        // Who can join cycles through the connector, before or while hosting.
        let w = idx_in(PageId::Main, &idle, RowId::WhoCanJoin);
        assert!(p.row_shifts(w, &idle));
        assert_eq!(
            p.shift_row(w, 1, &idle, None),
            vec![Effect::Steam(SteamAction::SetVisibility(
                Visibility::InviteOnly
            ))]
        );
        assert_eq!(
            p.shift_row(w, -1, &idle, None),
            vec![Effect::Steam(SteamAction::SetVisibility(
                Visibility::RoomCode
            ))]
        );
        assert_eq!(
            p.rows(&idle, None)[w].value.as_deref(),
            Some("Friends"),
            "the value is the connector's"
        );
        let mut h = up(SteamState::Hosting);
        h.is_host = true;
        h.visibility = Visibility::RoomCode;
        h.room_code = Some("K7QX2".into());
        h.members = vec![member("Ace", true, true, false, None)];
        let rows = p.rows(&h, None);
        assert_eq!(rows[0].label, "Leave Lobby");
        assert_eq!(
            rows[idx_in(PageId::Main, &h, RowId::WhoCanJoin)]
                .value
                .as_deref(),
            Some("Room Code")
        );
        assert!(rows[idx_in(PageId::Main, &h, RowId::InviteFriend)].enabled);
        assert_eq!(
            p.confirm_row(idx_in(PageId::Main, &h, RowId::InviteFriend), &h, None),
            vec![Effect::Steam(SteamAction::OpenInvite)]
        );
        let t = col_text(&p.view(&h, None).unwrap());
        assert!(
            t.contains("ROOM CODE\nK7QX2")
                && t.contains("PLAYERS 1/2")
                && t.contains("Ace (host) - you"),
            "{t}"
        );
        // Hosting with nobody yet: leaving needs no question.
        assert_eq!(
            p.confirm_row(0, &h, None),
            vec![Effect::Steam(SteamAction::Leave)]
        );
        // Overlay off: no invite, and the page says what to do instead.
        h.offers.invite = false;
        assert!(
            p.confirm_row(idx_in(PageId::Main, &h, RowId::InviteFriend), &h, None)
                .is_empty()
        );
        assert!(col_text(&p.view(&h, None).unwrap()).contains("overlay is off"));
        // Linked: leaving asks first; who can join is fixed.
        let mut l = up(SteamState::Linked);
        l.is_host = true;
        l.partner = Some("Sam".into());
        l.rtt_ms = Some(42.0);
        l.members = vec![
            member("Ace", true, true, false, None),
            member("Sam", false, false, true, Some(42.0)),
        ];
        let t = col_text(&p.view(&l, None).unwrap());
        assert!(
            t.contains("Linked with Sam") && t.contains("Sam   42 ms"),
            "{t}"
        );
        assert!(
            p.shift_row(idx_in(PageId::Main, &l, RowId::WhoCanJoin), 1, &l, None)
                .is_empty()
        );
        assert!(p.confirm_row(0, &l, None).is_empty(), "a question first");
        assert_eq!(p.confirm().unwrap().question, "Leave the lobby?");
        assert_eq!(p.answer(true), vec![Effect::Steam(SteamAction::Leave)]);
        // Room code entry goes to the connector.
        p.confirm_row(idx_in(PageId::Main, &idle, RowId::RoomCode), &idle, None);
        assert_eq!(p.entry().unwrap().field, Field::RoomCode);
        p.entry_text("k7qx2");
        assert_eq!(
            p.entry_done(),
            vec![Effect::Steam(SteamAction::JoinByCode("K7QX2".into()))]
        );
        p.steam_result(Err("No room with that code.".into()));
        assert!(col_text(&p.view(&idle, None).unwrap()).contains("No room with that code."));
    }

    #[test]
    fn reconnecting_and_failures_read_truthfully() {
        let mut p = page();
        let mut lost = up(SteamState::PeerLost {
            lost_for_s: Some(12),
        });
        lost.partner = Some("Sam".into());
        lost.members = vec![
            member("Sam", false, true, false, None),
            member("Ace", true, false, false, None),
        ];
        let t = col_text(&p.view(&lost, None).unwrap());
        assert!(t.contains("Lost the link to Sam 12 s ago."), "{t}");
        assert!(t.contains("Steam keeps trying to reconnect"), "{t}");
        assert!(t.contains("Sam (host)   reconnecting..."), "{t}");
        assert!(p.confirm_row(0, &lost, None).is_empty());
        assert_eq!(
            p.confirm().unwrap().question,
            "Stop reconnecting and leave?"
        );
        let _ = p.answer(false);
        // Error and refusal: the sentence, then Try Again (row 2).
        let err = up(SteamState::Error {
            message: "Steam could not open a lobby.".into(),
        });
        let rows = p.rows(&err, None);
        assert_eq!(rows[1].label, "Try Again");
        assert_eq!(rows[0].label, "Host Game");
        assert_eq!(
            p.confirm_row(1, &err, None),
            vec![Effect::Steam(SteamAction::Retry)]
        );
        let t = col_text(&p.view(&err, None).unwrap());
        assert!(
            t.contains("could not open a lobby") && t.contains("Try Again"),
            "{t}"
        );
        let mut left = up(SteamState::Left);
        left.partner = Some("Sam".into());
        assert!(col_text(&p.view(&left, None).unwrap()).contains("Sam left."));
        // No listener: joining by invite is explained.
        let mut idle = up(SteamState::Idle);
        idle.invites_arrive = false;
        assert!(col_text(&p.view(&idle, None).unwrap()).contains("join by room code"));
        p.confirm_row(idx_in(PageId::Main, &idle, RowId::JoinFriend), &idle, None);
        assert!(col_text(&p.view(&idle, None).unwrap()).contains("start the game from the invite"));
    }

    #[test]
    fn colour_and_labels_cycle_both_ways() {
        let mut p = page();
        let s = steam();
        let c = idx(PageId::Main, RowId::ShipColour);
        let e = p.shift_row(c, 1, &s, None);
        assert!(matches!(&e[0], Effect::SetColor(ch) if ch.label == "Blue"));
        let e = p.shift_row(c, -1, &s, None);
        assert!(matches!(&e[0], Effect::SetColor(ch) if ch.key == "auto"));
        let e = p.shift_row(c, -1, &s, None);
        assert!(
            matches!(&e[0], Effect::SetColor(ch) if ch.key == "purple"),
            "wraps"
        );
        let l = idx(PageId::Main, RowId::PartnerLabels);
        assert_eq!(
            p.confirm_row(l, &s, None),
            vec![Effect::SetLabels(LabelMode::Name)]
        );
        assert_eq!(p.rows(&s, None)[l].value.as_deref(), Some("Name"));
        assert_eq!(
            p.shift_row(l, -1, &s, None),
            vec![Effect::SetLabels(LabelMode::Off)]
        );
        assert!(p.row_shifts(l, &s) && !p.row_shifts(idx(PageId::Main, RowId::Back), &s));
        assert_eq!(LabelMode::from_flags(true, true), LabelMode::Both);
        assert!(LabelMode::Both.names() && LabelMode::Both.marker());
    }

    #[test]
    fn back_walks_out_one_level_at_a_time() {
        let mut p = page();
        let s = steam();
        assert_eq!(
            p.confirm_row(idx(PageId::Main, RowId::CodesAndLan), &s, None),
            vec![Effect::Switched(PageId::Lan)]
        );
        p.confirm_row(idx(PageId::Lan, RowId::Address), &s, None);
        assert!(p.entry().is_some());
        assert!(p.back().is_empty(), "closes the entry");
        assert_eq!(p.back(), vec![Effect::Switched(PageId::Main)]);
        assert_eq!(p.back(), vec![Effect::Closed]);
        assert!(p.open.is_none());
    }

    #[test]
    fn codes_page_drives_the_lobby() {
        let mut p = page();
        p.show(PageId::Lan);
        let s = steam();
        let st = status(LobbyState::Ready);
        let copy = p.confirm_row(idx(PageId::Lan, RowId::CopyCode), &s, Some(&st));
        assert_eq!(copy, vec![Effect::Copy(st.code.clone().unwrap())]);
        assert_eq!(
            p.confirm_row(idx(PageId::Lan, RowId::PasteCode), &s, Some(&st)),
            vec![Effect::Paste]
        );
        p.paste(Some(&partner().encode()), Some(&st));
        assert_eq!(p.lan.code, partner().encode());
        // Role: Host by default, right toggles to Join.
        let role = idx(PageId::Lan, RowId::Role);
        assert_eq!(p.rows(&s, Some(&st))[role].value.as_deref(), Some("Host"));
        p.shift_row(role, 1, &s, Some(&st));
        assert_eq!(p.rows(&s, Some(&st))[role].value.as_deref(), Some("Join"));
        let e = p.confirm_row(idx(PageId::Lan, RowId::Connect), &s, Some(&st));
        assert!(
            matches!(&e[0], Effect::Lobby(r) if r.role == RoleChoice::Join
            && r.partner_code.as_deref() == Some(partner().encode().as_str()))
        );
        // Linked: the row says Disconnect and asks first.
        let l = linked(42.0);
        let connect = idx(PageId::Lan, RowId::Connect);
        assert_eq!(p.rows(&s, Some(&l))[connect].label, "Disconnect");
        assert!(p.confirm_row(connect, &s, Some(&l)).is_empty());
        assert_eq!(p.confirm().unwrap().question, "Disconnect from Wing?");
        assert!(p.answer(false).is_empty() && p.confirm().is_none());
        p.confirm_row(connect, &s, Some(&l));
        assert!(matches!(&p.answer(true)[0], Effect::Lobby(r) if r.disconnect));
        // Waiting: no question.
        let w = status(LobbyState::Waiting);
        assert!(
            matches!(&p.confirm_row(connect, &s, Some(&w))[0], Effect::Lobby(r) if r.disconnect)
        );
    }

    #[test]
    fn text_entry_validates_live_and_commits_on_enter() {
        let mut p = page();
        p.show(PageId::Lan);
        let s = steam();
        let st = status(LobbyState::Ready);
        p.confirm_row(idx(PageId::Lan, RowId::Address), &s, Some(&st));
        p.entry_text("192.168.1.2");
        let v = p.view(&s, Some(&st)).unwrap().entry.unwrap();
        assert_eq!(v.boxed, "[ 192.168.1.2_ ]");
        assert!(v.check.unwrap().0.contains("192.168.1.2:7777"));
        p.entry_backspace();
        p.entry_text("20");
        assert!(p.entry_done().is_empty());
        assert_eq!(p.lan.direct, "192.168.1.20");
        // Esc keeps the old value.
        p.confirm_row(idx(PageId::Lan, RowId::Address), &s, Some(&st));
        p.entry_clear();
        p.entry_text("x");
        p.entry_cancel();
        assert_eq!(p.lan.direct, "192.168.1.20");
        // Ctrl+V while typing appends to the entry.
        p.confirm_row(idx(PageId::Lan, RowId::PartnerCode), &s, Some(&st));
        p.entry_clear();
        p.paste(Some(" 2ZR00-0 "), Some(&st));
        assert_eq!(p.entry().unwrap().text, "2ZR00-0");
        let v = p.view(&s, Some(&st)).unwrap().entry.unwrap();
        assert!(v.check.unwrap().0.contains("/20 characters"));
        // Rows don't act while an entry is open.
        assert!(p.confirm_row(0, &s, Some(&st)).is_empty());
    }

    #[test]
    fn names_from_steam_are_made_drawable() {
        let mut p = page();
        p.refresh(None, "english");
        let mut h = up(SteamState::Connecting);
        h.partner = Some("サム".into());
        h.members = vec![
            member("\u{1F600}", true, true, false, None),
            member("サム", false, false, false, None),
        ];
        let t = col_text(&p.view(&h, None).unwrap());
        assert!(t.contains("Player 2   connecting..."), "{t}");
        assert!(t.contains("You (host) - you"), "{t}");
        assert!(t.contains("Connecting to Player 2..."), "{t}");
        // The CJK fonts draw the name as it is.
        p.refresh(None, "japanese");
        let t = col_text(&p.view(&h, None).unwrap());
        assert!(t.contains("サム   connecting..."), "{t}");
    }

    #[test]
    fn a_steam_session_is_not_shown_as_a_codes_link() {
        let p = page();
        let mut h = up(SteamState::Hosting);
        h.room_code = Some("B2QAJ".into());
        let mut st = status(LobbyState::Waiting);
        st.connector = Connector::Steam;
        let v = p.view(&h, Some(&st)).unwrap();
        let t = col_text(&v);
        assert!(!t.contains("CODES AND LAN"), "{t}");
        let code = v.column.iter().find(|l| l.text == "B2QAJ").unwrap();
        assert_eq!(code.kind, LineKind::RoomCode, "read out: large");
        st.connector = Connector::Udp;
        assert!(col_text(&p.view(&h, Some(&st)).unwrap()).contains("CODES AND LAN"));
    }

    #[test]
    fn page_text_is_ascii() {
        let mut p = page();
        let states = [
            SteamState::Idle,
            SteamState::Creating,
            SteamState::Hosting,
            SteamState::Joining,
            SteamState::InLobby,
            SteamState::Connecting,
            SteamState::Linked,
            SteamState::InRun,
            SteamState::PeerLost {
                lost_for_s: Some(5),
            },
            SteamState::Refused {
                reason: "Full.".into(),
            },
            SteamState::Error {
                message: "Failed.".into(),
            },
            SteamState::Left,
        ];
        let mut views: Vec<SteamView> = states.into_iter().map(up).collect();
        views.push(steam());
        for s in &mut views {
            s.members = vec![member("Ace", true, true, false, None)];
            s.room_code = Some("K7QX2".into());
        }
        for s in &views {
            for page in [PageId::Main, PageId::Lan] {
                p.show(page);
                let v = p.view(s, Some(&linked(10.0))).unwrap();
                for r in &v.rows {
                    assert!(r.label.is_ascii());
                }
                for l in &v.column {
                    assert!(l.text.is_ascii(), "{}", l.text);
                }
            }
        }
    }

    #[test]
    fn the_hosts_run_settings_show_read_only_on_the_main_page() {
        let mut p = page();
        p.show(PageId::Main);
        let l = up(SteamState::Linked);
        assert!(!col_text(&p.view(&l, None).unwrap()).contains("'s run:"));
        p.set_host_run(Some("Sam's run: Danger Zone, Elite Enemy.".into()));
        let t = col_text(&p.view(&l, None).unwrap());
        assert!(t.contains("Sam's run: Danger Zone, Elite Enemy."), "{t}");
        p.show(PageId::Lan);
        assert!(!col_text(&p.view(&l, None).unwrap()).contains("Sam's run"));
        p.show(PageId::Main);
        p.set_host_run(None);
        assert!(!col_text(&p.view(&l, None).unwrap()).contains("Sam's run"));
    }
}
