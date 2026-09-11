/*
FILE HEADER (cleaning/tools/ai_editor/engines/flux2_klein/variant_presentation.rs)

Purpose:
The two facts about a `Flux2Variant` that are this ENGINE's business rather than the
configuration layer's: the localized picker caption and the Hugging Face repository the
model is downloaded from. Everything else about the enum — its wire token, its model
directory, its settings file name, its engine id, `all()` and the two capability
predicates — stays in `ms-config`, because those key runtime PATHS and persisted
documents, which is that crate's subject.

They live here as an extension TRAIT because an inherent `impl` may only be written in
the crate that defines the type, and `Flux2Variant` is defined in `ms-config`.

Key items:
- `Flux2VariantPresentation`: the trait, implemented for `Flux2Variant`.

Notes:
Both values are FROZEN in the same sense the identities in `ms-config` are: the captions
are the two picker entries users recognise, and a repository id that drifts turns every
download into a 404. The `title` keys are UI strings and go through `t!`; the repository
ids are backend identifiers and stay literal (`dev-docs/i18n_exclusions.md`).
*/

use super::*;

/// The presentation half of [`Flux2Variant`] — the caption the picker shows and the
/// repository the download names.
///
/// This is a trait rather than more methods on the enum for one reason: `Flux2Variant` is
/// declared in `ms-config`, an inherent `impl` is only legal in the defining crate, and a
/// localized caption plus a Hugging Face repository id are the concern of the ONE crate
/// that consumes them — this one — not of the bottom configuration crate.
pub(super) trait Flux2VariantPresentation {
    /// The localized picker caption of this engine.
    fn title(self) -> &'static str;

    /// The Hugging Face repository the model itself is downloaded from.
    ///
    /// A repository ID is a backend identifier and stays literal in the UI, exactly like
    /// the ids the `.download.check` rows carry.
    fn model_repo(self) -> &'static str;
}

impl Flux2VariantPresentation for Flux2Variant {
    fn title(self) -> &'static str {
        match self {
            Flux2Variant::Klein9B => t!("cleaning.tools.flux2_klein.title"),
            Flux2Variant::Klein4B => t!("cleaning.tools.flux2_klein.title_4b"),
        }
    }

    fn model_repo(self) -> &'static str {
        match self {
            Flux2Variant::Klein9B => "black-forest-labs/FLUX.2-klein-9B",
            Flux2Variant::Klein4B => "black-forest-labs/FLUX.2-klein-4B",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two engines are downloaded from DIFFERENT repositories, and the 9B id is what
    /// every existing installation already fetched from. The distinctness assertion moved
    /// here together with `model_repo` itself; the `ms-config` test keeps the identities
    /// that crate still owns (wire token, engine id, settings file, model directory).
    #[test]
    fn the_two_variants_name_distinct_frozen_repositories() {
        assert_eq!(Flux2Variant::Klein9B.model_repo(), "black-forest-labs/FLUX.2-klein-9B");
        assert_eq!(Flux2Variant::Klein4B.model_repo(), "black-forest-labs/FLUX.2-klein-4B");
        assert_ne!(Flux2Variant::Klein9B.model_repo(), Flux2Variant::Klein4B.model_repo());
    }

    /// A caption is what the picker distinguishes the two entries by, so an empty or
    /// shared one would leave the user unable to tell which engine is selected.
    #[test]
    fn each_variant_has_its_own_non_empty_caption() {
        assert!(!Flux2Variant::Klein9B.title().is_empty());
        assert!(!Flux2Variant::Klein4B.title().is_empty());
        assert_ne!(Flux2Variant::Klein9B.title(), Flux2Variant::Klein4B.title());
    }
}
