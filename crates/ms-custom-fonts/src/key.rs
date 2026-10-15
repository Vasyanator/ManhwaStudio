/*
File: crates/ms-custom-fonts/src/key.rs

Purpose:
Stable identities of compile inputs: the per-source content digest and the recipe key that
names a compiled face (disk-cache key, face-name suffix).

Key structures:
- `SourceDigest`: SHA-256 of one source face (bytes + face index).
- `RecipeKey`: SHA-256 of everything that determines a compiled face's bytes.

Key functions:
- `source_digest`, `recipe_key`, `RecipeKey::hex` / `short8`.
- Compiled-face naming (one owner): `RecipeKey::full_face_family_name` /
  `full_face_post_script_name` / `full_face_unique_id`, `face_unique_id` (crate-internal),
  and `RecipeKey::verify_full_face` (`VerifiedFullFace` / `FaceKeyMismatch`): the rule a
  disk cache uses to trust a stored face for a key (PostScript name AND the full-key unique id).
- `subset_tag` (crate-internal): the hash suffix that makes a preview face's name distinct.

Notes:
Everything here is SHA-256 over a fixed BINARY canonical encoding (big-endian integers,
length-prefixed UTF-8, `f32` bit patterns with `-0.0` folded into `0.0`), never over
`DefaultHasher` or `Debug` output, so a key is identical on every machine and Rust release.
`ENGINE_VERSION` is the first field: bumping it invalidates every cached face.
*/

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::ENGINE_VERSION;
use crate::kerning::KerningPairPerMille;
use crate::recipe::{CustomFontKind, CustomFontRecipe, MissingGlyphPolicy};

/// Domain separator of `source_digest`.
const SOURCE_DOMAIN: &[u8] = b"ms-custom-fonts/source/v1\0";
/// Domain separator of `recipe_key`.
const RECIPE_DOMAIN: &[u8] = b"ms-custom-fonts/recipe\0";
/// Domain separator of `subset_tag`.
const SUBSET_DOMAIN: &[u8] = b"ms-custom-fonts/subset\0";

/// SHA-256 content digest of one source face (its file bytes plus the face index inside them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceDigest(pub [u8; 32]);

/// SHA-256 key of a compiled custom face; equal keys mean byte-identical compile inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecipeKey(pub [u8; 32]);

impl RecipeKey {
    /// Lowercase hexadecimal form (64 characters) — the disk-cache file stem.
    #[must_use]
    pub fn hex(&self) -> String {
        hex_lower(&self.0)
    }

    /// First 8 hex characters — the suffix of the compiled face's family and PostScript names.
    #[must_use]
    pub fn short8(&self) -> String {
        hex_lower(&self.0[..4])
    }

    /// Family name of the compiled FULL face of this key: `MS Custom <short8>`. The one owner
    /// of the full-face naming rule (callers validate cached faces against it).
    #[must_use]
    pub fn full_face_family_name(&self) -> String {
        format!("MS Custom {}", self.short8())
    }

    /// PostScript name of the compiled FULL face of this key: `MSCustom-<short8>`.
    #[must_use]
    pub fn full_face_post_script_name(&self) -> String {
        format!("MSCustom-{}", self.short8())
    }

    /// Unique font identifier (`name` ID 3) of the compiled FULL face of this key: it carries
    /// the WHOLE key, which the 8-hex-digit names do not.
    #[must_use]
    pub fn full_face_unique_id(&self) -> String {
        face_unique_id(&self.full_face_post_script_name(), self)
    }

    /// Checks that `face` is the compiled FULL face of exactly this key: it parses, its
    /// PostScript name (`name` ID 6) is `full_face_post_script_name` and its unique identifier
    /// (`name` ID 3) is `full_face_unique_id`, so a face of another key that merely shares the
    /// 32-bit name prefix is rejected. The one owner of this rule (disk caches validate with
    /// it). A face rewritten by `with_kerning_overrides` keeps its names and still passes.
    ///
    /// # Errors
    /// `FaceKeyMismatch` naming the first check that failed.
    pub fn verify_full_face(&self, face: &[u8]) -> Result<VerifiedFullFace, FaceKeyMismatch> {
        let parsed = ttf_parser::Face::parse(face, 0).map_err(|err| FaceKeyMismatch::Unparsable { reason: err.to_string() })?;
        let name_of = |id: u16| parsed.names().into_iter().filter(|name| name.name_id == id).find_map(|name| name.to_string());
        let expected_post_script = self.full_face_post_script_name();
        match name_of(ttf_parser::name_id::POST_SCRIPT_NAME) {
            Some(found) if found == expected_post_script => {}
            Some(found) => return Err(FaceKeyMismatch::PostScriptName { found, expected: expected_post_script }),
            None => return Err(FaceKeyMismatch::MissingName { name_id: ttf_parser::name_id::POST_SCRIPT_NAME }),
        }
        let expected_unique = self.full_face_unique_id();
        match name_of(ttf_parser::name_id::UNIQUE_ID) {
            Some(found) if found == expected_unique => {}
            Some(found) => return Err(FaceKeyMismatch::UniqueId { found, expected: expected_unique }),
            None => return Err(FaceKeyMismatch::MissingName { name_id: ttf_parser::name_id::UNIQUE_ID }),
        }
        Ok(VerifiedFullFace { family_name: name_of(ttf_parser::name_id::FAMILY).unwrap_or_default() })
    }
}

/// What `RecipeKey::verify_full_face` learned about a face that passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFullFace {
    /// The face's family name (`name` ID 1; empty when it has none readable).
    pub family_name: String,
}

/// Why `RecipeKey::verify_full_face` rejected a face. `Display` is log text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FaceKeyMismatch {
    /// The bytes are not a face `ttf-parser` reads.
    #[error("not a font: {reason}")]
    Unparsable {
        /// Parser diagnostic.
        reason: String,
    },
    /// A required `name` record is absent or unreadable.
    #[error("the face has no readable name record {name_id}")]
    MissingName {
        /// The `name` ID (3 = unique identifier, 6 = PostScript name).
        name_id: u16,
    },
    /// The PostScript name belongs to another key (or is not a compiled face's name at all).
    #[error("PostScript name '{found}' does not match the expected '{expected}'")]
    PostScriptName {
        /// Name in the face.
        found: String,
        /// Name of this key's full face.
        expected: String,
    },
    /// The PostScript name matches but the unique identifier names another key (a face of a
    /// different recipe whose key shares the first 32 bits) or another engine build.
    #[error("unique identifier '{found}' does not match the expected '{expected}'")]
    UniqueId {
        /// Identifier in the face.
        found: String,
        /// Identifier of this key's full face.
        expected: String,
    },
}

/// `name` ID 3 of a compiled face named `post_script_name` (full or preview) for `key`:
/// `<PostScript name>;<key hex>;ms-custom-fonts engine <ENGINE_VERSION>`. Changing the format
/// changes output bytes (bump `ENGINE_VERSION`).
pub(crate) fn face_unique_id(post_script_name: &str, key: &RecipeKey) -> String {
    format!("{post_script_name};{};ms-custom-fonts engine {ENGINE_VERSION}", key.hex())
}

/// Digest of a source face: SHA-256 over a domain tag, `face_index` and the whole buffer.
#[must_use]
pub fn source_digest(data: &[u8], face_index: u32) -> SourceDigest {
    let mut hasher = Sha256::new();
    hasher.update(SOURCE_DOMAIN);
    hasher.update(face_index.to_be_bytes());
    hasher.update(data);
    SourceDigest(hasher.finalize().into())
}

/// Key of a compiled face.
///
/// Hashes `ENGINE_VERSION`, the recipe in canonical binary form (kind, default policy, every
/// source's identity, weight bits and override, in recipe order), then, per entry of
/// `sources` (expected in recipe order), the source digest and that source's EFFECTIVE user
/// kerning (later entries of the slice override earlier ones for the same pair; hashed sorted
/// by `(left, right)`). The hybrid's OWN user kerning is not part of the binary and therefore
/// not of the key.
#[must_use]
pub fn recipe_key(recipe: &CustomFontRecipe, sources: &[(SourceDigest, &[KerningPairPerMille])]) -> RecipeKey {
    let mut hasher = Sha256::new();
    hasher.update(RECIPE_DOMAIN);
    hasher.update(ENGINE_VERSION.to_be_bytes());
    hash_str(&mut hasher, recipe.kind.tag());
    match &recipe.kind {
        CustomFontKind::Hybrid(params) => {
            hasher.update([policy_byte(Some(params.missing_default))]);
            hash_len(&mut hasher, params.sources.len());
            for source in &params.sources {
                hash_str(&mut hasher, &source.identity);
                hasher.update(canonical_f32_bits(source.weight).to_be_bytes());
                hasher.update([policy_byte(source.missing_override)]);
            }
        }
    }
    hash_len(&mut hasher, sources.len());
    for (digest, user_kerning) in sources {
        hasher.update(digest.0);
        let effective = effective_user_pairs(user_kerning);
        hash_len(&mut hasher, effective.len());
        for ((left, right), em) in effective {
            hasher.update(u32::from(left).to_be_bytes());
            hasher.update(u32::from(right).to_be_bytes());
            hasher.update(canonical_f32_bits(em).to_be_bytes());
        }
    }
    RecipeKey(hasher.finalize().into())
}

/// 8-hex-character tag of a preview character set (sorted, deduplicated by the `BTreeSet`),
/// used to give each preview face a name of its own.
pub(crate) fn subset_tag(chars: &std::collections::BTreeSet<char>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SUBSET_DOMAIN);
    hash_len(&mut hasher, chars.len());
    for ch in chars {
        hasher.update(u32::from(*ch).to_be_bytes());
    }
    let digest: [u8; 32] = hasher.finalize().into();
    hex_lower(&digest[..4])
}

/// The user pairs of one source as a map, later slice entries overriding earlier ones.
pub(crate) fn effective_user_pairs(pairs: &[KerningPairPerMille]) -> BTreeMap<(char, char), f32> {
    pairs.iter().map(|pair| ((pair.left, pair.right), pair.em)).collect()
}

/// `f32` bits with `-0.0` folded into `0.0` (equal values hash equally).
fn canonical_f32_bits(value: f32) -> u32 {
    if value == 0.0 { 0 } else { value.to_bits() }
}

/// One byte per policy option: 0 = follow default, 1 = disable, 2 = no influence.
fn policy_byte(policy: Option<MissingGlyphPolicy>) -> u8 {
    match policy {
        None => 0,
        Some(MissingGlyphPolicy::Disable) => 1,
        Some(MissingGlyphPolicy::NoInfluence) => 2,
    }
}

/// Hashes a length as a big-endian `u64` (lossless on every platform).
fn hash_len(hasher: &mut Sha256, len: usize) {
    // `usize` is at most 64 bits on every supported target; saturate instead of panicking.
    hasher.update(u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes());
}

/// Hashes a length-prefixed UTF-8 string.
fn hash_str(hasher: &mut Sha256, text: &str) {
    hash_len(hasher, text.len());
    hasher.update(text.as_bytes());
}

/// Lowercase hex of `bytes`.
fn hex_lower(bytes: &[u8]) -> String {
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
    use crate::recipe::{HybridParams, HybridSource};

    fn recipe(weight_b: f32) -> CustomFontRecipe {
        CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: vec![
                    HybridSource { identity: "A".to_owned(), weight: 1.0, missing_override: None },
                    HybridSource { identity: "B".to_owned(), weight: weight_b, missing_override: None },
                ],
                missing_default: MissingGlyphPolicy::NoInfluence,
            }),
        }
    }

    #[test]
    fn key_is_stable_and_sensitive_to_every_input() {
        let digest_a = source_digest(b"font a", 0);
        let digest_b = source_digest(b"font b", 0);
        let none: &[KerningPairPerMille] = &[];
        let base = recipe_key(&recipe(1.0), &[(digest_a, none), (digest_b, none)]);
        assert_eq!(base, recipe_key(&recipe(1.0), &[(digest_a, none), (digest_b, none)]));
        assert_ne!(base, recipe_key(&recipe(2.0), &[(digest_a, none), (digest_b, none)]));
        assert_ne!(base, recipe_key(&recipe(1.0), &[(digest_b, none), (digest_a, none)]));
        assert_ne!(source_digest(b"font a", 0), source_digest(b"font a", 1));
        let pairs = [KerningPairPerMille { left: 'A', right: 'V', em: -40.0 }];
        assert_ne!(base, recipe_key(&recipe(1.0), &[(digest_a, &pairs), (digest_b, none)]));
        // -0.0 and 0.0 are the same value and the same key.
        let zero = [KerningPairPerMille { left: 'A', right: 'V', em: 0.0 }];
        let negative_zero = [KerningPairPerMille { left: 'A', right: 'V', em: -0.0 }];
        assert_eq!(
            recipe_key(&recipe(1.0), &[(digest_a, &zero), (digest_b, none)]),
            recipe_key(&recipe(1.0), &[(digest_a, &negative_zero), (digest_b, none)])
        );
        assert_eq!(base.hex().len(), 64);
        assert_eq!(base.short8(), base.hex()[..8]);
    }

    /// A minimal face written by the crate's own writer under the given names.
    fn named_face(post_script: &str, unique_id: &str) -> Vec<u8> {
        use crate::outline::TtPoint;
        use crate::sfnt::{FontBuild, FontMetrics, FontNames, GlyphData, write_font};
        let point = |x, y| TtPoint { x, y, on: true };
        let glyph = || GlyphData { advance: 500, contours: vec![vec![point(50, 0), point(50, 700), point(450, 700), point(450, 0)]] };
        write_font(&FontBuild {
            glyphs: vec![glyph(), glyph(), glyph()],
            cmap: vec![('A', 1), ('V', 2)],
            kern_pairs: vec![(1, 2, -80)],
            metrics: FontMetrics { ascender: 800, descender: -200, weight_class: 400, ..FontMetrics::default() },
            names: FontNames {
                family: "MS Custom test".to_owned(),
                subfamily: "Regular".to_owned(),
                unique_id: unique_id.to_owned(),
                full_name: "MS Custom test".to_owned(),
                version: "Version 1.000".to_owned(),
                postscript: post_script.to_owned(),
            },
        })
        .expect("writes")
    }

    #[test]
    fn full_face_verification_checks_the_whole_key() {
        let key = RecipeKey([0x5a; 32]);
        let face = named_face(&key.full_face_post_script_name(), &key.full_face_unique_id());
        assert_eq!(key.verify_full_face(&face), Ok(VerifiedFullFace { family_name: "MS Custom test".to_owned() }));
        // Own kerning baked for export keeps the names, so the rewritten face still verifies.
        let baked = crate::with_kerning_overrides(&face, &[KerningPairPerMille { left: 'A', right: 'V', em: -120.0 }]).expect("rewrites");
        assert_ne!(baked, face);
        assert!(key.verify_full_face(&baked).is_ok());

        // Another key sharing the first 32 bits has the SAME PostScript name: only the unique
        // identifier (which carries the whole key) tells the faces apart.
        let mut colliding = [0x5a; 32];
        colliding[31] = 0x00;
        let colliding = RecipeKey(colliding);
        assert_eq!(colliding.full_face_post_script_name(), key.full_face_post_script_name());
        assert!(matches!(colliding.verify_full_face(&face), Err(FaceKeyMismatch::UniqueId { .. })));

        let other = RecipeKey([0x11; 32]);
        assert!(matches!(other.verify_full_face(&face), Err(FaceKeyMismatch::PostScriptName { .. })));
        let no_unique = named_face(&key.full_face_post_script_name(), "");
        assert!(matches!(
            key.verify_full_face(&no_unique),
            Err(FaceKeyMismatch::MissingName { .. } | FaceKeyMismatch::UniqueId { .. })
        ));
        assert!(matches!(key.verify_full_face(b"not a font"), Err(FaceKeyMismatch::Unparsable { .. })));
    }

    #[test]
    fn unique_id_format_is_pinned() {
        // Pins the `name` ID 3 format the compiler writes: a change alters output bytes and
        // must come with an `ENGINE_VERSION` bump.
        let key = RecipeKey([0xab; 32]);
        assert_eq!(key.full_face_unique_id(), format!("MSCustom-abababab;{};ms-custom-fonts engine {ENGINE_VERSION}", "ab".repeat(32)));
    }

    #[test]
    fn digest_matches_a_fixed_vector() {
        // Pins the canonical encoding: a change here invalidates every cache on disk and must
        // come with an `ENGINE_VERSION` bump.
        let digest = source_digest(b"", 0);
        assert_eq!(
            hex_lower(&digest.0),
            hex_lower(&{
                let mut hasher = Sha256::new();
                hasher.update(b"ms-custom-fonts/source/v1\0\0\0\0\0");
                <[u8; 32]>::from(hasher.finalize())
            })
        );
    }
}
