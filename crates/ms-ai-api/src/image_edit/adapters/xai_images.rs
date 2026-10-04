/*
File: crates/ms-ai-api/src/image_edit/adapters/xai_images.rs

Purpose:
Adapter of the xAI images edits API (`POST {base}/images/edits` as JSON, Bearer; Grok
Imagine image models): one synchronous request, the image answered inline.

Key structures:
- XaiImages (the `EditProtocol`)

Notes:
Body `{model, prompt, image: {url: data URL, type: "image_url"}, response_format:
"b64_json"}`. xAI takes JSON only ("the OpenAI SDK's `images.edit()` ... is not supported"
because it is multipart), and `image.url` is documented as a "public URL or base64-encoded data
URL". The size is only `aspect_ratio` (default `auto`) plus a `resolution` tier with no
published pixel table, so no size is stated (`SizeParamStyle::None`) and the returned size is
checked by the pipeline. The answer is `data[0].b64_json` (`mime_type` may be JPEG or WebP;
decoding sniffs the bytes) through `images_data_step`; errors go through `classify_error`.
No cancel.
Sources: https://docs.x.ai/developers/model-capabilities/images/editing.md,
https://docs.x.ai/developers/model-capabilities/images/generation.md,
https://docs.x.ai/developers/rest-api-reference/inference/images.md,
https://docs.x.ai/developers/debugging.md (fetched 2026-10-04).
*/

use serde_json::json;

use super::{images_data_step, json_post, png_data_url, refuse_reference};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, NextStep, StepCtx};

/// The xAI images edits adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct XaiImages;

impl EditProtocol for XaiImages {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        // One image field: a reference cannot be expressed.
        refuse_reference(call)?;
        if call.size_param != SizeParamStyle::None {
            return Err(ImageEditError::RequestBuild { detail: format!("xAI image edits have no pixel size field; they cannot state {:?}", call.size_param) });
        }
        let body = json!({
            "model": call.model_id,
            "prompt": call.prompt,
            "image": { "url": png_data_url(&call.image_png), "type": "image_url" },
            "response_format": "b64_json"
        });
        Ok(json_post(format!("{}/images/edits", call.base_url), Vec::new(), body, AuthScheme::Bearer))
    }

    fn next(&self, _step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        images_data_step(response)
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::XaiImages;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn xai_call(size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Xai, "grok-imagine-image-2.0", "https://api.x.ai/v1", None, (1024, 1024), size_param, None)
    }

    // The editing guide / REST reference curl (`model`, `prompt`, `image: {url, type:
    // "image_url"}`) with a data URL and the documented `response_format: "b64_json"`.
    #[test]
    fn request_matches_the_documented_body() {
        let spec = XaiImages.submit(&xai_call(SizeParamStyle::None)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api.x.ai/v1/images/edits", Some(AuthScheme::Bearer)));
        let expected = json!({ "model": "grok-imagine-image-2.0", "prompt": "Remove the speech bubble text", "image": { "url": "data:image/png;base64,UE5HREFUQQ==", "type": "image_url" }, "response_format": "b64_json" });
        assert_eq!(spec.body, HttpBody::Json(expected));
        assert!(matches!(XaiImages.submit(&xai_call(SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // The REST reference's response body (`data[].b64_json`, `mime_type`, `usage`) and the
    // debugging page's status table (bodies are not documented; the table's cause stands in).
    #[test]
    fn answers_and_errors() {
        let call = xai_call(SizeParamStyle::None);
        let step = submit_step(&call);
        let ok = r#"{"data":[{"b64_json":"AQID","mime_type":"image/jpeg","url":null}],"usage":{"cost_in_usd_ticks":400000000}}"#;
        assert_eq!(XaiImages.next(&step, &json_response(200, ok)).ok(), Some(NextStep::Image(vec![1, 2, 3])));
        assert!(matches!(XaiImages.next(&step, &json_response(401, r#"{"error":"No authorization header or an invalid authorization token was provided."}"#)), Err(ImageEditError::KeyRejected)));
        assert!(matches!(XaiImages.next(&step, &json_response(422, r#"{"error":"A field in the POST request body has an invalid format."}"#)), Err(ImageEditError::ProviderRejected { status: 422, message }) if message.starts_with("A field")));
        assert!(matches!(XaiImages.next(&step, &json_response(429, r#"{"error":"You are sending requests too frequently and have reached the rate limit."}"#)), Err(ImageEditError::RateLimited)));
        assert!(XaiImages.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_takes_the_inline_image() {
        use crate::image_edit::adapters::test_support::run_scripted;
        let call = xai_call(SizeParamStyle::None);
        let (result, sent) = run_scripted(&XaiImages, &call, vec![json_response(200, r#"{"data":[{"b64_json":"SU1BR0U=","mime_type":"image/png"}]}"#)]);
        assert_eq!(result.ok(), Some(b"IMAGE".to_vec()));
        assert_eq!(sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect::<Vec<_>>(), [("https://api.x.ai/v1/images/edits", Some((AuthScheme::Bearer, "secret".to_string())))]);
    }
}
