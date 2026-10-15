/*
File: crates/ms-download/src/test_fs.rs

Purpose:
Shared helpers of this crate's unit tests (`file`, `small`, `unpack`, `busy`): self-removing
scratch directories under the system temp dir, deterministic content, sha256 of a byte
slice, and a recording backoff sleep that can set a cancel flag or run a hook. Test-only
(`cfg(test)`); dependents use `test_support` instead.
*/

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::Digest;

use crate::resumable::RetryPolicy;

/// A self-removing scratch directory under the system temp dir.
pub(crate) fn temp_root(prefix: &str) -> tempfile::TempDir {
    match tempfile::Builder::new().prefix(prefix).tempdir_in(std::env::temp_dir()) {
        Ok(dir) => dir,
        Err(err) => panic!("cannot create a temp dir: {err}"),
    }
}

/// Deterministic test content of `len` bytes, varied by `seed`.
pub(crate) fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|index| u8::try_from(index % 251).unwrap_or(0).wrapping_add(seed)).collect()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    crate::file::hex_lower(&sha2::Sha256::digest(bytes))
}

pub(crate) fn len_u64(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

pub(crate) fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

pub(crate) fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap_or_else(|err| panic!("write {}: {err}", path.display()));
}

/// Records every backoff sleep slice; optionally sets `cancel` on the first one and runs
/// `on_sleep` on every one.
pub(crate) struct SleepLog<'a> {
    slices: Mutex<Vec<Duration>>,
    cancel_on_sleep: Option<&'a AtomicBool>,
    on_sleep: Option<Box<dyn Fn() + 'a>>,
}

impl<'a> SleepLog<'a> {
    pub(crate) fn new(cancel_on_sleep: Option<&'a AtomicBool>) -> Self {
        Self { slices: Mutex::new(Vec::new()), cancel_on_sleep, on_sleep: None }
    }

    pub(crate) fn with_hook(on_sleep: impl Fn() + 'a) -> Self {
        Self { slices: Mutex::new(Vec::new()), cancel_on_sleep: None, on_sleep: Some(Box::new(on_sleep)) }
    }

    pub(crate) fn sleep(&self, slice: Duration) {
        if let Ok(mut slices) = self.slices.lock() {
            slices.push(slice);
        }
        if let Some(cancel) = self.cancel_on_sleep {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(hook) = &self.on_sleep {
            hook();
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.slices.lock().map(|slices| slices.len()).unwrap_or(usize::MAX)
    }
}

/// A fast test schedule over `sleep`: 3 retries, 1 s first delay, 100 ms slices.
pub(crate) fn policy<'a>(sleep: &'a dyn Fn(Duration)) -> RetryPolicy<'a> {
    RetryPolicy {
        max_retries: 3,
        first_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(4),
        retry_after_cap: Duration::from_secs(10),
        wait_slice: Duration::from_millis(100),
        sleep,
    }
}
