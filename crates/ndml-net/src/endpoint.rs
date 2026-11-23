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

//! Connections over one transport: the hole-punch handshake, keepalives,
//! timeouts, packing messages into datagrams, and turning arrivals into
//! [`Event`]s. Driven entirely by [`Endpoint::poll`] with the current time.
//!
//! * **Handshake:** the initiator sends `Punch`, the receiver answers
//!   `PunchAck`; a side is connected on a `PunchAck` or Data from the peer.
//!   Two-way code exchange has both sides punch (opening both NATs).
//! * **Identity:** a connection pins the peer's nonce; the same address with a
//!   new nonce replaces it. The host assigns client peer ids (1, 2, ...).
//! * **Liveness:** Data at least every keepalive interval; silence for the
//!   timeout disconnects.
//! * **Refusals:** a host answers a punch it will not admit with `Refused`
//!   (full, or [`Endpoint::set_refusing`]); another protocol version counts
//!   as a version-mismatch refusal.
//! * **Rejoining** (opt-in, [`EndpointConfig::rejoin`]): a host keeps a
//!   timed-out peer's slot, and a punch from the same address (and, on UDP,
//!   nonce) gets it back even while refusing.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use crate::clock::ClockSync;
use crate::msg::{Channel, Message, MsgError, PeerId};
pub use crate::packet::RefuseReason;
use crate::packet::{
    self, Chunk, Header, MAX_DATAGRAM, PacketError, PacketKind, PacketWriter, PunchBody,
    RefusedBody,
};
use crate::reliability::{
    Arrival, LatestFilter, LossEstimator, Received, RecvWindow, ReliableReceiver, ReliableSender,
    RttEstimator, SendWindow,
};
use crate::time;

/// Which side of the star this endpoint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Owns the world, assigns peer ids, relays between clients.
    Host,
    /// Connects to one host.
    Client,
}

/// Endpoint settings.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    /// Host or client.
    pub role: Role,
    /// Nonce (random per run; goes into this side's connection code).
    pub nonce: u32,
    /// Most peers including the host.
    pub max_peers: u8,
    /// Host: accept punches from addresses nobody registered with
    /// [`Endpoint::connect`] (LAN, direct IP, one-way codes).
    pub accept_unknown: bool,
    /// Time between punches while connecting.
    pub punch_interval: Duration,
    /// Longest gap between outgoing Data packets once connected.
    pub keepalive_interval: Duration,
    /// Longest time an owed ack waits for a packet to ride on.
    pub ack_delay: Duration,
    /// Silence after which a connected peer is dropped.
    pub timeout: Duration,
    /// How long to punch before giving up.
    pub handshake_timeout: Duration,
    /// Host: keep a timed-out peer's slot (on UDP, use with
    /// [`rejoin_pins_nonce`](Self::rejoin_pins_nonce)).
    pub rejoin: Rejoin,
    /// A held slot takes back only the address *and* nonce it was held for
    /// (UDP, where an address is no identity).
    pub rejoin_pins_nonce: bool,
}

impl EndpointConfig {
    /// Defaults for `role` with `nonce`: 4 peers, punch every 100 ms, keepalive
    /// 100 ms, ack delay 10 ms, 10 s timeout, 30 s handshake, no rejoining.
    #[must_use]
    pub fn new(role: Role, nonce: u32) -> Self {
        Self {
            role,
            nonce,
            max_peers: 4,
            accept_unknown: role == Role::Host,
            punch_interval: Duration::from_millis(100),
            keepalive_interval: Duration::from_millis(100),
            ack_delay: Duration::from_millis(10),
            timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(30),
            rejoin: Rejoin::Off,
            rejoin_pins_nonce: false,
        }
    }
}

/// Whether a peer that timed out may come back into its old slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rejoin {
    /// A lost peer is forgotten.
    #[default]
    Off,
    /// Its slot is kept this long after the loss.
    Within(Duration),
    /// Its slot is kept until the endpoint is dropped.
    Always,
}

impl Rejoin {
    /// Whether rejoining is on at all.
    #[must_use]
    pub fn enabled(self) -> bool {
        self != Self::Off
    }

    /// Whether a loss `since` ago is still within the window.
    #[must_use]
    pub fn allows(self, since: Duration) -> bool {
        match self {
            Self::Off => false,
            Self::Within(w) => since <= w,
            Self::Always => true,
        }
    }
}

/// A punch on a connection up longer than this is a peer restarting its side
/// (with [`Rejoin`] on).
pub const REJOIN_PUNCH_AGE: Duration = Duration::from_secs(5);

/// A slot held for a peer that timed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reserved {
    addr: SocketAddr,
    since: Duration,
    /// The peer's nonce when it timed out (pins a UDP slot).
    nonce: Option<u32>,
}

/// Why a peer went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Nothing heard for the timeout.
    Timeout,
    /// The handshake never completed.
    HandshakeTimeout,
    /// The peer said goodbye.
    Remote,
    /// The peer came back with a new nonce (restarted); a new connection follows.
    Replaced,
    /// Ended by this side.
    Local,
    /// One side turned the other away (sent or received a `Refused` packet, or
    /// saw another protocol version).
    Refused(RefuseReason),
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("timed out"),
            Self::HandshakeTimeout => f.write_str("could not connect"),
            Self::Remote => f.write_str("left"),
            Self::Replaced => f.write_str("restarted"),
            Self::Local => f.write_str("disconnected"),
            Self::Refused(r) => write!(f, "refused: {r}"),
        }
    }
}

/// Something that happened, for the game side to handle.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The handshake completed.
    Connected {
        /// Which peer.
        peer: PeerId,
        /// Its address.
        addr: SocketAddr,
    },
    /// A message arrived (reliable ones in order, exactly once).
    Message {
        /// Sender (the host for anything relayed; see the message's own peer field).
        from: PeerId,
        /// The message.
        msg: Message,
    },
    /// A peer is gone.
    Disconnected {
        /// Which peer.
        peer: PeerId,
        /// Why.
        reason: DisconnectReason,
    },
    /// Host: a newcomer was turned away (at most once a second per address).
    Refused {
        /// Who knocked.
        addr: SocketAddr,
        /// Why they were turned away.
        reason: RefuseReason,
    },
}

/// Sending failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// No connection to that peer.
    UnknownPeer(PeerId),
    /// The message cannot be encoded or is too large.
    Encode(MsgError),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPeer(p) => write!(f, "no connection to {p}"),
            Self::Encode(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SendError {}

impl From<MsgError> for SendError {
    fn from(e: MsgError) -> Self {
        Self::Encode(e)
    }
}

/// Per-connection numbers for logs and the stats line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    /// Smoothed round-trip time.
    pub rtt: Duration,
    /// Round-trip variation.
    pub rtt_var: Duration,
    /// Interarrival jitter of the peer's packets.
    pub jitter: Duration,
    /// Recent fraction of outgoing packets lost.
    pub loss: f64,
    /// Bytes sent (UDP payload).
    pub bytes_sent: u64,
    /// Datagrams sent.
    pub packets_sent: u64,
    /// Datagrams received and accepted.
    pub packets_recv: u64,
    /// Reliable messages retransmitted.
    pub resends: u64,
    /// Reliable messages waiting for an ack.
    pub unacked: usize,
    /// Duplicate packets dropped.
    pub duplicates: u64,
    /// Stale packets or messages dropped.
    pub stale: u64,
    /// Of those, whole packets: [`crate::reliability::RECV_HISTORY`] or
    /// more behind the newest (every message in them lost).
    pub stale_packets: u64,
    /// Packets taken from behind the ack field (not acked, not stale).
    pub late_packets: u64,
    /// Of `resends`, the ones sent at once because the packet that carried
    /// them fell out of the ack window unacked (not after the timeout).
    pub fast_resends: u64,
    /// Ack-only packets sent because [`ACK_EVERY`] packets had arrived
    /// since the last send (header only, as a keepalive).
    pub ack_only: u64,
    /// Reliable messages sent a second time beside a later packet's own
    /// content ([`REPEAT_BUDGET`]).
    pub repeats: u64,
    /// Their bytes.
    pub repeat_bytes: u64,
    /// Of `duplicates`, reliable messages received a second time (a repeat
    /// or a resend) and dropped.
    pub duplicate_reliable: u64,
    /// Reliable messages beyond the receive window ([`Received::BeyondWindow`]).
    pub beyond_window: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Connecting { since: Duration },
    Connected,
}

#[derive(Debug)]
struct Connection {
    addr: SocketAddr,
    remote_nonce: Option<u32>,
    /// This end's epoch for the connection ([`Header::epoch`]), new for each
    /// connection made.
    epoch: u8,
    /// The peer's, learned from its first punch or punch answer.
    remote_epoch: Option<u8>,
    state: State,
    initiator: bool,
    connected_at: Option<Duration>,
    last_recv: Duration,
    last_send: Option<Duration>,
    last_punch: Option<Duration>,
    ack_owed_since: Option<Duration>,
    /// Packets taken since the last send to this peer (see [`ACK_EVERY`]).
    recv_since_send: u32,
    recv_window: RecvWindow,
    send_window: SendWindow,
    rtt: RttEstimator,
    loss: LossEstimator,
    clock: ClockSync,
    rel_send: ReliableSender,
    rel_recv: ReliableReceiver,
    latest: LatestFilter,
    unreliable: Vec<Vec<u8>>,
    stats: Stats,
    /// The reliable stream is broken: nothing more is sent, taken or acked,
    /// and the next expiry ends it as a timeout.
    broken: bool,
    /// Host: the connection holds a slot kept for a peer that timed out (a
    /// rejoin): if its handshake times out too, the slot is kept again.
    rejoin: bool,
}

impl Connection {
    fn new(addr: SocketAddr, remote_nonce: Option<u32>, initiator: bool, now: Duration) -> Self {
        Self {
            addr,
            remote_nonce,
            epoch: NEXT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % packet::EPOCHS,
            remote_epoch: None,
            state: State::Connecting { since: now },
            initiator,
            connected_at: None,
            last_recv: now,
            last_send: None,
            last_punch: None,
            ack_owed_since: None,
            recv_since_send: 0,
            recv_window: RecvWindow::default(),
            send_window: SendWindow::default(),
            rtt: RttEstimator::default(),
            loss: LossEstimator::default(),
            clock: ClockSync::new(),
            rel_send: ReliableSender::default(),
            rel_recv: ReliableReceiver::default(),
            latest: LatestFilter::default(),
            unreliable: Vec::new(),
            stats: Stats::default(),
            broken: false,
            rejoin: false,
        }
    }

    fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    /// The reliable messages to repeat this poll, oldest first, except `due`.
    fn repeats(&self, now: Duration, due: &[(u16, Vec<u8>)]) -> Vec<(u16, Vec<u8>)> {
        if REPEAT_BUDGET == 0 {
            return Vec::new();
        }
        self.rel_send
            .repeat_candidates(now, REPEAT_WINDOW)
            .filter(|(id, _)| !due.iter().any(|(d, _)| d == id))
            .map(|(id, b)| (id, b.to_vec()))
            .collect()
    }

    /// Put repeats into `w` (at most [`REPEAT_BUDGET`] bytes, `reserve` left
    /// free); the ones that fit move from `repeats` to `ids`.
    fn push_repeats(
        &mut self,
        w: &mut PacketWriter,
        repeats: &mut Vec<(u16, Vec<u8>)>,
        ids: &mut Vec<u16>,
        reserve: usize,
    ) {
        let mut budget = REPEAT_BUDGET;
        repeats.retain(|(id, body)| {
            let need = body.len() + packet::RELIABLE_OVERHEAD;
            if need <= budget && w.remaining() >= need + reserve && w.push_reliable(*id, body) {
                budget -= need;
                self.rel_send.mark_repeated(*id);
                ids.push(*id);
                false
            } else {
                true
            }
        });
    }

    /// Check an arriving packet's epoch: `None` drops a Data or Disconnect
    /// packet of another epoch; `Some(true)` is a newer one (1 to 15 past).
    /// A punch teaches the first epoch.
    fn take_epoch(&mut self, h: &Header) -> Option<bool> {
        let other = self.remote_epoch.is_some_and(|e| e != h.epoch);
        let newer = self.remote_epoch.is_some_and(|e| {
            (1..packet::EPOCHS / 2).contains(&(h.epoch.wrapping_sub(e) % packet::EPOCHS))
        });
        match h.kind {
            PacketKind::Data | PacketKind::Disconnect if other => return None,
            PacketKind::Punch | PacketKind::PunchAck if self.remote_epoch.is_none() => {
                self.remote_epoch = Some(h.epoch);
            }
            _ => {}
        }
        Some(newer)
    }

    /// The header of this connection's next Data packet.
    fn data_header(&self, nonce: u32, now: Duration) -> Header {
        let (ack, ack_bits) = self.recv_window.ack();
        Header {
            kind: PacketKind::Data,
            epoch: self.epoch,
            nonce,
            seq: self.send_window.peek_seq(),
            ack,
            ack_bits,
            send_time_ms: time::to_wire_ms(now),
        }
    }

    fn snapshot_stats(&self) -> Stats {
        Stats {
            rtt: self.rtt.rtt(),
            rtt_var: self.rtt.rttvar(),
            jitter: self.clock.jitter(),
            loss: self.loss.loss(),
            resends: self.rel_send.resends(),
            fast_resends: self.rel_send.fast_resends(),
            repeats: self.rel_send.repeats().0,
            repeat_bytes: self.rel_send.repeats().1,
            unacked: self.rel_send.unacked(),
            ..self.stats
        }
    }
}

/// The next connection's epoch, process-wide (wraps at [`packet::EPOCHS`]).
static NEXT_EPOCH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Most datagrams one connection sends per [`Endpoint::transmit`], so a
/// reliable backlog drains over several polls instead of one burst.
const MAX_BURST: usize = 16;

/// An ack-only packet goes out once this many packets arrived since the last
/// send, so every packet of a burst is acked (half the 32-packet field).
pub const ACK_EVERY: u32 = 16;

/// Most bytes of repeated reliable messages per packet, placed after its own
/// reliable messages and ahead of its unreliable content. Each reliable
/// message rides once more within [`REPEAT_WINDOW`], so a lost packet costs a
/// packet interval, not a round trip.
pub const REPEAT_BUDGET: usize = 600;

/// How long after its first send a reliable message may be repeated.
pub const REPEAT_WINDOW: Duration = Duration::from_millis(50);
/// Most datagrams read per [`Endpoint::receive`].
const MAX_READS: usize = 1024;

/// See the module docs.
#[derive(Debug)]
pub struct Endpoint<T> {
    transport: T,
    config: EndpointConfig,
    conns: BTreeMap<PeerId, Connection>,
    local_peer: Option<PeerId>,
    events: VecDeque<Event>,
    buf: Vec<u8>,
    stray: u64,
    /// Reliable streams found broken ([`Stats::beyond_window`] and the send
    /// queue's bound), over every connection so far.
    broken_streams: u64,
    /// Reliable messages that arrived and could not be decoded (skipped:
    /// a peer with a newer message set), for the rate limit of their line.
    undecodable: u64,
    refusing: Option<RefuseReason>,
    /// Last newcomer reported as [`Event::Refused`], to throttle the reports.
    last_refused: Option<(SocketAddr, Duration)>,
    /// Newcomers allowed in (`None` = anyone); see [`Endpoint::set_allowlist`].
    allow: Option<BTreeSet<SocketAddr>>,
    /// Slots held for peers that timed out (rejoin on).
    reserved: BTreeMap<PeerId, Reserved>,
    /// Peers owed an ack-only packet now ([`ACK_EVERY`]), sent between reads.
    ack_now: Vec<PeerId>,
}

/// Shortest gap between two [`Event::Refused`] reports for one address.
const REFUSED_REPORT_GAP: Duration = Duration::from_secs(1);

impl<T: crate::transport::Transport> Endpoint<T> {
    /// An endpoint on `transport`.
    pub fn new(transport: T, config: EndpointConfig) -> Self {
        let local_peer = (config.role == Role::Host).then_some(PeerId::HOST);
        Self {
            transport,
            config,
            conns: BTreeMap::new(),
            local_peer,
            events: VecDeque::new(),
            buf: vec![0u8; MAX_DATAGRAM + 64],
            ack_now: Vec::new(),
            stray: 0,
            broken_streams: 0,
            undecodable: 0,
            refusing: None,
            last_refused: None,
            allow: None,
            reserved: BTreeMap::new(),
        }
    }

    /// Host: admit newcomers only from these addresses (`None` = anyone);
    /// others are dropped silently.
    pub fn set_allowlist(&mut self, allow: Option<BTreeSet<SocketAddr>>) {
        self.allow = allow;
    }

    /// Change the rejoin policy (see the module docs). Turning it off forgets
    /// every reserved slot.
    pub fn set_rejoin(&mut self, rejoin: Rejoin) {
        self.config.rejoin = rejoin;
        if !rejoin.enabled() {
            self.reserved.clear();
        }
    }

    /// Pin held slots to the nonce too ([`EndpointConfig::rejoin_pins_nonce`]).
    pub fn set_rejoin_pins_nonce(&mut self, on: bool) {
        self.config.rejoin_pins_nonce = on;
    }

    /// Slots held for peers that timed out, with their addresses.
    #[must_use]
    pub fn reserved(&self) -> Vec<(PeerId, SocketAddr)> {
        self.reserved.iter().map(|(&p, r)| (p, r.addr)).collect()
    }

    /// Give up a reserved slot (the player is not coming back).
    pub fn release(&mut self, peer: PeerId) {
        self.reserved.remove(&peer);
    }

    /// Give the transport back (no goodbyes; pending events are dropped).
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Host: while `Some`, newcomers are refused with this reason; `None`
    /// admits them again.
    pub fn set_refusing(&mut self, reason: Option<RefuseReason>) {
        self.refusing = reason;
    }

    /// The current [`Self::set_refusing`] reason.
    #[must_use]
    pub fn refusing(&self) -> Option<RefuseReason> {
        self.refusing
    }

    /// Turn away `peer`: tell it why, forget it, report `Disconnected`.
    pub fn refuse(&mut self, now: Duration, peer: PeerId, reason: RefuseReason) {
        if let Some(c) = self.conns.remove(&peer) {
            self.send_refused(now, c.addr, c.remote_nonce.unwrap_or(0), reason);
            self.events.push_back(Event::Disconnected {
                peer,
                reason: DisconnectReason::Refused(reason),
            });
        }
    }

    fn send_refused(&mut self, now: Duration, to: SocketAddr, their_nonce: u32, r: RefuseReason) {
        let header = Header {
            kind: PacketKind::Refused,
            epoch: 0,
            nonce: self.config.nonce,
            seq: 0,
            ack: 0,
            ack_bits: 0,
            send_time_ms: time::to_wire_ms(now),
        };
        let mut dg = Vec::with_capacity(packet::HEADER_LEN + RefusedBody::LEN);
        header.write(&mut dg);
        RefusedBody {
            reason: r,
            expected_nonce: their_nonce,
        }
        .write(&mut dg);
        // Best effort: a lost refusal is repeated on the next punch.
        let _ = self.transport.send_to(now, to, &dg);
    }

    /// Report a turned-away newcomer, at most once per
    /// [`REFUSED_REPORT_GAP`] per address.
    fn report_refused(&mut self, now: Duration, addr: SocketAddr, reason: RefuseReason) {
        let recent = self
            .last_refused
            .is_some_and(|(a, t)| a == addr && now.saturating_sub(t) < REFUSED_REPORT_GAP);
        if !recent {
            self.last_refused = Some((addr, now));
            self.events.push_back(Event::Refused { addr, reason });
        }
    }

    /// Another protocol version: drop a connection to that address, or (host)
    /// report and refuse the newcomer.
    fn version_mismatch(&mut self, now: Duration, from: SocketAddr) {
        let known = self
            .conns
            .iter()
            .find(|(_, c)| c.addr == from)
            .map(|(&p, _)| p);
        if let Some(peer) = known {
            self.conns.remove(&peer);
            self.events.push_back(Event::Disconnected {
                peer,
                reason: DisconnectReason::Refused(RefuseReason::VersionMismatch),
            });
        } else if self.config.role == Role::Host && self.config.accept_unknown {
            self.send_refused(now, from, 0, RefuseReason::VersionMismatch);
            self.report_refused(now, from, RefuseReason::VersionMismatch);
        }
    }

    /// Start punching toward `addr` (again: restart). Returns the peer id, or
    /// `None` if a host has no free slot.
    pub fn connect(
        &mut self,
        now: Duration,
        addr: SocketAddr,
        remote_nonce: Option<u32>,
    ) -> Option<PeerId> {
        let peer = match self.config.role {
            Role::Client => PeerId::HOST,
            Role::Host => self
                .conns
                .iter()
                .find(|(_, c)| c.addr == addr)
                .map(|(&p, _)| p)
                .or_else(|| self.reserved_for(addr, remote_nonce))
                .or_else(|| self.free_peer_id())?,
        };
        self.reserved.remove(&peer);
        self.conns
            .insert(peer, Connection::new(addr, remote_nonce, true, now));
        Some(peer)
    }

    fn free_peer_id(&self) -> Option<PeerId> {
        (1..self.config.max_peers)
            .map(PeerId)
            .find(|p| !self.conns.contains_key(p) && !self.reserved.contains_key(p))
    }

    /// Local peer id: always 0 on the host; on a client, known once the host
    /// answered.
    #[must_use]
    pub fn local_peer(&self) -> Option<PeerId> {
        self.local_peer
    }

    /// Local nonce.
    #[must_use]
    pub fn nonce(&self) -> u32 {
        self.config.nonce
    }

    /// Local role.
    #[must_use]
    pub fn role(&self) -> Role {
        self.config.role
    }

    /// Connected peers.
    #[must_use]
    pub fn peers(&self) -> Vec<PeerId> {
        self.conns
            .iter()
            .filter(|(_, c)| c.is_connected())
            .map(|(&p, _)| p)
            .collect()
    }

    /// Whether `peer` is connected.
    #[must_use]
    pub fn is_connected(&self, peer: PeerId) -> bool {
        self.conns.get(&peer).is_some_and(Connection::is_connected)
    }

    /// Numbers for `peer`.
    #[must_use]
    pub fn stats(&self, peer: PeerId) -> Option<Stats> {
        self.conns.get(&peer).map(Connection::snapshot_stats)
    }

    /// The clock tracker for `peer` (on a client, `PeerId::HOST` is the host
    /// frame clock).
    #[must_use]
    pub fn clock(&self, peer: PeerId) -> Option<ClockSync> {
        self.conns.get(&peer).map(|c| c.clock)
    }

    /// Smoothed RTT to `peer`.
    #[must_use]
    pub fn rtt(&self, peer: PeerId) -> Option<Duration> {
        self.conns.get(&peer).map(|c| c.rtt.rtt())
    }

    /// Datagrams dropped as foreign or unparseable.
    #[must_use]
    pub fn stray_datagrams(&self) -> u64 {
        self.stray
    }

    /// Reliable streams found broken and reset so far (a message beyond the
    /// receive window, or the send queue at its bound; each logged at warn).
    #[must_use]
    pub fn broken_streams(&self) -> u64 {
        self.broken_streams
    }

    /// The transport.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Queue `msg` for `peer`; unreliable ones are dropped while not connected.
    pub fn send(&mut self, peer: PeerId, msg: &Message) -> Result<(), SendError> {
        let body = msg.encode()?;
        self.send_encoded(peer, msg.channel(), body)?;
        Ok(())
    }

    fn send_encoded(&mut self, peer: PeerId, ch: Channel, body: Vec<u8>) -> Result<(), SendError> {
        let conn = self
            .conns
            .get_mut(&peer)
            .ok_or(SendError::UnknownPeer(peer))?;
        match ch {
            Channel::Reliable => {
                if conn.broken {
                    // The link is being reset; the resync after the
                    // rejoin brings the state over again.
                } else if conn.rel_send.is_full() {
                    conn.broken = true;
                    self.broken_streams += 1;
                    log::warn!(
                        "co-op net: {} reliable messages to {peer} unacked behind the oldest; \
                         the link is reset (lost-link wait, rejoin, resync)",
                        conn.rel_send.unacked()
                    );
                } else {
                    conn.rel_send.push(body);
                }
            }
            Channel::Unreliable => {
                if conn.is_connected() {
                    conn.unreliable.push(body);
                }
            }
        }
        Ok(())
    }

    /// Queue `msg` for every connected peer except `except` (host relaying).
    pub fn broadcast(&mut self, msg: &Message, except: Option<PeerId>) -> Result<(), SendError> {
        let body = msg.encode()?;
        let ch = msg.channel();
        let targets: Vec<PeerId> = self
            .conns
            .iter()
            .filter(|&(&p, c)| c.is_connected() && Some(p) != except)
            .map(|(&p, _)| p)
            .collect();
        for p in targets {
            self.send_encoded(p, ch, body.clone())?;
        }
        Ok(())
    }

    /// The peer started its side over: same slot, fresh state. `restarted`
    /// also renews the local epoch and reports a timeout.
    fn renew(&mut self, now: Duration, peer: PeerId, h: &Header, restarted: bool) {
        let Some(c) = self.conns.get(&peer) else {
            return;
        };
        let mut fresh = Connection::new(c.addr, Some(h.nonce), c.initiator, now);
        fresh.remote_epoch = Some(h.epoch);
        fresh.rejoin = c.rejoin || restarted;
        if !restarted {
            fresh.epoch = c.epoch;
        }
        self.conns.insert(peer, fresh);
        if restarted {
            self.events.push_back(Event::Disconnected {
                peer,
                reason: DisconnectReason::Timeout,
            });
        }
    }

    /// Next pending event.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Say goodbye to `peer` and forget it.
    pub fn disconnect(&mut self, now: Duration, peer: PeerId) {
        if let Some(c) = self.conns.remove(&peer) {
            let h = self.header(PacketKind::Disconnect, 0, &c, now);
            let mut dg = Vec::with_capacity(packet::HEADER_LEN);
            h.write(&mut dg);
            // Best effort: a lost goodbye ends in a timeout on the other side.
            let _ = self.transport.send_to(now, c.addr, &dg);
            self.events.push_back(Event::Disconnected {
                peer,
                reason: DisconnectReason::Local,
            });
        }
    }

    /// Receive everything waiting, run timeouts, and send what is due.
    pub fn poll(&mut self, now: Duration) -> io::Result<()> {
        // A receive error is returned after expiry and sends, which still run.
        let received = self.receive(now);
        self.expire(now);
        let sent = self.transmit(now);
        received.and(sent)
    }

    /// Read and process all waiting datagrams.
    pub fn receive(&mut self, now: Duration) -> io::Result<()> {
        let mut buf = std::mem::take(&mut self.buf);
        let mut result = Ok(());
        for _ in 0..MAX_READS {
            match self.transport.recv_from(now, &mut buf) {
                Ok(Some((n, from))) => {
                    self.handle_datagram(now, from, &buf[..n]);
                    while let Some(peer) = self.ack_now.pop() {
                        if let Err(e) = self.send_ack_only(now, peer) {
                            result = Err(e);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        self.buf = buf;
        result
    }

    fn header(&self, kind: PacketKind, seq: u16, c: &Connection, now: Duration) -> Header {
        let (ack, ack_bits) = c.recv_window.ack();
        Header {
            kind,
            epoch: c.epoch,
            nonce: self.config.nonce,
            seq,
            ack,
            ack_bits,
            send_time_ms: time::to_wire_ms(now),
        }
    }

    fn handle_datagram(&mut self, now: Duration, from: SocketAddr, dg: &[u8]) {
        let (h, payload) = match Header::read(dg) {
            Ok(v) => v,
            Err(PacketError::Version(_)) => {
                self.stray += 1;
                self.version_mismatch(now, from);
                return;
            }
            Err(_) => {
                self.stray += 1;
                return;
            }
        };
        let Some(peer) = self.route(now, from, &h) else {
            self.stray += 1;
            return;
        };
        let Some(c) = self.conns.get_mut(&peer) else {
            return;
        };
        if c.remote_nonce.is_some_and(|n| n != h.nonce) {
            self.stray += 1;
            return;
        }
        let Some(new_epoch) = c.take_epoch(&h) else {
            self.stray += 1;
            return;
        };
        c.remote_nonce = Some(h.nonce);
        c.last_recv = now;

        match h.kind {
            PacketKind::Punch | PacketKind::PunchAck => {
                let Ok(body) = PunchBody::read(payload) else {
                    self.stray += 1;
                    return;
                };
                if body.expected_nonce != 0 && body.expected_nonce != self.config.nonce {
                    self.stray += 1; // meant for an earlier local run
                    return;
                }
                c.stats.packets_recv += 1;
                // A punch from a connected peer after REJOIN_PUNCH_AGE, or
                // with a new epoch (its side made a new connection).
                let restarted = h.kind == PacketKind::Punch
                    && self.config.role == Role::Host
                    && self.config.rejoin.enabled()
                    && c.connected_at
                        .is_some_and(|t| new_epoch || now.saturating_sub(t) > REJOIN_PUNCH_AGE);
                // Still connecting and a newer epoch: the peer started a new
                // attempt; keeping the old epoch would drop all its Data.
                let superseded = new_epoch && !c.is_connected();
                if restarted || superseded {
                    self.renew(now, peer, &h, restarted);
                }
                if self.config.role == Role::Client && body.assigned != PunchBody::UNASSIGNED {
                    self.local_peer = Some(PeerId(body.assigned));
                }
                if h.kind == PacketKind::Punch {
                    self.send_punch(now, peer, PacketKind::PunchAck);
                } else {
                    self.mark_connected(peer);
                }
            }
            PacketKind::Data => {
                // A connection this side started is accepted by a PunchAck
                // only: Data while still punching is from the previous link
                // (same nonce) and would leave a half-open link.
                if self
                    .conns
                    .get(&peer)
                    .is_some_and(|c| c.initiator && !c.is_connected())
                {
                    self.stray += 1;
                    return;
                }
                self.mark_connected(peer);
                self.handle_data(now, peer, &h, payload);
            }
            PacketKind::Disconnect => {
                self.conns.remove(&peer);
                self.events.push_back(Event::Disconnected {
                    peer,
                    reason: DisconnectReason::Remote,
                });
            }
            PacketKind::Refused => {
                let Ok(body) = RefusedBody::read(payload) else {
                    self.stray += 1;
                    return;
                };
                if body.expected_nonce != 0 && body.expected_nonce != self.config.nonce {
                    self.stray += 1; // meant for an earlier local run
                    return;
                }
                self.conns.remove(&peer);
                self.events.push_back(Event::Disconnected {
                    peer,
                    reason: DisconnectReason::Refused(body.reason),
                });
            }
        }
    }

    /// Find (or create) the connection a datagram belongs to.
    fn route(&mut self, now: Duration, from: SocketAddr, h: &Header) -> Option<PeerId> {
        let known = self
            .conns
            .iter()
            .find(|(_, c)| c.addr == from)
            .map(|(&p, c)| (p, c.remote_nonce));
        match known {
            Some((peer, Some(n))) if n != h.nonce => {
                // Same address, new nonce: the peer restarted. Only a punch may
                // replace an accepted connection (a code pins the nonce).
                let replaceable = h.kind == PacketKind::Punch
                    && self.config.role == Role::Host
                    && self.config.accept_unknown
                    && self.conns.get(&peer).is_some_and(|c| !c.initiator);
                if !replaceable {
                    return None;
                }
                self.conns.remove(&peer);
                self.events.push_back(Event::Disconnected {
                    peer,
                    reason: DisconnectReason::Replaced,
                });
                self.conns
                    .insert(peer, Connection::new(from, Some(h.nonce), false, now));
                Some(peer)
            }
            Some((peer, _)) => Some(peer),
            None if h.kind == PacketKind::Punch => {
                // NAT may rewrite the port: match a punched peer by nonce.
                if let Some((&peer, c)) = self
                    .conns
                    .iter_mut()
                    .find(|(_, c)| !c.is_connected() && c.remote_nonce == Some(h.nonce))
                {
                    c.addr = from;
                    return Some(peer);
                }
                if self.config.role != Role::Host || !self.config.accept_unknown {
                    return None;
                }
                // A timed-out peer gets its own slot back, even when refusing.
                if let Some(peer) = self.reserved_for(from, Some(h.nonce)) {
                    self.reserved.remove(&peer);
                    let mut c = Connection::new(from, Some(h.nonce), false, now);
                    c.rejoin = true;
                    self.conns.insert(peer, c);
                    return Some(peer);
                }
                if self.allow.as_ref().is_some_and(|a| !a.contains(&from)) {
                    return None; // not invited: no answer at all
                }
                // `Full` wins over the refusing reason.
                let refusal = match (self.free_peer_id(), self.refusing) {
                    (None, _) => Err(RefuseReason::Full),
                    (Some(_), Some(reason)) => Err(reason),
                    (Some(peer), None) => Ok(peer),
                };
                let peer = match refusal {
                    Ok(peer) => peer,
                    Err(reason) => {
                        self.send_refused(now, from, h.nonce, reason);
                        self.report_refused(now, from, reason);
                        return None;
                    }
                };
                self.conns
                    .insert(peer, Connection::new(from, Some(h.nonce), false, now));
                Some(peer)
            }
            None => None,
        }
    }

    fn mark_connected(&mut self, peer: PeerId) {
        let Some(c) = self.conns.get_mut(&peer) else {
            return;
        };
        if c.is_connected() {
            return;
        }
        c.state = State::Connected;
        c.connected_at = Some(c.last_recv);
        c.last_send = None; // send a Data packet right away so the peer connects too
        self.events
            .push_back(Event::Connected { peer, addr: c.addr });
    }

    fn handle_data(&mut self, now: Duration, peer: PeerId, h: &Header, payload: &[u8]) {
        let Some(c) = self.conns.get_mut(&peer) else {
            return;
        };
        if c.broken {
            return; // nothing more taken (or acked) on a broken stream
        }
        // A late packet is taken but not acked and gives no RTT or clock
        // sample. A damaged packet is dropped unrecorded, so it is resent.
        if packet::chunks(payload).any(|ch| ch.is_err()) {
            self.stray += 1;
            return;
        }
        let late = match c.recv_window.record(h.seq) {
            Arrival::New => false,
            Arrival::Late => true,
            Arrival::Duplicate => {
                c.stats.duplicates += 1;
                return;
            }
            Arrival::Stale => {
                c.stats.stale += 1;
                c.stats.stale_packets += 1;
                return;
            }
        };
        c.stats.packets_recv += 1;
        if late {
            c.stats.late_packets += 1;
        } else {
            c.ack_owed_since.get_or_insert(now);
            c.recv_since_send += 1;
            if c.recv_since_send == ACK_EVERY && c.is_connected() {
                self.ack_now.push(peer);
            }
        }
        let acked = c.send_window.on_ack(now, h.ack, h.ack_bits);
        if let Some(rtt) = acked.rtt.filter(|_| !late) {
            c.rtt.sample(rtt);
        }
        c.loss.record(acked.acked, acked.lost);
        c.rel_send.delivered(&acked.delivered);
        c.rel_send.resend_now(&acked.lost_reliable);
        if !late {
            c.clock.observe(time::from_wire_ms(h.send_time_ms), now);
        }

        for chunk in packet::chunks(payload) {
            match chunk {
                Ok(Chunk::Reliable { id, body }) => match c.rel_recv.receive(id, body) {
                    Received::New => {}
                    Received::Duplicate => {
                        c.stats.duplicates += 1;
                        c.stats.duplicate_reliable += 1;
                    }
                    Received::BeyondWindow => {
                        c.stats.beyond_window += 1;
                        if !c.broken {
                            c.broken = true;
                            self.broken_streams += 1;
                            log::warn!(
                                "co-op net: reliable message {id} from {peer} is beyond the \
                                 receive window (next {}); the stream is broken and the link \
                                 is reset (lost-link wait, rejoin, resync), the packet not acked",
                                c.rel_recv.next_id()
                            );
                        }
                    }
                },
                Ok(Chunk::Unreliable { body }) => match Message::decode(body) {
                    Ok(msg) => {
                        let newest = c.recv_window.newest().unwrap_or(h.seq);
                        let fresh = msg
                            .stream()
                            .is_none_or(|s| c.latest.accept(s, h.seq, newest));
                        if fresh {
                            self.events.push_back(Event::Message { from: peer, msg });
                        } else {
                            c.stats.stale += 1;
                        }
                    }
                    Err(_) => self.stray += 1,
                },
                Err(_) => {
                    self.stray += 1;
                    break;
                }
            }
        }
        self.deliver_reliable(peer);
    }

    /// Hand the reliable messages now in order to the game.
    fn deliver_reliable(&mut self, peer: PeerId) {
        let Some(c) = self.conns.get_mut(&peer) else {
            return;
        };
        while let Some(body) = c.rel_recv.pop() {
            match Message::decode(&body) {
                Ok(msg) => self.events.push_back(Event::Message { from: peer, msg }),
                // Unreadable (a newer message set): skipped, logged now and then.
                Err(e) => {
                    self.stray += 1;
                    self.undecodable += 1;
                    let n = self.undecodable;
                    if n <= 3 || n.is_multiple_of(100) {
                        log::warn!(
                            "co-op net: a reliable message from {peer} could not be read and was \
                             skipped (kind byte {}, {} bytes: {e}; {n} so far)",
                            body.first().copied().unwrap_or_default(),
                            body.len()
                        );
                    }
                }
            }
        }
    }

    fn expire(&mut self, now: Duration) {
        let mut gone = Vec::new();
        for (&peer, c) in &self.conns {
            match c.state {
                State::Connected if c.broken => gone.push((peer, DisconnectReason::Timeout)),
                State::Connected if now.saturating_sub(c.last_recv) > self.config.timeout => {
                    gone.push((peer, DisconnectReason::Timeout));
                }
                State::Connecting { since }
                    if now.saturating_sub(since) > self.config.handshake_timeout =>
                {
                    gone.push((peer, DisconnectReason::HandshakeTimeout));
                }
                _ => {}
            }
        }
        let keep = self.config.role == Role::Host && self.config.rejoin.enabled();
        for (peer, reason) in gone {
            if let Some(c) = self.conns.remove(&peer) {
                // A rejoin whose handshake timed out keeps the slot too.
                let rejoin_lost = reason == DisconnectReason::HandshakeTimeout && c.rejoin;
                if keep && (reason == DisconnectReason::Timeout || rejoin_lost) {
                    self.reserved.insert(
                        peer,
                        Reserved {
                            addr: c.addr,
                            since: now,
                            nonce: c.remote_nonce,
                        },
                    );
                }
            }
            self.events.push_back(Event::Disconnected { peer, reason });
        }
        let rejoin = self.config.rejoin;
        self.reserved
            .retain(|_, r| rejoin.allows(now.saturating_sub(r.since)));
    }

    /// The reserved slot for `addr` (and, when pinned, `nonce`).
    fn reserved_for(&self, addr: SocketAddr, nonce: Option<u32>) -> Option<PeerId> {
        self.reserved
            .iter()
            .find(|(_, r)| {
                r.addr == addr
                    && (!self.config.rejoin_pins_nonce
                        || r.nonce.zip(nonce).is_none_or(|(held, now)| held == now))
            })
            .map(|(&p, _)| p)
    }

    fn send_punch(&mut self, now: Duration, peer: PeerId, kind: PacketKind) {
        let Some(c) = self.conns.get(&peer) else {
            return;
        };
        let assigned = if self.config.role == Role::Host {
            peer.0
        } else {
            PunchBody::UNASSIGNED
        };
        let mut dg = Vec::with_capacity(packet::HEADER_LEN + PunchBody::LEN);
        self.header(kind, 0, c, now).write(&mut dg);
        PunchBody {
            assigned,
            expected_nonce: c.remote_nonce.unwrap_or(0),
        }
        .write(&mut dg);
        let addr = c.addr;
        // Lost punches are expected; the next one follows shortly.
        let _ = self.transport.send_to(now, addr, &dg);
        if let Some(c) = self.conns.get_mut(&peer) {
            c.stats.bytes_sent += dg.len() as u64;
            c.stats.packets_sent += 1;
            if kind == PacketKind::Punch {
                c.last_punch = Some(now);
            }
        }
    }

    /// Send punches, keepalives and queued messages that are due.
    pub fn transmit(&mut self, now: Duration) -> io::Result<()> {
        let peers: Vec<PeerId> = self.conns.keys().copied().collect();
        for peer in peers {
            let Some(c) = self.conns.get(&peer) else {
                continue;
            };
            match c.state {
                State::Connecting { .. } => {
                    let due = c.initiator
                        && c.last_punch
                            .is_none_or(|t| now.saturating_sub(t) >= self.config.punch_interval);
                    if due {
                        self.send_punch(now, peer, PacketKind::Punch);
                    }
                }
                State::Connected => self.send_data(now, peer)?,
            }
        }
        Ok(())
    }

    fn send_data(&mut self, now: Duration, peer: PeerId) -> io::Result<()> {
        let keepalive = self.config.keepalive_interval;
        let ack_delay = self.config.ack_delay;
        let nonce = self.config.nonce;
        let Some(c) = self.conns.get_mut(&peer) else {
            return Ok(());
        };
        if c.broken {
            return Ok(()); // no ack for what showed the break
        }
        let rto = c.rtt.rto();
        let reliable: Vec<(u16, Vec<u8>)> = c
            .rel_send
            .due(now, rto)
            .map(|(id, b)| (id, b.to_vec()))
            .collect();
        let mut repeats = c.repeats(now, &reliable);
        let unreliable = std::mem::take(&mut c.unreliable);
        let keepalive_due = c
            .last_send
            .is_none_or(|t| now.saturating_sub(t) >= keepalive);
        let ack_due = c
            .ack_owed_since
            .is_some_and(|t| now.saturating_sub(t) >= ack_delay);
        if reliable.is_empty() && unreliable.is_empty() && !keepalive_due && !ack_due {
            return Ok(());
        }

        let mut rel = reliable.into_iter().peekable();
        let mut unrel = unreliable.into_iter().peekable();
        let mut datagrams = Vec::new();
        loop {
            let mut w = PacketWriter::new(&c.data_header(nonce, now));
            let mut ids = Vec::new();
            while let Some((id, body)) = rel.peek() {
                if !w.push_reliable(*id, body) {
                    break;
                }
                ids.push(*id);
                rel.next();
            }
            // Repeats go ahead of the unreliable content, keeping room for
            // the next unreliable item (none when an own reliable message
            // did not fit an empty packet).
            let reserve = match (ids.is_empty(), rel.peek(), unrel.peek()) {
                (true, Some(_), _) => None,
                (_, _, Some(b)) => Some(b.len() + packet::UNRELIABLE_OVERHEAD),
                _ => Some(0),
            };
            let mut repeated = Vec::new();
            if let Some(reserve) = reserve {
                c.push_repeats(&mut w, &mut repeats, &mut repeated, reserve);
            }
            while let Some(body) = unrel.peek() {
                if !w.push_unreliable(body) {
                    break;
                }
                unrel.next();
            }
            if w.chunk_count() == 0 && !datagrams.is_empty() {
                // An oversized item (impossible by encoding limits): drop it.
                if rel.next().is_none() {
                    unrel.next();
                }
                if rel.peek().is_none() && unrel.peek().is_none() {
                    break;
                }
                continue;
            }
            for &id in &ids {
                c.rel_send.mark_sent(id, now);
            }
            ids.extend(repeated);
            c.send_window.record(now, ids);
            datagrams.push(w.finish());
            let more = rel.peek().is_some() || unrel.peek().is_some();
            if !more || datagrams.len() >= MAX_BURST {
                break;
            }
        }
        // Burst cap hit: unreliable leftovers wait for the next poll.
        c.unreliable.extend(unrel);
        c.last_send = Some(now);
        c.ack_owed_since = None;
        c.recv_since_send = 0;
        let addr = c.addr;
        for dg in &datagrams {
            c.stats.bytes_sent += dg.len() as u64;
            c.stats.packets_sent += 1;
        }
        for dg in datagrams {
            self.send_datagram(now, addr, &dg)?;
        }
        Ok(())
    }

    /// An ack-only packet to `peer` now ([`ACK_EVERY`]): a Data packet with
    /// no chunks, the packet a keepalive or a delayed ack already is.
    fn send_ack_only(&mut self, now: Duration, peer: PeerId) -> io::Result<()> {
        let nonce = self.config.nonce;
        let Some(c) = self.conns.get_mut(&peer) else {
            return Ok(());
        };
        if !c.is_connected() || c.broken || c.recv_since_send < ACK_EVERY {
            return Ok(());
        }
        let dg = PacketWriter::new(&c.data_header(nonce, now)).finish();
        c.send_window.record(now, Vec::new());
        c.last_send = Some(now);
        c.ack_owed_since = None;
        c.recv_since_send = 0;
        c.stats.bytes_sent += dg.len() as u64;
        c.stats.packets_sent += 1;
        c.stats.ack_only += 1;
        let addr = c.addr;
        self.send_datagram(now, addr, &dg)
    }

    /// Send one datagram; unreachable networks and the like count as loss,
    /// other errors are reported.
    fn send_datagram(&mut self, now: Duration, addr: SocketAddr, dg: &[u8]) -> io::Result<()> {
        match self.transport.send_to(now, addr, dg) {
            Err(e)
                if !matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::HostUnreachable
                        | io::ErrorKind::NetworkUnreachable
                ) =>
            {
                Err(e)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conditioner::{Conditioner, Conditions};
    use crate::msg::{EnemySnapshot, EnemyState, PlayerState};
    use crate::transport::{LoopbackNet, LoopbackSocket, Transport};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    type Ep = Endpoint<Conditioner<LoopbackSocket>>;

    fn pair(conditions: Conditions, seed: u64) -> (Ep, Ep) {
        let net = LoopbackNet::new();
        let h = Conditioner::new(net.bind(addr(1)).unwrap(), conditions, seed);
        let c = Conditioner::new(net.bind(addr(2)).unwrap(), conditions, seed ^ 0x55);
        let host = Endpoint::new(h, EndpointConfig::new(Role::Host, 0x1111));
        let mut client = Endpoint::new(c, EndpointConfig::new(Role::Client, 0x2222));
        let _ = client.connect(Duration::ZERO, addr(1), Some(0x1111));
        (host, client)
    }

    fn drain<T: Transport>(ep: &mut Endpoint<T>) -> Vec<Event> {
        std::iter::from_fn(|| ep.next_event()).collect()
    }

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn handshake_assigns_ids_and_connects_both_sides() {
        let (mut host, mut client) = pair(Conditions::default(), 1);
        assert_eq!(client.local_peer(), None);
        let mut host_events = Vec::new();
        let mut client_events = Vec::new();
        for t in 0..50 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            host_events.extend(drain(&mut host));
            client_events.extend(drain(&mut client));
        }
        assert_eq!(client.local_peer(), Some(PeerId(1)));
        assert_eq!(host.local_peer(), Some(PeerId::HOST));
        assert_eq!(
            host_events,
            vec![Event::Connected {
                peer: PeerId(1),
                addr: addr(2)
            }]
        );
        assert_eq!(
            client_events,
            vec![Event::Connected {
                peer: PeerId::HOST,
                addr: addr(1)
            }]
        );
        assert_eq!(host.peers(), vec![PeerId(1)]);
    }

    /// The acceptance test for the reliable channel: everything arrives, in
    /// order, exactly once, under 20 % loss plus reordering and duplication.
    #[test]
    fn reliable_channel_survives_loss_reordering_and_duplication() {
        let conditions = Conditions {
            latency: ms(40),
            jitter: ms(15),
            loss: 0.2,
            duplicate: 0.05,
            reorder: 0.05,
            ..Conditions::default()
        };
        let (mut host, mut client) = pair(conditions, 7);
        let mut t = 0u64;
        while !(host.is_connected(PeerId(1)) && client.is_connected(PeerId::HOST)) {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            t += 1;
            assert!(t < 10_000, "handshake under loss");
        }
        drain(&mut host);
        drain(&mut client);
        let total = 2000u16;
        let mut sent = 0u16;
        let mut got_host = Vec::new();
        let mut got_client = Vec::new();
        for t in t..t + 60_000 {
            let now = ms(t);
            // Both directions, one message per 10 ms each way.
            if t % 10 == 0 && sent < total {
                client
                    .send(PeerId::HOST, &Message::UpgradeOpen { round: sent })
                    .unwrap();
                host.send(PeerId(1), &Message::UpgradeOpen { round: sent })
                    .unwrap();
                sent += 1;
            }
            client.poll(now).unwrap();
            host.poll(now).unwrap();
            for e in drain(&mut host) {
                if let Event::Message {
                    msg: Message::UpgradeOpen { round },
                    ..
                } = e
                {
                    got_host.push(round);
                }
            }
            for e in drain(&mut client) {
                if let Event::Message {
                    msg: Message::UpgradeOpen { round },
                    ..
                } = e
                {
                    got_client.push(round);
                }
            }
            if got_client.len() == usize::from(total) && got_host.len() == usize::from(total) {
                break;
            }
        }
        let want: Vec<u16> = (0..total).collect();
        assert_eq!(got_client, want, "host -> client");
        assert_eq!(got_host, want, "client -> host");
        let s = client.stats(PeerId::HOST).unwrap();
        assert!(s.resends > 0, "loss forced resends");
        assert!(s.duplicates > 0, "duplicates were dropped");
        assert!(s.loss > 0.1 && s.loss < 0.35, "loss estimate {}", s.loss);
        assert!(
            s.rtt > ms(70) && s.rtt < ms(140),
            "rtt estimate {:?}",
            s.rtt
        );
    }

    /// A capped (183 KiB/s), overfilled uplink with 80 % loss bursts and a
    /// heavy reliable load: every message arrives once and in order, no id
    /// beyond the window arrives, and the link stays up.
    #[test]
    fn the_reliable_lane_survives_a_capped_lossy_uplink() {
        let calm = Conditions {
            latency: ms(40),
            jitter: ms(5),
            rate: 183.0 * 1024.0,
            ..Conditions::default()
        };
        let burst = Conditions { loss: 0.8, ..calm };
        let (mut host, mut client, t0) = connected(calm, 77);
        let enemies: Vec<crate::msg::EnemyState> = (0..250)
            .map(|i| crate::msg::EnemyState {
                net_id: i,
                x: f32::from(i),
                y: 1.0,
                angle: 0.0,
                hull_frac: 1.0,
                target: None,
                seg: 0,
            })
            .collect();
        // 100 a second, plus 1,200 at once at each loss burst's start.
        let total: u16 = 6000 + 10 * 1200;
        let (mut sent, mut got, mut ended) = (0u16, Vec::new(), None);
        let queue_one = |host: &mut Ep, sent: &mut u16| {
            host.send(PeerId(1), &Message::UpgradeOpen { round: *sent })
                .unwrap();
            *sent += 1;
        };
        let end = t0 + 60_000;
        let mut t = t0;
        while t < end + 60_000 {
            let phase = (t - t0) % 6000;
            let lossy = t < end && phase >= 4000;
            let c = if lossy { burst } else { calm };
            host.transport_mut().set_conditions(c);
            client.transport_mut().set_conditions(c);
            if t < end && phase == 4000 {
                for _ in 0..1200 {
                    queue_one(&mut host, &mut sent);
                }
            }
            if t < end && phase % 10 == 0 {
                queue_one(&mut host, &mut sent);
                // About 300 KB/s of snapshot parts: more than the cap.
                for part in
                    crate::msg::EnemySnapshotFine::split(crate::time::to_wire_ms(ms(t)), &enemies)
                {
                    host.send(PeerId(1), &Message::EnemySnapshotFine(part))
                        .unwrap();
                }
            }
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            drain(&mut host);
            for e in drain(&mut client) {
                match e {
                    Event::Message {
                        msg: Message::UpgradeOpen { round },
                        ..
                    } => got.push(round),
                    Event::Disconnected { reason, .. } => ended = Some(reason),
                    _ => {}
                }
            }
            assert_eq!(ended, None, "the link ended");
            t += 1;
            if t >= end && got.len() == usize::from(total) {
                break;
            }
        }
        assert_eq!(sent, total);
        let want: Vec<u16> = (0..total).collect();
        assert_eq!(got.len(), want.len(), "every message delivered");
        assert_eq!(got, want, "exactly once, in order");
        // The last acks on their way, then the host has nothing unacked.
        for _ in 0..2000 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            t += 1;
        }
        assert_eq!(client.broken_streams(), 0);
        let s = client.stats(PeerId::HOST).unwrap();
        assert_eq!(s.beyond_window, 0);
        assert!(s.duplicate_reliable > 0, "the loss forced resends");
        assert!(
            host.is_connected(PeerId(1)) && client.is_connected(PeerId::HOST),
            "alive"
        );
        let hs = host.stats(PeerId(1)).unwrap();
        assert_eq!(hs.unacked, 0, "all acked");
        assert!(
            hs.loss > 0.05,
            "the bursts and the cap lost packets: {}",
            hs.loss
        );
    }

    /// A reliable id beyond the receive window is not taken, never acked,
    /// and ends the link as a timeout.
    #[test]
    fn an_id_beyond_the_window_breaks_the_stream_and_is_never_acked() {
        let (mut host, mut client, t) = connected(Conditions::default(), 5);
        host.send(PeerId(1), &Message::UpgradeOpen { round: 1 })
            .unwrap();
        let mut t = t;
        let mut first = false;
        while !first {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            drain(&mut host);
            first = drain(&mut client).iter().any(|e| {
                matches!(
                    e,
                    Event::Message {
                        msg: Message::UpgradeOpen { .. },
                        ..
                    }
                )
            });
            t += 1;
            assert!(t < 5000, "the first message arrives");
        }
        // Forge a packet carrying reliable id 5000 (the client's next is 1).
        for _ in 0..200 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            t += 1;
        }
        drain(&mut host);
        let seq = host.conns[&PeerId(1)]
            .send_window
            .peek_seq()
            .wrapping_add(10);
        let header = Header {
            kind: PacketKind::Data,
            epoch: host.conns[&PeerId(1)].epoch,
            nonce: 0x1111,
            seq,
            ack: 0,
            ack_bits: 0,
            send_time_ms: time::to_wire_ms(ms(t)),
        };
        let mut w = PacketWriter::new(&header);
        let body = Message::UpgradeOpen { round: 9 }.encode().unwrap();
        assert!(w.push_reliable(5000, &body));
        let dg = w.finish();
        host.transport_mut()
            .inner_mut()
            .send_to(ms(t), addr(2), &dg)
            .unwrap();
        let sent_before = client.stats(PeerId::HOST).unwrap().packets_sent;
        client.poll(ms(t)).unwrap();
        let ev = drain(&mut client);
        assert!(
            ev.iter().any(|e| matches!(
                e,
                Event::Disconnected {
                    peer: PeerId::HOST,
                    reason: DisconnectReason::Timeout
                }
            )),
            "the link ends as a timeout: {ev:?}"
        );
        assert!(
            !ev.iter().any(|e| matches!(
                e,
                Event::Message {
                    msg: Message::UpgradeOpen { round: 9 },
                    ..
                }
            )),
            "not delivered"
        );
        assert_eq!(client.broken_streams(), 1);
        assert!(
            client.stats(PeerId::HOST).is_none(),
            "the connection is gone"
        );
        // Nothing went back to the host after the forged packet: no ack.
        let mut buf = [0u8; 2048];
        let mut acks = 0;
        while let Some((n, _)) = host
            .transport_mut()
            .inner_mut()
            .recv_from(ms(t), &mut buf)
            .unwrap()
        {
            if let Ok((h, _)) = Header::read(&buf[..n]) {
                let d = crate::reliability::seq_diff(h.ack, seq);
                let names_it =
                    d == 0 || ((1..=32).contains(&d) && h.ack_bits & (1u32 << (d - 1)) != 0);
                if h.kind == PacketKind::Data && names_it {
                    acks += 1;
                }
            }
        }
        assert_eq!(
            acks, 0,
            "the forged packet was never acked ({sent_before} packets before)"
        );
    }

    /// Connect a pair under `conditions`; returns the time reached.
    fn connected(conditions: Conditions, seed: u64) -> (Ep, Ep, u64) {
        let (mut host, mut client) = pair(conditions, seed);
        let mut t = 0u64;
        while !(host.is_connected(PeerId(1)) && client.is_connected(PeerId::HOST)) {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            t += 1;
            assert!(t < 10_000, "handshake");
        }
        drain(&mut host);
        drain(&mut client);
        (host, client, t)
    }

    /// Send `n` datagram-sized reliable messages at `t0`; when each reached
    /// the client (ms after `t0`; `None` = not by `t_end`).
    fn burst_arrivals(
        host: &mut Ep,
        client: &mut Ep,
        t0: u64,
        n: u32,
        t_end: u64,
    ) -> Vec<Option<u64>> {
        for i in 0..n {
            let chunk = Message::ReportTransfer(crate::msg::ReportTransfer::Chunk {
                file: 0,
                epoch: 0,
                offset: i,
                raw_len: 0,
                data: vec![0x5A; crate::msg::ReportTransfer::MAX_DATA],
            });
            host.send(PeerId(1), &chunk).unwrap();
        }
        let mut at = vec![None; n as usize];
        let mut next = 0u32;
        for t in t0..t_end {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            drain(host);
            for e in drain(client) {
                if let Event::Message {
                    msg: Message::ReportTransfer(crate::msg::ReportTransfer::Chunk { offset, .. }),
                    ..
                } = e
                {
                    assert_eq!(offset, next, "in order");
                    at[offset as usize] = Some(t - t0);
                    next += 1;
                }
            }
        }
        at
    }

    /// A burst of 100 packets that arrives shuffled (many behind the ack
    /// field) is delivered whole, in order, without waiting for a resend.
    #[test]
    fn shuffled_burst_is_delivered_without_waiting_for_a_resend() {
        let conditions = Conditions {
            latency: ms(40),
            jitter: ms(8),
            ..Conditions::default()
        };
        for seed in [11, 12, 13] {
            let (mut host, mut client, t0) = connected(conditions, seed);
            let at = burst_arrivals(&mut host, &mut client, t0, 100, t0 + 2000);
            let last = at
                .iter()
                .map(|a| a.expect("every message arrives"))
                .max()
                .unwrap();
            // About 55 ms without a resend; a resend adds a round trip.
            assert!(
                last <= 60,
                "seed {seed}: the last message came {last} ms after the burst"
            );
            let s = client.stats(PeerId::HOST).unwrap();
            assert_eq!(s.stale_packets, 0, "seed {seed}: no packet dropped whole");
            assert!(
                s.late_packets > 0,
                "seed {seed}: packets arrived behind the ack field"
            );
        }
    }

    /// A clean burst is acked whole (an ack every [`ACK_EVERY`] packets):
    /// nothing is resent.
    #[test]
    fn a_burst_is_acked_whole_without_resends() {
        let conditions = Conditions {
            latency: ms(40),
            ..Conditions::default()
        };
        let (mut host, mut client, t0) = connected(conditions, 21);
        let at = burst_arrivals(&mut host, &mut client, t0, 100, t0 + 1500);
        assert!(at.iter().all(Option::is_some), "every message arrives");
        let h = host.stats(PeerId(1)).unwrap();
        assert_eq!((h.resends, h.fast_resends), (0, 0), "nothing resent");
        let c = client.stats(PeerId::HOST).unwrap();
        assert!(
            c.ack_only >= 5,
            "acks went out during the burst: {}",
            c.ack_only
        );
    }

    /// A transport that drops the `nth` datagram sent after it is armed.
    struct DropNth<T> {
        inner: T,
        armed: std::rc::Rc<std::cell::Cell<Option<u32>>>,
    }

    impl<T: Transport> Transport for DropNth<T> {
        fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
            if let Some(n) = self.armed.get() {
                if n == 0 {
                    self.armed.set(None);
                    return Ok(());
                }
                self.armed.set(Some(n - 1));
            }
            self.inner.send_to(now, to, data)
        }

        fn recv_from(
            &mut self,
            now: Duration,
            buf: &mut [u8],
        ) -> io::Result<Option<(usize, SocketAddr)>> {
            self.inner.recv_from(now, buf)
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }
    }

    /// A packet lost mid-burst on a jittery link is resent once the acks show
    /// it fell out of the window, before its timeout; all arrive in order.
    #[test]
    fn a_packet_lost_in_a_burst_is_resent_before_its_timeout() {
        type DropEp = Endpoint<DropNth<Conditioner<LoopbackSocket>>>;
        let conditions = Conditions {
            latency: ms(40),
            jitter: ms(15),
            ..Conditions::default()
        };
        let net = LoopbackNet::new();
        let armed = std::rc::Rc::new(std::cell::Cell::new(None));
        let h = DropNth {
            inner: Conditioner::new(net.bind(addr(1)).unwrap(), conditions, 31),
            armed: armed.clone(),
        };
        let c = DropNth {
            inner: Conditioner::new(net.bind(addr(2)).unwrap(), conditions, 32),
            armed: std::rc::Rc::new(std::cell::Cell::new(None)),
        };
        let mut host: DropEp = Endpoint::new(h, EndpointConfig::new(Role::Host, 0x1111));
        let mut client: DropEp = Endpoint::new(c, EndpointConfig::new(Role::Client, 0x2222));
        let _ = client.connect(Duration::ZERO, addr(1), Some(0x1111));
        // Connect and settle the round-trip estimate.
        let mut t = 0u64;
        while t < 3000 || !(host.is_connected(PeerId(1)) && client.is_connected(PeerId::HOST)) {
            if t.is_multiple_of(20)
                && host.is_connected(PeerId(1))
                && client.is_connected(PeerId::HOST)
            {
                host.send(PeerId(1), &Message::UpgradeOpen { round: 0 })
                    .unwrap();
                client
                    .send(PeerId::HOST, &Message::UpgradeOpen { round: 0 })
                    .unwrap();
            }
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            drain(&mut host);
            drain(&mut client);
            t += 1;
            assert!(t < 20_000, "handshake");
        }
        for _ in 0..200 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            t += 1;
        }
        drain(&mut host);
        drain(&mut client);
        let s0 = host.stats(PeerId(1)).unwrap();
        let rto = (s0.rtt + 4 * s0.rtt_var)
            .clamp(crate::reliability::MIN_RTO, crate::reliability::MAX_RTO);
        // The burst's 10th datagram is lost.
        armed.set(Some(9));
        let t0 = t;
        let n = 100u32;
        for i in 0..n {
            let chunk = Message::ReportTransfer(crate::msg::ReportTransfer::Chunk {
                file: 0,
                epoch: 0,
                offset: i,
                raw_len: 0,
                data: vec![0x5A; crate::msg::ReportTransfer::MAX_DATA],
            });
            host.send(PeerId(1), &chunk).unwrap();
        }
        let mut arrived = 0u32;
        let mut first_fast = None;
        for t in t0..t0 + 3000 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            drain(&mut host);
            for e in drain(&mut client) {
                if let Event::Message {
                    msg: Message::ReportTransfer(crate::msg::ReportTransfer::Chunk { offset, .. }),
                    ..
                } = e
                {
                    assert_eq!(offset, arrived, "in order");
                    arrived += 1;
                }
            }
            if first_fast.is_none() && host.stats(PeerId(1)).unwrap().fast_resends > 0 {
                first_fast = Some(t - t0);
            }
        }
        assert_eq!(arrived, n, "every message arrives");
        assert_eq!(armed.get(), None, "the 10th datagram was dropped");
        let fast = first_fast.expect("a fast resend");
        assert!(
            u128::from(fast) < rto.as_millis(),
            "resent {fast} ms after the burst, the timeout was {} ms",
            rto.as_millis()
        );
    }

    /// A transport that drops every `every`th datagram it sends.
    struct DropEvery<T> {
        inner: T,
        every: u32,
        sent: u32,
        dropped: std::rc::Rc<std::cell::Cell<u32>>,
    }

    impl<T: Transport> Transport for DropEvery<T> {
        fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
            self.sent += 1;
            if self.sent.is_multiple_of(self.every) {
                self.dropped.set(self.dropped.get() + 1);
                return Ok(());
            }
            self.inner.send_to(now, to, data)
        }

        fn recv_from(
            &mut self,
            now: Duration,
            buf: &mut [u8],
        ) -> io::Result<Option<(usize, SocketAddr)>> {
            self.inner.recv_from(now, buf)
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }
    }

    /// Every 10th datagram lost: each message still arrives within one packet
    /// interval plus the way, because the next packet repeats it.
    #[test]
    fn a_lost_packet_costs_a_packet_interval_not_a_round_trip() {
        type DropEp = Endpoint<DropEvery<Conditioner<LoopbackSocket>>>;
        let conditions = Conditions {
            latency: ms(40),
            ..Conditions::default()
        };
        let net = LoopbackNet::new();
        let dropped = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut host: DropEp = Endpoint::new(
            DropEvery {
                inner: Conditioner::new(net.bind(addr(1)).unwrap(), conditions, 41),
                every: u32::MAX,
                sent: 0,
                dropped: dropped.clone(),
            },
            EndpointConfig::new(Role::Host, 0x1111),
        );
        let mut client: DropEp = Endpoint::new(
            DropEvery {
                inner: Conditioner::new(net.bind(addr(2)).unwrap(), conditions, 42),
                every: u32::MAX,
                sent: 0,
                dropped: std::rc::Rc::new(std::cell::Cell::new(0)),
            },
            EndpointConfig::new(Role::Client, 0x2222),
        );
        let _ = client.connect(Duration::ZERO, addr(1), Some(0x1111));
        let mut t = 0u64;
        while !(host.is_connected(PeerId(1)) && client.is_connected(PeerId::HOST)) {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            t += 1;
            assert!(t < 10_000, "handshake");
        }
        drain(&mut host);
        drain(&mut client);
        host.transport.every = 10;
        let t0 = t;
        let total = 300u16;
        let mut sent_at = Vec::new();
        let mut worst = 0u64;
        let mut got = 0u16;
        for t in t0..t0 + 7000 {
            if (t - t0).is_multiple_of(16) && sent_at.len() < usize::from(total) {
                let round = sent_at.len() as u16;
                host.send(PeerId(1), &Message::UpgradeOpen { round })
                    .unwrap();
                sent_at.push(t);
            }
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            drain(&mut host);
            for e in drain(&mut client) {
                if let Event::Message {
                    msg: Message::UpgradeOpen { round },
                    ..
                } = e
                {
                    assert_eq!(round, got, "in order");
                    worst = worst.max(t - sent_at[usize::from(round)]);
                    got += 1;
                }
            }
        }
        assert_eq!(got, total, "every message arrives");
        assert!(dropped.get() >= 25, "packets were lost: {}", dropped.get());
        let s = host.stats(PeerId(1)).unwrap();
        assert!(
            s.repeats >= u64::from(total) - 5,
            "each message rode twice: {}",
            s.repeats
        );
        // The way, one packet interval and a poll or two.
        assert!(
            worst <= 58,
            "a message waited {worst} ms (40 ms is the way)"
        );
        let c = client.stats(PeerId::HOST).unwrap();
        assert!(
            c.duplicate_reliable > 0,
            "the repeats of delivered messages were dropped"
        );
    }

    /// A repeat goes ahead of unreliable content; nothing displaced is lost.
    #[test]
    fn repeats_go_ahead_of_unreliable_content() {
        let (mut host, mut client) = pair(Conditions::default(), 5);
        for t in 0..20 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        drain(&mut host);
        drain(&mut client);
        // A reliable message of 300 bytes goes out at 20 ms (its first send).
        let chunk = Message::ReportTransfer(crate::msg::ReportTransfer::Chunk {
            file: 0,
            epoch: 0,
            offset: 0,
            raw_len: 0,
            data: vec![0x5A; 300],
        });
        host.send(PeerId(1), &chunk).unwrap();
        host.poll(ms(20)).unwrap();
        let before = host.stats(PeerId(1)).unwrap();
        // Four snapshot parts of about 560 bytes (two fill a packet): the
        // repeat goes first and the displaced part takes one packet more.
        let n = 4;
        for part in 0..n {
            let enemies = (0..50)
                .map(|i| EnemyState {
                    seg: 0,
                    net_id: i,
                    x: 1.0,
                    y: 2.0,
                    angle: 0.0,
                    hull_frac: 1.0,
                    target: None,
                })
                .collect();
            let snap = EnemySnapshot {
                time_ms: 9,
                part,
                parts: n,
                enemies,
            };
            host.send(PeerId(1), &Message::EnemySnapshot(snap)).unwrap();
        }
        host.poll(ms(21)).unwrap();
        let after = host.stats(PeerId(1)).unwrap();
        assert_eq!(after.repeats - before.repeats, 1, "the repeat went out");
        assert_eq!(
            after.packets_sent - before.packets_sent,
            3,
            "the displaced part takes one packet more"
        );
        let n = usize::from(n);
        client.poll(ms(22)).unwrap();
        let got = drain(&mut client)
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::Message {
                        msg: Message::EnemySnapshot(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(got, n, "every part arrives");
        // Nothing left to repeat and nothing of its own to send: no packet.
        let quiet = host.stats(PeerId(1)).unwrap().packets_sent;
        host.poll(ms(23)).unwrap();
        assert_eq!(host.stats(PeerId(1)).unwrap().packets_sent, quiet);
    }

    /// Past `MAX_BURST` the displaced unreliable content goes in the next
    /// poll, in order, none dropped.
    #[test]
    fn displaced_content_past_the_burst_cap_goes_next_poll() {
        let (mut host, mut client) = pair(Conditions::default(), 6);
        for t in 0..20 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        drain(&mut host);
        drain(&mut client);
        for i in 0..12u32 {
            let chunk = Message::ReportTransfer(crate::msg::ReportTransfer::Chunk {
                file: 0,
                epoch: 0,
                offset: i,
                raw_len: 0,
                data: vec![0x5A; 550],
            });
            host.send(PeerId(1), &chunk).unwrap();
        }
        host.poll(ms(20)).unwrap();
        // 40 parts, two to a packet: over the cap.
        let n = 40;
        for part in 0..n {
            let enemies = (0..50)
                .map(|i| EnemyState {
                    seg: 0,
                    net_id: i,
                    x: 1.0,
                    y: 2.0,
                    angle: 0.0,
                    hull_frac: 1.0,
                    target: None,
                })
                .collect();
            let snap = EnemySnapshot {
                time_ms: 9,
                part,
                parts: n,
                enemies,
            };
            host.send(PeerId(1), &Message::EnemySnapshot(snap)).unwrap();
        }
        let n = usize::from(n);
        let s0 = host.stats(PeerId(1)).unwrap();
        host.poll(ms(21)).unwrap();
        let s1 = host.stats(PeerId(1)).unwrap();
        assert_eq!(
            s1.packets_sent - s0.packets_sent,
            MAX_BURST as u64,
            "the cap holds"
        );
        assert!(s1.repeats > s0.repeats, "repeats went ahead of the parts");
        host.poll(ms(22)).unwrap();
        host.poll(ms(23)).unwrap();
        client.poll(ms(24)).unwrap();
        let got = drain(&mut client)
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::Message {
                        msg: Message::EnemySnapshot(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            got, n,
            "every part arrives, the displaced ones a poll later"
        );
    }

    #[test]
    fn unreliable_drops_stale_and_snapshot_parts_fit() {
        let (mut host, mut client) = pair(Conditions::default(), 3);
        for t in 0..20 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        drain(&mut host);
        drain(&mut client);
        let enemies: Vec<EnemyState> = (0..250)
            .map(|i| EnemyState {
                seg: 0,
                net_id: i,
                x: 1.0,
                y: 2.0,
                angle: 0.0,
                hull_frac: 1.0,
                target: None,
            })
            .collect();
        for part in EnemySnapshot::split(5, &enemies) {
            host.send(PeerId(1), &Message::EnemySnapshot(part)).unwrap();
        }
        let ps = PlayerState {
            seg: 0,
            peer: PeerId::HOST,
            time_ms: 5,
            x: 0.0,
            y: 0.0,
            vx: 0.0,
            vy: 0.0,
            angle: 0.0,
            sprite: 0,
            frame: 0,
            firing: false,
            hull: 1.0,
            downed: false,
        };
        host.send(PeerId(1), &Message::PlayerState(ps)).unwrap();
        host.poll(ms(30)).unwrap();
        let sent = host.stats(PeerId(1)).unwrap();
        client.poll(ms(30)).unwrap();
        let msgs: Vec<_> = drain(&mut client);
        assert_eq!(msgs.len(), 4, "three parts and a player state");
        // Each part went in its own datagram, all under the cap.
        assert!(sent.packets_sent >= 3);
        assert!(sent.bytes_sent / sent.packets_sent <= MAX_DATAGRAM as u64);
    }

    #[test]
    fn timeout_and_goodbye() {
        let (mut host, mut client) = pair(Conditions::default(), 4);
        for t in 0..20 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        drain(&mut host);
        drain(&mut client);
        // Client goes silent; host times it out after 10 s.
        host.poll(ms(5_000)).unwrap();
        assert!(drain(&mut host).is_empty());
        host.poll(ms(10_100)).unwrap();
        assert_eq!(
            drain(&mut host),
            vec![Event::Disconnected {
                peer: PeerId(1),
                reason: DisconnectReason::Timeout
            }]
        );
        // Client says goodbye explicitly to a (new) host connection.
        let (mut host, mut client) = pair(Conditions::default(), 5);
        for t in 0..20 {
            client.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        drain(&mut host);
        client.disconnect(ms(20), PeerId::HOST);
        host.poll(ms(21)).unwrap();
        assert_eq!(
            drain(&mut host),
            vec![Event::Disconnected {
                peer: PeerId(1),
                reason: DisconnectReason::Remote
            }]
        );
    }

    #[test]
    fn handshake_times_out_without_a_peer() {
        let net = LoopbackNet::new();
        let mut c = Endpoint::new(
            net.bind(addr(2)).unwrap(),
            EndpointConfig::new(Role::Client, 9),
        );
        let _ = c.connect(ms(0), addr(1), None);
        for t in (0..31_000).step_by(100) {
            c.poll(ms(t)).unwrap();
        }
        let ev: Vec<_> = std::iter::from_fn(|| c.next_event()).collect();
        assert_eq!(
            ev,
            vec![Event::Disconnected {
                peer: PeerId::HOST,
                reason: DisconnectReason::HandshakeTimeout
            }]
        );
    }

    #[test]
    fn two_way_punch_and_foreign_nonces() {
        let net = LoopbackNet::new();
        let mut host = Endpoint::new(net.bind(addr(1)).unwrap(), {
            let mut c = EndpointConfig::new(Role::Host, 0xAAAA);
            c.accept_unknown = false;
            c
        });
        let mut client = Endpoint::new(
            net.bind(addr(2)).unwrap(),
            EndpointConfig::new(Role::Client, 0xBBBB),
        );
        let mut stranger = Endpoint::new(
            net.bind(addr(3)).unwrap(),
            EndpointConfig::new(Role::Client, 0xCCCC),
        );
        // Both sides punch (code exchange); the stranger is not admitted.
        assert_eq!(host.connect(ms(0), addr(2), Some(0xBBBB)), Some(PeerId(1)));
        let _ = client.connect(ms(0), addr(1), Some(0xAAAA));
        let _ = stranger.connect(ms(0), addr(1), None);
        for t in 0..300 {
            host.poll(ms(t)).unwrap();
            client.poll(ms(t)).unwrap();
            stranger.poll(ms(t)).unwrap();
        }
        assert_eq!(host.peers(), vec![PeerId(1)]);
        assert!(client.is_connected(PeerId::HOST));
        assert!(!stranger.is_connected(PeerId::HOST));
        assert!(host.stray_datagrams() > 0);
        assert_eq!(client.local_peer(), Some(PeerId(1)));
    }

    #[test]
    fn restarted_client_replaces_its_connection() {
        let net = LoopbackNet::new();
        let mut host = Endpoint::new(
            net.bind(addr(1)).unwrap(),
            EndpointConfig::new(Role::Host, 1),
        );
        let mut c1 = Endpoint::new(
            net.bind(addr(2)).unwrap(),
            EndpointConfig::new(Role::Client, 2),
        );
        let _ = c1.connect(ms(0), addr(1), None);
        for t in 0..20 {
            c1.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        drop(c1);
        let mut c2 = Endpoint::new(
            net.bind(addr(2)).unwrap(),
            EndpointConfig::new(Role::Client, 3),
        );
        let _ = c2.connect(ms(20), addr(1), None);
        for t in 20..60 {
            c2.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        let ev: Vec<_> = std::iter::from_fn(|| host.next_event()).collect();
        assert!(ev.contains(&Event::Disconnected {
            peer: PeerId(1),
            reason: DisconnectReason::Replaced
        }));
        assert!(c2.is_connected(PeerId::HOST));
        assert_eq!(host.peers(), vec![PeerId(1)]);
    }

    #[test]
    fn star_with_three_clients_and_a_full_house() {
        let net = LoopbackNet::new();
        let mut host = Endpoint::new(
            net.bind(addr(1)).unwrap(),
            EndpointConfig::new(Role::Host, 1),
        );
        let mut clients: Vec<_> = (0..4u16)
            .map(|i| {
                let mut c = Endpoint::new(
                    net.bind(addr(10 + i)).unwrap(),
                    EndpointConfig::new(Role::Client, 100 + u32::from(i)),
                );
                let _ = c.connect(ms(u64::from(i) * 50), addr(1), Some(1));
                c
            })
            .collect();
        for t in 0..400 {
            host.poll(ms(t)).unwrap();
            for c in &mut clients {
                c.poll(ms(t)).unwrap();
            }
        }
        // max_peers = 4: three clients get ids 1..=3, the fourth is not admitted.
        assert_eq!(host.peers(), vec![PeerId(1), PeerId(2), PeerId(3)]);
        let ids: Vec<_> = clients.iter().map(Endpoint::local_peer).collect();
        assert_eq!(
            ids,
            vec![Some(PeerId(1)), Some(PeerId(2)), Some(PeerId(3)), None]
        );
        // Host relays P2's pause to everyone but P2.
        host.broadcast(&Message::Pause { by: PeerId(1) }, Some(PeerId(1)))
            .unwrap();
        for t in 400..450 {
            host.poll(ms(t)).unwrap();
            for c in &mut clients {
                c.poll(ms(t)).unwrap();
            }
        }
        let got: Vec<bool> = clients
            .iter_mut()
            .map(|c| {
                std::iter::from_fn(|| c.next_event()).any(|e| {
                    matches!(
                        e,
                        Event::Message {
                            msg: Message::Pause { .. },
                            ..
                        }
                    )
                })
            })
            .collect();
        assert_eq!(got, vec![false, true, true, false]);
    }

    fn events_of(ep: &mut Endpoint<LoopbackSocket>) -> Vec<Event> {
        std::iter::from_fn(|| ep.next_event()).collect()
    }

    #[test]
    fn a_full_host_refuses_at_once_instead_of_timing_out() {
        let net = LoopbackNet::new();
        let mut cfg = EndpointConfig::new(Role::Host, 1);
        cfg.max_peers = 2;
        let mut host = Endpoint::new(net.bind(addr(1)).unwrap(), cfg);
        let mut first = Endpoint::new(
            net.bind(addr(2)).unwrap(),
            EndpointConfig::new(Role::Client, 2),
        );
        let mut third = Endpoint::new(
            net.bind(addr(3)).unwrap(),
            EndpointConfig::new(Role::Client, 3),
        );
        let _ = first.connect(ms(0), addr(1), None);
        for t in 0..20 {
            first.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        assert_eq!(host.peers(), vec![PeerId(1)]);
        events_of(&mut host);
        let _ = third.connect(ms(20), addr(1), None);
        for t in 20..30 {
            third.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
            first.poll(ms(t)).unwrap();
        }
        assert_eq!(
            events_of(&mut third),
            vec![Event::Disconnected {
                peer: PeerId::HOST,
                reason: DisconnectReason::Refused(RefuseReason::Full)
            }]
        );
        assert_eq!(
            events_of(&mut host),
            vec![Event::Refused {
                addr: addr(3),
                reason: RefuseReason::Full
            }]
        );
        assert!(first.is_connected(PeerId::HOST), "the admitted peer stays");
    }

    #[test]
    fn refusing_newcomers_and_refusing_a_connected_peer() {
        let net = LoopbackNet::new();
        let mut host = Endpoint::new(
            net.bind(addr(1)).unwrap(),
            EndpointConfig::new(Role::Host, 1),
        );
        let mut early = Endpoint::new(
            net.bind(addr(2)).unwrap(),
            EndpointConfig::new(Role::Client, 2),
        );
        let _ = early.connect(ms(0), addr(1), None);
        for t in 0..20 {
            early.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        events_of(&mut host);
        // Run starts: newcomers are turned away with the reason.
        host.set_refusing(Some(RefuseReason::RunInProgress));
        assert_eq!(host.refusing(), Some(RefuseReason::RunInProgress));
        let mut late = Endpoint::new(
            net.bind(addr(3)).unwrap(),
            EndpointConfig::new(Role::Client, 3),
        );
        let _ = late.connect(ms(20), addr(1), None);
        for t in 20..30 {
            late.poll(ms(t)).unwrap();
            host.poll(ms(t)).unwrap();
        }
        assert_eq!(
            events_of(&mut late),
            vec![Event::Disconnected {
                peer: PeerId::HOST,
                reason: DisconnectReason::Refused(RefuseReason::RunInProgress)
            }]
        );
        // The game finds a version mismatch in Hello and refuses the peer.
        host.refuse(ms(30), PeerId(1), RefuseReason::VersionMismatch);
        early.poll(ms(31)).unwrap();
        let want = DisconnectReason::Refused(RefuseReason::VersionMismatch);
        assert!(events_of(&mut early).contains(&Event::Disconnected {
            peer: PeerId::HOST,
            reason: want
        }));
        assert!(events_of(&mut host).contains(&Event::Disconnected {
            peer: PeerId(1),
            reason: want
        }));
        assert!(host.peers().is_empty());
    }

    #[test]
    fn stale_refusals_are_ignored_and_other_versions_are_reported() {
        let net = LoopbackNet::new();
        let mut client = Endpoint::new(
            net.bind(addr(2)).unwrap(),
            EndpointConfig::new(Role::Client, 2),
        );
        let mut fake = net.bind(addr(1)).unwrap();
        let _ = client.connect(ms(0), addr(1), None);
        client.poll(ms(0)).unwrap();
        // A refusal addressed to another nonce (an earlier local run).
        let mut dg = Vec::new();
        Header {
            kind: PacketKind::Refused,
            epoch: 0,
            nonce: 9,
            seq: 0,
            ack: 0,
            ack_bits: 0,
            send_time_ms: 0,
        }
        .write(&mut dg);
        RefusedBody {
            reason: RefuseReason::Full,
            expected_nonce: 777,
        }
        .write(&mut dg);
        fake.send_to(ms(1), addr(2), &dg).unwrap();
        client.poll(ms(1)).unwrap();
        assert!(events_of(&mut client).is_empty());
        // A packet with the right magic but protocol version 99.
        let mut other = dg.clone();
        other[2] = 99;
        fake.send_to(ms(2), addr(2), &other).unwrap();
        client.poll(ms(2)).unwrap();
        assert_eq!(
            events_of(&mut client),
            vec![Event::Disconnected {
                peer: PeerId::HOST,
                reason: DisconnectReason::Refused(RefuseReason::VersionMismatch)
            }]
        );
        // A host answers a newcomer of another version with a refusal.
        let mut host = Endpoint::new(
            net.bind(addr(3)).unwrap(),
            EndpointConfig::new(Role::Host, 3),
        );
        fake.send_to(ms(4), addr(3), &other).unwrap();
        host.poll(ms(4)).unwrap();
        assert!(events_of(&mut host).contains(&Event::Refused {
            addr: addr(1),
            reason: RefuseReason::VersionMismatch
        }));
        // (The client's punches to the fake are queued before it.)
        let mut buf = [0u8; 64];
        let mut answer = None;
        while let Some((n, from)) = fake.recv_from(ms(5), &mut buf).unwrap() {
            if from == addr(3) {
                answer = Some(Header::read(&buf[..n]).unwrap().0.kind);
            }
        }
        assert_eq!(answer, Some(PacketKind::Refused));
        // The socket survives the endpoint.
        let mut sock = client.into_transport();
        assert_eq!(sock.local_addr().unwrap(), addr(2));
        let mut buf = [0u8; 64];
        assert!(sock.recv_from(ms(3), &mut buf).unwrap().is_none());
    }

    type Lep = Endpoint<LoopbackSocket>;

    fn rejoin_host(net: &LoopbackNet, rejoin: Rejoin) -> Lep {
        let mut cfg = EndpointConfig::new(Role::Host, 1);
        cfg.max_peers = 2;
        cfg.rejoin = rejoin;
        Endpoint::new(net.bind(addr(1)).unwrap(), cfg)
    }

    fn client_at(net: &LoopbackNet, port: u16, nonce: u32, now: Duration) -> Lep {
        let mut c = Endpoint::new(
            net.bind(addr(port)).unwrap(),
            EndpointConfig::new(Role::Client, nonce),
        );
        let _ = c.connect(now, addr(1), None);
        c
    }

    fn run_eps(eps: &mut [&mut Lep], from: u64, to: u64) {
        for t in from..to {
            for e in eps.iter_mut() {
                e.poll(ms(t)).unwrap();
            }
        }
    }

    /// A client whose every receive fails still sends its keepalives.
    #[test]
    fn a_receive_error_does_not_stop_the_polls_sends() {
        struct Deaf<T>(T, bool);
        impl<T: Transport> Transport for Deaf<T> {
            fn send_to(&mut self, now: Duration, to: SocketAddr, d: &[u8]) -> io::Result<()> {
                self.0.send_to(now, to, d)
            }
            fn recv_from(
                &mut self,
                now: Duration,
                buf: &mut [u8],
            ) -> io::Result<Option<(usize, SocketAddr)>> {
                if self.1 {
                    return Err(io::Error::other("message too long"));
                }
                self.0.recv_from(now, buf)
            }
            fn local_addr(&self) -> io::Result<SocketAddr> {
                self.0.local_addr()
            }
        }
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Off);
        let mut c = Endpoint::new(
            Deaf(net.bind(addr(2)).unwrap(), false),
            EndpointConfig::new(Role::Client, 2),
        );
        let _ = c.connect(ms(0), addr(1), None);
        for t in 0..50 {
            host.poll(ms(t)).unwrap();
            c.poll(ms(t)).unwrap();
        }
        assert!(host.is_connected(PeerId(1)));
        c.transport_mut().1 = true;
        let before = host.stats(PeerId(1)).unwrap().packets_recv;
        for t in 50..2_000 {
            host.poll(ms(t)).unwrap();
            assert!(c.poll(ms(t)).is_err(), "the error is still returned");
        }
        let after = host.stats(PeerId(1)).unwrap().packets_recv;
        assert!(
            after >= before + 15,
            "keepalives kept coming: {before} -> {after}"
        );
    }

    #[test]
    fn a_lost_peer_gets_its_slot_back_and_strangers_still_cannot_get_in() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Always);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        assert_eq!(host.peers(), vec![PeerId(1)]);
        events_of(&mut host);
        host.set_refusing(Some(RefuseReason::RunInProgress));
        // The cable is pulled: the client vanishes (its socket closes).
        drop(c1);
        for t in (30..10_200).step_by(50) {
            host.poll(ms(t)).unwrap();
        }
        assert_eq!(
            events_of(&mut host),
            vec![Event::Disconnected {
                peer: PeerId(1),
                reason: DisconnectReason::Timeout
            }]
        );
        assert_eq!(host.reserved(), vec![(PeerId(1), addr(2))]);
        // A stranger is refused: the held slot makes the house full.
        let mut stranger = client_at(&net, 3, 3, ms(10_200));
        run_eps(&mut [&mut host, &mut stranger], 10_200, 10_220);
        assert_eq!(
            events_of(&mut stranger),
            vec![Event::Disconnected {
                peer: PeerId::HOST,
                reason: DisconnectReason::Refused(RefuseReason::Full)
            }]
        );
        events_of(&mut host);
        // Without the run lock, still full: the slot is held.
        host.set_refusing(None);
        let mut stranger = {
            drop(stranger);
            client_at(&net, 3, 4, ms(10_220))
        };
        run_eps(&mut [&mut host, &mut stranger], 10_220, 10_240);
        assert!(events_of(&mut stranger).contains(&Event::Disconnected {
            peer: PeerId::HOST,
            reason: DisconnectReason::Refused(RefuseReason::Full)
        }));
        events_of(&mut host);
        // Back three minutes later, same address, new nonce: same slot.
        host.set_refusing(Some(RefuseReason::RunInProgress));
        for t in (10_240..190_000).step_by(1000) {
            host.poll(ms(t)).unwrap();
        }
        let mut back = client_at(&net, 2, 99, ms(190_000));
        run_eps(&mut [&mut host, &mut back], 190_000, 190_030);
        assert_eq!(
            events_of(&mut host),
            vec![Event::Connected {
                peer: PeerId(1),
                addr: addr(2)
            }]
        );
        assert_eq!(back.local_peer(), Some(PeerId(1)));
        assert!(host.reserved().is_empty());
        // And data flows again.
        host.send(PeerId(1), &Message::Go { start_ms: 5 }).unwrap();
        run_eps(&mut [&mut host, &mut back], 190_030, 190_040);
        assert!(events_of(&mut back).contains(&Event::Message {
            from: PeerId::HOST,
            msg: Message::Go { start_ms: 5 }
        }));
    }

    #[test]
    fn the_rejoin_window_ends_and_off_means_forgotten() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Within(ms(5_000)));
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        drop(c1);
        for t in (30..10_200).step_by(50) {
            host.poll(ms(t)).unwrap();
        }
        assert_eq!(host.reserved().len(), 1);
        host.poll(ms(14_000)).unwrap();
        assert_eq!(host.reserved().len(), 1, "still inside 5 s");
        host.poll(ms(15_300)).unwrap();
        assert!(host.reserved().is_empty(), "window over");
        let mut other = client_at(&net, 3, 3, ms(15_300));
        run_eps(&mut [&mut host, &mut other], 15_300, 15_330);
        assert_eq!(
            other.local_peer(),
            Some(PeerId(1)),
            "the slot is free again"
        );

        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Off);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        drop(c1);
        for t in (30..10_200).step_by(50) {
            host.poll(ms(t)).unwrap();
        }
        assert!(host.reserved().is_empty(), "off: nothing is kept");
        let mut host2 = rejoin_host(&LoopbackNet::new(), Rejoin::Always);
        host2.set_rejoin(Rejoin::Off);
        assert!(host2.reserved().is_empty());
        host2.release(PeerId(1));
    }

    #[test]
    fn a_udp_slot_takes_back_only_the_same_address_and_nonce() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Within(ms(20_000)));
        host.set_rejoin_pins_nonce(true);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        host.set_refusing(Some(RefuseReason::RunInProgress));
        drop(c1);
        for t in (30..10_200).step_by(50) {
            host.poll(ms(t)).unwrap();
        }
        assert_eq!(host.reserved(), vec![(PeerId(1), addr(2))]);
        events_of(&mut host);
        // Same address, another game (another nonce): a stranger, refused.
        let mut other = client_at(&net, 2, 77, ms(10_200));
        run_eps(&mut [&mut host, &mut other], 10_200, 10_230);
        assert!(!other.is_connected(PeerId::HOST));
        assert_eq!(host.reserved().len(), 1, "the slot is still held");
        drop(other);
        events_of(&mut host);
        // The same game comes back (its nonce): its own slot, mid-run.
        let mut back = client_at(&net, 2, 2, ms(15_000));
        run_eps(&mut [&mut host, &mut back], 15_000, 15_030);
        assert_eq!(
            events_of(&mut host),
            vec![Event::Connected {
                peer: PeerId(1),
                addr: addr(2)
            }]
        );
        assert_eq!(back.local_peer(), Some(PeerId(1)));
    }

    #[test]
    fn the_allowlist_drops_uninvited_newcomers_silently() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Off);
        host.set_allowlist(Some([addr(2)].into_iter().collect()));
        let mut stranger = client_at(&net, 3, 3, ms(0));
        let mut invited = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut stranger, &mut invited], 0, 500);
        assert_eq!(host.peers(), vec![PeerId(1)]);
        assert!(invited.is_connected(PeerId::HOST));
        assert!(!stranger.is_connected(PeerId::HOST));
        assert!(
            events_of(&mut stranger).is_empty(),
            "no answer, not even a refusal"
        );
        assert!(
            !events_of(&mut host)
                .iter()
                .any(|e| matches!(e, Event::Refused { .. }))
        );
        host.set_allowlist(None);
    }

    #[test]
    fn a_peer_restarting_its_link_gets_a_fresh_connection_in_its_slot() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Always);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        events_of(&mut host);
        // Later, the client starts over with the same nonce.
        for _ in 0..50 {
            c1.send(PeerId::HOST, &Message::Go { start_ms: 1 }).unwrap();
        }
        run_eps(&mut [&mut host, &mut c1], 30, 6_000);
        events_of(&mut host);
        drop(c1);
        let mut again = client_at(&net, 2, 2, ms(6_000));
        run_eps(&mut [&mut host, &mut again], 6_000, 6_050);
        let ev = events_of(&mut host);
        assert_eq!(
            ev,
            vec![
                Event::Disconnected {
                    peer: PeerId(1),
                    reason: DisconnectReason::Timeout
                },
                Event::Connected {
                    peer: PeerId(1),
                    addr: addr(2)
                }
            ]
        );
        again
            .send(PeerId::HOST, &Message::Go { start_ms: 9 })
            .unwrap();
        run_eps(&mut [&mut host, &mut again], 6_050, 6_060);
        assert!(events_of(&mut host).contains(&Event::Message {
            from: PeerId(1),
            msg: Message::Go { start_ms: 9 }
        }));
    }

    /// A restart is known by its new epoch at once.
    #[test]
    fn a_restart_is_known_by_its_new_epoch_at_once() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Always);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        events_of(&mut host);
        let old = (
            host.conns[&PeerId(1)].epoch,
            host.conns[&PeerId(1)].remote_epoch,
        );
        assert_eq!(old.1, Some(c1.conns[&PeerId::HOST].epoch));
        drop(c1);
        // Well inside REJOIN_PUNCH_AGE.
        let mut again = client_at(&net, 2, 2, ms(1_000));
        run_eps(&mut [&mut host, &mut again], 1_000, 1_050);
        assert_eq!(
            events_of(&mut host),
            vec![
                Event::Disconnected {
                    peer: PeerId(1),
                    reason: DisconnectReason::Timeout
                },
                Event::Connected {
                    peer: PeerId(1),
                    addr: addr(2)
                }
            ]
        );
        let c = &host.conns[&PeerId(1)];
        assert_ne!(c.epoch, old.0, "the host's side is new too");
        assert_eq!(c.remote_epoch, Some(again.conns[&PeerId::HOST].epoch));
        assert_ne!(c.remote_epoch, old.1);
    }

    /// A late punch of an earlier epoch changes nothing.
    #[test]
    fn a_late_punch_of_an_earlier_epoch_changes_nothing() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Always);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        events_of(&mut host);
        let theirs = host.conns[&PeerId(1)].remote_epoch.unwrap();
        let mut dg = Vec::new();
        Header {
            kind: PacketKind::Punch,
            epoch: theirs.wrapping_sub(1) % packet::EPOCHS,
            nonce: 2,
            seq: 0,
            ack: 0,
            ack_bits: 0,
            send_time_ms: 30,
        }
        .write(&mut dg);
        PunchBody {
            assigned: PunchBody::UNASSIGNED,
            expected_nonce: 1,
        }
        .write(&mut dg);
        c1.transport_mut().send_to(ms(30), addr(1), &dg).unwrap();
        run_eps(&mut [&mut host, &mut c1], 30, 200);
        assert!(
            !events_of(&mut host)
                .iter()
                .any(|e| matches!(e, Event::Disconnected { .. })),
            "the connection stays"
        );
        assert_eq!(host.conns[&PeerId(1)].remote_epoch, Some(theirs));
        assert!(host.is_connected(PeerId(1)) && c1.is_connected(PeerId::HOST));
    }

    /// A restarted process with the same first epoch but a new nonce
    /// replaces the old connection.
    #[test]
    fn a_restarted_process_with_the_same_first_epoch_gets_a_fresh_connection() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Always);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        events_of(&mut host);
        let first = c1.conns[&PeerId::HOST].epoch;
        drop(c1);
        let mut again = client_at(&net, 2, 3, ms(100)); // a new process: a new nonce
        again.conns.get_mut(&PeerId::HOST).unwrap().epoch = first;
        run_eps(&mut [&mut host, &mut again], 100, 200);
        let ev = events_of(&mut host);
        assert!(
            ev.contains(&Event::Disconnected {
                peer: PeerId(1),
                reason: DisconnectReason::Replaced
            }),
            "{ev:?}"
        );
        again
            .send(PeerId::HOST, &Message::Go { start_ms: 5 })
            .unwrap();
        run_eps(&mut [&mut host, &mut again], 200, 260);
        assert!(events_of(&mut host).contains(&Event::Message {
            from: PeerId(1),
            msg: Message::Go { start_ms: 5 }
        }));
    }

    /// A late Data packet of an earlier connection is dropped by its epoch.
    #[test]
    fn a_late_packet_of_an_earlier_connection_is_dropped() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Always);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        events_of(&mut host);
        events_of(&mut c1);
        let hc = &host.conns[&PeerId(1)];
        let header = Header {
            kind: PacketKind::Data,
            epoch: (hc.epoch + 1) % packet::EPOCHS,
            nonce: 1,
            seq: hc.send_window.peek_seq().wrapping_add(30_000),
            ack: 0,
            ack_bits: 0,
            send_time_ms: 30,
        };
        let dg = PacketWriter::new(&header).finish();
        let stray = c1.stray_datagrams();
        host.transport_mut().send_to(ms(30), addr(2), &dg).unwrap();
        run_eps(&mut [&mut host, &mut c1], 30, 31);
        assert_eq!(c1.stray_datagrams(), stray + 1, "dropped as stray");
        host.send(PeerId(1), &Message::Go { start_ms: 4 }).unwrap();
        run_eps(&mut [&mut host, &mut c1], 31, 2_000);
        assert!(
            events_of(&mut c1).contains(&Event::Message {
                from: PeerId::HOST,
                msg: Message::Go { start_ms: 4 }
            }),
            "the fresh connection's packets still count"
        );
        assert!(c1.is_connected(PeerId::HOST));
    }

    /// A packet with a damaged chunk is dropped whole and not acked.
    #[test]
    fn a_packet_with_a_damaged_chunk_is_not_acked() {
        let net = LoopbackNet::new();
        let mut host = rejoin_host(&net, Rejoin::Always);
        let mut c1 = client_at(&net, 2, 2, ms(0));
        run_eps(&mut [&mut host, &mut c1], 0, 30);
        events_of(&mut host);
        events_of(&mut c1);
        let hc = &host.conns[&PeerId(1)];
        let header = Header {
            kind: PacketKind::Data,
            epoch: hc.epoch,
            nonce: 1,
            seq: hc.send_window.peek_seq().wrapping_add(5),
            ack: 0,
            ack_bits: 0,
            send_time_ms: 30,
        };
        let mut w = PacketWriter::new(&header);
        let body = Message::Go { start_ms: 4 }.encode().unwrap();
        assert!(w.push_reliable(0, &body));
        let mut dg = w.finish();
        dg.push(0x01); // a reliable chunk (tag 1) cut short
        let seq = header.seq;
        let stray = c1.stray_datagrams();
        host.transport_mut().send_to(ms(30), addr(2), &dg).unwrap();
        run_eps(&mut [&mut host, &mut c1], 30, 31);
        assert_eq!(c1.stray_datagrams(), stray + 1, "dropped");
        assert!(events_of(&mut c1).is_empty(), "nothing delivered");
        let (ack, _) = c1.conns[&PeerId::HOST].recv_window.ack();
        assert_ne!(ack, seq, "not recorded, so never acked");
    }
}
