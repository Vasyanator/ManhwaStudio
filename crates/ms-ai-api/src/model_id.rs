/*
File: crates/ms-ai-api/src/model_id.rs

Purpose:
Model-id rules. The UI and the settings files store a model id with a `prefix::` for
OpenRouter (`open_router::`) and Groq (`groq::`), because those providers' raw ids would
otherwise be resolved by `genai` to another adapter; `model_iden` strips it again for the
request. Both directions are a persistence contract (persisted model strings carry the
prefix). Model capabilities live in `model_caps`.

Key functions:
- model_iden()                 (native)
- ui_model_name()              (native, crate-private)
*/

#[cfg(not(target_arch = "wasm32"))]
use genai::ModelIden;

#[cfg(not(target_arch = "wasm32"))]
use crate::error::AiApiError;
#[cfg(not(target_arch = "wasm32"))]
use crate::service::AiApiService;

/// The request identity for a persisted UI model id: trims it, drops any `prefix::`, and
/// binds the name explicitly to `service`'s adapter.
///
/// # Errors
/// `EmptyModel` when no model name remains (a blank id, or a bare `prefix::`): such a request
/// would reach the provider with an empty `model` field.
#[cfg(not(target_arch = "wasm32"))]
pub fn model_iden(service: AiApiService, model: &str) -> Result<ModelIden, AiApiError> {
    let model_name = model
        .trim()
        .split_once("::")
        .map_or_else(|| model.trim(), |(_, name)| name.trim());
    if model_name.is_empty() {
        return Err(AiApiError::EmptyModel);
    }
    Ok(ModelIden::new(service.adapter_kind(), model_name))
}

/// The persisted UI id for a provider-listed model: adds `open_router::` / `groq::` to an
/// unprefixed `OpenRouter` / Groq id; every other id is returned unchanged.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn ui_model_name(service: AiApiService, model: &str) -> String {
    if service == AiApiService::OpenRouter && !model.contains("::") {
        format!("open_router::{model}")
    } else if service == AiApiService::Groq && !model.contains("::") {
        format!("groq::{model}")
    } else {
        model.to_string()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{model_iden, ui_model_name};
    use crate::error::AiApiError;
    use crate::service::AiApiService;
    use genai::adapter::AdapterKind;

    #[test]
    fn strips_ui_namespace_for_explicit_model_identity() {
        let model = model_iden(AiApiService::OpenRouter, "open_router::google/gemini-2.0-flash-001").ok();
        assert_eq!(model.as_ref().map(|model| model.adapter_kind), Some(AdapterKind::OpenRouter));
        assert_eq!(model.map(|model| model.model_name.to_string()).as_deref(), Some("google/gemini-2.0-flash-001"));
    }

    #[test]
    fn empty_model_is_a_typed_error() {
        for model in ["", "   ", "open_router::", "groq:: "] {
            assert!(matches!(model_iden(AiApiService::OpenAiCompatible, model), Err(AiApiError::EmptyModel)), "{model:?}");
        }
    }

    #[test]
    fn characterize_ui_model_name_prefixes() {
        assert_eq!(ui_model_name(AiApiService::OpenRouter, "google/x"), "open_router::google/x");
        assert_eq!(ui_model_name(AiApiService::Groq, "meta/llama"), "groq::meta/llama");
        assert_eq!(ui_model_name(AiApiService::OpenAi, "gpt-4o"), "gpt-4o");
        assert_eq!(ui_model_name(AiApiService::OpenRouter, "open_router::google/x"), "open_router::google/x");
    }
}
