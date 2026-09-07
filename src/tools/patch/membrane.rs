/*
File: tools/patch/membrane.rs

Purpose:
The gradient-domain (Poisson) maths behind the «Заплатка» tool. GUI-free and I/O-free: buffers
in, buffers out, so the whole thing is unit-testable and runs on a worker thread.

Main responsibilities:
- Solve the seamless-cloning membrane that adapts copied source pixels to the colour and
  luminance of the destination surrounding a free-form selection.
- Produce the per-pixel coverage (feather) ramp the caller turns into overlay alpha.

Key structures:
- `PatchBlend`: which adaptation is applied (none / linear / multiplicative).
- `PatchRequest`: one job, entirely in ROI-local coordinates.
- `PatchResult`: the solved colours plus their coverage.
- `PatchError`: contract violations of the request.

Key functions:
- `solve_patch()`: the entry point.
- `solve_channel()`: the cascadic-multigrid solve of one colour channel.
- `build_coverage()`: the feather ramp.

Notes:
The maths. With `Ω` the selection, `g` the source pixels and `f*` the destination surroundings,
seamless cloning minimizes `∫∫_Ω |∇f − ∇g|²` subject to `f|∂Ω = f*|∂Ω`. Substituting `f = g + u`
turns that into `Δu = 0` inside `Ω` with `u|∂Ω = (f* − g)|∂Ω`: the correction `u` is the harmonic
"membrane" spanned by the boundary difference. Because a harmonic function is low-frequency, the
source's texture (its gradients) survives intact while its colour and luminance are pulled onto
the destination's.

The Laplace solve reuses the project's shared red-black SOR kernel (`crate::tools::sor`); this
module only builds its `lam`/`denom`/`u0` buffers and the multigrid schedule around it. Plain SOR
needs iterations on the order of the region's DIAMETER, so a 1500-px selection would stall a worker for many seconds — the
cascadic pyramid below is what keeps the tool interactive.
*/
use crate::tools::red_black_sor_sweeps;
use rayon::prelude::*;

/// Colour channels solved independently. RGB only; alpha is not part of the membrane.
const CHANNELS: usize = 3;

/// Offset added before the logarithm of `PatchBlend::Multiplicative`, in value units.
///
/// One 8-bit step. It keeps `ln(0)` out of the solve while staying below the quantization
/// floor of the data itself, so it cannot bias a pixel by a representable amount.
const MULTIPLICATIVE_EPS: f32 = 1.0 / 255.0;

/// Data-fidelity weight that pins a cell to its known value (a soft Dirichlet condition).
///
/// The kernel's update is `u[i] += omega * ((Σ4 neighbours + lam*u0[i]) / (4 + lam) − u[i])`, so
/// a pinned cell leaks its neighbours in with relative weight `4 / lam = 4e-6` — three orders of
/// magnitude below one 8-bit step (3.9e-3) and therefore invisible, while staying far inside
/// `f32`'s 7 significant digits.
const DIRICHLET_LAM: f32 = 1.0e6;

/// Over-relaxation factor of the SOR sweeps.
///
/// Just under the `omega < 2` stability limit, matching `gradient.rs`'s own screened-Poisson
/// call sites; the multigrid cascade already removes the low-frequency error, so the sweeps only
/// have to smooth what prolongation left behind.
const SOR_OMEGA: f32 = 1.9;

/// Sweeps run on every level above the coarsest.
///
/// Enough to damp the high-frequency error that bilinear prolongation injects; more would only
/// re-solve what the coarser level already answered.
const SWEEPS_PER_LEVEL: usize = 24;

/// Sweeps run on the coarsest level, which is solved rather than smoothed.
///
/// The coarsest grid is at most `COARSEST_MIN_DIM` on its short side, so plain SOR converges
/// there in tens of sweeps; the generous count costs a few hundred thousand cell updates in
/// total and removes any doubt about the cascade's starting point.
const COARSEST_SWEEPS: usize = 600;

/// Coarsening stops once the SHORTER side of a level reaches this many cells.
const COARSEST_MIN_DIM: usize = 24;

/// Hard cap on pyramid depth, so a pathological aspect ratio cannot build levels forever.
const MAX_PYRAMID_LEVELS: usize = 16;

/// Smallest ROI the solve accepts on either axis.
///
/// The shared kernel updates only the INTERIOR of its grid, so a region without a 1-pixel border
/// around it has nothing to solve.
const MIN_ROI_DIM: usize = 3;

/// How the copied source pixels are adapted to the destination contour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum PatchBlend {
    /// No adaptation: the source pixels are copied verbatim. Photoshop's "Destination"-free
    /// paste, kept because a user sometimes wants exactly the pixels they pointed at.
    None,
    /// The membrane is solved on the value scale. The default: it is what seamless cloning
    /// classically means and it is exact for an additive lighting difference.
    #[default]
    Linear,
    /// The membrane is solved on `ln(value)`, i.e. the correction is a RATIO instead of an
    /// offset. This is the variant that survives dark line art and screentone, where an additive
    /// membrane lifts black strokes toward the destination's paper colour and washes them out.
    Multiplicative,
}

/// One patch job, entirely in ROI-local coordinates.
///
/// Every buffer is `roi_w * roi_h` entries, row-major. The caller owns the mapping back to page
/// pixels; nothing here knows about pages, canvases or overlays.
#[derive(Debug, Clone)]
pub(super) struct PatchRequest {
    pub(super) roi_w: usize,
    pub(super) roi_h: usize,
    /// Composited destination pixels (page + clean overlay) over the whole ROI.
    pub(super) dst: Vec<[u8; 3]>,
    /// The same composite translated by the drag offset: `src[i]` is the pixel that lands on
    /// `dst[i]`.
    pub(super) src: Vec<[u8; 3]>,
    /// Selection interior. MUST be `false` on the 1-pixel ROI border — the solve needs a ring of
    /// known destination values around `Ω`, and the caller is the only party that can pad for it.
    pub(super) mask: Vec<bool>,
    /// Width of the soft ramp at the selection edge, in pixels; `0` is a hard edge.
    ///
    /// A feather wider than the selection can accommodate is CLAMPED to the selection's own
    /// inradius — the deepest city-block distance to a pixel outside it — so the interior still
    /// reaches coverage `1.0`. An oversized feather is a legitimate input with exactly one sane
    /// reading ("as soft as this selection can be"), not an unsupported request.
    pub(super) feather_px: usize,
    pub(super) blend: PatchBlend,
}

/// The solved patch over the whole ROI.
#[derive(Debug, Clone)]
pub(super) struct PatchResult {
    /// The colour each ROI pixel must end up with; equal to `dst` outside the selection.
    pub(super) rgb: Vec<[u8; 3]>,
    /// How much of `rgb` replaces `dst`: `1.0` in the interior, ramping to `0.0` across
    /// `feather_px` at the selection edge, `0.0` outside. Doubles as the overlay alpha floor.
    pub(super) coverage: Vec<f32>,
}

/// A request that violates the solve's contract.
///
/// None of these is recoverable by guessing: the caller guarantees the padding and the buffer
/// lengths, so a violation is a bug on its side and must be reported, not repaired.
#[derive(Debug, thiserror::Error)]
pub(super) enum PatchError {
    /// The ROI has no interior for the kernel to update.
    #[error("patch ROI is {w}x{h}, but the solve needs at least {min}x{min} to have an interior")]
    RoiTooSmall { w: usize, h: usize, min: usize },
    /// A buffer length disagrees with `roi_w * roi_h`.
    #[error("patch buffer `{buffer}` holds {actual} entries, expected {expected}")]
    BufferLength {
        buffer: &'static str,
        expected: usize,
        actual: usize,
    },
    /// The selection reaches the ROI border, so there is no known boundary value beside it.
    #[error("patch mask touches the ROI border at ({x}, {y}); the ROI must be padded around it")]
    MaskTouchesBorder { x: usize, y: usize },
}

/// Solves the seamless-cloning membrane for one patch job.
///
/// Returns the colour every ROI pixel must show and how much of it applies. Outside the mask the
/// colour is `dst` verbatim and the coverage is `0.0`, so a caller can blend the whole ROI
/// uniformly without special-casing the selection.
///
/// An EMPTY mask is not an error: it yields `rgb == dst` and an all-zero coverage, i.e. a no-op.
///
/// The EFFECTIVE feather is clamped to the selection's inradius, measured in the same city-block
/// metric the ramp uses, so the coverage ALWAYS reaches `1.0` somewhere inside a non-empty
/// selection. A `feather_px` wider than the selection can hold therefore yields the widest ramp
/// that still commits the selection fully, instead of a uniformly translucent ghost of it.
///
/// # Errors
/// `PatchError::RoiTooSmall` when either ROI side is below three pixels,
/// `PatchError::BufferLength` when `dst`, `src` or `mask` disagrees with `roi_w * roi_h`, and
/// `PatchError::MaskTouchesBorder` when the selection reaches the ROI border.
pub(super) fn solve_patch(req: &PatchRequest) -> Result<PatchResult, PatchError> {
    validate(req)?;

    let coverage = build_coverage(&req.mask, req.roi_w, req.roi_h, req.feather_px);
    if !req.mask.iter().any(|inside| *inside) {
        // Nothing selected: the destination stays exactly as it is. `coverage` is already zero
        // everywhere, because it is zero outside the mask by construction.
        return Ok(PatchResult {
            rgb: req.dst.clone(),
            coverage,
        });
    }

    let mut rgb = req.dst.clone();
    match req.blend {
        PatchBlend::None => {
            for (out, (inside, source)) in rgb.iter_mut().zip(req.mask.iter().zip(req.src.iter())) {
                if *inside {
                    *out = *source;
                }
            }
        }
        PatchBlend::Linear | PatchBlend::Multiplicative => {
            let log_domain = req.blend == PatchBlend::Multiplicative;
            let levels = build_mask_pyramid(&req.mask, req.roi_w, req.roi_h);
            // The three channels are independent solves over the SAME pyramid, so they are the
            // natural outer parallelism. `red_black_sor_sweeps` parallelizes across rows inside
            // each of them; rayon's work-stealing pool absorbs the nesting.
            let solved: Vec<Vec<u8>> = (0..CHANNELS)
                .into_par_iter()
                .map(|channel| {
                    let src_val: Vec<f32> = req
                        .src
                        .iter()
                        .map(|px| to_value(px[channel], log_domain))
                        .collect();
                    // `u0 = diff` is both the Dirichlet data outside `Ω` and the initial guess
                    // inside it: any harmonic difference is then already the exact answer.
                    let diff: Vec<f32> = req
                        .dst
                        .iter()
                        .zip(src_val.iter())
                        .map(|(px, source)| to_value(px[channel], log_domain) - source)
                        .collect();
                    let membrane = solve_channel(&levels, &diff);
                    src_val
                        .iter()
                        .zip(membrane.iter())
                        .map(|(source, correction)| from_value(source + correction, log_domain))
                        .collect()
                })
                .collect();
            for (idx, out) in rgb.iter_mut().enumerate() {
                if !req.mask[idx] {
                    continue;
                }
                for (channel, plane) in solved.iter().enumerate().take(CHANNELS) {
                    if let Some(value) = plane.get(idx) {
                        out[channel] = *value;
                    }
                }
            }
        }
    }

    Ok(PatchResult { rgb, coverage })
}

/// Checks every contract `solve_patch` relies on before touching a pixel.
fn validate(req: &PatchRequest) -> Result<(), PatchError> {
    if req.roi_w < MIN_ROI_DIM || req.roi_h < MIN_ROI_DIM {
        return Err(PatchError::RoiTooSmall {
            w: req.roi_w,
            h: req.roi_h,
            min: MIN_ROI_DIM,
        });
    }
    let expected = req.roi_w.saturating_mul(req.roi_h);
    for (buffer, actual) in [
        ("dst", req.dst.len()),
        ("src", req.src.len()),
        ("mask", req.mask.len()),
    ] {
        if actual != expected {
            return Err(PatchError::BufferLength {
                buffer,
                expected,
                actual,
            });
        }
    }
    for x in 0..req.roi_w {
        for y in [0, req.roi_h - 1] {
            if req.mask[y * req.roi_w + x] {
                return Err(PatchError::MaskTouchesBorder { x, y });
            }
        }
    }
    for y in 0..req.roi_h {
        for x in [0, req.roi_w - 1] {
            if req.mask[y * req.roi_w + x] {
                return Err(PatchError::MaskTouchesBorder { x, y });
            }
        }
    }
    Ok(())
}

/// A stored 8-bit channel on the scale the membrane is solved in.
///
/// `log_domain` selects `PatchBlend::Multiplicative`'s logarithmic scale, where an additive
/// membrane becomes a multiplicative correction of the source value.
fn to_value(byte: u8, log_domain: bool) -> f32 {
    let value = f32::from(byte) / 255.0;
    if log_domain {
        (value + MULTIPLICATIVE_EPS).ln()
    } else {
        value
    }
}

/// Inverse of `to_value`, quantized back to a stored 8-bit channel.
fn from_value(value: f32, log_domain: bool) -> u8 {
    let linear = if log_domain {
        value.exp() - MULTIPLICATIVE_EPS
    } else {
        value
    };
    // `f32::clamp` PROPAGATES NaN instead of folding it into a bound, so a NaN survives both
    // clamps and the `round` untouched. What makes the conversion total is the CAST: `f32 as u8`
    // saturates in Rust, mapping NaN to `0` and anything above the range to `255`. The clamps
    // exist for the finite range only — in particular to catch `exp` overflowing in the log
    // domain before the multiplication by 255.
    (linear.clamp(0.0, 1.0) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// One level of the mask pyramid, with the solver weights it implies.
///
/// `lam`/`denom` are colour-independent, so they are built once per level and shared by all
/// three channel solves.
#[derive(Debug)]
struct MaskLevel {
    w: usize,
    h: usize,
    mask: Vec<bool>,
    lam: Vec<f32>,
    denom: Vec<f32>,
}

impl MaskLevel {
    /// Wraps a mask into a level, forcing the 1-pixel border outside the domain.
    ///
    /// The shared kernel never writes its border, so a border cell that claimed to be inside
    /// `Ω` would silently freeze at its initial guess instead of being solved.
    fn new(mut mask: Vec<bool>, w: usize, h: usize) -> Self {
        for x in 0..w {
            mask[x] = false;
            mask[(h - 1) * w + x] = false;
        }
        for y in 0..h {
            mask[y * w] = false;
            mask[y * w + (w - 1)] = false;
        }
        let lam: Vec<f32> = mask
            .iter()
            .map(|inside| if *inside { 0.0 } else { DIRICHLET_LAM })
            .collect();
        let denom: Vec<f32> = lam.iter().map(|weight| 4.0 + weight).collect();
        Self {
            w,
            h,
            mask,
            lam,
            denom,
        }
    }
}

/// Builds the mask pyramid, finest level first.
///
/// A coarse cell is inside the domain iff at least half of its (up to four) children are, which
/// keeps a thin selection alive for one or two levels instead of dropping it immediately, and
/// keeps a coarse boundary within half a coarse cell of the fine one.
fn build_mask_pyramid(mask: &[bool], w: usize, h: usize) -> Vec<MaskLevel> {
    let mut levels = vec![MaskLevel::new(mask.to_vec(), w, h)];
    while levels.len() < MAX_PYRAMID_LEVELS {
        let Some(last) = levels.last() else {
            break;
        };
        if last.w.min(last.h) <= COARSEST_MIN_DIM {
            break;
        }
        let cw = last.w.div_ceil(2);
        let ch = last.h.div_ceil(2);
        if cw < MIN_ROI_DIM || ch < MIN_ROI_DIM {
            break;
        }
        let coarse = coarsen_mask(&last.mask, last.w, last.h, cw, ch);
        levels.push(MaskLevel::new(coarse, cw, ch));
    }
    levels
}

/// Downsamples a mask 2x by majority over the children present in the fine grid.
fn coarsen_mask(mask: &[bool], w: usize, h: usize, cw: usize, ch: usize) -> Vec<bool> {
    let mut out = vec![false; cw.saturating_mul(ch)];
    for cy in 0..ch {
        for cx in 0..cw {
            let mut present = 0usize;
            let mut inside = 0usize;
            for dy in 0..2 {
                for dx in 0..2 {
                    let x = cx * 2 + dx;
                    let y = cy * 2 + dy;
                    if x >= w || y >= h {
                        continue;
                    }
                    present += 1;
                    if mask[y * w + x] {
                        inside += 1;
                    }
                }
            }
            // "At least half of the children", written so a clipped edge cell (fewer than four
            // children, on an odd dimension) is judged by the same fraction.
            out[cy * cw + cx] = present > 0 && inside * 2 >= present;
        }
    }
    out
}

/// Downsamples the boundary-difference field 2x, respecting the domain classification.
///
/// A coarse cell OUTSIDE the domain carries Dirichlet DATA, so it averages only the children
/// that are outside too: a straddling cell that also averaged its interior children would pin
/// the coarse boundary to a value the destination never had, and the error would survive every
/// prolongation back down (measured as a 3/255 rim on a 256-px region before this rule existed).
/// A coarse cell INSIDE the domain only carries an initial guess, so a plain average is right.
fn coarsen_field(field: &[f32], finer: &MaskLevel, coarse: &MaskLevel) -> Vec<f32> {
    let mut out = vec![0.0f32; coarse.w.saturating_mul(coarse.h)];
    for cy in 0..coarse.h {
        for cx in 0..coarse.w {
            let coarse_idx = cy * coarse.w + cx;
            let data_only = !coarse.mask[coarse_idx];
            let mut sum = 0.0f32;
            let mut count = 0.0f32;
            let mut fallback_sum = 0.0f32;
            let mut fallback_count = 0.0f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let x = cx * 2 + dx;
                    let y = cy * 2 + dy;
                    if x >= finer.w || y >= finer.h {
                        continue;
                    }
                    let value = field[y * finer.w + x];
                    fallback_sum += value;
                    fallback_count += 1.0;
                    if data_only && finer.mask[y * finer.w + x] {
                        continue;
                    }
                    sum += value;
                    count += 1.0;
                }
            }
            out[coarse_idx] = if count > 0.0 {
                sum / count
            } else if fallback_count > 0.0 {
                // No child of the wanted kind: keep the plain average rather than a hole.
                fallback_sum / fallback_count
            } else {
                0.0
            };
        }
    }
    out
}

/// Bilinearly interpolates a coarse solution onto the finer grid.
///
/// A coarse cell `c` covers the fine cells `2c` and `2c+1`, so its centre sits at fine
/// coordinate `2c + 1`, and a fine cell `x` (centre `x + 0.5`) therefore samples the coarse grid
/// at `p = (x − 0.5) / 2`. Clamping `p` into the coarse grid replicates the coarse border, which
/// is the correct extension: the border holds the Dirichlet data on both grids.
fn prolongate(coarse: &[f32], cw: usize, ch: usize, fw: usize, fh: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; fw.saturating_mul(fh)];
    if cw == 0 || ch == 0 {
        return out;
    }
    // Grid dimensions are image dimensions: far below `f32`'s exact-integer range (2^24).
    let max_cx = (cw - 1) as f32;
    let max_cy = (ch - 1) as f32;
    for y in 0..fh {
        let py = (((y as f32) - 0.5) * 0.5).clamp(0.0, max_cy);
        let y0 = py.floor();
        let ty = py - y0;
        let cy0 = (y0 as usize).min(ch - 1);
        let cy1 = (cy0 + 1).min(ch - 1);
        for x in 0..fw {
            let px = (((x as f32) - 0.5) * 0.5).clamp(0.0, max_cx);
            let x0 = px.floor();
            let tx = px - x0;
            let cx0 = (x0 as usize).min(cw - 1);
            let cx1 = (cx0 + 1).min(cw - 1);
            let v00 = coarse[cy0 * cw + cx0];
            let v10 = coarse[cy0 * cw + cx1];
            let v01 = coarse[cy1 * cw + cx0];
            let v11 = coarse[cy1 * cw + cx1];
            let top = v00 + (v10 - v00) * tx;
            let bottom = v01 + (v11 - v01) * tx;
            out[y * fw + x] = top + (bottom - top) * ty;
        }
    }
    out
}

/// Solves `Δu = 0` inside the mask with `u = diff` outside it, for one colour channel.
///
/// The cascade runs from the coarsest level upward: each level is initialized with the previous
/// (coarser) solution prolongated onto its grid, then smoothed. Only the coarsest level is
/// actually solved from scratch, which is what removes plain SOR's dependence on the region's
/// diameter — the long-range part of the answer is computed where the region is a few dozen
/// cells across.
fn solve_channel(levels: &[MaskLevel], fine_diff: &[f32]) -> Vec<f32> {
    let mut diffs: Vec<Vec<f32>> = Vec::with_capacity(levels.len());
    diffs.push(fine_diff.to_vec());
    for idx in 1..levels.len() {
        let (Some(finer), Some(level), Some(prev_diff)) =
            (levels.get(idx - 1), levels.get(idx), diffs.get(idx - 1))
        else {
            break;
        };
        let coarse = coarsen_field(prev_diff, finer, level);
        diffs.push(coarse);
    }

    let mut solution: Option<Vec<f32>> = None;
    for idx in (0..diffs.len()).rev() {
        let (Some(level), Some(diff)) = (levels.get(idx), diffs.get(idx)) else {
            continue;
        };
        // Start from the Dirichlet data everywhere; the pinned cells must hold it exactly, and
        // inside `Ω` it is the best available guess until a coarser answer overwrites it.
        let mut u = diff.clone();
        if let (Some(coarse_u), Some(coarser)) = (solution.as_ref(), levels.get(idx + 1)) {
            let guess = prolongate(coarse_u, coarser.w, coarser.h, level.w, level.h);
            for (cell, (inside, value)) in u.iter_mut().zip(level.mask.iter().zip(guess.iter())) {
                if *inside {
                    *cell = *value;
                }
            }
        }
        let iters = if idx + 1 == diffs.len() {
            COARSEST_SWEEPS
        } else {
            SWEEPS_PER_LEVEL
        };
        red_black_sor_sweeps(
            &mut u,
            diff,
            &level.lam,
            &level.denom,
            level.w,
            level.h,
            iters,
            SOR_OMEGA,
        );
        solution = Some(u);
    }
    solution.unwrap_or_else(|| fine_diff.to_vec())
}

/// Builds the feather ramp: how much of the solved colour replaces the destination.
///
/// `1.0` on mask pixels at least `feather` pixels deep, smoothly down to `0.0` on the outermost
/// mask ring, and `0.0` outside. The depth is the city-block distance to the nearest pixel
/// outside the mask — numerically the number of 4-neighbour erosions a pixel survives, obtained
/// in two passes instead of `feather` sweeps over the whole ROI. The ramp is `smoothstep`, hence
/// monotone in depth, and it never touches the solve: coverage is applied afterwards.
///
/// `feather` is an upper bound, not a promise: it is clamped to the selection's inradius in that
/// same metric (`max(depth) − 1`), so the deepest pixel always lands exactly on the ramp's `1.0`
/// end. Without the clamp an oversized feather would cap the whole selection far below `1.0` —
/// feather 10 on a six-pixel-wide selection topped out at 0.104, a patch the user cannot see.
fn build_coverage(mask: &[bool], w: usize, h: usize, feather: usize) -> Vec<f32> {
    let mut coverage = vec![0.0f32; mask.len()];
    if feather == 0 {
        for (out, inside) in coverage.iter_mut().zip(mask.iter()) {
            if *inside {
                *out = 1.0;
            }
        }
        return coverage;
    }

    // Depth is capped one past the ramp's end, so the buffer stays small and the passes cheap.
    let cap = u32::try_from(feather.saturating_add(1)).unwrap_or(u32::MAX);
    let mut depth = vec![0u32; mask.len()];
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            if !mask[idx] {
                continue;
            }
            let mut best = cap;
            if x > 0 {
                best = best.min(depth[idx - 1].saturating_add(1));
            }
            if y > 0 {
                best = best.min(depth[idx - w].saturating_add(1));
            }
            depth[idx] = best;
        }
    }
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let idx = y * w + x;
            if !mask[idx] {
                continue;
            }
            let mut best = depth[idx];
            if x + 1 < w {
                best = best.min(depth[idx + 1].saturating_add(1));
            }
            if y + 1 < h {
                best = best.min(depth[idx + w].saturating_add(1));
            }
            depth[idx] = best;
        }
    }

    // Clamp the ramp to what this selection can carry. `depth` saturates at `feather + 1`, which
    // is exactly the information needed: a selection deeper than the ramp reports the cap and
    // leaves `feather` untouched, a shallower one reports its true inradius and shortens the ramp
    // so that its deepest pixel still reaches coverage 1.0.
    let deepest = depth.iter().copied().max().unwrap_or(0);
    let effective = usize::try_from(deepest.saturating_sub(1))
        .unwrap_or(usize::MAX)
        .min(feather);
    if effective == 0 {
        // The selection is one pixel thick in this metric (or empty): there is nothing to ramp
        // across, so it commits as a hard edge rather than as a patch that is invisible
        // everywhere. `depth` is `0` outside the mask, which keeps those pixels uncovered.
        for (out, value) in coverage.iter_mut().zip(depth.iter()) {
            if *value > 0 {
                *out = 1.0;
            }
        }
        return coverage;
    }

    // `effective` is bounded by `feather`, a UI-bounded pixel count, and `depth` is capped by it,
    // so both stay far inside `f32`'s exact-integer range.
    let span = effective as f32;
    for (out, value) in coverage.iter_mut().zip(depth.iter()) {
        // A pixel of depth 1 is the outermost mask ring and sits at the ramp's zero end.
        let inside_depth = value.saturating_sub(1) as f32;
        let t = (inside_depth / span).clamp(0.0, 1.0);
        *out = t * t * (3.0 - 2.0 * t);
    }
    coverage
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROI whose mask is the axis-aligned rectangle `[x0, x1) x [y0, y1)`.
    fn rect_mask(w: usize, h: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<bool> {
        let mut mask = vec![false; w * h];
        for y in y0..y1 {
            for x in x0..x1 {
                mask[y * w + x] = true;
            }
        }
        mask
    }

    fn gray(value: u8) -> [u8; 3] {
        [value, value, value]
    }

    fn request(
        w: usize,
        h: usize,
        dst: Vec<[u8; 3]>,
        src: Vec<[u8; 3]>,
        mask: Vec<bool>,
        blend: PatchBlend,
    ) -> PatchRequest {
        PatchRequest {
            roi_w: w,
            roi_h: h,
            dst,
            src,
            mask,
            feather_px: 0,
            blend,
        }
    }

    /// `PatchBlend::None` is a verbatim copy: the source inside the selection, the destination
    /// everywhere else. Nothing is solved and nothing is adapted.
    #[test]
    fn blend_none_copies_source_inside_and_destination_outside() {
        let (w, h) = (16, 16);
        let dst = vec![gray(200); w * h];
        let src = vec![gray(40); w * h];
        let mask = rect_mask(w, h, 4, 4, 12, 12);
        let result = solve_patch(&request(w, h, dst, src, mask.clone(), PatchBlend::None))
            .expect("a padded 16x16 request is valid");
        for (idx, inside) in mask.iter().enumerate() {
            let expected = if *inside { gray(40) } else { gray(200) };
            assert_eq!(result.rgb[idx], expected, "pixel {idx}");
        }
    }

    /// A CONSTANT boundary difference is reproduced exactly: a patch over a flat background is
    /// invisible. `u = k` is harmonic, so the initial guess is already the answer — what this
    /// pins is that the pyramid and the prolongation do not DESTROY it.
    #[test]
    fn constant_boundary_difference_is_reproduced() {
        let (w, h) = (40, 40);
        let src = vec![gray(90); w * h];
        let dst = vec![gray(150); w * h];
        let mask = rect_mask(w, h, 6, 6, 34, 34);
        let result = solve_patch(&request(w, h, dst, src, mask.clone(), PatchBlend::Linear))
            .expect("a padded 40x40 request is valid");
        for (idx, inside) in mask.iter().enumerate() {
            if *inside {
                let value = i32::from(result.rgb[idx][0]);
                assert!((value - 150).abs() <= 2, "pixel {idx} solved to {value}, expected 150");
            }
        }
    }

    /// A LINEAR boundary difference is reproduced: linear functions are harmonic, so an exact
    /// solver must return them. This is the real convergence test — a broken coarsening,
    /// prolongation or Dirichlet weight shows up here as a visible gradient error.
    #[test]
    fn linear_boundary_difference_is_reproduced() {
        let (w, h) = (48, 48);
        let src = vec![gray(60); w * h];
        let mut dst = vec![gray(0); w * h];
        for y in 0..h {
            for x in 0..w {
                // `60 + 1.5x + 0.5y` stays inside `0..=255` over the whole ROI.
                let value = 60.0 + 1.5 * (x as f32) + 0.5 * (y as f32);
                let byte = value.round().clamp(0.0, 255.0) as u8;
                dst[y * w + x] = gray(byte);
            }
        }
        let mask = rect_mask(w, h, 8, 8, 40, 40);
        let result = solve_patch(&request(w, h, dst.clone(), src, mask.clone(), PatchBlend::Linear))
            .expect("a padded 48x48 request is valid");
        for (idx, inside) in mask.iter().enumerate() {
            if *inside {
                let got = i32::from(result.rgb[idx][0]);
                let want = i32::from(dst[idx][0]);
                assert!((got - want).abs() <= 2, "pixel {idx} solved to {got}, expected {want}");
            }
        }
    }

    /// The source's TEXTURE survives while its overall level is pulled onto the destination's.
    ///
    /// The interior gradients must match the source's (the membrane is low-frequency), the mean
    /// level inside the selection must match the destination's (the membrane carries the offset),
    /// and outside the selection nothing is touched. A pixel-exact match to `dst` at the boundary
    /// is deliberately NOT asserted: the texture is supposed to survive there too.
    #[test]
    fn texture_survives_while_level_is_adapted() {
        let (w, h) = (48, 48);
        let mut src = vec![gray(0); w * h];
        for y in 0..h {
            for x in 0..w {
                // Period-4 checkerboard around 130, so nothing clamps once 70 is added.
                let high = ((x / 2) + (y / 2)) % 2 == 0;
                src[y * w + x] = gray(if high { 160 } else { 100 });
            }
        }
        let dst = vec![gray(200); w * h];
        let mask = rect_mask(w, h, 8, 8, 40, 40);
        let result = solve_patch(&request(w, h, dst.clone(), src.clone(), mask.clone(), PatchBlend::Linear))
            .expect("a padded 48x48 request is valid");

        // Deep interior: a harmonic membrane with period-4 boundary data decays as
        // exp(-2*pi*d/4), i.e. below 1e-4 by six pixels in, so the gradients must be the
        // source's to well within one 8-bit step.
        for y in 16..32 {
            for x in 16..32 {
                let idx = y * w + x;
                let got = i32::from(result.rgb[idx][0]) - i32::from(result.rgb[idx + 1][0]);
                let want = i32::from(src[idx][0]) - i32::from(src[idx + 1][0]);
                assert!(
                    (got - want).abs() <= 2,
                    "gradient at ({x},{y}) is {got}, source has {want}"
                );
            }
        }

        let (sum, count) = mask.iter().enumerate().fold((0i64, 0i64), |(sum, count), (idx, inside)| {
            if *inside {
                (sum + i64::from(result.rgb[idx][0]), count + 1)
            } else {
                (sum, count)
            }
        });
        let mean = sum / count.max(1);
        assert!((mean - 200).abs() <= 2, "mean level inside the patch is {mean}, expected 200");

        for (idx, inside) in mask.iter().enumerate() {
            if !*inside {
                assert_eq!(result.rgb[idx], dst[idx], "pixel {idx} outside the mask was touched");
            }
        }
    }

    /// `PatchBlend::Multiplicative` reproduces a RATIO between source and destination, which is
    /// what keeps dark line art dark instead of lifting it toward the paper colour.
    #[test]
    fn multiplicative_blend_reproduces_a_ratio() {
        let (w, h) = (40, 40);
        let mut src = vec![gray(0); w * h];
        for y in 0..h {
            for x in 0..w {
                src[y * w + x] = gray(if (x + y) % 2 == 0 { 100 } else { 180 });
            }
        }
        // dst = src * 1.2, still inside 0..=255 for both texture levels.
        let dst: Vec<[u8; 3]> = src
            .iter()
            .map(|px| gray((f32::from(px[0]) * 1.2).round().clamp(0.0, 255.0) as u8))
            .collect();
        let mask = rect_mask(w, h, 6, 6, 34, 34);
        let result =
            solve_patch(&request(w, h, dst.clone(), src, mask.clone(), PatchBlend::Multiplicative))
                .expect("a padded 40x40 request is valid");
        for (idx, inside) in mask.iter().enumerate() {
            if *inside {
                let got = i32::from(result.rgb[idx][0]);
                let want = i32::from(dst[idx][0]);
                assert!((got - want).abs() <= 3, "pixel {idx} solved to {got}, expected {want}");
            }
        }
    }

    /// An empty selection is a no-op, not an error.
    #[test]
    fn empty_mask_is_a_no_op() {
        let (w, h) = (8, 8);
        let dst = vec![gray(70); w * h];
        let src = vec![gray(210); w * h];
        let result = solve_patch(&request(w, h, dst.clone(), src, vec![false; w * h], PatchBlend::Linear))
            .expect("an empty mask is valid");
        assert_eq!(result.rgb, dst);
        assert!(result.coverage.iter().all(|value| *value == 0.0));
    }

    /// Every `PatchError` variant is reachable and answers with an error instead of panicking.
    #[test]
    fn contract_violations_are_reported_not_panicked() {
        let tiny = request(2, 2, vec![gray(0); 4], vec![gray(0); 4], vec![false; 4], PatchBlend::Linear);
        assert!(matches!(solve_patch(&tiny), Err(PatchError::RoiTooSmall { .. })));

        let short = request(8, 8, vec![gray(0); 8 * 8], vec![gray(0); 7], vec![false; 8 * 8], PatchBlend::Linear);
        assert!(matches!(
            solve_patch(&short),
            Err(PatchError::BufferLength { buffer: "src", .. })
        ));

        let mut border = vec![false; 8 * 8];
        border[0] = true;
        let touching = request(8, 8, vec![gray(0); 8 * 8], vec![gray(0); 8 * 8], border, PatchBlend::Linear);
        assert!(matches!(
            solve_patch(&touching),
            Err(PatchError::MaskTouchesBorder { x: 0, y: 0 })
        ));
    }

    /// The feather ramp: full coverage deep inside, none outside, monotone across the ramp.
    #[test]
    fn feather_coverage_is_monotone_and_bounded() {
        let (w, h) = (32, 32);
        let mask = rect_mask(w, h, 8, 8, 24, 24);
        let mut req = request(w, h, vec![gray(120); w * h], vec![gray(120); w * h], mask, PatchBlend::None);
        req.feather_px = 4;
        let result = solve_patch(&req).expect("a padded 32x32 request is valid");

        let row = 16 * w;
        assert_eq!(result.coverage[row + 7], 0.0, "outside the mask must not be covered");
        assert_eq!(result.coverage[row + 8], 0.0, "the outermost mask ring is the ramp's zero end");
        assert_eq!(result.coverage[row + 16], 1.0, "the centre must be fully covered");
        for x in 8..16 {
            let here = result.coverage[row + x];
            let next = result.coverage[row + x + 1];
            assert!(next >= here, "coverage must not fall inward: {here} then {next} at x={x}");
        }
    }

    /// A LARGE region whose selection interior starts at the wrong value still converges under
    /// the shipped schedule.
    ///
    /// `diff` is `0` inside the selection and `C` outside, so the answer (`u = C` everywhere
    /// inside) has to travel across the whole region — the case plain SOR needs iterations on
    /// the order of the diameter for, and the reason the cascadic pyramid exists.
    #[test]
    fn large_region_converges_under_the_shipped_schedule() {
        let (w, h) = (256, 256);
        let mask = rect_mask(w, h, 4, 4, 252, 252);
        let src = vec![gray(100); w * h];
        let dst: Vec<[u8; 3]> = mask
            .iter()
            .map(|inside| if *inside { gray(100) } else { gray(160) })
            .collect();
        let result = solve_patch(&request(w, h, dst, src, mask.clone(), PatchBlend::Linear))
            .expect("a padded 256x256 request is valid");
        for (idx, inside) in mask.iter().enumerate() {
            if *inside {
                let value = i32::from(result.rgb[idx][0]);
                assert!(
                    (value - 160).abs() <= 2,
                    "pixel {idx} solved to {value}, expected the harmonic extension 160"
                );
            }
        }
    }

    /// The FINE levels of the cascade really solve; they are not just prolongating.
    ///
    /// Every other solver test above has a CONSTANT or LINEAR membrane, and the cascade
    /// reproduces those exactly with no fine-level sweep at all: 2x2 averaging commutes with the
    /// 5-point Laplacian, and bilinear prolongation is exact on them. A `SWEEPS_PER_LEVEL = 0`
    /// build passes all of them with zero error while being off by 6.28/255 on step data, so on
    /// its own the suite above cannot tell a working fine level from a dead one. The discrete
    /// saddle `x^2 - y^2` does not fix that either (measured: 0.51/255 with no fine sweeps
    /// against 0.59/255 with the shipped schedule) — nor does ANY globally discrete-harmonic
    /// field, because `solve_channel` seeds `u` with `diff` in the INTERIOR too, which makes such
    /// a field a fixed point of the sweeps before a single one has run.
    ///
    /// This probe therefore breaks both halves of that. The interior data is destroyed
    /// (`dst == src` inside the selection, so `diff` is `0` there and the answer has to be
    /// carried inward from the ring), and the ring carries
    ///
    /// ```text
    /// u(x, y) = A * cos(θ (x − cx)) * cosh(k (y − cy)),   with   2 cos θ + 2 cosh k = 4
    /// ```
    ///
    /// which that dispersion relation makes EXACTLY discrete-harmonic under the 5-point
    /// Laplacian — its four-neighbour sum equals four times its centre value, to `f32` rounding,
    /// which the test asserts before relying on it — so the exact answer is known in closed form.
    /// Unlike a polynomial it is a period-8 oscillation inside an exponential boundary layer:
    /// structure a grid of twice the spacing cannot represent, so the coarse levels cannot supply
    /// it and only fine-level smoothing can. Measured worst interior error: 0.59/255 under the
    /// shipped schedule, 14.25/255 with `SWEEPS_PER_LEVEL = 0`.
    #[test]
    fn fine_level_sweeps_reproduce_a_discrete_harmonic_boundary_layer() {
        let (w, h) = (96, 64);
        // An integer x-centre lets the cosine reach ±1 exactly; the y-centre is the ROI's own, so
        // the layer peaks on the ROI's top and bottom rows and `norm` is the peak of `cosh`,
        // which is what keeps `dst` inside `0..=255` without any clamping.
        let cx = 48.0f32;
        let cy = (h as f32 - 1.0) / 2.0;
        let theta = std::f32::consts::FRAC_PI_4;
        let cosh_k = 2.0 - theta.cos();
        let k = (cosh_k + (cosh_k * cosh_k - 1.0).sqrt()).ln();
        let norm = (k * cy).cosh();
        let amplitude = 110.0f32;
        let exact = |x: usize, y: usize| -> f32 {
            amplitude * (theta * (x as f32 - cx)).cos() * (k * (y as f32 - cy)).cosh() / norm
        };

        // The premise the assertions rest on: the closed form is a fixed point of the kernel's
        // stencil, so it IS the exact discrete solution for its own boundary values.
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let neighbours = exact(x - 1, y) + exact(x + 1, y) + exact(x, y - 1) + exact(x, y + 1);
                let residual = (neighbours - 4.0 * exact(x, y)).abs();
                assert!(residual < 1.0e-3, "closed form is not discrete-harmonic at ({x},{y}): residual {residual}");
            }
        }

        let mask = rect_mask(w, h, 2, 2, w - 2, h - 2);
        let src = vec![gray(128); w * h];
        let dst: Vec<[u8; 3]> = (0..w * h)
            .map(|idx| {
                if mask[idx] {
                    // Inside: no information at all, so the cascade cannot pass the test by
                    // keeping its own initial guess.
                    gray(128)
                } else {
                    gray((128.0 + exact(idx % w, idx / w)).round().clamp(0.0, 255.0) as u8)
                }
            })
            .collect();
        let result = solve_patch(&request(w, h, dst, src, mask.clone(), PatchBlend::Linear))
            .expect("a padded 96x64 request is valid");

        let mut worst = 0.0f32;
        for (idx, inside) in mask.iter().enumerate() {
            if *inside {
                let got = f32::from(result.rgb[idx][0]);
                let want = 128.0 + exact(idx % w, idx / w);
                worst = worst.max((got - want).abs());
            }
        }
        // One 8-bit step of budget: half of it is the output rounding, and up to half again the
        // quantization of the boundary ring, whose harmonic extension is bounded by it. Measured
        // 0.59; a dead fine level lands at 14.25.
        assert!(worst <= 1.0, "worst interior error is {worst}/255, expected the closed form to within one step");
    }

    /// A feather wider than the selection can hold is clamped to the selection's inradius, so a
    /// narrow patch still commits fully instead of applying as a translucent ghost of itself.
    #[test]
    fn oversized_feather_is_clamped_to_the_selection_inradius() {
        let (w, h) = (32, 32);
        // Six pixels wide: the city-block inradius is 3, so the ramp is clamped from 12 down to
        // 2 and the two innermost columns still reach full coverage.
        let mask = rect_mask(w, h, 13, 4, 19, 28);
        let mut req = request(w, h, vec![gray(120); w * h], vec![gray(120); w * h], mask.clone(), PatchBlend::None);
        req.feather_px = 12;
        let result = solve_patch(&req).expect("a padded 32x32 request is valid");

        let row = 16 * w;
        assert_eq!(result.coverage[row + 12], 0.0, "outside the mask must not be covered");
        assert_eq!(result.coverage[row + 13], 0.0, "the outermost mask ring is the ramp's zero end");
        assert_eq!(result.coverage[row + 14], 0.5, "the clamped ramp's midpoint");
        assert_eq!(result.coverage[row + 15], 1.0, "the deepest column must reach full coverage");
        assert_eq!(result.coverage[row + 16], 1.0, "the deepest column must reach full coverage");
        for x in 13..16 {
            let here = result.coverage[row + x];
            let next = result.coverage[row + x + 1];
            assert!(next >= here, "coverage must not fall inward: {here} then {next} at x={x}");
        }
        for (idx, inside) in mask.iter().enumerate() {
            if !*inside {
                assert_eq!(result.coverage[idx], 0.0, "pixel {idx} outside the mask must stay uncovered");
            }
        }
    }

    /// A feather the selection CAN hold is untouched by the clamp: the ramp keeps its full width
    /// and every step still matches the unclamped `smoothstep` closed form.
    #[test]
    fn feather_within_the_selection_is_not_clamped() {
        let (w, h) = (32, 32);
        // Sixteen pixels wide: inradius 8, twice the requested ramp, so the clamp is inert.
        let mask = rect_mask(w, h, 8, 8, 24, 24);
        let mut req = request(w, h, vec![gray(120); w * h], vec![gray(120); w * h], mask, PatchBlend::None);
        req.feather_px = 4;
        let result = solve_patch(&req).expect("a padded 32x32 request is valid");

        let row = 16 * w;
        for depth in 1..=5usize {
            // On this row the city-block depth of column `7 + depth` is `depth`.
            let t = ((depth - 1) as f32) / 4.0;
            let want = t * t * (3.0 - 2.0 * t);
            let got = result.coverage[row + 7 + depth];
            assert!((got - want).abs() < 1.0e-6, "depth {depth} covered {got}, expected the full-width ramp's {want}");
        }
        assert_eq!(result.coverage[row + 16], 1.0, "the centre must be fully covered");
    }
}
