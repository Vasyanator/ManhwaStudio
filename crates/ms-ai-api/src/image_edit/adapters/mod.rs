/*
File: crates/ms-ai-api/src/image_edit/adapters/mod.rs

Purpose:
The per-shape image-edit adapters (pure `EditProtocol` state machines, one per `ApiShape`) and
the helpers they share: the shape -> adapter dispatch, the provider-error classifier, the
`OpenAI`-style `data[]` image answer and data URLs.

Key functions:
- protocol_for(shape)   : the adapter of an `ApiShape` (every shape has one).
- classify_error()      : HTTP status + error body -> typed `ImageEditError`.
- job_failure()         : a failed async job's message -> `Moderated` | `ProviderFailed`.
- images_data_step()    : `{ "data": [{ "b64_json" | "url" }] }` -> `NextStep` (a `data:` url is
                          decoded via `result_url_step`).
- result_url_step()     : a result URL -> inline `data:` decode or an unauthenticated download.
- json_value(), required_str(), get_request(), json_post(), png_data_url(), mask_data_url(),
  mask_has_both_regions(): small shared builders / readers.

Submodules (synchronous):
- `openai_images`    : `POST {base}/images/edits` multipart (OpenAI, DeepInfra, aimlapi,
                       AITunnel, ProxyAPI, the user's own server).
- `openrouter_images`: `POST {base}/images` JSON with `input_references` (OpenRouter, RouterAI).
- `gemini_generate`  : `POST {base}/models/{id}:generateContent` with `imageConfig`.
- `ark_images`, `together_images`, `xai_images`, `recraft_edit`,
  `dashscope_multimodal`, `tencent_tokenhub`.
Submodules (submit, then poll):
- `bfl_async`, `fal_queue`, `replicate_predictions`, `ideogram_edit`, `runware_tasks`,
  `polza_media`, `genapi_async`, `luma_generations`, `runway_tasks`, `kling_image`.

Notes:
Adapters are pure and target-neutral: they never see a key (they name an `AuthScheme`) and
never perform I/O, so they are tested against documented request / response examples. Provider
error messages are passed through truncated; no adapter error carries the prompt.
*/

pub mod ark_images;
pub mod bfl_async;
pub mod dashscope_multimodal;
pub mod fal_queue;
pub mod gemini_generate;
pub mod genapi_async;
pub mod ideogram_edit;
pub mod kling_image;
pub mod luma_generations;
pub mod openai_images;
pub mod openrouter_images;
pub mod polza_media;
pub mod recraft_edit;
pub mod replicate_predictions;
pub mod runware_tasks;
pub mod runway_tasks;
pub mod tencent_tokenhub;
pub mod together_images;
pub mod xai_images;

use serde_json::Value;

use super::codec::{MaskPolarity, encode_mask_png};
use super::error::ImageEditError;
use super::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, NextStep};
use super::provider::ApiShape;
use crate::encoding::{base64_decode, base64_encode};

/// Longest provider message kept in an error, in characters.
const MAX_PROVIDER_MESSAGE_CHARS: usize = 500;

/// Body markers of a provider refusing the user's country or region (`OpenAI` 403
/// `unsupported_country_region_territory`, Gemini 400 "User location is not supported").
const REGION_MARKERS: [&str; 3] = ["unsupported_country_region_territory", "user location is not supported", "country, region, or territory not supported"];
/// Body markers of a content-filter refusal (`OpenAI` `moderation_blocked`, `OpenRouter` 403
/// "requires moderation ... input was flagged").
const MODERATION_MARKERS: [&str; 4] = ["moderation_blocked", "content_policy_violation", "requires moderation", "input was flagged"];
/// Extra markers of a content-filter refusal in a failed job's message (status bodies of the
/// async shapes: Replicate "NSFW content detected", Luma `content_moderated`, Polza
/// "content policy violation", Runway `SAFETY.*`).
const JOB_MODERATION_MARKERS: [&str; 5] = ["nsfw", "content_moderated", "content policy", "safety", "moderat"];
/// Body markers of an exhausted balance (`OpenAI` 429 `insufficient_quota`, 400
/// `billing_hard_limit_reached`).
const CREDIT_MARKERS: [&str; 4] = ["insufficient_quota", "billing_hard_limit_reached", "insufficient credits", "insufficient balance"];
/// Body markers of a rejected key sent with a 400 (Gemini `API_KEY_INVALID`).
const KEY_MARKERS: [&str; 4] = ["api_key_invalid", "api key not valid", "invalid_api_key", "incorrect api key"];

/// The adapter of `shape`. Every `ApiShape` has exactly one (exhaustive, no fallback arm).
#[must_use]
pub fn protocol_for(shape: ApiShape) -> &'static (dyn EditProtocol + Sync) {
    match shape {
        ApiShape::OpenAiImages => &openai_images::OpenAiImages,
        ApiShape::OpenRouterImages => &openrouter_images::OpenRouterImages,
        ApiShape::GeminiGenerate => &gemini_generate::GeminiGenerate,
        ApiShape::ArkImages => &ark_images::ArkImages,
        ApiShape::BflAsync => &bfl_async::BflAsync,
        ApiShape::XaiImages => &xai_images::XaiImages,
        ApiShape::IdeogramEdit => &ideogram_edit::IdeogramEdit,
        ApiShape::RecraftEdit => &recraft_edit::RecraftEdit,
        ApiShape::RunwayTasks => &runway_tasks::RunwayTasks,
        ApiShape::LumaGenerations => &luma_generations::LumaGenerations,
        ApiShape::DashScopeMultimodal => &dashscope_multimodal::DashScopeMultimodal,
        ApiShape::TencentTokenHub => &tencent_tokenhub::TencentTokenHub,
        ApiShape::KlingImage => &kling_image::KlingImage,
        ApiShape::FalQueue => &fal_queue::FalQueue,
        ApiShape::ReplicatePredictions => &replicate_predictions::ReplicatePredictions,
        ApiShape::TogetherImages => &together_images::TogetherImages,
        ApiShape::RunwareTasks => &runware_tasks::RunwareTasks,
        ApiShape::PolzaMedia => &polza_media::PolzaMedia,
        ApiShape::GenApiAsync => &genapi_async::GenApiAsync,
    }
}

/// Whether `status` is a 2xx success.
#[must_use]
pub fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Maps a non-success response to its typed error. Body markers win over the status (a region
/// block arrives as 403 at `OpenAI` and 400 at Gemini); then 401 / 403 = `KeyRejected`, 402 =
/// `OutOfCredits`, 429 = `RateLimited`, other 4xx = `ProviderRejected` with the provider's
/// message, everything else = `ProviderFailed`.
#[must_use]
pub fn classify_error(status: u16, body: &[u8]) -> ImageEditError {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_lowercase();
    let has = |markers: &[&str]| markers.iter().any(|marker| lower.contains(marker));
    if has(&REGION_MARKERS) {
        return ImageEditError::RegionBlocked;
    }
    if has(&MODERATION_MARKERS) {
        return ImageEditError::Moderated;
    }
    if has(&CREDIT_MARKERS) {
        return ImageEditError::OutOfCredits;
    }
    let message = serde_json::from_slice::<Value>(body).ok().and_then(|value| provider_message(&value)).unwrap_or_else(|| truncate_chars(text.trim()));
    match status {
        401 | 403 => ImageEditError::KeyRejected,
        402 => ImageEditError::OutOfCredits,
        429 => ImageEditError::RateLimited,
        400 if has(&KEY_MARKERS) => ImageEditError::KeyRejected,
        400..=499 => ImageEditError::ProviderRejected { status, message },
        _ => ImageEditError::ProviderFailed { detail: format!("HTTP {status}: {message}") },
    }
}

/// The provider's own error text from a JSON error body: `error.message`, a string `error`, a
/// top-level `message` / `detail`, fal's `detail[0].msg` or Runware's `errors[0].message`;
/// truncated.
#[must_use]
pub fn provider_message(value: &Value) -> Option<String> {
    let error = value.get("error");
    let text = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| error.and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("detail").and_then(Value::as_str))
        .or_else(|| value.pointer("/detail/0/msg").and_then(Value::as_str))
        .or_else(|| value.pointer("/errors/0/message").and_then(Value::as_str))?;
    Some(truncate_chars(text.trim()))
}

/// A provider job that ended in a failure state with `message` (status bodies, not HTTP
/// errors): content-filter wording is `Moderated`, anything else `ProviderFailed`.
#[must_use]
pub fn job_failure(message: &str) -> ImageEditError {
    let lower = message.to_lowercase();
    if MODERATION_MARKERS.iter().chain(JOB_MODERATION_MARKERS.iter()).any(|marker| lower.contains(marker)) {
        return ImageEditError::Moderated;
    }
    ImageEditError::ProviderFailed { detail: truncate_chars(message.trim()) }
}

/// The string at JSON `pointer` of a provider answer.
///
/// # Errors
/// `ImageEditError::ProviderFailed` naming `pointer` when it is missing or not a string.
pub fn required_str<'v>(value: &'v Value, pointer: &str) -> Result<&'v str, ImageEditError> {
    value.pointer(pointer).and_then(Value::as_str).ok_or_else(|| ImageEditError::ProviderFailed { detail: format!("the answer has no string at {pointer}") })
}

/// An unauthenticated or authenticated GET request description.
#[must_use]
pub fn get_request(url: String, headers: Vec<(&'static str, String)>, auth: Option<AuthScheme>) -> HttpRequestSpec {
    HttpRequestSpec { method: HttpMethod::Get, url, headers, body: HttpBody::Empty, auth }
}

/// A JSON POST request description.
#[must_use]
pub fn json_post(url: String, headers: Vec<(&'static str, String)>, body: Value, auth: AuthScheme) -> HttpRequestSpec {
    HttpRequestSpec { method: HttpMethod::Post, url, headers, body: HttpBody::Json(body), auth: Some(auth) }
}

/// The final step for a result URL a provider answered with: an inline `data:...;base64,` URL
/// is decoded here; anything else is downloaded WITHOUT auth (signed CDN links).
///
/// # Errors
/// `ImageEditError::Decode` for a malformed data URL.
pub fn result_url_step(url: &str) -> Result<NextStep, ImageEditError> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or_else(|| ImageEditError::Decode { detail: "data URL without a comma".to_string() })?;
        if !meta.ends_with(";base64") {
            return Err(ImageEditError::Decode { detail: format!("data URL is not base64 ({meta})") });
        }
        return Ok(NextStep::Image(decode_base64_image(data)?));
    }
    Ok(NextStep::Download(get_request(url.to_string(), Vec::new(), None)))
}

/// The native mask of `call` as a PNG data URL in `polarity`, or `None` when the call has none.
///
/// # Errors
/// `ShapeMismatch` / `Encode` from `codec::encode_mask_png`.
pub fn mask_data_url(call: &EditCall, polarity: MaskPolarity) -> Result<Option<String>, ImageEditError> {
    call.mask.as_deref().map(|mask| encode_mask_png(mask, call.width, call.height, polarity).map(|png| png_data_url(&png))).transpose()
}

/// Whether a native mask (255 = editable) has both editable and kept pixels. Providers that
/// require both regions (Ideogram) get no mask when this is false: an all-editable mask is the
/// same request as no mask, and an all-kept one never reaches an adapter.
#[must_use]
pub fn mask_has_both_regions(mask: &[u8]) -> bool {
    mask.iter().any(|&value| value != 0) && mask.contains(&0)
}

/// `text` cut to `MAX_PROVIDER_MESSAGE_CHARS` characters (a `...` marks the cut).
fn truncate_chars(text: &str) -> String {
    if text.chars().count() <= MAX_PROVIDER_MESSAGE_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_PROVIDER_MESSAGE_CHARS).collect();
    cut.push_str("...");
    cut
}

/// Parses a success body as JSON.
///
/// # Errors
/// `ImageEditError::ProviderFailed` when the body is not JSON.
pub fn json_value(response: &HttpResponse) -> Result<Value, ImageEditError> {
    serde_json::from_slice(&response.body).map_err(|error| ImageEditError::ProviderFailed { detail: format!("HTTP {} answer is not JSON: {error}", response.status) })
}

/// Decodes a base64 image payload from a provider answer.
///
/// # Errors
/// `ImageEditError::Decode` for invalid base64.
pub fn decode_base64_image(text: &str) -> Result<Vec<u8>, ImageEditError> {
    base64_decode(text.trim()).map_err(|error| ImageEditError::Decode { detail: format!("base64 image: {error}") })
}

/// The next step of an `OpenAI`-style images answer: an error status is classified; a success
/// takes `data[0].b64_json` inline, else hands `data[0].url` to `result_url_step` (an inline
/// `data:` URL is decoded locally, any other URL downloaded WITHOUT auth: result URLs are
/// signed CDN links, never the provider's API host).
///
/// # Errors
/// The classified provider error, `ProviderFailed` for a non-JSON body, `NoImageReturned` when
/// `data` holds neither field, `Decode` for invalid base64.
pub fn images_data_step(response: &HttpResponse) -> Result<NextStep, ImageEditError> {
    if !is_success(response.status) {
        return Err(classify_error(response.status, &response.body));
    }
    let value = json_value(response)?;
    let first = value.get("data").and_then(Value::as_array).and_then(|data| data.first());
    if let Some(b64) = first.and_then(|item| item.get("b64_json")).and_then(Value::as_str) {
        return Ok(NextStep::Image(decode_base64_image(b64)?));
    }
    if let Some(url) = first.and_then(|item| item.get("url")).and_then(Value::as_str) {
        return result_url_step(url);
    }
    let reason = provider_message(&value).unwrap_or_else(|| "the answer has no data[0].b64_json or data[0].url".to_string());
    Err(ImageEditError::NoImageReturned { reason })
}

/// `data:image/png;base64,{png}`.
#[must_use]
pub fn png_data_url(png: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64_encode(png))
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::protocol::{EditCall, HttpResponse, StepCtx, StepPhase};
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::size_rule::AspectTierEntry;

    /// A canned call: tiny fake PNG bytes keep the expected bodies readable.
    pub(crate) fn call(provider: ImageEditProvider, model_id: &str, base_url: &str, mask: Option<Vec<u8>>, size: (u32, u32), size_param: SizeParamStyle, size_entry: Option<AspectTierEntry>) -> EditCall {
        EditCall {
            provider,
            model_id: model_id.to_string(),
            base_url: base_url.to_string(),
            region_id: None,
            prompt: "Remove the speech bubble text".to_string(),
            image_png: b"PNGDATA".to_vec(),
            mask,
            width: size.0,
            height: size.1,
            size_param,
            size_entry,
        }
    }

    /// The submit-response step of `call`.
    pub(crate) fn submit_step(call: &EditCall) -> StepCtx<'_> {
        StepCtx { call, phase: StepPhase::Submit, polls: 0, job: None }
    }

    /// A JSON response.
    pub(crate) fn json_response(status: u16, body: &str) -> HttpResponse {
        HttpResponse { status, content_type: Some("application/json".to_string()), body: body.as_bytes().to_vec() }
    }

    /// The step answering poll number `polls` (stage `stage`) with the kept `job`.
    pub(crate) fn poll_step<'a>(call: &'a EditCall, stage: u8, polls: u32, job: Option<&'a crate::image_edit::protocol::JobRef>) -> StepCtx<'a> {
        StepCtx { call, phase: StepPhase::Poll { stage }, polls, job }
    }

    /// Runs `protocol` for `call` through the real executor over a scripted transport (no
    /// network, no delays, key "secret"); returns the result and every request sent.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn run_scripted(protocol: &dyn crate::image_edit::protocol::EditProtocol, call: &EditCall, answers: Vec<HttpResponse>) -> (Result<Vec<u8>, crate::image_edit::error::ImageEditError>, Vec<crate::image_edit::executor::tests::Sent>) {
        use crate::image_edit::executor::tests::{FakeTransport, instant_limits};
        use crate::image_edit::executor::{RunContext, execute};
        use crate::image_edit::request::CancelFlag;
        let transport = FakeTransport::new(answers.into_iter().map(Ok).collect());
        let cancel = CancelFlag::new();
        let result = execute(&transport, &instant_limits(), protocol, &RunContext { call, key: "secret", cancel: &cancel }, &mut |_| {});
        (result, transport.sent.into_inner())
    }

    /// An image body as a provider's CDN would serve it.
    pub(crate) fn image_response() -> HttpResponse {
        HttpResponse { status: 200, content_type: Some("image/png".to_string()), body: b"IMAGEBYTES".to_vec() }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{call, json_response};
    use super::{classify_error, images_data_step, job_failure, mask_has_both_regions, protocol_for, provider_message, result_url_step};
    use crate::image_edit::catalog::{MaskSupport, SizeParamStyle, all_offers};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{HttpMethod, NextStep};

    // Every catalogue offer's provider builds a request through its shape's adapter (a smoke
    // test of the dispatch: each adapter accepts the size style its offers name).
    #[test]
    fn every_offer_reaches_an_adapter_that_accepts_its_size_style() {
        for offer in all_offers() {
            let size_entry = match offer.size_param {
                SizeParamStyle::AspectTier(entries) => entries.first().copied(),
                SizeParamStyle::WxH | SizeParamStyle::WStarH | SizeParamStyle::WidthHeight | SizeParamStyle::ImageSizeObject | SizeParamStyle::None => None,
            };
            let (width, height) = size_entry.map_or((1024, 1024), |entry| (entry.width, entry.height));
            let mask = match offer.mask {
                MaskSupport::HardRequired => Some(vec![255; 1024 * 1024]),
                MaskSupport::None | MaskSupport::Soft | MaskSupport::Hard => None,
            };
            let model_id = if offer.model_id.is_empty() { "custom-model" } else { offer.model_id };
            let call = call(offer.provider, model_id, "https://api.example.com/v1", mask, (width, height), offer.size_param, size_entry);
            let spec = protocol_for(offer.provider.info().shape).submit(&call);
            assert!(spec.is_ok(), "{offer:?}: {spec:?}");
        }
    }

    #[test]
    fn job_failures_and_result_urls() {
        assert!(matches!(job_failure("NSFW content detected. Try running it again, or try a different prompt."), ImageEditError::Moderated));
        assert!(matches!(job_failure("CUDA out of memory"), ImageEditError::ProviderFailed { detail } if detail == "CUDA out of memory"));
        assert_eq!(result_url_step("data:image/png;base64,AQID").ok(), Some(NextStep::Image(vec![1, 2, 3])));
        assert!(matches!(result_url_step("data:image/png,raw"), Err(ImageEditError::Decode { .. })));
        assert!(matches!(result_url_step("https://cdn.example.net/x.png"), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.method == HttpMethod::Get));
        assert!(mask_has_both_regions(&[0, 255]));
        assert!(!mask_has_both_regions(&[255, 255]));
        // fal / Runware error shapes reach the provider message.
        assert_eq!(provider_message(&serde_json::json!({"detail":[{"loc":["body"],"msg":"Image too small","type":"image_too_small"}]})).as_deref(), Some("Image too small"));
        assert_eq!(provider_message(&serde_json::json!({"errors":[{"code":"invalidApiKey","message":"Invalid API key."}]})).as_deref(), Some("Invalid API key."));
    }

    // OpenAI's documented error object (https://platform.openai.com/docs/guides/error-codes).
    #[test]
    fn openai_error_bodies_are_typed() {
        let region = br#"{"error":{"message":"Country, region, or territory not supported","type":"request_forbidden","param":null,"code":"unsupported_country_region_territory"}}"#;
        assert!(matches!(classify_error(403, region), ImageEditError::RegionBlocked));
        let key = br#"{"error":{"message":"Incorrect API key provided: sk-abc***. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error","param":null,"code":"invalid_api_key"}}"#;
        assert!(matches!(classify_error(401, key), ImageEditError::KeyRejected));
        let quota = br#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#;
        assert!(matches!(classify_error(429, quota), ImageEditError::OutOfCredits));
        let rate = br#"{"error":{"message":"Rate limit reached for gpt-image-2 in organization org-x on images per min (IPM): Limit 5.","type":"requests","param":null,"code":"rate_limit_exceeded"}}"#;
        assert!(matches!(classify_error(429, rate), ImageEditError::RateLimited));
        let moderated = br#"{"error":{"message":"Your request was rejected by the safety system.","type":"image_generation_user_error","param":null,"code":"moderation_blocked"}}"#;
        assert!(matches!(classify_error(400, moderated), ImageEditError::Moderated));
        let size = br#"{"error":{"message":"Invalid size '1000x1000'. Width and height must be divisible by 16.","type":"invalid_request_error","param":"size","code":"invalid_value"}}"#;
        assert!(matches!(classify_error(400, size), ImageEditError::ProviderRejected { status: 400, message } if message.starts_with("Invalid size '1000x1000'")));
    }

    #[test]
    fn non_json_and_server_errors() {
        assert!(matches!(classify_error(502, b"<html>Bad Gateway</html>"), ImageEditError::ProviderFailed { detail } if detail == "HTTP 502: <html>Bad Gateway</html>"));
        assert!(matches!(classify_error(402, br#"{"error":{"code":402,"message":"Insufficient credits"}}"#), ImageEditError::OutOfCredits));
        let long = "x".repeat(2000);
        assert!(matches!(classify_error(400, long.as_bytes()), ImageEditError::ProviderRejected { message, .. } if message.chars().count() == 503));
    }

    #[test]
    fn images_data_takes_b64_or_downloads_the_url_without_auth() {
        let inline = images_data_step(&json_response(200, r#"{"created":1,"data":[{"b64_json":"AQID"}]}"#));
        assert_eq!(inline.ok(), Some(NextStep::Image(vec![1, 2, 3])));
        // aimlapi's documented answer: `b64_json: null` plus a CDN `url`.
        let url = images_data_step(&json_response(200, r#"{"data":[{"b64_json":null,"url":"https://cdn.aimlapi.com/generations/x.png"}],"meta":{"usage":{"credits_used":190450}}}"#));
        match url {
            Ok(NextStep::Download(spec)) => {
                assert_eq!(spec.method, HttpMethod::Get);
                assert_eq!(spec.url, "https://cdn.aimlapi.com/generations/x.png");
                assert!(spec.auth.is_none());
            }
            other => panic!("{other:?}"),
        }
        // An OpenAI-compatible server answering with an inline data URL: decoded, never fetched.
        let data_url = images_data_step(&json_response(200, r#"{"data":[{"url":"data:image/png;base64,AQID"}]}"#));
        assert_eq!(data_url.ok(), Some(NextStep::Image(vec![1, 2, 3])));
        assert!(matches!(images_data_step(&json_response(200, r#"{"data":[{"url":"data:image/png,raw"}]}"#)), Err(ImageEditError::Decode { .. })));
        assert!(matches!(images_data_step(&json_response(200, r#"{"data":[]}"#)), Err(ImageEditError::NoImageReturned { .. })));
        assert!(matches!(images_data_step(&json_response(200, r#"{"data":[{"b64_json":"!!"}]}"#)), Err(ImageEditError::Decode { .. })));
        assert!(matches!(images_data_step(&json_response(200, "not json")), Err(ImageEditError::ProviderFailed { .. })));
    }
}
