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

//! [`LobbyAgent`]: the game side of the lobby, driven by `poll(now)`.
//!
//! It owns the socket for the whole session (the connection code belongs to
//! its NAT mapping) and runs public endpoint discovery ([`Discovery`]; with
//! [`LobbyConfig::discover_at_start`] off, only once the code route is used),
//! applies requests (in-game, or from `peer.toml`), runs the [`Endpoint`] for
//! the chosen role, checks `Hello`s against [`VersionCheck`], and publishes
//! the status (`status.toml` when [`LobbyConfig::files`] is on). Nothing here
//! blocks; the STUN name lookup runs on a helper thread.
//!
//! In a Steam session ([`LobbyAgent::steam_request`]) the game thread's lobby
//! decides the roles and the host admits only lobby members. Steam sessions
//! rejoin after a lost link ([`LobbyConfig::steam_rejoin`]); UDP sessions do
//! for [`LobbyConfig::udp_rejoin`].

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use super::files::{
    self, Connector, ErrorKind, LobbyState, NatKind, PEER_FILE, PeerStatus, Plan, RoleChoice,
    STATUS_FILE, Status, SteamFileAction, SteamStatus, StunState, StunStatus, TurnedAway,
};
use crate::code::ConnectionCode;
use crate::endpoint::{
    DisconnectReason, Endpoint, EndpointConfig, Event, RefuseReason, Rejoin, Role,
};
use crate::msg::{Hello, Message, PeerId};
use crate::steam::{display_addr, steam_addr};
use crate::stun::{self, Discovery, NatBehavior};
use crate::transport::{Transport, UdpTransport};

/// A Steam session request from the game thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamRequest {
    /// Host a Steam session, admitting only these Steam ids; resent when the
    /// lobby members change.
    Host { allow: Vec<u64> },
    /// Join the session hosted by this Steam id (the lobby owner).
    Join { host: u64 },
    /// End the Steam session (goodbyes) and stay idle.
    Leave,
    /// This player's Steam name, sent in `Hello` in Steam sessions; applies
    /// from the next session started.
    Name(String),
}

/// A `peer.toml` Steam action waiting for the game thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamFileRequest {
    pub action: SteamFileAction,
    /// The room code, for [`SteamFileAction::JoinCode`].
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SteamSession {
    Host,
    Join { host: u64 },
}

/// Most STUN datagrams kept between two polls (the rest are dropped).
const STUN_INBOX: usize = 16;

/// Which `Hello`s are compatible, and whether the agent sends its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionCheck {
    /// Co-op mod version; must match exactly.
    pub mod_version: u32,
    /// Game build; must match exactly.
    pub build: String,
    /// Send `Hello` to each peer on connect.
    pub send_hello: bool,
}

impl VersionCheck {
    /// Whether a peer's `Hello` is compatible with this side's.
    #[must_use]
    pub fn accepts(&self, h: &Hello) -> bool {
        h.mod_version == self.mod_version && h.build == self.build
    }
}

/// A lobby choice made in the game's panel; same meaning as the matching
/// `peer.toml` fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LobbyRequest {
    /// Host or join (`None` = stay idle).
    pub role: RoleChoice,
    /// The partner's connection code (join: the host's; host: the joiner's).
    pub partner_code: Option<String>,
    /// Join only: the host's `ip:port`; wins over `partner_code`.
    pub direct: Option<String>,
    /// This player's display name (empty = keep the current one).
    pub name: String,
    /// Drop every link and stay idle.
    pub disconnect: bool,
}

/// A name lookup for a STUN server (`host:port` to its addresses).
pub type StunResolve = fn(&str) -> io::Result<Vec<SocketAddr>>;

/// The system's lookup (`getaddrinfo`; it may block for seconds).
pub fn system_resolve(server: &str) -> io::Result<Vec<SocketAddr>> {
    server.to_socket_addrs().map(Iterator::collect)
}

/// A STUN name lookup slower than this counts as failed (a late answer is
/// still taken).
pub const LOOKUP_SLOW: Duration = Duration::from_secs(5);

/// The first wait after a failed lookup; it doubles up to
/// [`LobbyConfig::stun_retry`].
pub const LOOKUP_BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// A lookup unanswered for this long is given up; until then a retry waits on
/// it, so a hung lookup never piles up threads.
const LOOKUP_ABANDON: Duration = Duration::from_secs(60);

/// A STUN name lookup on its helper thread.
#[derive(Debug)]
struct StunLookup {
    rx: mpsc::Receiver<Vec<SocketAddr>>,
    since: Duration,
}

/// Where the STUN name lookup stands at a poll.
enum Lookup {
    Ready,
    Pending,
    Failed(String),
}

/// Agent settings. [`LobbyConfig::new`] gives the defaults.
#[derive(Debug, Clone)]
pub struct LobbyConfig {
    /// The lobby directory (`<game>/ndml/coop`, see [`files::lobby_dir`]).
    pub dir: PathBuf,
    /// This run's nonce (goes into the codes; random per launch).
    pub nonce: u32,
    /// Default player name (codes and LAN sessions).
    pub name: String,
    /// `Some` when `name` is an automatic pick: the second pick, taken by a
    /// joining game whose host has the same name.
    pub name_alt: Option<String>,
    /// STUN servers (`host:port`); empty = no discovery, LAN only.
    pub stun_servers: Vec<String>,
    /// The name lookup for [`LobbyConfig::stun_servers`], run on a helper
    /// thread ([`system_resolve`] by default; IP literals need none).
    pub stun_resolve: StunResolve,
    /// Follow `peer.toml` and write `status.toml`.
    pub files: bool,
    /// Do the file work on a thread of its own ([`crate::fileio`]); `false`:
    /// on the caller's thread (tests on a synthetic clock).
    pub file_thread: bool,
    /// Find the public address from the start; `false`: only once the
    /// connection-code route is used.
    pub discover_at_start: bool,
    /// Most players including the host (2 for co-op).
    pub max_peers: u8,
    /// LAN IP for the LAN code; `None`: the bound or the outgoing interface's.
    pub lan_ip: Option<IpAddr>,
    /// Version policy; `None` accepts everyone and sends no `Hello`.
    pub version: Option<VersionCheck>,
    /// Wall clock at the agent's time zero (heartbeats are this plus `now`).
    pub wall_clock_at_epoch: SystemTime,
    /// How long a handshake may take.
    pub handshake_timeout: Duration,
    /// Silence after which a link counts as lost.
    pub link_timeout: Duration,
    /// Wait after a failed discovery before trying again.
    pub stun_retry: Duration,
    /// Rediscover this often while not linked.
    pub stun_refresh: Duration,
    /// How often `peer.toml` is read.
    pub peer_poll: Duration,
    /// Longest gap between status writes.
    pub heartbeat: Duration,
    /// How long a Steam session waits for a lost partner.
    pub steam_rejoin: Rejoin,
    /// How long a UDP session waits for a lost partner.
    pub udp_rejoin: Rejoin,
}

impl LobbyConfig {
    /// Defaults for `dir` and `nonce`.
    #[must_use]
    pub fn new(dir: PathBuf, nonce: u32) -> Self {
        Self {
            dir,
            nonce,
            name: "Pilot".into(),
            name_alt: None,
            stun_servers: stun::DEFAULT_SERVERS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            stun_resolve: system_resolve,
            discover_at_start: true,
            files: true,
            file_thread: true,
            max_peers: 2,
            lan_ip: None,
            version: None,
            wall_clock_at_epoch: SystemTime::now(),
            handshake_timeout: Duration::from_secs(30),
            link_timeout: Duration::from_secs(10),
            stun_retry: Duration::from_secs(15),
            stun_refresh: Duration::from_secs(25),
            peer_poll: Duration::from_millis(200),
            heartbeat: files::HEARTBEAT_INTERVAL,
            steam_rejoin: Rejoin::Always,
            udp_rejoin: Rejoin::Off,
        }
    }
}

/// A transport that sets STUN answers aside for the agent.
#[derive(Debug)]
pub struct LobbyTransport<T> {
    inner: T,
    stun: VecDeque<(SocketAddr, Vec<u8>)>,
}

impl<T: Transport> LobbyTransport<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            stun: VecDeque::new(),
        }
    }

    /// The wrapped transport.
    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    fn take_stun(&mut self) -> VecDeque<(SocketAddr, Vec<u8>)> {
        std::mem::take(&mut self.stun)
    }
}

impl<T: Transport> Transport for LobbyTransport<T> {
    fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        self.inner.send_to(now, to, data)
    }

    fn release_held(&mut self, now: Duration) -> io::Result<()> {
        self.inner.release_held(now)
    }

    fn recv_from(
        &mut self,
        now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            match self.inner.recv_from(now, buf)? {
                Some((n, from)) if stun::looks_like_stun(&buf[..n]) => {
                    if self.stun.len() < STUN_INBOX {
                        self.stun.push_back((from, buf[..n].to_vec()));
                    }
                }
                other => return Ok(other),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// The endpoint type a lobby agent drives.
pub type LobbyEndpoint<T> = Endpoint<LobbyTransport<T>>;

#[derive(Debug)]
enum Link<T> {
    Idle(LobbyTransport<T>),
    Active(Box<LobbyEndpoint<T>>),
    /// Only while the transport is moved.
    Gone,
}

#[derive(Debug, Clone)]
struct Failure {
    kind: ErrorKind,
    message: String,
    refused: Option<RefuseReason>,
}

/// Bind the game's UDP socket on `port`, or on any free port if taken.
pub fn bind_udp(port: u16) -> io::Result<UdpTransport> {
    UdpTransport::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
        .or_else(|_| UdpTransport::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))))
}

/// The IP of the interface that routes to the internet, from an unsent UDP
/// `connect` (no packet leaves the machine).
#[must_use]
pub fn detect_lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // TEST-NET-3: never contacted, only used to pick the outgoing interface.
    s.connect((Ipv4Addr::new(203, 0, 113, 1), 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// See the module docs.
// The flags belong to different concerns.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct LobbyAgent<T: Transport> {
    cfg: LobbyConfig,
    session: String,
    link: Link<T>,
    local_addr: Option<SocketAddr>,
    lan_addr: Option<SocketAddr>,
    buf: Vec<u8>,
    stun_resolved: Vec<SocketAddr>,
    /// The STUN name lookup in flight on its helper thread.
    stun_lookup: Option<StunLookup>,
    /// Lookups started so far.
    #[cfg(test)]
    stun_lookups_asked: u32,
    /// The wait after the next failed lookup.
    stun_backoff: Duration,
    discovery: Option<Discovery>,
    stun_next: Option<Duration>,
    stun_ever_finished: bool,
    /// Discovery may run (from the start, or since the code route was used).
    discovery_wanted: bool,
    public: Option<SocketAddr>,
    public_from: Option<SocketAddr>,
    nat: NatBehavior,
    stun_error: Option<String>,
    role: RoleChoice,
    plan: Plan,
    host_punching: bool,
    /// The name sent in `Hello`.
    name: String,
    /// The name of codes and LAN sessions.
    codes_name: String,
    /// The two automatic picks, while `codes_name` is one of them.
    auto_names: Option<(String, String)>,
    /// This player's Steam name ([`SteamRequest::Name`]).
    steam_name: Option<String>,
    applied: u64,
    failure: Option<Failure>,
    peer_addrs: BTreeMap<PeerId, SocketAddr>,
    peer_names: BTreeMap<PeerId, String>,
    turned_away: Option<TurnedAway>,
    events: VecDeque<Event>,
    go_idle: bool,
    refusing_peer: Option<PeerId>,
    /// The file service and its watch on `peer.toml` (`None`: files off).
    io: Option<(crate::fileio::FileService, crate::fileio::WatchId)>,
    next_peer_poll: Duration,
    last_peer_text: Option<String>,
    peer_file_error: Option<String>,
    status: Status,
    last_write: Option<Duration>,
    status_version: u64,
    file_error: Option<String>,
    socket_error: Option<String>,
    stopped: bool,
    steam: Option<SteamSession>,
    connector: Connector,
    reconnecting_since: Option<Duration>,
    /// UDP host: the slot it punches from while a lost partner may come back.
    rejoin_peer: Option<PeerId>,
    steam_status: Option<SteamStatus>,
    steam_actions: Vec<SteamFileRequest>,
    /// The game holds the link: a lost partner is waited for without limit.
    hold: bool,
}

impl LobbyAgent<UdpTransport> {
    /// Bind UDP `port` (see [`bind_udp`]) and make an agent on it.
    pub fn bind(cfg: LobbyConfig, port: u16) -> io::Result<Self> {
        Ok(Self::new(bind_udp(port)?, cfg))
    }
}

impl<T: Transport> LobbyAgent<T> {
    /// An agent on `transport`.
    pub fn new(transport: T, cfg: LobbyConfig) -> Self {
        let local_addr = transport.local_addr().ok();
        let lan_addr = local_addr.map(|a| {
            let ip = cfg.lan_ip.unwrap_or_else(|| {
                if a.ip().is_unspecified() {
                    detect_lan_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
                } else {
                    a.ip()
                }
            });
            SocketAddr::new(ip, a.port())
        });
        let (io, file_error) = lobby_io(&cfg);
        let name = files::clean_name(&cfg.name).unwrap_or_else(|| "Pilot".into());
        let auto_names = cfg
            .name_alt
            .as_deref()
            .and_then(files::clean_name)
            .filter(|alt| !same_name(alt, &name))
            .map(|alt| (name.clone(), alt));
        Self {
            session: files::session_id(cfg.nonce),
            link: Link::Idle(LobbyTransport::new(transport)),
            local_addr,
            lan_addr,
            buf: vec![0u8; 2048],
            stun_resolved: Vec::new(),
            stun_lookup: None,
            #[cfg(test)]
            stun_lookups_asked: 0,
            stun_backoff: LOOKUP_BACKOFF_FIRST,
            discovery: None,
            stun_next: None,
            stun_ever_finished: false,
            discovery_wanted: cfg.discover_at_start,
            public: None,
            public_from: None,
            nat: NatBehavior::Unknown,
            stun_error: None,
            role: RoleChoice::None,
            plan: Plan::Idle,
            host_punching: false,
            codes_name: name.clone(),
            name,
            auto_names,
            steam_name: None,
            applied: 0,
            failure: None,
            peer_addrs: BTreeMap::new(),
            peer_names: BTreeMap::new(),
            turned_away: None,
            events: VecDeque::new(),
            go_idle: false,
            refusing_peer: None,
            io,
            next_peer_poll: Duration::ZERO,
            last_peer_text: None,
            peer_file_error: None,
            status: Status::default(),
            last_write: None,
            status_version: 0,
            file_error,
            socket_error: None,
            stopped: false,
            steam: None,
            connector: Connector::Udp,
            reconnecting_since: None,
            rejoin_peer: None,
            steam_status: None,
            steam_actions: Vec::new(),
            hold: false,
            cfg,
        }
    }

    /// Do everything that is due at `now` (the endpoint's clock).
    pub fn poll(&mut self, now: Duration) {
        if self.stopped {
            return;
        }
        self.pump(now);
        self.udp_rejoin_over(now);
        self.drive_stun(now);
        // Threaded: the newest read is taken each poll; inline: on `peer_poll`.
        let threaded = self.io.as_ref().is_some_and(|(s, _)| s.threaded());
        if self.io.is_some() && (threaded || now >= self.next_peer_poll) {
            self.next_peer_poll = now + self.cfg.peer_poll;
            self.poll_peer_file(now);
        }
        self.publish(now);
    }

    /// Next connection event, in order.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// The endpoint, while a role is active.
    pub fn endpoint_mut(&mut self) -> Option<&mut LobbyEndpoint<T>> {
        match &mut self.link {
            Link::Active(ep) => Some(ep),
            _ => None,
        }
    }

    /// The endpoint, while a role is active.
    #[must_use]
    pub fn endpoint(&self) -> Option<&LobbyEndpoint<T>> {
        match &self.link {
            Link::Active(ep) => Some(ep),
            _ => None,
        }
    }

    /// The wrapped transport.
    pub fn transport_mut(&mut self) -> Option<&mut T> {
        self.lobby_transport().map(LobbyTransport::inner_mut)
    }

    fn lobby_transport(&mut self) -> Option<&mut LobbyTransport<T>> {
        match &mut self.link {
            Link::Idle(t) => Some(t),
            Link::Active(ep) => Some(ep.transport_mut()),
            Link::Gone => None,
        }
    }

    /// STUN name lookups asked so far ([`Self::resolve_stun_servers`]).
    #[cfg(test)]
    pub(crate) fn stun_lookups_asked(&self) -> u32 {
        self.stun_lookups_asked
    }

    /// The status as last written (or about to be).
    #[must_use]
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Increases whenever the status changes beyond heartbeat and link numbers.
    #[must_use]
    pub fn status_version(&self) -> u64 {
        self.status_version
    }

    /// The lobby state now.
    #[must_use]
    pub fn state(&self) -> LobbyState {
        if self.stopped {
            return LobbyState::Stopped;
        }
        match &self.link {
            Link::Active(ep) if !ep.peers().is_empty() => LobbyState::Connected,
            Link::Active(ep) if ep.role() == Role::Client || self.host_punching => {
                LobbyState::Connecting
            }
            Link::Active(_) => LobbyState::Waiting,
            Link::Idle(_) | Link::Gone if self.failure.is_some() => LobbyState::Failed,
            Link::Idle(_) | Link::Gone
                if self.connector == Connector::Udp
                    && self.discovery.is_some()
                    && self.public.is_none()
                    && !self.stun_ever_finished =>
            {
                LobbyState::Discovering
            }
            Link::Idle(_) | Link::Gone => LobbyState::Ready,
        }
    }

    /// The role in effect (`None` while idle).
    #[must_use]
    pub fn role(&self) -> Option<Role> {
        self.endpoint().map(Endpoint::role)
    }

    /// The player's name.
    #[must_use]
    pub fn player_name(&self) -> &str {
        &self.name
    }

    /// This run's session id.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The internet code, once discovery succeeded.
    #[must_use]
    pub fn code(&self) -> Option<ConnectionCode> {
        self.public.map(|addr| ConnectionCode {
            addr,
            nonce: self.cfg.nonce,
        })
    }

    /// Start finding the public address now (the code page was opened).
    pub fn want_discovery(&mut self) {
        self.discovery_wanted = true;
    }

    /// Whether discovery may run (asked for, or on from the start).
    #[must_use]
    pub fn discovery_wanted(&self) -> bool {
        self.discovery_wanted
    }

    /// The LAN code.
    #[must_use]
    pub fn lan_code(&self) -> Option<ConnectionCode> {
        self.lan_addr.map(|addr| ConnectionCode {
            addr,
            nonce: self.cfg.nonce,
        })
    }

    /// The socket's bound address.
    #[must_use]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// The last error writing the status file, if the last write failed.
    #[must_use]
    pub fn last_file_error(&self) -> Option<&str> {
        self.file_error.as_deref()
    }

    /// The last socket error, if any.
    #[must_use]
    pub fn last_socket_error(&self) -> Option<&str> {
        self.socket_error.as_deref()
    }

    /// Start, update or end a Steam session.
    pub fn steam_request(&mut self, now: Duration, req: &SteamRequest) {
        if self.stopped {
            return;
        }
        match *req {
            SteamRequest::Host { ref allow } => {
                if self.steam != Some(SteamSession::Host) || self.role() != Some(Role::Host) {
                    self.idle(now);
                    self.start(Role::Host);
                    self.begin_steam(SteamSession::Host, RoleChoice::Host);
                    let rejoin = self.steam_rejoin();
                    if let Link::Active(ep) = &mut self.link {
                        ep.set_rejoin(rejoin);
                    }
                }
                let allow: BTreeSet<SocketAddr> = allow.iter().map(|&id| steam_addr(id)).collect();
                if let Link::Active(ep) = &mut self.link {
                    ep.set_allowlist(Some(allow));
                }
            }
            SteamRequest::Join { host } => {
                let session = SteamSession::Join { host };
                let busy = matches!(self.state(), LobbyState::Connected | LobbyState::Connecting);
                if self.steam == Some(session) && busy {
                    return; // already on it
                }
                self.idle(now);
                self.start(Role::Client);
                self.begin_steam(session, RoleChoice::Join);
                if let Link::Active(ep) = &mut self.link {
                    let _ = ep.connect(now, steam_addr(host), None);
                }
            }
            SteamRequest::Leave => {
                if self.steam.is_some() {
                    self.idle(now);
                }
                self.role = RoleChoice::None;
                self.failure = None;
                self.name.clone_from(&self.codes_name);
            }
            SteamRequest::Name(ref n) => {
                self.steam_name = files::clean_name(n);
            }
        }
    }

    /// A joining game on its first automatic pick whose host has that name:
    /// take the second pick and say `Hello` again (codes and LAN only).
    fn avoid_host_name(&mut self, from: PeerId, host_name: &str) {
        let Some((first, second)) = self.auto_names.clone() else {
            return;
        };
        if self.steam.is_some()
            || !from.is_host()
            || self.role() != Some(Role::Client)
            || !same_name(&self.codes_name, &first)
            || !same_name(host_name, &first)
        {
            return;
        }
        self.codes_name.clone_from(&second);
        self.name = second;
        self.send_hello(from);
    }

    /// This side's `Hello` to `peer`, when the version policy sends one.
    fn send_hello(&mut self, peer: PeerId) {
        if let (Some(v), Link::Active(ep)) = (&self.cfg.version, &mut self.link)
            && v.send_hello
        {
            let hello = Message::Hello(Hello {
                mod_version: v.mod_version,
                build: v.build.clone(),
                name: self.name.clone(),
            });
            let _ = ep.send(peer, &hello);
        }
    }

    fn begin_steam(&mut self, session: SteamSession, role: RoleChoice) {
        // The Steam name; without one yet, the codes and LAN name.
        self.name = self
            .steam_name
            .clone()
            .unwrap_or_else(|| self.codes_name.clone());
        self.steam = Some(session);
        self.connector = Connector::Steam;
        self.role = role;
        self.failure = None;
        self.reconnecting_since = None;
    }

    /// The game thread's view of the Steam connector (`[steam]` in the status).
    pub fn set_steam_status(&mut self, status: Option<SteamStatus>) {
        self.steam_status = status;
    }

    /// Steam actions from `peer.toml`, oldest first, for the game thread.
    pub fn take_steam_actions(&mut self) -> Vec<SteamFileRequest> {
        std::mem::take(&mut self.steam_actions)
    }

    /// Whether a Steam session is active (hosting, or joined to a host).
    #[must_use]
    pub fn steam_active(&self) -> bool {
        self.steam.is_some()
    }

    /// Hold the link: a lost partner is waited for without limit, whatever
    /// the rejoin windows; released, the windows apply again from the loss.
    pub fn hold_link(&mut self, on: bool) {
        if self.hold == on {
            return;
        }
        self.hold = on;
        let rejoin = if self.steam.is_some() {
            self.steam_rejoin()
        } else {
            self.udp_rejoin()
        };
        if let Link::Active(ep) = &mut self.link {
            ep.set_rejoin(rejoin);
        }
    }

    /// Whether the link is held ([`Self::hold_link`]).
    #[must_use]
    pub fn link_held(&self) -> bool {
        self.hold
    }

    fn steam_rejoin(&self) -> Rejoin {
        if self.hold {
            Rejoin::Always
        } else {
            self.cfg.steam_rejoin
        }
    }

    fn udp_rejoin(&self) -> Rejoin {
        if self.hold {
            Rejoin::Always
        } else {
            self.cfg.udp_rejoin
        }
    }

    /// Whether a lost partner may come back: a host holding a slot, or a
    /// joiner still trying to reach its host.
    #[must_use]
    pub fn reconnecting(&self) -> bool {
        match (&self.link, self.steam) {
            (Link::Active(ep), Some(SteamSession::Host)) => !ep.reserved().is_empty(),
            (Link::Active(_), Some(SteamSession::Join { .. }) | None) => {
                self.reconnecting_since.is_some()
            }
            _ => false,
        }
    }

    /// Hand on at once what the transport holds back (after [`Self::stop`]).
    pub fn release_held(&mut self, now: Duration) {
        let released = match &mut self.link {
            Link::Idle(t) => t.release_held(now),
            Link::Active(ep) => ep.transport_mut().release_held(now),
            Link::Gone => Ok(()),
        };
        if let Err(e) = released {
            self.socket_error = Some(e.to_string());
        }
    }

    /// Shut down: goodbyes, `state = "stopped"`; later polls do nothing.
    pub fn stop(&mut self, now: Duration) {
        self.idle(now);
        self.stopped = true;
        // A lookup still in flight answers into nothing.
        self.stun_lookup = None;
        self.write_status(now);
    }

    fn pump(&mut self, now: Duration) {
        let mut buf = std::mem::take(&mut self.buf);
        let result = match &mut self.link {
            Link::Idle(t) => loop {
                // Game traffic while idle is dropped; it repeats once punched back.
                match t.recv_from(now, &mut buf) {
                    Ok(Some(_)) => {}
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e),
                }
            },
            Link::Active(ep) => ep.poll(now),
            Link::Gone => Ok(()),
        };
        self.buf = buf;
        if let Err(e) = result {
            self.socket_error = Some(e.to_string());
        }
        let answers = self
            .lobby_transport()
            .map_or_default(LobbyTransport::take_stun);
        if let Some(d) = &mut self.discovery {
            for (from, bytes) in answers {
                d.on_datagram(from, &bytes);
            }
        }
        self.after_discovery(now);
        self.drain_endpoint(now);
    }

    fn drain_endpoint(&mut self, now: Duration) {
        loop {
            let batch: Vec<Event> = match &mut self.link {
                Link::Active(ep) => std::iter::from_fn(|| ep.next_event()).collect(),
                _ => Vec::new(),
            };
            if batch.is_empty() {
                break;
            }
            for ev in batch {
                self.on_event(now, ev);
            }
        }
        if self.go_idle {
            self.idle(now);
        }
    }

    fn on_event(&mut self, now: Duration, ev: Event) {
        match &ev {
            Event::Connected { peer, addr } => {
                self.peer_addrs.insert(*peer, *addr);
                self.failure = None;
                self.reconnecting_since = None;
                self.rejoin_peer = None;
                if self.plan_partner_addr() == Some(*addr) {
                    self.host_punching = false;
                }
                self.send_hello(*peer);
            }
            Event::Message {
                from,
                msg: Message::Hello(h),
            } => {
                if let Some(n) = files::clean_name(&h.name) {
                    self.peer_names.insert(*from, n);
                }
                let compatible = self.cfg.version.as_ref().is_none_or(|v| v.accepts(h));
                if !compatible {
                    if let Link::Active(ep) = &mut self.link {
                        self.refusing_peer = Some(*from);
                        ep.refuse(now, *from, RefuseReason::VersionMismatch);
                    }
                    return; // the refusal's Disconnected follows
                }
                self.avoid_host_name(*from, &h.name);
            }
            Event::Message { .. } => {}
            Event::Disconnected { peer, reason } => self.on_disconnected(now, *peer, *reason),
            Event::Refused { addr, reason } => {
                self.turned_away = Some(TurnedAway {
                    addr: display_addr(addr),
                    reason: reason.key().into(),
                    at_ms: self.wall_ms(now),
                });
            }
        }
        self.events.push_back(ev);
    }

    fn plan_partner_addr(&self) -> Option<SocketAddr> {
        match self.plan {
            Plan::Host { partner } => partner.map(|c| c.addr),
            Plan::Join(t) => Some(t.addr()),
            Plan::Idle => None,
        }
    }

    /// A Steam session lost its partner to a timeout: keep the link going.
    /// `true` if handled.
    fn steam_reconnect(&mut self, now: Duration, reason: DisconnectReason) -> bool {
        let lost = reason == DisconnectReason::Timeout
            || (reason == DisconnectReason::HandshakeTimeout && self.reconnecting_since.is_some());
        if !lost || !self.steam_rejoin().enabled() {
            return false;
        }
        match self.steam {
            Some(SteamSession::Join { host }) => {
                let since = *self.reconnecting_since.get_or_insert(now);
                if !self.steam_rejoin().allows(now.saturating_sub(since)) {
                    return false;
                }
                if let Link::Active(ep) = &mut self.link {
                    let _ = ep.connect(now, steam_addr(host), None);
                }
                self.failure = Some(Failure {
                    kind: ErrorKind::LinkLost,
                    message: "Lost the link to the host. Reconnecting through Steam...".into(),
                    refused: None,
                });
                true
            }
            Some(SteamSession::Host) if reason == DisconnectReason::Timeout => {
                self.reconnecting_since.get_or_insert(now);
                self.failure = Some(Failure {
                    kind: ErrorKind::LinkLost,
                    message: "Lost the link to your partner. Their place is kept: they can \
                              come back."
                        .into(),
                    refused: None,
                });
                true
            }
            _ => false,
        }
    }

    /// A UDP session lost its partner to a timeout: within the rejoin window,
    /// keep the link going. `true` if handled.
    fn udp_reconnect(&mut self, now: Duration, reason: DisconnectReason) -> bool {
        let rejoin = self.udp_rejoin();
        let lost = reason == DisconnectReason::Timeout
            || (reason == DisconnectReason::HandshakeTimeout && self.reconnecting_since.is_some());
        if self.steam.is_some() || !lost || !rejoin.enabled() {
            return false;
        }
        let since = *self.reconnecting_since.get_or_insert(now);
        if !rejoin.allows(now.saturating_sub(since)) {
            self.reconnecting_since = None;
            return false;
        }
        let Link::Active(ep) = &mut self.link else {
            return false;
        };
        let message = match self.plan {
            Plan::Join(target) => {
                let _ = ep.connect(now, target.addr(), target.nonce());
                "Lost the link to the host. Trying to reconnect..."
            }
            Plan::Host { partner } => {
                if let Some(code) = partner {
                    self.rejoin_peer = ep.connect(now, code.addr, Some(code.nonce));
                    self.host_punching = self.rejoin_peer.is_some();
                }
                "Lost the link to your partner. Their place is kept: they can come back."
            }
            Plan::Idle => return false,
        };
        self.failure = Some(Failure {
            kind: ErrorKind::LinkLost,
            message: message.into(),
            refused: None,
        });
        true
    }

    /// The UDP rejoin window ran out: stop trying and report the timeout.
    fn udp_rejoin_over(&mut self, now: Duration) {
        let Some(since) = self.reconnecting_since else {
            return;
        };
        if self.steam.is_some() || self.udp_rejoin().allows(now.saturating_sub(since)) {
            return;
        }
        self.reconnecting_since = None;
        self.host_punching = false;
        let is_client = self.role() == Some(Role::Client);
        if let Link::Active(ep) = &mut self.link {
            if let Some(p) = self.rejoin_peer.take()
                && !ep.is_connected(p)
            {
                ep.disconnect(now, p);
            }
            for (p, _) in ep.reserved() {
                ep.release(p);
            }
        }
        self.failure = Some(Failure {
            kind: ErrorKind::LinkLost,
            message: format!(
                "Lost the link: nothing heard from your partner for {} s.",
                self.cfg.link_timeout.as_secs()
            ),
            refused: None,
        });
        if is_client {
            self.go_idle = true;
        }
    }

    fn on_disconnected(&mut self, now: Duration, peer: PeerId, reason: DisconnectReason) {
        let addr = self.peer_addrs.remove(&peer);
        self.peer_names.remove(&peer);
        if self.steam_reconnect(now, reason) || self.udp_reconnect(now, reason) {
            return;
        }
        let is_client = self.role() == Some(Role::Client);
        let steam = self.connector == Connector::Steam;
        let failure = match reason {
            DisconnectReason::Local | DisconnectReason::Replaced => None,
            DisconnectReason::HandshakeTimeout => {
                self.host_punching = false;
                let message = if steam {
                    format!(
                        "The host did not answer within {} s. Steam could not connect you; \
                         try again, or ask the host to invite you again.",
                        self.cfg.handshake_timeout.as_secs()
                    )
                } else {
                    format!(
                        "No answer from your partner within {} s. Both of you need each \
                         other's current code (a code changes when its game restarts).",
                        self.cfg.handshake_timeout.as_secs()
                    )
                };
                Some(Failure {
                    kind: ErrorKind::HandshakeTimeout,
                    message,
                    refused: None,
                })
            }
            DisconnectReason::Timeout => Some(Failure {
                kind: ErrorKind::LinkLost,
                message: format!(
                    "Lost the link: nothing heard from your partner for {} s.",
                    self.cfg.link_timeout.as_secs()
                ),
                refused: None,
            }),
            DisconnectReason::Remote => Some(Failure {
                kind: ErrorKind::LinkLost,
                message: "Your partner disconnected.".into(),
                refused: None,
            }),
            DisconnectReason::Refused(r) => {
                // Refused by this side's version check: record it as turned away.
                if self.refusing_peer.take() == Some(peer)
                    && let Some(a) = addr
                {
                    self.turned_away = Some(TurnedAway {
                        addr: display_addr(&a),
                        reason: r.key().into(),
                        at_ms: self.wall_ms(now),
                    });
                }
                Some(Failure {
                    kind: ErrorKind::Refused,
                    message: refusal_message(r),
                    refused: Some(r),
                })
            }
        };
        if let Some(f) = failure {
            // A host that still has partners keeps quiet about one leaving.
            let others = self.endpoint().is_some_and(|ep| !ep.peers().is_empty());
            if is_client || !others {
                self.failure = Some(f);
            }
        }
        if is_client && reason != DisconnectReason::Local {
            // The client's only connection is gone: hand the socket back.
            self.go_idle = true;
        }
        if !steam
            && matches!(
                reason,
                DisconnectReason::HandshakeTimeout | DisconnectReason::Timeout
            )
        {
            // Re-code: rediscover now so the code shown is current.
            self.stun_next = Some(now);
        }
    }

    fn start(&mut self, role: Role) {
        let Link::Idle(t) = std::mem::replace(&mut self.link, Link::Gone) else {
            return;
        };
        let mut c = EndpointConfig::new(role, self.cfg.nonce);
        c.max_peers = self.cfg.max_peers.max(2);
        c.handshake_timeout = self.cfg.handshake_timeout;
        c.timeout = self.cfg.link_timeout;
        let ep = Endpoint::new(t, c);
        self.link = Link::Active(Box::new(ep));
    }

    /// Say goodbye to every peer and take the socket back.
    fn idle(&mut self, now: Duration) {
        self.go_idle = false;
        self.host_punching = false;
        self.plan = Plan::Idle;
        self.steam = None;
        self.reconnecting_since = None;
        self.rejoin_peer = None;
        if let Link::Active(ep) = &mut self.link {
            for p in ep.peers() {
                ep.disconnect(now, p);
            }
        }
        // Forward the goodbyes' Disconnected events before dropping the endpoint.
        let batch: Vec<Event> = match &mut self.link {
            Link::Active(ep) => std::iter::from_fn(|| ep.next_event()).collect(),
            _ => Vec::new(),
        };
        for ev in batch {
            if let Event::Disconnected { peer, .. } = &ev {
                self.peer_addrs.remove(peer);
                self.peer_names.remove(peer);
            }
            self.events.push_back(ev);
        }
        match std::mem::replace(&mut self.link, Link::Gone) {
            Link::Active(ep) => self.link = Link::Idle((*ep).into_transport()),
            other => self.link = other,
        }
        self.peer_addrs.clear();
        self.peer_names.clear();
        self.go_idle = false;
    }

    fn drive_stun(&mut self, now: Duration) {
        if self.cfg.stun_servers.is_empty() || !self.discovery_wanted {
            return;
        }
        if self.stun_lookup.is_some() && self.stun_error.is_some() {
            self.take_late_lookup(now);
        }
        if self.discovery.is_none() && self.stun_next.is_none_or(|t| now >= t) {
            if self.state() == LobbyState::Connected && self.public.is_some() {
                // The link's keepalives hold the mapping; check again later.
                self.stun_next = Some(now + self.cfg.stun_refresh);
                return;
            }
            match self.resolve_stun_servers(now) {
                Lookup::Ready => {}
                Lookup::Pending => return,
                Lookup::Failed(why) => {
                    self.stun_error = Some(why);
                    self.stun_ever_finished = true;
                    self.stun_next = Some(now + self.stun_backoff);
                    self.stun_backoff = (self.stun_backoff * 2).min(self.cfg.stun_retry);
                    return;
                }
            }
            self.discovery = Some(Discovery::new(self.stun_resolved.clone()));
        }
        let Some(d) = &mut self.discovery else {
            return;
        };
        let request = d.poll(now);
        if let Some((to, bytes)) = request
            && let Some(t) = self.lobby_transport()
            && let Err(e) = t.send_to(now, to, &bytes)
        {
            self.socket_error = Some(e.to_string());
        }
        self.after_discovery(now);
    }

    /// The STUN servers' addresses, found once and kept, without waiting.
    fn resolve_stun_servers(&mut self, now: Duration) -> Lookup {
        if !self.stun_resolved.is_empty() {
            return Lookup::Ready;
        }
        let names = self.cfg.stun_servers.join(", ");
        if let Some(l) = &self.stun_lookup {
            match l.rx.try_recv() {
                Ok(found) => {
                    self.stun_lookup = None;
                    return self.found(found, &names);
                }
                Err(mpsc::TryRecvError::Empty) if now < l.since + LOOKUP_SLOW => {
                    return Lookup::Pending;
                }
                Err(mpsc::TryRecvError::Empty) if now < l.since + LOOKUP_ABANDON => {
                    return Lookup::Failed(format!("no answer looking up {names} (offline?)"));
                }
                // Given up, or the thread is gone: ask again below.
                Err(_) => self.stun_lookup = None,
            }
        }
        let want_v4 = self.local_addr.is_none_or(|a| a.is_ipv4());
        let pick = move |addrs: Vec<SocketAddr>| addrs.into_iter().find(|a| a.is_ipv4() == want_v4);
        let literals: Option<Vec<SocketAddr>> = self
            .cfg
            .stun_servers
            .iter()
            .map(|s| s.parse::<SocketAddr>().ok())
            .collect();
        if let Some(literals) = literals {
            let found = literals.into_iter().filter_map(|a| pick(vec![a])).collect();
            return self.found(found, &names);
        }
        let (tx, rx) = mpsc::channel();
        let servers = self.cfg.stun_servers.clone();
        let resolve = self.cfg.stun_resolve;
        let spawned = std::thread::Builder::new()
            .name("ndml-stun-lookup".into())
            .spawn(move || {
                let found: Vec<SocketAddr> = servers
                    .iter()
                    .filter_map(|s| pick(resolve(s).ok()?))
                    .collect();
                // The agent may be gone: the answer is then dropped.
                let _ = tx.send(found);
            });
        match spawned {
            Ok(_) => {
                self.stun_lookup = Some(StunLookup { rx, since: now });
                #[cfg(test)]
                {
                    self.stun_lookups_asked += 1;
                }
                Lookup::Pending
            }
            Err(e) => Lookup::Failed(format!("could not look up {names}: {e}")),
        }
    }

    /// The lookup's answer: kept when it found an address.
    fn found(&mut self, found: Vec<SocketAddr>, names: &str) -> Lookup {
        if found.is_empty() {
            return Lookup::Failed(format!("could not resolve {names} (offline?)"));
        }
        self.stun_resolved = found;
        self.stun_backoff = LOOKUP_BACKOFF_FIRST;
        Lookup::Ready
    }

    /// A lookup that answers after it counted as failed is taken at once.
    fn take_late_lookup(&mut self, now: Duration) {
        let Some(l) = &self.stun_lookup else {
            return;
        };
        match l.rx.try_recv() {
            Ok(found) => {
                self.stun_lookup = None;
                let names = self.cfg.stun_servers.join(", ");
                if matches!(self.found(found, &names), Lookup::Ready) {
                    self.stun_error = None;
                    self.stun_next = Some(now);
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.stun_lookup = None,
        }
    }

    fn after_discovery(&mut self, now: Duration) {
        let Some(d) = &self.discovery else {
            return;
        };
        if let Some(m) = d.mapped() {
            self.public = Some(m);
            self.public_from = d.mapped_by();
            self.stun_error = None;
        }
        if !d.is_done() {
            return;
        }
        if d.nat() != NatBehavior::Unknown {
            self.nat = d.nat();
        }
        let ok = d.mapped().is_some();
        if !ok {
            self.stun_error = d.error();
        }
        self.discovery = None;
        self.stun_ever_finished = true;
        self.stun_next = Some(
            now + if ok {
                self.cfg.stun_refresh
            } else {
                self.cfg.stun_retry
            },
        );
    }

    fn poll_peer_file(&mut self, now: Duration) {
        let Some(read) = self.io.as_ref().and_then(|(s, w)| s.take(*w)) else {
            return;
        };
        let text = match read {
            Ok(t) => t,
            Err((io::ErrorKind::NotFound, _)) => {
                self.peer_file_error = None;
                return;
            }
            Err((_, e)) => {
                self.peer_file_error = Some(e);
                return;
            }
        };
        if self.last_peer_text.as_deref() == Some(text.as_str()) {
            return;
        }
        let parsed = files::parse_peer(&text);
        self.last_peer_text = Some(text);
        match parsed {
            Err(e) => self.peer_file_error = Some(e.to_string()),
            Ok(req) => {
                self.peer_file_error = None;
                if req.session == self.session && req.request != self.applied {
                    self.apply(now, &req);
                }
            }
        }
    }

    /// Apply a request from the in-game panel, with the rules of a file
    /// request; the file request counter is left alone.
    pub fn request(&mut self, now: Duration, req: &LobbyRequest) {
        if self.stopped {
            return;
        }
        let mut r = files::PeerRequest::new(self.applied, &self.session);
        r.role = req.role;
        r.partner_code.clone_from(&req.partner_code);
        r.direct.clone_from(&req.direct);
        r.name.clone_from(&req.name);
        r.disconnect = req.disconnect;
        self.apply_choice(now, &r);
    }

    fn apply(&mut self, now: Duration, req: &files::PeerRequest) {
        self.applied = req.request;
        if let Some(action) = req.steam {
            // For the game thread, which owns every Steam call.
            if self.steam_actions.len() < 16 {
                self.steam_actions.push(SteamFileRequest {
                    action,
                    code: req.steam_code.clone(),
                });
            }
            return;
        }
        self.apply_choice(now, req);
    }

    fn apply_choice(&mut self, now: Duration, req: &files::PeerRequest) {
        let named = files::clean_name(&req.name).filter(|n| !same_name(n, files::AUTO_NAME));
        if let Some(n) = named {
            // Either automatic pick keeps the automatic name as it stands.
            let auto = self
                .auto_names
                .as_ref()
                .is_some_and(|(a, b)| same_name(&n, a) || same_name(&n, b));
            if !auto {
                self.codes_name = n;
                self.auto_names = None;
            }
        }
        if self.steam.is_none() {
            self.name.clone_from(&self.codes_name);
        }
        let plan = match req.plan() {
            Ok(p) => p,
            Err(message) => {
                self.failure = Some(Failure {
                    kind: ErrorKind::BadRequest,
                    message,
                    refused: None,
                });
                return;
            }
        };
        if self.steam.is_none() && plan == self.plan && self.state() == LobbyState::Connected {
            return; // already linked as asked (a re-sent request)
        }
        if self.steam.is_some() {
            self.idle(now); // a UDP choice replaces the Steam session
            self.name.clone_from(&self.codes_name);
        }
        if plan != Plan::Idle {
            self.connector = Connector::Udp;
        }
        // Hosting or joining by code uses the code route.
        if matches!(plan, Plan::Host { .. } | Plan::Join(files::Target::Code(_))) {
            self.want_discovery();
        }
        self.failure = None;
        self.role = if plan == Plan::Idle {
            RoleChoice::None
        } else {
            req.role
        };
        match plan {
            Plan::Idle => self.idle(now),
            Plan::Host { partner } => {
                if self.role() != Some(Role::Host) {
                    self.idle(now);
                    self.start(Role::Host);
                    let rejoin = self.udp_rejoin();
                    if let Link::Active(ep) = &mut self.link {
                        ep.set_rejoin(rejoin);
                        ep.set_rejoin_pins_nonce(true);
                    }
                }
                self.plan = plan;
                if let Some(code) = partner {
                    let linked = self.peer_addrs.values().any(|&a| a == code.addr);
                    if let (false, Link::Active(ep)) = (linked, &mut self.link)
                        && ep.connect(now, code.addr, Some(code.nonce)).is_some()
                    {
                        self.host_punching = true;
                    }
                }
            }
            Plan::Join(target) => {
                self.idle(now);
                self.start(Role::Client);
                self.plan = plan;
                if let Link::Active(ep) = &mut self.link {
                    let _ = ep.connect(now, target.addr(), target.nonce());
                }
            }
        }
    }

    fn wall_ms(&self, now: Duration) -> u64 {
        files::unix_ms(self.cfg.wall_clock_at_epoch)
            .saturating_add(u64::try_from(now.as_millis()).unwrap_or(u64::MAX))
    }

    fn build_status(&self, now: Duration) -> Status {
        let stun_state = if self.cfg.stun_servers.is_empty() {
            StunState::Disabled
        } else if self.public.is_some() {
            StunState::Ok
        } else if self.stun_error.is_some() {
            StunState::Failed
        } else {
            StunState::Pending
        };
        let retry_in_ms = (stun_state == StunState::Failed)
            .then(|| {
                self.stun_next
                    .map(|t| t.saturating_sub(now).as_millis() as u64)
            })
            .flatten();
        let peers = match &self.link {
            Link::Active(ep) => self
                .peer_addrs
                .iter()
                .map(|(&p, a)| {
                    let st = ep.stats(p).unwrap_or_default();
                    PeerStatus {
                        id: p.0,
                        name: self.peer_names.get(&p).cloned().unwrap_or_default(),
                        addr: display_addr(a),
                        connected: ep.is_connected(p),
                        rtt_ms: round2(st.rtt.as_secs_f64() * 1000.0),
                        jitter_ms: round2(st.jitter.as_secs_f64() * 1000.0),
                        loss_pct: round2(st.loss * 100.0),
                    }
                })
                .collect(),
            _ => Vec::new(),
        };
        Status {
            format: files::FORMAT,
            session: self.session.clone(),
            heartbeat_ms: self.wall_ms(now),
            state: self.state(),
            role: self.role,
            name: self.name.clone(),
            request: self.applied,
            error_kind: self.failure.as_ref().map(|f| f.kind),
            error: self.failure.as_ref().map(|f| f.message.clone()),
            refused: self
                .failure
                .as_ref()
                .and_then(|f| f.refused)
                .map(|r| r.key().to_owned()),
            code: self.code().map(|c| c.encode()),
            lan_code: self.lan_code().map(|c| c.encode()),
            local_addr: self.local_addr.map(|a| a.to_string()),
            lan_addr: self.lan_addr.map(|a| a.to_string()),
            public_addr: self.public.map(|a| a.to_string()),
            peer_file_error: self.peer_file_error.clone(),
            connector: self.connector,
            reconnecting: self.reconnecting(),
            stun: StunStatus {
                state: stun_state,
                server: self.public_from.map(|a| a.to_string()),
                error: self.stun_error.clone(),
                nat: match self.nat {
                    NatBehavior::Unknown => NatKind::Unknown,
                    NatBehavior::Consistent => NatKind::Consistent,
                    NatBehavior::Varies => NatKind::Varies,
                },
                retry_in_ms,
            },
            peers,
            turned_away: self.turned_away.clone(),
            steam: self.steam_status.clone(),
        }
    }

    fn publish(&mut self, now: Duration) {
        let next = self.build_status(now);
        let changed = next.stable() != self.status.stable();
        let due = self
            .last_write
            .is_none_or(|t| now.saturating_sub(t) >= self.cfg.heartbeat);
        if changed {
            self.status_version += 1;
        }
        self.status = next;
        if changed || due {
            self.write_status(now);
        }
    }

    fn write_status(&mut self, now: Duration) {
        if self.stopped {
            self.status.state = LobbyState::Stopped;
            self.status.peers.clear();
        }
        self.last_write = Some(now);
        let Some((io, _)) = &self.io else {
            return;
        };
        let path = self.cfg.dir.join(STATUS_FILE);
        // Threaded, the error is the last finished write's.
        self.file_error = match files::status_text(&self.status) {
            Ok(text) => {
                io.write(&path, text.into_bytes());
                io.write_error(&path)
            }
            Err(e) => Some(e.to_string()),
        }
        .map(|e| format!("{}: {e}", path.display()));
    }
}

/// The lobby files' service with its watch on `peer.toml`, and the first file
/// error.
fn lobby_io(
    cfg: &LobbyConfig,
) -> (
    Option<(crate::fileio::FileService, crate::fileio::WatchId)>,
    Option<String>,
) {
    use crate::fileio::FileService;
    if !cfg.files {
        return (None, None);
    }
    let peer = cfg.dir.join(PEER_FILE);
    if cfg.file_thread {
        return match FileService::spawn("ndml-lobby-files") {
            Ok(s) => {
                let w = s.watch(&peer, cfg.peer_poll);
                (Some((s, w)), None)
            }
            Err(e) => (None, Some(format!("the lobby files' thread: {e}"))),
        };
    }
    let err = std::fs::create_dir_all(&cfg.dir)
        .err()
        .map(|e| format!("{}: {e}", cfg.dir.display()));
    let s = FileService::inline();
    let w = s.watch(&peer, cfg.peer_poll);
    (Some((s, w)), err)
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Two player names that read the same (trimmed, any case).
fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// The player-facing sentence for a refusal.
#[must_use]
pub fn refusal_message(r: RefuseReason) -> String {
    match r {
        RefuseReason::Full => "The host's session is full.".into(),
        RefuseReason::RunInProgress => {
            "Your partner is in a run right now. Join when they are back in the main menu.".into()
        }
        RefuseReason::VersionMismatch => "Version mismatch: both players need the same game \
                                          build and co-op mod version."
            .into(),
        RefuseReason::Other(_) => format!("Refused: {r}."),
    }
}
