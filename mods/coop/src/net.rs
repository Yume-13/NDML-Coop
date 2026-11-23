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

//! The network side: starting the lobby on the `ndml-net` worker thread, the
//! game build fingerprint for the version check, and [`Out`], the messages the
//! pure logic asks the glue to send.
//!
//! The worker owns the sockets; the game thread talks to it only through
//! non-blocking channels. Only the one-time bind at init blocks.

#![forbid(unsafe_code)]

use std::io;
use std::path::Path;
use std::time::Instant;

use ndml_net::endpoint::{RefuseReason, Rejoin};
use ndml_net::lobby::{self, LobbyAgent, LobbyConfig, VersionCheck};
use ndml_net::msg::{Message, PeerId};
use ndml_net::steam::{NoUdp, SteamMux, SteamTransport};
use ndml_net::transport::Transport;
use ndml_net::worker::NetWorker;

use crate::config::Settings;

/// Co-op mod protocol version in `Hello`; both players must match exactly.
/// Bump it when co-op behaviour changes in a way an older partner cannot follow.
pub const COOP_VERSION: u32 = 2;

/// Something the pure logic wants sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// To one peer.
    Send(PeerId, Message),
    /// To every connected peer.
    Broadcast(Message),
    /// Refuse newcomers with this reason (`None` admits them again).
    Refusing(Option<RefuseReason>),
    /// Turn this connected peer away with a reason it is told.
    Refuse(PeerId, RefuseReason),
    /// Hold the link without a limit (a co-op run) or release it.
    HoldLink(bool),
}

/// Hand `outs` to the network thread (never blocks).
pub fn apply(net: &NetWorker, outs: Vec<Out>) {
    for o in outs {
        match o {
            Out::Send(p, m) => net.send(p, m),
            Out::Broadcast(m) => net.broadcast(m, None),
            Out::Refusing(r) => net.set_refusing(r),
            Out::Refuse(p, r) => net.refuse(p, r),
            Out::HoldLink(on) => net.hold_link(on),
        }
    }
}

/// A build fingerprint from the start of a PE file and its length: the COFF
/// link timestamp (unique per build) plus the size, e.g. `pe-6a1f03c2-31457280`.
/// `None` if `header` is not a PE header.
#[must_use]
pub fn pe_build(header: &[u8], len: u64) -> Option<String> {
    if header.get(..2)? != b"MZ" {
        return None;
    }
    let lfanew = u32::from_le_bytes(header.get(0x3C..0x40)?.try_into().ok()?) as usize;
    if header.get(lfanew..lfanew + 4)? != b"PE\0\0" {
        return None;
    }
    // IMAGE_FILE_HEADER: Machine u16, NumberOfSections u16, TimeDateStamp u32.
    let stamp_at = lfanew + 8;
    let stamp = u32::from_le_bytes(header.get(stamp_at..stamp_at + 4)?.try_into().ok()?);
    Some(format!("pe-{stamp:08x}-{len}"))
}

/// The running game's build fingerprint ([`pe_build`] of the executable), or
/// `"unknown"` (logged) if it can't be read.
#[must_use]
pub fn game_build() -> String {
    let read = || -> io::Result<Option<String>> {
        let exe = std::env::current_exe()?;
        let len = std::fs::metadata(&exe)?.len();
        let mut f = std::fs::File::open(&exe)?;
        let mut head = vec![0u8; 4096];
        let n = io::Read::read(&mut f, &mut head)?;
        head.truncate(n);
        Ok(pe_build(&head, len))
    };
    match read() {
        Ok(Some(b)) => b,
        Ok(None) => {
            log::warn!("co-op: unable to read the game's build (no PE header)");
            "unknown".into()
        }
        Err(e) => {
            log::warn!("co-op: unable to read the game's build ({e})");
            "unknown".into()
        }
    }
}

/// A random, nonzero nonce for this launch (it goes into the codes).
#[must_use]
pub fn random_nonce() -> u32 {
    (ndml_net::rng::random_u64() as u32).max(1)
}

/// Bind the socket and start the lobby on its own thread. `ndml_root` is the
/// `ndml/` folder (the parent of the config folder).
pub fn start(settings: &Settings, ndml_root: &Path, epoch: Instant) -> io::Result<NetWorker> {
    start_with_steam(settings, ndml_root, epoch, None).map(|s| s.worker)
}

/// What [`start_with_steam`] started.
#[derive(Debug)]
pub struct Started {
    /// The network thread.
    pub worker: NetWorker,
    /// The game build fingerprint sent in `Hello` (and in Steam lobby data).
    pub build: String,
}

/// The lobby agent's settings from the mod's (version check apart).
fn lobby_config(settings: &Settings, dir: std::path::PathBuf, nonce: u32) -> LobbyConfig {
    let mut cfg = LobbyConfig::new(dir, nonce);
    cfg.name.clone_from(&settings.name);
    cfg.name_alt.clone_from(&settings.name_alt);
    // `internet_lookup`: the shipped STUN servers, or none at all.
    cfg.stun_servers = if settings.internet_lookup {
        ndml_net::stun::DEFAULT_SERVERS
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        Vec::new()
    };
    // No STUN server is asked until the connection-code route is used.
    cfg.discover_at_start = false;
    cfg.files = false;
    // Outside a co-op run a lost partner may come back for a while; a co-op
    // run holds the link without a limit (`Out::HoldLink`).
    cfg.steam_rejoin = settings.steam.rejoin;
    cfg.udp_rejoin = UDP_LOBBY_REJOIN;
    cfg
}

/// Outside a co-op run, a UDP partner whose link timed out may come back for
/// this long.
pub const UDP_LOBBY_REJOIN: Rejoin = Rejoin::Within(std::time::Duration::from_secs(20));

/// [`start`], with the Steam connector's transport next to the UDP socket
/// when `steam` is given (one lobby agent then serves both connectors). If
/// the UDP port can't be bound, Steam still works (codes are then off).
pub fn start_with_steam(
    settings: &Settings,
    ndml_root: &Path,
    epoch: Instant,
    steam: Option<SteamTransport>,
) -> io::Result<Started> {
    let mut cfg = lobby_config(settings, lobby::lobby_dir(ndml_root), random_nonce());
    let build = game_build();
    cfg.version = Some(VersionCheck {
        mod_version: COOP_VERSION,
        build: build.clone(),
        send_hello: true,
    });
    log::info!(
        "co-op lobby: session {}, build {}",
        lobby::files::session_id(cfg.nonce),
        build
    );
    let boxed = |t: Box<dyn Transport + Send>| -> Box<dyn Transport + Send> { t };
    let udp = lobby::bind_udp(settings.port);
    let worker = match (udp, steam) {
        (Ok(udp), None) => {
            NetWorker::spawn_lobby(LobbyAgent::new(boxed(Box::new(udp)), cfg), epoch)
        }
        (Ok(udp), Some(st)) => NetWorker::spawn_lobby(
            LobbyAgent::new(
                SteamMux::new(boxed(Box::new(udp)), boxed(Box::new(st))),
                cfg,
            ),
            epoch,
        ),
        (Err(e), Some(st)) => {
            log::warn!("co-op: unable to open the UDP socket ({e}); codes off");
            NetWorker::spawn_lobby(
                LobbyAgent::new(SteamMux::new(NoUdp, boxed(Box::new(st))), cfg),
                epoch,
            )
        }
        (Err(e), None) => Err(e),
    }?;
    Ok(Started { worker, build })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped settings keep the STUN servers for the code route but ask
    /// none of them at start; `internet_lookup = false` leaves none at all.
    #[test]
    fn no_address_lookup_at_start() {
        let c: crate::config::Config =
            ndml_sdk::config::parse_defaults(crate::NDML_DEFAULT_CONFIG).unwrap();
        let (s, _) = c.validate();
        let cfg = lobby_config(&s, std::path::PathBuf::from("x"), 1);
        assert_eq!(
            cfg.stun_servers,
            ["stun.l.google.com:19302", "stun.cloudflare.com:3478"]
        );
        assert!(!cfg.discover_at_start);
        let off = crate::config::Config {
            internet_lookup: false,
            ..c.clone()
        };
        let cfg_off = lobby_config(&off.validate().0, std::path::PathBuf::from("x"), 1);
        assert!(cfg_off.stun_servers.is_empty(), "lookup off: no server");
        // Outside a run: 20 s for UDP, 120 s for Steam.
        assert_eq!(
            cfg.udp_rejoin,
            Rejoin::Within(std::time::Duration::from_secs(20))
        );
        assert_eq!(
            cfg.steam_rejoin,
            Rejoin::Within(std::time::Duration::from_secs(120))
        );
    }

    fn fake_pe(stamp: u32) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..2].copy_from_slice(b"MZ");
        h[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        h[0x80..0x84].copy_from_slice(b"PE\0\0");
        h[0x88..0x8C].copy_from_slice(&stamp.to_le_bytes());
        h
    }

    #[test]
    fn pe_build_reads_the_link_stamp() {
        assert_eq!(
            pe_build(&fake_pe(0x6A1F_03C2), 1234).as_deref(),
            Some("pe-6a1f03c2-1234")
        );
        assert_ne!(pe_build(&fake_pe(1), 5), pe_build(&fake_pe(2), 5));
        assert_eq!(pe_build(b"not a pe", 8), None);
        let mut bad = fake_pe(1);
        bad[0x3C..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(pe_build(&bad, 8), None, "e_lfanew out of range");
        assert_eq!(pe_build(&fake_pe(1)[..0x86], 8), None, "cut short");
    }

    #[test]
    fn this_test_binary_has_a_build() {
        // The test runner is a PE file too.
        assert!(game_build().starts_with("pe-"));
    }

    #[test]
    fn nonces_are_nonzero() {
        assert!((0..100).all(|_| random_nonce() != 0));
    }
}
