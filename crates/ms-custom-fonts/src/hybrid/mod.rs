/*
File: crates/ms-custom-fonts/src/hybrid/mod.rs

Purpose:
Compiler of the hybrid kind: recipe + source fonts -> TrueType face whose glyphs, metrics and
kerning are weighted averages of the sources.

Key structures:
- `GlyphJob`: one output glyph — the source glyphs shaping it and their renormalized shape
  and spacing weights.
- `GlyphAlgorithm`: the recipe's `BlendAlgorithm` resolved for the glyph loop (transfer with
  its font-wide targets).
- `BlendPath`: which outline path produced a glyph (summed into `BlendStats`).

Key functions:
- `compile`: the whole pipeline (match sources, plan characters, blend glyphs in parallel,
  blend metrics and kerning, name the face, write it).
- `coverage`: the character plan's report only (cmap work, no outlines).
- `included_chars`: the character plan's included set over given candidates (cmap work only).
- `blend_glyph`: one glyph — the per-algorithm dispatch (see the table below).

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
- Weights (`HybridParams::effective_weights`): shape weights drive the outline (and the
  `NoBlend` winner / transfer base substitute), spacing weights the per-glyph advance and the
  font metrics, kerning weights the inherited pairs.
- Dispatch for two or more inked sources (0 inked: empty; 1 inked: copied under every
  algorithm except Transfer, which transfers it like any other glyph):
  Auto = compatible -> matched -> sdf; Sdf = sdf; NoBlend = the inked source with the highest
  shape weight (ties: earliest in recipe order) copied; Skeleton = compatible -> skeleton ->
  sdf on `SkeletonFallback`; Transfer = `transfer::apply` on the base source's glyph, or, when
  the base has no ink for it, on the inked source with the highest shape weight (with that
  source's parameters, counted as substituted).
*/

pub(crate) mod compatible;
pub(crate) mod kerning;
pub(crate) mod matched;
pub(crate) mod metrics;
pub(crate) mod plan;
pub(crate) mod sdf;
pub(crate) mod skeleton;
#[cfg(test)]
mod tests;
pub(crate) mod trace;
pub(crate) mod transfer;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;
use ttf_parser::GlyphId;

use crate::error::CompileError;
use crate::key::{face_unique_id, recipe_key, source_digest, subset_tag};
use crate::outline::{Contour, Outline, Seg, TARGET_UPM, to_truetype};
use crate::recipe::{BlendAlgorithm, CustomFontRecipe, HybridParams};
use crate::sfnt::{FontBuild, FontNames, GlyphData, write_font};
use crate::source::{SourceFace, SourceFont};
use crate::{BlendStats, CompileScope, CompiledFont, CoverageReport};

/// Glyph jobs per parallel chunk (progress granularity).
const CHUNK_GLYPHS: usize = 64;

/// One output glyph to compute.
#[derive(Debug)]
struct GlyphJob {
    /// First character mapped to the glyph (error context).
    ch: char,
    /// `(source index, source glyph)` of every participating source, in recipe order.
    sources: Vec<(usize, GlyphId)>,
    /// Shape weights of `sources`, normalized to sum 1 (renormalized over the inked ones in
    /// `blend_glyph`).
    shape_weights: Vec<f64>,
    /// Spacing (advance) weights of `sources`, normalized to sum 1.
    spacing_weights: Vec<f64>,
}

/// The recipe's algorithm as the glyph loop runs it.
#[derive(Debug, Clone, Copy)]
enum GlyphAlgorithm<'a> {
    /// `BlendAlgorithm::Auto`.
    Auto,
    /// `BlendAlgorithm::Sdf`.
    Sdf,
    /// `BlendAlgorithm::NoBlend`.
    NoBlend,
    /// `BlendAlgorithm::Skeleton`.
    Skeleton,
    /// `BlendAlgorithm::Transfer` with its font-wide targets (measured once per compile).
    Transfer(&'a transfer::Targets),
}

/// Which outline path produced one glyph (one exclusive `BlendStats` count each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlendPath {
    /// No ink, or ink in one source under any algorithm but `Transfer`.
    Copied,
    /// Exact point interpolation.
    Compatible,
    /// Contour-correspondence interpolation.
    Matched,
    /// Distance-field blend (Auto's last resort or forced).
    Sdf,
    /// Copied from the heaviest inked source (`NoBlend`).
    NoBlend,
    /// Stroke-skeleton blend.
    Skeleton,
    /// Skeleton gave up (reason key); blended by distance field.
    SkeletonFallback(&'static str),
    /// Parameter transfer; `base_substituted` when the base font had no ink for the glyph.
    Transfer {
        /// The glyph was transferred from another source than the recipe's base.
        base_substituted: bool,
    },
}

impl BlendStats {
    /// Counts one glyph produced by `path`.
    fn record(&mut self, path: BlendPath) {
        match path {
            BlendPath::Copied => self.copied += 1,
            BlendPath::Compatible => self.compatible += 1,
            BlendPath::Matched => self.matched += 1,
            BlendPath::Sdf => self.sdf += 1,
            BlendPath::NoBlend => self.no_blend += 1,
            BlendPath::Skeleton => self.skeleton += 1,
            BlendPath::SkeletonFallback(reason) => {
                self.skeleton_fallback += 1;
                *self.skeleton_fallback_reasons.entry(reason).or_insert(0) += 1;
            }
            BlendPath::Transfer { base_substituted } => {
                self.transfer += 1;
                self.transfer_base_substituted += usize::from(base_substituted);
            }
        }
    }
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

    let weights = params.effective_weights();
    let (jobs, cmap) = glyph_jobs(&faces, &plan.included, &weights.shape, &weights.spacing);
    let glyph_count = jobs.len() + 1;
    if glyph_count > usize::from(u16::MAX) {
        return Err(CompileError::TooManyGlyphs { count: glyph_count });
    }

    // Font-wide transfer inputs are measured once, before the parallel loop, from fixed
    // reference glyphs of each face (independent of the scope, so a preview equals the face).
    let transfer_targets: transfer::Targets;
    let algorithm = match params.algorithm {
        BlendAlgorithm::Auto => GlyphAlgorithm::Auto,
        BlendAlgorithm::Sdf => GlyphAlgorithm::Sdf,
        BlendAlgorithm::NoBlend => GlyphAlgorithm::NoBlend,
        BlendAlgorithm::Skeleton => GlyphAlgorithm::Skeleton,
        BlendAlgorithm::Transfer { base_source } => {
            transfer_targets = transfer::targets(base_source, faces.iter().map(transfer::measure).collect(), &weights.shape);
            GlyphAlgorithm::Transfer(&transfer_targets)
        }
    };

    let mut glyphs = Vec::with_capacity(glyph_count);
    let mut stats = BlendStats::default();
    glyphs.push(notdef_glyph());
    let total = jobs.len();
    progress(0, total);
    for chunk in jobs.chunks(CHUNK_GLYPHS) {
        if cancel.load(Ordering::Relaxed) {
            return Err(CompileError::Cancelled);
        }
        let results: Vec<Result<(GlyphData, BlendPath), CompileError>> = chunk
            .par_iter()
            .map(|job| {
                if cancel.load(Ordering::Relaxed) {
                    return Err(CompileError::Cancelled);
                }
                blend_glyph(&faces, job, algorithm)
            })
            .collect();
        for result in results {
            let (glyph, path) = result?;
            glyphs.push(glyph);
            stats.record(path);
        }
        progress(glyphs.len() - 1, total);
    }

    let normalized = |raw: &[f64]| -> Vec<f64> {
        let sum: f64 = raw.iter().sum();
        raw.iter().map(|weight| weight / sum).collect()
    };
    let font_metrics = metrics::blend_metrics(&faces.iter().map(SourceFace::metrics).collect::<Vec<_>>(), &normalized(&weights.spacing));

    let cmap_map: BTreeMap<char, u16> = cmap.iter().copied().collect();
    let source_chars: Vec<BTreeSet<char>> = (0..faces.len())
        .map(|index| plan.included.iter().filter(|(_, mask)| mask & plan::bit(index) != 0).map(|(ch, _)| *ch).collect())
        .collect();
    let source_pairs: Vec<BTreeMap<(char, char), f64>> =
        faces.iter().zip(&source_chars).map(|(face, chars)| face.effective_pairs(chars)).collect();
    if cancel.load(Ordering::Relaxed) {
        return Err(CompileError::Cancelled);
    }
    let kern_pairs = kerning::blend_pairs(&source_pairs, &source_chars, &normalized(&weights.kerning), &cmap_map);

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
    Ok(CompiledFont { bytes, family_name, postscript_name, key, report: plan.report, stats })
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
/// and the cmap `(character, output glyph)`. `shape_weights` / `spacing_weights` are the raw
/// per-source weights (`EffectiveWeights`), normalized here over each job's participants.
fn glyph_jobs(
    faces: &[SourceFace<'_>],
    included: &[(char, u8)],
    shape_weights: &[f64],
    spacing_weights: &[f64],
) -> (Vec<GlyphJob>, Vec<(char, u16)>) {
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
            let normalize = |raw: &[f64]| -> Vec<f64> {
                let total: f64 = participants.iter().map(|&(index, _)| raw[index]).sum();
                participants.iter().map(|&(index, _)| raw[index] / total).collect()
            };
            jobs.push(GlyphJob { ch, shape_weights: normalize(shape_weights), spacing_weights: normalize(spacing_weights), sources: participants });
            by_sources.insert(identity, next);
            next
        };
        cmap.push((ch, glyph_id));
    }
    (jobs, cmap)
}

/// Computes one output glyph and the path that produced it (dispatch table in the file
/// header). Pure per glyph: no state shared with other glyphs.
fn blend_glyph(faces: &[SourceFace<'_>], job: &GlyphJob, algorithm: GlyphAlgorithm<'_>) -> Result<(GlyphData, BlendPath), CompileError> {
    let advances: Vec<f64> = job.sources.iter().map(|&(index, glyph)| faces[index].advance(glyph)).collect();
    let advance: f64 = advances.iter().zip(&job.spacing_weights).map(|(advance, weight)| weight * advance).sum();
    let outlines: Vec<Outline> = job.sources.iter().map(|&(index, glyph)| faces[index].outline(glyph)).collect();
    // Shape only from the sources with ink, weights renormalized among them.
    let mut inked = Vec::with_capacity(outlines.len());
    let mut inked_sources = Vec::with_capacity(outlines.len());
    let mut inked_weights = Vec::with_capacity(outlines.len());
    let mut inked_advances = Vec::with_capacity(outlines.len());
    for (((outline, weight), source_advance), &(source_index, _)) in outlines.iter().zip(&job.shape_weights).zip(&advances).zip(&job.sources) {
        if !outline.is_empty() {
            inked.push(outline);
            inked_sources.push(source_index);
            inked_weights.push(*weight);
            inked_advances.push(*source_advance);
        }
    }
    let ink_total: f64 = inked_weights.iter().sum();
    for weight in &mut inked_weights {
        *weight /= ink_total;
    }
    let too_complex = |reason: String| CompileError::GlyphTooComplex { ch: job.ch, reason };
    let sdf_blend = || sdf::blend(&inked, &inked_weights, &inked_advances).map_err(|rejection| too_complex(rejection.to_string()));
    let (blended, path): (Outline, BlendPath) = match (inked.as_slice(), algorithm) {
        ([], _) => (Vec::new(), BlendPath::Copied),
        // Transfer parameters are font-wide: a glyph only one source has is transferred too.
        (_, GlyphAlgorithm::Transfer(targets)) => {
            let (position, base_substituted) = match inked_sources.iter().position(|&source| source == targets.base) {
                Some(position) => (position, false),
                None => (heaviest(&inked_weights), true),
            };
            // `base_params` holds one entry per source and `inked_sources` are source
            // indices, so the lookup is in range.
            let base_params = &targets.base_params[inked_sources[position]];
            let outline = transfer::apply(inked[position], base_params, &targets.target).map_err(|rejection| too_complex(rejection.to_string()))?;
            (outline, BlendPath::Transfer { base_substituted })
        }
        ([single], GlyphAlgorithm::Auto | GlyphAlgorithm::Sdf | GlyphAlgorithm::NoBlend | GlyphAlgorithm::Skeleton) => ((*single).clone(), BlendPath::Copied),
        (_, GlyphAlgorithm::Auto) => {
            if let Some(outline) = compatible::interpolate(&inked, &inked_weights) {
                (outline, BlendPath::Compatible)
            } else if let Some(outline) = matched::interpolate(&inked, &inked_weights, &inked_advances) {
                (outline, BlendPath::Matched)
            } else {
                (sdf_blend()?, BlendPath::Sdf)
            }
        }
        (_, GlyphAlgorithm::Sdf) => (sdf_blend()?, BlendPath::Sdf),
        (_, GlyphAlgorithm::NoBlend) => (inked[heaviest(&inked_weights)].clone(), BlendPath::NoBlend),
        (_, GlyphAlgorithm::Skeleton) => {
            if let Some(outline) = compatible::interpolate(&inked, &inked_weights) {
                (outline, BlendPath::Compatible)
            } else {
                match skeleton::blend(&inked, &inked_weights, &inked_advances) {
                    Ok(outline) => (outline, BlendPath::Skeleton),
                    Err(fallback) => (sdf_blend()?, BlendPath::SkeletonFallback(fallback.reason())),
                }
            }
        }
    };
    let contours = to_truetype(&blended, job.ch)?;
    let advance = metrics::units(advance.max(0.0));
    Ok((GlyphData { advance: u16::try_from(advance).unwrap_or(0), contours }, path))
}

/// Position of the largest weight; ties go to the earliest position (recipe order, since a
/// job's sources are in recipe order). 0 for an empty slice (callers pass two or more).
fn heaviest(weights: &[f64]) -> usize {
    let mut best = 0;
    for (position, weight) in weights.iter().enumerate().skip(1) {
        if *weight > weights[best] {
            best = position;
        }
    }
    best
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
