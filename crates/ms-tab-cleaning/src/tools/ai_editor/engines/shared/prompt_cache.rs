/*
File: cleaning/tools/ai_editor/engines/shared/prompt_cache.rs

Purpose:
The model-agnostic CONTROLLER of a large-model engine's prompt-cache library (protocol 23):
what «Кэшировать» offers for the typed text, which library operations are open, the one-line
cache verdict under the prompt, the `.prompt_cache.*` wire block and the jobs that run it. An
engine supplies only what is its own — its method names and timeouts (`PromptCacheMethods`),
its request `params`, the provenance and precision ranking of its current encoder
(`CacheContext`) and the localized caption of an entry's encoder.

Main responsibilities:
- decide «Кэшировать» / «Переименовать» / closed-with-a-reason (`cache_button`) and the
  export / import gates (`prompt_cache_gates`);
- answer whether the last `.status` still describes the prompt currently typed
  (`prompt_cache_state_for`) and pick the one cache line (`prompt_cache_line`);
- read the cache half of a `.status` answer (`parse_prompt_cache_status`), build the request
  header (`prompt_cache_header`) and parse every answer;
- own the state of the library between frames (`PromptCacheController`): the one operation in
  flight (build / rename, export, import), the listing query with its one-shot arming, the
  preset-name field, the captured export file, the fold flag and the outcome lines; it reports
  what the ENGINE must re-ask (`PromptCacheEffects`);
- the two file-dialog requests (`prompt_cache_export_dialog`, `prompt_cache_import_dialog`).

Key structures:
- `PromptCacheMethods`, `CacheContext`, `CacheBlock`, `CacheButton`, `PromptCacheGates`
- `PromptCacheLine`, `PromptCacheOutcome`, `PromptCacheAction`, `PromptCacheEffects`
- `PromptCacheController`

Key functions:
- `cache_button()`, `prompt_cache_gates()`, `prompt_cache_state_for()`, `prompt_cache_line()`
- `parse_prompt_cache_status()`, `prompt_cache_header()`, `parse_prompt_cache_build()`,
  `parse_prompt_cache_import()`
- `PromptCacheController::{start_build, start_export, start_import, poll, poll_list}`

Notes:
GUI-free; the drawing half is `prompt_cache_ui.rs`. Which entry a run uses is the BACKEND's
selection; this file never re-derives it. Every call runs on an `ms_thread` worker and the
controller only drains channels. Texts are `cleaning.tools.prompt_library.*` keys; nothing here
names a model (log lines take `PromptCacheMethods::log_label`).
*/

use super::prompt_library::{PromptLibraryEntry, PromptLibraryListing, PromptLibraryText, normalize_preset_name, parse_entry_info, parse_prompt_library_listing};
use super::stream_progress::{EngineProgress, ProgressClaim, begin_progress_generation, map_call_error, publish_progress_frame, stream_call, update_progress};
use ms_backend_ipc as backend_ipc;
use ms_local_ai::backend_health::ai_backend_offline_error;
use ms_platform::file_dialog::{DialogKind, FileDialogRequest, SaveTarget};
use ms_thread as thread;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use web_time::Duration;

/// Extension of a prompt-cache file, without the dot.
///
/// A file-format identifier, not prose: it is what the file is NAMED on disk and what the
/// open dialog filters on, so it is the same in every language. Only the human caption of the
/// filter is localized.
pub(in crate::tools::ai_editor::engines) const PROMPT_CACHE_EXTENSION: &str = "msprompt";

/// The engine-specific facts of its `.prompt_cache.*` method family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct PromptCacheMethods {
    /// `.prompt_cache.build` (streaming; also a rename).
    pub(in crate::tools::ai_editor::engines) build: &'static str,
    /// `.prompt_cache.list` (one-shot).
    pub(in crate::tools::ai_editor::engines) list: &'static str,
    /// `.prompt_cache.export` (one-shot).
    pub(in crate::tools::ai_editor::engines) export: &'static str,
    /// `.prompt_cache.import` (one-shot).
    pub(in crate::tools::ai_editor::engines) import: &'static str,
    /// Longest silence between two progress frames of a build (it reads the text encoder).
    pub(in crate::tools::ai_editor::engines) build_timeout: Duration,
    /// Timeout of the three one-shot calls.
    pub(in crate::tools::ai_editor::engines) query_timeout: Duration,
    /// The engine's name in log lines (`"FLUX.2 klein"`).
    pub(in crate::tools::ai_editor::engines) log_label: &'static str,
}

/// What the engine knows about the typed text and its current encoder, for
/// [`cache_button`].
// No `PartialEq`: `rank` is a function pointer, and comparing those is not meaningful.
#[derive(Debug, Clone)]
pub(in crate::tools::ai_editor::engines) struct CacheContext<'a> {
    /// The prompt field as typed.
    pub(in crate::tools::ai_editor::engines) prompt: &'a str,
    /// An encoder path (or selection) is configured at all.
    pub(in crate::tools::ai_editor::engines) encoder_configured: bool,
    /// The (source id, precision id) of the encoder a build would use, when the engine knows
    /// it (download mode); `None` closes no "more precise" check.
    pub(in crate::tools::ai_editor::engines) current: Option<(String, String)>,
    /// The engine's precision ranking: lower is MORE precise.
    pub(in crate::tools::ai_editor::engines) rank: fn(&str) -> usize,
    /// The precision id of a weight-only fp8 encode (`PromptLibraryEntry::precision_id`).
    pub(in crate::tools::ai_editor::engines) fp8_id: &'a str,
    /// An engine-specific reason caching is closed right now (already localized), checked
    /// right after readiness.
    pub(in crate::tools::ai_editor::engines) engine_block: Option<String>,
}

/// Why «Кэшировать» is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum CacheBlock {
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
    /// The engine closes caching for a reason of its own (the localized text).
    Engine(String),
}

/// What the «Кэшировать» button offers for the typed text right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum CacheButton {
    /// «Кэшировать»: makes (or renames, when the name differs) the preset with the current
    /// encoder.
    Cache,
    /// «Переименовать»: the current encoder already serves the text, the name field differs
    /// from the preset's name.
    Rename,
    Closed(CacheBlock),
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
/// (`ctx.current`) — an entry of the same source with a strictly more precise format exists.
/// Neither closes it while `.status` says the prompt is NOT served (`Some(false)`): the run
/// then still needs something this build would make (the empty negative prompt under
/// guidance, an entry the backend skipped).
#[must_use]
pub(in crate::tools::ai_editor::engines) fn cache_button(
    ctx: &CacheContext<'_>,
    hit: Option<&PromptLibraryText>,
    name: &str,
    prompt_cached: Option<bool>,
    text_encoder_available: Option<bool>,
    ready: bool,
) -> CacheButton {
    if !ready {
        return CacheButton::Closed(CacheBlock::NotReady);
    }
    if let Some(reason) = ctx.engine_block.as_ref() {
        return CacheButton::Closed(CacheBlock::Engine(reason.clone()));
    }
    if text_encoder_available == Some(false) {
        return CacheButton::Closed(CacheBlock::NoEncoder);
    }
    if ctx.prompt.trim().is_empty() || !ctx.encoder_configured {
        return CacheButton::Closed(CacheBlock::NothingToEncode);
    }
    let Some(hit) = hit else {
        return CacheButton::Cache;
    };
    if prompt_cached == Some(false) {
        return CacheButton::Cache;
    }
    if hit.entries.iter().any(|entry| entry.current_encoder == Some(true)) {
        // Compared as the backend STORES names: a name it would clean or cap is no rename.
        let wanted = normalize_preset_name(name);
        return if !wanted.is_empty() && wanted != normalize_preset_name(&hit.display_name()) {
            CacheButton::Rename
        } else {
            CacheButton::Closed(CacheBlock::AlreadyCached)
        };
    }
    if let Some((source, precision)) = ctx.current.as_ref() {
        let rank = ctx.rank;
        let current_rank = rank(precision);
        let better = hit
            .entries
            .iter()
            .filter(|entry| entry.encoder_source == *source)
            .filter(|entry| rank(entry.precision_id(ctx.fp8_id)) < current_rank)
            .min_by_key(|entry| rank(entry.precision_id(ctx.fp8_id)));
        if let Some(better) = better {
            return CacheButton::Closed(CacheBlock::MorePrecise(better.clone()));
        }
    }
    CacheButton::Cache
}

/// Which prompt-cache controls are live: «Кэшировать» ([`cache_button`]), «Экспорт» (the
/// typed text is a preset: its selected entry's file is what is written) and «Импорт» (needs
/// only the backend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct PromptCacheGates {
    pub(in crate::tools::ai_editor::engines) build: CacheButton,
    pub(in crate::tools::ai_editor::engines) export: bool,
    pub(in crate::tools::ai_editor::engines) import: bool,
}

/// Decides every prompt-cache control at once; see [`cache_button`] for the build half.
/// `busy` is the engine's whole-pipeline busy flag: a cache operation, a generation OR any
/// other operation in flight, because all of them share the one progress bar and the
/// backend's one pipeline.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn prompt_cache_gates(
    ctx: &CacheContext<'_>,
    hit: Option<&PromptLibraryText>,
    name: &str,
    prompt_cached: Option<bool>,
    text_encoder_available: Option<bool>,
    backend_available: bool,
    busy: bool,
) -> PromptCacheGates {
    let ready = backend_available && !busy;
    PromptCacheGates {
        build: cache_button(ctx, hit, name, prompt_cached, text_encoder_available, ready),
        export: ready && hit.is_some(),
        import: ready,
    }
}

/// Decides the three-state `prompt_cached` from the last `.status` answer.
///
/// `asked_about` is the trimmed prompt that answer was asked about, `prompt_cached` the
/// answer itself; `current_prompt` is what the field holds now. The answer counts only while
/// the two prompts still agree — otherwise it describes a prompt the user has already typed
/// away from, and "not known" is the only honest report.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn prompt_cache_state_for(asked_about: Option<&str>, prompt_cached: Option<bool>, current_prompt: &str) -> Option<bool> {
    if asked_about? != current_prompt.trim() {
        return None;
    }
    prompt_cached
}

/// The at-most-one compact line drawn under the prompt field.
///
/// A separate decision from the drawing because it is a PRIORITY rule, not a formatting
/// one: three facts compete for one line and only the most consequential may take it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum PromptCacheLine {
    /// Nothing is claimed. The answer is outstanding, which is not information.
    Silent,
    /// The prompt is served from the cache: the next run skips the encoder read entirely.
    /// `encoder` names the encoder that made the entry in use (`None` when no entry was
    /// reported); `memory` says the embeddings are only in the backend's memory, not saved.
    Cached { encoder: Option<String>, memory: bool },
    /// It is not: the next run pays for the encoder.
    NotCached,
    /// This machine has NO text encoder, so nothing can be encoded here and only ready
    /// caches work. It outranks "not cached", which would read as a delay the user could
    /// wait out rather than as a run that cannot start.
    NoEncoder,
}

/// Decides which single line the prompt block shows about the cache.
///
/// `prompt_cached` / `text_encoder_available` are the project's three-state answers (`None` =
/// not known, never `false`); `status_entry` is the entry the last `.status` named for the
/// prompt, `listing_hit` the selected entry of the typed text's preset in the library listing,
/// `caption` the engine's localized name of an entry's encoder (an empty caption names none).
/// The priority is what the user can DO next:
/// - a served prompt wins outright, because the run then works whether an encoder exists or
///   not; the fresh `.status` entry names its encoder first (it also covers a memory-only
///   encode), the listing's selection second;
/// - while `.status` has not answered about this text, a preset of it in the listing is
///   already a fact the line reports;
/// - otherwise a missing encoder wins, because it changes what is POSSIBLE rather than what
///   something costs;
/// - "not cached" is left, and an outstanding answer says nothing at all.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn prompt_cache_line(
    listing_hit: Option<&PromptLibraryEntry>,
    status_entry: Option<&PromptLibraryEntry>,
    prompt_cached: Option<bool>,
    text_encoder_available: Option<bool>,
    caption: &dyn Fn(&PromptLibraryEntry) -> String,
) -> PromptCacheLine {
    let cached = |entry: Option<&PromptLibraryEntry>| PromptCacheLine::Cached {
        encoder: entry.map(caption).filter(|caption| !caption.is_empty()),
        memory: entry.is_some_and(PromptLibraryEntry::is_memory_only),
    };
    let uncached = if text_encoder_available == Some(false) { PromptCacheLine::NoEncoder } else { PromptCacheLine::NotCached };
    match prompt_cached {
        Some(true) => cached(status_entry.or(listing_hit)),
        Some(false) => uncached,
        None if listing_hit.is_some() => cached(listing_hit),
        None if text_encoder_available == Some(false) => PromptCacheLine::NoEncoder,
        None => PromptCacheLine::Silent,
    }
}

// ---------------------------------------------------------------------------------------
// Wire
// ---------------------------------------------------------------------------------------

/// Reads the cache half of a `.status` answer: `prompt_cached` (three-state, deliberately NOT
/// defaulted to `false`: a backend that does not know the prompt cache must read as "unknown",
/// never as "your prompt is not cached") and the entry a run would use (`prompt_cache_entry`;
/// `null`, an absent field and a non-object are all "no entry").
#[must_use]
pub(in crate::tools::ai_editor::engines) fn parse_prompt_cache_status(header: &Value) -> (Option<bool>, Option<PromptLibraryEntry>) {
    (header.get("prompt_cached").and_then(Value::as_bool), header.get("prompt_cache_entry").and_then(parse_entry_info))
}

/// Builds the request header of a prompt-cache call: the engine's normalized request
/// `params` under `params` (they carry the encoder, the library id and the precision order),
/// and the operation's own fields (`name`, `file`, `path`) BESIDE it at the top level, which is
/// where the backend reads them from.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn prompt_cache_header(params: Value, extra: &[(&str, Value)]) -> Value {
    let mut header = json!({ "params": params });
    // `json!` above always builds an object; the guard keeps this total rather than relying
    // on that from a distance.
    if let Some(map) = header.as_object_mut() {
        for (key, value) in extra {
            map.insert((*key).to_string(), value.clone());
        }
    }
    header
}

/// What a finished prompt-cache worker did. One enum for the three operations because at most
/// one of them is ever in flight, so they share a single channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum PromptCacheOutcome {
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
pub(in crate::tools::ai_editor::engines) enum PromptCacheAction {
    /// «Кэшировать»: encode with the current encoder and save under the name field.
    Build,
    /// «Переименовать»: the current encoder already serves the text; only the name changes.
    Rename,
    Export,
    Import,
}

/// Reads a `.prompt_cache.build` answer: the entries it wrote (an absent list = none) and
/// whether the preset was renamed — the request was a rename, or the answer says one happened
/// (a «Кэшировать» under a new name renames the preset too).
#[must_use]
pub(in crate::tools::ai_editor::engines) fn parse_prompt_cache_build(header: &Value, rename_requested: bool) -> PromptCacheOutcome {
    let written = header
        .get("written")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_entry_info).collect())
        .unwrap_or_default();
    let renamed = rename_requested || header.get("renamed").and_then(Value::as_bool).unwrap_or(false);
    PromptCacheOutcome::Built { written, renamed }
}

/// Reads a `.prompt_cache.import` answer. `library_matches` stays `None` when the answer does
/// not carry it (no warning is invented); an absent `already_present` is `false`.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn parse_prompt_cache_import(header: &Value) -> PromptCacheOutcome {
    let text = |key: &str| header.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let name = Some(text("name")).filter(|name| !name.is_empty()).unwrap_or_else(|| text("file"));
    PromptCacheOutcome::Imported {
        name,
        library_matches: header.get("library_matches").and_then(Value::as_bool),
        already_present: header.get("already_present").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// Makes the library entries of the CURRENT encoder for every text the run needs, under the
/// preset name the header carries (`renamed`: the request is a rename, for the outcome line).
///
/// Streaming, because reading a text encoder takes far longer than a call may block for: the
/// progress frames have the same shape as a generation's and drive the same bar. `claim` is the
/// progress claim taken on the GUI thread; every write is dropped once a newer operation — or a
/// cancel — has retired it, and the bar is cleared on EVERY exit.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable.
fn build_prompt_cache(methods: &PromptCacheMethods, header: Value, renamed: bool, progress: &Arc<Mutex<EngineProgress>>, claim: &ProgressClaim) -> Result<PromptCacheOutcome, String> {
    let outcome = stream_call(methods.build, header, &[], methods.build_timeout, |id| claim.publish_request_id(id), |frame| publish_progress_frame(progress, claim.generation, frame))
        .map(|(answer, _blob)| parse_prompt_cache_build(&answer, renamed));
    update_progress(progress, claim.generation, |state| state.active = false);
    outcome
}

/// One-shot prompt-cache call, returning the response header.
///
/// # Errors
/// Returns the backend's own message for a refused request (which is what refuses a file made
/// before an encoder fix), "unknown method" from a backend without the handler, or the offline
/// message when it cannot be reached at all. None of them is a panic and none is swallowed.
fn prompt_cache_call(method: &'static str, timeout: Duration, header: Value) -> Result<Value, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (response, _blob) = client.call(method, header, &[], timeout).map_err(map_call_error)?;
    Ok(response)
}

// ---------------------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------------------

/// What the ENGINE must re-ask after the controller applied an answer: the controller owns
/// the library, the engine owns `.status` and the memory forecast.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[must_use]
pub(in crate::tools::ai_editor::engines) struct PromptCacheEffects {
    /// Re-ask `.status` (whether the prompt is served).
    pub(in crate::tools::ai_editor::engines) status_wanted: bool,
    /// Invalidate and re-ask the memory forecast (whether the encoder counts).
    pub(in crate::tools::ai_editor::engines) forecast_invalidated: bool,
}

impl PromptCacheEffects {
    /// Both: the library changed in a way that may change what a run encodes.
    const BOTH: Self = Self { status_wanted: true, forecast_invalidated: true };
}

/// The library state of one engine between frames.
///
/// Fields are visible to the engines so a test can inject a receiver; product code goes
/// through the methods. At most ONE operation (build / rename / export / import) is ever in
/// flight (`rx`), and at most one listing query (`list_rx`).
pub(in crate::tools::ai_editor::engines) struct PromptCacheController {
    /// The one prompt-cache operation that may be in flight.
    pub(in crate::tools::ai_editor::engines) rx: Option<Receiver<Result<PromptCacheOutcome, String>>>,
    /// User-facing outcome of the last operation, shown in the library fold.
    pub(in crate::tools::ai_editor::engines) status: Option<String>,
    /// A warning about the LAST operation ("the imported entry belongs to another model's
    /// library"), shown beside its status line. Cleared when the next operation starts, so it
    /// is a remark about a result and never a standing banner.
    pub(in crate::tools::ai_editor::engines) warning: Option<String>,
    /// The model's library listing; `None` until `.list` answers.
    pub(in crate::tools::ai_editor::engines) library: Option<PromptLibraryListing>,
    pub(in crate::tools::ai_editor::engines) list_rx: Option<Receiver<Result<PromptLibraryListing, String>>>,
    pub(in crate::tools::ai_editor::engines) list_error: Option<String>,
    /// One-shot arming of `.list` (a failing query must not spawn a thread per frame). Starts
    /// armed; re-armed after a build or an import, and by the engine whenever its encoder
    /// changes (every entry's `current_encoder` follows it).
    pub(in crate::tools::ai_editor::engines) list_wanted: bool,
    /// The preset NAME field beside «Кэшировать»: filled with the typed text's preset name
    /// (`sync_preset_name`), editable, sent as the build's `name` (empty: the backend's
    /// default derived from the text). Session state.
    pub(in crate::tools::ai_editor::engines) name_input: String,
    /// The preset text the name field was last filled for (`sync_preset_name`).
    pub(in crate::tools::ai_editor::engines) name_synced_for: Option<String>,
    /// The library FILE an export dialog was opened for, captured when it started so a prompt
    /// changed while it is open cannot export another entry.
    pub(in crate::tools::ai_editor::engines) export_file: Option<String>,
    /// Whether the library is unfolded under the prompt block.
    ///
    /// SESSION state and deliberately not a persisted setting: the library is a workbench the
    /// user opens to save, load or carry a `.msprompt` and closes again, so remembering it
    /// across launches would re-expand an expert surface on every start. What the library
    /// EXPLAINS — whether the current prompt is cached — is drawn outside it either way.
    pub(in crate::tools::ai_editor::engines) library_open: bool,
}

impl Default for PromptCacheController {
    /// Nothing in flight, nothing known, the listing armed (the first `.list` of a process is
    /// also what migrates old caches).
    fn default() -> Self {
        Self {
            rx: None,
            status: None,
            warning: None,
            library: None,
            list_rx: None,
            list_error: None,
            list_wanted: true,
            name_input: String::new(),
            name_synced_for: None,
            export_file: None,
            library_open: false,
        }
    }
}

impl PromptCacheController {
    /// An operation (build / rename / export / import) is in flight.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// A `.list` query is in flight.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn list_busy(&self) -> bool {
        self.list_rx.is_some()
    }

    /// Re-arms the listing query (it is asked once the gate opens).
    pub(in crate::tools::ai_editor::engines) fn arm_list(&mut self) {
        self.list_wanted = true;
    }

    /// The preset of the typed text in the held listing, if any.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn hit(&self, prompt: &str) -> Option<&PromptLibraryText> {
        self.library.as_ref().and_then(|library| library.text_for(prompt))
    }

    /// Captures the library file of the typed text's preset for an export and answers whether
    /// there is one (the engine then opens the save dialog). Captured HERE, when the dialog
    /// opens, so a prompt changed while it is open cannot redirect the export.
    pub(in crate::tools::ai_editor::engines) fn arm_export(&mut self, prompt: &str) -> bool {
        self.export_file = self.hit(prompt).map(|text| text.selected.file.clone());
        self.export_file.is_some()
    }

    /// Starts the build (`renamed` = «Переименовать») on a worker thread.
    ///
    /// The preset name is the name field; an empty field sends no `name`, and the backend then
    /// keeps the preset's name or derives one from the text (its rule, one owner).
    /// `header_for` builds the request header from the operation's extra fields. Streaming, and
    /// it claims the engine's progress bar exactly as a generation does; claiming the generation
    /// here, on the GUI thread, is what makes a cancel able to retire and stop it. Does nothing
    /// while another operation is in flight.
    pub(in crate::tools::ai_editor::engines) fn start_build(
        &mut self,
        header_for: impl FnOnce(&[(&str, Value)]) -> Value,
        renamed: bool,
        methods: &PromptCacheMethods,
        progress: &Arc<Mutex<EngineProgress>>,
    ) {
        if self.busy() {
            return;
        }
        let name = normalize_preset_name(&self.name_input);
        let header = if name.is_empty() { header_for(&[]) } else { header_for(&[("name", json!(name))]) };
        let claim = begin_progress_generation(progress);
        let progress = Arc::clone(progress);
        let methods = *methods;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.status = Some(
            if renamed { t!("cleaning.tools.prompt_library.prompt_cache_renaming_status") } else { t!("cleaning.tools.prompt_library.prompt_cache_building_status") }
                .to_string(),
        );
        // A warning belongs to the operation that produced it, not to the next one.
        self.warning = None;
        thread::spawn(move || {
            // A closed channel means the engine dropped the operation (teardown); the answer
            // has no reader left, so the failed send is deliberately ignored.
            let _ = tx.send(build_prompt_cache(&methods, header, renamed, &progress, &claim));
        });
    }

    /// Writes the library file the export dialog was opened for ([`Self::arm_export`]) to the
    /// dialog's `target`; the backend writes the whole file to the target's write path, and
    /// only then is the target published (a no-op on desktop, a blocking stream on Android).
    pub(in crate::tools::ai_editor::engines) fn start_export(&mut self, header_for: impl FnOnce(&[(&str, Value)]) -> Value, target: SaveTarget, methods: &PromptCacheMethods) {
        if self.busy() {
            return;
        }
        let Some(file) = self.export_file.take() else {
            return;
        };
        let path = target.write_path().to_path_buf();
        let header = header_for(&[("file", json!(file)), ("path", json!(path.to_string_lossy()))]);
        let methods = *methods;
        self.start_job(t!("cleaning.tools.prompt_library.prompt_cache_exporting_status"), move || {
            prompt_cache_call(methods.export, methods.query_timeout, header)?;
            target.commit().map_err(|err| err.user_message())?;
            Ok(PromptCacheOutcome::Exported(path))
        });
    }

    /// Takes the file at `path` into the library it belongs to (the backend decides which,
    /// from the file itself).
    pub(in crate::tools::ai_editor::engines) fn start_import(&mut self, header_for: impl FnOnce(&[(&str, Value)]) -> Value, path: PathBuf, methods: &PromptCacheMethods) {
        let header = header_for(&[("path", json!(path.to_string_lossy()))]);
        let methods = *methods;
        self.start_job(t!("cleaning.tools.prompt_library.prompt_cache_importing_status"), move || {
            prompt_cache_call(methods.import, methods.query_timeout, header).map(|response| parse_prompt_cache_import(&response))
        });
    }

    /// Runs one non-streaming `job` on a worker thread, showing `running` meanwhile. Refuses
    /// a second operation while one is in flight — the backend holds one pipeline and one
    /// library, and the buttons are gated on the same condition.
    fn start_job<F>(&mut self, running: &str, job: F)
    where
        F: FnOnce() -> Result<PromptCacheOutcome, String> + Send + 'static,
    {
        if self.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.status = Some(running.to_string());
        // A warning belongs to the operation that produced it, not to the next one.
        self.warning = None;
        thread::spawn(move || {
            // A closed channel means the engine dropped the operation (teardown); the answer
            // has no reader left, so the failed send is deliberately ignored.
            let _ = tx.send(job());
        });
    }

    /// Drains the operation channel into the outcome line of the library fold and returns
    /// what the engine must re-ask. `log_label` names the engine in the failure log line.
    pub(in crate::tools::ai_editor::engines) fn poll(&mut self, log_label: &str) -> PromptCacheEffects {
        let Some(rx) = self.rx.as_ref() else {
            return PromptCacheEffects::default();
        };
        match rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.rx = None;
                let (line, effects) = self.apply_outcome(outcome);
                self.status = Some(line);
                effects
            }
            Ok(Err(err)) => {
                self.rx = None;
                ms_log::runtime_log::log_warn(format!("[cleaning] {log_label} prompt-cache operation failed: {err}"));
                self.status = Some(tf!("cleaning.tools.prompt_library.prompt_cache_error", err = err));
                PromptCacheEffects::default()
            }
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                ms_log::runtime_log::log_error(format!("[cleaning] {log_label} prompt-cache worker ended without an answer"));
                self.status = Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                PromptCacheEffects::default()
            }
            Err(TryRecvError::Empty) => PromptCacheEffects::default(),
        }
    }

    /// Applies one finished operation and returns the line to show for it with what the
    /// engine must re-ask.
    ///
    /// A build (or rename) and an import change the library, so both re-arm `.list` — whose
    /// arrival re-asks `.status` and the forecast. A build also re-asks them at once: the run
    /// may no longer need the encoder. An import that landed in ANOTHER model's library raises
    /// the one-off warning that it will not be listed here.
    pub(in crate::tools::ai_editor::engines) fn apply_outcome(&mut self, outcome: PromptCacheOutcome) -> (String, PromptCacheEffects) {
        match outcome {
            PromptCacheOutcome::Built { written, renamed } => {
                self.list_wanted = true;
                let line = if !written.is_empty() {
                    t!("cleaning.tools.prompt_library.prompt_library_saved_status").to_string()
                } else if renamed {
                    t!("cleaning.tools.prompt_library.prompt_cache_renamed_status").to_string()
                } else {
                    t!("cleaning.tools.prompt_library.prompt_cache_already_built_status").to_string()
                };
                (line, PromptCacheEffects::BOTH)
            }
            PromptCacheOutcome::Exported(path) => {
                (tf!("cleaning.tools.prompt_library.prompt_cache_exported_status", path = path.display()), PromptCacheEffects::default())
            }
            PromptCacheOutcome::Imported { name, library_matches, already_present } => {
                self.list_wanted = true;
                if library_matches == Some(false) {
                    self.warning = Some(t!("cleaning.tools.prompt_library.prompt_cache_import_other_library_warning").to_string());
                }
                let line = if already_present {
                    tf!("cleaning.tools.prompt_library.prompt_cache_import_already_present_status", name = name)
                } else {
                    tf!("cleaning.tools.prompt_library.prompt_cache_imported_status", name = name)
                };
                (line, PromptCacheEffects::default())
            }
        }
    }

    /// Polls the library listing and asks a new `.list` when one is wanted and `gate_open`
    /// (a reachable backend and no operation holding the pipeline).
    ///
    /// A listing that differs from the one held — the FIRST one included: the first `.list` of
    /// a process is what migrates old caches, and `.status` never does — re-asks `.status`
    /// (whether the prompt is served) and the forecast (whether the encoder counts).
    /// `header_for` builds the request header (no extra fields).
    pub(in crate::tools::ai_editor::engines) fn poll_list(&mut self, gate_open: bool, header_for: impl FnOnce(&[(&str, Value)]) -> Value, methods: &PromptCacheMethods) -> PromptCacheEffects {
        let mut effects = PromptCacheEffects::default();
        if let Some(rx) = self.list_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(library)) => {
                    self.list_rx = None;
                    self.list_error = None;
                    if self.library.as_ref() != Some(&library) {
                        effects = PromptCacheEffects::BOTH;
                    }
                    self.library = Some(library);
                }
                Ok(Err(err)) => {
                    self.list_rx = None;
                    ms_log::runtime_log::log_warn(format!("[cleaning] {} prompt library listing failed: {err}", methods.log_label));
                    self.list_error = Some(err);
                }
                Err(TryRecvError::Disconnected) => self.list_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.list_wanted && gate_open && self.list_rx.is_none() {
            self.list_wanted = false;
            let header = header_for(&[]);
            let methods = *methods;
            let (tx, rx) = mpsc::channel();
            self.list_rx = Some(rx);
            thread::spawn(move || {
                // A closed channel means the engine was torn down; the listing has no reader.
                let _ = tx.send(prompt_cache_call(methods.list, methods.query_timeout, header).map(|response| parse_prompt_library_listing(&response)));
            });
        }
        effects
    }
}

/// The save-dialog request of a prompt-cache export: the file is NAMED by its extension
/// (part of the file's identity, so the suggested name carries it; the stem is a plain
/// identifier and stays literal), the engine commits the target after the backend wrote it.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn prompt_cache_export_dialog() -> FileDialogRequest {
    FileDialogRequest::new(DialogKind::SaveFile)
        .title(t!("cleaning.tools.prompt_library.prompt_cache_export_dialog_title"))
        .filter(t!("cleaning.tools.prompt_library.prompt_cache_files_filter"), &[PROMPT_CACHE_EXTENSION])
        .suggested_name(format!("prompt.{PROMPT_CACHE_EXTENSION}"))
}

/// The open-dialog request of a prompt-cache import.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn prompt_cache_import_dialog() -> FileDialogRequest {
    FileDialogRequest::new(DialogKind::OpenFile)
        .title(t!("cleaning.tools.prompt_library.prompt_cache_import_dialog_title"))
        .filter(t!("cleaning.tools.prompt_library.prompt_cache_files_filter"), &[PROMPT_CACHE_EXTENSION])
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

    /// A ranking where `bf16` < `q8_0` < `fp8` < `q4_k_m` < anything else.
    fn rank(precision: &str) -> usize {
        ["bf16", "q8_0", "fp8", "q4_k_m"].iter().position(|known| *known == precision).unwrap_or(usize::MAX)
    }

    fn context(current: Option<(&str, &str)>) -> CacheContext<'static> {
        CacheContext {
            prompt: "remove the sfx",
            encoder_configured: true,
            current: current.map(|(source, precision)| (source.to_string(), precision.to_string())),
            rank,
            fp8_id: "fp8",
            engine_block: None,
        }
    }

    #[test]
    fn an_engine_block_closes_caching_after_readiness_with_its_own_reason() {
        let blocked = CacheContext { engine_block: Some("why".to_string()), ..context(None) };
        assert_eq!(cache_button(&blocked, None, "", None, None, false), CacheButton::Closed(CacheBlock::NotReady));
        assert_eq!(cache_button(&blocked, None, "", None, Some(false), true), CacheButton::Closed(CacheBlock::Engine("why".to_string())));
        let unconfigured = CacheContext { encoder_configured: false, ..context(None) };
        assert_eq!(cache_button(&unconfigured, None, "", None, None, true), CacheButton::Closed(CacheBlock::NothingToEncode));
    }

    #[test]
    fn the_more_precise_check_uses_the_engines_ranking_and_fp8_id() {
        let fp8_entry = PromptLibraryEntry { encoder_fp8: true, ..entry("f.msprompt", "stock", "bf16", Some(false)) };
        let hit = preset("sfx", vec![fp8_entry.clone()]);
        // fp8 ranks before q4_k_m: closes a q4_k_m build of the same source.
        assert_eq!(cache_button(&context(Some(("stock", "q4_k_m"))), Some(&hit), "sfx", Some(true), None, true), CacheButton::Closed(CacheBlock::MorePrecise(fp8_entry)));
        // …and not a bf16 one.
        assert_eq!(cache_button(&context(Some(("stock", "bf16"))), Some(&hit), "sfx", Some(true), None, true), CacheButton::Cache);
    }

    #[test]
    fn export_needs_a_preset_and_import_only_the_backend() {
        let ctx = context(None);
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
    fn the_cache_state_counts_only_for_the_prompt_it_was_asked_about() {
        assert_eq!(prompt_cache_state_for(Some("a prompt"), Some(true), "  a prompt  "), Some(true), "the comparison is on the trimmed prompt, which is what was sent");
        assert_eq!(prompt_cache_state_for(Some("a prompt"), Some(true), "a different prompt"), None, "an answer about an older prompt must read as unknown");
        assert_eq!(prompt_cache_state_for(None, Some(true), "a prompt"), None, "no answer has landed yet");
        assert_eq!(prompt_cache_state_for(Some("a prompt"), None, "a prompt"), None);
    }

    #[test]
    fn the_status_cache_half_is_three_state_and_tolerant() {
        let (cached, entry) = parse_prompt_cache_status(&json!({ "prompt_cached": true, "prompt_cache_entry": { "file": "a.msprompt", "origin": "built" } }));
        assert_eq!(cached, Some(true));
        assert_eq!(entry.map(|entry| entry.file), Some("a.msprompt".to_string()));
        for absent in [json!({ "prompt_cache_entry": null }), json!({}), json!({ "prompt_cache_entry": "a.msprompt" })] {
            assert_eq!(parse_prompt_cache_status(&absent), (None, None));
        }
        assert_eq!(parse_prompt_cache_status(&json!({ "prompt_cached": false })).0, Some(false));
    }

    #[test]
    fn prompt_cache_headers_put_the_operation_fields_beside_the_params() {
        let header = prompt_cache_header(json!({ "prompt": "x" }), &[("file", json!("a.msprompt")), ("path", json!("/tmp/a.msprompt"))]);
        assert_eq!(header["params"]["prompt"], json!("x"));
        assert_eq!(header["file"], json!("a.msprompt"));
        assert_eq!(header["path"], json!("/tmp/a.msprompt"));
        assert!(header["params"].get("file").is_none());
    }

    #[test]
    fn build_and_import_answers_parse_tolerantly() {
        let built = parse_prompt_cache_build(&json!({ "encoded": true, "written": [{ "file": "a.msprompt" }, 3] }), false);
        assert_eq!(built, PromptCacheOutcome::Built { written: vec![PromptLibraryEntry { file: "a.msprompt".to_string(), ..PromptLibraryEntry::default() }], renamed: false });
        assert_eq!(parse_prompt_cache_build(&json!({}), true), PromptCacheOutcome::Built { written: Vec::new(), renamed: true });
        assert_eq!(parse_prompt_cache_build(&json!({ "renamed": true }), false), PromptCacheOutcome::Built { written: Vec::new(), renamed: true });

        let imported = parse_prompt_cache_import(&json!({ "file": "a.msprompt", "name": "sfx", "library_matches": false, "already_present": true }));
        assert_eq!(imported, PromptCacheOutcome::Imported { name: "sfx".to_string(), library_matches: Some(false), already_present: true });
        let bare = parse_prompt_cache_import(&json!({ "file": "a.msprompt" }));
        assert_eq!(bare, PromptCacheOutcome::Imported { name: "a.msprompt".to_string(), library_matches: None, already_present: false });
    }

    /// The line names the encoder of the entry in use through the engine's caption: the fresh
    /// `.status` entry first, the listing's selection while `.status` has not answered, and an
    /// empty caption names nothing.
    #[test]
    fn the_cache_line_names_through_the_engine_caption() {
        let listed = entry("a.msprompt", "stock", "bf16", None);
        let caption = |entry: &PromptLibraryEntry| entry.encoder_format.clone();
        assert_eq!(prompt_cache_line(Some(&listed), None, Some(true), None, &caption), PromptCacheLine::Cached { encoder: Some("bf16".to_string()), memory: false });
        let silent = |_: &PromptLibraryEntry| String::new();
        assert_eq!(prompt_cache_line(Some(&listed), None, None, None, &silent), PromptCacheLine::Cached { encoder: None, memory: false });
        assert_eq!(prompt_cache_line(None, None, None, None, &silent), PromptCacheLine::Silent);
    }

    #[test]
    fn a_finished_operation_reports_what_the_engine_must_re_ask() {
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ctl = PromptCacheController { list_wanted: false, ..PromptCacheController::default() };
        let (_, effects) = ctl.apply_outcome(PromptCacheOutcome::Built { written: Vec::new(), renamed: true });
        assert_eq!(effects, PromptCacheEffects { status_wanted: true, forecast_invalidated: true });
        assert!(ctl.list_wanted, "a build re-arms the listing");
        let (_, effects) = ctl.apply_outcome(PromptCacheOutcome::Exported(PathBuf::from("/tmp/a.msprompt")));
        assert_eq!(effects, PromptCacheEffects::default());
    }

    /// An import into another model's library warns once; an entry already present says so.
    #[test]
    fn an_import_reports_another_library_and_an_entry_already_present() {
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ctl = PromptCacheController { list_wanted: false, ..PromptCacheController::default() };
        let (line, _) = ctl.apply_outcome(PromptCacheOutcome::Imported { name: "sfx".to_string(), library_matches: Some(false), already_present: false });
        assert_eq!(line, tf!("cleaning.tools.prompt_library.prompt_cache_imported_status", name = "sfx"));
        assert!(ctl.warning.is_some() && ctl.list_wanted);
        let mut present = PromptCacheController::default();
        let (line, _) = present.apply_outcome(PromptCacheOutcome::Imported { name: "sfx".to_string(), library_matches: None, already_present: true });
        assert_eq!(line, tf!("cleaning.tools.prompt_library.prompt_cache_import_already_present_status", name = "sfx"));
        assert!(present.warning.is_none(), "an unknown library match invents no warning");
    }

    /// A listing that differs from the one held (the first one included: it is what migrates
    /// the old caches) asks for `.status` and the forecast; an identical one does not. The
    /// gate keeps a wanted query from starting.
    #[test]
    fn a_changed_listing_asks_for_status_and_the_forecast() {
        const METHODS: PromptCacheMethods = PromptCacheMethods {
            build: "test.build",
            list: "test.list",
            export: "test.export",
            import: "test.import",
            build_timeout: Duration::from_secs(1),
            query_timeout: Duration::from_secs(1),
            log_label: "test",
        };
        let listing = |texts: &[&str]| {
            parse_prompt_library_listing(&json!({
                "texts": texts.iter().map(|text| json!({ "prompt": text, "entries": [{ "file": format!("{text}.msprompt") }] })).collect::<Vec<_>>()
            }))
        };
        let deliver = |ctl: &mut PromptCacheController, library: PromptLibraryListing| {
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(library)).expect("the receiver is alive");
            ctl.list_rx = Some(rx);
            ctl.list_wanted = false;
            ctl.poll_list(false, |_| json!({}), &METHODS)
        };
        let mut ctl = PromptCacheController::default();
        assert_eq!(deliver(&mut ctl, listing(&["sfx"])), PromptCacheEffects::BOTH, "the first listing may have migrated old caches");
        assert_eq!(deliver(&mut ctl, listing(&["sfx"])), PromptCacheEffects::default(), "an identical listing is not a change");
        assert_eq!(deliver(&mut ctl, listing(&["sfx", "title"])), PromptCacheEffects::BOTH, "a changed listing re-arms both");
        ctl.list_wanted = true;
        assert_eq!(ctl.poll_list(false, |_| json!({}), &METHODS), PromptCacheEffects::default());
        assert!(ctl.list_wanted && !ctl.list_busy(), "a closed gate starts no query");
    }

    #[test]
    fn an_export_captures_the_file_of_the_typed_preset() {
        let mut ctl = PromptCacheController::default();
        assert!(!ctl.arm_export("remove the sfx"), "no listing: nothing to export");
        ctl.library = Some(parse_prompt_library_listing(&json!({ "texts": [{ "prompt": "remove the sfx", "entries": [{ "file": "a.msprompt" }] }] })));
        assert!(ctl.arm_export("  remove the sfx "));
        assert_eq!(ctl.export_file.as_deref(), Some("a.msprompt"));
    }

    /// The prompt-library texts every engine shows exist in every shipped catalog with the
    /// placeholders the code fills, and the FLUX.2-namespaced ids they replaced are gone.
    #[test]
    fn every_catalog_carries_the_moved_prompt_library_keys_and_not_the_old_ids() {
        const MOVED: [(&str, &[&str]); 38] = [
            ("prompt_cache_already_built_status", &[]),
            ("prompt_cache_build_button", &[]),
            ("prompt_cache_build_current_exists_status", &[]),
            ("prompt_library_save_disabled_tooltip", &[]),
            ("prompt_cache_build_more_precise_status", &["{format}"]),
            ("prompt_library_save_tooltip", &[]),
            ("prompt_cache_building_status", &[]),
            ("prompt_library_saved_status", &[]),
            ("prompt_library_no_encoder_disabled_tooltip", &[]),
            ("prompt_cache_error", &["{err}"]),
            ("prompt_cache_export_button", &[]),
            ("prompt_cache_export_dialog_title", &[]),
            ("prompt_library_export_disabled_tooltip", &[]),
            ("prompt_library_export_tooltip", &[]),
            ("prompt_cache_exported_status", &["{path}"]),
            ("prompt_cache_exporting_status", &[]),
            ("prompt_cache_files_filter", &[]),
            ("prompt_cache_import_already_present_status", &["{name}"]),
            ("prompt_cache_import_button", &[]),
            ("prompt_cache_import_dialog_title", &[]),
            ("prompt_cache_import_disabled_tooltip", &[]),
            ("prompt_cache_import_other_library_warning", &[]),
            ("prompt_library_import_tooltip", &[]),
            ("prompt_cache_imported_status", &["{name}"]),
            ("prompt_cache_importing_status", &[]),
            ("prompt_cache_list_error", &["{err}"]),
            ("prompt_cache_memory_suffix", &[]),
            ("prompt_library_preset_name_hint", &[]),
            ("prompt_cache_name_tooltip", &[]),
            ("prompt_cache_rename_button", &[]),
            ("prompt_cache_rename_tooltip", &[]),
            ("prompt_cache_renamed_status", &[]),
            ("prompt_cache_renaming_status", &[]),
            ("prompt_cache_skipped_status", &["{count}"]),
            ("prompt_cached_status", &[]),
            ("prompt_cached_with_encoder_status", &["{encoder}"]),
            ("prompt_library_toggle_hint", &[]),
            ("prompt_library_label", &[]),
        ];
        const ALSO_MOVED: [&str; 2] = ["prompt_not_cached_status", "text_encoder_missing_warning"];
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            let suffixes = MOVED.iter().map(|(suffix, needed)| (*suffix, *needed)).chain(ALSO_MOVED.iter().map(|suffix| (*suffix, &[][..])));
            for (suffix, needed) in suffixes {
                let old = format!("cleaning.tools.flux2_klein.{suffix}");
                assert!(catalog.get(&old).is_none(), "locale `{tag}` still carries the moved `{old}`");
                let new = format!("cleaning.tools.prompt_library.{suffix}");
                // en and ru carry every key; es / fr / pt carry the ones they had translated
                // and fall back to en for the rest.
                let Some(value) = catalog.get(&new).and_then(Value::as_str) else {
                    assert!(!matches!(*tag, "en" | "ru"), "locale `{tag}` lacks `{new}`");
                    continue;
                };
                assert!(!value.trim().is_empty(), "locale `{tag}`: `{new}` is empty");
                for placeholder in needed {
                    assert!(value.contains(placeholder), "locale `{tag}`: `{new}` drops `{placeholder}`");
                }
            }
        }
    }
}
