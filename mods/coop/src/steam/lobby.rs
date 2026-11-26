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

//! The Steam lobby flow as a pure state machine: UI actions, async events,
//! polled lobby getters and the link status go in; Steam calls ([`Cmd`]) and
//! lobby agent requests ([`NetCmd`]) come out. Time is passed in.
//!
//! Host: Creating, then Hosting. Join (invite, `+connect_lobby`, room code):
//! Searching, Joining, Checking the host's lobby data (`KEY_*`; a mismatch is
//! refused before any handshake), then Session. If Steam hands the lobby to the
//! joining player, it is handed back once the host is a member again.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use ndml_net::lobby::{Connector, ErrorKind, LobbyState, Status};
use ndml_net::rng::Rng;

use super::roomcode;
use super::view::{Refusal, SteamAction, SteamError, SteamPage, SteamRole, Visibility};

/// Lobby data key: co-op mod version.
pub const KEY_MOD: &str = "ndml";
/// Lobby data key: game build.
pub const KEY_BUILD: &str = "ver";
/// Lobby data key: `lobby` or `in_run`.
pub const KEY_STATE: &str = "state";
/// Lobby data key: the room code.
pub const KEY_CODE: &str = "code";
/// Lobby data key: [`files_id`] of the co-op DLL, which tells builds with the
/// same protocol number apart. A host without it is not refused for it.
pub const KEY_FILES: &str = "files";

/// A short id of a file's bytes: the top half of FNV-1a 64 as 8 hex digits.
#[must_use]
pub fn files_id(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", h >> 32)
}

/// Longest wait for a lobby to be created.
pub const CREATE_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest wait to be in a lobby after asking to join.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest wait for a room-code search.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait for the host's lobby data after entering.
pub const DATA_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for the network thread to take a session request.
pub const NET_START_TIMEOUT: Duration = Duration::from_secs(5);
/// A lobby id reading 0 this long during a session means this side dropped out.
pub const LOBBY_LOST_GRACE: Duration = Duration::from_secs(3);
/// How often a dropped player re-joins the saved lobby.
pub const REJOIN_EVERY: Duration = Duration::from_secs(10);
/// How long a notice stays.
pub const NOTICE_FOR: Duration = Duration::from_secs(8);

/// `ELobbyType` as the extension takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyKind {
    /// Invite only.
    Private,
    /// Friends see it ("Join Game") and invites work.
    FriendsOnly,
    /// Searchable, not shown to friends.
    Invisible,
}

impl LobbyKind {
    /// The number `steam_lobby_create` / `steam_lobby_set_type` take.
    #[must_use]
    pub fn gml(self) -> f64 {
        match self {
            Self::Private => 0.0,
            Self::FriendsOnly => 1.0,
            Self::Invisible => 3.0,
        }
    }

    /// The lobby type for a visibility.
    #[must_use]
    pub fn of(v: Visibility) -> Self {
        match v {
            Visibility::Friends => Self::FriendsOnly,
            Visibility::InviteOnly => Self::Private,
            Visibility::RoomCode => Self::Invisible,
        }
    }
}

/// A request for the lobby agent (network thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetCmd {
    /// Host; admit these Steam ids.
    Host { allow: Vec<u64> },
    /// Join the host with this Steam id.
    Join { host: u64 },
    /// End the session.
    Leave,
}

/// What the glue must do on the game thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// `steam_lobby_create(kind, max)`.
    Create { kind: LobbyKind, max: u8 },
    /// `steam_lobby_set_type(kind)` (owner only).
    SetType(LobbyKind),
    /// `steam_lobby_set_data(key, value)` (owner only).
    SetData(&'static str, String),
    /// `steam_lobby_join_id(lobby)`.
    Join(u64),
    /// `steam_lobby_leave()`.
    Leave,
    /// `steam_lobby_activate_invite_overlay()`.
    Invite,
    /// A worldwide lobby search for `code`.
    FindCode(String),
    /// `steam_lobby_list_join(0)`: the search's first result.
    JoinFound,
    /// `steam_lobby_set_owner_id(id)`: hand the lobby back to the host.
    SetOwner(u64),
    /// For the lobby agent.
    Net(NetCmd),
}

/// The lobby getters, polled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Poll {
    /// `steam_lobby_get_lobby_id()` (0 = not in a lobby).
    pub lobby: u64,
    /// `steam_lobby_get_owner_id()`.
    pub owner: u64,
    /// Members by `steam_lobby_get_member_id(i)`, when read this time.
    pub members: Option<Vec<u64>>,
}

/// The host's lobby data, read by a joiner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LobbyData {
    pub ndml: String,
    pub ver: String,
    pub state: String,
    /// [`KEY_FILES`] (empty: a host build without it).
    pub files: String,
}

/// A lobby search, polled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListPoll {
    /// `steam_lobby_list_is_loading()`.
    pub loading: bool,
    /// `steam_lobby_list_get_count()`.
    pub count: u32,
}

/// How the game link ended, from the lobby agent's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEnd {
    /// Turned away (`refused` key).
    Refused(String),
    /// The handshake timed out.
    HandshakeTimeout,
    /// The partner said goodbye (or the link went and won't come back).
    Lost(String),
    /// The agent went idle for another reason (a UDP request replaced it).
    Dropped,
}

/// The lobby agent's Steam session, as far as the lobby flow cares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Link {
    /// A Steam session is in progress (waiting, connecting or connected).
    pub running: bool,
    /// A partner is connected.
    pub connected: bool,
    /// A lost partner may come back.
    pub reconnecting: bool,
    /// The last Steam session ended this way.
    pub ended: Option<LinkEnd>,
    /// Round trip to the partner.
    pub rtt_ms: Option<f64>,
}

/// Read the link out of `status.toml`'s data.
#[must_use]
pub fn link_of(s: &Status) -> Link {
    let steam = s.connector == Connector::Steam;
    let running = steam
        && matches!(
            s.state,
            LobbyState::Waiting | LobbyState::Connecting | LobbyState::Connected
        );
    let connected = running && s.peers.iter().any(|p| p.connected);
    let ended = match (steam, s.state) {
        (true, LobbyState::Failed) => Some(match s.error_kind {
            Some(ErrorKind::Refused) => {
                LinkEnd::Refused(s.refused.clone().unwrap_or_else(|| "other".into()))
            }
            Some(ErrorKind::HandshakeTimeout) => LinkEnd::HandshakeTimeout,
            _ => LinkEnd::Lost(s.error.clone().unwrap_or_default()),
        }),
        (true, LobbyState::Ready) => Some(LinkEnd::Dropped),
        _ => None,
    };
    Link {
        running,
        connected,
        reconnecting: running && s.reconnecting,
        ended,
        rtt_ms: s
            .peers
            .iter()
            .find(|p| p.connected && running)
            .map(|p| p.rtt_ms)
            .filter(|r| *r > 0.0),
    }
}

/// Check the host's lobby data against this side's.
pub fn check_data(
    d: &LobbyData,
    mod_version: u32,
    build: &str,
    files: &str,
    rejoining: bool,
) -> Result<(), Refusal> {
    if d.ndml.is_empty() {
        return Err(Refusal::NotCoop);
    }
    if d.ndml.trim() != mod_version.to_string() {
        return Err(Refusal::VersionMismatch {
            what: format!(
                "co-op mod version {} here, {} at the host",
                mod_version,
                d.ndml.trim()
            ),
        });
    }
    if d.ver.trim() != build {
        return Err(Refusal::VersionMismatch {
            what: "game build".into(),
        });
    }
    let theirs = d.files.trim();
    if !theirs.is_empty() && !files.is_empty() && theirs != files {
        return Err(Refusal::VersionMismatch {
            what: format!("co-op mod files {files} here, {theirs} at the host"),
        });
    }
    if d.state.trim() == "in_run" && !rejoining {
        return Err(Refusal::HostInRun);
    }
    Ok(())
}

/// A refusal from the agent's `refused` key.
#[must_use]
pub fn refusal_of(key: &str) -> Refusal {
    match key {
        "full" => Refusal::Full,
        "run_in_progress" => Refusal::HostInRun,
        "version_mismatch" => Refusal::VersionMismatch {
            what: "co-op mod version or game build".into(),
        },
        other => Refusal::Other(format!("Refused by the host ({other}).")),
    }
}

/// How the last attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Refused(Refusal),
    Error(SteamError),
    /// The partner left (their name, if known, is in the view).
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Intent {
    Host,
    Lobby(u64),
    Code(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Creating { since: Instant },
    Hosting,
    Searching { since: Instant, polls: u32 },
    Joining { since: Instant },
    Checking { since: Instant },
    Session { host: u64 },
    Failed,
}

/// What the glue should poll this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Wants {
    /// Poll the lobby getters every this many ticks (0 = not at all).
    pub poll_every: u32,
    /// Read the host's lobby data.
    pub data: bool,
    /// Poll the lobby search.
    pub list: bool,
}

/// What the machine needs to know at construction.
#[derive(Debug, Clone)]
pub struct MachineConfig {
    /// The local co-op mod version (lobby data `ndml`).
    pub mod_version: u32,
    /// The local game build (lobby data `ver`).
    pub build: String,
    /// The local co-op mod's [`files_id`] (lobby data `files`; empty: unknown).
    pub files: String,
    /// Most lobby members.
    pub max_members: u8,
    /// Visibility of a lobby hosted here.
    pub visibility: Visibility,
    /// Seed for room codes.
    pub seed: u64,
}

#[derive(Debug)]
pub struct Machine {
    cfg: MachineConfig,
    rng: Rng,
    phase: Phase,
    me: u64,
    lobby: u64,
    owner: u64,
    members: Vec<u64>,
    saved_lobby: Option<u64>,
    session_host: Option<u64>,
    visibility: Visibility,
    room_code: Option<String>,
    in_run: bool,
    link: Link,
    link_seen: bool,
    net_at: Option<Instant>,
    lost_since: Option<Instant>,
    lobby_gone_since: Option<Instant>,
    last_rejoin: Option<Instant>,
    outcome: Option<Outcome>,
    intent: Option<Intent>,
    notice: Option<(String, Instant)>,
}

impl Machine {
    /// An idle machine.
    #[must_use]
    pub fn new(cfg: MachineConfig) -> Self {
        Self {
            rng: Rng::new(cfg.seed),
            visibility: cfg.visibility,
            cfg,
            phase: Phase::Idle,
            me: 0,
            lobby: 0,
            owner: 0,
            members: Vec::new(),
            saved_lobby: None,
            session_host: None,
            room_code: None,
            in_run: false,
            link: Link::default(),
            link_seen: false,
            net_at: None,
            lost_since: None,
            lobby_gone_since: None,
            last_rejoin: None,
            outcome: None,
            intent: None,
            notice: None,
        }
    }

    /// The local Steam id (known once Steam is up).
    pub fn set_me(&mut self, me: u64) {
        self.me = me;
    }

    /// The local Steam id.
    #[must_use]
    pub fn me(&self) -> u64 {
        self.me
    }

    /// Most lobby members.
    #[must_use]
    pub fn max_members(&self) -> u8 {
        self.cfg.max_members
    }

    /// Show a one-off notice (from the glue: overlay off, ...).
    pub fn tell(&mut self, text: &str, now: Instant) {
        self.say(text, now);
    }

    /// Something is going on (the pump runs every frame while true).
    #[must_use]
    pub fn active(&self) -> bool {
        !matches!(self.phase, Phase::Idle | Phase::Failed) || self.link.running
    }

    /// What to poll.
    #[must_use]
    pub fn wants(&self) -> Wants {
        match self.phase {
            Phase::Idle | Phase::Failed => Wants::default(),
            Phase::Creating { .. } | Phase::Joining { .. } => Wants {
                poll_every: 3,
                ..Wants::default()
            },
            Phase::Searching { .. } => Wants {
                poll_every: 0,
                list: true,
                ..Wants::default()
            },
            Phase::Checking { .. } => Wants {
                poll_every: 3,
                data: true,
                ..Wants::default()
            },
            Phase::Hosting | Phase::Session { .. } => Wants {
                poll_every: 30,
                ..Wants::default()
            },
        }
    }

    fn in_lobby(&self) -> bool {
        matches!(
            self.phase,
            Phase::Hosting | Phase::Checking { .. } | Phase::Session { .. }
        )
    }

    fn busy_with_partner(&self) -> bool {
        self.link.connected || self.link.reconnecting
    }

    fn say(&mut self, text: impl Into<String>, now: Instant) {
        self.notice = Some((text.into(), now));
    }

    fn fail(&mut self, outcome: Outcome, leave_lobby: bool) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if self.net_at.is_some() {
            cmds.push(Cmd::Net(NetCmd::Leave));
        }
        if leave_lobby {
            cmds.push(Cmd::Leave);
        }
        log::info!("co-op steam: {}", outcome_line(&outcome));
        self.reset_session();
        self.outcome = Some(outcome);
        self.phase = Phase::Failed;
        cmds
    }

    fn reset_session(&mut self) {
        self.lobby = 0;
        self.owner = 0;
        self.members.clear();
        self.saved_lobby = None;
        self.session_host = None;
        self.room_code = None;
        self.link_seen = false;
        self.net_at = None;
        self.lost_since = None;
        self.lobby_gone_since = None;
        self.last_rejoin = None;
    }

    /// Leave everything (lobby and session) and go idle.
    fn leave_all(&mut self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if self.net_at.is_some() || self.link.running {
            cmds.push(Cmd::Net(NetCmd::Leave));
        }
        if !matches!(self.phase, Phase::Idle | Phase::Failed) {
            cmds.push(Cmd::Leave);
        }
        self.reset_session();
        self.phase = Phase::Idle;
        self.outcome = None;
        cmds
    }

    fn start_host(&mut self, now: Instant) -> Vec<Cmd> {
        self.intent = Some(Intent::Host);
        self.outcome = None;
        self.reset_session();
        self.room_code =
            (self.visibility == Visibility::RoomCode).then(|| roomcode::generate(&mut self.rng));
        self.phase = Phase::Creating { since: now };
        log::info!(
            "co-op steam: lobby requested ({}) for {}",
            self.visibility.key(),
            self.cfg.max_members
        );
        vec![Cmd::Create {
            kind: LobbyKind::of(self.visibility),
            max: self.cfg.max_members,
        }]
    }

    fn start_join(&mut self, lobby: u64, now: Instant) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if self.in_lobby() || matches!(self.phase, Phase::Creating { .. }) {
            cmds.extend(self.leave_all());
        }
        self.intent = Some(Intent::Lobby(lobby));
        self.outcome = None;
        self.reset_session();
        self.saved_lobby = Some(lobby);
        self.phase = Phase::Joining { since: now };
        cmds.push(Cmd::Join(lobby));
        cmds
    }

    fn start_code(&mut self, code: String, now: Instant) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if self.in_lobby() || matches!(self.phase, Phase::Creating { .. }) {
            cmds.extend(self.leave_all());
        }
        self.intent = Some(Intent::Code(code.clone()));
        self.outcome = None;
        self.reset_session();
        self.phase = Phase::Searching {
            since: now,
            polls: 0,
        };
        cmds.push(Cmd::FindCode(code));
        cmds
    }

    /// May a join (invite, launch argument, room code) replace the current state?
    fn may_switch(&self) -> bool {
        match self.phase {
            Phase::Idle | Phase::Failed => true,
            Phase::Hosting => !self.busy_with_partner() && self.members.len() <= 1,
            _ => false,
        }
    }

    /// A UI action.
    pub fn act(&mut self, action: SteamAction, now: Instant) -> Vec<Cmd> {
        match action {
            SteamAction::Host => match self.phase {
                Phase::Idle | Phase::Failed => self.start_host(now),
                _ => Vec::new(),
            },
            SteamAction::SetInviteOnly(on) => self.act(
                SteamAction::SetVisibility(if on {
                    Visibility::InviteOnly
                } else {
                    Visibility::Friends
                }),
                now,
            ),
            SteamAction::SetVisibility(v) => {
                if v == self.visibility {
                    return Vec::new();
                }
                self.visibility = v;
                if self.phase != Phase::Hosting {
                    return Vec::new();
                }
                let mut cmds = vec![Cmd::SetType(LobbyKind::of(v))];
                if v == Visibility::RoomCode {
                    let code = roomcode::generate(&mut self.rng);
                    cmds.push(Cmd::SetData(KEY_CODE, code.clone()));
                    self.room_code = Some(code);
                } else if self.room_code.take().is_some() {
                    cmds.push(Cmd::SetData(KEY_CODE, String::new()));
                }
                cmds
            }
            SteamAction::OpenInvite => {
                if self.in_lobby() {
                    vec![Cmd::Invite]
                } else {
                    self.say("Host a lobby first, then invite.", now);
                    Vec::new()
                }
            }
            SteamAction::JoinCode(text) => {
                if !self.may_switch() {
                    self.say("Leave the current session first.", now);
                    return Vec::new();
                }
                match roomcode::normalize(&text) {
                    Ok(code) => {
                        log::info!("co-op steam: looking up a room code");
                        self.start_code(code, now)
                    }
                    Err(why) => {
                        let mut cmds = Vec::new();
                        if self.phase == Phase::Hosting {
                            return cmds; // a typo doesn't close the lobby
                        }
                        cmds.extend(self.fail(Outcome::Error(SteamError::BadCode(why)), false));
                        cmds
                    }
                }
            }
            SteamAction::Leave => {
                let cmds = self.leave_all();
                if !cmds.is_empty() {
                    log::info!("co-op steam: left the lobby");
                }
                cmds
            }
            SteamAction::Retry => {
                if self.phase != Phase::Failed {
                    return Vec::new();
                }
                match self.intent.clone() {
                    Some(Intent::Host) => self.start_host(now),
                    Some(Intent::Lobby(id)) => self.start_join(id, now),
                    Some(Intent::Code(c)) => self.start_code(c, now),
                    None => Vec::new(),
                }
            }
        }
    }

    /// An invite accepted while the game runs, or `+connect_lobby` at start.
    pub fn join_request(&mut self, lobby: u64, now: Instant) -> Vec<Cmd> {
        if lobby == 0 {
            return Vec::new();
        }
        if self.lobby == lobby && self.in_lobby() {
            return Vec::new(); // already there
        }
        if !self.may_switch() {
            self.say(
                "A Steam invite arrived, but you are in a session. Leave first to accept it.",
                now,
            );
            return Vec::new();
        }
        if self.in_run {
            // No linking during a run.
            log::info!("co-op steam: invite skipped: a run is going");
            self.say(
                "You are in a run. Finish or quit it, then accept the invite again (Steam overlay, or your friend's name in the Steam friends list).",
                now,
            );
            return Vec::new();
        }
        log::info!("co-op steam: joining the host's lobby (invite)");
        self.say("Joining your friend's lobby...", now);
        self.start_join(lobby, now)
    }

    /// An async Steam result.
    pub fn on_event(&mut self, ev: &super::events::SteamEvent, now: Instant) -> Vec<Cmd> {
        use super::events::SteamEvent as E;
        match *ev {
            E::LobbyCreated { ok, lobby } => {
                if !matches!(self.phase, Phase::Creating { .. }) {
                    return Vec::new();
                }
                if ok && lobby != 0 {
                    self.lobby = lobby;
                    self.owner = self.me;
                    return self.hosting(now);
                }
                self.fail(Outcome::Error(SteamError::CreateFailed), true)
            }
            E::LobbyJoined { ok, .. } => {
                if matches!(self.phase, Phase::Joining { .. }) && !ok {
                    return self.fail(Outcome::Error(SteamError::JoinFailed), true);
                }
                Vec::new()
            }
            E::JoinRequested { lobby } => self.join_request(lobby, now),
            E::MemberChange { flags } => {
                if flags & 0x1E != 0 && self.phase == Phase::Hosting && !self.busy_with_partner() {
                    self.say("A player left the lobby.", now);
                }
                Vec::new()
            }
            E::LobbyList => Vec::new(),
        }
    }

    /// The lobby is up: publish the data and start hosting.
    fn hosting(&mut self, now: Instant) -> Vec<Cmd> {
        self.phase = Phase::Hosting;
        self.saved_lobby = Some(self.lobby);
        if !self.members.contains(&self.me) {
            self.members.push(self.me);
        }
        log::info!("co-op steam: lobby up");
        let mut cmds = vec![
            Cmd::SetData(KEY_MOD, self.cfg.mod_version.to_string()),
            Cmd::SetData(KEY_BUILD, self.cfg.build.clone()),
            Cmd::SetData(KEY_STATE, state_value(self.in_run).into()),
        ];
        if !self.cfg.files.is_empty() {
            cmds.push(Cmd::SetData(KEY_FILES, self.cfg.files.clone()));
        }
        if let Some(code) = &self.room_code {
            cmds.push(Cmd::SetData(KEY_CODE, code.clone()));
        }
        self.net_at.get_or_insert(now);
        cmds.push(Cmd::Net(NetCmd::Host {
            allow: self.others(),
        }));
        cmds
    }

    fn others(&self) -> Vec<u64> {
        self.members
            .iter()
            .copied()
            .filter(|&m| m != self.me && m != 0)
            .collect()
    }

    /// The lobby getters.
    pub fn on_poll(&mut self, p: &Poll, now: Instant) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let members_changed = p
            .members
            .as_ref()
            .is_some_and(|m| *m != self.members && p.lobby != 0);
        if let Some(m) = &p.members
            && p.lobby != 0
        {
            self.members.clone_from(m);
        }
        let host_before = self.owner;
        if p.lobby != 0 {
            self.lobby = p.lobby;
            self.owner = p.owner;
            self.lobby_gone_since = None;
        } else if self.in_lobby() {
            self.lobby_gone_since.get_or_insert(now);
        }
        // The host left before the link: Steam handed its lobby to this side
        // and nobody will answer.
        let host_left = p.lobby != 0
            && p.owner == self.me
            && match self.phase {
                Phase::Checking { .. } => {
                    host_before != 0
                        && host_before != self.me
                        && !self.members.contains(&host_before)
                }
                Phase::Session { host } => {
                    !self.link_seen && host != self.me && !self.members.contains(&host)
                }
                _ => false,
            };
        if host_left {
            log::info!("co-op steam: host left the lobby before the link");
            return self.fail(Outcome::Error(SteamError::LobbyEmpty), true);
        }
        match self.phase {
            Phase::Creating { .. } if p.lobby != 0 => {
                self.owner = if p.owner == 0 { self.me } else { p.owner };
                cmds.extend(self.hosting(now));
            }
            Phase::Joining { .. } if p.lobby != 0 && p.owner != 0 => {
                if p.owner == self.me {
                    return self.fail(Outcome::Error(SteamError::LobbyEmpty), true);
                }
                self.saved_lobby = Some(p.lobby);
                self.phase = Phase::Checking { since: now };
            }
            Phase::Hosting if members_changed => {
                cmds.push(Cmd::Net(NetCmd::Host {
                    allow: self.others(),
                }));
            }
            Phase::Session { host } if p.lobby != 0 => {
                // Steam gave this side the lobby while the host was away: hand it back.
                let due = self
                    .last_rejoin
                    .is_none_or(|t| now.saturating_duration_since(t) >= REJOIN_EVERY);
                if p.owner == self.me && host != self.me && self.members.contains(&host) && due {
                    self.last_rejoin = Some(now);
                    cmds.push(Cmd::SetOwner(host));
                }
            }
            _ => {}
        }
        cmds
    }

    /// The host's lobby data (while checking).
    pub fn on_data(&mut self, d: &LobbyData, now: Instant) -> Vec<Cmd> {
        let Phase::Checking { since } = self.phase else {
            return Vec::new();
        };
        if d.ndml.is_empty() && now.saturating_duration_since(since) < DATA_TIMEOUT {
            return Vec::new(); // the lobby data may still be on its way
        }
        let rejoining = self.session_host.is_some();
        if let Err(r) = check_data(
            d,
            self.cfg.mod_version,
            &self.cfg.build,
            &self.cfg.files,
            rejoining,
        ) {
            return self.fail(Outcome::Refused(r), true);
        }
        let host = self.owner;
        self.session_host = Some(host);
        self.phase = Phase::Session { host };
        self.net_at = Some(now);
        self.link_seen = false;
        log::info!("co-op steam: in the host's lobby; versions match");
        vec![Cmd::Net(NetCmd::Join { host })]
    }

    /// The room-code search.
    pub fn on_list(&mut self, l: ListPoll, now: Instant) -> Vec<Cmd> {
        let Phase::Searching { since, polls } = self.phase else {
            return Vec::new();
        };
        let polls = polls + 1;
        self.phase = Phase::Searching { since, polls };
        if polls < 2 || l.loading {
            return Vec::new();
        }
        if l.count == 0 {
            let code = match &self.intent {
                Some(Intent::Code(c)) => c.clone(),
                _ => String::new(),
            };
            return self.fail(Outcome::Error(SteamError::NoLobbyForCode(code)), false);
        }
        self.phase = Phase::Joining { since: now };
        vec![Cmd::JoinFound]
    }

    /// The lobby agent's view of the session.
    pub fn on_link(&mut self, link: Link, now: Instant) -> Vec<Cmd> {
        let was_lost = self.link.reconnecting;
        if link.running {
            self.link_seen = true;
        }
        if link.reconnecting && !was_lost {
            self.lost_since = Some(now);
            log::info!("co-op steam: connection lost");
        }
        if link.connected && (was_lost || !self.link.connected) {
            if was_lost {
                log::info!("co-op steam: reconnected");
            }
            self.lost_since = None;
        }
        self.link = link;
        if !self.link_seen {
            return Vec::new();
        }
        let ended = self.link.ended.clone();
        match (self.phase, ended) {
            (Phase::Session { .. }, Some(end)) => {
                let outcome = match end {
                    LinkEnd::Refused(k) => Outcome::Refused(refusal_of(&k)),
                    LinkEnd::HandshakeTimeout => Outcome::Error(SteamError::HandshakeTimeout),
                    LinkEnd::Lost(_) | LinkEnd::Dropped => Outcome::Left,
                };
                self.fail(outcome, true)
            }
            (Phase::Hosting, Some(LinkEnd::Dropped)) => {
                // The network side stopped hosting (a UDP request took over).
                let cmds = self.leave_all();
                self.say("The Steam lobby was closed.", now);
                cmds
            }
            _ => Vec::new(),
        }
    }

    /// Whether the local game is in a run (the host publishes it).
    pub fn set_in_run(&mut self, in_run: bool) -> Vec<Cmd> {
        if in_run == self.in_run {
            return Vec::new();
        }
        self.in_run = in_run;
        if self.phase == Phase::Hosting && self.owner == self.me {
            vec![Cmd::SetData(KEY_STATE, state_value(in_run).into())]
        } else {
            Vec::new()
        }
    }

    /// A Steam call failed synchronously (it returned false).
    pub fn call_failed(&mut self, cmd: &Cmd) -> Vec<Cmd> {
        match (cmd, self.phase) {
            (Cmd::Create { .. }, Phase::Creating { .. }) => {
                self.fail(Outcome::Error(SteamError::CreateFailed), false)
            }
            (Cmd::Join(_) | Cmd::JoinFound, Phase::Joining { .. }) => {
                self.fail(Outcome::Error(SteamError::JoinFailed), false)
            }
            (Cmd::FindCode(_), Phase::Searching { .. }) => self.fail(
                Outcome::Error(SteamError::Call("lobby search".into())),
                false,
            ),
            _ => Vec::new(),
        }
    }

    /// Timeouts and the periodic lobby rejoin.
    pub fn tick(&mut self, now: Instant) -> Vec<Cmd> {
        let since = |t: Instant| now.saturating_duration_since(t);
        match self.phase {
            Phase::Creating { since: t } if since(t) > CREATE_TIMEOUT => {
                return self.fail(Outcome::Error(SteamError::CreateFailed), true);
            }
            Phase::Joining { since: t } if since(t) > JOIN_TIMEOUT => {
                return self.fail(Outcome::Error(SteamError::JoinFailed), true);
            }
            Phase::Searching { since: t, .. } if since(t) > SEARCH_TIMEOUT => {
                return self.fail(
                    Outcome::Error(SteamError::Call("lobby search".into())),
                    false,
                );
            }
            Phase::Checking { since: t } if since(t) > DATA_TIMEOUT * 2 => {
                return self.fail(Outcome::Refused(Refusal::NotCoop), true);
            }
            _ => {}
        }
        let session = matches!(self.phase, Phase::Hosting | Phase::Session { .. });
        if session && !self.link_seen && self.net_at.is_some_and(|t| since(t) > NET_START_TIMEOUT) {
            return self.fail(
                Outcome::Error(SteamError::Link(
                    "The game connection did not start.".into(),
                )),
                true,
            );
        }
        // Dropped out of the lobby during a session: join it again.
        let gone = self
            .lobby_gone_since
            .is_some_and(|t| since(t) > LOBBY_LOST_GRACE);
        let due = self.last_rejoin.is_none_or(|t| since(t) >= REJOIN_EVERY);
        if session
            && gone
            && due
            && let Some(saved) = self.saved_lobby
        {
            self.last_rejoin = Some(now);
            if self.phase == Phase::Hosting && !self.busy_with_partner() {
                // Nobody to keep the old lobby alive for: make a new one.
                log::info!("co-op steam: lobby gone; new lobby requested");
                let net = self.net_at;
                let mut cmds = self.start_host(now);
                self.net_at = net;
                cmds.insert(0, Cmd::Leave);
                return cmds;
            }
            log::info!("co-op steam: dropped from the lobby; rejoin requested");
            return vec![Cmd::Join(saved)];
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| since(*at) > NOTICE_FOR)
        {
            self.notice = None;
        }
        Vec::new()
    }

    /// The page to show.
    #[must_use]
    pub fn page(&self) -> SteamPage {
        let linked = if self.in_run {
            SteamPage::InRun
        } else {
            SteamPage::Linked
        };
        match self.phase {
            Phase::Idle => SteamPage::Idle,
            Phase::Creating { .. } => SteamPage::Creating,
            Phase::Hosting | Phase::Session { .. } if self.link.connected => linked,
            Phase::Hosting | Phase::Session { .. } if self.link.reconnecting => SteamPage::PeerLost,
            Phase::Hosting if self.others().is_empty() => SteamPage::Hosting,
            Phase::Hosting | Phase::Session { .. } => SteamPage::Connecting,
            Phase::Searching { .. } | Phase::Joining { .. } => SteamPage::Joining,
            Phase::Checking { .. } => SteamPage::InLobby,
            Phase::Failed => match self.outcome {
                Some(Outcome::Refused(_)) => SteamPage::Refused,
                Some(Outcome::Error(_)) => SteamPage::Error,
                Some(Outcome::Left) => SteamPage::Left,
                None => SteamPage::Idle,
            },
        }
    }

    /// The local role while in a lobby.
    #[must_use]
    pub fn role(&self) -> Option<SteamRole> {
        match self.phase {
            Phase::Creating { .. } | Phase::Hosting => Some(SteamRole::Host),
            Phase::Searching { .. }
            | Phase::Joining { .. }
            | Phase::Checking { .. }
            | Phase::Session { .. } => Some(SteamRole::Join),
            Phase::Idle | Phase::Failed => None,
        }
    }

    /// Lobby members (the local one included).
    #[must_use]
    pub fn members(&self) -> &[u64] {
        &self.members
    }

    /// The session host's Steam id (the local one while hosting).
    #[must_use]
    pub fn host(&self) -> Option<u64> {
        match self.phase {
            Phase::Hosting | Phase::Creating { .. } => Some(self.me),
            _ => self.session_host,
        }
    }

    /// The lobby owner.
    #[must_use]
    pub fn owner(&self) -> u64 {
        self.owner
    }

    /// Visibility of a lobby hosted here.
    #[must_use]
    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    /// The room code, while hosting with one.
    #[must_use]
    pub fn room_code(&self) -> Option<&str> {
        self.room_code.as_deref()
    }

    /// The link as last reported.
    #[must_use]
    pub fn link(&self) -> &Link {
        &self.link
    }

    /// How the last attempt ended.
    #[must_use]
    pub fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }

    /// When the connection dropped, while reconnecting.
    #[must_use]
    pub fn lost_since(&self) -> Option<Instant> {
        self.lost_since.filter(|_| self.link.reconnecting)
    }

    /// The current notice.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_ref().map(|(t, _)| t.as_str())
    }

    /// The `[steam] lobby` state for `status.toml`.
    #[must_use]
    pub fn lobby_state(&self) -> ndml_net::lobby::SteamLobbyState {
        use ndml_net::lobby::SteamLobbyState as S;
        match self.phase {
            Phase::Idle => S::None,
            Phase::Creating { .. } => S::Creating,
            Phase::Hosting => S::Hosting,
            Phase::Searching { .. } => S::Searching,
            Phase::Joining { .. } => S::Joining,
            Phase::Checking { .. } | Phase::Session { .. } => S::InLobby,
            Phase::Failed => S::Failed,
        }
    }
}

fn state_value(in_run: bool) -> &'static str {
    if in_run { "in_run" } else { "lobby" }
}

fn outcome_line(o: &Outcome) -> String {
    match o {
        // A room code is a lobby's id: never in the log.
        Outcome::Error(e @ SteamError::NoLobbyForCode(_)) => {
            format!("failed ({}): the room code matched no lobby", e.key())
        }
        Outcome::Refused(r) => format!("refused ({}): {}", r.key(), r.message()),
        Outcome::Error(e) => format!("failed ({}): {}", e.key(), e.message()),
        Outcome::Left => "the partner left the session".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steam::events::SteamEvent;
    use ndml_net::lobby::PeerStatus;

    const ME: u64 = 0x0110_0001_0000_0001;
    const OTHER: u64 = 0x0110_0001_0000_0002;
    const LOBBY: u64 = 0x0186_0000_0000_0099;

    fn machine(v: Visibility) -> Machine {
        let mut m = Machine::new(MachineConfig {
            mod_version: 1,
            build: "pe-1".into(),
            files: String::new(),
            max_members: 4,
            visibility: v,
            seed: 3,
        });
        m.set_me(ME);
        m
    }

    fn poll(lobby: u64, owner: u64, members: &[u64]) -> Poll {
        Poll {
            lobby,
            owner,
            members: Some(members.to_vec()),
        }
    }

    fn link(running: bool, connected: bool) -> Link {
        Link {
            running,
            connected,
            ..Link::default()
        }
    }

    fn s(t0: Instant, secs: f64) -> Instant {
        t0 + Duration::from_secs_f64(secs)
    }

    #[test]
    fn hosting_creates_publishes_and_admits_members() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        assert!(!m.active());
        assert_eq!(m.page(), SteamPage::Idle);
        let cmds = m.act(SteamAction::Host, t0);
        assert_eq!(
            cmds,
            vec![Cmd::Create {
                kind: LobbyKind::FriendsOnly,
                max: 4
            }]
        );
        assert_eq!(m.page(), SteamPage::Creating);
        assert!(m.active());
        assert!(m.act(SteamAction::Host, t0).is_empty());
        // The lobby appears by polling (no listener needed).
        let cmds = m.on_poll(&poll(LOBBY, ME, &[ME]), s(t0, 0.1));
        assert_eq!(
            cmds,
            vec![
                Cmd::SetData(KEY_MOD, "1".into()),
                Cmd::SetData(KEY_BUILD, "pe-1".into()),
                Cmd::SetData(KEY_STATE, "lobby".into()),
                Cmd::Net(NetCmd::Host { allow: vec![] }),
            ]
        );
        assert_eq!(m.page(), SteamPage::Hosting);
        assert_eq!(m.role(), Some(SteamRole::Host));
        assert!(
            m.on_event(
                &SteamEvent::LobbyCreated {
                    ok: true,
                    lobby: LOBBY
                },
                s(t0, 0.2)
            )
            .is_empty()
        );
        m.on_link(link(true, false), s(t0, 0.3));
        // The other player joins the lobby: allowed in, then link.
        let cmds = m.on_poll(&poll(LOBBY, ME, &[ME, OTHER]), s(t0, 5.0));
        assert_eq!(cmds, vec![Cmd::Net(NetCmd::Host { allow: vec![OTHER] })]);
        assert_eq!(m.page(), SteamPage::Connecting);
        m.on_link(link(true, true), s(t0, 5.5));
        assert_eq!(m.page(), SteamPage::Linked);
        // A run starts: the lobby says so; the page too.
        assert_eq!(
            m.set_in_run(true),
            vec![Cmd::SetData(KEY_STATE, "in_run".into())]
        );
        assert_eq!(m.page(), SteamPage::InRun);
        assert!(m.set_in_run(true).is_empty());
        assert_eq!(
            m.act(SteamAction::OpenInvite, s(t0, 6.0)),
            vec![Cmd::Invite]
        );
        let cmds = m.act(SteamAction::Leave, s(t0, 7.0));
        assert_eq!(cmds, vec![Cmd::Net(NetCmd::Leave), Cmd::Leave]);
        assert_eq!(m.page(), SteamPage::Idle);
    }

    #[test]
    fn create_failures_and_retry() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::InviteOnly);
        assert_eq!(
            m.act(SteamAction::Host, t0),
            vec![Cmd::Create {
                kind: LobbyKind::Private,
                max: 4
            }]
        );
        let cmds = m.on_event(
            &SteamEvent::LobbyCreated {
                ok: false,
                lobby: 0,
            },
            s(t0, 0.1),
        );
        assert_eq!(cmds, vec![Cmd::Leave]);
        assert_eq!(m.page(), SteamPage::Error);
        assert!(!m.active());
        assert!(matches!(
            m.outcome(),
            Some(Outcome::Error(SteamError::CreateFailed))
        ));
        assert!(matches!(
            m.act(SteamAction::Retry, s(t0, 1.0))[0],
            Cmd::Create { .. }
        ));
        let cmds = m.tick(s(t0, 17.0));
        assert_eq!(cmds, vec![Cmd::Leave]);
        assert_eq!(m.page(), SteamPage::Error);
        // A false return from the call fails at once.
        m.act(SteamAction::Retry, s(t0, 18.0));
        m.call_failed(&Cmd::Create {
            kind: LobbyKind::Private,
            max: 4,
        });
        assert_eq!(m.page(), SteamPage::Error);
    }

    #[test]
    fn room_code_hosting_and_visibility_changes() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        // Changing visibility while idle only remembers it.
        assert!(
            m.act(SteamAction::SetVisibility(Visibility::RoomCode), t0)
                .is_empty()
        );
        let cmds = m.act(SteamAction::Host, t0);
        assert_eq!(
            cmds,
            vec![Cmd::Create {
                kind: LobbyKind::Invisible,
                max: 4
            }]
        );
        let cmds = m.on_poll(&poll(LOBBY, ME, &[ME]), s(t0, 0.1));
        let code = m.room_code().unwrap().to_owned();
        assert_eq!(code.len(), 5);
        assert!(cmds.contains(&Cmd::SetData(KEY_CODE, code.clone())));
        // Switch to invite-only while hosting: type changes, code removed.
        let cmds = m.act(SteamAction::SetInviteOnly(true), s(t0, 1.0));
        assert_eq!(
            cmds,
            vec![
                Cmd::SetType(LobbyKind::Private),
                Cmd::SetData(KEY_CODE, String::new())
            ]
        );
        assert!(m.room_code().is_none());
        let cmds = m.act(SteamAction::SetVisibility(Visibility::RoomCode), s(t0, 2.0));
        assert_eq!(cmds[0], Cmd::SetType(LobbyKind::Invisible));
        assert!(matches!(&cmds[1], Cmd::SetData(KEY_CODE, c) if c.len() == 5));
    }

    /// An invite accepted during a run is turned down, with why.
    #[test]
    fn an_invite_during_a_run_is_turned_down() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        let _ = m.set_in_run(true);
        let cmds = m.on_event(&SteamEvent::JoinRequested { lobby: LOBBY }, t0);
        assert!(cmds.is_empty());
        assert_eq!(m.page(), SteamPage::Idle);
        assert!(m.notice().is_some_and(|n| n.contains("You are in a run")));
        let _ = m.set_in_run(false);
        let cmds = m.on_event(&SteamEvent::JoinRequested { lobby: LOBBY }, t0);
        assert_eq!(cmds, vec![Cmd::Join(LOBBY)], "from the menu it joins");
    }

    fn joined(m: &mut Machine, t0: Instant) {
        let cmds = m.on_event(&SteamEvent::JoinRequested { lobby: LOBBY }, t0);
        assert_eq!(cmds, vec![Cmd::Join(LOBBY)]);
        assert_eq!(m.page(), SteamPage::Joining);
        assert!(
            m.on_poll(&poll(LOBBY, OTHER, &[OTHER, ME]), s(t0, 0.5))
                .is_empty()
        );
        assert_eq!(m.page(), SteamPage::InLobby);
        let data = LobbyData {
            ndml: "1".into(),
            ver: "pe-1".into(),
            state: "lobby".into(),
            ..LobbyData::default()
        };
        assert_eq!(
            m.on_data(&data, s(t0, 0.6)),
            vec![Cmd::Net(NetCmd::Join { host: OTHER })]
        );
        assert_eq!(m.page(), SteamPage::Connecting);
        assert_eq!(m.role(), Some(SteamRole::Join));
    }

    #[test]
    fn joining_by_invite_checks_versions_then_connects() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        joined(&mut m, t0);
        m.on_link(link(true, false), s(t0, 0.7));
        m.on_link(
            Link {
                rtt_ms: Some(40.0),
                ..link(true, true)
            },
            s(t0, 1.0),
        );
        assert_eq!(m.page(), SteamPage::Linked);
        assert_eq!(m.host(), Some(OTHER));
        // The cable is pulled: reconnecting, then back.
        m.on_link(
            Link {
                reconnecting: true,
                ..link(true, false)
            },
            s(t0, 20.0),
        );
        assert_eq!(m.page(), SteamPage::PeerLost);
        assert!(m.lost_since().is_some());
        m.on_link(link(true, true), s(t0, 50.0));
        assert_eq!(m.page(), SteamPage::Linked);
        assert!(m.lost_since().is_none());
        // The host says goodbye: Left, and this side leaves the lobby.
        let cmds = m.on_link(
            Link {
                ended: Some(LinkEnd::Lost("Your partner disconnected.".into())),
                ..Link::default()
            },
            s(t0, 60.0),
        );
        assert_eq!(cmds, vec![Cmd::Net(NetCmd::Leave), Cmd::Leave]);
        assert_eq!(m.page(), SteamPage::Left);
        // Retry joins the same lobby again.
        assert_eq!(
            m.act(SteamAction::Retry, s(t0, 61.0)),
            vec![Cmd::Join(LOBBY)]
        );
    }

    #[test]
    fn mismatches_are_refused_before_connecting() {
        let t0 = Instant::now();
        for (data, key) in [
            (("2", "pe-1", "lobby"), "version_mismatch"),
            (("1", "pe-2", "lobby"), "version_mismatch"),
            (("1", "pe-1", "in_run"), "run_in_progress"),
        ] {
            let mut m = machine(Visibility::Friends);
            m.join_request(LOBBY, t0);
            m.on_poll(&poll(LOBBY, OTHER, &[OTHER, ME]), s(t0, 0.5));
            let d = LobbyData {
                ndml: data.0.into(),
                ver: data.1.into(),
                state: data.2.into(),
                ..LobbyData::default()
            };
            let cmds = m.on_data(&d, s(t0, 0.6));
            assert_eq!(cmds, vec![Cmd::Leave], "{key}");
            assert_eq!(m.page(), SteamPage::Refused);
            assert!(
                matches!(m.outcome(), Some(Outcome::Refused(r)) if r.key() == key),
                "{key}: {:?}",
                m.outcome()
            );
        }
        // No lobby data at all: wait a while, then "not a co-op lobby".
        let mut m = machine(Visibility::Friends);
        m.join_request(LOBBY, t0);
        m.on_poll(&poll(LOBBY, OTHER, &[OTHER, ME]), s(t0, 0.5));
        assert!(m.on_data(&LobbyData::default(), s(t0, 1.0)).is_empty());
        assert_eq!(
            m.on_data(&LobbyData::default(), s(t0, 6.0)),
            vec![Cmd::Leave]
        );
        assert!(matches!(
            m.outcome(),
            Some(Outcome::Refused(Refusal::NotCoop))
        ));
    }

    /// Different co-op mod files are refused; a host without the key is not.
    #[test]
    fn different_coop_mod_files_are_refused() {
        let d = |files: &str| LobbyData {
            ndml: "1".into(),
            ver: "pe-1".into(),
            state: "lobby".into(),
            files: files.into(),
        };
        assert_eq!(
            check_data(&d("aaaa1111"), 1, "pe-1", "aaaa1111", false),
            Ok(())
        );
        assert_eq!(check_data(&d(""), 1, "pe-1", "aaaa1111", false), Ok(()));
        assert_eq!(check_data(&d("aaaa1111"), 1, "pe-1", "", false), Ok(()));
        let refused = check_data(&d("bbbb2222"), 1, "pe-1", "aaaa1111", false);
        let what = match &refused {
            Err(Refusal::VersionMismatch { what }) => what.as_str(),
            _ => "",
        };
        assert!(
            what.contains("aaaa1111") && what.contains("bbbb2222"),
            "{refused:?}"
        );
        assert_eq!(files_id(b"x"), files_id(b"x"));
        assert_ne!(files_id(b"x"), files_id(b"y"));
        // The host publishes it.
        let mut m = Machine::new(MachineConfig {
            mod_version: 1,
            build: "pe-1".into(),
            files: "aaaa1111".into(),
            max_members: 4,
            visibility: Visibility::Friends,
            seed: 3,
        });
        m.set_me(ME);
        let t0 = Instant::now();
        m.act(SteamAction::Host, t0);
        let cmds = m.on_poll(&poll(LOBBY, ME, &[ME]), s(t0, 0.2));
        assert!(
            cmds.contains(&Cmd::SetData(KEY_FILES, "aaaa1111".into())),
            "{cmds:?}"
        );
    }

    #[test]
    fn netcode_refusals_and_timeouts_end_the_attempt() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        joined(&mut m, t0);
        // A stale Failed status is ignored until the session has started.
        let stale = Link {
            ended: Some(LinkEnd::HandshakeTimeout),
            ..Link::default()
        };
        assert!(m.on_link(stale.clone(), s(t0, 0.7)).is_empty());
        m.on_link(link(true, false), s(t0, 0.8));
        let cmds = m.on_link(
            Link {
                ended: Some(LinkEnd::Refused("full".into())),
                ..Link::default()
            },
            s(t0, 1.0),
        );
        assert_eq!(cmds, vec![Cmd::Net(NetCmd::Leave), Cmd::Leave]);
        assert!(matches!(m.outcome(), Some(Outcome::Refused(Refusal::Full))));

        let mut m = machine(Visibility::Friends);
        joined(&mut m, t0);
        m.on_link(link(true, false), s(t0, 0.8));
        m.on_link(stale, s(t0, 31.0));
        assert!(matches!(
            m.outcome(),
            Some(Outcome::Error(SteamError::HandshakeTimeout))
        ));
        // The agent never picks the request up.
        let mut m = machine(Visibility::Friends);
        joined(&mut m, t0);
        assert!(!m.tick(s(t0, 3.0)).contains(&Cmd::Leave));
        assert_eq!(
            m.tick(s(t0, 6.0)),
            vec![Cmd::Net(NetCmd::Leave), Cmd::Leave]
        );
    }

    #[test]
    fn room_code_join_by_polling() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        let cmds = m.act(SteamAction::JoinCode(" k7q-x2 ".into()), t0);
        assert_eq!(cmds, vec![Cmd::FindCode("K7QX2".into())]);
        assert!(m.wants().list);
        assert_eq!(m.page(), SteamPage::Joining);
        // The first poll is not trusted (the request may not have started).
        assert!(
            m.on_list(
                ListPoll {
                    loading: false,
                    count: 0
                },
                s(t0, 0.02)
            )
            .is_empty()
        );
        assert!(
            m.on_list(
                ListPoll {
                    loading: true,
                    count: 0
                },
                s(t0, 0.05)
            )
            .is_empty()
        );
        assert_eq!(
            m.on_list(
                ListPoll {
                    loading: false,
                    count: 1
                },
                s(t0, 0.2)
            ),
            vec![Cmd::JoinFound]
        );
        assert_eq!(m.page(), SteamPage::Joining);
        m.on_poll(&poll(LOBBY, OTHER, &[OTHER, ME]), s(t0, 0.5));
        assert_eq!(m.page(), SteamPage::InLobby);
        let mut m = machine(Visibility::Friends);
        m.act(SteamAction::JoinCode("K7QX2".into()), t0);
        m.on_list(ListPoll::default(), s(t0, 0.1));
        assert!(m.on_list(ListPoll::default(), s(t0, 0.2)).is_empty());
        assert!(matches!(
            m.outcome(),
            Some(Outcome::Error(SteamError::NoLobbyForCode(c))) if c == "K7QX2"
        ));
        // The screen says the code; the log line never does.
        let line = outcome_line(m.outcome().unwrap());
        assert!(!line.contains("K7QX2"), "{line}");
        assert!(line.contains("matched no lobby"), "{line}");
        // A typo is reported without a search.
        let mut m = machine(Visibility::Friends);
        assert!(m.act(SteamAction::JoinCode("K7Q".into()), t0).is_empty());
        assert!(matches!(
            m.outcome(),
            Some(Outcome::Error(SteamError::BadCode(_)))
        ));
    }

    #[test]
    fn invites_switch_an_empty_lobby_but_not_a_live_session() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        m.act(SteamAction::Host, t0);
        m.on_poll(&poll(LOBBY, ME, &[ME]), s(t0, 0.1));
        m.on_link(link(true, false), s(t0, 0.2));
        // Alone in its own lobby: an invite moves this side over.
        let cmds = m.join_request(LOBBY + 1, s(t0, 1.0));
        assert_eq!(
            cmds,
            vec![Cmd::Net(NetCmd::Leave), Cmd::Leave, Cmd::Join(LOBBY + 1)]
        );
        // Linked: an invite is ignored with a notice.
        let mut m = machine(Visibility::Friends);
        joined(&mut m, t0);
        m.on_link(link(true, true), s(t0, 1.0));
        assert!(m.join_request(LOBBY + 2, s(t0, 2.0)).is_empty());
        assert!(m.notice().unwrap().contains("Leave first"));
        assert!(
            m.join_request(LOBBY, s(t0, 2.0)).is_empty(),
            "already there"
        );
        m.tick(s(t0, 20.0));
        assert!(m.notice().is_none());
    }

    #[test]
    fn a_dropped_player_rejoins_the_lobby_and_hands_ownership_back() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        joined(&mut m, t0);
        m.on_link(link(true, true), s(t0, 1.0));
        // The local Steam connection dropped: the lobby id reads 0; rejoin.
        m.on_link(
            Link {
                reconnecting: true,
                ..link(true, false)
            },
            s(t0, 12.0),
        );
        assert!(m.on_poll(&poll(0, 0, &[]), s(t0, 12.0)).is_empty());
        assert!(m.tick(s(t0, 14.0)).is_empty(), "grace");
        assert_eq!(m.tick(s(t0, 15.5)), vec![Cmd::Join(LOBBY)]);
        assert!(m.tick(s(t0, 20.0)).is_empty(), "not again so soon");
        assert_eq!(m.tick(s(t0, 26.0)), vec![Cmd::Join(LOBBY)]);
        // Back in, and Steam made this side the owner while the host was away.
        let cmds = m.on_poll(&poll(LOBBY, ME, &[ME, OTHER]), s(t0, 40.0));
        assert_eq!(cmds, vec![Cmd::SetOwner(OTHER)]);
        assert!(
            m.on_poll(&poll(LOBBY, ME, &[ME, OTHER]), s(t0, 41.0))
                .is_empty()
        );
        // The host keeps its role in the session regardless.
        assert_eq!(m.host(), Some(OTHER));
    }

    #[test]
    fn a_host_whose_lobby_vanished_rejoins_or_recreates() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        m.act(SteamAction::Host, t0);
        m.on_poll(&poll(LOBBY, ME, &[ME, OTHER]), s(t0, 0.1));
        m.on_link(link(true, true), s(t0, 1.0));
        m.on_link(
            Link {
                reconnecting: true,
                ..link(true, false)
            },
            s(t0, 2.0),
        );
        m.on_poll(&poll(0, 0, &[]), s(t0, 2.0));
        assert_eq!(
            m.tick(s(t0, 6.0)),
            vec![Cmd::Join(LOBBY)],
            "partner kept it alive"
        );
        // Nobody linked and the lobby gone: a fresh lobby.
        let mut m = machine(Visibility::Friends);
        m.act(SteamAction::Host, t0);
        m.on_poll(&poll(LOBBY, ME, &[ME]), s(t0, 0.1));
        m.on_link(link(true, false), s(t0, 0.2));
        m.on_poll(&poll(0, 0, &[]), s(t0, 1.0));
        let cmds = m.tick(s(t0, 5.0));
        assert_eq!(cmds[0], Cmd::Leave);
        assert!(matches!(cmds[1], Cmd::Create { .. }));
    }

    #[test]
    fn link_is_read_from_the_status() {
        let mut st = Status {
            connector: Connector::Steam,
            state: LobbyState::Connected,
            peers: vec![PeerStatus {
                connected: true,
                rtt_ms: 45.0,
                ..PeerStatus::default()
            }],
            ..Status::default()
        };
        let l = link_of(&st);
        assert!(l.running && l.connected && l.ended.is_none());
        assert_eq!(l.rtt_ms, Some(45.0));
        st.state = LobbyState::Failed;
        st.error_kind = Some(ErrorKind::Refused);
        st.refused = Some("full".into());
        assert_eq!(link_of(&st).ended, Some(LinkEnd::Refused("full".into())));
        st.error_kind = Some(ErrorKind::LinkLost);
        assert!(matches!(link_of(&st).ended, Some(LinkEnd::Lost(_))));
        st.state = LobbyState::Ready;
        assert_eq!(link_of(&st).ended, Some(LinkEnd::Dropped));
        st.connector = Connector::Udp;
        assert_eq!(
            link_of(&st),
            Link::default(),
            "a UDP session is not the Steam connector's"
        );
        st.connector = Connector::Steam;
        st.state = LobbyState::Connecting;
        st.reconnecting = true;
        assert!(link_of(&st).reconnecting);
        assert_eq!(refusal_of("run_in_progress"), Refusal::HostInRun);
        assert!(matches!(refusal_of("x"), Refusal::Other(_)));
    }

    fn page_text(m: &Machine) -> String {
        match m.outcome() {
            Some(Outcome::Error(e)) => e.message(),
            Some(Outcome::Refused(r)) => r.message(),
            other => format!("{other:?}"),
        }
    }

    /// The host leaving at any point before the link ends the join at once.
    #[test]
    fn the_host_leaving_mid_join_is_told_at_once() {
        let t0 = Instant::now();
        let alone = poll(LOBBY, ME, &[ME]);
        // Joining: the first poll already shows this side alone.
        let mut m = machine(Visibility::Friends);
        m.join_request(LOBBY, t0);
        assert_eq!(m.on_poll(&alone, s(t0, 0.5)), vec![Cmd::Leave]);
        assert_eq!(page_text(&m), "Nobody is hosting in that lobby any more.");
        // Checking its lobby data.
        let mut m = machine(Visibility::Friends);
        m.join_request(LOBBY, t0);
        m.on_poll(&poll(LOBBY, OTHER, &[OTHER, ME]), s(t0, 0.5));
        assert!(matches!(m.page(), SteamPage::InLobby));
        assert_eq!(m.on_poll(&alone, s(t0, 1.0)), vec![Cmd::Leave]);
        assert_eq!(page_text(&m), "Nobody is hosting in that lobby any more.");
        // Connecting, not linked yet.
        let mut m = machine(Visibility::Friends);
        joined(&mut m, t0);
        let cmds = m.on_poll(&alone, s(t0, 2.0));
        assert!(cmds.contains(&Cmd::Leave), "{cmds:?}");
        assert_eq!(page_text(&m), "Nobody is hosting in that lobby any more.");
        // Linked: the link, not the lobby, decides whether the host is gone.
        let mut m = machine(Visibility::Friends);
        joined(&mut m, t0);
        m.on_link(link(true, true), s(t0, 1.0));
        assert!(
            m.on_poll(&alone, s(t0, 2.0))
                .iter()
                .all(|c| *c != Cmd::Leave)
        );
        assert_eq!(m.page(), SteamPage::Linked);
    }

    /// An invite to a host in a run is refused before connecting.
    #[test]
    fn an_invite_to_a_host_in_a_run_is_refused_with_the_run_text() {
        let t0 = Instant::now();
        let mut m = machine(Visibility::Friends);
        assert_eq!(
            m.on_event(&SteamEvent::JoinRequested { lobby: LOBBY }, t0),
            vec![Cmd::Join(LOBBY)]
        );
        m.on_poll(&poll(LOBBY, OTHER, &[OTHER, ME]), s(t0, 0.5));
        let data = LobbyData {
            ndml: "1".into(),
            ver: "pe-1".into(),
            state: "in_run".into(),
            ..LobbyData::default()
        };
        assert_eq!(m.on_data(&data, s(t0, 0.6)), vec![Cmd::Leave]);
        assert_eq!(m.page(), SteamPage::Refused);
        assert_eq!(
            page_text(&m),
            "Your partner is in a run right now. Join when they are back in the main menu."
        );
    }
}
