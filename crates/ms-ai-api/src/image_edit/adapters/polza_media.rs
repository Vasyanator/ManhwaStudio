/*
File: crates/ms-ai-api/src/image_edit/adapters/polza_media.rs

Purpose:
Adapter of the Polza.ai Media API (`POST {base}/v1/media`, Bearer): create an asynchronous
generation, poll `GET {base}/v1/media/{id}` until `completed`, download `data.url`.

Key structures:
- PolzaMedia (the `EditProtocol`)

Notes:
Body `{model, input: {prompt, images: [{type: "base64", data: <data URI>}], aspect_ratio?,
image_resolution?, mask_url?}, async: true}` (`MediaRequestDto` / `ImageInputDto`). The size
is only ever stated as the table labels of an `AspectTier` offer (`aspect_ratio` +
`image_resolution`); `SizeParamStyle::None` sends neither. `qwen/image-2.1` takes `mask_url`
("white = change, black = keep"), sent as a PNG data URI: the catalogue parameter is a URL and
whether a data URI is accepted there is unverified (images accept base64 by contract). Status
`pending` / `processing` poll on (on the executor's 1 -> 3 s schedule; the docs suggest 3-5 s),
`completed` -> `data.url` (Polza's own CDN `s3.polza.ai`, downloaded without the key),
`failed` / `cancelled` -> `error {code, message, metadata.raw}`. HTTP errors carry
`{error: {code, message}}` (`INSUFFICIENT_BALANCE` with 402). No cancel endpoint documented.
Sources: https://polza.ai/docs/api-reference/media/create.md and
https://polza.ai/docs/api-reference/media/status.md, the per-model `parameters` of
`GET /api/v1/models`, fetched 2026-10-04.
*/

use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{classify_error, get_request, is_success, json_post, json_value, job_failure, mask_data_url, png_data_url, required_str, result_url_step};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::MaskPolarity;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx};

/// The Polza.ai media adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct PolzaMedia;

impl EditProtocol for PolzaMedia {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        let mut input = Map::new();
        input.insert("prompt".to_string(), json!(call.prompt));
        input.insert("images".to_string(), json!([{ "type": "base64", "data": png_data_url(&call.image_png) }]));
        match call.size_param {
            SizeParamStyle::AspectTier(_) => {
                let entry = call.size_entry.ok_or_else(|| ImageEditError::RequestBuild { detail: "a table offer reached the adapter without its table entry".to_string() })?;
                input.insert("aspect_ratio".to_string(), json!(entry.aspect));
                input.insert("image_resolution".to_string(), json!(entry.tier));
            }
            SizeParamStyle::None => {}
            SizeParamStyle::WxH | SizeParamStyle::WStarH | SizeParamStyle::WidthHeight | SizeParamStyle::ImageSizeObject => {
                return Err(ImageEditError::RequestBuild { detail: format!("Polza media cannot state the size as {:?}", call.size_param) });
            }
        }
        if let Some(mask) = mask_data_url(call, MaskPolarity::WhiteEdits)? {
            input.insert("mask_url".to_string(), json!(mask));
        }
        let body = json!({ "model": call.model_id, "input": Value::Object(input), "async": true });
        Ok(json_post(format!("{}/v1/media", call.base_url), Vec::new(), body, AuthScheme::Bearer))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(classify_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        match required_str(&value, "/status")? {
            "pending" | "processing" => {
                let id = required_str(&value, "/id")?.to_string();
                let url = format!("{}/v1/media/{id}", step.call.base_url);
                Ok(NextStep::Poll { request: get_request(url, Vec::new(), Some(AuthScheme::Bearer)), after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: None, cancel_url: None }) })
            }
            "completed" => {
                let data = value.get("data");
                let url = data.and_then(|data| data.get("url")).or_else(|| data.and_then(|data| data.pointer("/0/url"))).and_then(Value::as_str);
                let url = url.ok_or_else(|| {
                    let content = value.get("content").and_then(Value::as_str).unwrap_or("the completed generation has no data.url");
                    ImageEditError::NoImageReturned { reason: content.to_string() }
                })?;
                result_url_step(url)
            }
            "failed" | "cancelled" => {
                let message = value.pointer("/error/message").and_then(Value::as_str).unwrap_or("the generation failed");
                let raw = value.pointer("/error/metadata/raw").and_then(Value::as_str).unwrap_or_default();
                let code = value.pointer("/error/code").and_then(Value::as_str).unwrap_or_default();
                Err(job_failure(&format!("{code}: {message} {raw}")))
            }
            other => Err(ImageEditError::ProviderFailed { detail: format!("Polza media status {other}") }),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::PolzaMedia;
    use crate::image_edit::adapters::png_data_url;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::size_rule::GEMINI_31_FLASH_ENTRIES;

    const IMAGE: &str = "data:image/png;base64,UE5HREFUQQ==";

    fn pz_call(model: &str, mask: Option<Vec<u8>>) -> EditCall {
        call(ImageEditProvider::Polza, model, "https://polza.ai/api", mask, (2, 2), SizeParamStyle::None, None)
    }

    // The base64 sample `curl -X POST "https://polza.ai/api/v1/media" -H "Authorization: Bearer
    // ..." -d '{"model": ..., "input": {"prompt": ..., "images": [{"type": "base64", "data":
    // "data:image/png;base64,..."}]}}'` (media/create.md) plus `async` and the table labels.
    #[test]
    fn requests() {
        let entry = GEMINI_31_FLASH_ENTRIES.iter().find(|entry| (entry.aspect, entry.tier) == ("4:3", "1K")).copied();
        let flash = call(ImageEditProvider::Polza, "google/gemini-3.1-flash-image", "https://polza.ai/api", None, (1200, 896), SizeParamStyle::AspectTier(&GEMINI_31_FLASH_ENTRIES), entry);
        let spec = PolzaMedia.submit(&flash).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://polza.ai/api/v1/media", Some(AuthScheme::Bearer)));
        assert_eq!(spec.body, HttpBody::Json(json!({ "model": "google/gemini-3.1-flash-image", "input": { "prompt": "Remove the speech bubble text", "images": [{ "type": "base64", "data": IMAGE }], "aspect_ratio": "4:3", "image_resolution": "1K" }, "async": true })));
        let mask = vec![0, 255, 255, 0];
        let white = png_data_url(&encode_mask_png(&mask, 2, 2, MaskPolarity::WhiteEdits).unwrap_or_default());
        let qwen = PolzaMedia.submit(&pz_call("qwen/image-2.1", Some(mask))).ok().map(|spec| spec.body);
        assert_eq!(qwen, Some(HttpBody::Json(json!({ "model": "qwen/image-2.1", "input": { "prompt": "Remove the speech bubble text", "images": [{ "type": "base64", "data": IMAGE }], "mask_url": white }, "async": true }))));
        let sized = call(ImageEditProvider::Polza, "black-forest-labs/flux.2-pro", "https://polza.ai/api", None, (2, 2), SizeParamStyle::WxH, None);
        assert!(matches!(PolzaMedia.submit(&sized), Err(ImageEditError::RequestBuild { .. })));
    }

    // media/create's pending answer, media/status's completed example and the error presenters.
    #[test]
    fn status_machine() {
        let call = pz_call("black-forest-labs/flux.2-pro", None);
        let pending = r#"{"id":"aig_abc123","object":"media.generation","status":"pending","created":1703001244,"model":"black-forest-labs/flux.2-pro"}"#;
        assert!(matches!(PolzaMedia.next(&submit_step(&call), &json_response(200, pending)), Ok(NextStep::Poll { request, .. }) if request.url == "https://polza.ai/api/v1/media/aig_abc123"));
        let job = JobRef { id: "aig_abc123".to_string(), poll_url: None, cancel_url: None };
        let step = poll_step(&call, 0, 1, Some(&job));
        let completed = r#"{"id":"aig_abc123","object":"media.generation","status":"completed","created":1703001244,"model":"google/gemini-2.5-flash-image","data":{"url":"https://s3.polza.ai/f/205141/2026/03/aig_abc123.jpg"},"usage":{"output_units":1,"cost_rub":5.00,"cost":5.00}}"#;
        assert!(matches!(PolzaMedia.next(&step, &json_response(200, completed)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url == "https://s3.polza.ai/f/205141/2026/03/aig_abc123.jpg"));
        let refused = r#"{"id":"aig_1","object":"media.generation","status":"failed","created":1,"model":"qwen/image-2.1","error":{"code":"BAD_GATEWAY","message":"Ошибка генерации медиа контента","metadata":{"raw":"content policy violation","provider_name":"alibaba"}}}"#;
        assert!(matches!(PolzaMedia.next(&step, &json_response(200, refused)), Err(ImageEditError::Moderated)));
        let failed = r#"{"id":"aig_1","object":"media.generation","status":"failed","created":1,"model":"m","error":{"code":"REQUEST_TIMEOUT","message":"Истекло время ожидания"}}"#;
        assert!(matches!(PolzaMedia.next(&step, &json_response(200, failed)), Err(ImageEditError::ProviderFailed { .. })));
        let balance = r#"{"error":{"code":"INSUFFICIENT_BALANCE","message":"Недостаточно средств"},"trace_id":"550e8400-e29b-41d4-a716-446655440000"}"#;
        assert!(matches!(PolzaMedia.next(&submit_step(&call), &json_response(402, balance)), Err(ImageEditError::OutOfCredits)));
        assert!(PolzaMedia.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_and_downloads() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = pz_call("black-forest-labs/flux.2-pro", None);
        let answers = vec![
            json_response(200, r#"{"id":"aig_1","object":"media.generation","status":"pending","created":1,"model":"m"}"#),
            json_response(200, r#"{"id":"aig_1","object":"media.generation","status":"processing","created":1,"model":"m"}"#),
            json_response(200, r#"{"id":"aig_1","object":"media.generation","status":"completed","created":1,"model":"m","data":{"url":"https://s3.polza.ai/f/1/aig_1.png"}}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&PolzaMedia, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Bearer, "secret".to_string()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(trail, [("https://polza.ai/api/v1/media", key.clone()), ("https://polza.ai/api/v1/media/aig_1", key.clone()), ("https://polza.ai/api/v1/media/aig_1", key), ("https://s3.polza.ai/f/1/aig_1.png", None)]);
    }
}
