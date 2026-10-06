/*
File: crates/ms-ai-api/src/error.rs

Purpose:
`AiApiError`, the one typed error of the crate. Its `Display` is the localized user message,
rendered at display time through `t!` / `tf!`, so a consumer that still carries `String`
errors converts with `.to_string()` and shows exactly that text.

Notes:
Source errors (`keyring::Error`, `genai::Error`, `ureq::Error`, ...) are kept as their
`detail` text because those types do not exist on wasm; this keeps the enum target-neutral,
`Send` and `Clone`. No variant ever carries an API key.
*/

use std::fmt;

use crate::service::AiApiService;

/// Failure of an AI API key-store, target (base URL / model), model-list, account-status or
/// background-task operation.
#[derive(Debug, Clone)]
pub enum AiApiError {
    /// The OS credential store could not be opened at all.
    KeyringUnavailable { detail: String },
    /// Key storage was requested on the web build, which has no credential store.
    KeyStoreWebUnavailable,
    /// An empty (or whitespace-only) key was offered for storage.
    EmptyKey,
    /// Reading the stored key failed for a reason other than "no key stored".
    ReadKey { service: AiApiService, detail: String },
    /// Writing the key to the credential store failed.
    StoreKey { service: AiApiService, detail: String },
    /// Deleting the stored key failed for a reason other than "no key stored".
    DeleteKey { service: AiApiService, detail: String },
    /// Reading the key of a named slot (`keys::NamedKeyUser`) failed for a reason other than
    /// "no key stored"; `label` is the slot owner's display name.
    ReadNamedKey { label: &'static str, detail: String },
    /// Writing the key of a named slot to the credential store failed.
    StoreNamedKey { label: &'static str, detail: String },
    /// Deleting the key of a named slot failed for a reason other than "no key stored".
    DeleteNamedKey { label: &'static str, detail: String },
    /// The async runtime for the model-list request could not be created.
    ModelListRuntime { detail: String },
    /// The provider's model-list request failed.
    FetchModels { service: AiApiService, detail: String },
    /// Model metadata was requested on the web build, which has no `genai` client.
    MetadataWebUnavailable,
    /// The `OpenRouter` `/api/v1/key` request failed.
    OpenRouterRequest { detail: String },
    /// The `OpenRouter` `/api/v1/key` response was not JSON.
    OpenRouterNonJson { detail: String },
    /// The background thread for a connection request could not be started.
    TaskSpawn { detail: String },
    /// A compatible service has no base URL (server address) set.
    BaseUrlMissing { service: AiApiService },
    /// The base URL is not an `http(s)://host[/path]` address (see `target::normalize_base_url`).
    BaseUrlInvalid { url: String },
    /// A request was built for an empty model id.
    EmptyModel,
    /// The user stopped the generation (`generation::GenerationTracker::cancel` / `close`); the
    /// request was dropped. Consumers treat it as a neutral "stopped" outcome, not a failure.
    Cancelled,
    /// A chat request or its response stream failed; `detail` is the provider / transport error
    /// text (see `chat_failure_detail`).
    ChatRequest { detail: String },
}

impl AiApiError {
    /// The localized, human-readable message for the active UI locale. This is the text
    /// `Display` writes.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::KeyringUnavailable { detail } => tf!("ai_api.keys.keyring_unavailable_error", err = detail),
            Self::KeyStoreWebUnavailable => t!("ai_api.keys.web_unavailable_error").to_string(),
            Self::EmptyKey => t!("ai_api.keys.empty_key_error").to_string(),
            Self::ReadKey { service, detail } => tf!("ai_api.keys.read_error", service = service.label(), err = detail),
            Self::StoreKey { service, detail } => tf!("ai_api.keys.store_error", service = service.label(), err = detail),
            Self::DeleteKey { service, detail } => tf!("ai_api.keys.delete_error", service = service.label(), err = detail),
            Self::ReadNamedKey { label, detail } => tf!("ai_api.keys.read_named_error", provider = label, err = detail),
            Self::StoreNamedKey { label, detail } => tf!("ai_api.keys.store_named_error", provider = label, err = detail),
            Self::DeleteNamedKey { label, detail } => tf!("ai_api.keys.delete_named_error", provider = label, err = detail),
            Self::ModelListRuntime { detail } => tf!("ai_api.metadata.async_runtime_error", err = detail),
            Self::FetchModels { service, detail } => tf!("ai_api.metadata.fetch_models_error", service = service.label(), err = detail),
            Self::MetadataWebUnavailable => t!("ai_api.metadata.web_unavailable_error").to_string(),
            Self::OpenRouterRequest { detail } => tf!("ai_api.openrouter.key_request_error", err = detail),
            Self::OpenRouterNonJson { detail } => tf!("ai_api.openrouter.non_json_error", err = detail),
            Self::TaskSpawn { detail } => tf!("ai_api.tasks.spawn_error", err = detail),
            Self::BaseUrlMissing { service } => tf!("ai_api.target.base_url_missing_error", service = service.label()),
            Self::BaseUrlInvalid { url } => tf!("ai_api.target.base_url_invalid_error", url = url),
            Self::EmptyModel => t!("ai_api.target.empty_model_error").to_string(),
            Self::Cancelled => t!("ai_api.generation.cancelled_status").to_string(),
            Self::ChatRequest { detail } => tf!("ai_api.generation.request_failed_error", err = detail),
        }
    }

    /// The text a consumer wraps into its own "request failed: {err}" message: the raw
    /// provider error for `ChatRequest` (so the consumer's message does not repeat a generic
    /// prefix), the localized `user_message` for every other variant.
    #[must_use]
    pub fn chat_failure_detail(&self) -> String {
        if let Self::ChatRequest { detail } = self { detail.clone() } else { self.user_message() }
    }
}

impl fmt::Display for AiApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for AiApiError {}

#[cfg(test)]
mod tests {
    use super::AiApiError;
    use crate::service::AiApiService;

    // No UI locale is installed in this test binary, so only the Display == user_message
    // contract is asserted, never the localized text itself.
    #[test]
    fn display_is_user_message() {
        let error = AiApiError::ReadKey { service: AiApiService::Groq, detail: "boom-detail".to_string() };
        assert_eq!(error.to_string(), error.user_message());
        assert_eq!(AiApiError::EmptyKey.to_string(), AiApiError::EmptyKey.user_message());
        let named = AiApiError::StoreNamedKey { label: "Black Forest Labs", detail: "boom-detail".to_string() };
        assert_eq!(named.to_string(), named.user_message());
    }

    #[test]
    fn chat_failure_detail_is_the_raw_provider_text_only_for_chat_requests() {
        let failed = AiApiError::ChatRequest { detail: "HTTP 429".to_string() };
        assert_eq!(failed.chat_failure_detail(), "HTTP 429");
        assert_eq!(AiApiError::Cancelled.chat_failure_detail(), AiApiError::Cancelled.user_message());
    }
}
