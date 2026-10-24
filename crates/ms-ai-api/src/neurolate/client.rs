/*
File: crates/ms-ai-api/src/neurolate/client.rs

Purpose:
The native network half of the NeuroLate client: the paid OCR request (`recognize`) and the
free current-key request (`fetch_key_info`) over the crate's shared transport (`crate::http`).

Key structures:
- NeurolateLimits (timeouts, body caps, the key request's retry backoff, the abandon poll)
- PaidFlights (the process-wide cap on paid requests still out)

Key functions:
- recognize()        : one `POST /ext/v1/ocr`, NEVER retried, abandonable.
- fetch_key_info()   : `GET /ext/v1/key`, retried on transient failures.
- recognize_with() / fetch_key_info_with(): the same over an injected transport (test seam).

Notes:
Native only; the wasm build gets stubs answering `WebUnavailable`. Blocking: worker threads only.
A blocking `ureq` request cannot be aborted, and consumers join their workers on teardown, so
the paid POST runs on its own detached `ms_thread` worker while the caller waits on a channel in
`abandon_poll` slices; once the caller's `abandon` predicate turns true the call returns
`Cancelled` at once, and the detached request finishes on its own (it may still be charged) and
logs its request id and cost with "result discarded". At most two paid requests may be out
process-wide (`PaidFlights`, counting abandoned ones until their send returns), so cancel and
restart cannot pile up paid requests; a call over the cap sends nothing (`TooManyInFlight`).
Every paid POST carries a fresh v4 `Idempotency-Key` header (one per
`recognize` call, logged with the request id), so a duplicate delivery is refused by the server
(409 -> `DuplicateRequest`) instead of charged twice. The key goes only to `BASE_URL`'s origin
(`auth_allowed`), in the `Authorization: Bearer` header, never in a log, URL or error. Logs
(`runtime_log`): one line per OCR call with mode, image count, request id, blocks per image,
cost and duration; a failing body as NeuroLate's `{code, message}` plus the `log_body` digest
(never image bytes: the request body is never logged).
*/

#[cfg(not(target_arch = "wasm32"))]
pub use native::{fetch_key_info, recognize};

#[cfg(target_arch = "wasm32")]
pub use web::{fetch_key_info, recognize};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::{Duration, Instant};

    use ms_log::runtime_log;

    use crate::http::{AuthScheme, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, HttpTransport, SendOptions, UreqTransport, auth_allowed, log_body, retry_allowed};
    use crate::neurolate::error::NeurolateError;
    use crate::neurolate::protocol::{BASE_URL, KEY_PATH, KeyInfo, OCR_PATH, OcrRequest, OcrResponse, ocr_form, cost_text, parse_key_info, parse_ocr_response};

    /// Log tag of every client line.
    const LOG_TAG: &str = "[NeuroLate]";
    /// The header that makes a paid request idempotent on the server.
    const IDEMPOTENCY_HEADER: &str = "Idempotency-Key";
    /// Name of the detached OCR request thread (diagnostics only).
    const OCR_THREAD_NAME: &str = "neurolate-ocr";
    /// Most paid requests whose send has not returned yet (running or abandoned but still
    /// settling), process-wide.
    const MAX_PAID_FLIGHTS: usize = 2;

    /// The process-wide count of paid flights (`recognize`).
    static PAID_FLIGHTS: PaidFlights = PaidFlights::new(MAX_PAID_FLIGHTS);

    /// A bounded count of paid requests whose detached send has not returned yet. Abandoning a
    /// request does not free its slot: only the end of its send does, so cancel-and-restart
    /// can never pile up paid requests (each holds the key, the images and a response buffer).
    #[derive(Debug)]
    pub(crate) struct PaidFlights {
        in_flight: AtomicUsize,
        cap: usize,
    }

    impl PaidFlights {
        /// An empty count allowing `cap` flights.
        pub(crate) const fn new(cap: usize) -> Self {
            Self { in_flight: AtomicUsize::new(0), cap }
        }

        /// Takes a slot, or `None` when `cap` flights are already out.
        fn acquire(&'static self) -> Option<FlightSlot> {
            self.in_flight.try_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < self.cap).then_some(count + 1)).ok().map(|_| FlightSlot(self))
        }

        /// How many flights are out.
        #[cfg(test)]
        pub(crate) fn in_flight(&self) -> usize {
            self.in_flight.load(Ordering::Acquire)
        }
    }

    /// One taken slot of `PaidFlights`, released on drop: it lives in the detached request's
    /// closure, so it is released when the send returns, when the thread unwinds from a panic,
    /// and when a failed spawn drops the closure.
    struct FlightSlot(&'static PaidFlights);

    impl Drop for FlightSlot {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Time and size limits of the client. `Default` is the production policy; tests zero the
    /// delays.
    #[derive(Debug, Clone)]
    pub(crate) struct NeurolateLimits {
        /// TCP / TLS connect timeout (handed to `UreqTransport::new`).
        pub connect: Duration,
        /// Whole-request timeout of the paid OCR request (server time is highly variable).
        pub ocr: Duration,
        /// Whole-request timeout of one `GET /key` attempt.
        pub key: Duration,
        /// Body cap of an OCR answer (masks of many blocks).
        pub ocr_cap: u64,
        /// Body cap of a key answer.
        pub key_cap: u64,
        /// Delays before the retries of a failed `GET /key`; its length is the retry count.
        pub key_retry_backoff: [Duration; 3],
        /// How often a waiting `recognize` checks its `abandon` predicate.
        pub abandon_poll: Duration,
    }

    impl Default for NeurolateLimits {
        fn default() -> Self {
            Self {
                connect: Duration::from_secs(15),
                ocr: Duration::from_mins(5),
                key: Duration::from_secs(30),
                ocr_cap: 64 * 1024 * 1024,
                key_cap: 1024 * 1024,
                key_retry_backoff: [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)],
                abandon_poll: Duration::from_millis(100),
            }
        }
    }

    /// Sends one OCR request with `api_key` and returns `NeuroLate`'s answer. The request is paid
    /// and is sent at most ONCE, whatever fails. Blocking (worker threads only): it waits for
    /// the answer, checking `abandon` every 100 ms; when `abandon` returns true it returns
    /// `Cancelled` at once and the request, if already sent, finishes detached (its cost is
    /// logged, its result discarded).
    ///
    /// At most two paid requests may be out at once, process-wide, counting abandoned ones whose
    /// send has not returned yet; a call beyond that sends nothing and fails at once.
    ///
    /// # Errors
    /// `KeyMissing` for a blank key; `RequestBuild` for a request without images;
    /// `TooManyInFlight` when earlier requests are still out; `Cancelled`;
    /// `TaskSpawn` / `WorkerLost` for the worker thread; the transport's `Network` / `Timeout` /
    /// `ResponseTooLarge`; the status errors of `protocol::status_error`; `Decode`.
    pub fn recognize(api_key: &str, request: OcrRequest, abandon: &dyn Fn() -> bool) -> Result<OcrResponse, NeurolateError> {
        let limits = NeurolateLimits::default();
        recognize_with(Arc::new(UreqTransport::new(limits.connect)), &limits, &PAID_FLIGHTS, api_key, request, abandon)
    }

    /// `recognize` over `transport`, `limits` and the flight count `flights` (the test seam).
    ///
    /// # Errors
    /// As `recognize`.
    pub(crate) fn recognize_with(transport: Arc<dyn HttpTransport + Send + Sync>, limits: &NeurolateLimits, flights: &'static PaidFlights, api_key: &str, request: OcrRequest, abandon: &dyn Fn() -> bool) -> Result<OcrResponse, NeurolateError> {
        let key = api_key.trim();
        if key.is_empty() {
            return Err(NeurolateError::KeyMissing);
        }
        if abandon() {
            return Err(NeurolateError::Cancelled);
        }
        let mode = request.mode;
        let image_count = request.images_png.len();
        // One fresh key per logical request: if the same request ever reached the server twice,
        // the second would be refused (409 `DUPLICATE_REQUEST`) instead of charged again. Not a
        // secret, so it is logged with the request id.
        let idempotency_key = uuid::Uuid::new_v4().to_string();
        let spec = HttpRequestSpec { method: HttpMethod::Post, url: format!("{BASE_URL}{OCR_PATH}"), headers: vec![(IDEMPOTENCY_HEADER, idempotency_key.clone())], body: HttpBody::Multipart(ocr_form(&request)?), auth: Some(AuthScheme::Bearer) };
        drop(request);
        vet_url(&spec.url)?;
        let options = SendOptions { timeout: limits.ocr, cap: limits.ocr_cap, stage: "ocr" };
        let Some(slot) = flights.acquire() else {
            runtime_log::log_warn(format!("{LOG_TAG} OCR {} request ({image_count} image(s)) refused: {} earlier paid requests are still out", mode.wire(), flights.cap));
            return Err(NeurolateError::TooManyInFlight);
        };
        let key = key.to_string();
        let log_key = idempotency_key.clone();
        let (result_tx, result_rx) = channel();
        // Detached worker: a blocking request cannot be aborted, and the caller must be able to
        // give up on it without waiting (teardown joins the caller's own worker).
        let spawned = ms_thread::Builder::new().name(OCR_THREAD_NAME.to_string()).spawn(move || {
            let started = Instant::now();
            let result = transport.send(&spec, Some((AuthScheme::Bearer, key.as_str())), options).map_err(|error| {
                runtime_log::log_warn(format!("{LOG_TAG} OCR {} request ({image_count} image(s), idempotency key {idempotency_key}) failed: {error}", mode.wire()));
                NeurolateError::from(error)
            });
            let result = result.and_then(|response| parse_answer(&response, mode.wire(), image_count, &idempotency_key, started));
            // The send has returned: free the slot before the answer reaches the caller, so a
            // caller that starts its next request on this answer is never refused by its own
            // last one. (A panic before this point frees it by unwinding the closure.)
            drop(slot);
            if let Err(unsent) = result_tx.send(result) {
                match unsent.0 {
                    Ok(response) => runtime_log::log_info(format!("{LOG_TAG} OCR request {} finished after the caller gave up (cost {}); result discarded", response.request_id, cost_text(response.cost))),
                    Err(error) => runtime_log::log_info(format!("{LOG_TAG} OCR request failed after the caller gave up: {error:?}; result discarded")),
                }
            }
        });
        if let Err(error) = spawned {
            runtime_log::log_error(format!("{LOG_TAG} failed to spawn the {OCR_THREAD_NAME} thread: {error}"));
            return Err(NeurolateError::TaskSpawn { detail: error.to_string() });
        }
        loop {
            match result_rx.recv_timeout(limits.abandon_poll) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {
                    if abandon() {
                        runtime_log::log_info(format!("{LOG_TAG} OCR {} request ({image_count} image(s), idempotency key {log_key}) abandoned by the caller; it may still be charged", mode.wire()));
                        return Err(NeurolateError::Cancelled);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    runtime_log::log_error(format!("{LOG_TAG} the {OCR_THREAD_NAME} thread ended without answering"));
                    return Err(NeurolateError::WorkerLost);
                }
            }
        }
    }

    /// Parses an OCR answer and logs its outcome (success: request id, blocks per image, cost,
    /// duration; failure: status plus `NeuroLate`'s code / message and the body digest).
    fn parse_answer(response: &HttpResponse, mode: &str, image_count: usize, idempotency_key: &str, started: Instant) -> Result<OcrResponse, NeurolateError> {
        let elapsed_ms = started.elapsed().as_millis();
        match parse_ocr_response(response.status, &response.body, image_count) {
            Ok(parsed) => {
                let blocks: Vec<usize> = parsed.images.iter().map(|image| image.blocks.len()).collect();
                let image_errors: Vec<&str> = parsed.images.iter().filter_map(|image| image.error.as_ref().map(|error| error.code.as_str())).collect();
                runtime_log::log_info(format!("{LOG_TAG} OCR {mode}: {image_count} image(s), request {}, idempotency key {idempotency_key}, blocks per image {blocks:?}, image errors {image_errors:?}, cost {}, {elapsed_ms} ms", parsed.request_id, cost_text(parsed.cost)));
                Ok(parsed)
            }
            Err(error) => {
                log_failure(&format!("OCR {mode} ({image_count} image(s), idempotency key {idempotency_key}, {elapsed_ms} ms)"), response, &error);
                Err(error)
            }
        }
    }

    /// Logs a failed answer: what was asked, the status, the typed error and the body digest.
    /// A 401 also says that `NeuroLate` answers 401 for a missing form field.
    fn log_failure(what: &str, response: &HttpResponse, error: &NeurolateError) {
        let note = if response.status == 401 { " (NeuroLate also answers 401 for a missing form field)" } else { "" };
        runtime_log::log_warn(format!("{LOG_TAG} {what} answered HTTP {}{note}: {error:?}; body: {}", response.status, log_body(&response.body)));
    }

    /// Refuses a URL outside `BASE_URL`'s origin before the key is attached.
    fn vet_url(url: &str) -> Result<(), NeurolateError> {
        if auth_allowed(url, BASE_URL, &[]) { Ok(()) } else { Err(NeurolateError::RequestBuild { detail: format!("refusing to send the NeuroLate key to {url}") }) }
    }

    /// Reads the key's name, spending limit and use (`GET /ext/v1/key`, free). Blocking
    /// (worker threads only).
    ///
    /// # Errors
    /// `KeyMissing` for a blank key; the transport's errors after the retries; the status
    /// errors of `protocol::status_error` (`KeyRejected` for a bad key); `Decode`.
    pub fn fetch_key_info(api_key: &str) -> Result<KeyInfo, NeurolateError> {
        let limits = NeurolateLimits::default();
        fetch_key_info_with(&UreqTransport::new(limits.connect), &limits, api_key)
    }

    /// `fetch_key_info` over `transport` and `limits` (the test seam). A network failure, a
    /// timeout or a 5xx answer is retried after each delay of `key_retry_backoff`; anything
    /// else is final.
    ///
    /// # Errors
    /// As `fetch_key_info`.
    pub(crate) fn fetch_key_info_with(transport: &dyn HttpTransport, limits: &NeurolateLimits, api_key: &str) -> Result<KeyInfo, NeurolateError> {
        let key = api_key.trim();
        if key.is_empty() {
            return Err(NeurolateError::KeyMissing);
        }
        let spec = HttpRequestSpec { method: HttpMethod::Get, url: format!("{BASE_URL}{KEY_PATH}"), headers: Vec::new(), body: HttpBody::Empty, auth: Some(AuthScheme::Bearer) };
        vet_url(&spec.url)?;
        let options = SendOptions { timeout: limits.key, cap: limits.key_cap, stage: "key" };
        let mut delays = limits.key_retry_backoff.iter().filter(|_| retry_allowed(spec.method));
        loop {
            let result = transport.send(&spec, Some((AuthScheme::Bearer, key)), options);
            let transient = match &result {
                Ok(response) => response.status >= 500,
                Err(error) => matches!(error, crate::http::HttpError::Network { .. } | crate::http::HttpError::Timeout { .. }),
            };
            if transient && let Some(delay) = delays.next() {
                let failure = result.as_ref().map_or_else(ToString::to_string, |response| format!("HTTP {}", response.status));
                runtime_log::log_warn(format!("{LOG_TAG} key request failed ({failure}); retrying in {} ms", delay.as_millis()));
                std::thread::sleep(*delay);
                continue;
            }
            let response = result.map_err(|error| {
                runtime_log::log_warn(format!("{LOG_TAG} key request failed: {error}"));
                NeurolateError::from(error)
            })?;
            return parse_key_info(response.status, &response.body).inspect_err(|error| log_failure("key request", &response, error));
        }
    }

    #[cfg(test)]
    mod tests {
        use std::collections::VecDeque;
        use std::sync::mpsc::{Receiver, Sender, channel};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        use super::{NeurolateLimits, PaidFlights, fetch_key_info_with, recognize_with};
        use crate::http::{AuthScheme, HttpError, HttpMethod, HttpRequestSpec, HttpResponse, HttpTransport, SendOptions};
        use crate::neurolate::error::NeurolateError;
        use crate::neurolate::protocol::{NeurolateComicType, NeurolateLanguage, OcrMode, OcrParams, OcrRequest};

        const OCR_PAGE: &str = include_str!("../../fixtures/neurolate/ocr_page.json");
        const KEY: &str = include_str!("../../fixtures/neurolate/key.json");

        /// One recorded request: method, URL, whether the key was attached, the auth scheme.
        #[derive(Debug, Clone, PartialEq)]
        struct Sent {
            method: HttpMethod,
            url: String,
            key: Option<String>,
        }

        /// A scripted transport: answers from `script` in order, records every request. With
        /// `gate`, each send first blocks until the test releases it.
        struct FakeTransport {
            script: Mutex<VecDeque<Result<HttpResponse, HttpError>>>,
            sent: Mutex<Vec<Sent>>,
            headers: Mutex<Vec<Vec<(&'static str, String)>>>,
            gate: Option<Mutex<Receiver<()>>>,
            entered: Option<Mutex<Sender<()>>>,
        }

        impl FakeTransport {
            fn new(script: Vec<Result<HttpResponse, HttpError>>) -> Self {
                Self { script: Mutex::new(script.into()), sent: Mutex::new(Vec::new()), headers: Mutex::new(Vec::new()), gate: None, entered: None }
            }
            fn sent(&self) -> Vec<Sent> {
                self.sent.lock().map(|sent| sent.clone()).unwrap_or_default()
            }
        }

        impl HttpTransport for FakeTransport {
            fn send(&self, request: &HttpRequestSpec, auth: Option<(AuthScheme, &str)>, _options: SendOptions) -> Result<HttpResponse, HttpError> {
                if let Ok(mut sent) = self.sent.lock() {
                    sent.push(Sent { method: request.method, url: request.url.clone(), key: auth.map(|(_, key)| key.to_string()) });
                }
                if let Ok(mut headers) = self.headers.lock() {
                    headers.push(request.headers.clone());
                }
                if let Some(entered) = &self.entered
                    && let Ok(entered) = entered.lock()
                {
                    entered.send(()).unwrap_or_else(|error| panic!("test channel: {error}"));
                }
                if let Some(gate) = &self.gate
                    && let Ok(gate) = gate.lock()
                {
                    gate.recv().unwrap_or_else(|error| panic!("test gate: {error}"));
                }
                self.script.lock().ok().and_then(|mut script| script.pop_front()).unwrap_or_else(|| Err(HttpError::Network { detail: "script exhausted".to_string() }))
            }
        }

        fn answer(status: u16, body: &str) -> HttpResponse {
            HttpResponse { status, content_type: Some("application/json".to_string()), body: body.as_bytes().to_vec() }
        }

        fn fast_limits() -> NeurolateLimits {
            NeurolateLimits { key_retry_backoff: [Duration::ZERO; 3], abandon_poll: Duration::from_millis(5), ..NeurolateLimits::default() }
        }

        /// A fresh flight count per call (cap 2), so parallel tests never share slots.
        fn flights() -> &'static PaidFlights {
            Box::leak(Box::new(PaidFlights::new(2)))
        }

        fn request() -> OcrRequest {
            OcrRequest { mode: OcrMode::Mask, kind: None, params: OcrParams { language: NeurolateLanguage::Ko, comic_type: NeurolateComicType::Webtoon }, images_png: vec![vec![1, 2, 3]] }
        }

        #[test]
        fn recognize_sends_one_bearer_post_to_the_api_origin() {
            let transport = Arc::new(FakeTransport::new(vec![Ok(answer(200, OCR_PAGE))]));
            let response = recognize_with(transport.clone(), &fast_limits(), flights(), " nlk_secret ", request(), &|| false).unwrap_or_else(|error| panic!("recognize: {error:?}"));
            assert_eq!(response.images.len(), 1);
            assert_eq!(transport.sent(), [Sent { method: HttpMethod::Post, url: "https://api.neurolate.com/ext/v1/ocr".to_string(), key: Some("nlk_secret".to_string()) }]);
        }

        #[test]
        fn every_post_carries_its_own_idempotency_key() {
            let transport = Arc::new(FakeTransport::new(vec![Ok(answer(200, OCR_PAGE)), Ok(answer(200, OCR_PAGE))]));
            for _ in 0..2 {
                assert!(recognize_with(transport.clone(), &fast_limits(), flights(), "nlk_k", request(), &|| false).is_ok());
            }
            let keys: Vec<String> = transport.headers.lock().map(|headers| headers.iter().map(|headers| match headers.as_slice() {
                [("Idempotency-Key", key)] => key.clone(),
                other => panic!("headers {other:?}"),
            }).collect()).unwrap_or_default();
            assert_eq!(keys.len(), 2);
            assert_ne!(keys[0], keys[1], "one key per logical request");
            assert!(keys.iter().all(|key| uuid::Uuid::parse_str(key).is_ok_and(|uuid| uuid.get_version_num() == 4)), "{keys:?}");
        }

        #[test]
        fn a_duplicate_request_answer_is_its_own_error_and_not_retried() {
            let transport = Arc::new(FakeTransport::new(vec![Ok(answer(409, r#"{"code":"DUPLICATE_REQUEST","message":"used","requestId":"00000000-0000-0000-0000-000000000001"}"#)), Ok(answer(200, OCR_PAGE))]));
            let result = recognize_with(transport.clone(), &fast_limits(), flights(), "nlk_k", request(), &|| false);
            assert!(matches!(result, Err(NeurolateError::DuplicateRequest { first_request_id: Some(ref id) }) if id == "00000000-0000-0000-0000-000000000001"), "{result:?}");
            assert_eq!(transport.sent().len(), 1);
        }

        #[test]
        fn a_failed_post_is_never_retried() {
            for failure in [Err(HttpError::Network { detail: "reset".to_string() }), Err(HttpError::Timeout { stage: "ocr".to_string() }), Ok(answer(500, ""))] {
                let transport = Arc::new(FakeTransport::new(vec![failure, Ok(answer(200, OCR_PAGE))]));
                let result = recognize_with(transport.clone(), &fast_limits(), flights(), "nlk_k", request(), &|| false);
                assert!(result.is_err(), "{result:?}");
                assert_eq!(transport.sent().len(), 1, "a paid POST is sent exactly once");
            }
        }

        #[test]
        fn a_blank_key_or_an_early_abandon_sends_nothing() {
            let transport = Arc::new(FakeTransport::new(vec![Ok(answer(200, OCR_PAGE))]));
            assert!(matches!(recognize_with(transport.clone(), &fast_limits(), flights(), "  ", request(), &|| false), Err(NeurolateError::KeyMissing)));
            assert!(matches!(recognize_with(transport.clone(), &fast_limits(), flights(), "nlk_k", request(), &|| true), Err(NeurolateError::Cancelled)));
            assert_eq!(transport.sent(), [] as [Sent; 0]);
        }

        #[test]
        fn abandon_returns_at_once_while_the_request_is_still_in_flight() {
            let (release_tx, release_rx) = channel();
            let (entered_tx, entered_rx) = channel();
            let transport = Arc::new(FakeTransport { gate: Some(Mutex::new(release_rx)), entered: Some(Mutex::new(entered_tx)), ..FakeTransport::new(vec![Ok(answer(200, OCR_PAGE))]) });
            let started = Instant::now();
            // Abandon only once the request is really in flight inside the transport.
            let abandon = || entered_rx.try_recv().is_ok() || started.elapsed() > Duration::from_secs(1);
            let result = recognize_with(transport.clone(), &fast_limits(), flights(), "nlk_k", request(), &abandon);
            assert!(matches!(result, Err(NeurolateError::Cancelled)), "{result:?}");
            assert!(started.elapsed() < Duration::from_secs(5), "returned without waiting for the blocked request");
            assert_eq!(transport.sent().len(), 1);
            // Let the detached request finish so no thread outlives the test.
            release_tx.send(()).unwrap_or_else(|error| panic!("release: {error}"));
        }

        /// Waits (10 s deadline) until `flights` has `count` flights out.
        fn wait_for_flights(flights: &PaidFlights, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while flights.in_flight() != count && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(flights.in_flight(), count);
        }

        #[test]
        fn abandoned_requests_keep_their_slot_until_their_send_returns() {
            let (release_tx, release_rx) = channel();
            let (entered_tx, entered_rx) = channel();
            let transport = Arc::new(FakeTransport { gate: Some(Mutex::new(release_rx)), entered: Some(Mutex::new(entered_tx)), ..FakeTransport::new((0..3).map(|_| Ok(answer(200, OCR_PAGE))).collect()) });
            let flights = flights();
            // Two requests abandoned while their sends are blocked: both slots stay taken.
            for _ in 0..2 {
                let abandon = || entered_rx.try_recv().is_ok();
                assert!(matches!(recognize_with(transport.clone(), &fast_limits(), flights, "nlk_k", request(), &abandon), Err(NeurolateError::Cancelled)));
            }
            assert_eq!(flights.in_flight(), 2);
            let started = Instant::now();
            assert!(matches!(recognize_with(transport.clone(), &fast_limits(), flights, "nlk_k", request(), &|| false), Err(NeurolateError::TooManyInFlight)));
            assert!(started.elapsed() < Duration::from_secs(5), "refused at once");
            assert_eq!(transport.sent().len(), 2, "the refused request sent nothing");
            // Their sends return: the slots are free and a new request goes out.
            for _ in 0..3 {
                release_tx.send(()).unwrap_or_else(|error| panic!("release: {error}"));
            }
            wait_for_flights(flights, 0);
            let response = recognize_with(transport.clone(), &fast_limits(), flights, "nlk_k", request(), &|| false);
            assert!(response.is_ok(), "{response:?}");
            assert_eq!((transport.sent().len(), flights.in_flight()), (3, 0));
        }

        #[test]
        fn a_panicking_send_frees_its_slot() {
            struct Panics;
            impl HttpTransport for Panics {
                fn send(&self, _request: &HttpRequestSpec, _auth: Option<(AuthScheme, &str)>, _options: SendOptions) -> Result<HttpResponse, HttpError> {
                    panic!("test transport: simulated crash")
                }
            }
            let flights = flights();
            assert!(matches!(recognize_with(Arc::new(Panics), &fast_limits(), flights, "nlk_k", request(), &|| false), Err(NeurolateError::WorkerLost)));
            wait_for_flights(flights, 0);
        }

        #[test]
        fn the_key_request_retries_transient_failures_then_succeeds() {
            let transport = FakeTransport::new(vec![Err(HttpError::Network { detail: "reset".to_string() }), Ok(answer(503, "")), Err(HttpError::Timeout { stage: "key".to_string() }), Ok(answer(200, KEY))]);
            let info = fetch_key_info_with(&transport, &fast_limits(), "nlk_k").unwrap_or_else(|error| panic!("key info: {error:?}"));
            assert_eq!(info.key_prefix, "nlk_TEST0000");
            let sent = transport.sent();
            assert_eq!(sent.len(), 4);
            assert!(sent.iter().all(|sent| sent.method == HttpMethod::Get && sent.url == "https://api.neurolate.com/ext/v1/key" && sent.key.as_deref() == Some("nlk_k")));
        }

        #[test]
        fn the_key_request_gives_up_after_its_retries_and_never_retries_a_rejection() {
            let transport = FakeTransport::new((0..5).map(|_| Ok(answer(502, ""))).collect());
            assert!(matches!(fetch_key_info_with(&transport, &fast_limits(), "nlk_k"), Err(NeurolateError::ServiceFailed { .. })));
            assert_eq!(transport.sent().len(), 4, "one try plus three retries");
            let transport = FakeTransport::new(vec![Ok(answer(401, "")), Ok(answer(200, KEY))]);
            assert!(matches!(fetch_key_info_with(&transport, &fast_limits(), "nlk_k"), Err(NeurolateError::KeyRejected)));
            assert_eq!(transport.sent().len(), 1);
        }
    }
}

/// Web build: no HTTP client; every request is `WebUnavailable`.
#[cfg(target_arch = "wasm32")]
mod web {
    use crate::neurolate::error::NeurolateError;
    use crate::neurolate::protocol::{KeyInfo, OcrRequest, OcrResponse};

    /// Web stub of the native `recognize`.
    ///
    /// # Errors
    /// Always `WebUnavailable`.
    pub fn recognize(_api_key: &str, _request: OcrRequest, _abandon: &dyn Fn() -> bool) -> Result<OcrResponse, NeurolateError> {
        Err(NeurolateError::WebUnavailable)
    }

    /// Web stub of the native `fetch_key_info`.
    ///
    /// # Errors
    /// Always `WebUnavailable`.
    pub fn fetch_key_info(_api_key: &str) -> Result<KeyInfo, NeurolateError> {
        Err(NeurolateError::WebUnavailable)
    }
}
