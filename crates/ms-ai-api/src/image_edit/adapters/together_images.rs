/*
File: crates/ms-ai-api/src/image_edit/adapters/together_images.rs

Purpose:
Adapter of Together AI image generation with a reference image
(`POST {base}/images/generations`, Bearer): one synchronous request, the image answered inline.

Key structures:
- TogetherImages (the `EditProtocol`)

Notes:
Body `{model, prompt, width, height, reference_images: [data URL], response_format:
"base64", output_format: "png"}`. `reference_images` is the documented edit input of FLUX.2
and the Google models (`image_url` is Kontext's and not taken by Gemini); the reference
documents its items as image URLs, so passing a PNG data URL there is UNVERIFIED. FLUX.2
[pro] alone takes `prompt_upsampling` (default `true`, a prompt rewrite), sent as `false` so
the instruction reaches the model verbatim. The size is stated only as `width` / `height`
(`SizeParamStyle::WidthHeight`). The answer is `data[0].b64_json` (`images_data_step`);
errors are HTTP statuses (401 key, 402 spending limit, 429 rate) through `classify_error`.
No cancel.
Sources: https://docs.together.ai/reference/post-images-generations.md,
https://docs.together.ai/docs/inference/images/reference-images.md,
https://docs.together.ai/docs/inference/images/parameters.md,
https://docs.together.ai/docs/quickstart-flux.md, https://docs.together.ai/docs/error-codes.md
(fetched 2026-10-04).
*/

use serde_json::{Map, json};

use super::{images_data_step, json_post, png_data_url};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, NextStep, StepCtx};

/// The only model that takes `prompt_upsampling` (the FLUX quickstart's parameter matrix).
const PROMPT_UPSAMPLING_MODEL: &str = "black-forest-labs/FLUX.2-pro";

/// The Together images adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct TogetherImages;

impl EditProtocol for TogetherImages {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        if call.size_param != SizeParamStyle::WidthHeight {
            return Err(ImageEditError::RequestBuild { detail: format!("Together states the size only as width / height; it cannot state {:?}", call.size_param) });
        }
        let mut body = Map::new();
        body.insert("model".to_string(), json!(call.model_id));
        body.insert("prompt".to_string(), json!(call.prompt));
        body.insert("width".to_string(), json!(call.width));
        body.insert("height".to_string(), json!(call.height));
        // The edited image first, then the reference.
        let images: Vec<String> = std::iter::once(&call.image_png).chain(call.reference_png.as_ref()).map(|png| png_data_url(png)).collect();
        body.insert("reference_images".to_string(), json!(images));
        body.insert("response_format".to_string(), json!("base64"));
        body.insert("output_format".to_string(), json!("png"));
        if call.model_id == PROMPT_UPSAMPLING_MODEL {
            body.insert("prompt_upsampling".to_string(), json!(false));
        }
        Ok(json_post(format!("{}/images/generations", call.base_url), Vec::new(), serde_json::Value::Object(body), AuthScheme::Bearer))
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

    use super::TogetherImages;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn together_call(model_id: &str, size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Together, model_id, "https://api.together.ai/v1", None, (1024, 768), size_param, None)
    }

    // The reference-images curl (`model`, `width` 1024, `height` 768, `prompt`,
    // `reference_images`), with the documented `response_format: "base64"` and `output_format`.
    #[test]
    fn request_matches_the_documented_body() {
        let spec = TogetherImages.submit(&together_call("black-forest-labs/FLUX.2-max", SizeParamStyle::WidthHeight)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api.together.ai/v1/images/generations", Some(AuthScheme::Bearer)));
        let expected = json!({ "model": "black-forest-labs/FLUX.2-max", "prompt": "Remove the speech bubble text", "width": 1024, "height": 768, "reference_images": ["data:image/png;base64,UE5HREFUQQ=="], "response_format": "base64", "output_format": "png" });
        assert_eq!(spec.body, HttpBody::Json(expected));
        let HttpBody::Json(pro) = TogetherImages.submit(&together_call("black-forest-labs/FLUX.2-pro", SizeParamStyle::WidthHeight)).unwrap_or_else(|error| panic!("{error:?}")).body else { panic!("not JSON") };
        assert_eq!(pro["prompt_upsampling"], json!(false));
        assert!(matches!(TogetherImages.submit(&together_call("black-forest-labs/FLUX.2-max", SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // `ImageResponse` with a `b64_json` item (API reference) and the error-code table, which
    // documents statuses and causes only (bodies here carry the table's cause as the message).
    #[test]
    fn answers_and_errors() {
        let call = together_call("black-forest-labs/FLUX.2-max", SizeParamStyle::WidthHeight);
        let step = submit_step(&call);
        let ok = r#"{"id":"oFuwv7Y-2kFHot-99170ebf9e84e0ce-SJC","model":"black-forest-labs/FLUX.2-max","object":"list","data":[{"index":0,"b64_json":"AQID","type":"b64_json"}]}"#;
        assert_eq!(TogetherImages.next(&step, &json_response(200, ok)).ok(), Some(NextStep::Image(vec![1, 2, 3])));
        assert!(matches!(TogetherImages.next(&step, &json_response(401, r#"{"error":{"message":"Missing or Invalid API Key"}}"#)), Err(ImageEditError::KeyRejected)));
        assert!(matches!(TogetherImages.next(&step, &json_response(402, r#"{"error":{"message":"The account associated with the API key has reached its maximum allowed monthly spending limit."}}"#)), Err(ImageEditError::OutOfCredits)));
        assert!(matches!(TogetherImages.next(&step, &json_response(429, r#"{"error":{"message":"Serverless requests are being limited during high demand"}}"#)), Err(ImageEditError::RateLimited)));
        assert!(TogetherImages.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_takes_the_inline_image() {
        use crate::image_edit::adapters::test_support::run_scripted;
        let call = together_call("google/gemini-3-pro-image", SizeParamStyle::WidthHeight);
        let (result, sent) = run_scripted(&TogetherImages, &call, vec![json_response(200, r#"{"id":"x","model":"google/gemini-3-pro-image","object":"list","data":[{"index":0,"b64_json":"SU1BR0U=","type":"b64_json"}]}"#)]);
        assert_eq!(result.ok(), Some(b"IMAGE".to_vec()));
        assert_eq!(sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect::<Vec<_>>(), [("https://api.together.ai/v1/images/generations", Some((AuthScheme::Bearer, "secret".to_string())))]);
    }
}
