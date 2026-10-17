/*
File: cleaning/tools/ai_editor/engines/flux3_klein/download.rs

Purpose:
The FLUX.3 half of «Скачать»: what a `.download.check` / `.download.start` request carries
(the token and the weight format), what an answer is ABOUT (the format the backend echoes),
when it still describes the panel, and what a finished download reports. The verdicts, the
plan, the readiness rule, the two calls and the token controls are `../shared/hf_download.rs`.

Key structures:
- `Flux3DownloadCheck` (= `HfDownloadCheck<String>`, the question is the echoed format),
  `Flux3DownloadOutcome`

Key functions:
- `flux3_download_header()`, `check_flux3_download()`, `run_flux3_download()`,
  `parse_flux3_download_outcome()`, `download_check_matches_format()`

Notes:
The repository is gated: an empty token is legal on `check` (the backend answers `no_token`
without a network call) and refused on `start`. The token is put on the wire ONLY by
`flux3_download_header`, never persisted and never logged. The download is PER FORMAT: the
shared files plus the selected format's DiT.
*/

use super::*;

/// The whole `.download.check` answer; its question is the format the backend ECHOED.
pub(super) type Flux3DownloadCheck = HfDownloadCheck<String>;

/// The `.download.start` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux3DownloadOutcome {
    /// The absolute package root the files went into (the backend's `model_root()`).
    pub(super) root: String,
    pub(super) downloaded_bytes: u64,
    pub(super) skipped_files: u64,
}

/// Builds the request header of both download methods: the token and the weight format.
///
/// The ONE place the token is put on the wire, so there is exactly one place to audit.
#[must_use]
pub(super) fn flux3_download_header(hf_token: &str, format: Flux3Format) -> Value {
    json!({ "hf_token": hf_token, "format": format.wire() })
}

/// Whether a check answer still describes the selected format: the plan prices one format's
/// DiT, so an answer about another one must stop being shown.
pub(super) fn download_check_matches_format(check: Option<&Flux3DownloadCheck>, format: Flux3Format) -> bool {
    check.is_some_and(|check| check.question == format.wire())
}

/// Runs `.download.check` (worker threads only) and parses it, the question being the echoed
/// `format` (an answer without the echo matches nothing, so it is never shown as current).
///
/// # Errors
/// The user-facing message of a refused or unreachable call. A refused REPOSITORY is not an
/// error but a verdict.
pub(super) fn check_flux3_download(header: Value) -> Result<Flux3DownloadCheck, String> {
    call_hf_download_check(backend_ipc::protocol::METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK, header, FLUX3_QUERY_TIMEOUT)
        .map(|answer| parse_flux3_download_check(&answer))
}

/// Parses a `.download.check` answer through the shared parser.
pub(super) fn parse_flux3_download_check(answer: &Value) -> Flux3DownloadCheck {
    let format = answer.get("format").and_then(Value::as_str).unwrap_or_default().to_string();
    parse_hf_download_check(answer, format)
}

/// Runs the streaming `.download.start` on a worker, publishing byte progress under `claim`
/// (the shared `run_hf_download` clears the bar on every exit and hands the id to the claim's
/// cancel control).
///
/// # Errors
/// The user-facing message of a refused, cancelled or unreachable call.
pub(super) fn run_flux3_download(header: Value, progress: &Arc<Mutex<EngineProgress>>, claim: &ProgressClaim) -> Result<Flux3DownloadOutcome, String> {
    run_hf_download(backend_ipc::protocol::METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START, header, FLUX3_RUN_TIMEOUT, progress, claim)
        .map(|answer| parse_flux3_download_outcome(&answer))
}

/// Parses a `.download.start` answer; absent fields read as empty / zero.
pub(super) fn parse_flux3_download_outcome(answer: &Value) -> Flux3DownloadOutcome {
    Flux3DownloadOutcome {
        root: answer.get("root").and_then(Value::as_str).unwrap_or_default().to_string(),
        downloaded_bytes: answer.get("downloaded_bytes").and_then(Value::as_u64).unwrap_or(0),
        skipped_files: answer.get("skipped_files").and_then(Value::as_u64).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header carries exactly the token and the format; nothing of the settings.
    #[test]
    fn the_download_header_carries_the_token_and_the_format() {
        let header = flux3_download_header("hf_secret", Flux3Format::Nvfp4);
        assert_eq!(header, json!({ "hf_token": "hf_secret", "format": "nvfp4" }));
    }

    /// The answer shape the backend serves: the echoed format, the gated repo's verdict and a
    /// nullable plan. An answer about another format is stale.
    #[test]
    fn a_check_answer_is_about_the_format_it_echoes() {
        let answer = json!({
            "format": "fp8r",
            "repos": { "black-forest-labs/flux-3-image-klein": { "state": "not_accepted", "message": "403" } },
            "plan": null,
            "plan_error": ""
        });
        let check = parse_flux3_download_check(&answer);
        assert_eq!(check.question, "fp8r");
        assert_eq!(check.repos.len(), 1);
        assert_eq!(check.repos[0].state, Some(HfDownloadState::NotAccepted));
        assert!(check.plan.is_none());
        assert!(download_check_matches_format(Some(&check), Flux3Format::Fp8r));
        assert!(!download_check_matches_format(Some(&check), Flux3Format::Bf16), "another format's plan is stale");
        assert!(!download_check_matches_format(None, Flux3Format::Fp8r));
        let silent = parse_flux3_download_check(&json!({ "repos": {} }));
        assert!(!download_check_matches_format(Some(&silent), Flux3Format::Fp8r), "no echo matches nothing");
        let priced = parse_flux3_download_check(&json!({
            "format": "bf16",
            "repos": { FLUX3_REPO: { "state": "ok", "message": "" } },
            "plan": { "total_bytes": 20_300_000_000u64, "missing_bytes": 11_100_000_000u64, "missing_files": 3 },
            "plan_error": ""
        }));
        assert_eq!(hf_download_readiness(Some(&priced)), HfDownloadReadiness::Priced { missing_bytes: 11_100_000_000 });
    }

    #[test]
    fn a_download_answer_names_the_root() {
        let outcome = parse_flux3_download_outcome(&json!({ "root": "/models/side_models/FLUX.3-image-klein", "downloaded_bytes": 7, "skipped_files": 2 }));
        assert_eq!(outcome.root, "/models/side_models/FLUX.3-image-klein");
        assert_eq!((outcome.downloaded_bytes, outcome.skipped_files), (7, 2));
        assert_eq!(parse_flux3_download_outcome(&json!({})), Flux3DownloadOutcome::default());
    }
}
