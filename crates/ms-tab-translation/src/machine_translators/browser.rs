/*
File: crates/ms-tab-translation/src/machine_translators/browser.rs

Purpose:
Machine translation through the user's own browser: Google / DeepL / Yandex driven by the
ManhwaStudio extension's `translate` module (version 1) over the loopback bridge
(`ms-browser-bridge`). The MT side of the `browser` transport (`ms_config::mt_transport`).

Key items:
- `BrowserTranslateChannel`: the seam to the extension (`check_ready`, `translate`); production
  `BridgeChannel` over `BridgeService::shared()`, tests use a fake.
- `start_app_bridge`: the ONE way this crate starts the app's bridge (platform gate, registered
  app version, `ms_browser_bridge::app::ensure_started_for_app`); `start_app_bridge_in_background`
  runs it on a worker (startup, a toggle to the browser transport).
- `await_ready_peer`: the bounded wait for a serving extension right after a start (pure over an
  injected status source, sleep and clock).
- `BrowserMtSession`: the `MachineTranslatorBackend` of one run (`run_session`) or one detached
  call (`detached_session`).
- `flatten_line_breaks`, `pack_line_batches`, `split_translated_lines`, `single_answer`: the
  pure packing rules; `BROWSER_SIZE_UNIT` is the unit every size here is measured in.
- `signal_for`, `item_error_for`: what a bridge failure means for the limiter and for the run.

Rules:
- Decision D2: line breaks inside a bubble become ONE space (`flatten_line_breaks`); then the
  texts of a call are packed one per line, in order, at most `max_items` (DeepL 10, Google and
  Yandex 20) and the lane's `max_request_chars` per request (newlines counted). Sizes are UTF-16
  code units (`BROWSER_SIZE_UNIT`), the unit of the extension's own `too_long` check
  (`text.length > site.maxChars`), and the lane limits stay below the extension's site limits
  (mirror test `rust_limits_stay_below_the_extension_site_limits`). A text over the limit goes
  alone, cut by `mt_limits::split::split_for_request_in`; its chunks are requested one by one
  and joined back with their separators. Blank texts answer `""` without a request.
- The answer is split by lines (`split_translated_lines`: CRLF / CR / NBSP normalized, empty
  lines dropped) and must have exactly one line per text; otherwise that batch is re-sent one
  text per request. A packed batch the extension refuses as `too_long` is re-sent one text per
  request too. Every re-sent text checks Stop first (`BrowserMtSession::send`), and the texts
  answered before a Stop keep their translations in the call's results (the run loop decides
  whether a cancelled call's results are applied: today it discards them).
- ONE limiter permit (`MtCall::acquire`, the service's browser route) per bridge request.
  Signals: remote `rate_limited` -> `RateLimited`, remote or app-side `attention_timeout` ->
  `Blocked`, everything else -> `Neutral`. After a limit signal the cooled lane makes this and
  every later text `CoolingDown` (the run stops with "try again in N min").
- `MtItemErrorKind::ServiceUnavailable` (the run applies its batch, then fails with the
  message): no extension connected, outdated, disconnected, the bridge could not start, and the
  remote codes that hold for every later text too (`unknown_module`, `unknown_method`,
  `bad_params`, `site_changed`, `unsupported_language`). A stop-kind error (also `Cancelled`,
  `CoolingDown`) sends nothing more: every later text of the call answers that same error.
- A session checks the connection once, before its first request (`check_ready`), without
  taking a permit. Waiting rule (`await_ready_peer`): when the bridge was NOT listening before
  that check (this call started it, or a start is still binding) or has been listening for less
  than `READY_WAIT` (35 s: the extension's reconnect backoff caps at 30 s), it waits for a
  serving extension until the listener is `READY_WAIT` old, polling the bridge status every
  `READY_POLL` and honouring Stop; a detached call waits at most what is left of its 30 s. The
  listen-start time is this crate's first sighting (`LISTENING_SINCE`, `note_listen_state`).
  When the bridge has listened for longer, a missing or unusable extension fails the call at
  once. A listener failure or a connected extension without the module version never waits.
- Languages: each service's own normalizer (`normalize_google_lang`, `normalize_deepl_lang`,
  `normalize_yandex_lang`) owns the code spelling; DeepL's API spelling is lower-cased for its web
  page (`AUTO` -> `auto`, the module's auto token).
- Timings: a run's request waits up to `RUN_REQUEST_TIMEOUT` (90 s) plus attention states of up
  to `RUN_ATTENTION_TIMEOUT` (10 min, the user solving a captcha); the detached call (Cleaning
  prompt translator) has `DETACHED_OVERALL` (30 s) for everything. Stop sets the run's cancel
  flag, which the bridge sees within 250 ms and which cancels the in-flight request.
- Attention events (`attention` / `attention_cleared`) are forwarded to
  `MtCall::report_attention`, so the panel can say "solve the check in the browser tab".
- Logs carry the service, sizes and the error, never a text or a translation. A user Stop is
  logged at info level, not as a failure.

Notes:
- Native-only (the bridge server is); the web build has no browser transport.
- Every text the user sees is localized (`translation.mt.browser.*` keys, plus the bridge's own
  `browser_bridge.*` texts as the reason).
*/

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ms_browser_bridge::{BridgeError, BridgeStatus, ErrorCode, ListenState};
use ms_log::runtime_log;
use web_time::Instant;

use super::deepl::normalize_deepl_lang;
use super::google::{LangRole, normalize_google_lang};
use super::yandex::normalize_yandex_lang;
use super::{MachineTranslatorBackend, MtItemError, MtItemErrorKind};
use crate::machine_translation::MtService;
use crate::mt_limits::call::{MtAcquireError, MtCall, MtSignal};
use crate::mt_limits::split::{SizeUnit, join_chunks, split_for_request_in};
use crate::mt_limits::table::{MtLane, MtRoute, lane_limits};

/// The extension module this provider speaks (`browser-extension/src/modules/translate`).
pub(crate) const TRANSLATE_MODULE: &str = "translate";
/// The exact module version this provider speaks (`TRANSLATE_MODULE_VERSION` in `types.ts`).
pub(crate) const TRANSLATE_MODULE_VERSION: u32 = 1;
/// The module's one method.
const TRANSLATE_METHOD: &str = "translate";

/// Longest a run's request may take, excluding attention states.
pub(crate) const RUN_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// Longest single attention state (captcha shown to the user) a run's request waits out.
pub(crate) const RUN_ATTENTION_TIMEOUT: Duration = Duration::from_mins(10);
/// Everything a detached call (the Cleaning prompt translator) may take, attention included.
pub(crate) const DETACHED_OVERALL: Duration = Duration::from_secs(30);
/// Longest `check_ready` waits for a serving extension after the bridge was not yet listening:
/// the extension's reconnect backoff caps at 30 s, plus a margin for its handshake.
pub(crate) const READY_WAIT: Duration = Duration::from_secs(35);
/// How often that wait re-reads the bridge status (also how fast it notices Stop).
const READY_POLL: Duration = Duration::from_millis(250);

/// The unit of every request size on the browser lanes: the extension refuses a request whose
/// JavaScript `String.length` (UTF-16 code units) is over its site limit.
pub(crate) const BROWSER_SIZE_UNIT: SizeUnit = SizeUnit::Utf16;

/// How long a session's requests may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrowserTimings {
    /// Per-request deadline, excluding attention states.
    pub(crate) request: Duration,
    /// Longest single attention state.
    pub(crate) attention: Duration,
    /// Budget of the whole session from its first call (`None` = per request only).
    pub(crate) overall: Option<Duration>,
}

impl BrowserTimings {
    /// A Translation run's timings.
    pub(crate) const RUN: Self = Self { request: RUN_REQUEST_TIMEOUT, attention: RUN_ATTENTION_TIMEOUT, overall: None };
    /// The detached caller's timings: 30 s for everything.
    pub(crate) const DETACHED: Self = Self { request: DETACHED_OVERALL, attention: DETACHED_OVERALL, overall: Some(DETACHED_OVERALL) };
}

/// One `translate` call of the extension module (`{site, source, target, text}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BrowserTranslateRequest<'a> {
    /// The site key: `MtService::key()` (`google` / `deepl` / `yandex`), the module's site keys.
    pub(crate) site: &'static str,
    /// Source language in the site's spelling, or `auto`.
    pub(crate) source: &'a str,
    /// Target language in the site's spelling.
    pub(crate) target: &'a str,
    /// One request's text: packed texts one per line.
    pub(crate) text: &'a str,
}

/// How one channel call waits.
pub(crate) struct BrowserCallOptions<'a> {
    /// Deadline excluding attention states.
    pub(crate) timeout: Duration,
    /// Longest single attention state.
    pub(crate) attention_timeout: Duration,
    /// The run's Stop flag; `None` for a detached call.
    pub(crate) cancel: Option<&'a AtomicBool>,
    /// Told `true` when the browser tab needs the user, `false` when it no longer does; called
    /// on the requesting thread.
    pub(crate) on_attention: &'a dyn Fn(bool),
}

/// How long `check_ready` may wait for an extension (see `await_ready_peer`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadyWait<'a> {
    /// Longest wait when the bridge was not listening before the check.
    pub(crate) max_wait: Duration,
    /// The run's Stop flag; `None` for a detached call.
    pub(crate) cancel: Option<&'a AtomicBool>,
}

/// The seam between the MT provider and the browser extension.
pub(crate) trait BrowserTranslateChannel {
    /// Starts the bridge if needed and answers whether a `translate` request can be served (an
    /// extension that announces the module version is connected). BLOCKS up to `wait.max_wait`
    /// only when the bridge was not listening before this call (`await_ready_peer`); worker
    /// threads only.
    ///
    /// # Errors
    /// Why not: `Unavailable`, `NotConnected`, `Outdated`, `CapabilityMissing`, a start failure,
    /// `Cancelled` (Stop during the wait).
    fn check_ready(&self, wait: &ReadyWait<'_>) -> Result<(), BridgeError>;

    /// Runs one `translate` request; BLOCKS until the answer, a deadline, a cancel or a
    /// disconnect. Returns the module result's `text`.
    ///
    /// # Errors
    /// Every `BridgeError` of the request.
    fn translate(&self, request: &BrowserTranslateRequest<'_>, options: &BrowserCallOptions<'_>) -> Result<String, BridgeError>;
}

/// The production channel: the process-wide bridge, started through
/// `ms_browser_bridge::app::ensure_started_for_app`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BridgeChannel;

impl BrowserTranslateChannel for BridgeChannel {
    fn check_ready(&self, wait: &ReadyWait<'_>) -> Result<(), BridgeError> {
        let service = ms_browser_bridge::BridgeService::shared();
        // Read BEFORE the start: only a bridge that was not listening yet, or only recently,
        // is worth waiting for.
        let listening_for = listening_duration(&service.status(), Instant::now());
        start_app_bridge()?;
        let status = || {
            let snapshot = service.status();
            note_listen_state(&snapshot);
            snapshot
        };
        await_ready_peer(listening_for, wait, status, &std::thread::sleep, &Instant::now)
    }

    fn translate(&self, request: &BrowserTranslateRequest<'_>, options: &BrowserCallOptions<'_>) -> Result<String, BridgeError> {
        let params = serde_json::json!({"site": request.site, "source": request.source, "target": request.target, "text": request.text});
        let on_event: &dyn Fn(&ms_browser_bridge::BridgeEvent) = &|event: &ms_browser_bridge::BridgeEvent| match event.kind {
            ms_browser_bridge::EventKind::Attention => (options.on_attention)(true),
            ms_browser_bridge::EventKind::AttentionCleared => (options.on_attention)(false),
            ms_browser_bridge::EventKind::Progress => {}
        };
        let bridge_request = ms_browser_bridge::BridgeRequest { module: TRANSLATE_MODULE, module_version: TRANSLATE_MODULE_VERSION, method: TRANSLATE_METHOD, params: &params };
        let bridge_options =
            ms_browser_bridge::RequestOptions { timeout: options.timeout, attention_timeout: options.attention_timeout, cancel: options.cancel, on_event: Some(on_event) };
        let result = ms_browser_bridge::BridgeService::shared().request(&bridge_request, &bridge_options)?;
        result.get("text").and_then(serde_json::Value::as_str).map(str::to_owned).ok_or_else(|| BridgeError::Protocol { detail: "the translate result has no text field".to_owned() })
    }
}

/// When this crate first saw the bridge listening (process-wide; the server lives for the
/// process). The bridge's status carries no listen-start time, so every status read here records
/// it (`note_listen_state`): the background start worker, `check_ready` and the extension
/// panel's per-frame read. A first sighting long after the real start makes one check wait
/// longer than needed, never fail early.
static LISTENING_SINCE: OnceLock<Instant> = OnceLock::new();

/// Records the first moment `status` shows the bridge listening (no-op afterwards). GUI-safe.
pub(crate) fn note_listen_state(status: &BridgeStatus) {
    if matches!(status.listen, ListenState::Listening { .. }) {
        LISTENING_SINCE.get_or_init(Instant::now);
    }
}

/// How long the bridge has been listening at `now` per [`LISTENING_SINCE`] (recording `now`
/// as the start if this is the first sighting); `None` when `status` is not listening.
fn listening_duration(status: &BridgeStatus, now: Instant) -> Option<Duration> {
    matches!(status.listen, ListenState::Listening { .. }).then(|| now.saturating_duration_since(*LISTENING_SINCE.get_or_init(|| now)))
}

/// Starts the app's bridge: the platform gate, the registered app version, then
/// `ms_browser_bridge::app::ensure_started_for_app` (which may re-unpack a stale extension
/// folder). The one start path of this crate. Worker threads only; idempotent and cheap after
/// the first success.
///
/// # Errors
/// `Unavailable` on a platform without the extension; the start error of the bridge.
pub(crate) fn start_app_bridge() -> Result<(), BridgeError> {
    if !ms_platform::PlatformFeature::BrowserExtension.is_available() {
        return Err(BridgeError::Unavailable);
    }
    let version = ms_browser_bridge::app::registered_app_version().unwrap_or_else(|| {
        runtime_log::log_warn("[MT] browser bridge started before the app version was registered; the extension sees \"unregistered\"");
        "unregistered"
    });
    ms_browser_bridge::app::ensure_started_for_app(&ms_config::data_dir(), version)
}

/// Runs [`start_app_bridge`] on a detached worker, so the extension can connect before the
/// first browser-transport request (a failure is logged; the next request retries the start).
/// `reason` names the caller in the log. The worker then watches the status (every 50 ms, at
/// most `LISTEN_WATCH`) until the server listens, so [`LISTENING_SINCE`] holds the real start.
/// GUI-safe.
pub(crate) fn start_app_bridge_in_background(reason: &'static str) {
    /// Longest the start worker watches for the listener (binding takes milliseconds).
    const LISTEN_WATCH: Duration = Duration::from_secs(10);
    let spawned = ms_thread::Builder::new().name("ms-mt-bridge-start".to_owned()).spawn(move || {
        runtime_log::log_info(format!("[MT] starting the browser bridge ({reason})"));
        if let Err(err) = start_app_bridge() {
            runtime_log::log_warn(format!("[MT] the browser bridge did not start ({reason}): {err}; the first browser request retries"));
            return;
        }
        let watch_end = Instant::now() + LISTEN_WATCH;
        loop {
            let status = ms_browser_bridge::BridgeService::shared().status();
            note_listen_state(&status);
            if matches!(status.listen, ListenState::Listening { .. } | ListenState::Failed { .. }) || Instant::now() >= watch_end {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    // Detached on purpose: the outcome is only logged.
    if let Err(err) = spawned.map(|_detached| ()) {
        runtime_log::log_error(format!("[MT] could not spawn the browser bridge start worker ({reason}): {err}"));
    }
}

/// The waiting rule of `check_ready`, after the start: answers as soon as a peer serves the
/// module (`select_peer`). `listening_for` is how long the bridge had been listening when the
/// check began (`None` = not listening: this check started it, or a start is still binding).
/// Waits — re-reading `status` every `READY_POLL` through `sleep` — only while the answer is
/// `NotConnected` (nobody connected yet) or `Outdated` (an old extension the reload watcher is
/// about to reload) AND the bridge is young: not listening, or listening for less than
/// `READY_WAIT` (the extension's reconnect backoff may not have fired yet). The wait lasts at
/// most `wait.max_wait` and at most until the listener is `READY_WAIT` old, measured on `now`.
///
/// # Errors
/// A listener failure (at once); `Cancelled` when `wait.cancel` is set during the wait; else
/// the last `select_peer` error (`NotConnected`, `Outdated`, `CapabilityMissing`).
pub(crate) fn await_ready_peer(
    listening_for: Option<Duration>,
    wait: &ReadyWait<'_>,
    mut status: impl FnMut() -> BridgeStatus,
    sleep: &dyn Fn(Duration),
    now: &dyn Fn() -> Instant,
) -> Result<(), BridgeError> {
    let started = now();
    // An old listener: the extension had its chance to connect, so the check fails fast.
    let max_wait = match listening_for {
        None => Some(wait.max_wait),
        Some(age) => (age < READY_WAIT).then(|| wait.max_wait.min(READY_WAIT - age)),
    };
    let mut announced = false;
    loop {
        let snapshot = status();
        if let ListenState::Failed { error } = snapshot.listen {
            return Err(error);
        }
        let err = match ms_browser_bridge::api::select_peer(&snapshot.peers, TRANSLATE_MODULE, TRANSLATE_MODULE_VERSION) {
            Ok(_index) => {
                if announced {
                    runtime_log::log_info(format!("[MT] browser extension connected after {} ms", now().saturating_duration_since(started).as_millis()));
                }
                return Ok(());
            }
            Err(err) => err,
        };
        let Some(max_wait) = max_wait.filter(|_| matches!(err, BridgeError::NotConnected | BridgeError::Outdated)) else {
            return Err(err);
        };
        if wait.cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(BridgeError::Cancelled);
        }
        if now().saturating_duration_since(started) >= max_wait {
            runtime_log::log_warn(format!("[MT] no browser extension served translation within {} s: {err}", max_wait.as_secs()));
            return Err(err);
        }
        if !announced {
            runtime_log::log_info(format!("[MT] the browser bridge is young; waiting up to {} s for the extension ({err})", max_wait.as_secs()));
            announced = true;
        }
        sleep(READY_POLL);
    }
}

/// The browser lane's limiter route of `service`.
const fn route_of(service: MtService) -> MtRoute {
    match service {
        MtService::Google => MtRoute::GoogleBrowser,
        MtService::Deepl => MtRoute::DeeplBrowser,
        MtService::Yandex => MtRoute::YandexBrowser,
    }
}

/// Most texts one request carries.
const fn max_items(service: MtService) -> usize {
    match service {
        MtService::Deepl => 10,
        MtService::Google | MtService::Yandex => 20,
    }
}

/// Longest request text (UTF-16 code units, `BROWSER_SIZE_UNIT`) of `service`'s browser lane.
fn max_request_chars(service: MtService) -> usize {
    // Every browser lane has a limit (`table.rs`); `usize::MAX` would only matter for a lane
    // added without one, which then simply packs by item count.
    lane_limits(MtLane::browser(service)).max_request_chars.unwrap_or(usize::MAX)
}

/// The source / target codes in `service`'s web-page spelling.
///
/// # Errors
/// A localized message for a code the service does not offer (Google's table).
fn site_languages(service: MtService, source: &str, target: &str) -> Result<(String, String), String> {
    match service {
        MtService::Google => {
            let source = normalize_google_lang(source, LangRole::Source).map_err(|err| err.user_message())?;
            let target = normalize_google_lang(target, LangRole::Target).map_err(|err| err.user_message())?;
            Ok((source.to_owned(), target.to_owned()))
        }
        // The DeepL page takes the API codes in lower case (`#ja/ru/`), and `auto` is the
        // module's auto token.
        MtService::Deepl => Ok((normalize_deepl_lang(source, true).to_ascii_lowercase(), normalize_deepl_lang(target, false).to_ascii_lowercase())),
        MtService::Yandex => Ok((normalize_yandex_lang(source, true), normalize_yandex_lang(target, false))),
    }
}

/// Whether `c` ends a line for the flattening rule.
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}' | '\u{85}')
}

/// Decision D2: every run of line breaks (CRLF, CR, LF, Unicode line / paragraph separators)
/// becomes ONE space; the result is trimmed. A blank text becomes `""`.
#[must_use]
pub(crate) fn flatten_line_breaks(text: &str) -> String {
    let mut flat = String::with_capacity(text.len());
    let mut in_break = false;
    for c in text.chars() {
        if is_line_break(c) {
            if !in_break {
                flat.push(' ');
                in_break = true;
            }
        } else {
            flat.push(c);
            in_break = false;
        }
    }
    flat.trim().to_owned()
}

/// One planned request group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LineBatch {
    /// These texts (indices into the call's texts), one per line in one request.
    Lines(Vec<usize>),
    /// This text is over the request limit: cut into chunks, one request each.
    Split(usize),
}

/// Packs `texts` (`(index, flattened non-blank text)`, in order) into requests: consecutive
/// texts, at most `max_items` per request and at most `max_size` UTF-16 code units
/// (`BROWSER_SIZE_UNIT`) including the joining newlines; a text longer than `max_size` is a
/// `Split` of its own. Order is kept.
#[must_use]
pub(crate) fn pack_line_batches(texts: &[(usize, &str)], max_size: usize, max_items: usize) -> Vec<LineBatch> {
    let max_items = max_items.max(1);
    let mut batches = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_chars = 0usize;
    for &(index, text) in texts {
        let chars = BROWSER_SIZE_UNIT.measure(text);
        if chars > max_size {
            if !current.is_empty() {
                batches.push(LineBatch::Lines(std::mem::take(&mut current)));
                current_chars = 0;
            }
            batches.push(LineBatch::Split(index));
            continue;
        }
        // A newline joins it to the texts already in the batch.
        let added = if current.is_empty() { chars } else { chars.saturating_add(1) };
        if !current.is_empty() && (current.len() >= max_items || current_chars.saturating_add(added) > max_size) {
            batches.push(LineBatch::Lines(std::mem::take(&mut current)));
            current_chars = 0;
            current.push(index);
            current_chars += chars;
            continue;
        }
        current.push(index);
        current_chars = current_chars.saturating_add(added);
    }
    if !current.is_empty() {
        batches.push(LineBatch::Lines(current));
    }
    batches
}

/// The non-empty lines of a site answer, trimmed: CRLF / CR become LF, NBSP becomes a space.
fn answer_lines(output: &str) -> Vec<String> {
    output.replace("\r\n", "\n").replace('\r', "\n").replace('\u{a0}', " ").split('\n').map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned).collect()
}

/// Splits a packed answer into exactly `expected` translations (one per non-empty line);
/// `None` when the line count differs.
#[must_use]
pub(crate) fn split_translated_lines(output: &str, expected: usize) -> Option<Vec<String>> {
    let lines = answer_lines(output);
    (lines.len() == expected).then_some(lines)
}

/// The translation of ONE text's request: its non-empty lines joined by a space (the text had
/// no line break, decision D2); `None` for a blank answer.
#[must_use]
pub(crate) fn single_answer(output: &str) -> Option<String> {
    let lines = answer_lines(output);
    (!lines.is_empty()).then(|| lines.join(" "))
}

/// The limiter signal of a failed request.
#[must_use]
pub(crate) fn signal_for(err: &BridgeError) -> MtSignal {
    match err {
        BridgeError::Remote { code, retry_after, .. } => match code {
            ErrorCode::RateLimited => MtSignal::RateLimited { retry_after: *retry_after },
            ErrorCode::AttentionTimeout => MtSignal::Blocked { retry_after: None },
            ErrorCode::Cancelled
            | ErrorCode::Timeout
            | ErrorCode::UnknownModule
            | ErrorCode::UnknownMethod
            | ErrorCode::BadParams
            | ErrorCode::Busy
            | ErrorCode::SiteChanged
            | ErrorCode::TabClosed
            | ErrorCode::UnsupportedLanguage
            | ErrorCode::TooLong
            | ErrorCode::Internal => MtSignal::Neutral,
        },
        // The user did not solve the check in time: the site is challenging this browser.
        BridgeError::AttentionTimeout => MtSignal::Blocked { retry_after: None },
        BridgeError::Unavailable
        | BridgeError::NotStarted
        | BridgeError::NotConnected
        | BridgeError::Outdated
        | BridgeError::CapabilityMissing { .. }
        | BridgeError::Timeout
        | BridgeError::Cancelled
        | BridgeError::Disconnected
        | BridgeError::Protocol { .. }
        | BridgeError::PairingIo { .. }
        | BridgeError::OwnedByOtherProcess
        | BridgeError::Bind { .. }
        | BridgeError::System { .. } => MtSignal::Neutral,
    }
}

/// What a failed request means for its text (and, for a stop kind, the rest of the run).
#[must_use]
pub(crate) fn item_error_for(service: MtService, err: &BridgeError) -> MtItemError {
    let service_unavailable = |message: String| MtItemError { message, kind: MtItemErrorKind::ServiceUnavailable };
    let with_reason = || tf!("translation.mt.browser.request_error", service = service.title(), reason = err);
    match err {
        BridgeError::NotStarted | BridgeError::NotConnected => service_unavailable(tf!("translation.mt.browser.not_connected_error", service = service.title())),
        BridgeError::Outdated | BridgeError::CapabilityMissing { .. } => service_unavailable(tf!("translation.mt.browser.outdated_error", service = service.title())),
        BridgeError::Disconnected => service_unavailable(tf!("translation.mt.browser.disconnected_error", service = service.title())),
        BridgeError::Unavailable | BridgeError::PairingIo { .. } | BridgeError::OwnedByOtherProcess | BridgeError::Bind { .. } | BridgeError::System { .. } => service_unavailable(with_reason()),
        BridgeError::Cancelled => MtItemError::from_acquire(MtAcquireError::Cancelled, service),
        BridgeError::Timeout | BridgeError::AttentionTimeout | BridgeError::Protocol { .. } => MtItemError::failed(with_reason()),
        BridgeError::Remote { code, .. } => match code {
            ErrorCode::Cancelled => MtItemError::from_acquire(MtAcquireError::Cancelled, service),
            ErrorCode::UnknownModule | ErrorCode::UnknownMethod | ErrorCode::BadParams | ErrorCode::SiteChanged | ErrorCode::UnsupportedLanguage => service_unavailable(with_reason()),
            ErrorCode::Timeout | ErrorCode::Busy | ErrorCode::AttentionTimeout | ErrorCode::RateLimited | ErrorCode::TabClosed | ErrorCode::TooLong | ErrorCode::Internal => {
                MtItemError::failed(with_reason())
            }
        },
    }
}

/// Whether `kind` ends the call: nothing more is sent after it.
const fn is_stop_kind(kind: MtItemErrorKind) -> bool {
    match kind {
        MtItemErrorKind::Failed => false,
        MtItemErrorKind::Cancelled | MtItemErrorKind::CoolingDown { .. } | MtItemErrorKind::ServiceUnavailable => true,
    }
}

/// The browser-transport backend of one run or one detached call.
pub(crate) struct BrowserMtSession<C> {
    service: MtService,
    channel: C,
    /// The run's Stop flag, handed to the bridge so it cancels the request in flight (the
    /// `MtCall` keeps its own reference for the limiter); `None` for a detached call.
    cancel: Option<Arc<AtomicBool>>,
    timings: BrowserTimings,
    /// `check_ready` succeeded once; later calls of the session skip it.
    connection_checked: Cell<bool>,
    /// End of `timings.overall`, fixed by the session's first call.
    deadline: Cell<Option<Instant>>,
}

/// The browser session of a Translation run of `service`; `cancel` is the run's Stop flag.
pub(crate) fn run_session(service: MtService, cancel: Arc<AtomicBool>) -> BrowserMtSession<BridgeChannel> {
    BrowserMtSession::new(service, BridgeChannel, Some(cancel), BrowserTimings::RUN)
}

/// The browser session of one detached call (the Cleaning prompt translator): no Stop, 30 s.
pub(crate) fn detached_session(service: MtService) -> BrowserMtSession<BridgeChannel> {
    BrowserMtSession::new(service, BridgeChannel, None, BrowserTimings::DETACHED)
}

impl<C: BrowserTranslateChannel> BrowserMtSession<C> {
    /// A session over `channel`.
    pub(crate) fn new(service: MtService, channel: C, cancel: Option<Arc<AtomicBool>>, timings: BrowserTimings) -> Self {
        Self { service, channel, cancel, timings, connection_checked: Cell::new(false), deadline: Cell::new(None) }
    }

    /// `check_ready` once per session. Its wait is `READY_WAIT`, capped by what is left of the
    /// session's overall budget (the detached call), and Stop ends it.
    fn ensure_connection(&self) -> Result<(), BridgeError> {
        if self.connection_checked.get() {
            return Ok(());
        }
        let max_wait = self.deadline.get().map_or(READY_WAIT, |deadline| READY_WAIT.min(deadline.saturating_duration_since(Instant::now())));
        self.channel.check_ready(&ReadyWait { max_wait, cancel: self.cancel.as_deref() })?;
        self.connection_checked.set(true);
        Ok(())
    }

    /// `(request timeout, attention timeout)` for the next request; `None` when the session's
    /// overall budget is spent.
    fn request_budget(&self) -> Option<(Duration, Duration)> {
        let Some(deadline) = self.deadline.get() else {
            return Some((self.timings.request, self.timings.attention));
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        (!remaining.is_zero()).then(|| (self.timings.request.min(remaining), self.timings.attention.min(remaining)))
    }

    /// One bridge request under one limiter permit. Checks Stop first, so a run's re-sent texts
    /// stop between requests.
    fn send(&self, call: &MtCall<'_>, request: &BrowserTranslateRequest<'_>) -> Result<String, SendFailure> {
        let service = self.service;
        if call.is_cancelled() {
            return Err(SendFailure::from(MtItemError::from_acquire(MtAcquireError::Cancelled, service)));
        }
        let out_of_time = || SendFailure::from(MtItemError::failed(tf!("translation.mt.browser.request_error", service = service.title(), reason = BridgeError::Timeout)));
        if self.request_budget().is_none() {
            return Err(out_of_time());
        }
        let permit = call.acquire(route_of(service)).map_err(|err| SendFailure::from(MtItemError::from_acquire(err, service)))?;
        // The acquire may have waited: re-read the budget. Dropping the permit unused records
        // `Neutral`.
        let Some((timeout, attention_timeout)) = self.request_budget() else {
            return Err(out_of_time());
        };
        let on_attention = |active: bool| call.report_attention(service, active);
        let options = BrowserCallOptions { timeout, attention_timeout, cancel: self.cancel.as_deref(), on_attention: &on_attention };
        let outcome = self.channel.translate(request, &options);
        // An attention notice still shown ends with the request.
        call.report_attention(service, false);
        match outcome {
            Ok(text) => {
                permit.finish(MtSignal::Ok);
                Ok(text)
            }
            Err(err) => {
                let signal = signal_for(&err);
                permit.finish(signal);
                let size = BROWSER_SIZE_UNIT.measure(request.text);
                if is_user_stop(&err) {
                    runtime_log::log_info(format!("[MT] {} via the browser: a request of {size} UTF-16 units was stopped by the user", service.title()));
                } else {
                    runtime_log::log_warn(format!("[MT] {} via the browser: a request of {size} UTF-16 units failed: {err}", service.title()));
                }
                let too_long = matches!(err, BridgeError::Remote { code: ErrorCode::TooLong, .. });
                if matches!(signal, MtSignal::RateLimited { .. } | MtSignal::Blocked { .. })
                    && let Some(retry_in) = call.lane_cooldown(MtLane::browser(service))
                {
                    return Err(SendFailure::from(MtItemError::from_acquire(MtAcquireError::CoolingDown { retry_in }, service)));
                }
                Err(SendFailure { error: item_error_for(service, &err), too_long })
            }
        }
    }
}

/// Whether `err` is the user's Stop (the bridge's own or the extension's `cancelled`).
const fn is_user_stop(err: &BridgeError) -> bool {
    matches!(err, BridgeError::Cancelled | BridgeError::Remote { code: ErrorCode::Cancelled, .. })
}

/// A failed bridge request: what it means for the texts it carried, and whether the extension
/// refused it as over its site limit (`too_long`), which a packed batch answers by re-sending
/// its texts one by one.
#[derive(Debug, Clone)]
struct SendFailure {
    error: MtItemError,
    too_long: bool,
}

impl From<MtItemError> for SendFailure {
    fn from(error: MtItemError) -> Self {
        Self { error, too_long: false }
    }
}

/// The work of one `translate_texts` call: languages, the stop error once one happened, and
/// the per-text answers.
struct CallPass<'s, 'c, C> {
    session: &'s BrowserMtSession<C>,
    call: &'s MtCall<'c>,
    source: String,
    target: String,
    /// The first stop-kind error; every later request answers it without being sent.
    stop: Option<MtItemError>,
}

impl<C: BrowserTranslateChannel> CallPass<'_, '_, C> {
    /// Sends `text` unless the call already stopped; records a stop-kind failure.
    fn request(&mut self, text: &str) -> Result<String, SendFailure> {
        if let Some(stop) = &self.stop {
            return Err(SendFailure::from(stop.clone()));
        }
        let request = BrowserTranslateRequest { site: self.session.service.key(), source: &self.source, target: &self.target, text };
        let result = self.session.send(self.call, &request);
        if let Err(failure) = &result
            && is_stop_kind(failure.error.kind)
        {
            self.stop = Some(failure.error.clone());
        }
        result
    }

    /// One text alone: its answer's lines joined, or an empty-answer failure.
    fn translate_one(&mut self, text: &str) -> Result<String, MtItemError> {
        let output = self.request(text).map_err(|failure| failure.error)?;
        single_answer(&output).ok_or_else(|| MtItemError::failed(tf!("translation.mt.browser.empty_answer_error", service = self.session.service.title())))
    }

    /// One `Lines` batch: one request, split by lines; on a line-count mismatch or a `too_long`
    /// refusal every text is re-sent alone.
    fn run_lines(&mut self, indices: &[usize], texts: &[String], answers: &mut [Option<Result<String, MtItemError>>]) {
        if let [index] = indices {
            answers[*index] = Some(self.translate_one(&texts[*index]));
            return;
        }
        let joined = indices.iter().map(|&index| texts[index].as_str()).collect::<Vec<_>>().join("\n");
        match self.request(&joined) {
            Ok(output) => {
                if let Some(lines) = split_translated_lines(&output, indices.len()) {
                    for (&index, line) in indices.iter().zip(lines) {
                        answers[index] = Some(Ok(line));
                    }
                    return;
                }
                runtime_log::log_warn(format!(
                    "[MT] {} via the browser: the answer to {} packed texts has {} non-empty lines; re-sending them one by one",
                    self.session.service.title(),
                    indices.len(),
                    answer_lines(&output).len()
                ));
                for &index in indices {
                    answers[index] = Some(self.translate_one(&texts[index]));
                }
            }
            // The extension measured the packed request over its site limit (the app's limit
            // should keep this from happening): each text alone is within it.
            Err(failure) if failure.too_long => {
                runtime_log::log_warn(format!(
                    "[MT] {} via the browser: the extension refused {} packed texts ({} UTF-16 units) as too long; re-sending them one by one",
                    self.session.service.title(),
                    indices.len(),
                    BROWSER_SIZE_UNIT.measure(&joined)
                ));
                for &index in indices {
                    answers[index] = Some(self.translate_one(&texts[index]));
                }
            }
            // The request failed for every text it carried.
            Err(failure) => {
                for &index in indices {
                    answers[index] = Some(Err(failure.error.clone()));
                }
            }
        }
    }

    /// One `Split` text: its chunks one request each, joined back with their separators.
    fn run_split(&mut self, index: usize, max_size: usize, texts: &[String], answers: &mut [Option<Result<String, MtItemError>>]) {
        let chunks = split_for_request_in(&texts[index], max_size, BROWSER_SIZE_UNIT);
        let mut translated = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            if chunk.text.trim().is_empty() {
                translated.push(chunk.text.clone());
                continue;
            }
            match self.translate_one(&chunk.text) {
                Ok(text) => translated.push(text),
                Err(err) => {
                    answers[index] = Some(Err(err));
                    return;
                }
            }
        }
        answers[index] = Some(Ok(join_chunks(translated.iter().map(String::as_str).zip(chunks.iter().map(|chunk| chunk.separator.as_str())))));
    }
}

impl<C: BrowserTranslateChannel> MachineTranslatorBackend for BrowserMtSession<C> {
    fn translate_texts(&self, call: &MtCall<'_>, source_lang: &str, target_lang: &str, texts: Vec<String>) -> Result<Vec<Result<String, MtItemError>>, String> {
        let (source, target) = site_languages(self.service, source_lang, target_lang)?;
        if let (Some(overall), None) = (self.timings.overall, self.deadline.get()) {
            self.deadline.set(Some(Instant::now() + overall));
        }
        let flat: Vec<String> = texts.iter().map(|text| flatten_line_breaks(text)).collect();
        let mut answers: Vec<Option<Result<String, MtItemError>>> = flat.iter().map(|text| text.is_empty().then(|| Ok(String::new()))).collect();
        let pending: Vec<(usize, &str)> = flat.iter().enumerate().filter(|(_, text)| !text.is_empty()).map(|(index, text)| (index, text.as_str())).collect();
        if pending.is_empty() {
            return Ok(answers.into_iter().map(|answer| answer.unwrap_or_else(|| Ok(String::new()))).collect());
        }
        let mut pass = CallPass { session: self, call, source, target, stop: None };
        match self.ensure_connection() {
            Ok(()) => {
                let max_chars = max_request_chars(self.service);
                for batch in pack_line_batches(&pending, max_chars, max_items(self.service)) {
                    match batch {
                        LineBatch::Lines(indices) => pass.run_lines(&indices, &flat, &mut answers),
                        LineBatch::Split(index) => pass.run_split(index, max_chars, &flat, &mut answers),
                    }
                }
            }
            Err(err) => {
                if is_user_stop(&err) {
                    runtime_log::log_info(format!("[MT] {} via the browser: stopped by the user while waiting for the extension", self.service.title()));
                } else {
                    runtime_log::log_warn(format!("[MT] {} via the browser is not ready: {err}", self.service.title()));
                }
                pass.stop = Some(item_error_for(self.service, &err));
            }
        }
        // Every text not answered was never sent: it shares the stop error (a stop is the only
        // way a planned text stays unanswered).
        let unsent = pass.stop.unwrap_or_else(|| MtItemError::from_acquire(MtAcquireError::Cancelled, self.service));
        Ok(answers.into_iter().map(|answer| answer.unwrap_or_else(|| Err(unsent.clone()))).collect())
    }

    fn preferred_batch_size(&self) -> usize {
        max_items(self.service)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use ms_browser_bridge::protocol::{BrowserInfo, ModuleInfo};
    use ms_browser_bridge::{PeerInfo, PeerState};

    use super::*;
    use crate::mt_limits::call::{DETACHED_MAX_WAIT, MtWaitReason, MtWaitStatus};
    use crate::mt_limits::test_support::FakeClock;

    /// What the fake does while serving one request.
    #[derive(Debug, Clone)]
    enum Step {
        /// Answer this text.
        Answer(String),
        /// Fail with this error.
        Fail(BridgeError),
        /// Report attention on, then off, then answer.
        AttentionThen(String),
        /// Set the run's cancel flag (Stop pressed mid-request), then fail as the bridge does.
        CancelThen,
        /// Answer this text, and Stop is pressed right after the answer arrived.
        AnswerThenCancel(String),
    }

    /// Scripted channel: `ready` answers `check_ready`; each `translate` takes the next step
    /// (none left = echo `tr <line>` per line) and records the request.
    struct FakeChannel {
        ready: Result<(), BridgeError>,
        steps: RefCell<VecDeque<Step>>,
        requests: RefCell<Vec<(String, String, String, String)>>,
        cancel_seen: RefCell<Vec<bool>>,
        cancel: Option<Arc<AtomicBool>>,
        /// `(max_wait, has a cancel flag)` of every `check_ready`.
        ready_waits: RefCell<Vec<(Duration, bool)>>,
    }

    impl FakeChannel {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                ready: Ok(()),
                steps: RefCell::new(steps.into()),
                requests: RefCell::new(Vec::new()),
                cancel_seen: RefCell::new(Vec::new()),
                cancel: None,
                ready_waits: RefCell::new(Vec::new()),
            }
        }

        fn texts(&self) -> Vec<String> {
            self.requests.borrow().iter().map(|(_, _, _, text)| text.clone()).collect()
        }
    }

    impl BrowserTranslateChannel for &FakeChannel {
        fn check_ready(&self, wait: &ReadyWait<'_>) -> Result<(), BridgeError> {
            self.ready_waits.borrow_mut().push((wait.max_wait, wait.cancel.is_some()));
            self.ready.clone()
        }

        fn translate(&self, request: &BrowserTranslateRequest<'_>, options: &BrowserCallOptions<'_>) -> Result<String, BridgeError> {
            self.requests.borrow_mut().push((request.site.to_owned(), request.source.to_owned(), request.target.to_owned(), request.text.to_owned()));
            self.cancel_seen.borrow_mut().push(options.cancel.is_some());
            let step = self.steps.borrow_mut().pop_front();
            match step {
                Some(Step::Answer(text)) => Ok(text),
                Some(Step::Fail(err)) => Err(err),
                Some(Step::AttentionThen(text)) => {
                    (options.on_attention)(true);
                    (options.on_attention)(true);
                    (options.on_attention)(false);
                    Ok(text)
                }
                Some(Step::CancelThen) => {
                    if let Some(flag) = &self.cancel {
                        flag.store(true, Ordering::Relaxed);
                    }
                    assert!(options.cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)), "the bridge sees the run's own Stop flag");
                    Err(BridgeError::Cancelled)
                }
                Some(Step::AnswerThenCancel(text)) => {
                    if let Some(flag) = &self.cancel {
                        flag.store(true, Ordering::Relaxed);
                    }
                    Ok(text)
                }
                None => Ok(request.text.split('\n').map(|line| format!("tr {line}")).collect::<Vec<_>>().join("\n")),
            }
        }
    }

    fn remote(code: ErrorCode) -> BridgeError {
        BridgeError::Remote { code, message: String::new(), retry_after: None }
    }

    fn strings(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    fn ok_texts(results: &[Result<String, MtItemError>]) -> Vec<Option<String>> {
        results.iter().map(|result| result.as_ref().ok().cloned()).collect()
    }

    fn kinds(results: &[Result<String, MtItemError>]) -> Vec<Option<MtItemErrorKind>> {
        results.iter().map(|result| result.as_ref().err().map(|err| err.kind)).collect()
    }

    /// What `run_call` observed: the results, the limiter's sleeps and the wait reports.
    type CallOutcome = (Vec<Result<String, MtItemError>>, Vec<Duration>, Vec<Option<MtWaitStatus>>);

    /// Runs one `translate_texts` call of a run session over `channel` on a fake-clock limiter.
    fn run_call(service: MtService, channel: &FakeChannel, cancel: &Arc<AtomicBool>, texts: &[&str]) -> CallOutcome {
        let (clock, registry) = FakeClock::registry();
        let log = RefCell::new(Vec::new());
        let on_wait = |status: Option<MtWaitStatus>| log.borrow_mut().push(status);
        let results = {
            let call = MtCall::for_run(&registry, cancel, &on_wait);
            let session = BrowserMtSession::new(service, channel, Some(Arc::clone(cancel)), BrowserTimings::RUN);
            session.translate_texts(&call, "ja", "ru", strings(texts)).unwrap_or_else(|err| panic!("setup failed: {err}"))
        };
        (results, clock.sleeps(), log.into_inner())
    }

    #[test]
    fn line_breaks_become_one_space_and_blank_texts_empty() {
        assert_eq!(flatten_line_breaks("a\r\nb\n\nc\rd\u{2028}e"), "a b c d e");
        assert_eq!(flatten_line_breaks("  one \n two  "), "one   two");
        assert_eq!(flatten_line_breaks("\r\n \n"), "");
        assert_eq!(flatten_line_breaks("plain"), "plain");
    }

    #[test]
    fn packing_keeps_order_caps_items_and_chars_and_isolates_long_texts() {
        let texts = [(0, "aaaa"), (1, "bb"), (2, "c"), (4, "dddddddddddd"), (5, "e"), (6, "ff")];
        // 10 chars: "aaaa\nbb\nc" is 9; "dddddddddddd" (12) is over the limit and goes alone.
        assert_eq!(
            pack_line_batches(&texts, 10, 20),
            [LineBatch::Lines(vec![0, 1, 2]), LineBatch::Split(4), LineBatch::Lines(vec![5, 6])]
        );
        // The newline counts: "aaaa\nbb" = 7 > 6.
        assert_eq!(pack_line_batches(&texts[..2], 6, 20), [LineBatch::Lines(vec![0]), LineBatch::Lines(vec![1])]);
        // Item cap.
        let many: Vec<(usize, &str)> = (0..5).map(|index| (index, "x")).collect();
        assert_eq!(pack_line_batches(&many, 100, 2), [LineBatch::Lines(vec![0, 1]), LineBatch::Lines(vec![2, 3]), LineBatch::Lines(vec![4])]);
        assert!(pack_line_batches(&[], 10, 2).is_empty());
    }

    #[test]
    fn answers_split_by_lines_with_an_exact_count() {
        assert_eq!(split_translated_lines("a\r\n\r\nb\u{a0}x \n c\r", 3), Some(strings(&["a", "b x", "c"])));
        assert_eq!(split_translated_lines("a\nb", 3), None);
        assert_eq!(split_translated_lines("a\nb\nc\nd", 3), None);
        assert_eq!(single_answer(" one\r\ntwo "), Some("one two".to_owned()));
        assert_eq!(single_answer(" \n "), None);
    }

    #[test]
    fn texts_are_packed_into_one_request_and_mapped_back() {
        let channel = FakeChannel::new(vec![Step::Answer("A\r\n\r\nB\u{a0}b\nC".to_owned())]);
        let cancel = Arc::new(AtomicBool::new(false));
        let (results, sleeps, _) = run_call(MtService::Deepl, &channel, &cancel, &["one", "  ", "two\nlines", "three"]);
        assert_eq!(ok_texts(&results), [Some("A".to_owned()), Some(String::new()), Some("B b".to_owned()), Some("C".to_owned())]);
        // One request; the blank text was never sent; the line break became a space.
        assert_eq!(*channel.requests.borrow(), [("deepl".to_owned(), "ja".to_owned(), "ru".to_owned(), "one\ntwo lines\nthree".to_owned())]);
        assert!(sleeps.is_empty());
        assert_eq!(*channel.cancel_seen.borrow(), [true]);
    }

    #[test]
    fn a_line_count_mismatch_resends_each_text_with_its_own_permit() {
        let channel = FakeChannel::new(vec![Step::Answer("merged answer".to_owned())]);
        let cancel = Arc::new(AtomicBool::new(false));
        let (results, sleeps, _) = run_call(MtService::Google, &channel, &cancel, &["a", "b", "c"]);
        assert_eq!(ok_texts(&results), [Some("tr a".to_owned()), Some("tr b".to_owned()), Some("tr c".to_owned())]);
        assert_eq!(channel.texts(), strings(&["a\nb\nc", "a", "b", "c"]));
        // Four requests on the browser lane: three 2 s spacings, one permit each.
        assert_eq!(sleeps.iter().sum::<Duration>(), Duration::from_secs(6));
    }

    #[test]
    fn an_over_limit_text_is_split_and_joined() {
        let sentence = "x".repeat(1000);
        let long = format!("{sentence}. {sentence}.");
        let channel = FakeChannel::new(vec![Step::Answer("SHORT".to_owned()), Step::Answer("first.".to_owned()), Step::Answer("second.".to_owned())]);
        let cancel = Arc::new(AtomicBool::new(false));
        let (results, _, _) = run_call(MtService::Deepl, &channel, &cancel, &["short", &long]);
        assert_eq!(ok_texts(&results), [Some("SHORT".to_owned()), Some("first. second.".to_owned())]);
        assert_eq!(channel.texts(), [strings(&["short"]), vec![format!("{sentence}."), format!("{sentence}.")]].concat());
    }

    #[test]
    fn languages_use_each_services_own_spelling() {
        assert_eq!(site_languages(MtService::Google, "", "zh-cn"), Ok(("auto".to_owned(), "zh-CN".to_owned())));
        assert!(site_languages(MtService::Google, "ja", "xx").is_err());
        assert_eq!(site_languages(MtService::Deepl, "", "en-us"), Ok(("auto".to_owned(), "en".to_owned())));
        assert_eq!(site_languages(MtService::Deepl, "JA", "ru"), Ok(("ja".to_owned(), "ru".to_owned())));
        assert_eq!(site_languages(MtService::Yandex, "", "zh-tw"), Ok(("auto".to_owned(), "zh".to_owned())));
    }

    #[test]
    fn a_rate_limit_cools_the_lane_and_stops_the_call() {
        let channel = FakeChannel::new(vec![Step::Answer("ok".to_owned()), Step::Fail(BridgeError::Remote { code: ErrorCode::RateLimited, message: String::new(), retry_after: None })]);
        let cancel = Arc::new(AtomicBool::new(false));
        // Texts too long to share a DeepL request (1500 chars): one request each.
        let big = "y".repeat(800);
        let (results, _, _) = run_call(MtService::Deepl, &channel, &cancel, &[&big, &big, &big]);
        let retry_in = Duration::from_mins(1);
        assert_eq!(ok_texts(&results)[0].as_deref(), Some("ok"));
        assert_eq!(kinds(&results)[1..], [Some(MtItemErrorKind::CoolingDown { retry_in }), Some(MtItemErrorKind::CoolingDown { retry_in })]);
        assert_eq!(channel.requests.borrow().len(), 2, "nothing is sent after the limit");
    }

    #[test]
    fn signals_follow_the_error_code() {
        assert_eq!(signal_for(&BridgeError::Remote { code: ErrorCode::RateLimited, message: String::new(), retry_after: Some(Duration::from_secs(7)) }), MtSignal::RateLimited { retry_after: Some(Duration::from_secs(7)) });
        assert_eq!(signal_for(&remote(ErrorCode::AttentionTimeout)), MtSignal::Blocked { retry_after: None });
        assert_eq!(signal_for(&BridgeError::AttentionTimeout), MtSignal::Blocked { retry_after: None });
        for neutral in [remote(ErrorCode::Timeout), remote(ErrorCode::SiteChanged), BridgeError::Timeout, BridgeError::Disconnected, BridgeError::NotConnected] {
            assert_eq!(signal_for(&neutral), MtSignal::Neutral, "{neutral:?}");
        }
        // An attention timeout blocks the lane: the rest of the call cools.
        let channel = FakeChannel::new(vec![Step::Fail(remote(ErrorCode::AttentionTimeout))]);
        let cancel = Arc::new(AtomicBool::new(false));
        let (results, _, _) = run_call(MtService::Yandex, &channel, &cancel, &["a"]);
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::CoolingDown { retry_in: Duration::from_mins(1) })]);
    }

    #[test]
    fn error_kinds_separate_per_text_failures_from_run_stops() {
        let kind = |err: BridgeError| item_error_for(MtService::Google, &err).kind;
        for stop in [BridgeError::NotConnected, BridgeError::NotStarted, BridgeError::Outdated, BridgeError::Disconnected, BridgeError::Unavailable, remote(ErrorCode::SiteChanged), remote(ErrorCode::BadParams), remote(ErrorCode::UnsupportedLanguage)] {
            assert_eq!(kind(stop.clone()), MtItemErrorKind::ServiceUnavailable, "{stop:?}");
        }
        for failed in [BridgeError::Timeout, remote(ErrorCode::Timeout), remote(ErrorCode::TooLong), remote(ErrorCode::TabClosed), remote(ErrorCode::Internal)] {
            assert_eq!(kind(failed.clone()), MtItemErrorKind::Failed, "{failed:?}");
        }
        assert_eq!(kind(BridgeError::Cancelled), MtItemErrorKind::Cancelled);
        assert_eq!(kind(remote(ErrorCode::Cancelled)), MtItemErrorKind::Cancelled);
    }

    #[test]
    fn a_service_stop_answers_the_remaining_texts_without_sending() {
        let channel = FakeChannel::new(vec![Step::Fail(remote(ErrorCode::SiteChanged))]);
        let cancel = Arc::new(AtomicBool::new(false));
        let big = "z".repeat(1000);
        let (results, _, _) = run_call(MtService::Deepl, &channel, &cancel, &[&big, &big]);
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::ServiceUnavailable), Some(MtItemErrorKind::ServiceUnavailable)]);
        assert_eq!(channel.requests.borrow().len(), 1);
    }

    #[test]
    fn stop_mid_batch_cancels_the_request_and_sends_nothing_more() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut channel = FakeChannel::new(vec![Step::CancelThen]);
        channel.cancel = Some(Arc::clone(&cancel));
        let big = "w".repeat(1000);
        let (results, _, _) = run_call(MtService::Deepl, &channel, &cancel, &[&big, &big, "c"]);
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::Cancelled); 3]);
        assert_eq!(channel.requests.borrow().len(), 1);
    }

    #[test]
    fn a_missing_extension_fails_every_text_at_once() {
        let mut channel = FakeChannel::new(Vec::new());
        channel.ready = Err(BridgeError::NotConnected);
        let cancel = Arc::new(AtomicBool::new(false));
        let (results, sleeps, waits) = run_call(MtService::Google, &channel, &cancel, &["a", "", "b"]);
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::ServiceUnavailable), None, Some(MtItemErrorKind::ServiceUnavailable)]);
        assert!(channel.requests.borrow().is_empty());
        assert!(sleeps.is_empty() && waits.is_empty(), "no permit, no wait");
        // A run's check may wait the full `READY_WAIT` and is stopped by the run's flag.
        assert_eq!(*channel.ready_waits.borrow(), [(READY_WAIT, true)]);
    }

    #[test]
    fn the_detached_call_fails_at_once_when_nothing_is_connected() {
        let mut channel = FakeChannel::new(Vec::new());
        channel.ready = Err(BridgeError::NotConnected);
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let ignore = |_status: Option<MtWaitStatus>| {};
        let call = MtCall::for_run(&registry, &cancel, &ignore).with_max_wait(DETACHED_MAX_WAIT);
        let session = BrowserMtSession::new(MtService::Deepl, &channel, None, BrowserTimings::DETACHED);
        let started = std::time::Instant::now();
        let results = session.translate_texts(&call, "ja", "en", strings(&["prompt"])).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::ServiceUnavailable)]);
        assert!(clock.sleeps().is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(channel.requests.borrow().is_empty());
        // The detached call's check waits at most what is left of its 30 s, with no Stop flag.
        let waits = channel.ready_waits.borrow();
        assert!(matches!(waits.as_slice(), [(max_wait, false)] if *max_wait <= DETACHED_OVERALL && *max_wait > Duration::from_secs(25)), "{waits:?}");
    }

    #[test]
    fn attention_events_reach_the_wait_reporter() {
        let channel = FakeChannel::new(vec![Step::AttentionThen("done".to_owned())]);
        let cancel = Arc::new(AtomicBool::new(false));
        let (results, _, waits) = run_call(MtService::Google, &channel, &cancel, &["a"]);
        assert_eq!(ok_texts(&results), [Some("done".to_owned())]);
        assert_eq!(waits, [Some(MtWaitStatus { service: MtService::Google, remaining: None, reason: MtWaitReason::BrowserAttention }), None]);
    }

    #[test]
    fn packing_measures_utf16_code_units() {
        // "😀😀😀" is 3 chars but 6 UTF-16 units: with "ab" it is 9 units, "cc" would make 12.
        let texts = [(0, "😀😀😀"), (1, "ab"), (2, "cc")];
        assert_eq!(pack_line_batches(&texts, 10, 20), [LineBatch::Lines(vec![0, 1]), LineBatch::Lines(vec![2])]);
        // Over the limit only in UTF-16 units: a split of its own.
        assert_eq!(pack_line_batches(&[(0, "😀😀😀😀😀😀")], 10, 20), [LineBatch::Split(0)]);
    }

    #[test]
    fn deepl_requests_stay_within_the_lane_limit_in_utf16_units() {
        let limit = max_request_chars(MtService::Deepl);
        // 700 emoji are 700 chars but 1400 units: no room for another text in the request.
        let emoji = "😀".repeat(limit / 2);
        let channel = FakeChannel::new(Vec::new());
        let cancel = Arc::new(AtomicBool::new(false));
        let (results, _, _) = run_call(MtService::Deepl, &channel, &cancel, &[&emoji, "a"]);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(channel.texts(), [emoji.clone(), "a".to_owned()]);
        // Over the limit in units: cut at char boundaries, every request within the limit.
        let long = "😀".repeat(limit / 2 + 50);
        let channel = FakeChannel::new(Vec::new());
        let (results, _, _) = run_call(MtService::Deepl, &channel, &cancel, &[&long]);
        assert!(results[0].is_ok(), "{results:?}");
        let sent = channel.texts();
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|text| BROWSER_SIZE_UNIT.measure(text) <= limit), "a request over the limit");
        assert_eq!(sent.concat(), long);
    }

    #[test]
    fn a_too_long_packed_batch_is_resent_one_by_one() {
        let channel = FakeChannel::new(vec![Step::Fail(remote(ErrorCode::TooLong))]);
        let cancel = Arc::new(AtomicBool::new(false));
        let (results, _, _) = run_call(MtService::Deepl, &channel, &cancel, &["a", "b", "c"]);
        assert_eq!(ok_texts(&results), [Some("tr a".to_owned()), Some("tr b".to_owned()), Some("tr c".to_owned())]);
        assert_eq!(channel.texts(), strings(&["a\nb\nc", "a", "b", "c"]));
        // A single text refused as too long stays a per-text failure (nothing smaller to send).
        let channel = FakeChannel::new(vec![Step::Fail(remote(ErrorCode::TooLong))]);
        let (results, _, _) = run_call(MtService::Deepl, &channel, &cancel, &["a", ""]);
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::Failed), None]);
        assert_eq!(channel.texts(), strings(&["a"]));
    }

    #[test]
    fn stop_during_the_one_by_one_resend_keeps_the_received_answers() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut channel = FakeChannel::new(vec![Step::Answer("merged".to_owned()), Step::AnswerThenCancel("A".to_owned())]);
        channel.cancel = Some(Arc::clone(&cancel));
        let (results, _, _) = run_call(MtService::Google, &channel, &cancel, &["a", "b", "c"]);
        assert_eq!(ok_texts(&results)[0].as_deref(), Some("A"));
        assert_eq!(kinds(&results)[1..], [Some(MtItemErrorKind::Cancelled), Some(MtItemErrorKind::Cancelled)]);
        assert_eq!(channel.texts(), strings(&["a\nb\nc", "a"]), "nothing is sent after Stop");
    }

    #[test]
    fn rust_limits_stay_below_the_extension_site_limits() {
        let translate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../browser-extension/src/modules/translate");
        let read = |relative: &str| std::fs::read_to_string(translate_dir.join(relative)).unwrap_or_else(|err| panic!("reading the tracked {relative}: {err}"));
        // The unit: the extension compares JavaScript's `length` (UTF-16 code units).
        assert!(read("index.ts").contains("request.text.length > site.maxChars"), "the extension's too_long check changed: revisit BROWSER_SIZE_UNIT");
        assert_eq!(BROWSER_SIZE_UNIT, SizeUnit::Utf16);
        for &service in MtService::all() {
            let site = read(&format!("sites/{}.ts", service.key()));
            let ts_limit: usize = site
                .lines()
                .find_map(|line| line.trim().strip_prefix("maxChars:"))
                .and_then(|value| value.trim().trim_end_matches(',').replace('_', "").parse().ok())
                .unwrap_or_else(|| panic!("no numeric maxChars in sites/{}.ts", service.key()));
            let rust_limit = max_request_chars(service);
            assert!(rust_limit < ts_limit, "{service:?}: the app packs up to {rust_limit} units, the extension refuses over {ts_limit}");
        }
    }

    /// A ready peer that serves the translate module.
    fn serving_peer() -> PeerInfo {
        PeerInfo {
            id: 1,
            browser: BrowserInfo::default(),
            ext_version: "1.0.0".into(),
            build_id: None,
            protocol: 1,
            modules: vec![ModuleInfo { name: TRANSLATE_MODULE.into(), version: TRANSLATE_MODULE_VERSION, sites: Vec::new() }],
            state: PeerState::Ready,
            build_current: true,
        }
    }

    fn bridge(listen: ListenState, peers: Vec<PeerInfo>) -> BridgeStatus {
        BridgeStatus { listen, peers }
    }

    /// Runs `await_ready_peer` over scripted statuses (the last one repeats) on a fake clock
    /// that each sleep advances; returns the outcome and the sleeps.
    fn await_scripted(listening_for: Option<Duration>, statuses: Vec<BridgeStatus>, max_wait: Duration, cancel: Option<&AtomicBool>) -> (Result<(), BridgeError>, Vec<Duration>) {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let sleeps = RefCell::new(Vec::new());
        let mut statuses: VecDeque<BridgeStatus> = statuses.into();
        let status = || {
            if statuses.len() > 1
                && let Some(next) = statuses.pop_front()
            {
                return next;
            }
            statuses.front().cloned().unwrap_or_else(|| bridge(ListenState::NotStarted, Vec::new()))
        };
        let sleep = |duration: Duration| {
            sleeps.borrow_mut().push(duration);
            elapsed.set(elapsed.get() + duration);
        };
        let now = || base + elapsed.get();
        let outcome = await_ready_peer(listening_for, &ReadyWait { max_wait, cancel }, status, &sleep, &now);
        (outcome, sleeps.into_inner())
    }

    #[test]
    fn a_bridge_started_by_the_check_waits_for_the_extension() {
        let listening = || ListenState::Listening { port: 4242 };
        let statuses = vec![bridge(ListenState::Starting, Vec::new()), bridge(listening(), Vec::new()), bridge(listening(), vec![serving_peer()])];
        let (outcome, sleeps) = await_scripted(None, statuses, READY_WAIT, None);
        assert_eq!(outcome, Ok(()));
        assert_eq!(sleeps, [READY_POLL, READY_POLL]);
        // Never connects: gives up after the wait with the last answer.
        let (outcome, sleeps) = await_scripted(None, vec![bridge(listening(), Vec::new())], Duration::from_secs(2), None);
        assert_eq!(outcome, Err(BridgeError::NotConnected));
        assert_eq!(sleeps.iter().sum::<Duration>(), Duration::from_secs(2));
        // An old extension (reload pending) is waited out too.
        let mut outdated = serving_peer();
        outdated.state = PeerState::Outdated;
        let (outcome, _) = await_scripted(None, vec![bridge(listening(), vec![outdated]), bridge(listening(), vec![serving_peer()])], READY_WAIT, None);
        assert_eq!(outcome, Ok(()));
    }

    #[test]
    fn a_bridge_listening_for_long_fails_fast() {
        let (outcome, sleeps) = await_scripted(Some(Duration::from_secs(40)), vec![bridge(ListenState::Listening { port: 4242 }, Vec::new()), bridge(ListenState::Listening { port: 4242 }, vec![serving_peer()])], READY_WAIT, None);
        assert_eq!(outcome, Err(BridgeError::NotConnected));
        assert!(sleeps.is_empty());
        let (outcome, sleeps) = await_scripted(Some(Duration::from_secs(40)), vec![bridge(ListenState::Listening { port: 4242 }, vec![serving_peer()])], READY_WAIT, None);
        assert_eq!((outcome, sleeps.len()), (Ok(()), 0));
    }

    #[test]
    fn a_young_listener_still_waits_and_an_old_one_fails_fast() {
        let listening = || ListenState::Listening { port: 4242 };
        // Listening for 5 s, nobody connected: waits, until the listener is `READY_WAIT` old.
        let (outcome, sleeps) = await_scripted(Some(Duration::from_secs(5)), vec![bridge(listening(), Vec::new())], READY_WAIT, None);
        assert_eq!(outcome, Err(BridgeError::NotConnected));
        assert_eq!(sleeps.iter().sum::<Duration>(), READY_WAIT - Duration::from_secs(5));
        let (outcome, sleeps) = await_scripted(Some(Duration::from_secs(5)), vec![bridge(listening(), Vec::new()), bridge(listening(), vec![serving_peer()])], READY_WAIT, None);
        assert_eq!((outcome, sleeps.len()), (Ok(()), 1));
        // Listening for 40 s, nobody connected: fails at once.
        let (outcome, sleeps) = await_scripted(Some(Duration::from_secs(40)), vec![bridge(listening(), Vec::new())], READY_WAIT, None);
        assert_eq!((outcome, sleeps.len()), (Err(BridgeError::NotConnected), 0));
    }

    #[test]
    fn the_ready_wait_never_waits_for_what_cannot_change_and_honours_stop() {
        let failure = BridgeError::Bind { detail: "port taken".into() };
        let (outcome, sleeps) = await_scripted(None, vec![bridge(ListenState::Failed { error: failure.clone() }, Vec::new())], READY_WAIT, None);
        assert_eq!((outcome, sleeps.len()), (Err(failure), 0));
        let mut other_version = serving_peer();
        other_version.modules[0].version = TRANSLATE_MODULE_VERSION + 1;
        let (outcome, sleeps) = await_scripted(None, vec![bridge(ListenState::Listening { port: 4242 }, vec![other_version])], READY_WAIT, None);
        assert!(matches!(outcome, Err(BridgeError::CapabilityMissing { .. })), "{outcome:?}");
        assert!(sleeps.is_empty());
        let stop = AtomicBool::new(true);
        let (outcome, sleeps) = await_scripted(None, vec![bridge(ListenState::Starting, Vec::new())], READY_WAIT, Some(&stop));
        assert_eq!((outcome, sleeps.len()), (Err(BridgeError::Cancelled), 0));
    }
}
