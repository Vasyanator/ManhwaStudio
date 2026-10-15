/*
File: crates/ms-custom-fonts/src/hybrid/plan.rs

Purpose:
The hybrid's per-character decision: which characters exist in the hybrid and which sources
shape each of them, plus the coverage report derived from the same decision (one owner, so the
report can never disagree with the compiled cmap).

Key structures:
- `CharPlan`: included characters with their participating-source mask, and the report.

Key functions:
- `plan_chars`: applies the missing-glyph rule over the scope's characters.

Notes:
Rule (decision D9): for a character, every source that LACKS it is checked with its effective
policy (own override, else the hybrid default). Any `Disable` -> the character is absent;
otherwise it is shaped by the sources that have it (weights renormalized later). A character
no source has is not part of the scope at all (not counted).
*/

use std::collections::BTreeSet;

use crate::CoverageReport;
use crate::recipe::{HybridParams, MissingGlyphPolicy};
use crate::source::SourceFace;

/// At most this many disabled characters are listed in `CoverageReport::disabled_sample`.
pub(crate) const DISABLED_SAMPLE_CAP: usize = 64;

/// The included characters (ascending) with a bit mask of the sources that have each, and the
/// coverage report of the same scope.
#[derive(Debug)]
pub(crate) struct CharPlan {
    /// `(character, mask)`; bit `i` set = source `i` has the character. Never 0.
    pub included: Vec<(char, u8)>,
    /// Counts over the scope.
    pub report: CoverageReport,
}

/// Plans the characters of `scope` (`None` = the union of every source's cmap).
pub(crate) fn plan_chars(params: &HybridParams, faces: &[SourceFace<'_>], scope: Option<&BTreeSet<char>>) -> CharPlan {
    let candidates: BTreeSet<char> = match scope {
        Some(chars) => chars.clone(),
        None => faces.iter().flat_map(SourceFace::chars).collect(),
    };
    let all_mask = full_mask(faces.len());
    let mut included = Vec::with_capacity(candidates.len());
    let mut report = CoverageReport::default();
    for ch in candidates {
        let mask = faces
            .iter()
            .enumerate()
            .filter(|(_, face)| face.glyph(ch).is_some())
            .fold(0_u8, |mask, (index, _)| mask | bit(index));
        if mask == 0 {
            continue;
        }
        report.total_chars += 1;
        if mask == all_mask {
            included.push((ch, mask));
            continue;
        }
        report.missing_in_some += 1;
        let disabled = params
            .sources
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & bit(*index) == 0)
            .any(|(_, source)| source.effective_missing(params.missing_default) == MissingGlyphPolicy::Disable);
        if disabled {
            report.disabled += 1;
            if report.disabled_sample.len() < DISABLED_SAMPLE_CAP {
                report.disabled_sample.push(ch);
            }
        } else {
            included.push((ch, mask));
        }
    }
    CharPlan { included, report }
}

/// Mask bit of source `index` (the recipe has at most `MAX_HYBRID_SOURCES` = 4 sources).
pub(crate) fn bit(index: usize) -> u8 {
    u32::try_from(index).ok().and_then(|shift| 1_u8.checked_shl(shift)).unwrap_or(0)
}

/// Mask with the bits of all `count` sources set.
fn full_mask(count: usize) -> u8 {
    (0..count).fold(0, |mask, index| mask | bit(index))
}
