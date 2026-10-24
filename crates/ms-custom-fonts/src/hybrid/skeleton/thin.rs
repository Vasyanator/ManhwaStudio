/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/thin.rs

Purpose:
Raster thinning of one source glyph's inside mask to a one-pixel, 8-connected medial skeleton.

Key functions:
- `skeleton_pixels`: Zhang–Suen thinning, staircase cleanup, restoration of vanished blobs.
- `neighbour`: the shared 8-neighbourhood addressing (order N, NE, E, SE, S, SW, W, NW).

Notes:
- Zhang–Suen ("A fast parallel algorithm for thinning digital patterns", CACM 1984): two
  sub-iterations, each deciding every candidate on the image as it was at the sub-iteration's
  start, so the result is independent of visiting order. Only border pixels are candidates; a
  deleted pixel's foreground neighbours join the candidates, so total work is proportional to
  the ink, not to iterations x grid. Capped at `nx + ny` iterations.
- Cleanup: one raster pass deleting simple points (Yokoi connectivity number 1) that have at
  least two neighbours, which removes the redundant corner pixels of staircases so that every
  path pixel of the result has exactly two neighbours.
- Zhang–Suen deletes 2x2 blocks entirely (a known property), which can erase a small dot; an
  8-connected ink component left without any skeleton pixel gets its most interior pixel back.
*/

/// `(di, dj)` of the 8 neighbours in the order N, NE, E, SE, S, SW, W, NW (y up).
const OFFSETS: [(isize, isize); 8] = [(0, 1), (1, 1), (1, 0), (1, -1), (0, -1), (-1, -1), (-1, 0), (-1, 1)];

/// Slot of neighbour `k` (`OFFSETS` order) of node `slot` in an `nx` x `ny` raster; `None`
/// outside it.
pub(super) fn neighbour(slot: usize, k: usize, nx: usize, ny: usize) -> Option<usize> {
    let (di, dj) = OFFSETS[k % 8];
    let i = (slot % nx).checked_add_signed(di)?;
    let j = (slot / nx).checked_add_signed(dj)?;
    (i < nx && j < ny).then_some(j * nx + i)
}

/// The 8 neighbour flags of `slot` (outside the raster counts as background).
fn ring(image: &[bool], slot: usize, nx: usize, ny: usize) -> [bool; 8] {
    let mut flags = [false; 8];
    for (k, flag) in flags.iter_mut().enumerate() {
        *flag = neighbour(slot, k, nx, ny).is_some_and(|other| image[other]);
    }
    flags
}

/// Skeleton of the `nx` x `ny` inside mask (row-major, `inside.len() == nx * ny`).
pub(super) fn skeleton_pixels(inside: &[bool], nx: usize, ny: usize) -> Vec<bool> {
    let mut image = inside.to_vec();
    zhang_suen(&mut image, nx, ny);
    remove_staircase_corners(&mut image, nx, ny);
    restore_vanished_components(inside, &mut image, nx, ny);
    image
}

/// Zhang–Suen deletion test of sub-iteration `second` (false = first) for a pixel with
/// neighbours `around` (N, NE, E, SE, S, SW, W, NW).
fn deletable(around: [bool; 8], second: bool) -> bool {
    let count = around.iter().filter(|&&flag| flag).count();
    let transitions = (0..8).filter(|&position| !around[position] && around[(position + 1) % 8]).count();
    let (north, east, south, west) = (around[0], around[2], around[4], around[6]);
    // Second: not (N·E·W) and not (N·S·W); first: not (N·E·S) and not (E·S·W).
    let directional = if second { !(north && west && (east || south)) } else { !(east && south && (north || west)) };
    (2..=6).contains(&count) && transitions == 1 && directional
}

/// Zhang–Suen thinning in place.
fn zhang_suen(image: &mut [bool], nx: usize, ny: usize) {
    let has_background_neighbour = |image: &[bool], slot: usize| ring(image, slot, nx, ny).iter().any(|&flag| !flag);
    let mut candidates: Vec<usize> = (0..image.len()).filter(|&slot| image[slot] && has_background_neighbour(image, slot)).collect();
    let mut queued = vec![false; image.len()];
    for &slot in &candidates {
        queued[slot] = true;
    }
    for _ in 0..nx.saturating_add(ny) {
        let mut deleted_any = false;
        for second in [false, true] {
            let deletions: Vec<usize> =
                candidates.iter().copied().filter(|&slot| image[slot] && deletable(ring(image, slot, nx, ny), second)).collect();
            if deletions.is_empty() {
                continue;
            }
            deleted_any = true;
            for &slot in &deletions {
                image[slot] = false;
            }
            candidates.retain(|&slot| image[slot]);
            for &slot in &deletions {
                for k in 0..8 {
                    if let Some(other) = neighbour(slot, k, nx, ny)
                        && image[other]
                        && !queued[other]
                    {
                        queued[other] = true;
                        candidates.push(other);
                    }
                }
            }
        }
        if !deleted_any {
            break;
        }
    }
}

/// Yokoi 8-connectivity number of a pixel with neighbours `p` (N, NE, E, SE, S, SW, W, NW):
/// 1 for a simple border point, whose deletion keeps the topology.
fn connectivity_number(p: [bool; 8]) -> usize {
    let empty = |k: usize| !p[k % 8];
    [0_usize, 2, 4, 6].iter().filter(|&&k| empty(k) && !(empty(k + 1) && empty(k + 2))).count()
}

/// One raster pass deleting simple points with two or more neighbours (staircase corners).
fn remove_staircase_corners(image: &mut [bool], nx: usize, ny: usize) {
    for slot in 0..image.len() {
        if !image[slot] {
            continue;
        }
        let p = ring(image, slot, nx, ny);
        if p.iter().filter(|&&flag| flag).count() >= 2 && connectivity_number(p) == 1 {
            image[slot] = false;
        }
    }
}

/// Gives every 8-connected ink component without a skeleton pixel its most interior pixel
/// (most ink neighbours; first in scan order on ties).
fn restore_vanished_components(inside: &[bool], image: &mut [bool], nx: usize, ny: usize) {
    let mut seen = vec![false; inside.len()];
    let mut stack = Vec::new();
    for seed in 0..inside.len() {
        if !inside[seed] || seen[seed] {
            continue;
        }
        seen[seed] = true;
        stack.push(seed);
        let mut has_skeleton = false;
        let mut best: Option<(usize, usize)> = None;
        while let Some(slot) = stack.pop() {
            has_skeleton |= image[slot];
            let interior = ring(inside, slot, nx, ny).iter().filter(|&&flag| flag).count();
            if best.is_none_or(|(best_interior, best_slot)| interior > best_interior || (interior == best_interior && slot < best_slot)) {
                best = Some((interior, slot));
            }
            for k in 0..8 {
                if let Some(other) = neighbour(slot, k, nx, ny)
                    && inside[other]
                    && !seen[other]
                {
                    seen[other] = true;
                    stack.push(other);
                }
            }
        }
        if !has_skeleton && let Some((_, slot)) = best {
            image[slot] = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(rows: &[&str]) -> (Vec<bool>, usize, usize) {
        // Rows are given top-down; the raster is y-up.
        let ny = rows.len();
        let nx = rows[0].len();
        let mut image = vec![false; nx * ny];
        for (row, text) in rows.iter().enumerate() {
            for (i, ch) in text.chars().enumerate() {
                image[(ny - 1 - row) * nx + i] = ch == '#';
            }
        }
        (image, nx, ny)
    }

    #[test]
    fn a_bar_thins_to_a_one_pixel_line() {
        let (image, nx, ny) = raster(&[
            "..............",
            ".############.",
            ".############.",
            ".############.",
            ".############.",
            ".############.",
            "..............",
        ]);
        let skeleton = skeleton_pixels(&image, nx, ny);
        let pixels: Vec<usize> = (0..skeleton.len()).filter(|&slot| skeleton[slot]).collect();
        assert_ne!(pixels, [] as [usize; 0]);
        // Every skeleton pixel has at most two neighbours and they all sit on the middle rows.
        for &slot in &pixels {
            let neighbours = ring(&skeleton, slot, nx, ny).iter().filter(|&&flag| flag).count();
            assert!(neighbours <= 2, "slot {slot}");
            assert!((2..=4).contains(&(slot / nx)), "slot {slot}");
        }
    }

    #[test]
    fn a_two_by_two_dot_keeps_one_pixel() {
        let (image, nx, ny) = raster(&["....", ".##.", ".##.", "...."]);
        let skeleton = skeleton_pixels(&image, nx, ny);
        assert_eq!(skeleton.iter().filter(|&&flag| flag).count(), 1);
    }

    #[test]
    fn a_ring_keeps_its_hole() {
        let (image, nx, ny) = raster(&[
            "..........",
            ".########.",
            ".########.",
            ".##....##.",
            ".##....##.",
            ".########.",
            ".########.",
            "..........",
        ]);
        let skeleton = skeleton_pixels(&image, nx, ny);
        // The hole's centre stays background and is still enclosed: every skeleton pixel has
        // exactly two neighbours (a closed loop).
        let pixels: Vec<usize> = (0..skeleton.len()).filter(|&slot| skeleton[slot]).collect();
        assert!(pixels.len() >= 8);
        for &slot in &pixels {
            assert_eq!(ring(&skeleton, slot, nx, ny).iter().filter(|&&flag| flag).count(), 2, "slot {slot}");
        }
    }
}
