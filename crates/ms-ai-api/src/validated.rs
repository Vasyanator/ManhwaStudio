/*
File: crates/ms-ai-api/src/validated.rs

Purpose:
Native-only validated LLM execution: runs a tracked chat request, lets the consumer validate the
answer, and asks the model ONCE more with a repair message when the answer is not usable. Also
answers a provider that rejects the requested structured-output format by sending the request
again without it.

Key items:
- Validation<T>        : the consumer's verdict on one answer (its best value + an optional repair).
- RepairRequest        : why to ask again (`RetryReason`, shown by the status widget) and the
                         English repair message for the model.
- ValidatedAnswer<T>   : the outcome: last value, last raw answer, whether a repair retry ran,
                         what is still unresolved, whether structured output was rejected.
- exec_chat_validated(): the executor; `MAX_REPAIR_RETRIES` is its retry limit.

Notes:
Built on `generation::exec_attempt` (the tracked executor), so progress, Stop and the
non-streaming fallback behave exactly as in `exec_chat_tracked`. The repair turns exist only in
the request of the second attempt: the caller's own conversation never sees them.
*/

use genai::chat::{ChatMessage, ChatOptions, ChatRequest};
use genai::{Client, ModelIden};
use ms_log::runtime_log;

use crate::error::AiApiError;
use crate::generation::{AttemptFailure, GenerationTracker, RetryReason, TrackedAnswer, exec_attempt};
use crate::structured::response_format_rejected;

/// How many times `exec_chat_validated` asks the model again after an unusable answer; the
/// executor's loop is bounded by it (the product decision is exactly one paid repair).
pub const MAX_REPAIR_RETRIES: u8 = 1;

/// Why and how to ask the model again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairRequest {
    /// The reason the status widget shows while the retry runs.
    pub reason: RetryReason,
    /// The user message sent after the model's own unusable answer: what was wrong and what to
    /// return now (it may ask for only part of the original request). English model input,
    /// never UI text.
    pub message: String,
}

/// A validator's verdict on one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation<T> {
    /// The best usable result of the answer (possibly partial or empty).
    pub value: T,
    /// `Some` when the answer is not (fully) usable and asking again could help.
    pub repair: Option<RepairRequest>,
}

/// What `exec_chat_validated` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAnswer<T> {
    /// The validator's value for the LAST answer. A validator that merges answers keeps the
    /// merge in its own state and returns it here.
    pub value: T,
    /// The last answer as received.
    pub answer: TrackedAnswer,
    /// The reason of the last repair retry that ran, if any.
    pub retried: Option<RetryReason>,
    /// The validator's repair request for the last answer: problems left unresolved after the
    /// retry limit was reached.
    pub unresolved: Option<RepairRequest>,
    /// The provider rejected the structured-output format (`ChatOptions::response_format`) and
    /// the request was answered without it; a caller should stop sending it to this provider.
    pub structured_output_rejected: bool,
}

/// The parts of a call that stay fixed across its attempts.
struct Call<'a> {
    client: &'a Client,
    model: ModelIden,
    tracker: &'a GenerationTracker,
    request_tag: u64,
}

/// Runs `request` like `exec_chat_tracked` and validates the answer with `validate`, which gets
/// the answer and the attempt index (`0`, then `1` for the retry).
///
/// While the verdict carries a `RepairRequest` and the retry limit (`MAX_REPAIR_RETRIES` = 1)
/// is not used up, the request is sent again with two extra turns: the model's unusable answer
/// (as an assistant message) and the repair message (as a user message). A blank answer gets no
/// assistant turn (several providers refuse empty messages); the original request is simply sent
/// again. The verdict on the last allowed attempt is final: its repair request is returned as
/// `unresolved`.
///
/// Structured output: when `options` carry a `response_format` and an attempt fails before any
/// output with an error `structured::response_format_rejected` recognizes, the attempt is sent
/// again without the format (shown as `RetryReason::StructuredOutputUnsupported`; it does not
/// use up the repair retry) and the rest of the call runs without it
/// (`structured_output_rejected`).
///
/// Never retried: a cancellation, or any other provider / transport error (both are returned as
/// errors, also when they hit the repair attempt; values the validator already handed on stay
/// the caller's). A truncated answer (`TrackedAnswer::truncated`) is only reported to the
/// validator, which decides whether asking again (e.g. for fewer items) makes sense. Every
/// retry is logged.
///
/// # Errors
/// The errors of `exec_chat_tracked`: `AiApiError::Cancelled`, `AiApiError::ChatRequest`.
pub async fn exec_chat_validated<T, F>(client: &Client, model: ModelIden, request: ChatRequest, options: Option<&ChatOptions>, tracker: &GenerationTracker, request_tag: u64, mut validate: F) -> Result<ValidatedAnswer<T>, AiApiError>
where
    F: FnMut(&TrackedAnswer, u8) -> Validation<T>,
{
    let call = Call { client, model, tracker, request_tag };
    let model_name = call.model.model_name.to_string();
    let mut options = options.cloned();
    let mut structured_output_rejected = false;
    let mut next_request = request.clone();
    let mut retried: Option<RetryReason> = None;
    let mut attempt_index: u8 = 0;
    loop {
        let answer = attempt(&call, next_request, &mut options, retried, &mut structured_output_rejected).await?;
        let verdict = validate(&answer, attempt_index);
        let repair = match verdict.repair {
            Some(repair) if attempt_index < MAX_REPAIR_RETRIES => repair,
            unresolved => {
                if retried.is_some() {
                    match &unresolved {
                        Some(left) => runtime_log::log_warn(format!("[AI API] retried answer of '{model_name}' (tag {request_tag}) is still not fully usable ({:?})", left.reason)),
                        None => runtime_log::log_info(format!("[AI API] retried answer of '{model_name}' (tag {request_tag}) is usable")),
                    }
                }
                return Ok(ValidatedAnswer { value: verdict.value, answer, retried, unresolved, structured_output_rejected });
            }
        };
        runtime_log::log_warn(format!("[AI API] answer of '{model_name}' (tag {request_tag}) is not usable ({:?}, {} chars, truncated={}); asking again (retry {} of {MAX_REPAIR_RETRIES})", repair.reason, answer.text.chars().count(), answer.truncated, attempt_index + 1));
        // Every retry is the ORIGINAL request plus the last unusable answer and its repair
        // message (earlier repair turns are not stacked); a blank answer resends the request.
        next_request = if answer.text.trim().is_empty() {
            request.clone()
        } else {
            request.clone().append_message(ChatMessage::assistant(answer.text)).append_message(ChatMessage::user(repair.message))
        };
        retried = Some(repair.reason);
        attempt_index += 1;
    }
}

/// One attempt of `call`, with the structured-output fallback described on
/// `exec_chat_validated`: on a recognized rejection, `options.response_format` is cleared for
/// good, `rejected` is set and the attempt is sent once more.
async fn attempt(call: &Call<'_>, request: ChatRequest, options: &mut Option<ChatOptions>, retry: Option<RetryReason>, rejected: &mut bool) -> Result<TrackedAnswer, AiApiError> {
    let has_format = options.as_ref().is_some_and(|options| options.response_format.is_some());
    let fallback_request = has_format.then(|| request.clone());
    match exec_attempt(call.client, call.model.clone(), request, options.as_ref(), call.tracker, call.request_tag, retry).await {
        Ok(answer) => Ok(answer),
        Err(AttemptFailure { error, before_output }) => {
            let rejected_format = before_output && matches!(&error, AiApiError::ChatRequest { detail } if response_format_rejected(detail));
            let (Some(request), true) = (fallback_request, rejected_format) else {
                return Err(error);
            };
            runtime_log::log_warn(format!("[AI API] '{}' rejected the structured-output format ({}); sending the request again without it", call.model.model_name, error.chat_failure_detail()));
            if let Some(options) = options.as_mut() {
                options.response_format = None;
            }
            *rejected = true;
            exec_attempt(call.client, call.model.clone(), request, options.as_ref(), call.tracker, call.request_tag, Some(RetryReason::StructuredOutputUnsupported)).await.map_err(|failure| failure.error)
        }
    }
}

/// Checks against a loopback OpenAI-compatible server (no external network, no files).
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use genai::chat::{ChatMessage, ChatOptions, ChatRequest};

    use super::{RepairRequest, ValidatedAnswer, Validation, exec_chat_validated};
    use crate::client::{block_on, build_client};
    use crate::error::AiApiError;
    use crate::generation::{GenerationTracker, RetryReason, TrackedAnswer};
    use crate::loopback_test_server::{WAIT, delta_event, finish_events, finish_events_with, json_response, serve, sse_response};
    use crate::model_id::model_iden;
    use crate::service::AiApiService;
    use crate::structured::json_spec_format;
    use crate::target::AiApiTarget;

    fn answer_response(text: &str, finish: &str) -> String {
        let mut events = vec![delta_event("content", text)];
        events.extend(finish_events_with(finish));
        sse_response(&events)
    }

    /// Accepts an answer equal to "good"; asks again with reason `Malformed` otherwise.
    fn expect_good(answer: &TrackedAnswer, _attempt: u8) -> Validation<String> {
        let repair = (answer.text != "good").then(|| RepairRequest { reason: RetryReason::Malformed, message: "Answer exactly: good".to_string() });
        Validation { value: answer.text.clone(), repair }
    }

    fn run<T, F>(base_url: &str, options: Option<&ChatOptions>, tracker: &GenerationTracker, validate: F) -> Result<ValidatedAnswer<T>, AiApiError>
    where
        F: FnMut(&TrackedAnswer, u8) -> Validation<T>,
    {
        let target = AiApiTarget::new(AiApiService::OpenAiCompatible, base_url).expect("loopback base URL is valid");
        let client = build_client(&target, String::new());
        let model = model_iden(AiApiService::OpenAiCompatible, "test-model").expect("non-empty model id");
        let request = ChatRequest::default().append_message(ChatMessage::user("hi"));
        block_on(exec_chat_validated(&client, model, request, options, tracker, 3, validate)).expect("tokio runtime builds")
    }

    #[test]
    fn a_valid_answer_is_not_retried() {
        let server = serve(vec![answer_response("good", "stop")], false);
        let result = run(&server.base_url, None, &GenerationTracker::new(), expect_good).expect("request succeeds");
        assert_eq!(result.value, "good");
        assert_eq!((result.retried, result.unresolved, result.structured_output_rejected), (None, None, false));
        assert!(server.requests.recv_timeout(WAIT).is_ok());
        assert!(server.requests.recv_timeout(Duration::from_millis(200)).is_err(), "no second request");
    }

    #[test]
    fn an_unusable_answer_is_repaired_once_with_the_bad_answer_and_the_repair_message() {
        let server = serve(vec![answer_response("bad", "stop"), answer_response("good", "stop")], false);
        let tracker = GenerationTracker::new();
        let mut attempts = Vec::new();
        let result = run(&server.base_url, None, &tracker, |answer, attempt| {
            attempts.push(attempt);
            expect_good(answer, attempt)
        })
        .expect("request succeeds");
        assert_eq!(attempts, vec![0, 1]);
        assert_eq!(result.value, "good");
        assert_eq!(result.retried, Some(RetryReason::Malformed));
        assert_eq!(result.unresolved, None);
        assert_eq!(tracker.snapshot().retry, Some(RetryReason::Malformed));
        let first = server.requests.recv_timeout(WAIT).expect("first request");
        let second = server.requests.recv_timeout(WAIT).expect("repair request");
        assert!(!first.contains("Answer exactly"), "the first request carries no repair turn: {first}");
        assert!(second.contains(r#"{"content":"bad","role":"assistant"}"#), "the bad answer is replayed: {second}");
        assert!(second.contains("Answer exactly: good"), "the repair message is sent: {second}");
    }

    #[test]
    fn the_retry_limit_is_one() {
        let server = serve(vec![answer_response("bad", "stop"), answer_response("worse", "stop"), answer_response("good", "stop")], false);
        let result = run(&server.base_url, None, &GenerationTracker::new(), expect_good).expect("request succeeds");
        assert_eq!(result.value, "worse");
        assert_eq!(result.retried, Some(RetryReason::Malformed));
        assert_eq!(result.unresolved.map(|repair| repair.reason), Some(RetryReason::Malformed));
        assert!(server.requests.recv_timeout(WAIT).is_ok());
        assert!(server.requests.recv_timeout(WAIT).is_ok());
        assert!(server.requests.recv_timeout(Duration::from_millis(200)).is_err(), "no third request");
    }

    #[test]
    fn a_blank_answer_resends_the_original_request() {
        let empty = {
            let mut events = finish_events();
            events.insert(0, delta_event("content", ""));
            sse_response(&events)
        };
        let server = serve(vec![empty, answer_response("good", "stop")], false);
        let result = run(&server.base_url, None, &GenerationTracker::new(), expect_good).expect("request succeeds");
        assert_eq!(result.value, "good");
        let first = server.requests.recv_timeout(WAIT).expect("first request");
        let second = server.requests.recv_timeout(WAIT).expect("retry");
        assert_eq!(first, second, "the retry of a blank answer is the original request");
    }

    #[test]
    fn the_validator_sees_a_truncated_answer() {
        let server = serve(vec![answer_response("[1,", "length")], false);
        let mut seen = None;
        let result = run(&server.base_url, None, &GenerationTracker::new(), |answer, _| {
            seen = Some(answer.truncated);
            Validation { value: (), repair: None }
        });
        assert!(result.is_ok());
        assert_eq!(seen, Some(true));
    }

    #[test]
    fn provider_errors_are_not_retried() {
        let server = serve(vec![json_response("429 Too Many Requests", r#"{"error":{"message":"Rate limit reached"}}"#)], false);
        let result = run(&server.base_url, None, &GenerationTracker::new(), expect_good);
        assert!(matches!(&result, Err(AiApiError::ChatRequest { detail }) if detail.contains("429")), "got {result:?}");
        assert!(server.requests.recv_timeout(WAIT).is_ok());
        assert!(server.requests.recv_timeout(Duration::from_millis(200)).is_err(), "no second request");
    }

    #[test]
    fn a_rejected_response_format_is_dropped_without_using_the_repair_retry() {
        let rejection = r#"{"error":{"message":"Invalid parameter: 'response_format' of type 'json_schema' is not supported with this model.","type":"invalid_request_error","param":"response_format"}}"#;
        let server = serve(vec![json_response("400 Bad Request", rejection), answer_response("bad", "stop"), answer_response("good", "stop")], false);
        let options = ChatOptions::default().with_response_format(json_spec_format("test", serde_json::json!({"type": "object", "properties": {}})));
        let result = run(&server.base_url, Some(&options), &GenerationTracker::new(), expect_good).expect("request succeeds");
        assert!(result.structured_output_rejected);
        assert_eq!(result.retried, Some(RetryReason::Malformed), "the repair retry was still available");
        assert_eq!(result.value, "good");
        let first = server.requests.recv_timeout(WAIT).expect("first request");
        let second = server.requests.recv_timeout(WAIT).expect("request without the format");
        let third = server.requests.recv_timeout(WAIT).expect("repair request");
        assert!(first.contains("response_format"), "the first request carries the format: {first}");
        assert!(!second.contains("response_format") && !third.contains("response_format"), "later requests do not: {second} / {third}");
    }

    #[test]
    fn a_closed_tracker_never_sends_the_request() {
        let tracker = GenerationTracker::new();
        tracker.close();
        let result = run("http://127.0.0.1:9", None, &tracker, expect_good);
        assert!(matches!(result, Err(AiApiError::Cancelled)), "got {result:?}");
    }
}
