/*
File: crates/ms-ai-api/src/image_edit/adapters/runway_tasks.rs

Purpose:
Adapter of the Runway API text-to-image task with a reference image
(`POST {base}/text_to_image`, Bearer, `X-Runway-Version` on every request): start the task,
poll `GET {base}/tasks/{id}` until a terminal status, download `output[0]`; cancel with
`DELETE {base}/tasks/{id}`.

Key structures:
- RunwayTasks (the `EditProtocol`)

Notes:
Body `{model, promptText, ratio, referenceImages: [{uri}]}`; the image is a PNG data URI
("pass the base64 encoded image string as a data URI in the `uri`"), untagged (an untagged
reference guides the whole output). `ratio` is REQUIRED and is the output resolution
`"W:H"` in pixels from the model's fixed list, so the offers' `SizeParamStyle::None` ("keep the
input size") is stated as the sent size itself. The catalogue row's rule is that list
(`size_rule::RUNWAY_GEN4`), so the frame only produces accepted sizes; a size outside it would
be refused by the provider (`ProviderRejected`), never resized here. Statuses `PENDING` / `THROTTLED` /
`RUNNING` poll on, `SUCCEEDED` -> `output[0]` (an ephemeral CloudFront URL, downloaded without
the key), `FAILED` -> `failureCode` (`SAFETY.*` and `INPUT_PREPROCESSING.SAFETY.*` ->
`Moderated`, else `ProviderFailed` with `failure`), `CANCELLED` -> `ProviderFailed`. HTTP 400
carries `{"error": "..."}`.
Sources: https://docs.dev.runwayml.com/ai-context.md (base URL, version header, task lifecycle,
DELETE cancel, data URIs), https://docs.dev.runwayml.com/_llms-txt/core-api.txt (the
`gen4_image` curl sample, task output example), https://docs.dev.runwayml.com/errors/errors.md,
https://docs.dev.runwayml.com/errors/task-failures.md, fetched 2026-10-04.
*/

use std::time::Duration;

use serde_json::{Value, json};

use super::{classify_error, get_request, is_success, json_post, json_value, job_failure, png_data_url, required_str, result_url_step};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx};

/// The API version every request must name.
const VERSION_HEADER: (&str, &str) = ("X-Runway-Version", "2024-11-06");

/// The Runway tasks adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunwayTasks;

/// The version header, as a request header list.
fn version_headers() -> Vec<(&'static str, String)> {
    vec![(VERSION_HEADER.0, VERSION_HEADER.1.to_string())]
}

impl EditProtocol for RunwayTasks {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        if call.size_param != SizeParamStyle::None {
            return Err(ImageEditError::RequestBuild { detail: format!("Runway states the size only as its `ratio` resolution; it cannot state {:?}", call.size_param) });
        }
        let body = json!({
            "model": call.model_id,
            "promptText": call.prompt,
            "ratio": format!("{}:{}", call.width, call.height),
            // The edited image first, then the reference.
            "referenceImages": std::iter::once(&call.image_png).chain(call.reference_png.as_ref()).map(|png| json!({ "uri": png_data_url(png) })).collect::<Vec<Value>>()
        });
        Ok(json_post(format!("{}/text_to_image", call.base_url), version_headers(), body, AuthScheme::Bearer))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(classify_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        // The start answer is `{ "id": ... }` alone; task answers carry a status.
        match value.get("status").and_then(Value::as_str).unwrap_or("PENDING") {
            "PENDING" | "THROTTLED" | "RUNNING" => {
                let id = required_str(&value, "/id")?.to_string();
                let url = format!("{}/tasks/{id}", step.call.base_url);
                Ok(NextStep::Poll { request: get_request(url, version_headers(), Some(AuthScheme::Bearer)), after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: None, cancel_url: None }) })
            }
            "SUCCEEDED" => {
                let url = value.pointer("/output/0").and_then(Value::as_str).ok_or_else(|| ImageEditError::NoImageReturned { reason: "the task succeeded without an output URL".to_string() })?;
                result_url_step(url)
            }
            "FAILED" => {
                let code = value.get("failureCode").and_then(Value::as_str).unwrap_or_default();
                if code.starts_with("SAFETY.") || code.starts_with("INPUT_PREPROCESSING.SAFETY") {
                    return Err(ImageEditError::Moderated);
                }
                let failure = value.get("failure").and_then(Value::as_str).unwrap_or("the task failed");
                Err(job_failure(&format!("{code}: {failure}")))
            }
            "CANCELLED" => Err(ImageEditError::ProviderFailed { detail: "Runway: the task was cancelled".to_string() }),
            other => Err(ImageEditError::ProviderFailed { detail: format!("Runway task status {other}") }),
        }
    }

    fn cancel_request(&self, step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        let job = step.job?;
        Some(HttpRequestSpec { method: HttpMethod::Delete, url: format!("{}/tasks/{}", step.call.base_url, job.id), headers: version_headers(), body: HttpBody::Empty, auth: Some(AuthScheme::Bearer) })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::RunwayTasks;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn runway_call(size: (u32, u32), size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Runway, "gen4_image", "https://api.dev.runwayml.com/v1", None, size, size_param, None)
    }

    // The core API's `gen4_image` curl (`promptText`, `model`, `ratio`, `referenceImages[{uri}]`,
    // Bearer, `X-Runway-Version: 2024-11-06`) with a data-URI reference.
    #[test]
    fn request_matches_the_documented_body() {
        let spec = RunwayTasks.submit(&runway_call((1920, 1080), SizeParamStyle::None)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api.dev.runwayml.com/v1/text_to_image", Some(AuthScheme::Bearer)));
        assert_eq!(spec.headers, [("X-Runway-Version", "2024-11-06".to_string())]);
        assert_eq!(spec.body, HttpBody::Json(json!({ "model": "gen4_image", "promptText": "Remove the speech bubble text", "ratio": "1920:1080", "referenceImages": [{ "uri": "data:image/png;base64,UE5HREFUQQ==" }] })));
        assert!(matches!(RunwayTasks.submit(&runway_call((2, 2), SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // Task lifecycle (ai-context.md), the success example (core API) and failure codes.
    #[test]
    fn status_machine() {
        let call = runway_call((1920, 1080), SizeParamStyle::None);
        let started = r#"{"id":"d2e3d1f4-1b3c-4b5c-8d46-1c1d7ee86892"}"#;
        assert!(matches!(RunwayTasks.next(&submit_step(&call), &json_response(200, started)), Ok(NextStep::Poll { request, .. }) if request.url == "https://api.dev.runwayml.com/v1/tasks/d2e3d1f4-1b3c-4b5c-8d46-1c1d7ee86892" && request.headers == [("X-Runway-Version", "2024-11-06".to_string())]));
        let job = JobRef { id: "d2e3d1f4-1b3c-4b5c-8d46-1c1d7ee86892".to_string(), poll_url: None, cancel_url: None };
        let step = poll_step(&call, 0, 1, Some(&job));
        for status in ["PENDING", "THROTTLED", "RUNNING"] {
            let body = format!(r#"{{"id":"d2e3d1f4-1b3c-4b5c-8d46-1c1d7ee86892","status":"{status}","createdAt":"2024-06-27T19:49:32.335Z"}}"#);
            assert!(matches!(RunwayTasks.next(&step, &json_response(200, &body)), Ok(NextStep::Poll { .. })));
        }
        let succeeded = r#"{"id":"d2e3d1f4-1b3c-4b5c-8d46-1c1d7ee86892","status":"SUCCEEDED","createdAt":"2024-06-27T19:49:32.335Z","output":["https://dnznrvs05pmza.cloudfront.net/output.png?_jwt=abc"]}"#;
        assert!(matches!(RunwayTasks.next(&step, &json_response(200, succeeded)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.starts_with("https://dnznrvs05pmza.cloudfront.net/")));
        let unsafe_input = r#"{"id":"x","status":"FAILED","failure":"Input image was flagged","failureCode":"SAFETY.INPUT.IMAGE"}"#;
        assert!(matches!(RunwayTasks.next(&step, &json_response(200, unsafe_input)), Err(ImageEditError::Moderated)));
        let bad_output = r#"{"id":"x","status":"FAILED","failure":"An unexpected error occurred.","failureCode":"INTERNAL.BAD_OUTPUT.01"}"#;
        assert!(matches!(RunwayTasks.next(&step, &json_response(200, bad_output)), Err(ImageEditError::ProviderFailed { .. })));
        assert!(matches!(RunwayTasks.next(&submit_step(&call), &json_response(400, r#"{"error":"Invalid ratio for gen4_image: 2:2"}"#)), Err(ImageEditError::ProviderRejected { status: 400, message }) if message.starts_with("Invalid ratio")));
        assert!(matches!(RunwayTasks.next(&submit_step(&call), &json_response(401, r#"{"error":"Invalid API key"}"#)), Err(ImageEditError::KeyRejected)));
        // `DELETE /v1/tasks/{id}` cancels an in-progress task.
        assert!(matches!(RunwayTasks.cancel_request(&step), Some(spec) if spec.method == HttpMethod::Delete && spec.url.ends_with("/tasks/d2e3d1f4-1b3c-4b5c-8d46-1c1d7ee86892") && spec.headers.len() == 1));
        assert!(RunwayTasks.cancel_request(&submit_step(&call)).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_and_downloads() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = runway_call((1024, 1024), SizeParamStyle::None);
        let answers = vec![
            json_response(200, r#"{"id":"t1"}"#),
            json_response(200, r#"{"id":"t1","status":"RUNNING","createdAt":"2026-10-04T00:00:00Z"}"#),
            json_response(200, r#"{"id":"t1","status":"SUCCEEDED","createdAt":"2026-10-04T00:00:00Z","output":["https://dnznrvs05pmza.cloudfront.net/t1.png?_jwt=x"]}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&RunwayTasks, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Bearer, "secret".to_string()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(trail, [("https://api.dev.runwayml.com/v1/text_to_image", key.clone()), ("https://api.dev.runwayml.com/v1/tasks/t1", key.clone()), ("https://api.dev.runwayml.com/v1/tasks/t1", key), ("https://dnznrvs05pmza.cloudfront.net/t1.png?_jwt=x", None)]);
    }
}
