/*
File: tools/sor.rs

Purpose:
The shared red-black SOR (successive over-relaxation) kernel of the project. It is the ONE
Laplacian / screened-Poisson solver: a tool that needs a harmonic or screened-Poisson solve calls
this instead of copying it.

Main responsibilities:
- Run `iters` red-black SOR iterations over the interior of a padded ROI, in place.

Key functions:
- `red_black_sor_sweeps()`.

Notes:
Consumers are the cleaning tab's gradient fill (screened-Poisson L-channel consolidation) and the
patch tool's membrane solve (`tools/patch/membrane.rs`, `lam == 0` inside the selection). Both are
re-verified together when this kernel changes.
*/
use rayon::prelude::*;

/// Runs `iters` red-black SOR iterations over the interior of an `rw`×`rh` ROI in place.
///
/// Each iteration performs two half-sweeps (red then black). A half-sweep updates only the
/// cells of one color, where a cell's color is `(x + y) & 1`. The 5-point Laplacian stencil
/// reads only the 4 axis neighbors, which all have the opposite color, so within one half-sweep
/// no updated cell is read by another updated cell. The updates within a half-sweep are therefore
/// mutually independent and order-invariant, which makes a row-parallel sweep numerically
/// identical to the sequential one.
///
/// `u` is the working buffer (interior updated in place), `u0` the data-fidelity reference,
/// `lam` the per-cell fidelity weight, `denom` the precomputed `4 + lam[i]`. The two half-sweeps
/// stay sequential w.r.t. each other; parallelism is only within a half-sweep, across rows.
///
/// This is the SHARED Laplacian/SOR kernel of the whole project (`crate::tools`), not a
/// gradient-only helper: any tool needing a screened-Poisson or harmonic solve calls this one
/// instead of copying it. The per-cell update is
/// `u[i] += omega * ((Σ4 neighbours + lam[i]*u0[i]) / denom[i] - u[i])` with `denom[i] == 4 + lam[i]`,
/// so `lam[i] == 0` reduces it to the plain harmonic average of the four neighbours (a free,
/// membrane-like cell), while a large `lam[i]` pins cell `i` to `u0[i]` — a soft Dirichlet
/// condition, exact in the limit. A harmonic membrane is therefore obtained by setting `lam` to
/// `0` inside the region and to a large value on the boundary band that holds the known values.
///
/// Only the INTERIOR of the `rw`×`rh` grid is updated: the 1-pixel border is never written and
/// acts as fixed data. A caller must pad its region by at least one pixel (and `rw`/`rh` must be
/// at least 3, otherwise there is no interior and the call is a no-op).
// All parameters are distinct solver buffers or ROI dimensions; grouping would obscure the kernel.
#[allow(clippy::too_many_arguments)]
pub fn red_black_sor_sweeps(
    u: &mut [f32],
    u0: &[f32],
    lam: &[f32],
    denom: &[f32],
    rw: usize,
    rh: usize,
    iters: usize,
    omega: f32,
) {
    // Scratch holds the newly computed value for each updated cell of the current half-sweep.
    // Because updated cells are never read within the same half-sweep, computing into scratch
    // from the immutable snapshot `u` and copying back yields the exact in-place result while
    // letting rows be processed in parallel without aliasing the writes.
    //
    // The buffer is allocated once and deliberately NOT re-zeroed between half-sweeps or
    // iterations: cells of the parity NOT being updated this half-sweep retain stale values from
    // a previous half-sweep, but the copyback below reads only current-parity cells (it recomputes
    // the exact same parity-selection predicate as the compute loop), so those stale cells are
    // never read. Do not add a stale read of `scratch` outside that predicate.
    let mut scratch = vec![0.0f32; u.len()];
    for _ in 0..iters {
        for parity in 0..=1usize {
            // Compute updates for every interior cell of this color in parallel by row. Each row's
            // updated cells write only into `scratch[row]`; reads of `u` touch opposite-color
            // cells (neighbors) that are not updated this half-sweep, so the shared `&u` borrow is
            // race-free. The stride between consecutive rows in the flat buffer is `rw`.
            scratch
                .par_chunks_mut(rw)
                .enumerate()
                .skip(1)
                .take(rh.saturating_sub(2))
                .for_each(|(y, scratch_row)| {
                    let xstart = 1 + ((parity ^ (y & 1)) & 1);
                    let row_base = y * rw;
                    for x in (xstart..(rw - 1)).step_by(2) {
                        let i = row_base + x;
                        let nbr = u[i - 1] + u[i + 1] + u[i - rw] + u[i + rw];
                        let rhs = nbr + lam[i] * u0[i];
                        let next = rhs / denom[i];
                        scratch_row[x] = u[i] + omega * (next - u[i]);
                    }
                });

            // Apply the half-sweep results back into `u`. Only the cells of `parity` were written
            // in `scratch`; recompute the same membership to copy exactly those cells.
            for y in 1..(rh - 1) {
                let xstart = 1 + ((parity ^ (y & 1)) & 1);
                let row_base = y * rw;
                for x in (xstart..(rw - 1)).step_by(2) {
                    let i = row_base + x;
                    u[i] = scratch[i];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flat row-major index of `(x, y)` in a buffer of width `w`.
    #[inline]
    fn idx2d(x: usize, y: usize, w: usize) -> usize {
        y.saturating_mul(w).saturating_add(x)
    }

    /// Reference sequential red-black SOR, copied verbatim from the pre-parallel implementation.
    /// Used as the golden baseline that the parallel `red_black_sor_sweeps` must reproduce.
    // Mirrors the production kernel signature so the comparison is one-to-one.
    #[allow(clippy::too_many_arguments)]
    fn sequential_sor_reference(
        u: &mut [f32],
        u0: &[f32],
        lam: &[f32],
        denom: &[f32],
        rw: usize,
        rh: usize,
        iters: usize,
        omega: f32,
    ) {
        if rw < 3 || rh < 3 {
            return;
        }
        for _ in 0..iters {
            for parity in 0..=1usize {
                for y in 1..(rh - 1) {
                    let xstart = 1 + ((parity ^ (y & 1)) & 1);
                    for x in (xstart..(rw - 1)).step_by(2) {
                        let i = idx2d(x, y, rw);
                        let nbr = u[idx2d(x - 1, y, rw)]
                            + u[idx2d(x + 1, y, rw)]
                            + u[idx2d(x, y - 1, rw)]
                            + u[idx2d(x, y + 1, rw)];
                        let rhs = nbr + lam[i] * u0[i];
                        let next = rhs / denom[i];
                        u[i] = u[i] + omega * (next - u[i]);
                    }
                }
            }
        }
    }

    /// Builds a deterministic, mildly varied SOR fixture (interior masked, fidelity weights).
    fn build_sor_fixture(rw: usize, rh: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let n = rw * rh;
        let mut u0 = vec![0.0f32; n];
        let mut lam = vec![0.0f32; n];
        for y in 0..rh {
            for x in 0..rw {
                let i = idx2d(x, y, rw);
                // Smooth-ish initial field plus a deterministic ripple.
                u0[i] =
                    (x as f32) * 0.37 - (y as f32) * 0.21 + ((x * 7 + y * 13) % 11) as f32 * 0.05;
                // Mark an interior block as "inside the mask" with weak fidelity, rest strong.
                let inside = x >= rw / 4 && x < 3 * rw / 4 && y >= rh / 4 && y < 3 * rh / 4;
                lam[i] = if inside { 1.0 } else { 120.0 };
            }
        }
        let mut denom = vec![0.0f32; n];
        for i in 0..n {
            denom[i] = 4.0 + lam[i];
        }
        (u0, lam, denom)
    }

    /// Golden test: parallel red-black SOR must equal the sequential baseline bit-for-bit.
    ///
    /// The arithmetic per cell is identical and order-independent within a half-sweep, so the
    /// results should match exactly. The tolerance is kept explicit and tight to catch any
    /// stencil/index regression rather than to paper over reordering error.
    #[test]
    fn parallel_sor_matches_sequential() {
        // Bit-exact equality is expected: the parallel and sequential paths apply the exact same
        // per-cell arithmetic in the same operation order (one thread owns each cell, no float
        // reordering across the parallel split), so identical f32 results are guaranteed. Any
        // nonzero diff would signal a real stencil/index regression, not reordering noise.
        const TOL: f32 = 0.0;
        // (3, 3) exercises the minimum interior: a single interior cell at (1, 1), so the parallel
        // `take(rh - 2)` row range and the sequential reference must agree at the smallest ROI.
        for &(rw, rh) in &[(3usize, 3usize), (5, 4), (17, 23), (40, 9), (33, 33)] {
            let (u0, lam, denom) = build_sor_fixture(rw, rh);
            let iters = 64usize;
            let omega = 1.95f32;

            let mut u_seq = u0.clone();
            sequential_sor_reference(&mut u_seq, &u0, &lam, &denom, rw, rh, iters, omega);

            let mut u_par = u0.clone();
            red_black_sor_sweeps(&mut u_par, &u0, &lam, &denom, rw, rh, iters, omega);

            assert_eq!(u_seq.len(), u_par.len());
            for (i, (a, b)) in u_seq.iter().zip(u_par.iter()).enumerate() {
                assert!(
                    (a - b).abs() <= TOL,
                    "SOR mismatch at {i} ({rw}x{rh}): seq={a} par={b}",
                );
            }
        }
    }
}
