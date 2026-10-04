/*
File: crates/ms-ai-api/src/image_edit/catalog.rs

Purpose:
The image-edit model catalogue: every model each provider offers for editing, with its size
rule, how well that rule is evidenced, its native mask support, how the adapter states the
size, how many reference images it takes besides the edited one, and its retirement date. Pure
data plus two lookups.

Key structures:
- ModelOffer, MaskSupport, SizeParamStyle

Key functions:
- offers(provider)          : the provider's offers, in picker order.
- lookup(provider, model_id): the offer of one model id (any non-empty id for the user's own
                              OpenAI-compatible server).

Notes:
Facts come from `dev-docs/image_edit/` research (2026-10-04) and the provider catalogues /
schemas fetched on 2026-10-04 (OpenRouter `/api/v1/images/models`, AITunnel, RouterAI,
ProxyAPI pricing, Polza `/api/v1/models`, fal per-endpoint OpenAPI, Replicate `llms.txt`,
DeepInfra `/models/list`, Together serverless models, aimlapi docs, Runware `schema.json`,
GenAPI model pages). Model ids are what each provider's API expects and are persisted by the
tool's settings: never change one without a migration.

Deliberate exclusions (not offered on purpose):
- `gpt-image-1` (retires 2026-10-23), `chatgpt-image-latest` (moving alias, size unpinned),
  `dall-e-*` (shut down), `gemini-2.5-flash-image` (shut down 2026-10-02).
- BFL Erase: documented size change (1200 -> 1206), every run would be a `SizeMismatch`;
  also small. FLUX.2 klein: small, already a local engine. `qwen-image-edit` (base): no size
  parameter, superseded. `wanx2.1-imageedit`: Beijing-only real-name.
- aimlapi FLUX.2 / Kontext / Qwen edit: served on `/v1/images/generations` with an
  `image_size` object, not the `OpenAI` edits shape this provider is wired to.
- Recraft V4 / V4.1: no mask inpainting ("available with Recraft V3 ... only"); their
  `imageToImage` regenerates the whole image. Together `Qwen/Qwen-Image-2.0-Pro`: Together
  documents reference images only for FLUX.2 and the Google models.
- Reve: its API shut down on 2026-08-14 and the service on 2026-09-27; its persisted provider
  key `reve` is retired and must never be reused.
- Providers: Volcengine Ark (Chinese real-name), Vertex AI (OAuth service accounts, Blocked),
  Azure Foundry / Azure `OpenAI` (per-deployment, Blocked), Bedrock Nova Canvas (EOL
  2026-09-30), Adobe Firefly (enterprise contract, Blocked), Midjourney (no API), Krea /
  Leonardo / Freepik (first-party models small, resell others), Stability, Zhipu, Baidu,
  StepFun, Meituan (small), MiniMax (not an editor), Sber / Yandex / MTS (no edit API),
  VseGPT, BotHub, WaveSpeed, Kie, PiAPI, Novita, Segmind, Fireworks (API shape undocumented
  in the research, or no edit models).
*/

use super::error::ImageEditError;
use super::provider::ImageEditProvider;
use super::size_rule::{
    ARK_SEEDREAM_5_LITE, ARK_SEEDREAM_5_PRO, AspectTierEntry, CUSTOM_OPENAI, FLUX2, FLUX2_MAX_2048, GEMINI_3_PRO, GEMINI_3_PRO_ENTRIES, GEMINI_31_FLASH, GEMINI_31_FLASH_ENTRIES, GEMINI_31_LITE, GEMINI_31_LITE_ENTRIES, GENAPI_GPT_IMAGE, IDEOGRAM_45, ImageSizeRule, KLING_O1, KONTEXT,
    OPENAI_ARB, OPENAI_STD3, QWEN_3, QWEN_EDIT_PLUS, RUNWAY_GEN4, SEEDREAM_5, SizeEvidence, TENCENT_3, TENCENT_35, TOGETHER_FLUX2, UNVERIFIED_DEFAULT, WAN_27,
};

/// How a model takes a pixel mask. Polarity and encoding are the adapter's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskSupport {
    /// No mask parameter: only the local composite limits the change.
    None,
    /// A mask is accepted as guidance (the model may not follow its exact shape).
    Soft,
    /// A true inpainting mask, optional.
    Hard,
    /// A true inpainting mask the model requires (an empty user mask is sent as full).
    HardRequired,
}

/// How the adapter states the sent size `(W, H)` in its request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeParamStyle {
    /// A `"WxH"` string (`size`, `image_size`).
    WxH,
    /// A `"W*H"` string (`DashScope` `size`).
    WStarH,
    /// Separate integer `width` / `height` fields.
    WidthHeight,
    /// fal-style `image_size: { width, height }` object.
    ImageSizeObject,
    /// Aspect-ratio and tier labels of the size table entry equal to the sent size (Gemini
    /// `imageConfig`, `aspect_ratio` + `resolution`). The entries equal the offer's rule table.
    AspectTier(&'static [AspectTierEntry]),
    /// No size parameter: the model is expected to keep the input size.
    None,
}

/// One model a provider offers for editing.
#[derive(Debug, Clone, Copy)]
pub struct ModelOffer {
    /// The provider serving it.
    pub provider: ImageEditProvider,
    /// The id the provider's API expects (persisted). Empty only for the user's own server,
    /// whose single generic offer serves any id the user types.
    pub model_id: &'static str,
    /// Display name (brand, not localized). Empty for the generic offer (the UI shows the
    /// typed id instead).
    pub label: &'static str,
    /// Model family (brand, not localized) for grouping in the picker.
    pub family: &'static str,
    /// Sizes the model accepts and returns unchanged (sent-image pixels).
    pub rule: &'static ImageSizeRule,
    /// How well `rule` is evidenced.
    pub evidence: SizeEvidence,
    /// Native mask support.
    pub mask: MaskSupport,
    /// How the adapter states the size.
    pub size_param: SizeParamStyle,
    /// Announced shutdown date `(year, month, day)`, if any.
    pub retires_on: Option<(u16, u8, u8)>,
    /// How many reference images the model takes BESIDES the edited image (0 = none). Nonzero
    /// only where the provider documents multi-image input for this exact model / endpoint AND
    /// its adapter sends the images as a list (the edited image first); the source of every
    /// nonzero value is cited at its row and in `dev-docs/image_edit/references.md`.
    pub max_extra_references: u8,
}

/// Shorthand constructor for the table below (no retirement date): `size` is the rule and its
/// evidence, the pair every row states together.
const fn offer(provider: ImageEditProvider, model_id: &'static str, label: &'static str, family: &'static str, size: (&'static ImageSizeRule, SizeEvidence), mask: MaskSupport, size_param: SizeParamStyle) -> ModelOffer {
    ModelOffer { provider, model_id, label, family, rule: size.0, evidence: size.1, mask, size_param, retires_on: None, max_extra_references: 0 }
}

impl ModelOffer {
    /// The same offer with an announced shutdown date.
    const fn retiring(self, year: u16, month: u8, day: u8) -> Self {
        Self { retires_on: Some((year, month, day)), ..self }
    }

    /// The same offer taking up to `count` reference images besides the edited one.
    const fn references(self, count: u8) -> Self {
        Self { max_extra_references: count, ..self }
    }

    /// Whether the model takes at least one reference image besides the edited one.
    #[must_use]
    pub const fn accepts_references(&self) -> bool {
        self.max_extra_references > 0
    }
}

const GPT: &str = "GPT Image";
const GEMINI: &str = "Nano Banana";
const FLUX: &str = "FLUX";
const QWEN: &str = "Qwen Image";
const SEEDREAM: &str = "Seedream";
const GROK: &str = "Grok Imagine";
const IDEOGRAM: &str = "Ideogram";

// Short aliases for the offer table rows.
use ImageEditProvider as P;
use MaskSupport as M;
use SizeEvidence::{Documented as D, ParamOnly as PO, Unverified as U};
use SizeParamStyle as S;

/// Shutdown date of `gpt-image-1.5` / `gpt-image-1-mini` (`OpenAI` deprecations page).
const GPT_IMAGE_1X_RETIRE: (u16, u8, u8) = (2026, 12, 1);

/// Every offer, grouped by provider in `ImageEditProvider::ALL` order.
static OFFERS: &[ModelOffer] = &[
    // OpenAI: arbitrary /16 sizes on the 2.x models, three sizes on the 1.x ones; the mask is
    // prompt guidance (research_openai_gemini §A). References: "up to 16 images for GPT image
    // models" (images edit reference), sent as repeated `image[]`; the mask applies to the first.
    offer(P::OpenAi, "gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst", GPT, (&OPENAI_ARB, PO), M::Soft, S::WxH).references(15),
    offer(P::OpenAi, "gpt-image-2.5-flare", "GPT Image 2.5 Flare", GPT, (&OPENAI_ARB, PO), M::Soft, S::WxH).references(15),
    offer(P::OpenAi, "gpt-image-2", "GPT Image 2", GPT, (&OPENAI_ARB, PO), M::Soft, S::WxH).references(15),
    offer(P::OpenAi, "gpt-image-1.5", "GPT Image 1.5", GPT, (&OPENAI_STD3, PO), M::Soft, S::WxH).retiring(GPT_IMAGE_1X_RETIRE.0, GPT_IMAGE_1X_RETIRE.1, GPT_IMAGE_1X_RETIRE.2).references(15),
    offer(P::OpenAi, "gpt-image-1-mini", "GPT Image 1 Mini", GPT, (&OPENAI_STD3, PO), M::Soft, S::WxH).retiring(GPT_IMAGE_1X_RETIRE.0, GPT_IMAGE_1X_RETIRE.1, GPT_IMAGE_1X_RETIRE.2).references(15),
    // Gemini: output only from the per-ratio table; that a table-sized input yields that exact
    // size is inferred, not documented. References (image-generation guide, 2026-10-05): up to
    // 14 on Flash / Pro; Lite is "not optimized for multiple reference inputs", so none.
    offer(P::Gemini, "gemini-3.1-flash-image", "Nano Banana 2 (Gemini 3.1 Flash Image)", GEMINI, (&GEMINI_31_FLASH, PO), M::None, S::AspectTier(&GEMINI_31_FLASH_ENTRIES)).references(13),
    offer(P::Gemini, "gemini-3-pro-image", "Nano Banana Pro (Gemini 3 Pro Image)", GEMINI, (&GEMINI_3_PRO, PO), M::None, S::AspectTier(&GEMINI_3_PRO_ENTRIES)).references(13),
    offer(P::Gemini, "gemini-3.1-flash-lite-image", "Nano Banana 2 Lite (Gemini 3.1 Flash Lite Image)", GEMINI, (&GEMINI_31_LITE, PO), M::None, S::AspectTier(&GEMINI_31_LITE_ENTRIES)),
    // Black Forest Labs (api.bfl.ai OpenAPI): FLUX.2 width / height; Kontext matches the input
    // ~1 MP; Fill requires a mask; FLUX 3 has tier-only sizing. References: only FLUX 3's
    // `images` is a list (1-10); FLUX.2 / Kontext name single `input_image_N` fields, unwired.
    offer(P::Bfl, "flux-2-pro", "FLUX.2 [pro]", FLUX, (&FLUX2, PO), M::None, S::WidthHeight),
    offer(P::Bfl, "flux-2-flex", "FLUX.2 [flex]", FLUX, (&FLUX2, PO), M::None, S::WidthHeight),
    offer(P::Bfl, "flux-2-max", "FLUX.2 [max]", FLUX, (&FLUX2, PO), M::None, S::WidthHeight),
    offer(P::Bfl, "flux-kontext-pro", "FLUX.1 Kontext [pro]", FLUX, (&KONTEXT, U), M::None, S::None),
    offer(P::Bfl, "flux-kontext-max", "FLUX.1 Kontext [max]", FLUX, (&KONTEXT, U), M::None, S::None),
    offer(P::Bfl, "flux-pro-1.0-fill", "FLUX.1 Fill [pro]", FLUX, (&UNVERIFIED_DEFAULT, U), M::HardRequired, S::None),
    offer(P::Bfl, "flux-3-image", "FLUX 3 Image", FLUX, (&UNVERIFIED_DEFAULT, U), M::None, S::None).references(9),
    // xAI: edit output size undocumented.
    offer(P::Xai, "grok-imagine-image-2.0", "Grok Imagine Image 2.0", GROK, (&UNVERIFIED_DEFAULT, U), M::None, S::None),
    offer(P::Xai, "grok-imagine-image-quality", "Grok Imagine Image Quality", GROK, (&UNVERIFIED_DEFAULT, U), M::None, S::None),
    // Ideogram precise edit: output = input size, optional mask (black = edit).
    offer(P::Ideogram, "ideogram-4-5", "Ideogram 4.5 Precise Edit", IDEOGRAM, (&IDEOGRAM_45, D), M::Hard, S::None),
    // Recraft (`/images/inpaint`, recraft.ai api-reference, 2026-10-04): mask inpainting exists
    // only for Recraft V3; the mask is required; the output size is unstated.
    offer(P::Recraft, "recraftv3", "Recraft V3 Inpaint", "Recraft", (&UNVERIFIED_DEFAULT, U), M::HardRequired, S::None),
    // Runway: reference-based generation; `ratio` is "the resolution of the output image", one of
    // 16 values (the adapter sends the sent size as `ratio`, so `S::None`). `referenceImages`:
    // "An array of one to three images" (OpenAPI, 2026-10-05).
    offer(P::Runway, "gen4_image", "Gen-4 Image", "Runway", (&RUNWAY_GEN4, D), M::None, S::None).references(2),
    // Luma Agents API `image_edit` (docs.agents.lumalabs.ai, 2026-10-04): model ids `uni-1` /
    // `uni-1-max`; "edit output dimensions are derived from the source image". Photon "no
    // longer exists as a separate product" (lumalabs.ai/llm-info).
    offer(P::Luma, "uni-1", "Uni-1", "Luma", (&UNVERIFIED_DEFAULT, U), M::None, S::None),
    offer(P::Luma, "uni-1-max", "Uni-1 Max", "Luma", (&UNVERIFIED_DEFAULT, U), M::None, S::None),
    // Alibaba Model Studio: "W*H" snapped to /16 (documented example) on edit plus / max / 2.x;
    // 3.0 has an explicit size with documented limits; Wan 2.7 takes "W*H" whose output "may
    // have minor differences". References: qwen edit / 2.0 / 3.0 "one to three input images",
    // Wan 2.7 "0 to 9 images" (API references, 2026-10-05); `size` is always sent, so the
    // "output follows the last image" default never applies.
    offer(P::DashScope, "qwen-image-edit-max", "Qwen Image Edit Max", QWEN, (&QWEN_EDIT_PLUS, D), M::None, S::WStarH).references(2),
    offer(P::DashScope, "qwen-image-edit-plus", "Qwen Image Edit Plus", QWEN, (&QWEN_EDIT_PLUS, D), M::None, S::WStarH).references(2),
    offer(P::DashScope, "qwen-image-2.0-pro", "Qwen Image 2.0 Pro", QWEN, (&QWEN_EDIT_PLUS, D), M::None, S::WStarH).references(2),
    offer(P::DashScope, "qwen-image-2.0", "Qwen Image 2.0", QWEN, (&QWEN_EDIT_PLUS, D), M::None, S::WStarH).references(2),
    offer(P::DashScope, "qwen-image-3.0-pro", "Qwen Image 3.0 Pro", QWEN, (&QWEN_3, PO), M::None, S::WStarH).references(2),
    offer(P::DashScope, "qwen-image-3.0", "Qwen Image 3.0", QWEN, (&QWEN_3, PO), M::None, S::WStarH).references(2),
    offer(P::DashScope, "wan2.7-image-pro", "Wan 2.7 Image Pro", "Wan", (&WAN_27, U), M::None, S::WStarH).references(8),
    offer(P::DashScope, "wan2.7-image", "Wan 2.7 Image", "Wan", (&WAN_27, U), M::None, S::WStarH).references(8),
    // Tencent TokenHub: v3.5 renders at the given size; v3's limits are documented but its
    // output rule is not. References: v3.5 up to 20, v3 up to 3 (services_cn_ru [15]).
    offer(P::Tencent, "hy-image-v3.5-preview", "Hy-Image 3.5 Preview", "Hunyuan Image", (&TENCENT_35, D), M::None, S::WxH).references(19),
    offer(P::Tencent, "hy-image-v3", "Hy-Image 3.0", "Hunyuan Image", (&TENCENT_3, U), M::None, S::WxH).references(2),
    // Kling: aspect ratio + 1K / 2K tier only; documented input limits. `image_list`: "The sum
    // of reference elements and reference images must not exceed 10" (O1 API doc, 2026-10-05).
    offer(P::Kling, "kling-image-o1", "Kling Image O1", "Kling", (&KLING_O1, U), M::None, S::None).references(9),
    // BytePlus ModelArk (model list + image generation API, 2026-10-04): versioned model ids,
    // "WxH" with per-model total-pixel limits.
    offer(P::BytePlus, "dola-seedream-5-0-pro-260628", "Seedream 5.0 Pro", SEEDREAM, (&ARK_SEEDREAM_5_PRO, PO), M::None, S::WxH),
    offer(P::BytePlus, "seedream-5-0-lite-260128", "Seedream 5.0 Lite", SEEDREAM, (&ARK_SEEDREAM_5_LITE, PO), M::None, S::WxH),
    // OpenRouter (`/api/v1/images/models`, 2026-10-04): no mask anywhere; whether `size` reaches
    // the upstream is unverified. References: the live `input_references` max (2026-10-05):
    // GPT 16, Gemini 14, FLUX.2 8, Qwen Image 3 4.
    offer(P::OpenRouter, "openai/gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst", GPT, (&OPENAI_ARB, U), M::None, S::WxH).references(15),
    offer(P::OpenRouter, "openai/gpt-image-2.5-flare", "GPT Image 2.5 Flare", GPT, (&OPENAI_ARB, U), M::None, S::WxH).references(15),
    offer(P::OpenRouter, "openai/gpt-image-2", "GPT Image 2", GPT, (&OPENAI_ARB, U), M::None, S::WxH).references(15),
    offer(P::OpenRouter, "google/gemini-3.1-flash-image", "Nano Banana 2 (Gemini 3.1 Flash Image)", GEMINI, (&GEMINI_31_FLASH, U), M::None, S::AspectTier(&GEMINI_31_FLASH_ENTRIES)).references(13),
    offer(P::OpenRouter, "google/gemini-3-pro-image", "Nano Banana Pro (Gemini 3 Pro Image)", GEMINI, (&GEMINI_3_PRO, U), M::None, S::AspectTier(&GEMINI_3_PRO_ENTRIES)).references(13),
    offer(P::OpenRouter, "black-forest-labs/flux.2-pro", "FLUX.2 [pro]", FLUX, (&FLUX2, U), M::None, S::WxH).references(7),
    offer(P::OpenRouter, "black-forest-labs/flux.2-flex", "FLUX.2 [flex]", FLUX, (&FLUX2, U), M::None, S::WxH).references(7),
    offer(P::OpenRouter, "black-forest-labs/flux.2-max", "FLUX.2 [max]", FLUX, (&FLUX2, U), M::None, S::WxH).references(7),
    offer(P::OpenRouter, "qwen/qwen-image-3", "Qwen Image 3", QWEN, (&QWEN_3, U), M::None, S::WxH).references(3),
    offer(P::OpenRouter, "qwen/qwen-image-3-pro", "Qwen Image 3 Pro", QWEN, (&QWEN_3, U), M::None, S::WxH).references(3),
    // fal.ai (per-endpoint OpenAPI, 2026-10-04): `image_size {width, height}` where documented.
    // References (`image_urls`, OpenAPI 2026-10-05): GPT maxItems 16, Seedream "Up to 10"; the
    // Nano Banana example sends two, FLUX.2 / Qwen 2511 describe a list of input images without
    // a maximum, so one. Single-`image_url` endpoints take none.
    offer(P::Fal, "openai/gpt-image-2/edit", "GPT Image 2", GPT, (&OPENAI_ARB, PO), M::Soft, S::ImageSizeObject).references(15),
    offer(P::Fal, "fal-ai/nano-banana-pro/edit", "Nano Banana Pro", GEMINI, (&GEMINI_3_PRO, U), M::None, S::AspectTier(&GEMINI_3_PRO_ENTRIES)).references(1),
    offer(P::Fal, "fal-ai/flux-2-pro/edit", "FLUX.2 [pro]", FLUX, (&FLUX2, PO), M::None, S::ImageSizeObject).references(1),
    offer(P::Fal, "fal-ai/flux-2-max/edit", "FLUX.2 [max]", FLUX, (&FLUX2, PO), M::None, S::ImageSizeObject).references(1),
    offer(P::Fal, "fal-ai/flux-pro/v1/fill", "FLUX.1 Fill [pro]", FLUX, (&UNVERIFIED_DEFAULT, U), M::HardRequired, S::None),
    offer(P::Fal, "fal-ai/qwen-image-edit-2511", "Qwen Image Edit 2511", QWEN, (&QWEN_EDIT_PLUS, PO), M::None, S::ImageSizeObject).references(1),
    offer(P::Fal, "fal-ai/qwen-image-edit/inpaint", "Qwen Image Edit Inpaint", QWEN, (&QWEN_EDIT_PLUS, PO), M::HardRequired, S::ImageSizeObject),
    offer(P::Fal, "bytedance/seedream/v5/pro/edit", "Seedream 5.0 Pro", SEEDREAM, (&SEEDREAM_5, PO), M::None, S::ImageSizeObject).references(9),
    // fal's schema: `auto` "preserves source geometry" (masked edits take no size).
    offer(P::Fal, "ideogram/v4.5/edit", "Ideogram 4.5 Edit", IDEOGRAM, (&IDEOGRAM_45, D), M::Hard, S::None),
    // Replicate (model `llms.txt`, 2026-10-04): FLUX.2 custom width / height up to 2048.
    // References: FLUX.2 `input_images` "Maximum 8 images"; Qwen 2511 `image` is an array of
    // "Images to use as reference" without a maximum, so one (2026-10-05).
    offer(P::Replicate, "black-forest-labs/flux-2-pro", "FLUX.2 [pro]", FLUX, (&FLUX2_MAX_2048, PO), M::None, S::WidthHeight).references(7),
    offer(P::Replicate, "black-forest-labs/flux-2-max", "FLUX.2 [max]", FLUX, (&FLUX2_MAX_2048, PO), M::None, S::WidthHeight).references(7),
    offer(P::Replicate, "black-forest-labs/flux-fill-pro", "FLUX.1 Fill [pro]", FLUX, (&UNVERIFIED_DEFAULT, U), M::HardRequired, S::None),
    offer(P::Replicate, "black-forest-labs/flux-kontext-pro", "FLUX.1 Kontext [pro]", FLUX, (&KONTEXT, U), M::None, S::None),
    offer(P::Replicate, "qwen/qwen-image-edit-2511", "Qwen Image Edit 2511", QWEN, (&QWEN_EDIT_PLUS, U), M::None, S::None).references(1),
    offer(P::Replicate, "ideogram-ai/ideogram-v3-quality", "Ideogram 3.0 Quality", IDEOGRAM, (&UNVERIFIED_DEFAULT, U), M::HardRequired, S::None),
    // Together (serverless models + images reference, 2026-10-04): `reference_images` edits,
    // `width` / `height` (FLUX.2 256..1920). Gemini's size fields are not documented there, so
    // its table sizes go out as `width` / `height`. References (`reference_images`, "used by
    // ... FLUX.2, and Google models"; model cards 2026-10-05): FLUX.2 [pro] "Up to 8 reference
    // images via API", [flex] "Up to 10"; [max] and Gemini state no count, so one.
    offer(P::Together, "black-forest-labs/FLUX.2-pro", "FLUX.2 [pro]", FLUX, (&TOGETHER_FLUX2, U), M::None, S::WidthHeight).references(7),
    offer(P::Together, "black-forest-labs/FLUX.2-max", "FLUX.2 [max]", FLUX, (&TOGETHER_FLUX2, U), M::None, S::WidthHeight).references(1),
    offer(P::Together, "black-forest-labs/FLUX.2-flex", "FLUX.2 [flex]", FLUX, (&TOGETHER_FLUX2, U), M::None, S::WidthHeight).references(9),
    offer(P::Together, "google/gemini-3-pro-image", "Nano Banana Pro (Gemini 3 Pro Image)", GEMINI, (&GEMINI_3_PRO, U), M::None, S::WidthHeight).references(1),
    // DeepInfra `OpenAI` images edits (`/v1/images/edits`, schema has `mask`).
    offer(P::DeepInfra, "black-forest-labs/FLUX-2-pro", "FLUX.2 [pro]", FLUX, (&FLUX2, U), M::Soft, S::WxH),
    offer(P::DeepInfra, "black-forest-labs/FLUX-2-max", "FLUX.2 [max]", FLUX, (&FLUX2, U), M::Soft, S::WxH),
    offer(P::DeepInfra, "Qwen/Qwen-Image-Edit-Max", "Qwen Image Edit Max", QWEN, (&QWEN_EDIT_PLUS, U), M::Soft, S::WxH),
    offer(P::DeepInfra, "ByteDance/Seedream-4.5", "Seedream 4.5", SEEDREAM, (&UNVERIFIED_DEFAULT, U), M::Soft, S::WxH),
    // Runware (AIR ids from `schema.json`, 2026-10-04): `width` / `height`, `maskImage` white = edit.
    // The Fill and Ideogram edit schemas declare no `width` / `height` ("anything it does not
    // declare is rejected"), so those two state no size.
    offer(P::Runware, "bfl:1@2", "FLUX.1 Fill [pro]", FLUX, (&UNVERIFIED_DEFAULT, U), M::HardRequired, S::None),
    offer(P::Runware, "runware:108@22", "Qwen Image Edit Plus", QWEN, (&QWEN_EDIT_PLUS, PO), M::None, S::WidthHeight),
    offer(P::Runware, "google:4@2", "Nano Banana Pro", GEMINI, (&GEMINI_3_PRO, U), M::None, S::WidthHeight),
    offer(P::Runware, "bytedance:seedream@5.0-pro", "Seedream 5.0 Pro", SEEDREAM, (&SEEDREAM_5, PO), M::None, S::WidthHeight),
    offer(P::Runware, "ideogram:4@3", "Ideogram 3.0 Edit", IDEOGRAM, (&UNVERIFIED_DEFAULT, U), M::HardRequired, S::None),
    // aimlapi `OpenAI`-shaped edits: the three standard sizes only, PNG mask.
    offer(P::AimlApi, "openai/gpt-image-2", "GPT Image 2", GPT, (&OPENAI_STD3, PO), M::Soft, S::WxH),
    offer(P::AimlApi, "openai/gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst", GPT, (&OPENAI_STD3, PO), M::Soft, S::WxH),
    offer(P::AimlApi, "openai/gpt-image-2.5-flare", "GPT Image 2.5 Flare", GPT, (&OPENAI_STD3, PO), M::Soft, S::WxH),
    // AITunnel (public image catalogue, 2026-10-04): multipart edits, `size` WxH, no mask.
    offer(P::AiTunnel, "gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst", GPT, (&OPENAI_ARB, U), M::None, S::WxH),
    offer(P::AiTunnel, "gpt-image-2.5-flare", "GPT Image 2.5 Flare", GPT, (&OPENAI_ARB, U), M::None, S::WxH),
    offer(P::AiTunnel, "gpt-image-2", "GPT Image 2", GPT, (&OPENAI_ARB, U), M::None, S::WxH),
    offer(P::AiTunnel, "gemini-3-pro-image", "Nano Banana Pro (Gemini 3 Pro Image)", GEMINI, (&GEMINI_3_PRO, U), M::None, S::WxH),
    offer(P::AiTunnel, "flux.2-pro", "FLUX.2 [pro]", FLUX, (&FLUX2, U), M::None, S::WxH),
    offer(P::AiTunnel, "flux.2-max", "FLUX.2 [max]", FLUX, (&FLUX2, U), M::None, S::WxH),
    offer(P::AiTunnel, "seedream-5-0-pro", "Seedream 5.0 Pro", SEEDREAM, (&SEEDREAM_5, U), M::None, S::WxH),
    offer(P::AiTunnel, "qwen-image-3", "Qwen Image 3", QWEN, (&QWEN_3, U), M::None, S::WxH),
    offer(P::AiTunnel, "qwen-image-3-pro", "Qwen Image 3 Pro", QWEN, (&QWEN_3, U), M::None, S::WxH),
    offer(P::AiTunnel, "grok-imagine-image-2.0", "Grok Imagine Image 2.0", GROK, (&UNVERIFIED_DEFAULT, U), M::None, S::WxH),
    // ProxyAPI (pricing catalogue, 2026-10-04): `vendor/model` ids, multipart edits, no mask.
    offer(P::ProxyApi, "openai/gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst", GPT, (&OPENAI_ARB, U), M::None, S::WxH),
    offer(P::ProxyApi, "openai/gpt-image-2.5-flare", "GPT Image 2.5 Flare", GPT, (&OPENAI_ARB, U), M::None, S::WxH),
    offer(P::ProxyApi, "openai/gpt-image-2", "GPT Image 2", GPT, (&OPENAI_ARB, U), M::None, S::WxH),
    offer(P::ProxyApi, "google/gemini-3-pro-image", "Nano Banana Pro (Gemini 3 Pro Image)", GEMINI, (&GEMINI_3_PRO, U), M::None, S::WxH),
    offer(P::ProxyApi, "black-forest-labs/flux.2-pro", "FLUX.2 [pro]", FLUX, (&FLUX2, U), M::None, S::WxH),
    offer(P::ProxyApi, "black-forest-labs/flux.2-max", "FLUX.2 [max]", FLUX, (&FLUX2, U), M::None, S::WxH),
    offer(P::ProxyApi, "bytedance-seed/seedream-5-0-pro", "Seedream 5.0 Pro", SEEDREAM, (&SEEDREAM_5, U), M::None, S::WxH),
    offer(P::ProxyApi, "qwen/qwen-image-3-pro", "Qwen Image 3 Pro", QWEN, (&QWEN_3, U), M::None, S::WxH),
    // RouterAI (`/api/v1/models`, 2026-10-04): OpenRouter-style images; mask only on gpt-image.
    offer(P::RouterAi, "openai/gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst", GPT, (&OPENAI_ARB, U), M::Soft, S::WxH),
    offer(P::RouterAi, "openai/gpt-image-2.5-flare", "GPT Image 2.5 Flare", GPT, (&OPENAI_ARB, U), M::Soft, S::WxH),
    offer(P::RouterAi, "openai/gpt-image-2", "GPT Image 2", GPT, (&OPENAI_ARB, U), M::Soft, S::WxH),
    offer(P::RouterAi, "google/gemini-3-pro-image", "Nano Banana Pro (Gemini 3 Pro Image)", GEMINI, (&GEMINI_3_PRO, U), M::None, S::AspectTier(&GEMINI_3_PRO_ENTRIES)),
    offer(P::RouterAi, "black-forest-labs/flux.2-pro", "FLUX.2 [pro]", FLUX, (&FLUX2, U), M::None, S::WxH),
    offer(P::RouterAi, "black-forest-labs/flux.2-max", "FLUX.2 [max]", FLUX, (&FLUX2, U), M::None, S::WxH),
    offer(P::RouterAi, "bytedance-seed/seedream-5-0-pro", "Seedream 5.0 Pro", SEEDREAM, (&SEEDREAM_5, U), M::None, S::WxH),
    offer(P::RouterAi, "qwen/qwen-image-3", "Qwen Image 3", QWEN, (&QWEN_3, U), M::None, S::WxH),
    offer(P::RouterAi, "qwen/qwen-image-3-pro", "Qwen Image 3 Pro", QWEN, (&QWEN_3, U), M::None, S::WxH),
    // Polza.ai (`/api/v1/models`, 2026-10-04): only `aspect_ratio` + `image_resolution`, so no
    // explicit size outside the Gemini tables; `qwen/image-2.1` takes `mask_url`. References:
    // the catalogue's `images` max (2026-10-05); Gemini 3.1 Flash lists no `images` parameter.
    offer(P::Polza, "openai/gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst", GPT, (&UNVERIFIED_DEFAULT, U), M::None, S::None).references(15),
    offer(P::Polza, "google/gemini-3.1-flash-image", "Nano Banana 2 (Gemini 3.1 Flash Image)", GEMINI, (&GEMINI_31_FLASH, U), M::None, S::AspectTier(&GEMINI_31_FLASH_ENTRIES)),
    offer(P::Polza, "google/gemini-3-pro-image-preview", "Nano Banana Pro (Gemini 3 Pro Image)", GEMINI, (&GEMINI_3_PRO, U), M::None, S::AspectTier(&GEMINI_3_PRO_ENTRIES)).references(7),
    offer(P::Polza, "black-forest-labs/flux.2-pro", "FLUX.2 [pro]", FLUX, (&UNVERIFIED_DEFAULT, U), M::None, S::None).references(7),
    offer(P::Polza, "black-forest-labs/flux.2-flex", "FLUX.2 [flex]", FLUX, (&UNVERIFIED_DEFAULT, U), M::None, S::None).references(7),
    offer(P::Polza, "seedream/5-pro-text-to-image", "Seedream 5.0 Pro", SEEDREAM, (&UNVERIFIED_DEFAULT, U), M::None, S::None).references(9),
    offer(P::Polza, "qwen/image-2.1", "Qwen Image 2.1", QWEN, (&UNVERIFIED_DEFAULT, U), M::Hard, S::None).references(9),
    // GenAPI (model pages, 2026-10-04): gpt-image `image_size` is a 9-entry preset list.
    offer(P::GenApi, "gpt-image-2", "GPT Image 2", GPT, (&GENAPI_GPT_IMAGE, PO), M::None, S::WxH),
    offer(P::GenApi, "gpt-image-2-5", "GPT Image 2.5", GPT, (&GENAPI_GPT_IMAGE, PO), M::None, S::WxH),
    offer(P::GenApi, "nano-banana-pro", "Nano Banana Pro", GEMINI, (&GEMINI_3_PRO, U), M::None, S::AspectTier(&GEMINI_3_PRO_ENTRIES)),
    offer(P::GenApi, "flux-2", "FLUX.2", FLUX, (&UNVERIFIED_DEFAULT, U), M::None, S::None),
    offer(P::GenApi, "qwen-image-edit", "Qwen Image Edit", QWEN, (&UNVERIFIED_DEFAULT, U), M::None, S::None),
    // The user's own OpenAI-compatible images server: any typed model id.
    offer(P::OpenAiCompatible, "", "", "", (&CUSTOM_OPENAI, U), M::Soft, S::WxH),
];

/// The offers of `provider`, in picker order.
pub fn offers(provider: ImageEditProvider) -> impl Iterator<Item = &'static ModelOffer> {
    OFFERS.iter().filter(move |offer| offer.provider == provider)
}

/// Every offer of every provider.
#[must_use]
pub fn all_offers() -> &'static [ModelOffer] {
    OFFERS
}

/// The offer of `model_id` (trimmed) at `provider`. For the user's own OpenAI-compatible
/// server every non-empty id maps to its single generic offer.
///
/// # Errors
/// `ImageEditError::UnknownModel` when the id is empty or not in the provider's catalogue.
pub fn lookup(provider: ImageEditProvider, model_id: &str) -> Result<&'static ModelOffer, ImageEditError> {
    let id = model_id.trim();
    let unknown = || ImageEditError::UnknownModel { model_id: id.to_string() };
    if id.is_empty() {
        return Err(unknown());
    }
    offers(provider).find(|offer| offer.model_id.is_empty() || offer.model_id == id).ok_or_else(unknown)
}

#[cfg(test)]
mod tests {
    use super::{MaskSupport, SizeParamStyle, all_offers, lookup, offers};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::size_rule::{GEMINI_31_FLASH_ENTRIES, OPENAI_ARB, SizeEvidence};

    #[test]
    fn every_provider_has_offers_and_ids_are_unique_per_provider() {
        for provider in ImageEditProvider::ALL {
            let ids: Vec<&str> = offers(provider).map(|offer| offer.model_id).collect();
            assert!(!ids.is_empty(), "{provider:?} has no offer");
            for (index, id) in ids.iter().enumerate() {
                assert!(!ids[index + 1..].contains(id), "{provider:?}: duplicate id {id}");
                assert_eq!(id.trim(), *id, "{provider:?}: untrimmed id {id:?}");
            }
        }
        // Offers are grouped in `ALL` order, so the picker order is the catalogue order.
        let mut order = all_offers().iter().map(|offer| offer.provider).collect::<Vec<_>>();
        order.dedup();
        assert_eq!(order, ImageEditProvider::ALL);
    }

    #[test]
    fn only_the_own_server_has_a_generic_offer() {
        for offer in all_offers() {
            let generic = offer.provider == ImageEditProvider::OpenAiCompatible;
            assert_eq!(offer.model_id.is_empty(), generic, "{offer:?}");
            assert_eq!(offer.label.is_empty(), generic, "{offer:?}");
        }
        assert_eq!(lookup(ImageEditProvider::OpenAiCompatible, " qwen-image-edit ").ok().map(|offer| offer.model_id), Some(""));
        assert!(matches!(lookup(ImageEditProvider::OpenAiCompatible, "  "), Err(ImageEditError::UnknownModel { .. })));
    }

    #[test]
    fn lookup_finds_exact_ids_only() {
        let offer = lookup(ImageEditProvider::OpenAi, "gpt-image-2").ok();
        assert_eq!(offer.map(|offer| (offer.model_id, *offer.rule == OPENAI_ARB)), Some(("gpt-image-2", true)));
        assert!(matches!(lookup(ImageEditProvider::OpenAi, "gpt-image-1"), Err(ImageEditError::UnknownModel { model_id }) if model_id == "gpt-image-1"));
        assert!(lookup(ImageEditProvider::OpenAi, "GPT-IMAGE-2").is_err());
        assert!(lookup(ImageEditProvider::Gemini, "gpt-image-2").is_err());
    }

    #[test]
    fn aspect_tier_offers_label_exactly_their_rule_table() {
        for offer in all_offers() {
            if let SizeParamStyle::AspectTier(entries) = offer.size_param {
                assert_eq!(entries.len(), offer.rule.sizes.len(), "{offer:?}");
                for (entry, size) in entries.iter().zip(offer.rule.sizes) {
                    assert_eq!((entry.width, entry.height), *size, "{offer:?}");
                }
            }
        }
        let flash = lookup(ImageEditProvider::Gemini, "gemini-3.1-flash-image").ok().map(|offer| offer.size_param);
        assert_eq!(flash, Some(SizeParamStyle::AspectTier(&GEMINI_31_FLASH_ENTRIES)));
    }

    #[test]
    fn retire_dates_are_pinned() {
        let retiring: Vec<(ImageEditProvider, &str, (u16, u8, u8))> = all_offers().iter().filter_map(|offer| offer.retires_on.map(|date| (offer.provider, offer.model_id, date))).collect();
        assert_eq!(retiring, [(ImageEditProvider::OpenAi, "gpt-image-1.5", (2026, 12, 1)), (ImageEditProvider::OpenAi, "gpt-image-1-mini", (2026, 12, 1))]);
        for offer in all_offers() {
            for excluded in ["gpt-image-1", "openai/gpt-image-1", "chatgpt-image-latest", "dall-e-3", "gemini-2.5-flash-image", "google/gemini-2.5-flash-image"] {
                assert_ne!(offer.model_id, excluded, "{offer:?}");
            }
        }
    }

    #[test]
    fn every_rule_is_coherent() {
        for offer in all_offers() {
            let rule = offer.rule;
            assert!(rule.multiple >= 1 && rule.min_side >= 1, "{offer:?}");
            assert!(rule.effective_max_upscale() >= 1, "{offer:?}");
            if let Some(max_side) = rule.max_side {
                assert!(max_side >= rule.min_side && max_side >= rule.multiple, "{offer:?}");
            }
            if let (Some(min_area), Some(max_area)) = (rule.min_area, rule.max_area) {
                assert!(min_area <= max_area, "{offer:?}");
            }
            if let Some(aspect) = rule.aspect {
                assert!(aspect.max_w_over_h >= 1.0 && aspect.max_h_over_w >= 1.0, "{offer:?}");
            }
            // A size table is the whole rule: the scalar fields stay unconstrained.
            if !rule.sizes.is_empty() {
                assert!(rule.multiple == 1 && rule.min_side == 1 && rule.max_side.is_none() && rule.min_area.is_none() && rule.max_area.is_none() && rule.aspect.is_none(), "{offer:?}");
            }
        }
    }

    // Fixed-size APIs carry their documented tables, so the frame offers only accepted sizes.
    #[test]
    fn fixed_size_rows_pin_their_tables_and_ids() {
        let runway = lookup(ImageEditProvider::Runway, "gen4_image").unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!((runway.rule.sizes.len(), runway.evidence, runway.size_param), (16, SizeEvidence::Documented, SizeParamStyle::None));
        assert!(runway.rule.sizes.contains(&(1920, 1080)) && runway.rule.sizes.contains(&(720, 720)) && !runway.rule.sizes.contains(&(1024, 768)));
        let ark: Vec<&str> = offers(ImageEditProvider::BytePlus).map(|offer| offer.model_id).collect();
        assert_eq!(ark, ["dola-seedream-5-0-pro-260628", "seedream-5-0-lite-260128"]);
        assert_eq!(offers(ImageEditProvider::Recraft).map(|offer| (offer.model_id, offer.mask)).collect::<Vec<_>>(), [("recraftv3", MaskSupport::HardRequired)]);
        assert!(lookup(ImageEditProvider::Together, "Qwen/Qwen-Image-2.0-Pro").is_err());
    }

    // Reference counts are documented per row (sources at the rows); everything unconfirmed,
    // the user's own server and every single-image field stays at 0.
    #[test]
    fn reference_rows_are_pinned() {
        let count = |provider: ImageEditProvider, id: &str| lookup(provider, id).ok().map(|offer| offer.max_extra_references);
        assert_eq!(count(ImageEditProvider::OpenAi, "gpt-image-2"), Some(15));
        assert_eq!(count(ImageEditProvider::Gemini, "gemini-3-pro-image"), Some(13));
        assert_eq!(count(ImageEditProvider::Gemini, "gemini-3.1-flash-lite-image"), Some(0));
        assert_eq!(count(ImageEditProvider::Runway, "gen4_image"), Some(2));
        assert_eq!(count(ImageEditProvider::DashScope, "qwen-image-edit-plus"), Some(2));
        assert_eq!(count(ImageEditProvider::Bfl, "flux-3-image"), Some(9));
        assert_eq!(count(ImageEditProvider::Bfl, "flux-2-pro"), Some(0));
        assert_eq!(count(ImageEditProvider::OpenAiCompatible, "any-model"), Some(0));
        let none = [ImageEditProvider::Xai, ImageEditProvider::Ideogram, ImageEditProvider::Recraft, ImageEditProvider::Luma, ImageEditProvider::BytePlus, ImageEditProvider::DeepInfra, ImageEditProvider::AimlApi, ImageEditProvider::AiTunnel, ImageEditProvider::ProxyApi, ImageEditProvider::RouterAi, ImageEditProvider::Runware, ImageEditProvider::GenApi];
        for provider in none {
            assert!(offers(provider).all(|offer| !offer.accepts_references()), "{provider:?}");
        }
        assert!(all_offers().iter().all(|offer| offer.accepts_references() == (offer.max_extra_references > 0)));
    }

    #[test]
    fn mask_rows_match_the_plan() {
        let hard_required: Vec<&str> = all_offers().iter().filter(|offer| offer.mask == MaskSupport::HardRequired).map(|offer| offer.model_id).collect();
        assert_eq!(hard_required, ["flux-pro-1.0-fill", "recraftv3", "fal-ai/flux-pro/v1/fill", "fal-ai/qwen-image-edit/inpaint", "black-forest-labs/flux-fill-pro", "ideogram-ai/ideogram-v3-quality", "bfl:1@2", "ideogram:4@3"]);
        assert_eq!(lookup(ImageEditProvider::Ideogram, "ideogram-4-5").ok().map(|offer| (offer.mask, offer.evidence)), Some((MaskSupport::Hard, SizeEvidence::Documented)));
        // Gemini takes no mask anywhere (semantic masking only).
        assert!(all_offers().iter().filter(|offer| offer.family == "Nano Banana").all(|offer| offer.mask == MaskSupport::None));
    }
}
