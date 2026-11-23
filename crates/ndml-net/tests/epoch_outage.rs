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

//! A joiner's new connection (new epoch) after an outage whose recovery
//! straddles its 30 s attempt boundary: it ends up back in its slot.

#![allow(clippy::too_many_lines)] // a test: fixtures, one long scenario
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ndml_net::endpoint::{DisconnectReason, Endpoint, EndpointConfig, Event, Rejoin, Role};
use ndml_net::packet::RefuseReason;
use ndml_net::transport::{LoopbackNet, LoopbackSocket, Transport};

#[derive(Debug)]
struct Valve {
    inner: LoopbackSocket,
    drop_out: Arc<AtomicBool>,
    hold_in: Arc<AtomicBool>,
}

impl Transport for Valve {
    fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        if self.drop_out.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.inner.send_to(now, to, data)
    }
    fn recv_from(
        &mut self,
        now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        if self.hold_in.load(Ordering::Relaxed) {
            return Ok(None);
        }
        self.inner.recv_from(now, buf)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], port))
}
fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

#[test]
fn an_outage_straddling_the_joiners_attempt_leaves_the_joiner_in_its_slot() {
    let net = LoopbackNet::new();
    let (h_drop, h_hold) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let (c_drop, c_hold) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let mut hc = EndpointConfig::new(Role::Host, 1);
    hc.max_peers = 2;
    hc.rejoin = Rejoin::Always; // a co-op run holds the link
    let mut host = Endpoint::new(
        Valve {
            inner: net.bind(addr(1)).unwrap(),
            drop_out: h_drop.clone(),
            hold_in: h_hold.clone(),
        },
        hc,
    );
    let mut client = Endpoint::new(
        Valve {
            inner: net.bind(addr(2)).unwrap(),
            drop_out: c_drop.clone(),
            hold_in: c_hold.clone(),
        },
        EndpointConfig::new(Role::Client, 2),
    );
    let _ = client.connect(ms(0), addr(1), None);
    let mut t = 0u64;
    while t < 50 {
        host.poll(ms(t)).unwrap();
        client.poll(ms(t)).unwrap();
        t += 1;
    }
    assert_eq!(host.peers().len(), 1, "linked");
    assert!(client.is_connected(ndml_net::PeerId::HOST));
    while host.next_event().is_some() {}
    while client.next_event().is_some() {}
    host.set_refusing(Some(RefuseReason::RunInProgress)); // the run started

    // The outage: nothing passes either way.
    h_drop.store(true, Ordering::Relaxed);
    c_drop.store(true, Ordering::Relaxed);
    let mut attempt_started: Option<u64> = None; // the joiner's current reconnect attempt
    let mut attempts = 0;
    let mut restored_at: Option<u64> = None;
    let mut log: Vec<String> = Vec::new();
    let end = 140_000;
    while t < end {
        // Restore the network 150 ms before the joiner's first attempt ends.
        if restored_at.is_none()
            && attempts == 1
            && attempt_started.is_some_and(|s| t >= s + 29_850)
        {
            h_drop.store(false, Ordering::Relaxed);
            c_drop.store(false, Ordering::Relaxed);
            c_hold.store(true, Ordering::Relaxed);
            restored_at = Some(t);
            log.push(format!("{t}: network restored (host answers 250 ms away)"));
        }
        if restored_at.is_some_and(|r| t == r + 250) {
            c_hold.store(false, Ordering::Relaxed);
        }
        host.poll(ms(t)).unwrap();
        client.poll(ms(t)).unwrap();
        while let Some(ev) = host.next_event() {
            match ev {
                Event::Message { .. } => {}
                other => log.push(format!("{t}: host {other:?}")),
            }
        }
        while let Some(ev) = client.next_event() {
            match ev {
                Event::Message { .. } => {}
                Event::Disconnected { reason, .. } => {
                    log.push(format!("{t}: joiner Disconnected {reason:?}"));
                    // As `LobbyAgent` does on a timeout while reconnecting.
                    if matches!(
                        reason,
                        DisconnectReason::Timeout | DisconnectReason::HandshakeTimeout
                    ) {
                        let _ = client.connect(ms(t), addr(1), None);
                        attempt_started = Some(t);
                        attempts += 1;
                    }
                }
                other => log.push(format!("{t}: joiner {other:?}")),
            }
        }
        t += 1;
    }
    for l in &log {
        println!("{l}");
    }
    println!(
        "host peers at the end: {:?}; joiner connected: {}",
        host.peers(),
        client.is_connected(ndml_net::PeerId::HOST)
    );
    assert_eq!(
        host.peers().len(),
        1,
        "the joiner is back in its slot after the outage"
    );
    assert!(client.is_connected(ndml_net::PeerId::HOST));
    let drops = log.iter().filter(|l| l.contains("Disconnected")).count();
    assert_eq!(
        drops, 3,
        "the outage's own: the host's and the joiner's timeout, the joiner's attempt's end: {log:?}"
    );
}
