/*
File: cleaning/tools/ai_editor/engines/flux2_klein/download.rs

Purpose:
The FLUX.2 half of «Установить»: what a FLUX.2 `.download.check` / `.download.start` request
carries, what its answer is ABOUT (the checkpoint and the uncensored-encoder toggle), when an
answer still describes the panel, and what a finished download reports. The model-agnostic
half — the per-repository verdicts and their links, the plan, the readiness verdict, the two
calls and the token controls — is `../shared/hf_download.rs`.

Main responsibilities:
- build the request header of both methods (`flux2_download_header`, the ONE place the
  token is put on the wire);
- stamp what an answer describes (`Flux2CheckQuestion`: the toggle the worker sent, the
  checkpoint the backend echoed) and decide whether it still describes the panel
  (`download_check_matches_selection`);
- report an answer whose echoed checkpoint disagrees with the question
  (`download_check_variant_mismatch`);
- repoint the text-encoder path when the toggle flips
  (`flux2_text_encoder_path_after_toggle`);
- run the two calls over the shared ones and parse the FLUX.2 answers.

Key structures:
- `Flux2CheckQuestion`, `Flux2DownloadCheck` (= `HfDownloadCheck<Flux2CheckQuestion>`),
  `Flux2DownloadOutcome`

Key functions:
- `flux2_download_header()`, `check_flux2_download()`, `run_flux2_download()`
- `parse_flux2_download_check()`, `parse_flux2_download_outcome()`
- `download_check_matches_selection()`, `download_check_variant_mismatch()`,
  `flux2_text_encoder_path_after_toggle()`

Notes:
The HF token travels in the request and is NEVER persisted into the settings file. The
drawing of this block (`draw_download_block`, `Flux2DownloadView`) lives in
`ui/install.rs`, over the shared `hf_download_ui.rs`.
*/

use super::*;

/// What one `.download.check` answer is ABOUT: the encoder toggle the asking worker sent
/// and the checkpoint the backend echoed.
///
/// The toggle decides WHICH repositories are checked and what the plan counts, so an answer
/// is shown only while it still describes the toggle in the panel — the same rule
/// `prompt_cached` follows for the prompt it was asked about. Not a wire field: it is the
/// question, not the answer, and the worker stamps it from the value that actually travelled.
///
/// Unlike the toggle, the checkpoint IS echoed by the backend, because the two variants are
/// separate engines and a check answered for the other one prices a different download
/// entirely. It is stamped from the ECHO rather than from the asking worker: a backend that
/// answers about the wrong variant must be detectable here, not papered over. An answer that
/// carries no `variant` reads as 9B, which is the contract's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Flux2CheckQuestion {
    pub(super) uncensored: bool,
    pub(super) variant: Flux2Variant,
}

/// The whole FLUX.2 `.download.check` answer and what it is about.
pub(super) type Flux2DownloadCheck = HfDownloadCheck<Flux2CheckQuestion>;

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
    check.is_some_and(|check| check.question.uncensored == uncensored && check.question.variant == variant)
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
    (check.question.variant != requested).then(|| {
        tf!(
            "cleaning.tools.flux2_klein.download.variant_mismatch_error",
            expected = requested.wire(),
            actual = check.question.variant.wire()
        )
    })
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
    call_hf_download_check(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_CHECK,
        header,
        FLUX2_QUERY_TIMEOUT,
    )
    .map(|response| parse_flux2_download_check(&response))
}

/// Parses a FLUX.2 `.download.check` answer (the shared parse, `parse_hf_download_check`).
///
/// `uncensored` is the QUESTION, not part of the answer: the worker that asked stamps it
/// afterwards, because only it knows which toggle actually travelled, so it reads `false`
/// here. `variant` IS answered — the contract echoes it back — so it is read here, and an
/// answer that carries none reads as 9B, the contract's default for an absent field.
pub(super) fn parse_flux2_download_check(header: &Value) -> Flux2DownloadCheck {
    let variant = Flux2Variant::from_wire(header.get("variant").and_then(Value::as_str).unwrap_or_default());
    parse_hf_download_check(header, Flux2CheckQuestion { uncensored: false, variant })
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
    progress: &Arc<Mutex<EngineProgress>>,
    claim: &ProgressClaim,
) -> Result<Flux2DownloadOutcome, String> {
    run_hf_download(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_DOWNLOAD_START,
        header,
        FLUX2_RUN_TIMEOUT,
        progress,
        claim,
    )
    .map(|header| parse_flux2_download_outcome(&header))
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

    /// An answer describes ONE encoder choice. Flipping the toggle must stop it being
    /// shown, or the panel would keep claiming access to a repository it never asked about.
    #[test]
    fn a_check_stops_being_shown_when_the_encoder_toggle_flips() {
        let check = Flux2DownloadCheck {
            question: Flux2CheckQuestion { uncensored: true, ..Flux2CheckQuestion::default() },
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
            question: Flux2CheckQuestion { variant: Flux2Variant::Klein4B, ..Flux2CheckQuestion::default() },
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
        // The assertion below reads a UI string, and `t!` / `tf!` / `ms_i18n::lookup`
        // answer against the PROCESS-GLOBAL active catalog: with none installed they
        // degrade to the bare key. Install the reference catalog under the shared lock
        // (the catalog slot is one `ArcSwap`, so tests must serialize on it).
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");
        let check = Flux2DownloadCheck {
            question: Flux2CheckQuestion { variant: Flux2Variant::Klein4B, ..Flux2CheckQuestion::default() },
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

    /// The variant is ECHOED by the backend, so it is read from the answer and not
    /// stamped by the asking worker; an answer that carries none is the contract's 9B.
    #[test]
    fn the_echoed_variant_is_parsed_and_defaults_to_9b() {
        let echoed = parse_flux2_download_check(&json!({
            "repos": {}, "plan": null, "variant": "4b"
        }));
        assert_eq!(echoed.question.variant, Flux2Variant::Klein4B);
        let silent = parse_flux2_download_check(&json!({ "repos": {} }));
        assert_eq!(silent.question.variant, Flux2Variant::Klein9B);
        let bogus = parse_flux2_download_check(&json!({ "variant": "13b" }));
        assert_eq!(bogus.question.variant, Flux2Variant::Klein9B);
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
