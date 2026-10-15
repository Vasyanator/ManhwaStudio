/*
File: crates/ms-download/src/busy.rs

Purpose:
Process-wide "one download per id" guard: a second `try_acquire` of an id that is still held
fails with `Busy` instead of letting two downloads write the same staging files.

Key items:
- `BusyGuard`: RAII holder of one id; dropping it releases the id. `try_acquire` fails at
  once, `acquire_waiting` polls until the id is free (cancellable).
- `Busy`: the id is already held in this process.
- `dest_key()`: the id under which `fetch_verified` / `zip_member_atomic` guard a
  destination file, so two consumers that publish the same file never interleave.

Notes:
- Ids share ONE process-wide namespace across every consumer; a consumer whose ids could
  collide with another's must prefix them.
- Separate from any per-consumer download lock (e.g. `ms_sysprobe::ai_models::DOWNLOAD_LOCK`).
*/

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::error::DownloadError;
use crate::resumable::{RetryPolicy, sleep_cancellable};

/// Ids currently held by a [`BusyGuard`].
static ACTIVE: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// `try_acquire` failed: the id is already held by another guard in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a download with this id is already running in this process")
    }
}

impl std::error::Error for Busy {}

/// Marks an id as downloading until dropped.
#[derive(Debug)]
pub struct BusyGuard(String);

impl BusyGuard {
    /// Marks `id` as downloading and returns the guard that releases it on drop.
    ///
    /// # Errors
    /// [`Busy`] when `id` is already held in this process.
    pub fn try_acquire(id: &str) -> Result<Self, Busy> {
        // A poisoned lock still holds a consistent set: insert/remove are single calls that
        // cannot panic halfway, so recovering the guard is sound.
        let mut active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
        if active.insert(id.to_owned()) { Ok(Self(id.to_owned())) } else { Err(Busy) }
    }

    /// Waits until `key` is free and takes it. Polls every `retry.wait_slice` (at least
    /// 1 ms, through the injected `retry.sleep`) and checks `cancel` before every poll;
    /// `on_wait` is called once, the first time the key is found held, so the caller can
    /// show "waiting for another download".
    ///
    /// # Errors
    /// [`DownloadError::Cancelled`] when `cancel` is set before the key could be taken.
    pub fn acquire_waiting(key: &str, cancel: &AtomicBool, retry: &RetryPolicy<'_>, on_wait: &mut dyn FnMut()) -> Result<Self, DownloadError> {
        let mut waited = false;
        loop {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(DownloadError::Cancelled);
            }
            match Self::try_acquire(key) {
                Ok(guard) => return Ok(guard),
                Err(Busy) => {
                    if !waited {
                        waited = true;
                        on_wait();
                    }
                    // A zero slice would spin; one millisecond is the floor.
                    if sleep_cancellable(retry, cancel, retry.wait_slice.max(Duration::from_millis(1))) {
                        return Err(DownloadError::Cancelled);
                    }
                }
            }
        }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        ACTIVE.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.0);
    }
}

/// The busy id of a destination file: `dest:` + the absolute form of `dest` (made absolute
/// against the current directory, symlinks NOT resolved). Consumers that share a file must
/// therefore spell its path from the same root; the `dest:` prefix keeps these ids apart from
/// consumer-chosen ids such as model ids.
#[must_use]
pub fn dest_key(dest: &Path) -> String {
    // `absolute` fails only for an empty path or an unreadable current directory; the path as
    // given is then still a stable key for every caller that spells it the same way.
    let absolute = std::path::absolute(dest).unwrap_or_else(|_| dest.to_path_buf());
    format!("dest:{}", absolute.display())
}
