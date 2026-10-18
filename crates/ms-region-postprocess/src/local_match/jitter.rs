/*
File: local_match/jitter.rs

Purpose:
The seeded per-block sample positions of the local match's coarse cascade levels: ONE pixel per
`stride x stride` block, at an offset hashed from (level, block row, block col) with splitmix64.

Key functions:
- `splitmix64()`: the splitmix64 finalizer (wrapping u64).
- `jittered_sample_indices()`: the row / column index grids of one cascade level.

Notes:
Deliberate deviation from the reference (`region_imaging.py` at `c11c4eb`,
`_jittered_sample_indices`), which draws the offsets from numpy's `default_rng(level)` (PCG64 +
SeedSequence + buffered bounded integers, not reproducible cheaply). The parity fixtures are
generated with the reference patched to this exact rule (`tools/make_region_postprocess_fixtures.py`),
and `fixtures/jitter.json` pins its known answers. Point samples keep every sample a real pixel;
the per-block jitter keeps a periodic pattern whose period divides the stride from aliasing.
*/

/// splitmix64 finalizer of `value`: `z = x + 0x9E3779B97F4A7C15`, two xor-shift-multiply rounds,
/// a final xor-shift; all arithmetic wrapping.
#[must_use]
pub(super) const fn splitmix64(value: u64) -> u64 {
    let mut z = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Sample positions of one cascade level over a `padded_height x padded_width` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SampleGrid {
    /// Blocks per column: `ceil(padded_height / stride)`.
    pub(super) rows: usize,
    /// Blocks per row: `ceil(padded_width / stride)`.
    pub(super) cols: usize,
    /// Row-major `rows x cols` source row of each block's sample.
    pub(super) row_index: Vec<usize>,
    /// Row-major `rows x cols` source column of each block's sample.
    pub(super) col_index: Vec<usize>,
}

/// One sample per `stride x stride` block (`stride >= 1`) of a `padded_height x padded_width`
/// field (both >= 1), cascade level `level`:
/// `key = (level << 48) ^ (block_row << 24) ^ block_col`, `h = splitmix64(key)`,
/// `row = min(block_row·stride + (h & 0xFFFF_FFFF) % stride, padded_height - 1)`,
/// `col = min(block_col·stride + (h >> 32) % stride, padded_width - 1)`.
#[must_use]
pub(super) fn jittered_sample_indices(padded_height: usize, padded_width: usize, stride: usize, level: u32) -> SampleGrid {
    let rows = padded_height.div_ceil(stride);
    let cols = padded_width.div_ceil(stride);
    let stride_u64 = stride as u64;
    let mut row_index = Vec::with_capacity(rows * cols);
    let mut col_index = Vec::with_capacity(rows * cols);
    for block_row in 0..rows {
        for block_col in 0..cols {
            let key = (u64::from(level) << 48) ^ ((block_row as u64) << 24) ^ (block_col as u64);
            let hash = splitmix64(key);
            // Both offsets are `< stride`, which is a `usize`, so the conversions cannot fail.
            let row_offset = usize::try_from((hash & 0xFFFF_FFFF) % stride_u64).unwrap_or(stride - 1);
            let col_offset = usize::try_from((hash >> 32) % stride_u64).unwrap_or(stride - 1);
            row_index.push((block_row * stride + row_offset).min(padded_height - 1));
            col_index.push((block_col * stride + col_offset).min(padded_width - 1));
        }
    }
    SampleGrid { rows, cols, row_index, col_index }
}
