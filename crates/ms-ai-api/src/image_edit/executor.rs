/*
File: crates/ms-ai-api/src/image_edit/executor.rs

Purpose:
The ONE native owner of the image-edit network exchange: it drives an adapter's
`EditProtocol` steps (submit, polls, download) over blocking HTTP (`ureq`), injects the API key
only where it may go, applies timeouts, the run deadline, response size caps, cancellation and
the retry policy. Adapters stay pure; nothing else in the layer does I/O.

Key structures:
- HttpTransport (crate-private seam: `UreqTransport` in production, a fake in tests)
- UreqTransport, ExecutorLimits, SendOptions

Key functions:
- execute()          : run one call to its final image bytes.
- retry_allowed()    : only GET is ever retried (a POST is a paid request).
- auth_allowed()     : the key goes only to the origin of the call's resolved base URL or to
                       a host the provider's data trusts (`auth_host_suffixes`, BFL).
- poll_delay(), retry_delay(), log_body(), failing_body_summary(): the remaining pure policy.

Notes:
Native only (the module is not compiled on wasm, where `run_image_edit` is a stub). Runs on a
worker thread; it sleeps between polls in short slices so a cancel is seen within ~100 ms.
A request already in flight cannot be aborted: its answer is dropped after a cancel, but it
is still parsed for the provider job it created, and a user cancel or the run deadline sends
the adapter's cancel request for the latest job (best effort). Requests that carry a key never
follow redirects (a custom key header would otherwise travel to the redirect target);
unauthenticated downloads do. The key is never logged; `runtime_log` keeps only the provider's
error message of a failing JSON body (bodies may echo the prompt), the 2 KB digest with base64
runs elided goes to the opt-in trace log.
*/

use std::io::Read;
use std::time::{Duration, Instant};

use ms_log::runtime_log;

use super::adapters::{is_success, provider_message};
use super::error::ImageEditError;
use super::pipeline::TRACE_CATEGORY;
use super::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx, StepPhase};
use super::request::{CancelFlag, ImageEditStage};

/// Log tag of every executor line.
const LOG_TAG: &str = "[AI API/image_edit]";
/// Longest failing response body kept in a log line, in characters.
const LOG_BODY_CHARS: usize = 2048;
/// Shortest run of base64 characters that is elided from a logged body.
const LOG_BASE64_RUN: usize = 100;
/// Granularity of cancel-aware waits.
const WAIT_SLICE: Duration = Duration::from_millis(100);

/// Time and size limits of one run. `Default` is the production policy; tests zero the delays.
#[derive(Debug, Clone)]
pub(crate) struct ExecutorLimits {
    /// TCP / TLS connect timeout of every request.
    pub connect: Duration,
    /// Whole-request timeout of the paid submit (synchronous providers render inside it).
    pub submit: Duration,
    /// Whole-request timeout of a status poll.
    pub poll: Duration,
    /// Whole-request timeout of the result download.
    pub download: Duration,
    /// Deadline of the whole run, from the first request.
    pub run_deadline: Duration,
    /// Shortest delay before the first status poll; the schedule grows by 0.5 s per poll.
    pub poll_min: Duration,
    /// Longest scheduled poll delay (an adapter may still ask for a longer one).
    pub poll_max: Duration,
    /// Delays before the retries of a failed GET; its length is the retry count.
    pub retry_backoff: [Duration; 3],
    /// Cap of an API response body (inline base64 images inflate the image by 4/3).
    pub response_cap: u64,
    /// Cap of a downloaded result image.
    pub download_cap: u64,
}

impl Default for ExecutorLimits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(15),
            submit: Duration::from_mins(5),
            poll: Duration::from_secs(30),
            download: Duration::from_mins(2),
            run_deadline: Duration::from_mins(15),
            poll_min: Duration::from_secs(1),
            poll_max: Duration::from_secs(3),
            retry_backoff: [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)],
            response_cap: 128 * 1024 * 1024,
            download_cap: 64 * 1024 * 1024,
        }
    }
}

/// Per-request options handed to the transport.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SendOptions {
    /// Whole-request timeout.
    pub timeout: Duration,
    /// Body cap in bytes; a longer body is `DownloadTooLarge`.
    pub cap: u64,
    /// Step name for `Timeout` errors and logs (technical text).
    pub stage: &'static str,
}

/// One blocking HTTP exchange. `auth` is the already-vetted scheme and key. Any HTTP status,
/// error statuses included, is an `Ok` response; only transport failures are errors.
pub(crate) trait HttpTransport {
    /// Sends `request` and reads its body up to `options.cap`.
    ///
    /// # Errors
    /// `Network` (connection, TLS, IO), `Timeout { stage }`, `DownloadTooLarge` over the cap,
    /// `RequestBuild` when the body cannot be serialized.
    fn send(&self, request: &HttpRequestSpec, auth: Option<(AuthScheme, &str)>, options: SendOptions) -> Result<HttpResponse, ImageEditError>;
}

/// The production transport over two `ureq` agents: one that never follows redirects (for
/// requests carrying a key) and one that does (unauthenticated downloads).
#[derive(Debug)]
pub(crate) struct UreqTransport {
    keyed: ureq::Agent,
    anonymous: ureq::Agent,
}

impl UreqTransport {
    /// Agents with the connect timeout of `limits`.
    pub(crate) fn new(limits: &ExecutorLimits) -> Self {
        Self { keyed: ureq::AgentBuilder::new().timeout_connect(limits.connect).redirects(0).build(), anonymous: ureq::AgentBuilder::new().timeout_connect(limits.connect).build() }
    }
}

impl HttpTransport for UreqTransport {
    fn send(&self, request: &HttpRequestSpec, auth: Option<(AuthScheme, &str)>, options: SendOptions) -> Result<HttpResponse, ImageEditError> {
        let agent = if auth.is_some() { &self.keyed } else { &self.anonymous };
        let method = match request.method {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
        };
        let mut builder = agent.request(method, &request.url).timeout(options.timeout);
        for (name, value) in &request.headers {
            builder = builder.set(name, value);
        }
        builder = match auth {
            Some((AuthScheme::Bearer, key)) => builder.set("Authorization", &format!("Bearer {key}")),
            Some((AuthScheme::Header(name), key)) => builder.set(name, key),
            Some((AuthScheme::KeyPrefix(prefix), key)) => builder.set("Authorization", &format!("{prefix} {key}")),
            None => builder,
        };
        let result = match &request.body {
            HttpBody::Empty => builder.call(),
            HttpBody::Json(value) => {
                let bytes = serde_json::to_vec(value).map_err(|error| ImageEditError::RequestBuild { detail: error.to_string() })?;
                builder.set("Content-Type", "application/json").send_bytes(&bytes)
            }
            HttpBody::Multipart(form) => builder.set("Content-Type", &form.content_type).send_bytes(&form.bytes),
        };
        let response = match result {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(transport)) => return Err(transport_error(&transport, options.stage)),
        };
        let status = response.status();
        let content_type = response.header("Content-Type").map(str::to_string);
        let mut body = Vec::new();
        response.into_reader().take(options.cap.saturating_add(1)).read_to_end(&mut body).map_err(|error| io_error(&error, options.stage))?;
        if u64::try_from(body.len()).map_or(true, |len| len > options.cap) {
            return Err(ImageEditError::DownloadTooLarge { limit_bytes: options.cap });
        }
        Ok(HttpResponse { status, content_type, body })
    }
}

/// Maps a `ureq` transport failure: a timed-out socket is `Timeout`, the rest `Network`.
fn transport_error(transport: &ureq::Transport, stage: &'static str) -> ImageEditError {
    let timed_out = std::error::Error::source(transport).and_then(|source| source.downcast_ref::<std::io::Error>()).is_some_and(|error| matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock));
    if timed_out { ImageEditError::Timeout { stage: stage.to_string() } } else { ImageEditError::Network { detail: transport.to_string() } }
}

/// Maps a body read failure the same way.
fn io_error(error: &std::io::Error, stage: &'static str) -> ImageEditError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ImageEditError::Timeout { stage: stage.to_string() },
        _ => ImageEditError::Network { detail: error.to_string() },
    }
}

/// Whether a failed request with `method` may be sent again. Only GET (status polls,
/// downloads): a POST submit is a paid request and is NEVER retried, whatever failed.
#[must_use]
pub(crate) fn retry_allowed(method: HttpMethod) -> bool {
    match method {
        HttpMethod::Get => true,
        HttpMethod::Post | HttpMethod::Put | HttpMethod::Delete => false,
    }
}

/// The delay before retry number `attempt` (0-based), or `None` when the retries are spent.
#[must_use]
pub(crate) fn retry_delay(attempt: usize, limits: &ExecutorLimits) -> Option<Duration> {
    limits.retry_backoff.get(attempt).copied()
}

/// The wait before status poll number `polls` (1-based): the adapter's `requested` delay, but
/// at least the schedule `poll_min + 0.5 s * (polls - 1)`, capped at `poll_max`.
#[must_use]
pub(crate) fn poll_delay(polls: u32, requested: Duration, limits: &ExecutorLimits) -> Duration {
    let schedule = limits.poll_min.saturating_add(Duration::from_millis(500).saturating_mul(polls.saturating_sub(1))).min(limits.poll_max);
    requested.max(schedule)
}

/// The scheme, lowercase host and port of an absolute http(s) URL; `None` for anything else
/// (other schemes, userinfo, whitespace, an empty host, a bad port).
fn origin(url: &str) -> Option<(String, String, u16)> {
    if url.chars().any(char::is_whitespace) {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    // An IPv6 literal keeps its brackets; the port follows the closing one.
    let (host, port) = if authority.starts_with('[') {
        let close = authority.find(']')?;
        let (host, tail) = authority.split_at(close + 1);
        let port = if tail.is_empty() { None } else { Some(tail.strip_prefix(':')?) };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        Some(port) => port.parse::<u16>().ok()?,
        None => default_port,
    };
    Some((scheme, host.to_ascii_lowercase(), port))
}

/// Whether the API key may be attached to `url`: when it has exactly the origin (scheme, host,
/// port) of the call's resolved `base_url` — the provider's own API host, or the user's own
/// server — or when it is https on port 443 and its host is one of the provider's
/// `trusted_suffixes` or a subdomain of one (dot boundary: `api.eu1.bfl.ai` matches `bfl.ai`,
/// `evilbfl.ai` does not). Signed result URLs on CDNs and any other host never get the key.
#[must_use]
pub(crate) fn auth_allowed(url: &str, base_url: &str, trusted_suffixes: &[&str]) -> bool {
    let Some(target) = origin(url) else { return false };
    if origin(base_url).is_some_and(|base| base == target) {
        return true;
    }
    let (scheme, host, port) = target;
    scheme == "https"
        && port == 443
        && trusted_suffixes.iter().any(|suffix| {
            let suffix = suffix.to_ascii_lowercase();
            host == suffix || host.strip_suffix(&suffix).is_some_and(|prefix| prefix.ends_with('.'))
        })
}

/// A failing response body for a log line: lossy UTF-8, runs of base64 characters of at least
/// `LOG_BASE64_RUN` replaced by `<base64: N chars>`, cut to `LOG_BODY_CHARS` characters.
#[must_use]
pub(crate) fn log_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let is_b64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=');
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= LOG_BASE64_RUN {
            out.push_str("<base64: ");
            out.push_str(&run.chars().count().to_string());
            out.push_str(" chars>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if is_b64(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    if out.chars().count() > LOG_BODY_CHARS {
        out = out.chars().take(LOG_BODY_CHARS).collect();
        out.push_str("...");
    }
    out
}

/// What `runtime_log` may keep of a failing response body: the provider's own error message
/// (`adapters::provider_message`) of a JSON body, never the whole JSON (validation errors echo
/// request fields back, e.g. fal's `detail[].input`, which can be the prompt), or the
/// `log_body` digest of a non-JSON body (HTML error pages, plain text).
#[must_use]
pub(crate) fn failing_body_summary(body: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => provider_message(&value).unwrap_or_else(|| format!("<JSON body of {} bytes without an error message>", body.len())),
        Err(_) => log_body(body),
    }
}

/// Logs a failing response of `provider` (`what` = which request and its status): the
/// `failing_body_summary` at warn level in `runtime_log`, the full `log_body` digest only in the
/// opt-in trace log (category `IMAGE_EDIT`), where the prompt may appear.
fn log_failing_body(provider: &str, what: &str, body: &[u8]) {
    runtime_log::log_warn(format!("{LOG_TAG} {provider} {what}: {}", failing_body_summary(body)));
    ms_log::trace_log!(TRACE_CATEGORY, "{LOG_TAG} {provider} {what}, body: {}", log_body(body));
}

/// What a run needs besides the adapter: the call, the key and the cancel flag.
pub(crate) struct RunContext<'a> {
    pub call: &'a EditCall,
    /// The API key (may be empty for the user's own server: no auth header is sent then).
    pub key: &'a str,
    pub cancel: &'a CancelFlag,
}

/// Drives `protocol` for `run.call` to the final image bytes: submit (never retried), then the
/// adapter's polls (GET retried on IO / 5xx) and download, reporting stages through
/// `on_stage`. A cancel is honoured between steps and during waits; when the cancel or the run
/// deadline ends a run that already has a provider job, the adapter's cancel request is sent
/// best effort (a submit answer that arrived after the cancel is still parsed for its job).
///
/// # Errors
/// The adapter's typed provider errors; `Cancelled`; `Timeout` (a request or the run deadline);
/// `Network`; `DownloadTooLarge`; `ProviderFailed` for a failed download; `RequestBuild` when a
/// request asks for the key on a foreign origin or names the auth header itself.
pub(crate) fn execute(transport: &dyn HttpTransport, limits: &ExecutorLimits, protocol: &dyn EditProtocol, run: &RunContext<'_>, on_stage: &mut dyn FnMut(ImageEditStage)) -> Result<Vec<u8>, ImageEditError> {
    let exec = Execution { transport, limits, protocol, run, deadline: Instant::now() + limits.run_deadline };
    run.cancel.check()?;
    let submit = protocol.submit(run.call)?;
    let mut state = JobState { phase: StepPhase::Submit, polls: 0, job: None };
    match exec.drive(&submit, &mut state, on_stage) {
        Err(ImageEditError::Cancelled) => {
            exec.cancel_job(&state, "run cancelled");
            Err(ImageEditError::Cancelled)
        }
        // The app gave up on the job: a provider job still queued would otherwise run (and
        // bill) to completion, so it is cancelled exactly like a user cancel.
        Err(ImageEditError::Timeout { stage }) if stage == "run" || Instant::now() >= exec.deadline => {
            exec.cancel_job(&state, "run deadline passed");
            Err(ImageEditError::Timeout { stage })
        }
        other => other,
    }
}

/// Where a run stands, kept outside the step loop so the cancel paths see the latest job.
struct JobState {
    /// Which request the current response answers.
    phase: StepPhase,
    /// Follow-up requests made so far.
    polls: u32,
    /// The latest job handle an adapter returned.
    job: Option<JobRef>,
}

/// The fixed context of one `execute` call.
struct Execution<'a> {
    transport: &'a dyn HttpTransport,
    limits: &'a ExecutorLimits,
    protocol: &'a dyn EditProtocol,
    run: &'a RunContext<'a>,
    /// The run deadline.
    deadline: Instant,
}

impl Execution<'_> {
    /// The step loop of `execute` from the submit request on; `state` tracks the job so the
    /// caller can cancel it when this returns `Cancelled` or the run deadline passes.
    fn drive(&self, submit: &HttpRequestSpec, state: &mut JobState, on_stage: &mut dyn FnMut(ImageEditStage)) -> Result<Vec<u8>, ImageEditError> {
        let (run, limits, protocol) = (self.run, self.limits, self.protocol);
        on_stage(ImageEditStage::Sending);
        let mut response = self.send(submit, limits.submit, limits.response_cap, "submit")?;
        loop {
            if !is_success(response.status) {
                log_failing_body(run.call.provider.key(), &format!("{:?} answered HTTP {}", state.phase, response.status), &response.body);
            }
            let step = StepCtx { call: run.call, phase: state.phase, polls: state.polls, job: state.job.as_ref() };
            // A cancel that arrived while a request was in flight drops its answer. The answer
            // is still parsed (pure, no I/O) for a job handle: a submit that was in flight when
            // the user cancelled has created a paid job that must be cancelled too.
            if run.cancel.is_cancelled() {
                match protocol.next(&step, &response) {
                    Ok(NextStep::Poll { job: Some(fresh), .. }) => state.job = Some(fresh),
                    // No new handle (a final answer, or an error answer that created no job):
                    // the kept handle, if any, is the one to cancel.
                    Ok(NextStep::Poll { job: None, .. } | NextStep::Download(_) | NextStep::Image(_)) | Err(_) => {}
                }
                return Err(ImageEditError::Cancelled);
            }
            match protocol.next(&step, &response)? {
                NextStep::Image(bytes) => return Ok(bytes),
                NextStep::Download(request) => {
                    on_stage(ImageEditStage::Downloading);
                    let download = self.send(&request, limits.download, limits.download_cap, "download")?;
                    if !is_success(download.status) {
                        log_failing_body(run.call.provider.key(), &format!("result download answered HTTP {}", download.status), &download.body);
                        return Err(ImageEditError::ProviderFailed { detail: format!("result download answered HTTP {}", download.status) });
                    }
                    return Ok(download.body);
                }
                NextStep::Poll { request, after, stage, job: new_job } => {
                    if new_job.is_some() {
                        state.job = new_job;
                    }
                    state.polls = state.polls.saturating_add(1);
                    on_stage(ImageEditStage::Waiting { polls: state.polls });
                    wait(poll_delay(state.polls, after, limits), run.cancel, self.deadline)?;
                    response = self.send(&request, limits.poll, limits.response_cap, "poll")?;
                    state.phase = StepPhase::Poll { stage };
                }
            }
        }
    }

    /// Fires the adapter's cancel request for the job of `state`, if it has one (best effort:
    /// the outcome is only logged; `reason` names why, for the log).
    fn cancel_job(&self, state: &JobState, reason: &str) {
        let provider = self.run.call.provider.key();
        let step = StepCtx { call: self.run.call, phase: state.phase, polls: state.polls, job: state.job.as_ref() };
        let Some(request) = self.protocol.cancel_request(&step) else { return };
        let options = SendOptions { timeout: self.limits.poll, cap: self.limits.response_cap, stage: "cancel" };
        match vet_auth(&request, self.run).and_then(|auth| self.transport.send(&request, auth, options)) {
            Ok(response) if is_success(response.status) => runtime_log::log_info(format!("{LOG_TAG} {provider} job cancelled at the provider ({reason})")),
            Ok(response) => log_failing_body(provider, &format!("cancel request ({reason}) answered HTTP {}", response.status), &response.body),
            Err(error) => runtime_log::log_warn(format!("{LOG_TAG} {provider} cancel request ({reason}) failed: {error:?}")),
        }
    }

    /// Sends one request within the run deadline (`timeout` is its own limit, `cap` its body
    /// cap), retrying a GET on IO failure or 5xx per the backoff of `limits`.
    fn send(&self, request: &HttpRequestSpec, timeout: Duration, cap: u64, stage: &'static str) -> Result<HttpResponse, ImageEditError> {
        let auth = vet_auth(request, self.run)?;
        let mut attempt = 0;
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ImageEditError::Timeout { stage: "run".to_string() });
            }
            let result = self.transport.send(request, auth, SendOptions { timeout: timeout.min(remaining), cap, stage });
            let transient = match &result {
                Ok(response) => response.status >= 500,
                Err(ImageEditError::Network { .. } | ImageEditError::Timeout { .. }) => true,
                Err(_) => false,
            };
            if !transient || !retry_allowed(request.method) {
                return result;
            }
            let Some(delay) = retry_delay(attempt, self.limits) else { return result };
            let failure = result.as_ref().map_or_else(|error| format!("{error:?}"), |response| format!("HTTP {}", response.status));
            runtime_log::log_warn(format!("{LOG_TAG} {} {stage} failed ({failure}), retry {} of {}", self.run.call.provider.key(), attempt + 1, self.limits.retry_backoff.len()));
            attempt += 1;
            wait(delay, self.run.cancel, self.deadline)?;
        }
    }
}

/// The auth to attach to `request`: `None` when it asks for none or the key is empty (the
/// user's own server without a key).
///
/// # Errors
/// `RequestBuild` when the request asks for the key on a host `auth_allowed` refuses (not the
/// call's base-URL origin, not a host the provider trusts), or sets the auth header itself.
fn vet_auth<'k>(request: &HttpRequestSpec, run: &RunContext<'k>) -> Result<Option<(AuthScheme, &'k str)>, ImageEditError> {
    let auth_header = match request.auth {
        Some(AuthScheme::Header(name)) => name,
        Some(AuthScheme::Bearer | AuthScheme::KeyPrefix(_)) | None => "Authorization",
    };
    if request.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("Authorization") || name.eq_ignore_ascii_case(auth_header)) {
        return Err(ImageEditError::RequestBuild { detail: format!("a request to {} sets the auth header itself", request.url) });
    }
    let Some(scheme) = request.auth else { return Ok(None) };
    if !auth_allowed(&request.url, &run.call.base_url, run.call.provider.auth_host_suffixes()) {
        return Err(ImageEditError::RequestBuild { detail: format!("refusing to send the API key to {}, outside {}", request.url, run.call.base_url) });
    }
    let key = run.key.trim();
    Ok(if key.is_empty() { None } else { Some((scheme, key)) })
}

/// Sleeps `duration` in short slices, failing early on cancel or the run deadline.
///
/// # Errors
/// `Cancelled`, or `Timeout { stage: "run" }` when the deadline passes first.
fn wait(duration: Duration, cancel: &CancelFlag, deadline: Instant) -> Result<(), ImageEditError> {
    let until = Instant::now() + duration;
    loop {
        cancel.check()?;
        let now = Instant::now();
        if now >= until {
            return Ok(());
        }
        if now >= deadline {
            return Err(ImageEditError::Timeout { stage: "run".to_string() });
        }
        std::thread::sleep(WAIT_SLICE.min(until - now));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::time::Duration;

    use super::{ExecutorLimits, HttpTransport, RunContext, SendOptions, auth_allowed, execute, failing_body_summary, log_body, poll_delay, retry_allowed, retry_delay};
    use crate::image_edit::adapters::test_support::call;
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx, StepPhase};
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::request::{CancelFlag, ImageEditStage};

    /// Production limits with every delay zeroed, so tests never sleep.
    pub(crate) fn instant_limits() -> ExecutorLimits {
        ExecutorLimits { poll_min: Duration::ZERO, poll_max: Duration::ZERO, retry_backoff: [Duration::ZERO; 3], ..ExecutorLimits::default() }
    }

    /// One request the fake transport saw.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct Sent {
        pub method: HttpMethod,
        pub url: String,
        pub auth: Option<(AuthScheme, String)>,
        pub stage: &'static str,
    }

    /// A transport that answers from a script and records what was sent.
    #[derive(Debug, Default)]
    pub(crate) struct FakeTransport {
        pub answers: RefCell<VecDeque<Result<HttpResponse, ImageEditError>>>,
        pub sent: RefCell<Vec<Sent>>,
        /// Fired after the n-th send (1-based), to simulate a cancel during a request.
        pub cancel_after: Option<(usize, CancelFlag)>,
    }

    impl FakeTransport {
        pub(crate) fn new(answers: Vec<Result<HttpResponse, ImageEditError>>) -> Self {
            Self { answers: RefCell::new(answers.into()), ..Self::default() }
        }
    }

    impl HttpTransport for FakeTransport {
        fn send(&self, request: &HttpRequestSpec, auth: Option<(AuthScheme, &str)>, options: SendOptions) -> Result<HttpResponse, ImageEditError> {
            self.sent.borrow_mut().push(Sent { method: request.method, url: request.url.clone(), auth: auth.map(|(scheme, key)| (scheme, key.to_string())), stage: options.stage });
            if let Some((after, flag)) = &self.cancel_after
                && self.sent.borrow().len() == *after
            {
                flag.cancel();
            }
            self.answers.borrow_mut().pop_front().unwrap_or_else(|| Err(ImageEditError::Network { detail: "script exhausted".to_string() }))
        }
    }

    pub(crate) fn resp(status: u16, body: &[u8]) -> HttpResponse {
        HttpResponse { status, content_type: None, body: body.to_vec() }
    }

    /// An async test protocol: submit -> job "j1" -> polls until a body says "done" -> download.
    #[derive(Debug)]
    struct AsyncProtocol {
        cancel_url: Option<String>,
    }

    fn get(url: &str, auth: Option<AuthScheme>) -> HttpRequestSpec {
        HttpRequestSpec { method: HttpMethod::Get, url: url.to_string(), headers: Vec::new(), body: HttpBody::Empty, auth }
    }

    impl EditProtocol for AsyncProtocol {
        fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
            Ok(HttpRequestSpec { method: HttpMethod::Post, url: format!("{}/jobs", call.base_url), headers: Vec::new(), body: HttpBody::Empty, auth: Some(AuthScheme::KeyPrefix("Key")) })
        }

        fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
            if response.status != 200 {
                return Err(ImageEditError::ProviderRejected { status: response.status, message: String::new() });
            }
            let status_url = format!("{}/jobs/j1", step.call.base_url);
            match (step.phase, response.body.as_slice()) {
                (StepPhase::Submit, _) => Ok(NextStep::Poll { request: get(&status_url, Some(AuthScheme::KeyPrefix("Key"))), after: Duration::ZERO, stage: 0, job: Some(JobRef { id: "j1".to_string(), poll_url: None, cancel_url: self.cancel_url.clone() }) }),
                (StepPhase::Poll { .. }, b"done") => Ok(NextStep::Download(get("https://cdn.example.net/result.png", None))),
                (StepPhase::Poll { .. }, _) => Ok(NextStep::Poll { request: get(&status_url, Some(AuthScheme::KeyPrefix("Key"))), after: Duration::ZERO, stage: 0, job: None }),
            }
        }

        fn cancel_request(&self, step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
            step.job.and_then(|job| job.cancel_url.clone()).map(|url| HttpRequestSpec { method: HttpMethod::Put, url, headers: Vec::new(), body: HttpBody::Empty, auth: Some(AuthScheme::KeyPrefix("Key")) })
        }
    }

    fn async_call() -> EditCall {
        call(ImageEditProvider::Fal, "m", "https://queue.example.run", None, (2, 2), SizeParamStyle::None, None)
    }

    fn run(transport: &FakeTransport, protocol: &dyn EditProtocol, call: &EditCall, cancel: &CancelFlag) -> (Result<Vec<u8>, ImageEditError>, Vec<ImageEditStage>) {
        let mut stages = Vec::new();
        let result = execute(transport, &instant_limits(), protocol, &RunContext { call, key: " secret ", cancel }, &mut |stage| stages.push(stage));
        (result, stages)
    }

    #[test]
    fn retry_policy_never_repeats_a_post() {
        assert!(retry_allowed(HttpMethod::Get));
        assert!(!retry_allowed(HttpMethod::Post));
        assert!(!retry_allowed(HttpMethod::Put));
        assert!(!retry_allowed(HttpMethod::Delete));
        let limits = ExecutorLimits::default();
        assert_eq!((0..4).map(|attempt| retry_delay(attempt, &limits)).collect::<Vec<_>>(), [Some(Duration::from_secs(1)), Some(Duration::from_secs(2)), Some(Duration::from_secs(4)), None]);
    }

    #[test]
    fn poll_schedule_grows_from_one_to_three_seconds() {
        let limits = ExecutorLimits::default();
        let delays: Vec<u128> = (1..=6).map(|polls| poll_delay(polls, Duration::ZERO, &limits).as_millis()).collect();
        assert_eq!(delays, [1000, 1500, 2000, 2500, 3000, 3000]);
        assert_eq!(poll_delay(1, Duration::from_secs(10), &limits), Duration::from_secs(10));
    }

    #[test]
    fn the_key_goes_only_to_the_base_url_origin() {
        let base = "https://api.openai.com/v1";
        assert!(auth_allowed("https://api.openai.com/v1/images/edits", base, &[]));
        assert!(auth_allowed("HTTPS://API.OPENAI.COM:443/v1/x", base, &[]));
        assert!(!auth_allowed("http://api.openai.com/v1/images/edits", base, &[]));
        assert!(!auth_allowed("https://api.openai.com.evil.example/v1", base, &[]));
        assert!(!auth_allowed("https://api.openai.com:8443/v1", base, &[]));
        assert!(!auth_allowed("https://evil.example@api.openai.com/v1", base, &[]));
        assert!(!auth_allowed("https://cdn.aimlapi.com/generations/x.png", "https://api.aimlapi.com/v1", &[]));
        assert!(!auth_allowed("ftp://api.openai.com/v1", base, &[]));
        assert!(!auth_allowed("https://api.openai.com /v1", base, &[]));
        // The user's own server: plain http on its own port, IPv6 literals.
        assert!(auth_allowed("http://127.0.0.1:1234/v1/images/edits", "http://127.0.0.1:1234/v1", &[]));
        assert!(!auth_allowed("http://127.0.0.1:1235/v1/images/edits", "http://127.0.0.1:1234/v1", &[]));
        assert!(auth_allowed("http://[::1]:8080/v1/images/edits", "http://[::1]:8080/v1", &[]));
        assert!(!auth_allowed("http://[::1]/v1", "http://[::1]:8080/v1", &[]));
        // BFL: a regional polling host on a dot boundary, https:443 only.
        let bfl = ["bfl.ai"];
        assert!(auth_allowed("https://api.eu1.bfl.ai/v1/get_result?id=x", "https://api.bfl.ai", &bfl));
        assert!(auth_allowed("https://bfl.ai/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("https://evilbfl.ai/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("https://api.bfl.ai.evil.example/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("http://api.us1.bfl.ai/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("https://api.us1.bfl.ai:8443/v1/x", "https://api.bfl.ai", &bfl));
        assert!(!auth_allowed("https://api.us1.bfl.ai/v1/x", "https://api.bfl.ai", &[]));
    }

    #[test]
    fn logged_bodies_elide_base64_and_are_capped() {
        let body = format!("{{\"error\":\"bad\",\"b64_json\":\"{}\"}}", "A".repeat(5000));
        assert_eq!(log_body(body.as_bytes()), "{\"error\":\"bad\",\"b64_json\":\"<base64: 5000 chars>\"}");
        let long = "word ".repeat(1000);
        assert_eq!(log_body(long.as_bytes()).chars().count(), 2048 + 3);
    }

    #[test]
    fn async_run_polls_then_downloads_without_auth() {
        let call = async_call();
        let transport = FakeTransport::new(vec![Ok(resp(200, b"queued")), Ok(resp(200, b"running")), Ok(resp(200, b"done")), Ok(resp(200, b"IMAGE"))]);
        let (result, stages) = run(&transport, &AsyncProtocol { cancel_url: None }, &call, &CancelFlag::new());
        assert_eq!(result.ok(), Some(b"IMAGE".to_vec()));
        assert_eq!(stages, [ImageEditStage::Sending, ImageEditStage::Waiting { polls: 1 }, ImageEditStage::Waiting { polls: 2 }, ImageEditStage::Downloading]);
        let sent = transport.sent.borrow();
        let keyed = Some((AuthScheme::KeyPrefix("Key"), "secret".to_string()));
        assert_eq!(sent.iter().map(|sent| (sent.method, sent.stage, sent.auth.clone())).collect::<Vec<_>>(), [(HttpMethod::Post, "submit", keyed.clone()), (HttpMethod::Get, "poll", keyed.clone()), (HttpMethod::Get, "poll", keyed), (HttpMethod::Get, "download", None)]);
    }

    #[test]
    fn a_failed_submit_is_never_retried() {
        let call = async_call();
        for answer in [Ok(resp(503, b"busy")), Err(ImageEditError::Network { detail: "reset".to_string() }), Err(ImageEditError::Timeout { stage: "submit".to_string() })] {
            let transport = FakeTransport::new(vec![answer, Ok(resp(200, b"queued"))]);
            let (result, _) = run(&transport, &AsyncProtocol { cancel_url: None }, &call, &CancelFlag::new());
            assert!(result.is_err());
            assert_eq!(transport.sent.borrow().len(), 1, "the paid POST went out exactly once");
        }
    }

    #[test]
    fn polls_and_downloads_retry_transient_failures_three_times() {
        let call = async_call();
        let flaky = vec![Ok(resp(200, b"queued")), Ok(resp(502, b"")), Err(ImageEditError::Network { detail: "reset".to_string() }), Ok(resp(200, b"done")), Ok(resp(500, b"")), Ok(resp(200, b"IMAGE"))];
        let transport = FakeTransport::new(flaky);
        let (result, _) = run(&transport, &AsyncProtocol { cancel_url: None }, &call, &CancelFlag::new());
        assert_eq!(result.ok(), Some(b"IMAGE".to_vec()));
        let giving_up = vec![Ok(resp(200, b"queued")), Ok(resp(502, b"")), Ok(resp(502, b"")), Ok(resp(502, b"")), Ok(resp(502, b""))];
        let transport = FakeTransport::new(giving_up);
        let (result, _) = run(&transport, &AsyncProtocol { cancel_url: None }, &call, &CancelFlag::new());
        assert!(matches!(result, Err(ImageEditError::ProviderRejected { status: 502, .. })));
        assert_eq!(transport.sent.borrow().len(), 5, "one submit, one poll and three retries");
    }

    #[test]
    fn a_failed_download_is_a_provider_failure() {
        let call = async_call();
        let transport = FakeTransport::new(vec![Ok(resp(200, b"queued")), Ok(resp(200, b"done")), Ok(resp(403, b"expired"))]);
        let (result, _) = run(&transport, &AsyncProtocol { cancel_url: None }, &call, &CancelFlag::new());
        assert!(matches!(result, Err(ImageEditError::ProviderFailed { .. })));
    }

    #[test]
    fn cancel_before_start_sends_nothing() {
        let call = async_call();
        let transport = FakeTransport::new(Vec::new());
        let cancel = CancelFlag::new();
        cancel.cancel();
        let (result, _) = run(&transport, &AsyncProtocol { cancel_url: None }, &call, &cancel);
        assert!(matches!(result, Err(ImageEditError::Cancelled)));
        assert!(transport.sent.borrow().is_empty());
    }

    #[test]
    fn cancel_during_a_poll_fires_the_cancel_request_and_drops_the_answer() {
        let call = async_call();
        let cancel = CancelFlag::new();
        let mut transport = FakeTransport::new(vec![Ok(resp(200, b"queued")), Ok(resp(200, b"done")), Ok(resp(200, b""))]);
        transport.cancel_after = Some((2, cancel.clone()));
        let (result, _) = run(&transport, &AsyncProtocol { cancel_url: Some("https://queue.example.run/jobs/j1/cancel".to_string()) }, &call, &cancel);
        assert!(matches!(result, Err(ImageEditError::Cancelled)));
        let sent = transport.sent.borrow();
        assert_eq!(sent.iter().map(|sent| (sent.method, sent.stage)).collect::<Vec<_>>(), [(HttpMethod::Post, "submit"), (HttpMethod::Get, "poll"), (HttpMethod::Put, "cancel")]);
    }

    #[test]
    fn cancel_during_the_submit_still_cancels_the_created_job() {
        let call = async_call();
        let cancel = CancelFlag::new();
        let mut transport = FakeTransport::new(vec![Ok(resp(200, b"queued")), Ok(resp(200, b""))]);
        transport.cancel_after = Some((1, cancel.clone()));
        let (result, _) = run(&transport, &AsyncProtocol { cancel_url: Some("https://queue.example.run/jobs/j1/cancel".to_string()) }, &call, &cancel);
        assert!(matches!(result, Err(ImageEditError::Cancelled)));
        let sent = transport.sent.borrow();
        assert_eq!(sent.iter().map(|sent| (sent.method, sent.url.as_str(), sent.stage)).collect::<Vec<_>>(), [(HttpMethod::Post, "https://queue.example.run/jobs", "submit"), (HttpMethod::Put, "https://queue.example.run/jobs/j1/cancel", "cancel")]);
    }

    #[test]
    fn the_run_deadline_cancels_the_job_and_times_out() {
        let call = async_call();
        let transport = FakeTransport::new(vec![Ok(resp(200, b"queued")), Ok(resp(200, b""))]);
        // The first poll waits longer than the whole run may last.
        let limits = ExecutorLimits { run_deadline: Duration::from_millis(30), poll_min: Duration::from_secs(5), poll_max: Duration::from_secs(5), ..instant_limits() };
        let protocol = AsyncProtocol { cancel_url: Some("https://queue.example.run/jobs/j1/cancel".to_string()) };
        let result = execute(&transport, &limits, &protocol, &RunContext { call: &call, key: "k", cancel: &CancelFlag::new() }, &mut |_| {});
        assert!(matches!(result, Err(ImageEditError::Timeout { stage }) if stage == "run"));
        let sent = transport.sent.borrow();
        assert_eq!(sent.iter().map(|sent| (sent.method, sent.stage)).collect::<Vec<_>>(), [(HttpMethod::Post, "submit"), (HttpMethod::Put, "cancel")]);
    }

    #[test]
    fn a_failing_json_body_is_summarized_by_its_message_only() {
        // fal's validation error echoes the rejected field value (here: the prompt).
        let fal = br#"{"detail":[{"loc":["body","prompt"],"msg":"String too long","type":"value_error","input":"SECRET PROMPT"}]}"#;
        assert_eq!(failing_body_summary(fal), "String too long");
        assert_eq!(failing_body_summary(br#"{"input":"SECRET PROMPT"}"#), "<JSON body of 25 bytes without an error message>");
        assert_eq!(failing_body_summary(b"<html>Bad Gateway</html>"), "<html>Bad Gateway</html>");
    }

    /// A protocol whose follow-up asks for the key on a foreign host.
    #[derive(Debug)]
    struct LeakyProtocol;

    impl EditProtocol for LeakyProtocol {
        fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
            Ok(HttpRequestSpec { method: HttpMethod::Post, url: format!("{}/jobs", call.base_url), headers: Vec::new(), body: HttpBody::Empty, auth: Some(AuthScheme::Bearer) })
        }

        fn next(&self, _step: &StepCtx<'_>, _response: &HttpResponse) -> Result<NextStep, ImageEditError> {
            Ok(NextStep::Download(get("https://evil.example/x.png", Some(AuthScheme::Bearer))))
        }

        fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
            None
        }
    }

    #[test]
    fn the_key_is_never_sent_to_a_foreign_host() {
        let call = async_call();
        let transport = FakeTransport::new(vec![Ok(resp(200, b"")), Ok(resp(200, b"IMAGE"))]);
        let (result, _) = run(&transport, &LeakyProtocol, &call, &CancelFlag::new());
        assert!(matches!(result, Err(ImageEditError::RequestBuild { .. })));
        assert_eq!(transport.sent.borrow().len(), 1);
    }

    #[test]
    fn an_empty_key_sends_no_auth() {
        let call = call(ImageEditProvider::OpenAiCompatible, "m", "http://127.0.0.1:1234/v1", None, (2, 2), SizeParamStyle::None, None);
        let transport = FakeTransport::new(vec![Ok(resp(200, b"queued")), Ok(resp(200, b"done")), Ok(resp(200, b"IMAGE"))]);
        let mut stages = Vec::new();
        let result = execute(&transport, &instant_limits(), &AsyncProtocol { cancel_url: None }, &RunContext { call: &call, key: "  ", cancel: &CancelFlag::new() }, &mut |stage| stages.push(stage));
        assert!(result.is_ok());
        assert!(transport.sent.borrow().iter().all(|sent| sent.auth.is_none()));
    }
}
