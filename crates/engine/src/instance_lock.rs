//! Single-instance lock — an exclusive advisory lock on `{data_dir}/engine.lock`
//! (`flock` on unix, `LockFileEx` on Windows) held for the engine's lifetime. Two
//! engines sharing one data dir would race the SQLite snapshots DB and the
//! append-only run journals (WAL + `busy_timeout` guard individual statements, not
//! whole-file ownership), so the second instance must fail fast with a clear error
//! instead of corrupting state.
//!
//! The lock is taken in `EngineCore::assemble_with_identity` BEFORE any store is opened
//! and before the IPC port binds, which also closes the race where a headed app's
//! TCP probe sees no daemon during another instance's startup window.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::EngineError;

/// Held lock on the data dir. Dropping it (engine shutdown / process exit)
/// releases the advisory lock; a crash releases it too (kernel-owned).
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Acquire the exclusive lock, non-blocking. Errors with a descriptive
    /// message (including the holder's pid when readable) if another engine
    /// already owns this data dir.
    pub fn acquire(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join("engine.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // Bounded EWOULDBLOCK retries: a fork→exec window in ANY process
            // that inherited the previous holder's fd (git scans, harness
            // spawns — fds are duplicated between fork and CLOEXEC-at-exec)
            // keeps the flock alive for a few milliseconds after release. A
            // real second engine holds it forever; transient artifacts clear
            // well within the budget.
            let mut retries = 40u32; // × 25ms = 1s budget
            loop {
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if rc == 0 {
                    break;
                }
                let errno = std::io::Error::last_os_error();
                match errno.raw_os_error() {
                    Some(libc::EINTR) => continue, // signal-interrupted: retry
                    Some(libc::EWOULDBLOCK) if retries > 0 => {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Some(libc::EWOULDBLOCK) => return Err(contended(data_dir, &path)),
                    // Anything else (ENOLCK, filesystem without flock, …) is an
                    // environment problem, not a second engine — surface it as-is.
                    _ => return Err(EngineError::Io(errno)),
                }
            }
        }

        #[cfg(windows)]
        {
            // Same budget for the same reason as the unix arm: a handle the
            // previous holder let a child inherit (git scans, harness spawns)
            // keeps the byte-range lock alive for a few milliseconds after the
            // engine itself is gone. A real second engine never releases inside
            // the budget.
            let mut retries = 40u32; // × 25ms = 1s budget
            loop {
                match windows_lock::try_lock(&file) {
                    Ok(()) => break,
                    Err(windows_lock::TryLockError::Held) if retries > 0 => {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Err(windows_lock::TryLockError::Held) => {
                        return Err(contended(data_dir, &path));
                    }
                    // Anything else is an environment problem (a filesystem
                    // without byte-range locks, a revoked handle), not a second
                    // engine — surface it as-is.
                    Err(windows_lock::TryLockError::Io(error)) => {
                        return Err(EngineError::Io(error));
                    }
                }
            }
        }

        // Best-effort pid stamp for the contention error message above. The
        // Windows lock deliberately covers a range far past the stamp, so a
        // contending engine can still read it.
        let _ = file.set_len(0);
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        Ok(Self { _file: file })
    }

    /// Best-effort liveness probe: the pid stamped by the engine currently holding
    /// this data dir's lock, `None` when no engine is running (or the platform
    /// cannot test a lock without taking it). Used by `zeron status` and the
    /// login/logout guards; a single non-blocking try — no retry budget — so a
    /// starting engine's transient fork-window artifacts read as "running", which
    /// is the safe direction for those callers.
    pub fn holder(data_dir: &Path) -> Option<String> {
        let path = data_dir.join("engine.lock");

        #[cfg(any(unix, windows))]
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                // We took it: nothing is running. Closing the fd releases it, but
                // unlock explicitly so the window is as small as possible.
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
                return None;
            }
            Some(stamped_pid(&path))
        }

        #[cfg(windows)]
        {
            match windows_lock::try_lock(&file) {
                Ok(()) => {
                    // We took it: nothing is running. Closing the handle releases
                    // it, but unlock explicitly so the window is as small as
                    // possible.
                    windows_lock::unlock(&file);
                    None
                }
                Err(windows_lock::TryLockError::Held) => Some(stamped_pid(&path)),
                // A filesystem that cannot test the lock at all is the one case
                // left to guess at; "no engine" matches what this arm returned
                // before and keeps `zeron status` usable.
                Err(windows_lock::TryLockError::Io(_)) => None,
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            None
        }
    }
}

/// The pid the current holder stamped into the lock file, `"unknown"` when the
/// stamp is missing or unreadable.
#[cfg(any(unix, windows))]
fn stamped_pid(path: &Path) -> String {
    let pid = std::fs::read_to_string(path).unwrap_or_default();
    let pid = pid.trim();
    if pid.is_empty() {
        "unknown".to_string()
    } else {
        pid.to_string()
    }
}

#[cfg(any(unix, windows))]
fn contended(data_dir: &Path, path: &Path) -> EngineError {
    EngineError::Other(format!(
        "another zeron engine is already running on {} (pid {}); \
         stop it or use a different data dir (ZERON_DATA_DIR)",
        data_dir.display(),
        stamped_pid(path),
    ))
}

#[cfg(windows)]
mod windows_lock {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    /// The locked range sits a full 4 GiB past offset zero so it never covers the
    /// pid stamp: a contending engine has to READ that stamp for the error
    /// message, and a locked range makes reads from other processes fail. The
    /// file itself stays a few bytes long, a byte-range lock does not require the
    /// bytes to exist.
    const LOCK_OFFSET_LOW: u32 = 0;
    const LOCK_OFFSET_HIGH: u32 = 1;
    const LOCK_LENGTH_LOW: u32 = 1;
    const LOCK_LENGTH_HIGH: u32 = 0;

    pub(super) enum TryLockError {
        /// Another handle, in this process or any other, owns the range.
        Held,
        Io(std::io::Error),
    }

    fn range() -> OVERLAPPED {
        // SAFETY: OVERLAPPED is a plain C struct whose all-zero bit pattern is
        // the documented "no event, offset zero" state.
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = LOCK_OFFSET_LOW;
        overlapped.Anonymous.Anonymous.OffsetHigh = LOCK_OFFSET_HIGH;
        overlapped
    }

    /// Non-blocking exclusive lock on the sentinel range.
    pub(super) fn try_lock(file: &File) -> Result<(), TryLockError> {
        let mut overlapped = range();
        // SAFETY: the handle is owned by `file` and outlives the call; the
        // OVERLAPPED lives on this frame for the whole call, which is synchronous
        // because of LOCKFILE_FAIL_IMMEDIATELY on a non-overlapped handle.
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle() as HANDLE,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                LOCK_LENGTH_LOW,
                LOCK_LENGTH_HIGH,
                &mut overlapped,
            )
        };
        if ok != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Err(TryLockError::Held)
        } else {
            Err(TryLockError::Io(error))
        }
    }

    /// Best-effort release; closing the handle releases the range anyway.
    pub(super) fn unlock(file: &File) {
        let mut overlapped = range();
        // SAFETY: as in `try_lock`.
        unsafe {
            UnlockFileEx(
                file.as_raw_handle() as HANDLE,
                0,
                LOCK_LENGTH_LOW,
                LOCK_LENGTH_HIGH,
                &mut overlapped,
            );
        }
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn holder_probe_reports_pid_without_disturbing_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(InstanceLock::holder(dir.path()), None, "unlocked dir");
        let lock = InstanceLock::acquire(dir.path()).expect("acquire");
        assert_eq!(
            InstanceLock::holder(dir.path()).as_deref(),
            Some(std::process::id().to_string().as_str()),
        );
        // The probe must not have stolen the lock from the holder.
        InstanceLock::acquire(dir.path()).expect_err("still held after probe");
        drop(lock);
        assert_eq!(InstanceLock::holder(dir.path()), None, "released");
    }

    #[test]
    fn second_acquire_fails_while_held_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InstanceLock::acquire(dir.path()).expect("first acquire");
        let err = InstanceLock::acquire(dir.path()).expect_err("second acquire must fail");
        let msg = err.to_string();
        assert!(msg.contains("already running"), "unexpected error: {msg}");
        assert!(
            msg.contains(&std::process::id().to_string()),
            "holder pid missing from error: {msg}"
        );
        drop(lock);
        InstanceLock::acquire(dir.path()).expect("acquire after release");
    }
}
