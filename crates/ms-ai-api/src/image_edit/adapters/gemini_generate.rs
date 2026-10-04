/*
File: crates/ms-ai-api/src/image_edit/adapters/gemini_generate.rs

Purpose:
Adapter of the Gemini image models through `POST {base}/models/{id}:generateContent` with
image output (`responseModalities` TEXT + IMAGE, `imageConfig { aspectRatio, imageSize }`).

Key structures:
- GeminiGenerate (the `EditProtocol`)

Notes:
The key goes in the `x-goog-api-key` header (never in the `?key=` query, which would put it in
a URL). The prompt and the image are two parts of one user turn (`text`, then `inline_data`
PNG). The size is the table entry's labels (the Gemini offers are table offers). Gemini 3 image
models think and may emit interim images marked `"thought": true`: they are skipped, and the
LAST non-thought image of the first candidate is the result. No image: `IMAGE_SAFETY` /
`SAFETY` / `PROHIBITED_CONTENT` / ... finish reasons and a prompt `blockReason` are
`Moderated`, anything else is `NoImageReturned` with the finish reason or the model's text.
Sources: https://ai.google.dev/api/generate-content (request parts, `Part.thought`,
`FinishReason`, `BlockReason`, `ImageConfig`) and
https://ai.google.dev/gemini-api/docs/image-generation (fetched 2026-10-04).
*/

use serde_json::{Value, json};

use super::{classify_error, decode_base64_image, is_success, json_value};
use crate::encoding::base64_encode;
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, NextStep, StepCtx};

/// Finish / block reasons that mean the content filter refused (`FinishReason`, `BlockReason`).
const MODERATION_REASONS: [&str; 8] = ["SAFETY", "IMAGE_SAFETY", "PROHIBITED_CONTENT", "IMAGE_PROHIBITED_CONTENT", "BLOCKLIST", "SPII", "RECITATION", "IMAGE_RECITATION"];

/// The Gemini `generateContent` adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiGenerate;

impl EditProtocol for GeminiGenerate {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        let mut generation_config = json!({ "responseModalities": ["TEXT", "IMAGE"] });
        match call.size_param {
            SizeParamStyle::AspectTier(_) => {
                let entry = call.size_entry.ok_or_else(|| ImageEditError::RequestBuild { detail: "a table offer reached the adapter without its table entry".to_string() })?;
                generation_config["imageConfig"] = json!({ "aspectRatio": entry.aspect, "imageSize": entry.tier });
            }
            SizeParamStyle::None => {}
            SizeParamStyle::WxH | SizeParamStyle::WStarH | SizeParamStyle::WidthHeight | SizeParamStyle::ImageSizeObject => {
                return Err(ImageEditError::RequestBuild { detail: format!("generateContent cannot state the size as {:?}", call.size_param) });
            }
        }
        let body = json!({
            "contents": [{
                "parts": [
                    { "text": call.prompt },
                    { "inline_data": { "mime_type": "image/png", "data": base64_encode(&call.image_png) } }
                ]
            }],
            "generationConfig": generation_config
        });
        Ok(HttpRequestSpec { method: HttpMethod::Post, url: format!("{}/models/{}:generateContent", call.base_url, call.model_id), headers: Vec::new(), body: HttpBody::Json(body), auth: Some(AuthScheme::Header("x-goog-api-key")) })
    }

    fn next(&self, _step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(classify_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        if let Some(reason) = value.pointer("/promptFeedback/blockReason").and_then(Value::as_str) {
            return Err(if reason == "BLOCK_REASON_UNSPECIFIED" { ImageEditError::NoImageReturned { reason: reason.to_string() } } else { ImageEditError::Moderated });
        }
        let candidate = value.pointer("/candidates/0");
        let parts = candidate.and_then(|candidate| candidate.pointer("/content/parts")).and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let is_thought = |part: &Value| part.get("thought").and_then(Value::as_bool).unwrap_or(false);
        // REST answers use camelCase (`inlineData`); accept the snake_case spelling too.
        let image = parts.iter().filter(|part| !is_thought(part)).filter_map(|part| part.get("inlineData").or_else(|| part.get("inline_data"))).filter_map(|data| data.get("data").and_then(Value::as_str)).next_back();
        if let Some(data) = image {
            return Ok(NextStep::Image(decode_base64_image(data)?));
        }
        let finish = candidate.and_then(|candidate| candidate.get("finishReason")).and_then(Value::as_str);
        if finish.is_some_and(|reason| MODERATION_REASONS.contains(&reason)) {
            return Err(ImageEditError::Moderated);
        }
        let text: Vec<&str> = parts.iter().filter(|part| !is_thought(part)).filter_map(|part| part.get("text").and_then(Value::as_str)).collect();
        let reason = match (finish, text.is_empty()) {
            (Some(finish), true) => finish.to_string(),
            (Some(finish), false) => format!("{finish}: {}", text.join(" ")),
            (None, false) => text.join(" "),
            (None, true) => "the answer has no candidate image".to_string(),
        };
        Err(ImageEditError::NoImageReturned { reason })
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::GeminiGenerate;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::size_rule::GEMINI_31_FLASH_ENTRIES;

    fn gemini_call() -> crate::image_edit::protocol::EditCall {
        let entry = GEMINI_31_FLASH_ENTRIES.iter().find(|entry| (entry.aspect, entry.tier) == ("4:3", "1K")).copied();
        let size = entry.map_or((0, 0), |entry| (entry.width, entry.height));
        call(ImageEditProvider::Gemini, "gemini-3.1-flash-image", "https://generativelanguage.googleapis.com/v1beta", None, size, SizeParamStyle::AspectTier(&GEMINI_31_FLASH_ENTRIES), entry)
    }

    // The reference's inline-image curl (`contents[{parts[{text},{inline_data{mime_type,data}}]}]`,
    // https://ai.google.dev/api/generate-content) plus `generationConfig.responseModalities` and
    // `imageConfig {aspectRatio, imageSize}`; the key in `x-goog-api-key`, never `?key=`.
    #[test]
    fn request_matches_the_documented_body() {
        let call = gemini_call();
        assert_eq!((call.width, call.height), (1200, 896));
        let spec = GeminiGenerate.submit(&call).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(spec.method, HttpMethod::Post);
        assert_eq!(spec.url, "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.1-flash-image:generateContent");
        assert_eq!(spec.auth, Some(AuthScheme::Header("x-goog-api-key")));
        let expected = json!({
            "contents": [{ "parts": [
                { "text": "Remove the speech bubble text" },
                { "inline_data": { "mime_type": "image/png", "data": "UE5HREFUQQ==" } }
            ] }],
            "generationConfig": { "responseModalities": ["TEXT", "IMAGE"], "imageConfig": { "aspectRatio": "4:3", "imageSize": "1K" } }
        });
        assert_eq!(spec.body, HttpBody::Json(expected));
    }

    #[test]
    fn the_last_non_thought_image_wins() {
        let call = gemini_call();
        let step = submit_step(&call);
        // A thinking model's answer: an interim thought image, a text part, the final image.
        let body = r#"{"candidates":[{"content":{"role":"model","parts":[
            {"inlineData":{"mimeType":"image/png","data":"BAUG"},"thought":true},
            {"text":"Here is the edited image."},
            {"inlineData":{"mimeType":"image/png","data":"AQID"}}
        ]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":10}}"#;
        assert_eq!(GeminiGenerate.next(&step, &json_response(200, body)).ok(), Some(NextStep::Image(vec![1, 2, 3])));
    }

    #[test]
    fn no_image_answers_are_typed() {
        let call = gemini_call();
        let step = submit_step(&call);
        let text_only = r#"{"candidates":[{"content":{"parts":[{"text":"thinking...","thought":true},{"text":"I can't edit this image."}]},"finishReason":"STOP"}]}"#;
        assert!(matches!(GeminiGenerate.next(&step, &json_response(200, text_only)), Err(ImageEditError::NoImageReturned { reason }) if reason == "STOP: I can't edit this image."));
        let only_thought_image = r#"{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"BAUG"},"thought":true}]},"finishReason":"NO_IMAGE"}]}"#;
        assert!(matches!(GeminiGenerate.next(&step, &json_response(200, only_thought_image)), Err(ImageEditError::NoImageReturned { reason }) if reason == "NO_IMAGE"));
        let unsafe_image = r#"{"candidates":[{"content":{"parts":[]},"finishReason":"IMAGE_SAFETY"}]}"#;
        assert!(matches!(GeminiGenerate.next(&step, &json_response(200, unsafe_image)), Err(ImageEditError::Moderated)));
        let blocked_prompt = r#"{"promptFeedback":{"blockReason":"PROHIBITED_CONTENT"}}"#;
        assert!(matches!(GeminiGenerate.next(&step, &json_response(200, blocked_prompt)), Err(ImageEditError::Moderated)));
    }

    // Google API error objects (`{"error":{"code","message","status"}}`).
    #[test]
    fn error_bodies_are_typed() {
        let call = gemini_call();
        let step = submit_step(&call);
        let location = r#"{"error":{"code":400,"message":"User location is not supported for the API use.","status":"FAILED_PRECONDITION"}}"#;
        assert!(matches!(GeminiGenerate.next(&step, &json_response(400, location)), Err(ImageEditError::RegionBlocked)));
        let key = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID"}]}}"#;
        assert!(matches!(GeminiGenerate.next(&step, &json_response(400, key)), Err(ImageEditError::KeyRejected)));
        let quota = r#"{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota).","status":"RESOURCE_EXHAUSTED"}}"#;
        assert!(matches!(GeminiGenerate.next(&step, &json_response(429, quota)), Err(ImageEditError::RateLimited)));
        let overloaded = r#"{"error":{"code":503,"message":"The model is overloaded. Please try again later.","status":"UNAVAILABLE"}}"#;
        assert!(matches!(GeminiGenerate.next(&step, &json_response(503, overloaded)), Err(ImageEditError::ProviderFailed { .. })));
    }
}
