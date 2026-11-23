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

//! Steam peers on top of the netcode. A Steam id travels as a synthetic IPv6
//! address in the unique-local block `fd4e:444d:4c00::/64`, port 1
//! ([`steam_addr`], [`addr_steam`]), since the rest only keys on [`SocketAddr`].
//!
//! Steam calls must run on the game thread, so [`channel`] makes two halves
//! joined by bounded channels: [`SteamTransport`] (network thread; a full
//! channel drops like UDP loss) and [`SteamBridge`] (game thread, pumped each
//! frame). [`SteamMux`] puts Steam next to the UDP socket. Logs and
//! `status.toml` show [`id_tag`], never the id.

#![forbid(unsafe_code)]

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::Duration;

use crate::packet::MAX_DATAGRAM;
use crate::transport::Transport;

/// The first three groups of every synthetic Steam address (`fd4e:444d:4c00`).
pub const STEAM_PREFIX: [u16; 3] = [0xfd4e, 0x444d, 0x4c00];
/// The port of every synthetic Steam address.
pub const STEAM_PORT: u16 = 1;
/// Default outbound channel size (datagrams waiting for the game thread).
pub const OUT_CAPACITY: usize = 512;
/// Default inbound channel size (datagrams waiting for the network thread).
pub const IN_CAPACITY: usize = 1024;

/// The synthetic address that stands for Steam id `id`.
#[must_use]
pub fn steam_addr(id: u64) -> SocketAddr {
    let g = |shift: u32| ((id >> shift) & 0xFFFF) as u16;
    let ip = Ipv6Addr::new(
        STEAM_PREFIX[0],
        STEAM_PREFIX[1],
        STEAM_PREFIX[2],
        0,
        g(48),
        g(32),
        g(16),
        g(0),
    );
    SocketAddr::V6(SocketAddrV6::new(ip, STEAM_PORT, 0, 0))
}

/// The Steam id a synthetic address stands for, or `None` for a real address.
#[must_use]
pub fn addr_steam(addr: &SocketAddr) -> Option<u64> {
    let SocketAddr::V6(v6) = addr else {
        return None;
    };
    let s = v6.ip().segments();
    if s[..3] != STEAM_PREFIX || s[3] != 0 || v6.port() != STEAM_PORT {
        return None;
    }
    Some(
        s[4..]
            .iter()
            .fold(0u64, |acc, &g| (acc << 16) | u64::from(g)),
    )
}

/// Whether `addr` is a synthetic Steam address.
#[must_use]
pub fn is_steam(addr: &SocketAddr) -> bool {
    addr_steam(addr).is_some()
}

/// A short, stable tag for a Steam id (6 hex digits of an FNV-1a hash).
#[must_use]
pub fn id_tag(id: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:06x}", (h ^ (h >> 32)) & 0x00FF_FFFF)
}

/// An address for logs and `status.toml`: `steam:<tag>` for Steam peers.
#[must_use]
pub fn display_addr(addr: &SocketAddr) -> String {
    match addr_steam(addr) {
        Some(id) => format!("steam:{}", id_tag(id)),
        None => addr.to_string(),
    }
}

/// One datagram crossing the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamDatagram {
    /// The Steam id it goes to or came from.
    pub peer: u64,
    /// The bytes (at most [`MAX_DATAGRAM`]).
    pub data: Vec<u8>,
}

/// What the bridge has done so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SteamStats {
    /// Datagrams queued for the game thread.
    pub queued_out: u64,
    /// Outbound datagrams dropped because the channel was full.
    pub dropped_out: u64,
    /// Outbound datagrams refused for being over [`MAX_DATAGRAM`].
    pub oversize: u64,
    /// Inbound datagrams handed to the network thread.
    pub delivered_in: u64,
    /// Inbound datagrams dropped (channel full, oversize, network thread gone).
    pub dropped_in: u64,
}

#[derive(Debug, Default)]
struct Shared {
    local: AtomicU64,
    queued_out: AtomicU64,
    dropped_out: AtomicU64,
    oversize: AtomicU64,
    delivered_in: AtomicU64,
    dropped_in: AtomicU64,
}

impl Shared {
    fn stats(&self) -> SteamStats {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        SteamStats {
            queued_out: l(&self.queued_out),
            dropped_out: l(&self.dropped_out),
            oversize: l(&self.oversize),
            delivered_in: l(&self.delivered_in),
            dropped_in: l(&self.dropped_in),
        }
    }
}

fn bump(a: &AtomicU64) {
    a.fetch_add(1, Ordering::Relaxed);
}

/// Make a connected [`SteamTransport`] / [`SteamBridge`] pair (capacities at
/// least 1).
#[must_use]
pub fn channel(out_capacity: usize, in_capacity: usize) -> (SteamTransport, SteamBridge) {
    let (out_tx, out_rx) = mpsc::sync_channel(out_capacity.max(1));
    let (in_tx, in_rx) = mpsc::sync_channel(in_capacity.max(1));
    let shared = Arc::new(Shared::default());
    (
        SteamTransport {
            out: out_tx,
            inbox: in_rx,
            shared: Arc::clone(&shared),
        },
        SteamBridge {
            outbox: out_rx,
            inbox: in_tx,
            shared,
        },
    )
}

/// The network-thread half: a [`Transport`] that only talks to channels.
#[derive(Debug)]
pub struct SteamTransport {
    out: SyncSender<SteamDatagram>,
    inbox: Receiver<SteamDatagram>,
    shared: Arc<Shared>,
}

impl SteamTransport {
    /// Counters shared with the bridge.
    #[must_use]
    pub fn stats(&self) -> SteamStats {
        self.shared.stats()
    }

    /// This player's Steam id, once the game thread has set it (0 before).
    #[must_use]
    pub fn local_id(&self) -> u64 {
        self.shared.local.load(Ordering::Relaxed)
    }
}

impl Transport for SteamTransport {
    fn send_to(&mut self, _now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        let Some(peer) = addr_steam(&to) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a Steam peer address",
            ));
        };
        if data.len() > MAX_DATAGRAM {
            bump(&self.shared.oversize);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("datagram of {} bytes is over the Steam limit", data.len()),
            ));
        }
        let d = SteamDatagram {
            peer,
            data: data.to_vec(),
        };
        match self.out.try_send(d) {
            Ok(()) => bump(&self.shared.queued_out),
            // Full or disconnected: drop like UDP.
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                bump(&self.shared.dropped_out);
            }
        }
        Ok(())
    }

    fn recv_from(
        &mut self,
        _now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        match self.inbox.try_recv() {
            Ok(d) => {
                // Like UDP, a datagram larger than the buffer is truncated.
                let n = d.data.len().min(buf.len());
                buf[..n].copy_from_slice(&d.data[..n]);
                Ok(Some((n, steam_addr(d.peer))))
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(None),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(steam_addr(self.local_id()))
    }
}

/// The game-thread half.
#[derive(Debug)]
pub struct SteamBridge {
    outbox: Receiver<SteamDatagram>,
    inbox: SyncSender<SteamDatagram>,
    shared: Arc<Shared>,
}

impl SteamBridge {
    /// Record this player's Steam id.
    pub fn set_local_id(&self, id: u64) {
        self.shared.local.store(id, Ordering::Relaxed);
    }

    /// This player's Steam id (0 until set).
    #[must_use]
    pub fn local_id(&self) -> u64 {
        self.shared.local.load(Ordering::Relaxed)
    }

    /// The next datagram to send through Steam, if any.
    #[must_use]
    pub fn pop_outbound(&self) -> Option<SteamDatagram> {
        self.outbox.try_recv().ok()
    }

    /// Hand a datagram from Steam id `from` to the network thread; `false` if
    /// dropped (treat as loss).
    #[must_use = "false means the datagram was dropped"]
    pub fn deliver(&self, from: u64, data: Vec<u8>) -> bool {
        if data.len() > MAX_DATAGRAM {
            bump(&self.shared.dropped_in);
            return false;
        }
        match self.inbox.try_send(SteamDatagram { peer: from, data }) {
            Ok(()) => {
                bump(&self.shared.delivered_in);
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                bump(&self.shared.dropped_in);
                false
            }
        }
    }

    /// Counters shared with the transport.
    #[must_use]
    pub fn stats(&self) -> SteamStats {
        self.shared.stats()
    }
}

/// UDP and Steam behind one [`Transport`], routed by address; receives
/// alternate so neither starves. The local address is the UDP one.
#[derive(Debug)]
pub struct SteamMux<U, S> {
    udp: U,
    steam: S,
    steam_first: bool,
}

impl<U: Transport, S: Transport> SteamMux<U, S> {
    pub fn new(udp: U, steam: S) -> Self {
        Self {
            udp,
            steam,
            steam_first: true,
        }
    }

    /// The UDP side.
    pub fn udp_mut(&mut self) -> &mut U {
        &mut self.udp
    }

    /// The Steam side.
    pub fn steam_mut(&mut self) -> &mut S {
        &mut self.steam
    }
}

impl<U: Transport, S: Transport> Transport for SteamMux<U, S> {
    fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        if is_steam(&to) {
            self.steam.send_to(now, to, data)
        } else {
            self.udp.send_to(now, to, data)
        }
    }

    fn recv_from(
        &mut self,
        now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        self.steam_first = !self.steam_first;
        // One side's error is returned only when the other has nothing waiting.
        let first = if self.steam_first {
            self.steam.recv_from(now, buf)
        } else {
            self.udp.recv_from(now, buf)
        };
        if let Ok(Some(r)) = first {
            return Ok(Some(r));
        }
        let other = if self.steam_first {
            self.udp.recv_from(now, buf)
        } else {
            self.steam.recv_from(now, buf)
        };
        match (first, other) {
            (_, Ok(Some(r))) => Ok(Some(r)),
            (Err(e), _) | (Ok(_), Err(e)) => Err(e),
            (Ok(_), Ok(None)) => Ok(None),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    fn release_held(&mut self, now: Duration) -> io::Result<()> {
        let udp = self.udp.release_held(now);
        self.steam.release_held(now)?;
        udp
    }
}

/// A stand-in for an unusable UDP socket: sends vanish, nothing arrives.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoUdp;

impl Transport for NoUdp {
    fn send_to(&mut self, _now: Duration, _to: SocketAddr, _data: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn recv_from(
        &mut self,
        _now: Duration,
        _buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        Ok(None)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::conditioner::{Conditioner, Conditions};
    use crate::endpoint::{Endpoint, EndpointConfig, Event, Role};
    use crate::msg::{Message, PeerId};
    use crate::transport::LoopbackNet;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    const ALICE: u64 = 0x0110_0001_0000_0042;
    const BOB: u64 = 0x0110_0001_0BAD_F00D;

    #[test]
    fn addresses_round_trip_and_never_collide() {
        for id in [0, 1, ALICE, BOB, u64::MAX, 0x8000_0000_0000_0000] {
            let a = steam_addr(id);
            assert_eq!(addr_steam(&a), Some(id), "{a}");
            assert!(is_steam(&a));
            assert!(matches!(a.ip(), std::net::IpAddr::V6(ip) if ip.is_unique_local()));
        }
        assert_eq!(
            steam_addr(0x0011_2233_4455_6677).to_string(),
            "[fd4e:444d:4c00:0:11:2233:4455:6677]:1"
        );
        for real in [
            "10.0.0.1:1",
            "[fd4e:444d:4c00:0:1:2:3:4]:2",
            "[fd4e:444d:4c01:0:1:2:3:4]:1",
            "[fd4e:444d:4c00:1:1:2:3:4]:1",
            "[::1]:1",
        ] {
            let a: SocketAddr = real.parse().unwrap();
            assert_eq!(addr_steam(&a), None, "{real}");
        }
    }

    #[test]
    fn logs_get_a_tag_not_the_id() {
        let shown = display_addr(&steam_addr(ALICE));
        assert!(shown.starts_with("steam:"));
        assert!(!shown.contains(&ALICE.to_string()));
        assert!(!shown.contains(&format!("{ALICE:x}")));
        assert_eq!(id_tag(ALICE), id_tag(ALICE));
        assert_ne!(id_tag(ALICE), id_tag(BOB));
        assert_eq!(id_tag(ALICE).len(), 6);
        assert_eq!(display_addr(&"10.0.0.1:7".parse().unwrap()), "10.0.0.1:7");
    }

    #[test]
    fn bridge_moves_datagrams_both_ways() {
        let (mut t, b) = channel(4, 4);
        assert_eq!(t.local_addr().unwrap(), steam_addr(0));
        b.set_local_id(ALICE);
        assert_eq!(t.local_addr().unwrap(), steam_addr(ALICE));
        t.send_to(ms(0), steam_addr(BOB), b"hi").unwrap();
        assert_eq!(
            b.pop_outbound(),
            Some(SteamDatagram {
                peer: BOB,
                data: b"hi".to_vec()
            })
        );
        assert_eq!(b.pop_outbound(), None);
        assert!(b.deliver(BOB, b"back".to_vec()));
        let mut buf = [0u8; 2];
        // Truncated like UDP.
        assert_eq!(
            t.recv_from(ms(0), &mut buf).unwrap(),
            Some((2, steam_addr(BOB)))
        );
        assert_eq!(&buf, b"ba");
        assert_eq!(t.recv_from(ms(0), &mut buf).unwrap(), None);
        // Real addresses are not for this transport.
        let e = t
            .send_to(ms(0), "10.0.0.1:7".parse().unwrap(), b"x")
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn full_channels_drop_like_udp_and_count_it() {
        let (mut t, b) = channel(2, 2);
        for i in 0..5u8 {
            t.send_to(ms(0), steam_addr(BOB), &[i]).unwrap();
        }
        let got: Vec<_> = std::iter::from_fn(|| b.pop_outbound()).collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, vec![0]);
        let s = b.stats();
        assert_eq!((s.queued_out, s.dropped_out), (2, 3));
        assert!(b.deliver(BOB, vec![1]));
        assert!(b.deliver(BOB, vec![2]));
        assert!(!b.deliver(BOB, vec![3]), "inbound full");
        assert_eq!(t.stats().dropped_in, 1);
        assert_eq!(t.stats().delivered_in, 2);
        // A dropped network thread doesn't block or panic the game side.
        drop(t);
        assert!(!b.deliver(BOB, vec![4]));
    }

    #[test]
    fn the_1200_byte_limit() {
        let (mut t, b) = channel(4, 4);
        t.send_to(ms(0), steam_addr(BOB), &[7u8; MAX_DATAGRAM])
            .unwrap();
        assert!(
            t.send_to(ms(0), steam_addr(BOB), &[7u8; MAX_DATAGRAM + 1])
                .is_err()
        );
        assert_eq!(b.pop_outbound().unwrap().data.len(), MAX_DATAGRAM);
        assert!(b.pop_outbound().is_none());
        assert_eq!(b.stats().oversize, 1);
        assert!(b.deliver(BOB, vec![0u8; MAX_DATAGRAM]));
        assert!(!b.deliver(BOB, vec![0u8; MAX_DATAGRAM + 1]));
        assert_eq!(MAX_DATAGRAM, 1200, "Steam's unreliable limit");
    }

    /// A fake Steam: delivers each outbound datagram to the bridge it is
    /// addressed to. Optionally cut.
    #[derive(Default)]
    struct FakeSteam {
        bridges: HashMap<u64, SteamBridge>,
        cut: bool,
        moved: u64,
    }

    impl FakeSteam {
        fn add(&mut self, id: u64, b: SteamBridge) {
            b.set_local_id(id);
            self.bridges.insert(id, b);
        }

        fn pump(&mut self) {
            let mut moving = Vec::new();
            for (&from, b) in &self.bridges {
                while let Some(d) = b.pop_outbound() {
                    moving.push((from, d));
                }
            }
            if self.cut {
                return;
            }
            for (from, d) in moving {
                if let Some(to) = self.bridges.get(&d.peer) {
                    let _ = to.deliver(from, d.data);
                    self.moved += 1;
                }
            }
        }
    }

    type SteamEp = Endpoint<Conditioner<SteamTransport>>;

    fn steam_pair(conditions: Conditions) -> (SteamEp, SteamEp, FakeSteam) {
        let mut steam = FakeSteam::default();
        let (th, bh) = channel(OUT_CAPACITY, IN_CAPACITY);
        let (tc, bc) = channel(OUT_CAPACITY, IN_CAPACITY);
        steam.add(ALICE, bh);
        steam.add(BOB, bc);
        let host = Endpoint::new(
            Conditioner::new(th, conditions, 1),
            EndpointConfig::new(Role::Host, 0xA11CE),
        );
        let mut client = Endpoint::new(
            Conditioner::new(tc, conditions, 2),
            EndpointConfig::new(Role::Client, 0xB0B),
        );
        let _ = client.connect(ms(0), steam_addr(ALICE), None);
        (host, client, steam)
    }

    fn drain(ep: &mut SteamEp) -> Vec<Event> {
        std::iter::from_fn(|| ep.next_event()).collect()
    }

    #[test]
    fn full_handshake_and_data_over_a_fake_steam() {
        let conditions = Conditions {
            latency: ms(30),
            jitter: ms(5),
            loss: 0.05,
            duplicate: 0.01,
            reorder: 0.01,
            ..Conditions::default()
        };
        let (mut host, mut client, mut steam) = steam_pair(conditions);
        let mut host_ev = Vec::new();
        let mut client_ev = Vec::new();
        let mut got = Vec::new();
        let mut sent = 0u16;
        for t in 0..20_000u64 {
            let now = ms(t);
            client.poll(now).unwrap();
            host.poll(now).unwrap();
            steam.pump();
            host_ev.extend(drain(&mut host));
            for e in drain(&mut client) {
                if let Event::Message {
                    msg: Message::UpgradeOpen { round },
                    ..
                } = e
                {
                    got.push(round);
                } else {
                    client_ev.push(e);
                }
            }
            if host.is_connected(PeerId(1)) && t % 5 == 0 && sent < 300 {
                host.send(PeerId(1), &Message::UpgradeOpen { round: sent })
                    .unwrap();
                sent += 1;
            }
            if got.len() == 300 {
                break;
            }
        }
        assert!(host_ev.contains(&Event::Connected {
            peer: PeerId(1),
            addr: steam_addr(BOB)
        }));
        assert!(client_ev.contains(&Event::Connected {
            peer: PeerId::HOST,
            addr: steam_addr(ALICE)
        }));
        assert_eq!(client.local_peer(), Some(PeerId(1)));
        assert_eq!(
            got,
            (0..300).collect::<Vec<_>>(),
            "reliable, in order, once"
        );
        let rtt = client.stats(PeerId::HOST).unwrap().rtt;
        assert!(rtt > ms(40) && rtt < ms(120), "rtt {rtt:?}");
        assert!(steam.moved > 300);
    }

    #[test]
    fn mux_routes_by_address_and_alternates() {
        let net = LoopbackNet::new();
        let udp_addr: SocketAddr = "10.0.0.1:7777".parse().unwrap();
        let mut other = net.bind("10.0.0.2:7777".parse().unwrap()).unwrap();
        let (st, b) = channel(8, 8);
        let mut mux = SteamMux::new(net.bind(udp_addr).unwrap(), st);
        assert_eq!(mux.local_addr().unwrap(), udp_addr);
        mux.send_to(ms(0), steam_addr(BOB), b"steam").unwrap();
        mux.send_to(ms(0), "10.0.0.2:7777".parse().unwrap(), b"udp")
            .unwrap();
        assert_eq!(b.pop_outbound().unwrap().data, b"steam");
        let mut buf = [0u8; 16];
        assert_eq!(other.recv_from(ms(0), &mut buf).unwrap().unwrap().0, 3);
        // Both sides have traffic waiting: neither starves.
        for _ in 0..3 {
            let _ = b.deliver(BOB, b"s".to_vec());
            other.send_to(ms(0), udp_addr, b"u").unwrap();
        }
        let mut from = Vec::new();
        while let Some((_, a)) = mux.recv_from(ms(0), &mut buf).unwrap() {
            from.push(is_steam(&a));
        }
        assert_eq!(from.len(), 6);
        assert!(from[..2].contains(&true) && from[..2].contains(&false));
        let mut none = NoUdp;
        none.send_to(ms(0), udp_addr, b"x").unwrap();
        assert!(none.recv_from(ms(0), &mut buf).unwrap().is_none());
    }

    /// A UDP socket that fails every receive does not stop the Steam side's
    /// datagrams; its error comes back once Steam has nothing waiting.
    #[test]
    fn a_udp_error_does_not_stop_steam_receives() {
        struct Failing;
        impl Transport for Failing {
            fn send_to(&mut self, _: Duration, _: SocketAddr, _: &[u8]) -> io::Result<()> {
                Ok(())
            }
            fn recv_from(
                &mut self,
                _: Duration,
                _: &mut [u8],
            ) -> io::Result<Option<(usize, SocketAddr)>> {
                Err(io::Error::other("message too long"))
            }
            fn local_addr(&self) -> io::Result<SocketAddr> {
                Ok("10.0.0.1:7777".parse().unwrap())
            }
        }
        let (st, b) = channel(8, 8);
        let mut mux = SteamMux::new(Failing, st);
        for _ in 0..4 {
            let _ = b.deliver(BOB, b"s".to_vec());
        }
        let mut buf = [0u8; 16];
        for i in 0..4 {
            let got = mux.recv_from(ms(0), &mut buf).unwrap();
            assert!(got.is_some_and(|(_, a)| is_steam(&a)), "datagram {i}");
        }
        assert!(mux.recv_from(ms(0), &mut buf).is_err());
    }
}
