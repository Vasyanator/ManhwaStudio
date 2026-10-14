/*
File: crates/ms-sysprobe/src/hf_token.rs (re-exported as `crate::hf_token`)

Purpose:
The process-wide Hugging Face access token. ONE value for the whole application: the
FLUX.2 klein model-download block is its first UI surface, but the token is not that
block's property — any feature that has to fetch from a gated Hugging Face repository
reads the same value, so it lives here rather than inside an engine.

Storage:
The token is a CREDENTIAL and is kept in the platform secret store under the service name
`"ManhwaStudio Hugging Face"` (the OS store through `keyring` on desktop, the Keystore-backed
`ms_platform::credentials` store on Android), exactly as the AI API keys are kept under
`"ManhwaStudio AI API OCR"` (`ms_ai_api::keys`). It is NEVER written to `user_config.json`, to
any settings JSON, or to a log. The AI API entry is deliberately NOT reused or generalized:
that one is keyed by service, while this one is a single global value.

Shape:
A runtime global with free get/set/clear functions, in the shape of
`crates/ms-config/src/rotation_ctrl_wheel.rs`: a cached value behind an `RwLock`,
seeded once at startup (`seed_hf_token_from_secret_store`, called by `lib.rs`, reads
on its own worker) so that every later READ is a lock acquisition rather than an OS
round trip. Only the seed, a save and a delete
touch the secret store, and all three are BLOCKING — the GUI thread must never call
them directly (CLAUDE.md §5).

Key items:
- `HfTokenState`: the tri-state the UI badge renders (not known / not set / stored).
- `hf_token()` / `hf_token_state()`: free, non-blocking cache reads.
- `store_hf_token()` / `clear_hf_token()` / `read_hf_token()`: the blocking secret-store
  operations, each of which also refreshes the cache.
- `seed_hf_token_from_secret_store()`: the startup seed, on a worker thread.

Notes:
The token VALUE must never appear in a log line, an error message or a panic payload.
Every message in this file says "the token" and interpolates only the store's own
error, which never carries the secret.
The per-target store is chosen in ONE place, the private `backend` module: desktop `keyring`,
Android `ms_platform::credentials` (`keyring` would silently link its in-memory mock there, so
it is compiled out), web: none — the store operations fail with the localized
`ms_platform::Unavailable { CredentialStore }` message while the cache still works, and the
startup seed only logs that. On Android an APK without the Kotlin bridge answers the same
message with the `BridgeMissing` reason, and a token that can no longer be decrypted is a read
error, never "no token".
*/

use std::sync::RwLock;

/// Service name of this token's entry in the OS secret store.
///
/// Its own name, never shared with `"ManhwaStudio AI API OCR"`: the two are different
/// credentials with different lifetimes, and a shared entry would let deleting one
/// silently delete the other.
const HF_TOKEN_SERVICE: &str = "ManhwaStudio Hugging Face";

/// Account name inside that service. There is exactly one Hugging Face token per
/// installation, so the account is a constant rather than a user identity.
const HF_TOKEN_ACCOUNT: &str = "default";

/// What the UI knows about the stored token right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HfTokenState {
    /// The secret store has not been read yet (the startup seed is still in flight, or
    /// it failed). NOT the same as "there is no token": reporting "not set" here would
    /// send the user to create a second token they already have.
    Unknown,
    /// The secret store was read and holds no token.
    Missing,
    /// A token is stored and cached.
    Stored,
}

/// The cached token. `None` means "not read yet" ([`HfTokenState::Unknown`]);
/// `Some(String::new())` means "read, and there is none".
///
/// An `RwLock` and not an atomic because the value is a `String`; it is read once per
/// backend request and per drawn frame of the token badge, never on a hot path.
static TOKEN: RwLock<Option<String>> = RwLock::new(None);

/// Reads the cached token, or an empty string when none is stored or the cache has not
/// been seeded yet.
///
/// Non-blocking and safe on the GUI thread: it never touches the OS secret store. An
/// empty answer is what travels to the backend as `hf_token` and is what makes the
/// backend answer `no_token` without a network call.
#[must_use]
pub fn hf_token() -> String {
    read_cache().unwrap_or_default()
}

/// The tri-state the token badge renders. See [`HfTokenState`] for why "not read yet"
/// is distinct from "not stored".
///
/// Non-blocking and safe on the GUI thread.
#[must_use]
pub fn hf_token_state() -> HfTokenState {
    match read_cache() {
        None => HfTokenState::Unknown,
        Some(token) if token.is_empty() => HfTokenState::Missing,
        Some(_) => HfTokenState::Stored,
    }
}

/// Publishes `token` into the process-wide cache without touching the secret store.
///
/// `None` puts the cache back into [`HfTokenState::Unknown`]. Used by the startup seed
/// and by the store/clear paths after the OS store has accepted the change; tests use
/// it to drive the state directly.
pub fn set_cached_hf_token(token: Option<String>) {
    let normalized = token.map(|value| value.trim().to_string());
    match TOKEN.write() {
        Ok(mut guard) => *guard = normalized,
        // A poisoned lock means a panic happened while the cache was held. The value is
        // a plain `Option<String>` with no invariant spanning the two, so recovering it
        // is sound and losing the token would be worse than the panic already was.
        Err(poison) => *poison.into_inner() = normalized,
    }
}

/// Reads the cache, recovering from a poisoned lock for the reason given in
/// [`set_cached_hf_token`].
fn read_cache() -> Option<String> {
    match TOKEN.read() {
        Ok(guard) => guard.clone(),
        Err(poison) => (*poison.into_inner()).clone(),
    }
}

/// Writes `token` into the secret store and refreshes the cache.
///
/// BLOCKING: the store is an OS round trip. Never call this on the GUI thread —
/// spawn it (`ms_thread::spawn`), the way the token row of the FLUX.2 panel does.
/// The value is trimmed; an empty or whitespace-only token is refused rather than
/// stored, because an empty entry and a missing entry would then mean the same thing
/// to every reader.
///
/// # Errors
/// Returns a localized message when the token is empty, when the build has no usable store,
/// or when the secret store refuses the write. No message ever carries the token itself.
pub fn store_hf_token(token: &str) -> Result<(), String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(t!("hf_token.empty_error").to_string());
    }
    backend::set(HF_TOKEN_SERVICE, HF_TOKEN_ACCOUNT, trimmed).map_err(|failure| failure.into_message(|err| tf!("hf_token.store_error", err = err)))?;
    set_cached_hf_token(Some(trimmed.to_string()));
    Ok(())
}

/// Deletes the token from the secret store and empties the cache.
///
/// BLOCKING, like [`store_hf_token`]. Deleting a token that is not there succeeds: the
/// user asked for "no token stored", and that is the state either way.
///
/// # Errors
/// Returns a localized message when the build has no usable store or the secret store
/// refuses the deletion.
pub fn clear_hf_token() -> Result<(), String> {
    backend::delete(HF_TOKEN_SERVICE, HF_TOKEN_ACCOUNT).map_err(|failure| failure.into_message(|err| tf!("hf_token.delete_error", err = err)))?;
    set_cached_hf_token(Some(String::new()));
    Ok(())
}

/// Reads the token from the secret store and refreshes the cache.
///
/// BLOCKING, like [`store_hf_token`]. A missing entry is not an error: it answers with
/// an empty string, which is the honest "no token is stored". A store that cannot be used
/// is an error, so a caller cannot mistake "unavailable" for "the user has no token".
///
/// # Errors
/// Returns a localized message when the build has no usable store, or when the secret store
/// is unreachable or refuses the read (on Android also when the stored token can no longer
/// be decrypted). The cache is left untouched then, so a transient failure does not turn a
/// stored token into a missing one.
pub fn read_hf_token() -> Result<String, String> {
    let token = backend::get(HF_TOKEN_SERVICE, HF_TOKEN_ACCOUNT).map_err(|failure| failure.into_message(|err| tf!("hf_token.read_error", err = err)))?.unwrap_or_default();
    let token = token.trim().to_string();
    set_cached_hf_token(Some(token.clone()));
    Ok(token)
}

/// Seeds the process-wide cache from the secret store, on a worker thread.
///
/// Called once at startup. It is deliberately fire-and-forget: until the answer lands
/// the cache reports [`HfTokenState::Unknown`], which every surface renders as "not
/// known" rather than as "no token". A failure is logged WITHOUT the token and leaves
/// the cache unseeded, so the next explicit read can still succeed. A build without a store
/// (web) logs that once and spawns nothing.
pub fn seed_hf_token_from_secret_store() {
    if let Some(unavailable) = backend::UNAVAILABLE {
        ms_log::runtime_log::log_info(format!("[startup] Hugging Face token not seeded: {}", unavailable()));
        return;
    }
    ms_thread::spawn(|| {
        if let Err(err) = read_hf_token() {
            // The message carries the store's own error, never the token.
            ms_log::runtime_log::log_warn(format!("[startup] Hugging Face token could not be read from the secret store: {err}"));
        }
    });
}

/// Why one backend operation failed, before it becomes the caller's localized message.
#[derive(Debug)]
enum StoreFailure {
    /// The final localized message (no usable store, or the store could not be opened).
    Message(String),
    /// The operation itself failed; the store's own error text, never the token.
    Operation(String),
}

impl StoreFailure {
    /// The localized message: `Message` as is, `Operation` through `operation_message` (the
    /// operation's own `hf_token.*_error` sentence around the store's error text).
    fn into_message(self, operation_message: impl FnOnce(&str) -> String) -> String {
        match self {
            StoreFailure::Message(message) => message,
            StoreFailure::Operation(detail) => operation_message(&detail),
        }
    }
}

// The one per-target choice of this file: which store holds the token. Each backend answers
// `get` (`Ok(None)` = no entry), `set` and `delete` (a missing entry is success), and
// `UNAVAILABLE` (`Some(message)` on a build without any store).

/// Desktop: the OS secret store through `keyring`.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
mod backend {
    use super::StoreFailure;

    /// Desktop always has a store; failures surface per operation.
    pub(super) const UNAVAILABLE: Option<fn() -> String> = None;

    fn entry(service: &str, account: &str) -> Result<keyring::Entry, StoreFailure> {
        keyring::Entry::new(service, account).map_err(|err| StoreFailure::Message(tf!("hf_token.keyring_unavailable_error", err = err)))
    }

    pub(super) fn get(service: &str, account: &str) -> Result<Option<String>, StoreFailure> {
        match entry(service, account)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(StoreFailure::Operation(err.to_string())),
        }
    }

    pub(super) fn set(service: &str, account: &str, secret: &str) -> Result<(), StoreFailure> {
        entry(service, account)?.set_password(secret).map_err(|err| StoreFailure::Operation(err.to_string()))
    }

    pub(super) fn delete(service: &str, account: &str) -> Result<(), StoreFailure> {
        match entry(service, account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(StoreFailure::Operation(err.to_string())),
        }
    }
}

/// Android: the Keystore-backed `ms_platform::credentials` store (`keyring` has no Android
/// backend and would silently link its in-memory mock, so it is compiled out). An APK without
/// the bridge answers the localized `Unavailable { CredentialStore, BridgeMissing }` message;
/// an undecryptable entry is a read error, never "no token".
#[cfg(target_os = "android")]
mod backend {
    use super::StoreFailure;
    use ms_platform::credentials::{self, CredentialError};

    /// The store exists (the bridge is probed per operation).
    pub(super) const UNAVAILABLE: Option<fn() -> String> = None;

    fn failure(err: CredentialError) -> StoreFailure {
        match err {
            CredentialError::Unavailable(unavailable) => StoreFailure::Message(unavailable.user_message()),
            CredentialError::Unreadable { .. } | CredentialError::Backend { .. } => StoreFailure::Operation(err.to_string()),
        }
    }

    pub(super) fn get(service: &str, account: &str) -> Result<Option<String>, StoreFailure> {
        credentials::read_blocking(service, account).map_err(failure)
    }

    pub(super) fn set(service: &str, account: &str, secret: &str) -> Result<(), StoreFailure> {
        credentials::store_blocking(service, account, secret).map_err(failure)
    }

    pub(super) fn delete(service: &str, account: &str) -> Result<(), StoreFailure> {
        credentials::delete_blocking(service, account).map_err(failure)
    }
}

/// Web: no secret store in the browser; every operation answers the capability table's
/// localized `Unavailable { CredentialStore }` message while the cache still works.
#[cfg(target_arch = "wasm32")]
mod backend {
    use super::StoreFailure;

    /// The localized message of the capability table for the missing store.
    fn message() -> String {
        let store = ms_platform::PlatformFeature::CredentialStore;
        ms_platform::Unavailable::new(store, store.unavailable_reason()).user_message()
    }

    pub(super) const UNAVAILABLE: Option<fn() -> String> = Some(message);

    pub(super) fn get(_service: &str, _account: &str) -> Result<Option<String>, StoreFailure> {
        Err(StoreFailure::Message(message()))
    }

    pub(super) fn set(_service: &str, _account: &str, _secret: &str) -> Result<(), StoreFailure> {
        Err(StoreFailure::Message(message()))
    }

    pub(super) fn delete(_service: &str, _account: &str) -> Result<(), StoreFailure> {
        Err(StoreFailure::Message(message()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache is a process-wide global, so the three state cases are checked in one
    /// test rather than in three that could interleave.
    #[test]
    fn the_cache_reports_three_distinct_states() {
        set_cached_hf_token(None);
        assert_eq!(hf_token_state(), HfTokenState::Unknown);
        // "Not read yet" must still hand callers an empty token: an empty `hf_token`
        // is what makes the backend answer `no_token` without a network call.
        assert!(hf_token().is_empty());

        set_cached_hf_token(Some(String::new()));
        assert_eq!(hf_token_state(), HfTokenState::Missing);
        assert!(hf_token().is_empty());

        set_cached_hf_token(Some("hf_example".to_string()));
        assert_eq!(hf_token_state(), HfTokenState::Stored);
        assert_eq!(hf_token(), "hf_example");

        // Whitespace is not a token: a pasted "\n" must read as "nothing is stored".
        set_cached_hf_token(Some("  \n ".to_string()));
        assert_eq!(hf_token_state(), HfTokenState::Missing);

        // Leave the global in the state every other test expects.
        set_cached_hf_token(None);
    }
}
