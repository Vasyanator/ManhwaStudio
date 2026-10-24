/*
FILE OVERVIEW: crates/ms-tab-translation/src/machine_translators/yandex.rs
Yandex Translate backend (the unofficial mobile endpoint, adapted from translatepy) on the shared
machine-translation rate limiter (`crate::mt_limits`).

Main items:
- `YandexMtBackend`: stateless backend for one-shot callers (builds its transport per call).
- `run_session`: the backend of one Translation run: one `YandexSession` (one pooled agent with
  the shared timeouts of `http::build_agent`) for every bubble of the run.
- `YandexSession`: the translation flow: optional per-text language detection, then packed
  translate requests (`BATCH_TEXTS` texts / `BATCH_MAX_CHARS` chars, one source language each).
- `YandexHttp` / `UreqYandexHttp`: private transport seam (tests script a fake one).
- `classify_reply`: status -> `MtSignal` + parsed answer or localized error.
- `cached_ucid`: Yandex UCID generator with 360 s TTL (translatepy-compatible, process-wide).

Behavior notes:
- EVERY request (detect and translate) takes a `MtRoute::Yandex` permit first and finishes it
  with its signal: HTTP 429 -> `RateLimited` (classified before the body is parsed, so a non-JSON
  429 still counts); JSON codes 403 / 404 / 408 (daily request, daily / monthly character
  limits) -> `Blocked` with `retry_after` = the service's `Retry-After` cap, so the route cools
  past every in-place wait and the run stops at once instead of re-probing an exhausted quota;
  a full success -> `Ok`; anything else -> `Neutral`. Nothing here sleeps
  or retries: waits and cooldowns belong to the limiter.
- The translate endpoint answers one `text` entry per `text` form field, in order (verified
  live), so several texts share a request and are mapped back by position; a count mismatch
  fails the whole request's texts.
- Once an acquire says `Cancelled` or `CoolingDown`, no further request is sent and every
  remaining text gets that error.
*/

use super::{MachineTranslatorBackend, MtItemError};
use crate::mt_limits::call::MtCall;

/// Yandex Translate as a stateless backend: every `translate_texts` call builds its own
/// transport. Used by the one-shot caller (`machine_translation::translate_texts_via_translator`);
/// a Translation run uses `run_session` instead, which keeps one transport for the whole run.
#[derive(Debug, Default, Clone, Copy)]
pub struct YandexMtBackend;

/// The backend of one Translation run (`super::run_session`): one pooled agent for every text.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn run_session() -> Box<dyn MachineTranslatorBackend> {
    native::run_session_backend()
}

/// The backend of one Translation run on the web build: the failing stub.
#[cfg(target_arch = "wasm32")]
pub(crate) fn run_session() -> Box<dyn MachineTranslatorBackend> {
    Box::new(YandexMtBackend)
}

/// Web stub: Yandex translation uses native HTTP (`ureq`) calls that the browser
/// build cannot make, so every batch fails with a clear message instead of
/// returning a fake translation.
#[cfg(target_arch = "wasm32")]
impl MachineTranslatorBackend for YandexMtBackend {
    fn translate_texts(
        &self,
        _call: &MtCall<'_>,
        _source_lang: &str,
        _target_lang: &str,
        _texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        Err(t!("translation.mt.yandex.web_unavailable_error").to_string())
    }
}

// The language-code spelling has one owner per service; the browser provider (`browser.rs`)
// reuses it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::normalize_yandex_lang;

// The native Yandex implementation depends on `ureq` (native HTTP), which is not
// compiled for wasm. The whole native surface lives in this module so the cfg
// boundary is a single point; on wasm only the stub impl above is compiled.
#[cfg(not(target_arch = "wasm32"))]
mod native {
use super::{MachineTranslatorBackend, MtCall, MtItemError, YandexMtBackend};
use crate::machine_translation::MtService;
use crate::machine_translators::{MtItemErrorKind, http};
use crate::mt_limits::call::MtSignal;
use crate::mt_limits::table::{MtLane, MtRoute, lane_limits};
use ms_log::runtime_log;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use web_time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::DeserializeOwned;

const API_BASE: &str = "https://translate.yandex.net/api/v1/tr.json";
const DETECT_HINT: &str = "en";
const UCID_TTL: Duration = Duration::from_secs(360);
/// The Android app's User-Agent the endpoint is used with (translatepy-compatible).
const USER_AGENT: &str = "ru.yandex.translate/3.20.2024";
/// Most texts packed into one translate request (also the run loop's batch size).
const BATCH_TEXTS: usize = 8;
/// Most characters (sum over the packed texts) of one translate request: a conservative guess,
/// well below what the endpoint is known to accept. A single longer text is still sent alone.
const BATCH_MAX_CHARS: usize = 5000;
/// Longest piece of an error body quoted in a user-visible message.
const ERROR_BODY_QUOTE_CHARS: usize = 300;
/// The JSON `code` of a successful Yandex answer.
const CODE_OK: i32 = 200;
/// HTTP "Too Many Requests".
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

impl MachineTranslatorBackend for YandexMtBackend {
    fn translate_texts(
        &self,
        call: &MtCall<'_>,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        YandexSession { http: UreqYandexHttp::new() }.translate_texts(call, source_lang, target_lang, texts)
    }
}

/// The production run backend (see `super::run_session`).
pub(super) fn run_session_backend() -> Box<dyn MachineTranslatorBackend> {
    Box::new(YandexSession { http: UreqYandexHttp::new() })
}

/// One answer of the endpoint, whatever its HTTP status.
#[derive(Debug)]
struct YandexReply {
    status: u16,
    /// The server's delta-seconds `Retry-After`, when it sent one (`http::retry_after`).
    retry_after: Option<Duration>,
    /// The response body; empty when an error body could not be read (logged).
    body: String,
}

/// The transport seam: `UreqYandexHttp` in production, a scripted fake in tests.
trait YandexHttp {
    /// GET `url` with `query`. `Err` = transport failure (localized message); every HTTP status
    /// is an `Ok` reply.
    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<YandexReply, String>;
    /// POST `form` (url-encoded; a name may repeat) to `url` with `query`. Errors as `get`.
    fn post_form(&self, url: &str, query: &[(&str, &str)], form: &[(&str, &str)]) -> Result<YandexReply, String>;
}

/// The production transport: one `ureq` agent with the shared MT timeouts (STATE S-082),
/// following redirects like before the shared agent existed.
struct UreqYandexHttp {
    agent: ureq::Agent,
}

impl UreqYandexHttp {
    fn new() -> Self {
        Self { agent: http::build_agent(USER_AGENT, true) }
    }
}

/// Appends every `query` pair to `request`.
fn with_query(request: ureq::Request, query: &[(&str, &str)]) -> ureq::Request {
    query.iter().fold(request, |request, (name, value)| request.query(name, value))
}

impl YandexHttp for UreqYandexHttp {
    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<YandexReply, String> {
        read_response(with_query(self.agent.get(url), query).call())
    }

    fn post_form(&self, url: &str, query: &[(&str, &str)], form: &[(&str, &str)]) -> Result<YandexReply, String> {
        read_response(with_query(self.agent.post(url), query).send_form(form))
    }
}

/// Turns a `ureq` outcome into a reply: any HTTP status is `Ok`; a transport failure or an
/// unreadable SUCCESS body is a localized `Err`. An unreadable ERROR body is logged and replaced
/// by an empty one, because the status alone classifies such a reply.
fn read_response(response: Result<ureq::Response, ureq::Error>) -> Result<YandexReply, String> {
    match response {
        Ok(resp) => {
            let status = resp.status();
            let retry_after = http::retry_after(&resp);
            let body = resp
                .into_string()
                .map_err(|err| tf!("translation.mt.yandex.read_body_error", err = err))?;
            Ok(YandexReply { status, retry_after, body })
        }
        Err(ureq::Error::Status(status, resp)) => {
            let retry_after = http::retry_after(&resp);
            let body = resp.into_string().unwrap_or_else(|err| {
                runtime_log::log_warn(format!("Yandex MT: could not read the body of an HTTP {status} answer (the error is reported without it): {err}"));
                String::new()
            });
            Ok(YandexReply { status, retry_after, body })
        }
        Err(ureq::Error::Transport(err)) => Err(tf!("translation.mt.yandex.network_error", err = err)),
    }
}

/// Which endpoint an answer came from (for its "invalid JSON" message).
#[derive(Debug, Clone, Copy)]
enum Endpoint {
    Detect,
    Translate,
}

/// The JSON answers of both endpoints carry a Yandex status `code`.
trait YandexAnswer: DeserializeOwned {
    fn code(&self) -> i32;
}

/// Yandex's own limit codes: daily request limit, daily and monthly character limits. They
/// mean "this client may not translate for a long while", so the route is blocked for the
/// longest cooldown the limiter grants (`QUOTA_RETRY_AFTER`).
fn is_quota_code(code: i32) -> bool {
    matches!(code, 403 | 404 | 408)
}

/// The `retry_after` a quota answer reports: the service's cap. A daily / monthly quota does
/// not recover within a run, so a cooldown of minutes followed by re-probes would only delay
/// the stop; the cap makes the next acquire refuse at once (it exceeds the in-place wait)
/// with a "try again in about N min" that is as long as the limiter ever waits.
fn quota_retry_after() -> Duration {
    lane_limits(MtLane::builtin(MtService::Yandex)).retry_after_cap
}

/// Classifies one reply for the limiter and parses it. The status is checked BEFORE the body
/// is parsed, so a 429 with an HTML body still cools the route. Returns the permit's signal
/// and the parsed answer or a localized error.
fn classify_reply<T: YandexAnswer>(reply: &YandexReply, endpoint: Endpoint) -> (MtSignal, Result<T, String>) {
    if reply.status == HTTP_TOO_MANY_REQUESTS {
        return (MtSignal::RateLimited { retry_after: reply.retry_after }, Err(t!("translation.mt.yandex.rate_limited_error").to_string()));
    }
    let parsed: T = match serde_json::from_str(&reply.body) {
        Ok(parsed) => parsed,
        Err(_) if reply.status >= 400 => return (MtSignal::Neutral, Err(http_error_message(reply.status, &reply.body))),
        Err(err) => {
            let message = match endpoint {
                Endpoint::Detect => tf!("translation.mt.yandex.detect_invalid_json_error", err = err),
                Endpoint::Translate => tf!("translation.mt.yandex.translate_invalid_json_error", err = err),
            };
            return (MtSignal::Neutral, Err(message));
        }
    };
    let code = parsed.code();
    if reply.status < 400 && code == CODE_OK {
        return (MtSignal::Ok, Ok(parsed));
    }
    let message = api_error_message(code, reply.status, &reply.body);
    if is_quota_code(code) {
        (MtSignal::Blocked { retry_after: Some(quota_retry_after()) }, Err(message))
    } else {
        (MtSignal::Neutral, Err(message))
    }
}

/// Whether `err` means "send nothing more" (Stop, or the service cools too long).
fn stops_the_call(err: &MtItemError) -> bool {
    match err.kind {
        MtItemErrorKind::Cancelled | MtItemErrorKind::CoolingDown { .. } | MtItemErrorKind::ServiceUnavailable => true,
        MtItemErrorKind::Failed => false,
    }
}

/// A text that still needs a translate request.
#[derive(Debug)]
struct PendingText<'t> {
    /// Position in the caller's `texts`.
    index: usize,
    /// The trimmed text sent to Yandex.
    text: &'t str,
    /// Its source language (the run's, or the detected one in `auto` mode).
    source: String,
}

/// Cuts `pending` into consecutive translate requests: one source language each, at most
/// `BATCH_TEXTS` texts and `BATCH_MAX_CHARS` characters (a longer single text goes alone).
/// Consecutive cuts keep the results in input order, so a request that stops the call fails
/// only texts after every translated one.
fn plan_requests(pending: &[PendingText<'_>]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (position, item) in pending.iter().enumerate() {
        let item_chars = item.text.chars().count();
        let full = position > start
            && (pending[start].source != item.source || position - start >= BATCH_TEXTS || chars + item_chars > BATCH_MAX_CHARS);
        if full {
            ranges.push(start..position);
            start = position;
            chars = 0;
        }
        chars += item_chars;
    }
    if start < pending.len() {
        ranges.push(start..pending.len());
    }
    ranges
}

/// Yandex over one transport. A Translation run keeps one for all of its bubbles (pooled
/// connections); the UCID comes from the process-wide `cached_ucid`.
struct YandexSession<H: YandexHttp> {
    http: H,
}

impl<H: YandexHttp> MachineTranslatorBackend for YandexSession<H> {
    fn translate_texts(
        &self,
        call: &MtCall<'_>,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        Ok(self.translate_all(call, source_lang, target_lang, &texts))
    }

    fn preferred_batch_size(&self) -> usize {
        BATCH_TEXTS
    }
}

impl<H: YandexHttp> YandexSession<H> {
    /// Translates `texts` (one result each, in order): empty texts become empty translations
    /// without a request; in `auto` mode every text is detected first (one request each).
    fn translate_all(&self, call: &MtCall<'_>, source_lang: &str, target_lang: &str, texts: &[String]) -> Vec<Result<String, MtItemError>> {
        let source = normalize_yandex_lang(source_lang, true);
        let target = normalize_yandex_lang(target_lang, false);
        let mut results: Vec<Option<Result<String, MtItemError>>> = vec![None; texts.len()];
        // Set by the first Cancelled / CoolingDown: from then on nothing is sent and every
        // remaining text gets this error.
        let mut stop: Option<MtItemError> = None;
        let mut pending = Vec::new();
        for (index, text) in texts.iter().enumerate() {
            let text = text.trim();
            if text.is_empty() {
                results[index] = Some(Ok(String::new()));
            } else if source != "auto" {
                pending.push(PendingText { index, text, source: source.clone() });
            } else if let Some(err) = &stop {
                results[index] = Some(Err(err.clone()));
            } else {
                match self.detect_language(call, text) {
                    Ok(detected) => pending.push(PendingText { index, text, source: detected }),
                    Err(err) => {
                        if stops_the_call(&err) {
                            stop = Some(err.clone());
                        }
                        results[index] = Some(Err(err));
                    }
                }
            }
        }
        for range in plan_requests(&pending) {
            let request = &pending[range];
            let outcome = match &stop {
                Some(err) => Err(err.clone()),
                None => {
                    let lang_pair = format!("{}-{target}", request[0].source);
                    let request_texts: Vec<&str> = request.iter().map(|item| item.text).collect();
                    self.translate(call, &request_texts, &lang_pair)
                }
            };
            match outcome {
                Ok(translations) => {
                    for (item, translated) in request.iter().zip(translations) {
                        results[item.index] = Some(Ok(translated));
                    }
                }
                Err(err) => {
                    if stops_the_call(&err) {
                        stop = Some(err.clone());
                    }
                    for item in request {
                        results[item.index] = Some(Err(err.clone()));
                    }
                }
            }
        }
        // Every index was filled above: empty, failed detection, or in exactly one request
        // (`translate` returns exactly one translation per text). The fallback is an error,
        // never a fake translation.
        results.into_iter().map(|result| result.unwrap_or_else(|| Err(MtItemError::failed(t!("translation.mt.yandex.translate_empty_error").to_string())))).collect()
    }

    /// One permit-guarded request: acquire, send, classify, finish the permit.
    fn exchange<T: YandexAnswer>(&self, call: &MtCall<'_>, endpoint: Endpoint, send: impl FnOnce(&H) -> Result<YandexReply, String>) -> Result<T, MtItemError> {
        let permit = call.acquire(MtRoute::Yandex).map_err(|err| MtItemError::from_acquire(err, MtService::Yandex))?;
        let (signal, outcome) = match send(&self.http) {
            Ok(reply) => classify_reply::<T>(&reply, endpoint),
            Err(message) => (MtSignal::Neutral, Err(message)),
        };
        permit.finish(signal);
        outcome.map_err(MtItemError::failed)
    }

    /// The source language of `text` (lower-case base code, `auto` when Yandex names none).
    fn detect_language(&self, call: &MtCall<'_>, text: &str) -> Result<String, MtItemError> {
        let parsed: YandexDetectResponse = self.exchange(call, Endpoint::Detect, |http| {
            let ucid = cached_ucid();
            http.get(&format!("{API_BASE}/detect"), &[("ucid", &ucid), ("srv", "android"), ("text", text), ("hint", DETECT_HINT)])
        })?;
        let lang = parsed.lang.as_deref().unwrap_or("auto").trim().to_ascii_lowercase();
        if lang.is_empty() {
            Ok("auto".to_string())
        } else {
            Ok(normalize_detected_language(&lang))
        }
    }

    /// Translates `texts` (all in the source language of `lang_pair`) in ONE request; returns
    /// exactly one translation per text, in order.
    fn translate(&self, call: &MtCall<'_>, texts: &[&str], lang_pair: &str) -> Result<Vec<String>, MtItemError> {
        let parsed: YandexTranslateResponse = self.exchange(call, Endpoint::Translate, |http| {
            let ucid = cached_ucid();
            let mut form: Vec<(&str, &str)> = texts.iter().map(|text| ("text", *text)).collect();
            form.push(("lang", lang_pair));
            http.post_form(&format!("{API_BASE}/translate"), &[("ucid", &ucid), ("srv", "android"), ("format", "text")], &form)
        })?;
        match parsed.text {
            Some(translations) if translations.len() == texts.len() => Ok(translations),
            Some(translations) if !translations.is_empty() => {
                Err(MtItemError::failed(tf!("translation.mt.yandex.translate_count_error", expected = texts.len(), got = translations.len())))
            }
            _ => Err(MtItemError::failed(t!("translation.mt.yandex.translate_empty_error").to_string())),
        }
    }
}

/// Maps a panel language code to Yandex's spelling (lower case, `zh-*` -> `zh`; `auto` for an
/// empty source, `ru` for an empty target). Shared with the browser provider (`browser.rs`).
pub(crate) fn normalize_yandex_lang(raw: &str, source: bool) -> String {
    let fallback = if source { "auto" } else { "ru" };
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return fallback.to_string();
    }

    match trimmed.as_str() {
        "zh-cn" | "zh-tw" => "zh".to_string(),
        _ => trimmed,
    }
}

fn normalize_detected_language(lang: &str) -> String {
    let first = lang
        .split('-')
        .next()
        .unwrap_or(lang)
        .trim()
        .to_ascii_lowercase();
    if first.is_empty() {
        "auto".to_string()
    } else {
        first
    }
}

/// `body` trimmed to one line and at most `ERROR_BODY_QUOTE_CHARS` characters.
fn quote_body(body: &str) -> String {
    let compact = body.trim().replace(['\r', '\n'], " ");
    if compact.chars().count() <= ERROR_BODY_QUOTE_CHARS {
        compact
    } else {
        let mut quoted: String = compact.chars().take(ERROR_BODY_QUOTE_CHARS).collect();
        quoted.push('…');
        quoted
    }
}

/// The localized error of a JSON answer whose Yandex `code` (or HTTP status) is not a success.
fn api_error_message(code: i32, status: u16, body: &str) -> String {
    let code_name = yandex_error_code_text(code);
    if body.trim().is_empty() {
        tf!("translation.mt.yandex.api_error", code = code, code_name = code_name, status = status)
    } else {
        tf!("translation.mt.yandex.api_error_with_body", code = code, code_name = code_name, status = status, body = quote_body(body))
    }
}

/// The localized error of an HTTP error status whose body is not Yandex JSON.
fn http_error_message(status: u16, body: &str) -> String {
    if body.trim().is_empty() {
        tf!("translation.mt.yandex.http_error", status = status)
    } else {
        tf!("translation.mt.yandex.http_error_with_body", status = status, body = quote_body(body))
    }
}

fn yandex_error_code_text(code: i32) -> &'static str {
    match code {
        401 => "ERR_KEY_INVALID",
        402 => "ERR_KEY_BLOCKED",
        403 => "ERR_DAILY_REQ_LIMIT_EXCEEDED",
        404 => "ERR_DAILY_CHAR_LIMIT_EXCEEDED",
        408 => "ERR_MONTHLY_CHAR_LIMIT_EXCEEDED",
        413 => "ERR_TEXT_TOO_LONG",
        422 => "ERR_UNPROCESSABLE_TEXT",
        501 => "ERR_LANG_NOT_SUPPORTED",
        503 => "ERR_SERVICE_NOT_AVAILABLE",
        _ => "ERR_UNKNOWN",
    }
}

#[derive(Debug)]
struct UcidCache {
    value: String,
    generated_at: Instant,
}

static UCID_COUNTER: AtomicU64 = AtomicU64::new(1);
static UCID_CACHE: OnceLock<Mutex<Option<UcidCache>>> = OnceLock::new();

fn cached_ucid() -> String {
    let cache = UCID_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(_) => return generate_ucid(),
    };
    if let Some(existing) = guard.as_ref()
        && existing.generated_at.elapsed() < UCID_TTL
    {
        return existing.value.clone();
    }
    let value = generate_ucid();
    *guard = Some(UcidCache {
        value: value.clone(),
        generated_at: Instant::now(),
    });
    value
}

fn generate_ucid() -> String {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let ctr = UCID_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    format!("{ns:016x}{pid:08x}{ctr:08x}")
}

#[derive(Debug, Deserialize)]
struct YandexDetectResponse {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    lang: Option<String>,
}

impl YandexAnswer for YandexDetectResponse {
    fn code(&self) -> i32 {
        self.code
    }
}

#[derive(Debug, Deserialize)]
struct YandexTranslateResponse {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    text: Option<Vec<String>>,
}

impl YandexAnswer for YandexTranslateResponse {
    fn code(&self) -> i32 {
        self.code
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mt_limits::call::MtWaitStatus;
    use crate::mt_limits::registry::MtClock;
    use crate::mt_limits::test_support::FakeClock;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;

    /// One request the fake transport saw.
    #[derive(Debug)]
    struct Sent {
        url: String,
        query: Vec<(String, String)>,
        /// Empty for a GET.
        form: Vec<(String, String)>,
        at: Instant,
    }

    impl Sent {
        fn query_value(&self, name: &str) -> Option<&str> {
            self.query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
        }

        fn form_values(&self, name: &str) -> Vec<&str> {
            self.form.iter().filter(|(key, _)| key == name).map(|(_, value)| value.as_str()).collect()
        }
    }

    /// A scripted transport: answers requests from `replies` in order (a request with no reply
    /// left fails the test), records every request with the fake clock's time, and optionally
    /// sets a cancel flag after the first request.
    struct FakeHttp<'c> {
        clock: &'c FakeClock,
        replies: RefCell<VecDeque<Result<YandexReply, String>>>,
        sent: RefCell<Vec<Sent>>,
        cancel_after_first: Option<&'c AtomicBool>,
    }

    impl<'c> FakeHttp<'c> {
        fn new(clock: &'c FakeClock, replies: Vec<Result<YandexReply, String>>) -> Self {
            Self { clock, replies: RefCell::new(replies.into()), sent: RefCell::new(Vec::new()), cancel_after_first: None }
        }

        fn record(&self, url: &str, query: &[(&str, &str)], form: &[(&str, &str)]) -> Result<YandexReply, String> {
            let owned = |pairs: &[(&str, &str)]| pairs.iter().map(|(key, value)| ((*key).to_string(), (*value).to_string())).collect();
            self.sent.borrow_mut().push(Sent { url: url.to_string(), query: owned(query), form: owned(form), at: self.clock.now() });
            if let Some(cancel) = self.cancel_after_first {
                cancel.store(true, Ordering::Relaxed);
            }
            self.replies.borrow_mut().pop_front().unwrap_or_else(|| panic!("unexpected request to {url}"))
        }
    }

    impl YandexHttp for FakeHttp<'_> {
        fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<YandexReply, String> {
            self.record(url, query, &[])
        }

        fn post_form(&self, url: &str, query: &[(&str, &str)], form: &[(&str, &str)]) -> Result<YandexReply, String> {
            self.record(url, query, form)
        }
    }

    fn reply(status: u16, body: &str) -> Result<YandexReply, String> {
        Ok(YandexReply { status, retry_after: None, body: body.to_string() })
    }

    fn detected(lang: &str) -> Result<YandexReply, String> {
        reply(200, &format!(r#"{{"code":200,"lang":"{lang}"}}"#))
    }

    fn translated(texts: &[&str]) -> Result<YandexReply, String> {
        let body = serde_json::json!({ "code": 200, "lang": "en-ru", "text": texts });
        reply(200, &body.to_string())
    }

    fn strings(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_string()).collect()
    }

    fn run(session: &YandexSession<FakeHttp<'_>>, call: &MtCall<'_>, source: &str, texts: &[&str]) -> Vec<Result<String, MtItemError>> {
        session.translate_texts(call, source, "ru", strings(texts)).unwrap_or_else(|err| panic!("Yandex never fails a whole batch: {err}"))
    }

    fn kinds(results: &[Result<String, MtItemError>]) -> Vec<Option<MtItemErrorKind>> {
        results.iter().map(|result| result.as_ref().err().map(|err| err.kind)).collect()
    }

    fn ignore_wait(_status: Option<MtWaitStatus>) {}

    #[test]
    fn auto_mode_detects_each_text_through_the_limiter() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        let session = YandexSession { http: FakeHttp::new(&clock, vec![detected("en-US"), translated(&["Привет"])]) };
        let results = run(&session, &call, "auto", &["  Hello  "]);
        assert_eq!(results, [Ok("Привет".to_string())]);
        let sent = session.http.sent.borrow();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].url.ends_with("/detect") && sent[0].form.is_empty());
        assert_eq!(sent[0].query_value("text"), Some("Hello"));
        assert!(sent[1].url.ends_with("/translate"));
        assert_eq!(sent[1].form_values("lang"), ["en-ru"]);
        assert_eq!(sent[1].form_values("text"), ["Hello"]);
        // The detection is a request like any other: the translate waits the 1 s spacing.
        assert_eq!(sent[1].at - sent[0].at, Duration::from_secs(1));
    }

    #[test]
    fn texts_share_a_request_and_map_back_by_position() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        let session = YandexSession { http: FakeHttp::new(&clock, vec![translated(&["один", "три"])]) };
        let results = run(&session, &call, "en", &["one", "   ", "three"]);
        assert_eq!(results, [Ok("один".to_string()), Ok(String::new()), Ok("три".to_string())]);
        let sent = session.http.sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].form_values("text"), ["one", "three"]);
        assert_eq!(sent[0].form_values("lang"), ["en-ru"]);
    }

    #[test]
    fn auto_mode_packs_consecutive_texts_of_one_language() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        let replies = vec![detected("en"), detected("en"), detected("ja"), detected("en"), translated(&["a", "b"]), translated(&["c"]), translated(&["d"])];
        let session = YandexSession { http: FakeHttp::new(&clock, replies) };
        let results = run(&session, &call, "auto", &["A", "B", "C", "D"]);
        assert_eq!(results, ["a", "b", "c", "d"].map(|text| Ok(text.to_string())));
        let sent = session.http.sent.borrow();
        let posts: Vec<(Vec<&str>, Vec<&str>)> = sent.iter().filter(|sent| !sent.form.is_empty()).map(|sent| (sent.form_values("text"), sent.form_values("lang"))).collect();
        assert_eq!(posts, [(vec!["A", "B"], vec!["en-ru"]), (vec!["C"], vec!["ja-ru"]), (vec!["D"], vec!["en-ru"])]);
    }

    #[test]
    fn requests_are_cut_by_count_and_by_characters() {
        let short: Vec<PendingText<'_>> = (0..20).map(|index| PendingText { index, text: "x", source: "en".to_string() }).collect();
        assert_eq!(plan_requests(&short), [0..8, 8..16, 16..20]);
        let long = "y".repeat(BATCH_MAX_CHARS + 1);
        let near = "z".repeat(BATCH_MAX_CHARS - 1);
        let mixed = [
            PendingText { index: 0, text: "a", source: "en".to_string() },
            PendingText { index: 1, text: &long, source: "en".to_string() },
            PendingText { index: 2, text: &near, source: "en".to_string() },
            PendingText { index: 3, text: "b", source: "en".to_string() },
            PendingText { index: 4, text: "c", source: "en".to_string() },
        ];
        assert_eq!(plan_requests(&mixed), [0..1, 1..2, 2..4, 4..5]);
        assert!(plan_requests(&[]).is_empty());
    }

    #[test]
    fn a_translation_count_mismatch_fails_every_text_of_the_request() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        let session = YandexSession { http: FakeHttp::new(&clock, vec![translated(&["only one"])]) };
        let results = run(&session, &call, "en", &["one", "two"]);
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::Failed), Some(MtItemErrorKind::Failed)]);
    }

    #[test]
    fn repeated_429s_cool_down_step_by_step_until_the_run_must_stop() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let waits = RefCell::new(Vec::new());
        let on_wait = |status: Option<MtWaitStatus>| waits.borrow_mut().push(status);
        let call = MtCall::for_run(&registry, &cancel, &on_wait);
        let html = "<html><title>Too Many Requests</title></html>";
        let session = YandexSession { http: FakeHttp::new(&clock, vec![reply(429, html), reply(429, html), reply(429, html)]) };
        for _ in 0..3 {
            assert_eq!(kinds(&run(&session, &call, "en", &["text"])), [Some(MtItemErrorKind::Failed)]);
        }
        let cooling = run(&session, &call, "en", &["text"]);
        assert_eq!(kinds(&cooling), [Some(MtItemErrorKind::CoolingDown { retry_in: Duration::from_secs(120) })]);
        let sent = session.http.sent.borrow();
        assert_eq!(sent.len(), 3, "a cooling service gets no request");
        assert_eq!(sent[1].at - sent[0].at, Duration::from_secs(30));
        assert_eq!(sent[2].at - sent[1].at, Duration::from_secs(60));
        assert!(waits.borrow().iter().flatten().any(|status| status.service == MtService::Yandex && status.remaining.is_some()), "the cooldown waits are reported");
    }

    #[test]
    fn a_quota_code_stops_the_service_at_once_for_the_longest_cooldown() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        let replies = vec![reply(200, r#"{"code":404,"message":"daily limit"}"#)];
        let session = YandexSession { http: FakeHttp::new(&clock, replies) };
        let cap = lane_limits(MtLane::builtin(MtService::Yandex)).retry_after_cap;
        // The quota request fails its texts; the next request is refused at once, without
        // waiting or sending, with the full cap as the retry time.
        assert_eq!(kinds(&run(&session, &call, "en", &["first"])), [Some(MtItemErrorKind::Failed)]);
        assert_eq!(kinds(&run(&session, &call, "en", &["second"])), [Some(MtItemErrorKind::CoolingDown { retry_in: cap })]);
        assert_eq!(session.http.sent.borrow().len(), 1);
        assert!(clock.sleeps().is_empty(), "no in-place wait for an exhausted quota");
    }

    #[test]
    fn cancel_stops_every_further_request() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        let mut http = FakeHttp::new(&clock, vec![detected("en")]);
        http.cancel_after_first = Some(&cancel);
        let session = YandexSession { http };
        let results = run(&session, &call, "auto", &["one", "two", "three"]);
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::Cancelled); 3]);
        assert_eq!(session.http.sent.borrow().len(), 1);
    }

    #[test]
    fn one_session_reuses_its_transport_and_ucid() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        let session = YandexSession { http: FakeHttp::new(&clock, vec![translated(&["a"]), translated(&["b"])]) };
        assert_eq!(run(&session, &call, "en", &["A"]), [Ok("a".to_string())]);
        assert_eq!(run(&session, &call, "en", &["B"]), [Ok("b".to_string())]);
        let sent = session.http.sent.borrow();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].query_value("ucid").is_some());
        assert_eq!(sent[0].query_value("ucid"), sent[1].query_value("ucid"));
        assert_eq!(run_session_backend().preferred_batch_size(), BATCH_TEXTS);
    }

    #[test]
    fn a_network_error_fails_the_text_without_cooling() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        let session = YandexSession { http: FakeHttp::new(&clock, vec![Err("network down".to_string()), translated(&["ok"])]) };
        let failed = run(&session, &call, "en", &["one"]);
        assert_eq!(failed, [Err(MtItemError::failed("network down".to_string()))]);
        assert_eq!(run(&session, &call, "en", &["two"]), [Ok("ok".to_string())]);
        let sent = session.http.sent.borrow();
        assert_eq!(sent[1].at - sent[0].at, Duration::from_secs(1));
    }

    #[test]
    fn replies_are_classified_by_status_before_parsing() {
        let classify = |status: u16, retry_after: Option<Duration>, body: &str| {
            let reply = YandexReply { status, retry_after, body: body.to_string() };
            let (signal, outcome) = classify_reply::<YandexTranslateResponse>(&reply, Endpoint::Translate);
            (signal, outcome.is_ok())
        };
        let seven = Some(Duration::from_secs(7));
        assert_eq!(classify(429, seven, "<html>busy</html>"), (MtSignal::RateLimited { retry_after: seven }, false));
        assert_eq!(classify(429, None, r#"{"code":200,"text":["x"]}"#), (MtSignal::RateLimited { retry_after: None }, false));
        assert_eq!(classify(200, None, r#"{"code":200,"text":["x"]}"#), (MtSignal::Ok, true));
        for code in [403, 404, 408] {
            let quota = MtSignal::Blocked { retry_after: Some(quota_retry_after()) };
            assert_eq!(classify(200, None, &format!(r#"{{"code":{code}}}"#)), (quota, false), "code {code}");
            assert_eq!(classify(403, seven, &format!(r#"{{"code":{code}}}"#)), (quota, false), "code {code}");
        }
        assert_eq!(classify(200, None, r#"{"code":501}"#), (MtSignal::Neutral, false));
        assert_eq!(classify(500, None, "<html>oops</html>"), (MtSignal::Neutral, false));
        assert_eq!(classify(200, None, "not json"), (MtSignal::Neutral, false));
    }

    #[test]
    fn error_bodies_are_quoted_on_one_short_line() {
        assert_eq!(quote_body("  a\nb\r\nc  "), "a b  c");
        let quoted = quote_body(&"x".repeat(ERROR_BODY_QUOTE_CHARS + 10));
        assert_eq!(quoted.chars().count(), ERROR_BODY_QUOTE_CHARS + 1);
        assert!(quoted.ends_with('…'));
    }
}
} // mod native
