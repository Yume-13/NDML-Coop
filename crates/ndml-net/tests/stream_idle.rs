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

//! An unreliable stream silent for more than half the 16-bit packet seq
//! space, then speaking again: every part is taken from its first packet.

#![allow(clippy::unwrap_used)] // a test: fixtures

use std::net::SocketAddr;
use std::time::Duration;

use ndml_net::endpoint::{Endpoint, EndpointConfig, Event, Role};
use ndml_net::msg::{EnemySnapshot, EnemyState, Message};
use ndml_net::transport::{LoopbackNet, LoopbackSocket};

type Ep = Endpoint<LoopbackSocket>;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], port))
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

fn enemies(n: u16) -> Vec<EnemyState> {
    (0..n)
        .map(|i| EnemyState {
            seg: 0,
            net_id: i,
            x: f32::from(i),
            y: 10.0,
            angle: 0.0,
            hull_frac: 1.0,
            target: None,
        })
        .collect()
}

/// A linked host and client at the returned time.
fn linked() -> (Ep, Ep, u64) {
    let net = LoopbackNet::new();
    let mut hc = EndpointConfig::new(Role::Host, 1);
    hc.max_peers = 2;
    let mut host = Endpoint::new(net.bind(addr(1)).unwrap(), hc);
    let mut client = Endpoint::new(
        net.bind(addr(2)).unwrap(),
        EndpointConfig::new(Role::Client, 2),
    );
    let _ = client.connect(ms(0), addr(1), None);
    let mut t = 0;
    while t < 50 {
        host.poll(ms(t)).unwrap();
        client.poll(ms(t)).unwrap();
        t += 1;
    }
    assert_eq!(host.peers().len(), 1);
    while host.next_event().is_some() {}
    while client.next_event().is_some() {}
    (host, client, t)
}

/// One snapshot from the host, then a poll of both: parts taken (part 0, rest).
fn step(host: &mut Ep, client: &mut Ep, t: u64, e: &[EnemyState]) -> (u32, u32) {
    for part in EnemySnapshot::split(u32::try_from(t).unwrap(), e) {
        host.broadcast(&Message::EnemySnapshot(part), None).unwrap();
    }
    host.poll(ms(t)).unwrap();
    client.poll(ms(t)).unwrap();
    let (mut p0, mut p1) = (0, 0);
    while let Some(ev) = client.next_event() {
        if let Event::Message {
            msg: Message::EnemySnapshot(s),
            ..
        } = ev
        {
            if s.part == 0 {
                p0 += 1;
            } else {
                p1 += 1;
            }
        }
    }
    (p0, p1)
}

#[test]
fn a_second_snapshot_part_is_taken_after_a_quiet_spell() {
    let (mut host, mut client, mut t) = linked();
    let (big, small) = (enemies(95), enemies(40)); // two parts, one part
    for _ in 0..400 {
        assert_eq!(step(&mut host, &mut client, t, &big), (1, 1));
        t += 5;
    }
    // 200 s of one part (40,000 packets): part 1 idle past half the seq space.
    for _ in 0..40_000 {
        step(&mut host, &mut client, t, &small);
        t += 5;
    }
    for i in 0..2_000 {
        assert_eq!(
            step(&mut host, &mut client, t, &big),
            (1, 1),
            "snapshot {i} of the next heavy moment"
        );
        t += 5;
    }
}

#[test]
fn every_stream_is_taken_after_an_hour_linked_in_the_menus() {
    let (mut host, mut client, mut t) = linked();
    let big = enemies(95);
    for _ in 0..100 {
        assert_eq!(step(&mut host, &mut client, t, &big), (1, 1));
        t += 5;
    }
    // 70 minutes with nothing sent: keepalives only.
    let end = t + 70 * 60 * 1000;
    while t < end {
        host.poll(ms(t)).unwrap();
        client.poll(ms(t)).unwrap();
        while client.next_event().is_some() {}
        while host.next_event().is_some() {}
        t += 50;
    }
    assert!(client.is_connected(ndml_net::PeerId::HOST), "still linked");
    for i in 0..100 {
        assert_eq!(
            step(&mut host, &mut client, t, &big),
            (1, 1),
            "snapshot {i} of the next run"
        );
        t += 5;
    }
}
