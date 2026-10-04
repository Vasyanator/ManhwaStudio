/*
File: crates/ms-ai-api/src/image_edit/adapters/replicate_predictions.rs

Purpose:
Adapter of Replicate official-model predictions (`POST {base}/models/{owner}/{name}/predictions`,
Bearer): create a prediction, poll its `urls.get` until a terminal status, download the output
file; cancel through `urls.cancel`.

Key structures:
- ReplicatePredictions (the `EditProtocol`)

Notes:
Inputs per model (`llms.txt` and `/api/schema` of each model, fetched 2026-10-04): FLUX.2 pro /
max `{prompt, input_images[], aspect_ratio: "custom", width, height, output_format: "png"}`;
FLUX Fill pro `{prompt, image, mask, output_format}` (mask "black preserved, white
inpainted"); Kontext pro `{prompt, input_image, aspect_ratio: "match_input_image",
output_format}`; Qwen Image Edit 2511 `{prompt, image: [..], aspect_ratio:
"match_input_image", output_format}`; Ideogram v3 quality `{prompt, image, mask}` (mask "Black
pixels are inpainted, white pixels are preserved"). Files travel as PNG data URLs ("Files
should be passed as HTTP URLs or data URLs"). Statuses: `starting` / `processing` poll on,
`succeeded` -> `output` (a URL or a list of URLs), `failed` -> `error`, `canceled` ->
`ProviderFailed`. Output files are served by `replicate.delivery` (no key); a file URL on the
API host itself gets the Bearer key, as the reference asks.
Sources: https://replicate.com/docs/reference/http (models.predictions.create, predictions.get,
predictions.cancel), https://replicate.com/black-forest-labs/flux-2-pro/llms.txt and the
other models' `llms.txt` / `api/schema`, fetched 2026-10-04.
*/

use std::time::Duration;

use serde_json::{Value, json};

use super::{classify_error, get_request, is_success, json_post, json_value, job_failure, mask_data_url, png_data_url, refuse_reference, required_str, result_url_step};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::MaskPolarity;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, JobRef, NextStep, StepCtx};

/// The Replicate predictions adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReplicatePredictions;

/// `RequestBuild` for a size style the model's input cannot state.
fn unsupported_size(call: &EditCall) -> ImageEditError {
    ImageEditError::RequestBuild { detail: format!("Replicate {} cannot state the size as {:?}", call.model_id, call.size_param) }
}

/// The model's native mask as a data URL (required by the mask models).
fn required_mask(call: &EditCall, polarity: MaskPolarity) -> Result<String, ImageEditError> {
    mask_data_url(call, polarity)?.ok_or_else(|| ImageEditError::RequestBuild { detail: format!("Replicate {} requires a mask", call.model_id) })
}

/// The `input` object of `call`.
fn input(call: &EditCall) -> Result<Value, ImageEditError> {
    // Only FLUX.2 `input_images` and Qwen `image` are lists; the other models take one image.
    if !matches!(call.model_id.as_str(), "black-forest-labs/flux-2-pro" | "black-forest-labs/flux-2-max" | "qwen/qwen-image-edit-2511") {
        refuse_reference(call)?;
    }
    let image = png_data_url(&call.image_png);
    // The list inputs (FLUX.2 `input_images`, Qwen `image`): the edited image first, then the
    // reference.
    let images: Vec<String> = std::iter::once(image.clone()).chain(call.reference_png.as_deref().map(png_data_url)).collect();
    let only_none = |value: Value| if call.size_param == SizeParamStyle::None { Ok(value) } else { Err(unsupported_size(call)) };
    match call.model_id.as_str() {
        "black-forest-labs/flux-2-pro" | "black-forest-labs/flux-2-max" => match call.size_param {
            SizeParamStyle::WidthHeight => Ok(json!({ "prompt": call.prompt, "input_images": images, "aspect_ratio": "custom", "width": call.width, "height": call.height, "output_format": "png" })),
            SizeParamStyle::None => Ok(json!({ "prompt": call.prompt, "input_images": images, "aspect_ratio": "match_input_image", "resolution": "match_input_image", "output_format": "png" })),
            SizeParamStyle::WxH | SizeParamStyle::WStarH | SizeParamStyle::ImageSizeObject | SizeParamStyle::AspectTier(_) => Err(unsupported_size(call)),
        },
        "black-forest-labs/flux-fill-pro" => only_none(json!({ "prompt": call.prompt, "image": image, "mask": required_mask(call, MaskPolarity::WhiteEdits)?, "output_format": "png" })),
        "black-forest-labs/flux-kontext-pro" => only_none(json!({ "prompt": call.prompt, "input_image": image, "aspect_ratio": "match_input_image", "output_format": "png" })),
        "qwen/qwen-image-edit-2511" => only_none(json!({ "prompt": call.prompt, "image": images, "aspect_ratio": "match_input_image", "output_format": "png" })),
        "ideogram-ai/ideogram-v3-quality" => only_none(json!({ "prompt": call.prompt, "image": image, "mask": required_mask(call, MaskPolarity::BlackEdits)? })),
        other => Err(ImageEditError::RequestBuild { detail: format!("unknown Replicate model {other}") }),
    }
}

impl EditProtocol for ReplicatePredictions {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        Ok(json_post(format!("{}/models/{}/predictions", call.base_url, call.model_id), Vec::new(), json!({ "input": input(call)? }), AuthScheme::Bearer))
    }

    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError> {
        if !is_success(response.status) {
            return Err(classify_error(response.status, &response.body));
        }
        let prediction = json_value(response)?;
        match required_str(&prediction, "/status")? {
            "starting" | "processing" => {
                let id = required_str(&prediction, "/id")?.to_string();
                let get_url = prediction.pointer("/urls/get").and_then(Value::as_str).map_or_else(|| format!("{}/predictions/{id}", step.call.base_url), str::to_string);
                let cancel_url = prediction.pointer("/urls/cancel").and_then(Value::as_str).map(str::to_string);
                Ok(NextStep::Poll { request: get_request(get_url.clone(), Vec::new(), Some(AuthScheme::Bearer)), after: Duration::ZERO, stage: 0, job: Some(JobRef { id, poll_url: Some(get_url), cancel_url }) })
            }
            "succeeded" => {
                let output = prediction.get("output");
                let url = output.and_then(Value::as_str).or_else(|| output.and_then(|output| output.get(0)).and_then(Value::as_str));
                let url = url.ok_or_else(|| ImageEditError::NoImageReturned { reason: "the prediction succeeded without an output file".to_string() })?;
                // Files on the API host itself need the key; delivery-CDN files do not.
                if url.starts_with(&format!("{}/", step.call.base_url)) {
                    return Ok(NextStep::Download(get_request(url.to_string(), Vec::new(), Some(AuthScheme::Bearer))));
                }
                result_url_step(url)
            }
            "failed" => Err(job_failure(prediction.get("error").and_then(Value::as_str).unwrap_or("the prediction failed"))),
            "canceled" => Err(ImageEditError::ProviderFailed { detail: "Replicate: the prediction was canceled".to_string() }),
            other => Err(ImageEditError::ProviderFailed { detail: format!("Replicate prediction status {other}") }),
        }
    }

    fn cancel_request(&self, step: &StepCtx<'_>) -> Option<HttpRequestSpec> {
        let job = step.job?;
        let url = job.cancel_url.clone().unwrap_or_else(|| format!("{}/predictions/{}/cancel", step.call.base_url, job.id));
        Some(HttpRequestSpec { method: HttpMethod::Post, url, headers: Vec::new(), body: HttpBody::Empty, auth: Some(AuthScheme::Bearer) })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ReplicatePredictions;
    use crate::image_edit::adapters::png_data_url;
    use crate::image_edit::adapters::test_support::{call, json_response, poll_step, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, JobRef, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    const IMAGE: &str = "data:image/png;base64,UE5HREFUQQ==";
    const BASE: &str = "https://api.replicate.com/v1";

    fn rep_call(model: &str, mask: Option<Vec<u8>>, size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Replicate, model, BASE, mask, (2, 2), size_param, None)
    }

    fn input(call: &EditCall) -> Option<serde_json::Value> {
        match ReplicatePredictions.submit(call).ok().map(|spec| spec.body) {
            Some(HttpBody::Json(body)) => body.get("input").cloned(),
            _ => None,
        }
    }

    // `curl -X POST -d '{"input": {...}}' -H "Authorization: Bearer ..." .../v1/models/{owner}/{name}/predictions`
    // (reference, models.predictions.create) with each model's documented inputs.
    #[test]
    fn model_inputs() {
        let spec = ReplicatePredictions.submit(&rep_call("black-forest-labs/flux-2-max", None, SizeParamStyle::WidthHeight)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://api.replicate.com/v1/models/black-forest-labs/flux-2-max/predictions", Some(AuthScheme::Bearer)));
        assert_eq!(spec.body, HttpBody::Json(json!({ "input": { "prompt": "Remove the speech bubble text", "input_images": [IMAGE], "aspect_ratio": "custom", "width": 2, "height": 2, "output_format": "png" } })));
        let mask = vec![0, 255, 255, 0];
        let white = png_data_url(&encode_mask_png(&mask, 2, 2, MaskPolarity::WhiteEdits).unwrap_or_default());
        assert_eq!(input(&rep_call("black-forest-labs/flux-fill-pro", Some(mask.clone()), SizeParamStyle::None)), Some(json!({ "prompt": "Remove the speech bubble text", "image": IMAGE, "mask": white, "output_format": "png" })));
        let black = png_data_url(&encode_mask_png(&mask, 2, 2, MaskPolarity::BlackEdits).unwrap_or_default());
        assert_eq!(input(&rep_call("ideogram-ai/ideogram-v3-quality", Some(mask), SizeParamStyle::None)), Some(json!({ "prompt": "Remove the speech bubble text", "image": IMAGE, "mask": black })));
        assert_eq!(input(&rep_call("black-forest-labs/flux-kontext-pro", None, SizeParamStyle::None)), Some(json!({ "prompt": "Remove the speech bubble text", "input_image": IMAGE, "aspect_ratio": "match_input_image", "output_format": "png" })));
        assert_eq!(input(&rep_call("qwen/qwen-image-edit-2511", None, SizeParamStyle::None)), Some(json!({ "prompt": "Remove the speech bubble text", "image": [IMAGE], "aspect_ratio": "match_input_image", "output_format": "png" })));
        assert!(matches!(ReplicatePredictions.submit(&rep_call("black-forest-labs/flux-fill-pro", None, SizeParamStyle::None)), Err(ImageEditError::RequestBuild { .. })));
        assert!(matches!(ReplicatePredictions.submit(&rep_call("qwen/qwen-image-edit-2511", None, SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // The reference's prediction object (`id`, `status`, `output`, `error`, `urls.get/cancel`).
    #[test]
    fn status_machine() {
        let call = rep_call("black-forest-labs/flux-2-pro", None, SizeParamStyle::WidthHeight);
        let starting = r#"{"id":"gm3qorzdhgbfurvjtvhg6dckhu","model":"black-forest-labs/flux-2-pro","input":{},"logs":"","output":null,"error":null,"status":"starting","created_at":"2023-09-08T16:19:34.765994Z","urls":{"web":"https://replicate.com/p/gm3qorzdhgbfurvjtvhg6dckhu","get":"https://api.replicate.com/v1/predictions/gm3qorzdhgbfurvjtvhg6dckhu","cancel":"https://api.replicate.com/v1/predictions/gm3qorzdhgbfurvjtvhg6dckhu/cancel"}}"#;
        let job = JobRef {
            id: "gm3qorzdhgbfurvjtvhg6dckhu".to_string(),
            poll_url: Some("https://api.replicate.com/v1/predictions/gm3qorzdhgbfurvjtvhg6dckhu".to_string()),
            cancel_url: Some("https://api.replicate.com/v1/predictions/gm3qorzdhgbfurvjtvhg6dckhu/cancel".to_string()),
        };
        assert!(matches!(ReplicatePredictions.next(&submit_step(&call), &json_response(201, starting)), Ok(NextStep::Poll { job: Some(kept), request, .. }) if kept == job && request.auth == Some(AuthScheme::Bearer)));
        let step = poll_step(&call, 0, 1, Some(&job));
        let succeeded = r#"{"id":"gm3qorzdhgbfurvjtvhg6dckhu","status":"succeeded","output":"https://replicate.delivery/xezq/EXuyWm6qQuK9J19lUCtc9sbO4k2RyHwoOP6GoYMCpeyM4a2KA/tmpzd16m4x2.webp","error":null,"metrics":{"predict_time":4.2}}"#;
        assert!(matches!(ReplicatePredictions.next(&step, &json_response(200, succeeded)), Ok(NextStep::Download(spec)) if spec.auth.is_none() && spec.url.starts_with("https://replicate.delivery/")));
        let list = r#"{"id":"x","status":"succeeded","output":["https://replicate.delivery/xezq/R61h/out-0.webp"]}"#;
        assert!(matches!(ReplicatePredictions.next(&step, &json_response(200, list)), Ok(NextStep::Download(spec)) if spec.url.ends_with("out-0.webp")));
        let api_file = r#"{"id":"x","status":"succeeded","output":"https://api.replicate.com/v1/files/abc/download"}"#;
        assert!(matches!(ReplicatePredictions.next(&step, &json_response(200, api_file)), Ok(NextStep::Download(spec)) if spec.auth == Some(AuthScheme::Bearer)));
        let nsfw = r#"{"id":"x","status":"failed","output":null,"error":"NSFW content detected. Try running it again, or try a different prompt."}"#;
        assert!(matches!(ReplicatePredictions.next(&step, &json_response(200, nsfw)), Err(ImageEditError::Moderated)));
        assert!(matches!(ReplicatePredictions.next(&step, &json_response(200, r#"{"id":"x","status":"canceled"}"#)), Err(ImageEditError::ProviderFailed { .. })));
        assert!(matches!(ReplicatePredictions.next(&submit_step(&call), &json_response(401, r#"{"title":"Unauthenticated","detail":"You did not pass a valid authentication token","status":401}"#)), Err(ImageEditError::KeyRejected)));
        assert!(matches!(ReplicatePredictions.next(&submit_step(&call), &json_response(402, r#"{"title":"Insufficient credit","detail":"You have insufficient credit to run this model.","status":402}"#)), Err(ImageEditError::OutOfCredits)));
        // predictions.cancel: `curl -s -X POST -H "Authorization: Bearer ..." .../predictions/$ID/cancel`.
        assert!(matches!(ReplicatePredictions.cancel_request(&step), Some(spec) if spec.method == HttpMethod::Post && spec.url.ends_with("/cancel") && spec.auth == Some(AuthScheme::Bearer)));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_polls_urls_get_and_downloads() {
        use crate::image_edit::adapters::test_support::{image_response, run_scripted};
        let call = rep_call("black-forest-labs/flux-kontext-pro", None, SizeParamStyle::None);
        let answers = vec![
            json_response(201, r#"{"id":"p1","status":"starting","urls":{"get":"https://api.replicate.com/v1/predictions/p1","cancel":"https://api.replicate.com/v1/predictions/p1/cancel"}}"#),
            json_response(200, r#"{"id":"p1","status":"processing","urls":{"get":"https://api.replicate.com/v1/predictions/p1","cancel":"https://api.replicate.com/v1/predictions/p1/cancel"}}"#),
            json_response(200, r#"{"id":"p1","status":"succeeded","output":"https://replicate.delivery/x/p1.png"}"#),
            image_response(),
        ];
        let (result, sent) = run_scripted(&ReplicatePredictions, &call, answers);
        assert_eq!(result.ok(), Some(b"IMAGEBYTES".to_vec()));
        let key = Some((AuthScheme::Bearer, "secret".to_string()));
        let trail: Vec<(&str, Option<(AuthScheme, String)>)> = sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect();
        assert_eq!(
            trail,
            [
                ("https://api.replicate.com/v1/models/black-forest-labs/flux-kontext-pro/predictions", key.clone()),
                ("https://api.replicate.com/v1/predictions/p1", key.clone()),
                ("https://api.replicate.com/v1/predictions/p1", key),
                ("https://replicate.delivery/x/p1.png", None),
            ]
        );
    }
}
