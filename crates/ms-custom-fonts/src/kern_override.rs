/*
File: crates/ms-custom-fonts/src/kern_override.rs

Purpose:
Bakes a custom font's OWN user kerning pairs into a face this crate compiled, for consumers
that hand the file to another program (the PSD export puts it next to the documents, where
Photoshop applies only what the file declares). The in-app renderer never needs this: it
applies the own pairs at render time (the provider's custom kerning table).

Key functions:
- `with_kerning_overrides`: the face's `kern` pairs overridden by user pairs (per-mille of an
  em; `0.0` removes a pair), every other table byte-identical.

Notes:
- Semantics mirror the renderer's custom kerning contract (REPLACE, not add): an override
  stands in place of the face's own value for that pair; `0.0` cancels it.
- The rewrite goes through `sfnt::replace_kern`, the writer's own `kern` encoder and assembler,
  so the table format has one owner. Names are untouched: the PostScript name stays the one
  the compiled face (and any document referring to it) carries.
- The compiled face, its `RecipeKey` and caches are not affected; this returns new bytes.
*/

use std::collections::BTreeMap;

use ttf_parser::{Face, GlyphId};

use crate::error::CompileError;
use crate::hybrid::metrics::units;
use crate::kerning::KerningPairPerMille;
use crate::sfnt::replace_kern;

/// Returns `face` (a full or subset face `compile` produced) with its kerning replaced by the
/// EFFECTIVE kerning: its own `kern` pairs, overridden per pair by `overrides`.
///
/// - `overrides` are in thousandths of an em (converted with the face's `unitsPerEm`, rounded
///   to integer font units); for a repeated `(left, right)` the FIRST occurrence wins (the
///   font settings store's rule).
/// - An override of `0.0` (or one that rounds to 0 units) removes the pair; any other value
///   replaces or adds it.
/// - Pairs whose characters the face does not map are ignored.
/// - Pairs are glyph pairs in the file: when two characters share one glyph, the override of
///   the smaller character pair (ascending order) decides it, as in the inherited kerning.
/// - Every table except `kern` is copied byte for byte; checksums are recomputed. Empty
///   `overrides` reproduce a face this crate wrote byte for byte.
///
/// # Errors
/// - `UnsupportedFace`: not a parsable TrueType face, or a face with a GPOS table (shapers would
///   ignore `kern`) or with a `kern` subtable other than horizontal format 0.
/// - `InvalidUserKerning` (identity = the face's PostScript name): a non-finite value.
/// - `UserKerningOutOfRange`: an applied value beyond the 16-bit range in font units.
/// - `OutputTooLarge`: as in `compile`.
pub fn with_kerning_overrides(face: &[u8], overrides: &[KerningPairPerMille]) -> Result<Vec<u8>, CompileError> {
    let parsed = Face::parse(face, 0).map_err(|err| CompileError::UnsupportedFace { reason: err.to_string() })?;
    if parsed.tables().gpos.is_some() {
        return Err(unsupported("it has a GPOS table, so shapers would ignore its 'kern' table"));
    }
    let mut glyph_pairs = face_kern_pairs(&parsed)?;

    // First occurrence of a character pair wins; the map then iterates in ascending character
    // order, which decides glyph pairs shared by several character pairs.
    let mut by_chars: BTreeMap<(char, char), f32> = BTreeMap::new();
    for pair in overrides {
        if !pair.em.is_finite() {
            return Err(CompileError::InvalidUserKerning {
                identity: post_script_name(&parsed),
                left: pair.left,
                right: pair.right,
            });
        }
        by_chars.entry((pair.left, pair.right)).or_insert(pair.em);
    }
    let units_per_em = f64::from(parsed.units_per_em());
    let mut by_glyphs: BTreeMap<(u16, u16), i16> = BTreeMap::new();
    for ((left, right), em) in by_chars {
        let (Some(left_glyph), Some(right_glyph)) = (mapped_glyph(&parsed, left), mapped_glyph(&parsed, right)) else {
            continue;
        };
        let value = (f64::from(em) * units_per_em / 1000.0).round();
        if value < f64::from(i16::MIN) || value > f64::from(i16::MAX) {
            return Err(CompileError::UserKerningOutOfRange { left, right, em });
        }
        by_glyphs.entry((left_glyph, right_glyph)).or_insert(units(value));
    }
    for (glyphs, value) in by_glyphs {
        if value == 0 {
            glyph_pairs.remove(&glyphs);
        } else {
            glyph_pairs.insert(glyphs, i32::from(value));
        }
    }

    let pairs = glyph_pairs
        .into_iter()
        .filter(|(_, value)| *value != 0)
        .map(|((left, right), value)| {
            i16::try_from(value)
                .map(|value| (left, right, value))
                .map_err(|_| unsupported("its kerning subtables add up beyond the 16-bit range"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    replace_kern(face, &pairs)
}

/// The face's `kern` pairs (subtables are additive), keyed by `(left, right)` glyph ids.
///
/// # Errors
/// `UnsupportedFace` for a subtable that is not plain horizontal format 0 (this crate writes
/// only those; anything else cannot be re-encoded without loss).
fn face_kern_pairs(face: &Face<'_>) -> Result<BTreeMap<(u16, u16), i32>, CompileError> {
    let mut pairs = BTreeMap::new();
    let Some(kern) = face.tables().kern else {
        return Ok(pairs);
    };
    for subtable in kern.subtables {
        if !subtable.horizontal || subtable.variable || subtable.has_state_machine || subtable.has_cross_stream {
            return Err(unsupported("its 'kern' table has a subtable that is not plain horizontal kerning"));
        }
        let ttf_parser::kern::Format::Format0(format0) = &subtable.format else {
            return Err(unsupported("its 'kern' table has a subtable that is not format 0"));
        };
        for pair in format0.pairs {
            *pairs.entry((pair.left().0, pair.right().0)).or_insert(0) += i32::from(pair.value);
        }
    }
    Ok(pairs)
}

/// The glyph `ch` maps to, `None` when unmapped (glyph 0 is `.notdef`, i.e. unmapped).
fn mapped_glyph(face: &Face<'_>, ch: char) -> Option<u16> {
    face.glyph_index(ch).filter(|glyph| *glyph != GlyphId(0)).map(|glyph| glyph.0)
}

/// The face's PostScript name (empty when it has none readable), for error context.
fn post_script_name(face: &Face<'_>) -> String {
    face.names()
        .into_iter()
        .filter(|name| name.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
        .find_map(|name| name.to_string())
        .unwrap_or_default()
}

/// `UnsupportedFace` with `reason`.
fn unsupported(reason: &str) -> CompileError {
    CompileError::UnsupportedFace { reason: reason.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::TtPoint;
    use crate::sfnt::{FontBuild, FontMetrics, FontNames, GlyphData, write_font};

    /// A square contour so every glyph has ink and a box.
    fn square() -> Vec<Vec<TtPoint>> {
        let point = |x, y| TtPoint { x, y, on: true };
        vec![vec![point(50, 0), point(50, 700), point(450, 700), point(450, 0)]]
    }

    /// A small face: `.notdef`, A=1, V=2, B=3, C and D share glyph 4. Kern A/V -80, V/A -60,
    /// B/B 30.
    fn face() -> Vec<u8> {
        let glyph = || GlyphData { advance: 500, contours: square() };
        let build = FontBuild {
            glyphs: vec![glyph(), glyph(), glyph(), glyph(), glyph()],
            cmap: vec![('A', 1), ('B', 3), ('C', 4), ('D', 4), ('V', 2)],
            kern_pairs: vec![(1, 2, -80), (2, 1, -60), (3, 3, 30)],
            metrics: FontMetrics {
                ascender: 800,
                descender: -200,
                underline_position: -100,
                underline_thickness: 50,
                weight_class: 400,
                ..FontMetrics::default()
            },
            names: FontNames {
                family: "MS Custom 01234567".to_owned(),
                subfamily: "Regular".to_owned(),
                unique_id: "MSCustom-01234567".to_owned(),
                full_name: "MS Custom 01234567".to_owned(),
                version: "Version 1.0".to_owned(),
                postscript: "MSCustom-01234567".to_owned(),
            },
        };
        write_font(&build).expect("writes")
    }

    fn pair(left: char, right: char, em: f32) -> KerningPairPerMille {
        KerningPairPerMille { left, right, em }
    }

    /// The `kern` value between two characters, `None` when there is no pair.
    fn kern(bytes: &[u8], left: char, right: char) -> Option<i16> {
        let face = Face::parse(bytes, 0).expect("valid face");
        let (l, r) = (face.glyph_index(left)?, face.glyph_index(right)?);
        face.tables().kern?.subtables.into_iter().find_map(|subtable| subtable.glyphs_kerning(l, r))
    }

    /// Every table but `kern`, with `head.checkSumAdjustment` zeroed (it covers the file).
    fn tables_without_kern(bytes: &[u8]) -> Vec<crate::sfnt::Table> {
        let mut tables: Vec<_> = crate::sfnt::read_tables(bytes).expect("readable directory").into_iter().filter(|(tag, _)| tag != b"kern").collect();
        for (tag, data) in &mut tables {
            if tag == b"head" {
                data[8..12].fill(0);
            }
        }
        tables
    }

    #[test]
    fn no_overrides_reproduce_the_face_byte_for_byte() {
        let original = face();
        assert_eq!(with_kerning_overrides(&original, &[]).expect("rewrites"), original);
    }

    #[test]
    fn overrides_replace_cancel_and_add_pairs() {
        let original = face();
        let out = with_kerning_overrides(
            &original,
            &[pair('A', 'V', -120.0), pair('V', 'A', 0.0), pair('A', 'B', 45.4), pair('A', 'V', 999.0)],
        )
        .expect("rewrites");
        assert_eq!(kern(&out, 'A', 'V'), Some(-120), "an override replaces the inherited value; the first duplicate wins");
        assert_eq!(kern(&out, 'V', 'A'), None, "0 cancels the inherited pair");
        assert_eq!(kern(&out, 'A', 'B'), Some(45), "a new pair is added, rounded to units");
        assert_eq!(kern(&out, 'B', 'B'), Some(30), "untouched pairs stay");
        let face = Face::parse(&out, 0).expect("the output parses");
        assert_eq!(face.number_of_glyphs(), 5);
        assert_eq!(tables_without_kern(&out), tables_without_kern(&original), "every other table is byte-identical");
    }

    #[test]
    fn unknown_characters_are_ignored_and_cancelling_everything_drops_kern() {
        let original = face();
        let out = with_kerning_overrides(
            &original,
            &[pair('A', 'V', 0.0), pair('V', 'A', 0.0), pair('B', 'B', 0.0), pair('A', 'Ж', -50.0), pair('Ж', 'Ж', f32::MAX)],
        )
        .expect("rewrites");
        let face = Face::parse(&out, 0).expect("the output parses");
        assert!(face.tables().kern.is_none(), "no pair left: no 'kern' table");
        assert_eq!(tables_without_kern(&out), tables_without_kern(&original));
    }

    #[test]
    fn shared_glyph_is_decided_by_the_smaller_character_pair() {
        let out = with_kerning_overrides(&face(), &[pair('A', 'D', -10.0), pair('A', 'C', -20.0)]).expect("rewrites");
        assert_eq!(kern(&out, 'A', 'C'), Some(-20));
        assert_eq!(kern(&out, 'A', 'D'), Some(-20), "C and D are one glyph");
    }

    #[test]
    fn invalid_values_and_foreign_faces_are_errors() {
        let original = face();
        assert_eq!(
            with_kerning_overrides(&original, &[pair('A', 'V', f32::NAN)]),
            Err(CompileError::InvalidUserKerning { identity: "MSCustom-01234567".to_owned(), left: 'A', right: 'V' })
        );
        assert_eq!(
            with_kerning_overrides(&original, &[pair('A', 'V', 40_000.0)]),
            Err(CompileError::UserKerningOutOfRange { left: 'A', right: 'V', em: 40_000.0 })
        );
        assert!(matches!(with_kerning_overrides(b"not a font", &[]), Err(CompileError::UnsupportedFace { .. })));
    }
}
