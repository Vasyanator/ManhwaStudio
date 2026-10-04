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

/// Failure of an AI API key-store, model-list, account-status or background-task operation.
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
            Self::ModelListRuntime { detail } => tf!("ai_api.metadata.async_runtime_error", err = detail),
            Self::FetchModels { service, detail } => tf!("ai_api.metadata.fetch_models_error", service = service.label(), err = detail),
            Self::MetadataWebUnavailable => t!("ai_api.metadata.web_unavailable_error").to_string(),
            Self::OpenRouterRequest { detail } => tf!("ai_api.openrouter.key_request_error", err = detail),
            Self::OpenRouterNonJson { detail } => tf!("ai_api.openrouter.non_json_error", err = detail),
            Self::TaskSpawn { detail } => tf!("ai_api.tasks.spawn_error", err = detail),
        }
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
    }
}
