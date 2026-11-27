//! A stopped runtime clock, for a test whose answer depends on a deadline.
//!
//! On a busy host the wall clock runs on while the test waits for a CPU, so a
//! deadline the test sets can pass before a request has even been sent, and a
//! deadline the product sets can pass while a mock server's answer waits to
//! be read. Under a [`FrozenClock`] the runtime's clock (`tokio::time`) stands
//! still: socket work finishes at whatever speed the host allows, and a
//! deadline passes only when the test moves the clock with
//! `tokio::time::advance`. Code under test must take its deadlines from
//! `tokio::time` for this to hold.
//!
//! `tokio::time::pause` alone is not enough. A paused clock jumps to the next
//! timer whenever the runtime has nothing to run, and it has nothing to run
//! while it waits on a socket. Tokio does not move a paused clock while a
//! blocking task is running on a current-thread runtime, so the guard keeps
//! one running until it is dropped.

use std::sync::mpsc;
use std::time::Duration;

/// The stopped clock of the current-thread runtime the test runs on. The
/// clock stays stopped (moved only by `tokio::time::advance`) until this is
/// dropped.
pub struct FrozenClock {
    release: Option<mpsc::Sender<()>>,
}

impl FrozenClock {
    /// Stop the clock. Panics on a multi-thread runtime, as
    /// `tokio::time::pause` does.
    pub fn stop() -> Self {
        tokio::time::pause();
        let (release, held) = mpsc::channel::<()>();
        // Its receive returns when `release` is dropped.
        drop(tokio::task::spawn_blocking(move || {
            let _ = held.recv();
        }));
        Self {
            release: Some(release),
        }
    }
}

impl Drop for FrozenClock {
    fn drop(&mut self) {
        self.release.take();
    }
}

/// `future`'s output, or a panic naming `what` if it is not ready within
/// `limit` of real time. The runtime's clock may be stopped, so this bound is
/// kept on a blocking thread: it only stops a broken product from hanging the
/// test, and never decides a passing one.
pub async fn within_real_time<F: std::future::Future>(
    limit: Duration,
    what: &str,
    future: F,
) -> F::Output {
    let (done, finished) = mpsc::channel::<()>();
    let watchdog = tokio::task::spawn_blocking(move || {
        matches!(
            finished.recv_timeout(limit),
            Err(mpsc::RecvTimeoutError::Timeout)
        )
    });
    tokio::pin!(future);
    tokio::select! {
        biased;
        output = &mut future => {
            drop(done);
            output
        }
        timed_out = watchdog => {
            if timed_out.unwrap_or(false) {
                panic!("{what} did not finish within {limit:?} of real time");
            }
            future.await
        }
    }
}
