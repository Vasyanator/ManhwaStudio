/*
File: crates/ms-ai-api/src/image_edit/adapters/kling_image.rs

Purpose:
Adapter of the Kling Omni image task API (`POST {base}/v1/images/omni-image`, Bearer API key):
create a task with the reference image, poll `GET {base}/v1/images/omni-image/{task_id}` until
`succeed` / `failed`, download `task_result.images[0].url`.

Key structures:
- KlingImage (the `EditProtocol`)

Notes:
Authentication is the plain API key ("API Key (for all models)", `Authorization: Bearer
<API_KEY>`); the Access Key / Secret Key JWT is only the legacy scheme, so no request signing is
needed. Body `{model_name, prompt, image_list: [{image: plain base64}], n: 1}`: "When using
Base64, do NOT add any prefix like `data:image/png;base64,`". `resolution` (1k / 2k tier) and
`aspect_ratio` (default `auto`) are the only size controls and have no pixel table, so none is
stated (`SizeParamStyle::None`); the pipeline checks the returned size. Every answer is
`{code, message, request_id, data}`; `code` 0 is success, else (HTTP 400 / 401 / 403 / 429 /
5xx): 1000-1004 key, 1101 / 1102 arrears or exhausted package -> `OutOfCredits`, 1300 / 1301
platform / content-security policy -> `Moderated`, 1302 / 1303 -> `RateLimited`, the rest
`ProviderRejected` (4xx) or `ProviderFailed`. Task states `submitted` / `processing` poll on,
`succeed` -> the first image URL (signed, downloaded without the key), `failed` ->
`task_status_msg`. No cancel endpoint.
Sources: https://kling.ai/document-api/api/image/o1/image-generation.md,
https://kling.ai/document-api/api/get-started/authentication.md,
https://kling.ai/document-api/api/get-started/error-codes.md,
https://kling.ai/document-api/api/image/2-1/image-generation.md (Base64 note), fetched
2026-10-04 (index: https://kling.ai/document-api/llms.txt).
*/

use std::time::Duration;

use serde_json::{Value, json};

use super::{classify_error, get_request, is_success, job_failure, json_post, json_value, provider_message, required_str, result_url_step};
use crate::encoding::base64_encode;
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx};

/// The task path under the API host.
const TASK_PATH: &str = "/v1/images/omni-image";

/// The Kling Omni image adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct KlingImage;

/// The typed error of an answer whose service `code` is not 0 (or whose HTTP status failed).
fn kling_error(status: u16, value: Option<&Value>, body: &[u8]) -> ImageEditError {
    let message = value.and_then(provider_message).unwrap_or_default();
    match value.and_then(|value| value.get("code")).and_then(Value::as_i64) {
        Some(1000..=1004) => ImageEditError::KeyRejected,
        Some(1101 | 1102) => ImageEditError::OutOfCredits,
        Some(1300 | 1301) => ImageEditError::Moderated,
        Some(1302 | 1303) => ImageEditError::RateLimited,
        Some(code) if (400..500).contains(&status) => ImageEditError::ProviderRejected { status, message: format!("{code}: {message}") },
        Some(code) if is_success(status) => ImageEditError::ProviderFailed { detail: format!("Kling code {code}: {message}") },
        Some(_) | None => classify_error(status, body),
    }
}

impl EditProtocol for KlingImage {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        if call.size_param != SizeParamStyle::None {
            return Err(ImageEditError::RequestBuild { detail: format!("Kling image tasks have no pixel size field; they cannot state {:?}", call.size_param) });
        }
        let body = json!({
            "model_name": call.model_id,
            "prompt": call.prompt,
            "image_list": [{ "image": base64_encode(&call.image_png) }],
            "n": 1
        });
        Ok(json_post(format!("{}{TASK_PATH}", call.base_url), Vec::new(), body, AuthScheme::Bearer))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        let value = serde_json::from_slice::<Value>(&response.body).ok();
        let code = value.as_ref().and_then(|value| value.get("code")).and_then(Value::as_i64);
        if !is_success(response.status) || code.is_some_and(|code| code != 0) {
            return Err(kling_error(response.status, value.as_ref(), &response.body));
        }
        let value = json_value(response)?;
        match required_str(&value, "/data/task_status")? {
            "submitted" | "processing" => {
                let id = required_str(&value, "/data/task_id")?.to_string();
                let url = format!("{}{TASK_PATH}/{id}", step.call.base_url);
                Ok(NextStep::Poll { request: get_request(url, Vec::new(), Some(AuthScheme::Bearer)), after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: None, cancel_url: None }) })
            }
            "succeed" => {
                let url = value.pointer("/data/task_result/images/0/url").and_then(Value::as_str).ok_or_else(|| ImageEditError::NoImageReturned { reason: "the task succeeded without task_result.images[0].url".to_string() })?;
                result_url_step(url)
            }
            "failed" => Err(job_failure(value.pointer("/data/task_status_msg").and_then(Value::as_str).unwrap_or("the task failed"))),
            other => Err(ImageEditError::ProviderFailed { detail: format!("Kling task status {other}") }),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::KlingImage;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn kling_call(size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Kling, "kling-image-o1", "https://api-singapore.klingai.com", None, (1024, 1024), size_param, None)
    }

    // The create-task curl (`model_name`, `prompt`, `image_list[{image}]`, `n`) with plain base64.
    #[test]
    fn request_matches_the_documented_body() {
        let spec = KlingImage.submit(&kling_call(SizeParamStyle::None)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api-singapore.klingai.com/v1/images/omni-image", Some(AuthScheme::Bearer)));
        assert_eq!(spec.body, HttpBody::Json(json!({ "model_name": "kling-image-o1", "prompt": "Remove the speech bubble text", "image_list": [{ "image": "UE5HREFUQQ==" }], "n": 1 })));
        assert!(matches!(KlingImage.submit(&kling_call(SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // The create / query response examples and the error-code table.
    #[test]
    fn status_machine() {
        let call = kling_call(SizeParamStyle::None);
        let created = r#"{"code":0,"message":"SUCCEED","request_id":"r1","data":{"task_id":"860912345","task_info":{"external_task_id":""},"task_status":"submitted","created_at":1722769557708,"updated_at":1722769557708}}"#;
        assert!(matches!(KlingImage.next(&submit_step(&call), &json_response(200, created)), Ok(NextStep::Poll { request, job: Some(job), .. }) if request.url == "https://api-singapore.klingai.com/v1/images/omni-image/860912345" && request.auth == Some(AuthScheme::Bearer) && job.id == "860912345"));
        let job = JobRef { id: "860912345".to_string(), poll_url: None, cancel_url: None };
        let step = poll_step(&call, 0, 1, Some(&job));
        let processing = r#"{"code":0,"message":"SUCCEED","request_id":"r2","data":{"task_id":"860912345","task_status":"processing","task_status_msg":"","created_at":1722769557708,"updated_at":1722769558708}}"#;
        assert!(matches!(KlingImage.next(&step, &json_response(200, processing)), Ok(NextStep::Poll { .. })));
        let succeed = r#"{"code":0,"message":"SUCCEED","request_id":"r3","data":{"task_id":"860912345","task_status":"succeed","task_status_msg":"","task_result":{"result_type":"single","images":[{"index":0,"url":"https://cdn.klingai.com/bs2/upload-kling-api/x/image.png","watermark_url":"https://cdn.klingai.com/bs2/upload-kling-api/x/image_wm.png"}]},"created_at":1722769557708,"updated_at":1722769559708}}"#;
        assert!(matches!(KlingImage.next(&step, &json_response(200, succeed)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url == "https://cdn.klingai.com/bs2/upload-kling-api/x/image.png"));
        let failed = r#"{"code":0,"message":"SUCCEED","request_id":"r4","data":{"task_id":"860912345","task_status":"failed","task_status_msg":"Internal generation error"}}"#;
        assert!(matches!(KlingImage.next(&step, &json_response(200, failed)), Err(ImageEditError::ProviderFailed { .. })));
        let error = |code: u32, message: &str| format!(r#"{{"code":{code},"message":"{message}","request_id":"r5"}}"#);
        assert!(matches!(KlingImage.next(&submit_step(&call), &json_response(401, &error(1002, "Authorization is invalid"))), Err(ImageEditError::KeyRejected)));
        assert!(matches!(KlingImage.next(&submit_step(&call), &json_response(429, &error(1102, "Resource pack exhausted or expired"))), Err(ImageEditError::OutOfCredits)));
        assert!(matches!(KlingImage.next(&submit_step(&call), &json_response(400, &error(1301, "Trigger the platform's content security policy"))), Err(ImageEditError::Moderated)));
        assert!(matches!(KlingImage.next(&submit_step(&call), &json_response(429, &error(1302, "Too many requests; rate limit exceeded"))), Err(ImageEditError::RateLimited)));
        assert!(matches!(KlingImage.next(&submit_step(&call), &json_response(400, &error(1201, "Invalid parameters"))), Err(ImageEditError::ProviderRejected { status: 400, message }) if message == "1201: Invalid parameters"));
        assert!(matches!(KlingImage.next(&submit_step(&call), &json_response(500, &error(5000, "Server internal error"))), Err(ImageEditError::ProviderFailed { .. })));
        assert!(KlingImage.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_and_downloads() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = kling_call(SizeParamStyle::None);
        let answers = vec![
            json_response(200, r#"{"code":0,"message":"SUCCEED","request_id":"a","data":{"task_id":"t1","task_status":"submitted"}}"#),
            json_response(200, r#"{"code":0,"message":"SUCCEED","request_id":"b","data":{"task_id":"t1","task_status":"processing"}}"#),
            json_response(200, r#"{"code":0,"message":"SUCCEED","request_id":"c","data":{"task_id":"t1","task_status":"succeed","task_result":{"images":[{"index":0,"url":"https://cdn.klingai.com/t1.png"}]}}}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&KlingImage, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Bearer, "secret".to_string()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(trail, [("https://api-singapore.klingai.com/v1/images/omni-image", key.clone()), ("https://api-singapore.klingai.com/v1/images/omni-image/t1", key.clone()), ("https://api-singapore.klingai.com/v1/images/omni-image/t1", key), ("https://cdn.klingai.com/t1.png", None)]);
    }
}
