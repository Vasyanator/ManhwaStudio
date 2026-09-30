/*
File: crates/ms-sysprobe/src/hf_token.rs (re-exported as `crate::hf_token`)

Purpose:
The process-wide Hugging Face access token. ONE value for the whole application: the
FLUX.2 klein model-download block is its first UI surface, but the token is not that
block's property — any feature that has to fetch from a gated Hugging Face repository
reads the same value, so it lives here rather than inside an engine.

Storage:
The token is a CREDENTIAL and is kept in the OS secret store (`keyring`) under the
service name `"ManhwaStudio Hugging Face"`, exactly as the AI API keys are kept under
`"ManhwaStudio AI API OCR"` (`crates/ms-tab-translation/src/ocr.rs`). It is NEVER written to
`user_config.json`, to any settings JSON, or to a log. The OCR entry is deliberately
NOT reused or generalized: that one belongs to the translation tab and is keyed by
service, while this one is a single global value.

Shape:
A runtime global with free get/set/clear functions, in the shape of
`crates/ms-config/src/rotation_ctrl_wheel.rs`: a cached value behind an `RwLock`,
seeded once at startup (`seed_hf_token_from_secret_store`, called by `main.rs`, reads
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
Every message in this file says "the token" and interpolates only the keyring's own
error, which never carries the secret.
The keyring does not exist in the browser, so the wasm build gets the same stubs
`ocr.rs` uses: the store operations fail with a clear "unavailable on web" error while
the cache still works, so a wasm build compiles and behaves honestly.
*/

use std::sync::RwLock;

/// Service name of this token's entry in the OS secret store.
///
/// Its own name, never shared with `"ManhwaStudio AI API OCR"`: the two are different
/// credentials with different lifetimes, and a shared entry would let deleting one
/// silently delete the other.
#[cfg(not(target_arch = "wasm32"))]
const HF_TOKEN_SERVICE: &str = "ManhwaStudio Hugging Face";

/// Account name inside that service. There is exactly one Hugging Face token per
/// installation, so the account is a constant rather than a user identity.
#[cfg(not(target_arch = "wasm32"))]
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

/// Web stub: there is no OS credential store in the browser, so storing the token is
/// rejected with a clear error rather than silently dropped.
///
/// # Errors
/// Always returns the "unavailable on web" message.
#[cfg(target_arch = "wasm32")]
pub fn store_hf_token(_token: &str) -> Result<(), String> {
    Err(t!("hf_token.keystore_web_unavailable_error").to_string())
}

/// Writes `token` into the OS secret store and refreshes the cache.
///
/// BLOCKING: the keyring is an OS round trip. Never call this on the GUI thread —
/// spawn it (`ms_thread::spawn`), the way the token row of the FLUX.2 panel does.
/// The value is trimmed; an empty or whitespace-only token is refused rather than
/// stored, because an empty entry and a missing entry would then mean the same thing
/// to every reader.
///
/// # Errors
/// Returns a localized message when the token is empty or when the secret store
/// refuses the write. Neither message ever carries the token itself.
#[cfg(not(target_arch = "wasm32"))]
pub fn store_hf_token(token: &str) -> Result<(), String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(t!("hf_token.empty_error").to_string());
    }
    hf_keyring_entry()?
        .set_password(trimmed)
        .map_err(|err| tf!("hf_token.store_error", err = err))?;
    set_cached_hf_token(Some(trimmed.to_string()));
    Ok(())
}

/// Web stub: no OS credential store on the browser build.
///
/// # Errors
/// Always returns the "unavailable on web" message.
#[cfg(target_arch = "wasm32")]
pub fn clear_hf_token() -> Result<(), String> {
    Err(t!("hf_token.keystore_web_unavailable_error").to_string())
}

/// Deletes the token from the OS secret store and empties the cache.
///
/// BLOCKING, like [`store_hf_token`]. Deleting a token that is not there succeeds: the
/// user asked for "no token stored", and that is the state either way.
///
/// # Errors
/// Returns a localized message when the secret store refuses the deletion.
#[cfg(not(target_arch = "wasm32"))]
pub fn clear_hf_token() -> Result<(), String> {
    match hf_keyring_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            set_cached_hf_token(Some(String::new()));
            Ok(())
        }
        Err(err) => Err(tf!("hf_token.delete_error", err = err)),
    }
}

/// Web stub: no OS credential store on the browser build. An error rather than an empty
/// string, so a caller cannot mistake "unavailable" for "the user has no token".
///
/// # Errors
/// Always returns the "unavailable on web" message.
#[cfg(target_arch = "wasm32")]
pub fn read_hf_token() -> Result<String, String> {
    Err(t!("hf_token.keystore_web_unavailable_error").to_string())
}

/// Reads the token from the OS secret store and refreshes the cache.
///
/// BLOCKING, like [`store_hf_token`]. A missing entry is not an error: it answers with
/// an empty string, which is the honest "no token is stored".
///
/// # Errors
/// Returns a localized message when the secret store is unreachable or refuses the
/// read. The cache is left untouched then, so a transient failure does not turn a
/// stored token into a missing one.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_hf_token() -> Result<String, String> {
    let token = match hf_keyring_entry()?.get_password() {
        Ok(token) => token,
        Err(keyring::Error::NoEntry) => String::new(),
        Err(err) => return Err(tf!("hf_token.read_error", err = err)),
    };
    set_cached_hf_token(Some(token.trim().to_string()));
    Ok(token.trim().to_string())
}

/// Builds this token's secret-store entry.
///
/// # Errors
/// Returns a localized message when the platform has no usable credential store.
#[cfg(not(target_arch = "wasm32"))]
fn hf_keyring_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(HF_TOKEN_SERVICE, HF_TOKEN_ACCOUNT)
        .map_err(|err| tf!("hf_token.keyring_unavailable_error", err = err))
}

/// Seeds the process-wide cache from the OS secret store, on a worker thread.
///
/// Called once at startup. It is deliberately fire-and-forget: until the answer lands
/// the cache reports [`HfTokenState::Unknown`], which every surface renders as "not
/// known" rather than as "no token". A failure is logged WITHOUT the token and leaves
/// the cache unseeded, so the next explicit read can still succeed.
#[cfg(not(target_arch = "wasm32"))]
pub fn seed_hf_token_from_secret_store() {
    ms_thread::spawn(|| {
        if let Err(err) = read_hf_token() {
            // The message carries the keyring's own error, never the token.
            ms_log::runtime_log::log_warn(format!(
                "[startup] Hugging Face token could not be read from the secret store: {err}"
            ));
        }
    });
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
