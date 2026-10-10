/*
File: android/saf.rs

Purpose:
State shared between a Rust dialog request and the Storage Access Framework bridge: the
bridge class name and the registry of requests waiting for a result.

Key structures:
- PendingRegistry<T>: `id -> T` slots with monotonically increasing `u64` ids.

Notes:
The Kotlin side reports a result with the request id (`SafBridge.nativeOnResult`). A result for
an id that is no longer registered (the requester dropped its `PendingDialog`) is discarded by
the caller with a log line. Pure Rust, so it is host-tested.
*/

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// Binary name of the Kotlin SAF bridge class, loaded through the application class loader.
/// The single owner of this name; the Gradle project must declare the class under it.
#[cfg(target_os = "android")]
pub(crate) const SAF_BRIDGE_CLASS: &jni::strings::JNIStr = jni::jni_str!("io.github.vasyanator.manhwastudio.platform.SafBridge");

/// Requests waiting for an asynchronous result, keyed by a process-unique id.
pub(crate) struct PendingRegistry<T> {
    next_id: AtomicU64,
    slots: Mutex<BTreeMap<u64, T>>,
}

impl<T> PendingRegistry<T> {
    /// An empty registry; `const` so it can back a `static`.
    pub(crate) const fn new() -> Self {
        Self { next_id: AtomicU64::new(1), slots: Mutex::new(BTreeMap::new()) }
    }

    /// Stores `value` and returns its id (never 0, never reused within the process).
    pub(crate) fn register(&self, value: T) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id, value);
        id
    }

    /// Removes and returns the value registered under `id`; `None` when it was already taken
    /// or withdrawn (a late result).
    pub(crate) fn take(&self, id: u64) -> Option<T> {
        self.lock().remove(&id)
    }

    /// Number of waiting requests.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, T>> {
        // A panic while holding the lock can only interrupt a single map insert/remove, which
        // leaves the map itself consistent, so a poisoned lock is still safe to use.
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::PendingRegistry;

    #[test]
    fn register_then_resolve_once() {
        let registry: PendingRegistry<&str> = PendingRegistry::new();
        let first = registry.register("a");
        let second = registry.register("b");
        assert_ne!(first, second);
        assert_ne!(first, 0);
        assert_eq!(registry.take(second), Some("b"));
        assert_eq!(registry.take(second), None);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.take(first), Some("a"));
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn late_result_after_withdrawal_is_discarded() {
        let registry: PendingRegistry<std::sync::mpsc::Sender<u32>> = PendingRegistry::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        let id = registry.register(sender);
        // The requester goes away: its drop guard withdraws the slot, closing the channel.
        drop(registry.take(id));
        assert!(receiver.recv().is_err());
        // The bridge reports later: nothing is registered under the id any more.
        assert!(registry.take(id).is_none());
    }
}
