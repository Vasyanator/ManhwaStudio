/*
File: crates/ms-ai-api/src/service.rs

Purpose:
The AI API provider catalogue. `AiApiService::key()` is a persistence contract: it is the
id written to the title `settings.json` and the credential-store user name of the API key.
Two "compatible" services (`OpenAiCompatible`, `AnthropicCompatible`) speak a known wire
protocol to a user-given base URL (a local llama.cpp / vLLM / proxy server): they need that URL
(`uses_base_url`) and accept a missing API key (`requires_key`).

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
    /// Any server speaking the `OpenAI` chat-completions protocol at a user-given base URL.
    OpenAiCompatible,
    /// Any server speaking the Anthropic messages protocol at a user-given base URL.
    AnthropicCompatible,
}

impl AiApiService {
    /// Every service, in UI order.
    pub const ALL: [Self; 9] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::Gemini,
        Self::OpenRouter,
        Self::Groq,
        Self::DeepSeek,
        Self::Xai,
        Self::OpenAiCompatible,
        Self::AnthropicCompatible,
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
            Self::OpenAiCompatible => "openai_compatible",
            Self::AnthropicCompatible => "anthropic_compatible",
        }
    }

    /// Provider name shown in the UI and in error texts: the brand name (not localized) for a
    /// hosted provider, a localized "<protocol>-compatible" text for a compatible service.
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
            Self::OpenAiCompatible => t!("ai_api.service.openai_compatible_label"),
            Self::AnthropicCompatible => t!("ai_api.service.anthropic_compatible_label"),
        }
    }

    /// Whether requests go to a user-given base URL (`AiApiTarget::new` then requires one)
    /// instead of the provider's official endpoint.
    #[must_use]
    pub fn uses_base_url(self) -> bool {
        match self {
            Self::OpenAi | Self::Anthropic | Self::Gemini | Self::OpenRouter | Self::Groq | Self::DeepSeek | Self::Xai => false,
            Self::OpenAiCompatible | Self::AnthropicCompatible => true,
        }
    }

    /// Whether a stored API key is mandatory. A compatible service (often a local server
    /// without authentication) is used without a key when none is stored.
    #[must_use]
    pub fn requires_key(self) -> bool {
        match self {
            Self::OpenAi | Self::Anthropic | Self::Gemini | Self::OpenRouter | Self::Groq | Self::DeepSeek | Self::Xai => true,
            Self::OpenAiCompatible | Self::AnthropicCompatible => false,
        }
    }

    /// Maps this service to its `genai` adapter. Native-only: `genai` (and the
    /// `AdapterKind` type) is not compiled for the web build.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn adapter_kind(self) -> AdapterKind {
        match self {
            Self::OpenAi | Self::OpenAiCompatible => AdapterKind::OpenAI,
            Self::Anthropic | Self::AnthropicCompatible => AdapterKind::Anthropic,
            Self::Gemini => AdapterKind::Gemini,
            Self::OpenRouter => AdapterKind::OpenRouter,
            Self::Groq => AdapterKind::Groq,
            Self::DeepSeek => AdapterKind::DeepSeek,
            Self::Xai => AdapterKind::Xai,
        }
    }

    /// Model used when the provider lists no usable model, in the persisted UI-id form
    /// (`OpenRouter` and Groq ids carry their `prefix::`, see `model_id`). Empty for a
    /// compatible service: its models are whatever the server lists (or the user types).
    #[must_use]
    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o-mini",
            Self::Anthropic => "claude-3-5-haiku-latest",
            Self::Gemini => "gemini-2.5-flash",
            Self::OpenRouter => "open_router::google/gemini-2.0-flash-001",
            Self::Groq => "groq::meta-llama/llama-4-scout-17b-16e-instruct",
            Self::DeepSeek => "deepseek-flash",
            Self::Xai => "grok-2-vision-1212",
            Self::OpenAiCompatible | Self::AnthropicCompatible => "",
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
            "openai_compatible" => Self::OpenAiCompatible,
            "anthropic_compatible" => Self::AnthropicCompatible,
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
            (AiApiService::OpenAiCompatible, "openai_compatible"),
            (AiApiService::AnthropicCompatible, "anthropic_compatible"),
        ];
        for (service, key) in expected {
            assert_eq!(service.key(), key);
        }
        for service in AiApiService::ALL {
            assert_eq!(AiApiService::from_key(service.key()), service);
        }
    }

    #[test]
    fn only_compatible_services_use_a_base_url_and_make_the_key_optional() {
        for service in AiApiService::ALL {
            let compatible = matches!(service, AiApiService::OpenAiCompatible | AiApiService::AnthropicCompatible);
            assert_eq!(service.uses_base_url(), compatible, "{service:?}");
            assert_eq!(service.requires_key(), !compatible, "{service:?}");
            assert_eq!(service.default_model().is_empty(), compatible, "{service:?}");
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn compatible_services_use_the_protocol_adapter() {
        use genai::adapter::AdapterKind;
        assert_eq!(AiApiService::OpenAiCompatible.adapter_kind(), AdapterKind::OpenAI);
        assert_eq!(AiApiService::AnthropicCompatible.adapter_kind(), AdapterKind::Anthropic);
    }
}
