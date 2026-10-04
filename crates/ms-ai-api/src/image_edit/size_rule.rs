/*
File: crates/ms-ai-api/src/image_edit/size_rule.rs

Purpose:
The size-rule DATA of the image-edit catalogue: which input sizes a model accepts and returns
unchanged, as plain constants. This file never evaluates a rule. The one evaluator (validity,
the upscale factor `k`, snapping) is the cleaning frame's geometry, which maps an
`ImageSizeRule` field for field onto its `FrameConstraints`; a second evaluator here would be
a second owner of the same decision.

Key structures:
- ImageSizeRule, ImageAspectLimit
- SizeEvidence
- AspectTierEntry (Gemini-style `(aspectRatio, imageSize)` labels of an allowed size)

Named rules:
OPENAI_ARB, OPENAI_STD3, GENAPI_GPT_IMAGE, RUNWAY_GEN4, GEMINI_31_FLASH, GEMINI_3_PRO,
GEMINI_31_LITE, FLUX2, FLUX2_MAX_2048, TOGETHER_FLUX2, KONTEXT, QWEN_EDIT_PLUS, QWEN_3, WAN_27,
TENCENT_35, TENCENT_3, SEEDREAM_5, ARK_SEEDREAM_5_PRO, ARK_SEEDREAM_5_LITE, IDEOGRAM_45, KLING_O1,
UNVERIFIED_DEFAULT, CUSTOM_OPENAI.

Notes:
Sources are the research notes in `dev-docs/image_edit/` (fetched 2026-10-04) and the
provider pages cited beside each constant. A rule only promises what its `SizeEvidence`
says; whether the provider really returns the sent size is checked on every run (a
`SizeMismatch` error, never a resample).
*/

/// How strongly the catalogue knows that a model returns exactly the size it was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeEvidence {
    /// The provider documents that the output equals the input or the requested size.
    Documented,
    /// The provider documents an explicit size parameter and its limits, but not that the
    /// output always equals it.
    ParamOnly,
    /// Neither: a conservative default or a partly documented rule. The UI warns that the
    /// response size is not confirmed.
    Unverified,
}

/// Aspect-ratio limit as two independent maxima (`w <= h * max_w_over_h` and
/// `h <= w * max_h_over_w`), mirroring the cleaning frame's `AspectLimit`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageAspectLimit {
    /// Largest allowed width / height.
    pub max_w_over_h: f32,
    /// Largest allowed height / width.
    pub max_h_over_w: f32,
}

impl ImageAspectLimit {
    /// The same maximum `ratio` (>= 1) in both orientations, e.g. `3.0` for "1:3 .. 3:1".
    #[must_use]
    pub const fn symmetric(ratio: f32) -> Self {
        Self { max_w_over_h: ratio, max_h_over_w: ratio }
    }
}

/// The sizes a model accepts and returns unchanged, as data only (see the file header). Every
/// field maps 1:1 onto the cleaning frame's `FrameConstraints`. All sizes are in pixels of
/// the image SENT to the provider (after the caller's integer upscale).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageSizeRule {
    /// Both sides must be whole multiples of this; `1` = no grid.
    pub multiple: u32,
    /// Smallest allowed side; `1` = no extra minimum.
    pub min_side: u32,
    /// Largest allowed side, `None` = unlimited.
    pub max_side: Option<u32>,
    /// Smallest allowed `w * h`, `None` = unlimited.
    pub min_area: Option<u64>,
    /// Largest allowed `w * h`, `None` = unlimited.
    pub max_area: Option<u64>,
    /// Aspect limit, `None` = unlimited.
    pub aspect: Option<ImageAspectLimit>,
    /// Allowed-size table `(width, height)`; empty = any size the other fields allow. A
    /// non-empty table is the whole rule (the scalar fields are then left unconstrained).
    pub sizes: &'static [(u32, u32)],
    /// Largest integer upscale `K` the caller may apply before sending (`0` reads as 1).
    pub max_upscale: u8,
}

impl ImageSizeRule {
    /// A rule that accepts every size, with no upscale. Base for struct-update literals.
    pub const UNCONSTRAINED: Self = Self { multiple: 1, min_side: 1, max_side: None, min_area: None, max_area: None, aspect: None, sizes: &[], max_upscale: 1 };

    /// The effective upscale maximum (`max_upscale`, with `0` read as 1).
    #[must_use]
    pub const fn effective_max_upscale(&self) -> u8 {
        if self.max_upscale == 0 { 1 } else { self.max_upscale }
    }
}

/// One entry of a Gemini-style allowed-size table: the exact output pixels of the
/// `(aspect, tier)` request labels. Used both as the rule's size table and as the request's
/// size labels, so the two can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AspectTierEntry {
    /// The `aspectRatio` / `aspect_ratio` label, e.g. `"16:9"`.
    pub aspect: &'static str,
    /// The `imageSize` / `resolution` label, e.g. `"1K"`.
    pub tier: &'static str,
    /// Exact output width in pixels.
    pub width: u32,
    /// Exact output height in pixels.
    pub height: u32,
}

/// Shorthand for the table literals below.
const fn entry(aspect: &'static str, tier: &'static str, width: u32, height: u32) -> AspectTierEntry {
    AspectTierEntry { aspect, tier, width, height }
}

/// The `(width, height)` list of an entry table, for `ImageSizeRule::sizes`.
const fn sizes_of<const N: usize>(entries: &[AspectTierEntry; N]) -> [(u32, u32); N] {
    let mut out = [(0, 0); N];
    let mut index = 0;
    while index < N {
        out[index] = (entries[index].width, entries[index].height);
        index += 1;
    }
    out
}

/// Default upscale allowance `K` of every rule that can be upscaled (manager decision).
const K: u8 = 4;

/// `OpenAI` `gpt-image-2` / `-2.5-*`: arbitrary `WxH`, both sides /16, edge <= 3840, total
/// pixels 655,360..8,294,400, aspect 1:3..3:1 (`research_openai_gemini` §A: reference `size`
/// and the image-generation guide).
pub const OPENAI_ARB: ImageSizeRule = ImageSizeRule {
    multiple: 16,
    max_side: Some(3840),
    min_area: Some(655_360),
    max_area: Some(8_294_400),
    aspect: Some(ImageAspectLimit::symmetric(3.0)),
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// The three standard GPT Image sizes (older `OpenAI` models; aimlapi's `gpt-image-2` schema).
pub const OPENAI_STD3: ImageSizeRule = ImageSizeRule { sizes: &[(1024, 1024), (1536, 1024), (1024, 1536)], max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

/// `GenAPI`'s `gpt-image-2` / `gpt-image-2-5` `image_size` preset list (gen-api.ru model pages,
/// fetched 2026-10-04).
pub const GENAPI_GPT_IMAGE: ImageSizeRule = ImageSizeRule {
    sizes: &[(1024, 768), (1024, 1024), (1024, 1536), (1920, 1080), (1080, 1920), (2560, 1440), (1440, 2560), (3840, 2160), (2160, 3840)],
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Runway `gen4_image`: `ratio` is required and is "the resolution of the output image", one of
/// these 16 values (docs.dev.runwayml.com/api.md, `POST /v1/text_to_image` - model
/// `gen4_image`, fetched 2026-10-04).
pub const RUNWAY_GEN4: ImageSizeRule = ImageSizeRule {
    sizes: &[
        (1024, 1024),
        (1080, 1080),
        (1168, 880),
        (1360, 768),
        (1440, 1080),
        (1080, 1440),
        (1808, 768),
        (1920, 1080),
        (1080, 1920),
        (2112, 912),
        (1280, 720),
        (720, 1280),
        (720, 720),
        (960, 720),
        (720, 960),
        (1680, 720),
    ],
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Gemini 3.1 Flash Image exact output pixels: 14 aspect ratios x {512, 1K, 2K, 4K}
/// (`research_openai_gemini` §B). The 21:9 @ 512 entry (`792x168`) is left out: the official
/// table's value is a typo (4.7:1, not 21:9), so it cannot be relied on.
pub const GEMINI_31_FLASH_ENTRIES: [AspectTierEntry; 55] = [
    entry("1:1", "512", 512, 512),
    entry("1:1", "1K", 1024, 1024),
    entry("1:1", "2K", 2048, 2048),
    entry("1:1", "4K", 4096, 4096),
    entry("1:4", "512", 256, 1024),
    entry("1:4", "1K", 512, 2048),
    entry("1:4", "2K", 1024, 4096),
    entry("1:4", "4K", 2048, 8192),
    entry("1:8", "512", 192, 1536),
    entry("1:8", "1K", 384, 3072),
    entry("1:8", "2K", 768, 6144),
    entry("1:8", "4K", 1536, 12288),
    entry("2:3", "512", 424, 632),
    entry("2:3", "1K", 848, 1264),
    entry("2:3", "2K", 1696, 2528),
    entry("2:3", "4K", 3392, 5056),
    entry("3:2", "512", 632, 424),
    entry("3:2", "1K", 1264, 848),
    entry("3:2", "2K", 2528, 1696),
    entry("3:2", "4K", 5056, 3392),
    entry("3:4", "512", 448, 600),
    entry("3:4", "1K", 896, 1200),
    entry("3:4", "2K", 1792, 2400),
    entry("3:4", "4K", 3584, 4800),
    entry("4:1", "512", 1024, 256),
    entry("4:1", "1K", 2048, 512),
    entry("4:1", "2K", 4096, 1024),
    entry("4:1", "4K", 8192, 2048),
    entry("4:3", "512", 600, 448),
    entry("4:3", "1K", 1200, 896),
    entry("4:3", "2K", 2400, 1792),
    entry("4:3", "4K", 4800, 3584),
    entry("4:5", "512", 464, 576),
    entry("4:5", "1K", 928, 1152),
    entry("4:5", "2K", 1856, 2304),
    entry("4:5", "4K", 3712, 4608),
    entry("5:4", "512", 576, 464),
    entry("5:4", "1K", 1152, 928),
    entry("5:4", "2K", 2304, 1856),
    entry("5:4", "4K", 4608, 3712),
    entry("8:1", "512", 1536, 192),
    entry("8:1", "1K", 3072, 384),
    entry("8:1", "2K", 6144, 768),
    entry("8:1", "4K", 12288, 1536),
    entry("9:16", "512", 384, 688),
    entry("9:16", "1K", 768, 1376),
    entry("9:16", "2K", 1536, 2752),
    entry("9:16", "4K", 3072, 5504),
    entry("16:9", "512", 688, 384),
    entry("16:9", "1K", 1376, 768),
    entry("16:9", "2K", 2752, 1536),
    entry("16:9", "4K", 5504, 3072),
    entry("21:9", "1K", 1584, 672),
    entry("21:9", "2K", 3168, 1344),
    entry("21:9", "4K", 6336, 2688),
];

/// Gemini 3 Pro Image: the same 1K / 2K / 4K pixels for its 10 aspect ratios (no 512 tier, no
/// 1:4 / 4:1 / 1:8 / 8:1) (`research_openai_gemini` §B).
pub const GEMINI_3_PRO_ENTRIES: [AspectTierEntry; 30] = [
    entry("1:1", "1K", 1024, 1024),
    entry("1:1", "2K", 2048, 2048),
    entry("1:1", "4K", 4096, 4096),
    entry("2:3", "1K", 848, 1264),
    entry("2:3", "2K", 1696, 2528),
    entry("2:3", "4K", 3392, 5056),
    entry("3:2", "1K", 1264, 848),
    entry("3:2", "2K", 2528, 1696),
    entry("3:2", "4K", 5056, 3392),
    entry("3:4", "1K", 896, 1200),
    entry("3:4", "2K", 1792, 2400),
    entry("3:4", "4K", 3584, 4800),
    entry("4:3", "1K", 1200, 896),
    entry("4:3", "2K", 2400, 1792),
    entry("4:3", "4K", 4800, 3584),
    entry("4:5", "1K", 928, 1152),
    entry("4:5", "2K", 1856, 2304),
    entry("4:5", "4K", 3712, 4608),
    entry("5:4", "1K", 1152, 928),
    entry("5:4", "2K", 2304, 1856),
    entry("5:4", "4K", 4608, 3712),
    entry("9:16", "1K", 768, 1376),
    entry("9:16", "2K", 1536, 2752),
    entry("9:16", "4K", 3072, 5504),
    entry("16:9", "1K", 1376, 768),
    entry("16:9", "2K", 2752, 1536),
    entry("16:9", "4K", 5504, 3072),
    entry("21:9", "1K", 1584, 672),
    entry("21:9", "2K", 3168, 1344),
    entry("21:9", "4K", 6336, 2688),
];

/// Gemini 3.1 Flash Lite Image: the 14 aspect ratios at 1K only (`research_openai_gemini` §B,
/// "only supports 1K images").
pub const GEMINI_31_LITE_ENTRIES: [AspectTierEntry; 14] = [
    entry("1:1", "1K", 1024, 1024),
    entry("1:4", "1K", 512, 2048),
    entry("1:8", "1K", 384, 3072),
    entry("2:3", "1K", 848, 1264),
    entry("3:2", "1K", 1264, 848),
    entry("3:4", "1K", 896, 1200),
    entry("4:1", "1K", 2048, 512),
    entry("4:3", "1K", 1200, 896),
    entry("4:5", "1K", 928, 1152),
    entry("5:4", "1K", 1152, 928),
    entry("8:1", "1K", 3072, 384),
    entry("9:16", "1K", 768, 1376),
    entry("16:9", "1K", 1376, 768),
    entry("21:9", "1K", 1584, 672),
];

/// `GEMINI_31_FLASH_ENTRIES` as a size table.
const GEMINI_31_FLASH_SIZES: [(u32, u32); 55] = sizes_of(&GEMINI_31_FLASH_ENTRIES);
/// `GEMINI_3_PRO_ENTRIES` as a size table.
const GEMINI_3_PRO_SIZES: [(u32, u32); 30] = sizes_of(&GEMINI_3_PRO_ENTRIES);
/// `GEMINI_31_LITE_ENTRIES` as a size table.
const GEMINI_31_LITE_SIZES: [(u32, u32); 14] = sizes_of(&GEMINI_31_LITE_ENTRIES);

/// Gemini 3.1 Flash Image (and its resellers' `aspect_ratio` + `resolution` forms).
pub const GEMINI_31_FLASH: ImageSizeRule = ImageSizeRule { sizes: &GEMINI_31_FLASH_SIZES, max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };
/// Gemini 3 Pro Image ("Nano Banana Pro").
pub const GEMINI_3_PRO: ImageSizeRule = ImageSizeRule { sizes: &GEMINI_3_PRO_SIZES, max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };
/// Gemini 3.1 Flash Lite Image (1K only).
pub const GEMINI_31_LITE: ImageSizeRule = ImageSizeRule { sizes: &GEMINI_31_LITE_SIZES, max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

/// BFL FLUX.2 `width` / `height`: minimum 64, up to 4 MP; the /16 grid is stated by
/// Replicate's FLUX.2 schema (`research_flux_qwen` §A).
pub const FLUX2: ImageSizeRule = ImageSizeRule { multiple: 16, min_side: 64, max_area: Some(4_194_304), max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

/// FLUX.2 behind a 2048 px side cap: Replicate's `aspect_ratio: "custom"` width / height
/// ("multiple of 16", "maximum image size is 2048x2048"; replicate.com flux-2-pro / flux-2-max
/// llms.txt, fetched 2026-10-04).
pub const FLUX2_MAX_2048: ImageSizeRule = ImageSizeRule { multiple: 16, min_side: 64, max_side: Some(2048), max_area: Some(4_194_304), max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

/// FLUX.2 on Together: `width` / `height` 256..1920 each (docs.together.ai FLUX quickstart
/// parameter table, fetched 2026-10-04); the /16 grid as for BFL's FLUX.2.
pub const TOGETHER_FLUX2: ImageSizeRule = ImageSizeRule { multiple: 16, min_side: 256, max_side: Some(1920), max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

/// FLUX.1 Kontext: "match the input image dimensions as closely as possible (rounded to
/// multiples of 32)", "~1MP", ratios 3:7..7:3 (`research_flux_qwen` §A). The area window
/// around 1 MP is a tentative reading of "~1MP".
pub const KONTEXT: ImageSizeRule = ImageSizeRule {
    multiple: 32,
    min_area: Some(786_432),
    max_area: Some(1_048_576),
    aspect: Some(ImageAspectLimit::symmetric(7.0 / 3.0)),
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Qwen-Image-Edit plus / max (and qwen-image-2.0): `size` "W*H", each side 512..2048,
/// snapped to /16 (verbatim: 1033*1032 -> 1040*1024) (`research_flux_qwen` §B).
pub const QWEN_EDIT_PLUS: ImageSizeRule = ImageSizeRule { multiple: 16, min_side: 512, max_side: Some(2048), max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

/// qwen-image-3.0: total pixels 512²..2048², aspect 1:8..8:1, sides 384..2048 per input
/// image; the /16 grid is inferred from the 2.x models (`research_flux_qwen` §B,
/// `services_cn_ru` [1]).
pub const QWEN_3: ImageSizeRule = ImageSizeRule {
    multiple: 16,
    min_side: 384,
    max_side: Some(2048),
    min_area: Some(262_144),
    max_area: Some(4_194_304),
    aspect: Some(ImageAspectLimit::symmetric(8.0)),
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Wan 2.7 Image editing `size` "W*H": total pixels 768²..2048², aspect 1:8..8:1, and "the pixel
/// values of the output image may have minor differences from the specified values"
/// (alibabacloud.com model-studio wan-image-generation-and-editing-api-reference, fetched
/// 2026-10-04); /16 grid assumed.
pub const WAN_27: ImageSizeRule = ImageSizeRule {
    multiple: 16,
    min_area: Some(589_824),
    max_area: Some(4_194_304),
    aspect: Some(ImageAspectLimit::symmetric(8.0)),
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Tencent Hy-Image 3.5: `size` `"WxH"`, positive integers 256..8192 per side, area <= 16,777,216;
/// "If a specific value is passed, the image is generated at the specified size"
/// (`services_cn_ru` [15], the `TokenHub` reference fetched 2026-10-04).
pub const TENCENT_35: ImageSizeRule = ImageSizeRule { min_side: 256, max_side: Some(8192), max_area: Some(16_777_216), max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

/// Tencent Hy-Image 3.0: 512..2048 px per side, area <= 1024²; /16 grid assumed
/// (`services_cn_ru` [15]).
pub const TENCENT_3: ImageSizeRule = ImageSizeRule { multiple: 16, min_side: 512, max_side: Some(2048), max_area: Some(1_048_576), max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

/// Seedream 5.0: total pixels 1024²..2048², aspect 1/16..16 (fal `bytedance/seedream/v5/pro/edit`
/// schema `x-fal` limits, fetched 2026-10-04); /16 grid assumed.
pub const SEEDREAM_5: ImageSizeRule = ImageSizeRule {
    multiple: 16,
    min_area: Some(1_048_576),
    max_area: Some(4_194_304),
    aspect: Some(ImageAspectLimit::symmetric(16.0)),
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Seedream 5.0 pro on `BytePlus` `ModelArk`: `size` `"WxH"` with total pixels 1280x720 (921,600) ..
/// 2048x2048x1.1025 (4,624,220) and aspect 1/16..16 (docs.byteplus.com `ModelArk` 1541523,
/// fetched 2026-10-04). The grid is undocumented (a valid example is 3750x1250 for lite); /16
/// is assumed so the returned size can match.
pub const ARK_SEEDREAM_5_PRO: ImageSizeRule = ImageSizeRule {
    multiple: 16,
    min_area: Some(921_600),
    max_area: Some(4_624_220),
    aspect: Some(ImageAspectLimit::symmetric(16.0)),
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Seedream 5.0 lite on `ModelArk`: total pixels 2560x1440 (3,686,400) .. 4096x4096
/// (16,777,216), aspect 1/16..16 (same page); /16 grid assumed.
pub const ARK_SEEDREAM_5_LITE: ImageSizeRule = ImageSizeRule {
    multiple: 16,
    min_area: Some(3_686_400),
    max_area: Some(16_777_216),
    aspect: Some(ImageAspectLimit::symmetric(16.0)),
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Ideogram 4.5 precise edit: "The output always matches this image's width and height";
/// aspect outside 1:6..6:1 is rejected (developer.ideogram.ai precise-edit reference). No
/// upscale: the output already equals the input, so `K = 1`.
pub const IDEOGRAM_45: ImageSizeRule = ImageSizeRule { aspect: Some(ImageAspectLimit::symmetric(6.0)), max_upscale: 1, ..ImageSizeRule::UNCONSTRAINED };

/// The conservative rule of every model whose exact output size is undocumented: /16, sides
/// 256..2048, <= 1 MP, aspect <= 4:1. A planner's choice, not a provider fact.
pub const UNVERIFIED_DEFAULT: ImageSizeRule = ImageSizeRule {
    multiple: 16,
    min_side: 256,
    max_side: Some(2048),
    max_area: Some(1_048_576),
    aspect: Some(ImageAspectLimit::symmetric(4.0)),
    max_upscale: K,
    ..ImageSizeRule::UNCONSTRAINED
};

/// Kling Image O1: the output size is only an aspect ratio plus a 1K / 2K tier (no pixel
/// table), so the conservative default applies, narrowed to the documented input limits:
/// sides at least 300 px (304 on the /16 grid) and aspect 1:2.5..2.5:1
/// (kling.ai/document-api api/image/o1/image-generation.md, fetched 2026-10-04).
pub const KLING_O1: ImageSizeRule = ImageSizeRule { min_side: 304, aspect: Some(ImageAspectLimit::symmetric(2.5)), ..UNVERIFIED_DEFAULT };

/// A local / custom OpenAI-compatible images server (stable-diffusion.cpp `sd-server` takes
/// `size` "WxH"): /16, sides 64..4096.
pub const CUSTOM_OPENAI: ImageSizeRule = ImageSizeRule { multiple: 16, min_side: 64, max_side: Some(4096), max_upscale: K, ..ImageSizeRule::UNCONSTRAINED };

#[cfg(test)]
mod tests {
    use super::{GEMINI_3_PRO, GEMINI_3_PRO_ENTRIES, GEMINI_31_FLASH, GEMINI_31_FLASH_ENTRIES, GEMINI_31_LITE, GEMINI_31_LITE_ENTRIES, IDEOGRAM_45, ImageSizeRule};

    #[test]
    fn gemini_tables_are_exactly_their_entries() {
        for (rule, entries) in [(&GEMINI_31_FLASH, &GEMINI_31_FLASH_ENTRIES[..]), (&GEMINI_3_PRO, &GEMINI_3_PRO_ENTRIES[..]), (&GEMINI_31_LITE, &GEMINI_31_LITE_ENTRIES[..])] {
            assert_eq!(rule.sizes.len(), entries.len());
            for (size, entry) in rule.sizes.iter().zip(entries) {
                assert_eq!(*size, (entry.width, entry.height));
            }
        }
    }

    #[test]
    fn gemini_flash_table_has_every_documented_entry() {
        // research_openai_gemini §B, per ratio at 512 / 1K / 2K / 4K; 21:9 @ 512 excluded.
        let documented: [(&str, [(u32, u32); 4]); 14] = [
            ("1:1", [(512, 512), (1024, 1024), (2048, 2048), (4096, 4096)]),
            ("1:4", [(256, 1024), (512, 2048), (1024, 4096), (2048, 8192)]),
            ("1:8", [(192, 1536), (384, 3072), (768, 6144), (1536, 12288)]),
            ("2:3", [(424, 632), (848, 1264), (1696, 2528), (3392, 5056)]),
            ("3:2", [(632, 424), (1264, 848), (2528, 1696), (5056, 3392)]),
            ("3:4", [(448, 600), (896, 1200), (1792, 2400), (3584, 4800)]),
            ("4:1", [(1024, 256), (2048, 512), (4096, 1024), (8192, 2048)]),
            ("4:3", [(600, 448), (1200, 896), (2400, 1792), (4800, 3584)]),
            ("4:5", [(464, 576), (928, 1152), (1856, 2304), (3712, 4608)]),
            ("5:4", [(576, 464), (1152, 928), (2304, 1856), (4608, 3712)]),
            ("8:1", [(1536, 192), (3072, 384), (6144, 768), (12288, 1536)]),
            ("9:16", [(384, 688), (768, 1376), (1536, 2752), (3072, 5504)]),
            ("16:9", [(688, 384), (1376, 768), (2752, 1536), (5504, 3072)]),
            ("21:9", [(792, 168), (1584, 672), (3168, 1344), (6336, 2688)]),
        ];
        let tiers = ["512", "1K", "2K", "4K"];
        let mut expected = 0;
        for (aspect, sizes) in documented {
            for (tier, (width, height)) in tiers.iter().zip(sizes) {
                let found = GEMINI_31_FLASH_ENTRIES.iter().find(|entry| entry.aspect == aspect && entry.tier == *tier);
                if aspect == "21:9" && *tier == "512" {
                    assert!(found.is_none(), "the 21:9 @ 512 typo row must stay excluded");
                    continue;
                }
                let found = found.unwrap_or_else(|| panic!("missing {aspect} @ {tier}"));
                assert_eq!((found.width, found.height), (width, height), "{aspect} @ {tier}");
                expected += 1;
                // Pro and Lite repeat the Flash pixels for the ratios / tiers they offer.
                if let Some(pro) = GEMINI_3_PRO_ENTRIES.iter().find(|entry| entry.aspect == aspect && entry.tier == *tier) {
                    assert_eq!((pro.width, pro.height), (width, height), "pro {aspect} @ {tier}");
                }
                if let Some(lite) = GEMINI_31_LITE_ENTRIES.iter().find(|entry| entry.aspect == aspect && entry.tier == *tier) {
                    assert_eq!((lite.width, lite.height), (width, height), "lite {aspect} @ {tier}");
                }
            }
        }
        assert_eq!(expected, GEMINI_31_FLASH_ENTRIES.len());
    }

    #[test]
    fn gemini_pro_and_lite_offer_their_documented_ratio_sets() {
        let pro_ratios = ["1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"];
        for ratio in pro_ratios {
            for tier in ["1K", "2K", "4K"] {
                assert!(GEMINI_3_PRO_ENTRIES.iter().any(|entry| entry.aspect == ratio && entry.tier == tier), "pro {ratio} @ {tier}");
            }
        }
        assert!(GEMINI_3_PRO_ENTRIES.iter().all(|entry| entry.tier != "512"));
        assert!(GEMINI_31_LITE_ENTRIES.iter().all(|entry| entry.tier == "1K"));
        let mut lite_ratios: Vec<&str> = GEMINI_31_LITE_ENTRIES.iter().map(|entry| entry.aspect).collect();
        lite_ratios.dedup();
        assert_eq!(lite_ratios.len(), 14);
    }

    #[test]
    fn entries_are_unique_per_table() {
        for entries in [&GEMINI_31_FLASH_ENTRIES[..], &GEMINI_3_PRO_ENTRIES[..], &GEMINI_31_LITE_ENTRIES[..]] {
            for (index, entry) in entries.iter().enumerate() {
                assert!(entries[index + 1..].iter().all(|other| (other.aspect, other.tier) != (entry.aspect, entry.tier) && (other.width, other.height) != (entry.width, entry.height)), "{entry:?}");
            }
        }
    }

    #[test]
    fn upscale_maximum_reads_zero_as_one() {
        assert_eq!(ImageSizeRule { max_upscale: 0, ..ImageSizeRule::UNCONSTRAINED }.effective_max_upscale(), 1);
        assert_eq!(IDEOGRAM_45.effective_max_upscale(), 1);
        assert_eq!(GEMINI_31_FLASH.effective_max_upscale(), 4);
    }
}
