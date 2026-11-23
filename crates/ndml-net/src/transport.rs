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

//! Datagram transports behind one trait: real UDP and an in-memory loopback.
//!
//! Non-blocking; both methods take the current time so wrappers can delay
//! delivery without reading a clock.

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// A non-blocking datagram socket.
pub trait Transport {
    /// Send one datagram. Transient failures are reported but callers should
    /// treat them as packet loss.
    fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()>;

    /// Receive one datagram into `buf`, or `Ok(None)` if none is waiting.
    fn recv_from(
        &mut self,
        now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>>;

    /// The bound local address.
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Send at once whatever is held for later delivery (the game is closing).
    fn release_held(&mut self, _now: Duration) -> io::Result<()> {
        Ok(())
    }
}

/// A boxed transport is a transport (backend picked at run time).
impl<T: Transport + ?Sized> Transport for Box<T> {
    fn send_to(&mut self, now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        (**self).send_to(now, to, data)
    }

    fn recv_from(
        &mut self,
        now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        (**self).recv_from(now, buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        (**self).local_addr()
    }

    fn release_held(&mut self, now: Duration) -> io::Result<()> {
        (**self).release_held(now)
    }
}

/// A real UDP socket in non-blocking mode.
#[derive(Debug)]
pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Bind `addr` (e.g. `0.0.0.0:7777`, or port 0 for any) and go non-blocking.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        Self::from_socket(UdpSocket::bind(addr)?)
    }

    /// Wrap an existing socket (for example one already used for STUN).
    pub fn from_socket(socket: UdpSocket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self { socket })
    }

    /// The underlying socket.
    #[must_use]
    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }
}

impl Transport for UdpTransport {
    fn send_to(&mut self, _now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        match self.socket.send_to(data, to) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()), // dropped, like loss
            Err(e) => Err(e),
        }
    }

    fn recv_from(
        &mut self,
        _now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            match self.socket.recv_from(buf) {
                Ok((n, from)) => return Ok(Some((n, from))),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                // Windows reports an earlier send's ICMP "port unreachable" as
                // WSAECONNRESET here (normal while hole punching): skip it.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

#[derive(Debug, Default)]
struct LoopbackInner {
    queues: HashMap<SocketAddr, VecDeque<(SocketAddr, Vec<u8>)>>,
}

/// An in-memory network: bound sockets deliver instantly and in order.
/// Cheap to clone (shared state).
#[derive(Debug, Clone, Default)]
pub struct LoopbackNet {
    inner: Arc<Mutex<LoopbackInner>>,
}

impl LoopbackNet {
    /// An empty network.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a socket at `addr`. Fails if the address is taken.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<LoopbackSocket> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.queues.contains_key(&addr) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("loopback address {addr} in use"),
            ));
        }
        inner.queues.insert(addr, VecDeque::new());
        Ok(LoopbackSocket {
            net: self.clone(),
            addr,
        })
    }
}

/// One socket on a [`LoopbackNet`]. Unbinds on drop.
#[derive(Debug)]
pub struct LoopbackSocket {
    net: LoopbackNet,
    addr: SocketAddr,
}

impl Transport for LoopbackSocket {
    fn send_to(&mut self, _now: Duration, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        let mut inner = self
            .net
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Sending to an unbound address is silently lost, like UDP.
        if let Some(q) = inner.queues.get_mut(&to) {
            q.push_back((self.addr, data.to_vec()));
        }
        Ok(())
    }

    fn recv_from(
        &mut self,
        _now: Duration,
        buf: &mut [u8],
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        let mut inner = self
            .net
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some((from, data)) = inner
            .queues
            .get_mut(&self.addr)
            .and_then(VecDeque::pop_front)
        else {
            return Ok(None);
        };
        // Like UDP, a datagram larger than the buffer is truncated.
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(Some((n, from)))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

impl Drop for LoopbackSocket {
    fn drop(&mut self) {
        let mut inner = self
            .net
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        inner.queues.remove(&self.addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    #[test]
    fn loopback_delivers_in_order_and_unbinds() {
        let net = LoopbackNet::new();
        let mut a = net.bind(addr(1)).unwrap();
        let mut b = net.bind(addr(2)).unwrap();
        assert!(net.bind(addr(1)).is_err());
        let t = Duration::ZERO;
        a.send_to(t, addr(2), b"one").unwrap();
        a.send_to(t, addr(2), b"two").unwrap();
        a.send_to(t, addr(9), b"lost").unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = b.recv_from(t, &mut buf).unwrap().unwrap();
        assert_eq!((&buf[..n], from), (&b"one"[..], addr(1)));
        let (n, _) = b.recv_from(t, &mut buf).unwrap().unwrap();
        assert_eq!(&buf[..n], b"two");
        assert!(b.recv_from(t, &mut buf).unwrap().is_none());
        drop(b);
        assert!(net.bind(addr(2)).is_ok());
    }

    #[test]
    fn udp_round_trip_on_localhost() {
        let any = SocketAddr::from(([127, 0, 0, 1], 0));
        let mut a = UdpTransport::bind(any).unwrap();
        let mut b = UdpTransport::bind(any).unwrap();
        let t = Duration::ZERO;
        let to = b.local_addr().unwrap();
        a.send_to(t, to, b"ping").unwrap();
        let mut buf = [0u8; 16];
        let mut got = None;
        for _ in 0..200 {
            if let Some(r) = b.recv_from(t, &mut buf).unwrap() {
                got = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let (n, from) = got.expect("datagram arrives on localhost");
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(from, a.local_addr().unwrap());
    }
}
