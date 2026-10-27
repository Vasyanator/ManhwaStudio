/*
File: cleaning/tools/ai_editor/engines/flux2_klein/prompt_cache.rs

Purpose:
The FLUX.2 klein BINDINGS of the shared prompt-cache controller (`../shared/prompt_cache.rs`,
protocol 23): the engine's `.prompt_cache.*` method names and timeouts, what it knows about
its current encoder (the `CacheContext` the «Кэшировать» gate reads) and its request header.
The library itself — one per checkpoint, every encoder variant may serve an entry, entries are
named presets keyed by their exact text — lives backend-side; the listing shapes, the gates,
the jobs and the drawing are shared.

Main responsibilities:
- name the four methods and their timeouts (`FLUX2_PROMPT_CACHE_METHODS`);
- describe the current encoder for the gate (`flux2_cache_context`: configured path, the
  download-mode provenance, the FLUX.2 precision ranking);
- build a `.prompt_cache.*` header from the normalized settings (`flux2_prompt_cache_header`).

Key structures:
- `FLUX2_PROMPT_CACHE_METHODS`

Key functions:
- `flux2_cache_context()`, `flux2_prompt_cache_header()`

Notes:
The prompt token budget is part of an entry's key, so `FLUX2_MAX_SEQ` (in `wire.rs`) must not be
lowered without invalidating the whole library. Which entry a run uses is the backend's
selection; nothing here re-derives it. The encoder captions live in `decisions.rs`.
*/

use super::*;
use ms_config::flux2_prompt_cache_precision_rank;
use ms_config::flux2_weight_format::FLUX2_PROMPT_PRECISION_FP8;

/// The FLUX.2 klein `.prompt_cache.*` method family. A build streams under the run timeout
/// (it reads the Qwen3 encoder); the other three are one-shot queries.
pub(super) const FLUX2_PROMPT_CACHE_METHODS: PromptCacheMethods = PromptCacheMethods {
    build: backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
    list: backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
    export: backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
    import: backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
    build_timeout: FLUX2_RUN_TIMEOUT,
    query_timeout: FLUX2_QUERY_TIMEOUT,
    log_label: FLUX2_LOG_LABEL,
};

/// What the «Кэшировать» gate knows about the typed text and the encoder `settings` select:
/// whether an encoder path is configured (the EFFECTIVE one), the encoder's provenance in
/// download mode, and the FLUX.2 precision ranking (`ms_config::flux2_prompt_cache_precision_rank`).
#[must_use]
pub(super) fn flux2_cache_context(settings: &Flux2KleinSettings) -> CacheContext<'_> {
    CacheContext {
        prompt: &settings.prompt,
        encoder_configured: !settings.effective_paths().text_encoder.is_empty(),
        current: current_encoder_provenance(settings),
        rank: flux2_prompt_cache_precision_rank,
        fp8_id: FLUX2_PROMPT_PRECISION_FP8,
        engine_block: None,
    }
}

/// The (source id, precision id) of the encoder the selection runs, known only in download mode
/// (the catalog names its files); a hand-picked encoder has no known source. The precision is
/// `fp8` when weight-only fp8 is in effect (never with a GGUF encoder), else the format id.
fn current_encoder_provenance(settings: &Flux2KleinSettings) -> Option<(String, String)> {
    if settings.source_mode() != Flux2SourceMode::Download {
        return None;
    }
    let fp8 = settings.text_encoder_fp8 && !settings.gguf_text_encoder_in_effect();
    let precision = if fp8 { FLUX2_PROMPT_PRECISION_FP8 } else { settings.weight_format(Flux2WeightAxis::TextEncoder).wire() };
    Some((settings.encoder_source().wire().to_string(), precision.to_string()))
}

/// Builds the request header of a prompt-cache call from `settings` (which must already be
/// `normalized()`): their `params` (encoder paths, library id, encoder source, precision
/// order) plus the operation's own fields beside them ([`prompt_cache_header`]).
#[must_use]
pub(super) fn flux2_prompt_cache_header(settings: &Flux2KleinSettings, extra: &[(&str, Value)]) -> Value {
    // The mode is `false` here as on every other query path: a prompt-cache call carries no
    // mask.
    prompt_cache_header(settings.to_params(false), extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(file: &str, source: &str, format: &str, current: Option<bool>) -> PromptLibraryEntry {
        PromptLibraryEntry {
            file: file.to_string(),
            encoder_source: source.to_string(),
            encoder_format: format.to_string(),
            current_encoder: current,
            ..PromptLibraryEntry::default()
        }
    }

    fn preset(name: &str, entries: Vec<PromptLibraryEntry>) -> PromptLibraryText {
        PromptLibraryText { prompt: "remove the sfx".to_string(), name: name.to_string(), selected: entries[0].clone(), entries }
    }

    /// Download-mode settings on the uncensored 9B encoder in `format`.
    fn download_settings(format: Flux2WeightFormat, fp8: bool) -> Flux2KleinSettings {
        Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            uncensored_text_encoder: true,
            text_encoder_format: format.wire().to_string(),
            text_encoder_fp8: fp8,
            ..cacheable_settings()
        }
    }

    /// The shared gate over the FLUX.2 context of `settings`.
    fn flux2_cache_button(
        settings: &Flux2KleinSettings,
        hit: Option<&PromptLibraryText>,
        name: &str,
        prompt_cached: Option<bool>,
        text_encoder_available: Option<bool>,
        ready: bool,
    ) -> CacheButton {
        cache_button(&flux2_cache_context(settings), hit, name, prompt_cached, text_encoder_available, ready)
    }

    #[test]
    fn the_cache_button_closes_for_every_basic_reason_first() {
        let ready = cacheable_settings();
        assert_eq!(flux2_cache_button(&ready, None, "", None, None, true), CacheButton::Cache);
        assert_eq!(flux2_cache_button(&ready, None, "", None, None, false), CacheButton::Closed(CacheBlock::NotReady));
        assert_eq!(flux2_cache_button(&ready, None, "", None, Some(false), true), CacheButton::Closed(CacheBlock::NoEncoder));
        let blank = Flux2KleinSettings { prompt: "  ".to_string(), ..cacheable_settings() };
        assert_eq!(flux2_cache_button(&blank, None, "", None, None, true), CacheButton::Closed(CacheBlock::NothingToEncode));
        let no_path = Flux2KleinSettings { text_encoder_path: String::new(), ..cacheable_settings() };
        assert_eq!(flux2_cache_button(&no_path, None, "", None, None, true), CacheButton::Closed(CacheBlock::NothingToEncode));
        // `None` is "not known" and closes nothing by itself.
        assert_eq!(flux2_cache_button(&ready, None, "", None, None, true), CacheButton::Cache);
    }

    #[test]
    fn an_entry_of_the_current_encoder_closes_caching_but_a_new_name_renames() {
        let settings = cacheable_settings();
        let hit = preset("sfx", vec![entry("a.msprompt", "", "bf16", Some(true))]);
        assert_eq!(flux2_cache_button(&settings, Some(&hit), "sfx", None, None, true), CacheButton::Closed(CacheBlock::AlreadyCached));
        assert_eq!(flux2_cache_button(&settings, Some(&hit), "  ", Some(true), None, true), CacheButton::Closed(CacheBlock::AlreadyCached), "an empty field keeps the name");
        assert_eq!(flux2_cache_button(&settings, Some(&hit), "sound effects", Some(true), None, true), CacheButton::Rename);
        // A name the backend stored cleaned and capped is no pending rename once it lands.
        let long = format!("{}\u{200B}tail", "n".repeat(MAX_PRESET_NAME_CHARS));
        let stored = preset(&normalize_preset_name(&long), vec![entry("a.msprompt", "", "bf16", Some(true))]);
        assert_eq!(flux2_cache_button(&settings, Some(&stored), &long, Some(true), None, true), CacheButton::Closed(CacheBlock::AlreadyCached));
        assert_eq!(flux2_cache_button(&settings, Some(&hit), " sfx\t", Some(true), None, true), CacheButton::Closed(CacheBlock::AlreadyCached));
        // The run still needs something (`.status` says not served): caching stays open.
        assert_eq!(flux2_cache_button(&settings, Some(&hit), "sfx", Some(false), None, true), CacheButton::Cache);
        // Another encoder's entry is no reason to close anything in manual mode (its source is
        // not known here).
        let other = preset("sfx", vec![entry("b.msprompt", "stock", "bf16", Some(false))]);
        assert_eq!(flux2_cache_button(&settings, Some(&other), "sfx", Some(true), None, true), CacheButton::Cache);
    }

    #[test]
    fn a_more_precise_entry_of_the_same_source_closes_caching() {
        let q4 = download_settings(Flux2WeightFormat::Q4KM, false);
        let q8_same = entry("q8.msprompt", "uncensored", "q8_0", Some(false));
        let hit = preset("sfx", vec![q8_same.clone()]);
        assert_eq!(flux2_cache_button(&q4, Some(&hit), "sfx", Some(true), None, true), CacheButton::Closed(CacheBlock::MorePrecise(q8_same)));
        // The same entry is LESS precise than a bf16 encoder: caching with bf16 stays open.
        let bf16 = download_settings(Flux2WeightFormat::Bf16, false);
        assert_eq!(flux2_cache_button(&bf16, Some(&hit), "sfx", Some(true), None, true), CacheButton::Cache);
        // fp8 ranks after q8_0: a q8_0 entry closes an fp8 build.
        let fp8 = download_settings(Flux2WeightFormat::Bf16, true);
        assert!(matches!(flux2_cache_button(&fp8, Some(&hit), "sfx", Some(true), None, true), CacheButton::Closed(CacheBlock::MorePrecise(_))));
        // The OTHER source's precise entry is only a fallback: caching the own source stays open.
        let stock_bf16 = preset("sfx", vec![entry("s.msprompt", "stock", "bf16", Some(false))]);
        assert_eq!(flux2_cache_button(&q4, Some(&stock_bf16), "sfx", Some(true), None, true), CacheButton::Cache);
        // An equally precise entry is not "strictly more precise".
        let same = preset("sfx", vec![entry("q4.msprompt", "uncensored", "q4_k_m", Some(false))]);
        assert_eq!(flux2_cache_button(&q4, Some(&same), "sfx", Some(true), None, true), CacheButton::Cache);
    }

    #[test]
    fn export_needs_a_preset_and_import_only_the_backend() {
        let settings = cacheable_settings();
        let ctx = flux2_cache_context(&settings);
        let hit = preset("sfx", vec![entry("a.msprompt", "", "bf16", None)]);
        let open = prompt_cache_gates(&ctx, Some(&hit), "", None, None, true, false);
        assert!(open.export && open.import);
        let no_hit = prompt_cache_gates(&ctx, None, "", None, None, true, false);
        assert!(!no_hit.export && no_hit.import);
        for (backend, busy) in [(false, false), (true, true)] {
            let closed = prompt_cache_gates(&ctx, Some(&hit), "", None, None, backend, busy);
            assert_eq!(closed, PromptCacheGates { build: CacheButton::Closed(CacheBlock::NotReady), export: false, import: false });
        }
    }

    #[test]
    fn prompt_cache_headers_carry_the_settings_and_the_operation_fields() {
        let settings = cacheable_settings().normalized();
        let plain = flux2_prompt_cache_header(&settings, &[]);
        assert_eq!(plain["params"]["text_encoder_path"], json!("/models/qwen3"));
        assert_eq!(plain["params"]["prompt"], json!("remove the sfx"));
        assert_eq!(plain["params"]["prompt_library"], json!("flux2_klein_9b"));
        // The operation's own fields sit BESIDE `params`, where the backend reads them.
        let named = flux2_prompt_cache_header(&settings, &[("name", json!("sfx"))]);
        assert_eq!(named["name"], json!("sfx"));
        assert!(named["params"].get("name").is_none());
        let exported = flux2_prompt_cache_header(&settings, &[("file", json!("a.msprompt")), ("path", json!("/tmp/a.msprompt"))]);
        assert_eq!(exported["file"], json!("a.msprompt"));
        assert_eq!(exported["path"], json!("/tmp/a.msprompt"));
    }
}
