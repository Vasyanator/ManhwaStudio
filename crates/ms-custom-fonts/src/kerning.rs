/*
File: crates/ms-custom-fonts/src/kerning.rs

Purpose:
Reads a source face's BUILT-IN pair kerning, uncapped, the way the shaper applies it, and
defines the user kerning pair type callers pass in.

Key structures:
- `KerningPairPerMille`: one user-authored pair of a source, in thousandths of an em.

Key functions:
- `builtin_glyph_pairs`: every non-zero `(left glyph, right glyph) -> value` (source units)
  among a given glyph set.

Notes:
Which table counts follows rustybuzz (`hb/ot_shape.rs`, "Decide who does positioning"): the
GPOS lookups of the `kern` and `dist` features apply; the legacy `kern` table applies only when
GPOS has no `kern` feature. Within one GPOS lookup the FIRST subtable that matches a pair
decides it (a format-2 subtable matches every right glyph once its left glyph is covered);
values of different lookups and of legacy `kern` subtables add up. The value of a GPOS pair is
the first glyph's `x_advance`, as in the settings font inspector. Unlike that inspector nothing
here is capped: the caller restricts the glyph set instead (the compile scope).
*/

use std::collections::{BTreeMap, HashSet};

use ttf_parser::gpos::{PairAdjustment, PositioningSubtable};
use ttf_parser::opentype_layout::LayoutTable;
use ttf_parser::{Face, GlyphId, Tag};

/// One user-authored kerning pair of a font: `em` is the offset in THOUSANDTHS of an em
/// (`ms_text_render::CustomKerningTable` units). It replaces the font's built-in value for the
/// pair; `0.0` is meaningful (it cancels a built-in pair).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KerningPairPerMille {
    /// Left character.
    pub left: char,
    /// Right character.
    pub right: char,
    /// Offset in thousandths of an em.
    pub em: f32,
}

/// GPOS feature tags whose lookups the shaper applies as horizontal pair kerning.
const KERN_TAG: Tag = Tag::from_bytes(b"kern");
const DIST_TAG: Tag = Tag::from_bytes(b"dist");

/// Built-in kerning of `face` between glyphs of `glyphs` (sorted, unique), in source units.
/// Pairs whose total is zero are omitted.
pub(crate) fn builtin_glyph_pairs(face: &Face<'_>, glyphs: &[GlyphId]) -> BTreeMap<(u16, u16), i32> {
    let mut total: BTreeMap<(u16, u16), i32> = BTreeMap::new();
    let mut gpos_has_kern = false;
    if let Some(gpos) = face.tables().gpos {
        let (lookups, has_kern) = pair_lookup_indices(&gpos);
        gpos_has_kern = has_kern;
        for lookup_index in lookups {
            if let Some(lookup) = gpos.lookups.get(lookup_index) {
                for (pair, value) in lookup_pairs(&lookup, glyphs) {
                    *total.entry(pair).or_insert(0) += i32::from(value);
                }
            }
        }
    }
    if !gpos_has_kern {
        add_legacy_kern(face, glyphs, &mut total);
    }
    total.retain(|_, value| *value != 0);
    total
}

/// Lookup indices referenced by `kern` / `dist` features (deduplicated, first-seen order) and
/// whether a `kern` feature exists at all.
fn pair_lookup_indices(gpos: &LayoutTable<'_>) -> (Vec<u16>, bool) {
    let mut indices = Vec::new();
    let mut seen = HashSet::new();
    let mut has_kern = false;
    for feature in gpos.features {
        if feature.tag == KERN_TAG {
            has_kern = true;
        } else if feature.tag != DIST_TAG {
            continue;
        }
        for lookup_index in feature.lookup_indices {
            if seen.insert(lookup_index) {
                indices.push(lookup_index);
            }
        }
    }
    (indices, has_kern)
}

/// The pairs one GPOS lookup decides among `glyphs`, with the first-matching-subtable rule.
/// Zero values decided by a format-1 subtable are kept (they block later subtables).
fn lookup_pairs(lookup: &ttf_parser::opentype_layout::Lookup<'_>, glyphs: &[GlyphId]) -> BTreeMap<(u16, u16), i16> {
    let mut decided: BTreeMap<(u16, u16), i16> = BTreeMap::new();
    // Left glyphs a format-2 subtable already matched against every right glyph.
    let mut consumed_left: HashSet<u16> = HashSet::new();
    for subtable in lookup.subtables.into_iter::<PositioningSubtable>() {
        let PositioningSubtable::Pair(pair) = subtable else {
            continue;
        };
        match pair {
            PairAdjustment::Format1 { coverage, sets } => {
                for &left in glyphs {
                    if consumed_left.contains(&left.0) {
                        continue;
                    }
                    let Some(set) = coverage.get(left).and_then(|index| sets.get(index)) else {
                        continue;
                    };
                    for &right in glyphs {
                        if let Some((first, _)) = set.get(right) {
                            decided.entry((left.0, right.0)).or_insert(first.x_advance);
                        }
                    }
                }
            }
            PairAdjustment::Format2 { coverage, classes, matrix } => {
                let (class_def1, class_def2) = classes;
                // Right glyphs grouped by their second class, so each matrix cell is read once.
                let mut buckets: BTreeMap<u16, Vec<GlyphId>> = BTreeMap::new();
                for &right in glyphs {
                    buckets.entry(class_def2.get(right)).or_default().push(right);
                }
                for &left in glyphs {
                    if consumed_left.contains(&left.0) || coverage.get(left).is_none() {
                        continue;
                    }
                    let class1 = class_def1.get(left);
                    for (&class2, rights) in &buckets {
                        let value = matrix.get((class1, class2)).map_or(0, |(first, _)| first.x_advance);
                        if value == 0 {
                            continue;
                        }
                        for right in rights {
                            decided.entry((left.0, right.0)).or_insert(value);
                        }
                    }
                    consumed_left.insert(left.0);
                }
            }
        }
    }
    decided
}

/// Adds the legacy `kern` table's horizontal pairs among `glyphs` (subtables are additive).
fn add_legacy_kern(face: &Face<'_>, glyphs: &[GlyphId], total: &mut BTreeMap<(u16, u16), i32>) {
    let Some(kern) = face.tables().kern else {
        return;
    };
    let in_scope: HashSet<u16> = glyphs.iter().map(|glyph| glyph.0).collect();
    for subtable in kern.subtables {
        // Only plain horizontal advance kerning; cross-stream, variable and state-machine
        // subtables are not pair kerning.
        if !subtable.horizontal || subtable.variable || subtable.has_state_machine || subtable.has_cross_stream {
            continue;
        }
        if let ttf_parser::kern::Format::Format0(format0) = &subtable.format {
            for pair in format0.pairs {
                if in_scope.contains(&pair.left().0) && in_scope.contains(&pair.right().0) {
                    *total.entry((pair.left().0, pair.right().0)).or_insert(0) += i32::from(pair.value);
                }
            }
        } else {
            // Class-based formats cannot be enumerated: query every pair of the scope.
            for &left in glyphs {
                for &right in glyphs {
                    if let Some(value) = subtable.glyphs_kerning(left, right) {
                        *total.entry((left.0, right.0)).or_insert(0) += i32::from(value);
                    }
                }
            }
        }
    }
}
