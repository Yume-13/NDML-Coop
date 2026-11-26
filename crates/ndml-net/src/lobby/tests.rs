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

//! Lobby agent tests: simulated time, the in-memory loopback network, a fake
//! STUN server on it, and real files under `target/test-tmp/`.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::files::{self, PeerRequest, read_status, write_peer};
use super::*;
use crate::code::ConnectionCode;
use crate::endpoint::{DisconnectReason, Event, RefuseReason, Rejoin, Role};
use crate::msg::{Hello, Message, PeerId};
use crate::stun;
use crate::transport::{LoopbackNet, LoopbackSocket, Transport};

const STUN_ADDR: &str = "10.0.0.250:3478";

fn addr(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, last], port))
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

fn scratch(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-tmp/ndml-net-lobby")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn epoch() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

type Agent = LobbyAgent<LoopbackSocket>;

fn agent(net: &LoopbackNet, at: SocketAddr, dir: &Path, nonce: u32, stun: bool) -> Agent {
    let mut cfg = LobbyConfig::new(dir.to_path_buf(), nonce);
    // The files at the call: these tests run on a synthetic clock.
    cfg.file_thread = false;
    cfg.wall_clock_at_epoch = epoch();
    cfg.stun_servers = if stun {
        vec![STUN_ADDR.into()]
    } else {
        Vec::new()
    };
    LobbyAgent::new(net.bind(at).unwrap(), cfg)
}

/// A fake STUN server: answers each request with the sender's address, its
/// port shifted by `shift_port`.
struct FakeStun {
    sock: LoopbackSocket,
    requests: u32,
    shift_port: u16,
}

impl FakeStun {
    fn bind(net: &LoopbackNet) -> Self {
        Self {
            sock: net.bind(STUN_ADDR.parse().unwrap()).unwrap(),
            requests: 0,
            shift_port: 0,
        }
    }

    fn serve(&mut self, now: Duration) {
        let mut buf = [0u8; 576];
        while let Some((n, from)) = self.sock.recv_from(now, &mut buf).unwrap() {
            self.requests += 1;
            let txid = stun::transaction_id(&buf[..n]).unwrap();
            let mut mapped = from;
            mapped.set_port(from.port() + self.shift_port);
            let answer = stun::binding_success(&txid, mapped).unwrap();
            self.sock.send_to(now, from, &answer).unwrap();
        }
    }
}

fn run(agents: &mut [&mut Agent], stun: Option<&mut FakeStun>, from: u64, to: u64, step: u64) {
    let mut stun = stun;
    let mut t = from;
    while t < to {
        for a in agents.iter_mut() {
            a.poll(ms(t));
        }
        if let Some(s) = stun.as_deref_mut() {
            s.serve(ms(t));
        }
        t += step;
    }
}

fn status_of(dir: &Path) -> Status {
    read_status(&dir.join(STATUS_FILE)).unwrap().unwrap()
}

fn events(a: &mut Agent) -> Vec<Event> {
    std::iter::from_fn(|| a.next_event()).collect()
}

/// Write `peer.toml` as another process on the same machine would.
fn request(dir: &Path, n: u64, f: impl FnOnce(&mut PeerRequest)) {
    let mut r = PeerRequest::new(n, &status_of(dir).session);
    r.name = "Tester".into();
    f(&mut r);
    write_peer(&dir.join(PEER_FILE), &r).unwrap();
}

#[test]
fn discovers_the_public_endpoint_and_publishes_codes() {
    let net = LoopbackNet::new();
    let dir = scratch("discover");
    let mut stun = FakeStun::bind(&net);
    stun.shift_port = 1000; // the "NAT" maps 4000 -> 5000
    let mut a = agent(&net, addr(1, 4000), &dir, 0xA1, true);
    a.poll(ms(0));
    let s = status_of(&dir);
    assert_eq!(s.state, LobbyState::Discovering);
    assert_eq!(s.stun.state, StunState::Pending);
    assert_eq!(s.heartbeat_ms, files::unix_ms(epoch()));
    assert_eq!(s.session, "000000a1");
    let lan = ConnectionCode {
        addr: addr(1, 4000),
        nonce: 0xA1,
    };
    assert_eq!(s.lan_code, Some(lan.encode()));
    stun.serve(ms(1));
    a.poll(ms(10));
    let s = status_of(&dir);
    let public = addr(1, 5000);
    assert_eq!(s.state, LobbyState::Ready);
    assert_eq!(s.stun.state, StunState::Ok);
    assert_eq!(s.public_addr, Some(public.to_string()));
    let code = ConnectionCode {
        addr: public,
        nonce: 0xA1,
    };
    assert_eq!(s.code, Some(code.encode()));
    assert_eq!(a.code(), Some(code));
    assert_eq!(s.heartbeat_ms, files::unix_ms(epoch()) + 10);
    assert!(a.last_file_error().is_none());
}

/// The co-op mod's setting: no STUN server is asked until the code page opens.
#[test]
fn no_stun_server_is_asked_until_the_code_route_is_used() {
    let net = LoopbackNet::new();
    let dir = scratch("stun-on-demand");
    let mut stun = FakeStun::bind(&net);
    let mut a = {
        let mut cfg = LobbyConfig::new(dir.clone(), 0xA2);
        cfg.file_thread = false;
        cfg.wall_clock_at_epoch = epoch();
        cfg.stun_servers = vec![STUN_ADDR.into()];
        cfg.discover_at_start = false;
        LobbyAgent::new(net.bind(addr(1, 4000)).unwrap(), cfg)
    };
    run(&mut [&mut a], Some(&mut stun), 0, 60_000, 100);
    assert_eq!(stun.requests, 0, "nothing sent to the STUN server");
    assert!(!a.discovery_wanted());
    let s = status_of(&dir);
    assert_eq!(s.state, LobbyState::Ready, "not discovering");
    assert_eq!(s.stun.state, StunState::Pending);
    assert!(s.code.is_none() && s.public_addr.is_none());
    assert!(s.lan_code.is_some(), "LAN codes need no lookup");
    // The connection-code page opens: the lookup runs and the code appears.
    a.want_discovery();
    run(&mut [&mut a], Some(&mut stun), 60_000, 60_500, 10);
    assert!(stun.requests > 0);
    let s = status_of(&dir);
    assert_eq!(s.stun.state, StunState::Ok);
    assert!(s.code.is_some());
}

/// Without the file interface nothing is written and `peer.toml` is ignored.
#[test]
fn without_files_no_status_file_is_written_and_peer_toml_is_ignored() {
    let net = LoopbackNet::new();
    let dir = scratch("no-files");
    let mut a = {
        let mut cfg = LobbyConfig::new(dir.clone(), 0xA3);
        cfg.file_thread = false;
        cfg.wall_clock_at_epoch = epoch();
        cfg.stun_servers = Vec::new();
        cfg.files = false;
        LobbyAgent::new(net.bind(addr(1, 4000)).unwrap(), cfg)
    };
    run(&mut [&mut a], None, 0, 2_000, 100);
    assert!(!dir.exists(), "no lobby folder");
    assert_eq!(a.status().state, LobbyState::Ready);
    assert_eq!(a.status().session, "000000a3");
    // A peer.toml put there by hand changes nothing.
    std::fs::create_dir_all(&dir).unwrap();
    let mut r = PeerRequest::new(1, "000000a3");
    r.role = RoleChoice::Host;
    write_peer(&dir.join(PEER_FILE), &r).unwrap();
    run(&mut [&mut a], None, 2_000, 4_000, 100);
    assert_eq!(a.role(), None);
    assert!(!dir.join(STATUS_FILE).exists());
    // The in-game request still works.
    a.request(
        ms(4_000),
        &LobbyRequest {
            role: RoleChoice::Host,
            ..LobbyRequest::default()
        },
    );
    a.poll(ms(4_100));
    assert_eq!(a.role(), Some(Role::Host));
    assert!(!dir.join(STATUS_FILE).exists());
}

/// Hosting or joining by code starts the lookup; a join by address does not.
#[test]
fn hosting_or_a_code_join_starts_the_lookup_and_a_direct_join_does_not() {
    let net = LoopbackNet::new();
    let quiet = |dir: &Path, at: SocketAddr, nonce: u32| {
        let mut cfg = LobbyConfig::new(dir.to_path_buf(), nonce);
        cfg.file_thread = false;
        cfg.wall_clock_at_epoch = epoch();
        cfg.stun_servers = vec![STUN_ADDR.into()];
        cfg.discover_at_start = false;
        LobbyAgent::new(net.bind(at).unwrap(), cfg)
    };
    let mut stun = FakeStun::bind(&net);
    let dir = scratch("stun-direct-join");
    let mut joiner = quiet(&dir, addr(1, 4000), 1);
    joiner.request(
        ms(0),
        &LobbyRequest {
            role: RoleChoice::Join,
            direct: Some("10.0.0.9:7777".into()),
            name: "Tester".into(),
            ..LobbyRequest::default()
        },
    );
    run(&mut [&mut joiner], Some(&mut stun), 0, 2_000, 100);
    assert_eq!(stun.requests, 0, "a direct join asks no STUN server");
    assert!(!joiner.discovery_wanted());
    let code = ConnectionCode {
        addr: addr(9, 7777),
        nonce: 9,
    };
    joiner.request(
        ms(2_000),
        &LobbyRequest {
            role: RoleChoice::Join,
            partner_code: Some(code.encode()),
            name: "Tester".into(),
            ..LobbyRequest::default()
        },
    );
    assert!(joiner.discovery_wanted(), "a join by code");
    let dir = scratch("stun-host");
    let mut host = quiet(&dir, addr(2, 4000), 2);
    host.request(
        ms(0),
        &LobbyRequest {
            role: RoleChoice::Host,
            name: "Tester".into(),
            ..LobbyRequest::default()
        },
    );
    assert!(host.discovery_wanted(), "hosting");
    run(&mut [&mut host], Some(&mut stun), 0, 500, 10);
    assert!(stun.requests > 0);
}

/// No servers: nothing is ever looked up or sent, the status says the lookup
/// is off, and the LAN code is there.
#[test]
fn without_servers_nothing_is_ever_looked_up_or_sent() {
    static RESOLVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    #[allow(clippy::unnecessary_wraps)] // the `StunResolve` shape
    fn resolve(_: &str) -> std::io::Result<Vec<SocketAddr>> {
        RESOLVED.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(vec![STUN_ADDR.parse().unwrap()])
    }
    let net = LoopbackNet::new();
    let mut stun = FakeStun::bind(&net);
    let dir = scratch("stun-off");
    let mut a = {
        let mut cfg = LobbyConfig::new(dir.clone(), 0xA4);
        cfg.file_thread = false;
        cfg.wall_clock_at_epoch = epoch();
        cfg.stun_servers = Vec::new();
        cfg.stun_resolve = resolve;
        cfg.discover_at_start = false;
        LobbyAgent::new(net.bind(addr(1, 4000)).unwrap(), cfg)
    };
    a.want_discovery();
    a.request(
        ms(0),
        &LobbyRequest {
            role: RoleChoice::Host,
            name: "Tester".into(),
            ..LobbyRequest::default()
        },
    );
    run(&mut [&mut a], Some(&mut stun), 0, 30_000, 100);
    let code = ConnectionCode {
        addr: addr(9, 7777),
        nonce: 9,
    };
    a.request(
        ms(30_000),
        &LobbyRequest {
            role: RoleChoice::Join,
            partner_code: Some(code.encode()),
            name: "Tester".into(),
            ..LobbyRequest::default()
        },
    );
    run(&mut [&mut a], Some(&mut stun), 30_000, 60_000, 100);
    assert_eq!(stun.requests, 0, "nothing sent to a STUN server");
    assert!(
        !RESOLVED.load(std::sync::atomic::Ordering::Relaxed),
        "no name lookup"
    );
    let s = status_of(&dir);
    assert_eq!(s.stun.state, StunState::Disabled);
    assert_eq!(s.stun.retry_in_ms, None, "nothing retries");
    assert_ne!(s.state, LobbyState::Discovering);
    assert!(s.code.is_none() && s.public_addr.is_none());
    assert!(s.lan_code.is_some(), "LAN codes need no lookup");
}

#[test]
fn stun_failure_is_reported_and_retried() {
    let net = LoopbackNet::new();
    let dir = scratch("stun-fail");
    let mut a = agent(&net, addr(1, 4000), &dir, 1, true);
    run(&mut [&mut a], None, 0, 2_200, 100);
    let s = status_of(&dir);
    assert_eq!(s.state, LobbyState::Ready, "LAN play still works");
    assert_eq!(s.stun.state, StunState::Failed);
    assert!(
        s.stun.error.as_deref().unwrap().contains("no answer"),
        "{s:?}"
    );
    assert!(s.stun.retry_in_ms.unwrap() > 14_000);
    assert!(s.code.is_none() && s.lan_code.is_some());
    // The server comes back; the retry 15 s later succeeds.
    let mut stun = FakeStun::bind(&net);
    run(&mut [&mut a], Some(&mut stun), 2_200, 17_500, 100);
    let s = status_of(&dir);
    assert_eq!(s.stun.state, StunState::Ok);
    assert!(s.code.is_some());
    assert!(s.stun.error.is_none());
}

#[test]
fn two_way_code_exchange_links_host_and_joiner() {
    let net = LoopbackNet::new();
    let (dir_a, dir_b) = (scratch("two-way-a"), scratch("two-way-b"));
    let mut stun = FakeStun::bind(&net);
    let mut a = agent(&net, addr(1, 4000), &dir_a, 0xAAAA, true);
    let mut b = agent(&net, addr(2, 4000), &dir_b, 0xBBBB, true);
    run(&mut [&mut a, &mut b], Some(&mut stun), 0, 50, 5);
    let code_a = status_of(&dir_a).code.unwrap();
    let code_b = status_of(&dir_b).code.unwrap();
    request(&dir_a, 1, |r| {
        r.role = RoleChoice::Host;
        r.partner_code = Some(code_b.clone());
        r.name = "Ace".into();
    });
    request(&dir_b, 1, |r| {
        r.role = RoleChoice::Join;
        r.partner_code = Some(code_a.clone());
        r.name = "Wing".into();
    });
    run(&mut [&mut a, &mut b], Some(&mut stun), 50, 400, 5);
    assert!(matches!(
        events(&mut a).as_slice(),
        [Event::Connected {
            peer: PeerId(1),
            ..
        }]
    ));
    assert!(matches!(
        events(&mut b).as_slice(),
        [Event::Connected {
            peer: PeerId::HOST,
            ..
        }]
    ));
    assert_eq!(a.role(), Some(Role::Host));
    assert_eq!(b.player_name(), "Wing");
    let (sa, sb) = (status_of(&dir_a), status_of(&dir_b));
    assert_eq!(sa.state, LobbyState::Connected);
    assert_eq!(sa.role, RoleChoice::Host);
    assert_eq!(sa.request, 1);
    assert_eq!(sb.state, LobbyState::Connected);
    assert_eq!(sb.peers.len(), 1);
    assert!(sb.peers[0].connected);
    assert_eq!(sb.peers[0].id, 0);

    // Messages flow through the endpoint; names from Hello reach the status.
    let hello = |name: &str| {
        Message::Hello(Hello {
            mod_version: 1,
            build: "b".into(),
            name: name.into(),
        })
    };
    a.endpoint_mut()
        .unwrap()
        .send(PeerId(1), &hello("Ace"))
        .unwrap();
    b.endpoint_mut()
        .unwrap()
        .send(PeerId::HOST, &hello("Wing"))
        .unwrap();
    run(&mut [&mut a, &mut b], Some(&mut stun), 400, 1_000, 5);
    assert!(events(&mut b).contains(&Event::Message {
        from: PeerId::HOST,
        msg: hello("Ace")
    }));
    assert_eq!(status_of(&dir_a).peers[0].name, "Wing");
    assert_eq!(status_of(&dir_b).peers[0].name, "Ace");
    events(&mut a);

    // The same request again (UI re-sent it) does not disturb the link.
    request(&dir_b, 2, |r| {
        r.role = RoleChoice::Join;
        r.partner_code = Some(code_a.clone());
    });
    run(&mut [&mut a, &mut b], Some(&mut stun), 1_000, 1_300, 5);
    assert!(events(&mut b).is_empty());
    assert_eq!(status_of(&dir_b).request, 2);

    // The joiner disconnects: it goes idle, the host keeps waiting.
    request(&dir_b, 3, |r| r.disconnect = true);
    run(&mut [&mut a, &mut b], Some(&mut stun), 1_300, 1_600, 5);
    assert_eq!(
        events(&mut b),
        vec![Event::Disconnected {
            peer: PeerId::HOST,
            reason: DisconnectReason::Local
        }]
    );
    assert_eq!(
        events(&mut a),
        vec![Event::Disconnected {
            peer: PeerId(1),
            reason: DisconnectReason::Remote
        }]
    );
    let (sa, sb) = (status_of(&dir_a), status_of(&dir_b));
    assert_eq!(sb.state, LobbyState::Ready);
    assert_eq!(sb.role, RoleChoice::None);
    assert!(sb.error.is_none());
    assert_eq!(sa.state, LobbyState::Waiting);
    assert_eq!(sa.error_kind, Some(ErrorKind::LinkLost));
    assert_eq!(sa.error.as_deref(), Some("Your partner disconnected."));
}

#[test]
fn lan_join_by_address_and_a_refused_third_player() {
    let net = LoopbackNet::new();
    let dirs = [scratch("lan-a"), scratch("lan-b"), scratch("lan-c")];
    let mut a = agent(&net, addr(1, 7777), &dirs[0], 1, false);
    let mut b = agent(&net, addr(2, 7777), &dirs[1], 2, false);
    let mut c = agent(&net, addr(3, 7777), &dirs[2], 3, false);
    run(&mut [&mut a, &mut b, &mut c], None, 0, 10, 5);
    assert_eq!(status_of(&dirs[0]).stun.state, StunState::Disabled);
    request(&dirs[0], 1, |r| r.role = RoleChoice::Host);
    run(&mut [&mut a, &mut b, &mut c], None, 10, 250, 5);
    assert_eq!(status_of(&dirs[0]).state, LobbyState::Waiting);
    let lan = status_of(&dirs[0]).lan_addr.unwrap();
    request(&dirs[1], 1, |r| {
        r.role = RoleChoice::Join;
        r.direct = Some(lan.clone());
    });
    run(&mut [&mut a, &mut b, &mut c], None, 250, 500, 5);
    assert_eq!(status_of(&dirs[1]).state, LobbyState::Connected);
    assert_eq!(status_of(&dirs[0]).state, LobbyState::Connected);
    // A third player is turned away at once (2-player session), not after 30 s.
    request(&dirs[2], 1, |r| {
        r.role = RoleChoice::Join;
        r.direct = Some(lan.clone());
    });
    run(&mut [&mut a, &mut b, &mut c], None, 500, 800, 5);
    let sc = status_of(&dirs[2]);
    assert_eq!(sc.state, LobbyState::Failed);
    assert_eq!(sc.error_kind, Some(ErrorKind::Refused));
    assert_eq!(sc.refused.as_deref(), Some("full"));
    assert_eq!(sc.error.as_deref(), Some("The host's session is full."));
    assert!(events(&mut c).contains(&Event::Disconnected {
        peer: PeerId::HOST,
        reason: DisconnectReason::Refused(RefuseReason::Full)
    }));
    let sa = status_of(&dirs[0]);
    assert_eq!(sa.state, LobbyState::Connected, "the partner stays linked");
    let away = sa.turned_away.unwrap();
    assert_eq!(
        (away.addr.as_str(), away.reason.as_str()),
        ("10.0.0.3:7777", "full")
    );
    assert!(events(&mut a).contains(&Event::Refused {
        addr: addr(3, 7777),
        reason: RefuseReason::Full
    }));
    // The host's run starts: later joiners hear "run in progress".
    b.poll(ms(800));
    a.endpoint_mut()
        .unwrap()
        .set_refusing(Some(RefuseReason::RunInProgress));
    request(&dirs[1], 2, |r| r.disconnect = true);
    run(&mut [&mut a, &mut b, &mut c], None, 800, 1_000, 5);
    request(&dirs[2], 2, |r| {
        r.role = RoleChoice::Join;
        r.direct = Some(lan.clone());
    });
    run(&mut [&mut a, &mut b, &mut c], None, 1_000, 1_300, 5);
    assert_eq!(
        status_of(&dirs[2]).refused.as_deref(),
        Some("run_in_progress")
    );
}

#[test]
fn handshake_timeout_fails_clearly_and_recodes() {
    let net = LoopbackNet::new();
    let dir = scratch("timeout");
    let mut stun = FakeStun::bind(&net);
    let mut cfg = LobbyConfig::new(dir.clone(), 5);
    cfg.file_thread = false;
    cfg.wall_clock_at_epoch = epoch();
    cfg.stun_servers = vec![STUN_ADDR.into()];
    cfg.handshake_timeout = Duration::from_secs(2);
    let mut a = LobbyAgent::new(net.bind(addr(1, 4000)).unwrap(), cfg);
    run(&mut [&mut a], Some(&mut stun), 0, 50, 5);
    let before = stun.requests;
    let nobody = ConnectionCode {
        addr: addr(9, 4000),
        nonce: 99,
    };
    request(&dir, 1, |r| {
        r.role = RoleChoice::Join;
        r.partner_code = Some(nobody.encode());
    });
    run(&mut [&mut a], Some(&mut stun), 50, 1_000, 10);
    assert_eq!(status_of(&dir).state, LobbyState::Connecting);
    run(&mut [&mut a], Some(&mut stun), 1_000, 2_600, 10);
    let s = status_of(&dir);
    assert_eq!(s.state, LobbyState::Failed);
    assert_eq!(s.role, RoleChoice::Join);
    assert_eq!(s.error_kind, Some(ErrorKind::HandshakeTimeout));
    assert!(
        s.error
            .unwrap()
            .contains("No answer from your partner within 2 s")
    );
    assert!(stun.requests > before, "rediscovered after the failure");
    assert!(a.endpoint().is_none(), "socket handed back");
    // A new request (same content, next number) retries.
    request(&dir, 2, |r| {
        r.role = RoleChoice::Join;
        r.partner_code = Some(nobody.encode());
    });
    run(&mut [&mut a], Some(&mut stun), 2_600, 2_800, 10);
    let s = status_of(&dir);
    assert_eq!(s.state, LobbyState::Connecting);
    assert!(s.error.is_none());
}

#[test]
fn stale_malformed_and_bad_requests_are_handled() {
    let net = LoopbackNet::new();
    let dir = scratch("requests");
    let mut a = agent(&net, addr(1, 4000), &dir, 0x77, false);
    a.poll(ms(0));
    // A request from a previous game run (another session) is ignored.
    let mut old = PeerRequest::new(5, "deadbeef");
    old.role = RoleChoice::Host;
    write_peer(&dir.join(PEER_FILE), &old).unwrap();
    run(&mut [&mut a], None, 0, 500, 10);
    let s = status_of(&dir);
    assert_eq!((s.state, s.request), (LobbyState::Ready, 0));
    // A malformed file is reported and changes nothing.
    std::fs::write(dir.join(PEER_FILE), "format = 1\nrole = \"captain\"").unwrap();
    run(&mut [&mut a], None, 500, 800, 10);
    let s = status_of(&dir);
    assert!(s.peer_file_error.unwrap().contains("malformed"));
    assert_eq!(s.state, LobbyState::Ready);
    // A bad code fails with the parser's words.
    request(&dir, 1, |r| {
        r.role = RoleChoice::Join;
        r.partner_code = Some("2ZR00-00HWR-FBEDW-C85S9".into());
    });
    run(&mut [&mut a], None, 800, 1_000, 10);
    let s = status_of(&dir);
    assert!(s.peer_file_error.is_none());
    assert_eq!(s.state, LobbyState::Failed);
    assert_eq!(s.error_kind, Some(ErrorKind::BadRequest));
    assert!(s.error.unwrap().contains("mistyped"));
    assert_eq!(s.request, 1);
}

#[test]
fn version_mismatch_is_refused_with_a_reason() {
    let net = LoopbackNet::new();
    let (dir_a, dir_b) = (scratch("version-a"), scratch("version-b"));
    let make = |at, dir: &Path, nonce, mod_version| {
        let mut cfg = LobbyConfig::new(dir.to_path_buf(), nonce);
        cfg.file_thread = false;
        cfg.wall_clock_at_epoch = epoch();
        cfg.stun_servers = Vec::new();
        cfg.version = Some(VersionCheck {
            mod_version,
            build: "1.2.38.4".into(),
            send_hello: true,
        });
        LobbyAgent::new(net.bind(at).unwrap(), cfg)
    };
    let mut a = make(addr(1, 7777), &dir_a, 1, 2);
    let mut b = make(addr(2, 7777), &dir_b, 2, 3);
    run(&mut [&mut a, &mut b], None, 0, 10, 5);
    request(&dir_a, 1, |r| r.role = RoleChoice::Host);
    run(&mut [&mut a, &mut b], None, 10, 250, 5);
    request(&dir_b, 1, |r| {
        r.role = RoleChoice::Join;
        r.direct = Some("10.0.0.1:7777".into());
    });
    run(&mut [&mut a, &mut b], None, 250, 800, 5);
    let sb = status_of(&dir_b);
    assert_eq!(sb.state, LobbyState::Failed);
    assert_eq!(sb.refused.as_deref(), Some("version_mismatch"));
    assert!(sb.error.unwrap().contains("same game build"));
    assert!(a.endpoint().unwrap().peers().is_empty());
    let accept = VersionCheck {
        mod_version: 1,
        build: "1.2.38.4".into(),
        send_hello: false,
    };
    let h = |build: &str, mod_version| Hello {
        mod_version,
        build: build.into(),
        name: String::new(),
    };
    assert!(accept.accepts(&h("1.2.38.4", 1)));
    assert!(!accept.accepts(&h("1.2.38.5", 1)));
    assert!(!accept.accepts(&h("1.2.38.4", 2)));
}

/// Two games linked over UDP on the given `(name, alt)` names.
fn linked_names(
    tag: &str,
    host: (&str, Option<&str>),
    join: (&str, Option<&str>),
    join_request_name: &str,
) -> (Status, Status) {
    let net = LoopbackNet::new();
    let (dir_a, dir_b) = (
        scratch(&format!("names-{tag}-a")),
        scratch(&format!("names-{tag}-b")),
    );
    let make = |at, dir: &Path, nonce, (name, alt): (&str, Option<&str>)| {
        let mut cfg = LobbyConfig::new(dir.to_path_buf(), nonce);
        cfg.file_thread = false;
        cfg.wall_clock_at_epoch = epoch();
        cfg.stun_servers = Vec::new();
        cfg.name = name.into();
        cfg.name_alt = alt.map(str::to_owned);
        cfg.version = Some(VersionCheck {
            mod_version: 1,
            build: "1.2.38.4".into(),
            send_hello: true,
        });
        LobbyAgent::new(net.bind(at).unwrap(), cfg)
    };
    let mut a = make(addr(1, 7777), &dir_a, 1, host);
    let mut b = make(addr(2, 7777), &dir_b, 2, join);
    run(&mut [&mut a, &mut b], None, 0, 10, 5);
    request(&dir_a, 1, |r| {
        r.role = RoleChoice::Host;
        r.name = String::new();
    });
    run(&mut [&mut a, &mut b], None, 10, 250, 5);
    request(&dir_b, 1, |r| {
        r.role = RoleChoice::Join;
        r.direct = Some("10.0.0.1:7777".into());
        r.name = join_request_name.into();
    });
    run(&mut [&mut a, &mut b], None, 250, 1_000, 5);
    let (sa, sb) = (status_of(&dir_a), status_of(&dir_b));
    assert_eq!(sa.state, LobbyState::Connected);
    assert_eq!(sb.state, LobbyState::Connected);
    (sa, sb)
}

/// No two players on one automatic name: the joining game takes its second
/// pick and says `Hello` again; the host keeps its own.
#[test]
fn a_joining_game_on_the_hosts_automatic_name_takes_its_second_pick() {
    let (sa, sb) = linked_names(
        "clash",
        ("Comet", Some("Heron")),
        ("comet", Some("Raven")),
        "",
    );
    assert_eq!(sa.name, "Comet");
    assert_eq!(sb.name, "Raven");
    assert_eq!(sa.peers[0].name, "Raven", "the host shows the second pick");
    assert_eq!(sb.peers[0].name, "Comet");
    // The automatic pick sent back with a request is still automatic.
    let (sa, sb) = linked_names(
        "clash-req",
        ("Comet", Some("Heron")),
        ("Comet", Some("Raven")),
        "Comet",
    );
    assert_eq!(
        (sa.peers[0].name.as_str(), sb.name.as_str()),
        ("Raven", "Raven")
    );
    // "Random" in a request is never a name: the automatic one stands.
    let (sa, sb) = linked_names(
        "clash-random",
        ("Comet", Some("Heron")),
        ("Comet", Some("Raven")),
        "random",
    );
    assert_eq!(
        (sa.peers[0].name.as_str(), sb.name.as_str()),
        ("Raven", "Raven")
    );
    // No clash: both keep their first picks.
    let (sa, sb) = linked_names(
        "apart",
        ("Comet", Some("Heron")),
        ("Kestrel", Some("Raven")),
        "",
    );
    assert_eq!(
        (sa.peers[0].name.as_str(), sb.peers[0].name.as_str()),
        ("Kestrel", "Comet")
    );
}

/// A typed name is kept, also when it equals the host's.
#[test]
fn a_typed_name_is_kept_even_when_it_equals_the_hosts() {
    let (sa, sb) = linked_names("typed-cfg", ("Comet", Some("Heron")), ("Comet", None), "");
    assert_eq!(
        (sa.peers[0].name.as_str(), sb.name.as_str()),
        ("Comet", "Comet")
    );
    // Typed on the page this session, over an automatic name.
    let (sa, sb) = linked_names(
        "typed-page",
        ("Comet", Some("Heron")),
        ("Lark", Some("Raven")),
        "Comet",
    );
    assert_eq!(
        (sa.peers[0].name.as_str(), sb.name.as_str()),
        ("Comet", "Comet")
    );
}

#[test]
fn heartbeat_keeps_the_status_fresh_and_stop_says_so() {
    let net = LoopbackNet::new();
    let dir = scratch("heartbeat");
    let mut a = agent(&net, addr(1, 4000), &dir, 1, false);
    a.poll(ms(0));
    let v = a.status_version();
    run(&mut [&mut a], None, 0, 2_000, 50);
    let s = status_of(&dir);
    assert!(
        s.heartbeat_ms >= files::unix_ms(epoch()) + 1_500,
        "{}",
        s.heartbeat_ms
    );
    assert_eq!(s.freshness(s.heartbeat_ms + 100), Freshness::Live);
    assert_eq!(a.status_version(), v, "nothing changed but the heartbeat");
    a.stop(ms(2_000));
    a.poll(ms(2_100));
    assert_eq!(status_of(&dir).state, LobbyState::Stopped);
    assert_eq!(a.state(), LobbyState::Stopped);
}

/// A loopback socket whose link can be cut both ways.
struct Cuttable {
    inner: LoopbackSocket,
    cut: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Transport for Cuttable {
    fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> std::io::Result<()> {
        if self.cut.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        self.inner.send_to(now, to, data)
    }

    fn recv_from(
        &mut self,
        now: Duration,
        buf: &mut [u8],
    ) -> std::io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.inner.recv_from(now, buf)?;
            if got.is_none() || !self.cut.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(got);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

type CutAgent = LobbyAgent<Cuttable>;

/// Host and joiner linked over UDP (by address or by code), behind one switch.
fn udp_pair(
    name: &str,
    rejoin: Rejoin,
    by_code: bool,
) -> (
    CutAgent,
    CutAgent,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let net = LoopbackNet::new();
    let cut = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dirs = [scratch(&format!("{name}-h")), scratch(&format!("{name}-j"))];
    let make = |at, dir: &Path, nonce| {
        let mut cfg = LobbyConfig::new(dir.to_path_buf(), nonce);
        cfg.file_thread = false;
        cfg.wall_clock_at_epoch = epoch();
        cfg.stun_servers = Vec::new();
        cfg.udp_rejoin = rejoin;
        let t = Cuttable {
            inner: net.bind(at).unwrap(),
            cut: cut.clone(),
        };
        LobbyAgent::new(t, cfg)
    };
    let mut h = make(addr(1, 4000), &dirs[0], 0x1111);
    let mut j = make(addr(2, 4000), &dirs[1], 0x2222);
    let host_code = ConnectionCode {
        addr: addr(1, 4000),
        nonce: 0x1111,
    }
    .encode();
    let join_code = ConnectionCode {
        addr: addr(2, 4000),
        nonce: 0x2222,
    }
    .encode();
    cut_run(&mut h, &mut j, 0, 10);
    request(&dirs[0], 1, |r| {
        r.role = RoleChoice::Host;
        if by_code {
            r.partner_code = Some(join_code.clone());
        }
    });
    cut_run(&mut h, &mut j, 10, 250);
    request(&dirs[1], 1, |r| {
        r.role = RoleChoice::Join;
        if by_code {
            r.partner_code = Some(host_code.clone());
        } else {
            r.direct = Some("10.0.0.1:4000".into());
        }
    });
    cut_run(&mut h, &mut j, 250, 600);
    assert_eq!(h.state(), LobbyState::Connected, "{name}: linked");
    assert_eq!(j.state(), LobbyState::Connected);
    // A run in progress: newcomers are refused from now on.
    h.endpoint_mut()
        .unwrap()
        .set_refusing(Some(RefuseReason::RunInProgress));
    drain(&mut h);
    drain(&mut j);
    (h, j, cut)
}

fn cut_run(h: &mut CutAgent, j: &mut CutAgent, from: u64, to: u64) {
    let mut t = from;
    while t < to {
        h.poll(ms(t));
        j.poll(ms(t));
        t += 5;
    }
}

fn drain(a: &mut CutAgent) -> Vec<Event> {
    std::iter::from_fn(|| a.next_event())
        .filter(|e| !matches!(e, Event::Message { .. }))
        .collect()
}

#[test]
fn a_udp_session_comes_back_into_its_slot_after_an_outage_inside_the_window() {
    for by_code in [false, true] {
        let name = if by_code {
            "udp-rejoin-code"
        } else {
            "udp-rejoin-lan"
        };
        let (mut h, mut j, cut) = udp_pair(name, Rejoin::Within(ms(20_000)), by_code);
        // 18 s with the cable out: both time out after 10 s and keep trying.
        cut.store(true, std::sync::atomic::Ordering::Relaxed);
        cut_run(&mut h, &mut j, 600, 18_600);
        let lost = Event::Disconnected {
            peer: PeerId(1),
            reason: DisconnectReason::Timeout,
        };
        assert_eq!(drain(&mut h), vec![lost], "{name}");
        assert_eq!(
            drain(&mut j),
            vec![Event::Disconnected {
                peer: PeerId::HOST,
                reason: DisconnectReason::Timeout
            }]
        );
        assert!(h.reconnecting() && j.reconnecting(), "{name}");
        assert_eq!(
            j.state(),
            LobbyState::Connecting,
            "{name}: the joiner punches"
        );
        // Back: the same slot, mid-run, data flows.
        cut.store(false, std::sync::atomic::Ordering::Relaxed);
        cut_run(&mut h, &mut j, 18_600, 19_200);
        assert!(
            drain(&mut h).contains(&Event::Connected {
                peer: PeerId(1),
                addr: addr(2, 4000)
            }),
            "{name}"
        );
        assert!(matches!(
            drain(&mut j).as_slice(),
            [Event::Connected {
                peer: PeerId::HOST,
                ..
            }]
        ));
        assert!(!h.reconnecting() && !j.reconnecting());
        assert_eq!(h.endpoint().unwrap().peers(), vec![PeerId(1)]);
        assert_eq!(j.endpoint().unwrap().local_peer(), Some(PeerId(1)));
        assert_eq!(h.state(), LobbyState::Connected);
        assert_eq!(j.state(), LobbyState::Connected);
    }
}

/// A held link waits past any window, even with rejoin off: 300 s out, then
/// back into the same slot.
#[test]
fn a_held_udp_link_waits_for_as_long_as_it_takes() {
    for by_code in [false, true] {
        let (mut h, mut j, cut) = udp_pair("udp-hold", Rejoin::Off, by_code);
        h.hold_link(true);
        j.hold_link(true);
        cut.store(true, std::sync::atomic::Ordering::Relaxed);
        cut_run(&mut h, &mut j, 600, 300_600);
        assert!(h.reconnecting() && j.reconnecting(), "{by_code}");
        // With the joiner's code the host punches from the slot itself.
        let slot = usize::from(!by_code);
        assert_eq!(h.endpoint().unwrap().reserved().len(), slot, "{by_code}");
        assert_eq!(j.state(), LobbyState::Connecting);
        drain(&mut h);
        drain(&mut j);
        cut.store(false, std::sync::atomic::Ordering::Relaxed);
        cut_run(&mut h, &mut j, 300_600, 301_200);
        assert!(
            drain(&mut h).contains(&Event::Connected {
                peer: PeerId(1),
                addr: addr(2, 4000)
            }),
            "{by_code}"
        );
        assert_eq!(j.state(), LobbyState::Connected);
        assert!(h.link_held() && j.link_held());
    }
}

#[test]
fn a_udp_session_ends_when_the_outage_outlasts_the_window_or_rejoin_is_off() {
    for rejoin in [Rejoin::Within(ms(20_000)), Rejoin::Off] {
        let (mut h, mut j, cut) = udp_pair("udp-rejoin-over", rejoin, true);
        cut.store(true, std::sync::atomic::Ordering::Relaxed);
        cut_run(&mut h, &mut j, 600, 40_600);
        assert!(!h.reconnecting() && !j.reconnecting(), "{rejoin:?}");
        assert!(h.endpoint().unwrap().reserved().is_empty(), "{rejoin:?}");
        assert_eq!(j.state(), LobbyState::Failed, "{rejoin:?}");
        assert_eq!(h.state(), LobbyState::Waiting, "{rejoin:?}");
        drain(&mut h);
        drain(&mut j);
        // The cable is back: nothing comes back by itself.
        cut.store(false, std::sync::atomic::Ordering::Relaxed);
        cut_run(&mut h, &mut j, 40_600, 42_000);
        assert!(
            !drain(&mut h)
                .iter()
                .any(|e| matches!(e, Event::Connected { .. })),
            "{rejoin:?}"
        );
        assert!(h.endpoint().unwrap().peers().is_empty());
    }
}

/// On its own thread the files keep their contract: the same status text,
/// requests taken, the stopped status on disk after the agent is gone.
#[test]
fn threaded_files_keep_their_contract() {
    fn wait(mut what: impl FnMut() -> bool) -> bool {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < end {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
    let net = LoopbackNet::new();
    let dir = scratch("threaded");
    let mut cfg = LobbyConfig::new(dir.clone(), 0x7A);
    cfg.wall_clock_at_epoch = epoch();
    cfg.stun_servers = Vec::new();
    assert!(cfg.file_thread, "the default");
    let mut a = LobbyAgent::new(net.bind(addr(1, 7777)).unwrap(), cfg);
    a.poll(ms(0));
    let status = dir.join(STATUS_FILE);
    let text = files::status_text(a.status()).unwrap();
    assert!(
        wait(|| std::fs::read_to_string(&status).is_ok_and(|t| t == text)),
        "status.toml as an inline write makes it"
    );
    let mut r = PeerRequest::new(1, &files::session_id(0x7A));
    r.role = RoleChoice::Host;
    write_peer(&dir.join(PEER_FILE), &r).unwrap();
    let mut t = 0;
    assert!(
        wait(|| {
            t += 20;
            a.poll(ms(t));
            a.status().state == LobbyState::Waiting
        }),
        "the request is taken: {:?}",
        a.status().state
    );
    a.stop(ms(t + 20));
    drop(a);
    let s = read_status(&status).unwrap().unwrap();
    assert_eq!(
        s.state,
        LobbyState::Stopped,
        "the last status is written before the agent goes"
    );
}

/// A STUN server given by name; the tests' lookups map it to [`STUN_ADDR`].
const STUN_NAME: &str = "stun.test:3478";

/// The longest a timed poll may take: far below any lookup's wait.
const POLL_BOUND: Duration = Duration::from_millis(100);

/// An agent asking [`STUN_NAME`] through `resolve`, without the files.
fn named_stun_agent(net: &LoopbackNet, nonce: u32, resolve: StunResolve) -> Agent {
    let mut cfg = LobbyConfig::new(scratch(&format!("stun-lookup-{nonce:x}")), nonce);
    cfg.files = false;
    cfg.file_thread = false;
    cfg.wall_clock_at_epoch = epoch();
    cfg.stun_servers = vec![STUN_NAME.into()];
    cfg.stun_resolve = resolve;
    LobbyAgent::new(net.bind(addr(1, 4000)).unwrap(), cfg)
}

/// One poll at `t`, timed against [`POLL_BOUND`].
fn timed_poll(a: &mut Agent, t: u64) {
    let started = std::time::Instant::now();
    a.poll(ms(t));
    let took = started.elapsed();
    assert!(took < POLL_BOUND, "the poll at {t} ms took {took:?}");
}

/// Polls from `t` on until `done`; returns the agent time it was reached.
fn poll_until(
    a: &mut Agent,
    stun: &mut FakeStun,
    mut t: u64,
    step: u64,
    done: impl Fn(&Agent) -> bool,
) -> Option<u64> {
    for _ in 0..1000 {
        timed_poll(a, t);
        stun.serve(ms(t));
        if done(a) {
            return Some(t);
        }
        std::thread::sleep(Duration::from_millis(2));
        t += step;
    }
    None
}

// The `StunResolve` signature.
#[allow(clippy::unnecessary_wraps)]
fn hung_lookup(_: &str) -> io::Result<Vec<SocketAddr>> {
    std::thread::sleep(Duration::from_secs(3600));
    Ok(Vec::new())
}

/// A lookup that never answers holds no poll: pending, then failed past
/// [`LOOKUP_SLOW`] with retries, and one thread until it is given up.
#[test]
fn a_lookup_that_never_answers_does_not_hold_the_poll() {
    let net = LoopbackNet::new();
    let mut a = named_stun_agent(&net, 0xB1, hung_lookup);
    timed_poll(&mut a, 0);
    assert_eq!(a.status().stun.state, StunState::Pending);
    for t in (50..4_950).step_by(50) {
        timed_poll(&mut a, t);
    }
    assert_eq!(
        a.status().stun.state,
        StunState::Pending,
        "pending until slow"
    );
    timed_poll(&mut a, 5_000);
    let s = a.status();
    assert_eq!(s.stun.state, StunState::Failed, "slow counts as failed");
    assert_eq!(s.stun.retry_in_ms, Some(1_000), "the first back-off");
    for t in (5_050..59_950).step_by(50) {
        timed_poll(&mut a, t);
    }
    assert!(
        a.status().stun.retry_in_ms.unwrap() <= 15_000,
        "capped at stun_retry"
    );
    assert_eq!(a.stun_lookups_asked(), 1, "one lookup in flight");
    for t in (60_000..80_000).step_by(50) {
        timed_poll(&mut a, t);
    }
    assert_eq!(
        a.stun_lookups_asked(),
        2,
        "given up after a minute, then asked again"
    );
    assert!(a.code().is_none());
}

// The `StunResolve` signature.
#[allow(clippy::unnecessary_wraps)]
fn slow_lookup(_: &str) -> io::Result<Vec<SocketAddr>> {
    std::thread::sleep(Duration::from_millis(300));
    Ok(vec![STUN_ADDR.parse().unwrap()])
}

/// A lookup that answers after it counted as failed is taken at once.
#[test]
fn a_late_lookup_answer_is_taken() {
    let net = LoopbackNet::new();
    let mut stun = FakeStun::bind(&net);
    let mut a = named_stun_agent(&net, 0xB2, slow_lookup);
    timed_poll(&mut a, 0);
    assert_eq!(a.status().stun.state, StunState::Pending);
    // Past the slow mark before the answer (300 ms of real time) is in.
    timed_poll(&mut a, 5_000);
    assert_eq!(a.status().stun.state, StunState::Failed);
    let ok = poll_until(&mut a, &mut stun, 5_001, 1, |a| {
        a.status().stun.state == StunState::Ok
    });
    let at = ok.expect("the late answer is taken and discovery finishes");
    assert!(at < 6_000, "taken before the back-off ends: {at} ms");
    assert!(stun.requests > 0);
    assert!(a.code().is_some());
}

static FLAKY_CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn flaky_lookup(_: &str) -> io::Result<Vec<SocketAddr>> {
    if FLAKY_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no such host"));
    }
    Ok(vec![STUN_ADDR.parse().unwrap()])
}

/// A failed lookup is retried after the back-off, and the address is kept.
#[test]
fn a_failed_lookup_is_retried() {
    use std::sync::atomic::Ordering;
    let net = LoopbackNet::new();
    let mut stun = FakeStun::bind(&net);
    let mut a = named_stun_agent(&net, 0xB3, flaky_lookup);
    let failed = poll_until(&mut a, &mut stun, 0, 1, |a| {
        a.status().stun.state == StunState::Failed
    })
    .expect("the failure is taken");
    let s = a.status();
    assert!(s.stun.error.as_deref().unwrap().contains(STUN_NAME));
    assert!(
        s.stun.retry_in_ms.unwrap() <= 1_000,
        "{:?}",
        s.stun.retry_in_ms
    );
    assert_eq!(FLAKY_CALLS.load(Ordering::SeqCst), 1);
    let ok = poll_until(&mut a, &mut stun, failed + 1, 10, |a| {
        a.status().stun.state == StunState::Ok
    })
    .expect("the retry finds the server");
    assert!(ok >= failed + 1_000, "not before the back-off");
    assert_eq!(FLAKY_CALLS.load(Ordering::SeqCst), 2);
    assert!(a.code().is_some());
    // Kept: the refreshes (every 25 s) reuse the address.
    for t in (ok..ok + 60_000).step_by(100) {
        timed_poll(&mut a, t);
        stun.serve(ms(t));
    }
    assert_eq!(
        FLAKY_CALLS.load(Ordering::SeqCst),
        2,
        "resolved once and kept"
    );
    assert_eq!(a.status().stun.state, StunState::Ok);
}
