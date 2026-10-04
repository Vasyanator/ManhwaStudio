/*
File: crates/ms-ai-api/src/image_edit/adapters/ideogram_edit.rs

Purpose:
Adapter of Ideogram precise edit (`POST {base}/v2/image/precise-edit/{model}` multipart,
`Api-Key` header) in its asynchronous mode: submit with `async=true`, poll
`GET {base}/v2/generations/{generation_id}` until `completed`, download `data[0].url`.

Key structures:
- IdeogramEdit (the `EditProtocol`)

Notes:
Form fields: `prompt`, `image` (the PNG), an optional `mask` ("Black marks the area to edit and
white the area to keep ... must contain both black and white areas", so an all-editable mask is
not sent: it equals no mask), `async`. The output "always matches this image's width and
height", so no size field exists (only `SizeParamStyle::None`). A synchronous answer (`data`
present) is accepted too. Statuses `pending` poll on, `completed` -> `data[0]`
(`is_image_safe: false` = `Moderated`), `failed` -> `failure_reason`
(`content_policy_violation` = `Moderated`). HTTP 422 on submit is the safety check ("The prompt
did not pass safety checks"); 402 / 429 carry a `reject_reason`. Result URLs are signed
`ideogram.ai/api/images/ephemeral/...` links, downloaded without the key. No cancel endpoint.
Sources: https://developer.ideogram.ai/api-reference/images/precise-edit/ideogram-4-5.md and
https://developer.ideogram.ai/api-reference/generations/get-generation.md, fetched 2026-10-04.
*/

use std::time::Duration;

use serde_json::Value;

use super::{classify_error, get_request, is_success, json_value, job_failure, mask_has_both_regions, required_str, result_url_step};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
use crate::image_edit::error::ImageEditError;
use crate::image_edit::multipart::MultipartForm;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx};

/// The API key header.
const AUTH: AuthScheme = AuthScheme::Header("Api-Key");

/// The Ideogram precise-edit adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct IdeogramEdit;

impl EditProtocol for IdeogramEdit {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        if call.size_param != SizeParamStyle::None {
            return Err(ImageEditError::RequestBuild { detail: format!("Ideogram precise edit keeps the input size; it cannot state {:?}", call.size_param) });
        }
        let mut form = MultipartForm::new().text("prompt", &call.prompt).file("image", "image.png", "image/png", call.image_png.clone());
        if let Some(mask) = call.mask.as_deref().filter(|mask| mask_has_both_regions(mask)) {
            form = form.file("mask", "mask.png", "image/png", encode_mask_png(mask, call.width, call.height, MaskPolarity::BlackEdits)?);
        }
        form = form.text("async", "true");
        Ok(HttpRequestSpec { method: HttpMethod::Post, url: format!("{}/v2/image/precise-edit/{}", call.base_url, call.model_id), headers: Vec::new(), body: HttpBody::Multipart(form.finish()?), auth: Some(AUTH) })
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            // The precise-edit reference: 422 = "The prompt did not pass safety checks".
            return Err(if response.status == 422 { ImageEditError::Moderated } else { classify_error(response.status, &response.body) });
        }
        let value = json_value(response)?;
        if value.get("data").is_some_and(|data| !data.is_null()) {
            return image_step(&value);
        }
        match value.get("status").and_then(Value::as_str) {
            // The submit acknowledgement has no status; a poll answer says `pending`.
            None | Some("pending") => {
                let id = required_str(&value, "/generation_id")?.to_string();
                let url = format!("{}/v2/generations/{id}", step.call.base_url);
                Ok(NextStep::Poll { request: get_request(url, Vec::new(), Some(AUTH)), after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: None, cancel_url: None }) })
            }
            Some("failed") => {
                let reason = value.get("failure_reason").and_then(Value::as_str).unwrap_or("failed");
                Err(job_failure(&format!("Ideogram: {reason}")))
            }
            Some("completed") => Err(ImageEditError::NoImageReturned { reason: "the completed generation has no data".to_string() }),
            Some(other) => Err(ImageEditError::ProviderFailed { detail: format!("Ideogram generation status {other}") }),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

/// The final step from a `data` list: the first image, unless the safety check withheld it.
fn image_step(value: &Value) -> Result<NextStep, ImageEditError> {
    let first = value.pointer("/data/0").ok_or_else(|| ImageEditError::NoImageReturned { reason: "the answer has an empty data list".to_string() })?;
    if first.get("is_image_safe").and_then(Value::as_bool) == Some(false) {
        return Err(ImageEditError::Moderated);
    }
    let url = first.get("url").and_then(Value::as_str).filter(|url| !url.is_empty()).ok_or_else(|| ImageEditError::NoImageReturned { reason: "data[0] has no url".to_string() })?;
    result_url_step(url)
}

#[cfg(test)]
mod tests {
    use super::IdeogramEdit;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::multipart::MultipartForm;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn ideo_call(mask: Option<Vec<u8>>) -> EditCall {
        call(ImageEditProvider::Ideogram, "ideogram-4-5", "https://api.ideogram.ai", mask, (2, 2), SizeParamStyle::None, None)
    }

    // The reference's form (`prompt`, `image`, `mask` files, `async`) and `Api-Key` header.
    #[test]
    fn request_is_the_documented_form() {
        let mask = vec![0, 255, 255, 0];
        let spec = IdeogramEdit.submit(&ideo_call(Some(mask.clone()))).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api.ideogram.ai/v2/image/precise-edit/ideogram-4-5", Some(AuthScheme::Header("Api-Key"))));
        let black = encode_mask_png(&mask, 2, 2, MaskPolarity::BlackEdits).unwrap_or_default();
        let expected = MultipartForm::new().text("prompt", "Remove the speech bubble text").file("image", "image.png", "image/png", b"PNGDATA".to_vec()).file("mask", "mask.png", "image/png", black).text("async", "true").finish().ok();
        assert_eq!(Some(spec.body), expected.map(HttpBody::Multipart));
        // An all-editable mask is not a valid Ideogram mask (both regions required): omitted.
        let full = IdeogramEdit.submit(&ideo_call(Some(vec![255; 4]))).ok().map(|spec| spec.body);
        let no_mask = MultipartForm::new().text("prompt", "Remove the speech bubble text").file("image", "image.png", "image/png", b"PNGDATA".to_vec()).text("async", "true").finish().ok();
        assert_eq!(full, no_mask.map(HttpBody::Multipart));
        let sized = call(ImageEditProvider::Ideogram, "ideogram-4-5", "https://api.ideogram.ai", None, (2, 2), SizeParamStyle::WxH, None);
        assert!(matches!(IdeogramEdit.submit(&sized), Err(ImageEditError::RequestBuild { .. })));
    }

    // The poll reference's example answer and documented statuses / error statuses.
    #[test]
    fn status_machine() {
        let call = ideo_call(None);
        let ack = r#"{"generation_id":"bzue-VZtSlSMAneIbfCo2A"}"#;
        assert!(matches!(IdeogramEdit.next(&submit_step(&call), &json_response(200, ack)), Ok(NextStep::Poll { request, job: Some(job), .. }) if request.url == "https://api.ideogram.ai/v2/generations/bzue-VZtSlSMAneIbfCo2A" && job.id == "bzue-VZtSlSMAneIbfCo2A"));
        let job = JobRef { id: "bzue-VZtSlSMAneIbfCo2A".to_string(), poll_url: None, cancel_url: None };
        let step = poll_step(&call, 0, 1, Some(&job));
        let pending = r#"{"generation_id":"bzue-VZtSlSMAneIbfCo2A","status":"pending","created":"2024-01-15T09:30:00Z"}"#;
        assert!(matches!(IdeogramEdit.next(&step, &json_response(200, pending)), Ok(NextStep::Poll { .. })));
        let completed = r#"{"generation_id":"bzue-VZtSlSMAneIbfCo2A","status":"completed","created":"2024-01-15T09:30:00Z","response_type":"url","data":[{"is_image_safe":true,"prompt":"prompt","resolution":"2x2","seed":12345,"url":"https://ideogram.ai/api/images/ephemeral/xtdZiqPwRxqY1Y7NExFmzB.png?exp=1743867804&sig=e13e"}]}"#;
        assert!(matches!(IdeogramEdit.next(&step, &json_response(200, completed)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.starts_with("https://ideogram.ai/api/images/ephemeral/")));
        let unsafe_image = r#"{"generation_id":"g","status":"completed","response_type":"url","data":[{"is_image_safe":false,"prompt":"p","resolution":"2x2","seed":1,"url":""}]}"#;
        assert!(matches!(IdeogramEdit.next(&step, &json_response(200, unsafe_image)), Err(ImageEditError::Moderated)));
        let refused = r#"{"generation_id":"g","status":"failed","created":"2024-01-15T09:30:00Z","failure_reason":"content_policy_violation"}"#;
        assert!(matches!(IdeogramEdit.next(&step, &json_response(200, refused)), Err(ImageEditError::Moderated)));
        assert!(matches!(IdeogramEdit.next(&submit_step(&call), &json_response(422, r#"{"detail":"Prompt failed safety check"}"#)), Err(ImageEditError::Moderated)));
        assert!(matches!(IdeogramEdit.next(&submit_step(&call), &json_response(402, r#"{"error":"Insufficient credits.","reject_reason":"insufficient_funds"}"#)), Err(ImageEditError::OutOfCredits)));
        assert!(matches!(IdeogramEdit.next(&submit_step(&call), &json_response(429, r#"{"error":"Too many in-flight requests.","reject_reason":"inflight_limit","max_inflight_requests":10}"#)), Err(ImageEditError::RateLimited)));
        // The synchronous answer form (`data` present) is accepted as well.
        let sync = r#"{"data":[{"prompt":"p","resolution":"2x2","is_image_safe":true,"seed":1,"url":"https://ideogram.ai/api/images/ephemeral/s.png?sig=x"}],"seed":1}"#;
        assert!(matches!(IdeogramEdit.next(&submit_step(&call), &json_response(200, sync)), Ok(NextStep::Download(_))));
        assert!(IdeogramEdit.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_and_downloads() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = ideo_call(Some(vec![0, 255, 255, 0]));
        let answers = vec![
            json_response(200, r#"{"generation_id":"g1"}"#),
            json_response(200, r#"{"generation_id":"g1","status":"pending","created":"2026-10-04T09:30:00Z"}"#),
            json_response(200, r#"{"generation_id":"g1","status":"completed","response_type":"url","data":[{"is_image_safe":true,"prompt":"p","resolution":"2x2","seed":1,"url":"https://ideogram.ai/api/images/ephemeral/g1.png?sig=x"}]}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&IdeogramEdit, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Header("Api-Key"), "secret".to_string()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(
            trail,
            [
                ("https://api.ideogram.ai/v2/image/precise-edit/ideogram-4-5", key.clone()),
                ("https://api.ideogram.ai/v2/generations/g1", key.clone()),
                ("https://api.ideogram.ai/v2/generations/g1", key),
                ("https://ideogram.ai/api/images/ephemeral/g1.png?sig=x", None),
            ]
        );
    }
}
