/*
File: crates/ms-ai-api/src/image_edit/keys.rs

Purpose:
Where an image-edit provider's API key lives, and the read / store / clear glue onto the
crate's credential-store functions (`crate::keys`). Resolves a provider's `ProviderKeySlot`
plus the user's endpoint choice into one concrete `ImageEditKeySlot`.

Key structures:
- ImageEditKeySlot

Key functions:
- key_slot()  : provider + endpoint choice -> the concrete slot.
- ImageEditKeySlot::{read, store, clear}: the one dispatch onto `crate::keys`.
- read_key() / store_key() / clear_key(): `key_slot` + that dispatch.

Notes:
Shared chat slots (`OpenAI`, Gemini, `OpenRouter`, xAI) ARE the OCR / translation keys of that
service; the user's own server uses the per-URL key of the `openai_compatible` chat service for
the same base URL; every other provider has a named `image_edit:` slot (per region for
region-bound keys). Every operation is blocking credential-store I/O: worker threads only.
On wasm the underlying functions return `KeyStoreWebUnavailable` (wrapped as `Key`). A key is
never logged here.
*/

use super::error::ImageEditError;
use super::provider::{EndpointKind, ImageEditProvider, ProviderKeySlot};
use super::request::EndpointChoice;
use crate::keys::{NamedKeyUser, clear_api_key, clear_named_key, read_api_key, read_named_key, store_api_key, store_named_key};
use crate::service::AiApiService;
use crate::target::AiApiTarget;

/// One concrete credential-store slot of an image-edit key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageEditKeySlot {
    /// The chat key of a target (a hosted service's shared key, or the per-URL key of the
    /// `OpenAI`-compatible service).
    Chat(AiApiTarget),
    /// A named slot; `label` is the provider's display name (errors and logs).
    Named { user: NamedKeyUser, label: &'static str },
}

/// The slot holding `provider`'s key for `endpoint`. A region-bound provider uses the chosen
/// region (its first region for `Default`); the user's own server needs its base URL.
///
/// # Errors
/// `ImageEditError::InvalidEndpoint` when the endpoint does not fit the provider (unknown
/// region, missing or malformed base URL).
pub fn key_slot(provider: ImageEditProvider, endpoint: &EndpointChoice) -> Result<ImageEditKeySlot, ImageEditError> {
    let invalid = |detail: String| ImageEditError::InvalidEndpoint { detail };
    let info = provider.info();
    match info.key_slot {
        ProviderKeySlot::SharedChat(service) => AiApiTarget::new(service, "").map(ImageEditKeySlot::Chat).map_err(|error| invalid(error.to_string())),
        ProviderKeySlot::SharedCompatible => match endpoint {
            EndpointChoice::BaseUrl(url) => AiApiTarget::new(AiApiService::OpenAiCompatible, url).map(ImageEditKeySlot::Chat).map_err(|error| invalid(error.to_string())),
            EndpointChoice::Default | EndpointChoice::Region(_) => Err(invalid(crate::error::AiApiError::BaseUrlMissing { service: AiApiService::OpenAiCompatible }.to_string())),
        },
        ProviderKeySlot::Named => provider.named_key_user(None).map(|user| ImageEditKeySlot::Named { user, label: info.label }).ok_or_else(|| invalid(format!("provider {} has no named key slot", provider.key()))),
        ProviderKeySlot::NamedPerRegion => {
            let region = match (info.endpoint, endpoint) {
                (EndpointKind::Regions(regions), EndpointChoice::Default) => regions.first().map(|region| region.id),
                (EndpointKind::Regions(_), EndpointChoice::Region(id)) => provider.region(id).map(|region| region.id),
                (EndpointKind::Regions(_) | EndpointKind::Fixed(_) | EndpointKind::UserBaseUrl, _) => None,
            };
            let region = region.ok_or_else(|| invalid(format!("endpoint {endpoint:?} is not a region of provider {}", provider.key())))?;
            provider.named_key_user(Some(region)).map(|user| ImageEditKeySlot::Named { user, label: info.label }).ok_or_else(|| invalid(format!("provider {} has no named key slot", provider.key())))
        }
    }
}

impl ImageEditKeySlot {
    /// Reads the slot's key, untrimmed; `Ok("")` when none is stored. The secret must never be
    /// logged or persisted. Blocking credential-store I/O.
    ///
    /// # Errors
    /// `Key` when the credential store fails.
    pub fn read(&self) -> Result<String, ImageEditError> {
        match self {
            Self::Chat(target) => Ok(read_api_key(target)?),
            Self::Named { user, label } => Ok(read_named_key(user, label)?),
        }
    }

    /// Stores the trimmed `api_key` in the slot, replacing the previous one. For a shared chat
    /// slot this also changes the OCR / translation key of that service (by design). Blocking
    /// credential-store I/O.
    ///
    /// # Errors
    /// `Key(EmptyKey)` for a blank key; `Key` when the store fails.
    pub fn store(&self, api_key: &str) -> Result<(), ImageEditError> {
        match self {
            Self::Chat(target) => Ok(store_api_key(target, api_key)?),
            Self::Named { user, label } => Ok(store_named_key(user, label, api_key)?),
        }
    }

    /// Deletes the slot's key; no stored key is success. Blocking credential-store I/O.
    ///
    /// # Errors
    /// `Key` when the store fails.
    pub fn clear(&self) -> Result<(), ImageEditError> {
        match self {
            Self::Chat(target) => Ok(clear_api_key(target)?),
            Self::Named { user, label } => Ok(clear_named_key(user, label)?),
        }
    }
}

/// Reads the key of `provider` for `endpoint` (`key_slot` then `ImageEditKeySlot::read`).
///
/// # Errors
/// `InvalidEndpoint` as `key_slot`; `Key` when the credential store fails.
pub fn read_key(provider: ImageEditProvider, endpoint: &EndpointChoice) -> Result<String, ImageEditError> {
    key_slot(provider, endpoint)?.read()
}

/// Stores `api_key` for `provider` at `endpoint` (`key_slot` then `ImageEditKeySlot::store`).
///
/// # Errors
/// `InvalidEndpoint` as `key_slot`; `Key(EmptyKey)` for a blank key; `Key` when the store fails.
pub fn store_key(provider: ImageEditProvider, endpoint: &EndpointChoice, api_key: &str) -> Result<(), ImageEditError> {
    key_slot(provider, endpoint)?.store(api_key)
}

/// Deletes the key of `provider` at `endpoint` (`key_slot` then `ImageEditKeySlot::clear`).
///
/// # Errors
/// `InvalidEndpoint` as `key_slot`; `Key` when the store fails.
pub fn clear_key(provider: ImageEditProvider, endpoint: &EndpointChoice) -> Result<(), ImageEditError> {
    key_slot(provider, endpoint)?.clear()
}

#[cfg(test)]
mod tests {
    use super::{ImageEditKeySlot, key_slot};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::request::EndpointChoice;
    use crate::service::AiApiService;

    fn user(slot: Result<ImageEditKeySlot, ImageEditError>) -> Option<String> {
        match slot {
            Ok(ImageEditKeySlot::Named { user, .. }) => Some(user.as_str().to_string()),
            Ok(ImageEditKeySlot::Chat(_)) | Err(_) => None,
        }
    }

    // Pure slot resolution only: no test touches the OS credential store.
    #[test]
    fn shared_chat_providers_use_the_hosted_chat_target() {
        for (provider, service) in [(ImageEditProvider::OpenAi, AiApiService::OpenAi), (ImageEditProvider::Gemini, AiApiService::Gemini), (ImageEditProvider::OpenRouter, AiApiService::OpenRouter), (ImageEditProvider::Xai, AiApiService::Xai)] {
            match key_slot(provider, &EndpointChoice::Default) {
                Ok(ImageEditKeySlot::Chat(target)) => {
                    assert_eq!(target.service(), service);
                    assert_eq!(target.endpoint(), None);
                }
                other => panic!("{provider:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn own_server_uses_the_per_url_compatible_key() {
        match key_slot(ImageEditProvider::OpenAiCompatible, &EndpointChoice::BaseUrl("http://127.0.0.1:1234".to_string())) {
            Ok(ImageEditKeySlot::Chat(target)) => {
                assert_eq!(target.service(), AiApiService::OpenAiCompatible);
                assert_eq!(target.endpoint(), Some("http://127.0.0.1:1234/v1/"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(key_slot(ImageEditProvider::OpenAiCompatible, &EndpointChoice::Default), Err(ImageEditError::InvalidEndpoint { .. })));
        assert!(matches!(key_slot(ImageEditProvider::OpenAiCompatible, &EndpointChoice::BaseUrl("ftp://x".to_string())), Err(ImageEditError::InvalidEndpoint { .. })));
    }

    #[test]
    fn named_slots_are_frozen_and_region_bound_where_required() {
        assert_eq!(user(key_slot(ImageEditProvider::Bfl, &EndpointChoice::Region("eu"))).as_deref(), Some("image_edit:bfl"));
        assert_eq!(user(key_slot(ImageEditProvider::AiTunnel, &EndpointChoice::Default)).as_deref(), Some("image_edit:aitunnel"));
        assert_eq!(user(key_slot(ImageEditProvider::DashScope, &EndpointChoice::Default)).as_deref(), Some("image_edit:dashscope@intl"));
        assert_eq!(user(key_slot(ImageEditProvider::DashScope, &EndpointChoice::Region("cn"))).as_deref(), Some("image_edit:dashscope@cn"));
        assert!(matches!(key_slot(ImageEditProvider::DashScope, &EndpointChoice::Region("mars")), Err(ImageEditError::InvalidEndpoint { .. })));
        assert!(matches!(key_slot(ImageEditProvider::Fal, &EndpointChoice::Default), Ok(ImageEditKeySlot::Named { label: "fal.ai", .. })));
    }
}
