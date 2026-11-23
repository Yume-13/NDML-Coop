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

//! The "Codes and LAN" form: partner code, direct address, name and role,
//! validated live and turned into `LobbyRequest`s, plus the lines that
//! describe the lobby and the link. Pure; `page.rs` puts it on screen.

#![forbid(unsafe_code)]

use std::net::SocketAddr;

use ndml_net::code::{CodeError, ConnectionCode};
use ndml_net::lobby::files::{self, MAX_NAME_CHARS};
use ndml_net::lobby::{
    ErrorKind, LobbyRequest, LobbyState, NatKind, RoleChoice, Status, StunState,
};

use super::page::{ColLine, Tone};

/// Longest code input kept (an IPv6 code with dashes is 47 characters).
pub const CODE_MAX: usize = 48;
/// Longest direct address input kept.
pub const DIRECT_MAX: usize = 64;
/// Characters in an IPv4 code.
pub const CODE_CHARS: usize = 20;
/// Round trip above which the link counts as bad (shown in `NEGATIVE`).
pub const BAD_RTT_MS: f64 = 150.0;
/// Loss above which the link counts as bad.
pub const BAD_LOSS_PCT: f64 = 5.0;

/// A text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The partner's connection code.
    Code,
    /// The host's `ip:port` (join over LAN or a forwarded port).
    Direct,
    /// This player's name.
    Name,
    /// A Steam room code (main page; handed to the Steam connector).
    RoomCode,
}

impl Field {
    /// Most characters kept.
    #[must_use]
    pub const fn max(self) -> usize {
        match self {
            Self::Code => CODE_MAX,
            Self::Direct => DIRECT_MAX,
            Self::Name => MAX_NAME_CHARS,
            Self::RoomCode => 16,
        }
    }
}

/// Live validation of the code field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeCheck {
    Empty,
    /// Fewer characters than a code has (not an error yet).
    Typing(usize),
    Valid(ConnectionCode),
    /// This player's own code, pasted by mistake.
    Own,
    /// What is wrong, in the parser's words.
    Invalid(String),
}

/// Live validation of the direct-address field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddrCheck {
    Empty,
    Valid(SocketAddr),
    Invalid(String),
}

/// Append printable characters up to `max`.
pub fn push_limited(buf: &mut String, text: &str, max: usize) {
    for c in text.chars().filter(|c| !c.is_control()) {
        if buf.chars().count() >= max {
            break;
        }
        buf.push(c);
    }
}

/// The link counts as bad.
#[must_use]
pub fn bad_link(rtt_ms: f64, loss_pct: f64) -> bool {
    rtt_ms > BAD_RTT_MS || loss_pct > BAD_LOSS_PCT
}

fn own_nonce(status: Option<&Status>) -> Option<u32> {
    u32::from_str_radix(&status?.session, 16).ok()
}

/// Validate a partner code (`status` tells the own code apart).
#[must_use]
pub fn check_code(text: &str, status: Option<&Status>) -> CodeCheck {
    match ConnectionCode::parse(text) {
        Ok(c) if Some(c.nonce) == own_nonce(status) => CodeCheck::Own,
        Ok(c) => CodeCheck::Valid(c),
        Err(CodeError::Empty) => CodeCheck::Empty,
        Err(CodeError::Length(n)) if n < CODE_CHARS => CodeCheck::Typing(n),
        Err(e) => CodeCheck::Invalid(e.to_string()),
    }
}

/// Validate a direct address.
#[must_use]
pub fn check_direct(text: &str) -> AddrCheck {
    if text.trim().is_empty() {
        return AddrCheck::Empty;
    }
    match files::parse_direct(text) {
        Ok(a) => AddrCheck::Valid(a),
        Err(e) => AddrCheck::Invalid(e),
    }
}

/// What the form answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanEffect {
    Request(LobbyRequest),
    Copy(String),
}

/// The Codes and LAN form.
#[derive(Debug, Clone)]
pub struct LanForm {
    /// Host or join (Host until the player or the lobby says otherwise).
    pub role: RoleChoice,
    pub code: String,
    pub direct: String,
    /// The name sent with a request (the automatic pick while `name_auto`).
    pub name: String,
    /// The name is this launch's random pick; the field shows "Random".
    pub name_auto: bool,
    message: Option<(String, Tone, u64)>,
    copied_at: Option<u64>,
    code_changed: bool,
    role_touched: bool,
    seen_code: Option<String>,
    tick: u64,
}

impl LanForm {
    /// An empty form with the configured name.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            role: RoleChoice::Host,
            code: String::new(),
            direct: String::new(),
            name: name.to_owned(),
            name_auto: false,
            message: None,
            copied_at: None,
            code_changed: false,
            role_touched: false,
            seen_code: None,
            tick: 0,
        }
    }

    /// Show a message on the page.
    pub fn say(&mut self, text: impl Into<String>, tone: Tone) {
        self.message = Some((text.into(), tone, self.tick));
    }

    /// The current message and its age in frames, while it shows.
    #[must_use]
    pub fn message(&self) -> Option<(&str, Tone, u64)> {
        let (t, tone, at) = self.message.as_ref()?;
        let age = self.tick.saturating_sub(*at);
        (age < super::page::MESSAGE_FRAMES).then_some((t.as_str(), *tone, age))
    }

    /// Once per menu frame, with the latest status.
    pub fn refresh(&mut self, status: Option<&Status>) {
        self.tick += 1;
        let Some(s) = status else { return };
        if let Some(code) = &s.code {
            if self.seen_code.as_ref().is_some_and(|old| old != code) {
                self.code_changed = true;
            }
            self.seen_code = Some(code.clone());
        }
        if s.state == LobbyState::Connected {
            self.code_changed = false;
        }
    }

    /// The role to show: one set by the lobby's status until the player picks one.
    #[must_use]
    pub fn shown_role(&self, status: Option<&Status>) -> RoleChoice {
        match status.map(|s| s.role) {
            Some(r @ (RoleChoice::Host | RoleChoice::Join)) if !self.role_touched => r,
            _ => self.role,
        }
    }

    /// The player picked a role on the page.
    pub fn set_role(&mut self, role: RoleChoice) {
        self.role = role;
        self.role_touched = true;
    }

    /// Validate the code field.
    #[must_use]
    pub fn code_check(&self, status: Option<&Status>) -> CodeCheck {
        check_code(&self.code, status)
    }

    /// Validate the direct-address field.
    #[must_use]
    pub fn direct_check(&self) -> AddrCheck {
        check_direct(&self.direct)
    }

    fn request(&self, status: Option<&Status>) -> LobbyRequest {
        let mut r = LobbyRequest {
            role: self.role,
            name: self.name.trim().to_owned(),
            ..LobbyRequest::default()
        };
        if let CodeCheck::Valid(c) = self.code_check(status) {
            r.partner_code = Some(c.encode());
        }
        if self.role == RoleChoice::Join
            && let AddrCheck::Valid(a) = self.direct_check()
        {
            r.direct = Some(a.to_string());
        }
        r
    }

    fn send(&mut self, status: Option<&Status>, said: &str) -> Vec<LanEffect> {
        self.code_changed = false;
        self.say(said, Tone::Good);
        vec![LanEffect::Request(self.request(status))]
    }

    /// Validate the inputs for the chosen role and connect.
    pub fn connect(&mut self, status: Option<&Status>) -> Vec<LanEffect> {
        self.role = self.shown_role(status);
        let code = self.code_check(status);
        match self.role {
            RoleChoice::None => {
                self.say("Choose Host or Join first.", Tone::Warn);
                Vec::new()
            }
            RoleChoice::Host => match code {
                CodeCheck::Invalid(e) => {
                    self.say(format!("Partner code: {e}"), Tone::Bad);
                    Vec::new()
                }
                CodeCheck::Own => {
                    self.say(
                        "That is your own code. Paste your partner's code instead.",
                        Tone::Bad,
                    );
                    Vec::new()
                }
                CodeCheck::Typing(n) => {
                    self.say(
                        format!(
                            "The partner code is incomplete ({n}/{CODE_CHARS}). Finish it or clear it."
                        ),
                        Tone::Warn,
                    );
                    Vec::new()
                }
                CodeCheck::Valid(_) => self.send(
                    status,
                    "Hosting and reaching out to your partner. They need your code too.",
                ),
                CodeCheck::Empty => self.send(
                    status,
                    "Hosting. Send your code to your partner; on a LAN they enter your address.",
                ),
            },
            RoleChoice::Join => {
                match self.direct_check() {
                    AddrCheck::Valid(_) => {
                        return self.send(status, "Joining the host's address...");
                    }
                    AddrCheck::Invalid(e) => {
                        self.say(format!("Address: {e}"), Tone::Bad);
                        return Vec::new();
                    }
                    AddrCheck::Empty => {}
                }
                match code {
                    CodeCheck::Valid(_) => {
                        self.send(status, "Joining. The host needs your code too.")
                    }
                    CodeCheck::Empty => {
                        self.say(
                            "Paste the host's code first, or enter its address for a LAN game.",
                            Tone::Warn,
                        );
                        Vec::new()
                    }
                    CodeCheck::Typing(n) => {
                        self.say(
                            format!("The code is incomplete ({n}/{CODE_CHARS})."),
                            Tone::Warn,
                        );
                        Vec::new()
                    }
                    CodeCheck::Own => {
                        self.say("That is your own code. Paste the host's code.", Tone::Bad);
                        Vec::new()
                    }
                    CodeCheck::Invalid(e) => {
                        self.say(format!("Code: {e}"), Tone::Bad);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Drop every link and stay idle.
    pub fn disconnect(&mut self) -> Vec<LanEffect> {
        self.say("Disconnected.", Tone::Info);
        vec![LanEffect::Request(LobbyRequest {
            disconnect: true,
            name: self.name.trim().to_owned(),
            ..LobbyRequest::default()
        })]
    }

    /// Copy this player's code (the internet one, else the LAN one).
    pub fn copy_code(&mut self, status: Option<&Status>) -> Vec<LanEffect> {
        if let Some(c) = status.and_then(|s| s.code.clone().or_else(|| s.lan_code.clone())) {
            self.copied_at = Some(self.tick);
            self.say("Code copied. Send it to your partner.", Tone::Good);
            vec![LanEffect::Copy(c)]
        } else {
            self.say("No code yet: the lobby is still starting.", Tone::Warn);
            Vec::new()
        }
    }

    /// The name as the page shows it ("Random" while automatic).
    #[must_use]
    pub fn shown_name(&self) -> &str {
        if self.name_auto {
            crate::config::RANDOM
        } else {
            &self.name
        }
    }

    /// The text of a field.
    #[must_use]
    pub fn field(&self, f: Field) -> &str {
        match f {
            Field::Code => &self.code,
            Field::Direct => &self.direct,
            Field::Name => self.shown_name(),
            Field::RoomCode => &self.name,
        }
    }

    /// A finished text entry.
    pub fn set_field(&mut self, f: Field, text: &str) {
        let mut v = String::new();
        push_limited(&mut v, text.trim(), f.max());
        match f {
            Field::Code => self.code = v,
            Field::Direct => self.direct = v,
            Field::Name => {
                if v.eq_ignore_ascii_case(crate::config::RANDOM) {
                    crate::callsigns::launch_picks()
                        .0
                        .clone_into(&mut self.name);
                    self.name_auto = true;
                    self.say("Name kept. It goes with your next connect.", Tone::Info);
                } else if let Some(n) = files::clean_name(&v) {
                    self.name = n;
                    self.name_auto = false;
                    self.say("Name kept. It goes with your next connect.", Tone::Info);
                } else {
                    self.say("A name needs at least one character.", Tone::Warn);
                }
            }
            Field::RoomCode => {}
        }
        match f {
            Field::Direct if !self.direct.is_empty() => {
                self.set_role(RoleChoice::Join);
                match self.direct_check() {
                    AddrCheck::Valid(a) => {
                        self.say(format!("Host at {a}. Choose Connect."), Tone::Good);
                    }
                    AddrCheck::Invalid(e) => self.say(format!("Address: {e}"), Tone::Bad),
                    AddrCheck::Empty => {}
                }
            }
            Field::Code => self.say_code(None, "Choose Connect"),
            _ => {}
        }
    }

    fn say_code(&mut self, status: Option<&Status>, next: &str) {
        match self.code_check(status) {
            CodeCheck::Valid(_) => self.say(format!("Partner code ready. {next}."), Tone::Good),
            CodeCheck::Own => self.say(
                "That is your own code: your partner needs it, you need theirs.",
                Tone::Warn,
            ),
            CodeCheck::Typing(n) => self.say(
                format!("{n} of {CODE_CHARS} characters: was the code cut off?"),
                Tone::Warn,
            ),
            CodeCheck::Invalid(e) => self.say(format!("Code: {e}"), Tone::Bad),
            CodeCheck::Empty => {}
        }
    }

    /// Clipboard text for "Paste Partner Code": an address goes to the
    /// address field (and picks Join), anything else to the code field.
    pub fn paste(&mut self, text: Option<&str>, status: Option<&Status>) {
        let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
            self.say("The clipboard has no text.", Tone::Warn);
            return;
        };
        if files::parse_direct(text).is_ok() && ConnectionCode::parse(text).is_err() {
            self.direct.clear();
            push_limited(&mut self.direct, text, DIRECT_MAX);
            self.set_role(RoleChoice::Join);
            self.say(
                format!("Address {text} pasted. Choose Connect."),
                Tone::Good,
            );
            return;
        }
        self.code.clear();
        push_limited(&mut self.code, text, CODE_MAX);
        self.say_code(status, "Choose Connect");
    }

    /// Lines for the right column: own code, partner checks, the link.
    #[must_use]
    pub fn column(&self, status: Option<&Status>) -> Vec<ColLine> {
        let mut out = Vec::new();
        out.push(ColLine::heading("YOUR CODE"));
        let Some(s) = status else {
            out.push(ColLine::body("Starting the co-op lobby...", Tone::Info));
            return out;
        };
        match (&s.code, &s.lan_code) {
            (Some(c), _) => out.push(ColLine::code(c)),
            (None, Some(c)) => {
                out.push(ColLine::code(c));
                out.push(ColLine::small(
                    "LAN code: same machine or network.",
                    Tone::Info,
                ));
            }
            (None, None) => out.push(ColLine::body("No code yet.", Tone::Info)),
        }
        if self
            .copied_at
            .is_some_and(|t| self.tick.saturating_sub(t) < 90)
        {
            out.push(ColLine::small("Copied.", Tone::Good));
        }
        let mut ends = Vec::new();
        if let Some(p) = &s.public_addr {
            ends.push(format!("Public {p}"));
        }
        if let Some(l) = &s.lan_addr {
            ends.push(format!("LAN {l}"));
        }
        if !ends.is_empty() {
            out.push(ColLine::small(&ends.join("   "), Tone::Info));
        }
        let (d, tone) = discovery_line(s);
        out.push(ColLine::small(&d, tone));
        if self.code_changed {
            out.push(ColLine::small(
                "Your router gave the game a new public port, so your code changed: send it again.",
                Tone::Warn,
            ));
        }
        out.push(ColLine::gap());
        out.push(ColLine::heading("PARTNER"));
        let uses_direct = self.role == RoleChoice::Join && self.direct_check() != AddrCheck::Empty;
        let code_line = if uses_direct {
            ("Code not used: the address wins.".to_owned(), Tone::Info)
        } else {
            match self.code_check(Some(s)) {
                CodeCheck::Empty if self.role == RoleChoice::Join => {
                    ("No code yet: paste the host's code.".to_owned(), Tone::Info)
                }
                CodeCheck::Empty => (
                    "No code: a host only needs one for internet play.".to_owned(),
                    Tone::Info,
                ),
                CodeCheck::Typing(n) => (format!("Code: {n}/{CODE_CHARS} characters"), Tone::Info),
                CodeCheck::Valid(c) => (format!("Code valid: partner at {}", c.addr), Tone::Good),
                CodeCheck::Own => (
                    "That's your own code: your partner needs it, you need theirs.".to_owned(),
                    Tone::Warn,
                ),
                CodeCheck::Invalid(e) => (format!("Code: {e}"), Tone::Bad),
            }
        };
        out.push(ColLine::body(&code_line.0, code_line.1));
        match (self.direct_check(), self.role) {
            (AddrCheck::Empty, _) => {}
            (_, RoleChoice::Host) => {
                out.push(ColLine::small(
                    "The address is only used when joining.",
                    Tone::Info,
                ));
            }
            (AddrCheck::Valid(a), _) => {
                out.push(ColLine::body(&format!("Host at {a}"), Tone::Good));
            }
            (AddrCheck::Invalid(e), _) => {
                out.push(ColLine::body(&format!("Address: {e}"), Tone::Bad));
            }
        }
        out.push(ColLine::gap());
        out.extend(link_lines(s));
        out
    }
}

/// The link: state, quality, the lobby's failure messages.
#[must_use]
pub fn link_lines(s: &Status) -> Vec<ColLine> {
    let mut out = vec![ColLine::heading("LINK")];
    let (state, tone) = link_state(s);
    out.push(ColLine::body(&state, tone));
    for p in s.connected_peers() {
        let tone = if bad_link(p.rtt_ms, p.loss_pct) {
            Tone::Bad
        } else {
            Tone::Info
        };
        out.push(ColLine::body(
            &format!(
                "Ping: {:.0} ms   Jitter: {:.0} ms   Loss: {:.1} %",
                p.rtt_ms, p.jitter_ms, p.loss_pct
            ),
            tone,
        ));
    }
    if let Some(e) = &s.error {
        out.push(ColLine::body(e, Tone::Bad));
        if s.error_kind == Some(ErrorKind::HandshakeTimeout) {
            out.push(ColLine::small(
                "If your partner restarted their game, they have a new code: paste it again.",
                Tone::Info,
            ));
        }
    }
    if let Some(e) = &s.peer_file_error {
        out.push(ColLine::small(
            &format!("The game can't read peer.toml: {e}"),
            Tone::Bad,
        ));
    }
    if let Some(t) = &s.turned_away {
        out.push(ColLine::small(
            &format!("Turned away {} ({}).", t.addr, t.reason.replace('_', " ")),
            Tone::Info,
        ));
    }
    out
}

/// The discovery line (STUN state, symmetric NAT advice).
#[must_use]
pub fn discovery_line(s: &Status) -> (String, Tone) {
    match s.stun.state {
        StunState::Pending => ("Finding your internet address...".into(), Tone::Info),
        StunState::Failed => (
            format!(
                "No internet address ({}). Internet play needs one; LAN works.{}",
                s.stun.error.as_deref().unwrap_or("no answer"),
                s.stun
                    .retry_in_ms
                    .map_or_default(|ms| format!(" Retrying in {} s.", ms.div_ceil(1000)))
            ),
            Tone::Warn,
        ),
        StunState::Disabled => (
            "No internet code: internet_lookup is off in the co-op settings, so the game asks no server for your address. LAN codes and Steam play still work."
                .into(),
            Tone::Info,
        ),
        StunState::Ok if s.stun.nat == NatKind::Varies => (
            format!(
                "Your router changes ports per destination. If the link won't come up, forward UDP {} here and host.",
                s.local_addr
                    .as_deref()
                    .and_then(|a| a.rsplit(':').next())
                    .unwrap_or("7777")
            ),
            Tone::Warn,
        ),
        StunState::Ok => (
            "Good until this game closes. A mistyped character is caught.".into(),
            Tone::Info,
        ),
    }
}

/// The link state in words.
#[must_use]
pub fn link_state(s: &Status) -> (String, Tone) {
    match s.state {
        LobbyState::Connected => {
            let peers: Vec<String> = s
                .connected_peers()
                .map(|p| {
                    if p.name.is_empty() {
                        format!("P{} at {}", p.id + 1, p.addr)
                    } else {
                        format!("{} at {}", p.name, p.addr)
                    }
                })
                .collect();
            (
                format!(
                    "Linked with {}. Start a run: you wait for each other.",
                    peers.join(", ")
                ),
                Tone::Good,
            )
        }
        LobbyState::Connecting if s.role == RoleChoice::Host => (
            "Reaching out to your partner (LAN players are let in too)...".into(),
            Tone::Info,
        ),
        LobbyState::Connecting => ("Reaching out to the host...".into(), Tone::Info),
        LobbyState::Waiting => (
            format!(
                "Hosting: waiting for your partner (their code, or they enter {}).",
                s.lan_addr.as_deref().unwrap_or("your address")
            ),
            Tone::Info,
        ),
        LobbyState::Ready => ("Not connected.".into(), Tone::Info),
        LobbyState::Discovering => ("Finding your internet address...".into(), Tone::Info),
        LobbyState::Starting => ("Starting...".into(), Tone::Info),
        LobbyState::Failed => (
            "The last attempt failed. Connect tries again.".into(),
            Tone::Bad,
        ),
        LobbyState::Stopped => ("The lobby is stopped.".into(), Tone::Info),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ndml_net::lobby::{PeerStatus, StunStatus, TurnedAway};

    pub fn partner() -> ConnectionCode {
        ConnectionCode {
            addr: "203.0.113.9:40000".parse().unwrap(),
            nonce: 0xBEEF,
        }
    }

    pub fn status(state: LobbyState) -> Status {
        let own = ConnectionCode {
            addr: "203.0.113.5:40000".parse().unwrap(),
            nonce: 0x4a6b_decf,
        };
        Status {
            format: files::FORMAT,
            session: "4a6bdecf".into(),
            state,
            code: Some(own.encode()),
            lan_code: Some("LAN".into()),
            lan_addr: Some("192.168.1.5:7777".into()),
            local_addr: Some("0.0.0.0:7777".into()),
            stun: StunStatus {
                state: StunState::Ok,
                ..StunStatus::default()
            },
            ..Status::default()
        }
    }

    pub fn linked(rtt: f64) -> Status {
        let mut s = status(LobbyState::Connected);
        s.peers.push(PeerStatus {
            id: 1,
            name: "Wing".into(),
            addr: "198.51.100.4:5".into(),
            connected: true,
            rtt_ms: rtt,
            jitter_ms: 2.0,
            loss_pct: 0.0,
        });
        s
    }

    fn requests(e: &[LanEffect]) -> Vec<&LobbyRequest> {
        e.iter()
            .filter_map(|e| match e {
                LanEffect::Request(r) => Some(r),
                LanEffect::Copy(_) => None,
            })
            .collect()
    }

    fn text(lines: &[ColLine]) -> String {
        lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn host_without_code_and_join_by_code() {
        let s = status(LobbyState::Ready);
        let mut f = LanForm::new("Ace");
        let r = f.connect(Some(&s));
        let r = requests(&r);
        assert_eq!(r[0].role, RoleChoice::Host);
        assert_eq!(r[0].partner_code, None);
        assert_eq!(r[0].name, "Ace");
        let mut f = LanForm::new("Ace");
        f.role = RoleChoice::Join;
        assert!(f.connect(Some(&s)).is_empty(), "join needs a code");
        assert!(f.message().unwrap().0.contains("Paste the host's code"));
        f.set_field(Field::Code, &partner().encode());
        let e = f.connect(Some(&s));
        let r = requests(&e);
        assert_eq!(r[0].role, RoleChoice::Join);
        assert_eq!(
            r[0].partner_code.as_deref(),
            Some(partner().encode().as_str())
        );
    }

    #[test]
    fn join_by_direct_address_wins_over_the_code() {
        let s = status(LobbyState::Ready);
        let mut f = LanForm::new("Ace");
        f.set_field(Field::Direct, "192.168.1.20");
        assert_eq!(f.role, RoleChoice::Join, "an address picks Join");
        let r = f.connect(Some(&s));
        assert_eq!(requests(&r)[0].direct.as_deref(), Some("192.168.1.20:7777"));
        f.direct = "nope".into();
        assert!(f.connect(Some(&s)).is_empty());
        assert!(f.message().unwrap().0.starts_with("Address:"));
    }

    #[test]
    fn bad_codes_are_explained_and_not_sent() {
        let s = status(LobbyState::Ready);
        let mut f = LanForm::new("Ace");
        f.role = RoleChoice::Join;
        f.code = "2ZR00-00HWR".into();
        assert!(f.connect(Some(&s)).is_empty());
        assert!(f.message().unwrap().0.contains("incomplete"));
        f.code = s.code.clone().unwrap();
        assert!(f.connect(Some(&s)).is_empty());
        assert!(f.message().unwrap().0.contains("your own code"));
        f.code = "UUUUU-UUUUU-UUUUU-UUUUU".into();
        assert!(f.connect(Some(&s)).is_empty());
        assert_eq!(f.message().unwrap().1, Tone::Bad);
    }

    #[test]
    fn copy_and_paste() {
        let s = status(LobbyState::Ready);
        let mut f = LanForm::new("Ace");
        assert_eq!(
            f.copy_code(Some(&s)),
            vec![LanEffect::Copy(s.code.clone().unwrap())]
        );
        assert!(text(&f.column(Some(&s))).contains("Copied."));
        f.paste(Some(&format!("  {}\n", partner().encode())), Some(&s));
        assert_eq!(f.code, partner().encode());
        assert!(f.message().unwrap().0.contains("Partner code ready"));
        f.paste(Some("192.168.1.20:7777"), Some(&s));
        assert_eq!(
            f.direct, "192.168.1.20:7777",
            "an address goes to the address field"
        );
        f.paste(Some("2ZR00-0"), Some(&s));
        assert!(f.message().unwrap().0.contains("cut off"));
        f.paste(None, Some(&s));
        assert!(f.message().unwrap().0.contains("no text"));
        assert!(LanForm::new("x").copy_code(None).is_empty());
    }

    #[test]
    fn names_are_cleaned() {
        let mut f = LanForm::new("Ace");
        f.set_field(Field::Name, "   ");
        assert_eq!(f.name, "Ace");
        f.set_field(Field::Name, " Wing ");
        assert_eq!(f.name, "Wing");
        f.set_field(Field::Name, &"x".repeat(40));
        assert_eq!(f.name.chars().count(), MAX_NAME_CHARS);
    }

    /// An automatic name shows "Random" until a name is typed.
    #[test]
    fn an_automatic_name_shows_random_until_one_is_typed() {
        let (pick, _) = crate::callsigns::launch_picks();
        let mut f = LanForm::new(pick);
        f.name_auto = true;
        assert_eq!(f.shown_name(), "Random");
        assert_eq!(f.field(Field::Name), "Random");
        assert_eq!(f.request(None).name, pick);
        f.set_field(Field::Name, "Night Owl");
        assert!(!f.name_auto);
        assert_eq!(f.shown_name(), "Night Owl");
        assert_eq!(f.request(None).name, "Night Owl");
        f.set_field(Field::Name, " random ");
        assert!(f.name_auto);
        assert_eq!((f.shown_name(), f.name.as_str()), ("Random", pick));
    }

    #[test]
    fn link_and_failure_lines() {
        let f = LanForm::new("Ace");
        let t = text(&f.column(Some(&linked(42.0))));
        assert!(t.contains("Linked with Wing at 198.51.100.4:5"), "{t}");
        assert!(
            t.contains("Ping: 42 ms   Jitter: 2 ms   Loss: 0.0 %"),
            "{t}"
        );
        let bad = f.column(Some(&linked(300.0)));
        assert!(
            bad.iter()
                .any(|l| l.text.starts_with("Ping: 300 ms") && l.tone == Tone::Bad)
        );
        let mut s = status(LobbyState::Failed);
        s.error_kind = Some(ErrorKind::HandshakeTimeout);
        s.error = Some("No answer from your partner within 30 s.".into());
        let t = text(&f.column(Some(&s)));
        assert!(t.contains("No answer from your partner") && t.contains("restarted their game"));
        s.stun.state = StunState::Failed;
        s.stun.error = Some("timed out".into());
        s.stun.retry_in_ms = Some(12_000);
        let t = text(&f.column(Some(&s)));
        assert!(t.contains("No internet address (timed out)") && t.contains("Retrying in 12 s"));
        s.stun.state = StunState::Disabled;
        s.stun.retry_in_ms = None;
        let t = text(&f.column(Some(&s)));
        assert!(
            t.contains("No internet code: internet_lookup is off")
                && t.contains("LAN codes and Steam play still work")
                && !t.contains("Retrying")
        );
        s.stun.state = StunState::Ok;
        s.stun.nat = NatKind::Varies;
        assert!(text(&f.column(Some(&s))).contains("forward UDP 7777"));
        s.turned_away = Some(TurnedAway {
            addr: "198.51.100.9:1".into(),
            reason: "run_in_progress".into(),
            at_ms: 0,
        });
        assert!(text(&f.column(Some(&s))).contains("Turned away 198.51.100.9:1 (run in progress)"));
        let mut g = LanForm::new("Ace");
        g.refresh(Some(&s));
        s.code = Some(partner().encode());
        g.refresh(Some(&s));
        assert!(text(&g.column(Some(&s))).contains("code changed"));
        assert!(text(&g.column(None)).contains("Starting"));
    }

    #[test]
    fn disconnect_request() {
        let mut f = LanForm::new("Ace");
        let r = f.disconnect();
        assert!(requests(&r)[0].disconnect);
    }

    #[test]
    fn all_text_is_ascii() {
        let f = LanForm::new("Ace");
        let mut s = linked(10.0);
        s.stun.nat = NatKind::Varies;
        for l in f.column(Some(&s)) {
            assert!(l.text.is_ascii(), "{}", l.text);
        }
    }
}
