/*
File: storage.rs

Purpose:
Application seam over the `ms-storage` abstraction. Exposes the single
process-wide `Arc<dyn Storage>` that every persistence path uses instead of
calling `std::fs` directly, and selects the backend by build target:

- native  -> `PassthroughStorage` (real OS filesystem, absolute paths verbatim;
             behaviorally identical to the previous direct `std::fs` usage).
- wasm    -> `MemStorage` (in-memory session store; hydrated from / flushed to
             browser IndexedDB by the web layer).

Key functions:
- storage(): borrow the global backend (lazily initialized to the target default).
- install(): install a specific backend before first use (web hydration path).

Notes:
Read-only global dispatch via `OnceLock` is the sanctioned pattern for immutable
configuration. The handle is set once and never mutated; the backends themselves
are internally synchronized (`PassthroughStorage` is stateless; `MemStorage`
uses an `RwLock`). See docs/WEB_PORT.md.
*/

use std::sync::{Arc, OnceLock};

// `Storage` is named locally in the backend types below; the other seam types
// (`DirEntry`, `Metadata`, `StorageError`) are reached directly via
// `ms_storage::` at the call sites that need them, so re-exporting them here
// would be an unused import until a consumer names them.
pub use ms_storage::Storage;

/// Process-wide storage backend, set once at startup or lazily defaulted.
static STORAGE: OnceLock<Arc<dyn Storage>> = OnceLock::new();

/// Builds the default backend for the current build target.
fn default_backend() -> Arc<dyn Storage> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Arc::new(ms_storage::PassthroughStorage::new())
    }
    #[cfg(target_arch = "wasm32")]
    {
        Arc::new(ms_storage::MemStorage::new())
    }
}

/// Returns the process-wide storage backend, initializing the target default on
/// first use.
///
/// The returned handle is cheap to hold and `Send + Sync`; call methods directly
/// (`storage().read(path)`). On native this addresses the real filesystem by
/// absolute path, so it is a drop-in for the previous `std::fs` calls.
#[must_use]
pub fn storage() -> &'static Arc<dyn Storage> {
    STORAGE.get_or_init(default_backend)
}

/// Installs `backend` as the process-wide storage, if none is set yet.
///
/// The web entry point calls this with a `MemStorage` already hydrated from
/// IndexedDB, before any code touches [`storage`]. Web-only: the native build
/// always uses the lazily-defaulted `PassthroughStorage`, so this is compiled
/// out there to avoid a dead-code path.
///
/// # Errors
/// Returns the passed `backend` back if a backend was already initialized (the
/// global is write-once).
#[cfg(target_arch = "wasm32")]
pub fn install(backend: Arc<dyn Storage>) -> Result<(), Arc<dyn Storage>> {
    STORAGE.set(backend)
}
