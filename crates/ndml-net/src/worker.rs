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

//! Runs an [`Endpoint`] (or a [`LobbyAgent`] that owns one) on its own
//! thread, for the game.
//!
//! The game thread never touches the socket: it drains events and queues sends
//! through non-blocking channels, and reads the per-peer clock and stats the
//! network thread publishes (about every millisecond).
//!
//! The thread is detached, never joined (joining from a DLL unload could
//! deadlock on the loader lock); [`NetWorker::begin_shutdown`] lets the caller
//! wait for the goodbyes up to its own deadline.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::clock::ClockSync;
use crate::endpoint::{Endpoint, Event, RefuseReason, SendError, Stats};
use crate::lobby::{LobbyAgent, LobbyRequest, Status, SteamFileRequest, SteamRequest, SteamStatus};
use crate::msg::{Message, PeerId};
use crate::transport::Transport;

/// How long the network thread sleeps between polls.
pub const POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Debug)]
enum Command {
    Send(PeerId, Message),
    Broadcast(Message, Option<PeerId>),
    Connect(SocketAddr, Option<u32>),
    Disconnect(PeerId),
    Refuse(PeerId, RefuseReason),
    SetRefusing(Option<RefuseReason>),
    HoldLink(bool),
    Lobby(LobbyRequest),
    Discover,
    Steam(SteamRequest),
    SteamStatus(Option<Box<SteamStatus>>),
    Shutdown,
    /// Shut down and say how far it got ([`NetWorker::begin_shutdown`]).
    ShutdownTold(Sender<ShutdownStage>),
}

/// How far a shutdown got by the caller's deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownStage {
    /// The network thread did not answer in time (busy in a poll, or gone).
    NoAnswer,
    /// The goodbyes are sent; the last status write is still under way.
    Sent,
    /// Everything: the goodbyes sent and the last status written.
    Done,
}

/// A shutdown handed to the network thread ([`NetWorker::begin_shutdown`]).
#[derive(Debug)]
pub struct ShutdownWait {
    rx: Receiver<ShutdownStage>,
}

impl ShutdownWait {
    /// Wait for the shutdown, never past `deadline`; returns how far it got.
    #[must_use]
    pub fn wait_until(self, deadline: Instant) -> ShutdownStage {
        let mut got = ShutdownStage::NoAnswer;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(ShutdownStage::Done) => return ShutdownStage::Done,
                Ok(stage) => got = stage,
                Err(_) => return got,
            }
        }
    }
}

/// Per-peer snapshot published by the network thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct PeerInfo {
    /// Clock tracker for the peer.
    pub clock: ClockSync,
    /// Connection numbers.
    pub stats: Stats,
    /// Handshake completed.
    pub connected: bool,
}

/// What the network thread drives: a bare [`Endpoint`] or a [`LobbyAgent`]
/// (calls that need a link do nothing while it has no role).
pub trait Driver: Send + 'static {
    /// Do what is due at `now` (time since the worker's epoch).
    fn poll(&mut self, now: Duration) -> io::Result<()>;
    /// Next event for the game.
    fn next_event(&mut self) -> Option<Event>;
    /// Queue a message for one peer.
    fn send(&mut self, peer: PeerId, msg: &Message) -> Result<(), SendError>;
    /// Queue a message for every connected peer except `except`.
    fn broadcast(&mut self, msg: &Message, except: Option<PeerId>) -> Result<(), SendError>;
    /// Start punching toward `addr` (see [`Endpoint::connect`]).
    fn connect(&mut self, now: Duration, addr: SocketAddr, remote_nonce: Option<u32>);
    /// Say goodbye to `peer`.
    fn disconnect(&mut self, now: Duration, peer: PeerId);
    /// Turn `peer` away (see [`Endpoint::refuse`]).
    fn refuse(&mut self, now: Duration, peer: PeerId, reason: RefuseReason);
    /// Refuse newcomers (see [`Endpoint::set_refusing`]).
    fn set_refusing(&mut self, reason: Option<RefuseReason>);
    /// Leave: goodbyes to every peer.
    fn shutdown(&mut self, now: Duration);
    /// See [`Transport::release_held`].
    fn release_held(&mut self, _now: Duration) {}
    /// Local peer id, once known.
    fn local_peer(&self) -> Option<PeerId>;
    /// Clock and stats for every known peer.
    fn peer_infos(&self) -> Vec<(PeerId, PeerInfo)>;
    /// Apply an in-game lobby choice (only a lobby agent has a lobby).
    fn lobby_request(&mut self, _now: Duration, _req: &LobbyRequest) {}
    /// See [`LobbyAgent::hold_link`].
    fn hold_link(&mut self, _on: bool) {}
    /// Find the public address now (see [`LobbyAgent::want_discovery`]).
    fn lobby_discover(&mut self) {}
    /// The lobby status and a change key, when driving a lobby agent.
    fn lobby(&self) -> Option<((u64, u64), &Status)> {
        None
    }
    /// Start, update or end a Steam session (only a lobby agent has one).
    fn steam_request(&mut self, _now: Duration, _req: &SteamRequest) {}
    /// The game thread's Steam view for `status.toml`.
    fn steam_status(&mut self, _status: Option<SteamStatus>) {}
    /// Steam actions asked for in `peer.toml`, for the game thread.
    fn take_steam_actions(&mut self) -> Vec<SteamFileRequest> {
        Vec::new()
    }
}

/// Log an unencodable message now and then (kind byte and size only); a send
/// to a peer already gone is silent.
fn send_refused(count: &mut u64, m: &Message, e: &SendError) {
    let SendError::Encode(why) = e else {
        return;
    };
    *count += 1;
    if *count <= 3 || (*count).is_multiple_of(100) {
        let bytes = postcard::to_allocvec(m).unwrap_or_default();
        log::warn!(
            "co-op net: a message was not sent ({why}; kind byte {}, {} bytes; {} so far)",
            bytes.first().copied().unwrap_or_default(),
            bytes.len(),
            count
        );
    }
}

fn endpoint_peer_infos<T: Transport>(ep: &Endpoint<T>) -> Vec<(PeerId, PeerInfo)> {
    (0..=PeerId::MAX)
        .map(PeerId)
        .filter_map(|p| {
            let (clock, stats) = (ep.clock(p)?, ep.stats(p)?);
            Some((
                p,
                PeerInfo {
                    clock,
                    stats,
                    connected: ep.is_connected(p),
                },
            ))
        })
        .collect()
}

impl<T: Transport + Send + 'static> Driver for Endpoint<T> {
    fn poll(&mut self, now: Duration) -> io::Result<()> {
        Endpoint::poll(self, now)
    }

    fn next_event(&mut self) -> Option<Event> {
        Endpoint::next_event(self)
    }

    fn send(&mut self, peer: PeerId, msg: &Message) -> Result<(), SendError> {
        Endpoint::send(self, peer, msg)
    }

    fn broadcast(&mut self, msg: &Message, except: Option<PeerId>) -> Result<(), SendError> {
        Endpoint::broadcast(self, msg, except)
    }

    fn connect(&mut self, now: Duration, addr: SocketAddr, remote_nonce: Option<u32>) {
        let _ = Endpoint::connect(self, now, addr, remote_nonce);
    }

    fn disconnect(&mut self, now: Duration, peer: PeerId) {
        Endpoint::disconnect(self, now, peer);
    }

    fn refuse(&mut self, now: Duration, peer: PeerId, reason: RefuseReason) {
        Endpoint::refuse(self, now, peer, reason);
    }

    fn set_refusing(&mut self, reason: Option<RefuseReason>) {
        Endpoint::set_refusing(self, reason);
    }

    fn shutdown(&mut self, now: Duration) {
        for p in self.peers() {
            Endpoint::disconnect(self, now, p);
        }
    }

    fn release_held(&mut self, now: Duration) {
        let _ = self.transport_mut().release_held(now);
    }

    fn local_peer(&self) -> Option<PeerId> {
        Endpoint::local_peer(self)
    }

    fn peer_infos(&self) -> Vec<(PeerId, PeerInfo)> {
        endpoint_peer_infos(self)
    }
}

impl<T: Transport + Send + 'static> Driver for LobbyAgent<T> {
    fn poll(&mut self, now: Duration) -> io::Result<()> {
        LobbyAgent::poll(self, now);
        Ok(()) // socket errors are reported in the status
    }

    fn next_event(&mut self) -> Option<Event> {
        LobbyAgent::next_event(self)
    }

    fn send(&mut self, peer: PeerId, msg: &Message) -> Result<(), SendError> {
        self.endpoint_mut()
            .ok_or(SendError::UnknownPeer(peer))?
            .send(peer, msg)
    }

    fn broadcast(&mut self, msg: &Message, except: Option<PeerId>) -> Result<(), SendError> {
        match self.endpoint_mut() {
            Some(ep) => ep.broadcast(msg, except),
            None => Ok(()),
        }
    }

    fn connect(&mut self, now: Duration, addr: SocketAddr, remote_nonce: Option<u32>) {
        if let Some(ep) = self.endpoint_mut() {
            let _ = ep.connect(now, addr, remote_nonce);
        }
    }

    fn disconnect(&mut self, now: Duration, peer: PeerId) {
        if let Some(ep) = self.endpoint_mut() {
            ep.disconnect(now, peer);
        }
    }

    fn refuse(&mut self, now: Duration, peer: PeerId, reason: RefuseReason) {
        if let Some(ep) = self.endpoint_mut() {
            ep.refuse(now, peer, reason);
        }
    }

    fn set_refusing(&mut self, reason: Option<RefuseReason>) {
        if let Some(ep) = self.endpoint_mut() {
            ep.set_refusing(reason);
        }
    }

    fn shutdown(&mut self, now: Duration) {
        self.stop(now);
    }

    fn release_held(&mut self, now: Duration) {
        LobbyAgent::release_held(self, now);
    }

    fn lobby_request(&mut self, now: Duration, req: &LobbyRequest) {
        self.request(now, req);
    }

    fn hold_link(&mut self, on: bool) {
        LobbyAgent::hold_link(self, on);
    }

    fn lobby_discover(&mut self) {
        self.want_discovery();
    }

    fn local_peer(&self) -> Option<PeerId> {
        self.endpoint().and_then(Endpoint::local_peer)
    }

    fn peer_infos(&self) -> Vec<(PeerId, PeerInfo)> {
        self.endpoint().map_or_default(endpoint_peer_infos)
    }

    fn lobby(&self) -> Option<((u64, u64), &Status)> {
        let s = self.status();
        Some(((self.status_version(), s.heartbeat_ms), s))
    }

    fn steam_request(&mut self, now: Duration, req: &SteamRequest) {
        LobbyAgent::steam_request(self, now, req);
    }

    fn steam_status(&mut self, status: Option<SteamStatus>) {
        self.set_steam_status(status);
    }

    fn take_steam_actions(&mut self) -> Vec<SteamFileRequest> {
        LobbyAgent::take_steam_actions(self)
    }
}

#[derive(Debug, Default)]
struct Shared {
    peers: BTreeMap<PeerId, PeerInfo>,
    local_peer: Option<PeerId>,
    last_error: Option<String>,
    lobby: Option<Status>,
    steam_actions: Vec<SteamFileRequest>,
}

/// A cloneable handle to the network thread for lobby and Steam commands
/// (events go to the [`NetWorker`] only).
#[derive(Debug, Clone)]
pub struct NetHandle {
    tx: Sender<Command>,
    shared: Arc<Mutex<Shared>>,
}

impl NetHandle {
    /// See [`NetWorker::broadcast`].
    pub fn broadcast(&self, msg: Message, except: Option<PeerId>) {
        let _ = self.tx.send(Command::Broadcast(msg, except));
    }

    /// See [`NetWorker::steam_request`].
    pub fn steam_request(&self, req: SteamRequest) {
        let _ = self.tx.send(Command::Steam(req));
    }

    /// See [`NetWorker::set_steam_status`].
    pub fn set_steam_status(&self, status: Option<SteamStatus>) {
        let _ = self.tx.send(Command::SteamStatus(status.map(Box::new)));
    }

    /// See [`NetWorker::take_steam_actions`].
    #[must_use]
    pub fn take_steam_actions(&self) -> Vec<SteamFileRequest> {
        std::mem::take(
            &mut self
                .shared
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .steam_actions,
        )
    }

    /// See [`NetWorker::lobby_status`].
    #[must_use]
    pub fn lobby_status(&self) -> Option<Status> {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lobby
            .clone()
    }
}

/// Handle to the network thread.
#[derive(Debug)]
pub struct NetWorker {
    tx: Sender<Command>,
    rx: Receiver<Event>,
    shared: Arc<Mutex<Shared>>,
    epoch: Instant,
}

impl NetWorker {
    /// Move `endpoint` onto a new thread; `epoch` is its time zero.
    pub fn spawn<T: Transport + Send + 'static>(
        endpoint: Endpoint<T>,
        epoch: Instant,
    ) -> io::Result<Self> {
        Self::spawn_driver(endpoint, epoch)
    }

    /// Move a lobby agent (which runs the lobby and, once linked, the
    /// endpoint) onto a new thread; `epoch` is its time zero.
    pub fn spawn_lobby<T: Transport + Send + 'static>(
        agent: LobbyAgent<T>,
        epoch: Instant,
    ) -> io::Result<Self> {
        Self::spawn_driver(agent, epoch)
    }

    /// Move any [`Driver`] onto a new thread named `ndml-net`.
    pub fn spawn_driver<D: Driver>(driver: D, epoch: Instant) -> io::Result<Self> {
        let (tx, cmd_rx) = mpsc::channel();
        let (ev_tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ndml-net".into())
            .spawn(move || run(driver, epoch, &cmd_rx, &ev_tx, &thread_shared))?;
        Ok(Self {
            tx,
            rx,
            shared,
            epoch,
        })
    }

    /// Endpoint time for an instant (what the network thread calls `now`).
    #[must_use]
    pub fn local_time(&self, at: Instant) -> Duration {
        at.saturating_duration_since(self.epoch)
    }

    /// A cloneable handle for lobby and Steam commands (see [`NetHandle`]).
    #[must_use]
    pub fn handle(&self) -> NetHandle {
        NetHandle {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
        }
    }

    /// See [`LobbyAgent::steam_request`]. Never blocks.
    pub fn steam_request(&self, req: SteamRequest) {
        self.handle().steam_request(req);
    }

    /// The game thread's Steam view for `status.toml`. Never blocks.
    pub fn set_steam_status(&self, status: Option<SteamStatus>) {
        self.handle().set_steam_status(status);
    }

    /// Steam actions asked for in `peer.toml`, oldest first.
    #[must_use]
    pub fn take_steam_actions(&self) -> Vec<SteamFileRequest> {
        self.handle().take_steam_actions()
    }

    /// Queue a message. Never blocks.
    pub fn send(&self, peer: PeerId, msg: Message) {
        let _ = self.tx.send(Command::Send(peer, msg));
    }

    /// Queue a message for every connected peer except `except`. Never blocks.
    pub fn broadcast(&self, msg: Message, except: Option<PeerId>) {
        let _ = self.tx.send(Command::Broadcast(msg, except));
    }

    /// Start connecting to `addr` (see [`Endpoint::connect`]).
    pub fn connect(&self, addr: SocketAddr, remote_nonce: Option<u32>) {
        let _ = self.tx.send(Command::Connect(addr, remote_nonce));
    }

    /// Say goodbye to `peer`.
    pub fn disconnect(&self, peer: PeerId) {
        let _ = self.tx.send(Command::Disconnect(peer));
    }

    /// Turn `peer` away with a reason it is told (see [`Endpoint::refuse`]).
    pub fn refuse(&self, peer: PeerId, reason: RefuseReason) {
        let _ = self.tx.send(Command::Refuse(peer, reason));
    }

    /// See [`Endpoint::set_refusing`].
    pub fn set_refusing(&self, reason: Option<RefuseReason>) {
        let _ = self.tx.send(Command::SetRefusing(reason));
    }

    /// See [`LobbyAgent::hold_link`]. Never blocks.
    pub fn hold_link(&self, on: bool) {
        let _ = self.tx.send(Command::HoldLink(on));
    }

    /// Hand an in-game lobby choice to the lobby agent. Never blocks.
    pub fn lobby_request(&self, req: LobbyRequest) {
        let _ = self.tx.send(Command::Lobby(req));
    }

    /// See [`LobbyAgent::want_discovery`]. Never blocks.
    pub fn lobby_discover(&self) {
        let _ = self.tx.send(Command::Discover);
    }

    /// Everything that arrived since the last call.
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.rx.try_iter()
    }

    /// Latest snapshot for `peer`.
    #[must_use]
    pub fn peer_info(&self, peer: PeerId) -> Option<PeerInfo> {
        let s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        s.peers.get(&peer).copied()
    }

    /// Local peer id, once known.
    #[must_use]
    pub fn local_peer(&self) -> Option<PeerId> {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .local_peer
    }

    /// Shut the network thread down and get a handle to wait for it. Never
    /// blocks; later commands are ignored.
    #[must_use]
    pub fn begin_shutdown(&self) -> ShutdownWait {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Command::ShutdownTold(tx));
        ShutdownWait { rx }
    }

    /// The last socket error the network thread hit, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_error
            .clone()
    }

    /// The lobby status, when the worker runs a lobby agent.
    #[must_use]
    pub fn lobby_status(&self) -> Option<Status> {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lobby
            .clone()
    }

    /// Remote time to render `peer`'s entities at `at`, for snapshots sent every
    /// `snapshot_interval`, or `None` before the clock has synced.
    #[must_use]
    pub fn render_time(
        &self,
        peer: PeerId,
        at: Instant,
        snapshot_interval: Duration,
    ) -> Option<Duration> {
        let info = self.peer_info(peer)?;
        if !info.clock.is_synced() {
            return None;
        }
        let delay = info.clock.interp_delay(snapshot_interval);
        Some(info.clock.render_time(self.local_time(at), delay))
    }
}

impl Drop for NetWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

fn run<D: Driver>(
    mut d: D,
    epoch: Instant,
    cmd_rx: &Receiver<Command>,
    ev_tx: &Sender<Event>,
    shared: &Mutex<Shared>,
) {
    let mut lobby_key = None;
    let mut refused = 0u64;
    loop {
        let now = epoch.elapsed();
        loop {
            match cmd_rx.try_recv() {
                Ok(Command::Send(p, m)) => {
                    if let Err(e) = d.send(p, &m) {
                        send_refused(&mut refused, &m, &e);
                    }
                }
                Ok(Command::Broadcast(m, except)) => {
                    if let Err(e) = d.broadcast(&m, except) {
                        send_refused(&mut refused, &m, &e);
                    }
                }
                Ok(Command::Connect(addr, nonce)) => d.connect(now, addr, nonce),
                Ok(Command::Disconnect(p)) => d.disconnect(now, p),
                Ok(Command::Refuse(p, r)) => d.refuse(now, p, r),
                Ok(Command::SetRefusing(r)) => d.set_refusing(r),
                Ok(Command::HoldLink(on)) => d.hold_link(on),
                Ok(Command::Lobby(req)) => d.lobby_request(now, &req),
                Ok(Command::Discover) => d.lobby_discover(),
                Ok(Command::Steam(req)) => d.steam_request(now, &req),
                Ok(Command::SteamStatus(s)) => d.steam_status(s.map(|b| *b)),
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                    d.shutdown(now);
                    return;
                }
                Ok(Command::ShutdownTold(tell)) => {
                    d.shutdown(now);
                    d.release_held(now);
                    let _ = tell.send(ShutdownStage::Sent);
                    // Dropping the agent writes the last status.
                    drop(d);
                    let _ = tell.send(ShutdownStage::Done);
                    return;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        let err = d.poll(now).err();
        let steam_actions = d.take_steam_actions();
        while let Some(e) = d.next_event() {
            if ev_tx.send(e).is_err() {
                d.shutdown(now);
                return; // game side is gone
            }
        }
        {
            let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
            s.local_peer = d.local_peer();
            s.peers = d.peer_infos().into_iter().collect();
            if let Some((key, status)) = d.lobby()
                && lobby_key != Some(key)
            {
                lobby_key = Some(key);
                s.lobby = Some(status.clone());
            }
            if let Some(e) = err {
                s.last_error = Some(e.to_string());
            }
            if !steam_actions.is_empty() && s.steam_actions.len() < 16 {
                s.steam_actions.extend(steam_actions);
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{EndpointConfig, Role};
    use crate::lobby::{self, LobbyConfig, LobbyState, PeerRequest, RoleChoice};
    use crate::transport::LoopbackNet;

    #[test]
    fn two_workers_exchange_messages() {
        let net = LoopbackNet::new();
        let ha: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let ca: SocketAddr = "10.0.0.1:2".parse().unwrap();
        let epoch = Instant::now();
        let host = NetWorker::spawn(
            Endpoint::new(net.bind(ha).unwrap(), EndpointConfig::new(Role::Host, 1)),
            epoch,
        )
        .unwrap();
        let client = NetWorker::spawn(
            Endpoint::new(net.bind(ca).unwrap(), EndpointConfig::new(Role::Client, 2)),
            epoch,
        )
        .unwrap();
        client.connect(ha, Some(1));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = None;
        let mut sent = false;
        while Instant::now() < deadline && got.is_none() {
            for e in host.events() {
                if let Event::Connected { peer, .. } = e {
                    host.send(peer, Message::Go { start_ms: 42 });
                    sent = true;
                }
            }
            for e in client.events() {
                if let Event::Message { msg, .. } = e {
                    got = Some(msg);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(sent);
        assert_eq!(got, Some(Message::Go { start_ms: 42 }));
        assert_eq!(client.local_peer(), Some(PeerId(1)));
        let info = client.peer_info(PeerId::HOST).unwrap();
        assert!(info.connected);
        assert!(client.last_error().is_none());
        assert!(client.local_time(Instant::now()) > Duration::ZERO);
    }

    /// The mod's path: two lobby agents on worker threads, linked through
    /// `peer.toml` requests, then a message across and a clean shutdown.
    #[test]
    fn lobby_workers_link_through_peer_files() {
        let net = LoopbackNet::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/ndml-net-worker");
        let _ = std::fs::remove_dir_all(&root);
        let (dir_h, dir_c) = (root.join("host"), root.join("client"));
        let ha: SocketAddr = "10.0.0.1:7777".parse().unwrap();
        let ca: SocketAddr = "10.0.0.2:7777".parse().unwrap();
        let make = |dir: &std::path::Path, at, nonce| {
            let mut cfg = LobbyConfig::new(dir.to_path_buf(), nonce);
            cfg.stun_servers = Vec::new();
            LobbyAgent::new(net.bind(at).unwrap(), cfg)
        };
        let epoch = Instant::now();
        let host = NetWorker::spawn_lobby(make(&dir_h, ha, 0x11), epoch).unwrap();
        let client = NetWorker::spawn_lobby(make(&dir_c, ca, 0x22), epoch).unwrap();
        let ask = |dir: &std::path::Path, session: &str, f: &dyn Fn(&mut PeerRequest)| {
            let mut r = PeerRequest::new(1, session);
            f(&mut r);
            lobby::files::write_peer(&dir.join(lobby::PEER_FILE), &r).unwrap();
        };
        ask(&dir_h, "00000011", &|r| r.role = RoleChoice::Host);
        ask(&dir_c, "00000022", &|r| {
            r.role = RoleChoice::Join;
            r.direct = Some(ha.to_string());
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = None;
        while Instant::now() < deadline && got.is_none() {
            for e in host.events() {
                if let Event::Connected { peer, .. } = e {
                    host.send(peer, Message::Go { start_ms: 7 });
                }
            }
            for e in client.events() {
                if let Event::Message { msg, .. } = e {
                    got = Some(msg);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(got, Some(Message::Go { start_ms: 7 }));
        assert_eq!(client.local_peer(), Some(PeerId(1)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while client
            .lobby_status()
            .is_none_or(|s| s.state != LobbyState::Connected)
        {
            assert!(Instant::now() < deadline, "lobby status follows");
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let s = lobby::files::read_status(&dir_c.join(lobby::STATUS_FILE));
            if s.is_ok_and(|s| s.is_some_and(|s| s.state == LobbyState::Stopped)) {
                break;
            }
            assert!(Instant::now() < deadline, "shutdown writes stopped");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The in-game panel's path: choices go straight to the agents through
    /// `lobby_request`, no `peer.toml`; a bad choice is reported in the status.
    #[test]
    fn lobby_workers_link_through_direct_requests() {
        let net = LoopbackNet::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/ndml-net-worker-direct");
        let _ = std::fs::remove_dir_all(&root);
        let ha: SocketAddr = "10.0.0.1:7777".parse().unwrap();
        let ca: SocketAddr = "10.0.0.2:7777".parse().unwrap();
        let make = |dir: std::path::PathBuf, at, nonce| {
            let mut cfg = LobbyConfig::new(dir, nonce);
            cfg.stun_servers = Vec::new();
            LobbyAgent::new(net.bind(at).unwrap(), cfg)
        };
        let epoch = Instant::now();
        let host = NetWorker::spawn_lobby(make(root.join("h"), ha, 0x31), epoch).unwrap();
        let client = NetWorker::spawn_lobby(make(root.join("c"), ca, 0x32), epoch).unwrap();
        // A join without a target is refused with a reason, and nothing breaks.
        client.lobby_request(LobbyRequest {
            role: RoleChoice::Join,
            ..LobbyRequest::default()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while client
            .lobby_status()
            .is_none_or(|s| s.error_kind != Some(lobby::ErrorKind::BadRequest))
        {
            assert!(Instant::now() < deadline, "bad request reported");
            std::thread::sleep(Duration::from_millis(2));
        }
        host.lobby_request(LobbyRequest {
            role: RoleChoice::Host,
            name: "Ace".into(),
            ..LobbyRequest::default()
        });
        client.lobby_request(LobbyRequest {
            role: RoleChoice::Join,
            direct: Some(ha.to_string()),
            name: "Wing".into(),
            ..LobbyRequest::default()
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut linked = (false, false);
        while linked != (true, true) {
            assert!(Instant::now() < deadline, "direct requests link");
            linked.0 |= host.events().any(|e| matches!(e, Event::Connected { .. }));
            linked.1 |= client
                .events()
                .any(|e| matches!(e, Event::Connected { .. }));
            std::thread::sleep(Duration::from_millis(2));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let s = client.lobby_status();
            if s.as_ref().is_some_and(|s| {
                s.state == LobbyState::Connected && s.name == "Wing" && s.request == 0
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "status follows: {s:?}");
            std::thread::sleep(Duration::from_millis(2));
        }
        client.lobby_request(LobbyRequest {
            disconnect: true,
            ..LobbyRequest::default()
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while client
            .lobby_status()
            .is_none_or(|s| s.state != LobbyState::Ready)
        {
            assert!(Instant::now() < deadline, "disconnect goes idle");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The game-end bound used by the co-op mod (98 ms, then Steam's 2 ms).
    const EXIT_BOUND: Duration = Duration::from_millis(98);

    /// Generous limit for a healthy shutdown in a test (far below the
    /// partner's 10 s timeout).
    const HEALTHY_SHUTDOWN: Duration = Duration::from_secs(2);

    /// A healthy shutdown completes alone: the goodbye held by the conditioner
    /// is released and arrives, and `status.toml` reads stopped.
    #[test]
    fn a_bounded_shutdown_says_goodbye_and_writes_stopped_inside_the_bound() {
        use crate::conditioner::{Conditioner, Conditions};
        let net = LoopbackNet::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/ndml-net-worker-exit");
        let _ = std::fs::remove_dir_all(&root);
        let ha: SocketAddr = "10.0.0.1:7777".parse().unwrap();
        let ca: SocketAddr = "10.0.0.2:7777".parse().unwrap();
        let cfg = |dir: std::path::PathBuf, nonce| {
            let mut cfg = LobbyConfig::new(dir, nonce);
            cfg.stun_servers = Vec::new();
            cfg
        };
        let conditions = Conditions {
            latency: Duration::from_millis(40),
            ..Conditions::default()
        };
        let held = Conditioner::new(net.bind(ha).unwrap(), conditions, 7);
        let epoch = Instant::now();
        let host = NetWorker::spawn_lobby(LobbyAgent::new(held, cfg(root.join("h"), 0x41)), epoch)
            .unwrap();
        let client = NetWorker::spawn_lobby(
            LobbyAgent::new(net.bind(ca).unwrap(), cfg(root.join("c"), 0x42)),
            epoch,
        )
        .unwrap();
        host.lobby_request(LobbyRequest {
            role: RoleChoice::Host,
            ..LobbyRequest::default()
        });
        client.lobby_request(LobbyRequest {
            role: RoleChoice::Join,
            direct: Some(ha.to_string()),
            ..LobbyRequest::default()
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut linked = (false, false);
        while linked != (true, true) {
            assert!(Instant::now() < deadline, "linked");
            linked.0 |= host.events().any(|e| matches!(e, Event::Connected { .. }));
            linked.1 |= client
                .events()
                .any(|e| matches!(e, Event::Connected { .. }));
            std::thread::sleep(Duration::from_millis(2));
        }
        let started = Instant::now();
        let stage = host.begin_shutdown().wait_until(started + HEALTHY_SHUTDOWN);
        let took = started.elapsed();
        assert_eq!(stage, ShutdownStage::Done, "after {took:?}");
        assert!(took < HEALTHY_SHUTDOWN, "{took:?}");
        let s = lobby::files::read_status(&root.join("h").join(lobby::STATUS_FILE));
        assert_eq!(s.unwrap().unwrap().state, LobbyState::Stopped);
        // Without the release the held goodbye would never arrive.
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut left = false;
        while !left && Instant::now() < deadline {
            left = client.events().any(|e| {
                matches!(
                    e,
                    Event::Disconnected {
                        reason: crate::endpoint::DisconnectReason::Remote,
                        ..
                    }
                )
            });
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(left, "the partner hears the goodbye");
        // Later commands are ignored; a second wait returns at once.
        host.lobby_discover();
        let again = Instant::now();
        assert_eq!(
            host.begin_shutdown().wait_until(again + EXIT_BOUND),
            ShutdownStage::NoAnswer
        );
        assert!(again.elapsed() < Duration::from_millis(20));
    }

    /// A driver that stalls in its shutdown or in its drop.
    struct Stalling {
        in_shutdown: Duration,
        in_drop: Duration,
    }

    impl Drop for Stalling {
        fn drop(&mut self) {
            std::thread::sleep(self.in_drop);
        }
    }

    impl Driver for Stalling {
        fn poll(&mut self, _now: Duration) -> io::Result<()> {
            Ok(())
        }
        fn next_event(&mut self) -> Option<Event> {
            None
        }
        fn send(&mut self, _peer: PeerId, _msg: &Message) -> Result<(), SendError> {
            Ok(())
        }
        fn broadcast(&mut self, _msg: &Message, _except: Option<PeerId>) -> Result<(), SendError> {
            Ok(())
        }
        fn connect(&mut self, _now: Duration, _addr: SocketAddr, _remote_nonce: Option<u32>) {}
        fn disconnect(&mut self, _now: Duration, _peer: PeerId) {}
        fn refuse(&mut self, _now: Duration, _peer: PeerId, _reason: RefuseReason) {}
        fn set_refusing(&mut self, _reason: Option<RefuseReason>) {}
        fn shutdown(&mut self, _now: Duration) {
            std::thread::sleep(self.in_shutdown);
        }
        fn local_peer(&self) -> Option<PeerId> {
            None
        }
        fn peer_infos(&self) -> Vec<(PeerId, PeerInfo)> {
            Vec::new()
        }
    }

    /// A worker stalled for 3 s does not hold the caller past [`EXIT_BOUND`]
    /// (1 s is the test's margin for a loaded machine).
    #[test]
    fn a_stuck_worker_does_not_hold_the_caller_past_the_bound() {
        let returns_by = Duration::from_secs(1);
        for (in_shutdown, in_drop, want) in [
            (
                Duration::from_secs(3),
                Duration::ZERO,
                ShutdownStage::NoAnswer,
            ),
            (Duration::ZERO, Duration::from_secs(3), ShutdownStage::Sent),
        ] {
            let w = NetWorker::spawn_driver(
                Stalling {
                    in_shutdown,
                    in_drop,
                },
                Instant::now(),
            )
            .unwrap();
            let started = Instant::now();
            let stage = w.begin_shutdown().wait_until(started + EXIT_BOUND);
            let took = started.elapsed();
            assert_eq!(stage, want);
            assert!(took >= EXIT_BOUND, "waited the bound: {took:?}");
            assert!(took < returns_by, "not the 3 s stall: {took:?}");
        }
    }
}
