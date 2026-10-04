/*
File: crates/ms-ai-api/src/metadata.rs

Purpose:
Per-service metadata shown by the AI API connection UI: whether a key is stored, the
selectable model list, and an account status line (OpenRouter balance/limits; a fixed
"unavailable" or "key not set" text elsewhere).

Key structures:
- AiApiMetadata

Key functions:
- load_metadata()      (blocking; wasm stub returns `MetadataWebUnavailable`)
- fetch_model_names()  (private)

Notes:
The model list is every model the provider lists (not filtered by capability: text-only models
serve text translation), sorted, de-duplicated, and falls back to the service's default model
when the provider lists none (a compatible service has no default and stays empty). Model ids are
returned in the persisted UI form (`model_id::ui_model_name`). A compatible service lists its
models from its base URL with or without a stored key; a hosted service needs a key.
*/

use crate::error::AiApiError;
use crate::service::AiApiService;
use crate::target::AiApiTarget;
#[cfg(not(target_arch = "wasm32"))]
use ms_log::runtime_log;

/// What `load_metadata` learned about one service.
#[derive(Debug, Clone)]
pub struct AiApiMetadata {
    /// The service this metadata describes (consumers drop it if the selection changed).
    pub service: AiApiService,
    /// The normalized base URL the models were listed from (`AiApiTarget::endpoint`); `None`
    /// for a hosted service. Consumers drop the metadata if the URL changed meanwhile.
    pub endpoint: Option<String>,
    /// A non-blank key is stored in the credential store.
    pub key_configured: bool,
    /// Selectable model ids in the persisted UI form; empty when a hosted service has no key
    /// configured, or a compatible server lists no model.
    pub models: Vec<String>,
    /// Localized one-line account status.
    pub account_status: String,
}

/// Web stub: model listing / account status need `genai`/`tokio`/`ureq`, absent
/// on the browser build. Surfaces a clear error rather than empty metadata.
///
/// # Errors
/// Always `AiApiError::MetadataWebUnavailable`.
#[cfg(target_arch = "wasm32")]
pub fn load_metadata(_target: &AiApiTarget) -> Result<AiApiMetadata, AiApiError> {
    Err(AiApiError::MetadataWebUnavailable)
}

/// Loads the metadata of `target`: reads its key (bound to the base URL for a compatible
/// service, so `key_configured` answers for that URL), lists its models (a hosted service only with a
/// key, a compatible service always, from its base URL) and, for `OpenRouter`, fetches the
/// account status. Blocking network and credential-store I/O: worker threads only.
///
/// A key-store read failure is logged and treated as "no key configured" (the metadata then
/// reports `key_configured == false`); an `OpenRouter` status failure becomes the status text.
///
/// # Errors
/// Only the model listing fails the call: `ModelListRuntime` or `FetchModels`.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_metadata(target: &AiApiTarget) -> Result<AiApiMetadata, AiApiError> {
    let service = target.service();
    // For a compatible service this is the key saved for exactly this base URL (or none).
    let key = match crate::keys::read_api_key(target) {
        Ok(key) => key,
        Err(err) => {
            // The read failure itself is already logged by `read_api_key`; this records that
            // metadata deliberately continues as "no key configured".
            runtime_log::log_warn(format!("[AI API] metadata for {} continues without a key: {err}", service.label()));
            String::new()
        }
    };
    let key_configured = !key.trim().is_empty();
    let mut account_status = if key_configured || !service.requires_key() {
        t!("ai_api.metadata.balance_unavailable_status").to_string()
    } else {
        t!("ai_api.metadata.api_key_not_set_status").to_string()
    };
    let mut models = Vec::new();

    if key_configured || !service.requires_key() {
        models = fetch_model_names(target, &key)?;
        if service == AiApiService::OpenRouter {
            account_status = crate::openrouter::fetch_account_status(&key)
                .unwrap_or_else(|err| tf!("ai_api.openrouter.balance_error", err = err));
        }
    }

    Ok(AiApiMetadata {
        service,
        endpoint: target.endpoint().map(str::to_string),
        key_configured,
        models,
        account_status,
    })
}

/// Lists the models of `target` for `api_key`, sorted and de-duplicated as the file header
/// describes; never empty on success unless the service has no default model. A compatible
/// target is queried at its base URL, without an auth header when `api_key` is blank.
#[cfg(not(target_arch = "wasm32"))]
fn fetch_model_names(target: &AiApiTarget, api_key: &str) -> Result<Vec<String>, AiApiError> {
    use genai::Client;
    use genai::resolver::{AuthData, Endpoint, ProviderConfig};

    let service = target.service();
    let adapter = service.adapter_kind();
    // `AuthData::None` sends no auth header; with an explicit endpoint AND auth, `genai` resolves
    // neither from its defaults (so no `*_API_KEY` environment variable is read either).
    let auth = if api_key.trim().is_empty() { AuthData::None } else { AuthData::from_single(api_key.to_string()) };
    let config = match target.endpoint() {
        Some(endpoint) => ProviderConfig::from_auth(auth).with_endpoint(Endpoint::from_owned(endpoint.to_string())),
        None => ProviderConfig::from_auth(auth),
    };
    let listed = crate::client::block_on(async move {
        let client = Client::default();
        client.all_model_names(adapter, config).await
    })
    .map_err(|err| {
        runtime_log::log_error(format!("[AI API] failed to create the async runtime for the {} model list: {err}", service.label()));
        AiApiError::ModelListRuntime { detail: err.to_string() }
    })?;
    let models = listed.map_err(|err| {
        // Safe to log with the provider body: the key travels only in a request header and
        // providers echo at most a masked fragment of it (e.g. "sk-...abcd") in error bodies.
        runtime_log::log_error(format!("[AI API] failed to list {} models (endpoint: {}): {err}", service.label(), target.endpoint().unwrap_or("default")));
        AiApiError::FetchModels { service, detail: err.to_string() }
    })?;
    let mut listed = models
        .into_iter()
        .map(|model| crate::model_id::ui_model_name(service, &model))
        .collect::<Vec<_>>();
    listed.sort();
    listed.dedup();
    if listed.is_empty() && !service.default_model().is_empty() {
        listed.push(service.default_model().to_string());
    }
    Ok(listed)
}
