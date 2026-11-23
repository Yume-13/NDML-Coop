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

//! File work on a thread of its own, so a thread that keeps a pace never
//! waits on a disk (a disk can stall writes for seconds).
//!
//! * [`FileService::write`] never waits; the newest offer per path wins.
//! * [`FileService::watch`] reads a file every period; [`FileService::take`]
//!   hands over the newest read.
//! * Dropping the service writes what was offered, waiting at most
//!   [`STOP_WAIT`]. [`FileService::inline`] works on the caller's thread.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long dropping a threaded service waits for its last writes.
pub const STOP_WAIT: Duration = Duration::from_secs(2);

/// The file calls the service makes (tests put in one that sticks).
pub trait FileOps: Send + Sync + 'static {
    /// Replace `path` with `bytes`.
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// The text of `path`.
    fn read(&self, path: &Path) -> io::Result<String>;
}

/// The real file system (atomic replace, plain read).
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFiles;

impl FileOps for RealFiles {
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        crate::lobby::files::write_atomic(path, bytes)
    }
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
}

/// A read of a watched file: its text, or the error's kind and text.
pub type ReadResult = Result<String, (io::ErrorKind, String)>;

/// A watched file ([`FileService::watch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchId(usize);

#[derive(Debug)]
struct Watch {
    path: PathBuf,
    every: Duration,
    next: Instant,
    latest: Option<ReadResult>,
}

#[derive(Debug, Default)]
struct State {
    /// The newest content offered per path, not written yet.
    writes: BTreeMap<PathBuf, Vec<u8>>,
    /// The last write's error per path (`None`: it worked).
    errors: BTreeMap<PathBuf, Option<String>>,
    watches: Vec<Watch>,
    stop: bool,
    done: bool,
    /// Offers that replaced one still waiting (latest wins).
    replaced: u64,
}

struct Inner {
    state: Mutex<State>,
    wake: Condvar,
    ops: Arc<dyn FileOps>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// See the module docs.
pub struct FileService {
    inner: Arc<Inner>,
    threaded: bool,
    stop_wait: Duration,
}

impl std::fmt::Debug for FileService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileService")
            .field("threaded", &self.threaded)
            .finish_non_exhaustive()
    }
}

impl FileService {
    /// A service on its own thread, named `name`, on the real file system.
    pub fn spawn(name: &str) -> io::Result<Self> {
        Self::spawn_with(name, Arc::new(RealFiles), STOP_WAIT)
    }

    /// A service on its own thread with `ops`; dropping it waits at most
    /// `stop_wait` for the last writes.
    pub fn spawn_with(name: &str, ops: Arc<dyn FileOps>, stop_wait: Duration) -> io::Result<Self> {
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            ops,
        });
        let thread_inner = Arc::clone(&inner);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || run(&thread_inner))?;
        Ok(Self {
            inner,
            threaded: true,
            stop_wait,
        })
    }

    /// The same work on the caller's thread, at each call.
    #[must_use]
    pub fn inline() -> Self {
        Self::inline_with(Arc::new(RealFiles))
    }

    /// [`Self::inline`] with `ops`.
    #[must_use]
    pub fn inline_with(ops: Arc<dyn FileOps>) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
                ops,
            }),
            threaded: false,
            stop_wait: Duration::ZERO,
        }
    }

    /// Whether the work runs on a thread of its own.
    #[must_use]
    pub fn threaded(&self) -> bool {
        self.threaded
    }

    /// Replace `path` with `bytes`. Never waits on the file.
    pub fn write(&self, path: &Path, bytes: Vec<u8>) {
        if !self.threaded {
            let r = self.inner.ops.write(path, &bytes);
            self.inner
                .lock()
                .errors
                .insert(path.to_path_buf(), r.err().map(|e| e.to_string()));
            return;
        }
        let mut st = self.inner.lock();
        if st.writes.insert(path.to_path_buf(), bytes).is_some() {
            st.replaced += 1;
        }
        drop(st);
        self.inner.wake.notify_one();
    }

    /// The last finished write of `path` failed: its error.
    #[must_use]
    pub fn write_error(&self, path: &Path) -> Option<String> {
        self.inner.lock().errors.get(path).cloned().flatten()
    }

    /// Offers that replaced one still waiting, so far.
    #[must_use]
    pub fn replaced(&self) -> u64 {
        self.inner.lock().replaced
    }

    /// Read `path` every `every` (the first read at once). Inline: read at
    /// each [`Self::take`].
    #[must_use]
    pub fn watch(&self, path: &Path, every: Duration) -> WatchId {
        let mut st = self.inner.lock();
        st.watches.push(Watch {
            path: path.to_path_buf(),
            every,
            next: Instant::now(),
            latest: None,
        });
        let id = WatchId(st.watches.len() - 1);
        drop(st);
        self.inner.wake.notify_one();
        id
    }

    /// The newest read of `id` since the last take, or `None` (none yet).
    /// Inline: read now.
    #[must_use]
    pub fn take(&self, id: WatchId) -> Option<ReadResult> {
        if !self.threaded {
            let path = self.inner.lock().watches.get(id.0)?.path.clone();
            return Some(
                self.inner
                    .ops
                    .read(&path)
                    .map_err(|e| (e.kind(), e.to_string())),
            );
        }
        self.inner.lock().watches.get_mut(id.0)?.latest.take()
    }
}

impl Drop for FileService {
    fn drop(&mut self) {
        if !self.threaded {
            return;
        }
        let deadline = Instant::now() + self.stop_wait;
        let mut st = self.inner.lock();
        st.stop = true;
        self.inner.wake.notify_all();
        while !st.done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // The disk does not answer: the thread finishes on its own.
                break;
            }
            st = self
                .inner
                .wake
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// The service's thread: writes what was offered, reads what is due.
fn run(inner: &Inner) {
    loop {
        let mut st = inner.lock();
        loop {
            if st.stop || !st.writes.is_empty() {
                break;
            }
            let now = Instant::now();
            match st.watches.iter().map(|w| w.next).min() {
                Some(t) if t <= now => break,
                Some(t) => {
                    st = inner
                        .wake
                        .wait_timeout(st, t - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                None => st = inner.wake.wait(st).unwrap_or_else(PoisonError::into_inner),
            }
        }
        let stop = st.stop;
        let writes = std::mem::take(&mut st.writes);
        let now = Instant::now();
        let reads: Vec<(usize, PathBuf)> = if stop {
            Vec::new()
        } else {
            st.watches
                .iter_mut()
                .enumerate()
                .filter(|(_, w)| w.next <= now)
                .map(|(i, w)| {
                    w.next = now + w.every;
                    (i, w.path.clone())
                })
                .collect()
        };
        drop(st);
        for (path, bytes) in writes {
            let r = inner.ops.write(&path, &bytes);
            inner
                .lock()
                .errors
                .insert(path, r.err().map(|e| e.to_string()));
        }
        for (i, path) in reads {
            let r = inner.ops.read(&path).map_err(|e| (e.kind(), e.to_string()));
            if let Some(w) = inner.lock().watches.get_mut(i) {
                w.latest = Some(r);
            }
        }
        if stop {
            let mut st = inner.lock();
            if st.writes.is_empty() {
                st.done = true;
                drop(st);
                inner.wake.notify_all();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Files in memory; a write of `stuck_on` waits for a go on `gate`.
    struct Mem {
        files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
        log: Mutex<Vec<Vec<u8>>>,
        stuck_on: Option<Vec<u8>>,
        gate: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl Mem {
        fn new(stuck_on: Option<&[u8]>) -> (Arc<Self>, mpsc::Sender<()>) {
            let (tx, rx) = mpsc::channel();
            (
                Arc::new(Self {
                    files: Mutex::new(BTreeMap::new()),
                    log: Mutex::new(Vec::new()),
                    stuck_on: stuck_on.map(<[u8]>::to_vec),
                    gate: Mutex::new(Some(rx)),
                }),
                tx,
            )
        }
    }

    impl FileOps for Mem {
        fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if self.stuck_on.as_deref() == Some(bytes) {
                // The gate's lock is not held across the wait.
                let rx = self.gate.lock().unwrap().take();
                if let Some(rx) = rx {
                    let _ = rx.recv();
                }
            }
            self.log.lock().unwrap().push(bytes.to_vec());
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), bytes.to_vec());
            Ok(())
        }
        fn read(&self, path: &Path) -> io::Result<String> {
            self.files.lock().unwrap().get(path).map_or_else(
                || Err(io::Error::from(io::ErrorKind::NotFound)),
                |b| Ok(String::from_utf8_lossy(b).into_owned()),
            )
        }
    }

    fn until(what: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// A write stuck on the disk never holds the caller, and of what was
    /// offered meanwhile only the newest is written.
    #[test]
    fn a_stuck_write_never_holds_the_caller_and_the_newest_wins() {
        let (mem, go) = Mem::new(Some(b"0"));
        let s = FileService::spawn_with("t-files", mem.clone(), STOP_WAIT).unwrap();
        let p = Path::new("status.toml");
        s.write(p, b"0".to_vec());
        assert!(
            until(|| mem.gate.lock().unwrap().is_none()),
            "the thread is in the stuck write"
        );
        let t0 = Instant::now();
        for b in [b"1", b"2", b"3"] {
            s.write(p, b.to_vec());
        }
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(s.replaced(), 2);
        go.send(()).unwrap();
        assert!(until(|| mem.log.lock().unwrap().len() == 2));
        assert_eq!(*mem.log.lock().unwrap(), vec![b"0".to_vec(), b"3".to_vec()]);
        assert_eq!(s.write_error(p), None);
    }

    /// A watched file's newest read is handed over once; a missing file
    /// is its error.
    #[test]
    fn a_watched_file_is_read_on_its_period() {
        let (mem, _go) = Mem::new(None);
        let s = FileService::spawn_with("t-files", mem.clone(), STOP_WAIT).unwrap();
        let p = Path::new("peer.toml");
        let w = s.watch(p, Duration::from_millis(5));
        assert!(until(|| matches!(
            s.take(w),
            Some(Err((io::ErrorKind::NotFound, _)))
        )));
        mem.files
            .lock()
            .unwrap()
            .insert(p.to_path_buf(), b"role = 1".to_vec());
        assert!(until(|| s.take(w) == Some(Ok("role = 1".into()))));
    }

    /// Dropped, it writes what was offered first; a disk that never
    /// answers holds the drop no longer than its wait.
    #[test]
    fn a_stop_writes_what_was_offered_and_a_stuck_disk_does_not_hold_it() {
        let (mem, _go) = Mem::new(None);
        let s = FileService::spawn_with("t-files", mem.clone(), STOP_WAIT).unwrap();
        s.write(Path::new("a"), b"last".to_vec());
        drop(s);
        assert_eq!(
            mem.files.lock().unwrap().get(Path::new("a")).unwrap(),
            b"last"
        );

        let (stuck, _never) = Mem::new(Some(b"x"));
        let s = FileService::spawn_with("t-files", stuck, Duration::from_millis(100)).unwrap();
        s.write(Path::new("a"), b"x".to_vec());
        std::thread::sleep(Duration::from_millis(20));
        let t0 = Instant::now();
        drop(s);
        let waited = t0.elapsed();
        assert!(waited < Duration::from_secs(1), "{waited:?}");
    }

    /// Inline: the same work at the call.
    #[test]
    fn inline_does_the_work_at_the_call() {
        let (mem, _go) = Mem::new(None);
        let s = FileService::inline_with(mem.clone());
        let p = Path::new("x");
        let w = s.watch(p, Duration::from_secs(60));
        assert!(matches!(s.take(w), Some(Err((io::ErrorKind::NotFound, _)))));
        s.write(p, b"v".to_vec());
        assert_eq!(s.take(w), Some(Ok("v".into())));
    }
}
