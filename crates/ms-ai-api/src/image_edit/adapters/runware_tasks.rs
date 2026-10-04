/*
File: crates/ms-ai-api/src/image_edit/adapters/runware_tasks.rs

Purpose:
Adapter of the Runware task API (`POST https://api.runware.ai/v1`, a JSON ARRAY of tasks,
Bearer): one `imageInference` task, answered with `data[0].imageURL`; a task still
`processing` is followed with `getResponse` tasks.

Key structures:
- RunwareTasks (the `EditProtocol`)

Notes:
The task: `taskType`, a client-generated `taskUUID` (UUID v4, "must be unique per task"),
`model` (AIR id), `positivePrompt`, `outputType: "URL"`, `outputFormat: "PNG"`,
`numberResults: 1`, `width` / `height` only for `SizeParamStyle::WidthHeight`, and `inputs`
per the model's published schema: `seedImage` + `maskImage` (white = edit) for the FLUX Fill
and Ideogram edit models, `referenceImages[]` for the reference-edit models; images travel as
PNG data URIs (the schemas accept "UUID, URL, Data URI, or Base64"). Delivery is the default
synchronous one: `getResponse` is itself a POST, and the executor never retries a POST, so a
synchronous submit is the more robust exchange; a `processing` answer is still followed by
`getResponse` polls. Failures arrive as `{"errors": [{code, message, taskUUID}]}` (with an HTTP
status over REST, or inside a 200 for a failed task). Result URLs (`im.runware.ai`) are
downloaded without the key. No cancel endpoint.
`taskUUID` comes from `task_uuid`: std's per-thread random hasher keys plus a process counter,
formatted as a v4 UUID. The API needs uniqueness, not secrecy, so no RNG crate is added.
Sources: https://runware.ai/docs/models-api/authentication (REST shape, header auth, answer),
https://runware.ai/docs/models-api/task-polling (getResponse, statuses),
https://runware.ai/docs/models-api/errors, https://runware.ai/docs/models-api/model-schemas
and https://runware.ai/docs/models/{model}/schema.json of every catalogue model, fetched
2026-10-04.
*/

use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{classify_error, is_success, json_post, json_value, job_failure, mask_data_url, png_data_url, required_str, result_url_step};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::MaskPolarity;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx, StepPhase};

/// `errors[].code` values of a rejected key (`invalidApiKey` is the errors page's example).
const KEY_CODES: [&str; 2] = ["invalidApiKey", "missingApiKey"];
/// `errors[].code` values of an exhausted balance (HTTP 402 is classified as such regardless).
const CREDIT_CODES: [&str; 1] = ["insufficientCredits"];

/// Distinguishes task UUIDs minted in one process (hash input, not a secret).
static TASK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The Runware task adapter (stateless; the task UUID travels in the answer).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunwareTasks;

/// A fresh UUID v4 string. Uniqueness comes from std's per-thread random hasher keys (seeded
/// from the OS on native targets) and a process-wide counter; the version and variant bits
/// are set as RFC 9562 requires.
fn task_uuid() -> String {
    let counter = TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0u8; 16];
    for (half, chunk) in bytes.chunks_exact_mut(8).enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(counter);
        hasher.write_usize(half);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes.iter().fold(String::with_capacity(32), |mut hex, byte| {
        // Writing into a `String` cannot fail.
        if write!(hex, "{byte:02x}").is_err() {
            hex.push_str("00");
        }
        hex
    });
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// Whether a catalogue model takes `seedImage` + `maskImage` (else `referenceImages`).
fn seed_and_mask_model(model_id: &str) -> bool {
    matches!(model_id, "bfl:1@2" | "ideogram:4@3")
}

/// The `imageInference` task of `call` with the given `task_uuid`.
fn inference_task(call: &EditCall, task_uuid: &str) -> Result<Value, ImageEditError> {
    let image = png_data_url(&call.image_png);
    let inputs = if seed_and_mask_model(&call.model_id) {
        let mask = mask_data_url(call, MaskPolarity::WhiteEdits)?.ok_or_else(|| ImageEditError::RequestBuild { detail: format!("Runware {} requires a mask", call.model_id) })?;
        json!({ "seedImage": image, "maskImage": mask })
    } else {
        json!({ "referenceImages": [image] })
    };
    let mut task = Map::new();
    task.insert("taskType".to_string(), json!("imageInference"));
    task.insert("taskUUID".to_string(), json!(task_uuid));
    task.insert("model".to_string(), json!(call.model_id));
    task.insert("positivePrompt".to_string(), json!(call.prompt));
    task.insert("inputs".to_string(), inputs);
    match call.size_param {
        SizeParamStyle::WidthHeight => {
            task.insert("width".to_string(), json!(call.width));
            task.insert("height".to_string(), json!(call.height));
        }
        SizeParamStyle::None => {}
        SizeParamStyle::WxH | SizeParamStyle::WStarH | SizeParamStyle::ImageSizeObject | SizeParamStyle::AspectTier(_) => {
            return Err(ImageEditError::RequestBuild { detail: format!("Runware cannot state the size as {:?}", call.size_param) });
        }
    }
    task.insert("outputType".to_string(), json!("URL"));
    task.insert("outputFormat".to_string(), json!("PNG"));
    task.insert("numberResults".to_string(), json!(1));
    Ok(Value::Object(task))
}

impl EditProtocol for RunwareTasks {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        Ok(json_post(call.base_url.clone(), Vec::new(), json!([inference_task(call, &task_uuid())?]), AuthScheme::Bearer))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(runware_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        if value.pointer("/errors/0").is_some() {
            return Err(runware_error(response.status, &response.body));
        }
        let result = value.pointer("/data/0").ok_or_else(|| ImageEditError::NoImageReturned { reason: "the answer has no data[0]".to_string() })?;
        if let Some(url) = result.get("imageURL").and_then(Value::as_str) {
            return result_url_step(url);
        }
        match result.get("status").and_then(Value::as_str) {
            Some("processing") => {
                let id = match (step.phase, step.job) {
                    (StepPhase::Poll { .. }, Some(job)) => job.id.clone(),
                    _ => required_str(result, "/taskUUID")?.to_string(),
                };
                let poll = json_post(step.call.base_url.clone(), Vec::new(), json!([{ "taskType": "getResponse", "taskUUID": id }]), AuthScheme::Bearer);
                Ok(NextStep::Poll { request: poll, after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: None, cancel_url: None }) })
            }
            Some("error") => Err(job_failure(result.pointer("/error/message").and_then(Value::as_str).unwrap_or("the task failed"))),
            _ => Err(ImageEditError::NoImageReturned { reason: "data[0] has no imageURL".to_string() }),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

/// A Runware failure: the first `errors[]` code decides key / credit / moderation, then the
/// shared classifier (status + markers + `errors[0].message`).
fn runware_error(status: u16, body: &[u8]) -> ImageEditError {
    let value: Option<Value> = serde_json::from_slice(body).ok();
    let code = value.as_ref().and_then(|value| value.pointer("/errors/0/code")).and_then(Value::as_str).unwrap_or_default();
    if KEY_CODES.contains(&code) {
        return ImageEditError::KeyRejected;
    }
    if CREDIT_CODES.contains(&code) {
        return ImageEditError::OutOfCredits;
    }
    let lower = code.to_lowercase();
    if lower.contains("nsfw") || lower.contains("moderat") || lower.contains("contentpolicy") || lower.contains("safety") {
        return ImageEditError::Moderated;
    }
    if is_success(status) {
        // A failed task inside a 200 answer.
        let message = value.as_ref().and_then(|value| value.pointer("/errors/0/message")).and_then(Value::as_str).unwrap_or("the task failed");
        return job_failure(&format!("{code}: {message}"));
    }
    classify_error(status, body)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{RunwareTasks, task_uuid};
    use crate::image_edit::adapters::png_data_url;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    const IMAGE: &str = "data:image/png;base64,UE5HREFUQQ==";

    fn rw_call(model: &str, mask: Option<Vec<u8>>, size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Runware, model, "https://api.runware.ai/v1", mask, (2, 2), size_param, None)
    }

    /// The single task of a submit body, with its generated UUID taken out.
    fn task(call: &EditCall) -> Option<(Value, String)> {
        let HttpBody::Json(Value::Array(mut tasks)) = RunwareTasks.submit(call).ok()?.body else { return None };
        let mut task = tasks.pop()?;
        let uuid = task.as_object_mut()?.remove("taskUUID")?.as_str()?.to_string();
        Some((task, uuid))
    }

    fn is_uuid_v4(text: &str) -> bool {
        let groups: Vec<&str> = text.split('-').collect();
        groups.iter().map(|group| group.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12] && text.chars().all(|c| c == '-' || c.is_ascii_hexdigit()) && text.as_bytes().get(14) == Some(&b'4') && matches!(text.as_bytes().get(19), Some(b'8' | b'9' | b'a' | b'b'))
    }

    #[test]
    fn task_uuids_are_unique_v4() {
        let first = task_uuid();
        let second = task_uuid();
        assert!(is_uuid_v4(&first), "{first}");
        assert_ne!(first, second);
    }

    // The header-auth sample `curl https://api.runware.ai/v1 -H "Authorization: Bearer ..." -d
    // '[{"taskType":"imageInference", ...}]'` (authentication page) with each model's schema inputs.
    #[test]
    fn tasks_follow_the_model_schemas() {
        let spec = RunwareTasks.submit(&rw_call("runware:108@22", None, SizeParamStyle::WidthHeight)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api.runware.ai/v1", Some(AuthScheme::Bearer)));
        let (reference, uuid) = task(&rw_call("runware:108@22", None, SizeParamStyle::WidthHeight)).unwrap_or_else(|| panic!("no task"));
        assert!(is_uuid_v4(&uuid), "{uuid}");
        assert_eq!(reference, json!({ "taskType": "imageInference", "model": "runware:108@22", "positivePrompt": "Remove the speech bubble text", "inputs": { "referenceImages": [IMAGE] }, "width": 2, "height": 2, "outputType": "URL", "outputFormat": "PNG", "numberResults": 1 }));
        let mask = vec![0, 255, 255, 0];
        let white = png_data_url(&encode_mask_png(&mask, 2, 2, MaskPolarity::WhiteEdits).unwrap_or_default());
        let fill = task(&rw_call("bfl:1@2", Some(mask), SizeParamStyle::None)).map(|(task, _)| task);
        assert_eq!(fill, Some(json!({ "taskType": "imageInference", "model": "bfl:1@2", "positivePrompt": "Remove the speech bubble text", "inputs": { "seedImage": IMAGE, "maskImage": white }, "outputType": "URL", "outputFormat": "PNG", "numberResults": 1 })));
        assert!(matches!(RunwareTasks.submit(&rw_call("ideogram:4@3", None, SizeParamStyle::None)), Err(ImageEditError::RequestBuild { .. })));
        assert!(matches!(RunwareTasks.submit(&rw_call("google:4@2", None, SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // Answers: the authentication page's `data[0].imageURL`, task-polling's `processing` /
    // `errors[]` shapes, the errors page's `invalidApiKey` example.
    #[test]
    fn answers_and_errors() {
        let call = rw_call("google:4@2", None, SizeParamStyle::WidthHeight);
        let done = r#"{"data":[{"taskType":"imageInference","taskUUID":"39d7207a-87ef-4c93-8082-1431f9c1dc97","imageUUID":"b7db282d-2943-4f12-992f-77df3ad3ec71","imageURL":"https://im.runware.ai/image/os/a14d18/ws/2/ii/b7db282d-2943-4f12-992f-77df3ad3ec71.png"}]}"#;
        assert!(matches!(RunwareTasks.next(&submit_step(&call), &json_response(200, done)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.starts_with("https://im.runware.ai/")));
        let processing = r#"{"data":[{"taskType":"imageInference","taskUUID":"24cd5dff-cb81-4db5-8506-b72a9425f9d1","status":"processing","progress":47}]}"#;
        match RunwareTasks.next(&submit_step(&call), &json_response(200, processing)) {
            Ok(NextStep::Poll { request, job: Some(job), .. }) => {
                assert_eq!(job.id, "24cd5dff-cb81-4db5-8506-b72a9425f9d1");
                assert_eq!((request.method, request.body), (HttpMethod::Post, HttpBody::Json(json!([{ "taskType": "getResponse", "taskUUID": "24cd5dff-cb81-4db5-8506-b72a9425f9d1" }]))));
            }
            other => panic!("{other:?}"),
        }
        let job = JobRef { id: "24cd5dff-cb81-4db5-8506-b72a9425f9d1".to_string(), poll_url: None, cancel_url: None };
        let failed = r#"{"data":[],"errors":[{"code":"timeoutProvider","status":"error","message":"The external provider did not respond within the timeout window. The request was automatically terminated.","documentation":"https://runware.ai/docs/models-api/errors","taskUUID":"24cd5dff-cb81-4db5-8506-b72a9425f9d1"}]}"#;
        assert!(matches!(RunwareTasks.next(&poll_step(&call, 0, 1, Some(&job)), &json_response(200, failed)), Err(ImageEditError::ProviderFailed { .. })));
        let key = r#"{"errors":[{"code":"invalidApiKey","message":"Invalid API key. Get one at https://runware.ai/signup","parameter":"apiKey","taskType":"authentication"}]}"#;
        assert!(matches!(RunwareTasks.next(&submit_step(&call), &json_response(401, key)), Err(ImageEditError::KeyRejected)));
        let bad = r#"{"errors":[{"code":"invalidWidth","message":"Invalid value for width parameter.","parameter":"width","taskType":"imageInference"}]}"#;
        assert!(matches!(RunwareTasks.next(&submit_step(&call), &json_response(400, bad)), Err(ImageEditError::ProviderRejected { status: 400, message }) if message == "Invalid value for width parameter."));
        assert!(matches!(RunwareTasks.next(&submit_step(&call), &json_response(402, r#"{"errors":[{"code":"insufficientCredits","message":"Insufficient credits."}]}"#)), Err(ImageEditError::OutOfCredits)));
        assert!(RunwareTasks.cancel_request(&submit_step(&call)).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_follows_a_processing_task() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = rw_call("bytedance:seedream@5.0-pro", None, SizeParamStyle::WidthHeight);
        let answers = vec![
            json_response(200, r#"{"data":[{"taskType":"imageInference","taskUUID":"u1","status":"processing"}]}"#),
            json_response(200, r#"{"data":[{"taskType":"imageInference","taskUUID":"u1","status":"processing","progress":50}]}"#),
            json_response(200, r#"{"data":[{"taskType":"imageInference","taskUUID":"u1","status":"success","imageUUID":"i1","imageURL":"https://im.runware.ai/image/i1.png"}]}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&RunwareTasks, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Bearer, "secret".to_string()));
        assert_eq!(sent.iter().map(|sent| sent.method).collect::<Vec<_>>(), [HttpMethod::Post, HttpMethod::Post, HttpMethod::Post, HttpMethod::Get]);
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(trail, [("https://api.runware.ai/v1", key.clone()), ("https://api.runware.ai/v1", key.clone()), ("https://api.runware.ai/v1", key), ("https://im.runware.ai/image/i1.png", None)]);
    }
}
