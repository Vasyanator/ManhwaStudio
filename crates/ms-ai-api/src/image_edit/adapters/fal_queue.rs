/*
File: crates/ms-ai-api/src/image_edit/adapters/fal_queue.rs

Purpose:
Adapter of the fal.ai queue (`POST https://queue.fal.run/{model}`, `Authorization: Key ...`):
submit, poll the returned `status_url` until `COMPLETED`, fetch the returned `response_url`,
download `images[0].url`; cancel through the returned `cancel_url`.

Key structures:
- FalQueue (the `EditProtocol`)

Notes:
Request inputs per endpoint OpenAPI (fetched 2026-10-04, see `catalog.rs`): `image_urls[]` for
the edit endpoints, a single `image_url` + `mask_url` for `fal-ai/flux-pro/v1/fill`,
`fal-ai/qwen-image-edit/inpaint` and `ideogram/v4.5/edit`; the size as `image_size {width,
height}` or (Nano Banana Pro) `aspect_ratio` + `resolution`; `output_format: "png"` where the
schema has it. Images and masks travel as PNG data URIs (fal-cdn: "Some models also accept
data URIs", so a model refusing them answers with a provider error). Mask polarity per
endpoint: Fill / Qwen inpaint white = edit; Ideogram black = edit ("Black edits, white
preserves", both regions required, otherwise no mask is sent); `openai/gpt-image-2/edit`
documents none, so it gets the `OpenAI` convention (transparent = edit) of its upstream.
Statuses `IN_QUEUE` / `IN_PROGRESS` poll on; `COMPLETED` carries `error` / `error_type` on a
failed request, else the result is fetched (stage 1). Model errors are `{"detail": [{"msg",
"type"}]}` (`content_policy_violation` -> `Moderated`, `no_media_generated` ->
`NoImageReturned`); queue errors `{"detail", "error_type"}`. The queue hands back URLs on its
own host, so the key never leaves `queue.fal.run`; result files are fal CDN links fetched
without the key.
Sources: https://fal.ai/docs/model-apis/model-endpoints/queue.md (submit / status / result /
cancel examples), https://fal.ai/docs/documentation/model-apis/errors.md,
https://fal.ai/docs/documentation/model-apis/request-errors.md,
https://fal.ai/docs/documentation/model-apis/fal-cdn.md (data URIs), fetched 2026-10-04.
*/

use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{classify_error, get_request, is_success, json_post, json_value, job_failure, mask_data_url, mask_has_both_regions, png_data_url, required_str, result_url_step};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::MaskPolarity;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx, StepPhase};

/// `Authorization: Key {key}`.
const AUTH: AuthScheme = AuthScheme::KeyPrefix("Key");
/// Stage label of a `status_url` poll.
const STAGE_STATUS: u8 = 0;
/// Stage label of the `response_url` fetch.
const STAGE_RESULT: u8 = 1;

/// The fal.ai queue adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct FalQueue;

/// How one fal endpoint takes its inputs.
#[derive(Debug, Clone, Copy)]
struct Endpoint {
    /// One `image_url` instead of the `image_urls` list.
    single_image: bool,
    /// The endpoint's mask polarity, `None` for endpoints without a mask input.
    mask: Option<MaskPolarity>,
    /// The mask must contain both regions (Ideogram); an all-editable mask is not sent.
    mask_needs_both: bool,
    /// The schema has `output_format` (png forced).
    png_field: bool,
}

/// The input layout of a catalogue fal endpoint id.
fn endpoint(model_id: &str) -> Option<Endpoint> {
    let edit = Endpoint { single_image: false, mask: None, mask_needs_both: false, png_field: true };
    match model_id {
        "openai/gpt-image-2/edit" => Some(Endpoint { mask: Some(MaskPolarity::TransparentEdits), ..edit }),
        "fal-ai/nano-banana-pro/edit" | "fal-ai/flux-2-pro/edit" | "fal-ai/flux-2-max/edit" | "fal-ai/qwen-image-edit-2511" | "bytedance/seedream/v5/pro/edit" => Some(edit),
        "fal-ai/flux-pro/v1/fill" | "fal-ai/qwen-image-edit/inpaint" => Some(Endpoint { single_image: true, mask: Some(MaskPolarity::WhiteEdits), ..edit }),
        "ideogram/v4.5/edit" => Some(Endpoint { single_image: true, mask: Some(MaskPolarity::BlackEdits), mask_needs_both: true, png_field: false }),
        _ => None,
    }
}

impl EditProtocol for FalQueue {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        let endpoint = endpoint(&call.model_id).ok_or_else(|| ImageEditError::RequestBuild { detail: format!("unknown fal endpoint {}", call.model_id) })?;
        let mut input = Map::new();
        input.insert("prompt".to_string(), json!(call.prompt));
        let image = png_data_url(&call.image_png);
        if endpoint.single_image {
            input.insert("image_url".to_string(), json!(image));
        } else {
            input.insert("image_urls".to_string(), json!([image]));
        }
        if let Some(polarity) = endpoint.mask {
            let send = call.mask.as_deref().is_some_and(|mask| !endpoint.mask_needs_both || mask_has_both_regions(mask));
            if send && let Some(mask) = mask_data_url(call, polarity)? {
                input.insert("mask_url".to_string(), json!(mask));
            }
        }
        match call.size_param {
            SizeParamStyle::ImageSizeObject => {
                input.insert("image_size".to_string(), json!({ "width": call.width, "height": call.height }));
            }
            SizeParamStyle::AspectTier(_) => {
                let entry = call.size_entry.ok_or_else(|| ImageEditError::RequestBuild { detail: "a table offer reached the adapter without its table entry".to_string() })?;
                input.insert("aspect_ratio".to_string(), json!(entry.aspect));
                input.insert("resolution".to_string(), json!(entry.tier));
            }
            SizeParamStyle::None => {}
            SizeParamStyle::WxH | SizeParamStyle::WStarH | SizeParamStyle::WidthHeight => {
                return Err(ImageEditError::RequestBuild { detail: format!("fal cannot state the size as {:?}", call.size_param) });
            }
        }
        if endpoint.png_field {
            input.insert("output_format".to_string(), json!("png"));
        }
        Ok(json_post(format!("{}/{}", call.base_url, call.model_id), Vec::new(), Value::Object(input), AUTH))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(fal_error(response));
        }
        let value = json_value(response)?;
        match step.phase {
            StepPhase::Submit => {
                let id = required_str(&value, "/request_id")?.to_string();
                let status_url = required_str(&value, "/status_url")?.to_string();
                let cancel_url = value.get("cancel_url").and_then(Value::as_str).map(str::to_string);
                Ok(status_poll(status_url.clone(), Some(JobRef { id, poll_url: Some(status_url), cancel_url })))
            }
            StepPhase::Poll { stage: STAGE_STATUS } => match required_str(&value, "/status")? {
                "IN_QUEUE" | "IN_PROGRESS" => {
                    let url = step.job.and_then(|job| job.poll_url.clone()).ok_or_else(|| ImageEditError::RequestBuild { detail: "fal poll without a kept status URL".to_string() })?;
                    Ok(status_poll(url, None))
                }
                "COMPLETED" => {
                    if let Some(error) = value.get("error").and_then(Value::as_str) {
                        let kind = value.get("error_type").and_then(Value::as_str).unwrap_or_default();
                        return Err(if kind == "content_policy_violation" { ImageEditError::Moderated } else { job_failure(&format!("{kind}: {error}")) });
                    }
                    let url = required_str(&value, "/response_url")?.to_string();
                    Ok(NextStep::Poll { request: get_request(url, Vec::new(), Some(AUTH)), after: Duration::ZERO, stage: STAGE_RESULT, job: None })
                }
                other => Err(ImageEditError::ProviderFailed { detail: format!("fal queue status {other}") }),
            },
            StepPhase::Poll { .. } => {
                let url = value.pointer("/images/0/url").and_then(Value::as_str).ok_or_else(|| ImageEditError::NoImageReturned { reason: "the fal result has no images[0].url".to_string() })?;
                result_url_step(url)
            }
        }
    }

    fn cancel_request(&self, step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        let url = step.job.and_then(|job| job.cancel_url.clone())?;
        Some(HttpRequestSpec { method: HttpMethod::Put, url, headers: Vec::new(), body: HttpBody::Empty, auth: Some(AUTH) })
    }
}

/// A `status_url` poll (no delay of its own: the executor's schedule applies).
fn status_poll(url: String, job: Option<JobRef>) -> NextStep {
    NextStep::Poll { request: get_request(url, Vec::new(), Some(AUTH)), after: Duration::ZERO, stage: STAGE_STATUS, job }
}

/// A failing fal answer: a model error `detail[0].type` of `no_media_generated` is
/// `NoImageReturned`; everything else goes through the shared classifier (which reads
/// `content_policy_violation` as `Moderated` and `detail[0].msg` as the message).
fn fal_error(response: &HttpResponse) -> ImageEditError {
    let value: Option<Value> = serde_json::from_slice(&response.body).ok();
    if let Some(detail) = value.as_ref().and_then(|value| value.pointer("/detail/0"))
        && detail.get("type").and_then(Value::as_str) == Some("no_media_generated")
    {
        let reason = detail.get("msg").and_then(Value::as_str).unwrap_or("no_media_generated").to_string();
        return ImageEditError::NoImageReturned { reason };
    }
    classify_error(response.status, &response.body)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::FalQueue;
    use crate::image_edit::adapters::png_data_url;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::size_rule::GEMINI_3_PRO_ENTRIES;

    const IMAGE: &str = "data:image/png;base64,UE5HREFUQQ==";

    fn fal_call(model: &str, mask: Option<Vec<u8>>, size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Fal, model, "https://queue.fal.run", mask, (2, 2), size_param, None)
    }

    fn body(call: &EditCall) -> Option<HttpBody> {
        FalQueue.submit(call).ok().map(|spec| spec.body)
    }

    fn mask_url(mask: &[u8], polarity: MaskPolarity) -> String {
        png_data_url(&encode_mask_png(mask, 2, 2, polarity).unwrap_or_default())
    }

    // The queue submit sample `curl -X POST https://queue.fal.run/fal-ai/flux/schnell -H
    // "Authorization: Key $FAL_KEY" -d '{"prompt": ...}'` (queue.md) with each endpoint's
    // OpenAPI input fields.
    #[test]
    fn edit_endpoint_bodies() {
        let spec = FalQueue.submit(&fal_call("fal-ai/flux-2-pro/edit", None, SizeParamStyle::ImageSizeObject)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://queue.fal.run/fal-ai/flux-2-pro/edit", Some(AuthScheme::KeyPrefix("Key"))));
        assert_eq!(spec.body, HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_urls": [IMAGE], "image_size": { "width": 2, "height": 2 }, "output_format": "png" })));
        let entry = GEMINI_3_PRO_ENTRIES.iter().find(|entry| entry.tier == "2K" && entry.aspect == "3:4").copied();
        let banana = call(ImageEditProvider::Fal, "fal-ai/nano-banana-pro/edit", "https://queue.fal.run", None, (1792, 2400), SizeParamStyle::AspectTier(&GEMINI_3_PRO_ENTRIES), entry);
        assert_eq!(body(&banana), Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_urls": [IMAGE], "aspect_ratio": "3:4", "resolution": "2K", "output_format": "png" }))));
        assert!(matches!(FalQueue.submit(&fal_call("fal-ai/flux-2-pro/edit", None, SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
        assert!(matches!(FalQueue.submit(&fal_call("fal-ai/unknown", None, SizeParamStyle::None)), Err(ImageEditError::RequestBuild { .. })));
    }

    // Mask polarity per endpoint schema: Fill "mask URL to inpaint" (white), Ideogram "Black
    // edits, white preserves. Must contain both regions", GPT Image 2 (OpenAI alpha).
    #[test]
    fn mask_endpoints() {
        let mask = vec![0, 255, 255, 0];
        let fill = fal_call("fal-ai/flux-pro/v1/fill", Some(mask.clone()), SizeParamStyle::None);
        assert_eq!(body(&fill), Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_url": IMAGE, "mask_url": mask_url(&mask, MaskPolarity::WhiteEdits), "output_format": "png" }))));
        let ideogram = fal_call("ideogram/v4.5/edit", Some(mask.clone()), SizeParamStyle::None);
        assert_eq!(body(&ideogram), Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_url": IMAGE, "mask_url": mask_url(&mask, MaskPolarity::BlackEdits) }))));
        let all_edit = fal_call("ideogram/v4.5/edit", Some(vec![255; 4]), SizeParamStyle::None);
        assert_eq!(body(&all_edit), Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_url": IMAGE }))));
        let gpt = fal_call("openai/gpt-image-2/edit", Some(mask.clone()), SizeParamStyle::ImageSizeObject);
        assert_eq!(body(&gpt), Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_urls": [IMAGE], "mask_url": mask_url(&mask, MaskPolarity::TransparentEdits), "image_size": { "width": 2, "height": 2 }, "output_format": "png" }))));
    }

    // The documented submit / status / result answers (queue.md) and error bodies (errors.md).
    #[test]
    fn status_machine_and_errors() {
        let call = fal_call("fal-ai/flux-2-pro/edit", None, SizeParamStyle::ImageSizeObject);
        let submit = r#"{"request_id":"764cabcf-b745-4b3e-ae38-1200304cf45b","response_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/764cabcf/response","status_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/764cabcf/status","cancel_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/764cabcf/cancel","queue_position":0}"#;
        let job = JobRef {
            id: "764cabcf-b745-4b3e-ae38-1200304cf45b".to_string(),
            poll_url: Some("https://queue.fal.run/fal-ai/flux-2-pro/requests/764cabcf/status".to_string()),
            cancel_url: Some("https://queue.fal.run/fal-ai/flux-2-pro/requests/764cabcf/cancel".to_string()),
        };
        assert!(matches!(FalQueue.next(&submit_step(&call), &json_response(200, submit)), Ok(NextStep::Poll { stage: 0, job: Some(kept), .. }) if kept == job));
        let status = poll_step(&call, 0, 1, Some(&job));
        let queued = r#"{"status":"IN_QUEUE","request_id":"764cabcf-...","queue_position":2,"response_url":"https://queue.fal.run/.../response"}"#;
        assert!(matches!(FalQueue.next(&status, &json_response(200, queued)), Ok(NextStep::Poll { stage: 0, request, .. }) if request.url.ends_with("/status")));
        let done = r#"{"status":"COMPLETED","request_id":"764cabcf-...","response_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/764cabcf/response","logs":[{"message":"Done.","timestamp":"2026-02-17T10:30:05.789Z"}],"metrics":{"inference_time":3.42}}"#;
        assert!(matches!(FalQueue.next(&status, &json_response(200, done)), Ok(NextStep::Poll { stage: 1, request, .. }) if request.url.ends_with("/response") && request.auth == Some(AuthScheme::KeyPrefix("Key"))));
        let failed = r#"{"status":"COMPLETED","request_id":"x","response_url":"https://queue.fal.run/x","error":"Request timed out","error_type":"request_timeout"}"#;
        assert!(matches!(FalQueue.next(&status, &json_response(200, failed)), Err(ImageEditError::ProviderFailed { .. })));
        let result = poll_step(&call, 1, 3, Some(&job));
        let images = r#"{"images":[{"url":"https://v3.fal.media/files/rabbit/abc123.png","width":2,"height":2,"content_type":"image/png"}],"seed":42}"#;
        assert!(matches!(FalQueue.next(&result, &json_response(200, images)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url == "https://v3.fal.media/files/rabbit/abc123.png"));
        let flagged = r#"{"detail":[{"loc":["body","prompt"],"msg":"The content could not be processed because it contained material flagged by a content checker.","type":"content_policy_violation","url":"https://docs.fal.ai/errors#content_policy_violation","input":"x"}]}"#;
        assert!(matches!(FalQueue.next(&result, &json_response(422, flagged)), Err(ImageEditError::Moderated)));
        let nothing = r#"{"detail":[{"loc":["body"],"msg":"The model did not generate the expected output for this prompt.","type":"no_media_generated","url":"https://docs.fal.ai/errors#no_media_generated"}]}"#;
        assert!(matches!(FalQueue.next(&result, &json_response(422, nothing)), Err(ImageEditError::NoImageReturned { .. })));
        let invalid = r#"{"detail":[{"loc":["body","image_size"],"msg":"Image too large","type":"image_too_large","url":"https://docs.fal.ai/errors#image_too_large"}]}"#;
        assert!(matches!(FalQueue.next(&result, &json_response(422, invalid)), Err(ImageEditError::ProviderRejected { status: 422, message }) if message == "Image too large"));
        // The queue's cancel: `curl -X PUT .../requests/{request_id}/cancel -H "Authorization: Key ..."`.
        let cancel = FalQueue.cancel_request(&status);
        assert!(matches!(cancel, Some(spec) if spec.method == HttpMethod::Put && spec.url.ends_with("/cancel") && spec.auth == Some(AuthScheme::KeyPrefix("Key"))));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_fetches_and_downloads() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = fal_call("fal-ai/flux-2-pro/edit", None, SizeParamStyle::ImageSizeObject);
        let answers = vec![
            json_response(200, r#"{"request_id":"r1","status_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/r1/status","response_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/r1","cancel_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/r1/cancel"}"#),
            json_response(200, r#"{"status":"IN_PROGRESS","request_id":"r1","response_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/r1"}"#),
            json_response(200, r#"{"status":"COMPLETED","request_id":"r1","response_url":"https://queue.fal.run/fal-ai/flux-2-pro/requests/r1"}"#),
            json_response(200, r#"{"images":[{"url":"https://v3.fal.media/files/x/out.png"}]}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&FalQueue, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::KeyPrefix("Key"), "secret".to_string()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(
            trail,
            [
                ("https://queue.fal.run/fal-ai/flux-2-pro/edit", key.clone()),
                ("https://queue.fal.run/fal-ai/flux-2-pro/requests/r1/status", key.clone()),
                ("https://queue.fal.run/fal-ai/flux-2-pro/requests/r1/status", key.clone()),
                ("https://queue.fal.run/fal-ai/flux-2-pro/requests/r1", key),
                ("https://v3.fal.media/files/x/out.png", None),
            ]
        );
    }
}
