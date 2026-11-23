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

//! Packet acks, reliable-ordered delivery, RTT/jitter/loss estimation and
//! duplicate/stale rejection.
//!
//! * Each Data packet acks the newest seq plus the 32 before it
//!   ([`RecvWindow`]); a late packet within [`RECV_HISTORY`] is still taken.
//! * Reliable messages are resent after the timeout, or at once when their
//!   packet falls out of the ack window, and each rides once more in a later
//!   packet. The sender only sends ids less than [`RELIABLE_WINDOW`] past its
//!   oldest unacked one, so every id is inside the receiver's window; one
//!   beyond it is a broken stream ([`Received::BeyondWindow`]).
//! * Unreliable messages older than the newest delivered on their stream are
//!   dropped ([`LatestFilter`]).
//!
//! Nothing here reads a clock.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

/// Whether sequence number `a` is newer than `b`, with wrap-around.
#[must_use]
pub fn seq_newer(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) > 0
}

/// Signed distance `a - b` with wrap-around.
#[must_use]
pub fn seq_diff(a: u16, b: u16) -> i32 {
    i32::from(a.wrapping_sub(b) as i16)
}

/// Result of recording an incoming packet sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// First time seen; process it.
    New,
    /// First time seen, but behind the ack field: process it; it cannot be
    /// acked.
    Late,
    /// Already received; drop it.
    Duplicate,
    /// Older than [`RECV_HISTORY`]; drop it.
    Stale,
}

/// Packets remembered behind the newest one (older ones are stale). A
/// multiple of 64 dividing 65536, so the ring lines up with seq wrap-around.
pub const RECV_HISTORY: u16 = 1024;

/// Which incoming packets were received, for building acks.
#[derive(Debug, Clone, Default)]
pub struct RecvWindow {
    latest: Option<u16>,
    /// The ack field: bit i set = seq `latest - 1 - i` received.
    bits: u32,
    /// Seqs received within [`RECV_HISTORY`]: bit `seq % RECV_HISTORY`.
    seen: [u64; RECV_HISTORY as usize / 64],
}

impl RecvWindow {
    fn slot(seq: u16) -> (usize, u64) {
        let i = usize::from(seq % RECV_HISTORY);
        (i / 64, 1u64 << (i % 64))
    }

    fn mark(&mut self, seq: u16) {
        let (w, b) = Self::slot(seq);
        self.seen[w] |= b;
    }

    fn seen(&self, seq: u16) -> bool {
        let (w, b) = Self::slot(seq);
        self.seen[w] & b != 0
    }

    /// The newest seq received, once one has been.
    #[must_use]
    pub fn newest(&self) -> Option<u16> {
        self.latest
    }

    /// Record an arriving seq.
    pub fn record(&mut self, seq: u16) -> Arrival {
        let Some(latest) = self.latest else {
            self.latest = Some(seq);
            self.bits = 0;
            self.seen = Default::default();
            self.mark(seq);
            return Arrival::New;
        };
        let d = seq_diff(seq, latest);
        if d == 0 {
            return Arrival::Duplicate;
        }
        if d > 0 {
            let d = d as u32;
            self.bits =
                self.bits.checked_shl(d).unwrap_or(0) | 1u32.checked_shl(d - 1).unwrap_or(0);
            // Clear the ring slots of the seqs passed over.
            if d >= u32::from(RECV_HISTORY) {
                self.seen = Default::default();
            } else {
                for k in 1..=d as u16 {
                    let (w, b) = Self::slot(latest.wrapping_add(k));
                    self.seen[w] &= !b;
                }
            }
            self.latest = Some(seq);
            self.mark(seq);
            return Arrival::New;
        }
        let back = (-d) as u32;
        if back > 32 {
            if back >= u32::from(RECV_HISTORY) {
                return Arrival::Stale;
            }
            if self.seen(seq) {
                return Arrival::Duplicate;
            }
            self.mark(seq);
            return Arrival::Late;
        }
        let bit = 1u32 << (back - 1);
        if self.bits & bit != 0 {
            Arrival::Duplicate
        } else {
            self.bits |= bit;
            self.mark(seq);
            Arrival::New
        }
    }

    /// `(ack, ack_bits)` for the next header; `(0, 0)` acks nothing.
    #[must_use]
    pub fn ack(&self) -> (u16, u32) {
        (self.latest.unwrap_or(0), self.bits)
    }

    /// Whether anything has been received yet.
    #[must_use]
    pub fn has_received(&self) -> bool {
        self.latest.is_some()
    }
}

/// RFC 6298 smoothed RTT and variation.
#[derive(Debug, Clone, Copy)]
pub struct RttEstimator {
    srtt: f64,
    rttvar: f64,
    samples: u64,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self {
            srtt: 0.1,
            rttvar: 0.05,
            samples: 0,
        }
    }
}

/// Retransmission timeout bounds.
pub const MIN_RTO: Duration = Duration::from_millis(40);
/// See [`MIN_RTO`].
pub const MAX_RTO: Duration = Duration::from_secs(1);

impl RttEstimator {
    /// Feed one RTT sample.
    pub fn sample(&mut self, rtt: Duration) {
        let r = rtt.as_secs_f64();
        if self.samples == 0 {
            self.srtt = r;
            self.rttvar = r / 2.0;
        } else {
            self.rttvar = 0.75 * self.rttvar + 0.25 * (self.srtt - r).abs();
            self.srtt = 0.875 * self.srtt + 0.125 * r;
        }
        self.samples += 1;
    }

    /// Smoothed round-trip time.
    #[must_use]
    pub fn rtt(&self) -> Duration {
        Duration::from_secs_f64(self.srtt)
    }

    /// Round-trip variation (mean deviation), a jitter measure.
    #[must_use]
    pub fn rttvar(&self) -> Duration {
        Duration::from_secs_f64(self.rttvar)
    }

    /// Number of samples taken.
    #[must_use]
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Retransmission timeout: `srtt + 4 * rttvar`, clamped.
    #[must_use]
    pub fn rto(&self) -> Duration {
        Duration::from_secs_f64(self.srtt + 4.0 * self.rttvar).clamp(MIN_RTO, MAX_RTO)
    }
}

#[derive(Debug, Clone)]
struct SentPacket {
    seq: u16,
    time: Duration,
    reliable: Vec<u16>,
    acked: bool,
}

/// What an incoming ack resolved.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AckOutcome {
    /// Reliable message ids now known delivered.
    pub delivered: Vec<u16>,
    /// RTT sample from the newest acked packet, if it was newly acked.
    pub rtt: Option<Duration>,
    /// Packets newly acked.
    pub acked: u32,
    /// Packets given up on (fell out of the ack window unacked).
    pub lost: u32,
    /// The reliable ids they carried, with their packet's send time.
    pub lost_reliable: Vec<(u16, Duration)>,
}

/// Outgoing packet records, consecutive by seq, numbered from 1.
#[derive(Debug, Clone)]
pub struct SendWindow {
    next_seq: u16,
    sent: VecDeque<SentPacket>,
}

impl Default for SendWindow {
    fn default() -> Self {
        Self {
            next_seq: 1,
            sent: VecDeque::new(),
        }
    }
}

/// Unacked packets older than this are counted lost even without newer acks.
const PACKET_GIVE_UP: Duration = Duration::from_secs(2);

impl SendWindow {
    /// Allocate the next seq and remember what the packet carried.
    pub fn record(&mut self, now: Duration, reliable: Vec<u16>) -> u16 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.sent.push_back(SentPacket {
            seq,
            time: now,
            reliable,
            acked: false,
        });
        seq
    }

    /// Seq the next [`record`](Self::record) will return.
    #[must_use]
    pub fn peek_seq(&self) -> u16 {
        self.next_seq
    }

    fn get_mut(&mut self, seq: u16) -> Option<&mut SentPacket> {
        let front = self.sent.front()?.seq;
        let idx = seq.wrapping_sub(front) as usize;
        self.sent.get_mut(idx).filter(|p| p.seq == seq)
    }

    /// Apply an incoming `(ack, ack_bits)`; unknown seqs are ignored.
    pub fn on_ack(&mut self, now: Duration, ack: u16, bits: u32) -> AckOutcome {
        let mut out = AckOutcome::default();
        let mark = |w: &mut Self, seq: u16, newest: bool, out: &mut AckOutcome| {
            if let Some(p) = w.get_mut(seq)
                && !p.acked
            {
                p.acked = true;
                out.acked += 1;
                out.delivered.append(&mut p.reliable);
                if newest {
                    out.rtt = Some(now.saturating_sub(p.time));
                }
            }
        };
        // Only trust acks for seqs already sent.
        let trusted = seq_newer(self.next_seq, ack);
        if trusted {
            mark(self, ack, true, &mut out);
            for i in 0..32u16 {
                if bits & (1 << i) != 0 {
                    mark(self, ack.wrapping_sub(i + 1), false, &mut out);
                }
            }
        }
        // Drop resolved records from the front: acked, out of the window, or old.
        while let Some(front) = self.sent.front() {
            let out_of_window = trusted && seq_diff(ack, front.seq) > 32;
            let too_old = now.saturating_sub(front.time) > PACKET_GIVE_UP;
            if front.acked {
                self.sent.pop_front();
            } else if out_of_window || too_old {
                out.lost += 1;
                if let Some(p) = self.sent.pop_front() {
                    out.lost_reliable
                        .extend(p.reliable.into_iter().map(|id| (id, p.time)));
                }
            } else {
                break;
            }
        }
        out
    }

    /// Packets sent but not yet resolved.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.sent.iter().filter(|p| !p.acked).count()
    }
}

/// Reliable ids sent at most this far past the oldest unacked one; also the
/// receiver's out-of-order buffer.
pub const RELIABLE_WINDOW: u16 = 1024;

/// The most ids a sender's queue may span (below half the id space); at the
/// bound the endpoint resets the link rather than skip a message.
pub const MAX_UNACKED: u16 = 16384;

#[derive(Debug, Clone)]
struct Pending {
    id: u16,
    body: Vec<u8>,
    last_sent: Option<Duration>,
    /// Due now: the packet that last carried it was lost.
    resend_now: bool,
    /// When it first went out.
    first_sent: Option<Duration>,
    /// It went out a second time (a repeat or a resend): no repeat now.
    second_copy: bool,
}

/// Outgoing reliable messages awaiting acks.
#[derive(Debug, Clone, Default)]
pub struct ReliableSender {
    next_id: u16,
    queue: VecDeque<Pending>,
    resends: u64,
    fast_resends: u64,
    repeats: u64,
    repeat_bytes: u64,
    sent_bytes: u64,
}

impl ReliableSender {
    /// Queue an encoded message; returns its id.
    pub fn push(&mut self, body: Vec<u8>) -> u16 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.queue.push_back(Pending {
            id,
            body,
            last_sent: None,
            resend_now: false,
            first_sent: None,
            second_copy: false,
        });
        id
    }

    /// Queued messages within [`RELIABLE_WINDOW`] of the oldest (the queue
    /// is in id order).
    fn eligible(&self) -> impl Iterator<Item = &Pending> {
        let oldest = self.queue.front().map_or(0, |p| p.id);
        self.queue
            .iter()
            .take_while(move |p| p.id.wrapping_sub(oldest) < RELIABLE_WINDOW)
    }

    /// Whether the queue spans [`MAX_UNACKED`] ids.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.queue
            .front()
            .is_some_and(|p| self.next_id.wrapping_sub(p.id) >= MAX_UNACKED)
    }

    /// Messages due: never sent, unacked for `rto`, or marked by
    /// [`resend_now`](Self::resend_now).
    pub fn due(&self, now: Duration, rto: Duration) -> impl Iterator<Item = (u16, &[u8])> {
        self.eligible()
            .filter(move |p| {
                p.resend_now || p.last_sent.is_none_or(|t| now.saturating_sub(t) >= rto)
            })
            .map(|p| (p.id, p.body.as_slice()))
    }

    /// Record that message `id` went out at `now`.
    pub fn mark_sent(&mut self, id: u16, now: Duration) {
        if let Some(p) = self.queue.iter_mut().find(|p| p.id == id) {
            if p.last_sent.is_some() {
                self.resends += 1;
                p.second_copy = true;
                if p.resend_now {
                    self.fast_resends += 1;
                }
            } else {
                p.first_sent = Some(now);
            }
            self.sent_bytes += p.body.len() as u64;
            p.last_sent = Some(now);
            p.resend_now = false;
        }
    }

    /// Messages to repeat, oldest first: sent once, less than `window` ago,
    /// not acked, not due again.
    pub fn repeat_candidates(
        &self,
        now: Duration,
        window: Duration,
    ) -> impl Iterator<Item = (u16, &[u8])> {
        self.eligible()
            .filter(move |p| {
                !p.second_copy
                    && !p.resend_now
                    && p.first_sent.is_some_and(|t| now.saturating_sub(t) < window)
            })
            .map(|p| (p.id, p.body.as_slice()))
    }

    /// Record a repeat of `id` (its timeout still counts from the first send).
    pub fn mark_repeated(&mut self, id: u16) {
        if let Some(p) = self.queue.iter_mut().find(|p| p.id == id) {
            p.second_copy = true;
            self.repeats += 1;
            self.repeat_bytes += p.body.len() as u64;
            self.sent_bytes += p.body.len() as u64;
        }
    }

    /// These messages' packets were lost: each still waiting is due at once,
    /// if that packet was its latest send.
    pub fn resend_now(&mut self, lost: &[(u16, Duration)]) {
        for &(id, sent) in lost {
            if let Some(p) = self.queue.iter_mut().find(|p| p.id == id)
                && p.last_sent == Some(sent)
            {
                p.resend_now = true;
            }
        }
    }

    /// A sender whose next id is `id` (tests across the wrap).
    #[cfg(test)]
    fn starting_at(id: u16) -> Self {
        Self {
            next_id: id,
            ..Self::default()
        }
    }

    /// Forget messages the peer has received.
    pub fn delivered(&mut self, ids: &[u16]) {
        if ids.is_empty() {
            return;
        }
        self.queue.retain(|p| !ids.contains(&p.id));
    }

    /// Messages not yet acked.
    #[must_use]
    pub fn unacked(&self) -> usize {
        self.queue.len()
    }

    /// Total retransmissions.
    #[must_use]
    pub fn resends(&self) -> u64 {
        self.resends
    }

    /// Of those, the ones sent early by [`resend_now`](Self::resend_now).
    #[must_use]
    pub fn fast_resends(&self) -> u64 {
        self.fast_resends
    }

    /// Repeats sent ([`mark_repeated`](Self::mark_repeated)) and their bytes.
    #[must_use]
    pub fn repeats(&self) -> (u64, u64) {
        (self.repeats, self.repeat_bytes)
    }

    /// Reliable message bytes sent: first sends, resends and repeats.
    #[must_use]
    pub fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }
}

/// What [`ReliableReceiver::receive`] did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// Taken (buffered until the ones before it are in).
    New,
    /// Already delivered or buffered; dropped.
    Duplicate,
    /// [`RELIABLE_WINDOW`] or more ahead: not taken, not acked, the stream
    /// is broken.
    BeyondWindow,
}

/// Incoming reliable messages, released in order exactly once.
#[derive(Debug, Clone, Default)]
pub struct ReliableReceiver {
    next: u16,
    pending: BTreeMap<u16, Vec<u8>>, // keyed by distance from `next`
}

impl ReliableReceiver {
    /// Accept message `id` (behind `next` by up to half the id space: already
    /// delivered).
    pub fn receive(&mut self, id: u16, body: &[u8]) -> Received {
        let dist = id.wrapping_sub(self.next);
        if (dist as i16) < 0 {
            return Received::Duplicate;
        }
        if dist >= RELIABLE_WINDOW {
            return Received::BeyondWindow;
        }
        if self.pending.contains_key(&dist) {
            return Received::Duplicate;
        }
        self.pending.insert(dist, body.to_vec());
        Received::New
    }

    /// The next id it will release.
    #[must_use]
    pub fn next_id(&self) -> u16 {
        self.next
    }

    /// A receiver expecting `id` next (tests across the wrap).
    #[cfg(test)]
    fn starting_at(id: u16) -> Self {
        Self {
            next: id,
            ..Self::default()
        }
    }

    /// Next in-order message, if it has arrived.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let body = self.pending.remove(&0)?;
        self.next = self.next.wrapping_add(1);
        // Re-key the rest one step closer.
        let rest = std::mem::take(&mut self.pending);
        self.pending = rest.into_iter().map(|(d, b)| (d - 1, b)).collect();
        Some(body)
    }

    /// Messages buffered out of order.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }
}

/// Drops unreliable messages older than the newest one already delivered on
/// the same stream.
#[derive(Debug, Clone, Default)]
pub struct LatestFilter {
    last: HashMap<u16, u16>,
}

impl LatestFilter {
    /// Whether a message on `stream` from packet `seq` is fresh (several in
    /// one packet all are). A stream whose last packet is more than
    /// [`RECV_HISTORY`] behind `newest` was idle, so its next message is taken.
    pub fn accept(&mut self, stream: u16, seq: u16, newest: u16) -> bool {
        match self.last.get(&stream) {
            Some(&last) if newest.wrapping_sub(last) <= RECV_HISTORY && seq_newer(last, seq) => {
                false
            }
            _ => {
                self.last.insert(stream, seq);
                true
            }
        }
    }
}

/// Exponentially weighted packet-loss estimate.
#[derive(Debug, Clone, Copy, Default)]
pub struct LossEstimator {
    loss: f64,
    acked: u64,
    lost: u64,
}

impl LossEstimator {
    const GAIN: f64 = 1.0 / 64.0;

    /// Record resolved packets.
    pub fn record(&mut self, acked: u32, lost: u32) {
        for _ in 0..acked {
            self.loss *= 1.0 - Self::GAIN;
        }
        for _ in 0..lost {
            self.loss = self.loss * (1.0 - Self::GAIN) + Self::GAIN;
        }
        self.acked += u64::from(acked);
        self.lost += u64::from(lost);
    }

    /// Recent loss fraction in `[0, 1]`.
    #[must_use]
    pub fn loss(&self) -> f64 {
        self.loss
    }

    /// Lifetime totals `(acked, lost)`.
    #[must_use]
    pub fn totals(&self) -> (u64, u64) {
        (self.acked, self.lost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn sequence_comparison_wraps() {
        assert!(seq_newer(1, 0));
        assert!(seq_newer(0, 65535));
        assert!(!seq_newer(65535, 0));
        assert!(!seq_newer(5, 5));
        assert_eq!(seq_diff(2, 65534), 4);
        assert_eq!(seq_diff(65534, 2), -4);
    }

    #[test]
    fn recv_window_detects_duplicates_and_stale() {
        let mut w = RecvWindow::default();
        assert_eq!(w.record(65530), Arrival::New);
        assert_eq!(w.record(65530), Arrival::Duplicate);
        assert_eq!(w.record(2), Arrival::New); // wrapped forward by 8
        assert_eq!(w.ack(), (2, 1 << 7));
        assert_eq!(w.record(65535), Arrival::New); // late but in window
        assert_eq!(w.record(65535), Arrival::Duplicate);
        assert_eq!(w.ack(), (2, (1 << 7) | (1 << 2)));
        assert_eq!(w.record(40), Arrival::New);
        assert_eq!(w.record(5), Arrival::Late); // 35 behind: taken, not ackable
        assert_eq!(w.record(5), Arrival::Duplicate);
        assert_eq!(w.ack().1 >> 31, 0, "a late packet is not in the ack field");
        assert_eq!(w.record(8), Arrival::New); // 32 behind: last bit
        assert_eq!(w.ack().1 >> 31, 1);
        assert_eq!(w.record(200), Arrival::New); // far jump clears the bits
        assert_eq!(w.ack(), (200, 0));
        assert_eq!(
            w.record(40),
            Arrival::Duplicate,
            "remembered in the history"
        );
        assert_eq!(w.record(39), Arrival::Late);
        // RECV_HISTORY behind or more: stale.
        assert_eq!(w.record(200u16.wrapping_sub(RECV_HISTORY)), Arrival::Stale);
        assert_eq!(
            w.record(200u16.wrapping_sub(RECV_HISTORY - 1)),
            Arrival::Late
        );
    }

    /// The history ring across wrap-around and long jumps.
    #[test]
    fn recv_window_history_survives_wrap_and_jumps() {
        let mut w = RecvWindow::default();
        let mut rng = Rng::new(9);
        let mut latest = 65000u16;
        assert_eq!(w.record(latest), Arrival::New);
        let mut got = std::collections::HashSet::new();
        got.insert(latest);
        for _ in 0..20_000 {
            // Mostly forward by 1-3, sometimes back within the history.
            let seq = if rng.chance(0.7) {
                latest = latest.wrapping_add(1 + (rng.range_f64(0.0, 3.0) as u16));
                latest
            } else {
                latest.wrapping_sub(rng.range_f64(1.0, f64::from(RECV_HISTORY - 1)) as u16)
            };
            let first = got.insert(seq);
            let a = w.record(seq);
            if first {
                assert!(matches!(a, Arrival::New | Arrival::Late), "{seq}: {a:?}");
            } else {
                assert_eq!(a, Arrival::Duplicate, "{seq}");
            }
            // Forget what fell out of the history (a later lap may reuse it).
            got.retain(|&g| seq_diff(latest, g) < i32::from(RECV_HISTORY) - 1);
        }
        // A jump past the whole history forgets it.
        let far = latest.wrapping_add(RECV_HISTORY + 5);
        assert_eq!(w.record(far), Arrival::New);
        assert_eq!(w.record(far.wrapping_sub(RECV_HISTORY)), Arrival::Stale);
        assert_eq!(w.record(far.wrapping_sub(40)), Arrival::Late);
    }

    #[test]
    fn send_window_acks_measure_rtt_and_count_losses() {
        let mut s = SendWindow::default();
        let ms = Duration::from_millis;
        assert_eq!(s.peek_seq(), 1, "numbering starts at 1");
        assert_eq!(s.on_ack(ms(0), 0, 0).acked, 0, "(0, 0) acks nothing");
        for i in 0..40u64 {
            // seq i + 1 carries reliable id i, sent at i * 10 ms.
            s.record(ms(i * 10), vec![i as u16]);
        }
        // Peer got seqs 39 and 37, nothing else.
        let out = s.on_ack(ms(450), 39, 0b10);
        assert_eq!(out.rtt, Some(ms(70)));
        assert_eq!(out.acked, 2);
        assert_eq!(out.delivered, vec![38, 36]);
        // Seqs 1..=6 are more than 32 behind 39: lost.
        assert_eq!(out.lost, 6);
        // Re-acking changes nothing; acks for unsent seqs are ignored.
        let again = s.on_ack(ms(460), 39, 0b10);
        assert_eq!((again.acked, again.rtt), (0, None));
        assert_eq!(s.on_ack(ms(470), 1000, u32::MAX).acked, 0);
        // Everything else times out eventually.
        let late = s.on_ack(ms(5000), 39, 0);
        assert_eq!(late.lost, 32);
        assert_eq!(s.in_flight(), 0);
    }

    /// A lost packet's messages are resent at once where it was their latest
    /// send.
    #[test]
    fn lost_packets_make_their_messages_due_at_once() {
        let ms = Duration::from_millis;
        let mut w = SendWindow::default();
        let mut r = ReliableSender::default();
        for i in 0..40u16 {
            let id = r.push(vec![i as u8]);
            r.mark_sent(id, ms(u64::from(i)));
            w.record(ms(u64::from(i)), vec![id]);
        }
        // Seq s (1..=40) carries message s - 1. The first ack (newest 33)
        // names every seq from 1 to 32 but 4 and 5 (messages 3 and 4): bit
        // i is seq 32 - i.
        let mut bits = 0u32;
        for seq in 1..=32u16 {
            if seq != 4 && seq != 5 {
                bits |= 1 << (32 - seq);
            }
        }
        let out = w.on_ack(ms(45), 33, bits);
        assert_eq!(
            (out.lost, out.lost_reliable.len()),
            (0, 0),
            "4 and 5 still in the window"
        );
        r.delivered(&out.delivered);
        // Message 3 went out again at 50 ms (seq 41, not acked yet).
        r.mark_sent(3, ms(50));
        w.record(ms(50), vec![3]);
        // The next ack (newest 40, every seq 8..=39 named) leaves seqs 4
        // and 5 more than 32 behind, unacked: lost.
        let out = w.on_ack(ms(60), 40, u32::MAX);
        assert_eq!(out.lost, 2);
        assert_eq!(out.lost_reliable, vec![(3, ms(3)), (4, ms(4))]);
        r.delivered(&out.delivered);
        r.resend_now(&out.lost_reliable);
        let rto = ms(1000);
        let due: Vec<u16> = r.due(ms(61), rto).map(|(id, _)| id).collect();
        // Message 3's latest copy is still on its way; message 4 is due now.
        assert_eq!(due, vec![4]);
        r.mark_sent(4, ms(61));
        assert_eq!((r.resends(), r.fast_resends()), (2, 1));
        assert_eq!(r.due(ms(62), rto).count(), 0);
        assert_eq!(r.unacked(), 2, "messages 3 and 4 wait for their acks");
    }

    /// A message is a repeat candidate once, within the window.
    #[test]
    fn repeat_candidates_ride_once() {
        let ms = Duration::from_millis;
        let mut sender = ReliableSender::default();
        let first = sender.push(vec![1; 10]);
        let second = sender.push(vec![2; 20]);
        let third = sender.push(vec![3; 30]);
        assert_eq!(
            sender.repeat_candidates(ms(0), ms(50)).count(),
            0,
            "nothing sent yet"
        );
        sender.mark_sent(first, ms(0));
        sender.mark_sent(second, ms(0));
        sender.mark_sent(third, ms(0));
        let ids: Vec<u16> = sender
            .repeat_candidates(ms(16), ms(50))
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![first, second, third], "oldest first");
        sender.mark_repeated(first);
        sender.mark_sent(second, ms(20)); // a resend is its second copy
        sender.delivered(&[third]);
        assert_eq!(sender.repeat_candidates(ms(20), ms(50)).count(), 0);
        assert_eq!(sender.repeats(), (1, 10));
        assert_eq!(sender.sent_bytes(), 10 + 20 + 30 + 10 + 20);
        let fourth = sender.push(vec![4; 5]);
        sender.mark_sent(fourth, ms(30));
        assert_eq!(sender.repeat_candidates(ms(79), ms(50)).count(), 1);
        assert_eq!(
            sender.repeat_candidates(ms(80), ms(50)).count(),
            0,
            "past the window"
        );
    }

    #[test]
    fn rtt_estimator_converges_and_bounds_rto() {
        let mut r = RttEstimator::default();
        assert_eq!(r.samples(), 0);
        let mut rng = Rng::new(3);
        for _ in 0..500 {
            r.sample(Duration::from_secs_f64(0.08 + rng.range_f64(-0.01, 0.01)));
        }
        assert!(
            (r.rtt().as_secs_f64() - 0.08).abs() < 0.005,
            "{:?}",
            r.rtt()
        );
        assert!(r.rttvar() < Duration::from_millis(10));
        assert!(r.rto() >= MIN_RTO && r.rto() <= MAX_RTO);
        let mut slow = RttEstimator::default();
        slow.sample(Duration::from_secs(5));
        assert_eq!(slow.rto(), MAX_RTO);
    }

    #[test]
    fn reliable_receiver_orders_and_dedups() {
        let mut r = ReliableReceiver::default();
        assert_eq!(r.receive(2, b"c"), Received::New);
        assert_eq!(r.receive(1, b"b"), Received::New);
        assert_eq!(r.receive(1, b"b"), Received::Duplicate, "buffered");
        assert_eq!(r.pop(), None, "0 missing");
        assert_eq!(r.receive(0, b"a"), Received::New);
        let got: Vec<_> = std::iter::from_fn(|| r.pop()).collect();
        assert_eq!(got, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        assert_eq!(r.receive(1, b"b"), Received::Duplicate, "already delivered");
        assert_eq!(
            r.receive(3 + RELIABLE_WINDOW, b"x"),
            Received::BeyondWindow,
            "too far ahead"
        );
        assert_eq!(
            r.receive(3 + RELIABLE_WINDOW - 1, b"y"),
            Received::New,
            "the window's last id"
        );
        assert_eq!(
            r.receive(3u16.wrapping_sub(1000), b"z"),
            Received::Duplicate,
            "far behind is delivered"
        );
        assert_eq!(r.buffered(), 1, "only the window's last id is buffered");
    }

    #[test]
    fn reliable_sender_resends_after_rto() {
        let mut s = ReliableSender::default();
        let ms = Duration::from_millis;
        let a = s.push(b"a".to_vec());
        let b = s.push(b"b".to_vec());
        let due: Vec<_> = s.due(ms(0), ms(100)).map(|(id, _)| id).collect();
        assert_eq!(due, vec![a, b]);
        s.mark_sent(a, ms(0));
        s.mark_sent(b, ms(0));
        assert_eq!(s.due(ms(50), ms(100)).count(), 0);
        s.delivered(&[a]);
        let due: Vec<_> = s.due(ms(100), ms(100)).map(|(id, _)| id).collect();
        assert_eq!(due, vec![b]);
        s.mark_sent(b, ms(100));
        assert_eq!(s.resends(), 1);
        assert_eq!(s.unacked(), 1);
    }

    #[test]
    fn latest_filter_drops_older_packets_per_stream() {
        let mut f = LatestFilter::default();
        assert!(f.accept(1, 10, 10));
        assert!(f.accept(1, 10, 10), "same packet");
        assert!(!f.accept(1, 9, 10));
        assert!(f.accept(2, 9, 10), "other stream");
        assert!(f.accept(1, 11, 11));
    }

    /// An idle stream takes its next message; within the history an older
    /// arrival is still dropped.
    #[test]
    fn a_stream_idle_past_half_the_seq_space_is_taken_again() {
        for idle in [1_100u16, 32_767, 32_768, 40_000, 65_000, 65_535] {
            let mut f = LatestFilter::default();
            assert!(f.accept(0x201, 1000, 1000));
            let now = 1000u16.wrapping_add(idle);
            assert!(f.accept(0x201, now, now), "idle {idle} packets");
            assert!(f.accept(0x201, now.wrapping_add(1), now.wrapping_add(1)));
        }
        let mut f = LatestFilter::default();
        assert!(f.accept(7, 5_000, 5_000));
        assert!(
            !f.accept(7, 4_990, 5_500),
            "late, within the history: older"
        );
    }

    #[test]
    fn loss_estimator_tracks_rate() {
        let mut l = LossEstimator::default();
        for i in 0..2000 {
            if i % 10 == 0 {
                l.record(0, 1);
            } else {
                l.record(1, 0);
            }
        }
        assert!((l.loss() - 0.1).abs() < 0.05, "{}", l.loss());
        assert_eq!(l.totals(), (1800, 200));
    }

    /// Run a sender and receiver for `rounds`, losing copies per `lost`;
    /// returns the bodies released, in order.
    fn lane_rounds(
        s: &mut ReliableSender,
        r: &mut ReliableReceiver,
        rounds: std::ops::Range<u64>,
        produce: &mut dyn FnMut(u64, &mut ReliableSender),
        lost: &dyn Fn(u64, u16) -> bool,
        oldest_at_start: u16,
    ) -> Vec<Vec<u8>> {
        let rto = Duration::from_millis(100);
        let mut out = Vec::new();
        for round in rounds {
            let now = Duration::from_millis(round * 10);
            produce(round, s);
            let oldest = s.queue.front().map_or(oldest_at_start, |p| p.id);
            let due: Vec<(u16, Vec<u8>)> =
                s.due(now, rto).map(|(id, b)| (id, b.to_vec())).collect();
            let mut acked = Vec::new();
            for (id, body) in due {
                assert!(
                    id.wrapping_sub(oldest) < RELIABLE_WINDOW,
                    "id {id} due with the oldest unacked at {oldest}"
                );
                s.mark_sent(id, now);
                if lost(round, id) {
                    continue;
                }
                let next = r.next_id();
                assert_ne!(
                    r.receive(id, &body),
                    Received::BeyondWindow,
                    "id {id} beyond the receiver's window at {next}"
                );
                acked.push(id);
            }
            s.delivered(&acked);
            while let Some(b) = r.pop() {
                out.push(b);
            }
        }
        out
    }

    fn numbered(n: u32) -> Vec<u8> {
        n.to_le_bytes().to_vec()
    }

    /// The oldest message lost for a long time: nothing past the window is
    /// due, and everything is released once and in order afterwards.
    #[test]
    fn a_lost_oldest_never_lets_ids_past_the_window_out() {
        for start in [0u16, 65_000] {
            let mut s = ReliableSender::starting_at(start);
            let mut r = ReliableReceiver::starting_at(start);
            let mut made = 0u32;
            let mut produce = |round: u64, s: &mut ReliableSender| {
                // 1,100 at once, then 10 a round (100 a second) to 3,000.
                let n = if round == 0 { 1100 } else { 10 };
                for _ in 0..n {
                    if made < 3000 {
                        s.push(numbered(made));
                        made += 1;
                    }
                }
            };
            // The oldest's every copy lost for 20 s (2,000 rounds).
            let lost = |round: u64, id: u16| id == start && round < 2000;
            let got = lane_rounds(&mut s, &mut r, 0..2000, &mut produce, &lost, start);
            assert!(got.is_empty(), "nothing released behind the hole");
            assert!(s.unacked() >= 3000 - 1023, "the rest wait: {}", s.unacked());
            assert_eq!(
                r.buffered(),
                usize::from(RELIABLE_WINDOW) - 1,
                "the window is full"
            );
            let got = lane_rounds(&mut s, &mut r, 2000..4000, &mut produce, &lost, start);
            assert_eq!(got.len(), 3000, "every message released once");
            assert!(
                got.iter()
                    .enumerate()
                    .all(|(i, b)| *b == numbered(i as u32)),
                "in order"
            );
            assert_eq!(s.unacked(), 0);
            assert_eq!(r.next_id(), start.wrapping_add(3000));
        }
    }

    /// Ids across 65535: the window, the order and the duplicates hold.
    #[test]
    fn the_lane_holds_across_the_id_wrap() {
        let mut s = ReliableSender::starting_at(65_500);
        let mut r = ReliableReceiver::starting_at(65_500);
        let mut made = 0u32;
        let mut produce = |_round: u64, s: &mut ReliableSender| {
            for _ in 0..7 {
                if made < 2000 {
                    s.push(numbered(made));
                    made += 1;
                }
            }
        };
        // A third of the copies lost, by id and round.
        let lost = |round: u64, id: u16| (u64::from(id) * 7 + round).is_multiple_of(3);
        let got = lane_rounds(&mut s, &mut r, 0..3000, &mut produce, &lost, 65_500);
        assert_eq!(got.len(), 2000);
        assert!(
            got.iter()
                .enumerate()
                .all(|(i, b)| *b == numbered(i as u32))
        );
        assert_eq!(r.next_id(), 65_500u16.wrapping_add(2000));
        assert_eq!(
            r.receive(65_535, b"old"),
            Received::Duplicate,
            "delivered before the wrap"
        );
    }

    /// The queue never spans `MAX_UNACKED` ids: full before that.
    #[test]
    fn the_sender_is_full_at_max_unacked() {
        let mut s = ReliableSender::starting_at(60_000);
        for i in 0..u32::from(MAX_UNACKED) - 1 {
            assert!(!s.is_full(), "at {i}");
            s.push(Vec::new());
        }
        assert!(!s.is_full());
        s.push(Vec::new());
        assert!(s.is_full(), "MAX_UNACKED ids queued");
        s.delivered(&[60_000]);
        assert!(!s.is_full(), "the oldest acked: room again");
    }
}
