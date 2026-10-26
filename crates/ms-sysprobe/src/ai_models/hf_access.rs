/*
File: crates/ms-sysprobe/src/ai_models/hf_access.rs

Purpose:
The ONE owner of "may this installation download from that gated Hugging Face repository?"
for the pinned external-model catalog: the Hub's access probe
(`GET {endpoint}/api/models/{repo}/auth-check`) and the classification of its answer — or of
a 401 / 403 met during a transfer — into a typed per-repository state.

Key items:
- `HfAccess`: granted / no token / token rejected / terms not accepted / no such repository /
  unreachable (with an English diagnostic detail).
- `hf_repo_access()`: the production probe with the cached token and the caller's cancel flag
  (native, worker threads only; on wasm always `Unreachable`).
- `HfAccess::refuses_download()`: the one rule for which answers stop a download up front
  (`Unreachable` does not: the transfer itself retries and maps a real refusal).
- `hf_repo_access_with()`: the testable core over an injected `RangeFetcher` + `RetryPolicy`.
- `access_from_status()`: the status -> state rule shared by the probe and by
  `external::map_download_error` (401 / 403 during a transfer).

Notes:
- The token is read from `hf_token::hf_token()`, sent only as the bearer header of the
  auth-check URL (built by `hf_url` on the Hugging Face endpoint, `HF_ENDPOINT` honoured),
  and never logged: the log line names the repository and the state only, `AuthCheckTarget`'s
  `Debug` redacts it, and no `HfAccess` value can carry it (an `Unreachable` detail is a
  `DownloadError` text, which never contains request headers).
- Measured Hub answers: 200 = granted; 401 = no token or an invalid one; 403 (`GatedRepo`) =
  the token's account has not accepted the repository terms; 404 = no such repository.
- Blocking: one small request with a short retry budget (`ACCESS_RETRIES` retries of a
  transient failure); the caller's cancel flag ends the request's backoff wait and is polled
  between reads (a cancelled probe answers `Unreachable`); callers own the worker thread.
*/

use std::sync::atomic::AtomicBool;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use ms_download::resumable::{RangeFetcher, RetryPolicy, UreqFetcher};
#[cfg(not(target_arch = "wasm32"))]
use ms_download::{DownloadError, SmallRequest, fetch_small};

/// Log prefix of every line this module writes.
#[cfg(not(target_arch = "wasm32"))]
const LOG_TAG: &str = "[hf-access]";

/// Retries of a transient failure (connect / read errors, 408 / 429 / 5xx) before the probe
/// reports `Unreachable`; deliberately short, the probe only gates a download.
#[cfg(not(target_arch = "wasm32"))]
const ACCESS_RETRIES: u32 = 2;

/// Largest auth-check answer read; the Hub answers with a tiny JSON body (or none).
#[cfg(not(target_arch = "wasm32"))]
const ACCESS_BODY_CAP: u64 = 64 * 1024;

/// Whether this installation may download from one Hugging Face repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfAccess {
    /// The Hub granted access (public repository, or a token whose account accepted the terms).
    Granted,
    /// The repository needs a token and none is stored (HTTP 401 without a token).
    NoToken,
    /// The stored token is invalid or revoked (HTTP 401 with a token).
    TokenRejected,
    /// The token is valid but its account has not accepted the repository's terms (HTTP 403).
    TermsNotAccepted,
    /// The Hub knows no such repository (HTTP 404).
    RepoNotFound,
    /// The Hub could not be asked: network failure, retries exhausted, any other answer, or no
    /// HTTP stack on this target. `detail` is English diagnostic text without secrets.
    Unreachable { detail: String },
}

impl HfAccess {
    /// Whether this answer stops a download before its first transfer: the Hub DEFINITELY
    /// refused (`NoToken`, `TokenRejected`, `TermsNotAccepted`, `RepoNotFound`). `Granted` and
    /// `Unreachable` do not: an unanswered probe is not a refusal, and the transfer itself
    /// retries transient failures under the full download policy and maps a real 401 / 403 to
    /// an access refusal. The one owner of this rule for every download gate.
    #[must_use]
    pub fn refuses_download(&self) -> bool {
        match self {
            Self::NoToken | Self::TokenRejected | Self::TermsNotAccepted | Self::RepoNotFound => true,
            Self::Granted | Self::Unreachable { .. } => false,
        }
    }
}

impl std::fmt::Display for HfAccess {
    // English diagnostic text for logs and `ExternalModelError`'s `Display`; the UI maps the
    // variants to localized text itself.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Granted => f.write_str("access granted"),
            Self::NoToken => f.write_str("a Hugging Face token is required"),
            Self::TokenRejected => f.write_str("the Hugging Face token was rejected"),
            Self::TermsNotAccepted => f.write_str("the repository terms have not been accepted for this token's account"),
            Self::RepoNotFound => f.write_str("the repository does not exist"),
            Self::Unreachable { detail } => write!(f, "the access check failed: {detail}"),
        }
    }
}

/// The access state an HTTP refusal `status` means, or `None` for a status that is not an
/// access refusal. `token_sent` tells a missing token (401 without one) from a rejected one.
/// Shared by the probe and by the downloader's mapping of a 401 / 403 met mid-transfer.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub(crate) fn access_from_status(status: u16, token_sent: bool) -> Option<HfAccess> {
    match status {
        401 if token_sent => Some(HfAccess::TokenRejected),
        401 => Some(HfAccess::NoToken),
        403 => Some(HfAccess::TermsNotAccepted),
        404 => Some(HfAccess::RepoNotFound),
        _ => None,
    }
}

/// Where the probe of one repository goes and with which token. `bearer` is `Some` only for
/// a non-empty token; `Debug` never prints it.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AuthCheckTarget {
    /// The auth-check URL on the Hugging Face endpoint.
    pub url: String,
    /// The token sent as `Authorization: Bearer …` to that URL only.
    pub bearer: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for AuthCheckTarget {
    // Redacts the token: this value may reach a log or a test failure message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthCheckTarget").field("url", &self.url).field("bearer", &self.bearer.as_ref().map(|_| "<redacted>")).finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl AuthCheckTarget {
    /// The probe of `url` (an auth-check URL from `hf_url`) with `token` (empty = none).
    #[must_use]
    pub(crate) fn new(url: String, token: &str) -> Self {
        Self { url, bearer: Some(token.to_owned()).filter(|token| !token.is_empty()) }
    }
}

/// Asks the Hub whether this installation may download from `repo_id` (`owner/name`), with
/// the cached Hugging Face token when one is stored. Logs one INFO line (repository and
/// state, never the token).
///
/// Blocking: worker threads only (one small request, at most [`ACCESS_RETRIES`] short
/// retries of a transient failure). `cancel` (the caller's job flag) ends the retry wait and
/// is polled between reads; a cancelled probe answers `Unreachable`, so the caller checks its
/// own flag afterwards. A hung connect is still bounded by the fetcher's connect timeout.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn hf_repo_access(repo_id: &str, cancel: &AtomicBool) -> HfAccess {
    let target = AuthCheckTarget::new(super::hf_url::hf_auth_check_url(repo_id), &crate::hf_token::hf_token());
    let token_sent = target.bearer.is_some();
    let fetcher = UreqFetcher::new(target.bearer);
    let retry = RetryPolicy {
        max_retries: ACCESS_RETRIES,
        first_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(4),
        retry_after_cap: Duration::from_secs(10),
        wait_slice: Duration::from_millis(100),
        sleep: &std::thread::sleep,
    };
    let access = hf_repo_access_with(&fetcher, &target.url, token_sent, &retry, cancel);
    ms_log::runtime_log::log_info(access_log_line(repo_id, token_sent, &access));
    access
}

/// Web build: there is no blocking HTTP stack, so the Hub cannot be asked.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn hf_repo_access(_repo_id: &str, _cancel: &AtomicBool) -> HfAccess {
    HfAccess::Unreachable { detail: "no HTTP client on this platform".to_owned() }
}

/// Testable core of [`hf_repo_access`]: one GET of the auth-check `url` through `fetcher`
/// (whose token presence is `token_sent`), transient failures retried per `retry`, `cancel`
/// polled like a download's. 200 -> `Granted`; 401 / 403 / 404 per [`access_from_status`];
/// anything else, a cancel included -> `Unreachable`.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub(crate) fn hf_repo_access_with(fetcher: &dyn RangeFetcher, url: &str, token_sent: bool, retry: &RetryPolicy<'_>, cancel: &AtomicBool) -> HfAccess {
    let request = SmallRequest { url, max_bytes: ACCESS_BODY_CAP };
    match fetch_small(fetcher, retry, cancel, &request, LOG_TAG, "auth-check") {
        Ok(_body) => HfAccess::Granted,
        Err(DownloadError::Http { status: Some(status), detail }) => access_from_status(status, token_sent).unwrap_or(HfAccess::Unreachable { detail }),
        Err(other) => HfAccess::Unreachable { detail: other.to_string() },
    }
}

/// The one log line of a probe: repository, whether a token was sent, and the state.
#[cfg(not(target_arch = "wasm32"))]
fn access_log_line(repo_id: &str, token_sent: bool, access: &HfAccess) -> String {
    format!("{LOG_TAG} access check: repo={repo_id} token_sent={token_sent} state={access}")
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use ms_download::test_support::{ScriptedFetcher, Step};

    const URL: &str = "https://huggingface.co/api/models/owner/gated/auth-check";

    fn no_sleep(_: Duration) {}

    fn policy() -> RetryPolicy<'static> {
        RetryPolicy { max_retries: ACCESS_RETRIES, first_delay: Duration::ZERO, max_delay: Duration::ZERO, retry_after_cap: Duration::ZERO, wait_slice: Duration::from_millis(1), sleep: &no_sleep }
    }

    fn probe(script: &[Step], token_sent: bool) -> (HfAccess, ScriptedFetcher) {
        let fetcher = ScriptedFetcher::new(b"{}", script);
        let access = hf_repo_access_with(&fetcher, URL, token_sent, &policy(), &AtomicBool::new(false));
        (access, fetcher)
    }

    /// A cancelled job's probe makes no request and answers `Unreachable` (not a refusal).
    #[test]
    fn a_cancelled_probe_asks_nothing_and_refuses_nothing() {
        let fetcher = ScriptedFetcher::new(b"{}", &[Step::Status(503, None); 3]);
        let access = hf_repo_access_with(&fetcher, URL, true, &policy(), &AtomicBool::new(true));
        assert!(matches!(access, HfAccess::Unreachable { .. }), "{access:?}");
        assert!(!access.refuses_download());
        assert!(fetcher.urls().is_empty(), "{:?}", fetcher.urls());
    }

    #[test]
    fn only_a_definite_refusal_stops_a_download() {
        for access in [HfAccess::NoToken, HfAccess::TokenRejected, HfAccess::TermsNotAccepted, HfAccess::RepoNotFound] {
            assert!(access.refuses_download(), "{access:?}");
        }
        assert!(!HfAccess::Granted.refuses_download());
        assert!(!HfAccess::Unreachable { detail: "timeout".to_owned() }.refuses_download());
    }

    #[test]
    fn answers_classify_into_access_states() {
        assert_eq!(probe(&[], true).0, HfAccess::Granted);
        assert_eq!(probe(&[], false).0, HfAccess::Granted);
        assert_eq!(probe(&[Step::Status(401, None)], false).0, HfAccess::NoToken);
        assert_eq!(probe(&[Step::Status(401, None)], true).0, HfAccess::TokenRejected);
        assert_eq!(probe(&[Step::Status(403, None)], true).0, HfAccess::TermsNotAccepted);
        assert_eq!(probe(&[Step::Status(404, None)], true).0, HfAccess::RepoNotFound);
        assert!(matches!(probe(&[Step::Status(410, None)], true).0, HfAccess::Unreachable { detail } if detail.contains("410")));
    }

    #[test]
    fn the_probe_asks_the_given_url_once() {
        let (_, fetcher) = probe(&[Step::Status(403, None)], true);
        assert_eq!(fetcher.urls(), vec![URL.to_owned()]);
    }

    #[test]
    fn a_transient_failure_is_retried_within_the_short_budget() {
        let (access, fetcher) = probe(&[Step::Status(503, None)], true);
        assert_eq!(access, HfAccess::Granted);
        assert_eq!(fetcher.urls().len(), 2);
        let (access, fetcher) = probe(&[Step::Status(503, None); 3], true);
        assert!(matches!(&access, HfAccess::Unreachable { detail } if detail.contains("503")), "{access:?}");
        assert_eq!(fetcher.urls().len(), 3, "the first attempt plus {ACCESS_RETRIES} retries");
    }

    #[test]
    fn status_rule_covers_only_access_refusals() {
        assert_eq!(access_from_status(401, false), Some(HfAccess::NoToken));
        assert_eq!(access_from_status(401, true), Some(HfAccess::TokenRejected));
        assert_eq!(access_from_status(403, false), Some(HfAccess::TermsNotAccepted));
        assert_eq!(access_from_status(404, true), Some(HfAccess::RepoNotFound));
        assert_eq!(access_from_status(500, true), None);
        assert_eq!(access_from_status(400, true), None);
    }

    #[test]
    fn the_token_goes_only_to_the_hub_and_is_never_printed() {
        const SECRET: &str = "hf_SecretTokenValue123";
        let at = |endpoint: &str, token: &str| AuthCheckTarget::new(crate::ai_models::hf_url::hf_auth_check_url_at(endpoint, "owner/gated"), token);
        let target = at("https://huggingface.co", SECRET);
        assert_eq!(target.url, URL);
        assert_eq!(target.bearer.as_deref(), Some(SECRET));
        assert!(!format!("{target:?}").contains(SECRET));
        let mirror = at("https://hf-mirror.example", SECRET);
        assert!(mirror.url.starts_with("https://hf-mirror.example/api/models/"), "{mirror:?}");
        assert_eq!(at("https://huggingface.co", "").bearer, None);
        for access in [HfAccess::Granted, HfAccess::NoToken, HfAccess::TokenRejected, HfAccess::TermsNotAccepted, HfAccess::RepoNotFound] {
            let line = access_log_line("owner/gated", true, &access);
            assert!(line.contains("repo=owner/gated") && !line.contains(SECRET), "{line}");
        }
    }
}
