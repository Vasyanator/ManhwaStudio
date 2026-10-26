/*
FILE HEADER (cleaning/tools/ai_editor/engines/flux2_klein/variant_presentation.rs)

Purpose:
The one fact about a `Flux2Variant` that is this ENGINE's business rather than the
configuration layer's: the localized picker caption. Everything else about the enum — its
wire token, its model directory, its settings file name, its engine id, its size token and
`all()` — stays in `ms-config`, because those key runtime PATHS and persisted documents,
which is that crate's subject; which encoder sources and weight formats a variant offers is
`ms_config::flux2_weight_format`, and where its files come from (and which repositories are
gated) is catalog data (`ms_sysprobe::ai_models::external_catalog`).

It lives here as an extension TRAIT because an inherent `impl` may only be written in the
crate that defines the type, and `Flux2Variant` is defined in `ms-config`.

Key items:
- `Flux2VariantPresentation`: the trait, implemented for `Flux2Variant`.

Notes:
The captions are the two picker entries users recognise; the `title` keys are UI strings and
go through `t!`.
*/

use super::*;

/// The presentation half of [`Flux2Variant`] — the caption the picker shows.
///
/// This is a trait rather than one more method on the enum for one reason: `Flux2Variant` is
/// declared in `ms-config`, an inherent `impl` is only legal in the defining crate, and a
/// localized caption is the concern of the ONE crate that consumes it — this one — not of the
/// bottom configuration crate.
pub(super) trait Flux2VariantPresentation {
    /// The localized picker caption of this engine.
    fn title(self) -> &'static str;
}

impl Flux2VariantPresentation for Flux2Variant {
    fn title(self) -> &'static str {
        match self {
            Flux2Variant::Klein9B => t!("cleaning.tools.flux2_klein.title"),
            Flux2Variant::Klein4B => t!("cleaning.tools.flux2_klein.title_4b"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caption is what the picker distinguishes the two entries by, so an empty or
    /// shared one would leave the user unable to tell which engine is selected.
    #[test]
    fn each_variant_has_its_own_non_empty_caption() {
        assert!(!Flux2Variant::Klein9B.title().is_empty());
        assert!(!Flux2Variant::Klein4B.title().is_empty());
        assert_ne!(Flux2Variant::Klein9B.title(), Flux2Variant::Klein4B.title());
    }
}
