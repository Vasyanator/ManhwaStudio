/*
File: crates/ms-ai-api/src/image_edit/adapters/luma_generations.rs

Purpose:
Adapter of the Luma Agents API image editing (`POST {base}/generations` with
`type: "image_edit"`, Bearer): create a generation, poll `GET {base}/generations/{id}` until
`completed`, download `output[0].url`.

Key structures:
- LumaGenerations (the `EditProtocol`)

Notes:
Body `{type: "image_edit", model, prompt, source: {data, media_type: "image/png"},
output_format: "png"}`: inline base64 is one of the four documented `source` forms. "Edit
output dimensions are derived from the source image", so no size field exists (only
`SizeParamStyle::None`; `aspect_ratio` is ignored on edits). States `queued` / `processing`
poll on, `completed` -> `output[0].url` (presigned, 1 h; downloaded without the key), `failed`
-> `failure_code` (`content_moderated` -> `Moderated`, `budget_exhausted` -> `OutOfCredits`,
`rate_limited` -> `RateLimited`, else `ProviderFailed` with `failure_reason`). Synchronous
errors are HTTP statuses (400 / 401 / 402 / 403 / 413 / 422 / 429 / 502 / 503). The legacy
Dream Machine API (`api.lumalabs.ai/dream-machine/v1`) is not used: it takes images only as
hosted URLs and offers no Uni model. No cancel endpoint (DELETE deletes a generation).
Sources: https://docs.agents.lumalabs.ai/guides/images/editing/,
https://docs.agents.lumalabs.ai/api/resources/generations/methods/create/ and .../get/,
https://docs.agents.lumalabs.ai/guides/error-handling/, https://docs.agents.lumalabs.ai/guides/model/,
https://lumalabs.ai/llm-info (points to the Agents API), fetched 2026-10-04.
*/

use std::time::Duration;

use serde_json::{Value, json};

use super::{classify_error, get_request, is_success, json_post, json_value, job_failure, refuse_reference, required_str, result_url_step};
use crate::encoding::base64_encode;
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx};

/// The Luma Agents generations adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct LumaGenerations;

impl EditProtocol for LumaGenerations {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        // One image field: a reference cannot be expressed.
        refuse_reference(call)?;
        if call.size_param != SizeParamStyle::None {
            return Err(ImageEditError::RequestBuild { detail: format!("Luma image edits take the source size; they cannot state {:?}", call.size_param) });
        }
        let body = json!({
            "type": "image_edit",
            "model": call.model_id,
            "prompt": call.prompt,
            "source": { "data": base64_encode(&call.image_png), "media_type": "image/png" },
            "output_format": "png"
        });
        Ok(json_post(format!("{}/generations", call.base_url), Vec::new(), body, AuthScheme::Bearer))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(classify_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        match required_str(&value, "/state")? {
            "queued" | "processing" => {
                let id = required_str(&value, "/id")?.to_string();
                let url = format!("{}/generations/{id}", step.call.base_url);
                Ok(NextStep::Poll { request: get_request(url, Vec::new(), Some(AuthScheme::Bearer)), after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: None, cancel_url: None }) })
            }
            "completed" => {
                let url = value.pointer("/output/0/url").and_then(Value::as_str).ok_or_else(|| ImageEditError::NoImageReturned { reason: "the completed generation has no output[0].url".to_string() })?;
                result_url_step(url)
            }
            "failed" => {
                let reason = value.get("failure_reason").and_then(Value::as_str).unwrap_or("the generation failed");
                Err(match value.get("failure_code").and_then(Value::as_str) {
                    Some("content_moderated") => ImageEditError::Moderated,
                    Some("budget_exhausted") => ImageEditError::OutOfCredits,
                    Some("rate_limited") => ImageEditError::RateLimited,
                    Some(code) => job_failure(&format!("{code}: {reason}")),
                    None => job_failure(reason),
                })
            }
            other => Err(ImageEditError::ProviderFailed { detail: format!("Luma generation state {other}") }),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::LumaGenerations;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn luma_call(size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Luma, "uni-1-max", "https://agents.lumalabs.ai/v1", None, (2, 2), size_param, None)
    }

    // The editing guide's curl (`type: "image_edit"`, `source`) with the base64 `source` form
    // `{"data": ..., "media_type": ...}` and the create reference's `output_format`.
    #[test]
    fn request_matches_the_documented_body() {
        let spec = LumaGenerations.submit(&luma_call(SizeParamStyle::None)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://agents.lumalabs.ai/v1/generations", Some(AuthScheme::Bearer)));
        assert_eq!(spec.body, HttpBody::Json(json!({ "type": "image_edit", "model": "uni-1-max", "prompt": "Remove the speech bubble text", "source": { "data": "UE5HREFUQQ==", "media_type": "image/png" }, "output_format": "png" })));
        assert!(matches!(LumaGenerations.submit(&luma_call(SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // The editing guide's response example and the get reference's states / failure codes.
    #[test]
    fn status_machine() {
        let call = luma_call(SizeParamStyle::None);
        let queued = r#"{"id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","type":"image_edit","state":"queued","model":"uni-1-max","created_at":"2026-04-08T12:00:00Z","output":null,"failure_reason":null,"failure_code":null}"#;
        assert!(matches!(LumaGenerations.next(&submit_step(&call), &json_response(201, queued)), Ok(NextStep::Poll { request, .. }) if request.url == "https://agents.lumalabs.ai/v1/generations/a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        let job = JobRef { id: "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string(), poll_url: None, cancel_url: None };
        let step = poll_step(&call, 0, 1, Some(&job));
        let completed = r#"{"id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","type":"image_edit","state":"completed","model":"uni-1","created_at":"2026-04-08T12:00:00Z","output":[{"type":"image","url":"https://storage.example.com/generations/a1b2c3d4/output.png?X-Amz-Expires=3600&X-Amz-Signature=abc"}],"failure_reason":null,"failure_code":null}"#;
        assert!(matches!(LumaGenerations.next(&step, &json_response(200, completed)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.starts_with("https://storage.example.com/")));
        let moderated = r#"{"id":"x","type":"image_edit","state":"failed","model":"uni-1","created_at":"2026-04-08T12:00:00Z","output":null,"failure_reason":"Prompt or input image violated content guidelines","failure_code":"content_moderated"}"#;
        assert!(matches!(LumaGenerations.next(&step, &json_response(200, moderated)), Err(ImageEditError::Moderated)));
        let budget = r#"{"id":"x","state":"failed","failure_reason":"Ran out of funds mid-generation","failure_code":"budget_exhausted"}"#;
        assert!(matches!(LumaGenerations.next(&step, &json_response(200, budget)), Err(ImageEditError::OutOfCredits)));
        let failed = r#"{"id":"x","state":"failed","failure_reason":"Internal model error during generation","failure_code":"generation_failed"}"#;
        assert!(matches!(LumaGenerations.next(&step, &json_response(200, failed)), Err(ImageEditError::ProviderFailed { .. })));
        assert!(matches!(LumaGenerations.next(&submit_step(&call), &json_response(402, r#"{"detail":"Insufficient balance"}"#)), Err(ImageEditError::OutOfCredits)));
        assert!(matches!(LumaGenerations.next(&submit_step(&call), &json_response(400, r#"{"detail":"source: 'media_type' is required with 'data'"}"#)), Err(ImageEditError::ProviderRejected { status: 400, .. })));
        assert!(LumaGenerations.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_and_downloads() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = luma_call(SizeParamStyle::None);
        let answers = vec![
            json_response(201, r#"{"id":"g1","type":"image_edit","state":"queued","model":"uni-1-max"}"#),
            json_response(200, r#"{"id":"g1","type":"image_edit","state":"processing","model":"uni-1-max"}"#),
            json_response(200, r#"{"id":"g1","type":"image_edit","state":"completed","model":"uni-1-max","output":[{"type":"image","url":"https://storage.example.com/g1/output.png?sig=x"}]}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&LumaGenerations, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Bearer, "secret".to_string()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(trail, [("https://agents.lumalabs.ai/v1/generations", key.clone()), ("https://agents.lumalabs.ai/v1/generations/g1", key.clone()), ("https://agents.lumalabs.ai/v1/generations/g1", key), ("https://storage.example.com/g1/output.png?sig=x", None)]);
    }
}
