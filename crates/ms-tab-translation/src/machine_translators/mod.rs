/*
FILE OVERVIEW: crates/ms-tab-translation/src/machine_translators/mod.rs
Machine translation backends for Translation tab worker.

Main items:
- `MachineTranslatorBackend`: common backend contract for batch translation; every call
  carries the caller's `MtCall` (rate limiter + cancel + wait reporter, `crate::mt_limits`)
  and answers one typed `MtItemError` per failed text.
- `MtItemError` / `MtItemErrorKind`: a text's failure (`Failed`, `Cancelled`, `CoolingDown`,
  `ServiceUnavailable`), with the localized message the run shows.
- `run_session`: the per-run backend factory: ONE instance per Translation run, service and
  transport (`MtTransport`), so a provider may keep run-scoped state (agent, endpoint memory,
  batching, the browser connection check).
- `google` / `yandex` / `deepl`: concrete service implementations over built-in HTTP; `http`: the
  one owner of the providers' HTTP transport facts (timeouts, redirects, `Retry-After`);
  `browser`: the same services in the user's browser through the extension bridge.

Design notes:
- This module is UI-agnostic and runs only inside background MT worker thread.
- New providers should implement `MachineTranslatorBackend`, expose a `run_session()`
  constructor and be dispatched from `machine_translation.rs` / `run_session` by `MtService`.
*/

// The browser transport (the user's browser through the ManhwaStudio extension); native-only
// like the bridge server.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod browser;
pub mod deepl;
pub mod google;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod http;
pub mod yandex;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ms_config::mt_transport::MtTransport;

use crate::machine_translation::{MtService, whole_minutes_ceil};
use crate::mt_limits::call::{MtAcquireError, MtCall};

/// A machine-translation provider.
pub(crate) trait MachineTranslatorBackend {
    /// Translates `texts` as one batch, returning exactly one result per input text in input
    /// order. `call` is the caller's limiter handle: a provider takes a permit from it before
    /// EVERY HTTP request, and once `call.is_cancelled()` (or an acquire says `Cancelled`) it
    /// sends nothing more and answers `MtItemErrorKind::Cancelled` for every remaining text.
    /// Results may mix kinds in any order: the run loop applies EVERY `Ok` / `Failed` result
    /// of the batch, even after a `Cancelled` / `CoolingDown` one, and only then stops the run
    /// (the first stop-kind result decides how).
    ///
    /// # Errors
    /// The outer `Err` is a setup failure that prevents the whole batch (unsupported language,
    /// web build); a text's own failure is its inner `Err`.
    fn translate_texts(
        &self,
        call: &MtCall<'_>,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String>;

    /// How many texts the run loop should hand to one `translate_texts` call. `1` (the
    /// default) = one call per bubble; a provider that packs several texts into one request
    /// returns its pack size. Values below 1 are treated as 1.
    fn preferred_batch_size(&self) -> usize {
        1
    }
}

/// The backend a Translation run uses for `service` over `transport`: built once per run, used
/// for every text of the run on the run's worker thread (it need not be `Send`). `cancel` is the
/// run's Stop flag (the same one its `MtCall` carries): the browser session hands it to the
/// bridge, which cancels the request in flight; the HTTP providers see Stop through `MtCall`.
#[cfg_attr(target_arch = "wasm32", expect(unused_variables, reason = "only the native browser session takes the Stop flag"))]
pub(crate) fn run_session(service: MtService, transport: MtTransport, cancel: &Arc<AtomicBool>) -> Box<dyn MachineTranslatorBackend> {
    match transport {
        MtTransport::Builtin => builtin_run_session(service),
        #[cfg(not(target_arch = "wasm32"))]
        MtTransport::Browser => Box::new(browser::run_session(service, Arc::clone(cancel))),
        // The web build has no browser transport; the service's web stub fails every batch
        // with its "unavailable on the web" message.
        #[cfg(target_arch = "wasm32")]
        MtTransport::Browser => builtin_run_session(service),
    }
}

/// The built-in HTTP backend of a run of `service`.
fn builtin_run_session(service: MtService) -> Box<dyn MachineTranslatorBackend> {
    match service {
        MtService::Google => google::run_session(),
        MtService::Yandex => yandex::run_session(),
        MtService::Deepl => deepl::run_session(),
    }
}

/// What kind of failure an `MtItemError` is; decides how the run loop reacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MtItemErrorKind {
    /// The text failed; the run goes on with the next text.
    Failed,
    /// The run was stopped before this text was translated.
    Cancelled,
    /// The whole service is throttled for longer than the run may wait: the run stops, the
    /// service is usable again in about `retry_in`.
    CoolingDown { retry_in: Duration },
    /// The transport cannot serve any later text either (the browser extension is not
    /// connected, outdated or disconnected, or the site cannot be driven): the run applies the
    /// batch's other results, then fails with this message.
    ServiceUnavailable,
}

/// One text's failure: a localized message for the user plus its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MtItemError {
    /// Localized, user-facing reason (shown in the run's issue list).
    pub message: String,
    pub kind: MtItemErrorKind,
}

impl MtItemError {
    /// An ordinary per-text failure with an already localized `message`.
    #[must_use]
    pub(crate) fn failed(message: String) -> Self {
        Self { message, kind: MtItemErrorKind::Failed }
    }

    /// The failure of a text whose request could not take a limiter permit of `service`.
    /// `Busy` is an ordinary failure (the next text may succeed); `Cancelled` and
    /// `CoolingDown` keep their kind so the run loop can stop.
    #[must_use]
    pub(crate) fn from_acquire(err: MtAcquireError, service: MtService) -> Self {
        match err {
            MtAcquireError::Cancelled => {
                Self { message: t!("translation.mt.rate_limit.cancelled_error").to_string(), kind: MtItemErrorKind::Cancelled }
            }
            MtAcquireError::CoolingDown { retry_in } => Self {
                message: tf!("translation.mt.rate_limit.cooling_down_error", service = service.title(), minutes = whole_minutes_ceil(retry_in.as_secs())),
                kind: MtItemErrorKind::CoolingDown { retry_in },
            },
            MtAcquireError::Busy => Self::failed(tf!("translation.mt.rate_limit.busy_error", service = service.title())),
        }
    }
}

impl fmt::Display for MtItemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test binaries install no i18n catalog, so the messages are checked only for being set
    // and distinct; their wording lives in the locale files.
    #[test]
    fn acquire_errors_keep_the_kind_the_run_loop_reacts_to() {
        let cancelled = MtItemError::from_acquire(MtAcquireError::Cancelled, MtService::Deepl);
        assert_eq!(cancelled.kind, MtItemErrorKind::Cancelled);

        let retry_in = Duration::from_secs(61);
        let cooling = MtItemError::from_acquire(MtAcquireError::CoolingDown { retry_in }, MtService::Google);
        assert_eq!(cooling.kind, MtItemErrorKind::CoolingDown { retry_in });

        let busy = MtItemError::from_acquire(MtAcquireError::Busy, MtService::Yandex);
        assert_eq!(busy.kind, MtItemErrorKind::Failed);
        assert_eq!(busy.to_string(), busy.message);

        let messages = [&cancelled.message, &cooling.message, &busy.message];
        assert!(messages.iter().all(|message| !message.is_empty()));
        assert!(messages[0] != messages[1] && messages[1] != messages[2] && messages[0] != messages[2]);
    }

    #[test]
    fn run_session_builds_a_backend_for_every_service_and_transport() {
        let cancel = Arc::new(AtomicBool::new(false));
        for &service in MtService::all() {
            for transport in MtTransport::ALL {
                // Building a session does no I/O: the browser one connects on its first call.
                assert!(run_session(service, transport, &cancel).preferred_batch_size() >= 1, "{service:?} {transport:?}");
            }
        }
    }
}
