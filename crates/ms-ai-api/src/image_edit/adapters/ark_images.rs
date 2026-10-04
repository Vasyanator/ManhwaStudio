/*
File: crates/ms-ai-api/src/image_edit/adapters/ark_images.rs

Purpose:
Adapter of the `BytePlus` `ModelArk` image generation API (`POST {base}/images/generations`,
Bearer; Seedream models): one synchronous request with the reference image, answered with the
image inline.

Key structures:
- ArkImages (the `EditProtocol`)

Notes:
Body `{model, prompt, image: data URL, size: "WxH", response_format: "b64_json",
output_format: "png", watermark: false}`. Base64 input must be
`data:image/<lowercase format>;base64,...`; `watermark` defaults to `true` and `output_format`
to `jpeg`, so both are stated; `sequential_image_generation` is left out (Seedream 5.0 pro
does not support it, lite defaults to `disabled`). The pixel size is method 2 of `size`
(`"WxH"`, total-pixel and aspect limits per model, see the catalogue rules). The answer is
`data[0].b64_json` (`images_data_step`). Errors are `{error: {code, message}}`: a
`*SensitiveContentDetected*` code (input / output image or text) -> `Moderated`,
`AccountOverdueError` / `OperationDenied.ServiceOverdue` (403) -> `OutOfCredits`, the rest
through `classify_error` (401 `AuthenticationError`, 429 rate / quota). No cancel.
Sources: https://docs.byteplus.com/en/docs/ModelArk/1541523 (image generation API),
https://docs.byteplus.com/en/docs/ModelArk/1330310 (model ids),
https://docs.byteplus.com/en/docs/ModelArk/1299023 (error codes), fetched 2026-10-04 (the pages
render client-side; their markdown is embedded in the served HTML).
*/

use serde_json::{Value, json};

use super::{classify_error, images_data_step, is_success, json_post, png_data_url, refuse_reference};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, NextStep, StepCtx};

/// Error codes of an overdue account.
const OVERDUE_CODES: [&str; 2] = ["AccountOverdueError", "OperationDenied.ServiceOverdue"];

/// The `ModelArk` images adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct ArkImages;

/// The typed error of a non-success answer: the documented `error.code`s first, then the
/// shared classification.
fn ark_error(status: u16, body: &[u8]) -> ImageEditError {
    let code = serde_json::from_slice::<Value>(body).ok().and_then(|value| value.pointer("/error/code").and_then(Value::as_str).map(str::to_string)).unwrap_or_default();
    if code.contains("SensitiveContentDetected") {
        return ImageEditError::Moderated;
    }
    if OVERDUE_CODES.contains(&code.as_str()) {
        return ImageEditError::OutOfCredits;
    }
    classify_error(status, body)
}

impl EditProtocol for ArkImages {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        // One image field: a reference cannot be expressed.
        refuse_reference(call)?;
        if call.size_param != SizeParamStyle::WxH {
            return Err(ImageEditError::RequestBuild { detail: format!("ModelArk states a pixel size only as \"WxH\"; it cannot state {:?}", call.size_param) });
        }
        let body = json!({
            "model": call.model_id,
            "prompt": call.prompt,
            "image": png_data_url(&call.image_png),
            "size": format!("{}x{}", call.width, call.height),
            "response_format": "b64_json",
            "output_format": "png",
            "watermark": false
        });
        Ok(json_post(format!("{}/images/generations", call.base_url), Vec::new(), body, AuthScheme::Bearer))
    }

    fn next(&self, _step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(ark_error(response.status, &response.body));
        }
        images_data_step(response)
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ArkImages;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn ark_call(size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::BytePlus, "dola-seedream-5-0-pro-260628", "https://ark.ap-southeast.bytepluses.com/api/v3", None, (2048, 1024), size_param, None)
    }

    // The image generation reference: `model`, `prompt`, `image` as `data:image/png;base64,...`,
    // `size` method 2 ("2048x1024", its valid example), `response_format`, `output_format`,
    // `watermark`.
    #[test]
    fn request_matches_the_documented_body() {
        let spec = ArkImages.submit(&ark_call(SizeParamStyle::WxH)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://ark.ap-southeast.bytepluses.com/api/v3/images/generations", Some(AuthScheme::Bearer)));
        let expected = json!({ "model": "dola-seedream-5-0-pro-260628", "prompt": "Remove the speech bubble text", "image": "data:image/png;base64,UE5HREFUQQ==", "size": "2048x1024", "response_format": "b64_json", "output_format": "png", "watermark": false });
        assert_eq!(spec.body, HttpBody::Json(expected));
        assert!(matches!(ArkImages.submit(&ark_call(SizeParamStyle::None)), Err(ImageEditError::RequestBuild { .. })));
    }

    // The documented response fields (`data[].b64_json`, `size`, `output_format`) and the
    // error-code table (400 InputImageSensitiveContentDetected, 401 AuthenticationError, 403
    // AccountOverdueError, 429 ModelAccountIpmRateLimitExceeded, 400 InvalidParameter).
    #[test]
    fn answers_and_errors() {
        let call = ark_call(SizeParamStyle::WxH);
        let step = submit_step(&call);
        let ok = r#"{"model":"dola-seedream-5-0-pro-260628","created":1757321139,"data":[{"b64_json":"AQID","size":"2048x1024","output_format":"png"}]}"#;
        assert_eq!(ArkImages.next(&step, &json_response(200, ok)).ok(), Some(NextStep::Image(vec![1, 2, 3])));
        let error = |code: &str, message: &str| format!(r#"{{"error":{{"code":"{code}","message":"{message}","param":"","type":"BadRequest"}}}}"#);
        assert!(matches!(ArkImages.next(&step, &json_response(400, &error("InputImageSensitiveContentDetected", "The request failed because the input image may contain sensitive information. Request ID: x"))), Err(ImageEditError::Moderated)));
        assert!(matches!(ArkImages.next(&step, &json_response(400, &error("OutputImageSensitiveContentDetected", "The request failed because the output image may contain sensitive information."))), Err(ImageEditError::Moderated)));
        assert!(matches!(ArkImages.next(&step, &json_response(401, &error("AuthenticationError", "The API key or AK/SK in the request is missing or invalid. Request ID: x"))), Err(ImageEditError::KeyRejected)));
        assert!(matches!(ArkImages.next(&step, &json_response(403, &error("AccountOverdueError", "The request failed because your account has an overdue balance. Request ID: x"))), Err(ImageEditError::OutOfCredits)));
        assert!(matches!(ArkImages.next(&step, &json_response(429, &error("ModelAccountIpmRateLimitExceeded", "IPM (Images Per Minute) limit of the model is exceeded."))), Err(ImageEditError::RateLimited)));
        assert!(matches!(ArkImages.next(&step, &json_response(400, &error("InvalidParameter", "The specified parameter size is invalid."))), Err(ImageEditError::ProviderRejected { status: 400, message }) if message == "The specified parameter size is invalid."));
        assert!(ArkImages.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_takes_the_inline_image() {
        use crate::image_edit::adapters::test_support::run_scripted;
        let call = ark_call(SizeParamStyle::WxH);
        let (result, sent) = run_scripted(&ArkImages, &call, vec![json_response(200, r#"{"data":[{"b64_json":"SU1BR0U=","size":"2048x1024"}]}"#)]);
        assert_eq!(result.ok(), Some(b"IMAGE".to_vec()));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].auth, Some((AuthScheme::Bearer, "secret".to_string())));
    }
}
