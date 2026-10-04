/*
File: crates/ms-ai-api/src/image_edit/adapters/genapi_async.rs

Purpose:
Adapter of the GenAPI network API (`POST {base}/networks/{network}`, Bearer): create a task,
poll `GET {base}/request/get/{request_id}` until `success`, download `result[0]`.

Key structures:
- GenApiAsync (the `EditProtocol`)

Notes:
GenAPI's docs pages render their code samples client-side, so the wire shape comes from the
best official sources available: the official PHP SDK (`EndpointsEnum`: `/api/v1/networks`,
`/api/v1/request/get`; Bearer token), the docs' generation lifecycle (statuses `processing`,
`success`, `error`) and the site's own client code (submit answers `{request_id, status}`,
results in `result[]`), plus each model page's parameter list: `image_urls` (a file array;
`qwen-image-edit` takes one `image_url`), `image_size` "WxH" on GPT Image, `aspect_ratio` +
`resolution` on Nano Banana Pro. Images are sent as PNG data URLs inside the JSON body; that
these "file" parameters accept data URLs is UNVERIFIED (a refusal surfaces as the provider's
error). Result files are downloaded without the key. No cancel endpoint.
Sources: https://github.com/GenAPI-ru/genapi-sdk-php (src/Enums/Http/EndpointsEnum.php,
src/Client.php), https://gen-api.ru/docs/generation,
https://gen-api.ru/model/{gpt-image-2,gpt-image-2-5,nano-banana-pro,flux-2,qwen-image-edit}/api,
fetched 2026-10-04.
*/

use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{classify_error, get_request, is_success, json_post, json_value, job_failure, png_data_url, provider_message, result_url_step};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx};

/// The `GenAPI` network adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct GenApiAsync;

impl EditProtocol for GenApiAsync {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        let mut body = Map::new();
        body.insert("prompt".to_string(), json!(call.prompt));
        let image = png_data_url(&call.image_png);
        // The model pages: `qwen-image-edit` has a single `image_url`, the others `image_urls`.
        if call.model_id == "qwen-image-edit" {
            body.insert("image_url".to_string(), json!(image));
        } else {
            body.insert("image_urls".to_string(), json!([image]));
        }
        match call.size_param {
            SizeParamStyle::WxH => {
                body.insert("image_size".to_string(), json!(format!("{}x{}", call.width, call.height)));
            }
            SizeParamStyle::AspectTier(_) => {
                let entry = call.size_entry.ok_or_else(|| ImageEditError::RequestBuild { detail: "a table offer reached the adapter without its table entry".to_string() })?;
                body.insert("aspect_ratio".to_string(), json!(entry.aspect));
                body.insert("resolution".to_string(), json!(entry.tier));
            }
            SizeParamStyle::None => {}
            SizeParamStyle::WStarH | SizeParamStyle::WidthHeight | SizeParamStyle::ImageSizeObject => {
                return Err(ImageEditError::RequestBuild { detail: format!("GenAPI cannot state the size as {:?}", call.size_param) });
            }
        }
        Ok(json_post(format!("{}/networks/{}", call.base_url, call.model_id), Vec::new(), Value::Object(body), AuthScheme::Bearer))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(classify_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        let status = value.get("status").and_then(Value::as_str).ok_or_else(|| ImageEditError::ProviderFailed { detail: "the answer has no status".to_string() })?;
        match status {
            "starting" | "processing" => {
                // The request id is a number in the SDK (`int $requestId`); accept a string too.
                let id = match (value.get("request_id"), step.job) {
                    (Some(Value::Number(number)), _) => number.to_string(),
                    (Some(Value::String(text)), _) => text.clone(),
                    (_, Some(job)) => job.id.clone(),
                    (_, None) => return Err(ImageEditError::ProviderFailed { detail: "the answer has no request_id".to_string() }),
                };
                let url = format!("{}/request/get/{id}", step.call.base_url);
                Ok(NextStep::Poll { request: get_request(url, Vec::new(), Some(AuthScheme::Bearer)), after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: None, cancel_url: None }) })
            }
            "success" => {
                let result = value.get("result");
                let url = result.and_then(|result| result.get(0)).and_then(Value::as_str).or_else(|| result.and_then(Value::as_str));
                let url = url.ok_or_else(|| ImageEditError::NoImageReturned { reason: "the finished request has no result URL".to_string() })?;
                result_url_step(url)
            }
            "error" => Err(job_failure(&provider_message(&value).unwrap_or_else(|| "the request failed".to_string()))),
            other => Err(ImageEditError::ProviderFailed { detail: format!("GenAPI request status {other}") }),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::GenApiAsync;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::size_rule::GEMINI_3_PRO_ENTRIES;

    const IMAGE: &str = "data:image/png;base64,UE5HREFUQQ==";
    const BASE: &str = "https://api.gen-api.ru/api/v1";

    fn gen_call(model: &str, size: (u32, u32), size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::GenApi, model, BASE, None, size, size_param, None)
    }

    // SDK `createNetworkTask`: POST /api/v1/networks/{networkId} with the model page's parameters.
    #[test]
    fn requests() {
        let spec = GenApiAsync.submit(&gen_call("gpt-image-2", (1024, 768), SizeParamStyle::WxH)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api.gen-api.ru/api/v1/networks/gpt-image-2", Some(AuthScheme::Bearer)));
        assert_eq!(spec.body, HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_urls": [IMAGE], "image_size": "1024x768" })));
        let entry = GEMINI_3_PRO_ENTRIES.iter().find(|entry| (entry.aspect, entry.tier) == ("1:1", "2K")).copied();
        let banana = call(ImageEditProvider::GenApi, "nano-banana-pro", BASE, None, (2048, 2048), SizeParamStyle::AspectTier(&GEMINI_3_PRO_ENTRIES), entry);
        assert_eq!(GenApiAsync.submit(&banana).ok().map(|spec| spec.body), Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_urls": [IMAGE], "aspect_ratio": "1:1", "resolution": "2K" }))));
        let qwen = GenApiAsync.submit(&gen_call("qwen-image-edit", (2, 2), SizeParamStyle::None)).ok().map(|spec| spec.body);
        assert_eq!(qwen, Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "image_url": IMAGE }))));
        assert!(matches!(GenApiAsync.submit(&gen_call("flux-2", (2, 2), SizeParamStyle::WidthHeight)), Err(ImageEditError::RequestBuild { .. })));
    }

    // Lifecycle statuses (gen-api.ru/docs/generation) in the shape the site's client reads.
    #[test]
    fn status_machine() {
        let call = gen_call("flux-2", (2, 2), SizeParamStyle::None);
        let started = r#"{"request_id":12345678,"model":"flux-2","status":"starting"}"#;
        assert!(matches!(GenApiAsync.next(&submit_step(&call), &json_response(200, started)), Ok(NextStep::Poll { request, job: Some(job), .. }) if request.url == "https://api.gen-api.ru/api/v1/request/get/12345678" && job.id == "12345678"));
        let job = JobRef { id: "12345678".to_string(), poll_url: None, cancel_url: None };
        let step = poll_step(&call, 0, 1, Some(&job));
        assert!(matches!(GenApiAsync.next(&step, &json_response(200, r#"{"id":12345678,"status":"processing","progress":30}"#)), Ok(NextStep::Poll { .. })));
        let success = r#"{"id":12345678,"status":"success","response_type":"image","result":["https://api.gen-api.ru/storage/requests/12345678/out.png"]}"#;
        assert!(matches!(GenApiAsync.next(&step, &json_response(200, success)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.ends_with("out.png")));
        assert!(matches!(GenApiAsync.next(&step, &json_response(200, r#"{"id":1,"status":"error","error":"Content moderation: prompt rejected"}"#)), Err(ImageEditError::Moderated)));
        assert!(matches!(GenApiAsync.next(&step, &json_response(200, r#"{"id":1,"status":"error","error":"Internal error"}"#)), Err(ImageEditError::ProviderFailed { .. })));
        assert!(matches!(GenApiAsync.next(&submit_step(&call), &json_response(401, r#"{"message":"Unauthenticated."}"#)), Err(ImageEditError::KeyRejected)));
        assert!(matches!(GenApiAsync.next(&submit_step(&call), &json_response(422, r#"{"errors_validation":{"prompt":["The prompt field is required."]}}"#)), Err(ImageEditError::ProviderRejected { status: 422, .. })));
        assert!(GenApiAsync.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_and_downloads() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = gen_call("gpt-image-2", (1024, 768), SizeParamStyle::WxH);
        let answers = vec![
            json_response(200, r#"{"request_id":7,"model":"gpt-image-2","status":"processing"}"#),
            json_response(200, r#"{"id":7,"status":"processing"}"#),
            json_response(200, r#"{"id":7,"status":"success","result":["https://api.gen-api.ru/storage/r/7.png"]}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&GenApiAsync, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Bearer, "secret".to_string()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(trail, [("https://api.gen-api.ru/api/v1/networks/gpt-image-2", key.clone()), ("https://api.gen-api.ru/api/v1/request/get/7", key.clone()), ("https://api.gen-api.ru/api/v1/request/get/7", key), ("https://api.gen-api.ru/storage/r/7.png", None)]);
    }
}
