/*
File: incoming/mod.rs

Purpose:
Images other apps hand to ManhwaStudio while it is not the one asking: Android's Share sheet
(one or several images) and "Open with". The Kotlin `IncomingImages` copies them into an inbox
under the app cache and publishes one batch per incoming intent; this module takes those
batches over JNI and hands them to the shell as typed `IncomingBatch`es.

Main responsibilities:
- the cross-target API: every function exists on every target; outside Android it answers
  "nothing incoming" (`Ok(vec![])`, `false`), so callers never `cfg`;
- the pull-plus-wake hand-off: Kotlin wakes Rust through the native `nativeOnIncoming`, which
  only raises a flag and calls the registered waker; the GUI polls `has_pending()` (no JNI) and
  takes the batches on a worker (`take_pending_blocking`).

Key structures:
- IncomingBatch, IncomingSkip, IncomingSkipKind: one taken batch and its skipped documents.
- IncomingError: a failed bridge call or an undecodable listing.

Key functions:
- init(): cold start (pre-window, android_main thread): binds the wake-up and takes the
  batches that arrived before the app ran.
- set_waker(), has_pending(), take_pending_blocking(), release_batch_blocking().

Notes:
Submodules: `listing.rs` (pure wire decoding, host-tested), `android.rs` (JNI half, Android
only). The inbox layout is owned by Kotlin (`IncomingImages.kt`); Rust never lists the inbox
folder itself. A taken batch's files stay on disk until the consumer calls
`release_batch_blocking` (or the next process start sweeps claimed batches).
*/

// Pure decoding of the Kotlin listing: only the Android backend consumes it; host tests cover it.
#[cfg(any(target_os = "android", test))]
mod listing;

#[cfg(target_os = "android")]
mod android;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::file_dialog::Waker;

/// Set by the Kotlin wake-up, cleared by `take_pending_blocking` before it asks Kotlin.
static PENDING: AtomicBool = AtomicBool::new(false);
/// Repaint callback of the GUI that polls `has_pending`.
static WAKER: Mutex<Option<Waker>> = Mutex::new(None);

/// One incoming intent's images, as received: the copied files in the sender's order and the
/// documents that could not be copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingBatch {
    /// Local copies, in the order the sender listed them; readable until the batch is released.
    pub files: Vec<PathBuf>,
    /// Documents that were not copied, in the sender's order.
    pub skipped: Vec<IncomingSkip>,
    /// The inbox folder holding `files`; pass it to [`release_batch_blocking`] once the files
    /// were opened (copied into a session). `None` only for a batch that owns no folder.
    pub batch_dir: Option<PathBuf>,
}

/// One document of a batch that was not copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingSkip {
    /// The name the sender gave the document, or its 1-based position when it had no stream.
    pub name: String,
    /// Why it was skipped.
    pub kind: IncomingSkipKind,
    /// Technical detail (exception text) for logs; may be empty. Not localized.
    pub detail: String,
}

/// Why an incoming document was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IncomingSkipKind {
    /// The document had a URI but reading or copying it failed.
    CopyFailed,
    /// The sender listed an item without any document stream (e.g. a text item).
    NoStream,
}

impl IncomingSkipKind {
    /// The i18n key of [`Self::user_reason`].
    #[must_use]
    pub const fn message_key(self) -> &'static str {
        match self {
            Self::CopyFailed => "shell.incoming.copy_failed_reason",
            Self::NoStream => "shell.incoming.no_stream_reason",
        }
    }

    /// The localized one-line reason, for a "skipped files" list.
    #[must_use]
    pub fn user_reason(self) -> &'static str {
        ms_i18n::resolve_key(self.message_key())
    }
}

/// A failed hand-off. `Display` is the technical text for logs; show [`Self::user_message`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IncomingError {
    /// The JVM side could not be reached: the bridge class is missing from the APK, a JNI call
    /// failed, or the native could not be bound.
    #[error("incoming-images bridge failed: {0}")]
    Bridge(String),
    /// The inbox answered a listing that does not decode; every batch of that take was dropped.
    #[error("incoming-images listing is malformed: {0}")]
    Listing(String),
}

impl IncomingError {
    /// The i18n key of [`Self::user_message`].
    #[must_use]
    pub const fn message_key(&self) -> &'static str {
        match self {
            Self::Bridge(_) | Self::Listing(_) => "shell.incoming.bridge_error",
        }
    }

    /// The localized message for the user (the technical detail goes to the log).
    #[must_use]
    pub fn user_message(&self) -> &'static str {
        ms_i18n::resolve_key(self.message_key())
    }
}

/// Cold start: binds the wake-up and takes every batch that was published before the app ran
/// (a share that started the process). Batches an earlier session claimed but never released
/// are deleted first. Call once per native session, before the first window frame, on the
/// thread that runs `run_main` (one short series of JNI calls; the documented pre-window
/// exception to "no JNI on the GUI thread"). Outside Android: `Ok(vec![])`.
///
/// # Errors
/// [`IncomingError::Bridge`] when the Kotlin side cannot be reached (the app keeps working
/// without incoming images); [`IncomingError::Listing`] for an undecodable listing.
pub fn init() -> Result<Vec<IncomingBatch>, IncomingError> {
    #[cfg(target_os = "android")]
    {
        android::init()
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(Vec::new())
    }
}

/// Registers the repaint callback the wake-up calls (replaces an earlier one). GUI-safe.
pub fn set_waker(waker: Waker) {
    *WAKER.lock().unwrap_or_else(PoisonError::into_inner) = Some(waker);
}

/// Whether a batch was published since the last take. An atomic read: GUI-safe, no JNI.
#[must_use]
pub fn has_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// Takes every published batch, in arrival order. The flag is cleared BEFORE Kotlin is asked, so
/// a batch published during the call raises it again and is never missed. Blocking JNI: call it
/// on a worker thread only. Outside Android: `Ok(vec![])`.
///
/// # Errors
/// As [`init`]; a malformed take releases every batch it claimed.
pub fn take_pending_blocking() -> Result<Vec<IncomingBatch>, IncomingError> {
    PENDING.store(false, Ordering::Release);
    #[cfg(target_os = "android")]
    {
        android::take_pending()
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(Vec::new())
    }
}

/// Deletes the taken batch folder `dir` (an [`IncomingBatch::batch_dir`]) once its files are no
/// longer needed. Failures are logged, never returned: a leftover is swept at the next process
/// start. Blocking JNI + file I/O: worker threads only. Outside Android there is no inbox, so
/// nothing is deleted.
pub fn release_batch_blocking(dir: &Path) {
    #[cfg(target_os = "android")]
    android::release_batch(dir);
    #[cfg(not(target_os = "android"))]
    ms_log::runtime_log::log_warn(format!("[incoming] release of '{}' ignored: this platform has no incoming-images inbox", dir.display()));
}

/// The wake-up: raises the flag, then calls the waker (outside the lock). Called by the
/// Android native only.
#[cfg(any(target_os = "android", test))]
fn signal_pending() {
    PENDING.store(true, Ordering::Release);
    let waker = WAKER.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if let Some(waker) = waker {
        waker();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// One test owns the process-global flag and waker, so parallel tests never interleave on it.
    #[test]
    fn pull_plus_wake_contract_on_the_host() {
        assert!(!has_pending(), "nothing is pending before a wake-up");
        assert_eq!(take_pending_blocking(), Ok(Vec::new()));
        assert_eq!(init(), Ok(Vec::new()));

        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        set_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        signal_pending();
        assert!(has_pending());
        assert_eq!(wakes.load(Ordering::SeqCst), 1);

        assert_eq!(take_pending_blocking(), Ok(Vec::new()));
        assert!(!has_pending(), "a take clears the flag");
        *WAKER.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    #[test]
    fn messages_exist_in_the_shipped_catalogs() {
        for key in [IncomingSkipKind::CopyFailed.message_key(), IncomingSkipKind::NoStream.message_key(), IncomingError::Bridge(String::new()).message_key()] {
            crate::test_catalog::assert_key_in_shipped_catalogs(key);
        }
    }
}
