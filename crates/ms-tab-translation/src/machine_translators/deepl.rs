/*
FILE OVERVIEW: crates/ms-tab-translation/src/machine_translators/deepl.rs
DeepL backend over DeepL's web JSON-RPC endpoint (flow adapted from translatepy).

Main items:
- `DeeplMtBackend`: stateless backend for callers without a run (the shared dispatcher): a
  fresh session per call.
- `run_session` -> `DeeplMtSession`: the backend of one Translation run.
- `DeeplClient`: the JSON-RPC client over the injectable `DeeplHttp` seam (`UreqDeeplHttp` in
  production, a scripted fake in tests) with its own JSON-RPC id sequence.
- `shared_agent`: the one process-wide `ureq::Agent` every production session uses.
- `pack_batches`, `parse_split_batch`, `build_jobs`: how several bubbles share one request pair.

Behavior notes:
- One batch of bubbles costs TWO requests: `LMT_split_into_sentences` with every text of the
  batch, then one `LMT_handle_jobs` with the jobs of every sentence; translations are mapped back
  to their bubble by the per-text sentence counts (a bubble's context never crosses into its
  neighbours, so each bubble's text boundaries stay exact). `DEEPL_BATCH_MAX_TEXTS` /
  `DEEPL_BATCH_MAX_CHARS` bound a batch. No other request is sent (no `getClientState`: the
  endpoint answers without it, and it would only spend the request budget).
- JSON-RPC ids: each client starts at a random `NNNN0001` baseline (`initial_request_id`) and
  increments per request.
- Pacing is NOT decided here: every request takes a permit from the caller's `MtCall`
  (`crate::mt_limits`, route `MtRoute::Deepl`: a request budget of 6 tokens, one more per
  minute) and finishes it with its `MtSignal`. HTTP 429 and every DeepL code `10429xx` are
  `RateLimited` (with the server's `Retry-After`). A rate-limited request is never re-sent
  (`rate_limit_retries` 0: requests sent while limited escalate DeepL's block); the batch ends
  with `CoolingDown` carrying the cooldown the limiter started, so the run stops with "try again
  in N min". No sleep and no throttle live in this file.
- `shared_agent` is a read-only global initialised once (global §5): its only state is the
  agent's own connection pool and cookie jar, so runs and the Cleaning prompt translator reuse
  one warm TLS connection instead of paying a handshake per run.
- Transport timeouts come from `machine_translators::http` (`build_agent`).
- The native HTTP stack is not compiled for the web build, where every call fails with a clear
  "unavailable on web" error instead of a fake translation.
*/

use super::{MachineTranslatorBackend, MtItemError};
use crate::mt_limits::call::MtCall;

#[derive(Debug, Default, Clone, Copy)]
pub struct DeeplMtBackend;

/// The backend of one Translation run (`super::run_session`): a `DeeplMtSession` on native
/// builds, so the batching spans every bubble of the run (the HTTP agent is process-wide,
/// `shared_agent`).
pub(crate) fn run_session() -> Box<dyn MachineTranslatorBackend> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Box::new(native::DeeplMtSession::new())
    }
    #[cfg(target_arch = "wasm32")]
    {
        Box::new(DeeplMtBackend)
    }
}

/// Web stub: DeepL translation uses native HTTP (`ureq`) JSON-RPC calls that the
/// browser build cannot make, so every batch fails with a clear message instead
/// of returning a fake translation.
#[cfg(target_arch = "wasm32")]
impl MachineTranslatorBackend for DeeplMtBackend {
    fn translate_texts(
        &self,
        _call: &MtCall<'_>,
        _source_lang: &str,
        _target_lang: &str,
        _texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        Err(t!("translation.mt.deepl.web_unavailable_error").to_string())
    }
}

// The language-code spelling has one owner per service; the browser provider (`browser.rs`)
// reuses it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::normalize_deepl_lang;

// The native DeepL implementation depends on `ureq` (native HTTP), which is not
// compiled for wasm. The whole native surface lives in this module so the cfg
// boundary is a single point; on wasm only the stub impl above is compiled.
#[cfg(not(target_arch = "wasm32"))]
mod native {
use super::{DeeplMtBackend, MachineTranslatorBackend, MtCall, MtItemError};
use crate::machine_translation::MtService;
use crate::machine_translators::MtItemErrorKind;
use crate::machine_translators::http;
use crate::mt_limits::call::{MtAcquireError, MtSignal};
use crate::mt_limits::table::{MtLane, MtRoute, lane_limits};
use ms_log::runtime_log;
use std::cell::Cell;
use std::ops::{Range, RangeInclusive};
use std::sync::OnceLock;
use std::time::Duration;
use web_time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const DEEPL_JSONRPC_URL: &str = "https://www2.deepl.com/jsonrpc";
const DEEPL_PREFERRED_LANGS: [&str; 2] = ["EN", "RU"];
/// The browser UA DeepL's web client is expected to send.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36";
/// Most bubbles packed into one request pair (split + handle_jobs). Ten typical bubbles are
/// 20-40 sentences, well inside what the web client itself sends in one `LMT_handle_jobs`.
const DEEPL_BATCH_MAX_TEXTS: usize = 10;
/// Most characters (`char`s, summed over the packed texts) of one batch. DeepL's free web
/// translator accepts 1500 characters per input; 1000 leaves a margin. A single longer text
/// still goes alone (it is never cut here).
const DEEPL_BATCH_MAX_CHARS: usize = 1000;
/// Longest piece of a DeepL error body quoted in a user-facing message (`char`s).
const ERROR_BODY_QUOTE_CHARS: usize = 300;
/// DeepL's JSON-RPC "too many requests" codes: 1042901 / 1042902 (a short block) and
/// 1042911 / 1042912 (the escalated one) were observed; the whole `10429xx` family counts as a
/// rate limit, whatever the HTTP status.
const DEEPL_RATE_LIMIT_CODES: RangeInclusive<i64> = 1_042_900..=1_042_999;

/// One HTTP answer, whatever its status.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeeplReply {
    status: u16,
    /// The server's delta-seconds `Retry-After`, when it sent one (`http::retry_after`).
    retry_after: Option<Duration>,
    body: String,
}

/// A request that produced no HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeeplTransportError {
    /// Connect, DNS, TLS or timeout failure (the transport's own description).
    Network(String),
    /// A 2xx answer whose body could not be read.
    ReadBody(String),
}

impl DeeplTransportError {
    /// The localized message shown for the affected bubbles.
    fn user_message(&self) -> String {
        match self {
            DeeplTransportError::Network(err) => tf!("translation.mt.deepl.network_error", err = err),
            DeeplTransportError::ReadBody(err) => tf!("translation.mt.deepl.read_body_error", err = err),
        }
    }
}

/// The HTTP seam: one JSON POST. Any HTTP status is an `Ok` reply; `Err` means no answer
/// arrived.
trait DeeplHttp {
    fn post_json(&self, url: &str, body: &str) -> Result<DeeplReply, DeeplTransportError>;
}

impl<T: DeeplHttp + ?Sized> DeeplHttp for &T {
    fn post_json(&self, url: &str, body: &str) -> Result<DeeplReply, DeeplTransportError> {
        (**self).post_json(url, body)
    }
}

/// The one DeepL agent of the process (`shared_agent`).
static DEEPL_AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// The process-wide DeepL agent (shared MT timeouts, redirects followed), built on first use.
/// A `ureq::Agent` clone shares the original's connection pool and cookie jar, so every session
/// reuses a warm TLS connection. The static is read-only once initialised (global §5); the
/// agent's internal pool and cookie jar are its only state.
fn shared_agent() -> ureq::Agent {
    DEEPL_AGENT.get_or_init(|| http::build_agent(USER_AGENT, true)).clone()
}

/// Production seam: the process-wide agent (`shared_agent`).
#[derive(Debug)]
struct UreqDeeplHttp {
    agent: ureq::Agent,
}

impl UreqDeeplHttp {
    fn new() -> Self {
        Self { agent: shared_agent() }
    }
}

impl DeeplHttp for UreqDeeplHttp {
    fn post_json(&self, url: &str, body: &str) -> Result<DeeplReply, DeeplTransportError> {
        let request = self
            .agent
            .post(url)
            .set("Accept", "*/*")
            .set("Accept-Language", "en-US,en-GB;q=0.5")
            .set("Accept-Encoding", "gzip, deflate")
            .set("Connection", "keep-alive")
            .set("Referer", "https://www.deepl.com/")
            .set("Origin", "https://www.deepl.com")
            .set("Content-Type", "application/json");
        match request.send_string(body) {
            Ok(response) => {
                let status = response.status();
                let retry_after = http::retry_after(&response);
                let body = response.into_string().map_err(|err| DeeplTransportError::ReadBody(err.to_string()))?;
                Ok(DeeplReply { status, retry_after, body })
            }
            // ureq reports 4xx / 5xx as an error that still carries the response.
            Err(ureq::Error::Status(status, response)) => {
                let retry_after = http::retry_after(&response);
                // The status alone classifies the answer; an unreadable error body only loses
                // the detail quoted in the message, so it is logged and treated as empty.
                let body = response.into_string().unwrap_or_else(|err| {
                    runtime_log::log_warn(format!("[MT][DeepL] reading the HTTP {status} error body from {url} failed: {err}"));
                    String::new()
                });
                Ok(DeeplReply { status, retry_after, body })
            }
            Err(ureq::Error::Transport(err)) => Err(DeeplTransportError::Network(err.to_string())),
        }
    }
}

/// Why a DeepL request (or a whole batch) failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeeplError {
    /// No limiter permit (cancelled, cooling down for longer than the call may wait, busy), or
    /// DeepL rate-limited the request and no re-send is allowed (`CoolingDown` with the
    /// cooldown that answer started).
    Acquire(MtAcquireError),
    /// Any other failure, with its localized message.
    Failed(String),
}

impl DeeplError {
    /// The per-text error the run loop sees (keeps `Cancelled` / `CoolingDown` kinds).
    fn into_item_error(self) -> MtItemError {
        match self {
            DeeplError::Acquire(err) => MtItemError::from_acquire(err, MtService::Deepl),
            DeeplError::Failed(message) => MtItemError::failed(message),
        }
    }
}

/// One MT run's DeepL backend: a JSON-RPC client over the process-wide agent.
/// Single-threaded (`Cell`): it lives on the run's worker thread.
#[derive(Debug)]
pub(crate) struct DeeplMtSession {
    client: DeeplClient<UreqDeeplHttp>,
}

impl DeeplMtSession {
    /// A session over the shared agent (building it does no I/O).
    pub(crate) fn new() -> Self {
        Self { client: DeeplClient::new(UreqDeeplHttp::new()) }
    }
}

impl MachineTranslatorBackend for DeeplMtSession {
    fn translate_texts(
        &self,
        call: &MtCall<'_>,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        Ok(self.client.translate_texts(call, source_lang, target_lang, &texts))
    }

    fn preferred_batch_size(&self) -> usize {
        DEEPL_BATCH_MAX_TEXTS
    }
}

impl MachineTranslatorBackend for DeeplMtBackend {
    /// Stateless path for callers without a run (the shared dispatcher): a fresh session per
    /// call, over the process-wide agent.
    fn translate_texts(
        &self,
        call: &MtCall<'_>,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, MtItemError>>, String> {
        DeeplMtSession::new().translate_texts(call, source_lang, target_lang, texts)
    }
}

/// The JSON-RPC client over an HTTP seam.
#[derive(Debug)]
struct DeeplClient<H> {
    http: H,
    /// The last JSON-RPC id this client sent (its random baseline before the first request).
    last_id: Cell<i64>,
}

impl<H: DeeplHttp> DeeplClient<H> {
    fn new(http: H) -> Self {
        Self { http, last_id: Cell::new(initial_request_id()) }
    }

    /// The id of the next request.
    fn next_id(&self) -> i64 {
        let id = self.last_id.get().saturating_add(1);
        self.last_id.set(id);
        id
    }

    /// Translates `texts` in input order, packing them into batches (`pack_batches`). Blank
    /// texts answer `""` without a request. A batch-wide failure becomes every text's error;
    /// after a `Cancelled` / `CoolingDown` failure nothing more is sent and every later text
    /// gets that same error.
    fn translate_texts(&self, call: &MtCall<'_>, source_lang: &str, target_lang: &str, texts: &[String]) -> Vec<Result<String, MtItemError>> {
        let source = normalize_deepl_lang(source_lang, true);
        let target = normalize_deepl_lang(target_lang, false);
        let trimmed: Vec<&str> = texts.iter().map(|text| text.trim()).collect();
        // Every slot starts as the blank answer; non-blank slots are all overwritten below
        // because the batches cover every index of `pending`.
        let mut out: Vec<Result<String, MtItemError>> = vec![Ok(String::new()); texts.len()];
        let pending: Vec<usize> = (0..trimmed.len()).filter(|&index| !trimmed[index].is_empty()).collect();
        let lengths: Vec<usize> = pending.iter().map(|&index| trimmed[index].chars().count()).collect();
        let mut stop: Option<MtItemError> = None;
        for batch in pack_batches(&lengths, DEEPL_BATCH_MAX_TEXTS, DEEPL_BATCH_MAX_CHARS) {
            let indices = &pending[batch];
            if stop.is_none() && call.is_cancelled() {
                stop = Some(MtItemError::from_acquire(MtAcquireError::Cancelled, MtService::Deepl));
            }
            let results = if let Some(error) = &stop {
                vec![Err(error.clone()); indices.len()]
            } else {
                let batch_texts: Vec<&str> = indices.iter().map(|&index| trimmed[index]).collect();
                match self.translate_batch(call, &batch_texts, &source, &target) {
                    Ok(results) => results,
                    Err(error) => {
                        let item = error.into_item_error();
                        match item.kind {
                            MtItemErrorKind::Failed => {}
                            MtItemErrorKind::Cancelled | MtItemErrorKind::CoolingDown { .. } | MtItemErrorKind::ServiceUnavailable => stop = Some(item.clone()),
                        }
                        vec![Err(item); indices.len()]
                    }
                }
            };
            for (&index, result) in indices.iter().zip(results) {
                out[index] = result;
            }
        }
        out
    }

    /// Translates one batch with two requests (split, handle_jobs). Returns one result per
    /// text; a sentence DeepL left untranslated fails only its own text.
    ///
    /// # Errors
    /// A failed request, or a `translations` array that does not match the jobs sent (the
    /// answer can then not be mapped back to the texts).
    fn translate_batch(&self, call: &MtCall<'_>, texts: &[&str], source_lang: &str, target_lang: &str) -> Result<Vec<Result<String, MtItemError>>, DeeplError> {
        let (sentences, computed_lang) = self.split_into_sentences(call, texts, source_lang, target_lang)?;
        let jobs: Vec<Value> = sentences.iter().flat_map(|text_sentences| build_jobs(text_sentences)).collect();
        let timestamp = build_timestamp(sentences.iter().flatten());

        let mut user_preferred_langs = vec![target_lang.to_string()];
        let mut lang = json!({
            "target_lang": target_lang,
            "user_preferred_langs": user_preferred_langs.clone(),
        });
        if source_lang == "AUTO" {
            if let Some(computed) = computed_lang {
                let normalized = normalize_deepl_lang(&computed, false);
                if !user_preferred_langs.iter().any(|it| it == &normalized) {
                    user_preferred_langs.push(normalized.clone());
                    lang["user_preferred_langs"] = json!(user_preferred_langs);
                }
                lang["source_lang_computed"] = Value::String(normalized);
            } else {
                lang["source_lang_user_selected"] = Value::String("AUTO".to_string());
            }
        } else {
            lang["source_lang_user_selected"] = Value::String(source_lang.to_string());
        }

        let params = json!({
            "jobs": jobs,
            "lang": lang,
            "priority": 1,
            "timestamp": timestamp,
        });
        let result = self.send_jsonrpc(call, "LMT_handle_jobs", &params)?;
        let translations = result
            .get("translations")
            .and_then(Value::as_array)
            .ok_or_else(|| DeeplError::Failed(t!("translation.mt.deepl.no_translations_error").to_string()))?;
        let job_count: usize = sentences.iter().map(Vec::len).sum();
        if translations.len() != job_count {
            return Err(DeeplError::Failed(tf!("translation.mt.deepl.translations_size_error", sentences = job_count, translations = translations.len())));
        }
        // Jobs were sent text by text, so each text owns the next `len` translations.
        let mut answers = translations.iter();
        Ok(sentences.iter().map(|text_sentences| merge_translations(answers.by_ref().take(text_sentences.len()))).collect())
    }

    /// Splits every text of the batch into sentences with one request. Returns one non-empty
    /// sentence list per text and DeepL's detected source language.
    fn split_into_sentences(&self, call: &MtCall<'_>, texts: &[&str], source_lang: &str, target_lang: &str) -> Result<(Vec<Vec<String>>, Option<String>), DeeplError> {
        let split_source = if source_lang == "AUTO" { "auto" } else { source_lang };
        let mut preferred = DEEPL_PREFERRED_LANGS.into_iter().map(ToString::to_string).collect::<Vec<_>>();
        if !preferred.iter().any(|it| it == target_lang) {
            preferred.push(target_lang.to_string());
        }
        let params = json!({
            "texts": texts,
            "lang": {
                "lang_user_selected": split_source,
                "user_preferred_langs": preferred,
            }
        });
        let result = self.send_jsonrpc(call, "LMT_split_into_sentences", &params)?;
        let computed_lang = result
            .get("lang")
            .and_then(Value::as_str)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Ok((parse_split_batch(&result, texts), computed_lang))
    }

    /// Sends one JSON-RPC request. After a rate-limit answer it is re-sent only while the
    /// table's `rate_limit_retries` allows (0 for DeepL); once that is spent the error is
    /// `CoolingDown` with the cooldown the limiter started, so the run stops instead of failing
    /// text by text.
    ///
    /// # Errors
    /// No permit or the rate limit (`DeeplError::Acquire`), a transport failure, an error
    /// answer, or an answer without `result`.
    fn send_jsonrpc(&self, call: &MtCall<'_>, method: &str, params: &Value) -> Result<Value, DeeplError> {
        let retries = lane_limits(MtLane::builtin(MtService::Deepl)).rate_limit_retries;
        for attempt in 0..=retries {
            let permit = call.acquire(MtRoute::Deepl).map_err(DeeplError::Acquire)?;
            let id = self.next_id();
            let payload = json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
                "id": id,
            });
            // A failure before the POST drops the permit unfinished, which counts as Neutral.
            let payload_text = serialize_json_like_requests(&payload)
                .map_err(|err| DeeplError::Failed(tf!("translation.mt.deepl.serialize_error", method = method, err = err)))?;
            let reply = match self.http.post_json(DEEPL_JSONRPC_URL, &payload_text) {
                Ok(reply) => reply,
                Err(err) => {
                    permit.finish(MtSignal::Neutral);
                    runtime_log::log_warn(format!("[MT][DeepL] {method}: no answer: {err:?}"));
                    return Err(DeeplError::Failed(err.user_message()));
                }
            };
            // Classified before anything else: a rate-limit answer need not be JSON.
            let parsed = serde_json::from_str::<Value>(&reply.body);
            let code = parsed.as_ref().ok().and_then(jsonrpc_error_code);
            if is_deepl_rate_limited(reply.status, code) {
                permit.finish(MtSignal::RateLimited { retry_after: reply.retry_after });
                runtime_log::log_warn(format!(
                    "[MT][DeepL] {method}: rate limited (HTTP {}, code {}, Retry-After {:?}), attempt {}/{}",
                    reply.status,
                    code.unwrap_or(0),
                    reply.retry_after,
                    attempt + 1,
                    retries + 1
                ));
                continue;
            }
            if reply.status != 200 {
                permit.finish(MtSignal::Neutral);
                return Err(DeeplError::Failed(api_error_message(method, code, &reply)));
            }
            permit.finish(MtSignal::Ok);
            let value = parsed.map_err(|err| DeeplError::Failed(tf!("translation.mt.deepl.invalid_json_error", method = method, err = err)))?;
            if let Some(result) = value.get("result") {
                return Ok(result.clone());
            }
            if code.is_some() {
                return Err(DeeplError::Failed(api_error_message(method, code, &reply)));
            }
            return Err(DeeplError::Failed(tf!("translation.mt.deepl.missing_result_error", method = method)));
        }
        // Out of re-sends: the last answer cooled the route; report that cooldown.
        match call.lane_cooldown(MtLane::builtin(MtService::Deepl)) {
            Some(retry_in) => Err(DeeplError::Acquire(MtAcquireError::CoolingDown { retry_in })),
            None => Err(DeeplError::Failed(tf!("translation.mt.deepl.retries_exceeded_error", method = method))),
        }
    }
}

/// Groups consecutive texts (given by their `char` lengths) into batches of at most
/// `max_texts` texts and `max_chars` characters. A text longer than `max_chars` forms a batch
/// of its own. Returns index ranges into `lengths`, covering it in order.
fn pack_batches(lengths: &[usize], max_texts: usize, max_chars: usize) -> Vec<Range<usize>> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut chars = 0_usize;
    for (index, &length) in lengths.iter().enumerate() {
        let count = index - start;
        if count > 0 && (count >= max_texts || chars.saturating_add(length) > max_chars) {
            batches.push(start..index);
            start = index;
            chars = 0;
        }
        chars = chars.saturating_add(length);
    }
    if start < lengths.len() {
        batches.push(start..lengths.len());
    }
    batches
}

/// One sentence list per text from a `LMT_split_into_sentences` result for `texts`. A text
/// DeepL returned no sentences for is sent as one sentence; an answer whose shape does not match
/// the texts is logged and every text is sent as one sentence (DeepL translates a
/// multi-sentence job too, only with less context).
fn parse_split_batch(result: &Value, texts: &[&str]) -> Vec<Vec<String>> {
    let whole_texts = || texts.iter().map(|text| vec![(*text).to_string()]).collect::<Vec<_>>();
    let Some(splitted) = result.get("splitted_texts").and_then(Value::as_array) else {
        runtime_log::log_warn(format!("[MT][DeepL] LMT_split_into_sentences: no splitted_texts for {} texts; sending each text whole", texts.len()));
        return whole_texts();
    };
    let per_text: Option<Vec<Vec<String>>> = if splitted.iter().all(Value::is_array) {
        Some(splitted.iter().map(|item| clean_sentences(item.as_array().map_or(&[][..], Vec::as_slice))).collect())
    } else if texts.len() == 1 {
        // Older answers list the sentences of a single text without the per-text nesting.
        Some(vec![clean_sentences(splitted)])
    } else {
        None
    };
    match per_text {
        Some(lists) if lists.len() == texts.len() => lists
            .into_iter()
            .zip(texts)
            .map(|(sentences, text)| if sentences.is_empty() { vec![(*text).to_string()] } else { sentences })
            .collect(),
        _ => {
            runtime_log::log_warn(format!("[MT][DeepL] LMT_split_into_sentences: {} entries for {} texts; sending each text whole", splitted.len(), texts.len()));
            whole_texts()
        }
    }
}

/// The non-blank string items of a sentence list, trimmed.
fn clean_sentences(items: &[Value]) -> Vec<String> {
    items.iter().filter_map(Value::as_str).map(str::trim).filter(|value| !value.is_empty()).map(ToString::to_string).collect()
}

/// One text's translation from its own `translations` entries, sentences joined by a space.
/// A sentence without a beam fails the text instead of silently dropping the sentence.
fn merge_translations<'v>(entries: impl Iterator<Item = &'v Value>) -> Result<String, MtItemError> {
    let mut parts = Vec::new();
    for (index, entry) in entries.enumerate() {
        let sentence = entry
            .get("beams")
            .and_then(Value::as_array)
            .and_then(|beams| beams.first())
            .and_then(|beam| beam.get("postprocessed_sentence"))
            .and_then(Value::as_str)
            .ok_or_else(|| MtItemError::failed(tf!("translation.mt.deepl.missing_sentence_error", sentence = index + 1)))?;
        parts.push(sentence);
    }
    Ok(parts.join(" "))
}

/// The jobs of one text's sentences, each with up to five preceding and one following sentence
/// of the SAME text as context.
fn build_jobs(sentences: &[String]) -> Vec<Value> {
    let mut jobs = Vec::with_capacity(sentences.len());
    let mut before: Vec<String> = Vec::new();

    for (index, sentence) in sentences.iter().enumerate() {
        let after: Vec<String> = sentences.get(index + 1).cloned().into_iter().collect();
        if let Some(previous) = index.checked_sub(1).and_then(|previous| sentences.get(previous)) {
            if before.len() >= 5 {
                before.remove(0);
            }
            before.push(previous.clone());
        }
        jobs.push(json!({
            "kind": "default",
            "raw_en_context_after": after,
            "raw_en_context_before": before.clone(),
            "raw_en_sentence": sentence,
        }));
    }
    jobs
}

/// The `timestamp` the web client sends: the current time rounded to 100 ms plus one second,
/// moved up to a multiple of (1 + the number of `i` letters in the sentences).
fn build_timestamp<'s>(sentences: impl Iterator<Item = &'s String>) -> i64 {
    let letters: usize = sentences.map(|sentence| sentence.matches('i').count()).sum();
    let i_count = i64::try_from(letters).unwrap_or(i64::MAX - 1).saturating_add(1);
    // A clock before 1970 or past i64 milliseconds cannot happen on a working machine; 0 only
    // makes the timestamp look old, it never fails the request.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0);
    let ts = (now_ms / 100) * 100 + 1000;
    ts + (i_count - ts.rem_euclid(i_count))
}

/// Maps a panel language code to DeepL's API spelling (upper case, primary subtag; `AUTO` for an
/// empty source, `RU` for an empty target). Shared with the browser provider (`browser.rs`).
pub(crate) fn normalize_deepl_lang(raw: &str, source: bool) -> String {
    let fallback = if source { "AUTO" } else { "RU" };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    let upper = trimmed.to_ascii_uppercase();
    let code = upper.split('-').next().unwrap_or(&upper).to_string();
    match code.as_str() {
        "AUTO" => "AUTO".to_string(),
        "ZH" | "ZH_CN" | "ZH_TW" => "ZH".to_string(),
        _ => code,
    }
}

/// A fresh JSON-RPC id baseline `NNNN0001` (NNNN in 1000..=9999 from the clock), as translatepy
/// builds it.
fn initial_request_id() -> i64 {
    // Only a seed: a broken clock yields the smallest baseline instead of failing.
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_micros()).ok())
        .unwrap_or(1);
    let suffix = (seed.abs() % 9000) + 1000;
    suffix * 10000 + 1
}

fn serialize_json_like_requests(value: &Value) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value)
}

/// The JSON-RPC `error.code` of an answer, when it has one.
fn jsonrpc_error_code(value: &Value) -> Option<i64> {
    value.get("error").and_then(|err| err.get("code")).and_then(Value::as_i64)
}

/// HTTP 429, or any DeepL "too many requests" code `10429xx` (whatever the status).
fn is_deepl_rate_limited(status: u16, code: Option<i64>) -> bool {
    status == 429 || code.is_some_and(|code| DEEPL_RATE_LIMIT_CODES.contains(&code))
}

/// The localized message of an error answer, quoting the start of its body.
fn api_error_message(method: &str, code: Option<i64>, reply: &DeeplReply) -> String {
    let compact = reply.body.trim().replace('\n', " ");
    let quoted: String = compact.chars().take(ERROR_BODY_QUOTE_CHARS).collect();
    tf!("translation.mt.deepl.api_error", method = method, code = code.unwrap_or(0), status = reply.status, body = quoted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mt_limits::call::MtWaitStatus;
    use crate::mt_limits::test_support::FakeClock;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    const MINUTE: Duration = Duration::from_secs(60);

    /// One request the fake saw.
    #[derive(Debug, Clone)]
    struct Seen {
        url: String,
        /// The JSON-RPC method.
        method: String,
        body: Value,
    }

    /// Answers requests from a script, in order, and records every request.
    #[derive(Debug, Default)]
    struct FakeHttp {
        replies: RefCell<VecDeque<Result<DeeplReply, DeeplTransportError>>>,
        seen: RefCell<Vec<Seen>>,
    }

    impl FakeHttp {
        fn new(replies: Vec<Result<DeeplReply, DeeplTransportError>>) -> Self {
            Self { replies: RefCell::new(replies.into()), seen: RefCell::new(Vec::new()) }
        }

        fn methods(&self) -> Vec<String> {
            self.seen.borrow().iter().map(|seen| seen.method.clone()).collect()
        }

        fn seen(&self, index: usize) -> Seen {
            self.seen.borrow().get(index).cloned().unwrap_or_else(|| panic!("no request #{index}"))
        }
    }

    impl DeeplHttp for FakeHttp {
        fn post_json(&self, url: &str, body: &str) -> Result<DeeplReply, DeeplTransportError> {
            let body: Value = serde_json::from_str(body).unwrap_or_else(|err| panic!("request body must be JSON: {err}"));
            let method = body.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
            self.seen.borrow_mut().push(Seen { url: url.to_string(), method: method.clone(), body });
            self.replies.borrow_mut().pop_front().unwrap_or_else(|| panic!("unscripted request {method}"))
        }
    }

    fn reply(status: u16, body: &Value) -> Result<DeeplReply, DeeplTransportError> {
        Ok(DeeplReply { status, retry_after: None, body: body.to_string() })
    }

    fn split(lists: &[&[&str]]) -> Result<DeeplReply, DeeplTransportError> {
        reply(200, &json!({ "result": { "lang": "JA", "splitted_texts": lists } }))
    }

    fn handled(sentences: &[&str]) -> Result<DeeplReply, DeeplTransportError> {
        let translations: Vec<Value> = sentences.iter().map(|sentence| json!({ "beams": [{ "postprocessed_sentence": sentence }] })).collect();
        reply(200, &json!({ "result": { "translations": translations } }))
    }

    fn too_many(retry_after: Option<Duration>) -> Result<DeeplReply, DeeplTransportError> {
        Ok(DeeplReply { status: 429, retry_after, body: "<html>Too Many Requests</html>".to_string() })
    }

    /// A JSON-RPC rate-limit answer with DeepL's `code` and HTTP `status`.
    fn coded(status: u16, code: i64) -> Result<DeeplReply, DeeplTransportError> {
        reply(status, &json!({ "jsonrpc": "2.0", "error": { "code": code, "message": "Too many requests" } }))
    }

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    fn no_wait(_status: Option<MtWaitStatus>) {}

    fn kinds(results: &[Result<String, MtItemError>]) -> Vec<Option<MtItemErrorKind>> {
        results.iter().map(|result| result.as_ref().err().map(|err| err.kind)).collect()
    }

    fn cooling(retry_in: Duration) -> Option<MtItemErrorKind> {
        Some(MtItemErrorKind::CoolingDown { retry_in })
    }

    fn id_of(seen: &Seen) -> i64 {
        seen.body["id"].as_i64().unwrap_or_else(|| panic!("the request id must be a number"))
    }

    #[test]
    fn one_request_pair_per_batch_and_nothing_else() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &no_wait);
        let fake = FakeHttp::new(vec![
            split(&[&["A1.", "A2."], &["B1."], &["C1."]]),
            handled(&["a1", "a2", "b1", "c1"]),
            split(&[&["D1."], &["E1."]]),
            handled(&["d1", "e1"]),
        ]);
        let client = DeeplClient::new(&fake);
        let first = client.translate_texts(&call, "ja", "ru", &texts(&["A1. A2.", "B1.", "C1."]));
        let second = client.translate_texts(&call, "ja", "ru", &texts(&["D1.", "E1."]));
        assert_eq!(first, vec![Ok("a1 a2".to_string()), Ok("b1".to_string()), Ok("c1".to_string())]);
        assert_eq!(second, vec![Ok("d1".to_string()), Ok("e1".to_string())]);
        // No getClientState: only the JSON-RPC pair per batch, all to the one endpoint.
        assert_eq!(fake.methods(), ["LMT_split_into_sentences", "LMT_handle_jobs", "LMT_split_into_sentences", "LMT_handle_jobs"]);
        assert!(fake.seen.borrow().iter().all(|seen| seen.url == DEEPL_JSONRPC_URL));
        // Four requests fit the full bucket of six: nothing waited.
        assert!(clock.sleeps().is_empty());
        // The ids continue the client's own sequence.
        let ids: Vec<i64> = (0..4).map(|index| id_of(&fake.seen(index))).collect();
        assert_eq!(ids, [ids[0], ids[0] + 1, ids[0] + 2, ids[0] + 3]);
        assert_eq!(ids[0] % 10_000, 2, "the first id follows the NNNN0001 baseline");
    }

    #[test]
    fn a_batch_packs_every_text_and_keeps_context_inside_each_bubble() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &no_wait);
        let fake = FakeHttp::new(vec![split(&[&["A1.", "A2."], &["B1."]]), handled(&["a1", "a2", "b1"])]);
        let client = DeeplClient::new(&fake);
        let results = client.translate_texts(&call, "auto", "ru", &texts(&["  A1. A2. ", "", "B1."]));
        assert_eq!(results, vec![Ok("a1 a2".to_string()), Ok(String::new()), Ok("b1".to_string())]);
        // Blank texts are not sent; the others are trimmed.
        assert_eq!(fake.seen(0).body["params"]["texts"], json!(["A1. A2.", "B1."]));
        let jobs = fake.seen(1).body["params"]["jobs"].clone();
        let sentences: Vec<&str> = jobs.as_array().map_or(&[][..], Vec::as_slice).iter().filter_map(|job| job["raw_en_sentence"].as_str()).collect();
        assert_eq!(sentences, ["A1.", "A2.", "B1."]);
        assert_eq!(jobs[1]["raw_en_context_before"], json!(["A1."]));
        assert_eq!(jobs[1]["raw_en_context_after"], json!([]));
        assert_eq!(jobs[2]["raw_en_context_before"], json!([]));
        // Auto mode sends the language DeepL detected while splitting.
        assert_eq!(fake.seen(1).body["params"]["lang"]["source_lang_computed"], json!("JA"));
    }

    #[test]
    fn batches_are_bounded_by_count_and_characters() {
        assert_eq!(pack_batches(&[], 10, 1000), Vec::<Range<usize>>::new());
        assert_eq!(pack_batches(&[5; 25], 10, 1000), [0..10, 10..20, 20..25]);
        assert_eq!(pack_batches(&[400, 400, 400, 10], 10, 1000), [0..2, 2..4]);
        // An over-long text goes alone, never cut.
        assert_eq!(pack_batches(&[10, 1500, 10], 10, 1000), [0..1, 1..2, 2..3]);
        assert_eq!(DEEPL_BATCH_MAX_TEXTS, DeeplMtSession::new().preferred_batch_size());
    }

    #[test]
    fn a_missing_sentence_fails_only_its_own_text() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &no_wait);
        let broken = reply(200, &json!({ "result": { "translations": [
            { "beams": [{ "postprocessed_sentence": "a1" }] },
            { "beams": [] },
            { "beams": [{ "postprocessed_sentence": "c1" }] },
        ] } }));
        let fake = FakeHttp::new(vec![split(&[&["A1."], &["B1."], &["C1."]]), broken]);
        let results = DeeplClient::new(&fake).translate_texts(&call, "ja", "ru", &texts(&["A1.", "B1.", "C1."]));
        assert_eq!(results[0], Ok("a1".to_string()));
        assert_eq!(kinds(&results), [None, Some(MtItemErrorKind::Failed), None]);
        assert_eq!(results[2], Ok("c1".to_string()));
    }

    #[test]
    fn an_unmappable_answer_fails_the_whole_batch() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &no_wait);
        let fake = FakeHttp::new(vec![split(&[&["A1."], &["B1."]]), handled(&["only one"])]);
        let results = DeeplClient::new(&fake).translate_texts(&call, "ja", "ru", &texts(&["A1.", "B1."]));
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::Failed); 2]);
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn an_unexpected_split_shape_sends_each_text_whole() {
        let result = json!({ "splitted_texts": ["x", "y", "z"] });
        assert_eq!(parse_split_batch(&result, &["T1", "T2"]), [vec!["T1".to_string()], vec!["T2".to_string()]]);
        // The flat legacy shape is still read for a single text; an empty list falls back.
        assert_eq!(parse_split_batch(&result, &["T1"]), [vec!["x".to_string(), "y".to_string(), "z".to_string()]]);
        assert_eq!(parse_split_batch(&json!({ "splitted_texts": [[], ["b"]] }), &["T1", "T2"]), [vec!["T1".to_string()], vec!["b".to_string()]]);
    }

    #[test]
    fn every_rate_limit_answer_stops_without_a_re_send() {
        // HTTP 429 with a non-JSON body, and the 10429xx codes the user saw (with and without
        // an HTTP 429 status).
        for answer in [too_many(None), coded(429, 1_042_901), coded(200, 1_042_902), coded(429, 1_042_912)] {
            let (clock, registry) = FakeClock::registry();
            let cancel = AtomicBool::new(false);
            let call = MtCall::for_run(&registry, &cancel, &no_wait);
            let fake = FakeHttp::new(vec![answer.clone()]);
            // Twelve texts = two batches: the second one is never sent either.
            let results = DeeplClient::new(&fake).translate_texts(&call, "ja", "ru", &texts(&["A1."; 12]));
            assert_eq!(fake.methods(), ["LMT_split_into_sentences"], "{answer:?}");
            assert_eq!(kinds(&results), vec![cooling(2 * MINUTE); 12], "{answer:?}");
            assert!(clock.sleeps().is_empty(), "a DeepL cooldown is never waited out in place");
        }
    }

    #[test]
    fn rate_limit_codes_are_the_10429xx_family() {
        for code in [1_042_900, 1_042_901, 1_042_902, 1_042_911, 1_042_912, 1_042_999] {
            assert!(is_deepl_rate_limited(200, Some(code)), "{code}");
            assert!(is_deepl_rate_limited(503, Some(code)), "{code}");
        }
        assert!(is_deepl_rate_limited(429, None));
        for code in [1_042_899, 1_043_000, -32600, 1_156_049] {
            assert!(!is_deepl_rate_limited(200, Some(code)), "{code}");
        }
        assert!(!is_deepl_rate_limited(503, None));
    }

    #[test]
    fn a_run_during_the_cooldown_sends_nothing_and_a_repeat_limit_escalates() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &no_wait);
        let first = FakeHttp::new(vec![too_many(None)]);
        assert_eq!(kinds(&DeeplClient::new(&first).translate_texts(&call, "ja", "ru", &texts(&["A1."]))), [cooling(2 * MINUTE)]);
        // A new run 30 s later is refused before any request, with the time left.
        clock.advance(Duration::from_secs(30));
        let during = FakeHttp::new(Vec::new());
        assert_eq!(kinds(&DeeplClient::new(&during).translate_texts(&call, "ja", "ru", &texts(&["B1."]))), [cooling(Duration::from_secs(90))]);
        assert!(during.methods().is_empty());
        // After the cooldown the probe is limited again: the next step, 5 min.
        clock.advance(Duration::from_secs(90));
        let probe = FakeHttp::new(vec![coded(429, 1_042_901)]);
        assert_eq!(kinds(&DeeplClient::new(&probe).translate_texts(&call, "ja", "ru", &texts(&["C1."]))), [cooling(5 * MINUTE)]);
        // A success ends the probation; a limit within the 30 min level memory continues at
        // 10 min instead of restarting at 2 min.
        clock.advance(5 * MINUTE);
        let ok = FakeHttp::new(vec![split(&[&["D1."]]), handled(&["d1"])]);
        assert_eq!(DeeplClient::new(&ok).translate_texts(&call, "ja", "ru", &texts(&["D1."])), vec![Ok("d1".to_string())]);
        clock.advance(20 * MINUTE);
        let again = FakeHttp::new(vec![too_many(None)]);
        assert_eq!(kinds(&DeeplClient::new(&again).translate_texts(&call, "ja", "ru", &texts(&["E1."]))), [cooling(10 * MINUTE)]);
    }

    #[test]
    fn retry_after_lengthens_the_cooldown() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &no_wait);
        let fake = FakeHttp::new(vec![too_many(Some(20 * MINUTE))]);
        let results = DeeplClient::new(&fake).translate_texts(&call, "ja", "ru", &texts(&["A1."]));
        assert_eq!(kinds(&results), [cooling(20 * MINUTE)]);
    }

    #[test]
    fn single_text_batches_wait_for_the_request_budget_instead_of_failing() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let log = RefCell::new(Vec::new());
        let on_wait = |status: Option<MtWaitStatus>| log.borrow_mut().push(status);
        let call = MtCall::for_run(&registry, &cancel, &on_wait);
        let mut replies = Vec::new();
        for _ in 0..12 {
            replies.push(split(&[&["A1."]]));
            replies.push(handled(&["a1"]));
        }
        let fake = FakeHttp::new(replies);
        let client = DeeplClient::new(&fake);
        // Twelve bubbles translated one by one: 24 requests.
        for _ in 0..12 {
            assert_eq!(client.translate_texts(&call, "ja", "ru", &texts(&["A1."])), vec![Ok("a1".to_string())]);
        }
        assert_eq!(fake.methods().len(), 24);
        // Six go from the full bucket; each of the other 18 waits for its own token.
        assert_eq!(clock.sleeps().iter().sum::<Duration>(), 18 * MINUTE);
        assert!(log.borrow().iter().flatten().all(|status| status.service == MtService::Deepl && status.remaining.is_some()));
        assert_eq!(log.borrow().iter().filter(|status| status.is_none()).count(), 18, "every token wait is reported and ended");
    }

    #[test]
    fn cancel_during_a_budget_wait_sends_nothing_more() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let on_wait = |status: Option<MtWaitStatus>| {
            if status.is_some() {
                cancel.store(true, Ordering::Relaxed);
            }
        };
        let call = MtCall::for_run(&registry, &cancel, &on_wait);
        // Three single-text batches empty the bucket; the fourth waits and is stopped.
        let fake = FakeHttp::new(vec![
            split(&[&["A1."]]),
            handled(&["a1"]),
            split(&[&["B1."]]),
            handled(&["b1"]),
            split(&[&["C1."]]),
            handled(&["c1"]),
        ]);
        let long = "x".repeat(600);
        let mut items = vec![format!("A1. {long}"), format!("B1. {long}"), format!("C1. {long}"), format!("D1. {long}"), format!("E1. {long}")];
        items.push(String::new());
        let results = DeeplClient::new(&fake).translate_texts(&call, "ja", "ru", &items);
        assert_eq!(fake.methods().len(), 6);
        assert_eq!(kinds(&results[..5]), [None, None, None, Some(MtItemErrorKind::Cancelled), Some(MtItemErrorKind::Cancelled)]);
        // The blank text still answers "".
        assert_eq!(results[5], Ok(String::new()));
    }

    #[test]
    fn transport_and_error_answers_fail_the_batch_without_retry() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &no_wait);
        let fake = FakeHttp::new(vec![
            Err(DeeplTransportError::Network("timed out".to_string())),
            reply(400, &json!({ "error": { "code": -32600 } })),
            split(&[&["C1."]]),
            handled(&["c1"]),
        ]);
        let client = DeeplClient::new(&fake);
        let results = client.translate_texts(&call, "ja", "ru", &texts(&["A1."]));
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::Failed)]);
        let results = client.translate_texts(&call, "ja", "ru", &texts(&["B1."]));
        assert_eq!(kinds(&results), [Some(MtItemErrorKind::Failed)]);
        assert_eq!(client.translate_texts(&call, "ja", "ru", &texts(&["C1."])), vec![Ok("c1".to_string())]);
        assert_eq!(fake.methods().len(), 4);
        // Neither failure touched the route's health.
        assert_eq!(call.lane_cooldown(MtLane::builtin(MtService::Deepl)), None);
    }
}
} // mod native
