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

//! Lobby agent tests for Steam sessions: agents over [`SteamMux`] with a fake
//! Steam that moves datagrams between bridges by Steam id, and can cut, lose,
//! refuse, reorder, duplicate or stall them. It does not model P2P session
//! setup through NAT or a relay, the lobby service or the overlay.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::files::{self, PeerRequest, SteamFileAction, read_status, write_peer};
use super::*;
use crate::endpoint::{DisconnectReason, Event, RefuseReason, Rejoin};
use crate::msg::PeerId;
use crate::steam::{self as st, SteamBridge, SteamMux, SteamTransport};
use crate::transport::{LoopbackNet, LoopbackSocket};

const ALICE: u64 = 0x0110_0001_0000_00A1;
const BOB: u64 = 0x0110_0001_0000_0B0B;
const CAROL: u64 = 0x0110_0001_0000_0CA1;

type SAgent = LobbyAgent<SteamMux<LoopbackSocket, SteamTransport>>;

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

fn addr(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, last], port))
}

fn epoch() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn scratch(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-tmp/ndml-net-lobby-steam")
        .join(name);
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn status_of(dir: &Path) -> Status {
    read_status(&dir.join(STATUS_FILE)).unwrap().unwrap()
}

/// A fake Steam: moves datagrams between bridges by Steam id. `cut`,
/// `loss_pct`, `refuse_pct`, `reorder` and `dup_pct` shape delivery;
/// `stalled` is an id whose game thread does not pump; `per_pump` caps sends
/// per pump (0: none); `out_capacity` sizes later agents' queues (0: default);
/// `drop_after_stall` drops the stalled side's backlog, as the co-op pump does.
#[derive(Default)]
struct FakeSteam {
    bridges: HashMap<u64, SteamBridge>,
    stalled: Option<u64>,
    was_stalled: Option<u64>,
    drop_after_stall: bool,
    per_pump: usize,
    out_capacity: usize,
    held: Vec<(u64, u64, Vec<u8>)>,
    cut: bool,
    loss_pct: u64,
    refuse_pct: u64,
    reorder: bool,
    dup_pct: u64,
    refused: u64,
    rng: u64,
}

impl FakeSteam {
    /// A deterministic draw in 0..100.
    fn roll(&mut self) -> u64 {
        self.rng = self
            .rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.rng >> 33) % 100
    }

    fn pump(&mut self) {
        if self.stalled.is_none()
            && self.drop_after_stall
            && let Some(id) = self.was_stalled.take()
        {
            self.held.clear();
            if let Some(b) = self.bridges.get(&id) {
                while b.pop_outbound().is_some() {}
            }
        }
        if self.stalled.is_some() {
            self.was_stalled = self.stalled;
        }
        if self.stalled.is_none() {
            for (from, to, data) in std::mem::take(&mut self.held) {
                if let Some(b) = self.bridges.get(&to) {
                    let _ = b.deliver(from, data);
                }
            }
        }
        let mut moving = Vec::new();
        for (&from, b) in &self.bridges {
            if self.stalled == Some(from) {
                continue;
            }
            let mut n = 0;
            while self.per_pump == 0 || n < self.per_pump {
                let Some(d) = b.pop_outbound() else { break };
                moving.push((from, d));
                n += 1;
            }
        }
        if self.cut {
            return;
        }
        if self.reorder {
            moving.reverse();
        }
        for (from, d) in moving {
            if self.refuse_pct > 0 && self.roll() < self.refuse_pct {
                self.refused += 1;
                continue;
            }
            if self.loss_pct > 0 && self.roll() < self.loss_pct {
                continue;
            }
            let twice = self.dup_pct > 0 && self.roll() < self.dup_pct;
            if self.stalled == Some(d.peer) {
                self.held.push((from, d.peer, d.data));
                continue;
            }
            if let Some(to) = self.bridges.get(&d.peer) {
                if twice {
                    let _ = to.deliver(from, d.data.clone());
                }
                let _ = to.deliver(from, d.data);
            }
        }
    }
}

fn steam_agent_with(
    net: &LoopbackNet,
    steam: &mut FakeSteam,
    id: u64,
    last: u8,
    dir: &Path,
    nonce: u32,
    rejoin: Rejoin,
) -> SAgent {
    let cap = if steam.out_capacity == 0 {
        st::OUT_CAPACITY
    } else {
        steam.out_capacity
    };
    let (t, b) = st::channel(cap, st::IN_CAPACITY);
    b.set_local_id(id);
    steam.bridges.insert(id, b);
    let mut cfg = LobbyConfig::new(dir.to_path_buf(), nonce);
    // The files at the call: these tests run on a synthetic clock.
    cfg.file_thread = false;
    cfg.wall_clock_at_epoch = epoch();
    cfg.stun_servers = Vec::new();
    cfg.steam_rejoin = rejoin;
    cfg.version = Some(VersionCheck {
        mod_version: 1,
        build: "b".into(),
        send_hello: true,
    });
    LobbyAgent::new(SteamMux::new(net.bind(addr(last, 4000)).unwrap(), t), cfg)
}

fn steam_agent(
    net: &LoopbackNet,
    steam: &mut FakeSteam,
    id: u64,
    last: u8,
    dir: &Path,
    nonce: u32,
) -> SAgent {
    steam_agent_with(net, steam, id, last, dir, nonce, Rejoin::Always)
}

fn run(agents: &mut [&mut SAgent], steam: &mut FakeSteam, from: u64, to: u64, step: u64) {
    let mut t = from;
    while t < to {
        for a in agents.iter_mut() {
            a.poll(ms(t));
        }
        steam.pump();
        t += step;
    }
}

fn events(a: &mut SAgent) -> Vec<Event> {
    std::iter::from_fn(|| a.next_event()).collect()
}

#[test]
fn a_steam_session_links_and_the_status_holds_no_ids() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let (dh, dc) = (scratch("host"), scratch("join"));
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &dh, 0xA1);
    let mut join = steam_agent(&net, &mut steam, BOB, 2, &dc, 0xB0);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    assert!(host.steam_active() && join.steam_active());
    run(&mut [&mut host, &mut join], &mut steam, 0, 300, 5);
    assert_eq!(
        events(&mut host)[0],
        Event::Connected {
            peer: PeerId(1),
            addr: st::steam_addr(BOB)
        }
    );
    assert!(events(&mut join).contains(&Event::Connected {
        peer: PeerId::HOST,
        addr: st::steam_addr(ALICE)
    }));
    let s = status_of(&dh);
    assert_eq!(s.connector, Connector::Steam);
    assert_eq!(s.state, LobbyState::Connected);
    assert_eq!(s.role, RoleChoice::Host);
    assert!(s.peers[0].addr.starts_with("steam:"), "{:?}", s.peers);
    assert_eq!(s.peers[0].name, "Pilot");
    let text = std::fs::read_to_string(dh.join(STATUS_FILE)).unwrap();
    assert!(
        !text.contains("fd4e") && !text.contains(&BOB.to_string()),
        "{text}"
    );
    assert_eq!(status_of(&dc).state, LobbyState::Connected);
    // The game thread's view goes into [steam].
    host.set_steam_status(Some(SteamStatus {
        available: true,
        page: "linked".into(),
        ..SteamStatus::default()
    }));
    run(&mut [&mut host], &mut steam, 300, 310, 5);
    assert_eq!(status_of(&dh).steam.unwrap().page, "linked");
    // A re-sent join (same host) leaves the link alone.
    join.steam_request(ms(310), &SteamRequest::Join { host: ALICE });
    assert!(events(&mut join).is_empty());
    // Leave: goodbyes, idle.
    join.steam_request(ms(320), &SteamRequest::Leave);
    run(&mut [&mut host, &mut join], &mut steam, 320, 400, 5);
    assert!(!join.steam_active());
    assert_eq!(status_of(&dc).state, LobbyState::Ready);
    assert!(events(&mut host).contains(&Event::Disconnected {
        peer: PeerId(1),
        reason: DisconnectReason::Remote
    }));
}

/// A Steam session goes by the Steam names; leaving brings the configured
/// name back, and a Steam name that arrives late waits for the next session.
#[test]
fn a_steam_session_goes_by_the_steam_names() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let (dh, dc) = (scratch("names-h"), scratch("names-c"));
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &dh, 0xA1);
    let mut join = steam_agent(&net, &mut steam, BOB, 2, &dc, 0xB0);
    host.steam_request(ms(0), &SteamRequest::Name("Star Fox Fan".into()));
    join.steam_request(ms(0), &SteamRequest::Name("  Night Owl ".into()));
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 0, 300, 5);
    let (sh, sc) = (status_of(&dh), status_of(&dc));
    assert_eq!(sh.state, LobbyState::Connected);
    assert_eq!(sh.name, "Star Fox Fan");
    assert_eq!(sh.peers[0].name, "Night Owl", "cleaned as a typed name");
    assert_eq!(sc.peers[0].name, "Star Fox Fan");
    join.steam_request(ms(300), &SteamRequest::Leave);
    run(&mut [&mut host, &mut join], &mut steam, 300, 400, 5);
    assert_eq!(status_of(&dc).name, "Pilot", "the configured name again");

    // No Steam name yet: the configured name; a late one waits.
    let (dh, dc) = (scratch("names-late-h"), scratch("names-late-c"));
    let mut host = steam_agent(&net, &mut steam, CAROL, 3, &dh, 0xC1);
    let mut join = steam_agent(&net, &mut steam, BOB, 4, &dc, 0xB1);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: CAROL });
    run(&mut [&mut host, &mut join], &mut steam, 0, 300, 5);
    assert_eq!(status_of(&dh).peers[0].name, "Pilot");
    join.steam_request(ms(300), &SteamRequest::Name("Night Owl".into()));
    run(&mut [&mut host, &mut join], &mut steam, 300, 400, 5);
    assert_eq!(status_of(&dc).name, "Pilot", "not on the link that is up");
    assert_eq!(status_of(&dh).peers[0].name, "Pilot");
}

#[test]
fn only_lobby_members_get_in() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &scratch("allow-h"), 1);
    let dc = scratch("allow-c");
    let mut carol = steam_agent(&net, &mut steam, CAROL, 3, &dc, 3);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    carol.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut carol], &mut steam, 0, 31_000, 50);
    assert!(events(&mut host).is_empty(), "silently ignored");
    let s = status_of(&dc);
    assert_eq!(s.state, LobbyState::Failed);
    assert_eq!(s.error_kind, Some(ErrorKind::HandshakeTimeout));
    assert!(s.error.unwrap().contains("Steam could not connect"));
    // Once Carol is a member (the list is sent again), Carol gets in.
    host.steam_request(
        ms(31_000),
        &SteamRequest::Host {
            allow: vec![BOB, CAROL],
        },
    );
    carol.steam_request(ms(31_000), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut carol], &mut steam, 31_000, 31_300, 5);
    assert!(
        events(&mut host)
            .iter()
            .any(|e| matches!(e, Event::Connected { .. }))
    );
}

/// The host's member list lags the joiner's first punches: the same join
/// attempt gets in once the list names it.
#[test]
fn a_member_the_host_sees_late_gets_in_on_the_same_try() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &scratch("late-h"), 1);
    let dc = scratch("late-c");
    let mut join = steam_agent(&net, &mut steam, BOB, 2, &dc, 2);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 0, 2_500, 10);
    assert!(events(&mut host).is_empty(), "not a member yet: no answer");
    host.steam_request(ms(2_500), &SteamRequest::Host { allow: vec![BOB] });
    run(&mut [&mut host, &mut join], &mut steam, 2_500, 8_000, 10);
    assert!(
        events(&mut host)
            .iter()
            .any(|e| matches!(e, Event::Connected { .. })),
        "in on the same try"
    );
    assert_eq!(status_of(&dc).state, LobbyState::Connected);
}

/// Steam drops what is sent before a P2P session is up: a handshake whose
/// first seconds are lost still links.
#[test]
fn a_slow_steam_session_setup_still_links() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &scratch("slow-h"), 1);
    let dc = scratch("slow-c");
    let mut join = steam_agent(&net, &mut steam, BOB, 2, &dc, 2);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    steam.cut = true;
    run(&mut [&mut host, &mut join], &mut steam, 0, 6_000, 10);
    steam.cut = false;
    run(&mut [&mut host, &mut join], &mut steam, 6_000, 12_000, 10);
    assert!(
        events(&mut host)
            .iter()
            .any(|e| matches!(e, Event::Connected { .. }))
    );
    assert_eq!(status_of(&dc).state, LobbyState::Connected);
}

/// Link up, cut the "cable" for `seconds` mid-run, reconnect.
fn outage(seconds: u64) {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let (dh, dc) = (
        scratch(&format!("out-h{seconds}")),
        scratch(&format!("out-c{seconds}")),
    );
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &dh, 0xA1);
    let mut join = steam_agent(&net, &mut steam, BOB, 2, &dc, 0xB0);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 0, 300, 5);
    events(&mut host);
    events(&mut join);
    // Mid-run: newcomers are refused; the slot is kept regardless.
    host.endpoint_mut()
        .unwrap()
        .set_refusing(Some(RefuseReason::RunInProgress));
    steam.cut = true;
    let end = 300 + seconds * 1000;
    run(&mut [&mut host, &mut join], &mut steam, 300, end, 50);
    if seconds >= 11 {
        let (h, c) = (status_of(&dh), status_of(&dc));
        assert!(h.reconnecting && c.reconnecting, "{h:?} {c:?}");
        assert_eq!(h.state, LobbyState::Waiting);
        assert_eq!(c.state, LobbyState::Connecting);
        assert_eq!(c.error_kind, Some(ErrorKind::LinkLost));
        assert!(events(&mut host).contains(&Event::Disconnected {
            peer: PeerId(1),
            reason: DisconnectReason::Timeout
        }));
    }
    steam.cut = false;
    run(&mut [&mut host, &mut join], &mut steam, end, end + 2_000, 5);
    assert_eq!(
        host.endpoint().unwrap().peers(),
        vec![PeerId(1)],
        "same slot"
    );
    assert_eq!(join.endpoint().unwrap().local_peer(), Some(PeerId(1)));
    let (h, c) = (status_of(&dh), status_of(&dc));
    assert!(!h.reconnecting && !c.reconnecting);
    assert_eq!(
        (h.state, c.state),
        (LobbyState::Connected, LobbyState::Connected)
    );
    assert!(c.error.is_none());
}

#[test]
fn a_five_second_outage_rides_through() {
    outage(5);
}

#[test]
fn a_thirty_second_outage_reconnects_into_the_same_slot() {
    outage(30);
}

#[test]
fn a_three_minute_outage_reconnects_into_the_same_slot() {
    outage(180);
}

#[test]
fn a_limited_rejoin_window_gives_up() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let dc = scratch("window-c");
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &scratch("window-h"), 1);
    let mut join = steam_agent_with(&net, &mut steam, BOB, 2, &dc, 2, Rejoin::Within(ms(20_000)));
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 0, 300, 5);
    steam.cut = true;
    run(&mut [&mut host, &mut join], &mut steam, 300, 80_000, 100);
    let c = status_of(&dc);
    assert_eq!(c.state, LobbyState::Failed);
    assert!(!c.reconnecting);
    assert!(!join.steam_active());
}

#[test]
fn peer_file_steam_actions_go_to_the_game_thread_and_udp_replaces_steam() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let dir = scratch("file");
    let mut a = steam_agent(&net, &mut steam, ALICE, 1, &dir, 0x51);
    a.poll(ms(0));
    let mut r = PeerRequest::new(1, &status_of(&dir).session);
    r.steam = Some(SteamFileAction::JoinCode);
    r.steam_code = Some("k7qx2".into());
    r.role = RoleChoice::Host; // ignored: the request is a Steam action
    write_peer(&dir.join(PEER_FILE), &r).unwrap();
    a.poll(ms(300));
    assert_eq!(
        a.take_steam_actions(),
        vec![SteamFileRequest {
            action: SteamFileAction::JoinCode,
            code: Some("k7qx2".into())
        }]
    );
    assert!(a.take_steam_actions().is_empty());
    assert_eq!(a.role(), None, "no UDP role started");
    a.poll(ms(400));
    assert_eq!(status_of(&dir).request, 1);
    // A Steam host session, then a UDP host request: UDP wins.
    a.steam_request(ms(400), &SteamRequest::Host { allow: vec![] });
    assert!(a.steam_active());
    a.request(
        ms(410),
        &LobbyRequest {
            role: RoleChoice::Host,
            ..LobbyRequest::default()
        },
    );
    a.poll(ms(420));
    assert!(!a.steam_active());
    let s = status_of(&dir);
    assert_eq!(
        (s.connector, s.state),
        (Connector::Udp, LobbyState::Waiting)
    );
    let _ = files::FORMAT;
}

/// Host and joiner linked over the fake Steam, events drained.
fn linked(
    name: &str,
    steam: &mut FakeSteam,
    net: &LoopbackNet,
) -> (SAgent, SAgent, PathBuf, PathBuf) {
    let (dh, dc) = (scratch(&format!("{name}-h")), scratch(&format!("{name}-c")));
    let mut host = steam_agent(net, steam, ALICE, 1, &dh, 0xA1);
    let mut join = steam_agent(net, steam, BOB, 2, &dc, 0xB0);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], steam, 0, 300, 5);
    assert!(
        events(&mut host)
            .iter()
            .any(|e| matches!(e, Event::Connected { .. }))
    );
    events(&mut join);
    (host, join, dh, dc)
}

/// The host sends a reliable message every `every` ms while `shape` sets the
/// fake's conditions; returns the rounds the joiner got and the disconnects.
fn stream(
    host: &mut SAgent,
    join: &mut SAgent,
    steam: &mut FakeSteam,
    (from, to, every): (u64, u64, u64),
    shape: &mut dyn FnMut(&mut FakeSteam, u64),
) -> (Vec<u16>, Vec<Event>) {
    let mut got = Vec::new();
    let mut gone = Vec::new();
    let mut next: u16 = 0;
    let mut t = from;
    let take = |host: &mut SAgent, join: &mut SAgent, got: &mut Vec<u16>, gone: &mut Vec<Event>| {
        for e in events(join) {
            match e {
                Event::Message {
                    msg: crate::msg::Message::UpgradeOpen { round },
                    ..
                } => got.push(round),
                e @ Event::Disconnected { .. } => gone.push(e),
                _ => {}
            }
        }
        gone.extend(
            events(host)
                .into_iter()
                .filter(|e| matches!(e, Event::Disconnected { .. })),
        );
    };
    while t < to + 1_000 {
        if t < to {
            shape(steam, t);
            if t % every == 0 {
                let _ = host
                    .endpoint_mut()
                    .unwrap()
                    .send(PeerId(1), &crate::msg::Message::UpgradeOpen { round: next });
                next += 1;
            }
        } else {
            *steam = FakeSteam {
                bridges: std::mem::take(&mut steam.bridges),
                refused: steam.refused,
                ..FakeSteam::default()
            };
        }
        host.poll(ms(t));
        join.poll(ms(t));
        steam.pump();
        take(host, join, &mut got, &mut gone);
        t += 10;
    }
    (got, gone)
}

fn in_order(got: &[u16], n: u16) {
    let want: Vec<u16> = (0..n).collect();
    assert_eq!(
        got,
        want.as_slice(),
        "every reliable message once, in order"
    );
}

/// Loss bursts (60 % for 2 s in every 5 s): the link holds and every reliable
/// message arrives once, in order.
#[test]
fn loss_bursts_keep_the_link_and_every_message() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam {
        rng: 7,
        ..FakeSteam::default()
    };
    let (mut host, mut join, dh, dc) = linked("burst", &mut steam, &net);
    let (got, gone) = stream(
        &mut host,
        &mut join,
        &mut steam,
        (300, 60_300, 100),
        &mut |s, t| {
            s.loss_pct = if (t - 300) % 5_000 < 2_000 { 60 } else { 0 };
        },
    );
    assert!(gone.is_empty(), "{gone:?}");
    in_order(&got, 600);
    assert_eq!(status_of(&dh).state, LobbyState::Connected);
    assert_eq!(status_of(&dc).state, LobbyState::Connected);
}

/// Reordered and duplicated datagrams: exactly once, in order.
#[test]
fn reordered_and_duplicated_datagrams_arrive_once_in_order() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam {
        rng: 11,
        ..FakeSteam::default()
    };
    let (mut host, mut join, _, _) = linked("reorder", &mut steam, &net);
    let (got, gone) = stream(
        &mut host,
        &mut join,
        &mut steam,
        (300, 20_300, 20),
        &mut |s, _| {
            s.reorder = true;
            s.dup_pct = 30;
        },
    );
    assert!(gone.is_empty(), "{gone:?}");
    in_order(&got, 1_000);
}

/// Refused sends are loss: a third refused costs nothing; all refused for 15 s
/// is a lost link that reconnects into the same slot.
#[test]
fn refused_sends_are_loss_and_a_long_refusal_reconnects() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam {
        rng: 3,
        ..FakeSteam::default()
    };
    let (mut host, mut join, dh, dc) = linked("refuse", &mut steam, &net);
    let (got, gone) = stream(
        &mut host,
        &mut join,
        &mut steam,
        (300, 20_300, 100),
        &mut |s, _| {
            s.refuse_pct = 33;
        },
    );
    assert!(steam.refused > 100, "refusals counted: {}", steam.refused);
    assert!(gone.is_empty(), "{gone:?}");
    in_order(&got, 200);
    // Every send refused for 15 s.
    steam.refuse_pct = 100;
    run(&mut [&mut host, &mut join], &mut steam, 21_300, 36_300, 50);
    let (h, c) = (status_of(&dh), status_of(&dc));
    assert!(h.reconnecting && c.reconnecting, "{h:?} {c:?}");
    assert_eq!(c.error_kind, Some(ErrorKind::LinkLost));
    steam.refuse_pct = 0;
    run(&mut [&mut host, &mut join], &mut steam, 36_300, 39_300, 5);
    assert_eq!(
        host.endpoint().unwrap().peers(),
        vec![PeerId(1)],
        "same slot"
    );
    let (h, c) = (status_of(&dh), status_of(&dc));
    assert_eq!(
        (h.state, c.state),
        (LobbyState::Connected, LobbyState::Connected)
    );
}

/// A cut longer than the reconnect window: the joining game gives up, the
/// host keeps its lobby open, and a new join links.
#[test]
fn the_reconnect_window_running_out_ends_the_session_cleanly() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let (dh, dc) = (scratch("w120-h"), scratch("w120-c"));
    let window = Rejoin::Within(ms(120_000));
    let mut host = steam_agent_with(&net, &mut steam, ALICE, 1, &dh, 0xA1, window);
    let mut join = steam_agent_with(&net, &mut steam, BOB, 2, &dc, 0xB0, window);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 0, 300, 5);
    events(&mut host);
    events(&mut join);
    steam.cut = true;
    run(&mut [&mut host, &mut join], &mut steam, 300, 100_300, 100);
    assert!(
        status_of(&dc).reconnecting,
        "still trying inside the window"
    );
    run(
        &mut [&mut host, &mut join],
        &mut steam,
        100_300,
        140_300,
        100,
    );
    let (h, c) = (status_of(&dh), status_of(&dc));
    assert_eq!(c.state, LobbyState::Failed, "{c:?}");
    assert!(!c.reconnecting && !join.steam_active());
    assert!(c.error.is_some(), "the joining player is told why");
    assert!(!h.reconnecting, "the host has nothing pending: {h:?}");
    assert!(host.steam_active(), "the host's lobby stays open");
    let left: Vec<Event> = events(&mut host);
    assert!(
        left.iter().any(|e| matches!(
            e,
            Event::Disconnected {
                peer: PeerId(1),
                ..
            }
        )),
        "{left:?}"
    );
    // The cable back: nothing until the joining player joins again.
    steam.cut = false;
    run(
        &mut [&mut host, &mut join],
        &mut steam,
        140_300,
        145_300,
        50,
    );
    assert!(
        events(&mut host)
            .iter()
            .all(|e| !matches!(e, Event::Connected { .. }))
    );
    join.steam_request(ms(145_300), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 145_300, 150_300, 5);
    assert!(
        events(&mut host)
            .iter()
            .any(|e| matches!(e, Event::Connected { .. }))
    );
    assert_eq!(status_of(&dc).state, LobbyState::Connected);
}

/// A held link survives a cut far past the window; released, the window
/// applies again.
#[test]
fn a_held_link_waits_past_the_reconnect_window() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let (dh, dc) = (scratch("hold-h"), scratch("hold-c"));
    let window = Rejoin::Within(ms(120_000));
    let mut host = steam_agent_with(&net, &mut steam, ALICE, 1, &dh, 0xA1, window);
    let mut join = steam_agent_with(&net, &mut steam, BOB, 2, &dc, 0xB0, window);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 0, 300, 5);
    events(&mut host);
    events(&mut join);
    host.hold_link(true);
    join.hold_link(true);
    steam.cut = true;
    run(&mut [&mut host, &mut join], &mut steam, 300, 400_300, 100);
    let (h, c) = (status_of(&dh), status_of(&dc));
    assert!(c.reconnecting && join.steam_active(), "{c:?}");
    assert!(h.reconnecting, "the host keeps the slot: {h:?}");
    assert_ne!(c.state, LobbyState::Failed);
    steam.cut = false;
    run(&mut [&mut host, &mut join], &mut steam, 400_300, 403_300, 5);
    assert!(
        events(&mut host)
            .iter()
            .any(|e| matches!(e, Event::Connected { .. })),
        "back without a new join"
    );
    assert_eq!(status_of(&dc).state, LobbyState::Connected);
    // Released (the run is over) and cut again: the window applies.
    host.hold_link(false);
    join.hold_link(false);
    events(&mut join);
    steam.cut = true;
    run(
        &mut [&mut host, &mut join],
        &mut steam,
        403_300,
        563_300,
        100,
    );
    let c = status_of(&dc);
    assert_eq!(c.state, LobbyState::Failed, "{c:?}");
    assert!(!join.steam_active());
}

/// Events with the time they were read.
type Timed = Vec<(u64, Event)>;

/// One game's thread stalls for `stall_ms` from 1 s after linking; returns
/// the events after the stall began, per side, with their times.
fn stall_run(name: &str, who: u64, stall_ms: u64, drop_after_stall: bool) -> (Timed, Timed) {
    let net = LoopbackNet::new();
    // A queue of 128 here stands for the bridge's 512 in a game.
    let mut steam = FakeSteam {
        out_capacity: 128,
        drop_after_stall,
        ..FakeSteam::default()
    };
    let (mut host, mut join, _dh, _dc) = linked(name, &mut steam, &net);
    steam.per_pump = 64;
    // A second of play: each game sends a reliable message a frame.
    let mut t = 300;
    let mut round: u16 = 0;
    while t < 1_300 {
        if t % 15 == 0 {
            for (a, to) in [(&mut host, PeerId(1)), (&mut join, PeerId::HOST)] {
                let _ = a
                    .endpoint_mut()
                    .unwrap()
                    .send(to, &crate::msg::Message::UpgradeOpen { round });
            }
            round += 1;
        }
        host.poll(ms(t));
        join.poll(ms(t));
        steam.pump();
        t += 5;
    }
    events(&mut host);
    events(&mut join);
    let (mut he, mut je) = (Vec::new(), Vec::new());
    let end = 1_300 + stall_ms + 5_000;
    let mut t = 1_300;
    while t < end {
        steam.stalled = (t < 1_300 + stall_ms).then_some(who);
        host.poll(ms(t));
        join.poll(ms(t));
        steam.pump();
        he.extend(events(&mut host).into_iter().map(|e| (t, e)));
        je.extend(events(&mut join).into_iter().map(|e| (t, e)));
        t += 5;
    }
    (he, je)
}

fn connected_at(ev: &[(u64, Event)]) -> Vec<u64> {
    ev.iter()
        .filter(|(_, e)| matches!(e, Event::Connected { .. }))
        .map(|(t, _)| *t)
        .collect()
}

/// Stalls shorter than the 10 s timeout leave the link up on both sides.
#[test]
fn a_game_thread_stall_under_the_timeout_keeps_the_link() {
    for who in [ALICE, BOB] {
        let (he, je) = stall_run("stall5", who, 5_000, true);
        let gone = |ev: &[(u64, Event)]| {
            ev.iter()
                .any(|(_, e)| matches!(e, Event::Disconnected { .. }))
        };
        assert!(!gone(&he) && !gone(&je), "{he:?} {je:?}");
    }
}

/// Linked again within a second of the stall's end, the host first.
fn relinked_at_once(he: &[(u64, Event)], je: &[(u64, Event)], back: u64) {
    let h = connected_at(he);
    assert!(
        h.iter().any(|&t| t >= back && t < back + 1_000),
        "host: {he:?}"
    );
    let j = connected_at(je);
    assert!(
        j.iter().any(|&t| t >= back && t < back + 1_000),
        "joiner: {je:?}"
    );
    assert!(
        j.iter().all(|&t| h.iter().any(|&u| u <= t + 100)),
        "joiner linked before the host took it back: {je:?}"
    );
}

/// The host's game thread stalls 12 s: linked again within a second.
#[test]
fn a_host_stalled_past_the_timeout_links_again_at_once() {
    let (he, je) = stall_run("stall12h", ALICE, 12_000, true);
    relinked_at_once(&he, &je, 1_300 + 12_000);
}

/// The joiner's game thread stalls 12 s: linked again within a second.
#[test]
fn a_joiner_stalled_past_the_timeout_links_again_at_once() {
    let (he, je) = stall_run("stall12c", BOB, 12_000, true);
    relinked_at_once(&he, &je, 1_300 + 12_000);
}

/// Without the pump's drop: a connection this side started takes only the
/// peer's `PunchAck` as the accept, so the old link's Data is dropped.
#[test]
fn a_joiner_stalled_past_the_timeout_without_the_pumps_drop() {
    let (he, je) = stall_run("stall12c0", BOB, 12_000, false);
    relinked_at_once(&he, &je, 1_300 + 12_000);
}

/// The same for the host stalled, without the pump's drop.
#[test]
fn a_host_stalled_past_the_timeout_without_the_pumps_drop() {
    let (he, je) = stall_run("stall12h0", ALICE, 12_000, false);
    relinked_at_once(&he, &je, 1_300 + 12_000);
}

/// Two accounts on different co-op builds: the host refuses the joiner's
/// `Hello` before the link is up; the joining player reads why.
#[test]
fn a_second_id_on_another_build_is_refused_with_the_reason() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let (dh, dc) = (scratch("ver-h"), scratch("ver-c"));
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &dh, 0xA1);
    let (t, b) = st::channel(st::OUT_CAPACITY, st::IN_CAPACITY);
    b.set_local_id(BOB);
    steam.bridges.insert(BOB, b);
    let mut cfg = LobbyConfig::new(dc.clone(), 0xB0);
    cfg.file_thread = false;
    cfg.wall_clock_at_epoch = epoch();
    cfg.stun_servers = Vec::new();
    cfg.version = Some(VersionCheck {
        mod_version: 1,
        build: "other".into(),
        send_hello: true,
    });
    let mut join = LobbyAgent::new(SteamMux::new(net.bind(addr(2, 4000)).unwrap(), t), cfg);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 0, 3_000, 5);
    let c = status_of(&dc);
    assert_eq!(c.error_kind, Some(ErrorKind::Refused), "{c:?}");
    let text = c.error.unwrap_or_default();
    assert!(text.contains("Version mismatch"), "{text}");
    assert!(!status_of(&dh).reconnecting);
}

/// The host is in a run when the joiner arrives: turned away with the run's
/// sentence.
#[test]
fn a_join_while_the_host_is_in_a_run_gets_the_run_text() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let (dh, dc) = (scratch("inrun-h"), scratch("inrun-c"));
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &dh, 0xA1);
    let mut join = steam_agent(&net, &mut steam, BOB, 2, &dc, 0xB0);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![BOB] });
    host.poll(ms(0));
    host.endpoint_mut()
        .unwrap()
        .set_refusing(Some(RefuseReason::RunInProgress));
    join.steam_request(ms(10), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 10, 3_000, 5);
    let c = status_of(&dc);
    assert_eq!(c.error_kind, Some(ErrorKind::Refused), "{c:?}");
    assert_eq!(
        c.error.as_deref(),
        Some("Your partner is in a run right now. Join when they are back in the main menu.")
    );
    assert!(
        events(&mut host)
            .iter()
            .all(|e| !matches!(e, Event::Connected { .. }))
    );
}

/// The host leaves mid-join: the attempt ends within the handshake time.
#[test]
fn the_host_leaving_mid_join_ends_the_attempt_in_time() {
    let net = LoopbackNet::new();
    let mut steam = FakeSteam::default();
    let (dh, dc) = (scratch("hleft-h"), scratch("hleft-c"));
    let mut host = steam_agent(&net, &mut steam, ALICE, 1, &dh, 0xA1);
    let mut join = steam_agent(&net, &mut steam, BOB, 2, &dc, 0xB0);
    host.steam_request(ms(0), &SteamRequest::Host { allow: vec![] });
    join.steam_request(ms(0), &SteamRequest::Join { host: ALICE });
    run(&mut [&mut host, &mut join], &mut steam, 0, 500, 5);
    host.steam_request(ms(500), &SteamRequest::Leave);
    run(&mut [&mut host, &mut join], &mut steam, 500, 31_000, 50);
    let c = status_of(&dc);
    assert_eq!(c.state, LobbyState::Failed, "{c:?}");
    assert_eq!(c.error_kind, Some(ErrorKind::HandshakeTimeout));
    assert!(
        c.error
            .unwrap_or_default()
            .contains("Steam could not connect you")
    );
    assert!(!host.steam_active());
}
