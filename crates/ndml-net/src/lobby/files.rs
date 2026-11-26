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

//! The lobby's two files. Both are TOML with a `format` number ([`FORMAT`]);
//! unknown keys are ignored, a file without `format` is malformed. Writers
//! replace a file atomically ([`write_atomic`]).

#![forbid(unsafe_code)]

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::code::ConnectionCode;

/// Directory under `<game>/ndml/` that holds the lobby files.
pub const COOP_DIR: &str = "coop";
/// Written by the game (the co-op mod).
pub const STATUS_FILE: &str = "status.toml";
/// Read by the game: requests that drive its lobby from outside the in-game panel.
pub const PEER_FILE: &str = "peer.toml";
/// Current version of both file formats.
pub const FORMAT: u32 = 1;
/// UDP port the game tries first; if taken, any free port.
pub const DEFAULT_PORT: u16 = 7777;
/// The game rewrites the status at least this often.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
/// A status whose heartbeat is older than this is stale.
pub const STALE_AFTER: Duration = Duration::from_secs(3);
/// Longest player name kept (characters).
pub const MAX_NAME_CHARS: usize = 24;
/// The name (any case) that asks for an automatic one.
pub const AUTO_NAME: &str = "Random";

/// `<ndml root>/coop`: the lobby directory.
#[must_use]
pub fn lobby_dir(ndml_root: &Path) -> PathBuf {
    ndml_root.join(COOP_DIR)
}

/// Milliseconds since the Unix epoch (0 for clocks before it).
#[must_use]
pub fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The session id for a nonce: 8 lowercase hex digits. Changes every launch.
#[must_use]
pub fn session_id(nonce: u32) -> String {
    format!("{nonce:08x}")
}

/// A player name as shown to others: no control characters, trimmed, at most
/// [`MAX_NAME_CHARS`]; `None` when empty.
#[must_use]
pub fn clean_name(raw: &str) -> Option<String> {
    let s: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    let s = s.trim_end().to_owned();
    (!s.is_empty()).then_some(s)
}

/// Where the lobby is, as written to `status.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LobbyState {
    /// The agent exists but has not run yet.
    #[default]
    Starting,
    /// Asking STUN servers for the public endpoint.
    Discovering,
    /// Idle with a code to share; no role chosen.
    Ready,
    /// Hosting: listening for a partner.
    Waiting,
    /// Punching toward the partner.
    Connecting,
    /// At least one partner is linked.
    Connected,
    /// The last attempt failed; `error` says why. A new request retries.
    Failed,
    /// The game shut the lobby down.
    Stopped,
}

impl LobbyState {
    /// Display name in capitals.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "STARTING",
            Self::Discovering => "DISCOVERING",
            Self::Ready => "READY",
            Self::Waiting => "WAITING",
            Self::Connecting => "CONNECTING",
            Self::Connected => "CONNECTED",
            Self::Failed => "FAILED",
            Self::Stopped => "STOPPED",
        }
    }
}

/// The role a player picked in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleChoice {
    /// Not decided (or disconnected).
    #[default]
    None,
    /// Host the session.
    Host,
    Join,
}

/// What kind of failure `status.error` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Socket,
    /// The partner never answered within the handshake timeout.
    HandshakeTimeout,
    /// One side turned the other away.
    Refused,
    /// An established link went quiet or the partner left.
    LinkLost,
    /// `peer.toml` asked for something impossible.
    BadRequest,
}

/// Public endpoint discovery, as written to `status.toml` (`[stun]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StunState {
    /// Not asked yet or waiting for an answer.
    #[default]
    Pending,
    /// The public endpoint is known.
    Ok,
    /// No server answered; it retries after `retry_in_ms`.
    Failed,
    /// No servers configured (LAN only).
    Disabled,
}

/// What two STUN servers say about the NAT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatKind {
    /// Fewer than two answers.
    #[default]
    Unknown,
    /// Same mapping toward every destination: hole punching should work.
    Consistent,
    /// The mapping changes per destination (symmetric NAT).
    Varies,
}

/// The `[stun]` table.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StunStatus {
    pub state: StunState,
    /// The server that answered (`host:port` as resolved).
    pub server: Option<String>,
    /// Why discovery failed (also kept after a failed refresh).
    pub error: Option<String>,
    pub nat: NatKind,
    /// Milliseconds until the next attempt, while failed.
    pub retry_in_ms: Option<u64>,
}

/// One `[[peers]]` entry.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerStatus {
    /// Peer id (0 = host; shown as P1).
    pub id: u8,
    /// Name from the peer's `Hello` (empty until it arrives).
    pub name: String,
    /// The peer's address as seen from here.
    pub addr: String,
    pub connected: bool,
    /// Smoothed round-trip time, ms.
    pub rtt_ms: f64,
    /// Jitter of the peer's packets, ms.
    pub jitter_ms: f64,
    /// Recent loss of this side's packets, percent.
    pub loss_pct: f64,
}

/// The `[turned_away]` table: the last newcomer this host refused.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnedAway {
    /// Who knocked.
    pub addr: String,
    /// `full`, `run_in_progress`, `version_mismatch` or `other`.
    pub reason: String,
    /// Wall clock of the refusal, Unix ms.
    pub at_ms: u64,
}

/// `status.toml`, written by the game.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub format: u32,
    /// This game run's session id; requests for another session are ignored.
    pub session: String,
    /// Wall clock of this write, Unix ms.
    pub heartbeat_ms: u64,
    pub state: LobbyState,
    pub role: RoleChoice,
    pub name: String,
    /// The last `peer.toml` request number applied (0 = none yet).
    pub request: u64,
    /// The kind of the sticky error.
    pub error_kind: Option<ErrorKind>,
    /// Human-readable error, cleared by the next request or connection.
    pub error: Option<String>,
    /// With `error_kind = "refused"`: the reason key.
    pub refused: Option<String>,
    /// This player's connection code for internet play (needs STUN).
    pub code: Option<String>,
    /// Connection code with the LAN address (same machine or network).
    pub lan_code: Option<String>,
    pub local_addr: Option<String>,
    /// LAN address (`ip:port`) a joiner on the same network types.
    pub lan_addr: Option<String>,
    pub public_addr: Option<String>,
    /// Why the last `peer.toml` could not be read.
    pub peer_file_error: Option<String>,
    /// The connector of the current or last session. Codes, addresses and
    /// `[stun]` describe the UDP socket.
    pub connector: Connector,
    /// A lost partner may come back (Steam sessions).
    pub reconnecting: bool,
    pub stun: StunStatus,
    /// Peers, connected or connecting (Steam peers as `steam:<tag>`).
    pub peers: Vec<PeerStatus>,
    /// The last newcomer turned away (host).
    pub turned_away: Option<TurnedAway>,
    /// The Steam connector, as the game thread sees it.
    pub steam: Option<SteamStatus>,
}

/// Which connector a session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connector {
    /// Direct UDP: connection codes, LAN addresses.
    #[default]
    Udp,
    /// Steam lobbies and P2P packets.
    Steam,
}

/// The Steam lobby as the game thread sees it (`[steam] lobby`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteamLobbyState {
    #[default]
    None,
    /// `steam_lobby_create` sent, waiting for the lobby.
    Creating,
    /// In this player's own lobby (it hosts the session).
    Hosting,
    /// Looking a room code up.
    Searching,
    /// `steam_lobby_join_id` sent, waiting to be in.
    Joining,
    /// In another player's lobby (it joins the session).
    InLobby,
    /// The last lobby step failed; `error` says why.
    Failed,
}

/// The hidden listener for asynchronous Steam results (`[steam] listener`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerState {
    /// Switched off in the config, or not needed yet.
    #[default]
    Off,
    Active,
    /// Kept out of the way of the game's leaderboard screens.
    Suppressed,
    /// Could not be created; polling only.
    Failed,
}

/// One `[[steam.members]]` entry: a lobby member, by persona name only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SteamMember {
    /// Steam persona name, cleaned to plain text.
    pub name: String,
    pub me: bool,
    pub owner: bool,
    /// The co-op session's host (normally the owner).
    pub host: bool,
}

/// The `[steam]` table. Holds no Steam or lobby ids.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SteamStatus {
    /// Steam is running and logged in (`steam_initialised`).
    pub available: bool,
    pub lobby: SteamLobbyState,
    /// The page the co-op UI shows (`idle`, `hosting`, `linked`, ...).
    pub page: String,
    /// Who may join: `friends`, `invite_only` or `code`.
    pub visibility: String,
    pub max_members: u8,
    /// The short room code, while hosting with `visibility = "code"`.
    pub room_code: Option<String>,
    pub listener: ListenerState,
    /// Machine-readable kind of the last error, if any.
    pub error_kind: Option<String>,
    /// The last error, in words for the player.
    pub error: Option<String>,
    pub members: Vec<SteamMember>,
}

/// How current a status is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Heartbeat within [`STALE_AFTER`].
    Live,
    /// Heartbeat this long ago.
    Stale(Duration),
}

impl Status {
    /// Freshness at wall-clock `now_ms`; a heartbeat in the future is live.
    #[must_use]
    pub fn freshness(&self, now_ms: u64) -> Freshness {
        let age = Duration::from_millis(now_ms.saturating_sub(self.heartbeat_ms));
        if age <= STALE_AFTER {
            Freshness::Live
        } else {
            Freshness::Stale(age)
        }
    }

    /// The status without the fields that change on every write.
    #[must_use]
    pub fn stable(&self) -> Self {
        let mut s = self.clone();
        s.heartbeat_ms = 0;
        s.stun.retry_in_ms = None;
        for p in &mut s.peers {
            p.rtt_ms = 0.0;
            p.jitter_ms = 0.0;
            p.loss_pct = 0.0;
        }
        s
    }

    pub fn connected_peers(&self) -> impl Iterator<Item = &PeerStatus> {
        self.peers.iter().filter(|p| p.connected)
    }
}

/// `peer.toml`, written by another process on the same machine; the game
/// applies each request number once, for its own session only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerRequest {
    pub format: u32,
    /// Increases with every change.
    pub request: u64,
    /// The session id from the `status.toml` this request answers.
    pub session: String,
    /// Host or join (`none` = stay idle).
    pub role: RoleChoice,
    /// The partner's connection code (join: the host's; host: the joiner's).
    pub partner_code: Option<String>,
    /// Join only: the host's `ip:port` (wins over `partner_code`; a bare IP
    /// means [`DEFAULT_PORT`]).
    pub direct: Option<String>,
    /// This player's display name (empty = keep the game's default).
    pub name: String,
    /// Drop every link and stay idle.
    pub disconnect: bool,
    /// A Steam connector action for the game thread; when set, the rest of
    /// the request is ignored.
    pub steam: Option<SteamFileAction>,
    /// The room code for `steam = "join_code"`.
    pub steam_code: Option<String>,
}

/// `peer.toml`'s `steam` key: the co-op page's Steam actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteamFileAction {
    /// Host a friends-only lobby.
    Host,
    /// Host an invite-only lobby.
    HostInviteOnly,
    /// Host a lobby others join by room code.
    HostCode,
    /// Join by `steam_code`.
    JoinCode,
    Invite,
    Leave,
    Retry,
}

/// Where to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A connection code: address plus the nonce to pin.
    Code(ConnectionCode),
    /// A bare address (LAN, forwarded port); the nonce is learned on contact.
    Direct(SocketAddr),
}

impl Target {
    /// Address to punch toward.
    #[must_use]
    pub fn addr(self) -> SocketAddr {
        match self {
            Self::Code(c) => c.addr,
            Self::Direct(a) => a,
        }
    }

    /// Nonce to pin, if known.
    #[must_use]
    pub fn nonce(self) -> Option<u32> {
        match self {
            Self::Code(c) => Some(c.nonce),
            Self::Direct(_) => None,
        }
    }
}

/// What a request asks the game to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Drop links, stay idle.
    Idle,
    /// Host; with the partner's code also punch toward them.
    Host { partner: Option<ConnectionCode> },
    /// Join the host at the target.
    Join(Target),
}

fn non_empty(s: Option<&String>) -> Option<&str> {
    s.map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// Parse a direct address typed by a person: `ip:port`, `[v6]:port` or a bare
/// IP. Host names are not resolved.
pub fn parse_direct(input: &str) -> Result<SocketAddr, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("the address is empty".into());
    }
    let addr = s
        .parse::<SocketAddr>()
        .or_else(|_| {
            s.parse::<IpAddr>()
                .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
        })
        .map_err(|_| format!("'{s}' is not an address like 192.168.1.20:{DEFAULT_PORT}"))?;
    if addr.port() == 0 {
        return Err("port 0 is not a real port".into());
    }
    if addr.ip().is_unspecified() {
        return Err(format!("{} is not a reachable address", addr.ip()));
    }
    Ok(addr)
}

impl PeerRequest {
    /// A request with the current format number.
    #[must_use]
    pub fn new(request: u64, session: &str) -> Self {
        Self {
            format: FORMAT,
            request,
            session: session.to_owned(),
            ..Self::default()
        }
    }

    /// What this request asks for, or why it can't be done (for the player).
    pub fn plan(&self) -> Result<Plan, String> {
        if self.disconnect {
            return Ok(Plan::Idle);
        }
        let code = non_empty(self.partner_code.as_ref())
            .map(|c| ConnectionCode::parse(c).map_err(|e| format!("partner code: {e}")))
            .transpose()?;
        match self.role {
            RoleChoice::None => Ok(Plan::Idle),
            RoleChoice::Host => Ok(Plan::Host { partner: code }),
            RoleChoice::Join => {
                if let Some(d) = non_empty(self.direct.as_ref()) {
                    let addr = parse_direct(d).map_err(|e| format!("direct address: {e}"))?;
                    return Ok(Plan::Join(Target::Direct(addr)));
                }
                code.map(|c| Plan::Join(Target::Code(c)))
                    .ok_or_else(|| "to join, paste the host's code or type its address".into())
            }
        }
    }
}

/// Why a lobby file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The file exists but could not be read.
    Io(String),
    /// Not valid TOML for this format (or empty / cut short).
    Malformed(String),
    /// Written by a newer version with an incompatible format.
    NewerFormat(u32),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "can't read: {e}"),
            Self::Malformed(e) => write!(f, "malformed: {e}"),
            Self::NewerFormat(v) => write!(
                f,
                "written by a newer version (format {v}, this one reads {FORMAT}); update NDML"
            ),
        }
    }
}

impl std::error::Error for ReadError {}

fn check_format(format: u32) -> Result<(), ReadError> {
    match format {
        0 => Err(ReadError::Malformed(
            "no `format` key (empty or cut-short file)".into(),
        )),
        f if f > FORMAT => Err(ReadError::NewerFormat(f)),
        _ => Ok(()),
    }
}

fn malformed(e: &toml::de::Error) -> ReadError {
    ReadError::Malformed(e.message().trim().to_owned())
}

/// Parse `status.toml` text.
pub fn parse_status(text: &str) -> Result<Status, ReadError> {
    let s: Status = toml::from_str(text).map_err(|e| malformed(&e))?;
    check_format(s.format)?;
    Ok(s)
}

/// Parse `peer.toml` text.
pub fn parse_peer(text: &str) -> Result<PeerRequest, ReadError> {
    let p: PeerRequest = toml::from_str(text).map_err(|e| malformed(&e))?;
    check_format(p.format)?;
    Ok(p)
}

fn read_text(path: &Path) -> Result<Option<String>, ReadError> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ReadError::Io(e.to_string())),
    }
}

/// Read a status file; `Ok(None)` if there is none.
pub fn read_status(path: &Path) -> Result<Option<Status>, ReadError> {
    read_text(path)?.map(|t| parse_status(&t)).transpose()
}

/// Read a peer file; `Ok(None)` if there is none.
pub fn read_peer(path: &Path) -> Result<Option<PeerRequest>, ReadError> {
    read_text(path)?.map(|t| parse_peer(&t)).transpose()
}

fn to_toml<S: Serialize>(value: &S) -> io::Result<String> {
    toml::to_string(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Write a status file atomically.
pub fn write_status(path: &Path, status: &Status) -> io::Result<()> {
    write_atomic(path, status_text(status)?.as_bytes())
}

/// A status file's text.
pub fn status_text(status: &Status) -> io::Result<String> {
    to_toml(status)
}

/// Write a peer file atomically.
pub fn write_peer(path: &Path, request: &PeerRequest) -> io::Result<()> {
    write_atomic(path, to_toml(request)?.as_bytes())
}

/// Replace `path` with `bytes` atomically (a sibling temp file renamed over
/// it; renames refused by a reader on Windows are retried).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    let mut last = None;
    for attempt in 0..20u32 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(u64::from(attempt.min(5)) + 1));
            }
            Err(e) => {
                last = Some(e);
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/ndml-net-lobby-files")
            .join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample_status() -> Status {
        Status {
            format: FORMAT,
            session: session_id(0xDEAD_BEEF),
            heartbeat_ms: 1_700_000_000_000,
            state: LobbyState::Connected,
            role: RoleChoice::Host,
            name: "Pilot \"Ace\"".into(),
            request: 3,
            error_kind: Some(ErrorKind::HandshakeTimeout),
            error: Some("no answer".into()),
            refused: None,
            code: Some("2ZR00-00HWR-FBEDW-C85S8".into()),
            lan_code: Some("2ZR00-00HWR-FBEDW-C85S9".into()),
            local_addr: Some("0.0.0.0:7777".into()),
            lan_addr: Some("192.168.1.5:7777".into()),
            public_addr: Some("203.0.113.5:40000".into()),
            peer_file_error: None,
            connector: Connector::Udp,
            reconnecting: false,
            stun: StunStatus {
                state: StunState::Ok,
                server: Some("192.0.2.10:19302".into()),
                error: None,
                nat: NatKind::Consistent,
                retry_in_ms: None,
            },
            peers: vec![PeerStatus {
                id: 1,
                name: "Wing".into(),
                addr: "198.51.100.7:5000".into(),
                connected: true,
                rtt_ms: 42.5,
                jitter_ms: 3.25,
                loss_pct: 0.5,
            }],
            turned_away: Some(TurnedAway {
                addr: "198.51.100.9:5000".into(),
                reason: "full".into(),
                at_ms: 1_700_000_000_500,
            }),
            steam: None,
        }
    }

    #[test]
    fn a_steam_session_round_trips_without_ids() {
        let mut s = sample_status();
        s.connector = Connector::Steam;
        s.reconnecting = true;
        s.peers[0].addr =
            crate::steam::display_addr(&crate::steam::steam_addr(0x0110_0001_0000_0042));
        s.steam = Some(SteamStatus {
            available: true,
            lobby: SteamLobbyState::Hosting,
            page: "linked".into(),
            visibility: "friends".into(),
            max_members: 4,
            room_code: Some("K7QX2".into()),
            listener: ListenerState::Active,
            error_kind: None,
            error: None,
            members: vec![
                SteamMember {
                    name: "Ace".into(),
                    me: true,
                    owner: true,
                    host: true,
                },
                SteamMember {
                    name: "Wing \"2\"".into(),
                    ..SteamMember::default()
                },
            ],
        });
        let text = toml::to_string(&s).unwrap();
        assert!(text.contains("connector = \"steam\""), "{text}");
        assert!(text.contains("[[steam.members]]"), "{text}");
        assert!(text.contains("addr = \"steam:"), "{text}");
        assert!(!text.contains("fd4e"), "no synthetic address: {text}");
        assert_eq!(parse_status(&text).unwrap(), s);
        // A file with only `format` and `state` reads (UDP, no Steam).
        let old = parse_status("format = 1\nstate = \"ready\"").unwrap();
        assert_eq!(old.connector, Connector::Udp);
        assert!(old.steam.is_none() && !old.reconnecting);
    }

    #[test]
    fn status_round_trips_through_toml() {
        let s = sample_status();
        let text = toml::to_string(&s).unwrap();
        assert!(text.contains("state = \"connected\""), "{text}");
        assert!(text.contains("[[peers]]"), "{text}");
        assert_eq!(parse_status(&text).unwrap(), s);
        // A minimal status (all optional fields absent) round-trips too.
        let min = Status {
            format: FORMAT,
            ..Status::default()
        };
        assert_eq!(parse_status(&toml::to_string(&min).unwrap()).unwrap(), min);
    }

    #[test]
    fn peer_request_round_trips_and_plans() {
        let code = ConnectionCode {
            addr: "203.0.113.5:40000".parse().unwrap(),
            nonce: 77,
        };
        let mut p = PeerRequest::new(4, "0000beef");
        p.role = RoleChoice::Join;
        p.partner_code = Some(code.encode().to_lowercase());
        p.name = "Wing".into();
        let text = toml::to_string(&p).unwrap();
        assert_eq!(parse_peer(&text).unwrap(), p);
        assert_eq!(p.plan(), Ok(Plan::Join(Target::Code(code))));
        // Direct wins for a joiner; a bare IP gets the default port.
        p.direct = Some(" 192.168.1.20 ".into());
        let want: SocketAddr = "192.168.1.20:7777".parse().unwrap();
        assert_eq!(p.plan(), Ok(Plan::Join(Target::Direct(want))));
        // A host with the joiner's code punches too; without, it just listens.
        p.role = RoleChoice::Host;
        assert_eq!(
            p.plan(),
            Ok(Plan::Host {
                partner: Some(code)
            })
        );
        p.partner_code = Some(String::new());
        assert_eq!(p.plan(), Ok(Plan::Host { partner: None }));
        p.disconnect = true;
        assert_eq!(p.plan(), Ok(Plan::Idle));
    }

    #[test]
    fn bad_requests_say_what_is_wrong() {
        let mut p = PeerRequest::new(1, "s");
        p.role = RoleChoice::Join;
        assert!(p.plan().unwrap_err().contains("paste the host's code"));
        p.partner_code = Some("2ZR00-00HWR-FBEDW-C85S".into());
        assert!(
            p.plan()
                .unwrap_err()
                .starts_with("partner code: a code has 20")
        );
        p.partner_code = None;
        p.direct = Some("my-pc:7777".into());
        assert!(p.plan().unwrap_err().contains("not an address"));
        assert!(parse_direct("10.0.0.1:0").unwrap_err().contains("port 0"));
        assert!(parse_direct("0.0.0.0:7777").is_err());
        assert!(parse_direct("").is_err());
        assert_eq!(
            parse_direct("[::1]:9000").unwrap(),
            "[::1]:9000".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn malformed_and_partial_files_are_errors_not_panics() {
        let text = toml::to_string(&sample_status()).unwrap();
        // Empty and cut-short files are rejected or read as partial.
        assert!(matches!(parse_status(""), Err(ReadError::Malformed(_))));
        for cut in [10, text.len() / 3, text.len() / 2, text.len() - 3] {
            let r = parse_status(&text[..cut]);
            let rejected = matches!(r, Err(ReadError::Malformed(_)));
            let partial = r.as_ref().is_ok_and(|s| *s != sample_status());
            assert!(rejected || partial, "cut at {cut}: {r:?}");
        }
        assert!(matches!(
            parse_status("format = \"one\""),
            Err(ReadError::Malformed(_))
        ));
        assert!(matches!(
            parse_status("format = 1\nstate = \"dancing\""),
            Err(ReadError::Malformed(_))
        ));
        assert_eq!(
            parse_peer("format = 9\nrequest = 1"),
            Err(ReadError::NewerFormat(9))
        );
        // Unknown keys from a newer writer are fine.
        let p = parse_peer("format = 1\nrequest = 2\nfuture_key = true").unwrap();
        assert_eq!(p.request, 2);
        assert!(ReadError::NewerFormat(9).to_string().contains("newer"));
    }

    #[test]
    fn atomic_writes_replace_and_leave_no_temp_files() {
        let dir = scratch("atomic").join("nested");
        let path = dir.join(STATUS_FILE);
        assert_eq!(read_status(&path), Ok(None));
        let mut s = sample_status();
        write_status(&path, &s).unwrap();
        s.request = 9;
        write_status(&path, &s).unwrap();
        assert_eq!(read_status(&path).unwrap(), Some(s));
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![STATUS_FILE.to_owned()]);
        std::fs::write(dir.join(PEER_FILE), "format = 1\nrole = [").unwrap();
        assert!(matches!(
            read_peer(&dir.join(PEER_FILE)),
            Err(ReadError::Malformed(_))
        ));
    }

    /// A reader polling while a writer replaces the file never sees a torn one.
    #[test]
    fn readers_never_see_a_partial_write() {
        let dir = scratch("concurrent");
        let path = dir.join(STATUS_FILE);
        let a = sample_status();
        let mut b = sample_status();
        b.peers.clear();
        b.state = LobbyState::Waiting;
        write_status(&path, &a).unwrap();
        let writer_path = path.clone();
        let (wa, wb) = (a.clone(), b.clone());
        let writer = std::thread::spawn(move || {
            for i in 0..300 {
                write_status(&writer_path, if i % 2 == 0 { &wb } else { &wa }).unwrap();
            }
        });
        let mut reads = 0;
        while !writer.is_finished() || reads < 300 {
            let got = read_status(&path).unwrap().expect("file is always there");
            assert!(got == a || got == b);
            reads += 1;
        }
        writer.join().unwrap();
    }

    #[test]
    fn freshness_names_and_ids() {
        let s = Status {
            heartbeat_ms: 10_000,
            ..Status::default()
        };
        assert_eq!(s.freshness(12_000), Freshness::Live);
        assert_eq!(s.freshness(9_000), Freshness::Live, "clock stepped back");
        assert_eq!(
            s.freshness(20_000),
            Freshness::Stale(Duration::from_secs(10))
        );
        assert_eq!(session_id(0xBEEF), "0000beef");
        assert_eq!(clean_name("  Ace\u{7}  "), Some("Ace".into()));
        assert_eq!(clean_name(" \n "), None);
        assert_eq!(clean_name(&"x".repeat(40)).unwrap().len(), MAX_NAME_CHARS);
        assert_eq!(lobby_dir(Path::new("ndml")), Path::new("ndml").join("coop"));
        let mut st = sample_status();
        let stable = st.stable();
        st.peers[0].rtt_ms = 99.0;
        st.heartbeat_ms += 500;
        assert_eq!(st.stable(), stable);
        assert_eq!(st.connected_peers().count(), 1);
        assert_eq!(LobbyState::Waiting.label(), "WAITING");
    }
}
