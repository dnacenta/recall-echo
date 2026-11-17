// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Advisory exclusive locks for the archive record files.
//!
//! `flock` locks belong to the open file description, so two [`std::fs::File`]
//! values in one process exclude each other. Threaded archive tests rely on
//! that. The lock is advisory and is not reliable on NFS.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::RecallError;

/// How long a record write waits for the lock before failing.
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(50);

/// The wait [`lock_exclusive`] should use.
///
/// Tests can shorten it for the hook path. Production always waits [`LOCK_WAIT`].
pub(crate) fn lock_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = WAIT_OVERRIDE.with(std::cell::Cell::get) {
        return wait;
    }
    LOCK_WAIT
}

/// Take a blocking exclusive `flock` on `file`, or fail once `wait` elapses.
///
/// Dropping `file` releases the lock. `LOCK_EX` is tried with `LOCK_NB` and
/// retried about every 50 ms, never sleeping past `wait`.
pub(crate) fn lock_exclusive(file: &File, path: &Path, wait: Duration) -> Result<(), RecallError> {
    let deadline = Instant::now() + wait;
    loop {
        match try_lock(file) {
            Attempt::Acquired => return Ok(()),
            Attempt::Busy => {
                if !pause_until(deadline) {
                    return Err(timeout_error(path, wait));
                }
            }
            Attempt::Interrupted => {}
            Attempt::Failed(err) => return Err(io_error(path, err)),
        }
    }
}

enum Attempt {
    Acquired,
    Busy,
    Interrupted,
    Failed(io::Error),
}

fn try_lock(file: &File) -> Attempt {
    if flock_nb(file) == 0 {
        return Attempt::Acquired;
    }
    let err = io::Error::last_os_error();
    match err.kind() {
        ErrorKind::WouldBlock => Attempt::Busy,
        ErrorKind::Interrupted => Attempt::Interrupted,
        _ => Attempt::Failed(err),
    }
}

fn flock_nb(file: &File) -> i32 {
    // SAFETY: `file` is an open descriptor. `flock` does not read or write
    // Rust memory; it only sets an advisory lock on that open file description.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
}

/// Sleep a slice of the remaining wait. `false` means the deadline has passed.
fn pause_until(deadline: Instant) -> bool {
    let now = Instant::now();
    if now >= deadline {
        return false;
    }
    let slice = POLL.min(deadline - now);
    if slice.is_zero() {
        return false;
    }
    thread::sleep(slice);
    true
}

fn timeout_error(path: &Path, wait: Duration) -> RecallError {
    RecallError::Other(format!(
        "{}: locked by another recall-echo process for {} s; not written",
        path.display(),
        wait.as_secs()
    ))
}

fn io_error(path: &Path, err: io::Error) -> RecallError {
    RecallError::Other(format!("{}: {err}", path.display()))
}

#[cfg(test)]
thread_local! {
    static WAIT_OVERRIDE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// Restores the previous test wait when dropped.
#[cfg(test)]
pub(crate) struct WaitOverride {
    previous: Option<Duration>,
}

#[cfg(test)]
impl WaitOverride {
    pub(crate) fn at(wait: Duration) -> Self {
        let previous = WAIT_OVERRIDE.with(|cell| cell.replace(Some(wait)));
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for WaitOverride {
    fn drop(&mut self) {
        let previous = self.previous.take();
        WAIT_OVERRIDE.with(|cell| cell.set(previous));
    }
}

/// Holds an exclusive lock until dropped. The holder thread reports within 2 s.
#[cfg(test)]
pub(crate) struct HeldLock {
    release: Option<std::sync::mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

#[cfg(test)]
impl HeldLock {
    pub(crate) fn on(path: &Path) -> Self {
        let path = path.to_path_buf();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || hold_until_released(path, ready_tx, release_rx));
        let locked = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("holder did not report");
        assert!(locked, "holder could not lock the file");
        Self {
            release: Some(release_tx),
            handle: Some(handle),
        }
    }
}

#[cfg(test)]
fn hold_until_released(
    path: std::path::PathBuf,
    ready: std::sync::mpsc::Sender<bool>,
    release: std::sync::mpsc::Receiver<()>,
) {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open lock target");
    let locked = lock_exclusive(&file, &path, Duration::from_secs(2)).is_ok();
    let _ = ready.send(locked);
    if locked {
        let _ = release.recv();
    }
}

#[cfg(test)]
impl Drop for HeldLock {
    fn drop(&mut self) {
        drop(self.release.take());
        let Some(handle) = self.handle.take() else {
            return;
        };
        if thread::panicking() {
            let _ = handle.join();
        } else {
            handle.join().expect("lock holder");
        }
    }
}

#[cfg(test)]
pub(crate) fn capture_stderr<T>(action: impl FnOnce() -> T) -> (T, String) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let capture = StderrCapture::start();
    let value = action();
    (value, capture.finish())
}

#[cfg(test)]
struct StderrCapture {
    saved: libc::c_int,
    read_end: libc::c_int,
    write_end: libc::c_int,
    restored: bool,
}

#[cfg(test)]
impl StderrCapture {
    fn start() -> Self {
        let mut pipe = [0; 2];
        // SAFETY: `pipe` is a two-slot buffer for the POSIX pipe call.
        let rc = unsafe { libc::pipe(pipe.as_mut_ptr()) };
        assert_eq!(rc, 0);
        // SAFETY: dup of stderr returns a new fd or -1.
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        assert!(saved >= 0);
        let capture = Self {
            saved,
            read_end: pipe[0],
            write_end: pipe[1],
            restored: false,
        };
        // SAFETY: both fds are open. Drop restores stderr if this fails.
        let swapped = unsafe { libc::dup2(capture.write_end, libc::STDERR_FILENO) };
        assert_eq!(swapped, libc::STDERR_FILENO);
        capture
    }

    fn finish(mut self) -> String {
        use std::io::Write;
        let _ = std::io::stderr().flush();
        self.restore();
        let mut bytes = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            // SAFETY: `buf` is writable and `read_end` is this pipe.
            let n = unsafe { libc::read(self.read_end, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n as usize]);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        // SAFETY: `saved` and `write_end` are fds this capture opened.
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
            libc::close(self.write_end);
        }
        self.restored = true;
    }
}

#[cfg(test)]
impl Drop for StderrCapture {
    fn drop(&mut self) {
        self.restore();
        if self.read_end >= 0 {
            // SAFETY: `read_end` is the pipe read end, closed once.
            unsafe { libc::close(self.read_end) };
            self.read_end = -1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_target(path: &Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn lock_exclusive_excludes_a_second_open_in_the_same_process() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ARCHIVE.md");
        let first = open_target(&path);
        lock_exclusive(&first, &path, Duration::from_secs(1)).unwrap();
        let second = open_target(&path);
        let err = lock_exclusive(&second, &path, Duration::ZERO).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(
            message.contains("locked by another recall-echo process"),
            "{message}"
        );
    }
}
