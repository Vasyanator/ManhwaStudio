/*
File: crates/ms-ai-api/src/keys.rs

Purpose:
API keys of the AI API services, stored ONLY in the OS credential store (`keyring`), never in
a project or user settings file. One entry per target: credential-store service name
`KEYRING_SERVICE`; user name `AiApiService::key()` for a hosted service (frozen), and
`"{service.key()}@{normalized base URL}"` for a compatible service, so a stored key is only ever
sent to the exact server it was saved for.

Key functions:
- read_api_key()  : `Ok("")` when no key is stored (for a compatible service: for that URL).
- store_api_key() : trims, rejects an empty key.
- clear_api_key() : deleting a missing key is success.

Notes:
Every call is blocking credential-store I/O: never call it on the GUI thread. The web build
has no credential store; each function there returns `AiApiError::KeyStoreWebUnavailable`.
A key is never logged; failures are logged with the service and the store's error text.
*/

use crate::error::AiApiError;
use crate::target::AiApiTarget;
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
pub fn store_api_key(_target: &AiApiTarget, _api_key: &str) -> Result<(), AiApiError> {
    Err(AiApiError::KeyStoreWebUnavailable)
}

/// Stores the trimmed `api_key` for `target` (its service, plus its base URL for a compatible
/// service), replacing any previous key of that target.
///
/// # Errors
/// `EmptyKey` for an empty/whitespace key, `KeyringUnavailable` when the store cannot be
/// opened, `StoreKey` when the write fails.
#[cfg(not(target_arch = "wasm32"))]
pub fn store_api_key(target: &AiApiTarget, api_key: &str) -> Result<(), AiApiError> {
    let service = target.service();
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err(AiApiError::EmptyKey);
    }
    keyring_entry(target)?.set_password(trimmed).map_err(|err| {
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
pub fn clear_api_key(_target: &AiApiTarget) -> Result<(), AiApiError> {
    Err(AiApiError::KeyStoreWebUnavailable)
}

/// Deletes the stored key of `target`; a target without a stored key is success. For a
/// compatible service only the key of that base URL is deleted.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened, `DeleteKey` when the delete fails.
#[cfg(not(target_arch = "wasm32"))]
pub fn clear_api_key(target: &AiApiTarget) -> Result<(), AiApiError> {
    let service = target.service();
    match keyring_entry(target)?.delete_credential() {
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
pub fn read_api_key(_target: &AiApiTarget) -> Result<String, AiApiError> {
    Err(AiApiError::KeyStoreWebUnavailable)
}

/// Reads the stored key of `target`, untrimmed; `Ok("")` when no key is stored. For a
/// compatible service that is the key saved for exactly this normalized base URL: a key saved
/// for another URL is never returned. The returned secret must never be logged or persisted.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened, `ReadKey` when the read fails for a
/// reason other than a missing entry.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_api_key(target: &AiApiTarget) -> Result<String, AiApiError> {
    let service = target.service();
    match keyring_entry(target)?.get_password() {
        Ok(key) => Ok(key),
        Err(keyring::Error::NoEntry) => Ok(String::new()),
        Err(err) => {
            runtime_log::log_error(format!("[AI API] failed to read the API key for {} from the OS credential store: {err}", service.label()));
            Err(AiApiError::ReadKey { service, detail: err.to_string() })
        }
    }
}

/// Opens the credential-store entry `(KEYRING_SERVICE, credential_user(target))`.
#[cfg(not(target_arch = "wasm32"))]
fn keyring_entry(target: &AiApiTarget) -> Result<keyring::Entry, AiApiError> {
    keyring::Entry::new(KEYRING_SERVICE, &credential_user(target)).map_err(|err| {
        runtime_log::log_error(format!("[AI API] OS credential store unavailable for {}: {err}", target.service().label()));
        AiApiError::KeyringUnavailable { detail: err.to_string() }
    })
}

/// The credential-store user name of `target`: `service.key()` for a hosted service (a frozen
/// persistence contract), `"{service.key()}@{endpoint}"` for a compatible service. The endpoint
/// is `AiApiTarget`'s normalized base URL, which a compatible target always carries, so two
/// spellings of one server share a key and two servers never do.
#[cfg(not(target_arch = "wasm32"))]
fn credential_user(target: &AiApiTarget) -> String {
    let service = target.service();
    match target.endpoint() {
        Some(endpoint) => format!("{}@{endpoint}", service.key()),
        None => service.key().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::KEYRING_SERVICE;
    #[cfg(not(target_arch = "wasm32"))]
    use super::credential_user;
    #[cfg(not(target_arch = "wasm32"))]
    use crate::service::AiApiService;
    #[cfg(not(target_arch = "wasm32"))]
    use crate::target::AiApiTarget;

    #[cfg(not(target_arch = "wasm32"))]
    fn target(service: AiApiService, base_url: &str) -> AiApiTarget {
        AiApiTarget::new(service, base_url).unwrap_or_else(|error| panic!("test target {service:?} {base_url:?}: {error:?}"))
    }

    // Persistence contract: changing this name orphans every key users already saved.
    #[test]
    fn keyring_service_name_is_frozen() {
        assert_eq!(KEYRING_SERVICE, "ManhwaStudio AI API OCR");
    }

    // Persistence contract: hosted services keep the user names users' saved keys live under,
    // whatever base URL is left over in the settings.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn hosted_user_name_is_the_frozen_service_key() {
        for service in AiApiService::ALL.into_iter().filter(|service| !service.uses_base_url()) {
            assert_eq!(credential_user(&target(service, "http://127.0.0.1:8080")), service.key(), "{service:?}");
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn compatible_user_name_is_bound_to_the_normalized_endpoint() {
        let local = target(AiApiService::OpenAiCompatible, " HTTP://127.0.0.1:8080 ");
        assert_eq!(credential_user(&local), "openai_compatible@http://127.0.0.1:8080/v1/");
        assert_eq!(credential_user(&target(AiApiService::OpenAiCompatible, "http://127.0.0.1:8080/v1")), credential_user(&local));
        assert_ne!(credential_user(&target(AiApiService::OpenAiCompatible, "http://127.0.0.1:9090")), credential_user(&local));
        assert_eq!(credential_user(&target(AiApiService::AnthropicCompatible, "https://proxy.example/api")), "anthropic_compatible@https://proxy.example/api/");
    }
}
