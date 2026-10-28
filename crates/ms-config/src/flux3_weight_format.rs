/*
File: crates/ms-config/src/flux3_weight_format.rs

Purpose:
The ONE list of FLUX.3 klein weight formats the «ИИ-редактор области» may run each component
in: the diffusion transformer (DiT: the three native BFL formats bf16 / fp8r / nvfp4 plus the
unsloth GGUF quants) and the Qwen3-VL text encoder (bf16 shards plus the official Qwen GGUF
quants). No other crate and nothing in the Python backend enumerates these formats; the
backend treats a GGUF id as opaque and knows only the three native DiT ids.

Key items:
- `Flux3DitFormat`: the DiT format, its frozen wire id (`wire` / `from_wire`), its GGUF quant
  token (`quant_token`), `is_gguf` / `is_native`; `all()` is the quality order, best first;
  `offered_on(runtime)` the formats a runtime can run (PyTorch: all; Stable-Diffusion.cpp: bf16
  and the GGUF quants, which it reads natively; it has no BFL fp8r / nvfp4 kernels).
- `Flux3EncoderFormat`: the text-encoder format, same surface; `all()` best first (the same
  list on both image-edit runtimes).
- `FLUX3_PROMPT_LIBRARY_ID`: the frozen prompt-library id of FLUX.3 klein (one library for
  every encoder format).
- `flux3_prompt_cache_precision_order` / `flux3_prompt_cache_precision_rank`: the prompt cache's
  precision preference (the encoder format ids, most precise first) and an id's place in it.

Notes:
- The files behind each format are pinned in `ms_sysprobe::ai_models::external_catalog`
  (`flux3_klein_dit_spec`, `flux3_klein_encoder_spec`, `flux3_klein_package_specs`); its tests
  require every format to resolve to a spec.
- Wire ids are persisted in the engine's settings file and sent to the backend: frozen. The
  native DiT ids equal the ones the engine persisted before GGUF existed.
*/

use crate::sdcpp_choice::ImageEditRuntime;

/// The weight format of the FLUX.3 klein diffusion transformer.
///
/// `Bf16`, `Fp8r` and `Nvfp4` are the BFL repository's own files (root `model.safetensors`
/// and `variants/<format>/`); the GGUF variants are single unsloth files dequantized per
/// forward by the backend. `Default` is `Fp8r`, the engine's historic default: near-reference
/// quality at about half the bf16 size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Flux3DitFormat {
    /// Full precision bf16, the reference quality (about 11.1 GB).
    Bf16,
    /// BFL row-wise fp8 (about 6.3 GB).
    #[default]
    Fp8r,
    /// BFL 4-bit NVFP4 (about 5.3 GB).
    Nvfp4,
    /// GGUF `Q8_0`.
    Q80,
    /// GGUF `Q6_K`.
    Q6K,
    /// GGUF `Q5_K_M`.
    Q5KM,
    /// GGUF `Q4_K_M`.
    Q4KM,
}

/// Every DiT format in quality order, best first (plan decision D3): the 8-bit GGUF sits below
/// the BFL fp8 rows, NVFP4 between the 5-bit and the 4-bit GGUF quants.
const ALL_DIT_FORMATS: [Flux3DitFormat; 7] = [
    Flux3DitFormat::Bf16,
    Flux3DitFormat::Fp8r,
    Flux3DitFormat::Q80,
    Flux3DitFormat::Q6K,
    Flux3DitFormat::Q5KM,
    Flux3DitFormat::Nvfp4,
    Flux3DitFormat::Q4KM,
];

/// The DiT formats Stable-Diffusion.cpp runs: [`ALL_DIT_FORMATS`] without the BFL fp8r / nvfp4
/// files (no kernels for them there), same order (a test pins the agreement).
const SDCPP_DIT_FORMATS: [Flux3DitFormat; 5] = [
    Flux3DitFormat::Bf16,
    Flux3DitFormat::Q80,
    Flux3DitFormat::Q6K,
    Flux3DitFormat::Q5KM,
    Flux3DitFormat::Q4KM,
];

impl Flux3DitFormat {
    /// Every DiT format, best quality first (the order format rows and refusal suggestions use).
    #[must_use]
    pub fn all() -> &'static [Self] {
        &ALL_DIT_FORMATS
    }

    /// The DiT formats `runtime` can run, best quality first: every format on PyTorch; bf16 and
    /// the GGUF quants on Stable-Diffusion.cpp. The encoder list does not depend on the runtime.
    #[must_use]
    pub fn offered_on(runtime: ImageEditRuntime) -> &'static [Self] {
        match runtime {
            ImageEditRuntime::PyTorch => &ALL_DIT_FORMATS,
            ImageEditRuntime::SdCpp => &SDCPP_DIT_FORMATS,
        }
    }

    /// The frozen id this format is persisted and sent on the wire with (matches
    /// `^[a-z0-9_]{1,32}$`). A literal by design: never localized, never renamed.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Bf16 => "bf16",
            Self::Fp8r => "fp8r",
            Self::Nvfp4 => "nvfp4",
            Self::Q80 => "q8_0",
            Self::Q6K => "q6_k",
            Self::Q5KM => "q5_k_m",
            Self::Q4KM => "q4_k_m",
        }
    }

    /// Parses a persisted or received id (surrounding whitespace ignored, otherwise exact).
    /// `None` for an unknown id; the caller decides the fallback (normally `Fp8r`).
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        let value = value.trim();
        ALL_DIT_FORMATS.into_iter().find(|format| format.wire() == value)
    }

    /// The llama.cpp quantization name of a GGUF format (`"Q8_0"`, …), `None` for the three
    /// native formats. A technical token shown as is in the UI (listed in
    /// `dev-docs/i18n_exclusions.md`).
    #[must_use]
    pub fn quant_token(self) -> Option<&'static str> {
        match self {
            Self::Bf16 | Self::Fp8r | Self::Nvfp4 => None,
            Self::Q80 => Some("Q8_0"),
            Self::Q6K => Some("Q6_K"),
            Self::Q5KM => Some("Q5_K_M"),
            Self::Q4KM => Some("Q4_K_M"),
        }
    }

    /// Whether the DiT is a single GGUF file (the backend is sent its absolute path).
    #[must_use]
    pub fn is_gguf(self) -> bool {
        self.quant_token().is_some()
    }

    /// Whether this is one of the BFL repository's own formats (`bf16` / `fp8r` / `nvfp4`),
    /// whose files the backend locates itself under the package root. Exactly `!is_gguf()`.
    #[must_use]
    pub fn is_native(self) -> bool {
        !self.is_gguf()
    }
}

/// The weight format of the FLUX.3 klein Qwen3-VL text encoder.
///
/// `Bf16` is the BFL repository's `text_encoder/` shards (the only format that also runs the
/// vision tower, so the only one supporting reference images in the encoder); the GGUF variants
/// are text-only files of the official `Qwen/Qwen3-VL-4B-Instruct-GGUF` repository. `Default` is
/// `Bf16`: a settings file written before encoder formats existed describes the shards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Flux3EncoderFormat {
    /// Unquantized bf16 safetensors shards (`text_encoder/`).
    #[default]
    Bf16,
    /// GGUF `Q8_0`.
    Q80,
    /// GGUF `Q4_K_M`.
    Q4KM,
}

/// Every encoder format, best quality first.
const ALL_ENCODER_FORMATS: [Flux3EncoderFormat; 3] = [Flux3EncoderFormat::Bf16, Flux3EncoderFormat::Q80, Flux3EncoderFormat::Q4KM];

impl Flux3EncoderFormat {
    /// Every encoder format, best quality first.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &ALL_ENCODER_FORMATS
    }

    /// The frozen id this format is persisted and sent on the wire with (matches
    /// `^[a-z0-9_]{1,32}$`). A literal by design: never localized, never renamed.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Bf16 => "bf16",
            Self::Q80 => "q8_0",
            Self::Q4KM => "q4_k_m",
        }
    }

    /// Parses a persisted or received id (surrounding whitespace ignored, otherwise exact).
    /// `None` for an unknown id; the caller decides the fallback (normally `Bf16`).
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        let value = value.trim();
        ALL_ENCODER_FORMATS.into_iter().find(|format| format.wire() == value)
    }

    /// The llama.cpp quantization name of a GGUF format (`"Q8_0"`, `"Q4_K_M"`), `None` for
    /// `Bf16`. A technical token shown as is in the UI (listed in `dev-docs/i18n_exclusions.md`).
    #[must_use]
    pub fn quant_token(self) -> Option<&'static str> {
        match self {
            Self::Bf16 => None,
            Self::Q80 => Some("Q8_0"),
            Self::Q4KM => Some("Q4_K_M"),
        }
    }

    /// Whether the encoder is a single GGUF file rather than the bf16 shards folder.
    #[must_use]
    pub fn is_gguf(self) -> bool {
        self.quant_token().is_some()
    }
}

/// The prompt-library id of FLUX.3 klein: one library for every encoder format (an entry's
/// precision tells the formats apart). Frozen: it names a backend directory of cached entries.
pub const FLUX3_PROMPT_LIBRARY_ID: &str = "flux3_klein";

/// The prompt-cache precision order: every [`Flux3EncoderFormat`] wire id in
/// [`Flux3EncoderFormat::all`] order (a test pins the agreement).
const PROMPT_CACHE_PRECISION_ORDER: [&str; 3] = ["bf16", "q8_0", "q4_k_m"];

/// The order in which the backend prefers FLUX.3 klein prompt-cache entries, most precise
/// first (sent as `params.prompt_cache_precision_order`). An entry's precision id is the
/// encoder format id it was made with; an id missing from this list ranks last. Rust owns the
/// order; the backend treats the ids as opaque.
#[must_use]
pub fn flux3_prompt_cache_precision_order() -> &'static [&'static str] {
    &PROMPT_CACHE_PRECISION_ORDER
}

/// The rank of a precision id in [`flux3_prompt_cache_precision_order`] (0 = most precise); an
/// unknown id ranks after every known one, exactly as the backend ranks it.
#[must_use]
pub fn flux3_prompt_cache_precision_rank(precision: &str) -> usize {
    PROMPT_CACHE_PRECISION_ORDER
        .iter()
        .position(|id| *id == precision)
        .unwrap_or(PROMPT_CACHE_PRECISION_ORDER.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the wire-id grammar `^[a-z0-9_]{1,32}$`.
    fn assert_wire_grammar(id: &str) {
        assert!(!id.is_empty() && id.len() <= 32, "{id}");
        assert!(id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'), "{id}");
    }

    #[test]
    fn dit_wire_ids_round_trip_and_are_frozen() {
        let ids: Vec<&str> = Flux3DitFormat::all().iter().map(|format| format.wire()).collect();
        assert_eq!(ids, ["bf16", "fp8r", "q8_0", "q6_k", "q5_k_m", "nvfp4", "q4_k_m"]);
        for &format in Flux3DitFormat::all() {
            assert_eq!(Flux3DitFormat::from_wire(format.wire()), Some(format));
            assert_wire_grammar(format.wire());
        }
        assert_eq!(Flux3DitFormat::from_wire(" nvfp4 "), Some(Flux3DitFormat::Nvfp4));
        assert_eq!(Flux3DitFormat::from_wire("FP8R"), None);
        assert_eq!(Flux3DitFormat::from_wire("Q8_0"), None);
        assert_eq!(Flux3DitFormat::from_wire(""), None);
        assert_eq!(Flux3DitFormat::from_wire("gguf"), None);
        assert_eq!(Flux3DitFormat::default(), Flux3DitFormat::Fp8r);
    }

    #[test]
    fn dit_quant_tokens_mark_exactly_the_gguf_formats() {
        let natives: Vec<Flux3DitFormat> = Flux3DitFormat::all().iter().copied().filter(|format| format.is_native()).collect();
        assert_eq!(natives, [Flux3DitFormat::Bf16, Flux3DitFormat::Fp8r, Flux3DitFormat::Nvfp4]);
        for &format in Flux3DitFormat::all() {
            assert_eq!(format.is_gguf(), !format.is_native(), "{format:?}");
            match format.quant_token() {
                // The token is the wire id in upper case: one spelling, two casings.
                Some(token) => assert_eq!(token.to_ascii_lowercase(), format.wire()),
                None => assert!(format.is_native()),
            }
        }
    }

    #[test]
    fn offered_on_lists_every_format_on_pytorch_and_no_bfl_quants_on_sdcpp() {
        assert_eq!(Flux3DitFormat::offered_on(ImageEditRuntime::PyTorch), Flux3DitFormat::all());
        let expected: Vec<Flux3DitFormat> =
            Flux3DitFormat::all().iter().copied().filter(|&format| format == Flux3DitFormat::Bf16 || format.is_gguf()).collect();
        assert_eq!(Flux3DitFormat::offered_on(ImageEditRuntime::SdCpp), expected.as_slice());
        assert!(!Flux3DitFormat::offered_on(ImageEditRuntime::SdCpp).contains(&Flux3DitFormat::Fp8r));
        assert!(!Flux3DitFormat::offered_on(ImageEditRuntime::SdCpp).contains(&Flux3DitFormat::Nvfp4));
    }

    #[test]
    fn encoder_wire_ids_round_trip_and_are_frozen() {
        let ids: Vec<&str> = Flux3EncoderFormat::all().iter().map(|format| format.wire()).collect();
        assert_eq!(ids, ["bf16", "q8_0", "q4_k_m"]);
        for &format in Flux3EncoderFormat::all() {
            assert_eq!(Flux3EncoderFormat::from_wire(format.wire()), Some(format));
            assert_wire_grammar(format.wire());
            assert_eq!(format.is_gguf(), format != Flux3EncoderFormat::Bf16);
            if let Some(token) = format.quant_token() {
                assert_eq!(token.to_ascii_lowercase(), format.wire());
            }
        }
        assert_eq!(Flux3EncoderFormat::from_wire(" q4_k_m "), Some(Flux3EncoderFormat::Q4KM));
        assert_eq!(Flux3EncoderFormat::from_wire("q6_k"), None);
        assert_eq!(Flux3EncoderFormat::from_wire(""), None);
        assert_eq!(Flux3EncoderFormat::default(), Flux3EncoderFormat::Bf16);
    }

    /// Every GGUF encoder quant is also a DiT quant with the same id (one vocabulary).
    #[test]
    fn encoder_gguf_ids_are_dit_ids() {
        for &format in Flux3EncoderFormat::all() {
            let dit = Flux3DitFormat::from_wire(format.wire());
            assert!(dit.is_some(), "{format:?}");
            assert_eq!(dit.map(Flux3DitFormat::quant_token), Some(format.quant_token()));
        }
    }

    #[test]
    fn the_prompt_cache_precision_order_is_the_encoder_format_list() {
        let expected: Vec<&str> = Flux3EncoderFormat::all().iter().map(|format| format.wire()).collect();
        assert_eq!(flux3_prompt_cache_precision_order(), expected.as_slice());
        assert_eq!(flux3_prompt_cache_precision_order(), ["bf16", "q8_0", "q4_k_m"]);
        assert_eq!(flux3_prompt_cache_precision_rank("bf16"), 0);
        assert!(flux3_prompt_cache_precision_rank("q8_0") < flux3_prompt_cache_precision_rank("q4_k_m"));
        assert_eq!(flux3_prompt_cache_precision_rank("fp8"), flux3_prompt_cache_precision_order().len(), "FLUX.3 has no fp8 encoder");
        assert_eq!(flux3_prompt_cache_precision_rank(""), flux3_prompt_cache_precision_order().len());
        assert_eq!(FLUX3_PROMPT_LIBRARY_ID, "flux3_klein");
        assert_wire_grammar(FLUX3_PROMPT_LIBRARY_ID);
    }
}
