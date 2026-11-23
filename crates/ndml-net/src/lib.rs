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

//! Co-op netcode for Nova Drift mods.
//!
//! Pure safe Rust on `std::net::UdpSocket`, with no async runtime. Protocol
//! logic is deterministic: time is passed in as a [`Duration`] since a local
//! epoch ([`time`]) and the socket sits behind [`transport::Transport`], so
//! tests run on an in-memory loopback with a seeded [`conditioner`].
//!
//! Layers, bottom up:
//!
//! * [`transport`], [`conditioner`]: datagram IO and a simulated bad network.
//! * [`packet`]: the wire header and chunk framing.
//! * [`reliability`]: acks, resends, RTT/jitter/loss estimation.
//! * [`clock`]: remote clock tracking and the interpolation delay.
//! * [`interp`]: per-entity snapshot buffers and interpolation.
//! * [`msg`]: the co-op message set.
//! * [`damage`], [`status`]: shared damage and status effects.
//! * [`session`]: co-op rules as pure state machines.
//! * [`endpoint`]: connections, the hole-punch handshake and keepalives;
//!   [`worker`] runs one on its own thread.
//! * [`stun`], [`code`]: public endpoint discovery and connection codes.
//! * [`steam`]: Steam ids as addresses and the channel-bridged transport.
//! * [`lobby`]: the lobby agent and its `status.toml` / `peer.toml` files.
//! * [`fileio`]: file work on a thread of its own.
//!
//! [`Duration`]: std::time::Duration

#![forbid(unsafe_code)]

pub mod clock;
pub mod code;
pub mod conditioner;
pub mod damage;
pub mod endpoint;
pub mod fileio;
pub mod interp;
pub mod lobby;
pub mod mirror;
pub mod mirror_vars;
pub mod msg;
pub mod packet;
pub mod reliability;
pub mod rng;
pub mod session;
pub mod shots;
pub mod status;
pub mod steam;
pub mod stun;
pub mod time;
pub mod transport;
pub mod worker;

pub use endpoint::{Endpoint, EndpointConfig, Event, Role};
pub use msg::{Message, PeerId};
pub use transport::Transport;
