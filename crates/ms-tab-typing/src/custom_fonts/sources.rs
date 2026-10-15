/*
File: crates/ms-tab-typing/src/custom_fonts/sources.rs

Purpose:
The seam between the typing font model and the custom-font compiler: how a recipe's source
identity becomes bytes + user kerning. The model (font list / provider) implements
`SourceResolver`; the compile service only consumes `ResolvedSource`s.

Key structures:
- `ResolvedSource`: one source font's bytes, face index, in-process content id and EFFECTIVE
  user kerning, ready for `ms_custom_fonts::SourceFont`.
- `SourceResolver`: identity -> `ResolvedSource` (implemented by the font model, package P3).
- `SourceError`: why a source identity could not be resolved.

Key functions:
- `user_kerning_per_mille`: the ONE conversion from the store's `CustomKerningPair` to the
  compiler's `KerningPairPerMille` (mirrors `panel::font_provider::custom_kerning_table`).
- `ResolvedSource::as_source_font`.

Notes:
`content_id` is `ms_text_render::font_content_id` (`DefaultHasher`): stable within one process
only, so it keys in-memory memos and is never written to disk.
*/

use std::sync::Arc;

use ms_custom_fonts::{KerningPairPerMille, SourceFont};

use crate::panel::fonts_data::CustomKerningPair;
use crate::render_next::FontBytes;

/// One source font of a recipe, resolved to bytes by the font model.
#[derive(Clone)]
pub(crate) struct ResolvedSource {
    /// The recipe's identity string for this source (exactly as the recipe spells it).
    pub(crate) identity: String,
    /// The font file bytes (shared buffer; a bundled font's `'static` bytes or a file read).
    pub(crate) data: FontBytes,
    /// Face index inside `data`.
    pub(crate) face_index: usize,
    /// `font_content_id(data)`: in-process memo key, never persisted.
    pub(crate) content_id: u64,
    /// The source's EFFECTIVE user kerning in thousandths of an em (later entries win).
    pub(crate) user_kerning: Arc<[KerningPairPerMille]>,
}

impl std::fmt::Debug for ResolvedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSource")
            .field("identity", &self.identity)
            .field("bytes", &self.bytes().len())
            .field("face_index", &self.face_index)
            .field("content_id", &self.content_id)
            .field("user_kerning", &self.user_kerning.len())
            .finish()
    }
}

impl ResolvedSource {
    /// The font file bytes.
    #[must_use]
    pub(crate) fn bytes(&self) -> &[u8] {
        (*self.data).as_ref()
    }

    /// The compiler's borrowed view of this source.
    ///
    /// # Errors
    /// `SourceError::Unreadable` when the face index does not fit the compiler's `u32`.
    pub(crate) fn as_source_font(&self) -> Result<SourceFont<'_>, SourceError> {
        let face_index = u32::try_from(self.face_index).map_err(|_| SourceError::Unreadable {
            identity: self.identity.clone(),
            reason: format!("face index {} is out of range", self.face_index),
        })?;
        Ok(SourceFont { identity: &self.identity, data: self.bytes(), face_index, user_kerning: &self.user_kerning })
    }
}

/// Why a recipe source identity could not be turned into bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SourceError {
    /// No font with this identity is installed / listed.
    #[error("source font '{0}' is not available")]
    Unknown(String),
    /// The identity names a custom font; custom fonts are never sources (decision D4).
    #[error("source font '{0}' is a custom font, which cannot be a hybrid source")]
    CustomNotAllowed(String),
    /// The font exists but its bytes could not be read.
    #[error("source font '{identity}' could not be read: {reason}")]
    Unreadable {
        /// Identity of the source.
        identity: String,
        /// OS / parser reason.
        reason: String,
    },
}

/// Resolves recipe source identities to bytes. Implemented by the typing font model; called
/// on worker threads only (it may read files).
pub(crate) trait SourceResolver: Send + Sync {
    /// Resolves one source identity.
    ///
    /// # Errors
    /// `SourceError` when the identity is unknown, names a custom font, or cannot be read.
    fn resolve_source(&self, identity: &str) -> Result<ResolvedSource, SourceError>;
}

/// Converts the store's user kerning pairs of one font into the compiler's units (both are
/// thousandths of an em). Order is kept, so later duplicates still win; `0.0` pairs are kept
/// (they cancel a built-in pair).
#[must_use]
pub(crate) fn user_kerning_per_mille(pairs: &[CustomKerningPair]) -> Arc<[KerningPairPerMille]> {
    pairs
        .iter()
        .map(|pair| KerningPairPerMille { left: pair.left, right: pair.right, em: pair.offset_per_mille })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kerning_conversion_keeps_order_units_and_zero_pairs() {
        let pairs = [
            CustomKerningPair { left: 'A', right: 'V', offset_per_mille: -40.0 },
            CustomKerningPair { left: 'T', right: 'o', offset_per_mille: 0.0 },
        ];
        let converted = user_kerning_per_mille(&pairs);
        assert_eq!(
            &*converted,
            &[
                KerningPairPerMille { left: 'A', right: 'V', em: -40.0 },
                KerningPairPerMille { left: 'T', right: 'o', em: 0.0 },
            ]
        );
    }

    #[test]
    fn source_view_borrows_bytes_and_identity() {
        let data: FontBytes = Arc::new(vec![1_u8, 2, 3]);
        let source = ResolvedSource {
            identity: "Font-A".to_owned(),
            data,
            face_index: 2,
            content_id: 7,
            user_kerning: Arc::from(Vec::new()),
        };
        let view = source.as_source_font().expect("face index 2 fits u32");
        assert_eq!(view.identity, "Font-A");
        assert_eq!(view.data, &[1, 2, 3]);
        assert_eq!(view.face_index, 2);
        assert!(format!("{source:?}").contains("Font-A"));
    }

    #[test]
    fn errors_name_the_source() {
        for err in [
            SourceError::Unknown("Font-A".to_owned()),
            SourceError::CustomNotAllowed("Font-A".to_owned()),
            SourceError::Unreadable { identity: "Font-A".to_owned(), reason: "gone".to_owned() },
        ] {
            assert!(err.to_string().contains("'Font-A'"), "{err}");
        }
    }
}
