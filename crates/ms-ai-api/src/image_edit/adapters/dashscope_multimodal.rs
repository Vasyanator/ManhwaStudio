/*
File: crates/ms-ai-api/src/image_edit/adapters/dashscope_multimodal.rs

Purpose:
Adapter of Alibaba Model Studio (`DashScope`) synchronous multimodal image generation,
`POST {base}/api/v1/services/aigc/multimodal-generation/generation` (Bearer), shared by the
Qwen-Image edit / 2.0 / 3.0 models and Wan 2.7 Image: one request, one answer with a signed
result URL that is downloaded without the key.

Key structures:
- DashScopeMultimodal (the `EditProtocol`)

Notes:
Body `{model, input: {messages: [{role: "user", content: [{image: data URL}, {text}]}]},
parameters: {n: 1, size: "W*H", watermark: false, ...}}`; the image goes first, as in every
documented sample, and Base64 input is documented as `data:{MIME};base64,...`. Per family:
- Qwen edit / 2.0: `prompt_extend: false` (the default `true` rewrites the instruction);
- Qwen 3.0: also `enable_thinking: false`, because thinking "requires prompt_extend=true";
- Wan 2.7: neither field exists there (its `thinking_mode` applies only without an input image).
The answer is `output.choices[0].message.content[].image` (an OSS URL valid 24 h). Errors are
`{request_id, code, message}` with an HTTP status: `DataInspectionFailed` /
`IPInfringementSuspect` -> `Moderated`, `Arrearage` -> `OutOfCredits`,
`AccessDenied.Unpurchased` (403, service not activated) -> `ProviderRejected`, the rest through
`classify_error` (401 `InvalidApiKey`, 429 `Throttling.*`). No cancel (synchronous).
The size is stated only as `"W*H"` (`SizeParamStyle::WStarH`).
Sources: https://www.alibabacloud.com/help/en/model-studio/qwen-image-edit-api,
https://www.alibabacloud.com/help/en/model-studio/qwen-image-generation-and-editing-api-reference,
https://www.alibabacloud.com/help/en/model-studio/wan-image-generation-and-editing-api-reference,
https://www.alibabacloud.com/help/en/model-studio/error-code (fetched 2026-10-04).
*/

use serde_json::{Map, Value, json};

use super::{classify_error, is_success, json_post, json_value, png_data_url, provider_message, result_url_step};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, NextStep, StepCtx};

/// The request path under the region's base URL.
const GENERATION_PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";

/// Error codes of a content-safety refusal (Green Net inspection, IP infringement).
const MODERATION_CODES: [&str; 3] = ["DataInspectionFailed", "data_inspection_failed", "IPInfringementSuspect"];

/// The `DashScope` multimodal generation adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct DashScopeMultimodal;

/// The model families whose `parameters` differ (see the file header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// `qwen-image-edit-*`, `qwen-image-2.0*`.
    QwenEdit,
    /// `qwen-image-3.0*`.
    Qwen3,
    /// `wan2.7-image*`.
    Wan,
}

impl Family {
    /// The family of `model_id`, by its documented id prefix.
    fn of(model_id: &str) -> Self {
        if model_id.starts_with("wan") {
            Self::Wan
        } else if model_id.starts_with("qwen-image-3.") {
            Self::Qwen3
        } else {
            Self::QwenEdit
        }
    }
}

/// The typed error of a non-success answer: the documented `code`s first, then the shared
/// status / marker classification.
fn dashscope_error(status: u16, body: &[u8]) -> ImageEditError {
    let value = serde_json::from_slice::<Value>(body).ok();
    let code = value.as_ref().and_then(|value| value.get("code")).and_then(Value::as_str).unwrap_or_default();
    if MODERATION_CODES.contains(&code) {
        return ImageEditError::Moderated;
    }
    if code == "Arrearage" {
        return ImageEditError::OutOfCredits;
    }
    if code.starts_with("AccessDenied.Unpurchased") {
        let message = value.as_ref().and_then(provider_message).unwrap_or_else(|| code.to_string());
        return ImageEditError::ProviderRejected { status, message };
    }
    classify_error(status, body)
}

impl EditProtocol for DashScopeMultimodal {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        if call.size_param != SizeParamStyle::WStarH {
            return Err(ImageEditError::RequestBuild { detail: format!("DashScope states the size only as \"W*H\"; it cannot state {:?}", call.size_param) });
        }
        let mut parameters = Map::new();
        parameters.insert("n".to_string(), json!(1));
        parameters.insert("size".to_string(), json!(format!("{}*{}", call.width, call.height)));
        parameters.insert("watermark".to_string(), json!(false));
        match Family::of(&call.model_id) {
            Family::QwenEdit => {
                parameters.insert("prompt_extend".to_string(), json!(false));
            }
            Family::Qwen3 => {
                parameters.insert("prompt_extend".to_string(), json!(false));
                parameters.insert("enable_thinking".to_string(), json!(false));
            }
            Family::Wan => {}
        }
        // The edited image first, then the reference, then the instruction.
        let content: Vec<Value> = std::iter::once(&call.image_png).chain(call.reference_png.as_ref()).map(|png| json!({ "image": png_data_url(png) })).chain(std::iter::once(json!({ "text": call.prompt }))).collect();
        let body = json!({
            "model": call.model_id,
            "input": { "messages": [{ "role": "user", "content": content }] },
            "parameters": Value::Object(parameters)
        });
        Ok(json_post(format!("{}{GENERATION_PATH}", call.base_url), Vec::new(), body, AuthScheme::Bearer))
    }

    fn next(&self, _step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(dashscope_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        let content = value.pointer("/output/choices/0/message/content").and_then(Value::as_array);
        let url = content.and_then(|items| items.iter().find_map(|item| item.get("image").and_then(Value::as_str)));
        match url {
            Some(url) => result_url_step(url),
            None => Err(ImageEditError::NoImageReturned { reason: provider_message(&value).unwrap_or_else(|| "the answer has no output.choices[0].message.content[].image".to_string()) }),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::DashScopeMultimodal;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    const BASE: &str = "https://dashscope-intl.aliyuncs.com";

    fn dashscope_call(model_id: &str, size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::DashScope, model_id, BASE, None, (1536, 1024), size_param, None)
    }

    fn body_of(model_id: &str) -> HttpBody {
        DashScopeMultimodal.submit(&dashscope_call(model_id, SizeParamStyle::WStarH)).unwrap_or_else(|error| panic!("{error:?}")).body
    }

    // The single-image editing curl of the Qwen-Image Edit reference (`input.messages[0].content`
    // = `{image}` then `{text}`, `parameters.size` "1536*1024", `watermark`, `prompt_extend`), with
    // the documented Base64 form `data:{mime};base64,...` and our fixed `n: 1`.
    #[test]
    fn qwen_edit_request_matches_the_documented_body() {
        let spec = DashScopeMultimodal.submit(&dashscope_call("qwen-image-edit-max", SizeParamStyle::WStarH)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://dashscope-intl.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation", Some(AuthScheme::Bearer)));
        assert!(spec.headers.is_empty());
        let expected = json!({
            "model": "qwen-image-edit-max",
            "input": { "messages": [{ "role": "user", "content": [{ "image": "data:image/png;base64,UE5HREFUQQ==" }, { "text": "Remove the speech bubble text" }] }] },
            "parameters": { "n": 1, "size": "1536*1024", "watermark": false, "prompt_extend": false }
        });
        assert_eq!(spec.body, HttpBody::Json(expected));
    }

    // Qwen 3.0: `enable_thinking` "requires prompt_extend=true", so it is switched off too; Wan
    // 2.7 documents neither field.
    #[test]
    fn family_parameters_follow_the_references() {
        let HttpBody::Json(qwen3) = body_of("qwen-image-3.0-pro") else { panic!("not JSON") };
        assert_eq!(qwen3["parameters"], json!({ "n": 1, "size": "1536*1024", "watermark": false, "prompt_extend": false, "enable_thinking": false }));
        let HttpBody::Json(wan) = body_of("wan2.7-image-pro") else { panic!("not JSON") };
        assert_eq!(wan["parameters"], json!({ "n": 1, "size": "1536*1024", "watermark": false }));
        assert!(matches!(DashScopeMultimodal.submit(&dashscope_call("qwen-image-edit-max", SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // The references' success example and their `{request_id, code, message}` errors
    // (error-code page: 400 DataInspectionFailed / Arrearage, 401 InvalidApiKey, 403
    // AccessDenied.Unpurchased, 429 Throttling.RateQuota).
    #[test]
    fn answers_and_errors() {
        let call = dashscope_call("qwen-image-edit-max", SizeParamStyle::WStarH);
        let step = submit_step(&call);
        let ok = r#"{"output":{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":[{"image":"https://dashscope-result-sz.oss-cn-shenzhen.aliyuncs.com/xxx.png?Expires=xxx"}]}}]},"usage":{"width":1536,"image_count":1,"height":1024},"request_id":"bf37ca26-0abe-98e4-8065-xxxxxx"}"#;
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(200, ok)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.starts_with("https://dashscope-result-sz.oss-cn-shenzhen.aliyuncs.com/")));
        // Wan 2.7's answer adds `type: "image"` and `finished`.
        let wan = r#"{"output":{"choices":[{"finish_reason":"stop","message":{"content":[{"image":"https://dashscope-xxx.oss-xxx.aliyuncs.com/xxx.png?Expires=xxx","type":"image"}],"role":"assistant"}}],"finished":true},"usage":{"image_count":1,"size":"1488*704"},"request_id":"71dfc3c6"}"#;
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(200, wan)), Ok(NextStep::Download(_))));
        let key = r#"{"request_id":"31f808fd-8eef-9004-xxxxx","code":"InvalidApiKey","message":"Invalid API-key provided."}"#;
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(401, key)), Err(ImageEditError::KeyRejected)));
        let inspected = r#"{"request_id":"r","code":"DataInspectionFailed","message":"Input data may contain inappropriate content."}"#;
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(400, inspected)), Err(ImageEditError::Moderated)));
        let arrears = r#"{"request_id":"r","code":"Arrearage","message":"Access denied, please make sure your account is in good standing."}"#;
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(400, arrears)), Err(ImageEditError::OutOfCredits)));
        let unpurchased = r#"{"request_id":"r","code":"AccessDenied.Unpurchased","message":"Access to model denied. Please make sure you are eligible for using the model."}"#;
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(403, unpurchased)), Err(ImageEditError::ProviderRejected { status: 403, message }) if message.starts_with("Access to model denied")));
        let throttled = r#"{"request_id":"r","code":"Throttling.RateQuota","message":"Requests rate limit exceeded, please try again later."}"#;
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(429, throttled)), Err(ImageEditError::RateLimited)));
        let invalid = r#"{"request_id":"a4d78a5f","code":"InvalidParameter","message":"num_images_per_prompt must be 1"}"#;
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(400, invalid)), Err(ImageEditError::ProviderRejected { status: 400, message }) if message == "num_images_per_prompt must be 1"));
        assert!(matches!(DashScopeMultimodal.next(&step, &json_response(200, r#"{"output":{"choices":[]},"request_id":"r"}"#)), Err(ImageEditError::NoImageReturned { .. })));
        assert!(DashScopeMultimodal.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_downloads_without_the_key() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = dashscope_call("qwen-image-2.0-pro", SizeParamStyle::WStarH);
        let answers = vec![json_response(200, r#"{"output":{"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":[{"image":"https://dashscope-result-sz.oss-cn-shenzhen.aliyuncs.com/r.png?Expires=1"}]}}]},"request_id":"r"}"#), image_response()];
        let (result, sent) = run_scripted(&DashScopeMultimodal, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(trail, [("https://dashscope-intl.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation", Some((AuthScheme::Bearer, "secret".to_string()))), ("https://dashscope-result-sz.oss-cn-shenzhen.aliyuncs.com/r.png?Expires=1", None)]);
    }
}
