/*
File: crates/ms-ai-api/src/model_caps.rs

Purpose:
The LLM model capability database, seeded with one capability: whether a model accepts image
input. A hard-coded, ordered table of model ids and documented family patterns answers
`Supported` / `NotSupported`; every model it does not list is `Unknown`.

Key structures:
- ImageInputSupport
- IdPattern (private), NON_CHAT_MARKERS (private), IMAGE_INPUT_TABLE (private)

Key functions:
- image_input_support()
- is_non_chat_model()  (the non-chat markers alone; used to pick a default model)
- normalized_model_name()  (private)

Notes:
Matching rule: the model id is lowercased, a UI namespace (`open_router::`, `groq::`, any
`prefix::`) is dropped, then everything up to the last `/` (a vendor path such as `openai/`,
`meta-llama/`, `../models/`) is dropped, then a `:variant` suffix (OpenRouter `:free`, `:beta`,
`:nitro`, `:online`, ...) is dropped. The remaining name is tested against the table IN
ORDER and the first matching entry wins, so a specific carve-out (a text-only or doubtful
variant) is listed before the family prefix it would otherwise fall under. The non-chat markers
(`NON_CHAT_MARKERS`) are tested before the table and win over every family entry. Only models whose
capability is known for certain are listed; there is deliberately no substring guessing on
family names (`vl`, `vision`, ...), and a model the table does not list is `Unknown`.
*/

/// Whether a model accepts images in a chat request.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ImageInputSupport {
    /// Listed as accepting image input.
    Supported,
    /// Listed as text-only (or not a chat model at all).
    NotSupported,
    /// Not listed: the capability is not known.
    Unknown,
}

/// How one table entry matches a normalized model name.
#[derive(Debug, Clone, Copy)]
enum IdPattern {
    /// The whole name.
    Exact(&'static str),
    /// A family: the name starts with this text (dated and size variants included).
    Prefix(&'static str),
    /// A modality marker that is one or more whole `-`-delimited segments anywhere in the name
    /// (`tts` matches `tts-1`, `gpt-4o-mini-tts-2025-03-20`, `gemini-2.5-flash-preview-tts`, but
    /// not `gpt-4o-mini-ttsx`); used only for NOT-chat markers.
    Segment(&'static str),
    /// A modality marker anywhere in the name (`embedding`); used only for NOT-chat markers.
    Contains(&'static str),
}

impl IdPattern {
    fn matches(self, name: &str) -> bool {
        match self {
            Self::Exact(id) => name == id,
            Self::Prefix(prefix) => name.starts_with(prefix),
            Self::Segment(token) => has_segment(name, token),
            Self::Contains(marker) => name.contains(marker),
        }
    }
}

/// Whether `token` occurs in `name` as whole `-`-delimited segments: each occurrence is checked
/// for a `-` (or the name boundary) on both sides.
fn has_segment(name: &str, token: &str) -> bool {
    // `match_indices` yields char-boundary offsets, so both slices are valid.
    name.match_indices(token).any(|(start, _)| {
        let end = start + token.len();
        (start == 0 || name[..start].ends_with('-')) && (end == name.len() || name[end..].starts_with('-'))
    })
}

use IdPattern::{Contains, Exact, Prefix, Segment};
use ImageInputSupport::{NotSupported, Supported, Unknown};

/// Markers of models that are not chat models at all (embeddings, speech, transcription,
/// realtime audio, moderation, image generation, legacy completions), whatever their family and
/// wherever the marker sits (dated snapshots such as `gpt-4o-mini-tts-2025-03-20` included).
/// Checked before `IMAGE_INPUT_TABLE` (they are `NotSupported`) and by `is_non_chat_model`.
const NON_CHAT_MARKERS: &[IdPattern] = &[
    Contains("embedding"),
    Segment("tts"),
    Segment("whisper"),
    Segment("transcribe"),
    Segment("realtime"),
    Segment("moderation"),
    Segment("dall-e"),
    Prefix("gpt-image"),
    Exact("babbage-002"),
    Exact("davinci-002"),
];

/// The ordered image-input table of chat models; the first matching entry wins (see the file
/// header).
const IMAGE_INPUT_TABLE: &[(IdPattern, ImageInputSupport)] = &[
    // OpenAI GPT-4o: the audio variants are not image chat models (realtime and transcription
    // are non-chat markers); the search-preview variants are not documented either way.
    (Prefix("gpt-4o-audio"), NotSupported),
    (Prefix("gpt-4o-mini-audio"), NotSupported),
    (Prefix("gpt-4o-search"), Unknown),
    (Prefix("gpt-4o-mini-search"), Unknown),
    (Prefix("gpt-4o"), Supported),
    (Exact("chatgpt-4o-latest"), Supported),
    // OpenAI GPT-4.1 and GPT-5 families (all sizes accept images); GPT-5 search is undocumented.
    (Prefix("gpt-4.1"), Supported),
    (Prefix("gpt-5-search"), Unknown),
    (Prefix("gpt-5"), Supported),
    // OpenAI GPT-4 Turbo: only the 2024-04 GA model has vision; the previews are text-only.
    (Exact("gpt-4-turbo"), Supported),
    (Exact("gpt-4-turbo-2024-04-09"), Supported),
    (Exact("gpt-4-turbo-preview"), NotSupported),
    (Exact("gpt-4-0125-preview"), NotSupported),
    (Exact("gpt-4-1106-preview"), NotSupported),
    (Exact("gpt-4"), NotSupported),
    (Exact("gpt-4-0613"), NotSupported),
    // OpenAI text-only families: GPT-3.5 and the open-weight gpt-oss models.
    (Prefix("gpt-3.5-"), NotSupported),
    (Prefix("gpt-oss-"), NotSupported),
    // OpenAI reasoning models: o1 / o1-pro / o3 / o3-pro / o4-mini accept images; o1-mini,
    // o1-preview and o3-mini are text-only; deep-research variants are not listed.
    (Prefix("o1-mini"), NotSupported),
    (Prefix("o1-preview"), NotSupported),
    (Prefix("o3-mini"), NotSupported),
    (Prefix("o3-deep-research"), Unknown),
    (Prefix("o4-mini-deep-research"), Unknown),
    (Exact("o1"), Supported),
    (Prefix("o1-20"), Supported),
    (Prefix("o1-pro"), Supported),
    (Exact("o3"), Supported),
    (Prefix("o3-20"), Supported),
    (Prefix("o3-pro"), Supported),
    (Prefix("o4-mini"), Supported),
    // Anthropic: the Claude 3 / 3.5 Sonnet / 3.7 / 4.x models listed here accept images (dashed
    // API ids and dotted OpenRouter ids); Claude 2 / Instant are text-only. Claude 3.5 Haiku is
    // deliberately unlisted (`Unknown`): sources disagree on its image input, and no prefix below
    // covers it (`claude-3-haiku` does not match `claude-3-5-haiku`).
    (Prefix("claude-3-opus"), Supported),
    (Prefix("claude-3-sonnet"), Supported),
    (Prefix("claude-3-haiku"), Supported),
    (Prefix("claude-3-5-sonnet"), Supported),
    (Prefix("claude-3.5-sonnet"), Supported),
    (Prefix("claude-3-7-sonnet"), Supported),
    (Prefix("claude-3.7-sonnet"), Supported),
    (Prefix("claude-sonnet-4"), Supported),
    (Prefix("claude-opus-4"), Supported),
    (Prefix("claude-haiku-4"), Supported),
    (Prefix("claude-2"), NotSupported),
    (Prefix("claude-instant"), NotSupported),
    // Google Gemini 1.5 / 2.0 / 2.5 / 3 Flash and Pro (embedding and TTS variants are caught
    // above); the native-audio dialog models are not image chat models as far as documented.
    (Contains("native-audio"), Unknown),
    (Prefix("gemini-1.5-"), Supported),
    (Prefix("gemini-2.0-flash"), Supported),
    (Prefix("gemini-2.0-pro"), Supported),
    (Prefix("gemini-2.5-flash"), Supported),
    (Prefix("gemini-2.5-pro"), Supported),
    (Prefix("gemini-3-pro"), Supported),
    (Prefix("gemini-3-flash"), Supported),
    (Exact("gemini-pro-vision"), Supported),
    // Google Gemma 3: the 1B and 270M models are text-only, the larger ones accept images.
    (Prefix("gemma-3-1b"), NotSupported),
    (Prefix("gemma-3-270m"), NotSupported),
    (Prefix("gemma-3-"), Supported),
    // xAI: Grok 2 Vision and Grok 4 accept images; plain Grok 2 and the Grok 3 family are text-only.
    (Prefix("grok-2-vision"), Supported),
    (Exact("grok-vision-beta"), Supported),
    (Exact("grok-2-1212"), NotSupported),
    (Exact("grok-2-latest"), NotSupported),
    (Prefix("grok-3"), NotSupported),
    (Prefix("grok-4"), Supported),
    // Meta Llama: Llama 4 is natively multimodal; Llama 3.1 and 3.3 are text-only.
    (Prefix("llama-4-"), Supported),
    (Prefix("llama-3.1-"), NotSupported),
    (Prefix("llama-3.3-"), NotSupported),
    // Mistral Pixtral and the Qwen VL families are vision models.
    (Prefix("pixtral-"), Supported),
    (Prefix("qwen-vl-"), Supported),
    (Prefix("qwen2.5-vl-"), Supported),
    (Prefix("qwen3-vl-"), Supported),
    // DeepSeek: V4.1 Flash (`deepseek-flash`, plus the legacy `deepseek-v4-flash*` names routed
    // to it) has native vision, as does the open-weight VL2 family; V4 Pro is text-only, as were
    // the legacy chat / reasoner (V3) and V3 / R1 families.
    (Prefix("deepseek-v4-flash"), Supported),
    (Exact("deepseek-flash"), Supported),
    (Prefix("deepseek-vl2"), Supported),
    (Prefix("deepseek-v4-pro"), NotSupported),
    (Prefix("deepseek-chat"), NotSupported),
    (Prefix("deepseek-reasoner"), NotSupported),
    (Prefix("deepseek-v3"), NotSupported),
    (Prefix("deepseek-r1"), NotSupported),
];

/// Whether `model` accepts image input, looked up in the capability table with the matching
/// rule of the file header. Accepts any id form the app stores or a provider lists (UI
/// namespace, vendor path, `:variant`, any case); a non-chat model is `NotSupported`, a model
/// the table does not list is `Unknown`.
#[must_use]
pub fn image_input_support(model: &str) -> ImageInputSupport {
    let name = normalized_model_name(model);
    if NON_CHAT_MARKERS.iter().any(|pattern| pattern.matches(&name)) {
        return ImageInputSupport::NotSupported;
    }
    IMAGE_INPUT_TABLE
        .iter()
        .find(|(pattern, _)| pattern.matches(&name))
        .map_or(ImageInputSupport::Unknown, |(_, support)| *support)
}

/// Whether `model` is known not to be a chat model at all (embedding, speech, transcription,
/// realtime, moderation, image generation, legacy completion), by the same normalization and
/// markers as `image_input_support`. `false` means "a chat model or not known".
#[must_use]
pub fn is_non_chat_model(model: &str) -> bool {
    let name = normalized_model_name(model);
    NON_CHAT_MARKERS.iter().any(|pattern| pattern.matches(&name))
}

/// The table lookup key: lowercased, without a `prefix::` UI namespace, without the vendor path
/// up to the last `/` and without a `:variant` suffix.
fn normalized_model_name(model: &str) -> String {
    let lower = model.trim().to_lowercase();
    let without_namespace = lower.rsplit_once("::").map_or(lower.as_str(), |(_, name)| name);
    let without_vendor = without_namespace.rsplit_once('/').map_or(without_namespace, |(_, name)| name);
    // The `::` namespace is already gone, so a remaining `:` starts a routing variant.
    let without_variant = without_vendor.split_once(':').map_or(without_vendor, |(name, _)| name);
    without_variant.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{ImageInputSupport, image_input_support, is_non_chat_model, normalized_model_name};

    fn assert_all(models: &[&str], expected: ImageInputSupport) {
        for model in models {
            assert_eq!(image_input_support(model), expected, "{model}");
        }
    }

    #[test]
    fn normalization_drops_namespace_vendor_path_and_case() {
        assert_eq!(normalized_model_name("open_router::Google/Gemini-2.0-Flash-001"), "gemini-2.0-flash-001");
        assert_eq!(normalized_model_name("groq::meta-llama/llama-4-scout-17b-16e-instruct"), "llama-4-scout-17b-16e-instruct");
        assert_eq!(normalized_model_name("../llm_models/Qwen3.6-35B.gguf"), "qwen3.6-35b.gguf");
        assert_eq!(normalized_model_name(" gpt-4o "), "gpt-4o");
        assert_eq!(normalized_model_name("open_router::openai/o3:online"), "o3");
        assert_eq!(normalized_model_name("google/gemma-3-12b-it:free"), "gemma-3-12b-it");
    }

    #[test]
    fn openrouter_variants_match_their_base_model() {
        assert_all(&["open_router::openai/o3:online", "openai/chatgpt-4o-latest:nitro", "open_router::anthropic/claude-3.7-sonnet:beta", "deepseek/deepseek-flash:free"], ImageInputSupport::Supported);
        assert_all(&["open_router::openai/o3-mini:nitro", "meta-llama/llama-3.3-70b-instruct:free"], ImageInputSupport::NotSupported);
    }

    #[test]
    fn non_chat_models_are_not_supported() {
        assert_all(&["text-embedding-3-small", "gemini-embedding-001", "openai/text-embedding-ada-002", "tts-1-hd", "gpt-4o-mini-tts", "gemini-2.5-flash-preview-tts", "whisper-1", "dall-e-3"], ImageInputSupport::NotSupported);
        // Dated snapshots and markers in the middle of the id are caught too.
        assert_all(&["gpt-4o-mini-tts-2025-03-20", "tts-1", "groq::whisper-large-v3-turbo", "distil-whisper-large-v3-en", "dall-e-2"], ImageInputSupport::NotSupported);
    }

    #[test]
    fn non_chat_detection_covers_only_the_markers() {
        for model in ["babbage-002", "davinci-002", "dall-e-2", "gpt-image-1", "omni-moderation-latest", "gpt-4o-realtime-preview", "gpt-realtime", "gpt-4o-mini-transcribe", "open_router::openai/text-embedding-3-large", "gpt-4o-mini-tts-2025-03-20"] {
            assert!(is_non_chat_model(model), "{model}");
            assert_eq!(image_input_support(model), ImageInputSupport::NotSupported, "{model}");
        }
        // Text-only chat models are NotSupported for images but still chat models.
        for model in ["gpt-3.5-turbo", "gpt-4o", "chatgpt-4o-latest", "gpt-4o-audio-preview", "my-local-model", ""] {
            assert!(!is_non_chat_model(model), "{model}");
        }
    }

    #[test]
    fn segment_markers_need_whole_segments() {
        assert!(super::has_segment("gpt-4o-mini-tts-2025-03-20", "tts"));
        assert!(super::has_segment("dall-e-3", "dall-e"));
        assert!(!super::has_segment("gpt-4o-mini-ttsx", "tts"));
        assert!(!super::has_segment("battstat", "tts"));
        assert!(!super::has_segment("dall-ex", "dall-e"));
    }

    #[test]
    fn openai_families() {
        assert_all(&["gpt-4o", "gpt-4o-mini", "gpt-4o-2024-08-06", "chatgpt-4o-latest", "gpt-4.1", "gpt-4.1-nano", "gpt-5", "gpt-5-mini", "gpt-5.1", "gpt-4-turbo", "open_router::openai/gpt-4o-mini"], ImageInputSupport::Supported);
        assert_all(&["gpt-4o-audio-preview", "gpt-4o-realtime-preview", "gpt-4o-transcribe", "gpt-3.5-turbo", "gpt-4", "gpt-4-turbo-preview", "gpt-oss-120b"], ImageInputSupport::NotSupported);
        assert_all(&["gpt-4o-search-preview", "gpt-5-search-api"], ImageInputSupport::Unknown);
    }

    #[test]
    fn openai_reasoning_models() {
        assert_all(&["o1", "o1-2024-12-17", "o1-pro", "o3", "o3-2025-04-16", "o3-pro", "o4-mini", "o4-mini-2025-04-16"], ImageInputSupport::Supported);
        assert_all(&["o1-mini", "o1-preview", "o3-mini", "o3-mini-2025-01-31"], ImageInputSupport::NotSupported);
        assert_all(&["o3-deep-research", "o4-mini-deep-research"], ImageInputSupport::Unknown);
    }

    #[test]
    fn anthropic_families() {
        assert_all(&["claude-3-opus-20240229", "claude-3-haiku-20240307", "claude-3-5-sonnet-latest", "claude-3-7-sonnet-20250219", "claude-sonnet-4-20250514", "claude-opus-4-1", "claude-haiku-4-5", "open_router::anthropic/claude-3.5-sonnet"], ImageInputSupport::Supported);
        assert_all(&["claude-2.1", "claude-instant-1.2"], ImageInputSupport::NotSupported);
        // Claude 3.5 Haiku is unlisted, and no Claude 3 / 3.5 family prefix may capture it.
        assert_all(&["claude-3-5-haiku-latest", "claude-3-5-haiku-20241022", "anthropic/claude-3.5-haiku", "open_router::anthropic/claude-3.5-haiku:beta"], ImageInputSupport::Unknown);
    }

    #[test]
    fn google_families() {
        assert_all(&["gemini-1.5-flash", "gemini-1.5-pro-002", "gemini-2.0-flash-001", "gemini-2.0-flash-lite", "gemini-2.5-flash", "gemini-2.5-pro", "open_router::google/gemini-2.0-flash-001", "gemma-3-27b-it", "google/gemma-3-12b-it:free"], ImageInputSupport::Supported);
        assert_all(&["gemma-3-1b-it", "gemma-3-270m"], ImageInputSupport::NotSupported);
        assert_all(&["gemini-2.5-flash-native-audio-preview", "gemini-1.0-pro"], ImageInputSupport::Unknown);
    }

    #[test]
    fn xai_families() {
        assert_all(&["grok-2-vision-1212", "grok-4", "grok-4-0709", "grok-4-fast-reasoning"], ImageInputSupport::Supported);
        assert_all(&["grok-2-1212", "grok-3", "grok-3-mini", "grok-3-mini-fast"], ImageInputSupport::NotSupported);
    }

    #[test]
    fn open_weight_families() {
        assert_all(&["groq::meta-llama/llama-4-scout-17b-16e-instruct", "llama-4-maverick-17b-128e-instruct", "pixtral-12b-2409", "qwen2.5-vl-72b-instruct"], ImageInputSupport::Supported);
        assert_all(&["groq::llama-3.1-8b-instant", "llama-3.3-70b-versatile"], ImageInputSupport::NotSupported);
    }

    #[test]
    fn deepseek_family() {
        assert_all(&["deepseek-flash", "deepseek-v4-flash", "deepseek-v4-flash-vision-exp", "deepseek-ai/deepseek-vl2-small", "deepseek-vl2"], ImageInputSupport::Supported);
        assert_all(&["deepseek-v4-pro", "deepseek-v4-pro-2026", "deepseek-chat", "deepseek-reasoner", "deepseek-v3.2", "deepseek/deepseek-r1", "deepseek-r1-distill-llama-70b"], ImageInputSupport::NotSupported);
        assert_all(&["deepseek-coder"], ImageInputSupport::Unknown);
    }

    #[test]
    fn unlisted_models_are_unknown_without_substring_guessing() {
        assert_all(&["", "my-local-model", "../llm_models/Qwen3.6-35B-A3B-IQ4_NL.gguf", "some-vl-model", "vision-thing", "omni-x", "llama-3.2-11b-vision-instruct"], ImageInputSupport::Unknown);
    }
}
