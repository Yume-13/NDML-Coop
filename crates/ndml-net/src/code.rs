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

//! Connection codes: a player's public endpoint and session nonce as a short,
//! typo-checked string to paste to the partner.
//!
//! Layout (big-endian bit stream, Crockford base32, groups of five joined by
//! `-`):
//!
//! ```text
//! version:4  ip:32|128  port:16  nonce:32  crc16:16
//! ```
//!
//! An IPv4 code is exactly 100 bits = 20 characters (`XXXXX-XXXXX-XXXXX-XXXXX`);
//! an IPv6 code is 40 characters. The CRC (CRC-16/CCITT-FALSE over the version
//! nibble and the fields) catches any single mistyped character. Crockford's
//! alphabet has no `I L O U`; on input `I`/`L` read as `1`, `O` as `0`, case is
//! ignored and dashes and spaces are skipped.

#![forbid(unsafe_code)]

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Current code format version.
pub const CODE_VERSION: u8 = 1;
const V4_CHARS: usize = 20;
const V6_CHARS: usize = 40;

/// A decoded connection code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionCode {
    /// Where to send packets (public endpoint from STUN, or a LAN address).
    pub addr: SocketAddr,
    /// The sender nonce the owner of the code will use.
    pub nonce: u32,
}

/// Why a code could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeError {
    /// Nothing but separators.
    Empty,
    /// A character outside the code alphabet (1-based position in the input).
    InvalidChar {
        /// The offending character.
        ch: char,
        /// Its position in the input, counting from 1.
        pos: usize,
    },
    /// Wrong number of code characters.
    Length(usize),
    /// Made by an incompatible version.
    Version(u8),
    /// Checksum mismatch: a character is wrong.
    Checksum,
}

impl fmt::Display for CodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the code is empty"),
            Self::InvalidChar { ch, pos } => {
                write!(f, "'{ch}' at position {pos} is not a valid code character")
            }
            Self::Length(n) => write!(
                f,
                "a code has {V4_CHARS} characters (or {V6_CHARS} for IPv6), this one has {n}: \
                 check that it was copied completely"
            ),
            Self::Version(v) => write!(
                f,
                "the code was made by an incompatible version (format {v}, we read {CODE_VERSION})"
            ),
            Self::Checksum => write!(f, "the code doesn't check out: a character is mistyped"),
        }
    }
}

impl std::error::Error for CodeError {}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[derive(Default)]
struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    fn put(&mut self, value: u128, width: u32) {
        for i in (0..width).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
    }
}

struct BitReader<'a> {
    bits: &'a [bool],
    at: usize,
}

impl BitReader<'_> {
    fn take(&mut self, width: usize) -> u128 {
        let mut v = 0u128;
        for _ in 0..width {
            v = (v << 1) | u128::from(self.bits.get(self.at).copied().unwrap_or(false));
            self.at += 1;
        }
        v
    }
}

impl ConnectionCode {
    fn fields(&self) -> (u128, u32) {
        match self.addr.ip() {
            IpAddr::V4(a) => (u128::from(u32::from(a)), 32),
            IpAddr::V6(a) => (u128::from(a), 128),
        }
    }

    fn checksum(version: u8, ip: u128, ip_bits: u32, port: u16, nonce: u32) -> u16 {
        let mut bytes = vec![version];
        let ip_bytes = ip.to_be_bytes();
        bytes.extend_from_slice(&ip_bytes[16 - (ip_bits as usize / 8)..]);
        bytes.extend_from_slice(&port.to_be_bytes());
        bytes.extend_from_slice(&nonce.to_be_bytes());
        crc16(&bytes)
    }

    /// The code string, grouped in fives.
    #[must_use]
    pub fn encode(&self) -> String {
        let (ip, ip_bits) = self.fields();
        let port = self.addr.port();
        let mut w = BitWriter::default();
        w.put(u128::from(CODE_VERSION), 4);
        w.put(ip, ip_bits);
        w.put(u128::from(port), 16);
        w.put(u128::from(self.nonce), 32);
        w.put(
            u128::from(Self::checksum(CODE_VERSION, ip, ip_bits, port, self.nonce)),
            16,
        );
        while w.bits.len() % 5 != 0 {
            w.bits.push(false);
        }
        let chars: Vec<char> = w
            .bits
            .chunks(5)
            .map(|c| {
                let idx = c.iter().fold(0usize, |v, &b| (v << 1) | usize::from(b));
                char::from(ALPHABET[idx])
            })
            .collect();
        chars
            .chunks(5)
            .map(|g| g.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Read a code typed or pasted by a person.
    pub fn parse(input: &str) -> Result<Self, CodeError> {
        let mut values = Vec::with_capacity(V6_CHARS);
        for (i, ch) in input.chars().enumerate() {
            let c = ch.to_ascii_uppercase();
            let c = match c {
                '-' | ' ' | '_' | '\t' | '\r' | '\n' => continue,
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            };
            let idx = u8::try_from(c)
                .ok()
                .and_then(|b| ALPHABET.iter().position(|&a| a == b))
                .ok_or(CodeError::InvalidChar { ch, pos: i + 1 })?;
            values.push(idx);
        }
        let ip_bits = match values.len() {
            0 => return Err(CodeError::Empty),
            V4_CHARS => 32,
            V6_CHARS => 128,
            n => return Err(CodeError::Length(n)),
        };
        let bits: Vec<bool> = values
            .iter()
            .flat_map(|&v| (0..5).rev().map(move |i| (v >> i) & 1 == 1))
            .collect();
        let mut r = BitReader { bits: &bits, at: 0 };
        let version = r.take(4) as u8;
        let ip = r.take(ip_bits);
        let port = r.take(16) as u16;
        let nonce = r.take(32) as u32;
        let crc = r.take(16) as u16;
        // A mistyped first character could also change the version: check
        // the checksum first so the message points at the typo.
        if crc != Self::checksum(version, ip, ip_bits as u32, port, nonce) {
            return Err(CodeError::Checksum);
        }
        if version != CODE_VERSION {
            return Err(CodeError::Version(version));
        }
        let addr = if ip_bits == 32 {
            IpAddr::V4(Ipv4Addr::from(ip as u32))
        } else {
            IpAddr::V6(Ipv6Addr::from(ip))
        };
        Ok(Self {
            addr: SocketAddr::new(addr, port),
            nonce,
        })
    }
}

impl fmt::Display for ConnectionCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

impl FromStr for ConnectionCode {
    type Err = CodeError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4() -> ConnectionCode {
        ConnectionCode {
            addr: "203.0.113.45:51234".parse().unwrap(),
            nonce: 0xC0FF_EE42,
        }
    }

    #[test]
    fn ipv4_round_trip_is_twenty_chars() {
        let s = v4().encode();
        assert_eq!(s.len(), 23, "{s}");
        assert_eq!(s.matches('-').count(), 3);
        assert_eq!(ConnectionCode::parse(&s), Ok(v4()));
        assert_eq!(s.parse::<ConnectionCode>(), Ok(v4()));
    }

    #[test]
    fn ipv6_round_trip() {
        let c = ConnectionCode {
            addr: "[2001:db8::1]:7777".parse().unwrap(),
            nonce: 1,
        };
        let s = c.to_string();
        assert_eq!(s.replace('-', "").len(), 40);
        assert_eq!(ConnectionCode::parse(&s), Ok(c));
    }

    #[test]
    fn forgiving_input() {
        let s = v4().encode();
        let messy = format!("  {}  ", s.to_lowercase().replace('-', " "));
        assert_eq!(ConnectionCode::parse(&messy), Ok(v4()));
        // Crockford look-alikes: O -> 0, I/L -> 1.
        let looky = s.replace('0', "o").replace('1', "l");
        assert_eq!(ConnectionCode::parse(&looky), Ok(v4()));
    }

    #[test]
    fn every_single_character_typo_is_caught() {
        let s = v4().encode();
        let chars: Vec<char> = s.chars().collect();
        for (i, &orig) in chars.iter().enumerate() {
            if orig == '-' {
                continue;
            }
            for &a in ALPHABET {
                let c = char::from(a);
                if c == orig {
                    continue;
                }
                let mut t = chars.clone();
                t[i] = c;
                let typo: String = t.into_iter().collect();
                assert_eq!(
                    ConnectionCode::parse(&typo),
                    Err(CodeError::Checksum),
                    "{typo}"
                );
            }
        }
    }

    #[test]
    fn clear_errors() {
        assert_eq!(ConnectionCode::parse(" - "), Err(CodeError::Empty));
        let s = v4().encode();
        assert_eq!(
            ConnectionCode::parse(&s[..s.len() - 2]),
            Err(CodeError::Length(18))
        );
        let bad = format!("{}U{}", &s[..3], &s[4..]);
        assert_eq!(
            ConnectionCode::parse(&bad),
            Err(CodeError::InvalidChar { ch: 'U', pos: 4 })
        );
        assert!(matches!(
            ConnectionCode::parse("ÄÄÄ"),
            Err(CodeError::InvalidChar { pos: 1, .. })
        ));
        assert!(CodeError::Checksum.to_string().contains("mistyped"));
        assert!(CodeError::Length(3).to_string().contains("20 characters"));
    }

    #[test]
    fn version_mismatch_is_reported() {
        // Build a valid-checksum code with version 2 by hand.
        let c = v4();
        let (ip, bits) = c.fields();
        let mut w = BitWriter::default();
        w.put(2, 4);
        w.put(ip, bits);
        w.put(u128::from(c.addr.port()), 16);
        w.put(u128::from(c.nonce), 32);
        w.put(
            u128::from(ConnectionCode::checksum(
                2,
                ip,
                bits,
                c.addr.port(),
                c.nonce,
            )),
            16,
        );
        let s: String = w
            .bits
            .chunks(5)
            .map(|ch| {
                let idx = ch.iter().fold(0usize, |v, &b| (v << 1) | usize::from(b));
                char::from(ALPHABET[idx])
            })
            .collect();
        assert_eq!(ConnectionCode::parse(&s), Err(CodeError::Version(2)));
    }
}
