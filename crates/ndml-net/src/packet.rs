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

//! Wire format: a fixed 20-byte header followed by framed chunks.
//!
//! ```text
//! offset size field
//!  0     2    magic "ND"
//!  2     1    protocol version (PROTOCOL_VERSION)
//!  3     1    packet kind (Punch, PunchAck, Data, Disconnect, Refused) in the
//!             low 3 bits, the sender's connection epoch (0-31) in the high 5
//!             (see [`Header::epoch`])
//!  4     4    sender nonce (random per process run; stale or foreign packets are dropped)
//!  8     2    seq       (this packet's sequence number, wraps)
//! 10     2    ack       (newest seq received from the peer)
//! 12     4    ack_bits  (bit i set = seq `ack - 1 - i` was received too)
//! 16     4    send time (ms since the sender's epoch)
//! ```
//!
//! All integers are little-endian. A Data packet's payload is a run of chunks:
//!
//! ```text
//! 0x01 id:u16 len:u16 body   reliable-ordered message
//! 0x02        len:u16 body   unreliable-latest message
//! ```
//!
//! Bodies are `postcard`-encoded [`Message`](crate::msg::Message)s, at most
//! [`MAX_MESSAGE`] bytes. `Punch`/`PunchAck` carry [`PunchBody`], `Refused`
//! carries [`RefusedBody`].

#![forbid(unsafe_code)]

use std::fmt;

/// First two bytes of every datagram.
pub const MAGIC: [u8; 2] = *b"ND";
/// Bumped on any incompatible wire or protocol change; packets with another
/// version are dropped.
pub const PROTOCOL_VERSION: u8 = 11;
/// Largest datagram ever sent.
pub const MAX_DATAGRAM: usize = 1200;
/// Size of the fixed header.
pub const HEADER_LEN: usize = 20;
/// Framing bytes in front of a reliable chunk body (tag, id, len).
pub const RELIABLE_OVERHEAD: usize = 5;
/// Framing bytes in front of an unreliable chunk body (tag, len).
pub const UNRELIABLE_OVERHEAD: usize = 3;
/// Largest encoded message that fits in one datagram on either channel.
pub const MAX_MESSAGE: usize = MAX_DATAGRAM - HEADER_LEN - RELIABLE_OVERHEAD;

const TAG_RELIABLE: u8 = 0x01;
const TAG_UNRELIABLE: u8 = 0x02;

/// What a datagram is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    /// Hole-punch probe, sent repeatedly while connecting.
    Punch,
    /// Answer to a punch: "I hear you".
    PunchAck,
    /// Acks plus zero or more chunks. An empty one is a keepalive.
    Data,
    /// The sender is leaving.
    Disconnect,
    /// The sender turns the receiver away.
    Refused,
}

impl PacketKind {
    fn to_byte(self) -> u8 {
        match self {
            Self::Punch => 1,
            Self::PunchAck => 2,
            Self::Data => 3,
            Self::Disconnect => 4,
            Self::Refused => 5,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Punch,
            2 => Self::PunchAck,
            3 => Self::Data,
            4 => Self::Disconnect,
            5 => Self::Refused,
            _ => return None,
        })
    }
}

/// The fixed header (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Packet kind.
    pub kind: PacketKind,
    /// Sender's nonce.
    pub nonce: u32,
    /// This packet's sequence number.
    pub seq: u16,
    /// Newest sequence number received from the peer.
    pub ack: u16,
    /// Receipt bits for the 32 packets before `ack`.
    pub ack_bits: u32,
    /// Sender's clock in ms.
    pub send_time_ms: u32,
    /// The sender's connection epoch (below [`EPOCHS`], new per connection):
    /// a packet of another epoch than the learned one is dropped.
    pub epoch: u8,
}

/// Connection epochs (5 bits of the kind byte).
pub const EPOCHS: u8 = 32;

/// Why a datagram was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    /// Shorter than the header.
    TooShort,
    /// Foreign (wrong magic).
    BadMagic,
    /// Another protocol version.
    Version(u8),
    /// Unknown packet kind byte.
    UnknownKind(u8),
    /// A chunk runs past the end of the datagram.
    Truncated,
    /// Unknown chunk tag.
    UnknownChunk(u8),
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => write!(f, "datagram shorter than the header"),
            Self::BadMagic => write!(f, "not an NDML datagram"),
            Self::Version(v) => write!(
                f,
                "peer speaks protocol version {v}, this build speaks {PROTOCOL_VERSION}"
            ),
            Self::UnknownKind(k) => write!(f, "unknown packet kind {k}"),
            Self::Truncated => write!(f, "chunk runs past the end of the datagram"),
            Self::UnknownChunk(t) => write!(f, "unknown chunk tag {t:#04x}"),
        }
    }
}

impl std::error::Error for PacketError {}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl Header {
    /// Append the 20 header bytes to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC);
        out.push(PROTOCOL_VERSION);
        out.push(self.kind.to_byte() | (self.epoch % EPOCHS) << 3);
        out.extend_from_slice(&self.nonce.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.ack.to_le_bytes());
        out.extend_from_slice(&self.ack_bits.to_le_bytes());
        out.extend_from_slice(&self.send_time_ms.to_le_bytes());
    }

    /// Parse the header; returns it and the payload after it.
    pub fn read(datagram: &[u8]) -> Result<(Self, &[u8]), PacketError> {
        if datagram.len() < HEADER_LEN {
            return Err(PacketError::TooShort);
        }
        if datagram[..2] != MAGIC {
            return Err(PacketError::BadMagic);
        }
        if datagram[2] != PROTOCOL_VERSION {
            return Err(PacketError::Version(datagram[2]));
        }
        let kind = PacketKind::from_byte(datagram[3] & 0x07)
            .ok_or(PacketError::UnknownKind(datagram[3]))?;
        let header = Self {
            kind,
            epoch: datagram[3] >> 3,
            nonce: u32_at(datagram, 4),
            seq: u16_at(datagram, 8),
            ack: u16_at(datagram, 10),
            ack_bits: u32_at(datagram, 12),
            send_time_ms: u32_at(datagram, 16),
        };
        Ok((header, &datagram[HEADER_LEN..]))
    }
}

/// Body of `Punch` and `PunchAck` packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchBody {
    /// Peer id the host assigns to the receiver ([`Self::UNASSIGNED`] from clients).
    pub assigned: u8,
    /// The nonce the sender expects the receiver to have (0 = unknown).
    pub expected_nonce: u32,
}

impl PunchBody {
    /// "No peer id assigned" marker.
    pub const UNASSIGNED: u8 = 0xFF;
    /// Encoded size.
    pub const LEN: usize = 5;

    /// Append the body bytes.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.assigned);
        out.extend_from_slice(&self.expected_nonce.to_le_bytes());
    }

    /// Parse a body.
    pub fn read(payload: &[u8]) -> Result<Self, PacketError> {
        if payload.len() < Self::LEN {
            return Err(PacketError::Truncated);
        }
        Ok(Self {
            assigned: payload[0],
            expected_nonce: u32_at(payload, 1),
        })
    }
}

/// Why a peer was turned away (wire byte append-only; unknown codes are
/// [`RefuseReason::Other`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// The host has no free player slot.
    Full,
    /// The host is in a run; joining happens at the main menu.
    RunInProgress,
    /// Different protocol, mod version or game build.
    VersionMismatch,
    /// A reason code from a newer build.
    Other(u8),
}

impl RefuseReason {
    /// The wire byte.
    #[must_use]
    pub fn to_byte(self) -> u8 {
        match self {
            Self::Full => 1,
            Self::RunInProgress => 2,
            Self::VersionMismatch => 3,
            Self::Other(b) => b,
        }
    }

    /// From the wire byte.
    #[must_use]
    pub fn from_byte(b: u8) -> Self {
        match b {
            1 => Self::Full,
            2 => Self::RunInProgress,
            3 => Self::VersionMismatch,
            other => Self::Other(other),
        }
    }

    /// Short machine name, as written to the lobby status file.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::RunInProgress => "run_in_progress",
            Self::VersionMismatch => "version_mismatch",
            Self::Other(_) => "other",
        }
    }
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("the session is full"),
            Self::RunInProgress => f.write_str("a run is already in progress"),
            Self::VersionMismatch => f.write_str("the game or co-op mod versions differ"),
            Self::Other(b) => write!(f, "refused (reason code {b})"),
        }
    }
}

/// Body of `Refused` packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusedBody {
    /// Why.
    pub reason: RefuseReason,
    /// The refused peer's nonce (0 = unknown); a stale refusal is ignored.
    pub expected_nonce: u32,
}

impl RefusedBody {
    /// Encoded size.
    pub const LEN: usize = 5;

    /// Append the body bytes.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.reason.to_byte());
        out.extend_from_slice(&self.expected_nonce.to_le_bytes());
    }

    /// Parse a body.
    pub fn read(payload: &[u8]) -> Result<Self, PacketError> {
        if payload.len() < Self::LEN {
            return Err(PacketError::Truncated);
        }
        Ok(Self {
            reason: RefuseReason::from_byte(payload[0]),
            expected_nonce: u32_at(payload, 1),
        })
    }
}

/// One framed message inside a Data packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunk<'a> {
    /// Reliable-ordered message with its channel sequence id.
    Reliable {
        /// Reliable message id (wraps).
        id: u16,
        /// Encoded message.
        body: &'a [u8],
    },
    /// Unreliable-latest message.
    Unreliable {
        /// Encoded message.
        body: &'a [u8],
    },
}

/// Builds one Data datagram, refusing chunks that would exceed the size cap.
#[derive(Debug)]
pub struct PacketWriter {
    buf: Vec<u8>,
    chunks: usize,
}

impl PacketWriter {
    /// Start a datagram with `header`.
    #[must_use]
    pub fn new(header: &Header) -> Self {
        let mut buf = Vec::with_capacity(MAX_DATAGRAM);
        header.write(&mut buf);
        Self { buf, chunks: 0 }
    }

    /// Bytes still free.
    #[must_use]
    pub fn remaining(&self) -> usize {
        MAX_DATAGRAM.saturating_sub(self.buf.len())
    }

    /// Number of chunks written.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks
    }

    /// Append a reliable chunk if it fits. Returns whether it was written.
    pub fn push_reliable(&mut self, id: u16, body: &[u8]) -> bool {
        if body.len() + RELIABLE_OVERHEAD > self.remaining() {
            return false;
        }
        self.buf.push(TAG_RELIABLE);
        self.buf.extend_from_slice(&id.to_le_bytes());
        self.push_body(body);
        true
    }

    /// Append an unreliable chunk if it fits. Returns whether it was written.
    pub fn push_unreliable(&mut self, body: &[u8]) -> bool {
        if body.len() + UNRELIABLE_OVERHEAD > self.remaining() {
            return false;
        }
        self.buf.push(TAG_UNRELIABLE);
        self.push_body(body);
        true
    }

    fn push_body(&mut self, body: &[u8]) {
        // Fits in u16: callers checked it against MAX_DATAGRAM.
        self.buf
            .extend_from_slice(&(body.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(body);
        self.chunks += 1;
    }

    /// The finished datagram.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Iterate over the chunks of a Data payload. Stops at the first error.
#[must_use]
pub fn chunks(payload: &[u8]) -> Chunks<'_> {
    Chunks { rest: payload }
}

/// Iterator returned by [`chunks`].
#[derive(Debug)]
pub struct Chunks<'a> {
    rest: &'a [u8],
}

impl<'a> Chunks<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PacketError> {
        if self.rest.len() < n {
            self.rest = &[];
            return Err(PacketError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn next_chunk(&mut self) -> Result<Chunk<'a>, PacketError> {
        let tag = self.take(1)?[0];
        match tag {
            TAG_RELIABLE => {
                let h = self.take(4)?;
                let id = u16_at(h, 0);
                let len = usize::from(u16_at(h, 2));
                Ok(Chunk::Reliable {
                    id,
                    body: self.take(len)?,
                })
            }
            TAG_UNRELIABLE => {
                let h = self.take(2)?;
                let len = usize::from(u16_at(h, 0));
                Ok(Chunk::Unreliable {
                    body: self.take(len)?,
                })
            }
            other => {
                self.rest = &[];
                Err(PacketError::UnknownChunk(other))
            }
        }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Result<Chunk<'a>, PacketError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            None
        } else {
            Some(self.next_chunk())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            kind: PacketKind::Data,
            epoch: 29,
            nonce: 0xDEAD_BEEF,
            seq: 65535,
            ack: 7,
            ack_bits: 0x8000_0001,
            send_time_ms: 123_456,
        }
    }

    /// The epoch rides in the kind byte; protocol version 7 is refused.
    #[test]
    fn the_epoch_rides_the_kind_byte_and_version_7_is_refused() {
        for kind in [
            PacketKind::Punch,
            PacketKind::PunchAck,
            PacketKind::Data,
            PacketKind::Disconnect,
            PacketKind::Refused,
        ] {
            for epoch in [0, 1, 17, EPOCHS - 1] {
                let h = Header {
                    kind,
                    epoch,
                    ..header()
                };
                let mut buf = Vec::new();
                h.write(&mut buf);
                assert_eq!(buf.len(), HEADER_LEN, "no extra byte");
                assert_eq!(Header::read(&buf).unwrap().0, h);
            }
        }
        assert_eq!(PROTOCOL_VERSION, 11);
        let mut v7 = Vec::new();
        header().write(&mut v7);
        v7[2] = 7;
        v7[3] = PacketKind::Punch.to_byte(); // a version 7 punch: no epoch bits
        assert_eq!(Header::read(&v7), Err(PacketError::Version(7)));
    }

    #[test]
    fn header_round_trips_at_fixed_size() {
        let mut buf = Vec::new();
        header().write(&mut buf);
        assert_eq!(buf.len(), HEADER_LEN);
        let (h, rest) = Header::read(&buf).unwrap();
        assert_eq!(h, header());
        assert!(rest.is_empty());
    }

    #[test]
    fn header_rejects_foreign_data() {
        let mut buf = Vec::new();
        header().write(&mut buf);
        assert_eq!(Header::read(&buf[..19]), Err(PacketError::TooShort));
        let mut bad = buf.clone();
        bad[0] = b'X';
        assert_eq!(Header::read(&bad), Err(PacketError::BadMagic));
        let mut bad = buf.clone();
        bad[2] = 99;
        assert_eq!(Header::read(&bad), Err(PacketError::Version(99)));
        let mut bad = buf;
        bad[3] = 0;
        assert_eq!(Header::read(&bad), Err(PacketError::UnknownKind(0)));
    }

    #[test]
    fn chunks_round_trip_and_respect_the_cap() {
        let mut w = PacketWriter::new(&header());
        assert!(w.push_reliable(5, b"hello"));
        assert!(w.push_unreliable(b""));
        assert!(w.push_unreliable(&[9; 100]));
        let big = vec![1u8; MAX_MESSAGE];
        assert!(!w.push_reliable(6, &big), "does not fit any more");
        assert_eq!(w.chunk_count(), 3);
        let dg = w.finish();
        assert!(dg.len() <= MAX_DATAGRAM);
        let (_, payload) = Header::read(&dg).unwrap();
        let got: Vec<_> = chunks(payload).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            got,
            vec![
                Chunk::Reliable {
                    id: 5,
                    body: b"hello"
                },
                Chunk::Unreliable { body: b"" },
                Chunk::Unreliable { body: &[9; 100] },
            ]
        );

        // A maximal message exactly fills an empty packet.
        let mut w = PacketWriter::new(&header());
        assert!(w.push_reliable(1, &big));
        assert_eq!(w.finish().len(), MAX_DATAGRAM);
    }

    #[test]
    fn malformed_chunks_are_errors_not_panics() {
        let cases: [&[u8]; 4] = [&[0x01, 1], &[0x01, 1, 0, 9, 0, 1], &[0x02, 5, 0], &[0x77]];
        for c in cases {
            let r: Result<Vec<_>, _> = chunks(c).collect();
            assert!(r.is_err(), "{c:?}");
        }
        assert_eq!(PunchBody::read(&[1, 2]), Err(PacketError::Truncated),);
        let p = PunchBody {
            assigned: 3,
            expected_nonce: 99,
        };
        let mut b = Vec::new();
        p.write(&mut b);
        assert_eq!(PunchBody::read(&b), Ok(p));
    }

    #[test]
    fn refused_body_round_trips_and_keeps_unknown_reasons() {
        for reason in [
            RefuseReason::Full,
            RefuseReason::RunInProgress,
            RefuseReason::VersionMismatch,
            RefuseReason::Other(200),
        ] {
            let body = RefusedBody {
                reason,
                expected_nonce: 0xABCD,
            };
            let mut b = Vec::new();
            body.write(&mut b);
            assert_eq!(b.len(), RefusedBody::LEN);
            assert_eq!(RefusedBody::read(&b), Ok(body));
        }
        assert_eq!(RefusedBody::read(&[1, 0]), Err(PacketError::Truncated));
        assert_eq!(PacketKind::from_byte(5), Some(PacketKind::Refused));
        assert_eq!(PacketKind::Refused.to_byte(), 5);
        assert!(RefuseReason::Full.to_string().contains("full"));
        assert_eq!(RefuseReason::RunInProgress.key(), "run_in_progress");
    }
}
