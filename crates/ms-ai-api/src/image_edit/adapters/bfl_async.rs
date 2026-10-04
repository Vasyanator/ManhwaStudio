/*
File: crates/ms-ai-api/src/image_edit/adapters/bfl_async.rs

Purpose:
Adapter of the Black Forest Labs API (`POST {base}/v1/{model}`, `x-key` header): submit a JSON
job, poll the returned `polling_url` until `Ready`, download the signed `result.sample` URL.

Key structures:
- BflAsync (the `EditProtocol`)

Notes:
Bodies per model family (api.bfl.ai OpenAPI schemas `Flux2Inputs`, `FluxKontextProInputs`,
`FluxProFillInputs`, `Flux3ImageInputs`): FLUX.2 `{prompt, input_image, width?, height?,
output_format}`, Kontext `{prompt, input_image, output_format}`, Fill `{image, mask, prompt,
output_format}` (mask white = inpaint), FLUX 3 `{prompt, images[], aspect_ratio: "auto"}` (no
`output_format` field). Images are sent as raw base64: documented for Kontext ("Base64 encoded
image or URL"), Fill ("A Base64-encoded string") and FLUX 3 ("an http(s) URL or base64"); for
FLUX.2 it is UNVERIFIED (`Flux2Inputs.input_image` says only "Path to the input image." and
every FLUX.2 editing sample sends URLs; a live probe settles it). `output_format` is
forced to `png` where the schema has it (defaults are jpeg). The submit answers
`{id, polling_url}`; the polling URL MUST be used as returned and may name another regional
`*.bfl.ai` host, which the provider's `auth_host_suffixes` lets the executor key. Result
statuses: Pending / Reasoning / Generating poll on; Ready -> `result.sample` (delivery host,
downloaded without the key); Request Moderated / Content Moderated -> `Moderated`; Error /
Failed / Task not found -> `ProviderFailed`. No cancel endpoint.
Sources: https://api.bfl.ai/openapi.json (paths, schemas, `StatusResponse`),
https://docs.bfl.ml/api_integration/integration_guidelines.md (polling URL, delivery hosts),
https://docs.bfl.ml/api_integration/errors.md (statuses, moderation body), fetched 2026-10-04.
*/

use std::time::Duration;

use serde_json::{Value, json};

use super::{classify_error, get_request, is_success, json_post, json_value, job_failure, refuse_reference, required_str, result_url_step};
use crate::encoding::base64_encode;
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx, StepPhase};

/// The API key header.
const AUTH: AuthScheme = AuthScheme::Header("x-key");

/// The Black Forest Labs async adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct BflAsync;

/// The request body family of a BFL model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Flux2,
    Kontext,
    Fill,
    Flux3,
}

/// The body family of `model_id` (the catalogue's BFL ids).
fn family(model_id: &str) -> Option<Family> {
    match model_id {
        "flux-2-pro" | "flux-2-flex" | "flux-2-max" => Some(Family::Flux2),
        "flux-kontext-pro" | "flux-kontext-max" => Some(Family::Kontext),
        "flux-pro-1.0-fill" => Some(Family::Fill),
        "flux-3-image" => Some(Family::Flux3),
        _ => None,
    }
}

/// `RequestBuild` for a size style `family` cannot state.
fn unsupported_size(call: &EditCall) -> ImageEditError {
    ImageEditError::RequestBuild { detail: format!("BFL {} cannot state the size as {:?}", call.model_id, call.size_param) }
}

impl EditProtocol for BflAsync {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        let family = family(&call.model_id).ok_or_else(|| ImageEditError::RequestBuild { detail: format!("unknown BFL model {}", call.model_id) })?;
        let image = base64_encode(&call.image_png);
        // Only FLUX 3's `images` is a list; the other families name single image fields.
        if family != Family::Flux3 {
            refuse_reference(call)?;
        }
        let body = match family {
            Family::Flux2 => {
                let mut body = json!({ "prompt": call.prompt, "input_image": image, "output_format": "png" });
                match call.size_param {
                    SizeParamStyle::WidthHeight => {
                        body["width"] = json!(call.width);
                        body["height"] = json!(call.height);
                    }
                    SizeParamStyle::None => {}
                    SizeParamStyle::WxH | SizeParamStyle::WStarH | SizeParamStyle::ImageSizeObject | SizeParamStyle::AspectTier(_) => return Err(unsupported_size(call)),
                }
                body
            }
            // Kontext matches the input size, Fill keeps it, FLUX 3 has tier-only sizing: no
            // size field (`SizeParamStyle::None` is the only style these rows carry).
            Family::Kontext | Family::Fill | Family::Flux3 if call.size_param != SizeParamStyle::None => return Err(unsupported_size(call)),
            Family::Kontext => json!({ "prompt": call.prompt, "input_image": image, "output_format": "png" }),
            Family::Fill => {
                let mask = call.mask.as_deref().ok_or_else(|| ImageEditError::RequestBuild { detail: "FLUX.1 Fill requires a mask".to_string() })?;
                let mask = base64_encode(&encode_mask_png(mask, call.width, call.height, MaskPolarity::WhiteEdits)?);
                json!({ "image": image, "mask": mask, "prompt": call.prompt, "output_format": "png" })
            }
            // The edited image first: `auto` "keeps the first reference image's framing".
            Family::Flux3 => {
                let images: Vec<String> = std::iter::once(image).chain(call.reference_png.as_deref().map(base64_encode)).collect();
                json!({ "prompt": call.prompt, "images": images, "aspect_ratio": "auto" })
            }
        };
        Ok(json_post(format!("{}/v1/{}", call.base_url, call.model_id), Vec::new(), body, AUTH))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(classify_error(response.status, &response.body));
        }
        let value = json_value(response)?;
        match step.phase {
            StepPhase::Submit => {
                let id = required_str(&value, "/id")?.to_string();
                let polling_url = required_str(&value, "/polling_url")?.to_string();
                Ok(NextStep::Poll { request: get_request(polling_url.clone(), Vec::new(), Some(AUTH)), after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: Some(polling_url), cancel_url: None }) })
            }
            StepPhase::Poll { .. } => result_step(step, &value),
        }
    }

    fn cancel_request(&self, _step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        None
    }
}

/// The next step after a `get_result` answer.
fn result_step(step: &StepCtx<'_>, value: &Value) -> Result<NextStep, ImageEditError> {
    let status = required_str(value, "/status")?;
    match status {
        "Pending" | "Reasoning" | "Generating" => {
            let url = step.job.and_then(|job| job.poll_url.clone()).ok_or_else(|| ImageEditError::RequestBuild { detail: "BFL poll without a kept polling URL".to_string() })?;
            Ok(NextStep::Poll { request: get_request(url, Vec::new(), Some(AUTH)), after: Duration::ZERO, stage: 0, job: None })
        }
        "Ready" => result_url_step(required_str(value, "/result/sample")?),
        "Request Moderated" | "Content Moderated" => Err(ImageEditError::Moderated),
        "Task not found" => Err(ImageEditError::ProviderFailed { detail: "BFL: task not found or expired".to_string() }),
        // `Error`, the guide's `Failed`, and any status this adapter does not know.
        other => {
            let details = value.get("details").filter(|details| !details.is_null()).map(Value::to_string).unwrap_or_default();
            Err(job_failure(&format!("BFL status {other} {details}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::BflAsync;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn bfl_call(model: &str, mask: Option<Vec<u8>>, size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Bfl, model, "https://api.eu.bfl.ai", mask, (2, 2), size_param, None)
    }

    // The FLUX.2 editing sample `{prompt, input_image}` (https://docs.bfl.ml/flux_2/flux2_image_editing.md)
    // plus the schema's `width` / `height` and `output_format` (https://api.bfl.ai/openapi.json `Flux2Inputs`).
    #[test]
    fn flux2_request_states_width_and_height() {
        let call = bfl_call("flux-2-pro", None, SizeParamStyle::WidthHeight);
        let spec = BflAsync.submit(&call).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api.eu.bfl.ai/v1/flux-2-pro", Some(AuthScheme::Header("x-key"))));
        assert_eq!(spec.body, HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "input_image": "UE5HREFUQQ==", "output_format": "png", "width": 2, "height": 2 })));
        assert!(matches!(BflAsync.submit(&bfl_call("flux-2-pro", None, SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
        assert!(matches!(BflAsync.submit(&bfl_call("flux-1-dev", None, SizeParamStyle::None)), Err(ImageEditError::RequestBuild { .. })));
    }

    // Kontext `{prompt, input_image: "<base64>"}` and Fill `{image, mask, prompt}` samples
    // (https://docs.bfl.ml/kontext/kontext_image_editing.md, https://docs.bfl.ml/flux_1_fill.md);
    // FLUX 3 `images` + `aspect_ratio: auto` (`Flux3ImageInputs`).
    #[test]
    fn kontext_fill_and_flux3_bodies() {
        let kontext = BflAsync.submit(&bfl_call("flux-kontext-max", None, SizeParamStyle::None)).map(|spec| spec.body).ok();
        assert_eq!(kontext, Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "input_image": "UE5HREFUQQ==", "output_format": "png" }))));
        let mask = vec![0, 255, 255, 0];
        let fill = BflAsync.submit(&bfl_call("flux-pro-1.0-fill", Some(mask.clone()), SizeParamStyle::None)).map(|spec| spec.body).ok();
        let mask_b64 = crate::encoding::base64_encode(&encode_mask_png(&mask, 2, 2, MaskPolarity::WhiteEdits).unwrap_or_default());
        assert_eq!(fill, Some(HttpBody::Json(json!({ "image": "UE5HREFUQQ==", "mask": mask_b64, "prompt": "Remove the speech bubble text", "output_format": "png" }))));
        assert!(matches!(BflAsync.submit(&bfl_call("flux-pro-1.0-fill", None, SizeParamStyle::None)), Err(ImageEditError::RequestBuild { .. })));
        let flux3 = BflAsync.submit(&bfl_call("flux-3-image", None, SizeParamStyle::None)).map(|spec| spec.body).ok();
        assert_eq!(flux3, Some(HttpBody::Json(json!({ "prompt": "Remove the speech bubble text", "images": ["UE5HREFUQQ=="], "aspect_ratio": "auto" }))));
    }

    // Status bodies: `ResultResponse` shape and the moderation example of the errors page.
    #[test]
    fn status_machine() {
        let call = bfl_call("flux-2-pro", None, SizeParamStyle::WidthHeight);
        let submitted = BflAsync.next(&submit_step(&call), &json_response(200, r#"{"id":"task-1","polling_url":"https://api.eu4.bfl.ai/v1/get_result?id=task-1","cost":4.5}"#));
        let job = JobRef { id: "task-1".to_string(), poll_url: Some("https://api.eu4.bfl.ai/v1/get_result?id=task-1".to_string()), cancel_url: None };
        match submitted {
            Ok(NextStep::Poll { request, job: Some(kept), .. }) => {
                assert_eq!((request.url.as_str(), request.auth), ("https://api.eu4.bfl.ai/v1/get_result?id=task-1", Some(AuthScheme::Header("x-key"))));
                assert_eq!(kept, job);
            }
            other => panic!("{other:?}"),
        }
        let step = poll_step(&call, 0, 1, Some(&job));
        for status in ["Pending", "Reasoning", "Generating"] {
            let body = format!(r#"{{"id":"task-1","status":"{status}","result":null,"progress":0.4}}"#);
            assert!(matches!(BflAsync.next(&step, &json_response(200, &body)), Ok(NextStep::Poll { request, job: None, .. }) if request.url == job.poll_url.clone().unwrap_or_default()));
        }
        let ready = r#"{"id":"task-1","status":"Ready","result":{"sample":"https://delivery-eu4.bfl.ai/results/x/sample.png?se=2026&sig=abc","prompt":"p"}}"#;
        assert!(matches!(BflAsync.next(&step, &json_response(200, ready)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.starts_with("https://delivery-eu4.bfl.ai/")));
        let moderated = r#"{"id":"your-task-id","status":"Request Moderated","result":null,"progress":null,"details":{"Moderation Reasons":["Violence","Sexual Content","Self Harm"]},"preview":null}"#;
        assert!(matches!(BflAsync.next(&step, &json_response(200, moderated)), Err(ImageEditError::Moderated)));
        let content = moderated.replace("Request Moderated", "Content Moderated");
        assert!(matches!(BflAsync.next(&step, &json_response(200, &content)), Err(ImageEditError::Moderated)));
        assert!(matches!(BflAsync.next(&step, &json_response(200, r#"{"id":"t","status":"Error","result":null,"details":{"error":"worker crashed"}}"#)), Err(ImageEditError::ProviderFailed { .. })));
        assert!(matches!(BflAsync.next(&step, &json_response(200, r#"{"id":"t","status":"Task not found"}"#)), Err(ImageEditError::ProviderFailed { .. })));
        assert!(matches!(BflAsync.next(&submit_step(&call), &json_response(402, r#"{"detail":"Insufficient credits"}"#)), Err(ImageEditError::OutOfCredits)));
        assert!(matches!(BflAsync.next(&submit_step(&call), &json_response(429, r#"{"detail":"Too many active tasks"}"#)), Err(ImageEditError::RateLimited)));
        assert!(BflAsync.cancel_request(&step).is_none());
    }

    // One full run through the executor: the key goes to the regional polling host the answer
    // named (trusted `*.bfl.ai`), never to the delivery download.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_the_regional_url_and_downloads_without_the_key() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = bfl_call("flux-2-pro", None, SizeParamStyle::WidthHeight);
        let answers = vec![
            json_response(200, r#"{"id":"t1","polling_url":"https://api.us1.bfl.ai/v1/get_result?id=t1"}"#),
            json_response(200, r#"{"id":"t1","status":"Pending","result":null}"#),
            json_response(200, r#"{"id":"t1","status":"Ready","result":{"sample":"https://delivery-us1.bfl.ai/r/t1/sample.png?sig=x"}}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&BflAsync, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Header("x-key"), "secret".to_string()));
        assert_eq!(sent.iter().map(|sent| sent.method).collect::<Vec<_>>(), [HttpMethod::Post, HttpMethod::Get, HttpMethod::Get, HttpMethod::Get]);
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(
            trail,
            [
                ("https://api.eu.bfl.ai/v1/flux-2-pro", key.clone()),
                ("https://api.us1.bfl.ai/v1/get_result?id=t1", key.clone()),
                ("https://api.us1.bfl.ai/v1/get_result?id=t1", key),
                ("https://delivery-us1.bfl.ai/r/t1/sample.png?sig=x", None),
            ]
        );
    }
}
