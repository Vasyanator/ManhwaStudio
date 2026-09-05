/*
File: cleaning/tools/ai_editor/engines/flux2_klein/download.rs

Purpose:
The «Установить» half of the FLUX.2 klein panel: the pre-flight `.download_check` answer
(what each repository is, whether the token opens it, and what the transfer would cost),
the verdict derived from it, and the `.download` call itself.

Main responsibilities:
- own the download-check shape and the per-repository state vocabulary, including the
  link a state offers (token settings page, or the repository's own page);
- decide whether a check is permission to download (`flux2_download_readiness`) and
  whether a check still describes what the panel asks about — the same checkpoint and the
  same uncensored-encoder toggle (`download_check_matches_selection`);
- report an answer whose echoed checkpoint disagrees with the question
  (`download_check_variant_mismatch`) and an answer that blames the token
  (`download_check_blames_token`), which is what reopens the token row on the ungated
  checkpoint;
- repoint the text-encoder path when that toggle flips
  (`flux2_text_encoder_path_after_toggle`);
- build the request headers and parse the check and outcome answers.

Key structures:
- `Flux2DownloadCheck`, `Flux2DownloadRepo`, `Flux2DownloadPlan`, `Flux2DownloadOutcome`
- `Flux2DownloadState`, `Flux2DownloadLink`, `Flux2DownloadReadiness`
- `Flux2DownloadAction`, `Flux2HfTokenAction`

Key functions:
- `flux2_download_header()`, `check_flux2_download()`, `run_flux2_download()`
- `parse_flux2_download_check()`, `parse_flux2_download_outcome()`
- `flux2_download_readiness()`, `download_check_matches_selection()`,
  `download_check_variant_mismatch()`, `download_check_blames_token()`,
  `flux2_text_encoder_path_after_toggle()`

Notes:
The HF token travels in the request and is NEVER persisted into the settings file. The
drawing of this block (`draw_download_block`, `Flux2DownloadView`) lives in
`ui/install.rs`.
*/

use super::*;

// ---------------------------------------------------------------------------------------
// Model download from Hugging Face (`dev-docs/flux2_model_download.md`)
// ---------------------------------------------------------------------------------------

/// Where a Hugging Face access token is created. The link the `no_token` and
/// `invalid_token` verdicts offer.
pub(super) const FLUX2_HF_TOKEN_SETTINGS_URL: &str = "https://huggingface.co/settings/tokens";

/// Prefix of a repository's own page, where its gating conditions are accepted. The link
/// the `not_accepted` verdict offers, completed with the repository id the backend named.
pub(super) const FLUX2_HF_REPO_URL_PREFIX: &str = "https://huggingface.co/";

/// One repository's access verdict, as `.download.check` reports it.
///
/// The mapping is FIXED by the pinned contract (`dev-docs/flux2_model_download.md` §3) and
/// each state answers a different question for the user, which is the whole reason the
/// backend distinguishes them instead of returning one "no access" error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2DownloadState {
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

impl Flux2DownloadState {
    /// The wire literal of this verdict.
    pub(super) fn wire(self) -> &'static str {
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
    pub(super) fn from_wire(value: &str) -> Option<Self> {
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
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Ok => t!("cleaning.tools.flux2_klein.download.state_ok"),
            Self::NoToken => t!("cleaning.tools.flux2_klein.download.state_no_token"),
            Self::InvalidToken => t!("cleaning.tools.flux2_klein.download.state_invalid_token"),
            Self::NotAccepted => t!("cleaning.tools.flux2_klein.download.state_not_accepted"),
            Self::NotFound => t!("cleaning.tools.flux2_klein.download.state_not_found"),
            Self::NetworkError => t!("cleaning.tools.flux2_klein.download.state_network_error"),
        }
    }

    /// The colour the verdict is drawn in; `None` for a neutral line.
    pub(super) fn color(self) -> Option<Color32> {
        match self {
            Self::Ok => Some(FLUX2_STATUS_OK_COLOR),
            // Amber, not red: nothing is broken, the user simply has a step left to take.
            Self::NoToken | Self::NotAccepted => Some(FLUX2_STATUS_WARN_COLOR),
            Self::InvalidToken | Self::NotFound | Self::NetworkError => {
                Some(FLUX2_STATUS_ERROR_COLOR)
            }
        }
    }

    /// The link that makes this verdict actionable.
    ///
    /// This is the FEATURE: a gated repository fails in three different ways and each one
    /// has a different page behind it. A verdict with no link is one the user cannot click
    /// their way out of.
    pub(super) fn link(self) -> Flux2DownloadLink {
        match self {
            Self::Ok | Self::NotFound | Self::NetworkError => Flux2DownloadLink::None,
            Self::NoToken | Self::InvalidToken => Flux2DownloadLink::TokenSettings,
            Self::NotAccepted => Flux2DownloadLink::RepoPage,
        }
    }
}

/// The page one verdict sends the user to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2DownloadLink {
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
pub(super) struct Flux2DownloadRepo {
    /// The repository id, e.g. `black-forest-labs/FLUX.2-klein-9B`. A backend identifier
    /// and therefore literal — it is also what the repository link is built from.
    pub(super) repo: String,
    /// The parsed verdict; `None` when the backend sent a literal this build does not
    /// know ([`Flux2DownloadState::from_wire`]).
    pub(super) state: Option<Flux2DownloadState>,
    /// The literal the backend actually sent, kept verbatim so an unrecognised one can
    /// still be shown on hover instead of vanishing without trace.
    pub(super) state_wire: String,
    /// The backend's own message about this repository, empty for `ok`.
    ///
    /// It is the TECHNICAL half: `network_error` has no localizable content — only the
    /// transport's own wording says what actually went wrong — so the row shows the
    /// localized state and carries this verbatim on hover and in the log, never in place
    /// of the sentence the user reads.
    pub(super) message: String,
}

impl Flux2DownloadRepo {
    /// The link this row offers — its localized caption and its URL — or `None` when the
    /// verdict offers none.
    ///
    /// Caption and URL are decided together so a row can never show "open the model page"
    /// pointing at the token settings. An unrecognised state offers no link at all.
    pub(super) fn link(&self) -> Option<(String, String)> {
        match self.state?.link() {
            Flux2DownloadLink::None => None,
            Flux2DownloadLink::TokenSettings => Some((
                t!("cleaning.tools.flux2_klein.download.token_settings_link").to_string(),
                FLUX2_HF_TOKEN_SETTINGS_URL.to_string(),
            )),
            Flux2DownloadLink::RepoPage => Some((
                t!("cleaning.tools.flux2_klein.download.repo_page_link").to_string(),
                format!("{FLUX2_HF_REPO_URL_PREFIX}{}", self.repo),
            )),
        }
    }
}

/// What the download would cost, computed by the backend from the same listing the
/// access check already fetched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Flux2DownloadPlan {
    /// Size of the complete file set the current toggle needs.
    pub(super) total_bytes: u64,
    /// Of that, what is missing or empty on disk — the number the button carries.
    pub(super) missing_bytes: u64,
    pub(super) missing_files: u64,
}

/// The whole `.download.check` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2DownloadCheck {
    /// One row per repository the toggle actually needs, in the order the backend listed
    /// them.
    pub(super) repos: Vec<Flux2DownloadRepo>,
    /// `None` when the backend could not price the download — the contract's
    /// `"plan": null`.
    ///
    /// ACCESS AND LISTING ARE TWO DIFFERENT NETWORK OPERATIONS: the token can pass
    /// `auth_check` for every repository while fetching the file listing still fails. An
    /// absent plan therefore means the size is NOT KNOWN, exactly like the absent
    /// `components` of the residency block, and must never collapse into "nothing is
    /// missing" — a zero-valued plan beside `ok` states renders a complete installation on
    /// an empty machine, which is the worst failure this block has.
    pub(super) plan: Option<Flux2DownloadPlan>,
    /// Why the plan is missing, verbatim from the backend. Empty whenever `plan` is
    /// present. The TECHNICAL half, shown on hover beside a localized line, exactly as a
    /// repository's own `message` is.
    pub(super) plan_error: String,
    /// The encoder toggle this answer describes, stamped by the worker that asked.
    ///
    /// The toggle decides WHICH repositories are checked and what the plan counts, so an
    /// answer is shown only while it still describes the toggle in the panel — the same
    /// rule `prompt_cached` follows for the prompt it was asked about. Not a wire field:
    /// it is the question, not the answer.
    pub(super) uncensored: bool,
    /// The checkpoint this answer describes.
    ///
    /// Unlike `uncensored` this one IS echoed by the backend, because the two variants are
    /// separate engines and a check answered for the other one prices a different
    /// download entirely. It is still stamped from the ECHO rather than from the asking
    /// worker: a backend that answers about the wrong variant must be detectable here, not
    /// papered over. An answer that carries no `variant` reads as 9B, which is the
    /// contract's default.
    pub(super) variant: Flux2Variant,
}

/// Whether an access check still describes what the panel is asking about right now:
/// the same checkpoint AND the same encoder toggle.
///
/// Either one changes WHICH repositories are needed and what the plan counts, so an answer
/// about the other combination must stop being shown — the same honesty
/// `prompt_cache_state` applies to the prompt a `.status` answer was asked about. The
/// variant half also catches a backend that answered about a checkpoint nobody asked for,
/// because `.download.check` echoes the field back.
pub(super) fn download_check_matches_selection(
    check: Option<&Flux2DownloadCheck>,
    uncensored: bool,
    variant: Flux2Variant,
) -> bool {
    check.is_some_and(|check| check.uncensored == uncensored && check.variant == variant)
}

/// The message to report when a check answer describes a checkpoint nobody asked about;
/// `None` while the echo agrees.
///
/// [`download_check_matches_selection`] already refuses to SHOW such an answer, which on
/// its own leaves the block dead and silent — the failure mode this exists to prevent. The
/// disagreement is a backend fault, not a stale question: the encoder toggle is stamped by
/// the asking worker and can only go stale, while `variant` is stamped from the ECHO, so a
/// mismatch means the backend answered about the wrong checkpoint (most often one too old
/// to read the field, which reads back as the contract's 9B default).
///
/// The two tokens are wire literals and stay untranslated (`dev-docs/i18n_exclusions.md`
/// §A5).
pub(super) fn download_check_variant_mismatch(
    check: &Flux2DownloadCheck,
    requested: Flux2Variant,
) -> Option<String> {
    (check.variant != requested).then(|| {
        tf!(
            "cleaning.tools.flux2_klein.download.variant_mismatch_error",
            expected = requested.wire(),
            actual = check.variant.wire()
        )
    })
}

/// Whether the check blames the Hugging Face TOKEN for at least one repository.
///
/// Only [`Flux2DownloadState::NoToken`] and [`Flux2DownloadState::InvalidToken`] count:
/// they are the two verdicts nothing but the token can resolve. `NotAccepted` is about the
/// repository's conditions and already carries its own link, and an unrecognised literal is
/// never guessed at.
///
/// It exists for the UNGATED checkpoint, whose panel draws no token row at all: without
/// this the user is handed a verdict about a credential they have no control over anywhere
/// in the application ([`crate::hf_token`] has exactly one UI surface, this block).
pub(super) fn download_check_blames_token(check: Option<&Flux2DownloadCheck>) -> bool {
    check.is_some_and(|check| {
        check.repos.iter().any(|repo| {
            matches!(
                repo.state,
                Some(Flux2DownloadState::NoToken | Flux2DownloadState::InvalidToken)
            )
        })
    })
}

impl Flux2DownloadCheck {
    /// Whether every repository the check covered answered `ok`.
    ///
    /// An EMPTY row list is not "ready": it means the backend named no repository, so
    /// there is nothing to conclude. An unrecognised literal is not "ready" either.
    pub(super) fn ready(&self) -> bool {
        !self.repos.is_empty()
            && self
                .repos
                .iter()
                .all(|repo| repo.state == Some(Flux2DownloadState::Ok))
    }
}

/// What the download controls render for the access check they were given.
///
/// A separate type, and pure, because the difference between its two middle variants is a
/// defect the type system can otherwise not see: an unpriced download and a finished one
/// both have "no missing bytes" and must render as opposites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2DownloadReadiness {
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

/// Decides what the download controls render from the last access check.
///
/// **The ABSENT PLAN decides on its own**, before the repository states are consulted: a
/// `null` plan means no size and no completion state may be drawn, whatever the states say.
/// The backend sends it for all three of "no token", "a repository is inaccessible" and "the
/// listing failed", because a zero-valued plan is the same lie in every one of them — it
/// reads as "nothing left to download", and was observed reporting a complete installation
/// on an empty machine. The states are consulted only afterwards, to decide whether
/// «Скачать» can usefully be pressed.
pub(super) fn flux2_download_readiness(check: Option<&Flux2DownloadCheck>) -> Flux2DownloadReadiness {
    let Some(check) = check else {
        return Flux2DownloadReadiness::Blocked;
    };
    let Some(plan) = check.plan else {
        // Access granted means the listing is the only thing that failed, so a retry is
        // worth offering; anything else is already explained by its own row.
        return if check.ready() {
            Flux2DownloadReadiness::SizeUnknown
        } else {
            Flux2DownloadReadiness::Blocked
        };
    };
    // A plan behind a refusal prices a download that cannot start, so it is not offered.
    if !check.ready() {
        return Flux2DownloadReadiness::Blocked;
    }
    if plan.missing_bytes == 0 {
        Flux2DownloadReadiness::Complete
    } else {
        Flux2DownloadReadiness::Priced {
            missing_bytes: plan.missing_bytes,
        }
    }
}

/// The `.download.start` answer: where the files ended up and what it cost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2DownloadOutcome {
    /// Absolute paths of the three components, empty when the backend named none. Only a
    /// non-empty path is written into the settings, so a partial answer cannot blank a
    /// path the user had configured.
    pub(super) transformer_path: String,
    pub(super) text_encoder_path: String,
    pub(super) vae_path: String,
    pub(super) downloaded_bytes: u64,
    pub(super) skipped_files: u64,
}

/// The one download control the user pressed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2DownloadAction {
    /// Run `.download.check` for the current token and toggle.
    Check,
    /// Run the streaming `.download.start`.
    Start,
    /// Abandon the download in flight and tell the backend to stop.
    Cancel,
}

/// The one secret-store control the user pressed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2HfTokenAction {
    /// Write the field's contents into the OS secret store.
    Save,
    /// Delete the stored token.
    Delete,
}

/// The `text_encoder_path` a flip of the «Расцензуренный энкодер» toggle must leave
/// behind, or `None` when the path must be left exactly as it is.
///
/// The toggle owns only the two directories this download block MANAGES. A path that is
/// empty, or that names the OTHER managed encoder, is repointed — which is what makes
/// flipping the toggle with both encoders on disk cost nothing. Any other path is a
/// deliberate choice of the user's (an encoder they downloaded elsewhere, a shared model
/// tree) and survives the flip untouched; silently rewriting it would lose a value the
/// tool cannot recover.
pub(super) fn flux2_text_encoder_path_after_toggle(
    current: &str,
    uncensored: bool,
    variant: Flux2Variant,
) -> Option<String> {
    let wanted = config::flux2_klein_text_encoder_dir(variant, uncensored)
        .to_string_lossy()
        .to_string();
    let other = config::flux2_klein_text_encoder_dir(variant, !uncensored)
        .to_string_lossy()
        .to_string();
    let current = current.trim();
    if current == wanted {
        return None;
    }
    if current.is_empty() || current == other {
        return Some(wanted);
    }
    None
}

// ---------------------------------------------------------------------------------------
// Model download from Hugging Face
// ---------------------------------------------------------------------------------------

/// Builds the request header of the two download methods.
///
/// The token travels as a per-call REQUEST FIELD and never as an environment variable of
/// the backend process, never inside `params`, and never in a log line — this function is
/// the only place it is put on the wire, so there is exactly one place to audit.
/// An empty token is legal and is what makes the backend answer `no_token` without making
/// a network call — and it is the NORMAL case for a variant whose repository is not gated
/// (`Flux2Variant::requires_hf_token`).
///
/// `variant` selects the checkpoint the call is about (`"9b"` / `"4b"`); the backend
/// defaults to 9B when the field is absent, and refuses `"4b"` together with
/// `uncensored = true`, which is why the panel never offers that pair.
#[must_use]
pub(super) fn flux2_download_header(hf_token: &str, uncensored: bool, variant: Flux2Variant) -> Value {
    json!({
        "hf_token": hf_token,
        "uncensored": uncensored,
        "variant": variant.wire(),
    })
}

/// Runs `.download.check` and returns the per-repository verdicts plus the plan.
///
/// BLOCKING: worker threads only.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable. A repository that refused access is NOT an error — it is a verdict, and
/// the whole point of the call is to tell the six of them apart.
pub(super) fn check_flux2_download(header: Value) -> Result<Flux2DownloadCheck, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (response, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_CHECK,
            header,
            &[],
            FLUX2_QUERY_TIMEOUT,
        )
        .map_err(map_flux2_call_error)?;
    Ok(parse_flux2_download_check(&response))
}

/// Parses a `.download.check` answer.
///
/// Every field is optional and degrades rather than failing: an absent `repos` yields no
/// rows (the block then says nothing was checked), a row whose `state` is missing or
/// unrecognised keeps its literal for the hover, and an absent `plan` stays `None` — never
/// a plan of zero bytes, which would put "0.0 GiB" on the download button.
///
/// `message` is contractually always present and empty for `ok`, but it is read as
/// optional all the same: a missing one costs a hover, while treating its absence as an
/// error would cost the whole verdict.
pub(super) fn parse_flux2_download_check(header: &Value) -> Flux2DownloadCheck {
    let repos = header
        .get("repos")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .map(|(repo, value)| {
                    let state_wire = value
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Flux2DownloadRepo {
                        repo: repo.clone(),
                        state: Flux2DownloadState::from_wire(&state_wire),
                        state_wire,
                        message: value
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
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
        Some(Flux2DownloadPlan {
            total_bytes: total_bytes.unwrap_or(0),
            missing_bytes: missing_bytes.unwrap_or(0),
            missing_files: field("missing_files").unwrap_or(0),
        })
    });
    // Only meaningful without a plan; the contract leaves it absent or empty otherwise, and
    // it is dropped here in that case so nothing downstream has to re-check the pairing.
    let plan_error = if plan.is_some() {
        String::new()
    } else {
        header
            .get("plan_error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    // `uncensored` is the QUESTION, not part of the answer: the worker that asked stamps
    // it, because only it knows which toggle actually travelled. `variant` IS answered —
    // the contract echoes it back — so it is read here, and an answer that carries none
    // reads as 9B, the contract's default for an absent field.
    Flux2DownloadCheck {
        repos,
        plan,
        plan_error,
        uncensored: false,
        variant: Flux2Variant::from_wire(
            header.get("variant").and_then(Value::as_str).unwrap_or_default(),
        ),
    }
}

/// Runs the streaming `.download.start` and returns where the files ended up.
///
/// Streaming, and it drives the SAME bar a generation, a `.prompt_cache.build` and a
/// `.component_action` drive — which is what makes the four mutually exclusive.
/// `generation` is the progress generation claimed on the GUI thread; every write is
/// dropped once a newer operation (or a cancel) has retired it, and the bar is cleared on
/// EVERY exit.
///
/// # Errors
/// Returns a user-facing message when the backend refuses (no access to a gated
/// repository, not enough free space for the plan), when the transfer fails or is
/// cancelled, when it does not know the method, or when it is unreachable. No message
/// carries the token.
pub(super) fn run_flux2_download(
    header: Value,
    progress: &Arc<Mutex<Flux2Progress>>,
    generation: u64,
) -> Result<Flux2DownloadOutcome, String> {
    let outcome = flux2_stream_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_START,
        header,
        &[],
        |id| update_progress(progress, generation, |state| state.cancel_id = Some(id)),
        |frame| publish_progress_frame(progress, generation, frame),
    )
    .map(|(header, _blob)| parse_flux2_download_outcome(&header));
    update_progress(progress, generation, |state| {
        state.active = false;
        state.file = None;
        state.cancel_id = None;
    });
    outcome
}

/// Parses a `.download.start` answer.
///
/// A path the backend did not name comes back EMPTY rather than as an error, and the
/// caller writes only the non-empty ones into the settings: a partial answer must not
/// blank a path the user had configured by hand.
pub(super) fn parse_flux2_download_outcome(header: &Value) -> Flux2DownloadOutcome {
    let paths = header.get("paths");
    let path = |name: &str| {
        paths
            .and_then(|paths| paths.get(name))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let count = |name: &str| header.get(name).and_then(Value::as_u64).unwrap_or(0);
    Flux2DownloadOutcome {
        transformer_path: path("transformer"),
        text_encoder_path: path("text_encoder"),
        vae_path: path("vae"),
        downloaded_bytes: count("downloaded_bytes"),
        skipped_files: count("skipped_files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------------------
    // Model download from Hugging Face (`dev-docs/flux2_model_download.md`)
    // -----------------------------------------------------------------------------------

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

    /// The literal -> verdict mapping is the feature: each of the six states sends the
    /// user somewhere different, and an unknown one must send them nowhere at all.
    #[test]
    fn every_download_state_literal_maps_to_its_own_verdict_and_link() {
        // Round trip first: the wire spelling is what the contract pins, and a typo here
        // would silently turn every verdict into "not known".
        for state in [
            Flux2DownloadState::Ok,
            Flux2DownloadState::NoToken,
            Flux2DownloadState::InvalidToken,
            Flux2DownloadState::NotAccepted,
            Flux2DownloadState::NotFound,
            Flux2DownloadState::NetworkError,
        ] {
            assert_eq!(Flux2DownloadState::from_wire(state.wire()), Some(state));
        }
        // The exact literals, spelled out, because the Python half matches on these.
        for (wire, expected) in [
            ("ok", Flux2DownloadState::Ok),
            ("no_token", Flux2DownloadState::NoToken),
            ("invalid_token", Flux2DownloadState::InvalidToken),
            ("not_accepted", Flux2DownloadState::NotAccepted),
            ("not_found", Flux2DownloadState::NotFound),
            ("network_error", Flux2DownloadState::NetworkError),
        ] {
            assert_eq!(Flux2DownloadState::from_wire(wire), Some(expected));
        }

        // The three states that carry a link, and WHICH link. Getting these two round the
        // wrong way is the failure the states exist to prevent: sending someone whose
        // token is merely invalid to accept conditions they have already accepted.
        let repo = |state: Flux2DownloadState| Flux2DownloadRepo {
            repo: "black-forest-labs/FLUX.2-klein-9B".to_string(),
            state: Some(state),
            state_wire: state.wire().to_string(),
            message: String::new(),
        };
        for state in [
            Flux2DownloadState::NoToken,
            Flux2DownloadState::InvalidToken,
        ] {
            assert_eq!(state.link(), Flux2DownloadLink::TokenSettings);
            let (_caption, url) = repo(state).link().expect("a token state offers a link");
            assert_eq!(url, "https://huggingface.co/settings/tokens");
        }
        assert_eq!(
            Flux2DownloadState::NotAccepted.link(),
            Flux2DownloadLink::RepoPage
        );
        let (_caption, url) = repo(Flux2DownloadState::NotAccepted)
            .link()
            .expect("a gated repo links to its own page");
        assert_eq!(url, "https://huggingface.co/black-forest-labs/FLUX.2-klein-9B");

        // The three that carry none: there is no page that fixes them.
        for state in [
            Flux2DownloadState::Ok,
            Flux2DownloadState::NotFound,
            Flux2DownloadState::NetworkError,
        ] {
            assert_eq!(state.link(), Flux2DownloadLink::None);
            assert!(repo(state).link().is_none());
        }
    }

    /// The degradation rule of D7: a literal this build does not know is "not known", it
    /// keeps the literal for the hover, and it offers no link — never one of the six.
    #[test]
    fn an_unrecognised_download_state_claims_no_verdict_and_keeps_the_literal() {
        assert_eq!(Flux2DownloadState::from_wire("quota_exceeded"), None);
        assert_eq!(Flux2DownloadState::from_wire(""), None);
        assert_eq!(Flux2DownloadState::from_wire("OK"), None);

        let check = parse_flux2_download_check(&download_answer(
            "black-forest-labs/FLUX.2-klein-9B",
            "quota_exceeded",
            "monthly quota exhausted",
        ));
        let row = check.repos.first().expect("one row");
        assert_eq!(row.state, None, "an unknown literal must claim no state");
        assert_eq!(
            row.state_wire, "quota_exceeded",
            "the literal survives for the hover"
        );
        assert_eq!(row.message, "monthly quota exhausted");
        assert!(
            row.link().is_none(),
            "an unknown state must not send the user to a page guessed from it"
        );
        assert!(
            !check.ready(),
            "an unrecognised verdict is not permission to download"
        );
    }

    /// The `message` field of §3: always present, empty for `ok`, and the ONLY content a
    /// `network_error` row has — so it has to survive parsing.
    #[test]
    fn the_technical_message_survives_for_every_state() {
        let check = parse_flux2_download_check(&download_answer(
            "black-forest-labs/FLUX.2-klein-9B",
            "network_error",
            "  HTTPSConnectionPool: read timed out  ",
        ));
        let row = check.repos.first().expect("one row");
        assert_eq!(row.state, Some(Flux2DownloadState::NetworkError));
        assert_eq!(row.message, "HTTPSConnectionPool: read timed out");

        // `ok` carries the empty string, and an answer that omits the field entirely
        // still parses — it costs a hover, not the verdict.
        let ok = parse_flux2_download_check(&json!({
            "repos": { "r": { "state": "ok", "message": "" } }
        }));
        assert_eq!(ok.repos[0].message, "");
        let legacy = parse_flux2_download_check(&json!({ "repos": { "r": { "state": "ok" } } }));
        assert_eq!(legacy.repos[0].state, Some(Flux2DownloadState::Ok));
        assert_eq!(legacy.repos[0].message, "");
    }

    /// `ready` decides whether the download button opens, so its two negative cases are
    /// pinned as tightly as its positive one.
    #[test]
    fn only_an_all_ok_answer_is_permission_to_download() {
        let mut check = parse_flux2_download_check(&json!({
            "repos": {
                "black-forest-labs/FLUX.2-klein-9B": { "state": "ok", "message": "" },
                "ponpoke/flux2-klein-9b-uncensored-text-encoder": { "state": "ok", "message": "" }
            }
        }));
        assert!(check.ready());
        // One refusal is enough: the download needs BOTH repositories the toggle asked for.
        check.repos[1].state = Some(Flux2DownloadState::NotAccepted);
        assert!(!check.ready());
        // An empty list is "nothing was checked", never "everything is fine".
        assert!(!Flux2DownloadCheck::default().ready());
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
        let check = parse_flux2_download_check(&answer);

        // The repositories keep rendering as `ok`: authentication genuinely succeeded, and
        // inventing a gating state here would send the user to fix something intact.
        assert_eq!(check.repos[0].state, Some(Flux2DownloadState::Ok));
        assert!(check.ready(), "access itself was granted");
        assert_eq!(check.plan, None, "an explicit null must not become a plan");
        assert_eq!(check.plan_error, "ConnectionError: offline");

        // Neither a size nor a completion state.
        assert_eq!(
            flux2_download_readiness(Some(&check)),
            Flux2DownloadReadiness::SizeUnknown,
            "an unpriced download must not be reported as complete on an empty machine"
        );

        // The line rule: a `plan_error` is sent exactly when the ROWS cannot explain the
        // missing plan, and it is what the block shows on hover.
        assert!(
            check.repos.iter().all(|repo| repo.state == Some(Flux2DownloadState::Ok))
                && !check.plan_error.is_empty(),
            "every row reads `ok`, so only `plan_error` can say what happened"
        );

        // A plan of zeros is the OPPOSITE verdict, and the two must not be confusable —
        // this is the exact pair the defect collapsed.
        let complete = parse_flux2_download_check(&json!({
            "repos": { "black-forest-labs/FLUX.2-klein-9B": { "state": "ok", "message": "" } },
            "plan": { "total_bytes": 40_000_000_000u64, "missing_bytes": 0, "missing_files": 0 }
        }));
        assert_eq!(
            flux2_download_readiness(Some(&complete)),
            Flux2DownloadReadiness::Complete
        );
        assert!(
            complete.plan_error.is_empty(),
            "`plan_error` is empty whenever a plan is present"
        );

        // An omitted `plan` key behaves like an explicit null, and a `plan_error` that the
        // backend omitted costs the hover, not the verdict.
        let bare = parse_flux2_download_check(
            &json!({ "repos": { "r": { "state": "ok", "message": "" } } }),
        );
        assert_eq!(
            flux2_download_readiness(Some(&bare)),
            Flux2DownloadReadiness::SizeUnknown
        );
        assert!(bare.plan_error.is_empty());

        // And a stray `plan_error` beside a real plan is dropped, so nothing downstream
        // has to re-check the pairing the contract already guarantees.
        let priced = parse_flux2_download_check(&json!({
            "repos": { "r": { "state": "ok", "message": "" } },
            "plan": { "total_bytes": 40_000_000_000u64, "missing_bytes": 18_000_000_000u64, "missing_files": 3 },
            "plan_error": "should be ignored"
        }));
        assert!(priced.plan_error.is_empty());
        assert_eq!(
            flux2_download_readiness(Some(&priced)),
            Flux2DownloadReadiness::Priced {
                missing_bytes: 18_000_000_000
            },
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
        assert_eq!(
            flux2_download_readiness(None),
            Flux2DownloadReadiness::Blocked
        );
        let refused = parse_flux2_download_check(&json!({
            "repos": { "r": { "state": "not_accepted", "message": "gated" } },
            "plan": null,
            "plan_error": ""
        }));
        assert_eq!(
            flux2_download_readiness(Some(&refused)),
            Flux2DownloadReadiness::Blocked,
            "a refusal is explained by its own row, not by a missing size"
        );

        // THE FIRST-RUN PATH: a user who has never entered a token. The backend sends a
        // null plan with an EMPTY `plan_error`, and a stray "0.0 GiB" here would be the
        // most misleading of all — it would tell someone with nothing on disk and no token
        // that there is nothing to do.
        let untokened = parse_flux2_download_check(&json!({
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
            flux2_download_readiness(Some(&untokened)),
            Flux2DownloadReadiness::Blocked,
            "no token must never price or complete the download"
        );
        assert_ne!(
            flux2_download_readiness(Some(&untokened)),
            Flux2DownloadReadiness::Complete
        );
        // No plan means no size line at all, so there is no zero to render; and with an
        // empty `plan_error` no second line is synthesised either — the `no_token` row and
        // its token link are the explanation.
        assert!(
            untokened.plan.is_none() && untokened.plan_error.is_empty(),
            "the row explains it; the block must add nothing"
        );
        assert_eq!(
            untokened.repos[0].state,
            Some(Flux2DownloadState::NoToken),
            "and the row keeps its own actionable verdict"
        );

        // The startability rule the button reads, kept beside the variants it is derived
        // from so a new variant cannot be added without deciding this.
        for (readiness, startable) in [
            (Flux2DownloadReadiness::Blocked, false),
            (Flux2DownloadReadiness::SizeUnknown, true),
            (Flux2DownloadReadiness::Priced { missing_bytes: 1 }, true),
            (Flux2DownloadReadiness::Complete, false),
        ] {
            let actual = match readiness {
                Flux2DownloadReadiness::Priced { .. } | Flux2DownloadReadiness::SizeUnknown => true,
                Flux2DownloadReadiness::Blocked | Flux2DownloadReadiness::Complete => false,
            };
            assert_eq!(actual, startable, "{readiness:?} must be startable={startable}");
        }
    }

    /// A plan is the number on the button, so an answer without one must stay `None`
    /// rather than becoming a free download of zero bytes.
    #[test]
    fn a_missing_plan_is_not_a_plan_of_zero() {
        let with_plan = parse_flux2_download_check(&download_answer("r", "ok", ""));
        assert_eq!(
            with_plan.plan,
            Some(Flux2DownloadPlan {
                total_bytes: 40_000_000_000,
                missing_bytes: 18_000_000_000,
                missing_files: 3,
            })
        );
        assert!(parse_flux2_download_check(&json!({ "repos": {} })).plan.is_none());
        assert!(
            parse_flux2_download_check(&json!({ "plan": {} }))
                .plan
                .is_none(),
            "a plan object with neither total is no plan"
        );
        // A complete download is a real answer and must survive as one.
        let done = parse_flux2_download_check(&json!({
            "repos": { "r": { "state": "ok", "message": "" } },
            "plan": { "total_bytes": 40_000_000_000u64, "missing_bytes": 0, "missing_files": 0 }
        }));
        assert_eq!(done.plan.map(|plan| plan.missing_bytes), Some(0));
    }

    /// An answer describes ONE encoder choice. Flipping the toggle must stop it being
    /// shown, or the panel would keep claiming access to a repository it never asked about.
    #[test]
    fn a_check_stops_being_shown_when_the_encoder_toggle_flips() {
        let check = Flux2DownloadCheck {
            uncensored: true,
            ..Flux2DownloadCheck::default()
        };
        assert!(download_check_matches_selection(
            Some(&check),
            true,
            Flux2Variant::Klein9B
        ));
        assert!(!download_check_matches_selection(
            Some(&check),
            false,
            Flux2Variant::Klein9B
        ));
        assert!(!download_check_matches_selection(
            None,
            true,
            Flux2Variant::Klein9B
        ));
    }

    /// The other half of the same staleness rule: an answer about the OTHER checkpoint
    /// prices a different download entirely, so it must stop being shown as well. This is
    /// what makes the echoed `variant` field useful — a backend that answers about the
    /// wrong model is caught here instead of putting the 9B's 35 GB on a 4B button.
    #[test]
    fn a_check_answered_for_the_other_variant_is_stale() {
        let check = Flux2DownloadCheck {
            variant: Flux2Variant::Klein4B,
            ..Flux2DownloadCheck::default()
        };
        assert!(download_check_matches_selection(
            Some(&check),
            false,
            Flux2Variant::Klein4B
        ));
        assert!(!download_check_matches_selection(
            Some(&check),
            false,
            Flux2Variant::Klein9B
        ));
    }

    /// Refusing to SHOW a mismatched answer is only half the rule: on its own it leaves the
    /// block silent and «Скачать» dead, which is indistinguishable from a broken panel. The
    /// disagreement is reported instead, and it names both checkpoints.
    #[test]
    fn a_check_answered_for_the_other_variant_is_reported_not_swallowed() {
        let check = Flux2DownloadCheck {
            variant: Flux2Variant::Klein4B,
            ..Flux2DownloadCheck::default()
        };
        assert!(
            download_check_variant_mismatch(&check, Flux2Variant::Klein4B).is_none(),
            "an agreeing echo has nothing to report"
        );
        let message = download_check_variant_mismatch(&check, Flux2Variant::Klein9B)
            .expect("a disagreeing echo must be reported");
        assert!(
            message.contains(Flux2Variant::Klein9B.wire()) && message.contains(Flux2Variant::Klein4B.wire()),
            "the message must name what was asked and what was answered: {message}"
        );
    }

    /// The ungated checkpoint draws no token row, so a token verdict there is a dead end
    /// unless the panel notices it: `hf_token` has exactly one UI surface in the whole
    /// application, and it is that row.
    #[test]
    fn only_the_two_token_verdicts_reopen_the_token_row() {
        let with = |state: &str| {
            Some(parse_flux2_download_check(&json!({
                "repos": { "black-forest-labs/FLUX.2-klein-4B": { "state": state, "message": "" } }
            })))
        };
        for state in ["no_token", "invalid_token"] {
            assert!(
                download_check_blames_token(with(state).as_ref()),
                "{state} is the user's token and nothing else"
            );
        }
        // `not_accepted` is about the repository's conditions and carries its own link;
        // an unknown literal is never guessed at; and nothing is blamed before a check.
        for state in ["ok", "not_accepted", "not_found", "network_error", "a_state_from_the_future"] {
            assert!(!download_check_blames_token(with(state).as_ref()), "{state}");
        }
        assert!(!download_check_blames_token(None));
    }

    /// The variant is ECHOED by the backend, so it is read from the answer and not
    /// stamped by the asking worker; an answer that carries none is the contract's 9B.
    #[test]
    fn the_echoed_variant_is_parsed_and_defaults_to_9b() {
        let echoed = parse_flux2_download_check(&json!({
            "repos": {}, "plan": null, "variant": "4b"
        }));
        assert_eq!(echoed.variant, Flux2Variant::Klein4B);
        let silent = parse_flux2_download_check(&json!({ "repos": {} }));
        assert_eq!(silent.variant, Flux2Variant::Klein9B);
        let bogus = parse_flux2_download_check(&json!({ "variant": "13b" }));
        assert_eq!(bogus.variant, Flux2Variant::Klein9B);
    }

    /// The request names the checkpoint, and `"9b"` / `"4b"` are the pinned tokens the
    /// backend matches on.
    #[test]
    fn the_download_header_carries_the_variant() {
        for variant in Flux2Variant::all() {
            let header = flux2_download_header("", false, variant);
            assert_eq!(header["variant"], json!(variant.wire()));
        }
        assert_eq!(
            flux2_download_header("", false, Flux2Variant::Klein9B)["variant"],
            json!("9b")
        );
        assert_eq!(
            flux2_download_header("", false, Flux2Variant::Klein4B)["variant"],
            json!("4b")
        );
    }

    /// The toggle owns the two directories the download block manages and nothing else.
    #[test]
    fn the_encoder_toggle_repoints_only_the_paths_this_block_manages() {
        let variant = Flux2Variant::Klein9B;
        let official = config::flux2_klein_text_encoder_dir(variant, false)
            .to_string_lossy()
            .to_string();
        let uncensored = config::flux2_klein_text_encoder_dir(variant, true)
            .to_string_lossy()
            .to_string();
        assert_ne!(official, uncensored, "the two encoders need separate homes");
        assert!(official.ends_with("text_encoder"));
        assert!(uncensored.ends_with("text_encoder_uncensored"));

        // An empty path is filled in with the encoder the toggle selects.
        assert_eq!(
            flux2_text_encoder_path_after_toggle("", true, variant),
            Some(uncensored.clone())
        );
        assert_eq!(
            flux2_text_encoder_path_after_toggle("   ", false, variant),
            Some(official.clone())
        );
        // The other managed directory is repointed — this is the "both on disk, flipping
        // costs nothing" case of §5.
        assert_eq!(
            flux2_text_encoder_path_after_toggle(&official, true, variant),
            Some(uncensored.clone())
        );
        assert_eq!(
            flux2_text_encoder_path_after_toggle(&uncensored, false, variant),
            Some(official.clone())
        );
        // A path that already names the wanted encoder is left alone rather than rewritten.
        assert_eq!(
            flux2_text_encoder_path_after_toggle(&uncensored, true, variant),
            None
        );
        // And a hand-picked encoder anywhere else SURVIVES: rewriting it would discard a
        // value the tool cannot recover.
        assert_eq!(
            flux2_text_encoder_path_after_toggle("/mnt/models/qwen3-my-own", true, variant),
            None
        );
        // The OTHER variant's managed directory is not one of the two this toggle owns, so
        // it survives untouched — the two engines never repoint each other's paths.
        let other_official = config::flux2_klein_text_encoder_dir(Flux2Variant::Klein4B, false)
            .to_string_lossy()
            .to_string();
        assert_eq!(
            flux2_text_encoder_path_after_toggle(&other_official, true, variant),
            None
        );
    }

    /// The two method names are the wire, and the Python half matches on these exact
    /// strings; a rename on either side has to fail here rather than at runtime.
    #[test]
    fn the_download_methods_carry_their_pinned_names() {
        assert_eq!(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_CHECK,
            "inpaint.flux2_klein.download.check"
        );
        assert_eq!(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_START,
            "inpaint.flux2_klein.download.start"
        );
    }
}
