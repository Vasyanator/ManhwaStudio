/*
File: cleaning/tools/ai_editor/engines/flux2_klein/prompt_cache.rs

Purpose:
The FLUX.2 klein side of the model's shared prompt-cache LIBRARY (protocol 23): what the
«Кэшировать» button offers for the typed text, which of the library operations are open, and
the `.prompt_cache.*` wire block. The library itself — one per checkpoint, every encoder variant
may serve an entry, entries are named presets keyed by their exact text — lives backend-side;
its listing shapes and the preset/picker rules are the shared `../shared/prompt_library.rs`.

Main responsibilities:
- decide «Кэшировать» / «Переименовать» / closed-with-a-reason (`flux2_cache_button`) and the
  export / import gates (`flux2_prompt_cache_gates`);
- answer whether the last `.status` still describes the prompt currently typed
  (`prompt_cache_state_for`);
- build the `.prompt_cache.*` request headers and parse every answer.

Key structures:
- `Flux2CacheButton`, `Flux2CacheBlock`, `Flux2PromptCacheGates`
- `Flux2PromptCacheOutcome`, `Flux2PromptCacheAction`

Key functions:
- `flux2_cache_button()`, `flux2_prompt_cache_gates()`, `prompt_cache_state_for()`
- `flux2_prompt_cache_header()`, `flux2_prompt_cache_call()`
- `build_flux2_prompt_cache()`, `list_flux2_prompt_caches()`, `export_flux2_prompt_cache()`,
  `import_flux2_prompt_cache()` and their parsers

Notes:
The prompt token budget is part of an entry's key, so `FLUX2_MAX_SEQ` (in `wire.rs`) must not be
lowered without invalidating the whole library. Which entry a run uses is the backend's
selection; this file never re-derives it. A rename travels as a `.build` with a different
`name`: a text the current encoder already serves is not encoded again. The one-line panel
verdict (`flux2_prompt_cache_line`) and the encoder captions live in `decisions.rs`.
*/

use super::*;
use ms_config::flux2_weight_format::FLUX2_PROMPT_PRECISION_FP8;
use ms_config::flux2_prompt_cache_precision_rank;

/// Extension of a prompt-cache file, without the dot.
///
/// A file-format identifier, not prose: it is what the file is NAMED on disk and what the
/// open dialog filters on, so it is the same in every language. Only the human caption of the
/// filter is localized.
pub(super) const FLUX2_PROMPT_CACHE_EXTENSION: &str = "msprompt";

/// What a finished prompt-cache worker did. One enum for the three operations because at most
/// one of them is ever in flight, so they share a single channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux2PromptCacheOutcome {
    /// `.build` answered: the entries it WROTE (empty when every text was already served by the
    /// current encoder), and whether the request was a rename.
    Built { written: Vec<PromptLibraryEntry>, renamed: bool },
    /// A library entry was written to this file.
    Exported(PathBuf),
    /// A file was taken into the library.
    Imported {
        /// The preset name the entry carries, else its file name; empty when neither came back.
        name: String,
        /// Whether it landed in the library this engine lists. `None` when the answer did not
        /// say — no warning is invented then.
        library_matches: Option<bool>,
        /// The library already held this very entry; nothing was written.
        already_present: bool,
    },
}

/// The one prompt-cache control the user pressed in a frame.
///
/// An enum rather than booleans: only one operation can run at a time (they share the
/// backend's pipeline and the one channel), and this makes that a property of the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2PromptCacheAction {
    /// «Кэшировать»: encode with the current encoder and save under the name field.
    Build,
    /// «Переименовать»: the current encoder already serves the text; only the name changes.
    Rename,
    Export,
    Import,
}

/// Why «Кэшировать» is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux2CacheBlock {
    /// The backend is unreachable, or another operation holds the pipeline.
    NotReady,
    /// The backend reports no text encoder for the configured path.
    NoEncoder,
    /// An empty prompt or no encoder path at all.
    NothingToEncode,
    /// An entry of the SAME encoder source with a strictly more precise format already exists
    /// for this text: a new entry would never be chosen. Carries that entry, for its caption.
    MorePrecise(PromptLibraryEntry),
    /// The current encoder's own entry exists and the name field asks for no rename.
    AlreadyCached,
}

/// What the «Кэшировать» button offers for the typed text right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux2CacheButton {
    /// «Кэшировать»: makes (or renames, when the name differs) the preset with the current
    /// encoder.
    Cache,
    /// «Переименовать»: the current encoder already serves the text, the name field differs
    /// from the preset's name.
    Rename,
    Closed(Flux2CacheBlock),
}

/// Decides what «Кэшировать» offers.
///
/// `hit` is the preset of the typed text (`PromptLibraryListing::text_for`), `name` the name
/// field, `prompt_cached` the three-state `.status` answer for the typed prompt and
/// `text_encoder_available` the backend's own encoder answer (`None` = not known, never
/// `false`). `ready` = a reachable backend and no operation holding the pipeline.
///
/// Building needs a prompt and an encoder. A text the library already holds closes it when a
/// new entry would never be USED (user rule): the current encoder's own entry exists (only a
/// different name keeps it open, as «Переименовать»), or — while the current encoder is known
/// from the selection (download mode) — an entry of the same source with a strictly more
/// precise format exists. Neither closes it while `.status` says the prompt is NOT served
/// (`Some(false)`): the run then still needs something this build would make (the empty
/// negative prompt under guidance, an entry the backend skipped).
#[must_use]
pub(super) fn flux2_cache_button(
    settings: &Flux2KleinSettings,
    hit: Option<&PromptLibraryText>,
    name: &str,
    prompt_cached: Option<bool>,
    text_encoder_available: Option<bool>,
    ready: bool,
) -> Flux2CacheButton {
    if !ready {
        return Flux2CacheButton::Closed(Flux2CacheBlock::NotReady);
    }
    if text_encoder_available == Some(false) {
        return Flux2CacheButton::Closed(Flux2CacheBlock::NoEncoder);
    }
    if settings.prompt.trim().is_empty() || settings.effective_paths().text_encoder.is_empty() {
        return Flux2CacheButton::Closed(Flux2CacheBlock::NothingToEncode);
    }
    let Some(hit) = hit else {
        return Flux2CacheButton::Cache;
    };
    if prompt_cached == Some(false) {
        return Flux2CacheButton::Cache;
    }
    if hit.entries.iter().any(|entry| entry.current_encoder == Some(true)) {
        // Compared as the backend STORES names: a name it would clean or cap is no rename.
        let wanted = normalize_preset_name(name);
        return if !wanted.is_empty() && wanted != normalize_preset_name(&hit.display_name()) {
            Flux2CacheButton::Rename
        } else {
            Flux2CacheButton::Closed(Flux2CacheBlock::AlreadyCached)
        };
    }
    if let Some((source, precision)) = current_encoder_provenance(settings) {
        let current_rank = flux2_prompt_cache_precision_rank(&precision);
        let better = hit
            .entries
            .iter()
            .filter(|entry| entry.encoder_source == source)
            .filter(|entry| flux2_prompt_cache_precision_rank(entry.precision_id(FLUX2_PROMPT_PRECISION_FP8)) < current_rank)
            .min_by_key(|entry| flux2_prompt_cache_precision_rank(entry.precision_id(FLUX2_PROMPT_PRECISION_FP8)));
        if let Some(better) = better {
            return Flux2CacheButton::Closed(Flux2CacheBlock::MorePrecise(better.clone()));
        }
    }
    Flux2CacheButton::Cache
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

/// Which prompt-cache controls are live: «Кэшировать» ([`flux2_cache_button`]), «Экспорт» (the
/// typed text is a preset: its selected entry's file is what is written) and «Импорт» (needs
/// only the backend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Flux2PromptCacheGates {
    pub(super) build: Flux2CacheButton,
    pub(super) export: bool,
    pub(super) import: bool,
}

/// Decides every prompt-cache control at once; see [`flux2_cache_button`] for the build half.
/// `busy` is [`flux2_pipeline_busy`]: a cache operation, a generation OR a component action in
/// flight, because all three share the one progress bar and the backend's one pipeline.
#[must_use]
pub(super) fn flux2_prompt_cache_gates(
    settings: &Flux2KleinSettings,
    hit: Option<&PromptLibraryText>,
    name: &str,
    prompt_cached: Option<bool>,
    text_encoder_available: Option<bool>,
    backend_available: bool,
    busy: bool,
) -> Flux2PromptCacheGates {
    let ready = backend_available && !busy;
    Flux2PromptCacheGates {
        build: flux2_cache_button(settings, hit, name, prompt_cached, text_encoder_available, ready),
        export: ready && hit.is_some(),
        import: ready,
    }
}

/// Decides the three-state `prompt_cached` from the last `.status` answer.
///
/// `asked_about` is the trimmed prompt that answer was asked about; `current_prompt` is what
/// the field holds now. The answer counts only while the two still agree — otherwise it
/// describes a prompt the user has already typed away from, and "not known" is the only honest
/// report.
pub(super) fn prompt_cache_state_for(
    status: Option<&Flux2Status>,
    asked_about: Option<&str>,
    current_prompt: &str,
) -> Option<bool> {
    let status = status?;
    if asked_about? != current_prompt.trim() {
        return None;
    }
    status.prompt_cached
}

// ---------------------------------------------------------------------------------------
// Wire
// ---------------------------------------------------------------------------------------

/// Builds the request header of a prompt-cache call: the normalized settings under `params`
/// (they carry the encoder paths, the library id, the encoder source and the precision order),
/// and the operation's own fields (`name`, `file`, `path`) BESIDE it at the top level, which is
/// where the backend reads them from.
///
/// `settings` must already be `normalized()`.
#[must_use]
pub(super) fn flux2_prompt_cache_header(settings: &Flux2KleinSettings, extra: &[(&str, Value)]) -> Value {
    // The mode is `false` here as on every other query path: a prompt-cache call carries no
    // mask.
    let mut header = json!({ "params": settings.to_params(false) });
    // `json!` above always builds an object; the guard keeps this total rather than relying
    // on that from a distance.
    if let Some(map) = header.as_object_mut() {
        for (key, value) in extra {
            map.insert((*key).to_string(), value.clone());
        }
    }
    header
}

/// Makes the library entries of the CURRENT encoder for every text the run needs, under the
/// preset name the header carries (`renamed`: the request is a rename, for the outcome line).
///
/// Streaming, because reading the Qwen3 encoder takes far longer than a call may block for: the
/// progress frames have the same shape as a generation's and drive the same bar. `claim` is the
/// progress claim taken on the GUI thread; every write is dropped once a newer operation — or a
/// cancel — has retired it, and the bar is cleared on EVERY exit.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable.
pub(super) fn build_flux2_prompt_cache(
    header: Value,
    renamed: bool,
    progress: &Arc<Mutex<EngineProgress>>,
    claim: &ProgressClaim,
) -> Result<Flux2PromptCacheOutcome, String> {
    let outcome = stream_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
        header,
        &[],
        FLUX2_RUN_TIMEOUT,
        |id| claim.publish_request_id(id),
        |frame| publish_progress_frame(progress, claim.generation, frame),
    )
    .map(|(answer, _blob)| parse_flux2_prompt_cache_build(&answer, renamed));
    update_progress(progress, claim.generation, |state| state.active = false);
    outcome
}

/// Reads a `.prompt_cache.build` answer: the entries it wrote (an absent list = none) and
/// whether the preset was renamed — the request was a rename, or the answer says one happened
/// (a «Кэшировать» under a new name renames the preset too).
pub(super) fn parse_flux2_prompt_cache_build(header: &Value, rename_requested: bool) -> Flux2PromptCacheOutcome {
    let written = header
        .get("written")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_entry_info).collect())
        .unwrap_or_default();
    let renamed = rename_requested || header.get("renamed").and_then(Value::as_bool).unwrap_or(false);
    Flux2PromptCacheOutcome::Built { written, renamed }
}

/// Lists the library `params.prompt_library`.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable.
pub(super) fn list_flux2_prompt_caches(header: Value) -> Result<PromptLibraryListing, String> {
    let response = flux2_prompt_cache_call(backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST, header)?;
    Ok(parse_prompt_library_listing(&response))
}

/// Writes library entry `file` to the file `path` (both top-level in `header`).
///
/// # Errors
/// Returns the backend's own message, or a transport message when it is unreachable.
pub(super) fn export_flux2_prompt_cache(header: Value) -> Result<(), String> {
    flux2_prompt_cache_call(backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT, header).map(|_response| ())
}

/// Takes the file `path` into the library it belongs to.
///
/// # Errors
/// Returns the backend's own message — which is what refuses a file made before the encoder
/// fix — or a transport message when it is unreachable.
pub(super) fn import_flux2_prompt_cache(header: Value) -> Result<Flux2PromptCacheOutcome, String> {
    let response = flux2_prompt_cache_call(backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT, header)?;
    Ok(parse_flux2_prompt_cache_import(&response))
}

/// Reads a `.prompt_cache.import` answer. `library_matches` stays `None` when the answer does
/// not carry it (no warning is invented); an absent `already_present` is `false`.
pub(super) fn parse_flux2_prompt_cache_import(header: &Value) -> Flux2PromptCacheOutcome {
    let text = |key: &str| header.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let name = Some(text("name")).filter(|name| !name.is_empty()).unwrap_or_else(|| text("file"));
    Flux2PromptCacheOutcome::Imported {
        name,
        library_matches: header.get("library_matches").and_then(Value::as_bool),
        already_present: header.get("already_present").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// One-shot prompt-cache call, returning the response header.
///
/// # Errors
/// Returns the backend's own message for a refused request, or "unknown method" from a backend
/// without the handler, or the offline message when it cannot be reached at all. None of them
/// is a panic and none is swallowed.
pub(super) fn flux2_prompt_cache_call(method: &'static str, header: Value) -> Result<Value, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (response, _blob) = client.call(method, header, &[], FLUX2_QUERY_TIMEOUT).map_err(map_call_error)?;
    Ok(response)
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

    #[test]
    fn the_cache_button_closes_for_every_basic_reason_first() {
        let ready = cacheable_settings();
        assert_eq!(flux2_cache_button(&ready, None, "", None, None, true), Flux2CacheButton::Cache);
        assert_eq!(flux2_cache_button(&ready, None, "", None, None, false), Flux2CacheButton::Closed(Flux2CacheBlock::NotReady));
        assert_eq!(flux2_cache_button(&ready, None, "", None, Some(false), true), Flux2CacheButton::Closed(Flux2CacheBlock::NoEncoder));
        let blank = Flux2KleinSettings { prompt: "  ".to_string(), ..cacheable_settings() };
        assert_eq!(flux2_cache_button(&blank, None, "", None, None, true), Flux2CacheButton::Closed(Flux2CacheBlock::NothingToEncode));
        let no_path = Flux2KleinSettings { text_encoder_path: String::new(), ..cacheable_settings() };
        assert_eq!(flux2_cache_button(&no_path, None, "", None, None, true), Flux2CacheButton::Closed(Flux2CacheBlock::NothingToEncode));
        // `None` is "not known" and closes nothing by itself.
        assert_eq!(flux2_cache_button(&ready, None, "", None, None, true), Flux2CacheButton::Cache);
    }

    #[test]
    fn an_entry_of_the_current_encoder_closes_caching_but_a_new_name_renames() {
        let settings = cacheable_settings();
        let hit = preset("sfx", vec![entry("a.msprompt", "", "bf16", Some(true))]);
        assert_eq!(flux2_cache_button(&settings, Some(&hit), "sfx", None, None, true), Flux2CacheButton::Closed(Flux2CacheBlock::AlreadyCached));
        assert_eq!(flux2_cache_button(&settings, Some(&hit), "  ", Some(true), None, true), Flux2CacheButton::Closed(Flux2CacheBlock::AlreadyCached), "an empty field keeps the name");
        assert_eq!(flux2_cache_button(&settings, Some(&hit), "sound effects", Some(true), None, true), Flux2CacheButton::Rename);
        // A name the backend stored cleaned and capped is no pending rename once it lands.
        let long = format!("{}\u{200B}tail", "n".repeat(MAX_PRESET_NAME_CHARS));
        let stored = preset(&normalize_preset_name(&long), vec![entry("a.msprompt", "", "bf16", Some(true))]);
        assert_eq!(flux2_cache_button(&settings, Some(&stored), &long, Some(true), None, true), Flux2CacheButton::Closed(Flux2CacheBlock::AlreadyCached));
        assert_eq!(flux2_cache_button(&settings, Some(&hit), " sfx\t", Some(true), None, true), Flux2CacheButton::Closed(Flux2CacheBlock::AlreadyCached));
        // The run still needs something (`.status` says not served): caching stays open.
        assert_eq!(flux2_cache_button(&settings, Some(&hit), "sfx", Some(false), None, true), Flux2CacheButton::Cache);
        // Another encoder's entry is no reason to close anything in manual mode (its source is
        // not known here).
        let other = preset("sfx", vec![entry("b.msprompt", "stock", "bf16", Some(false))]);
        assert_eq!(flux2_cache_button(&settings, Some(&other), "sfx", Some(true), None, true), Flux2CacheButton::Cache);
    }

    #[test]
    fn a_more_precise_entry_of_the_same_source_closes_caching() {
        let q4 = download_settings(Flux2WeightFormat::Q4KM, false);
        let q8_same = entry("q8.msprompt", "uncensored", "q8_0", Some(false));
        let hit = preset("sfx", vec![q8_same.clone()]);
        assert_eq!(flux2_cache_button(&q4, Some(&hit), "sfx", Some(true), None, true), Flux2CacheButton::Closed(Flux2CacheBlock::MorePrecise(q8_same)));
        // The same entry is LESS precise than a bf16 encoder: caching with bf16 stays open.
        let bf16 = download_settings(Flux2WeightFormat::Bf16, false);
        assert_eq!(flux2_cache_button(&bf16, Some(&hit), "sfx", Some(true), None, true), Flux2CacheButton::Cache);
        // fp8 ranks after q8_0: a q8_0 entry closes an fp8 build.
        let fp8 = download_settings(Flux2WeightFormat::Bf16, true);
        assert!(matches!(flux2_cache_button(&fp8, Some(&hit), "sfx", Some(true), None, true), Flux2CacheButton::Closed(Flux2CacheBlock::MorePrecise(_))));
        // The OTHER source's precise entry is only a fallback: caching the own source stays open.
        let stock_bf16 = preset("sfx", vec![entry("s.msprompt", "stock", "bf16", Some(false))]);
        assert_eq!(flux2_cache_button(&q4, Some(&stock_bf16), "sfx", Some(true), None, true), Flux2CacheButton::Cache);
        // An equally precise entry is not "strictly more precise".
        let same = preset("sfx", vec![entry("q4.msprompt", "uncensored", "q4_k_m", Some(false))]);
        assert_eq!(flux2_cache_button(&q4, Some(&same), "sfx", Some(true), None, true), Flux2CacheButton::Cache);
    }

    #[test]
    fn export_needs_a_preset_and_import_only_the_backend() {
        let settings = cacheable_settings();
        let hit = preset("sfx", vec![entry("a.msprompt", "", "bf16", None)]);
        let open = flux2_prompt_cache_gates(&settings, Some(&hit), "", None, None, true, false);
        assert!(open.export && open.import);
        let no_hit = flux2_prompt_cache_gates(&settings, None, "", None, None, true, false);
        assert!(!no_hit.export && no_hit.import);
        for (backend, busy) in [(false, false), (true, true)] {
            let closed = flux2_prompt_cache_gates(&settings, Some(&hit), "", None, None, backend, busy);
            assert_eq!(
                closed,
                Flux2PromptCacheGates { build: Flux2CacheButton::Closed(Flux2CacheBlock::NotReady), export: false, import: false }
            );
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

    #[test]
    fn build_and_import_answers_parse_tolerantly() {
        let built = parse_flux2_prompt_cache_build(&json!({ "encoded": true, "written": [{ "file": "a.msprompt" }, 3] }), false);
        assert_eq!(built, Flux2PromptCacheOutcome::Built { written: vec![PromptLibraryEntry { file: "a.msprompt".to_string(), ..PromptLibraryEntry::default() }], renamed: false });
        assert_eq!(parse_flux2_prompt_cache_build(&json!({}), true), Flux2PromptCacheOutcome::Built { written: Vec::new(), renamed: true });
        assert_eq!(parse_flux2_prompt_cache_build(&json!({ "renamed": true }), false), Flux2PromptCacheOutcome::Built { written: Vec::new(), renamed: true });

        let imported = parse_flux2_prompt_cache_import(&json!({ "file": "a.msprompt", "name": "sfx", "library_matches": false, "already_present": true }));
        assert_eq!(imported, Flux2PromptCacheOutcome::Imported { name: "sfx".to_string(), library_matches: Some(false), already_present: true });
        let bare = parse_flux2_prompt_cache_import(&json!({ "file": "a.msprompt" }));
        assert_eq!(bare, Flux2PromptCacheOutcome::Imported { name: "a.msprompt".to_string(), library_matches: None, already_present: false });
    }
}
