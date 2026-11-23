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

//! Minimal STUN (RFC 5389) client: one Binding request, read the
//! XOR-MAPPED-ADDRESS (or MAPPED-ADDRESS) of the response. The request goes
//! out on the game's socket, so the mapping is the one the game uses.

#![forbid(unsafe_code)]

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use crate::rng;

/// RFC 5389 magic cookie.
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// Public STUN servers tried by default.
pub const DEFAULT_SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const BINDING_ERROR: u16 = 0x0111;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const HEADER_LEN: usize = 20;

/// A 96-bit STUN transaction id.
pub type TransactionId = [u8; 12];

/// Why a STUN response was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StunError {
    /// Shorter than a STUN header.
    TooShort,
    /// Missing magic cookie or bad leading bits: not STUN.
    NotStun,
    /// Declared length does not match the datagram.
    BadLength,
    /// A response to some other request.
    WrongTransaction,
    /// A STUN message, but not a Binding response.
    UnexpectedType(u16),
    /// The server returned a Binding error (code, reason).
    ErrorResponse(u16, String),
    /// An attribute is malformed.
    BadAttribute(u16),
    /// Success response without a mapped address.
    NoAddress,
}

impl fmt::Display for StunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => write!(f, "STUN response too short"),
            Self::NotStun => write!(f, "not a STUN message"),
            Self::BadLength => write!(f, "STUN length field does not match the datagram"),
            Self::WrongTransaction => write!(f, "STUN response for another transaction"),
            Self::UnexpectedType(t) => write!(f, "unexpected STUN message type {t:#06x}"),
            Self::ErrorResponse(c, r) => write!(f, "STUN server error {c}: {r}"),
            Self::BadAttribute(a) => write!(f, "malformed STUN attribute {a:#06x}"),
            Self::NoAddress => write!(f, "STUN response carries no mapped address"),
        }
    }
}

impl std::error::Error for StunError {}

/// A fresh random transaction id.
#[must_use]
pub fn new_transaction_id() -> TransactionId {
    let mut id = [0u8; 12];
    id[..8].copy_from_slice(&rng::random_u64().to_le_bytes());
    id[8..].copy_from_slice(&rng::random_u64().to_le_bytes()[..4]);
    id
}

/// A Binding request with no attributes.
#[must_use]
pub fn binding_request(txid: &TransactionId) -> [u8; HEADER_LEN] {
    let mut b = [0u8; HEADER_LEN];
    b[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    b[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    b[8..20].copy_from_slice(txid);
    b
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn parse_address(
    attr: u16,
    v: &[u8],
    xor: bool,
    txid: &TransactionId,
) -> Result<SocketAddr, StunError> {
    if v.len() < 4 {
        return Err(StunError::BadAttribute(attr));
    }
    let family = v[1];
    let mut port = be16(v, 2);
    let cookie = MAGIC_COOKIE.to_be_bytes();
    if xor {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match family {
        0x01 => {
            let raw: [u8; 4] = v
                .get(4..8)
                .and_then(|s| s.try_into().ok())
                .ok_or(StunError::BadAttribute(attr))?;
            let mut a = raw;
            if xor {
                for (x, k) in a.iter_mut().zip(cookie) {
                    *x ^= k;
                }
            }
            IpAddr::V4(Ipv4Addr::from(a))
        }
        0x02 => {
            let raw: [u8; 16] = v
                .get(4..20)
                .and_then(|s| s.try_into().ok())
                .ok_or(StunError::BadAttribute(attr))?;
            let mut a = raw;
            if xor {
                let key: Vec<u8> = cookie.iter().chain(txid.iter()).copied().collect();
                for (x, k) in a.iter_mut().zip(key) {
                    *x ^= k;
                }
            }
            IpAddr::V6(Ipv6Addr::from(a))
        }
        _ => return Err(StunError::BadAttribute(attr)),
    };
    Ok(SocketAddr::new(ip, port))
}

/// Parse a Binding response to the request with id `txid`.
pub fn parse_binding_response(bytes: &[u8], txid: &TransactionId) -> Result<SocketAddr, StunError> {
    if bytes.len() < HEADER_LEN {
        return Err(StunError::TooShort);
    }
    let ty = be16(bytes, 0);
    let len = usize::from(be16(bytes, 2));
    if ty & 0xC000 != 0 || bytes[4..8] != MAGIC_COOKIE.to_be_bytes() {
        return Err(StunError::NotStun);
    }
    if len % 4 != 0 || HEADER_LEN + len > bytes.len() {
        return Err(StunError::BadLength);
    }
    if bytes[8..20] != txid[..] {
        return Err(StunError::WrongTransaction);
    }
    if ty != BINDING_SUCCESS && ty != BINDING_ERROR {
        return Err(StunError::UnexpectedType(ty));
    }

    let mut attrs = &bytes[HEADER_LEN..HEADER_LEN + len];
    let mut mapped = None;
    let mut xor_mapped = None;
    let mut error = None;
    while !attrs.is_empty() {
        if attrs.len() < 4 {
            return Err(StunError::BadLength);
        }
        let at = be16(attrs, 0);
        let alen = usize::from(be16(attrs, 2));
        let padded = alen.div_ceil(4) * 4;
        let value = attrs.get(4..4 + alen).ok_or(StunError::BadAttribute(at))?;
        match at {
            ATTR_XOR_MAPPED_ADDRESS => xor_mapped = Some(parse_address(at, value, true, txid)?),
            ATTR_MAPPED_ADDRESS => mapped = Some(parse_address(at, value, false, txid)?),
            ATTR_ERROR_CODE if value.len() >= 4 => {
                let code = u16::from(value[2] & 0x07) * 100 + u16::from(value[3]);
                error = Some((code, String::from_utf8_lossy(&value[4..]).into_owned()));
            }
            _ => {} // unknown or uninteresting (SOFTWARE, FINGERPRINT, ...)
        }
        attrs = attrs.get(4 + padded..).unwrap_or(&[]);
    }
    if ty == BINDING_ERROR {
        let (code, reason) = error.unwrap_or((0, String::new()));
        return Err(StunError::ErrorResponse(code, reason));
    }
    xor_mapped.or(mapped).ok_or(StunError::NoAddress)
}

/// Whether `bytes` could be a STUN message (top bits zero, magic cookie). NDML
/// datagrams start with `ND`, so the two never mix.
#[must_use]
pub fn looks_like_stun(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN && bytes[0] & 0xC0 == 0 && bytes[4..8] == MAGIC_COOKIE.to_be_bytes()
}

/// The transaction id of a request (or response). `None` if too short.
#[must_use]
pub fn transaction_id(bytes: &[u8]) -> Option<TransactionId> {
    bytes.get(8..HEADER_LEN)?.try_into().ok()
}

/// A Binding success response carrying `mapped` (IPv4 only), for tests and
/// fake servers.
#[must_use]
pub fn binding_success(txid: &TransactionId, mapped: SocketAddr) -> Option<Vec<u8>> {
    let SocketAddr::V4(m) = mapped else {
        return None;
    };
    let mut b = Vec::with_capacity(HEADER_LEN + 12);
    b.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    b.extend_from_slice(&12u16.to_be_bytes());
    b.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    b.extend_from_slice(txid);
    b.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    b.extend_from_slice(&8u16.to_be_bytes());
    b.extend_from_slice(&[0, 1]);
    b.extend_from_slice(&(m.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
    b.extend_from_slice(&(u32::from(*m.ip()) ^ MAGIC_COOKIE).to_be_bytes());
    Some(b)
}

/// How long [`Discovery`] waits for one server before trying the next.
pub const PER_SERVER_TIMEOUT: Duration = Duration::from_secs(2);
/// How often [`Discovery`] repeats a request to the server it is asking.
pub const RESEND_INTERVAL: Duration = Duration::from_millis(500);

/// What [`Discovery`] learned about the NAT from two servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatBehavior {
    /// Fewer than two servers answered.
    Unknown,
    /// Both servers saw the same `ip:port`: hole punching should work.
    Consistent,
    /// The servers saw different endpoints (a symmetric NAT): punching works
    /// only against a permissive NAT; a forwarded port is the fallback.
    Varies,
}

#[derive(Debug, Clone)]
struct Attempt {
    server: SocketAddr,
    txid: TransactionId,
    started: Duration,
    last_send: Option<Duration>,
}

/// Non-blocking public endpoint discovery over any transport, driven by
/// [`Discovery::poll`]. Asks the servers in turn, each every
/// [`RESEND_INTERVAL`] for up to [`PER_SERVER_TIMEOUT`], and stops after two
/// answers (enough to tell a symmetric NAT) or when all were tried.
#[derive(Debug, Clone)]
pub struct Discovery {
    servers: Vec<SocketAddr>,
    next: usize,
    current: Option<Attempt>,
    results: Vec<(SocketAddr, SocketAddr)>,
    failures: Vec<(SocketAddr, String)>,
    want: usize,
}

impl Discovery {
    /// Ask `servers` (already resolved, same address family as the socket).
    #[must_use]
    pub fn new(servers: Vec<SocketAddr>) -> Self {
        Self {
            servers,
            next: 0,
            current: None,
            results: Vec::new(),
            failures: Vec::new(),
            want: 2,
        }
    }

    /// The request to send now, if one is due: `(server, bytes)`.
    pub fn poll(&mut self, now: Duration) -> Option<(SocketAddr, [u8; HEADER_LEN])> {
        loop {
            if self.is_done() {
                self.current = None;
                return None;
            }
            let Some(a) = &mut self.current else {
                let server = *self.servers.get(self.next)?;
                self.current = Some(Attempt {
                    server,
                    txid: new_transaction_id(),
                    started: now,
                    last_send: None,
                });
                continue;
            };
            if now.saturating_sub(a.started) >= PER_SERVER_TIMEOUT {
                self.failures.push((a.server, "no answer".into()));
                self.current = None;
                self.next += 1;
                continue;
            }
            if a.last_send
                .is_none_or(|t| now.saturating_sub(t) >= RESEND_INTERVAL)
            {
                a.last_send = Some(now);
                return Some((a.server, binding_request(&a.txid)));
            }
            return None;
        }
    }

    /// Offer a datagram from `from`; returns whether it was STUN (consumed).
    pub fn on_datagram(&mut self, from: SocketAddr, bytes: &[u8]) -> bool {
        if !looks_like_stun(bytes) {
            return false;
        }
        let Some(a) = &self.current else {
            return true; // late answer to an attempt given up on
        };
        if from != a.server {
            return true;
        }
        match parse_binding_response(bytes, &a.txid) {
            Ok(mapped) => {
                self.results.push((a.server, mapped));
                self.current = None;
                self.next += 1;
            }
            Err(e @ StunError::ErrorResponse(..)) => {
                self.failures.push((a.server, e.to_string()));
                self.current = None;
                self.next += 1;
            }
            Err(_) => {} // a stale transaction or garbage: keep waiting
        }
        true
    }

    /// The public endpoint from the first server that answered.
    #[must_use]
    pub fn mapped(&self) -> Option<SocketAddr> {
        self.results.first().map(|&(_, m)| m)
    }

    /// The server that gave [`Self::mapped`].
    #[must_use]
    pub fn mapped_by(&self) -> Option<SocketAddr> {
        self.results.first().map(|&(s, _)| s)
    }

    /// No more requests will be sent.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.results.len() >= self.want
            || (self.current.is_none() && self.next >= self.servers.len())
    }

    /// What two answers say about the NAT.
    #[must_use]
    pub fn nat(&self) -> NatBehavior {
        match self.results.as_slice() {
            [(_, a), (_, b), ..] if a == b => NatBehavior::Consistent,
            [_, _, ..] => NatBehavior::Varies,
            _ => NatBehavior::Unknown,
        }
    }

    /// Once done without any answer: what went wrong, per server.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        if !self.is_done() || !self.results.is_empty() {
            return None;
        }
        if self.servers.is_empty() {
            return Some("no STUN server to ask".into());
        }
        let parts: Vec<String> = self
            .failures
            .iter()
            .map(|(s, e)| format!("{s}: {e}"))
            .collect();
        Some(parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Response modelled on RFC 5769 section 2.2 (XOR-MAPPED-ADDRESS
    /// 192.0.2.1:32853), without integrity and fingerprint.
    const TXID: TransactionId = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];
    const IPV4_RESPONSE: [u8; 48] = [
        0x01, 0x01, 0x00, 0x1c, // Binding success, length 28
        0x21, 0x12, 0xa4, 0x42, // cookie
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae, // txid
        0x80, 0x22, 0x00, 0x0b, // SOFTWARE, 11 bytes + 1 pad
        0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00,
        0x08, // XOR-MAPPED-ADDRESS
        0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
    ];

    #[test]
    fn request_layout() {
        let r = binding_request(&TXID);
        assert_eq!(&r[..8], &[0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42]);
        assert_eq!(&r[8..], &TXID);
        assert_ne!(new_transaction_id(), new_transaction_id());
    }

    #[test]
    fn parses_ipv4_xor_mapped_address() {
        let addr = parse_binding_response(&IPV4_RESPONSE, &TXID).unwrap();
        assert_eq!(addr, "192.0.2.1:32853".parse().unwrap());
    }

    #[test]
    fn parses_ipv6_xor_mapped_address() {
        // RFC 5769 section 2.3: 2001:db8:1234:5678:11:2233:4455:6677 port 32853.
        let mut b = IPV4_RESPONSE[..20].to_vec();
        b[2..4].copy_from_slice(&24u16.to_be_bytes());
        b.extend_from_slice(&[0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47]);
        b.extend_from_slice(&[
            0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2,
            0xb9, 0xd9,
        ]);
        let addr = parse_binding_response(&b, &TXID).unwrap();
        assert_eq!(
            addr,
            "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                .parse()
                .unwrap()
        );
    }

    #[test]
    fn falls_back_to_plain_mapped_address() {
        let mut b = IPV4_RESPONSE[..20].to_vec();
        b[2..4].copy_from_slice(&12u16.to_be_bytes());
        b.extend_from_slice(&[
            0x00, 0x01, 0x00, 0x08, 0x00, 0x01, 0x1f, 0x90, 203, 0, 113, 7,
        ]);
        let addr = parse_binding_response(&b, &TXID).unwrap();
        assert_eq!(addr, "203.0.113.7:8080".parse().unwrap());
    }

    #[test]
    fn rejects_malformed_responses() {
        let ok = IPV4_RESPONSE;
        assert_eq!(
            parse_binding_response(&ok[..10], &TXID),
            Err(StunError::TooShort)
        );
        let mut b = ok;
        b[4] = 0;
        assert_eq!(parse_binding_response(&b, &TXID), Err(StunError::NotStun));
        let mut b = ok;
        b[0] = 0xC1;
        assert_eq!(parse_binding_response(&b, &TXID), Err(StunError::NotStun));
        let mut b = ok;
        b[3] = 0x40; // claims 64 bytes
        assert_eq!(parse_binding_response(&b, &TXID), Err(StunError::BadLength));
        let mut other = TXID;
        other[0] ^= 1;
        assert_eq!(
            parse_binding_response(&ok, &other),
            Err(StunError::WrongTransaction)
        );
        let mut b = ok;
        b[1] = 0x01; // 0x0001 = request
        b[0] = 0x00;
        assert_eq!(
            parse_binding_response(&b, &TXID),
            Err(StunError::UnexpectedType(1))
        );
        // Attribute length runs past the message.
        let mut b = ok;
        b[38] = 0x00;
        b[39] = 0x40;
        assert_eq!(
            parse_binding_response(&b, &TXID),
            Err(StunError::BadAttribute(0x0020))
        );
        // Unknown address family.
        let mut b = ok;
        b[41] = 0x07;
        assert_eq!(
            parse_binding_response(&b, &TXID),
            Err(StunError::BadAttribute(0x0020))
        );
        // Success without an address.
        let mut b = ok[..36].to_vec();
        b[2..4].copy_from_slice(&16u16.to_be_bytes());
        assert_eq!(parse_binding_response(&b, &TXID), Err(StunError::NoAddress));
    }

    fn success_for(txid: &TransactionId, mapped: SocketAddr) -> Vec<u8> {
        binding_success(txid, mapped).unwrap()
    }

    fn txid_of(request: &[u8]) -> TransactionId {
        request[8..20].try_into().unwrap()
    }

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn discovery_resends_moves_on_and_checks_the_nat() {
        let a: SocketAddr = "10.9.0.1:3478".parse().unwrap();
        let b: SocketAddr = "10.9.0.2:3478".parse().unwrap();
        let mapped: SocketAddr = "203.0.113.5:40000".parse().unwrap();
        let mut d = Discovery::new(vec![a, b]);
        let (to, req) = d.poll(ms(0)).unwrap();
        assert_eq!(to, a);
        assert!(looks_like_stun(&req));
        assert!(d.poll(ms(100)).is_none(), "resend not due yet");
        let (_, again) = d.poll(ms(500)).unwrap();
        assert_eq!(txid_of(&again), txid_of(&req), "same transaction");
        // Server a never answers; after 2 s it's b's turn.
        let (to, req_b) = d.poll(ms(2000)).unwrap();
        assert_eq!(to, b);
        // A NDML datagram is not STUN; an answer from a stranger is ignored.
        assert!(!d.on_datagram(b, b"ND\x01\x03...................."));
        assert!(d.on_datagram(a, &success_for(&txid_of(&req_b), mapped)));
        assert_eq!(d.mapped(), None);
        assert!(d.on_datagram(b, &success_for(&txid_of(&req_b), mapped)));
        assert_eq!(d.mapped(), Some(mapped));
        assert_eq!(d.mapped_by(), Some(b));
        assert!(d.is_done());
        assert!(d.poll(ms(2100)).is_none());
        assert_eq!(d.nat(), NatBehavior::Unknown, "only one server answered");
        assert_eq!(d.error(), None);
    }

    #[test]
    fn discovery_tells_a_symmetric_nat_and_reports_failures() {
        let a: SocketAddr = "10.9.0.1:3478".parse().unwrap();
        let b: SocketAddr = "10.9.0.2:3478".parse().unwrap();
        let mut d = Discovery::new(vec![a, b]);
        let (_, r) = d.poll(ms(0)).unwrap();
        d.on_datagram(
            a,
            &success_for(&txid_of(&r), "203.0.113.5:40000".parse().unwrap()),
        );
        assert!(!d.is_done(), "asks a second server");
        let (_, r) = d.poll(ms(10)).unwrap();
        d.on_datagram(
            b,
            &success_for(&txid_of(&r), "203.0.113.5:40001".parse().unwrap()),
        );
        assert!(d.is_done());
        assert_eq!(d.nat(), NatBehavior::Varies);

        let mut d = Discovery::new(vec![a]);
        d.poll(ms(0));
        assert!(d.poll(ms(2000)).is_none());
        assert!(d.is_done());
        assert_eq!(d.error().unwrap(), "10.9.0.1:3478: no answer");
        assert_eq!(
            Discovery::new(Vec::new()).error().unwrap(),
            "no STUN server to ask"
        );
    }

    #[test]
    fn reports_error_responses() {
        let mut b = IPV4_RESPONSE[..20].to_vec();
        b[0..2].copy_from_slice(&BINDING_ERROR.to_be_bytes());
        b[2..4].copy_from_slice(&16u16.to_be_bytes());
        // ERROR-CODE 400 "Bad Request"[..8]
        b.extend_from_slice(&[0x00, 0x09, 0x00, 0x0c, 0, 0, 4, 0]);
        b.extend_from_slice(b"Bad Requ");
        assert_eq!(
            parse_binding_response(&b, &TXID),
            Err(StunError::ErrorResponse(400, "Bad Requ".into()))
        );
    }
}
