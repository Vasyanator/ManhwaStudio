/*
File: crates/ms-ai-api/src/image_edit/adapters/tencent_tokenhub.rs

Purpose:
Adapter of Tencent Cloud `TokenHub` Hunyuan image generation (Bearer API key, no request
signing): one synchronous request per edit, answered with a signed result URL that is
downloaded without the key. The two models speak two different documented protocols.

Key structures:
- TencentTokenHub (the `EditProtocol`)

Notes:
- `hy-image-v3`: `POST {base}/wand/hunyuan-image/v3-generation` with `{model, prompt,
  images: [base64], size: "WxH", revise: false}`; answer `data[0].url`. The reference says the
  images "support image URLs or Base64" without naming a data-URL prefix, so plain base64 is
  sent (UNVERIFIED).
- `hy-image-v3.5-preview`: `POST {base}/wand/hunyuan-image/v35-generation` with the
  Chat/Messages body `{model, size, messages: [{role: "user", content: [{type: "text"},
  {type: "image_url", image_url: {url: data URL}}]}]}` ("http(s) ... or
  data:image/...;base64,... format"); answer `choices[0].delta.image.url`. "If a specific
  value is passed, the image is generated at the specified size." A failed final frame is a
  200 with `error {type, code, message}` (`content_filter` -> `Moderated`).
Both: HTTP 422 = "Input or output moderation failed" -> `Moderated`; 400 / 401 / 429 / 500
through `classify_error`. Result URLs are temporary (12 h) signed COS links. No cancel.
Sources: https://intl.cloud.tencent.com/document/product/1300/83708 (fetched 2026-10-04).
*/

use serde_json::{Value, json};

use super::{classify_error, is_success, job_failure, json_post, json_value, png_data_url, result_url_step};
use crate::encoding::base64_encode;
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, NextStep, StepCtx};

/// The model that uses the classic v3 protocol; every other id is the v3.5 Chat protocol.
const V3_MODEL: &str = "hy-image-v3";

/// The `TokenHub` Hunyuan image adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct TencentTokenHub;

impl EditProtocol for TencentTokenHub {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        if call.size_param != SizeParamStyle::WxH {
            return Err(ImageEditError::RequestBuild { detail: format!("TokenHub states the size only as \"WxH\"; it cannot state {:?}", call.size_param) });
        }
        let size = format!("{}x{}", call.width, call.height);
        let (path, body) = if call.model_id == V3_MODEL {
            ("v3-generation", json!({ "model": call.model_id, "prompt": call.prompt, "images": [base64_encode(&call.image_png)], "size": size, "revise": false }))
        } else {
            let content = json!([{ "type": "text", "text": call.prompt }, { "type": "image_url", "image_url": { "url": png_data_url(&call.image_png) } }]);
            ("v35-generation", json!({ "model": call.model_id, "size": size, "messages": [{ "role": "user", "content": content }] }))
        };
        Ok(json_post(format!("{}/wand/hunyuan-image/{path}", call.base_url), Vec::new(), body, AuthScheme::Bearer))
    }

    fn next(&self, _step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if response.status == 422 {
            return Err(ImageEditError::Moderated);
        }
        if !is_success(response.status) {
            return Err(classify_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            if error.get("code").and_then(Value::as_str) == Some("content_filter") {
                return Err(ImageEditError::Moderated);
            }
            return Err(job_failure(error.get("message").and_then(Value::as_str).unwrap_or("the generation failed")));
        }
        let url = value.pointer("/choices/0/delta/image/url").or_else(|| value.pointer("/data/0/url")).and_then(Value::as_str);
        match url {
            Some(url) => result_url_step(url),
            None => Err(ImageEditError::NoImageReturned { reason: "the answer has no choices[0].delta.image.url or data[0].url".to_string() }),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::TencentTokenHub;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    const BASE: &str = "https://tokenhub-intl.tencentcloudmaas.com/v1";

    fn tencent_call(model_id: &str, size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Tencent, model_id, BASE, None, (1024, 1024), size_param, None)
    }

    // The v3.5 "reference-based image generation" curl (`model`, `messages[0]` with a text and
    // an `image_url` part) plus the documented `size`, with a data URL.
    #[test]
    fn v35_request_matches_the_documented_body() {
        let spec = TencentTokenHub.submit(&tencent_call("hy-image-v3.5-preview", SizeParamStyle::WxH)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://tokenhub-intl.tencentcloudmaas.com/v1/wand/hunyuan-image/v35-generation", Some(AuthScheme::Bearer)));
        let expected = json!({ "model": "hy-image-v3.5-preview", "size": "1024x1024", "messages": [{ "role": "user", "content": [{ "type": "text", "text": "Remove the speech bubble text" }, { "type": "image_url", "image_url": { "url": "data:image/png;base64,UE5HREFUQQ==" } }] }] });
        assert_eq!(spec.body, HttpBody::Json(expected));
        assert!(matches!(TencentTokenHub.submit(&tencent_call("hy-image-v3.5-preview", SizeParamStyle::None)), Err(ImageEditError::RequestBuild { .. })));
    }

    // The v3 text-to-image curl (`model`, `prompt`, `size` "1024x1024") with the documented
    // `images` (reference images) and `revise` fields.
    #[test]
    fn v3_request_matches_the_documented_body() {
        let spec = TencentTokenHub.submit(&tencent_call("hy-image-v3", SizeParamStyle::WxH)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(spec.url, "https://tokenhub-intl.tencentcloudmaas.com/v1/wand/hunyuan-image/v3-generation");
        assert_eq!(spec.body, HttpBody::Json(json!({ "model": "hy-image-v3", "prompt": "Remove the speech bubble text", "images": ["UE5HREFUQQ=="], "size": "1024x1024", "revise": false })));
    }

    // Both sample responses, the v3.5 failure frame and the 422 moderation status.
    #[test]
    fn answers_and_errors() {
        let call = tencent_call("hy-image-v3.5-preview", SizeParamStyle::WxH);
        let step = submit_step(&call);
        let v35 = r#"{"id":"1374200352-WandImage-085edfe2367d4a688f68e813af3665a5","object":"image.chat.completion.chunk","created":1789720599,"model":"HY-Image-3.5-Preview-4090-Tob-v1.2","round":0,"choices":[{"index":0,"delta":{"type":"image","image":{"url":"https://aigc-output-image-file-1326893053.cos.ap-guangzhou.myqcloud.com/xxx/main.png?sign=1","width":4096,"height":4096,"source":"generate","tool_call_id":"generate@call_0"}},"finish_reason":null}],"usage":{"total_tokens":20000},"tokenhub_usage":{"total_tokens":20000},"request_id":"a1c00d07-041b-4daa-bae0-b7eabf2bc33a"}"#;
        assert!(matches!(TencentTokenHub.next(&step, &json_response(200, v35)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.starts_with("https://aigc-output-image-file-1326893053.cos.ap-guangzhou.myqcloud.com/")));
        let v3 = r#"{"id":"4-WandImage-a786becfdc80433b8cff4aa344c8fd3d","created":1785125529,"data":[{"url":"https://aigc-image.cos.myqcloud.com/xxx/result.png","revised_prompt":"An orange kitten"}],"request_id":"3aec3299-06ad-4654-8b45-c57b823a15d2","tokenhub_usage":{"total_tokens":1024}}"#;
        assert!(matches!(TencentTokenHub.next(&step, &json_response(200, v3)), Ok(NextStep::Download(spec)) if spec.url == "https://aigc-image.cos.myqcloud.com/xxx/result.png"));
        let filtered = r#"{"id":"abc123","object":"image.chat.completion.chunk","created":1785125530,"model":"HY-Image-3.5-Preview-4090-Tob-v1.2","round":0,"choices":[{"index":0,"delta":{},"finish_reason":"error"}],"error":{"type":"invalid_request_error","code":"content_filter","message":"input moderation rejected","request_id":"xxxxxxxx"}}"#;
        assert!(matches!(TencentTokenHub.next(&step, &json_response(200, filtered)), Err(ImageEditError::Moderated)));
        assert!(matches!(TencentTokenHub.next(&step, &json_response(422, r#"{"error":{"message":"blocked"}}"#)), Err(ImageEditError::Moderated)));
        assert!(matches!(TencentTokenHub.next(&step, &json_response(401, r#"{"error":{"message":"Authentication failed."}}"#)), Err(ImageEditError::KeyRejected)));
        assert!(matches!(TencentTokenHub.next(&step, &json_response(429, r#"{"error":{"message":"The number of concurrent requests exceeds the limit."}}"#)), Err(ImageEditError::RateLimited)));
        assert!(TencentTokenHub.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_downloads_without_the_key() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = tencent_call("hy-image-v3", SizeParamStyle::WxH);
        let answers = vec![json_response(200, r#"{"id":"4-WandImage-x","created":1785125529,"data":[{"url":"https://aigc-image.cos.myqcloud.com/x/result.png"}],"request_id":"r"}"#), image_response()];
        let (result, sent) = run_scripted(&TencentTokenHub, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(trail, [("https://tokenhub-intl.tencentcloudmaas.com/v1/wand/hunyuan-image/v3-generation", Some((AuthScheme::Bearer, "secret".to_string()))), ("https://aigc-image.cos.myqcloud.com/x/result.png", None)]);
    }
}
