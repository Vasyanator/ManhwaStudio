/*
File: crates/ms-ai-api/src/image_edit/adapters/openrouter_images.rs

Purpose:
Adapter of the `OpenRouter`-style images shape, `POST {base}/images` with a JSON body, used by
`OpenRouter` and the Russian mirror RouterAI.

Key structures:
- OpenRouterImages (the `EditProtocol`)

Notes:
The edited image travels as the single `input_references` entry (`{"type":"image_url",
"image_url":{"url":"data:image/png;base64,..."}}`). The size is the explicit `size` "WxH"
("authoritative" in the docs) or, for table offers, `aspect_ratio` + `resolution` of the table
entry. A native mask (RouterAI gpt-image only; no `OpenRouter` model takes one) is
`mask: {"image_url": {"url": data URL}}`, RGBA PNG, transparent = edit (the upstream `OpenAI`
convention). `n` is pinned to 1. The answer is `data[0].b64_json`. Single step, synchronous.
Sources: https://openrouter.ai/docs/guides/overview/multimodal/image-generation and
https://routerai.ru/docs/guides/overview/multimodal/image-generation (fetched 2026-10-04).
*/

use serde_json::{Map, Value, json};

use super::{images_data_step, png_data_url};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, NextStep, StepCtx};

/// The `OpenRouter` images adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenRouterImages;

impl EditProtocol for OpenRouterImages {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        let mut body = Map::new();
        body.insert("model".to_string(), json!(call.model_id));
        body.insert("prompt".to_string(), json!(call.prompt));
        body.insert("n".to_string(), json!(1));
        // The edited image first, then the reference.
        let references: Vec<serde_json::Value> = std::iter::once(&call.image_png).chain(call.reference_png.as_ref()).map(|png| json!({ "type": "image_url", "image_url": { "url": png_data_url(png) } })).collect();
        body.insert("input_references".to_string(), json!(references));
        match call.size_param {
            SizeParamStyle::WxH => {
                body.insert("size".to_string(), json!(format!("{}x{}", call.width, call.height)));
            }
            SizeParamStyle::AspectTier(_) => {
                let entry = call.size_entry.ok_or_else(|| ImageEditError::RequestBuild { detail: "a table offer reached the adapter without its table entry".to_string() })?;
                body.insert("aspect_ratio".to_string(), json!(entry.aspect));
                body.insert("resolution".to_string(), json!(entry.tier));
            }
            SizeParamStyle::None => {}
            SizeParamStyle::WStarH | SizeParamStyle::WidthHeight | SizeParamStyle::ImageSizeObject => {
                return Err(ImageEditError::RequestBuild { detail: format!("the OpenRouter images shape cannot state the size as {:?}", call.size_param) });
            }
        }
        if let Some(mask) = &call.mask {
            let png = encode_mask_png(mask, call.width, call.height, MaskPolarity::TransparentEdits)?;
            body.insert("mask".to_string(), json!({ "image_url": { "url": png_data_url(&png) } }));
        }
        Ok(HttpRequestSpec { method: HttpMethod::Post, url: format!("{}/images", call.base_url), headers: Vec::new(), body: HttpBody::Json(Value::Object(body)), auth: Some(AuthScheme::Bearer) })
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

    use super::OpenRouterImages;
    use crate::image_edit::adapters::png_data_url;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::size_rule::GEMINI_3_PRO_ENTRIES;

    // The documented body (model, prompt, n, input_references[{type, image_url{url}}], size)
    // from https://openrouter.ai/docs/guides/overview/multimodal/image-generation.
    #[test]
    fn explicit_size_request_matches_the_documented_body() {
        let call = call(ImageEditProvider::OpenRouter, "black-forest-labs/flux.2-pro", "https://openrouter.ai/api/v1", None, (1200, 704), SizeParamStyle::WxH, None);
        let spec = OpenRouterImages.submit(&call).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://openrouter.ai/api/v1/images", Some(AuthScheme::Bearer)));
        let expected = json!({
            "model": "black-forest-labs/flux.2-pro",
            "prompt": "Remove the speech bubble text",
            "n": 1,
            "input_references": [{ "type": "image_url", "image_url": { "url": "data:image/png;base64,UE5HREFUQQ==" } }],
            "size": "1200x704"
        });
        assert_eq!(spec.body, HttpBody::Json(expected));
    }

    // Table offers state `aspect_ratio` + `resolution` (documented tiers "1K", "2K", ...);
    // RouterAI's inpaint sample adds `mask: {image_url: {url}}`
    // (https://routerai.ru/docs/guides/overview/multimodal/image-generation).
    #[test]
    fn table_size_and_mask_follow_the_documented_fields() {
        let entry = GEMINI_3_PRO_ENTRIES[0];
        let gemini = call(ImageEditProvider::RouterAi, "google/gemini-3-pro-image", "https://routerai.ru/api/v1", None, (entry.width, entry.height), SizeParamStyle::AspectTier(&GEMINI_3_PRO_ENTRIES), Some(entry));
        let spec = OpenRouterImages.submit(&gemini).unwrap_or_else(|error| panic!("{error:?}"));
        let HttpBody::Json(body) = spec.body else { panic!("JSON body expected") };
        assert_eq!((body.get("aspect_ratio"), body.get("resolution"), body.get("size")), (Some(&json!(entry.aspect)), Some(&json!(entry.tier)), None));

        let mask = vec![255, 0, 0, 0];
        let gpt = call(ImageEditProvider::RouterAi, "openai/gpt-image-2", "https://routerai.ru/api/v1", Some(mask.clone()), (2, 2), SizeParamStyle::WxH, None);
        let spec = OpenRouterImages.submit(&gpt).unwrap_or_else(|error| panic!("{error:?}"));
        let HttpBody::Json(body) = spec.body else { panic!("JSON body expected") };
        let mask_png = encode_mask_png(&mask, 2, 2, MaskPolarity::TransparentEdits).unwrap_or_default();
        assert_eq!(body.get("mask"), Some(&json!({ "image_url": { "url": png_data_url(&mask_png) } })));

        let missing_entry = call(ImageEditProvider::OpenRouter, "google/gemini-3-pro-image", "https://openrouter.ai/api/v1", None, (2, 2), SizeParamStyle::AspectTier(&GEMINI_3_PRO_ENTRIES), None);
        assert!(matches!(OpenRouterImages.submit(&missing_entry), Err(ImageEditError::RequestBuild { .. })));
    }

    // The documented 200 answer and OpenRouter's `{"error":{"code","message"}}` errors.
    #[test]
    fn answers_are_parsed() {
        let call = call(ImageEditProvider::OpenRouter, "openai/gpt-image-2", "https://openrouter.ai/api/v1", None, (2, 2), SizeParamStyle::WxH, None);
        let step = submit_step(&call);
        let ok = OpenRouterImages.next(&step, &json_response(200, r#"{"created":1748372400,"data":[{"b64_json":"AQID","media_type":"image/png"}],"usage":{"prompt_tokens":0,"completion_tokens":4175,"total_tokens":4175,"cost":0.04}}"#));
        assert_eq!(ok.ok(), Some(NextStep::Image(vec![1, 2, 3])));
        assert!(matches!(OpenRouterImages.next(&step, &json_response(402, r#"{"error":{"code":402,"message":"This request requires more credits"}}"#)), Err(ImageEditError::OutOfCredits)));
        assert!(matches!(OpenRouterImages.next(&step, &json_response(401, r#"{"error":{"code":401,"message":"No auth credentials found"}}"#)), Err(ImageEditError::KeyRejected)));
        assert!(matches!(OpenRouterImages.next(&step, &json_response(403, r#"{"error":{"code":403,"message":"openai/gpt-image-2 requires moderation on OpenAI. Your input was flagged for \"violence\"."}}"#)), Err(ImageEditError::Moderated)));
    }
}
