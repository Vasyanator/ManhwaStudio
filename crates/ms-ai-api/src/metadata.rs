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
The model list is FILTERED to likely-multimodal models (`model_id::is_likely_multimodal_model`)
for every consumer, sorted, de-duplicated, and falls back to the service's default model when
nothing remains. Model ids are returned in the persisted UI form (`model_id::ui_model_name`).
*/

use crate::error::AiApiError;
use crate::service::AiApiService;
#[cfg(not(target_arch = "wasm32"))]
use ms_log::runtime_log;

/// What `load_metadata` learned about one service.
#[derive(Debug, Clone)]
pub struct AiApiMetadata {
    /// The service this metadata describes (consumers drop it if the selection changed).
    pub service: AiApiService,
    /// A non-blank key is stored in the credential store.
    pub key_configured: bool,
    /// Selectable model ids in the persisted UI form; empty when no key is configured.
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
pub fn load_metadata(_service: AiApiService) -> Result<AiApiMetadata, AiApiError> {
    Err(AiApiError::MetadataWebUnavailable)
}

/// Loads the metadata of `service`: reads its key, lists its models and, for `OpenRouter`,
/// fetches the account status. Blocking network and credential-store I/O: worker threads
/// only.
///
/// A key-store read failure is logged and treated as "no key configured" (the metadata then
/// reports `key_configured == false`); an `OpenRouter` status failure becomes the status text.
///
/// # Errors
/// Only the model listing fails the call: `ModelListRuntime` or `FetchModels`.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_metadata(service: AiApiService) -> Result<AiApiMetadata, AiApiError> {
    let key = match crate::keys::read_api_key(service) {
        Ok(key) => key,
        Err(err) => {
            // The read failure itself is already logged by `read_api_key`; this records that
            // metadata deliberately continues as "no key configured".
            runtime_log::log_warn(format!("[AI API] metadata for {} continues without a key: {err}", service.label()));
            String::new()
        }
    };
    let key_configured = !key.trim().is_empty();
    let mut account_status = if key_configured {
        t!("ai_api.metadata.balance_unavailable_status").to_string()
    } else {
        t!("ai_api.metadata.api_key_not_set_status").to_string()
    };
    let mut models = Vec::new();

    if key_configured {
        models = fetch_model_names(service, &key)?;
        if service == AiApiService::OpenRouter {
            account_status = crate::openrouter::fetch_account_status(&key)
                .unwrap_or_else(|err| tf!("ai_api.openrouter.balance_error", err = err));
        }
    }

    Ok(AiApiMetadata {
        service,
        key_configured,
        models,
        account_status,
    })
}

/// Lists the provider's models for `api_key`, filtered, sorted and de-duplicated as the file
/// header describes; never empty on success.
#[cfg(not(target_arch = "wasm32"))]
fn fetch_model_names(service: AiApiService, api_key: &str) -> Result<Vec<String>, AiApiError> {
    use genai::Client;
    use genai::resolver::{AuthData, ProviderConfig};

    let adapter = service.adapter_kind();
    let key = api_key.to_string();
    let listed = crate::client::block_on(async move {
        let client = Client::default();
        client
            .all_model_names(
                adapter,
                ProviderConfig::from_auth(AuthData::from_single(key)),
            )
            .await
    })
    .map_err(|err| {
        runtime_log::log_error(format!("[AI API] failed to create the async runtime for the {} model list: {err}", service.label()));
        AiApiError::ModelListRuntime { detail: err.to_string() }
    })?;
    let models = listed.map_err(|err| {
        // Safe to log with the provider body: the key travels only in a request header and
        // providers echo at most a masked fragment of it (e.g. "sk-...abcd") in error bodies.
        runtime_log::log_error(format!("[AI API] failed to list {} models: {err}", service.label()));
        AiApiError::FetchModels { service, detail: err.to_string() }
    })?;
    let mut filtered = models
        .into_iter()
        .filter(|model| crate::model_id::is_likely_multimodal_model(model))
        .map(|model| crate::model_id::ui_model_name(service, &model))
        .collect::<Vec<_>>();
    filtered.sort();
    filtered.dedup();
    if filtered.is_empty() {
        filtered.push(service.default_model().to_string());
    }
    Ok(filtered)
}
