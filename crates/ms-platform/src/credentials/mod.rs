/*
File: credentials/mod.rs

Purpose:
The Android credential-store seam: read / store / delete one secret (API key, access token)
addressed by a (service, account) name, with a typed error. The Android backend is the Kotlin
`CredentialBridge` (AES-256-GCM key in the AndroidKeyStore, encrypted entries in app-private
storage); see `android.rs` and the Kotlin file header for the wire contract.

Key items:
- CredentialError: typed failure; `Display` is the localized user message.
- read_blocking() / store_blocking() / delete_blocking(): the blocking operations (Android).
- entry_name(): the frozen, collision-free entry name of a (service, account) pair.
- decode_read_answer(): pure decoding of the Kotlin `read` answer.

Notes:
The module exists only on Android (and in host tests of its pure half). Desktop builds keep the
OS credential store through `keyring` in the consumers (`ms_ai_api::keys`,
`ms_sysprobe::hf_token`), and the web build has no store, so this seam answers nothing there
rather than a fake "unavailable" on a desktop that has a store. Every operation is blocking
JNI + Keystore I/O: worker threads only, never the GUI thread. A secret never reaches a log
line, an error, or a `Debug` output of this module.
*/

#[cfg(target_os = "android")]
mod android;

use std::fmt;

use crate::Unavailable;

/// Failure of a credential-store operation. `Display` is the localized [`Self::user_message`];
/// no variant ever carries a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// The store is not usable in this build (the Kotlin bridge class is missing from the APK).
    Unavailable(Unavailable),
    /// An entry exists but cannot be decrypted (the Keystore key was lost or invalidated, or the
    /// stored value is malformed). Never reported as "no secret"; the entry is left in place
    /// until the user stores a new secret (which replaces it) or deletes it.
    Unreadable { detail: String },
    /// Any other failure of the store or of the JNI bridge.
    Backend { detail: String },
}

impl CredentialError {
    /// The i18n key of the message (the feature/reason sentences for `Unavailable`).
    const fn message_key(&self) -> &'static str {
        match self {
            CredentialError::Unavailable(unavailable) => unavailable.feature.message_key(),
            CredentialError::Unreadable { .. } => "platform.credentials.unreadable_error",
            CredentialError::Backend { .. } => "platform.credentials.backend_error",
        }
    }

    /// Localized message for a status line or toast (embedded by consumers as the `{err}` of
    /// their own "could not read the key" sentence).
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            CredentialError::Unavailable(unavailable) => unavailable.user_message(),
            CredentialError::Unreadable { detail } | CredentialError::Backend { detail } => {
                ms_i18n::interpolate(ms_i18n::resolve_key(self.message_key()), &[("detail", detail as &dyn fmt::Display)])
            }
        }
    }
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for CredentialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CredentialError::Unavailable(unavailable) => Some(unavailable),
            CredentialError::Unreadable { .. } | CredentialError::Backend { .. } => None,
        }
    }
}

/// Reads the secret stored for (`service`, `account`). `Ok(None)` = nothing is stored.
/// BLOCKING (JNI + Keystore): worker threads only.
///
/// # Errors
/// [`CredentialError::Unreadable`] when an entry exists but cannot be decrypted,
/// [`CredentialError::Unavailable`] (`CredentialStore`, `BridgeMissing`) when the APK lacks the
/// bridge, [`CredentialError::Backend`] for any other failure.
#[cfg(target_os = "android")]
pub fn read_blocking(service: &str, account: &str) -> Result<Option<String>, CredentialError> {
    android::read(&entry_name(service, account))
}

/// Stores `secret` for (`service`, `account`), replacing any previous value, including an
/// unreadable one (an explicit save is how the user repairs it). The secret is stored as given:
/// trimming and refusing empty secrets is the caller's contract. BLOCKING: worker threads only.
///
/// # Errors
/// [`CredentialError::Unavailable`] when the APK lacks the bridge, [`CredentialError::Backend`]
/// when the Keystore or the storage write fails.
#[cfg(target_os = "android")]
pub fn store_blocking(service: &str, account: &str, secret: &str) -> Result<(), CredentialError> {
    android::write(&entry_name(service, account), secret)
}

/// Deletes the secret of (`service`, `account`); nothing stored is success. BLOCKING: worker
/// threads only.
///
/// # Errors
/// [`CredentialError::Unavailable`] when the APK lacks the bridge, [`CredentialError::Backend`]
/// when the storage write fails.
#[cfg(target_os = "android")]
pub fn delete_blocking(service: &str, account: &str) -> Result<(), CredentialError> {
    android::delete(&entry_name(service, account))
}

/// Prefix of every entry name; the version lets a later scheme coexist with stored entries.
const ENTRY_NAME_PREFIX: &str = "v1.";

/// The store's entry name of (`service`, `account`): `"v1." + hex(service) + "." + hex(account)`
/// (lowercase hex of the UTF-8 bytes). A persistence contract shared with nothing else (Kotlin
/// treats it as opaque): changing it orphans every stored secret. Hex keeps the name to
/// `[0-9a-f.v]` and makes the pair boundary unambiguous, so two different pairs never share an
/// entry. Not a secret: may be logged.
pub(crate) fn entry_name(service: &str, account: &str) -> String {
    let mut name = String::with_capacity(ENTRY_NAME_PREFIX.len() + 2 * (service.len() + account.len()) + 1);
    name.push_str(ENTRY_NAME_PREFIX);
    push_hex(&mut name, service);
    name.push('.');
    push_hex(&mut name, account);
    name
}

/// Appends the lowercase hex of `text`'s UTF-8 bytes.
fn push_hex(out: &mut String, text: &str) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in text.bytes() {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
}

/// Status codes of the Kotlin `read` answer (`CredentialBridge.STATUS_*`).
const STATUS_OK: &str = "ok";
const STATUS_MISSING: &str = "missing";
const STATUS_UNREADABLE: &str = "unreadable";
const STATUS_FAILED: &str = "failed";

/// Decodes the Kotlin `read` answer: `{"ok", secret}` -> `Ok(Some)`, `{"missing"}` -> `Ok(None)`,
/// `{"unreadable", detail}` / `{"failed", detail}` -> the typed error. Any other shape is a
/// `Backend` error whose detail names only the shape, never a field value (one of them could be
/// the secret).
pub(crate) fn decode_read_answer(answer: Vec<String>) -> Result<Option<String>, CredentialError> {
    let len = answer.len();
    let mut fields = answer.into_iter();
    let status = fields.next();
    let value = fields.next();
    match (status.as_deref(), value, len) {
        (Some(STATUS_OK), Some(secret), 2) => Ok(Some(secret)),
        (Some(STATUS_MISSING), None, 1) => Ok(None),
        (Some(STATUS_UNREADABLE), Some(detail), 2) => Err(CredentialError::Unreadable { detail }),
        (Some(STATUS_FAILED), Some(detail), 2) => Err(CredentialError::Backend { detail }),
        _ => Err(CredentialError::Backend { detail: format!("malformed credential bridge answer ({len} field(s))") }),
    }
}

#[cfg(test)]
mod tests;
