/*
File: crates/ms-ai-api/src/keys.rs

Purpose:
API keys of the AI API services, stored ONLY in the platform credential store, never in a
project or user settings file: the OS store through `keyring` on desktop, the Keystore-backed
`ms_platform::credentials` store on Android. One entry per target: credential-store service
name `KEYRING_SERVICE`; user name `AiApiService::key()` for a hosted service (frozen), and
`"{service.key()}@{normalized base URL}"` for a compatible service, so a stored key is only ever
sent to the exact server it was saved for.

Named slots: a key that belongs to no `AiApiTarget` (an image-edit provider such as BFL or a
region-bound DashScope account) lives in the same credential-store service under a
`NamedKeyUser`, whose user name `"image_edit:{provider_key}"` /
`"image_edit:{provider_key}@{region}"` is a frozen persistence contract built only here. The
NeuroLate cloud OCR key lives in the named slot `"neurolate:api"` (`NamedKeyUser::neurolate`),
frozen the same way. Both prefixes contain a `:`, which no target user name does, so named and
target slots never collide.

Key functions:
- read_api_key()  : `Ok("")` when no key is stored (for a compatible service: for that URL).
- store_api_key() : trims, rejects an empty key.
- clear_api_key() : deleting a missing key is success.
- read_named_key() / store_named_key() / clear_named_key(): the same contracts for a
  `NamedKeyUser`.

Notes:
Every call is blocking credential-store I/O: never call it on the GUI thread. The per-target
store is chosen in ONE place, the private `backend` module (desktop `keyring`, Android
`ms_platform::credentials`, web: none, each function returns
`AiApiError::KeyStoreWebUnavailable`); the public functions and the user-name rules are
target-neutral. On Android an APK without the Kotlin bridge answers
`AiApiError::KeyStoreUnavailable` (`Unavailable { CredentialStore, BridgeMissing }`), and an
entry that can no longer be decrypted (Keystore key lost) is a read error, never "no key".
"No key stored" is `Ok("")` on every store, so a keyless compatible endpoint works.
A key is never logged; failures are logged with the service and the store's error text.
*/

use crate::error::AiApiError;
use crate::target::AiApiTarget;
use ms_log::runtime_log;

/// Credential-store service name of every AI API key. A persistence contract: users' saved
/// keys are addressed by it, and it is shared by OCR and machine translation despite the
/// historical "OCR" in the name. Never change it.
pub const KEYRING_SERVICE: &str = "ManhwaStudio AI API OCR";

/// Credential-store user-name prefix of every image-edit named slot. A persistence contract:
/// never change it.
const IMAGE_EDIT_USER_PREFIX: &str = "image_edit:";

/// Credential-store user name of the `NeuroLate` API key (`ms_ai_api::neurolate`). A persistence
/// contract: never change it.
const NEUROLATE_USER: &str = "neurolate:api";

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

    /// The slot of the `NeuroLate` API key: `"neurolate:api"` (frozen).
    #[must_use]
    pub fn neurolate() -> Self {
        Self(NEUROLATE_USER.to_string())
    }

    /// The exact credential-store user name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stores the trimmed `api_key` for `target` (its service, plus its base URL for a compatible
/// service), replacing any previous key of that target.
///
/// # Errors
/// `EmptyKey` for an empty/whitespace key; otherwise the store's failure (`KeyringUnavailable` /
/// `KeyStoreUnavailable` / `KeyStoreWebUnavailable` when no store is usable): `StoreKey` when the write fails.
pub fn store_api_key(target: &AiApiTarget, api_key: &str) -> Result<(), AiApiError> {
    let service = target.service();
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err(AiApiError::EmptyKey);
    }
    backend::set(&credential_user(target), trimmed)
        .map_err(|failure| failure.into_error(service.label(), "store the API key", |detail| AiApiError::StoreKey { service, detail }))
}

/// Deletes the stored key of `target`; a target without a stored key is success. For a
/// compatible service only the key of that base URL is deleted.
///
/// # Errors
/// The store's failure (`KeyringUnavailable` /
/// `KeyStoreUnavailable` / `KeyStoreWebUnavailable` when no store is usable): `DeleteKey` when the delete fails.
pub fn clear_api_key(target: &AiApiTarget) -> Result<(), AiApiError> {
    let service = target.service();
    backend::delete(&credential_user(target))
        .map_err(|failure| failure.into_error(service.label(), "delete the API key", |detail| AiApiError::DeleteKey { service, detail }))
}

/// Reads the stored key of `target`, untrimmed; `Ok("")` when no key is stored. For a
/// compatible service that is the key saved for exactly this normalized base URL: a key saved
/// for another URL is never returned. The returned secret must never be logged or persisted.
/// A store that cannot be used is an error, never "no key", so callers cannot mistake it for a
/// keyless setup.
///
/// # Errors
/// The store's failure (`KeyringUnavailable` /
/// `KeyStoreUnavailable` / `KeyStoreWebUnavailable` when no store is usable): `ReadKey` when the read fails for a
/// reason other than a missing entry (on Android also when the stored key can no longer be
/// decrypted).
pub fn read_api_key(target: &AiApiTarget) -> Result<String, AiApiError> {
    let service = target.service();
    backend::get(&credential_user(target))
        .map(Option::unwrap_or_default)
        .map_err(|failure| failure.into_error(service.label(), "read the API key", |detail| AiApiError::ReadKey { service, detail }))
}

/// Stores the trimmed `api_key` in the named slot `user`, replacing any previous key there.
/// `label` is the provider's display name, used in the error and the failure log.
///
/// # Errors
/// `EmptyKey` for an empty/whitespace key; otherwise the store's failure (`KeyringUnavailable` /
/// `KeyStoreUnavailable` / `KeyStoreWebUnavailable` when no store is usable): `StoreNamedKey` when the write fails.
pub fn store_named_key(user: &NamedKeyUser, label: &'static str, api_key: &str) -> Result<(), AiApiError> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return Err(AiApiError::EmptyKey);
    }
    backend::set(user.as_str(), trimmed)
        .map_err(|failure| failure.into_error(&named_owner(user, label), "store the API key", |detail| AiApiError::StoreNamedKey { label, detail }))
}

/// Deletes the key of the named slot `user`; a slot without a stored key is success.
///
/// # Errors
/// The store's failure (`KeyringUnavailable` /
/// `KeyStoreUnavailable` / `KeyStoreWebUnavailable` when no store is usable): `DeleteNamedKey` when the delete
/// fails.
pub fn clear_named_key(user: &NamedKeyUser, label: &'static str) -> Result<(), AiApiError> {
    backend::delete(user.as_str())
        .map_err(|failure| failure.into_error(&named_owner(user, label), "delete the API key", |detail| AiApiError::DeleteNamedKey { label, detail }))
}

/// Reads the key of the named slot `user`, untrimmed; `Ok("")` when no key is stored. The
/// returned secret must never be logged or persisted.
///
/// # Errors
/// The store's failure (`KeyringUnavailable` /
/// `KeyStoreUnavailable` / `KeyStoreWebUnavailable` when no store is usable): `ReadNamedKey` when the read fails
/// for a reason other than a missing entry.
pub fn read_named_key(user: &NamedKeyUser, label: &'static str) -> Result<String, AiApiError> {
    backend::get(user.as_str())
        .map(Option::unwrap_or_default)
        .map_err(|failure| failure.into_error(&named_owner(user, label), "read the API key", |detail| AiApiError::ReadNamedKey { label, detail }))
}

/// The owner text of a named slot in failure logs: provider label plus the (non-secret) slot.
fn named_owner(user: &NamedKeyUser, label: &str) -> String {
    format!("{label} (slot {})", user.as_str())
}

/// The credential-store user name of `target`: `service.key()` for a hosted service (a frozen
/// persistence contract), `"{service.key()}@{endpoint}"` for a compatible service. The endpoint
/// is `AiApiTarget`'s normalized base URL, which a compatible target always carries, so two
/// spellings of one server share a key and two servers never do.
fn credential_user(target: &AiApiTarget) -> String {
    let service = target.service();
    match target.endpoint() {
        Some(endpoint) => format!("{}@{endpoint}", service.key()),
        None => service.key().to_string(),
    }
}

/// Why one backend operation failed, before it becomes the caller's typed error.
#[derive(Debug)]
enum StoreFailure {
    /// The store cannot be used (no store in this build, the desktop store could not be
    /// opened, the Android bridge is missing); already the final error.
    Store(AiApiError),
    /// The operation itself failed; the text is the store's own error, never the key.
    Operation(String),
}

impl StoreFailure {
    /// The caller's typed error: `Store` as is, `Operation` -> `failed(detail)`. Both are
    /// logged with `owner` (service label or provider + slot) and `action` (e.g. "read the API
    /// key"); never with the key.
    fn into_error(self, owner: &str, action: &str, failed: impl FnOnce(String) -> AiApiError) -> AiApiError {
        match self {
            StoreFailure::Store(error) => {
                runtime_log::log_error(format!("[AI API] credential store unavailable to {action} for {owner}: {error:?}"));
                error
            }
            StoreFailure::Operation(detail) => {
                runtime_log::log_error(format!("[AI API] failed to {action} for {owner} in the credential store: {detail}"));
                failed(detail)
            }
        }
    }
}

// The one per-target choice of this file: which store holds the entries. Each backend answers
// `get` (`Ok(None)` = no entry), `set` and `delete` (a missing entry is success) for one user
// name under `KEYRING_SERVICE`.

/// Desktop: the OS credential store through `keyring`.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
mod backend {
    use super::{KEYRING_SERVICE, StoreFailure};
    use crate::error::AiApiError;

    fn entry(user: &str) -> Result<keyring::Entry, StoreFailure> {
        keyring::Entry::new(KEYRING_SERVICE, user).map_err(|err| StoreFailure::Store(AiApiError::KeyringUnavailable { detail: err.to_string() }))
    }

    pub(super) fn get(user: &str) -> Result<Option<String>, StoreFailure> {
        match entry(user)?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(StoreFailure::Operation(err.to_string())),
        }
    }

    pub(super) fn set(user: &str, secret: &str) -> Result<(), StoreFailure> {
        entry(user)?.set_password(secret).map_err(|err| StoreFailure::Operation(err.to_string()))
    }

    pub(super) fn delete(user: &str) -> Result<(), StoreFailure> {
        match entry(user)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(StoreFailure::Operation(err.to_string())),
        }
    }
}

/// Android: the Keystore-backed store of `ms_platform::credentials` (`keyring` has no Android
/// backend and would silently link its in-memory mock, so it is compiled out there). A bridge
/// missing from the APK is `KeyStoreUnavailable`; an entry that can no longer be decrypted is an
/// `Operation` failure (so `ReadKey`), never "no key".
#[cfg(target_os = "android")]
mod backend {
    use super::{KEYRING_SERVICE, StoreFailure};
    use crate::error::AiApiError;
    use ms_platform::credentials::{self, CredentialError};

    fn failure(err: &CredentialError) -> StoreFailure {
        match err {
            CredentialError::Unavailable(unavailable) => StoreFailure::Store(AiApiError::KeyStoreUnavailable(*unavailable)),
            CredentialError::Unreadable { .. } | CredentialError::Backend { .. } => StoreFailure::Operation(err.to_string()),
        }
    }

    pub(super) fn get(user: &str) -> Result<Option<String>, StoreFailure> {
        credentials::read_blocking(KEYRING_SERVICE, user).map_err(|err| failure(&err))
    }

    pub(super) fn set(user: &str, secret: &str) -> Result<(), StoreFailure> {
        credentials::store_blocking(KEYRING_SERVICE, user, secret).map_err(|err| failure(&err))
    }

    pub(super) fn delete(user: &str) -> Result<(), StoreFailure> {
        credentials::delete_blocking(KEYRING_SERVICE, user).map_err(|err| failure(&err))
    }
}

/// Web: no credential store; every operation is `KeyStoreWebUnavailable`.
#[cfg(target_arch = "wasm32")]
mod backend {
    use super::StoreFailure;
    use crate::error::AiApiError;

    pub(super) fn get(_user: &str) -> Result<Option<String>, StoreFailure> {
        Err(StoreFailure::Store(AiApiError::KeyStoreWebUnavailable))
    }

    pub(super) fn set(_user: &str, _secret: &str) -> Result<(), StoreFailure> {
        Err(StoreFailure::Store(AiApiError::KeyStoreWebUnavailable))
    }

    pub(super) fn delete(_user: &str) -> Result<(), StoreFailure> {
        Err(StoreFailure::Store(AiApiError::KeyStoreWebUnavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::{KEYRING_SERVICE, NamedKeyUser, credential_user};
    use crate::service::AiApiService;
    use crate::target::AiApiTarget;

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
    #[test]
    fn hosted_user_name_is_the_frozen_service_key() {
        for service in AiApiService::ALL.into_iter().filter(|service| !service.uses_base_url()) {
            assert_eq!(credential_user(&target(service, "http://127.0.0.1:8080")), service.key(), "{service:?}");
        }
    }

    #[test]
    fn compatible_user_name_is_bound_to_the_normalized_endpoint() {
        let local = target(AiApiService::OpenAiCompatible, " HTTP://127.0.0.1:8080 ");
        assert_eq!(credential_user(&local), "openai_compatible@http://127.0.0.1:8080/v1/");
        assert_eq!(credential_user(&target(AiApiService::OpenAiCompatible, "http://127.0.0.1:8080/v1")), credential_user(&local));
        assert_ne!(credential_user(&target(AiApiService::OpenAiCompatible, "http://127.0.0.1:9090")), credential_user(&local));
        assert_eq!(credential_user(&target(AiApiService::AnthropicCompatible, "https://proxy.example/api")), "anthropic_compatible@https://proxy.example/api/");
    }

    // Persistence contract: changing a named user name orphans every image-edit / NeuroLate key
    // users saved.
    #[test]
    fn named_user_names_are_frozen() {
        assert_eq!(NamedKeyUser::image_edit("bfl", None).as_str(), "image_edit:bfl");
        assert_eq!(NamedKeyUser::image_edit("dashscope", Some("intl")).as_str(), "image_edit:dashscope@intl");
        assert_ne!(NamedKeyUser::image_edit("dashscope", Some("intl")), NamedKeyUser::image_edit("dashscope", Some("cn")));
        assert_ne!(NamedKeyUser::image_edit("dashscope", Some("intl")), NamedKeyUser::image_edit("dashscope", None));
        assert_eq!(NamedKeyUser::neurolate().as_str(), "neurolate:api");
    }

    // A named slot never aliases a target slot: target user names are `service.key()` or
    // `"{service.key()}@{url}"`, and no service key contains the `:` of the named prefix.
    #[test]
    fn named_user_names_never_collide_with_target_user_names() {
        for service in AiApiService::ALL {
            assert!(!service.key().contains(':'), "{service:?}");
            assert!(!service.key().starts_with("image_edit"), "{service:?}");
            assert!(!service.key().starts_with("neurolate"), "{service:?}");
        }
        assert!(!NamedKeyUser::neurolate().as_str().starts_with(super::IMAGE_EDIT_USER_PREFIX), "the NeuroLate slot is not an image-edit slot");
        let compatible = credential_user(&target(AiApiService::OpenAiCompatible, "http://127.0.0.1:8080"));
        assert!(!compatible.starts_with(NamedKeyUser::image_edit("openai_compatible", None).as_str()));
    }
}
