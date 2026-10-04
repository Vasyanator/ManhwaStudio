/*
File: crates/ms-ai-api/src/image_edit/adapters/openai_images.rs

Purpose:
Adapter of the `OpenAI` images edits shape, `POST {base}/images/edits` as
`multipart/form-data`, used by `OpenAI`, DeepInfra, aimlapi, AITunnel, ProxyAPI and the user's
own `OpenAI`-compatible server (stable-diffusion.cpp `sd-server` and alike).

Key structures:
- OpenAiImages (the `EditProtocol`)

Notes:
Form fields, in the order of the documented curl samples: `model`, `prompt`, the image, the
optional `mask`, `size`. The image field is `image[]` for `OpenAI` (its current samples) and the
user's server (`sd-server` documents `image[]` as preferred), `image` for every reseller (their
samples). The mask is an RGBA PNG, transparent = edit (`MaskPolarity::TransparentEdits`), at
the sent size. No `n` / `output_format` / `response_format`: their defaults (one PNG) are what
the pipeline needs and not every compatible server accepts them. The answer is
`data[0].b64_json` or (aimlapi) a signed `data[0].url`. Single step, synchronous.
Sources: https://developers.openai.com/api/docs/guides/image-generation (edit with mask),
https://aitunnel.ru/docs/images, https://proxyapi.ru/docs/image-generation,
https://docs.deepinfra.com/api-reference/image-generation/openai-images-edits,
https://docs.aimlapi.com/api-references/image-models/openai/gpt-image-2,
https://raw.githubusercontent.com/leejet/stable-diffusion.cpp/master/examples/server/api.md
(fetched 2026-10-04).
*/

use super::images_data_step;
use crate::image_edit::catalog::SizeParamStyle;
use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
use crate::image_edit::error::ImageEditError;
use crate::image_edit::multipart::MultipartForm;
use crate::image_edit::protocol::{AuthScheme, EditCall, EditProtocol, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse, NextStep, StepCtx};
use crate::image_edit::provider::ImageEditProvider;

/// The `OpenAI` images edits adapter (stateless).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiImages;

/// The multipart field name of the edited image at `provider`.
fn image_field(provider: ImageEditProvider) -> &'static str {
    match provider {
        ImageEditProvider::OpenAi | ImageEditProvider::OpenAiCompatible => "image[]",
        // The resellers' samples; providers of other shapes never reach this adapter and get
        // the shape's common single-file field.
        ImageEditProvider::DeepInfra
        | ImageEditProvider::AimlApi
        | ImageEditProvider::AiTunnel
        | ImageEditProvider::ProxyApi
        | ImageEditProvider::Gemini
        | ImageEditProvider::OpenRouter
        | ImageEditProvider::BytePlus
        | ImageEditProvider::Bfl
        | ImageEditProvider::Xai
        | ImageEditProvider::Ideogram
        | ImageEditProvider::Recraft
        | ImageEditProvider::Runway
        | ImageEditProvider::Luma
        | ImageEditProvider::DashScope
        | ImageEditProvider::Tencent
        | ImageEditProvider::Kling
        | ImageEditProvider::Fal
        | ImageEditProvider::Replicate
        | ImageEditProvider::Together
        | ImageEditProvider::Runware
        | ImageEditProvider::RouterAi
        | ImageEditProvider::Polza
        | ImageEditProvider::GenApi => "image",
    }
}

impl EditProtocol for OpenAiImages {
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError> {
        let size = match call.size_param {
            SizeParamStyle::WxH => Some(format!("{}x{}", call.width, call.height)),
            SizeParamStyle::None => None,
            SizeParamStyle::WStarH | SizeParamStyle::WidthHeight | SizeParamStyle::ImageSizeObject | SizeParamStyle::AspectTier(_) => {
                return Err(ImageEditError::RequestBuild { detail: format!("the images edits shape cannot state the size as {:?}", call.size_param) });
            }
        };
        let mut form = MultipartForm::new().text("model", &call.model_id).text("prompt", &call.prompt).file(image_field(call.provider), "image.png", "image/png", call.image_png.clone());
        if let Some(mask) = &call.mask {
            form = form.file("mask", "mask.png", "image/png", encode_mask_png(mask, call.width, call.height, MaskPolarity::TransparentEdits)?);
        }
        if let Some(size) = size {
            form = form.text("size", &size);
        }
        // The user's own server may run without a key; the executor sends no header then.
        Ok(HttpRequestSpec { method: HttpMethod::Post, url: format!("{}/images/edits", call.base_url), headers: Vec::new(), body: HttpBody::Multipart(form.finish()?), auth: Some(AuthScheme::Bearer) })
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
    use super::OpenAiImages;
    use crate::image_edit::adapters::test_support::{call, json_response, submit_step};
    use crate::image_edit::catalog::SizeParamStyle;
    use crate::image_edit::codec::{MaskPolarity, encode_mask_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::multipart::MultipartForm;
    use crate::image_edit::protocol::{AuthScheme, EditProtocol, HttpBody, HttpMethod, NextStep};
    use crate::image_edit::provider::ImageEditProvider;

    // The OpenAI guide's mask sample: `-F model=... -F mask=@mask.png -F image[]=@... -F prompt=...`
    // (https://developers.openai.com/api/docs/guides/image-generation), plus the documented
    // `size` WIDTHxHEIGHT; fields in our fixed order.
    #[test]
    fn openai_request_matches_the_documented_form() {
        let mask = vec![0, 255, 0, 0];
        let call = call(ImageEditProvider::OpenAi, "gpt-image-2.5-sunburst", "https://api.openai.com/v1", Some(mask.clone()), (2, 2), SizeParamStyle::WxH, None);
        let spec = OpenAiImages.submit(&call).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(spec.method, HttpMethod::Post);
        assert_eq!(spec.url, "https://api.openai.com/v1/images/edits");
        assert_eq!(spec.auth, Some(AuthScheme::Bearer));
        assert!(spec.headers.is_empty());
        let mask_png = encode_mask_png(&mask, 2, 2, MaskPolarity::TransparentEdits).unwrap_or_default();
        let expected = MultipartForm::new()
            .text("model", "gpt-image-2.5-sunburst")
            .text("prompt", "Remove the speech bubble text")
            .file("image[]", "image.png", "image/png", b"PNGDATA".to_vec())
            .file("mask", "mask.png", "image/png", mask_png)
            .text("size", "2x2")
            .finish()
            .ok();
        assert_eq!(Some(spec.body), expected.map(HttpBody::Multipart));
    }

    // AITunnel's sample: `-F model=... -F image=@photo.png -F prompt=...` (https://aitunnel.ru/docs/images).
    #[test]
    fn resellers_use_the_single_image_field_and_no_mask_when_none() {
        let call = call(ImageEditProvider::AiTunnel, "gpt-image-2", "https://api.aitunnel.ru/v1", None, (1024, 1536), SizeParamStyle::WxH, None);
        let spec = OpenAiImages.submit(&call).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(spec.url, "https://api.aitunnel.ru/v1/images/edits");
        let expected = MultipartForm::new().text("model", "gpt-image-2").text("prompt", "Remove the speech bubble text").file("image", "image.png", "image/png", b"PNGDATA".to_vec()).text("size", "1024x1536").finish().ok();
        assert_eq!(Some(spec.body), expected.map(HttpBody::Multipart));
    }

    #[test]
    fn size_styles_other_than_wxh_are_refused() {
        let call = call(ImageEditProvider::OpenAi, "gpt-image-2", "https://api.openai.com/v1", None, (2, 2), SizeParamStyle::WidthHeight, None);
        assert!(matches!(OpenAiImages.submit(&call), Err(ImageEditError::RequestBuild { .. })));
    }

    // The documented answer shape `{"data":[{"b64_json":"..."}]}` (guide above; AITunnel adds
    // `media_type` and `usage`).
    #[test]
    fn answers_are_parsed() {
        let call = call(ImageEditProvider::AiTunnel, "gpt-image-2", "https://api.aitunnel.ru/v1", None, (2, 2), SizeParamStyle::WxH, None);
        let step = submit_step(&call);
        let ok = OpenAiImages.next(&step, &json_response(200, r#"{"data":[{"b64_json":"iVBORw==","media_type":"image/png"}],"usage":{"cost_rub":3.4,"balance":1245.6}}"#));
        assert_eq!(ok.ok(), Some(NextStep::Image(vec![0x89, 0x50, 0x4e, 0x47])));
        let blocked = OpenAiImages.next(&step, &json_response(403, r#"{"error":{"message":"Country, region, or territory not supported","type":"request_forbidden","param":null,"code":"unsupported_country_region_territory"}}"#));
        assert!(matches!(blocked, Err(ImageEditError::RegionBlocked)));
        assert!(OpenAiImages.cancel_request(&step).is_none());
    }
}
