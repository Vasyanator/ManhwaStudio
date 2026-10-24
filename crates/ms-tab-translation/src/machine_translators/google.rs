/*
FILE OVERVIEW: crates/ms-tab-translation/src/machine_translators/google.rs
Google Translate backend of the Translation tab MT worker: an in-house blocking client for
Google's free JSON endpoints (no API key).

Main items:
- `GoogleMtBackend`: stateless backend implementing the shared `MachineTranslatorBackend`
  contract (one fresh `GoogleMtSession` per call; used by the shared dispatcher).
- `GoogleMtSession`: one MT run's client (HTTP agent + remembered endpoint); the Translation
  worker builds it once per run so the endpoint memory and connection pool span every bubble.
- `ENDPOINTS`: the ordered endpoint chain (primary first, then fallbacks), each on its limiter
  route (`MtRoute::GoogleGtx` for both gtx hosts, `MtRoute::GoogleClients5`).
- `GoogleHttp`: the injectable HTTP seam (`UreqHttp` in production, a scripted fake in tests).
- `translate_text`: splits a text longer than the service's request limit and joins the
  translated chunks back with the original separators.
- `translate_one`: walks the endpoint chain for one request text and maps each answer to
  `GoogleError`; `request_endpoint` takes the limiter permit and finishes it with `limit_signal`.
- `normalize_google_lang`: maps the MT panel's language codes to Google's spelling and rejects
  codes outside the supported table.

Behavior:
- One `POST <endpoint>?client=..&sl=..&tl=..` per non-empty text, the text in the form field `q`
  (POST, so long texts never hit Google's ~16 KB URL limit). A text longer than
  `LaneLimits::max_request_chars` is cut by `mt_limits::split` into several requests; the text
  fails as a whole if any chunk fails. Redirects are NOT followed: a redirect to
  `google.com/sorry` (CAPTCHA) or `consent.google.com` is detected and reported.
- Endpoint chain: `translate.googleapis.com/translate_a/single` (client `gtx`), the same path on
  `translate.google.com`, then `clients5.google.com/translate_a/t` (client `dict-chrome-ex`). A
  later endpoint is tried after any failure of the previous one (block, 429, consent, 400 — with
  POST a 400 means this endpoint / client id was rejected, not that the text is too long — 5xx,
  network error, unparsable 200).
- Rate limits (`crate::mt_limits`): every request takes a permit of its endpoint's route from the
  run's `MtCall` (app-wide pacing, one Google request in flight) and finishes it with a typed
  `MtSignal`. A 429 / CAPTCHA cools the WHOLE route (both gtx hosts), so the other gtx host is
  skipped without a request and the text goes on to `clients5`; the limiter decides when the
  route is probed again. When every route cools, the text fails with
  `MtItemErrorKind::CoolingDown` (the run loop stops the run). The provider itself never sleeps,
  retries on a timer or throttles.
- Endpoint memory: the session remembers where the next walk starts, advancing only past
  endpoints that failed for endpoint-specific reasons (400, other HTTP status, format, consent, network).
  Limit failures never move it: route health is the limiter's, so a gtx route leaving its
  cooldown is probed again instead of the session staying on `clients5`.
- Transport: `http::build_agent` (shared timeouts) with redirects disabled; `Retry-After` is read
  by `http::retry_after` and handed to the limiter.
- Errors are typed (`GoogleError`), localized for the user, and logged with endpoint host, HTTP
  status, text length and body length / kind (never the text or the body content).
- The native HTTP stack is not compiled for the web build, so the wasm target implements the same
  contract with a body that returns a clear "unavailable on web" error instead of a fake
  translation.
*/

use super::{MachineTranslatorBackend, MtItemError};
use crate::mt_limits::call::MtCall;

/// Google Translate provider over Google's free web JSON endpoints.
#[derive(Debug, Default, Clone, Copy)]
pub struct GoogleMtBackend;

/// Web stub: Google Translate uses native blocking HTTP (`ureq`), which is not available in the
/// browser build. The whole batch fails with a clear message so no fake translation is produced.
#[cfg(target_arch = "wasm32")]
impl MachineTranslatorBackend for GoogleMtBackend {
    fn translate_texts(
        &self,
        _call: &MtCall<'_>,
        _source_lang: &str,
        _target_lang: &str,
        _texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        Err(t!("translation.mt.google.web_unavailable_error").to_string())
    }
}

/// Web stub of the per-run session: every call fails like `GoogleMtBackend` on wasm.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Default)]
pub struct GoogleMtSession;

#[cfg(target_arch = "wasm32")]
impl MachineTranslatorBackend for GoogleMtSession {
    fn translate_texts(
        &self,
        _call: &MtCall<'_>,
        _source_lang: &str,
        _target_lang: &str,
        _texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        Err(t!("translation.mt.google.web_unavailable_error").to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::GoogleMtSession;
// The language-code spelling has one owner per service; the browser provider (`browser.rs`)
// reuses it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{LangRole, normalize_google_lang};

/// The backend of one Translation run (`super::run_session`): a `GoogleMtSession`, so the
/// endpoint memory and the connection pool span every bubble of the run.
pub(crate) fn run_session() -> Box<dyn MachineTranslatorBackend> {
    Box::new(GoogleMtSession::default())
}

// The native implementation depends on `ureq` (native HTTP), which is not compiled for wasm.
// The whole native surface lives in this module so the cfg boundary is a single point.
#[cfg(not(target_arch = "wasm32"))]
mod native {
use super::{GoogleMtBackend, MachineTranslatorBackend, MtCall, MtItemError};
use crate::machine_translation::MtService;
use crate::machine_translators::http as mt_http;
use crate::mt_limits::call::{MtAcquireError, MtSignal};
use crate::mt_limits::split::{join_chunks, split_for_request};
use crate::mt_limits::table::{MtLane, MtRoute, lane_limits};
use ms_log::runtime_log;
use serde_json::Value;
use std::cell::Cell;
use std::time::Duration;

/// A plain desktop browser UA: the free endpoints answer any UA today, but an empty one is the
/// most likely to be singled out.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Response layout of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseShape {
    /// `translate_a/single?dt=t`: `[[["seg", "src", ...], ...], null, "detected", ...]`.
    Single,
    /// `translate_a/t` (dict-chrome-ex): `["text"]`, `"text"` or `[["text", "detected"]]`.
    DictChromeEx,
}

/// One Google endpoint of the fallback chain.
#[derive(Debug)]
struct Endpoint {
    /// Full URL without query.
    url: &'static str,
    /// Host name, for logs.
    host: &'static str,
    /// Value of the `client` query parameter the endpoint accepts without a token.
    client: &'static str,
    /// Whether the endpoint takes `dt=t` (translation segments only).
    wants_dt: bool,
    shape: ResponseShape,
    /// The limiter route the endpoint is throttled on (both gtx hosts share one).
    route: MtRoute,
}

/// The endpoint chain, primary first. Verified live on 2026-10-22: all three answer `200` with
/// JSON while the mobile HTML page `translate.google.com/m` redirects to the CAPTCHA page.
const ENDPOINTS: [Endpoint; 3] = [
    Endpoint {
        url: "https://translate.googleapis.com/translate_a/single",
        host: "translate.googleapis.com",
        client: "gtx",
        wants_dt: true,
        shape: ResponseShape::Single,
        route: MtRoute::GoogleGtx,
    },
    Endpoint {
        url: "https://translate.google.com/translate_a/single",
        host: "translate.google.com",
        client: "gtx",
        wants_dt: true,
        shape: ResponseShape::Single,
        route: MtRoute::GoogleGtx,
    },
    Endpoint {
        url: "https://clients5.google.com/translate_a/t",
        host: "clients5.google.com",
        client: "dict-chrome-ex",
        wants_dt: false,
        shape: ResponseShape::DictChromeEx,
        route: MtRoute::GoogleClients5,
    },
];

/// Language codes the MT panel offers (`panels/machine_translation.rs`), mapped to Google's
/// spelling. Google answers an unknown `tl` with `200` and an untranslated or English text, so a
/// code outside this table is rejected instead of sent. `zh` is accepted as an alias.
const GOOGLE_LANGUAGES: &[(&str, &str)] = &[
    ("ru", "ru"),
    ("en", "en"),
    ("ko", "ko"),
    ("ja", "ja"),
    ("zh", "zh-CN"),
    ("zh-cn", "zh-CN"),
    ("zh-tw", "zh-TW"),
    ("es", "es"),
    ("fr", "fr"),
    ("de", "de"),
    ("it", "it"),
    ("pt", "pt"),
    ("pl", "pl"),
    ("tr", "tr"),
    ("uk", "uk"),
    ("ar", "ar"),
    ("hi", "hi"),
    ("id", "id"),
    ("th", "th"),
    ("vi", "vi"),
];

/// Which side of the language pair a code is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LangRole {
    /// Source: empty means `auto`, and `auto` is allowed.
    Source,
    /// Target: empty means `ru`; `auto` is not a target.
    Target,
}

/// One HTTP answer, whatever its status (redirects are not followed, so 3xx arrive here too).
#[derive(Debug, PartialEq, Eq)]
struct HttpReply {
    status: u16,
    /// The `Location` header of a redirect, when present.
    location: Option<String>,
    /// The server's delta-seconds `Retry-After`, when present (`http::retry_after`).
    retry_after: Option<Duration>,
    body: String,
}

/// The HTTP seam: one form POST. `Err` carries a transport failure description (connect, DNS,
/// timeout, body read); any HTTP status, including 4xx / 5xx / 3xx, is an `Ok` reply.
trait GoogleHttp {
    fn post_form(&self, url: &str, query: &[(&str, &str)], form: &[(&str, &str)]) -> Result<HttpReply, String>;
}

/// Production seam: a `ureq` agent with the shared MT timeouts and redirects disabled.
#[derive(Debug)]
struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    fn new() -> Self {
        // Redirects off: a redirect to the CAPTCHA or consent page must be classified, not
        // followed into an HTML page.
        Self { agent: mt_http::build_agent(USER_AGENT, false) }
    }
}

impl GoogleHttp for UreqHttp {
    fn post_form(&self, url: &str, query: &[(&str, &str)], form: &[(&str, &str)]) -> Result<HttpReply, String> {
        let request = query
            .iter()
            .fold(self.agent.post(url), |request, (name, value)| request.query(name, value));
        let response = match request.send_form(form) {
            Ok(response) => response,
            // ureq reports 4xx / 5xx as an error that still carries the response.
            Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(err)) => return Err(err.to_string()),
        };
        let status = response.status();
        let location = response.header("location").map(str::to_string);
        let retry_after = mt_http::retry_after(&response);
        let body = response
            .into_string()
            .map_err(|err| format!("reading the HTTP {status} response body failed: {err}"))?;
        Ok(HttpReply { status, location, retry_after, body })
    }
}

/// Why one Google request (or the whole chain) failed. `user_message` is the localized text the
/// MT panel shows for the bubble.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GoogleError {
    /// The panel sent a language code outside `GOOGLE_LANGUAGES` (holds the code as sent).
    UnsupportedLanguage(String),
    /// Google's "unusual traffic" block: redirect to `/sorry`, a CAPTCHA page, or HTTP 403.
    Blocked,
    /// HTTP 429 without a CAPTCHA page.
    RateLimited,
    /// Redirect to `consent.google.com` (EU cookie consent), which cannot be accepted here.
    ConsentRequired,
    /// HTTP 400: the endpoint rejected the request (with POST most likely the client id).
    BadRequest,
    /// A `200` whose body is not the expected JSON shape (Google changed the format).
    UnexpectedFormat,
    /// Any other status (5xx, an unexpected redirect, 404, ...).
    HttpStatus(u16),
    /// Transport failure (connect, DNS, timeout, body read) with its description.
    Network(String),
    /// No request was sent: the limiter refused the endpoint's route. `CoolingDown` on one
    /// endpoint only skips it; after a walk it means every Google route cools (the run stops).
    /// `Cancelled` (Stop) and `Busy` end the walk.
    Limiter(MtAcquireError),
}

impl GoogleError {
    /// Whether the next endpoint of the chain should be tried after this failure. A local
    /// language error, Stop and a busy service stop it; every answer from Google is
    /// endpoint-specific (a 400 included: the text travels in the POST body, so a 400 points at
    /// the endpoint / client id), and a cooling route is simply skipped.
    fn falls_through(&self) -> bool {
        match self {
            Self::UnsupportedLanguage(_) | Self::Limiter(MtAcquireError::Cancelled | MtAcquireError::Busy) => false,
            Self::BadRequest
            | Self::Blocked
            | Self::RateLimited
            | Self::ConsentRequired
            | Self::UnexpectedFormat
            | Self::HttpStatus(_)
            | Self::Network(_)
            | Self::Limiter(MtAcquireError::CoolingDown { .. }) => true,
        }
    }

    /// Whether this failure belongs to the endpoint itself (its host, client id or answer
    /// format), so later walks may start past it. Limit answers and limiter refusals are route
    /// health, which the limiter owns: they never move the session's starting endpoint.
    fn is_endpoint_specific(&self) -> bool {
        match self {
            Self::BadRequest | Self::ConsentRequired | Self::UnexpectedFormat | Self::HttpStatus(_) | Self::Network(_) => true,
            Self::UnsupportedLanguage(_) | Self::Blocked | Self::RateLimited | Self::Limiter(_) => false,
        }
    }

    /// How informative the error is when every endpoint failed: the highest rank is reported, so
    /// a block on the primary is not hidden behind a later endpoint's network error. A skipped
    /// endpoint (no request) ranks lowest.
    fn rank(&self) -> u8 {
        match self {
            Self::Limiter(MtAcquireError::CoolingDown { .. }) => 0,
            Self::Network(_) => 1,
            Self::HttpStatus(_) => 2,
            Self::UnexpectedFormat => 3,
            Self::ConsentRequired => 4,
            Self::RateLimited => 5,
            Self::Blocked => 6,
            Self::UnsupportedLanguage(_) | Self::BadRequest | Self::Limiter(MtAcquireError::Cancelled | MtAcquireError::Busy) => 7,
        }
    }

    /// Localized, user-readable message.
    pub(crate) fn user_message(&self) -> String {
        match self {
            Self::UnsupportedLanguage(lang) => tf!("translation.mt.google.unsupported_language_error", lang = lang),
            Self::Blocked => t!("translation.mt.google.blocked_error").to_string(),
            Self::RateLimited => t!("translation.mt.google.rate_limited_error").to_string(),
            Self::ConsentRequired => t!("translation.mt.google.consent_required_error").to_string(),
            Self::BadRequest => t!("translation.mt.google.bad_request_error").to_string(),
            Self::UnexpectedFormat => t!("translation.mt.google.unexpected_format_error").to_string(),
            Self::HttpStatus(status) => tf!("translation.mt.google.http_status_error", status = status),
            Self::Network(err) => tf!("translation.mt.google.network_error", err = err),
            Self::Limiter(err) => MtItemError::from_acquire(*err, MtService::Google).message,
        }
    }

    /// The text's failure as the run loop sees it: limiter refusals keep their kind
    /// (`Cancelled`, `CoolingDown` stop the run), everything else is an ordinary failure.
    fn into_item_error(self) -> MtItemError {
        match self {
            Self::Limiter(err) => MtItemError::from_acquire(err, MtService::Google),
            Self::UnsupportedLanguage(_)
            | Self::Blocked
            | Self::RateLimited
            | Self::ConsentRequired
            | Self::BadRequest
            | Self::UnexpectedFormat
            | Self::HttpStatus(_)
            | Self::Network(_) => MtItemError::failed(self.user_message()),
        }
    }
}

/// A successful translation.
#[derive(Debug, PartialEq, Eq)]
struct Translation {
    text: String,
    /// Source language Google detected (`auto`) or echoed, when the endpoint reports it.
    detected_source: Option<String>,
}

/// One MT run's Google client: a pooled HTTP agent plus the index into `ENDPOINTS` where the
/// next walk starts. Build it once per run and call it for every text of the run, so an endpoint
/// that keeps failing for its own reasons is skipped after the first failure. Route health
/// (rate limits) is NOT kept here but in the shared limiter. Single-threaded (`Cell`): it lives
/// on the run's worker thread.
#[derive(Debug)]
pub struct GoogleMtSession {
    http: UreqHttp,
    preferred: Cell<usize>,
}

impl Default for GoogleMtSession {
    fn default() -> Self {
        Self { http: UreqHttp::new(), preferred: Cell::new(0) }
    }
}

impl MachineTranslatorBackend for GoogleMtSession {
    fn translate_texts(
        &self,
        call: &MtCall<'_>,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        translate_texts_with(&self.http, &self.preferred, call, source_lang, target_lang, &texts)
    }
}

impl MachineTranslatorBackend for GoogleMtBackend {
    /// Stateless path for callers without a run (the shared dispatcher): a fresh session, so the
    /// endpoint memory lasts only for this call (route health still comes from `call`'s limiter).
    fn translate_texts(
        &self,
        call: &MtCall<'_>,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        GoogleMtSession::default().translate_texts(call, source_lang, target_lang, texts)
    }
}

/// `translate_texts` over an injected HTTP seam and endpoint memory. A language outside the
/// supported table is the outer `Err` (nothing can be translated); every other failure is that
/// text's own `Err`. Once `call` is cancelled, every remaining text is `Cancelled` and nothing
/// more is sent.
fn translate_texts_with(
    http: &dyn GoogleHttp,
    preferred: &Cell<usize>,
    call: &MtCall<'_>,
    source_lang: &str,
    target_lang: &str,
    texts: &[String],
) -> Result<Vec<Result<String, MtItemError>>, String> {
    let source = normalize_google_lang(source_lang, LangRole::Source).map_err(|err| err.user_message())?;
    let target = normalize_google_lang(target_lang, LangRole::Target).map_err(|err| err.user_message())?;
    Ok(texts
        .iter()
        .map(|text| {
            if call.is_cancelled() {
                return Err(GoogleError::Limiter(MtAcquireError::Cancelled).into_item_error());
            }
            let source_text = text.trim();
            if source_text.is_empty() {
                return Ok(String::new());
            }
            translate_text(http, preferred, call, source, target, source_text).map_err(GoogleError::into_item_error)
        })
        .collect())
}

/// Translates one non-empty text. A text longer than Google's `max_request_chars` is split by
/// `split_for_request` into several requests whose translations are joined with the original
/// separators; the text fails as a whole (no partial result) when any chunk fails.
fn translate_text(
    http: &dyn GoogleHttp,
    preferred: &Cell<usize>,
    call: &MtCall<'_>,
    source: &str,
    target: &str,
    text: &str,
) -> Result<String, GoogleError> {
    let text_chars = text.chars().count();
    let Some(max_chars) = lane_limits(MtLane::builtin(MtService::Google)).max_request_chars.filter(|&max_chars| text_chars > max_chars) else {
        return translate_one(http, preferred, call, source, target, text).map(|translation| translation.text);
    };
    let chunks = split_for_request(text, max_chars);
    runtime_log::log_info(format!(
        "[MT][Google] text of {text_chars} chars exceeds the {max_chars}-char request limit; sending it as {} requests",
        chunks.len()
    ));
    let translated = chunks
        .iter()
        .map(|chunk| translate_chunk(http, preferred, call, source, target, &chunk.text))
        .collect::<Result<Vec<String>, GoogleError>>()?;
    Ok(join_chunks(translated.iter().map(String::as_str).zip(chunks.iter().map(|chunk| chunk.separator.as_str()))))
}

/// Translates one chunk of a split text, keeping the chunk's own leading and trailing
/// whitespace around the translation; a blank chunk (empty lines) is passed through unsent.
fn translate_chunk(
    http: &dyn GoogleHttp,
    preferred: &Cell<usize>,
    call: &MtCall<'_>,
    source: &str,
    target: &str,
    chunk: &str,
) -> Result<String, GoogleError> {
    let core = chunk.trim();
    if core.is_empty() {
        return Ok(chunk.to_string());
    }
    // `trim_start` / `trim` remove whole chars, so both offsets are char boundaries.
    let lead_len = chunk.len() - chunk.trim_start().len();
    let (lead, rest) = chunk.split_at(lead_len);
    let trail = &rest[core.len()..];
    let translation = translate_one(http, preferred, call, source, target, core)?;
    Ok(format!("{lead}{}{trail}", translation.text))
}

/// Translates one request text, walking the endpoint chain from `preferred` (wrapping around).
/// On success `preferred` becomes the first endpoint of this walk that did not fail for
/// endpoint-specific reasons (`GoogleError::is_endpoint_specific`), so a route skipped for a
/// cooldown is tried first again once the limiter lets it through. On failure returns the
/// stopping error; when every route of the service cools, `Limiter(CoolingDown)` with the time
/// until the first one is usable; otherwise the highest-`rank` error.
fn translate_one(
    http: &dyn GoogleHttp,
    preferred: &Cell<usize>,
    call: &MtCall<'_>,
    source: &str,
    target: &str,
    text: &str,
) -> Result<Translation, GoogleError> {
    let start = preferred.get().min(ENDPOINTS.len() - 1);
    let mut next_start: Option<usize> = None;
    let mut request_failed = false;
    let mut worst: Option<GoogleError> = None;
    for offset in 0..ENDPOINTS.len() {
        let index = (start + offset) % ENDPOINTS.len();
        let endpoint = &ENDPOINTS[index];
        match request_endpoint(http, call, endpoint, source, target, text) {
            Ok(translation) => {
                if request_failed {
                    runtime_log::log_info(format!(
                        "[MT][Google] fallback endpoint {} answered (detected source: {})",
                        endpoint.host,
                        translation.detected_source.as_deref().unwrap_or("-"),
                    ));
                }
                preferred.set(next_start.unwrap_or(index));
                return Ok(translation);
            }
            Err(err) => {
                if !err.falls_through() {
                    return Err(err);
                }
                if next_start.is_none() && !err.is_endpoint_specific() {
                    next_start = Some(index);
                }
                // Only an answered (or attempted) request is a fallback worth logging; a skipped
                // cooling route sent nothing.
                request_failed |= !matches!(err, GoogleError::Limiter(_));
                if worst.as_ref().is_none_or(|current| err.rank() > current.rank()) {
                    worst = Some(err);
                }
            }
        }
    }
    if let Some(retry_in) = call.lane_cooldown(MtLane::builtin(MtService::Google)) {
        runtime_log::log_warn(format!(
            "[MT][Google] every Google route is cooling down after rate limits; usable again in {} s",
            retry_in.as_secs()
        ));
        return Err(GoogleError::Limiter(MtAcquireError::CoolingDown { retry_in }));
    }
    // `ENDPOINTS` is non-empty and every non-stopping failure sets `worst`, so the fallback
    // value is unreachable; it only avoids a panic path. A walk of skips only (a route that
    // left its cooldown between the skip and the check above) reports the skip itself.
    Err(worst.unwrap_or(GoogleError::UnexpectedFormat))
}

/// Sends one request to `endpoint` under a limiter permit of its route and classifies the
/// answer; the permit is finished with `limit_signal`. A refused permit sends nothing and is
/// `GoogleError::Limiter`. Every failure is logged with the host, status, text length and the
/// body's length and kind; never the text or body content (Google's JSON echoes the source text).
fn request_endpoint(
    http: &dyn GoogleHttp,
    call: &MtCall<'_>,
    endpoint: &Endpoint,
    source: &str,
    target: &str,
    text: &str,
) -> Result<Translation, GoogleError> {
    let permit = call.acquire(endpoint.route).map_err(GoogleError::Limiter)?;
    let mut query: Vec<(&str, &str)> = vec![("client", endpoint.client), ("sl", source), ("tl", target)];
    if endpoint.wants_dt {
        query.push(("dt", "t"));
    }
    query.extend([("ie", "UTF-8"), ("oe", "UTF-8")]);
    let text_chars = text.chars().count();
    let reply = match http.post_form(endpoint.url, &query, &[("q", text)]) {
        Ok(reply) => reply,
        Err(detail) => {
            permit.finish(MtSignal::Neutral);
            runtime_log::log_warn(format!(
                "[MT][Google] {} request failed: network error: {detail} (text {text_chars} chars, {source}->{target})",
                endpoint.host
            ));
            return Err(GoogleError::Network(detail));
        }
    };
    let result = classify_reply(&reply, endpoint.shape);
    permit.finish(limit_signal(&result, reply.retry_after));
    if let Err(err) = &result {
        runtime_log::log_warn(format!(
            "[MT][Google] {} answered HTTP {} -> {err:?} (text {text_chars} chars, {source}->{target}, redirect host: {}, retry-after: {}, body {} bytes, {})",
            endpoint.host,
            reply.status,
            reply.location.as_deref().map_or("-", redirect_host),
            reply.retry_after.map_or_else(|| "-".to_string(), |wait| format!("{} s", wait.as_secs())),
            reply.body.len(),
            body_kind(&reply.body),
        ));
    }
    result
}

/// What one classified answer tells the limiter: a block or a 429 cools the route (with the
/// server's `Retry-After`), a translation is `Ok`, anything else says nothing about the rate.
fn limit_signal(result: &Result<Translation, GoogleError>, retry_after: Option<Duration>) -> MtSignal {
    match result {
        Ok(_) => MtSignal::Ok,
        Err(GoogleError::Blocked) => MtSignal::Blocked { retry_after },
        Err(GoogleError::RateLimited) => MtSignal::RateLimited { retry_after },
        Err(
            GoogleError::UnsupportedLanguage(_)
            | GoogleError::ConsentRequired
            | GoogleError::BadRequest
            | GoogleError::UnexpectedFormat
            | GoogleError::HttpStatus(_)
            | GoogleError::Network(_)
            | GoogleError::Limiter(_),
        ) => MtSignal::Neutral,
    }
}

/// Maps one HTTP answer to a translation or a `GoogleError`.
fn classify_reply(reply: &HttpReply, shape: ResponseShape) -> Result<Translation, GoogleError> {
    match reply.status {
        200..=299 => {
            if looks_like_block_page(&reply.body) {
                return Err(GoogleError::Blocked);
            }
            let parsed = match shape {
                ResponseShape::Single => parse_single(&reply.body),
                ResponseShape::DictChromeEx => parse_dict_chrome_ex(&reply.body),
            };
            parsed.ok_or(GoogleError::UnexpectedFormat)
        }
        300..=399 => {
            let location = reply.location.as_deref().unwrap_or("");
            if location.contains("consent.google.") {
                Err(GoogleError::ConsentRequired)
            } else if location.contains("/sorry") {
                Err(GoogleError::Blocked)
            } else {
                Err(GoogleError::HttpStatus(reply.status))
            }
        }
        400 => Err(GoogleError::BadRequest),
        403 => Err(GoogleError::Blocked),
        429 if looks_like_block_page(&reply.body) => Err(GoogleError::Blocked),
        429 => Err(GoogleError::RateLimited),
        _ => Err(GoogleError::HttpStatus(reply.status)),
    }
}

/// Host of a redirect target, for logs (its query can carry the original URL).
fn redirect_host(location: &str) -> &str {
    let after_scheme = location.split_once("://").map_or(location, |(_, rest)| rest);
    after_scheme.split(['/', '?']).next().unwrap_or(after_scheme)
}

/// A content-free description of a body for logs.
fn body_kind(body: &str) -> &'static str {
    match body.trim_start().chars().next() {
        None => "empty",
        Some('<') => "html",
        Some('[' | '{' | '"') => "json-like",
        Some(_) => "other",
    }
}

/// Markers of Google's block pages (lower-case). `<title>sorry...</title>` is the observed plain
/// HTTP 429 page of the gtx hosts (no `Retry-After`, 2026-10-22); the others are the CAPTCHA
/// ("unusual traffic", "automated queries") variants.
const BLOCK_PAGE_MARKERS: &[&str] = &["<title>sorry...</title>", "unusual traffic", "automated queries", "captcha", "/sorry/"];

/// Whether a body is Google's "unusual traffic" / CAPTCHA / "Sorry..." page rather than API JSON.
fn looks_like_block_page(body: &str) -> bool {
    // The block pages state their reason near the top; a bounded prefix keeps this cheap.
    let lower: String = body.trim_start().chars().take(4096).collect::<String>().to_ascii_lowercase();
    lower.starts_with('<') && BLOCK_PAGE_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Parses a `translate_a/single?dt=t` body: the translation is the concatenation (with "") of the
/// string at index 0 of every segment in `v[0]` (null segments skipped; newlines live inside the
/// segments), the detected source is `v[2]`. `None` when the result is blank: callers send only
/// non-blank text, and Google echoes untranslatable text (`"!!!"`, `"…"`, digits) rather than
/// answering empty (checked live 2026-10-22), so a blank result is a format change.
fn parse_single(body: &str) -> Option<Translation> {
    let value: Value = serde_json::from_str(body).ok()?;
    let segments = value.get(0)?.as_array()?;
    let text: String = segments.iter().filter_map(|segment| segment.get(0).and_then(Value::as_str)).collect();
    let detected_source = value.get(2).and_then(Value::as_str).map(str::to_string);
    non_blank(Translation { text, detected_source })
}

/// `Some` only for a non-blank translation (see `parse_single` for why blank is a failure).
fn non_blank(translation: Translation) -> Option<Translation> {
    (!translation.text.trim().is_empty()).then_some(translation)
}

/// Parses a `clients5 translate_a/t` body. With an explicit `sl` Google answers `["text"]`, with
/// `sl=auto` it answers `[["text","detected"]]`; a bare `"text"` is accepted too. `None` for a
/// blank result, as in `parse_single`.
fn parse_dict_chrome_ex(body: &str) -> Option<Translation> {
    let value: Value = serde_json::from_str(body).ok()?;
    let translation = if let Some(text) = value.as_str() {
        Translation { text: text.to_string(), detected_source: None }
    } else {
        let first = value.get(0)?;
        if let Some(text) = first.as_str() {
            Translation { text: text.to_string(), detected_source: None }
        } else {
            let text = first.get(0)?.as_str()?.to_string();
            let detected_source = first.get(1).and_then(Value::as_str).map(str::to_string);
            Translation { text, detected_source }
        }
    };
    non_blank(translation)
}

/// Maps a panel language code to Google's spelling. Empty means `auto` for the source and `ru`
/// for the target (the panel's defaults). `auto` is valid only as a source. Shared with the
/// browser provider (`browser.rs`): the Google web page takes the same spelling.
///
/// # Errors
/// `GoogleError::UnsupportedLanguage` for a code outside `GOOGLE_LANGUAGES`.
pub(crate) fn normalize_google_lang(raw: &str, role: LangRole) -> Result<&'static str, GoogleError> {
    let code = raw.trim().to_ascii_lowercase();
    match (code.as_str(), role) {
        ("", LangRole::Source) | ("auto", LangRole::Source) => return Ok("auto"),
        ("", LangRole::Target) => return Ok("ru"),
        _ => {}
    }
    GOOGLE_LANGUAGES
        .iter()
        .find(|(ui, _)| *ui == code)
        .map(|(_, google)| *google)
        .ok_or_else(|| GoogleError::UnsupportedLanguage(raw.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine_translators::MtItemErrorKind;
    use crate::mt_limits::call::MtWaitStatus;
    use crate::mt_limits::registry::{MtClock, MtLimiterRegistry};
    use crate::mt_limits::test_support::FakeClock;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Scripted seam: answers queued replies in order and records every requested URL. With
    /// `cancel_after`, it sets the run's cancel flag while serving the n-th request (Stop pressed
    /// mid-request).
    struct FakeHttp {
        replies: RefCell<VecDeque<Result<HttpReply, String>>>,
        urls: RefCell<Vec<String>>,
        forms: RefCell<Vec<String>>,
        cancel_after: Option<(usize, Arc<AtomicBool>)>,
    }

    impl FakeHttp {
        fn new(replies: Vec<Result<HttpReply, String>>) -> Self {
            Self { replies: RefCell::new(replies.into()), urls: RefCell::new(Vec::new()), forms: RefCell::new(Vec::new()), cancel_after: None }
        }

        fn cancel_after(mut self, requests: usize, cancel: Arc<AtomicBool>) -> Self {
            self.cancel_after = Some((requests, cancel));
            self
        }

        fn urls(&self) -> Vec<String> {
            self.urls.borrow().clone()
        }

        /// Requested URLs without their query.
        fn paths(&self) -> Vec<String> {
            self.urls().iter().map(|url| url.split('?').next().unwrap_or_default().to_string()).collect()
        }
    }

    impl GoogleHttp for FakeHttp {
        fn post_form(&self, url: &str, query: &[(&str, &str)], form: &[(&str, &str)]) -> Result<HttpReply, String> {
            let query: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            self.urls.borrow_mut().push(format!("{url}?{}", query.join("&")));
            self.forms.borrow_mut().extend(form.iter().map(|(k, v)| format!("{k}={v}")));
            if let Some((requests, cancel)) = &self.cancel_after
                && self.urls.borrow().len() == *requests
            {
                cancel.store(true, Ordering::Relaxed);
            }
            self.replies.borrow_mut().pop_front().unwrap_or_else(|| Err("fake: no scripted reply".to_string()))
        }
    }

    fn ignore_wait(_status: Option<MtWaitStatus>) {}

    /// A private limiter on a fake clock plus a run's cancel flag: each test starts with fresh
    /// route health, and waits advance the fake clock instead of sleeping.
    struct Limits {
        clock: Arc<FakeClock>,
        registry: Arc<MtLimiterRegistry>,
        cancel: Arc<AtomicBool>,
    }

    impl Limits {
        fn new() -> Self {
            let (clock, registry) = FakeClock::registry();
            Self { clock, registry, cancel: Arc::new(AtomicBool::new(false)) }
        }

        fn call(&self) -> MtCall<'_> {
            MtCall::for_run(&self.registry, &self.cancel, &ignore_wait)
        }

        /// Runs `step` and returns its result with the fake time it spent waiting.
        fn timed<T>(&self, step: impl FnOnce() -> T) -> (T, Duration) {
            let before = self.clock.now();
            let out = step();
            (out, self.clock.now() - before)
        }
    }

    /// The kind of every failed item (`None` for a translated one).
    fn kinds(out: &[Result<String, MtItemError>]) -> Vec<Option<MtItemErrorKind>> {
        out.iter().map(|result| result.as_ref().err().map(|err| err.kind)).collect()
    }

    fn ok(body: &str) -> Result<HttpReply, String> {
        Ok(HttpReply { status: 200, location: None, retry_after: None, body: body.to_string() })
    }

    fn status(status: u16, body: &str) -> Result<HttpReply, String> {
        Ok(HttpReply { status, location: None, retry_after: None, body: body.to_string() })
    }

    fn redirect(location: &str) -> Result<HttpReply, String> {
        Ok(HttpReply { status: 302, location: Some(location.to_string()), retry_after: None, body: String::new() })
    }

    const SINGLE_OK: &str = r#"[[["Привет","こんにちは",null,null,10]],null,"ja"]"#;
    const CAPTCHA_HTML: &str = "<!DOCTYPE html><html><body>Our systems have detected unusual traffic from your computer network.</body></html>";
    /// The start of the plain 429 page the gtx hosts answered during the measured block
    /// (1103 bytes, no `Retry-After`).
    const SORRY_429: &str = r#"<html><head><meta http-equiv="content-type" content="text/html; charset=utf-8"/><title>Sorry...</title><style> body { font-family: verdana, arial, sans-serif; background-color: #fff; }</style></head><body></body></html>"#;
    const GAPIS: &str = "https://translate.googleapis.com/translate_a/single";
    const GCOM: &str = "https://translate.google.com/translate_a/single";
    const CLIENTS5: &str = "https://clients5.google.com/translate_a/t";
    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn parse_single_concatenates_segments_keeps_newlines_and_skips_nulls() {
        let body = r#"[[["Привет.\n","Hi.\n",null,null,3],null,["Как дела?","How are you?"],[null,null,"Privet"]],null,"en",null]"#;
        let parsed = parse_single(body).expect("valid shape");
        assert_eq!(parsed.text, "Привет.\nКак дела?");
        assert_eq!(parsed.detected_source.as_deref(), Some("en"));
    }

    #[test]
    fn parse_single_rejects_bodies_without_segment_text() {
        assert_eq!(parse_single("[null,null,\"ja\"]"), None);
        assert_eq!(parse_single("[[[null]],null,\"ja\"]"), None);
        assert_eq!(parse_single("<html>"), None);
        assert_eq!(parse_single("{\"sentences\":[]}"), None);
    }

    #[test]
    fn blank_results_are_a_format_failure_and_echoed_symbols_are_kept() {
        assert_eq!(parse_single(r#"[[["","!!!"]],null,"en"]"#), None);
        assert_eq!(parse_single(r#"[[[" \n","x"]],null,"en"]"#), None);
        assert_eq!(parse_dict_chrome_ex(r#"[["","en"]]"#), None);
        assert_eq!(parse_dict_chrome_ex(r#"[""]"#), None);
        // Live shape for symbol-only input: Google echoes it, which is a valid translation.
        let echoed = parse_single(r#"[[["!!!","!!!",null,null,3]],null,"en"]"#).expect("echo");
        assert_eq!(echoed.text, "!!!");
    }

    #[test]
    fn parse_dict_chrome_ex_accepts_every_known_shape() {
        let plain = parse_dict_chrome_ex(r#"["Привет, как дела?"]"#).expect("explicit-sl shape");
        assert_eq!(plain, Translation { text: "Привет, как дела?".to_string(), detected_source: None });
        let auto = parse_dict_chrome_ex(r#"[["Привет","ja"]]"#).expect("auto shape");
        assert_eq!(auto, Translation { text: "Привет".to_string(), detected_source: Some("ja".to_string()) });
        let bare = parse_dict_chrome_ex(r#""Привет""#).expect("bare string");
        assert_eq!(bare.text, "Привет");
        assert_eq!(parse_dict_chrome_ex("[]"), None);
        assert_eq!(parse_dict_chrome_ex("[[1,2]]"), None);
    }

    #[test]
    fn primary_success_sends_post_form_with_mapped_codes() {
        let limits = Limits::new();
        let http = FakeHttp::new(vec![ok(SINGLE_OK)]);
        let out = translate_texts_with(&http, &Cell::new(0), &limits.call(), "auto", "zh-cn", &["こんにちは".to_string()]).expect("provider runs");
        assert_eq!(out, vec![Ok("Привет".to_string())]);
        assert_eq!(http.urls(), vec![format!("{GAPIS}?client=gtx&sl=auto&tl=zh-CN&dt=t&ie=UTF-8&oe=UTF-8")]);
        assert_eq!(*http.forms.borrow(), vec!["q=こんにちは".to_string()]);
    }

    #[test]
    fn empty_and_whitespace_items_skip_the_network() {
        let limits = Limits::new();
        let http = FakeHttp::new(Vec::new());
        let out = translate_texts_with(&http, &Cell::new(0), &limits.call(), "ja", "ru", &[String::new(), " \n\t".to_string()]).expect("provider runs");
        assert_eq!(out, vec![Ok(String::new()), Ok(String::new())]);
        assert!(http.urls().is_empty());
    }

    /// A block on googleapis cools the whole gtx route: google.com is skipped without a
    /// request. When clients5 is blocked too, the text (and the next one, without any request)
    /// fails as `CoolingDown` until the first route is usable again.
    #[test]
    fn every_route_cooling_is_a_cooldown_error_without_more_requests() {
        let limits = Limits::new();
        let sorry = "https://www.google.com/sorry/index?continue=x";
        let http = FakeHttp::new(vec![redirect(sorry), status(429, CAPTCHA_HTML)]);
        let preferred = Cell::new(0);
        let out = translate_texts_with(&http, &preferred, &limits.call(), "auto", "ru", &["hi".to_string(), "again".to_string()]).expect("provider runs");
        assert_eq!(http.paths(), vec![GAPIS, CLIENTS5]);
        // gtx cooled at t0 for 5 min; clients5 was asked 1 s later (no burst while throttled).
        let retry_in = Duration::from_secs(299);
        assert_eq!(kinds(&out), vec![Some(MtItemErrorKind::CoolingDown { retry_in }); 2]);
        assert_eq!(preferred.get(), 0);
    }

    #[test]
    fn consent_redirect_is_reported_as_consent() {
        let limits = Limits::new();
        let consent = "https://consent.google.com/ml?continue=x";
        let http = FakeHttp::new(vec![redirect(consent), redirect(consent), redirect(consent)]);
        let err = translate_one(&http, &Cell::new(0), &limits.call(), "auto", "ru", "hi").expect_err("consent");
        assert_eq!(err, GoogleError::ConsentRequired);
        // Consent says nothing about the rate: every endpoint was asked.
        assert_eq!(http.urls().len(), 3);
    }

    /// The observed block: googleapis answers the plain "Sorry..." 429. The session goes on to
    /// clients5 at the normal 1 s pace, keeps starting its walks on googleapis, probes it once
    /// the 5 min cooldown ends, and gets the burst back after 3 successful probes.
    #[test]
    fn sorry_429_switches_to_clients5_and_the_gtx_route_recovers_after_probes() {
        let limits = Limits::new();
        let call = limits.call();
        let preferred = Cell::new(0);
        let mut replies = vec![status(429, SORRY_429), ok(r#"[["Привет","ja"]]"#), ok(r#"["Пока"]"#)];
        replies.extend((0..7).map(|_| ok(SINGLE_OK)));
        let http = FakeHttp::new(replies);
        let translate = || translate_one(&http, &preferred, &call, "ja", "ru", "hi").map(|translation| translation.text);

        let (first, spent) = limits.timed(translate);
        assert_eq!(first, Ok("Привет".to_string()));
        assert_eq!(spent, SECOND);
        assert_eq!(http.paths(), vec![GAPIS, CLIENTS5]);
        assert_eq!(preferred.get(), 0, "a limit never moves the session's starting endpoint");

        let (second, spent) = limits.timed(translate);
        assert_eq!(second, Ok("Пока".to_string()));
        assert_eq!(spent, SECOND);
        assert_eq!(http.paths()[2..], [CLIENTS5]);

        limits.clock.advance(Duration::from_secs(300));
        let spent: Vec<Duration> = (0..3).map(|_| limits.timed(translate)).map(|(result, spent)| {
            assert_eq!(result, Ok("Привет".to_string()));
            spent
        }).collect();
        assert_eq!(spent, [Duration::ZERO, SECOND, SECOND]);
        assert_eq!(http.paths()[3..], [GAPIS, GAPIS, GAPIS]);

        limits.clock.advance(Duration::from_secs(10));
        let spent: Vec<Duration> = (0..4).map(|_| limits.timed(translate).1).collect();
        assert_eq!(spent, [Duration::ZERO, Duration::ZERO, Duration::ZERO, SECOND]);
        assert_eq!(http.urls().len(), 10);
    }

    /// The server's `Retry-After` reaches the limiter: a 15 min wait outlasts the 5 min step.
    #[test]
    fn retry_after_lengthens_the_route_cooldown() {
        let limits = Limits::new();
        let call = limits.call();
        let preferred = Cell::new(0);
        let limited = Ok(HttpReply { status: 429, location: None, retry_after: Some(Duration::from_secs(900)), body: String::new() });
        let http = FakeHttp::new(vec![limited, ok(r#"["a"]"#), ok(r#"["b"]"#), ok(SINGLE_OK)]);
        let translate = || translate_one(&http, &preferred, &call, "ja", "ru", "hi").map(|translation| translation.text);
        assert!(translate().is_ok());
        limits.clock.advance(Duration::from_secs(300));
        assert!(translate().is_ok());
        assert_eq!(http.paths(), vec![GAPIS, CLIENTS5, CLIENTS5]);
        limits.clock.advance(Duration::from_secs(600));
        assert!(translate().is_ok());
        assert_eq!(http.paths()[3..], [GAPIS]);
    }

    #[test]
    fn every_outcome_finishes_the_permit_with_its_signal() {
        let wait = Some(Duration::from_secs(7));
        let translated = Ok(Translation { text: "x".to_string(), detected_source: None });
        assert_eq!(limit_signal(&translated, wait), MtSignal::Ok);
        assert_eq!(limit_signal(&Err(GoogleError::Blocked), wait), MtSignal::Blocked { retry_after: wait });
        assert_eq!(limit_signal(&Err(GoogleError::RateLimited), wait), MtSignal::RateLimited { retry_after: wait });
        assert_eq!(limit_signal(&Err(GoogleError::RateLimited), None), MtSignal::RateLimited { retry_after: None });
        let neutral = [
            GoogleError::UnsupportedLanguage("xx".to_string()),
            GoogleError::ConsentRequired,
            GoogleError::BadRequest,
            GoogleError::UnexpectedFormat,
            GoogleError::HttpStatus(503),
            GoogleError::Network("reset".to_string()),
            GoogleError::Limiter(MtAcquireError::Busy),
        ];
        for err in neutral {
            assert_eq!(limit_signal(&Err(err), wait), MtSignal::Neutral);
        }
    }

    #[test]
    fn unparsable_200_falls_back_to_dict_chrome_ex() {
        let limits = Limits::new();
        let http = FakeHttp::new(vec![ok("<html>new layout</html>"), Err("connection reset".to_string()), ok(r#"[["Привет","ja"]]"#)]);
        let preferred = Cell::new(0);
        let translation = translate_one(&http, &preferred, &limits.call(), "auto", "ru", "こんにちは").expect("third endpoint answers");
        assert_eq!(translation, Translation { text: "Привет".to_string(), detected_source: Some("ja".to_string()) });
        assert_eq!(http.urls()[2], format!("{CLIENTS5}?client=dict-chrome-ex&sl=auto&tl=ru&ie=UTF-8&oe=UTF-8"));
        // Both failures were the endpoints' own, so later walks start at clients5.
        assert_eq!(preferred.get(), 2);
    }

    /// Mirrors the worker's production path: one session-like memory (`Cell`) shared by
    /// separate one-text calls, as `run_translate_request` makes them. A 5xx does not cool the
    /// route (google.com, the same route, is asked right after it).
    #[test]
    fn the_working_endpoint_is_remembered_across_one_text_calls() {
        let limits = Limits::new();
        let http = FakeHttp::new(vec![
            status(503, ""),
            ok(SINGLE_OK),
            ok(SINGLE_OK),
            // Third call: the remembered endpoint (index 1) fails, the chain wraps to 2.
            status(502, ""),
            ok(r#"["Пока"]"#),
        ]);
        let preferred = Cell::new(0);
        for expected in ["Привет", "Привет", "Пока"] {
            let out = translate_texts_with(&http, &preferred, &limits.call(), "ja", "ru", &["a".to_string()]).expect("provider runs");
            assert_eq!(out, vec![Ok(expected.to_string())]);
        }
        assert_eq!(http.paths(), vec![GAPIS, GCOM, GCOM, GCOM, CLIENTS5]);
        assert_eq!(preferred.get(), 2);
    }

    #[test]
    fn bad_request_falls_through_and_keeps_its_rank() {
        let limits = Limits::new();
        let http = FakeHttp::new(vec![status(400, "<html>Error 400 (Bad Request)</html>"), ok(SINGLE_OK)]);
        let translation = translate_one(&http, &Cell::new(0), &limits.call(), "ja", "ru", "hi").expect("next endpoint answers");
        assert_eq!(translation.text, "Привет");

        let limits = Limits::new();
        let http = FakeHttp::new(vec![status(400, ""), status(429, ""), Err("timed out".to_string())]);
        let err = translate_one(&http, &Cell::new(0), &limits.call(), "ja", "ru", "hi").expect_err("all failed");
        assert_eq!(err, GoogleError::BadRequest);
        assert_eq!(http.urls().len(), 3);
    }

    #[test]
    fn worst_error_wins_and_network_errors_are_per_item() {
        let limits = Limits::new();
        let http = FakeHttp::new(vec![Err("dns failed".to_string()), status(500, ""), Err("timed out".to_string())]);
        let err = translate_one(&http, &Cell::new(0), &limits.call(), "ja", "ru", "hi").expect_err("all failed");
        assert_eq!(err, GoogleError::HttpStatus(500));

        let http = FakeHttp::new(vec![Err("dns failed".to_string()), Err("dns failed".to_string()), Err("timed out".to_string())]);
        let out = translate_texts_with(&http, &Cell::new(0), &limits.call(), "ja", "ru", &["hi".to_string()]).expect("provider runs");
        assert_eq!(kinds(&out), vec![Some(MtItemErrorKind::Failed)]);
    }

    #[test]
    fn long_text_is_split_into_requests_and_joined_with_its_separators() {
        let line = "x".repeat(999);
        let mut text = String::new();
        for index in 0..12 {
            if index > 0 {
                text.push_str(if index == 5 { "\n\n" } else { "\n" });
            }
            text.push_str(&line);
        }
        // 12 lines of 999 chars + 10 single and 1 double newline: well over the 5000-char limit.
        assert_eq!(text.chars().count(), 12_000);
        let single = |translated: &str| ok(&format!(r#"[[["{translated}","x"]],null,"en"]"#));
        let limits = Limits::new();
        let http = FakeHttp::new(vec![single("Один"), single("Два"), single("Три")]);
        let out = translate_texts_with(&http, &Cell::new(0), &limits.call(), "en", "ru", std::slice::from_ref(&text)).expect("provider runs");
        assert_eq!(out, vec![Ok("Один\n\nДва\nТри".to_string())]);
        let sent: Vec<String> = http.forms.borrow().iter().map(|form| form.strip_prefix("q=").unwrap_or(form).to_string()).collect();
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|chunk| chunk.chars().count() <= 5000));
        assert_eq!(format!("{}\n\n{}\n{}", sent[0], sent[1], sent[2]), text);

        // A failing chunk fails the whole text, and later chunks are not sent.
        let limits = Limits::new();
        let reset = || Err("connection reset".to_string());
        let http = FakeHttp::new(vec![single("Один"), reset(), reset(), reset()]);
        let out = translate_texts_with(&http, &Cell::new(0), &limits.call(), "en", "ru", &[text]).expect("provider runs");
        assert_eq!(kinds(&out), vec![Some(MtItemErrorKind::Failed)]);
        assert_eq!(http.urls().len(), 4);
    }

    #[test]
    fn split_chunks_keep_their_outer_whitespace_and_blank_chunks_are_not_sent() {
        let limits = Limits::new();
        let http = FakeHttp::new(vec![ok(SINGLE_OK)]);
        let call = limits.call();
        assert_eq!(translate_chunk(&http, &Cell::new(0), &call, "ja", "ru", "  hi \t"), Ok("  Привет \t".to_string()));
        assert_eq!(translate_chunk(&http, &Cell::new(0), &call, "ja", "ru", " \n "), Ok(" \n ".to_string()));
        assert_eq!(http.urls().len(), 1);
    }

    #[test]
    fn stop_mid_walk_cancels_the_text_and_the_rest_without_requests() {
        let limits = Limits::new();
        let http = FakeHttp::new(vec![status(503, "")]).cancel_after(1, Arc::clone(&limits.cancel));
        let out = translate_texts_with(&http, &Cell::new(0), &limits.call(), "ja", "ru", &["a".to_string(), "b".to_string()]).expect("provider runs");
        assert_eq!(http.urls().len(), 1);
        assert_eq!(kinds(&out), vec![Some(MtItemErrorKind::Cancelled); 2]);
    }

    #[test]
    fn captcha_page_with_200_is_blocked() {
        let reply = HttpReply { status: 200, location: None, retry_after: None, body: CAPTCHA_HTML.to_string() };
        assert_eq!(classify_reply(&reply, ResponseShape::Single), Err(GoogleError::Blocked));
        let forbidden = HttpReply { status: 403, location: None, retry_after: None, body: String::new() };
        assert_eq!(classify_reply(&forbidden, ResponseShape::Single), Err(GoogleError::Blocked));
    }

    #[test]
    fn observed_sorry_429_page_is_a_block_and_a_bare_429_a_rate_limit() {
        assert!(looks_like_block_page(SORRY_429));
        let sorry = HttpReply { status: 429, location: None, retry_after: None, body: SORRY_429.to_string() };
        assert_eq!(classify_reply(&sorry, ResponseShape::Single), Err(GoogleError::Blocked));
        let automated = HttpReply { status: 429, location: None, retry_after: None, body: "<html><body>Sending automated queries is not allowed.</body></html>".to_string() };
        assert_eq!(classify_reply(&automated, ResponseShape::DictChromeEx), Err(GoogleError::Blocked));
        let bare = HttpReply { status: 429, location: None, retry_after: None, body: "Too Many Requests".to_string() };
        assert_eq!(classify_reply(&bare, ResponseShape::Single), Err(GoogleError::RateLimited));
        // A translation that merely contains the marker is JSON, not a block page.
        let echoed = HttpReply { status: 200, location: None, retry_after: None, body: r#"[[["<title>Sorry...</title>","x"]],null,"en"]"#.to_string() };
        assert!(classify_reply(&echoed, ResponseShape::Single).is_ok());
    }

    #[test]
    fn log_helpers_never_echo_content() {
        assert_eq!(redirect_host("https://www.google.com/sorry/index?continue=https://x/y?q=secret"), "www.google.com");
        assert_eq!(redirect_host("consent.google.com?x=1"), "consent.google.com");
        assert_eq!(body_kind(""), "empty");
        assert_eq!(body_kind("  <html>"), "html");
        assert_eq!(body_kind(SINGLE_OK), "json-like");
        assert_eq!(body_kind("secret"), "other");
    }

    #[test]
    fn language_codes_are_mapped_and_unknown_ones_rejected() {
        assert_eq!(normalize_google_lang("", LangRole::Source), Ok("auto"));
        assert_eq!(normalize_google_lang(" AUTO ", LangRole::Source), Ok("auto"));
        assert_eq!(normalize_google_lang("", LangRole::Target), Ok("ru"));
        assert_eq!(normalize_google_lang("zh-cn", LangRole::Target), Ok("zh-CN"));
        assert_eq!(normalize_google_lang("ZH-TW", LangRole::Source), Ok("zh-TW"));
        assert_eq!(normalize_google_lang("zh", LangRole::Target), Ok("zh-CN"));
        assert_eq!(normalize_google_lang("Ko", LangRole::Target), Ok("ko"));
        assert_eq!(normalize_google_lang("auto", LangRole::Target), Err(GoogleError::UnsupportedLanguage("auto".to_string())));
        assert_eq!(normalize_google_lang("xx", LangRole::Target), Err(GoogleError::UnsupportedLanguage("xx".to_string())));
    }

    #[test]
    fn unsupported_target_fails_the_whole_call_without_a_request() {
        let limits = Limits::new();
        let http = FakeHttp::new(Vec::new());
        let err = translate_texts_with(&http, &Cell::new(0), &limits.call(), "ja", "klingon", &["hi".to_string()]).expect_err("outer error");
        assert_eq!(err, GoogleError::UnsupportedLanguage("klingon".to_string()).user_message());
        assert!(http.urls().is_empty());
    }

    #[test]
    fn every_panel_language_is_supported() {
        use crate::panels::machine_translation::{MT_SOURCE_LANGUAGES, MT_TARGET_LANGUAGES};
        for language in MT_SOURCE_LANGUAGES {
            assert!(normalize_google_lang(language.code, LangRole::Source).is_ok(), "source {}", language.code);
        }
        for language in MT_TARGET_LANGUAGES {
            assert!(normalize_google_lang(language.code, LangRole::Target).is_ok(), "target {}", language.code);
        }
    }

    /// Serves one canned HTTP/1.1 response per connection on 127.0.0.1 and records each
    /// request line. A request to `/stop` ends the server.
    fn serve_loopback(listener: &TcpListener, responses: &[String]) -> Vec<String> {
        let mut seen = Vec::new();
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header");
                if header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).expect("request body");
            let request_line = request_line.trim().to_string();
            let stop = request_line.contains(" /stop");
            let response = if stop {
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else {
                responses.get(seen.len()).cloned().unwrap_or_else(|| "HTTP/1.1 500 Unscripted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string())
            };
            stream.write_all(response.as_bytes()).expect("write response");
            if stop {
                break;
            }
            seen.push(request_line);
        }
        seen
    }

    /// Pins the ureq mapping on a real socket: a 302 is returned (not followed) with its
    /// `Location`, a 429 arrives as a reply (not a transport error), and both classify.
    #[test]
    fn ureq_seam_returns_redirects_and_error_statuses_unfollowed() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let responses = vec![
            format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/sorry/index?continue=x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy".to_string(),
        ];
        let server = std::thread::spawn(move || serve_loopback(&listener, &responses));
        let http = UreqHttp::new();
        let base = format!("http://127.0.0.1:{port}");

        let redirect = http.post_form(&format!("{base}/translate_a/single"), &[("client", "gtx")], &[("q", "hi")]).expect("302 reply");
        assert_eq!(redirect.status, 302);
        assert_eq!(redirect.location.as_deref(), Some(format!("{base}/sorry/index?continue=x").as_str()));
        assert_eq!(classify_reply(&redirect, ResponseShape::Single), Err(GoogleError::Blocked));

        let limited = http.post_form(&format!("{base}/translate_a/t"), &[("client", "dict-chrome-ex")], &[("q", "hi")]).expect("429 reply");
        assert_eq!(limited.status, 429);
        assert_eq!(limited.body, "busy");
        assert_eq!(limited.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(redirect.retry_after, None);
        assert_eq!(classify_reply(&limited, ResponseShape::DictChromeEx), Err(GoogleError::RateLimited));

        http.post_form(&format!("{base}/stop"), &[], &[]).expect("stop server");
        let seen = server.join().expect("server thread");
        // Exactly the two requests: the redirect to `/sorry` was never followed.
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].starts_with("POST /translate_a/single?client=gtx"), "{seen:?}");
        assert!(seen[1].starts_with("POST /translate_a/t?client=dict-chrome-ex"), "{seen:?}");
    }
}
} // mod native
