/*
File: cleaning/tools/ai_editor/engines/flux2_klein/prompt_cache.rs

Purpose:
The prompt-cache library of the FLUX.2 klein engine: the saved `.msprompt` entries that
let a run start without loading the text encoder, the gates that say which of the six
library operations are open right now, and the whole `.prompt_cache.*` wire block.

Main responsibilities:
- own the library entry/listing/load shapes and the outcome an import reports;
- decide, from the settings and the current job state, which library operations are open
  and why one is closed (`flux2_prompt_cache_gates`);
- answer whether a cached prompt still describes the prompt currently typed
  (`prompt_cache_state_for`);
- build the `.prompt_cache.*` request headers and parse every answer.

Key structures:
- `Flux2PromptCacheEntry`, `Flux2PromptCacheList`, `Flux2PromptCacheLoad`
- `Flux2PromptCacheOutcome`, `Flux2PromptCacheAction`, `Flux2PromptCacheGates`

Key functions:
- `flux2_prompt_cache_gates()`, `prompt_cache_state_for()`, `format_prompt_cache_created()`
- `flux2_prompt_cache_header()`, `flux2_prompt_cache_call()`
- `build_flux2_prompt_cache()`, `list_flux2_prompt_caches()`, `save_flux2_prompt_cache()`,
  `load_flux2_prompt_cache()`, `export_flux2_prompt_cache()`, `import_flux2_prompt_cache()`
- `prompt_cache_entry_tooltip()`

Notes:
The prompt token budget is part of the cache key, so `FLUX2_MAX_SEQ` (in `wire.rs`) must
not be lowered without invalidating the whole saved library. The one-line panel verdict
(`flux2_prompt_cache_line`) lives in `decisions.rs` with the other panel decisions.
*/

use super::*;

/// Extension of a saved prompt-cache file, without the dot.
///
/// A file-format identifier, not prose: it is what the file is NAMED on disk and what
/// the open dialog filters on, so it is the same in every language. Only the human
/// caption of the filter is localized.
pub(super) const FLUX2_PROMPT_CACHE_EXTENSION: &str = "msprompt";

/// One saved entry of the prompt-cache LIBRARY, as reported by `.prompt_cache.list`.
///
/// The library lives backend-side (a `prompt_cache/` directory next to `fonts/`, split
/// into one folder per encoder FAMILY); this side never builds a path into it and works
/// with names alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2PromptCacheEntry {
    /// The entry's identity in the library, and what `.save`/`.load`/`.export` name.
    pub(super) name: String,
    /// The encoder family the entry itself belongs to, empty when the backend did not
    /// report one. Every entry carries its own, because a listing made on a machine with
    /// NO encoder spans every family in the library — see [`Flux2PromptCacheList::family`].
    /// It is display-only: the wire identifies an entry by NAME alone.
    pub(super) family: String,
    /// The prompt the entry was built from. Shown on hover, so a name like "sfx" can
    /// still be checked against what it actually encodes.
    pub(super) prompt: String,
    /// When it was created, already formatted for display — see
    /// [`format_prompt_cache_created`]. Empty when the backend reported nothing usable.
    pub(super) created: String,
}

impl Flux2PromptCacheEntry {
    /// The row caption: the bare name, or `<family> / <name>` while the listing spans more
    /// than one family (`show_family`) and the entry actually reports one.
    ///
    /// The family is never part of the identity sent to the backend — the wire names an
    /// entry by `name` alone — so this exists only to keep the user from mistaking another
    /// encoder's cache for one of their own.
    pub(super) fn label(&self, show_family: bool) -> String {
        if !show_family || self.family.is_empty() {
            return self.name.clone();
        }
        tf!(
            "cleaning.tools.flux2_klein.prompt_cache_entry_with_family",
            family = self.family,
            name = self.name
        )
    }
}

/// The `.prompt_cache.list` answer: the ACTIVE encoder family plus the saved entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2PromptCacheList {
    /// Name of the ACTIVE encoder family, i.e. the one the current settings select.
    ///
    /// Empty means there is none — either the backend did not report one, or (the case
    /// this field now has to distinguish) no encoder is installed at all, and the listing
    /// then spans EVERY family in the library. The rows are shown with their own
    /// [`Flux2PromptCacheEntry::family`] in that case, so the user can see that what they
    /// are looking at is not only "their" caches.
    pub(super) family: String,
    /// The backend's `text_encoder_available` for the paths this listing was asked about;
    /// `None` when it did not report the field. Same three-state rule as
    /// [`Flux2Status::text_encoder_available`], and the fallback source for the warning
    /// line when no `.status` answer carries one.
    pub(super) text_encoder_available: Option<bool>,
    pub(super) entries: Vec<Flux2PromptCacheEntry>,
}

/// A finished `.prompt_cache.load`: the prompt the entry was built from, and how much of
/// the entry's identity the backend could actually check.
///
/// `encoder_verified` is the backend's own three-state answer: `Some(true)` the encoder
/// fingerprint in the file was compared against the encoder on disk, `Some(false)` there
/// is no local encoder to compare against and the file's metadata was taken on trust (the
/// format marker, the version, the sequence length, the dtype and the fp8 flag are checked
/// in both cases), `None` the backend did not say. Only `Some(false)` is reported to the
/// user, and only once, as the outcome of that load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2PromptCacheLoad {
    /// The prompt the entry encodes, trimmed. Empty means the entry carried none, which is
    /// refused rather than applied.
    pub(super) prompt: String,
    pub(super) encoder_verified: Option<bool>,
}

/// What a finished prompt-cache worker did. One enum for all five operations because at
/// most one of them is ever in flight, so they share a single channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux2PromptCacheOutcome {
    /// The embeddings of the current prompt now live in the backend's live cache.
    Built,
    /// They were stored in the library under this name.
    Saved(String),
    /// A library entry was loaded.
    Loaded(Flux2PromptCacheLoad),
    /// A library entry was written to this file.
    Exported(PathBuf),
    /// A file was taken into the library.
    Imported {
        /// The entry's name in the library, empty when the backend did not report one.
        name: String,
        /// Whether it landed in the CURRENT encoder family. `None` when the answer did
        /// not say and nothing local could decide it — no warning is shown then, because
        /// inventing one would be a guess either way.
        family_matches: Option<bool>,
    },
}

/// The one prompt-cache control the user pressed in a frame.
///
/// An enum rather than five booleans: only one operation can run at a time (they share
/// the backend's pipeline and the one channel), and this makes that a property of the
/// type instead of a rule the UI has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2PromptCacheAction {
    Build,
    Save,
    Load,
    Export,
    Import,
}

/// Which of the five prompt-cache controls may be used right now.
///
/// A named struct with a free constructor rather than five expressions inline in the UI,
/// for the same reason [`flux2_run_block_reason`] is a free function: the gates are the
/// contract worth testing, and a [`Flux2PanelCtx`] cannot be built outside a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Flux2PromptCacheGates {
    pub(super) build: bool,
    pub(super) save: bool,
    pub(super) load: bool,
    pub(super) export: bool,
    pub(super) import: bool,
}

/// Decides which prompt-cache controls are live.
///
/// `prompt_cached` is the three-state answer from `.status` for the CURRENT prompt
/// (`None` = not known yet, or a backend that does not report the field). `busy` is
/// [`flux2_pipeline_busy`] — a prompt-cache operation, a generation OR a per-component
/// action already in flight — because all three share the one progress bar and the
/// backend's one pipeline. `name` is the save-name field and `has_selection` says whether
/// the library combo points at an entry.
///
/// Building needs the encoder on disk and something to encode. Saving additionally needs
/// a cache that actually exists — `None` is not a promise that one does — and a name to
/// store it under. Loading and exporting act on a listed entry, so both need a selection;
/// importing needs neither an entry nor an encoder, because the file supplies everything.
///
/// `text_encoder_available` is the backend's own answer for the configured path
/// ([`Flux2Status::text_encoder_available`]). It closes BUILD and SAVE on top of the local
/// path check, because those two are the only library operations that need the encoder:
/// a build encodes, and a save has to name the encoder that produced the entry, so the
/// backend refuses both outright. `None` — a backend that does not report the field —
/// leaves the decision to the path check alone, which is what this gate has always used.
pub(super) fn flux2_prompt_cache_gates(
    settings: &Flux2KleinSettings,
    prompt_cached: Option<bool>,
    text_encoder_available: Option<bool>,
    name: &str,
    has_selection: bool,
    backend_available: bool,
    busy: bool,
) -> Flux2PromptCacheGates {
    let ready = backend_available && !busy;
    // A configured path the backend cannot find is the same situation as no path at all —
    // that is exactly what a settings file copied from another machine looks like.
    let encoder_present =
        !settings.effective_paths().text_encoder.is_empty() && text_encoder_available != Some(false);
    let encodable = !settings.prompt.trim().is_empty() && encoder_present;
    Flux2PromptCacheGates {
        build: ready && encodable,
        save: ready && encodable && prompt_cached == Some(true) && !name.trim().is_empty(),
        load: ready && has_selection,
        export: ready && has_selection,
        import: ready,
    }
}

/// Decides the three-state prompt-cache line from the last `.status` answer.
///
/// `asked_about` is the trimmed prompt that answer was asked about; `current_prompt` is
/// what the field holds now. The answer counts only while the two still agree — otherwise
/// it describes a prompt the user has already typed away from, and "not known" is the only
/// honest report. A free function so this rule can be tested without a live tool.
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

/// Formats the creation time of a library entry for display.
///
/// The backend writes an ISO-8601 UTC string (`created_at`), which is used verbatim; a
/// Unix timestamp in seconds is accepted too and rendered in the machine's local time,
/// because a timestamp is the other shape this field is written in and a wrong "no date"
/// would be indistinguishable from a genuinely missing one. Anything else — and a
/// timestamp outside the representable range — yields an empty string, which the UI shows
/// as "no date" rather than as a wrong one.
pub(super) fn format_prompt_cache_created(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| {
                // Python's `time.time()` is a FLOAT, so an integer-only read would drop
                // every timestamp the backend actually sends. Cast justification: the
                // guard keeps the value finite and inside ±1e15 s, which is orders of
                // magnitude below `i64`'s range, so the truncation can neither wrap nor
                // lose the whole-second part. `f64` has no fallible conversion to `i64`.
                let seconds = number.as_f64()?;
                (seconds.is_finite() && seconds.abs() < 1e15).then(|| seconds.trunc() as i64)
            })
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .map(|utc| {
                utc.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default(),
        Some(_) | None => String::new(),
    }
}

/// Hover text of one library row: the prompt the entry encodes, when it was made, and —
/// while the listing spans several families — which family it belongs to.
///
/// `show_family` is the combo's own decision (no active family), so the row and its
/// tooltip can never disagree about whether the family is on screen.
pub(super) fn prompt_cache_entry_tooltip(entry: &Flux2PromptCacheEntry, show_family: bool) -> String {
    let head = if entry.created.is_empty() {
        entry.prompt.clone()
    } else {
        tf!(
            "cleaning.tools.flux2_klein.prompt_cache_entry_tooltip",
            prompt = entry.prompt,
            created = entry.created
        )
    };
    if !show_family || entry.family.is_empty() {
        return head;
    }
    let family = tf!(
        "cleaning.tools.flux2_klein.prompt_cache_entry_family",
        family = entry.family
    );
    if head.is_empty() {
        return family;
    }
    format!("{head}\n{family}")
}

// ---------------------------------------------------------------------------------------
// Prompt cache
// ---------------------------------------------------------------------------------------

/// Builds the request header of a prompt-cache call: the normalized settings under
/// `params`, and the operation's own fields (`name`, `path`, or neither) BESIDE it at the
/// top level, which is where the backend reads them from.
///
/// The settings travel with EVERY one of the six, not only with the build: they carry the
/// text-encoder path, which is what decides the encoder FAMILY the library is split by. A
/// `.list` without them would describe some other family's entries — and the backend
/// refuses the call outright rather than guessing one.
///
/// `overwrite` is never sent: it defaults to `false` backend-side, so a name already taken
/// comes back as an explicit error the user is shown, instead of silently replacing a
/// cache that cost a 16 GB encoder read to build.
///
/// `settings` must already be `normalized()`.
#[must_use]
pub(super) fn flux2_prompt_cache_header(settings: &Flux2KleinSettings, extra: &[(&str, Value)]) -> Value {
    // The mode is `false` here as on every other query path: a prompt-cache call carries
    // no mask, and only the text-encoder path of `params` is read from it anyway.
    let mut header = json!({ "params": settings.to_params(false) });
    // `json!` above always builds an object; the guard keeps this total rather than
    // relying on that from a distance.
    if let Some(map) = header.as_object_mut() {
        for (key, value) in extra {
            map.insert((*key).to_string(), value.clone());
        }
    }
    header
}

/// Encodes the prompt in `params` and leaves the embeddings in the backend's live cache.
///
/// Streaming, because reading the Qwen3 encoder takes far longer than a call may block for:
/// the progress frames have the same shape as a generation's and drive the same bar.
/// `generation` is the
/// progress generation claimed on the GUI thread; every write is dropped once a newer run
/// — or a cancel — has retired it, and the bar is cleared on EVERY exit.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable.
pub(super) fn build_flux2_prompt_cache(
    header: Value,
    progress: &Arc<Mutex<Flux2Progress>>,
    generation: u64,
) -> Result<Flux2PromptCacheOutcome, String> {
    let outcome = flux2_stream_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
        header,
        &[],
        |id| update_progress(progress, generation, |state| state.cancel_id = Some(id)),
        |frame| publish_progress_frame(progress, generation, frame),
    )
    .map(|(_header, _blob)| Flux2PromptCacheOutcome::Built);
    update_progress(progress, generation, |state| {
        state.active = false;
        state.cancel_id = None;
    });
    outcome
}

/// Lists the library entries of the encoder family `params` identifies.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable.
pub(super) fn list_flux2_prompt_caches(header: Value) -> Result<Flux2PromptCacheList, String> {
    let response = flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
        header,
    )?;
    Ok(parse_flux2_prompt_cache_list(&response))
}

/// Parses a `.prompt_cache.list` answer.
///
/// Every field is optional: an answer that reports less than the full record degrades to
/// an entry with empty extras rather than to an error, and an entry with no NAME is
/// dropped outright — the name is the only field the other four methods can act on.
///
/// The top-level `family` and `text_encoder_available` are read the same way, and an
/// EMPTY `family` is kept as such: on a machine with no encoder it is the backend's way of
/// saying that no family is active and the entries come from all of them.
pub(super) fn parse_flux2_prompt_cache_list(header: &Value) -> Flux2PromptCacheList {
    let entries = header
        .get("entries")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name").and_then(Value::as_str)?.trim();
                    if name.is_empty() {
                        return None;
                    }
                    Some(Flux2PromptCacheEntry {
                        name: name.to_string(),
                        // Each entry names its own family, which is the only thing that
                        // keeps a library-wide listing (no encoder installed) unambiguous.
                        family: item
                            .get("family")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        prompt: item
                            .get("prompt")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        // `created_at` is the backend's spelling (an ISO-8601 UTC
                        // string); `created` is accepted as well so a shorter spelling
                        // does not silently read as "no date".
                        created: format_prompt_cache_created(
                            item.get("created_at").or_else(|| item.get("created")),
                        ),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Flux2PromptCacheList {
        // An EMPTY family is a fact, not a gap: it is how the backend reports that no
        // encoder is installed and the listing therefore spans every family.
        family: header
            .get("family")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        text_encoder_available: header.get("text_encoder_available").and_then(Value::as_bool),
        entries,
    }
}

/// Stores the live cache of `params.prompt` in the library under `params.name`.
///
/// # Errors
/// Returns the backend's own message — which is what reports a name already taken — or a
/// transport message when it is unreachable.
pub(super) fn save_flux2_prompt_cache(header: Value) -> Result<(), String> {
    flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE,
        header,
    )
    .map(|_response| ())
}

/// Loads library entry `params.name` into the live cache.
///
/// # Errors
/// Returns the backend's own message — which is what reports an entry of a different
/// encoder family — or a transport message when it is unreachable.
pub(super) fn load_flux2_prompt_cache(header: Value) -> Result<Flux2PromptCacheLoad, String> {
    let response = flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD,
        header,
    )?;
    Ok(parse_flux2_prompt_cache_load(&response))
}

/// Reads a `.prompt_cache.load` answer: the prompt the entry was built from, trimmed, and
/// whether the encoder's fingerprint was actually compared.
///
/// An empty prompt means the answer carried none, which the caller refuses rather than
/// writing into the field — a blank prompt would block the run gate.
///
/// `encoder_verified` is `None` when the backend did not report it (an older build, which
/// only ever verified). It is NOT read as `false`: the notice it drives says the file's
/// metadata was taken on trust, and inventing that would be a false alarm.
pub(super) fn parse_flux2_prompt_cache_load(header: &Value) -> Flux2PromptCacheLoad {
    Flux2PromptCacheLoad {
        prompt: header
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        encoder_verified: header.get("encoder_verified").and_then(Value::as_bool),
    }
}

/// Writes library entry `params.name` to the file `params.path`.
///
/// # Errors
/// Returns the backend's own message, or a transport message when it is unreachable.
pub(super) fn export_flux2_prompt_cache(header: Value) -> Result<(), String> {
    flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
        header,
    )
    .map(|_response| ())
}

/// Takes the file `params.path` into the library.
///
/// `current_family` is the family the tool currently lists, used only to decide whether
/// the import landed outside it when the backend does not say so itself.
///
/// # Errors
/// Returns the backend's own message, or a transport message when it is unreachable.
pub(super) fn import_flux2_prompt_cache(
    header: Value,
    current_family: &str,
) -> Result<Flux2PromptCacheOutcome, String> {
    let response = flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
        header,
    )?;
    Ok(parse_flux2_prompt_cache_import(&response, current_family))
}

/// Reads a `.prompt_cache.import` answer.
///
/// `family_matches` is decided in this order: the backend's own `family_matches` flag if
/// it reported one, then the `foreign` spelling of the same fact, then a comparison of the
/// reported `family` against `current_family` when both are known. When none of the three
/// applies the answer is `None` — "not known" — and no warning is shown, because a guess
/// here would either hide a lost entry or accuse the backend of losing one it did not.
pub(super) fn parse_flux2_prompt_cache_import(header: &Value, current_family: &str) -> Flux2PromptCacheOutcome {
    let family = header
        .get("family")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let family_matches = header
        .get("family_matches")
        .and_then(Value::as_bool)
        .or_else(|| header.get("foreign").and_then(Value::as_bool).map(|foreign| !foreign))
        .or_else(|| {
            (!family.is_empty() && !current_family.is_empty()).then(|| family == current_family)
        });
    Flux2PromptCacheOutcome::Imported {
        name: header
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        family_matches,
    }
}

/// One-shot prompt-cache call, returning the response header.
///
/// # Errors
/// Returns the backend's own message for a refused request — a name already taken, an
/// entry of another encoder family, or "unknown method", which is what a backend without
/// the prompt-cache handlers answers — or the offline message when it cannot be reached at
/// all. None of them is a panic and none is swallowed.
pub(super) fn flux2_prompt_cache_call(method: &'static str, header: Value) -> Result<Value, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (response, _blob) = client
        .call(method, header, &[], FLUX2_QUERY_TIMEOUT)
        .map_err(map_flux2_call_error)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_library_listing_drops_entries_without_a_name() {
        let header = json!({
            "family": "qwen3-4b",
            "entries": [
                {
                    "name": "sfx",
                    "prompt": "remove the sfx",
                    "created_at": "2024-05-01T09:00:00Z",
                    "size_bytes": 4096
                },
                { "name": "  ", "prompt": "nameless" },
                { "prompt": "no name key at all" },
                { "name": "bare" }
            ]
        });
        let library = parse_flux2_prompt_cache_list(&header);
        assert_eq!(library.family, "qwen3-4b");
        // The name is the only field `.save`/`.load`/`.export` can act on, so an entry
        // without one is not listed at all rather than shown as an unusable row.
        assert_eq!(library.entries.len(), 2);
        assert_eq!(library.entries[0].name, "sfx");
        assert_eq!(library.entries[0].prompt, "remove the sfx");
        assert_eq!(library.entries[0].created, "2024-05-01T09:00:00Z");
        // The shorter `created` spelling is accepted too, so a rename on the wire does
        // not silently turn every entry into "no date".
        let shorter = parse_flux2_prompt_cache_list(&json!({
            "entries": [{ "name": "sfx", "created": 1_700_000_000u64 }]
        }));
        assert!(!shorter.entries[0].created.is_empty());
        // A record reporting nothing but its name still lists, with empty extras.
        assert_eq!(library.entries[1].name, "bare");
        assert!(library.entries[1].prompt.is_empty());
        assert!(library.entries[1].created.is_empty());
        // An empty answer degrades instead of failing.
        assert_eq!(
            parse_flux2_prompt_cache_list(&json!({})),
            Flux2PromptCacheList::default()
        );
    }

    #[test]
    fn a_created_timestamp_is_read_in_every_shape_the_backend_may_send() {
        // A ready-made string is shown verbatim.
        assert_eq!(
            format_prompt_cache_created(Some(&json!("  2024-05-01 12:00  "))),
            "2024-05-01 12:00"
        );
        // Python's `time.time()` is a float, so both integer and fractional Unix
        // timestamps have to render.
        assert!(!format_prompt_cache_created(Some(&json!(1_700_000_000u64))).is_empty());
        assert_eq!(
            format_prompt_cache_created(Some(&json!(1_700_000_000.75f64))),
            format_prompt_cache_created(Some(&json!(1_700_000_000u64))),
            "the fractional second must not change the rendered minute"
        );
        // Anything unusable is "no date", never a wrong one.
        assert!(format_prompt_cache_created(None).is_empty());
        assert!(format_prompt_cache_created(Some(&Value::Null)).is_empty());
        assert!(format_prompt_cache_created(Some(&json!(1e300))).is_empty());
    }

    #[test]
    fn an_import_reports_a_foreign_family_however_the_backend_spells_it() {
        let matched = parse_flux2_prompt_cache_import(
            &json!({ "name": "sfx", "family_matches": true }),
            "qwen3-4b",
        );
        assert_eq!(
            matched,
            Flux2PromptCacheOutcome::Imported {
                name: "sfx".to_string(),
                family_matches: Some(true)
            }
        );
        // The `foreign` spelling of the same fact is the inverse.
        let foreign = parse_flux2_prompt_cache_import(
            &json!({ "name": "sfx", "foreign": true }),
            "qwen3-4b",
        );
        assert_eq!(
            foreign,
            Flux2PromptCacheOutcome::Imported {
                name: "sfx".to_string(),
                family_matches: Some(false)
            }
        );
        // With neither flag, the reported family is compared against the listed one.
        let compared =
            parse_flux2_prompt_cache_import(&json!({ "name": "sfx", "family": "t5" }), "qwen3-4b");
        assert_eq!(
            compared,
            Flux2PromptCacheOutcome::Imported {
                name: "sfx".to_string(),
                family_matches: Some(false)
            }
        );
        // Nothing to compare: "not known", so no warning is invented in either direction.
        for header in [json!({ "name": "sfx" }), json!({ "name": "sfx", "family": "t5" })] {
            let unknown = parse_flux2_prompt_cache_import(&header, "");
            assert_eq!(
                unknown,
                Flux2PromptCacheOutcome::Imported {
                    name: "sfx".to_string(),
                    family_matches: None
                }
            );
        }
    }

    #[test]
    fn a_loaded_entry_answers_with_its_trimmed_prompt() {
        assert_eq!(
            parse_flux2_prompt_cache_load(&json!({ "prompt": "  remove the sfx  " })).prompt,
            "remove the sfx"
        );
        // An answer carrying no prompt yields an empty string, which the caller shows as
        // an error instead of clearing the user's field.
        assert!(parse_flux2_prompt_cache_load(&json!({})).prompt.is_empty());
        assert!(
            parse_flux2_prompt_cache_load(&json!({ "prompt": "   " }))
                .prompt
                .is_empty()
        );
        assert!(
            parse_flux2_prompt_cache_load(&json!({ "prompt": 7 }))
                .prompt
                .is_empty()
        );
    }

    #[test]
    fn a_load_says_whether_the_encoder_fingerprint_was_actually_compared() {
        let verified =
            parse_flux2_prompt_cache_load(&json!({ "prompt": "sfx", "encoder_verified": true }));
        assert_eq!(verified.encoder_verified, Some(true));
        let trusted =
            parse_flux2_prompt_cache_load(&json!({ "prompt": "sfx", "encoder_verified": false }));
        assert_eq!(trusted.encoder_verified, Some(false));
        // A backend that does not report the field only ever verified, so its silence must
        // read as "not known" and raise no notice — never as "taken on trust".
        let silent = parse_flux2_prompt_cache_load(&json!({ "prompt": "sfx" }));
        assert_eq!(silent.encoder_verified, None);
        // Neither does a value of the wrong shape.
        let wrong =
            parse_flux2_prompt_cache_load(&json!({ "prompt": "sfx", "encoder_verified": "no" }));
        assert_eq!(wrong.encoder_verified, None);
    }

    #[test]
    fn a_listing_without_an_active_family_names_the_family_of_every_entry() {
        // What an encoder-less machine gets: no active family, entries from all of them.
        let library = parse_flux2_prompt_cache_list(&json!({
            "family": "",
            "directory": "/root/prompt_cache",
            "text_encoder_available": false,
            "entries": [
                { "name": "sfx", "family": "qwen3-4b-aabbccdd", "prompt": "remove the sfx" },
                { "name": "bg", "family": "qwen3-8b-11223344" }
            ]
        }));
        assert!(
            library.family.is_empty(),
            "an empty family is the backend saying none is active"
        );
        assert_eq!(library.text_encoder_available, Some(false));
        assert_eq!(library.entries[0].family, "qwen3-4b-aabbccdd");

        // The row shows which family it came from, so the user can see that the list is
        // not only "their" caches. The rendered text depends on the active catalog (a unit
        // test runs with none, where `t!`/`tf!` answer the key itself), so what is asserted
        // here is the BRANCH: with a family to show, the caption is no longer the bare name,
        // and the tooltip is no longer the one that omits it. The placeholders of the
        // templates themselves are guarded by the catalog test below.
        let entry = &library.entries[0];
        assert_ne!(entry.label(true), entry.name);
        assert_ne!(
            prompt_cache_entry_tooltip(entry, true),
            prompt_cache_entry_tooltip(entry, false)
        );
        // With one active family the name stands alone: the family is the same for every
        // row and would be pure noise.
        assert_eq!(entry.label(false), "sfx");
        // An entry that reports no family of its own is shown by name, never by a made-up
        // prefix — and its tooltip cannot change either.
        let anonymous = parse_flux2_prompt_cache_list(&json!({ "entries": [{ "name": "sfx" }] }));
        assert!(anonymous.entries[0].family.is_empty());
        assert_eq!(anonymous.entries[0].label(true), "sfx");
        assert_eq!(
            prompt_cache_entry_tooltip(&anonymous.entries[0], true),
            prompt_cache_entry_tooltip(&anonymous.entries[0], false)
        );
        // And a backend that reports neither flag says nothing about the encoder.
        assert_eq!(anonymous.text_encoder_available, None);
    }

    #[test]
    fn prompt_cache_gates_name_every_blocking_condition() {
        let ready = cacheable_settings();
        let all_open =
            flux2_prompt_cache_gates(&ready, Some(true), Some(true), "entry", true, true, false);
        assert_eq!(
            all_open,
            Flux2PromptCacheGates {
                build: true,
                save: true,
                load: true,
                export: true,
                import: true
            }
        );

        // An unreachable backend closes every one of them: none can be served locally.
        let offline =
            flux2_prompt_cache_gates(&ready, Some(true), Some(true), "entry", true, false, false);
        assert_eq!(
            offline,
            Flux2PromptCacheGates {
                build: false,
                save: false,
                load: false,
                export: false,
                import: false
            }
        );
        // So does an operation already in flight — one pipeline, one progress bar.
        let busy =
            flux2_prompt_cache_gates(&ready, Some(true), Some(true), "entry", true, true, true);
        assert_eq!(busy, offline);

        // An empty prompt or a missing encoder blocks building and saving, and nothing
        // else: loading, exporting and importing do not encode anything.
        for broken in [
            Flux2KleinSettings {
                prompt: "   ".to_string(),
                ..cacheable_settings()
            },
            Flux2KleinSettings {
                text_encoder_path: String::new(),
                ..cacheable_settings()
            },
        ] {
            let gates =
                flux2_prompt_cache_gates(&broken, Some(true), Some(true), "entry", true, true, false);
            assert!(!gates.build);
            assert!(!gates.save);
            assert!(gates.load && gates.export && gates.import);
        }

        // Saving needs a cache that EXISTS. `None` is "not known yet" and is not a
        // promise that one does, so it blocks saving exactly as `Some(false)` does.
        for state in [None, Some(false)] {
            let gates =
                flux2_prompt_cache_gates(&ready, state, Some(true), "entry", true, true, false);
            assert!(!gates.save, "{state:?} must not offer a save");
            assert!(gates.build, "{state:?} still allows building one");
        }
        // …and a name to store it under.
        for name in ["", "   "] {
            let gates =
                flux2_prompt_cache_gates(&ready, Some(true), Some(true), name, true, true, false);
            assert!(!gates.save, "an empty name ({name:?}) is not accepted");
        }

        // Loading and exporting act on a listed entry; importing does not.
        let no_selection =
            flux2_prompt_cache_gates(&ready, Some(true), Some(true), "entry", false, true, false);
        assert!(!no_selection.load);
        assert!(!no_selection.export);
        assert!(no_selection.import);
    }

    #[test]
    fn a_missing_local_encoder_closes_only_the_two_operations_that_encode() {
        // The settings still NAME an encoder — this is exactly a settings file carried
        // over from another machine, where the path is filled in and points nowhere.
        let stale_path = cacheable_settings();
        let gates =
            flux2_prompt_cache_gates(&stale_path, Some(true), Some(false), "entry", true, true, false);
        assert!(
            !gates.build,
            "there is nothing on this machine to encode the prompt with"
        );
        assert!(
            !gates.save,
            "a saved entry has to name the encoder that produced it"
        );
        // Everything that only moves ready files around keeps working — that is what makes
        // an encoder-less machine usable at all.
        assert!(gates.load, "a ready cache can still be loaded");
        assert!(gates.export, "copying a file out needs no encoder");
        assert!(gates.import, "the imported file supplies everything");

        // `None` is "not known" and must not close anything by itself: the local path
        // check stays the only rule, exactly as before the field existed.
        let unknown =
            flux2_prompt_cache_gates(&stale_path, Some(true), None, "entry", true, true, false);
        assert!(unknown.build && unknown.save);
    }

    #[test]
    fn prompt_cache_headers_carry_the_settings_and_the_operation_fields() {
        let settings = cacheable_settings().normalized();
        let plain = flux2_prompt_cache_header(&settings, &[]);
        // The settings travel with EVERY prompt-cache call: the encoder path is what
        // decides the family the library is split by, and the backend refuses the call
        // outright without one.
        assert_eq!(plain["params"]["text_encoder_path"], json!("/models/qwen3"));
        assert_eq!(plain["params"]["prompt"], json!("remove the sfx"));
        assert!(plain.get("name").is_none());

        // `name` and `path` sit BESIDE `params`, not inside it: that is where the backend
        // reads them from (`_require_non_empty_str(header, ...)`).
        let named = flux2_prompt_cache_header(&settings, &[("name", json!("sfx"))]);
        assert_eq!(named["name"], json!("sfx"));
        assert!(named["params"].get("name").is_none());
        assert_eq!(named["params"]["text_encoder_path"], json!("/models/qwen3"));

        let exported = flux2_prompt_cache_header(
            &settings,
            &[("name", json!("sfx")), ("path", json!("/tmp/a.msprompt"))],
        );
        assert_eq!(exported["name"], json!("sfx"));
        assert_eq!(exported["path"], json!("/tmp/a.msprompt"));
        // `overwrite` is never sent: a name already taken must come back as an explicit
        // error, never silently replace a cache that cost a 16 GB encoder read.
        assert!(exported.get("overwrite").is_none());
    }
}
