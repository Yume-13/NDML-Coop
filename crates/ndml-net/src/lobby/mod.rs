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

//! The co-op lobby: how two games get connected.
//!
//! The game owns the UDP socket, so a connection code exists only while it
//! runs. Players drive the lobby from the co-op mod's panel
//! ([`LobbyAgent::request`]). Two files in `<game>/ndml/coop/` let an outside
//! tool drive the lobby: `status.toml` ([`Status`]), written by the game with
//! a heartbeat, and `peer.toml` ([`PeerRequest`]), requests for the current
//! session only. Both are replaced atomically. [`LobbyAgent`] is driven by
//! `poll(now)` on the network thread.

#![forbid(unsafe_code)]

pub mod agent;
pub mod files;

pub use agent::{
    LOOKUP_BACKOFF_FIRST, LOOKUP_SLOW, LobbyAgent, LobbyConfig, LobbyEndpoint, LobbyRequest,
    LobbyTransport, SteamFileRequest, SteamRequest, StunResolve, VersionCheck, bind_udp,
    detect_lan_ip, refusal_message, system_resolve,
};
pub use files::{
    COOP_DIR, Connector, DEFAULT_PORT, ErrorKind, Freshness, ListenerState, LobbyState, NatKind,
    PEER_FILE, PeerRequest, PeerStatus, Plan, RoleChoice, STALE_AFTER, STATUS_FILE, Status,
    SteamFileAction, SteamLobbyState, SteamMember, SteamStatus, StunState, StunStatus, Target,
    TurnedAway, lobby_dir,
};

#[cfg(test)]
mod steam_tests;
#[cfg(test)]
mod tests;
