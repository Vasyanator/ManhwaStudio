/*
File: crates/ms-custom-fonts/src/hybrid/kerning.rs

Purpose:
The hybrid's INHERITED kerning (decision D6): a weighted average of the sources' effective
pairs, written as glyph pairs for the legacy `kern` table.

Key functions:
- `blend_pairs`: per-source effective pairs -> sorted `(left glyph, right glyph, value)`.

Notes:
- Only pairs whose two characters are both in the hybrid are considered.
- Contributors of a pair = the sources that HAVE both characters; their recipe weights are
  renormalized over them (consistent with the glyphs, which a source lacking a character does
  not shape either). A contributor without the pair adds 0 (its default spacing); a pair no
  contributor has does not exist.
- Values are per-mille of an em, which at `TARGET_UPM` = 1000 are font units; rounded, zero
  results dropped. When two characters share one output glyph, the pair of the smaller
  characters (ascending order) decides the glyph pair.
- The hybrid's OWN user pairs are not here: the provider attaches them at render time.
*/

use std::collections::{BTreeMap, BTreeSet};

use crate::hybrid::metrics::units;

/// Blends the effective pairs of every source.
///
/// `pairs[i]` and `chars[i]` are source `i`'s effective pairs (per-mille) and the hybrid
/// characters it has; `weights[i]` its normalized recipe weight; `cmap` maps every hybrid
/// character to its output glyph.
pub(crate) fn blend_pairs(
    pairs: &[BTreeMap<(char, char), f64>],
    chars: &[BTreeSet<char>],
    weights: &[f64],
    cmap: &BTreeMap<char, u16>,
) -> Vec<(u16, u16, i16)> {
    let keys: BTreeSet<(char, char)> = pairs
        .iter()
        .zip(chars)
        .flat_map(|(source_pairs, source_chars)| {
            source_pairs.keys().filter(|(left, right)| source_chars.contains(left) && source_chars.contains(right))
        })
        .copied()
        .collect();
    let mut glyph_pairs: BTreeMap<(u16, u16), i16> = BTreeMap::new();
    for (left, right) in keys {
        let (Some(&left_glyph), Some(&right_glyph)) = (cmap.get(&left), cmap.get(&right)) else {
            continue;
        };
        let mut sum = 0.0;
        let mut total = 0.0;
        for ((source_pairs, source_chars), weight) in pairs.iter().zip(chars).zip(weights) {
            if source_chars.contains(&left) && source_chars.contains(&right) {
                total += weight;
                sum += weight * source_pairs.get(&(left, right)).copied().unwrap_or(0.0);
            }
        }
        if total <= 0.0 {
            continue;
        }
        // Zero is stored too: it still decides the glyph pair against later character pairs.
        glyph_pairs.entry((left_glyph, right_glyph)).or_insert(units(sum / total));
    }
    glyph_pairs.into_iter().filter(|(_, value)| *value != 0).map(|((left, right), value)| (left, right, value)).collect()
}
