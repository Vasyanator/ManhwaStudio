/*
File: crates/ms-custom-fonts/src/source.rs

Purpose:
One parsed source font in normalized units: `ttf-parser` face, unit scale, cmap,
outlines, advances, vertical metrics and EFFECTIVE kerning (built-in pairs overridden by the
source's user pairs), all scaled to `TARGET_UPM`.

Key structures:
- `SourceFont`: the caller's input (identity, bytes, face index, user kerning).
- `SourceFace`: the parsed, normalized view used by the compilers.
- `SourceMetrics`: normalized vertical metrics and style facts.

Key functions:
- `SourceFace::parse`, `chars`, `glyph`, `outline`, `advance`, `metrics`, `effective_pairs`.

Notes:
CFF outlines are counter-clockwise (PostScript convention); they are reversed on read so
every source shares the TrueType orientation (outer contours clockwise), which the compatible
blend and the output rely on. Composite TrueType glyphs arrive decomposed from `ttf-parser`.
*/

use std::collections::{BTreeMap, BTreeSet};

use ttf_parser::{Face, GlyphId};

use crate::error::CompileError;
use crate::kerning::{KerningPairPerMille, builtin_glyph_pairs};
use crate::outline::{Outline, OutlineCollector, TARGET_UPM, reverse_contour};

/// One source font supplied by the caller for a compile, in recipe order.
#[derive(Debug, Clone, Copy)]
pub struct SourceFont<'a> {
    /// Identity of the font; must equal the recipe's identity at the same position.
    pub identity: &'a str,
    /// The whole font file (`.ttf` / `.otf` / collection).
    pub data: &'a [u8],
    /// Face inside `data` (0 for single-face files).
    pub face_index: u32,
    /// The user's custom kerning of THIS source (overrides its built-in pairs).
    pub user_kerning: &'a [KerningPairPerMille],
}

/// Vertical metrics and style facts of a source, in normalized units.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceMetrics {
    pub ascender: f64,
    pub descender: f64,
    pub line_gap: f64,
    pub x_height: Option<f64>,
    pub cap_height: Option<f64>,
    pub underline_position: Option<f64>,
    pub underline_thickness: Option<f64>,
    pub weight_class: f64,
    pub italic: bool,
    pub italic_angle: f64,
}

/// A parsed source in normalized units.
pub(crate) struct SourceFace<'a> {
    face: Face<'a>,
    /// `TARGET_UPM / units_per_em` of the source.
    scale: f64,
    /// PostScript-flavoured outlines (CFF/CFF2) are reversed on read.
    reverse_outlines: bool,
    user_kerning: &'a [KerningPairPerMille],
}

impl<'a> SourceFace<'a> {
    /// Parses a source.
    ///
    /// # Errors
    /// `SourceUnparsable` when the bytes are not a readable face or report zero units per em;
    /// `InvalidUserKerning` when a user pair is not finite.
    pub(crate) fn parse(source: &SourceFont<'a>) -> Result<Self, CompileError> {
        let face = Face::parse(source.data, source.face_index).map_err(|error| CompileError::SourceUnparsable {
            identity: source.identity.to_owned(),
            reason: error.to_string(),
        })?;
        let units_per_em = face.units_per_em();
        if units_per_em == 0 {
            return Err(CompileError::SourceUnparsable {
                identity: source.identity.to_owned(),
                reason: "the font reports 0 units per em".to_owned(),
            });
        }
        if let Some(bad) = source.user_kerning.iter().find(|pair| !pair.em.is_finite()) {
            return Err(CompileError::InvalidUserKerning {
                identity: source.identity.to_owned(),
                left: bad.left,
                right: bad.right,
            });
        }
        let tables = face.tables();
        let reverse_outlines = tables.glyf.is_none() && tables.cff.is_some();
        Ok(Self {
            scale: f64::from(TARGET_UPM) / f64::from(units_per_em),
            reverse_outlines,
            user_kerning: source.user_kerning,
            face,
        })
    }

    /// Every character the source maps to a real glyph (not `.notdef`), sorted.
    pub(crate) fn chars(&self) -> BTreeSet<char> {
        let mut chars = BTreeSet::new();
        if let Some(cmap) = self.face.tables().cmap {
            for subtable in cmap.subtables {
                if !subtable.is_unicode() {
                    continue;
                }
                subtable.codepoints(|codepoint| {
                    if let Some(ch) = char::from_u32(codepoint)
                        && self.glyph(ch).is_some()
                    {
                        chars.insert(ch);
                    }
                });
            }
        }
        chars
    }

    /// The glyph for `ch`, or `None` when the source lacks it (unmapped or mapped to 0).
    pub(crate) fn glyph(&self, ch: char) -> Option<GlyphId> {
        self.face.glyph_index(ch).filter(|glyph| glyph.0 != 0)
    }

    /// The glyph's outline in normalized units, TrueType orientation; empty for blank glyphs.
    pub(crate) fn outline(&self, glyph: GlyphId) -> Outline {
        let mut collector = OutlineCollector::new(self.scale);
        // `None` means "no outline" (space) or unreadable glyph data; both draw nothing.
        if self.face.outline_glyph(glyph, &mut collector).is_none() {
            return Vec::new();
        }
        let outline = collector.finish();
        if self.reverse_outlines { outline.iter().map(reverse_contour).collect() } else { outline }
    }

    /// The glyph's horizontal advance in normalized units (0 when the font has none).
    pub(crate) fn advance(&self, glyph: GlyphId) -> f64 {
        f64::from(self.face.glyph_hor_advance(glyph).unwrap_or(0)) * self.scale
    }

    /// Vertical metrics and style facts.
    pub(crate) fn metrics(&self) -> SourceMetrics {
        let scale = |value: i16| f64::from(value) * self.scale;
        let underline = self.face.underline_metrics();
        SourceMetrics {
            ascender: scale(self.face.ascender()),
            descender: scale(self.face.descender()),
            line_gap: scale(self.face.line_gap()),
            x_height: self.face.x_height().map(scale),
            cap_height: self.face.capital_height().map(scale),
            underline_position: underline.map(|metrics| scale(metrics.position)),
            underline_thickness: underline.map(|metrics| scale(metrics.thickness)),
            weight_class: f64::from(self.face.weight().to_number()),
            italic: self.face.is_italic(),
            italic_angle: f64::from(self.face.italic_angle()),
        }
    }

    /// EFFECTIVE kerning among `chars` (characters this source has), in per-mille of an em:
    /// built-in pairs, then the source's user pairs replacing them (a user `0` cancels).
    pub(crate) fn effective_pairs(&self, chars: &BTreeSet<char>) -> BTreeMap<(char, char), f64> {
        let mut by_glyph: BTreeMap<u16, Vec<char>> = BTreeMap::new();
        for &ch in chars {
            if let Some(glyph) = self.glyph(ch) {
                by_glyph.entry(glyph.0).or_default().push(ch);
            }
        }
        let glyphs: Vec<GlyphId> = by_glyph.keys().map(|&glyph| GlyphId(glyph)).collect();
        let mut pairs = BTreeMap::new();
        for ((left, right), value) in builtin_glyph_pairs(&self.face, &glyphs) {
            let (Some(lefts), Some(rights)) = (by_glyph.get(&left), by_glyph.get(&right)) else {
                continue;
            };
            let per_mille = f64::from(value) * self.scale;
            for &left_char in lefts {
                for &right_char in rights {
                    pairs.insert((left_char, right_char), per_mille);
                }
            }
        }
        for pair in self.user_kerning {
            if chars.contains(&pair.left) && chars.contains(&pair.right) {
                pairs.insert((pair.left, pair.right), f64::from(pair.em));
            }
        }
        pairs
    }
}
