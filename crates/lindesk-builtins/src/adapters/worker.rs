//! Thread plumbing shared by the built-in adapters: a stoppable worker thread that waits on a
//! connection fd, a `connected` flag, and reconnect backoff.

use std::io::{self, Read, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;

/// What woke [`Control::wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The watched fd is readable (or hung up / errored: the next read will tell).
    Ready,
    /// [`Control::stop`] was called.
    Stopped,
    Timeout,
}

/// State shared between an adapter and its worker thread.
pub(crate) struct Control {
    stopped: AtomicBool,
    connected: AtomicBool,
    wake_tx: UnixStream,
    wake_rx: UnixStream,
}

impl Control {
    fn new() -> io::Result<Self> {
        let (wake_tx, wake_rx) = UnixStream::pair()?;
        wake_tx.set_nonblocking(true)?;
        wake_rx.set_nonblocking(true)?;
        Ok(Control { stopped: AtomicBool::new(false), connected: AtomicBool::new(false), wake_tx, wake_rx })
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Non-blocking; a full buffer already means "wake up".
        let _ = (&self.wake_tx).write(&[1]);
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub(crate) fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::SeqCst);
    }

    pub(crate) fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Blocks until `fd` is readable, [`Control::stop`] is called or `timeout` elapses
    /// (`None` = no timeout).
    pub(crate) fn wait(&self, fd: Option<BorrowedFd<'_>>, timeout: Option<Duration>) -> io::Result<Wake> {
        if self.is_stopped() {
            return Ok(Wake::Stopped);
        }
        let timeout = timeout.map(|d| Timespec::try_from(d).unwrap_or(Timespec { tv_sec: i64::MAX, tv_nsec: 0 }));
        let mut fds = vec![PollFd::new(&self.wake_rx, PollFlags::IN)];
        if let Some(fd) = fd {
            fds.push(PollFd::from_borrowed_fd(fd, PollFlags::IN));
        }
        let n = loop {
            match poll(&mut fds, timeout.as_ref()) {
                Ok(n) => break n,
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        };
        if self.is_stopped() || !fds[0].revents().is_empty() {
            // Drain so a later wait doesn't spin (stop is sticky anyway).
            let mut buf = [0u8; 16];
            let _ = (&self.wake_rx).read(&mut buf);
            return Ok(Wake::Stopped);
        }
        if n > 0 && fds.get(1).is_some_and(|p| !p.revents().is_empty()) {
            return Ok(Wake::Ready);
        }
        Ok(Wake::Timeout)
    }

    /// Sleeps for `d` unless stopped first. Returns true if stopped.
    pub(crate) fn sleep(&self, d: Duration) -> bool {
        matches!(self.wait(None, Some(d)), Ok(Wake::Stopped) | Err(_)) || self.is_stopped()
    }
}

/// A named worker thread with a [`Control`]. Dropping it stops and joins the thread.
pub(crate) struct Worker {
    ctl: Arc<Control>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// Spawns `body` on a thread. `connected` is the initial state of [`Worker::connected`]: true
    /// when the caller already established the connection the body will use.
    pub(crate) fn spawn(
        name: &str,
        connected: bool,
        body: impl FnOnce(&Control) + Send + 'static,
    ) -> io::Result<Worker> {
        let ctl = Arc::new(Control::new()?);
        ctl.set_connected(connected);
        let thread_ctl = Arc::clone(&ctl);
        let handle = std::thread::Builder::new().name(name.to_owned()).spawn(move || {
            // Clears `connected` however the body exits, including by panic.
            struct Disconnect<'a>(&'a Control);
            impl Drop for Disconnect<'_> {
                fn drop(&mut self) {
                    self.0.set_connected(false);
                }
            }
            let _guard = Disconnect(&thread_ctl);
            body(&thread_ctl);
        })?;
        Ok(Worker { ctl, handle: Some(handle) })
    }

    pub(crate) fn connected(&self) -> bool {
        self.ctl.connected()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Signals the thread and waits for it to exit.
    pub(crate) fn stop(&mut self) {
        self.ctl.stop();
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::warn!("adapter thread panicked");
        }
        self.ctl.set_connected(false);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Exponential reconnect backoff.
#[derive(Clone, Debug)]
pub(crate) struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub(crate) fn new(min: Duration, max: Duration) -> Self {
        Backoff { min, max, next: min }
    }

    /// The delay to wait now; doubles the following one, up to `max`.
    pub(crate) fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(self.max);
        d
    }

    pub(crate) fn reset(&mut self) {
        self.next = self.min;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::new(Duration::from_millis(500), Duration::from_secs(30))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        let got: Vec<_> = (0..5).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(got, [100, 200, 400, 500, 500]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }

    #[test]
    fn wait_times_out_and_stops() {
        let ctl = Control::new().unwrap();
        assert_eq!(ctl.wait(None, Some(Duration::from_millis(10))).unwrap(), Wake::Timeout);
        ctl.stop();
        assert_eq!(ctl.wait(None, None).unwrap(), Wake::Stopped);
        assert!(ctl.sleep(Duration::from_secs(60)));
    }

    #[test]
    fn wait_reports_readable_fd() {
        use std::os::fd::AsFd;
        let ctl = Control::new().unwrap();
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(b"x").unwrap();
        assert_eq!(ctl.wait(Some(b.as_fd()), Some(Duration::from_secs(5))).unwrap(), Wake::Ready);
    }

    #[test]
    fn worker_stop_wakes_a_blocked_thread() {
        let mut w = Worker::spawn("test-worker", false, |ctl| {
            ctl.set_connected(true);
            while ctl.wait(None, None).unwrap() != Wake::Stopped {}
        })
        .unwrap();
        let t0 = Instant::now();
        while !w.connected() {
            assert!(t0.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
        assert!(w.is_running());
        w.stop();
        assert!(!w.connected());
        assert!(!w.is_running());
        assert!(t0.elapsed() < Duration::from_secs(5));
    }
}
