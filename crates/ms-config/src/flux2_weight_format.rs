/*
File: crates/ms-config/src/flux2_weight_format.rs

Purpose:
The ONE list of FLUX.2 klein weight formats (bf16 plus the GGUF quants) the «ИИ-редактор
области» may run a component in, and which of them each variant offers per component
(transformer / text encoder). Both axes share this single list; no other crate and nothing in
the Python backend enumerates the quants (the backend treats the wire ids as opaque).

Key items:
- `Flux2WeightFormat`: the format, its frozen wire id (`wire` / `from_wire`), its GGUF quant
  token (`quant_token`) and `is_gguf`; `all()` is ordered best quality first.
- `Flux2WeightAxis`: which component a format choice applies to.
- `Flux2WeightFormat::offered_for`: per variant and axis, the formats the engine may select.

Notes:
- The files behind each offered format are pinned in
  `ms_sysprobe::ai_models::external_catalog` (`flux2_klein_package_specs`,
  `flux2_klein_weights_path`); its tests require every offered format to resolve to a spec.
- Wire ids are persisted in the engine's settings file and sent to the backend: frozen.
*/

use crate::Flux2Variant;

/// The weight format of one FLUX.2 klein component.
///
/// `Bf16` is the upstream diffusers folder; the other variants are single GGUF files of the
/// matching llama.cpp quantization, dequantized per forward by the backend. `Default` is
/// `Bf16`: a settings file written before formats existed describes the bf16 folders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Flux2WeightFormat {
    /// Unquantized bf16 safetensors (the upstream package).
    #[default]
    Bf16,
    /// GGUF `Q8_0`.
    Q80,
    /// GGUF `Q6_K`.
    Q6K,
    /// GGUF `Q5_K_M`.
    Q5KM,
    /// GGUF `Q4_K_M`.
    Q4KM,
}

/// Which FLUX.2 klein component a weight-format choice applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flux2WeightAxis {
    /// The diffusion transformer.
    Transformer,
    /// The Qwen3 text encoder.
    TextEncoder,
}

impl Flux2WeightAxis {
    /// Both axes, transformer first.
    #[must_use]
    pub fn all() -> [Self; 2] {
        [Self::Transformer, Self::TextEncoder]
    }
}

/// Every format, best quality first (the order `all()` returns and `offered_for` keeps).
const ALL_FORMATS: [Flux2WeightFormat; 5] = [
    Flux2WeightFormat::Bf16,
    Flux2WeightFormat::Q80,
    Flux2WeightFormat::Q6K,
    Flux2WeightFormat::Q5KM,
    Flux2WeightFormat::Q4KM,
];

/// The formats of a variant/axis without a GGUF choice.
const BF16_ONLY: [Flux2WeightFormat; 1] = [Flux2WeightFormat::Bf16];

impl Flux2WeightFormat {
    /// Every format, best quality first.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &ALL_FORMATS
    }

    /// The frozen id this format is persisted and sent on the wire with (matches
    /// `^[a-z0-9_]{1,32}$`). A literal by design: never localized, never renamed.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Bf16 => "bf16",
            Self::Q80 => "q8_0",
            Self::Q6K => "q6_k",
            Self::Q5KM => "q5_k_m",
            Self::Q4KM => "q4_k_m",
        }
    }

    /// Parses a persisted or received id (surrounding whitespace ignored, otherwise exact).
    /// `None` for an unknown id; the caller decides the fallback (normally `Bf16`).
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        let value = value.trim();
        ALL_FORMATS.into_iter().find(|format| format.wire() == value)
    }

    /// The llama.cpp quantization name of a GGUF format (`"Q8_0"`, …), `None` for `Bf16`.
    /// A technical token shown as is in the UI (listed in `dev-docs/i18n_exclusions.md`).
    #[must_use]
    pub fn quant_token(self) -> Option<&'static str> {
        match self {
            Self::Bf16 => None,
            Self::Q80 => Some("Q8_0"),
            Self::Q6K => Some("Q6_K"),
            Self::Q5KM => Some("Q5_K_M"),
            Self::Q4KM => Some("Q4_K_M"),
        }
    }

    /// Whether the component is a single GGUF file rather than a bf16 safetensors folder.
    #[must_use]
    pub fn is_gguf(self) -> bool {
        self.quant_token().is_some()
    }

    /// The formats `variant` offers for `axis`, best quality first. Always starts with
    /// `Bf16`. Only the 4B variant has pinned GGUF files (both axes); the 9B one offers bf16
    /// only.
    #[must_use]
    pub fn offered_for(variant: Flux2Variant, axis: Flux2WeightAxis) -> &'static [Self] {
        match (variant, axis) {
            (Flux2Variant::Klein4B, Flux2WeightAxis::Transformer | Flux2WeightAxis::TextEncoder) => &ALL_FORMATS,
            (Flux2Variant::Klein9B, Flux2WeightAxis::Transformer | Flux2WeightAxis::TextEncoder) => &BF16_ONLY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_ids_round_trip_and_are_frozen() {
        let ids: Vec<&str> = Flux2WeightFormat::all().iter().map(|format| format.wire()).collect();
        assert_eq!(ids, ["bf16", "q8_0", "q6_k", "q5_k_m", "q4_k_m"]);
        for &format in Flux2WeightFormat::all() {
            assert_eq!(Flux2WeightFormat::from_wire(format.wire()), Some(format));
            let id = format.wire();
            assert!(!id.is_empty() && id.len() <= 32);
            assert!(id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'), "{id}");
        }
        assert_eq!(Flux2WeightFormat::from_wire(" q4_k_m "), Some(Flux2WeightFormat::Q4KM));
        assert_eq!(Flux2WeightFormat::from_wire("Q8_0"), None);
        assert_eq!(Flux2WeightFormat::from_wire(""), None);
        assert_eq!(Flux2WeightFormat::from_wire("gguf"), None);
        assert_eq!(Flux2WeightFormat::default(), Flux2WeightFormat::Bf16);
    }

    #[test]
    fn quant_tokens_mark_exactly_the_gguf_formats() {
        assert_eq!(Flux2WeightFormat::Bf16.quant_token(), None);
        assert!(!Flux2WeightFormat::Bf16.is_gguf());
        for &format in &Flux2WeightFormat::all()[1..] {
            assert!(format.is_gguf());
            // The token is the wire id in upper case: one spelling, two casings.
            assert_eq!(format.quant_token().map(str::to_ascii_lowercase).as_deref(), Some(format.wire()));
        }
    }

    #[test]
    fn offered_formats_start_with_bf16_and_only_4b_has_gguf() {
        for variant in Flux2Variant::all() {
            for axis in Flux2WeightAxis::all() {
                let offered = Flux2WeightFormat::offered_for(variant, axis);
                assert_eq!(offered.first(), Some(&Flux2WeightFormat::Bf16));
                assert_eq!(offered.iter().any(|format| format.is_gguf()), variant == Flux2Variant::Klein4B);
            }
        }
        assert_eq!(Flux2WeightFormat::offered_for(Flux2Variant::Klein4B, Flux2WeightAxis::TextEncoder), Flux2WeightFormat::all());
    }
}
