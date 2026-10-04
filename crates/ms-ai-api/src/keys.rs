/*
File: crates/ms-ai-api/src/keys.rs

Purpose:
API keys of the AI API services, stored ONLY in the OS credential store (`keyring`), never in
a project or user settings file. One entry per service: credential-store service name
`KEYRING_SERVICE`, user name `AiApiService::key()`.

Key functions:
- read_api_key()  : `Ok("")` when no key is stored.
- store_api_key() : trims, rejects an empty key.
- clear_api_key() : deleting a missing key is success.

Notes:
Every call is blocking credential-store I/O: never call it on the GUI thread. The web build
has no credential store; each function there returns `AiApiError::KeyStoreWebUnavailable`.
A key is never logged; failures are logged with the service and the store's error text.
*/

use crate::error::AiApiError;
use crate::service::AiApiService;
#[cfg(not(target_arch = "wasm32"))]
use ms_log::runtime_log;

/// Credential-store service name of every AI API key. A persistence contract: users' saved
/// keys are addressed by it, and it is shared by OCR and machine translation despite the
/// historical "OCR" in the name. Never change it.
pub const KEYRING_SERVICE: &str = "ManhwaStudio AI API OCR";

/// Web stub: the OS credential store (`keyring`) does not exist in the browser,
/// so storing an API key is rejected with a clear error.
///
/// # Errors
/// Always `AiApiError::KeyStoreWebUnavailable`.
#[cfg(target_arch = "wasm32")]
pub fn store_api_key(_service: AiApiService, _api_key: &str) -> Result<(), AiApiError> {
    Err(AiApiError::KeyStoreWebUnavailable)
}

/// Stores the trimmed `api_key` for `service`, replacing any previous key.
///
/// # Errors
/// `EmptyKey` for an empty/whitespace key, `KeyringUnavailable` when the store cannot be
/// opened, `StoreKey` when the write fails.
#[cfg(not(target_arch = "wasm32"))]
pub fn store_api_key(service: AiApiService, api_key: &str) -> Result<(), AiApiError> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err(AiApiError::EmptyKey);
    }
    keyring_entry(service)?.set_password(trimmed).map_err(|err| {
        runtime_log::log_error(format!("[AI API] failed to store the API key for {} in the OS credential store: {err}", service.label()));
        AiApiError::StoreKey { service, detail: err.to_string() }
    })
}

/// Web stub: no OS credential store on the browser build, so clearing a key is
/// rejected with a clear error.
///
/// # Errors
/// Always `AiApiError::KeyStoreWebUnavailable`.
#[cfg(target_arch = "wasm32")]
pub fn clear_api_key(_service: AiApiService) -> Result<(), AiApiError> {
    Err(AiApiError::KeyStoreWebUnavailable)
}

/// Deletes the stored key of `service`; a service without a stored key is success.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened, `DeleteKey` when the delete fails.
#[cfg(not(target_arch = "wasm32"))]
pub fn clear_api_key(service: AiApiService) -> Result<(), AiApiError> {
    match keyring_entry(service)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => {
            runtime_log::log_error(format!("[AI API] failed to delete the API key for {} from the OS credential store: {err}", service.label()));
            Err(AiApiError::DeleteKey { service, detail: err.to_string() })
        }
    }
}

/// Web stub: no OS credential store on the browser build. Returns a clear error
/// so callers (OCR warmup, MT run, metadata) surface "unavailable on web" rather
/// than treating a missing key as an empty one.
///
/// # Errors
/// Always `AiApiError::KeyStoreWebUnavailable`.
#[cfg(target_arch = "wasm32")]
pub fn read_api_key(_service: AiApiService) -> Result<String, AiApiError> {
    Err(AiApiError::KeyStoreWebUnavailable)
}

/// Reads the stored key of `service`, untrimmed; `Ok("")` when no key is stored. The
/// returned secret must never be logged or persisted.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened, `ReadKey` when the read fails for a
/// reason other than a missing entry.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_api_key(service: AiApiService) -> Result<String, AiApiError> {
    match keyring_entry(service)?.get_password() {
        Ok(key) => Ok(key),
        Err(keyring::Error::NoEntry) => Ok(String::new()),
        Err(err) => {
            runtime_log::log_error(format!("[AI API] failed to read the API key for {} from the OS credential store: {err}", service.label()));
            Err(AiApiError::ReadKey { service, detail: err.to_string() })
        }
    }
}

/// Opens the credential-store entry `(KEYRING_SERVICE, service.key())`.
#[cfg(not(target_arch = "wasm32"))]
fn keyring_entry(service: AiApiService) -> Result<keyring::Entry, AiApiError> {
    keyring::Entry::new(KEYRING_SERVICE, service.key()).map_err(|err| {
        runtime_log::log_error(format!("[AI API] OS credential store unavailable for {}: {err}", service.label()));
        AiApiError::KeyringUnavailable { detail: err.to_string() }
    })
}

#[cfg(test)]
mod tests {
    use super::KEYRING_SERVICE;

    // Persistence contract: changing this name orphans every key users already saved.
    #[test]
    fn keyring_service_name_is_frozen() {
        assert_eq!(KEYRING_SERVICE, "ManhwaStudio AI API OCR");
    }
}
