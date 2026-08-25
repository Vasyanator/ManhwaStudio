/*
File: tabs/page_manager/split_layout.rs

Purpose:
GUI-free core of the "split page" feature (Layer 2 of
`dev-docs/split_page_plan.md`): where the cut lines sit along the cut axis, the
parts they produce, how the user's chosen page order is kept consistent while
lines are added and removed, and the validation that mirrors the engine's own
`PageOpKind::Split` contract. Contains no egui code and performs no I/O, so
every rule here is unit-testable.

Key structures:
- SplitPart: one resulting part as an origin/size pair along the cut axis.
- SplitLayoutError: why a set of cuts or an order is not a legal engine request.

Key functions:
- parts(): cut coordinates -> the parts they produce.
- validate(): the engine's own preconditions, checked before the confirm button.
- insert_cut() / remove_cut(): cut edits that keep `order` a permutation AND
  keep the user's relative ordering of the untouched parts.
- swap_positions(): the order widget's SWAP semantics.
- clamp_cut() / suggest_cut(): drag bounds and the "add a line" default.
- page_number_for_position(): the real 1-based page number a page position gets.

Notes:
The module is axis-agnostic: everything is expressed along ONE axis, as an
`extent` in SOURCE pixels. The window maps `SplitAxis::Horizontal` to the page
height and `SplitAxis::Vertical` to the page width, so the same math serves both
orientations. Coordinates are ALWAYS source pixels — the board's preview is
downscaled and its resolution never reaches this module.
*/

/// Why a set of cuts, or a part order, is not a legal `PageOpKind::Split`.
///
/// Every variant is a refusal, never a silently corrected value: the window
/// turns it into a disabled confirm button plus a localized message, and the
/// engine would reject the same request with its own validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum SplitLayoutError {
    /// The page is shorter than two pixels along the cut axis, so no cut can
    /// leave a part on both sides of it.
    #[error("the page is {extent} px along the cut axis, too small to cut")]
    PageTooSmall { extent: u32 },
    /// No cut line has been placed yet.
    #[error("the split has no cut lines")]
    NoCuts,
    /// A cut sits on or outside a page edge, which would produce an empty part.
    #[error("cut #{index} at {value} px is not strictly inside a page of {extent} px")]
    CutOutsidePage { index: usize, value: u32, extent: u32 },
    /// Two cuts share a coordinate or are out of order, which would produce a
    /// zero-height part.
    #[error("cut #{index} does not lie strictly after the previous cut")]
    CutsNotIncreasing { index: usize },
    /// `order` is not a bijection onto `0..parts`.
    #[error("the part order is not a permutation of 0..{parts}")]
    OrderNotPermutation { parts: usize },
}

/// One resulting part, as a half-open interval `[origin, origin + size)` along
/// the cut axis, in SOURCE pixels. The other axis always spans the whole page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SplitPart {
    /// Offset of the part's first pixel from the page's top/left edge.
    pub origin: u32,
    /// Length of the part along the cut axis; always `>= 1` for a part this
    /// module produces.
    pub size: u32,
}

/// Splits an axis of `extent` pixels at `cuts` and returns the resulting parts,
/// top-to-bottom (or left-to-right) in GEOMETRIC order.
///
/// Total by construction: a cut that is not strictly inside the page, or that
/// does not lie strictly after the previous accepted cut, is IGNORED, so the
/// returned parts are always non-empty and contiguous. Callers that need such a
/// cut to be an error must run [`validate`] first — the window does, so the
/// confirm button never enables on a set of cuts this function silently
/// repaired. Returns an empty vector only for `extent == 0`.
#[must_use]
pub(super) fn parts(extent: u32, cuts: &[u32]) -> Vec<SplitPart> {
    if extent == 0 {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(cuts.len() + 1);
    let mut start = 0_u32;
    for &cut in cuts {
        if cut <= start || cut >= extent {
            continue;
        }
        result.push(SplitPart {
            origin: start,
            size: cut - start,
        });
        start = cut;
    }
    result.push(SplitPart {
        origin: start,
        size: extent - start,
    });
    result
}

/// Number of parts `cuts` produces on a page that accepts them: `cuts + 1`.
#[must_use]
pub(super) fn part_count(cuts: &[u32]) -> usize {
    cuts.len() + 1
}

/// Checks the preconditions of `PageOpKind::Split` against a page of `extent`
/// pixels along the cut axis.
///
/// The single validation gate of the window: the confirm button is enabled
/// exactly when this succeeds. It deliberately mirrors the engine's contract
/// (`page_ops::PageOpKind::Split`) so the dialog can never emit a request the
/// engine then refuses.
///
/// # Errors
/// [`SplitLayoutError::PageTooSmall`], [`SplitLayoutError::NoCuts`],
/// [`SplitLayoutError::CutOutsidePage`], [`SplitLayoutError::CutsNotIncreasing`]
/// or [`SplitLayoutError::OrderNotPermutation`], in that order of checking.
pub(super) fn validate(
    extent: u32,
    cuts: &[u32],
    order: &[usize],
) -> Result<(), SplitLayoutError> {
    if extent < 2 {
        return Err(SplitLayoutError::PageTooSmall { extent });
    }
    if cuts.is_empty() {
        return Err(SplitLayoutError::NoCuts);
    }
    // `previous` starts at 0, the page's own leading edge: a cut at 0 is not
    // "strictly after the previous cut" either, but it is reported as the more
    // specific out-of-page error by the check above it.
    let mut previous = 0_u32;
    for (index, &value) in cuts.iter().enumerate() {
        if value == 0 || value >= extent {
            return Err(SplitLayoutError::CutOutsidePage {
                index,
                value,
                extent,
            });
        }
        if value <= previous {
            return Err(SplitLayoutError::CutsNotIncreasing { index });
        }
        previous = value;
    }
    // Strictly increasing cuts strictly inside the page already guarantee that
    // every part is at least 1 px, so the engine's minimum-part rule needs no
    // separate check here.
    let count = part_count(cuts);
    if !is_permutation(order, count) {
        return Err(SplitLayoutError::OrderNotPermutation { parts: count });
    }
    Ok(())
}

/// Whether `order` is a bijection from `0..len` onto `0..len`.
#[must_use]
fn is_permutation(order: &[usize], len: usize) -> bool {
    if order.len() != len {
        return false;
    }
    let mut seen = vec![false; len];
    for &position in order {
        match seen.get_mut(position) {
            Some(slot) if !*slot => *slot = true,
            // Out of range, or already taken by another part.
            Some(_) | None => return false,
        }
    }
    true
}

/// The default order: geometric order, i.e. top-to-bottom for horizontal cuts
/// and LEFT-TO-RIGHT for vertical cuts (a user decision recorded in
/// `dev-docs/split_page_plan.md`). Both are the identity permutation, because
/// part 0 is by definition the topmost / leftmost part.
#[must_use]
pub(super) fn default_order(parts: usize) -> Vec<usize> {
    (0..parts).collect()
}

/// Gives `part` the new page position `position`, SWAPPING it with whichever
/// part currently holds that position.
///
/// A no-op when `part` already holds `position`, when `part` is out of range, or
/// when no part holds `position` (which cannot happen for a valid permutation).
/// Swapping — rather than shifting — is the window's specified semantics: the
/// order widget of a part shows the position it takes, and picking an occupied
/// position exchanges the two parts.
pub(super) fn swap_positions(order: &mut [usize], part: usize, position: usize) {
    let Some(&current) = order.get(part) else {
        return;
    };
    if current == position {
        return;
    }
    let Some(other) = order.iter().position(|held| *held == position) else {
        return;
    };
    order.swap(part, other);
}

/// Inserts a cut at `value` and extends `order` so it stays a permutation.
///
/// The geometric part the cut falls into is split in two; the NEW (lower/right)
/// half takes the page position right after the part it was cut from, and every
/// later position shifts down by one. The relative order the user chose for
/// every other part is therefore preserved.
///
/// Returns the index of the inserted cut, or `None` when `value` is not
/// strictly inside the page or a cut already sits there. `order` must be a
/// permutation of `0..parts(cuts)`; on `None` neither argument is modified.
pub(super) fn insert_cut(
    extent: u32,
    cuts: &mut Vec<u32>,
    order: &mut Vec<usize>,
    value: u32,
) -> Option<usize> {
    if value == 0 || value >= extent {
        return None;
    }
    if order.len() != part_count(cuts) {
        return None;
    }
    let index = match cuts.binary_search(&value) {
        // A cut already sits exactly here: adding a second one would produce a
        // zero-sized part.
        Ok(_) => return None,
        Err(index) => index,
    };
    // Cut `index` splits geometric part `index` into parts `index` (the part
    // that keeps the cut's upper side) and `index + 1`.
    let split_position = *order.get(index)?;
    let mut next = vec![0_usize; order.len() + 1];
    for (part, &position) in order.iter().enumerate() {
        let destination = if part <= index { part } else { part + 1 };
        // Everything after the split part's own position makes room for the new
        // part, which lands immediately behind it.
        let shifted = if position > split_position {
            position + 1
        } else {
            position
        };
        next[destination] = shifted;
    }
    next[index + 1] = split_position + 1;
    cuts.insert(index, value);
    *order = next;
    Some(index)
}

/// Removes cut `index`, merging the two parts it separated back into one.
///
/// The merged part keeps the EARLIER of the two page positions and the later one
/// disappears, every position after it shifting up by one, so the remaining
/// parts keep their relative order. Does nothing when `index` is out of range or
/// `order` is not sized for `cuts`.
pub(super) fn remove_cut(cuts: &mut Vec<u32>, order: &mut Vec<usize>, index: usize) {
    if index >= cuts.len() || order.len() != part_count(cuts) {
        return;
    }
    let (Some(&first), Some(&second)) = (order.get(index), order.get(index + 1)) else {
        return;
    };
    let kept = first.min(second);
    let dropped = first.max(second);
    let mut next = Vec::with_capacity(order.len() - 1);
    for (part, &position) in order.iter().enumerate() {
        if part == index + 1 {
            continue;
        }
        let position = if part == index { kept } else { position };
        // `kept < dropped`, so the merged part's own position is never shifted.
        next.push(if position > dropped {
            position - 1
        } else {
            position
        });
    }
    cuts.remove(index);
    *order = next;
}

/// Clamps a dragged cut coordinate so it stays strictly inside the page and
/// strictly between its neighbouring cuts, leaving at least 1 px on each side.
///
/// `index` is the cut being dragged; the other cuts are assumed sorted and
/// valid. For a page smaller than 2 px the bounds collapse and the lower bound
/// is returned, which [`validate`] then refuses as [`SplitLayoutError::PageTooSmall`].
#[must_use]
pub(super) fn clamp_cut(extent: u32, cuts: &[u32], index: usize, value: u32) -> u32 {
    let lower = index
        .checked_sub(1)
        .and_then(|previous| cuts.get(previous))
        .map_or(1, |previous| previous.saturating_add(1))
        .max(1);
    let upper = cuts
        .get(index + 1)
        .map_or(extent.saturating_sub(1), |next| next.saturating_sub(1))
        .min(extent.saturating_sub(1));
    value.clamp(lower, upper.max(lower))
}

/// Coordinate for a new cut placed by the "add a line" button: the middle of the
/// largest part that is at least 2 px long (ties resolve to the first such
/// part). `None` when no part can be cut any further.
#[must_use]
pub(super) fn suggest_cut(extent: u32, cuts: &[u32]) -> Option<u32> {
    parts(extent, cuts)
        .into_iter()
        .filter(|part| part.size >= 2)
        .max_by_key(|part| part.size)
        .map(|part| part.origin + part.size / 2)
}

/// The real 1-based page number a part gets by taking page position `position`.
///
/// The split replaces page `page_idx` with its parts at indices
/// `page_idx .. page_idx + parts`, so the part holding position `p` lands at
/// index `page_idx + p`. This is what the order picker shows in parentheses:
/// for geometric part `k` the current number is
/// `page_number_for_position(page_idx, order[k])`.
#[must_use]
pub(super) fn page_number_for_position(page_idx: usize, position: usize) -> usize {
    page_idx.saturating_add(position).saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes(extent: u32, cuts: &[u32]) -> Vec<(u32, u32)> {
        parts(extent, cuts)
            .into_iter()
            .map(|part| (part.origin, part.size))
            .collect()
    }

    #[test]
    fn parts_split_the_axis_into_contiguous_pieces() {
        assert_eq!(sizes(100, &[]), vec![(0, 100)]);
        assert_eq!(sizes(100, &[30]), vec![(0, 30), (30, 70)]);
        assert_eq!(sizes(100, &[30, 60]), vec![(0, 30), (30, 30), (60, 40)]);
        // A 1 px page cannot be cut, but still is one part.
        assert_eq!(sizes(1, &[]), vec![(0, 1)]);
        assert_eq!(sizes(0, &[]), Vec::new());
    }

    #[test]
    fn parts_ignores_cuts_that_would_produce_an_empty_piece() {
        // On the edges and beyond, duplicated, and out of order.
        assert_eq!(sizes(100, &[0, 100, 150]), vec![(0, 100)]);
        assert_eq!(sizes(100, &[40, 40]), vec![(0, 40), (40, 60)]);
        assert_eq!(sizes(100, &[60, 30]), vec![(0, 60), (60, 40)]);
    }

    #[test]
    fn validate_accepts_a_legal_request() {
        assert_eq!(validate(100, &[30, 60], &[0, 1, 2]), Ok(()));
        // Any permutation is legal, not only the identity.
        assert_eq!(validate(100, &[30, 60], &[2, 0, 1]), Ok(()));
        // A cut one pixel from each edge leaves 1 px parts, which is the minimum.
        assert_eq!(validate(2, &[1], &[0, 1]), Ok(()));
    }

    #[test]
    fn validate_refuses_every_engine_precondition() {
        assert_eq!(
            validate(1, &[], &[0]),
            Err(SplitLayoutError::PageTooSmall { extent: 1 })
        );
        assert_eq!(validate(100, &[], &[0]), Err(SplitLayoutError::NoCuts));
        assert_eq!(
            validate(100, &[0], &[0, 1]),
            Err(SplitLayoutError::CutOutsidePage {
                index: 0,
                value: 0,
                extent: 100
            })
        );
        assert_eq!(
            validate(100, &[100], &[0, 1]),
            Err(SplitLayoutError::CutOutsidePage {
                index: 0,
                value: 100,
                extent: 100
            })
        );
        assert_eq!(
            validate(100, &[30, 30], &[0, 1, 2]),
            Err(SplitLayoutError::CutsNotIncreasing { index: 1 })
        );
        assert_eq!(
            validate(100, &[60, 30], &[0, 1, 2]),
            Err(SplitLayoutError::CutsNotIncreasing { index: 1 })
        );
    }

    #[test]
    fn validate_refuses_an_order_that_is_not_a_permutation() {
        // Wrong length.
        assert_eq!(
            validate(100, &[30], &[0]),
            Err(SplitLayoutError::OrderNotPermutation { parts: 2 })
        );
        // Duplicate position.
        assert_eq!(
            validate(100, &[30], &[1, 1]),
            Err(SplitLayoutError::OrderNotPermutation { parts: 2 })
        );
        // Position out of range.
        assert_eq!(
            validate(100, &[30], &[0, 2]),
            Err(SplitLayoutError::OrderNotPermutation { parts: 2 })
        );
    }

    #[test]
    fn default_order_is_geometric_order() {
        assert_eq!(default_order(1), vec![0]);
        assert_eq!(default_order(3), vec![0, 1, 2]);
        assert_eq!(default_order(0), Vec::<usize>::new());
    }

    #[test]
    fn swap_positions_exchanges_the_two_parts() {
        let mut order = vec![0, 1, 2];
        swap_positions(&mut order, 0, 2);
        assert_eq!(order, vec![2, 1, 0]);
        // Idempotent on the position a part already holds.
        swap_positions(&mut order, 0, 2);
        assert_eq!(order, vec![2, 1, 0]);
        // Out-of-range arguments are no-ops, never panics.
        swap_positions(&mut order, 9, 0);
        swap_positions(&mut order, 0, 9);
        assert_eq!(order, vec![2, 1, 0]);
    }

    #[test]
    fn insert_cut_keeps_the_order_a_permutation() {
        let mut cuts = vec![50_u32];
        let mut order = vec![0, 1];
        assert_eq!(insert_cut(100, &mut cuts, &mut order, 25), Some(0));
        assert_eq!(cuts, vec![25, 50]);
        // Part 0 (0..50) split into 0..25 and 25..50; the new half follows it.
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(validate(100, &cuts, &order), Ok(()));
    }

    #[test]
    fn insert_cut_preserves_a_reordered_layout() {
        // Two parts shown in reverse order: part 0 is page 2, part 1 is page 1.
        let mut cuts = vec![50_u32];
        let mut order = vec![1, 0];
        assert_eq!(insert_cut(100, &mut cuts, &mut order, 75), Some(1));
        assert_eq!(cuts, vec![50, 75]);
        // Part 1 (50..100) split; its lower half must follow it immediately, so
        // the old part 0 (position 1) is pushed to position 2.
        assert_eq!(order, vec![2, 0, 1]);
        assert_eq!(validate(100, &cuts, &order), Ok(()));
    }

    #[test]
    fn insert_cut_refuses_edges_and_duplicates() {
        let mut cuts = vec![50_u32];
        let mut order = vec![0, 1];
        assert_eq!(insert_cut(100, &mut cuts, &mut order, 0), None);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, 100), None);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, 200), None);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, 50), None);
        // Refused inserts leave both arguments untouched.
        assert_eq!(cuts, vec![50]);
        assert_eq!(order, vec![0, 1]);
    }

    #[test]
    fn remove_cut_merges_and_keeps_the_earlier_position() {
        let mut cuts = vec![25_u32, 50];
        let mut order = vec![2, 0, 1];
        remove_cut(&mut cuts, &mut order, 0);
        assert_eq!(cuts, vec![50]);
        // Parts 0 (position 2) and 1 (position 0) merge; the merged part keeps
        // position 0 and the old position 1 shifts up into 0..2.
        assert_eq!(order, vec![0, 1]);
        assert_eq!(validate(100, &cuts, &order), Ok(()));
    }

    #[test]
    fn remove_cut_is_the_inverse_of_insert_cut() {
        let mut cuts = vec![50_u32];
        let mut order = vec![1, 0];
        let index = insert_cut(100, &mut cuts, &mut order, 75).expect("75 is inside the page");
        remove_cut(&mut cuts, &mut order, index);
        assert_eq!(cuts, vec![50]);
        assert_eq!(order, vec![1, 0]);
    }

    #[test]
    fn remove_cut_ignores_an_out_of_range_index() {
        let mut cuts = vec![50_u32];
        let mut order = vec![0, 1];
        remove_cut(&mut cuts, &mut order, 5);
        assert_eq!(cuts, vec![50]);
        assert_eq!(order, vec![0, 1]);
    }

    #[test]
    fn clamp_cut_keeps_one_pixel_on_every_side() {
        let cuts = vec![30_u32, 60, 90];
        // A lone cut is bounded by the page edges only.
        assert_eq!(clamp_cut(100, &[50], 0, 0), 1);
        assert_eq!(clamp_cut(100, &[50], 0, 500), 99);
        // A middle cut is bounded by both its neighbours.
        assert_eq!(clamp_cut(100, &cuts, 1, 0), 31);
        assert_eq!(clamp_cut(100, &cuts, 1, 100), 89);
        assert_eq!(clamp_cut(100, &cuts, 1, 45), 45);
        // The last cut keeps its lower neighbour and the page's far edge.
        assert_eq!(clamp_cut(100, &cuts, 2, 0), 61);
        assert_eq!(clamp_cut(100, &cuts, 2, 100), 99);
        // A degenerate page collapses the bounds instead of panicking.
        assert_eq!(clamp_cut(1, &[0], 0, 0), 1);
    }

    #[test]
    fn suggest_cut_halves_the_largest_part() {
        assert_eq!(suggest_cut(100, &[]), Some(50));
        // Parts 0..30 and 30..100: the second one is larger.
        assert_eq!(suggest_cut(100, &[30]), Some(65));
        // Every part is 1 px: nothing can be cut any further.
        assert_eq!(suggest_cut(2, &[1]), None);
        assert_eq!(suggest_cut(1, &[]), None);
    }

    #[test]
    fn page_numbers_follow_the_chosen_order() {
        // Page 5 (index 4) cut in three, shown in reverse: the topmost part
        // becomes page 7 and the bottom one page 5.
        let order = [2_usize, 1, 0];
        let number = |part: usize| page_number_for_position(4, order[part]);
        assert_eq!(number(0), 7);
        assert_eq!(number(1), 6);
        assert_eq!(number(2), 5);
        // The identity order keeps the parts in their geometric sequence.
        assert_eq!(page_number_for_position(0, 0), 1);
        assert_eq!(page_number_for_position(4, 0), 5);
    }
}
