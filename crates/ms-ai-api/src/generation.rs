/*
File: crates/ms-ai-api/src/generation.rs

Purpose:
Live progress and cancellation of one LLM generation: the GUI-free `GenerationTracker` a
consumer shares between its worker and its panel, and (native only) `exec_chat_tracked`, the
one streaming executor that runs a chat request while feeding that tracker and aborting the
HTTP request when the user presses "Stop".

Key structures:
- GenerationPhase    : `Thinking` (request start, reasoning chunks) / `Answering` (answer chunks).
- GenerationSnapshot : what the status widget draws (active, phase, char counts) plus the run id
                       and caller tag a "Stop" click targets.
- GenerationTracker  : cheap `Clone` (`Arc`), `Send + Sync`; `begin` / `cancel_run` / `close` /
                       `is_closed` / `snapshot`.
- GenerationRun      : the worker-side guard of one generation (records chunks, exposes the
                       cancellation future, finishes on drop).

Key functions:
- exec_chat_tracked()      : native-only streaming executor (`genai::Client::exec_chat_stream`),
                             `AiApiError::Cancelled` on Stop, an error for a stream without its
                             terminal event, one non-streaming retry when the provider forbids
                             streaming the model.
- streaming_not_permitted(): native-only, crate-private; recognizes that provider refusal.
- normalize_think_answer() : native-only, crate-private; genai's non-streaming `<think>`
                             extraction, reproduced for the streamed answer.

Notes:
The cancellation future is a plain `std::task::Waker` slot inside the tracker's mutex (one
awaiting task per run), so the tracker stays target-neutral and needs no tokio `sync`.
Dropping the `genai` stream (or the pending `exec_chat_stream` future) closes the reqwest
connection, which is what really aborts the provider request.
*/

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

/// Which part of its output a model is producing right now.
///
/// A generation starts in `Thinking` (that also covers providers that reason silently and stream
/// nothing until the answer, e.g. the official `OpenAI` API) and follows the kind of the last
/// non-empty chunk: an answer chunk switches to `Answering`, a reasoning chunk back to `Thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationPhase {
    /// No answer text yet, or the model is streaming reasoning.
    Thinking,
    /// The model is streaming answer text.
    Answering,
}

/// A point-in-time view of a tracker, cheap to copy into a panel each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationSnapshot {
    /// A generation is in flight; the status widget draws nothing otherwise.
    pub active: bool,
    /// Phase of the in-flight (or last) generation.
    pub phase: GenerationPhase,
    /// Reasoning characters (Unicode scalar values, not bytes) received so far.
    pub reasoning_chars: usize,
    /// Answer characters (Unicode scalar values, not bytes) received so far.
    pub answer_chars: usize,
    /// Id of the generation shown (`0` before the tracker's first `begin`); a "Stop" click passes
    /// it to `GenerationTracker::cancel_run`, so it can never stop a later generation.
    pub run_id: u64,
    /// The caller's tag of that generation (`exec_chat_tracked`'s `request_tag`, e.g. the OCR
    /// request id), so the consumer knows which of its requests the click was about.
    pub request_tag: u64,
}

impl GenerationSnapshot {
    /// The inactive snapshot (nothing received, nothing to draw).
    pub const IDLE: Self = Self { active: false, phase: GenerationPhase::Thinking, reasoning_chars: 0, answer_chars: 0, run_id: 0, request_tag: 0 };

    /// Every output character received so far: reasoning plus answer.
    #[must_use]
    pub fn total_chars(&self) -> usize {
        self.reasoning_chars.saturating_add(self.answer_chars)
    }
}

impl Default for GenerationSnapshot {
    fn default() -> Self {
        Self::IDLE
    }
}

/// Mutable tracker state; every critical section only assigns plain fields (counters use
/// saturating arithmetic), so no section can panic half-way through an update.
#[derive(Debug)]
struct TrackerState {
    /// Id of the most recently begun run; a `GenerationRun` with another id is stale.
    run_id: u64,
    /// Caller tag of that run.
    request_tag: u64,
    active: bool,
    phase: GenerationPhase,
    reasoning_chars: usize,
    answer_chars: usize,
    /// The current run was cancelled.
    cancelled: bool,
    /// `close` was called: the current run and every later one start cancelled.
    closed: bool,
    /// Waker of the task awaiting the current run's cancellation.
    waker: Option<Waker>,
}

impl Default for TrackerState {
    fn default() -> Self {
        Self { run_id: 0, request_tag: 0, active: false, phase: GenerationPhase::Thinking, reasoning_chars: 0, answer_chars: 0, cancelled: false, closed: false, waker: None }
    }
}

/// Shared progress and cancel switch of a consumer's LLM generations, one at a time.
///
/// The consumer keeps one clone on the GUI side (`snapshot`, `cancel_run`, `close`) and hands
/// another to the worker that runs `exec_chat_tracked`. All methods are non-blocking apart
/// from a tiny mutex section, so they are safe to call on the GUI thread every frame.
#[derive(Debug, Clone, Default)]
pub struct GenerationTracker {
    state: Arc<Mutex<TrackerState>>,
}

impl GenerationTracker {
    /// A tracker with no generation in flight.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, TrackerState> {
        // A poisoned lock still holds consistent state: no critical section can panic between
        // two related assignments (see `TrackerState`), so recovering the guard is sound.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current progress, for the status widget.
    #[must_use]
    pub fn snapshot(&self) -> GenerationSnapshot {
        let state = self.lock();
        GenerationSnapshot { active: state.active, phase: state.phase, reasoning_chars: state.reasoning_chars, answer_chars: state.answer_chars, run_id: state.run_id, request_tag: state.request_tag }
    }

    /// Starts a new generation tagged `request_tag` (echoed in the snapshot): counters reset,
    /// phase `Thinking`, active until the returned run is dropped. A run still in flight is
    /// superseded (it then reads as cancelled and its chunks are ignored). After `close`, the new
    /// run starts already cancelled.
    #[must_use]
    pub fn begin(&self, request_tag: u64) -> GenerationRun {
        let (run_id, superseded_waker) = {
            let mut state = self.lock();
            state.run_id = state.run_id.wrapping_add(1);
            state.request_tag = request_tag;
            state.active = true;
            state.phase = GenerationPhase::Thinking;
            state.reasoning_chars = 0;
            state.answer_chars = 0;
            state.cancelled = state.closed;
            (state.run_id, state.waker.take())
        };
        // The superseded run's awaiting task must observe that it is stale.
        if let Some(waker) = superseded_waker {
            waker.wake();
        }
        GenerationRun { tracker: self.clone(), run_id }
    }

    /// Cancels generation `run_id` (a snapshot's `run_id`) if it is still the one in flight: its
    /// executor returns `AiApiError::Cancelled` and drops the request. A finished or superseded
    /// run is left alone, so a click on a stale frame never stops a later generation. A later
    /// `begin` starts uncancelled (unless `close` was called).
    pub fn cancel_run(&self, run_id: u64) {
        let waker = {
            let mut state = self.lock();
            if !state.active || state.run_id != run_id {
                return;
            }
            state.cancelled = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Cancels the generation in flight AND every later one, for good: for a consumer that
    /// stops a whole run (or is shutting down) and must not let a request slip through between
    /// its own "cancelled?" check and the next `begin`.
    pub fn close(&self) {
        let waker = {
            let mut state = self.lock();
            state.closed = true;
            state.cancelled = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// `true` once `close` was called: every generation begun from now on is cancelled at once,
    /// so a worker may skip its preparation (keyring reads, image encoding) for it.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

/// The worker-side guard of one generation started by `GenerationTracker::begin`.
///
/// Dropping it ends the generation (the snapshot turns inactive, counters are kept). A stale
/// run (superseded by a newer `begin`) records nothing and reads as cancelled.
#[derive(Debug)]
pub struct GenerationRun {
    tracker: GenerationTracker,
    run_id: u64,
}

impl GenerationRun {
    /// Counts a reasoning chunk (in chars) and switches to `Thinking` when it is non-empty.
    pub fn record_reasoning(&self, text: &str) {
        self.record(text, GenerationPhase::Thinking);
    }

    /// Counts an answer chunk (in chars) and switches to `Answering` when it is non-empty.
    pub fn record_answer(&self, text: &str) {
        self.record(text, GenerationPhase::Answering);
    }

    fn record(&self, text: &str, phase: GenerationPhase) {
        if text.is_empty() {
            return;
        }
        let chars = text.chars().count();
        let mut state = self.tracker.lock();
        if state.run_id != self.run_id {
            return;
        }
        state.phase = phase;
        match phase {
            GenerationPhase::Thinking => state.reasoning_chars = state.reasoning_chars.saturating_add(chars),
            GenerationPhase::Answering => state.answer_chars = state.answer_chars.saturating_add(chars),
        }
    }

    /// `true` once this run was cancelled, closed or superseded.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        let state = self.tracker.lock();
        state.run_id != self.run_id || state.cancelled
    }

    /// A future that completes once this run is cancelled, closed or superseded. Only one task
    /// may await a run's cancellation at a time (a second waiter replaces the first's waker).
    #[must_use]
    pub fn cancelled(&self) -> RunCancelled<'_> {
        RunCancelled { run: self }
    }
}

impl Drop for GenerationRun {
    fn drop(&mut self) {
        let mut state = self.tracker.lock();
        if state.run_id == self.run_id {
            state.active = false;
            state.waker = None;
        }
    }
}

/// Future returned by `GenerationRun::cancelled`.
#[derive(Debug)]
pub struct RunCancelled<'a> {
    run: &'a GenerationRun,
}

impl Future for RunCancelled<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.run.tracker.lock();
        if state.run_id != self.run.run_id || state.cancelled {
            return Poll::Ready(());
        }
        // Checked and registered under the same lock `cancel` takes, so a cancel can never
        // slip between the check and the registration (no lost wake-up).
        match &mut state.waker {
            Some(waker) if waker.will_wake(cx.waker()) => {}
            slot => *slot = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::exec_chat_tracked;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use futures_util::StreamExt;
    use genai::adapter::AdapterKind;
    use genai::chat::{ChatOptions, ChatRequest, ChatStreamEvent};
    use genai::{Client, ModelIden};
    use ms_log::runtime_log;

    use super::{GenerationRun, GenerationTracker};
    use crate::error::AiApiError;

    /// Runs `request` against `model` as a stream, feeding `tracker` per chunk (the generation is
    /// tagged `request_tag`, echoed in the snapshot), and returns the answer text: the
    /// concatenation of EVERY answer chunk, i.e. of all text parts of the reply (reasoning is
    /// counted, never returned). This is the one owner of tracked LLM execution; call it inside
    /// `client::block_on` on a worker thread.
    ///
    /// `options` are the caller's chat options, sent unchanged: reasoning capture is deliberately
    /// NOT added, because for Gemini it injects `thinkingConfig.includeThoughts`, which models
    /// without thinking support reject (their hidden thinking simply shows as `Thinking`). When
    /// `options` request `normalize_reasoning_content` and the model's adapter is one whose
    /// non-streaming parser applies it (the `OpenAI` chat family), the answer goes through the
    /// same `<think>` extraction `genai` would have applied to an `exec_chat` response
    /// (`normalize_think_answer`), unless real reasoning chunks arrived. The returned text is
    /// otherwise untrimmed.
    ///
    /// A stream must end with the provider's terminal event: a body that closes before it
    /// (a truncated connection, or a mid-stream provider error `genai` does not surface, such as
    /// Anthropic's `event: error`) is an error, never a partial answer.
    ///
    /// Non-streaming fallback: when the stream request fails before any output arrived with an
    /// error saying the provider does not permit streaming this model (`streaming_not_permitted`,
    /// e.g. `OpenAI`'s "organization must be verified to stream this model"), the request is
    /// retried ONCE with `exec_chat` and the caller's options (genai then applies its own
    /// `<think>` normalization). That reply has no live counts (the phase stays `Thinking`) and
    /// is its first text part, as before streaming. A refused stream produced no output, so the
    /// retry sends the request only once more.
    ///
    /// Every await races the run's cancellation: `GenerationTracker::cancel_run` / `close` drop
    /// the pending request or stream at once, which closes the HTTP connection.
    ///
    /// # Errors
    /// `AiApiError::Cancelled` when the generation was cancelled (or the tracker closed) before
    /// or during the request; `AiApiError::ChatRequest` with the provider error text when the
    /// request or the stream failed, or with the localized "stream ended early" text when the
    /// terminal event never came. All are logged here.
    pub async fn exec_chat_tracked(client: &Client, model: ModelIden, request: ChatRequest, options: Option<&ChatOptions>, tracker: &GenerationTracker, request_tag: u64) -> Result<String, AiApiError> {
        let run = tracker.begin(request_tag);
        let model_name = model.model_name.to_string();
        if run.is_cancelled() {
            return Err(cancelled(&model_name));
        }
        let normalize = options.and_then(|options| options.normalize_reasoning_content) == Some(true) && adapter_normalizes_think_tags(model.adapter_kind);
        // Kept for the one non-streaming retry; the streaming call consumes its own copy.
        let fallback_request = request.clone();
        match stream_answer(client, model.clone(), request, options, &run).await {
            StreamOutcome::Done { answer, reasoning_received } => {
                if normalize && !reasoning_received {
                    return Ok(normalize_think_answer(&answer));
                }
                Ok(answer)
            }
            StreamOutcome::Cancelled => Err(cancelled(&model_name)),
            StreamOutcome::Incomplete => {
                runtime_log::log_error(format!("[AI API] chat stream for '{model_name}' ended before the provider's terminal event; the partial answer is discarded"));
                Err(AiApiError::ChatRequest { detail: t!("ai_api.generation.stream_incomplete_error").to_string() })
            }
            StreamOutcome::Failed { error, output_received } => {
                let detail = error.to_string();
                if output_received || !streaming_not_permitted(&detail) {
                    return Err(request_failed(&model_name, &error));
                }
                runtime_log::log_warn(format!("[AI API] streaming '{model_name}' was refused by the provider ({detail}); retrying once without streaming"));
                let reply = tokio::select! {
                    biased;
                    () = run.cancelled() => return Err(cancelled(&model_name)),
                    reply = client.exec_chat(model, fallback_request, options) => reply,
                };
                match reply {
                    Ok(response) => Ok(response.first_text().unwrap_or("").to_string()),
                    Err(err) => Err(request_failed(&model_name, &err)),
                }
            }
        }
    }

    /// How one streaming attempt ended.
    enum StreamOutcome {
        /// The terminal event arrived; `answer` is every answer chunk concatenated.
        Done { answer: String, reasoning_received: bool },
        /// The run was cancelled; the request / stream was dropped.
        Cancelled,
        /// The body ended without the provider's terminal event.
        Incomplete,
        /// The request or the stream failed; `output_received` tells whether any non-empty
        /// answer or reasoning chunk had arrived before.
        Failed { error: genai::Error, output_received: bool },
    }

    /// One streaming attempt of `request`, recording chunks on `run` and racing every await
    /// against its cancellation.
    async fn stream_answer(client: &Client, model: ModelIden, request: ChatRequest, options: Option<&ChatOptions>, run: &GenerationRun) -> StreamOutcome {
        let started = tokio::select! {
            biased;
            () = run.cancelled() => return StreamOutcome::Cancelled,
            response = client.exec_chat_stream(model, request, options) => response,
        };
        let mut stream = match started {
            Ok(response) => response.stream,
            Err(error) => return StreamOutcome::Failed { error, output_received: false },
        };
        let mut answer = String::new();
        let mut reasoning_received = false;
        loop {
            let next = tokio::select! {
                biased;
                () = run.cancelled() => return StreamOutcome::Cancelled,
                next = stream.next() => next,
            };
            match next {
                None => return StreamOutcome::Incomplete,
                Some(Err(error)) => return StreamOutcome::Failed { error, output_received: reasoning_received || !answer.is_empty() },
                Some(Ok(ChatStreamEvent::Chunk(chunk))) => {
                    run.record_answer(&chunk.content);
                    answer.push_str(&chunk.content);
                }
                Some(Ok(ChatStreamEvent::ReasoningChunk(chunk))) => {
                    reasoning_received |= !chunk.content.is_empty();
                    run.record_reasoning(&chunk.content);
                }
                Some(Ok(ChatStreamEvent::Start | ChatStreamEvent::ThoughtSignatureChunk(_) | ChatStreamEvent::ToolCallChunk(_))) => {}
                Some(Ok(ChatStreamEvent::End(_))) => return StreamOutcome::Done { answer, reasoning_received },
            }
        }
    }

    /// `true` when a provider error says streaming is not permitted for this model / account, the
    /// one case `exec_chat_tracked` retries without streaming. Deliberately conservative: the text
    /// must mention streaming AND verification or the organization (`OpenAI`: "Your organization
    /// must be verified to stream this model"), matched case-insensitively.
    pub(crate) fn streaming_not_permitted(error_text: &str) -> bool {
        let text = error_text.to_lowercase();
        text.contains("stream") && (text.contains("verif") || text.contains("organization"))
    }

    /// Adapters whose non-streaming response parser is `OpenAIAdapter::to_chat_response`, the only
    /// one in `genai` 0.6 that honours `normalize_reasoning_content` (genai
    /// `src/adapter/adapters/openai/adapter_impl.rs`, reused by each of these adapters).
    fn adapter_normalizes_think_tags(kind: AdapterKind) -> bool {
        matches!(
            kind,
            AdapterKind::OpenAI
                | AdapterKind::OpenRouter
                | AdapterKind::Groq
                | AdapterKind::DeepSeek
                | AdapterKind::Xai
                | AdapterKind::Fireworks
                | AdapterKind::Together
                | AdapterKind::Aihubmix
                | AdapterKind::Mimo
                | AdapterKind::Moonshot
                | AdapterKind::Nebius
                | AdapterKind::Zai
                | AdapterKind::BigModel
                | AdapterKind::Aliyun
                | AdapterKind::Baidu
                | AdapterKind::GithubCopilot
                | AdapterKind::OpenCodeGo
        )
    }

    /// `genai`'s non-streaming `<think>` normalization, reproduced byte for byte: the text is
    /// trimmed, then the FIRST `<think>…</think>` block (anywhere) is removed together with the
    /// whitespace that follows it; text before the block is kept. Without a closed block the
    /// trimmed text is returned unchanged.
    pub(crate) fn normalize_think_answer(text: &str) -> String {
        const START_TAG: &str = "<think>";
        const END_TAG: &str = "</think>";
        let content = text.trim();
        if let Some(start) = content.find(START_TAG) {
            let body_start = start + START_TAG.len();
            if let Some(end) = content[body_start..].find(END_TAG) {
                let after = content[body_start + end + END_TAG.len()..].trim_start();
                return format!("{}{after}", &content[..start]);
            }
        }
        content.to_string()
    }

    fn cancelled(model_name: &str) -> AiApiError {
        runtime_log::log_info(format!("[AI API] chat generation for '{model_name}' cancelled (stop, close or superseded); request dropped"));
        AiApiError::Cancelled
    }

    fn request_failed(model_name: &str, err: &genai::Error) -> AiApiError {
        runtime_log::log_error(format!("[AI API] chat stream for '{model_name}' failed: {err}"));
        AiApiError::ChatRequest { detail: err.to_string() }
    }

    #[cfg(test)]
    mod tests {
        use super::{normalize_think_answer, streaming_not_permitted};

        #[test]
        fn recognizes_only_the_streaming_permission_refusal() {
            assert!(streaming_not_permitted("Status: 400 Bad Request\nBody: {\"error\":{\"message\":\"Your organization must be verified to stream this model. Please go to: https://platform.openai.com/settings/organization/general\",\"param\":\"stream\"}}"));
            assert!(streaming_not_permitted("STREAMING requires a VERIFIED account"));
            assert!(!streaming_not_permitted("Status: 429 Too Many Requests"));
            assert!(!streaming_not_permitted("Your organization has exceeded its quota"));
            assert!(!streaming_not_permitted("stream ended unexpectedly"));
            assert!(!streaming_not_permitted(""));
        }

        #[test]
        fn strips_a_leading_think_block_and_following_whitespace() {
            assert_eq!(normalize_think_answer("<think>plan\nmore</think>\n\n[{\"id\":1}]"), "[{\"id\":1}]");
        }

        #[test]
        fn trims_before_extracting_and_keeps_text_before_the_block() {
            assert_eq!(normalize_think_answer("  \nlead <think> x </think>  tail \n"), "lead tail");
        }

        #[test]
        fn removes_only_the_first_block() {
            assert_eq!(normalize_think_answer("<think>a</think>b<think>c</think>d"), "b<think>c</think>d");
        }

        #[test]
        fn unclosed_or_absent_block_returns_trimmed_text() {
            assert_eq!(normalize_think_answer("  <think>never closed  "), "<think>never closed");
            assert_eq!(normalize_think_answer(" plain answer "), "plain answer");
            assert_eq!(normalize_think_answer(""), "");
        }

        #[test]
        fn handles_multibyte_text_around_the_block() {
            assert_eq!(normalize_think_answer("<think>думаю…</think> 안녕 — привет"), "안녕 — привет");
        }
    }

    /// End-to-end checks of `exec_chat_tracked` against a loopback server speaking the
    /// OpenAI-compatible streaming protocol (no external network, no files).
    #[cfg(test)]
    mod stream_tests {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        use genai::chat::{ChatMessage, ChatOptions, ChatRequest};

        use super::exec_chat_tracked;
        use crate::client::{block_on, build_client};
        use crate::error::AiApiError;
        use crate::generation::{GenerationPhase, GenerationTracker};
        use crate::model_id::model_iden;
        use crate::service::AiApiService;
        use crate::target::AiApiTarget;

        const WAIT: Duration = Duration::from_secs(10);

        fn delta_event(field: &str, text: &str) -> String {
            let delta = serde_json::json!({ "choices": [{ "index": 0, "delta": { field: text }, "finish_reason": null }] });
            format!("data: {delta}\n\n")
        }

        fn finish_events() -> Vec<String> {
            let finish = serde_json::json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] });
            vec![format!("data: {finish}\n\n"), "data: [DONE]\n\n".to_string()]
        }

        /// Reads one HTTP request (headers + `Content-Length` body) and returns its body, so the
        /// client is never answered before it finished sending.
        fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    return Ok(String::new());
                }
                buffer.extend_from_slice(&chunk[..read]);
                let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
                let content_length = headers.lines().find_map(|line| line.strip_prefix("content-length:")).and_then(|value| value.trim().parse::<usize>().ok()).unwrap_or(0);
                if buffer.len() >= header_end + 4 + content_length {
                    return Ok(String::from_utf8_lossy(&buffer[header_end + 4..]).into_owned());
                }
            }
        }

        /// A 200 server-sent-events response whose body is `events`, closed after them.
        fn sse_response(events: &[String]) -> String {
            let mut response = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n");
            for event in events {
                response.push_str(event);
            }
            response
        }

        /// A JSON response with `status_line` (e.g. `"400 Bad Request"`).
        fn json_response(status_line: &str, body: &str) -> String {
            format!("HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        }

        /// A loopback server answering one connection per entry of `responses`, in order.
        struct TestServer {
            base_url: String,
            /// Signalled when the client closed the stalled last connection.
            closed: mpsc::Receiver<()>,
            /// The request bodies, in order.
            requests: mpsc::Receiver<String>,
        }

        /// Starts a `TestServer`. With `stall`, the last connection is held open after its
        /// response until the client closes it.
        fn serve(responses: Vec<String>, stall: bool) -> TestServer {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port for the test server");
            let port = listener.local_addr().expect("loopback listener has an address").port();
            let (closed_tx, closed) = mpsc::channel();
            let (requests_tx, requests) = mpsc::channel();
            std::thread::spawn(move || {
                let last = responses.len().saturating_sub(1);
                for (index, response) in responses.iter().enumerate() {
                    let Ok((mut stream, _)) = listener.accept() else { return };
                    let Ok(body) = read_request(&mut stream) else { return };
                    // An `Err` only means the test does not inspect the requests.
                    requests_tx.send(body).ok();
                    if stream.write_all(response.as_bytes()).and_then(|()| stream.flush()).is_err() {
                        return;
                    }
                    if stall && index == last {
                        // Blocks until the client drops the connection (read returns 0), bounded
                        // by `WAIT` so a broken test cannot leave the thread hanging.
                        if stream.set_read_timeout(Some(WAIT)).is_err() {
                            return;
                        }
                        let mut probe = [0_u8; 64];
                        if matches!(stream.read(&mut probe), Ok(0)) {
                            // An `Err` only means the test stopped waiting for the close.
                            closed_tx.send(()).ok();
                        }
                    }
                }
            });
            TestServer { base_url: format!("http://127.0.0.1:{port}"), closed, requests }
        }

        fn run(base_url: &str, options: Option<&ChatOptions>, tracker: &GenerationTracker) -> Result<String, AiApiError> {
            let target = AiApiTarget::new(AiApiService::OpenAiCompatible, base_url).expect("loopback base URL is valid");
            let client = build_client(&target, String::new());
            let model = model_iden(AiApiService::OpenAiCompatible, "test-model").expect("non-empty model id");
            let request = ChatRequest::default().append_message(ChatMessage::user("hi"));
            block_on(exec_chat_tracked(&client, model, request, options, tracker, 7)).expect("tokio runtime builds")
        }

        #[test]
        fn streams_answer_and_counts_reasoning_without_stripping() {
            let mut events = vec![delta_event("reasoning_content", "abc"), delta_event("content", "<think>x</think>\nHello"), delta_event("content", " мир")];
            events.extend(finish_events());
            let server = serve(vec![sse_response(&events)], false);
            let tracker = GenerationTracker::new();
            let options = ChatOptions::default().with_normalize_reasoning_content(true);
            let text = run(&server.base_url, Some(&options), &tracker).expect("stream completes");
            // Real reasoning chunks arrived, so genai would not have normalized either.
            assert_eq!(text, "<think>x</think>\nHello мир");
            let snapshot = tracker.snapshot();
            assert!(!snapshot.active);
            assert_eq!(snapshot.phase, GenerationPhase::Answering);
            assert_eq!(snapshot.reasoning_chars, 3);
            assert_eq!(snapshot.answer_chars, "<think>x</think>\nHello мир".chars().count());
            assert_eq!(snapshot.request_tag, 7);
        }

        #[test]
        fn normalizes_inline_think_like_the_non_streaming_path() {
            let mut events = vec![delta_event("content", "<think>plan</think>\n"), delta_event("content", "[1]")];
            events.extend(finish_events());
            let server = serve(vec![sse_response(&events)], false);
            let options = ChatOptions::default().with_normalize_reasoning_content(true);
            assert_eq!(run(&server.base_url, Some(&options), &GenerationTracker::new()).expect("stream completes"), "[1]");
            let server = serve(vec![sse_response(&events)], false);
            assert_eq!(run(&server.base_url, None, &GenerationTracker::new()).expect("stream completes"), "<think>plan</think>\n[1]");
        }

        #[test]
        fn a_body_ending_without_the_terminal_event_is_an_error() {
            let server = serve(vec![sse_response(&[delta_event("content", "partial answ")])], false);
            let tracker = GenerationTracker::new();
            let result = run(&server.base_url, None, &tracker);
            assert!(matches!(result, Err(AiApiError::ChatRequest { .. })), "got {result:?}");
            assert_eq!(tracker.snapshot().answer_chars, "partial answ".chars().count());
        }

        #[test]
        fn a_refused_stream_is_retried_once_without_streaming() {
            let refusal = r#"{"error":{"message":"Your organization must be verified to stream this model. Please go to: https://platform.openai.com/settings/organization/general and click on Verify Organization.","type":"invalid_request_error","param":"stream","code":"unsupported_value"}}"#;
            let reply = r#"{"id":"x","object":"chat.completion","created":0,"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"<think>t</think> [1]"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            let server = serve(vec![json_response("400 Bad Request", refusal), json_response("200 OK", reply)], false);
            let tracker = GenerationTracker::new();
            let options = ChatOptions::default().with_normalize_reasoning_content(true);
            // genai's own non-streaming normalization applies to the retried reply.
            assert_eq!(run(&server.base_url, Some(&options), &tracker).expect("retry succeeds"), "[1]");
            let first = server.requests.recv_timeout(WAIT).expect("streaming request received");
            let second = server.requests.recv_timeout(WAIT).expect("retry received");
            assert!(first.contains(r#""stream":true"#), "first request streams: {first}");
            assert!(!second.contains(r#""stream":true"#), "retry does not stream: {second}");
            let snapshot = tracker.snapshot();
            assert_eq!((snapshot.phase, snapshot.answer_chars), (GenerationPhase::Thinking, 0));
        }

        #[test]
        fn other_stream_errors_are_not_retried() {
            let server = serve(vec![json_response("429 Too Many Requests", r#"{"error":{"message":"Rate limit reached"}}"#)], false);
            let result = run(&server.base_url, None, &GenerationTracker::new());
            assert!(matches!(&result, Err(AiApiError::ChatRequest { detail }) if detail.contains("429")), "got {result:?}");
            assert!(server.requests.recv_timeout(WAIT).is_ok());
            assert!(server.requests.recv_timeout(Duration::from_millis(200)).is_err(), "no second request");
        }

        #[test]
        fn stop_aborts_the_request_and_closes_the_connection() {
            let server = serve(vec![sse_response(&[delta_event("content", "partial")])], true);
            let tracker = GenerationTracker::new();
            let worker_tracker = tracker.clone();
            let base_url = server.base_url.clone();
            let worker = std::thread::spawn(move || run(&base_url, None, &worker_tracker));
            let deadline = Instant::now() + WAIT;
            while tracker.snapshot().answer_chars == 0 {
                assert!(Instant::now() < deadline, "the first chunk never arrived");
                std::thread::sleep(Duration::from_millis(10));
            }
            let shown = tracker.snapshot();
            assert!(shown.active);
            tracker.cancel_run(shown.run_id);
            let result = worker.join().expect("worker thread does not panic");
            assert!(matches!(result, Err(AiApiError::Cancelled)), "got {result:?}");
            assert!(!tracker.snapshot().active);
            assert!(server.closed.recv_timeout(WAIT).is_ok(), "the HTTP connection was not closed");
        }

        #[test]
        fn a_closed_tracker_never_sends_the_request() {
            let tracker = GenerationTracker::new();
            tracker.close();
            let result = run("http://127.0.0.1:9", None, &tracker);
            assert!(matches!(result, Err(AiApiError::Cancelled)), "got {result:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use super::{GenerationPhase, GenerationSnapshot, GenerationTracker};

    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<CountingWaker>, Waker) {
        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        (counter, waker)
    }

    #[test]
    fn idle_tracker_reports_an_inactive_thinking_snapshot() {
        let tracker = GenerationTracker::new();
        assert_eq!(tracker.snapshot(), GenerationSnapshot::IDLE);
    }

    #[test]
    fn phase_follows_the_last_non_empty_chunk() {
        let tracker = GenerationTracker::new();
        let run = tracker.begin(0);
        assert_eq!(tracker.snapshot().phase, GenerationPhase::Thinking);
        assert!(tracker.snapshot().active);
        run.record_reasoning("plan");
        assert_eq!(tracker.snapshot().phase, GenerationPhase::Thinking);
        run.record_answer("");
        assert_eq!(tracker.snapshot().phase, GenerationPhase::Thinking, "an empty answer chunk changes nothing");
        run.record_answer("Hi");
        assert_eq!(tracker.snapshot().phase, GenerationPhase::Answering);
        run.record_reasoning("again");
        assert_eq!(tracker.snapshot().phase, GenerationPhase::Thinking);
    }

    #[test]
    fn counts_chars_not_bytes() {
        let tracker = GenerationTracker::new();
        let run = tracker.begin(0);
        run.record_reasoning("думаю");
        run.record_answer("안녕");
        run.record_answer("🙂a");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.reasoning_chars, 5);
        assert_eq!(snapshot.answer_chars, 4);
        assert_eq!(snapshot.total_chars(), 9);
    }

    #[test]
    fn dropping_the_run_ends_the_generation_and_keeps_counts() {
        let tracker = GenerationTracker::new();
        let run = tracker.begin(0);
        run.record_answer("abc");
        drop(run);
        let snapshot = tracker.snapshot();
        assert!(!snapshot.active);
        assert_eq!(snapshot.answer_chars, 3);
    }

    #[test]
    fn a_new_begin_resets_counters_phase_and_cancel() {
        let tracker = GenerationTracker::new();
        let first = tracker.begin(0);
        first.record_answer("abc");
        tracker.cancel_run(tracker.snapshot().run_id);
        assert!(first.is_cancelled());
        drop(first);
        let second = tracker.begin(5);
        assert!(!second.is_cancelled());
        assert_eq!(tracker.snapshot(), GenerationSnapshot { active: true, phase: GenerationPhase::Thinking, reasoning_chars: 0, answer_chars: 0, run_id: 2, request_tag: 5 });
    }

    #[test]
    fn cancel_without_an_active_run_is_a_no_op() {
        let tracker = GenerationTracker::new();
        tracker.cancel_run(0);
        tracker.cancel_run(1);
        let run = tracker.begin(0);
        assert!(!run.is_cancelled());
    }

    #[test]
    fn cancel_run_ignores_a_finished_or_other_run() {
        let tracker = GenerationTracker::new();
        let first = tracker.begin(0);
        let shown = tracker.snapshot().run_id;
        drop(first);
        let second = tracker.begin(0);
        tracker.cancel_run(shown);
        assert!(!second.is_cancelled(), "a click on the finished run must not stop the next one");
        tracker.cancel_run(tracker.snapshot().run_id);
        assert!(second.is_cancelled());
    }

    #[test]
    fn close_cancels_the_active_and_every_later_run() {
        let tracker = GenerationTracker::new();
        let run = tracker.begin(0);
        assert!(!tracker.is_closed());
        tracker.close();
        assert!(tracker.is_closed());
        assert!(run.is_cancelled());
        drop(run);
        assert!(tracker.begin(0).is_cancelled());
    }

    #[test]
    fn a_superseded_run_reads_cancelled_and_records_nothing() {
        let tracker = GenerationTracker::new();
        let old = tracker.begin(0);
        let new = tracker.begin(0);
        assert!(old.is_cancelled());
        assert!(!new.is_cancelled());
        old.record_answer("stale");
        assert_eq!(tracker.snapshot().answer_chars, 0);
        drop(old);
        assert!(tracker.snapshot().active, "dropping a stale run must not end the current one");
    }

    #[test]
    fn cancel_wakes_the_pending_cancellation_future() {
        let tracker = GenerationTracker::new();
        let run = tracker.begin(0);
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(run.cancelled());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        tracker.cancel_run(tracker.snapshot().run_id);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn a_cancel_before_the_first_poll_completes_at_once() {
        let tracker = GenerationTracker::new();
        let run = tracker.begin(0);
        tracker.cancel_run(tracker.snapshot().run_id);
        let (_counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(run.cancelled());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn superseding_a_run_wakes_its_cancellation_future() {
        let tracker = GenerationTracker::new();
        let old = tracker.begin(0);
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(old.cancelled());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        // Held so the newer run stays current while the old future is polled.
        let _new = tracker.begin(0);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn tracker_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GenerationTracker>();
    }
}
