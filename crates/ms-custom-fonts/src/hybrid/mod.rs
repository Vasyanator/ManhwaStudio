/*
File: crates/ms-custom-fonts/src/hybrid/mod.rs

Purpose:
Compiler of the hybrid kind: recipe + source fonts -> TrueType face whose glyphs, metrics and
kerning are weighted averages of the sources.

Key structures:
- `GlyphJob`: one output glyph — the source glyphs shaping it and their renormalized weights.

Key functions:
- `compile`: the whole pipeline (match sources, plan characters, blend glyphs in parallel,
  blend metrics and kerning, name the face, write it).
- `coverage`: the character plan's report only (cmap work, no outlines).
- `included_chars`: the character plan's included set over given candidates (cmap work only).
- `blend_glyph`: one glyph — single source copy, compatible interpolation, matched
  (correspondence) interpolation, or SDF blend, in that order.

Notes:
- Glyph order: `.notdef`, then glyphs in order of their first character. Characters whose
  source glyphs are identical in every participating source share one output glyph.
- Parallelism: glyph jobs run on the caller's rayon pool in fixed-size chunks; each glyph is
  independent and results are assembled by index, so the bytes do not depend on thread
  count. Progress is reported on the calling thread after each chunk; cancel is polled per
  glyph and between chunks.
- Empty outlines (a space, or a blank glyph in one source while another has ink): the shape
  comes from the sources with ink only (weights renormalized among them); every participating
  source still counts for the advance.
*/

pub(crate) mod compatible;
pub(crate) mod kerning;
pub(crate) mod matched;
pub(crate) mod metrics;
pub(crate) mod plan;
pub(crate) mod sdf;
#[cfg(test)]
mod tests;
pub(crate) mod trace;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;
use ttf_parser::GlyphId;

use crate::error::CompileError;
use crate::key::{face_unique_id, recipe_key, source_digest, subset_tag};
use crate::outline::{Contour, Outline, Seg, TARGET_UPM, to_truetype};
use crate::recipe::{CustomFontRecipe, HybridParams};
use crate::sfnt::{FontBuild, FontNames, GlyphData, write_font};
use crate::source::{SourceFace, SourceFont};
use crate::{CompileScope, CompiledFont, CoverageReport};

/// Glyph jobs per parallel chunk (progress granularity).
const CHUNK_GLYPHS: usize = 64;

/// One output glyph to compute.
#[derive(Debug)]
struct GlyphJob {
    /// First character mapped to the glyph (error context).
    ch: char,
    /// `(source index, source glyph)` of every participating source, in recipe order.
    sources: Vec<(usize, GlyphId)>,
    /// Weights of `sources`, normalized to sum 1.
    weights: Vec<f64>,
}

/// Compiles a hybrid. See `crate::compile` for the contract.
pub(crate) fn compile(
    recipe: &CustomFontRecipe,
    params: &HybridParams,
    sources: &[SourceFont<'_>],
    scope: CompileScope<'_>,
    progress: &(dyn Fn(usize, usize) + Sync),
    cancel: &AtomicBool,
) -> Result<CompiledFont, CompileError> {
    let faces = prepare(recipe, params, sources)?;
    let scope_chars = match scope {
        CompileScope::Full => None,
        CompileScope::Subset(chars) => Some(chars),
    };
    let plan = plan::plan_chars(params, &faces, scope_chars);
    if cancel.load(Ordering::Relaxed) {
        return Err(CompileError::Cancelled);
    }

    let recipe_weights: Vec<f64> = params.sources.iter().map(|source| f64::from(source.weight)).collect();
    let (jobs, cmap) = glyph_jobs(&faces, &plan.included, &recipe_weights);
    let glyph_count = jobs.len() + 1;
    if glyph_count > usize::from(u16::MAX) {
        return Err(CompileError::TooManyGlyphs { count: glyph_count });
    }

    let mut glyphs = Vec::with_capacity(glyph_count);
    glyphs.push(notdef_glyph());
    let total = jobs.len();
    progress(0, total);
    for chunk in jobs.chunks(CHUNK_GLYPHS) {
        if cancel.load(Ordering::Relaxed) {
            return Err(CompileError::Cancelled);
        }
        let results: Vec<Result<GlyphData, CompileError>> = chunk
            .par_iter()
            .map(|job| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(CompileError::Cancelled);
                }
                blend_glyph(&faces, job)
            })
            .collect();
        for result in results {
            glyphs.push(result?);
        }
        progress(glyphs.len() - 1, total);
    }

    let weight_sum: f64 = recipe_weights.iter().sum();
    let normalized: Vec<f64> = recipe_weights.iter().map(|weight| weight / weight_sum).collect();
    let font_metrics = metrics::blend_metrics(&faces.iter().map(SourceFace::metrics).collect::<Vec<_>>(), &normalized);

    let cmap_map: BTreeMap<char, u16> = cmap.iter().copied().collect();
    let source_chars: Vec<BTreeSet<char>> = (0..faces.len())
        .map(|index| plan.included.iter().filter(|(_, mask)| mask & plan::bit(index) != 0).map(|(ch, _)| *ch).collect())
        .collect();
    let source_pairs: Vec<BTreeMap<(char, char), f64>> =
        faces.iter().zip(&source_chars).map(|(face, chars)| face.effective_pairs(chars)).collect();
    if cancel.load(Ordering::Relaxed) {
        return Err(CompileError::Cancelled);
    }
    let kern_pairs = kerning::blend_pairs(&source_pairs, &source_chars, &normalized, &cmap_map);

    let digests: Vec<_> = sources.iter().map(|source| (source_digest(source.data, source.face_index), source.user_kerning)).collect();
    let key = recipe_key(recipe, &digests);
    let short = key.short8();
    let (family_name, postscript_name) = match scope {
        CompileScope::Full => (key.full_face_family_name(), key.full_face_post_script_name()),
        CompileScope::Subset(chars) => {
            let tag = subset_tag(chars);
            (format!("MS Custom Preview {short} {tag}"), format!("MSCustomPreview-{short}-{tag}"))
        }
    };
    let subfamily = match (font_metrics.weight_class >= 700, font_metrics.italic) {
        (false, false) => "Regular",
        (true, false) => "Bold",
        (false, true) => "Italic",
        (true, true) => "Bold Italic",
    };
    let names = FontNames {
        family: family_name.clone(),
        subfamily: subfamily.to_owned(),
        unique_id: face_unique_id(&postscript_name, &key),
        full_name: family_name.clone(),
        version: "Version 1.000".to_owned(),
        postscript: postscript_name.clone(),
    };
    let bytes = write_font(&FontBuild { glyphs, cmap, kern_pairs, metrics: font_metrics, names })?;
    Ok(CompiledFont { bytes, family_name, postscript_name, key, report: plan.report })
}

/// The coverage report of the full character set (cmap lookups only).
pub(crate) fn coverage(
    recipe: &CustomFontRecipe,
    params: &HybridParams,
    sources: &[SourceFont<'_>],
) -> Result<CoverageReport, CompileError> {
    let faces = prepare(recipe, params, sources)?;
    Ok(plan::plan_chars(params, &faces, None).report)
}

/// The characters of `candidates` the compiled face maps (the character plan only).
pub(crate) fn included_chars(
    recipe: &CustomFontRecipe,
    params: &HybridParams,
    sources: &[SourceFont<'_>],
    candidates: &BTreeSet<char>,
) -> Result<BTreeSet<char>, CompileError> {
    let faces = prepare(recipe, params, sources)?;
    Ok(plan::plan_chars(params, &faces, Some(candidates)).included.into_iter().map(|(ch, _)| ch).collect())
}

/// Validates the recipe's structure, matches `sources` to it by position and identity, and
/// parses them.
fn prepare<'a>(
    recipe: &CustomFontRecipe,
    params: &HybridParams,
    sources: &[SourceFont<'a>],
) -> Result<Vec<SourceFace<'a>>, CompileError> {
    // Duplicates by exact spelling; the caller's normalization was applied by its own
    // `validate` call (the compiler has no identity rule of its own).
    recipe.validate(&str::to_owned)?;
    for (index, recipe_source) in params.sources.iter().enumerate() {
        let Some(source) = sources.get(index) else {
            return Err(CompileError::SourceMissing { identity: recipe_source.identity.clone() });
        };
        if source.identity != recipe_source.identity {
            return Err(CompileError::SourceMismatch {
                index,
                expected: recipe_source.identity.clone(),
                found: source.identity.to_owned(),
            });
        }
    }
    if let Some((index, extra)) = sources.iter().enumerate().nth(params.sources.len()) {
        return Err(CompileError::SourceMismatch { index, expected: String::new(), found: extra.identity.to_owned() });
    }
    sources.iter().map(SourceFace::parse).collect()
}

/// Builds one job per distinct combination of source glyphs, in order of first character,
/// and the cmap `(character, output glyph)`.
fn glyph_jobs(faces: &[SourceFace<'_>], included: &[(char, u8)], recipe_weights: &[f64]) -> (Vec<GlyphJob>, Vec<(char, u16)>) {
    let mut jobs: Vec<GlyphJob> = Vec::new();
    let mut by_sources: BTreeMap<Vec<(usize, u16)>, u16> = BTreeMap::new();
    let mut cmap = Vec::with_capacity(included.len());
    for &(ch, mask) in included {
        let participants: Vec<(usize, GlyphId)> = faces
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & plan::bit(*index) != 0)
            .filter_map(|(index, face)| face.glyph(ch).map(|glyph| (index, glyph)))
            .collect();
        let identity: Vec<(usize, u16)> = participants.iter().map(|&(index, glyph)| (index, glyph.0)).collect();
        let glyph_id = if let Some(&existing) = by_sources.get(&identity) {
            existing
        } else {
            // Glyph ids beyond u16 are rejected by the caller's glyph-count check; saturate here.
            let next = u16::try_from(jobs.len() + 1).unwrap_or(u16::MAX);
            let total: f64 = participants.iter().map(|&(index, _)| recipe_weights[index]).sum();
            let weights = participants.iter().map(|&(index, _)| recipe_weights[index] / total).collect();
            jobs.push(GlyphJob { ch, sources: participants, weights });
            by_sources.insert(identity, next);
            next
        };
        cmap.push((ch, glyph_id));
    }
    (jobs, cmap)
}

/// Computes one output glyph.
fn blend_glyph(faces: &[SourceFace<'_>], job: &GlyphJob) -> Result<GlyphData, CompileError> {
    let advances: Vec<f64> = job.sources.iter().map(|&(index, glyph)| faces[index].advance(glyph)).collect();
    let advance: f64 = advances.iter().zip(&job.weights).map(|(advance, weight)| weight * advance).sum();
    let outlines: Vec<Outline> = job.sources.iter().map(|&(index, glyph)| faces[index].outline(glyph)).collect();
    // Shape only from the sources with ink, weights renormalized among them.
    let mut inked = Vec::with_capacity(outlines.len());
    let mut inked_weights = Vec::with_capacity(outlines.len());
    let mut inked_advances = Vec::with_capacity(outlines.len());
    for ((outline, weight), source_advance) in outlines.iter().zip(&job.weights).zip(&advances) {
        if !outline.is_empty() {
            inked.push(outline);
            inked_weights.push(*weight);
            inked_advances.push(*source_advance);
        }
    }
    let ink_total: f64 = inked_weights.iter().sum();
    for weight in &mut inked_weights {
        *weight /= ink_total;
    }
    let blended: Outline = match inked.as_slice() {
        [] => Vec::new(),
        [single] => (*single).clone(),
        _ => match compatible::interpolate(&inked, &inked_weights).or_else(|| matched::interpolate(&inked, &inked_weights, &inked_advances)) {
            Some(outline) => outline,
            None => sdf::blend(&inked, &inked_weights, &inked_advances)
                .map_err(|rejection| CompileError::GlyphTooComplex { ch: job.ch, reason: rejection.to_string() })?,
        },
    };
    let contours = to_truetype(&blended, job.ch)?;
    let advance = metrics::units(advance.max(0.0));
    Ok(GlyphData { advance: u16::try_from(advance).unwrap_or(0), contours })
}

/// `.notdef`: an empty rectangle (outer clockwise, inner counter-clockwise).
fn notdef_glyph() -> GlyphData {
    let rectangle = |x0: f64, y0: f64, x1: f64, y1: f64, clockwise: bool| {
        use kurbo::Point;
        let mut corners = [Point::new(x0, y0), Point::new(x0, y1), Point::new(x1, y1), Point::new(x1, y0)];
        if !clockwise {
            corners.reverse();
        }
        Contour { start: corners[0], segs: corners[1..].iter().map(|&point| Seg::Line(point)).collect() }
    };
    let upm = f64::from(TARGET_UPM);
    let outline = vec![
        rectangle(0.05 * upm, 0.0, 0.45 * upm, 0.7 * upm, true),
        rectangle(0.1 * upm, 0.05 * upm, 0.4 * upm, 0.65 * upm, false),
    ];
    // Fixed geometry well inside the i16 range: conversion cannot fail.
    let contours = to_truetype(&outline, '\0').unwrap_or_default();
    GlyphData { advance: TARGET_UPM / 2, contours }
}
