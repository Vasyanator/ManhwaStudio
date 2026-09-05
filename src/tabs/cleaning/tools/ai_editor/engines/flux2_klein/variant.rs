/*
File: cleaning/tools/ai_editor/engines/flux2_klein/variant.rs

Purpose:
The MODEL and UI facts of a FLUX.2 klein checkpoint: which engine id and picker caption
it uses, which Hugging Face repository it comes from, whether an uncensored text encoder
exists for it, and whether downloading it needs an access token.

Main responsibilities:
- carry everything that differs between the 9B and the 4B engine and is NOT a path;
- keep those facts in ONE place, so a new difference is added here and nowhere else.

Key structures:
- `Flux2Variant` (declared in `crate::config`; this file adds a second inherent `impl`).

Notes:
The enum itself lives in `config.rs` because it keys five path functions there and
runtime path decisions belong in that file (`src/MODULE_README.md`). Inherent impls may
be written anywhere in the DEFINING CRATE, so this block adds the engine-side half
without a second enum and without a mapping table that could drift from it. Everything
here is `pub(super)`, i.e. confined to `engines::flux2_klein`.
*/

use super::*;

impl Flux2Variant {
    /// The engine's `AiEngine::id` — a widget-id stem and a log token, never shown to the
    /// user.
    ///
    /// The 9B id is FROZEN at its historic value: it is what the host's picker and every
    /// widget id derived from it already use.
    pub(super) fn engine_id(self) -> &'static str {
        match self {
            Self::Klein9B => "flux2_klein",
            Self::Klein4B => "flux2_klein_4b",
        }
    }

    /// The localized picker caption of this engine.
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Klein9B => t!("cleaning.tools.flux2_klein.title"),
            Self::Klein4B => t!("cleaning.tools.flux2_klein.title_4b"),
        }
    }

    /// The Hugging Face repository the model itself is downloaded from.
    ///
    /// A repository ID is a backend identifier and stays literal in the UI, exactly like
    /// the ids the `.download.check` rows carry.
    pub(super) fn model_repo(self) -> &'static str {
        match self {
            Self::Klein9B => "black-forest-labs/FLUX.2-klein-9B",
            Self::Klein4B => "black-forest-labs/FLUX.2-klein-4B",
        }
    }

    /// Whether an UNCENSORED text encoder is published for this variant.
    ///
    /// Only the 9B one has it (`ponpoke/flux2-klein-9b-uncensored-text-encoder`). For the
    /// 4B model the toggle is not drawn at all and the flag is forced off before it can
    /// reach a path or the wire ([`Flux2KleinSettings::normalized`]) — the backend refuses
    /// `variant = "4b"` together with `uncensored = true`, and offering a control that
    /// produces a refusal is worse than not offering it.
    pub(super) fn supports_uncensored_encoder(self) -> bool {
        match self {
            Self::Klein9B => true,
            Self::Klein4B => false,
        }
    }

    /// Whether downloading this variant needs a Hugging Face access token.
    ///
    /// The 9B repository is GATED (licence `other`), so without an accepted token the
    /// download cannot even be priced. The 4B one is apache-2.0 and ungated: the token
    /// block is not drawn for it, and the empty token the request still carries is legal.
    pub(super) fn requires_hf_token(self) -> bool {
        match self {
            Self::Klein9B => true,
            Self::Klein4B => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two engines share every widget-id stem and every settings path unless these
    /// four identities really differ, and the 9B half of each is frozen: an installation
    /// that already holds `flux2_klein_settings.json` and a downloaded
    /// `side_models/FLUX.2-klein-9B` must keep reading exactly those.
    #[test]
    fn the_two_variants_are_distinct_and_the_9b_identity_is_frozen() {
        assert_eq!(Flux2Variant::Klein9B.wire(), "9b");
        assert_eq!(Flux2Variant::Klein4B.wire(), "4b");
        assert_eq!(Flux2Variant::Klein9B.engine_id(), "flux2_klein");
        assert_eq!(
            Flux2Variant::Klein9B.settings_file_name(),
            "flux2_klein_settings.json",
            "the historic settings file name is what existing installations hold"
        );
        assert_eq!(Flux2Variant::Klein9B.dir_name(), "FLUX.2-klein-9B");
        assert_eq!(Flux2Variant::Klein4B.dir_name(), "FLUX.2-klein-4B");
        let (a, b) = (Flux2Variant::Klein9B, Flux2Variant::Klein4B);
        assert_ne!(a.wire(), b.wire());
        assert_ne!(a.engine_id(), b.engine_id());
        assert_ne!(a.settings_file_name(), b.settings_file_name());
        assert_ne!(a.dir_name(), b.dir_name());
        assert_ne!(a.model_repo(), b.model_repo());
    }

    /// An absent variant field is what every settings file written before the 4B engine
    /// existed looks like, and those files describe the 9B model.
    #[test]
    fn an_unknown_variant_token_reads_as_9b() {
        assert_eq!(Flux2Variant::from_wire(""), Flux2Variant::Klein9B);
        assert_eq!(Flux2Variant::from_wire("   "), Flux2Variant::Klein9B);
        assert_eq!(Flux2Variant::from_wire("klein-42b"), Flux2Variant::Klein9B);
        assert_eq!(Flux2Variant::from_wire(" 4b "), Flux2Variant::Klein4B);
    }

    /// The uncensored encoder and the access token are the two capabilities the panel
    /// gates a control on, so a wrong answer here draws a control that cannot work.
    #[test]
    fn only_the_9b_variant_has_an_uncensored_encoder_and_a_gated_repository() {
        assert!(Flux2Variant::Klein9B.supports_uncensored_encoder());
        assert!(!Flux2Variant::Klein4B.supports_uncensored_encoder());
        assert!(Flux2Variant::Klein9B.requires_hf_token());
        assert!(!Flux2Variant::Klein4B.requires_hf_token());
    }

    /// Each variant's three model paths must live under ITS OWN directory: a shared one
    /// would make the two engines overwrite each other's download.
    #[test]
    fn the_derived_paths_are_variant_specific() {
        for variant in Flux2Variant::all() {
            let root = config::flux2_klein_dir(variant);
            assert!(root.ends_with(variant.dir_name()));
            assert!(config::flux2_klein_transformer_dir(variant).starts_with(&root));
            assert!(config::flux2_klein_vae_dir(variant).starts_with(&root));
            assert!(config::flux2_klein_text_encoder_dir(variant, false).starts_with(&root));
        }
        assert_ne!(
            config::flux2_klein_transformer_dir(Flux2Variant::Klein9B),
            config::flux2_klein_transformer_dir(Flux2Variant::Klein4B),
        );
        assert_ne!(
            config::flux2_klein_settings_path(Flux2Variant::Klein9B),
            config::flux2_klein_settings_path(Flux2Variant::Klein4B),
        );
    }
}
