/*
File: crates/ms-ai-api/src/image_edit/adapters/recraft_edit.rs

Purpose:
Adapter of Recraft image inpainting (`POST {base}/images/inpaint` as JSON, Bearer): one
synchronous request with the image and its mask, the image answered inline.

Key structures:
- RecraftEdit (the `EditProtocol`)

Notes:
Body `{model, prompt, image_url: data URL, mask_url: data URL, response_format: "b64_json",
image_format: "png"}`: every image field of the JSON form accepts "a public URL or a data URL",
and raster results default to WebP, so PNG is asked for. The mask is REQUIRED, grayscale,
"exactly the same size as the image", each pixel pure white (inpaint) or pure black (keep):
`MaskPolarity::WhiteEdits` at the sent size; a call without a mask is refused (the pipeline
gives the `HardRequired` offer a full mask when nothing is painted). Inpainting is "available
with Recraft V3, Recraft V3 Vector models only"; V4.x only has whole-image `imageToImage`. No
size field (`SizeParamStyle::None`). The answer is `data[0].b64_json` (`images_data_step`);
errors keep "their JSON body and non-200 status" and go through `classify_error`. No cancel.
Sources: https://www.recraft.ai/docs/api-reference/endpoints.md (authentication, image
inpainting), https://www.recraft.ai/docs/api-reference/image-inputs-and-results.md (fetched
2026-10-04).
*/

use serde_json::json;

use super::{images_data_step, json_post, mask_data_url, png_data_url, refuse_reference};
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::MaskPolarity;
use crate::image_edit::error::ImageEditError;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpRequestSpec, HttpResponse, NextStep, StepCtx};

/// The Recraft inpainting adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct RecraftEdit;

impl EditProtocol for RecraftEdit {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        // One image field: a reference cannot be expressed.
        refuse_reference(call)?;
        if call.size_param != SizeParamStyle::None {
            return Err(ImageEditError::RequestBuild { detail: format!("Recraft inpainting keeps the input size; it cannot state {:?}", call.size_param) });
        }
        let mask = mask_data_url(call, MaskPolarity::WhiteEdits)?.ok_or_else(|| ImageEditError::RequestBuild { detail: "Recraft inpainting requires a mask".to_string() })?;
        let body = json!({
            "model": call.model_id,
            "prompt": call.prompt,
            "image_url": png_data_url(&call.image_png),
            "mask_url": mask,
            "response_format": "b64_json",
            "image_format": "png"
        });
        Ok(json_post(format!("{}/images/inpaint", call.base_url), Vec::new(), body, AuthScheme::Bearer))
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

    use super::RecraftEdit;
    use crate::image_edit::adapters::png_data_url;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    fn recraft_call(mask: Option<Vec<u8>>, size_param: SizeParamStyle) -> EditCall {
        call(ImageEditProvider::Recraft, "recraftv3", "https://external.api.recraft.ai/v1", mask, (2, 2), size_param, None)
    }

    // The inpainting JSON curl (`image_url`, `mask_url`, `prompt`) with data URLs, the
    // documented `model`, `response_format: "b64_json"` and `image_format: "png"`.
    #[test]
    fn request_matches_the_documented_body() {
        let mask = vec![255, 0, 0, 0];
        let spec = RecraftEdit.submit(&recraft_call(Some(mask.clone()), SizeParamStyle::None)).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((spec.method, spec.url.as_str(), spec.auth), (HttpMethod::Post, "https://external.api.recraft.ai/v1/images/inpaint", Some(AuthScheme::Bearer)));
        let mask_png = encode_mask_png(&mask, 2, 2, MaskPolarity::WhiteEdits).unwrap_or_else(|error| panic!("{error:?}"));
        let expected = json!({ "model": "recraftv3", "prompt": "Remove the speech bubble text", "image_url": "data:image/png;base64,UE5HREFUQQ==", "mask_url": png_data_url(&mask_png), "response_format": "b64_json", "image_format": "png" });
        assert_eq!(spec.body, HttpBody::Json(expected));
        assert!(matches!(RecraftEdit.submit(&recraft_call(None, SizeParamStyle::None)), Err(ImageEditError::RequestBuild { .. })));
        assert!(matches!(RecraftEdit.submit(&recraft_call(Some(mask), SizeParamStyle::WxH)), Err(ImageEditError::RequestBuild { .. })));
    }

    // "every image carries its bytes Base64-encoded in `b64_json`" (image results). Error
    // bodies are documented only as "JSON body and non-200 status", so the two below are
    // illustrative `{code, message}` bodies, not transcriptions.
    #[test]
    fn answers_and_errors() {
        let call = recraft_call(Some(vec![255; 4]), SizeParamStyle::None);
        let step = submit_step(&call);
        assert_eq!(RecraftEdit.next(&step, &json_response(200, r#"{"data":[{"b64_json":"AQID"}]}"#)).ok(), Some(NextStep::Image(vec![1, 2, 3])));
        assert!(matches!(RecraftEdit.next(&step, &json_response(401, r#"{"code":"not_authenticated","message":"Not authenticated"}"#)), Err(ImageEditError::KeyRejected)));
        assert!(matches!(RecraftEdit.next(&step, &json_response(400, r#"{"code":"invalid_mask","message":"mask must have the same size as the image"}"#)), Err(ImageEditError::ProviderRejected { status: 400, message }) if message == "mask must have the same size as the image"));
        assert!(RecraftEdit.cancel_request(&step).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn scripted_run_takes_the_inline_image() {
        use crate::image_edit::adapters::test_support::run_scripted;
        let call = recraft_call(Some(vec![255; 4]), SizeParamStyle::None);
        let (result, sent) = run_scripted(&RecraftEdit, &call, vec![json_response(200, r#"{"data":[{"b64_json":"SU1BR0U="}]}"#)]);
        assert_eq!(result.ok(), Some(b"IMAGE".to_vec()));
        assert_eq!(sent.iter().map(|sent| (sent.url.as_str(), sent.auth.clone())).collect::<Vec<_>>(), [("https://external.api.recraft.ai/v1/images/inpaint", Some((AuthScheme::Bearer, "secret".to_string())))]);
    }
}
