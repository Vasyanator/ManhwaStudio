/*
File: cleaning/tools/ai_editor/engines/shared/hf_download.rs

Purpose:
The model-agnostic half of a Hugging Face model download driven through the backend: the
`.download.check` answer (what each repository is, whether the stored token opens it, and
what the transfer would cost), the verdict derived from it, the streaming `.download.start`
call, and the secret-store controls of the process-wide Hugging Face token. A model engine
adds only its request header, its staleness rule and what a finished download means to it.

Main responsibilities:
- own the per-repository state vocabulary and the page each state links to
  (`HfDownloadState`, `HfDownloadLink`, `HfDownloadRepo::link`);
- own the check shape, generic over the QUESTION an engine stamps into it
  (`HfDownloadCheck<Q>`), its parser (`parse_hf_download_check`) and the verdict whether it
  is permission to download (`hf_download_readiness`) or blames the token
  (`download_check_blames_token`);
- run the two calls (`call_hf_download_check`, `run_hf_download`);
- keep the token entry field and its store/delete worker (`HfTokenControl`).

Key structures:
- `HfDownloadState`, `HfDownloadLink`, `HfDownloadRepo`, `HfDownloadPlan`, `HfDownloadCheck`
- `HfDownloadReadiness`, `HfDownloadAction`, `HfTokenAction`, `HfTokenControl`

Key functions:
- `parse_hf_download_check()`, `hf_download_readiness()`, `download_check_blames_token()`
- `call_hf_download_check()`, `run_hf_download()`

Notes:
The pinned contract is `dev-docs/flux2_model_download.md` §3-§4 (the FLUX.2 methods were the
first to use it). The HF token travels only in a request header the ENGINE builds and is
never persisted, logged or formatted here. The drawing half is `hf_download_ui.rs`. Every
text is a `cleaning.tools.hf_download.*` key.
*/

use super::stream_progress::{EngineProgress, ProgressClaim, publish_progress_frame, stream_call, update_progress};
use super::stream_progress::map_call_error;
use eframe::egui::Color32;
use ms_backend_ipc as backend_ipc;
use ms_local_ai::backend_health::ai_backend_offline_error;
use ms_thread as thread;
// The one owner of the Hugging Face page URLs (also linked by the access hint).
use ms_widgets::hf_access_hint::{HF_TOKEN_SETTINGS_URL, hf_repo_url};
use serde_json::Value;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use web_time::Duration;

/// One repository's access verdict, as `.download.check` reports it.
///
/// The mapping is FIXED by the pinned contract (`dev-docs/flux2_model_download.md` §3) and
/// each state answers a different question for the user, which is the whole reason the
/// backend distinguishes them instead of returning one "no access" error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum HfDownloadState {
    /// `HfApi.auth_check()` passed for this token on this repository, so it is ready to
    /// download from. Deliberately NOT "the listing came back": a gated repository's
    /// METADATA is public, so a listing succeeds for a user holding no token at all.
    Ok,
    /// The request carried an empty token and no network call was made.
    NoToken,
    /// HTTP 401 — the token is wrong or has been revoked.
    InvalidToken,
    /// HTTP 403 (or a `GatedRepoError` carrying no status) — the repository is gated and
    /// its conditions have not been accepted. The backend decides this by STATUS first and
    /// exception class second, because `GatedRepoError` is also raised with status 401 for
    /// a merely bad token.
    NotAccepted,
    /// HTTP 404 — the repository id no longer exists.
    NotFound,
    /// Anything else; the backend's own message is the only useful detail.
    NetworkError,
}

impl HfDownloadState {
    /// The wire literal of this verdict.
    pub(in crate::tools::ai_editor::engines) fn wire(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoToken => "no_token",
            Self::InvalidToken => "invalid_token",
            Self::NotAccepted => "not_accepted",
            Self::NotFound => "not_found",
            Self::NetworkError => "network_error",
        }
    }

    /// Reads a verdict literal.
    ///
    /// `None` for anything this build does not know — a NEWER backend reporting a state
    /// added after this release. The honest answer there is "not known", shown with the
    /// literal on hover: silently folding an unknown literal onto one of the six would
    /// send the user to accept conditions they have already accepted, or tell them a token
    /// they just created is invalid. Same rule the residency block already follows.
    pub(in crate::tools::ai_editor::engines) fn from_wire(value: &str) -> Option<Self> {
        [
            Self::Ok,
            Self::NoToken,
            Self::InvalidToken,
            Self::NotAccepted,
            Self::NotFound,
            Self::NetworkError,
        ]
        .into_iter()
        .find(|candidate| candidate.wire() == value)
    }

    /// The localized line this verdict puts on screen.
    pub(in crate::tools::ai_editor::engines) fn message(self) -> &'static str {
        match self {
            Self::Ok => t!("cleaning.tools.hf_download.state_ok"),
            Self::NoToken => t!("cleaning.tools.hf_download.state_no_token"),
            Self::InvalidToken => t!("cleaning.tools.hf_download.state_invalid_token"),
            Self::NotAccepted => t!("cleaning.tools.hf_download.state_not_accepted"),
            Self::NotFound => t!("cleaning.tools.hf_download.state_not_found"),
            Self::NetworkError => t!("cleaning.tools.hf_download.state_network_error"),
        }
    }

    /// The colour the verdict is drawn in; `None` for a neutral line.
    pub(in crate::tools::ai_editor::engines) fn color(self) -> Option<Color32> {
        match self {
            Self::Ok => Some(ms_theme::status::SUCCESS),
            // Amber, not red: nothing is broken, the user simply has a step left to take.
            Self::NoToken | Self::NotAccepted => Some(ms_theme::status::WARNING),
            Self::InvalidToken | Self::NotFound | Self::NetworkError => Some(ms_theme::status::ERROR),
        }
    }

    /// The link that makes this verdict actionable.
    ///
    /// This is the FEATURE: a gated repository fails in three different ways and each one
    /// has a different page behind it. A verdict with no link is one the user cannot click
    /// their way out of.
    pub(in crate::tools::ai_editor::engines) fn link(self) -> HfDownloadLink {
        match self {
            Self::Ok | Self::NotFound | Self::NetworkError => HfDownloadLink::None,
            Self::NoToken | Self::InvalidToken => HfDownloadLink::TokenSettings,
            Self::NotAccepted => HfDownloadLink::RepoPage,
        }
    }
}

/// The page one verdict sends the user to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum HfDownloadLink {
    /// Nothing to click: the state is not something a page can fix.
    None,
    /// `https://huggingface.co/settings/tokens` — where a token is created.
    TokenSettings,
    /// The gated repository's own page, where the conditions are accepted.
    RepoPage,
}

/// One row of the `repos` block: the repository the backend was asked about and how it
/// answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct HfDownloadRepo {
    /// The repository id, e.g. `black-forest-labs/FLUX.2-klein-9B`. A backend identifier
    /// and therefore literal — it is also what the repository link is built from.
    pub(in crate::tools::ai_editor::engines) repo: String,
    /// The parsed verdict; `None` when the backend sent a literal this build does not
    /// know ([`HfDownloadState::from_wire`]).
    pub(in crate::tools::ai_editor::engines) state: Option<HfDownloadState>,
    /// The literal the backend actually sent, kept verbatim so an unrecognised one can
    /// still be shown on hover instead of vanishing without trace.
    pub(in crate::tools::ai_editor::engines) state_wire: String,
    /// The backend's own message about this repository, empty for `ok`.
    ///
    /// It is the TECHNICAL half: `network_error` has no localizable content — only the
    /// transport's own wording says what actually went wrong — so the row shows the
    /// localized state and carries this verbatim on hover and in the log, never in place
    /// of the sentence the user reads.
    pub(in crate::tools::ai_editor::engines) message: String,
}

impl HfDownloadRepo {
    /// The link this row offers — its localized caption and its URL — or `None` when the
    /// verdict offers none.
    ///
    /// Caption and URL are decided together so a row can never show "open the model page"
    /// pointing at the token settings. An unrecognised state offers no link at all.
    pub(in crate::tools::ai_editor::engines) fn link(&self) -> Option<(String, String)> {
        match self.state?.link() {
            HfDownloadLink::None => None,
            HfDownloadLink::TokenSettings => Some((
                t!("cleaning.tools.hf_download.token_settings_link").to_string(),
                HF_TOKEN_SETTINGS_URL.to_string(),
            )),
            HfDownloadLink::RepoPage => Some((
                t!("cleaning.tools.hf_download.repo_page_link").to_string(),
                hf_repo_url(&self.repo),
            )),
        }
    }
}

/// What the download would cost, computed by the backend from the same listing the
/// access check already fetched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct HfDownloadPlan {
    /// Size of the complete file set the request needs.
    pub(in crate::tools::ai_editor::engines) total_bytes: u64,
    /// Of that, what is missing or empty on disk — the number the button carries.
    pub(in crate::tools::ai_editor::engines) missing_bytes: u64,
    pub(in crate::tools::ai_editor::engines) missing_files: u64,
}

/// The whole `.download.check` answer, plus the QUESTION it answers.
///
/// `Q` is the engine's own description of what was asked (FLUX.2: the encoder toggle and the
/// echoed checkpoint), which its staleness rule compares against the panel; an engine with
/// nothing to compare uses `()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct HfDownloadCheck<Q> {
    /// One row per repository the request actually needs, in the order the backend listed
    /// them.
    pub(in crate::tools::ai_editor::engines) repos: Vec<HfDownloadRepo>,
    /// `None` when the backend could not price the download — the contract's
    /// `"plan": null`.
    ///
    /// ACCESS AND LISTING ARE TWO DIFFERENT NETWORK OPERATIONS: the token can pass
    /// `auth_check` for every repository while fetching the file listing still fails. An
    /// absent plan therefore means the size is NOT KNOWN and must never collapse into
    /// "nothing is missing" — a zero-valued plan beside `ok` states renders a complete
    /// installation on an empty machine, which is the worst failure this block has.
    pub(in crate::tools::ai_editor::engines) plan: Option<HfDownloadPlan>,
    /// Why the plan is missing, verbatim from the backend. Empty whenever `plan` is
    /// present. The TECHNICAL half, shown on hover beside a localized line, exactly as a
    /// repository's own `message` is.
    pub(in crate::tools::ai_editor::engines) plan_error: String,
    /// What this answer is about, stamped by the engine (from the asking worker, or from an
    /// echoed field). Not interpreted here.
    pub(in crate::tools::ai_editor::engines) question: Q,
}

impl<Q> HfDownloadCheck<Q> {
    /// Whether every repository the check covered answered `ok`.
    ///
    /// An EMPTY row list is not "ready": it means the backend named no repository, so
    /// there is nothing to conclude. An unrecognised literal is not "ready" either.
    pub(in crate::tools::ai_editor::engines) fn ready(&self) -> bool {
        !self.repos.is_empty() && self.repos.iter().all(|repo| repo.state == Some(HfDownloadState::Ok))
    }
}

/// Whether the check blames the Hugging Face TOKEN for at least one repository.
///
/// Only [`HfDownloadState::NoToken`] and [`HfDownloadState::InvalidToken`] count: they are
/// the two verdicts nothing but the token can resolve. `NotAccepted` is about the
/// repository's conditions and already carries its own link, and an unrecognised literal is
/// never guessed at.
///
/// It exists for an UNGATED model, whose panel draws no token row at all: without this the
/// user is handed a verdict about a credential they have no control over anywhere in the
/// application ([`ms_sysprobe::hf_token`] has its UI surface only in these download blocks).
pub(in crate::tools::ai_editor::engines) fn download_check_blames_token<Q>(check: Option<&HfDownloadCheck<Q>>) -> bool {
    check.is_some_and(|check| {
        check
            .repos
            .iter()
            .any(|repo| matches!(repo.state, Some(HfDownloadState::NoToken | HfDownloadState::InvalidToken)))
    })
}

/// What the download controls render for the access check they were given.
///
/// A separate type, and pure, because the difference between its two middle variants is a
/// defect the type system can otherwise not see: an unpriced download and a finished one
/// both have "no missing bytes" and must render as opposites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum HfDownloadReadiness {
    /// Nothing to price and nothing to start: no check yet, or a check with no plan whose
    /// repository rows already explain why (no token, or a repository the user cannot
    /// reach). «Скачать» stays closed and no extra line is synthesised — the rows are the
    /// explanation.
    Blocked,
    /// No plan although access was granted everywhere: the FILE LISTING failed. Neither a
    /// size nor a completion state may be drawn, and «Скачать» stays AVAILABLE — pressing
    /// it re-lists and either succeeds or reports the same failure honestly. The
    /// repositories keep rendering as `ok`, because authentication really did succeed and
    /// inventing a gating state here would send the user to fix something intact.
    SizeUnknown,
    /// A plan was computed and something is missing: `missing_bytes` is a real number.
    Priced { missing_bytes: u64 },
    /// A plan was computed and every file is already on disk.
    Complete,
}

impl HfDownloadReadiness {
    /// Whether «Скачать» may be pressed. `SizeUnknown` keeps the button OPEN: pressing it
    /// re-lists, and either succeeds or reports the same failure honestly — leaving it
    /// closed would strand a user whose listing failed once with no way to retry.
    pub(in crate::tools::ai_editor::engines) fn startable(self) -> bool {
        match self {
            Self::Priced { .. } | Self::SizeUnknown => true,
            Self::Blocked | Self::Complete => false,
        }
    }
}

/// Decides what the download controls render from the last access check.
///
/// **The ABSENT PLAN decides on its own**, before the repository states are consulted: a
/// `null` plan means no size and no completion state may be drawn, whatever the states say.
/// The backend sends it for all three of "no token", "a repository is inaccessible" and "the
/// listing failed", because a zero-valued plan is the same lie in every one of them — it
/// reads as "nothing left to download", and was observed reporting a complete installation
/// on an empty machine. The states are consulted only afterwards, to decide whether
/// «Скачать» can usefully be pressed.
pub(in crate::tools::ai_editor::engines) fn hf_download_readiness<Q>(check: Option<&HfDownloadCheck<Q>>) -> HfDownloadReadiness {
    let Some(check) = check else {
        return HfDownloadReadiness::Blocked;
    };
    let Some(plan) = check.plan else {
        // Access granted means the listing is the only thing that failed, so a retry is
        // worth offering; anything else is already explained by its own row.
        return if check.ready() { HfDownloadReadiness::SizeUnknown } else { HfDownloadReadiness::Blocked };
    };
    // A plan behind a refusal prices a download that cannot start, so it is not offered.
    if !check.ready() {
        return HfDownloadReadiness::Blocked;
    }
    if plan.missing_bytes == 0 {
        HfDownloadReadiness::Complete
    } else {
        HfDownloadReadiness::Priced { missing_bytes: plan.missing_bytes }
    }
}

/// The one download control the user pressed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum HfDownloadAction {
    /// Run `.download.check` for the stored token and the current selection.
    Check,
    /// Run the streaming `.download.start`.
    Start,
    /// Abandon the download in flight and tell the backend to stop.
    Cancel,
}

/// The one secret-store control the user pressed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum HfTokenAction {
    /// Write the field's contents into the OS secret store.
    Save,
    /// Delete the stored token.
    Delete,
}

/// Parses a `.download.check` answer and stamps `question` into it.
///
/// Every field is optional and degrades rather than failing: an absent `repos` yields no
/// rows (the block then says nothing was checked), a row whose `state` is missing or
/// unrecognised keeps its literal for the hover, and an absent `plan` stays `None` — never
/// a plan of zero bytes, which would put "0.0 GiB" on the download button.
///
/// `message` is contractually always present and empty for `ok`, but it is read as
/// optional all the same: a missing one costs a hover, while treating its absence as an
/// error would cost the whole verdict.
pub(in crate::tools::ai_editor::engines) fn parse_hf_download_check<Q>(header: &Value, question: Q) -> HfDownloadCheck<Q> {
    let text = |value: &Value, name: &str| value.get(name).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let repos = header
        .get("repos")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .map(|(repo, value)| {
                    let state_wire = value.get("state").and_then(Value::as_str).unwrap_or_default().to_string();
                    HfDownloadRepo {
                        repo: repo.clone(),
                        state: HfDownloadState::from_wire(&state_wire),
                        state_wire,
                        message: text(value, "message"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    // `plan` is NULLABLE by contract: an explicit `null` is how a failed file listing is
    // reported, and it must stay `None` here. A plan object carrying neither total counts
    // as no plan for the same reason — the alternative is a free download of zero bytes,
    // which renders as "everything is already installed".
    let plan = header.get("plan").and_then(|plan| {
        let field = |name: &str| plan.get(name).and_then(Value::as_u64);
        let total_bytes = field("total_bytes");
        let missing_bytes = field("missing_bytes");
        if total_bytes.is_none() && missing_bytes.is_none() {
            return None;
        }
        Some(HfDownloadPlan {
            total_bytes: total_bytes.unwrap_or(0),
            missing_bytes: missing_bytes.unwrap_or(0),
            missing_files: field("missing_files").unwrap_or(0),
        })
    });
    // Only meaningful without a plan; the contract leaves it absent or empty otherwise, and
    // it is dropped here in that case so nothing downstream has to re-check the pairing.
    let plan_error = if plan.is_some() { String::new() } else { text(header, "plan_error") };
    HfDownloadCheck { repos, plan, plan_error, question }
}

/// Runs a one-shot `.download.check`-shaped call and returns its answer header.
///
/// BLOCKING: worker threads only. `header` is the engine's request (token, selection);
/// `timeout` bounds the call.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable. A repository that refused access is NOT an error — it is a verdict, and
/// the whole point of the call is to tell the six of them apart.
pub(in crate::tools::ai_editor::engines) fn call_hf_download_check(
    method: &'static str,
    header: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (response, _blob) = client.call(method, header, &[], timeout).map_err(map_call_error)?;
    Ok(response)
}

/// Runs a streaming `.download.start`-shaped call and returns its answer header.
///
/// Streaming, and it drives the engine's ONE progress bar, which is what makes a download
/// mutually exclusive with the engine's other long operations. `claim` is the progress claim
/// taken on the GUI thread: every write is dropped once a newer operation (or a cancel) has
/// retired its generation, the request id goes to its control (so a cancel that came first is
/// sent the moment the id exists), and the bar — second level included — is cleared on EVERY
/// exit. `frame_timeout` is per progress frame (see `stream_call`).
///
/// # Errors
/// Returns a user-facing message when the backend refuses (no access to a gated
/// repository, not enough free space for the plan), when the transfer fails or is
/// cancelled, when it does not know the method, or when it is unreachable. No message
/// carries the token.
pub(in crate::tools::ai_editor::engines) fn run_hf_download(
    method: &'static str,
    header: Value,
    frame_timeout: Duration,
    progress: &Arc<Mutex<EngineProgress>>,
    claim: &ProgressClaim,
) -> Result<Value, String> {
    let outcome = stream_call(
        method,
        header,
        &[],
        frame_timeout,
        |id| claim.publish_request_id(id),
        |frame| publish_progress_frame(progress, claim.generation, frame),
    )
    .map(|(header, _blob)| header);
    update_progress(progress, claim.generation, |state| {
        state.active = false;
        state.file = None;
    });
    outcome
}

/// The Hugging Face token entry field and its secret-store worker.
///
/// The token itself lives in the process-wide `ms_sysprobe::hf_token` store; this holds
/// only what the user is TYPING (`input`, never persisted, never sent anywhere on its own),
/// the store/delete operation in flight, and the line about the last one.
#[derive(Debug, Default)]
pub(in crate::tools::ai_editor::engines) struct HfTokenControl {
    /// Buffer of the masked token field. Cleared as soon as «Сохранить» hands it to the
    /// worker, so the value does not linger on screen.
    pub(in crate::tools::ai_editor::engines) input: String,
    /// A secret-store save or delete in flight. `Ok` carries the localized outcome line;
    /// neither branch ever carries the token.
    pub(in crate::tools::ai_editor::engines) rx: Option<Receiver<Result<String, String>>>,
    /// User-facing line about the last token operation, shown beside the badge.
    pub(in crate::tools::ai_editor::engines) status: Option<String>,
}

impl HfTokenControl {
    /// Whether a store or delete is in flight.
    pub(in crate::tools::ai_editor::engines) fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// Applies the control the user pressed.
    pub(in crate::tools::ai_editor::engines) fn apply(&mut self, action: HfTokenAction) {
        match action {
            HfTokenAction::Save => self.start_store(),
            HfTokenAction::Delete => self.start_clear(),
        }
    }

    /// Writes the field into the OS secret store, on a worker thread.
    ///
    /// The keyring is an OS round trip, so it never happens on the GUI thread
    /// (`machine_translation.rs` spawns for the AI API keys for the same reason). The
    /// buffer is cleared IMMEDIATELY, before the worker even starts: the value is already
    /// on its way to the store, and leaving it in a widget only keeps a secret on screen.
    pub(in crate::tools::ai_editor::engines) fn start_store(&mut self) {
        if self.rx.is_some() {
            return;
        }
        let token = std::mem::take(&mut self.input);
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let result = ms_sysprobe::hf_token::store_hf_token(&token)
                .map(|()| t!("cleaning.tools.hf_download.token_saved_status").to_string());
            // A failed send means the engine was dropped meanwhile: nobody waits for it.
            let _ = tx.send(result);
        });
    }

    /// Deletes the stored token, on a worker thread. See [`Self::start_store`].
    pub(in crate::tools::ai_editor::engines) fn start_clear(&mut self) {
        if self.rx.is_some() {
            return;
        }
        self.input.clear();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let result = ms_sysprobe::hf_token::clear_hf_token()
                .map(|()| t!("cleaning.tools.hf_download.token_deleted_status").to_string());
            // A failed send means the engine was dropped meanwhile: nobody waits for it.
            let _ = tx.send(result);
        });
    }

    /// Drains the secret-store channel into the status line. Returns `true` when an
    /// operation SUCCEEDED: the stored token changed, so the caller must drop any access
    /// check computed for the previous one. `log_label` names the engine in the log.
    pub(in crate::tools::ai_editor::engines) fn poll(&mut self, log_label: &str) -> bool {
        let Some(rx) = self.rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(status)) => {
                self.rx = None;
                self.status = Some(status);
                true
            }
            Ok(Err(err)) => {
                self.rx = None;
                // The error carries the keyring's message, which never contains the token.
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] {log_label} Hugging Face token operation failed: {err}"
                ));
                self.status = Some(err);
                false
            }
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.status = Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                false
            }
            Err(TryRecvError::Empty) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Builds a `.download.check` answer with one repository in the given state.
    ///
    /// The byte figures here are ROUND ARBITRARY NUMBERS on purpose. The real manifest sizes
    /// belong to the backend and reach this side only through the `plan` object at runtime;
    /// pinning one in a fixture would turn a re-shard or a manifest change into a red test
    /// about nothing, and invite someone to hard-code it in the UI.
    fn download_answer(repo: &str, state: &str, message: &str) -> Value {
        json!({
            "repos": { repo: { "state": state, "message": message } },
            "plan": { "total_bytes": 40_000_000_000u64, "missing_bytes": 18_000_000_000u64, "missing_files": 3 }
        })
    }

    /// The question-free parse every test here uses.
    fn parse(header: &Value) -> HfDownloadCheck<()> {
        parse_hf_download_check(header, ())
    }

    /// The literal -> verdict mapping is the feature: each of the six states sends the
    /// user somewhere different, and an unknown one must send them nowhere at all.
    #[test]
    fn every_download_state_literal_maps_to_its_own_verdict_and_link() {
        // Round trip first: the wire spelling is what the contract pins, and a typo here
        // would silently turn every verdict into "not known".
        for state in [
            HfDownloadState::Ok,
            HfDownloadState::NoToken,
            HfDownloadState::InvalidToken,
            HfDownloadState::NotAccepted,
            HfDownloadState::NotFound,
            HfDownloadState::NetworkError,
        ] {
            assert_eq!(HfDownloadState::from_wire(state.wire()), Some(state));
        }
        // The exact literals, spelled out, because the Python half matches on these.
        for (wire, expected) in [
            ("ok", HfDownloadState::Ok),
            ("no_token", HfDownloadState::NoToken),
            ("invalid_token", HfDownloadState::InvalidToken),
            ("not_accepted", HfDownloadState::NotAccepted),
            ("not_found", HfDownloadState::NotFound),
            ("network_error", HfDownloadState::NetworkError),
        ] {
            assert_eq!(HfDownloadState::from_wire(wire), Some(expected));
        }

        // The three states that carry a link, and WHICH link. Getting these two round the
        // wrong way is the failure the states exist to prevent: sending someone whose
        // token is merely invalid to accept conditions they have already accepted.
        let repo = |state: HfDownloadState| HfDownloadRepo {
            repo: "black-forest-labs/FLUX.2-klein-9B".to_string(),
            state: Some(state),
            state_wire: state.wire().to_string(),
            message: String::new(),
        };
        for state in [HfDownloadState::NoToken, HfDownloadState::InvalidToken] {
            assert_eq!(state.link(), HfDownloadLink::TokenSettings);
            let (_caption, url) = repo(state).link().expect("a token state offers a link");
            assert_eq!(url, "https://huggingface.co/settings/tokens");
        }
        assert_eq!(HfDownloadState::NotAccepted.link(), HfDownloadLink::RepoPage);
        let (_caption, url) = repo(HfDownloadState::NotAccepted)
            .link()
            .expect("a gated repo links to its own page");
        assert_eq!(url, "https://huggingface.co/black-forest-labs/FLUX.2-klein-9B");

        // The three that carry none: there is no page that fixes them.
        for state in [HfDownloadState::Ok, HfDownloadState::NotFound, HfDownloadState::NetworkError] {
            assert_eq!(state.link(), HfDownloadLink::None);
            assert!(repo(state).link().is_none());
        }
    }

    /// The degradation rule of D7: a literal this build does not know is "not known", it
    /// keeps the literal for the hover, and it offers no link — never one of the six.
    #[test]
    fn an_unrecognised_download_state_claims_no_verdict_and_keeps_the_literal() {
        assert_eq!(HfDownloadState::from_wire("quota_exceeded"), None);
        assert_eq!(HfDownloadState::from_wire(""), None);
        assert_eq!(HfDownloadState::from_wire("OK"), None);

        let check = parse(&download_answer(
            "black-forest-labs/FLUX.2-klein-9B",
            "quota_exceeded",
            "monthly quota exhausted",
        ));
        let row = check.repos.first().expect("one row");
        assert_eq!(row.state, None, "an unknown literal must claim no state");
        assert_eq!(row.state_wire, "quota_exceeded", "the literal survives for the hover");
        assert_eq!(row.message, "monthly quota exhausted");
        assert!(row.link().is_none(), "an unknown state must not send the user to a page guessed from it");
        assert!(!check.ready(), "an unrecognised verdict is not permission to download");
    }

    /// The `message` field of §3: always present, empty for `ok`, and the ONLY content a
    /// `network_error` row has — so it has to survive parsing.
    #[test]
    fn the_technical_message_survives_for_every_state() {
        let check = parse(&download_answer(
            "black-forest-labs/FLUX.2-klein-9B",
            "network_error",
            "  HTTPSConnectionPool: read timed out  ",
        ));
        let row = check.repos.first().expect("one row");
        assert_eq!(row.state, Some(HfDownloadState::NetworkError));
        assert_eq!(row.message, "HTTPSConnectionPool: read timed out");

        // `ok` carries the empty string, and an answer that omits the field entirely
        // still parses — it costs a hover, not the verdict.
        let ok = parse(&json!({
            "repos": { "r": { "state": "ok", "message": "" } }
        }));
        assert_eq!(ok.repos[0].message, "");
        let legacy = parse(&json!({ "repos": { "r": { "state": "ok" } } }));
        assert_eq!(legacy.repos[0].state, Some(HfDownloadState::Ok));
        assert_eq!(legacy.repos[0].message, "");
    }

    /// `ready` decides whether the download button opens, so its two negative cases are
    /// pinned as tightly as its positive one.
    #[test]
    fn only_an_all_ok_answer_is_permission_to_download() {
        let mut check = parse(&json!({
            "repos": {
                "black-forest-labs/FLUX.2-klein-9B": { "state": "ok", "message": "" },
                "ponpoke/flux2-klein-9b-uncensored-text-encoder": { "state": "ok", "message": "" }
            }
        }));
        assert!(check.ready());
        // One refusal is enough: the download needs BOTH repositories the toggle asked for.
        check.repos[1].state = Some(HfDownloadState::NotAccepted);
        assert!(!check.ready());
        // An empty list is "nothing was checked", never "everything is fine".
        assert!(!HfDownloadCheck::<()>::default().ready());
    }

    /// The defect this variant exists for: access and listing are two different network
    /// operations, so every repository can answer `ok` while the plan is missing. That must
    /// render as "size not known" — never as a finished installation.
    #[test]
    fn an_all_ok_check_without_a_plan_is_unknown_and_never_complete() {
        let answer = json!({
            "repos": { "black-forest-labs/FLUX.2-klein-9B": { "state": "ok", "message": "" } },
            "plan": null,
            "plan_error": "ConnectionError: offline"
        });
        let check = parse(&answer);

        // The repositories keep rendering as `ok`: authentication genuinely succeeded, and
        // inventing a gating state here would send the user to fix something intact.
        assert_eq!(check.repos[0].state, Some(HfDownloadState::Ok));
        assert!(check.ready(), "access itself was granted");
        assert_eq!(check.plan, None, "an explicit null must not become a plan");
        assert_eq!(check.plan_error, "ConnectionError: offline");

        // Neither a size nor a completion state.
        assert_eq!(
            hf_download_readiness(Some(&check)),
            HfDownloadReadiness::SizeUnknown,
            "an unpriced download must not be reported as complete on an empty machine"
        );

        // The line rule: a `plan_error` is sent exactly when the ROWS cannot explain the
        // missing plan, and it is what the block shows on hover.
        assert!(
            check.repos.iter().all(|repo| repo.state == Some(HfDownloadState::Ok)) && !check.plan_error.is_empty(),
            "every row reads `ok`, so only `plan_error` can say what happened"
        );

        // A plan of zeros is the OPPOSITE verdict, and the two must not be confusable —
        // this is the exact pair the defect collapsed.
        let complete = parse(&json!({
            "repos": { "black-forest-labs/FLUX.2-klein-9B": { "state": "ok", "message": "" } },
            "plan": { "total_bytes": 40_000_000_000u64, "missing_bytes": 0, "missing_files": 0 }
        }));
        assert_eq!(hf_download_readiness(Some(&complete)), HfDownloadReadiness::Complete);
        assert!(complete.plan_error.is_empty(), "`plan_error` is empty whenever a plan is present");

        // An omitted `plan` key behaves like an explicit null, and a `plan_error` that the
        // backend omitted costs the hover, not the verdict.
        let bare = parse(&json!({ "repos": { "r": { "state": "ok", "message": "" } } }));
        assert_eq!(hf_download_readiness(Some(&bare)), HfDownloadReadiness::SizeUnknown);
        assert!(bare.plan_error.is_empty());

        // And a stray `plan_error` beside a real plan is dropped, so nothing downstream
        // has to re-check the pairing the contract already guarantees.
        let priced = parse(&json!({
            "repos": { "r": { "state": "ok", "message": "" } },
            "plan": { "total_bytes": 40_000_000_000u64, "missing_bytes": 18_000_000_000u64, "missing_files": 3 },
            "plan_error": "should be ignored"
        }));
        assert!(priced.plan_error.is_empty());
        assert_eq!(
            hf_download_readiness(Some(&priced)),
            HfDownloadReadiness::Priced { missing_bytes: 18_000_000_000 },
            "a real plan still prices the download exactly as before"
        );
    }

    /// The four readiness verdicts, and which of them opens «Скачать».
    ///
    /// `SizeUnknown` is the one that must stay OPEN: the listing failed, pressing the
    /// button re-lists, and a closed button would strand the user with no way to retry.
    #[test]
    fn only_a_priced_or_unpriced_check_opens_the_download_button() {
        // No check at all, and a refused one, are both blocked.
        assert_eq!(hf_download_readiness::<()>(None), HfDownloadReadiness::Blocked);
        let refused = parse(&json!({
            "repos": { "r": { "state": "not_accepted", "message": "gated" } },
            "plan": null,
            "plan_error": ""
        }));
        assert_eq!(
            hf_download_readiness(Some(&refused)),
            HfDownloadReadiness::Blocked,
            "a refusal is explained by its own row, not by a missing size"
        );

        // THE FIRST-RUN PATH: a user who has never entered a token. The backend sends a
        // null plan with an EMPTY `plan_error`, and a stray "0.0 GiB" here would be the
        // most misleading of all — it would tell someone with nothing on disk and no token
        // that there is nothing to do.
        let untokened = parse(&json!({
            "repos": {
                "black-forest-labs/FLUX.2-klein-9B": {
                    "state": "no_token",
                    "message": "no token was provided"
                }
            },
            "plan": null,
            "plan_error": ""
        }));
        assert_eq!(untokened.plan, None);
        assert!(untokened.plan_error.is_empty());
        assert_eq!(
            hf_download_readiness(Some(&untokened)),
            HfDownloadReadiness::Blocked,
            "no token must never price or complete the download"
        );
        assert_ne!(hf_download_readiness(Some(&untokened)), HfDownloadReadiness::Complete);
        // No plan means no size line at all, so there is no zero to render; and with an
        // empty `plan_error` no second line is synthesised either — the `no_token` row and
        // its token link are the explanation.
        assert!(
            untokened.plan.is_none() && untokened.plan_error.is_empty(),
            "the row explains it; the block must add nothing"
        );
        assert_eq!(
            untokened.repos[0].state,
            Some(HfDownloadState::NoToken),
            "and the row keeps its own actionable verdict"
        );

        // The startability rule the button reads, kept beside the variants it is derived
        // from so a new variant cannot be added without deciding this.
        for (readiness, startable) in [
            (HfDownloadReadiness::Blocked, false),
            (HfDownloadReadiness::SizeUnknown, true),
            (HfDownloadReadiness::Priced { missing_bytes: 1 }, true),
            (HfDownloadReadiness::Complete, false),
        ] {
            assert_eq!(readiness.startable(), startable, "{readiness:?} must be startable={startable}");
        }
    }

    /// A plan is the number on the button, so an answer without one must stay `None`
    /// rather than becoming a free download of zero bytes.
    #[test]
    fn a_missing_plan_is_not_a_plan_of_zero() {
        let with_plan = parse(&download_answer("r", "ok", ""));
        assert_eq!(
            with_plan.plan,
            Some(HfDownloadPlan {
                total_bytes: 40_000_000_000,
                missing_bytes: 18_000_000_000,
                missing_files: 3,
            })
        );
        assert!(parse(&json!({ "repos": {} })).plan.is_none());
        assert!(parse(&json!({ "plan": {} })).plan.is_none(), "a plan object with neither total is no plan");
        // A complete download is a real answer and must survive as one.
        let done = parse(&json!({
            "repos": { "r": { "state": "ok", "message": "" } },
            "plan": { "total_bytes": 40_000_000_000u64, "missing_bytes": 0, "missing_files": 0 }
        }));
        assert_eq!(done.plan.map(|plan| plan.missing_bytes), Some(0));
    }

    /// An ungated model draws no token row, so a token verdict there is a dead end unless
    /// the panel notices it.
    #[test]
    fn only_the_two_token_verdicts_reopen_the_token_row() {
        let with = |state: &str| {
            Some(parse(&json!({
                "repos": { "black-forest-labs/FLUX.2-klein-4B": { "state": state, "message": "" } }
            })))
        };
        for state in ["no_token", "invalid_token"] {
            assert!(download_check_blames_token(with(state).as_ref()), "{state} is the user's token and nothing else");
        }
        // `not_accepted` is about the repository's conditions and carries its own link;
        // an unknown literal is never guessed at; and nothing is blamed before a check.
        for state in ["ok", "not_accepted", "not_found", "network_error", "a_state_from_the_future"] {
            assert!(!download_check_blames_token(with(state).as_ref()), "{state}");
        }
        assert!(!download_check_blames_token::<()>(None));
    }

    /// The question is the caller's: the parser stamps it verbatim and reads nothing for it.
    #[test]
    fn the_question_is_stamped_verbatim() {
        let check = parse_hf_download_check(&json!({ "repos": {} }), ("asked", 7));
        assert_eq!(check.question, ("asked", 7));
    }
}
