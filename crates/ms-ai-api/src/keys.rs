/*
File: crates/ms-ai-api/src/keys.rs

Purpose:
API keys of the AI API services, stored ONLY in the OS credential store (`keyring`), never in
a project or user settings file. One entry per target: credential-store service name
`KEYRING_SERVICE`; user name `AiApiService::key()` for a hosted service (frozen), and
`"{service.key()}@{normalized base URL}"` for a compatible service, so a stored key is only ever
sent to the exact server it was saved for.

Named slots: a key that belongs to no `AiApiTarget` (an image-edit provider such as BFL or a
region-bound DashScope account) lives in the same credential-store service under a
`NamedKeyUser`, whose user name `"image_edit:{provider_key}"` /
`"image_edit:{provider_key}@{region}"` is a frozen persistence contract built only here. The
`image_edit:` prefix contains a `:`, which no target user name does, so the two families never
collide.

Key functions:
- read_api_key()  : `Ok("")` when no key is stored (for a compatible service: for that URL).
- store_api_key() : trims, rejects an empty key.
- clear_api_key() : deleting a missing key is success.
- read_named_key() / store_named_key() / clear_named_key(): the same contracts for a
  `NamedKeyUser`.

Notes:
Every call is blocking credential-store I/O: never call it on the GUI thread. The web build
has no credential store; each function there returns `AiApiError::KeyStoreWebUnavailable`.
Android has none either until a Keystore-backed store exists (`keyring` would silently link
its in-memory mock there, so it is compiled out); each function there returns
`AiApiError::KeyStoreUnavailable` carrying `ms_platform::Unavailable { CredentialStore }`.
A key is never logged; failures are logged with the service and the store's error text.
*/

use crate::error::AiApiError;
use crate::target::AiApiTarget;
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
use ms_log::runtime_log;

/// Credential-store service name of every AI API key. A persistence contract: users' saved
/// keys are addressed by it, and it is shared by OCR and machine translation despite the
/// historical "OCR" in the name. Never change it.
pub const KEYRING_SERVICE: &str = "ManhwaStudio AI API OCR";

/// Credential-store user-name prefix of every image-edit named slot. A persistence contract:
/// never change it.
const IMAGE_EDIT_USER_PREFIX: &str = "image_edit:";

/// The credential-store user name of a named key slot (a key bound to no `AiApiTarget`). Built
/// only by its constructors, so every named user name has one frozen shape; the user name is
/// not a secret and may be logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedKeyUser(String);

impl NamedKeyUser {
    /// The slot of image-edit provider `provider_key` (its frozen catalogue id):
    /// `"image_edit:{provider_key}"`, or `"image_edit:{provider_key}@{region}"` for a provider
    /// whose keys are bound to one region (`region` is that region's frozen id). Both ids are
    /// persistence contracts of the caller's catalogue and must be non-empty and free of `@`.
    #[must_use]
    pub fn image_edit(provider_key: &'static str, region: Option<&'static str>) -> Self {
        match region {
            Some(region) => Self(format!("{IMAGE_EDIT_USER_PREFIX}{provider_key}@{region}")),
            None => Self(format!("{IMAGE_EDIT_USER_PREFIX}{provider_key}")),
        }
    }

    /// The exact credential-store user name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The error of every key-store call on the web build, which has no credential store.
#[cfg(target_arch = "wasm32")]
fn key_store_unavailable() -> AiApiError {
    AiApiError::KeyStoreWebUnavailable
}

/// The error of every key-store call on Android: the capability table's typed answer for
/// `PlatformFeature::CredentialStore`. Should the table ever report a store there, this build
/// still compiled `keyring` out, so the bridge to that store is what is missing.
#[cfg(target_os = "android")]
fn key_store_unavailable() -> AiApiError {
    use ms_platform::{PlatformFeature, Unavailable, UnavailableReason};
    match PlatformFeature::CredentialStore.require() {
        Err(unavailable) => AiApiError::KeyStoreUnavailable(unavailable),
        Ok(()) => AiApiError::KeyStoreUnavailable(Unavailable::new(PlatformFeature::CredentialStore, UnavailableReason::BridgeMissing)),
    }
}

/// Stub for builds without an OS credential store (wasm, Android): storing an API key is
/// rejected with a clear error.
///
/// # Errors
/// Always the no-credential-store error (see `key_store_unavailable`).
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn store_api_key(_target: &AiApiTarget, _api_key: &str) -> Result<(), AiApiError> {
    Err(key_store_unavailable())
}

/// Stores the trimmed `api_key` for `target` (its service, plus its base URL for a compatible
/// service), replacing any previous key of that target.
///
/// # Errors
/// `EmptyKey` for an empty/whitespace key, `KeyringUnavailable` when the store cannot be
/// opened, `StoreKey` when the write fails.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
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

/// Stub for builds without an OS credential store (wasm, Android): clearing a key is
/// rejected with a clear error.
///
/// # Errors
/// Always the no-credential-store error (see `key_store_unavailable`).
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn clear_api_key(_target: &AiApiTarget) -> Result<(), AiApiError> {
    Err(key_store_unavailable())
}

/// Deletes the stored key of `target`; a target without a stored key is success. For a
/// compatible service only the key of that base URL is deleted.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened, `DeleteKey` when the delete fails.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
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

/// Stub for builds without an OS credential store (wasm, Android). Returns a clear error
/// so callers (OCR warmup, MT run, metadata) surface "credential store unavailable" rather
/// than treating a missing key as an empty one.
///
/// # Errors
/// Always the no-credential-store error (see `key_store_unavailable`).
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn read_api_key(_target: &AiApiTarget) -> Result<String, AiApiError> {
    Err(key_store_unavailable())
}

/// Reads the stored key of `target`, untrimmed; `Ok("")` when no key is stored. For a
/// compatible service that is the key saved for exactly this normalized base URL: a key saved
/// for another URL is never returned. The returned secret must never be logged or persisted.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened, `ReadKey` when the read fails for a
/// reason other than a missing entry.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
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
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn keyring_entry(target: &AiApiTarget) -> Result<keyring::Entry, AiApiError> {
    open_entry(&credential_user(target), target.service().label())
}

/// Opens the credential-store entry `(KEYRING_SERVICE, user)`; `label` names the key's owner
/// (service or provider) in the failure log.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn open_entry(user: &str, label: &str) -> Result<keyring::Entry, AiApiError> {
    keyring::Entry::new(KEYRING_SERVICE, user).map_err(|err| {
        runtime_log::log_error(format!("[AI API] OS credential store unavailable for {label}: {err}"));
        AiApiError::KeyringUnavailable { detail: err.to_string() }
    })
}

/// Stub for builds without an OS credential store (wasm, Android).
///
/// # Errors
/// Always the no-credential-store error (see `key_store_unavailable`).
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn store_named_key(_user: &NamedKeyUser, _label: &'static str, _api_key: &str) -> Result<(), AiApiError> {
    Err(key_store_unavailable())
}

/// Stores the trimmed `api_key` in the named slot `user`, replacing any previous key there.
/// `label` is the provider's display name, used in the error and the failure log.
///
/// # Errors
/// `EmptyKey` for an empty/whitespace key, `KeyringUnavailable` when the store cannot be
/// opened, `StoreNamedKey` when the write fails.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn store_named_key(user: &NamedKeyUser, label: &'static str, api_key: &str) -> Result<(), AiApiError> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err(AiApiError::EmptyKey);
    }
    open_entry(user.as_str(), label)?.set_password(trimmed).map_err(|err| {
        runtime_log::log_error(format!("[AI API] failed to store the API key for {label} (slot {}) in the OS credential store: {err}", user.as_str()));
        AiApiError::StoreNamedKey { label, detail: err.to_string() }
    })
}

/// Stub for builds without an OS credential store (wasm, Android).
///
/// # Errors
/// Always the no-credential-store error (see `key_store_unavailable`).
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn clear_named_key(_user: &NamedKeyUser, _label: &'static str) -> Result<(), AiApiError> {
    Err(key_store_unavailable())
}

/// Deletes the key of the named slot `user`; a slot without a stored key is success.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened, `DeleteNamedKey` when the delete fails.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn clear_named_key(user: &NamedKeyUser, label: &'static str) -> Result<(), AiApiError> {
    match open_entry(user.as_str(), label)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => {
            runtime_log::log_error(format!("[AI API] failed to delete the API key for {label} (slot {}) from the OS credential store: {err}", user.as_str()));
            Err(AiApiError::DeleteNamedKey { label, detail: err.to_string() })
        }
    }
}

/// Stub for builds without an OS credential store (wasm, Android), so a missing store is never
/// mistaken for a missing key.
///
/// # Errors
/// Always the no-credential-store error (see `key_store_unavailable`).
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn read_named_key(_user: &NamedKeyUser, _label: &'static str) -> Result<String, AiApiError> {
    Err(key_store_unavailable())
}

/// Reads the key of the named slot `user`, untrimmed; `Ok("")` when no key is stored. The
/// returned secret must never be logged or persisted.
///
/// # Errors
/// `KeyringUnavailable` when the store cannot be opened, `ReadNamedKey` when the read fails for
/// a reason other than a missing entry.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn read_named_key(user: &NamedKeyUser, label: &'static str) -> Result<String, AiApiError> {
    match open_entry(user.as_str(), label)?.get_password() {
        Ok(key) => Ok(key),
        Err(keyring::Error::NoEntry) => Ok(String::new()),
        Err(err) => {
            runtime_log::log_error(format!("[AI API] failed to read the API key for {label} (slot {}) from the OS credential store: {err}", user.as_str()));
            Err(AiApiError::ReadNamedKey { label, detail: err.to_string() })
        }
    }
}

/// The credential-store user name of `target`: `service.key()` for a hosted service (a frozen
/// persistence contract), `"{service.key()}@{endpoint}"` for a compatible service. The endpoint
/// is `AiApiTarget`'s normalized base URL, which a compatible target always carries, so two
/// spellings of one server share a key and two servers never do.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn credential_user(target: &AiApiTarget) -> String {
    let service = target.service();
    match target.endpoint() {
        Some(endpoint) => format!("{}@{endpoint}", service.key()),
        None => service.key().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{KEYRING_SERVICE, NamedKeyUser};
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    use super::credential_user;
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    use crate::service::AiApiService;
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    use crate::target::AiApiTarget;

    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
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
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    #[test]
    fn hosted_user_name_is_the_frozen_service_key() {
        for service in AiApiService::ALL.into_iter().filter(|service| !service.uses_base_url()) {
            assert_eq!(credential_user(&target(service, "http://127.0.0.1:8080")), service.key(), "{service:?}");
        }
    }

    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    #[test]
    fn compatible_user_name_is_bound_to_the_normalized_endpoint() {
        let local = target(AiApiService::OpenAiCompatible, " HTTP://127.0.0.1:8080 ");
        assert_eq!(credential_user(&local), "openai_compatible@http://127.0.0.1:8080/v1/");
        assert_eq!(credential_user(&target(AiApiService::OpenAiCompatible, "http://127.0.0.1:8080/v1")), credential_user(&local));
        assert_ne!(credential_user(&target(AiApiService::OpenAiCompatible, "http://127.0.0.1:9090")), credential_user(&local));
        assert_eq!(credential_user(&target(AiApiService::AnthropicCompatible, "https://proxy.example/api")), "anthropic_compatible@https://proxy.example/api/");
    }

    // Persistence contract: changing a named user name orphans every image-edit key users saved.
    #[test]
    fn named_user_names_are_frozen() {
        assert_eq!(NamedKeyUser::image_edit("bfl", None).as_str(), "image_edit:bfl");
        assert_eq!(NamedKeyUser::image_edit("dashscope", Some("intl")).as_str(), "image_edit:dashscope@intl");
        assert_ne!(NamedKeyUser::image_edit("dashscope", Some("intl")), NamedKeyUser::image_edit("dashscope", Some("cn")));
        assert_ne!(NamedKeyUser::image_edit("dashscope", Some("intl")), NamedKeyUser::image_edit("dashscope", None));
    }

    // A named slot never aliases a target slot: target user names are `service.key()` or
    // `"{service.key()}@{url}"`, and no service key contains the `:` of the named prefix.
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    #[test]
    fn named_user_names_never_collide_with_target_user_names() {
        for service in AiApiService::ALL {
            assert!(!service.key().contains(':'), "{service:?}");
            assert!(!service.key().starts_with("image_edit"), "{service:?}");
        }
        let compatible = credential_user(&target(AiApiService::OpenAiCompatible, "http://127.0.0.1:8080"));
        assert!(!compatible.starts_with(NamedKeyUser::image_edit("openai_compatible", None).as_str()));
    }
}
