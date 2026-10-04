/*
File: crates/ms-ai-api/src/service.rs

Purpose:
The AI API provider catalogue. `AiApiService::key()` is a persistence contract: it is the
id written to the title `settings.json` and the credential-store user name of the API key.

Key structures:
- AiApiService

Notes:
`adapter_kind` is native only because `genai` is not compiled for wasm.
*/

#[cfg(not(target_arch = "wasm32"))]
use genai::adapter::AdapterKind;

/// A hosted LLM provider reachable through `genai`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AiApiService {
    OpenAi,
    Anthropic,
    Gemini,
    OpenRouter,
    Groq,
    DeepSeek,
    Xai,
}

impl AiApiService {
    /// Every service, in UI order.
    pub const ALL: [Self; 7] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::Gemini,
        Self::OpenRouter,
        Self::Groq,
        Self::DeepSeek,
        Self::Xai,
    ];

    /// Stable persisted id (settings files, credential-store user name). Never change a value:
    /// stored settings and saved API keys are addressed by it.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::OpenRouter => "open_router",
            Self::Groq => "groq",
            Self::DeepSeek => "deepseek",
            Self::Xai => "xai",
        }
    }

    /// Provider brand name shown in the UI and in error texts (not localized: brand names).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
            Self::OpenRouter => "OpenRouter",
            Self::Groq => "Groq",
            Self::DeepSeek => "DeepSeek",
            Self::Xai => "xAI",
        }
    }

    /// Maps this service to its `genai` adapter. Native-only: `genai` (and the
    /// `AdapterKind` type) is not compiled for the web build.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn adapter_kind(self) -> AdapterKind {
        match self {
            Self::OpenAi => AdapterKind::OpenAI,
            Self::Anthropic => AdapterKind::Anthropic,
            Self::Gemini => AdapterKind::Gemini,
            Self::OpenRouter => AdapterKind::OpenRouter,
            Self::Groq => AdapterKind::Groq,
            Self::DeepSeek => AdapterKind::DeepSeek,
            Self::Xai => AdapterKind::Xai,
        }
    }

    /// Model used when the provider lists no usable model, in the persisted UI-id form
    /// (`OpenRouter` and Groq ids carry their `prefix::`, see `model_id`).
    #[must_use]
    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o-mini",
            Self::Anthropic => "claude-3-5-haiku-latest",
            Self::Gemini => "gemini-2.5-flash",
            Self::OpenRouter => "open_router::google/gemini-2.0-flash-001",
            Self::Groq => "groq::meta-llama/llama-4-scout-17b-16e-instruct",
            Self::DeepSeek => "deepseek-chat",
            Self::Xai => "grok-2-vision-1212",
        }
    }

    /// Parses a persisted id, case-insensitively and with historical aliases
    /// (`claude`, `google`, `openrouter`, `deep_seek`, `x_ai`, `grok`). An unknown id maps to
    /// `OpenAi`, the default service.
    #[must_use]
    pub fn from_key(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Self::Anthropic,
            "gemini" | "google" => Self::Gemini,
            "openrouter" | "open_router" => Self::OpenRouter,
            "groq" => Self::Groq,
            "deepseek" | "deep_seek" => Self::DeepSeek,
            "xai" | "x_ai" | "grok" => Self::Xai,
            _ => Self::OpenAi,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AiApiService;

    #[test]
    fn parses_ai_api_service_keys() {
        assert_eq!(AiApiService::from_key("open_router"), AiApiService::OpenRouter);
        assert_eq!(AiApiService::from_key("grok"), AiApiService::Xai);
        assert_eq!(AiApiService::from_key("unknown"), AiApiService::OpenAi);
    }

    #[test]
    fn characterize_ai_api_service_keys_round_trip() {
        let expected = [
            (AiApiService::OpenAi, "openai"),
            (AiApiService::Anthropic, "anthropic"),
            (AiApiService::Gemini, "gemini"),
            (AiApiService::OpenRouter, "open_router"),
            (AiApiService::Groq, "groq"),
            (AiApiService::DeepSeek, "deepseek"),
            (AiApiService::Xai, "xai"),
        ];
        for (service, key) in expected {
            assert_eq!(service.key(), key);
        }
        for service in AiApiService::ALL {
            assert_eq!(AiApiService::from_key(service.key()), service);
        }
    }
}
