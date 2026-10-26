/*
File: crates/ms-config/src/flux2_weight_format.rs

Purpose:
The ONE list of FLUX.2 klein weight formats (bf16 plus the GGUF quants) the «ИИ-редактор
области» may run a component in, and which of them each variant offers per component
(transformer / text encoder) and text-encoder source (stock / uncensored). Both axes share
this single list; no other crate and nothing in the Python backend enumerates the quants (the
backend treats the wire ids as opaque).

Key items:
- `Flux2WeightFormat`: the format, its frozen wire id (`wire` / `from_wire`), its GGUF quant
  token (`quant_token`) and `is_gguf`; `all()` is ordered best quality first.
- `Flux2WeightAxis`: which component a format choice applies to.
- `Flux2EncoderSource`: which text encoder (stock / uncensored) a variant runs, its frozen
  wire id, the sources each variant offers and the dtype token of its unquantized files.
- `Flux2WeightFormat::offered_for`: per variant, axis and encoder source, the formats the
  engine may select; `closest_offered` maps a stored choice onto such a list.

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

/// Which published text encoder a FLUX.2 klein variant runs.
///
/// `Default` is `Stock`. The engine settings persist it as the historic bool
/// `uncensored_text_encoder` (no separate field, no migration). The uncensored encoder is
/// published only for the 9B variant (`ponpoke/flux2-klein-9b-uncensored-text-encoder`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Flux2EncoderSource {
    /// The variant's official Qwen3 encoder from the BFL package.
    #[default]
    Stock,
    /// The community uncensored Qwen3 encoder (9B only).
    Uncensored,
}

/// Sources of a variant that has only its official encoder.
const STOCK_ONLY: [Flux2EncoderSource; 1] = [Flux2EncoderSource::Stock];

/// Sources of a variant that also has the uncensored encoder, stock first.
const STOCK_AND_UNCENSORED: [Flux2EncoderSource; 2] = [Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored];

impl Flux2EncoderSource {
    /// The frozen id this source is spelled with in logs and on the wire. A literal by
    /// design: never localized, never renamed.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Uncensored => "uncensored",
        }
    }

    /// Parses an id written by [`Self::wire`] (surrounding whitespace ignored, otherwise
    /// exact). `None` for an unknown id; the caller decides the fallback (normally `Stock`).
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        let value = value.trim();
        STOCK_AND_UNCENSORED.into_iter().find(|source| source.wire() == value)
    }

    /// The sources `variant` offers, `Stock` first. A variant with a single entry has no
    /// encoder choice, so the UI draws no toggle for it.
    #[must_use]
    pub fn offered_for(variant: Flux2Variant) -> &'static [Self] {
        match variant {
            Flux2Variant::Klein9B => &STOCK_AND_UNCENSORED,
            Flux2Variant::Klein4B => &STOCK_ONLY,
        }
    }

    /// The real dtype of this source's UNQUANTIZED encoder files (`"bf16"` / `"f16"`), shown
    /// next to the `Bf16` format, whose wire id means "unquantized safetensors" for both.
    /// A technical token shown as is in the UI (listed in `dev-docs/i18n_exclusions.md`).
    #[must_use]
    pub fn unquantized_token(self) -> &'static str {
        match self {
            Self::Stock => "bf16",
            Self::Uncensored => "f16",
        }
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

/// The formats of the uncensored 9B encoder: its publisher ships no `Q5_K_M` quant.
const UNCENSORED_9B_FORMATS: [Flux2WeightFormat; 4] = [
    Flux2WeightFormat::Bf16,
    Flux2WeightFormat::Q80,
    Flux2WeightFormat::Q6K,
    Flux2WeightFormat::Q4KM,
];

/// The formats of a component that is not published at all (no format can be selected).
const NONE_OFFERED: [Flux2WeightFormat; 0] = [];

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

    /// The formats `variant` offers for `axis` with the given encoder `source`, best quality
    /// first. `source` only matters for [`Flux2WeightAxis::TextEncoder`]; the transformer
    /// ignores it.
    ///
    /// Every offered list starts with `Bf16`. Both variants offer all five formats for the
    /// transformer and the stock encoder; the 9B uncensored encoder lacks `Q5_K_M`; a source
    /// the variant does not offer ([`Flux2EncoderSource::offered_for`], the 4B uncensored
    /// encoder) yields an EMPTY list, never a fallback to another encoder's files.
    #[must_use]
    pub fn offered_for(variant: Flux2Variant, axis: Flux2WeightAxis, source: Flux2EncoderSource) -> &'static [Self] {
        match (variant, axis, source) {
            (Flux2Variant::Klein4B | Flux2Variant::Klein9B, Flux2WeightAxis::Transformer, _)
            | (Flux2Variant::Klein4B | Flux2Variant::Klein9B, Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock) => &ALL_FORMATS,
            (Flux2Variant::Klein9B, Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored) => &UNCENSORED_9B_FORMATS,
            (Flux2Variant::Klein4B, Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored) => &NONE_OFFERED,
        }
    }

    /// The format of `offered` that best keeps this choice: `self` when it is offered, else
    /// the first offered format after it in [`Self::all`] order (the next smaller quant,
    /// e.g. `Q5_K_M` -> `Q4_K_M`), else `Bf16`. Used when a source switch leaves a stored
    /// encoder format unavailable.
    #[must_use]
    pub fn closest_offered(self, offered: &[Self]) -> Self {
        if offered.contains(&self) {
            return self;
        }
        ALL_FORMATS
            .into_iter()
            .skip_while(|format| *format != self)
            .find(|format| offered.contains(format))
            .unwrap_or(Self::Bf16)
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
    fn every_offered_list_starts_with_bf16() {
        for variant in Flux2Variant::all() {
            for &source in Flux2EncoderSource::offered_for(variant) {
                for axis in Flux2WeightAxis::all() {
                    let offered = Flux2WeightFormat::offered_for(variant, axis, source);
                    assert_eq!(offered.first(), Some(&Flux2WeightFormat::Bf16), "{variant:?} {axis:?} {source:?}");
                }
            }
        }
    }

    #[test]
    fn offered_formats_per_variant_axis_and_source() {
        use Flux2EncoderSource::{Stock, Uncensored};
        use Flux2WeightAxis::{TextEncoder, Transformer};
        use Flux2WeightFormat::{Bf16, Q4KM, Q5KM, Q6K, Q80};
        for variant in Flux2Variant::all() {
            assert_eq!(Flux2WeightFormat::offered_for(variant, Transformer, Stock), Flux2WeightFormat::all());
            // The transformer ignores the encoder source.
            assert_eq!(Flux2WeightFormat::offered_for(variant, Transformer, Uncensored), Flux2WeightFormat::all());
            assert_eq!(Flux2WeightFormat::offered_for(variant, TextEncoder, Stock), Flux2WeightFormat::all());
        }
        let uncensored_9b = Flux2WeightFormat::offered_for(Flux2Variant::Klein9B, TextEncoder, Uncensored);
        assert_eq!(uncensored_9b, [Bf16, Q80, Q6K, Q4KM]);
        assert!(!uncensored_9b.contains(&Q5KM));
        assert!(Flux2WeightFormat::offered_for(Flux2Variant::Klein4B, TextEncoder, Uncensored).is_empty());
    }

    #[test]
    fn encoder_sources_per_variant() {
        assert_eq!(Flux2EncoderSource::offered_for(Flux2Variant::Klein4B), [Flux2EncoderSource::Stock]);
        assert_eq!(Flux2EncoderSource::offered_for(Flux2Variant::Klein9B), [Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored]);
        assert_eq!(Flux2EncoderSource::default(), Flux2EncoderSource::Stock);
        assert_eq!(Flux2EncoderSource::Stock.unquantized_token(), "bf16");
        assert_eq!(Flux2EncoderSource::Uncensored.unquantized_token(), "f16");
    }

    #[test]
    fn encoder_source_wire_ids_are_frozen() {
        assert_eq!(Flux2EncoderSource::Stock.wire(), "stock");
        assert_eq!(Flux2EncoderSource::Uncensored.wire(), "uncensored");
        for source in [Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored] {
            assert_eq!(Flux2EncoderSource::from_wire(source.wire()), Some(source));
        }
        assert_eq!(Flux2EncoderSource::from_wire(" uncensored "), Some(Flux2EncoderSource::Uncensored));
        assert_eq!(Flux2EncoderSource::from_wire("Uncensored"), None);
        assert_eq!(Flux2EncoderSource::from_wire(""), None);
    }

    #[test]
    fn closest_offered_keeps_or_steps_down() {
        use Flux2WeightFormat::{Bf16, Q4KM, Q5KM, Q6K, Q80};
        let uncensored_9b = Flux2WeightFormat::offered_for(Flux2Variant::Klein9B, Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored);
        // An offered value maps to itself.
        for &format in uncensored_9b {
            assert_eq!(format.closest_offered(uncensored_9b), format);
        }
        for &format in Flux2WeightFormat::all() {
            assert_eq!(format.closest_offered(Flux2WeightFormat::all()), format);
        }
        // A missing quant steps to the next smaller offered one.
        assert_eq!(Q5KM.closest_offered(uncensored_9b), Q4KM);
        // Nothing offered after it: falls back to Bf16.
        assert_eq!(Q4KM.closest_offered(&[Bf16, Q80]), Bf16);
        assert_eq!(Q6K.closest_offered(&[]), Bf16);
    }
}
