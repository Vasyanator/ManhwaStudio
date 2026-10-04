/*
File: crates/ms-ai-api/src/model_id.rs

Purpose:
Model-id rules. The UI and the settings files store a model id with a `prefix::` for
OpenRouter (`open_router::`) and Groq (`groq::`), because those providers' raw ids would
otherwise be resolved by `genai` to another adapter; `model_iden` strips it again for the
request. Both directions are a persistence contract (persisted model strings carry the
prefix). Also owns the multimodal-model heuristic.

Key functions:
- model_iden()                 (native)
- ui_model_name()              (native, crate-private)
- is_likely_multimodal_model()

Notes:
The heuristic is the seam a future model database (`model_db/`) replaces.
*/

#[cfg(not(target_arch = "wasm32"))]
use genai::ModelIden;

#[cfg(not(target_arch = "wasm32"))]
use crate::service::AiApiService;

/// The request identity for a persisted UI model id: trims it, drops any `prefix::`, and
/// binds the name explicitly to `service`'s adapter.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn model_iden(service: AiApiService, model: &str) -> ModelIden {
    let model_name = model
        .trim()
        .split_once("::")
        .map_or_else(|| model.trim(), |(_, name)| name.trim());
    ModelIden::new(service.adapter_kind(), model_name)
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

/// Best-effort, case-insensitive guess whether `model` accepts image input, by substring
/// match on known multimodal family names. A heuristic: it can be wrong both ways.
#[must_use]
pub fn is_likely_multimodal_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("vision")
        || model.contains("vl")
        || model.contains("omni")
        || model.contains("multimodal")
        || model.contains("gpt-4o")
        || model.contains("gpt-4.1")
        || model.contains("gpt-5")
        || model.contains("claude-3")
        || model.contains("claude-4")
        || model.contains("gemini")
        || model.contains("grok-2")
        || model.contains("grok-3")
        || model.contains("llama-4")
}

#[cfg(test)]
mod tests {
    use super::is_likely_multimodal_model;
    #[cfg(not(target_arch = "wasm32"))]
    use super::{model_iden, ui_model_name};
    #[cfg(not(target_arch = "wasm32"))]
    use crate::service::AiApiService;
    #[cfg(not(target_arch = "wasm32"))]
    use genai::adapter::AdapterKind;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn strips_ui_namespace_for_explicit_model_identity() {
        let model = model_iden(AiApiService::OpenRouter, "open_router::google/gemini-2.0-flash-001");
        assert_eq!(model.adapter_kind, AdapterKind::OpenRouter);
        assert_eq!(model.model_name.to_string(), "google/gemini-2.0-flash-001");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn characterize_ui_model_name_prefixes() {
        assert_eq!(ui_model_name(AiApiService::OpenRouter, "google/x"), "open_router::google/x");
        assert_eq!(ui_model_name(AiApiService::Groq, "meta/llama"), "groq::meta/llama");
        assert_eq!(ui_model_name(AiApiService::OpenAi, "gpt-4o"), "gpt-4o");
        assert_eq!(ui_model_name(AiApiService::OpenRouter, "open_router::google/x"), "open_router::google/x");
    }

    #[test]
    fn detects_common_multimodal_model_names() {
        assert!(is_likely_multimodal_model("gpt-4o-mini"));
        assert!(is_likely_multimodal_model("claude-3-5-haiku-latest"));
        assert!(is_likely_multimodal_model("google/gemini-2.0-flash-001"));
        assert!(!is_likely_multimodal_model("text-embedding-3-small"));
    }
}
