/*
File: crates/ms-tab-typing/src/custom_fonts/identity.rs

Purpose:
The ONE owner of the custom-font identity namespace. Every name a custom font can carry in
this crate is minted or recognized here, so no other file spells the `MSCustom` prefixes.

Name shapes (three, pairwise disjoint):
- saved identity: `MSCustom-<16 lowercase hex>` (random, from a v4 UUID; stable across recipe
  edits and renames, so layers follow an edited font) — `new_saved_identity`;
- persisted ad-hoc identity: `MSCustom-adhoc-hybrid` (`ADHOC_CUSTOM_FONT_IDENTITY`; the recipe
  lives in the layer's `text_params.font_recipe`);
- ad-hoc RENDER name: `MSCustom-adhoc-<16 hex>` (a pure function of the recipe, minted by
  `custom_fonts::adhoc`), never persisted;
- plus the compiled faces of `ms-custom-fonts`: `MSCustom-<8 hex>` (full face) and
  `MSCustomPreview-<8 hex>-<8 hex>` (preview subset). Lengths and the `adhoc-` marker keep all
  of them apart.

Key functions:
- `new_saved_identity`, `is_reserved_custom_identity`, `is_saved_identity`,
  `is_minted_saved_identity`, `is_adhoc_identity`, `is_adhoc_render_name`, `compiled_face_post_script_name`.

Notes:
Recognition is case-insensitive and whitespace-trimmed, exactly like
`panel::fonts::normalize_font_identity`, because a user font's identity is compared under that
rule everywhere else.
*/

use crate::panel::fonts::normalize_font_identity;

/// Prefix of every custom-font identity and of every compiled full face's PostScript name.
pub(crate) const CUSTOM_IDENTITY_PREFIX: &str = "MSCustom-";
/// The identity a text layer stores when it uses its own (ad-hoc) hybrid recipe.
pub(crate) const ADHOC_CUSTOM_FONT_IDENTITY: &str = "MSCustom-adhoc-hybrid";
/// Prefix of the per-recipe render name of an ad-hoc hybrid (`MSCustom-adhoc-<16 hex>`).
pub(crate) const ADHOC_RENDER_NAME_PREFIX: &str = "MSCustom-adhoc-";
/// Prefix of the preview (subset) faces `ms-custom-fonts` compiles.
const PREVIEW_FACE_PREFIX: &str = "MSCustomPreview-";
/// Number of hex digits after the prefix of a saved identity and of an ad-hoc render name.
pub(crate) const IDENTITY_HEX_LEN: usize = 16;

/// Mints a fresh saved custom-font identity: `MSCustom-<16 lowercase hex>`.
///
/// Random (v4 UUID bytes), so it never depends on the user-visible name (a slug of a Cyrillic
/// name would be lossy) and never collides with the `adhoc-` shapes (hex only).
#[must_use]
pub(crate) fn new_saved_identity() -> String {
    let uuid = uuid::Uuid::new_v4();
    let mut out = String::with_capacity(CUSTOM_IDENTITY_PREFIX.len() + IDENTITY_HEX_LEN);
    out.push_str(CUSTOM_IDENTITY_PREFIX);
    out.push_str(&hex_lower(&uuid.as_bytes()[..IDENTITY_HEX_LEN / 2]));
    out
}

/// `true` when `name` falls in the reserved custom-font namespace (`MSCustom-…` or
/// `MSCustomPreview-…`, case-insensitive, trimmed).
///
/// A USER font whose identity or alias matches must never be registered under that name: the
/// font list suffixes it, as it does for the bundled interface family.
#[must_use]
pub(crate) fn is_reserved_custom_identity(name: &str) -> bool {
    let normalized = normalize_font_identity(name);
    normalized.starts_with(&CUSTOM_IDENTITY_PREFIX.to_ascii_lowercase())
        || normalized.starts_with(&PREVIEW_FACE_PREFIX.to_ascii_lowercase())
}

/// `true` when `name` has the exact shape of a saved identity (`MSCustom-<16 hex>`).
#[must_use]
pub(crate) fn is_saved_identity(name: &str) -> bool {
    let normalized = normalize_font_identity(name);
    normalized
        .strip_prefix(&CUSTOM_IDENTITY_PREFIX.to_ascii_lowercase())
        .is_some_and(is_hex_digest)
}

/// `true` when `name` is EXACTLY the spelling `new_saved_identity` mints
/// (`MSCustom-<16 lowercase hex>`, no padding, prefix case as minted). Unlike
/// `is_saved_identity` it does not normalize: it tells the app's own spelling apart from a
/// hand-edited variant of the same identity (`fonts_data` key-collision tie-break).
#[must_use]
pub(crate) fn is_minted_saved_identity(name: &str) -> bool {
    name.strip_prefix(CUSTOM_IDENTITY_PREFIX).is_some_and(is_hex_digest)
}

/// `true` when `name` is the persisted ad-hoc identity (`MSCustom-adhoc-hybrid`), compared under
/// the identity normalization. The ONE test for "this layer / preset / snapshot uses its own
/// hybrid recipe"; an ad-hoc RENDER name is not it (`is_adhoc_render_name`).
#[must_use]
pub(crate) fn is_adhoc_identity(name: &str) -> bool {
    normalize_font_identity(name) == normalize_font_identity(ADHOC_CUSTOM_FONT_IDENTITY)
}

/// `true` when `name` has the exact shape of an ad-hoc render name (`MSCustom-adhoc-<16 hex>`).
///
/// The persisted ad-hoc identity (`MSCustom-adhoc-hybrid`) is NOT a render name.
#[must_use]
pub(crate) fn is_adhoc_render_name(name: &str) -> bool {
    let normalized = normalize_font_identity(name);
    normalized
        .strip_prefix(&ADHOC_RENDER_NAME_PREFIX.to_ascii_lowercase())
        .is_some_and(is_hex_digest)
}

/// PostScript name of the compiled FULL face of `key`: `MSCustom-<key.short8()>`.
///
/// Delegates to the one owner of the rule, `RecipeKey::full_face_post_script_name`; the disk
/// cache validates a cached face against it.
#[must_use]
pub(crate) fn compiled_face_post_script_name(key: &ms_custom_fonts::RecipeKey) -> String {
    key.full_face_post_script_name()
}

/// `true` for exactly `IDENTITY_HEX_LEN` lowercase hex digits.
fn is_hex_digest(text: &str) -> bool {
    text.len() == IDENTITY_HEX_LEN && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_identity_has_its_shape_and_is_random() {
        let first = new_saved_identity();
        let second = new_saved_identity();
        assert_ne!(first, second);
        assert!(first.starts_with(CUSTOM_IDENTITY_PREFIX));
        assert_eq!(first.len(), CUSTOM_IDENTITY_PREFIX.len() + IDENTITY_HEX_LEN);
        assert!(is_saved_identity(&first));
        assert!(is_reserved_custom_identity(&first));
        assert!(!is_adhoc_render_name(&first));
    }

    #[test]
    fn identity_shapes_are_disjoint() {
        let saved = new_saved_identity();
        let render = format!("{ADHOC_RENDER_NAME_PREFIX}0123456789abcdef");
        let compiled_face = compiled_face_post_script_name(&ms_custom_fonts::RecipeKey([0x5a; 32]));
        assert_eq!(compiled_face, "MSCustom-5a5a5a5a");
        let compiled_face = compiled_face.as_str();
        let preview_face = "MSCustomPreview-0123abcd-89abcdef";

        assert!(is_saved_identity(&saved) && !is_adhoc_render_name(&saved));
        assert!(is_adhoc_render_name(&render) && !is_saved_identity(&render));
        for other in [ADHOC_CUSTOM_FONT_IDENTITY, compiled_face, preview_face] {
            assert!(!is_saved_identity(other), "{other} must not look saved");
            assert!(!is_adhoc_render_name(other), "{other} must not look like a render name");
        }
        for reserved in [saved.as_str(), render.as_str(), ADHOC_CUSTOM_FONT_IDENTITY, compiled_face, preview_face] {
            assert!(is_reserved_custom_identity(reserved), "{reserved} must be reserved");
        }
    }

    #[test]
    fn recognition_follows_the_identity_normalization() {
        assert!(is_reserved_custom_identity("  mscustom-Whatever "));
        assert!(is_saved_identity(" MSCUSTOM-0123456789ABCDEF"));
        assert!(!is_reserved_custom_identity("MSCustomSans-Regular"));
        assert!(!is_reserved_custom_identity("NotoSans-Regular"));
        assert!(!is_saved_identity("MSCustom-0123456789abcde"));
        assert!(!is_saved_identity("MSCustom-0123456789abcdeg"));
        assert!(is_minted_saved_identity(&new_saved_identity()));
        assert!(!is_minted_saved_identity("MSCUSTOM-0123456789abcdef"), "prefix case differs");
        assert!(!is_minted_saved_identity("MSCustom-0123456789ABCDEF"), "uppercase hex");
        assert!(!is_minted_saved_identity(" MSCustom-0123456789abcdef"), "padded");
        assert!(is_adhoc_identity(ADHOC_CUSTOM_FONT_IDENTITY));
        assert!(is_adhoc_identity("  mscustom-ADHOC-hybrid "));
        assert!(!is_adhoc_identity("MSCustom-adhoc-0123456789abcdef"));
        assert!(!is_adhoc_identity(&new_saved_identity()));
        assert!(!is_adhoc_identity(""));
    }
}
