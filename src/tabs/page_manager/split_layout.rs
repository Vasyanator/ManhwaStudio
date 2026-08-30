/*
File: tabs/page_manager/split_layout.rs

Purpose:
GUI-free core of the "split page" feature (Layer 2 of
`dev-docs/split_page_plan.md`): where the cut lines sit along the cut axis, the
parts they produce, how the user's chosen page order is kept consistent while
lines are added and removed, which parts are dropped instead of becoming pages,
and the validation that mirrors the engine's own `PageOpKind::Split` contract.
Contains no egui code and performs no I/O, so every rule here is unit-testable.

Key structures:
- SplitPart: one resulting part as an origin/size pair along the cut axis.
- SplitLayoutError: why a set of cuts or an order is not a legal engine request.
- PartChoice: what the order picker of one part was set to (a rank, or Delete).

Key functions:
- parts(): cut coordinates -> the parts they produce.
- validate(): the engine's own preconditions, checked before the confirm button.
- insert_cut() / remove_cut(): cut edits that keep `order` a permutation and the
  `deleted` mask aligned with it, without disturbing the user's relative order.
- apply_choice(): the order picker's semantics (swap, un-delete + swap, delete).
- wheel_choice(): what a wheel notch over a picker selects — never Delete.
- kept_count() / survivor_rank(): the survivors and their page ranks.
- swap_positions(): the order widget's SWAP semantics.
- clamp_cut() / suggest_cut(): drag bounds and the "add a line" default.
- page_number_for_position(): the real 1-based page number a page rank gets.

Notes:
The module is axis-agnostic: everything is expressed along ONE axis, as an
`extent` in SOURCE pixels. The window maps `SplitAxis::Horizontal` to the page
height and `SplitAxis::Vertical` to the page width, so the same math serves both
orientations. Coordinates are ALWAYS source pixels — the board's preview is
downscaled and its resolution never reaches this module.

The part model is TWO parallel arrays over the geometric parts, exactly as the
engine's request carries them: `order` stays a permutation of `0..parts` over
ALL parts (deleted ones included) and `deleted[k]` says whether part `k` becomes
a page at all. Keeping a deleted part's position in `order` is what lets an
un-delete restore its own place for free, and it keeps every function below on
the permutation math it was already tested against.
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
    /// The `deleted` mask and `order` disagree about how many parts there are.
    ///
    /// An internal desync of the dialog's two parallel arrays, not a state the
    /// user can reach; it shares the "the part order is broken" message with
    /// [`SplitLayoutError::OrderNotPermutation`] because it says the same thing
    /// to the user.
    #[error("the deleted mask holds {deleted} entries for {parts} parts")]
    DeletedLengthMismatch { parts: usize, deleted: usize },
    /// Every part is marked for deletion, which would destroy the page instead
    /// of splitting it. The engine refuses the same request.
    #[error("every part is marked for deletion")]
    AllPartsDeleted,
}

/// What the order picker of ONE geometric part was set to.
///
/// Exhaustive on purpose: a new way of targeting a part must force every
/// decision site (the picker, [`apply_choice`]) to be reconsidered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PartChoice {
    /// The part survives and takes survivor rank `usize` (0-based) among the
    /// parts that are not deleted.
    Rank(usize),
    /// The part is dropped: it becomes no page and its content is discarded.
    Delete,
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
/// `deleted` is the parallel drop mask; it must have one entry per part and
/// must leave at least one survivor.
///
/// # Errors
/// [`SplitLayoutError::PageTooSmall`], [`SplitLayoutError::NoCuts`],
/// [`SplitLayoutError::CutOutsidePage`], [`SplitLayoutError::CutsNotIncreasing`],
/// [`SplitLayoutError::OrderNotPermutation`],
/// [`SplitLayoutError::DeletedLengthMismatch`] or
/// [`SplitLayoutError::AllPartsDeleted`], in that order of checking.
pub(super) fn validate(
    extent: u32,
    cuts: &[u32],
    order: &[usize],
    deleted: &[bool],
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
    if deleted.len() != count {
        return Err(SplitLayoutError::DeletedLengthMismatch {
            parts: count,
            deleted: deleted.len(),
        });
    }
    // A split that keeps nothing is a deletion of the page, which this window
    // does not offer and the engine refuses; keeping exactly ONE part is legal
    // and is how a crop is expressed.
    if kept_count(deleted) == 0 {
        return Err(SplitLayoutError::AllPartsDeleted);
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

/// The default drop mask: nothing is deleted, one entry per part.
#[must_use]
pub(super) fn default_deleted(parts: usize) -> Vec<bool> {
    vec![false; parts]
}

/// How many parts survive, i.e. how many pages the split produces.
#[must_use]
pub(super) fn kept_count(deleted: &[bool]) -> usize {
    deleted.iter().filter(|gone| !**gone).count()
}

/// Rank of `part` among the SURVIVING parts, ordered by their `order` value, or
/// `None` when `part` is deleted or out of range.
///
/// The rank — not the raw `order` value — is what the engine turns into a page
/// index and what the picker must display, so deleting a part renumbers the
/// survivors after it automatically.
#[must_use]
pub(super) fn survivor_rank(order: &[usize], deleted: &[bool], part: usize) -> Option<usize> {
    if *deleted.get(part)? {
        return None;
    }
    let own = *order.get(part)?;
    Some(
        order
            .iter()
            .zip(deleted.iter())
            .filter(|(position, gone)| !**gone && **position < own)
            .count(),
    )
}

/// The surviving part that currently holds survivor rank `rank`, or `None` when
/// no survivor does.
#[must_use]
fn part_at_rank(order: &[usize], deleted: &[bool], rank: usize) -> Option<usize> {
    let mut survivors: Vec<(usize, usize)> = order
        .iter()
        .zip(deleted.iter())
        .enumerate()
        .filter(|(_, (_, gone))| !**gone)
        .map(|(part, (position, _))| (*position, part))
        .collect();
    // Ranks are read off the positions, so the survivors must be sorted by them;
    // positions are distinct in a valid permutation, so the sort is total.
    survivors.sort_unstable();
    survivors.get(rank).map(|(_, part)| *part)
}

/// Applies the order picker's choice for geometric part `part`.
///
/// [`PartChoice::Delete`] marks the part dropped, keeping its position in
/// `order` so that a later un-delete restores its own place.
/// [`PartChoice::Rank`] un-deletes the part first (if it was deleted) and then
/// SWAPS it with whichever survivor holds `rank` — the window's specified
/// semantics, unchanged from before deletion existed.
///
/// A no-op — never a partial edit — when `part` is out of range, when the two
/// arrays disagree in length, or when `rank` is beyond the ranks the part could
/// take. That "never partial" guarantee rests on the length/range guard at the
/// top: it makes every `get`/`get_mut` below infallible, so the `let … else`
/// arms are unreachable safety nets rather than early exits from a half-applied
/// edit. Any new lookup added after the first mutation must be covered by that
/// same guard, or the guarantee is gone.
pub(super) fn apply_choice(
    order: &mut [usize],
    deleted: &mut [bool],
    part: usize,
    choice: PartChoice,
) {
    if order.len() != deleted.len() || part >= order.len() {
        return;
    }
    match choice {
        PartChoice::Delete => {
            if let Some(flag) = deleted.get_mut(part) {
                *flag = true;
            }
        }
        PartChoice::Rank(rank) => {
            let Some(&was_deleted) = deleted.get(part) else {
                return;
            };
            // Ranks are read in the world the pick CREATES, in which this part is
            // a survivor again — so un-deleting adds one rank to choose from.
            // Checked before any mutation, so a rank nobody can hold leaves the
            // state untouched instead of half-applied.
            if rank >= kept_count(deleted) + usize::from(was_deleted) {
                return;
            }
            if let Some(flag) = deleted.get_mut(part) {
                *flag = false;
            }
            let Some(other) = part_at_rank(order, deleted, rank) else {
                return;
            };
            let Some(&position) = order.get(other) else {
                return;
            };
            swap_positions(order, part, position);
        }
    }
}

/// What a wheel notch over an order picker selects, or `None` when it selects
/// nothing.
///
/// This is the picker's safety-critical rule, kept here rather than in the
/// drawing code so it is unit-testable: the wheel walks the NUMERIC ranks ALONE
/// and can NEVER return [`PartChoice::Delete`]. Handing the whole list to
/// `WheelComboBox::show_index` instead would cycle the "Delete" entry too — over
/// a CLOSED picker, and wrapping — so a single stray notch past the last rank
/// would discard a part's content with no click at all.
///
/// `rank` is the part's current survivor rank and `kept` the number of
/// survivors. `None` for `rank` means the part is already deleted: it holds no
/// rank to step from, so the wheel leaves it alone and un-deleting stays a
/// deliberate click. Also `None` when the notch lands back on the current rank.
#[must_use]
pub(super) fn wheel_choice(rank: Option<usize>, kept: usize, steps: i32) -> Option<PartChoice> {
    let current = rank?;
    let next = cycle_rank(current, kept, steps);
    (next != current).then_some(PartChoice::Rank(next))
}

/// The survivor rank `steps` wheel notches away from `current`, wrapping inside
/// `0..len`. A no-op for an empty range or a zero step.
///
/// Repeated here rather than reused from `widgets::wheel_input_guard`
/// (`cycle_wrapped_index`), which is private to `src/widgets`: the picker must
/// cycle the NUMERIC ranks ALONE, never the "Delete" entry that follows them in
/// its list, so it cannot hand the whole list to `WheelComboBox::show_index`.
/// An out-of-range `current` is reduced into range first, and neither branch
/// ever forms `current + shift`, which would overflow near `usize::MAX`.
#[must_use]
fn cycle_rank(current: usize, len: usize, steps: i32) -> usize {
    if len == 0 {
        return current;
    }
    let current = current % len;
    // Infallible on every target this project builds for; a hypothetical
    // narrower `usize` would merely ignore the notch.
    let magnitude = usize::try_from(steps.unsigned_abs()).unwrap_or(0) % len;
    if magnitude == 0 {
        return current;
    }
    // Stepping back by `magnitude` is stepping forward by `len - magnitude`.
    let shift = if steps > 0 { magnitude } else { len - magnitude };
    if current < len - shift {
        current + shift
    } else {
        current - (len - shift)
    }
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

/// Inserts a cut at `value`, extending `order` so it stays a permutation and
/// `deleted` so it stays aligned with it.
///
/// The geometric part the cut falls into is split in two; the NEW (lower/right)
/// half takes the page position right after the part it was cut from, and every
/// later position shifts down by one. The relative order the user chose for
/// every other part is therefore preserved. The new half INHERITS the drop flag
/// of the part it was cut from: cutting a part that is already being dropped
/// cannot silently resurrect half of it.
///
/// Returns the index of the inserted cut, or `None` when `value` is not
/// strictly inside the page or a cut already sits there. `order` must be a
/// permutation of `0..parts(cuts)` and `deleted` must be the same length; on
/// `None` no argument is modified.
pub(super) fn insert_cut(
    extent: u32,
    cuts: &mut Vec<u32>,
    order: &mut Vec<usize>,
    deleted: &mut Vec<bool>,
    value: u32,
) -> Option<usize> {
    if value == 0 || value >= extent {
        return None;
    }
    if order.len() != part_count(cuts) || deleted.len() != order.len() {
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
    let inherited = *deleted.get(index)?;
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
    deleted.insert(index + 1, inherited);
    Some(index)
}

/// Removes cut `index`, merging the two parts it separated back into one.
///
/// The merged part keeps the EARLIER of the two page positions and the later one
/// disappears, every position after it shifting up by one, so the remaining
/// parts keep their relative order.
///
/// It is deleted only when BOTH halves were: the merge must not be able to
/// discard content the user never marked. The rule is deliberately independent
/// of the two halves' page positions — a deleted part's picker shows "Delete"
/// instead of a number, so its position is INVISIBLE and a position-based rule
/// would make the outcome unpredictable from what is on screen. It is still the
/// exact inverse of [`insert_cut`], which gives both halves their parent's flag.
///
/// Does nothing when `index` is out of range or the arrays are not sized for
/// `cuts`.
pub(super) fn remove_cut(
    cuts: &mut Vec<u32>,
    order: &mut Vec<usize>,
    deleted: &mut Vec<bool>,
    index: usize,
) {
    if index >= cuts.len() || order.len() != part_count(cuts) || deleted.len() != order.len() {
        return;
    }
    let (Some(&first), Some(&second)) = (order.get(index), order.get(index + 1)) else {
        return;
    };
    let (Some(&first_deleted), Some(&second_deleted)) =
        (deleted.get(index), deleted.get(index + 1))
    else {
        return;
    };
    let merged_deleted = first_deleted && second_deleted;
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
    deleted.remove(index + 1);
    if let Some(flag) = deleted.get_mut(index) {
        *flag = merged_deleted;
    }
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

/// The real 1-based page number a surviving part gets at survivor rank `rank`.
///
/// The split replaces page `page_idx` with its SURVIVING parts at indices
/// `page_idx .. page_idx + kept`, so the survivor of rank `r` lands at index
/// `page_idx + r`. This is what the order picker shows in parentheses: for
/// geometric part `k` the current number is
/// `page_number_for_position(page_idx, survivor_rank(order, deleted, k)?)`.
/// Deleted parts have no rank and therefore no page number.
#[must_use]
pub(super) fn page_number_for_position(page_idx: usize, rank: usize) -> usize {
    page_idx.saturating_add(rank).saturating_add(1)
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

    /// The drop mask of a request in which nothing is deleted.
    fn alive(parts: usize) -> Vec<bool> {
        default_deleted(parts)
    }

    #[test]
    fn validate_accepts_a_legal_request() {
        assert_eq!(validate(100, &[30, 60], &[0, 1, 2], &alive(3)), Ok(()));
        // Any permutation is legal, not only the identity.
        assert_eq!(validate(100, &[30, 60], &[2, 0, 1], &alive(3)), Ok(()));
        // A cut one pixel from each edge leaves 1 px parts, which is the minimum.
        assert_eq!(validate(2, &[1], &[0, 1], &alive(2)), Ok(()));
    }

    #[test]
    fn validate_refuses_every_engine_precondition() {
        assert_eq!(
            validate(1, &[], &[0], &alive(1)),
            Err(SplitLayoutError::PageTooSmall { extent: 1 })
        );
        assert_eq!(
            validate(100, &[], &[0], &alive(1)),
            Err(SplitLayoutError::NoCuts)
        );
        assert_eq!(
            validate(100, &[0], &[0, 1], &alive(2)),
            Err(SplitLayoutError::CutOutsidePage {
                index: 0,
                value: 0,
                extent: 100
            })
        );
        assert_eq!(
            validate(100, &[100], &[0, 1], &alive(2)),
            Err(SplitLayoutError::CutOutsidePage {
                index: 0,
                value: 100,
                extent: 100
            })
        );
        assert_eq!(
            validate(100, &[30, 30], &[0, 1, 2], &alive(3)),
            Err(SplitLayoutError::CutsNotIncreasing { index: 1 })
        );
        assert_eq!(
            validate(100, &[60, 30], &[0, 1, 2], &alive(3)),
            Err(SplitLayoutError::CutsNotIncreasing { index: 1 })
        );
    }

    #[test]
    fn validate_refuses_an_order_that_is_not_a_permutation() {
        // Wrong length.
        assert_eq!(
            validate(100, &[30], &[0], &alive(1)),
            Err(SplitLayoutError::OrderNotPermutation { parts: 2 })
        );
        // Duplicate position.
        assert_eq!(
            validate(100, &[30], &[1, 1], &alive(2)),
            Err(SplitLayoutError::OrderNotPermutation { parts: 2 })
        );
        // Position out of range.
        assert_eq!(
            validate(100, &[30], &[0, 2], &alive(2)),
            Err(SplitLayoutError::OrderNotPermutation { parts: 2 })
        );
    }

    #[test]
    fn validate_refuses_a_drop_mask_that_does_not_match_the_parts() {
        assert_eq!(
            validate(100, &[30], &[0, 1], &[false]),
            Err(SplitLayoutError::DeletedLengthMismatch {
                parts: 2,
                deleted: 1
            })
        );
    }

    #[test]
    fn validate_refuses_deleting_every_part_but_accepts_keeping_one() {
        assert_eq!(
            validate(100, &[30, 60], &[0, 1, 2], &[true, true, true]),
            Err(SplitLayoutError::AllPartsDeleted)
        );
        // Keeping exactly one part is a CROP, which is legal.
        assert_eq!(
            validate(100, &[30, 60], &[0, 1, 2], &[true, false, true]),
            Ok(())
        );
    }

    #[test]
    fn default_order_is_geometric_order() {
        assert_eq!(default_order(1), vec![0]);
        assert_eq!(default_order(3), vec![0, 1, 2]);
        assert_eq!(default_order(0), Vec::<usize>::new());
        assert_eq!(default_deleted(3), vec![false, false, false]);
        assert_eq!(default_deleted(0), Vec::<bool>::new());
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
        let mut deleted = alive(2);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, &mut deleted, 25), Some(0));
        assert_eq!(cuts, vec![25, 50]);
        // Part 0 (0..50) split into 0..25 and 25..50; the new half follows it.
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(deleted, alive(3));
        assert_eq!(validate(100, &cuts, &order, &deleted), Ok(()));
    }

    #[test]
    fn insert_cut_preserves_a_reordered_layout() {
        // Two parts shown in reverse order: part 0 is page 2, part 1 is page 1.
        let mut cuts = vec![50_u32];
        let mut order = vec![1, 0];
        let mut deleted = alive(2);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, &mut deleted, 75), Some(1));
        assert_eq!(cuts, vec![50, 75]);
        // Part 1 (50..100) split; its lower half must follow it immediately, so
        // the old part 0 (position 1) is pushed to position 2.
        assert_eq!(order, vec![2, 0, 1]);
        assert_eq!(validate(100, &cuts, &order, &deleted), Ok(()));
    }

    #[test]
    fn insert_cut_refuses_edges_and_duplicates() {
        let mut cuts = vec![50_u32];
        let mut order = vec![0, 1];
        let mut deleted = alive(2);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, &mut deleted, 0), None);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, &mut deleted, 100), None);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, &mut deleted, 200), None);
        assert_eq!(insert_cut(100, &mut cuts, &mut order, &mut deleted, 50), None);
        // Refused inserts leave every argument untouched.
        assert_eq!(cuts, vec![50]);
        assert_eq!(order, vec![0, 1]);
        assert_eq!(deleted, alive(2));
    }

    /// Cutting a part that is already dropped must not resurrect half of it: the
    /// new half inherits the flag of the part it was cut from.
    #[test]
    fn insert_cut_gives_the_new_half_its_parents_drop_flag() {
        let mut cuts = vec![50_u32];
        let mut order = vec![0, 1];
        let mut deleted = vec![false, true];
        assert_eq!(insert_cut(100, &mut cuts, &mut order, &mut deleted, 75), Some(1));
        assert_eq!(deleted, vec![false, true, true]);
        // Cutting the SURVIVING part leaves two survivors.
        assert_eq!(insert_cut(100, &mut cuts, &mut order, &mut deleted, 25), Some(0));
        assert_eq!(deleted, vec![false, false, true, true]);
        assert_eq!(validate(100, &cuts, &order, &deleted), Ok(()));
    }

    #[test]
    fn remove_cut_merges_and_keeps_the_earlier_position() {
        let mut cuts = vec![25_u32, 50];
        let mut order = vec![2, 0, 1];
        let mut deleted = alive(3);
        remove_cut(&mut cuts, &mut order, &mut deleted, 0);
        assert_eq!(cuts, vec![50]);
        // Parts 0 (position 2) and 1 (position 0) merge; the merged part keeps
        // position 0 and the old position 1 shifts up into 0..2.
        assert_eq!(order, vec![0, 1]);
        assert_eq!(deleted, alive(2));
        assert_eq!(validate(100, &cuts, &order, &deleted), Ok(()));
    }

    /// The merged part is deleted only when BOTH halves were, for all four flag
    /// combinations. A merge must never discard content the user did not mark:
    /// a deleted part's picker shows "Delete" instead of a number, so a rule
    /// keyed on the halves' page positions would be invisible on screen.
    #[test]
    fn remove_cut_deletes_the_merged_part_only_when_both_halves_were() {
        for (halves, expected) in [
            ([false, false], false),
            ([true, false], false),
            ([false, true], false),
            ([true, true], true),
        ] {
            let mut cuts = vec![50_u32];
            let mut order = vec![0, 1];
            let mut deleted = halves.to_vec();
            remove_cut(&mut cuts, &mut order, &mut deleted, 0);
            assert_eq!(deleted, vec![expected], "halves {halves:?}");
        }
    }

    /// The same rule, independent of which half holds the earlier page position:
    /// reversing the order must not change what survives.
    #[test]
    fn remove_cut_flag_rule_does_not_depend_on_the_page_positions() {
        for order_pair in [vec![0_usize, 1], vec![1, 0]] {
            for halves in [[true, false], [false, true]] {
                let mut cuts = vec![50_u32];
                let mut order = order_pair.clone();
                let mut deleted = halves.to_vec();
                remove_cut(&mut cuts, &mut order, &mut deleted, 0);
                assert_eq!(deleted, vec![false], "{order_pair:?} / {halves:?}");
            }
        }
    }

    /// The reviewer's repro: three parts with the MIDDLE one deleted; removing
    /// the second cut line merges the middle into the bottom third. The survivor
    /// must stay a page, so `kept` stays 2 -> 1 pages worth of content is never
    /// silently discarded by a cut-line removal.
    #[test]
    fn removing_a_cut_next_to_a_deleted_part_keeps_the_surviving_half() {
        let mut cuts = vec![30_u32, 60];
        let mut order = vec![0, 1, 2];
        let mut deleted = vec![false, true, false];
        assert_eq!(kept_count(&deleted), 2);
        remove_cut(&mut cuts, &mut order, &mut deleted, 1);
        assert_eq!(cuts, vec![30]);
        assert_eq!(deleted, vec![false, false]);
        assert_eq!(kept_count(&deleted), 2);
        assert_eq!(validate(100, &cuts, &order, &deleted), Ok(()));
    }

    /// `insert_cut` gives both halves their parent's flag, so the "delete only
    /// when both halves were" merge rule restores exactly it — for a parent that
    /// was deleted and for one that was not.
    #[test]
    fn remove_cut_is_the_inverse_of_insert_cut() {
        for parent_deleted in [false, true] {
            let mut cuts = vec![50_u32];
            let mut order = vec![1, 0];
            let mut deleted = vec![false, parent_deleted];
            let index = insert_cut(100, &mut cuts, &mut order, &mut deleted, 75)
                .expect("75 is inside the page");
            remove_cut(&mut cuts, &mut order, &mut deleted, index);
            assert_eq!(cuts, vec![50]);
            assert_eq!(order, vec![1, 0]);
            assert_eq!(deleted, vec![false, parent_deleted]);
        }
    }

    #[test]
    fn remove_cut_ignores_an_out_of_range_index() {
        let mut cuts = vec![50_u32];
        let mut order = vec![0, 1];
        let mut deleted = alive(2);
        remove_cut(&mut cuts, &mut order, &mut deleted, 5);
        assert_eq!(cuts, vec![50]);
        assert_eq!(order, vec![0, 1]);
        assert_eq!(deleted, alive(2));
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
        let deleted = alive(3);
        let number = |part: usize| {
            survivor_rank(&order, &deleted, part).map(|rank| page_number_for_position(4, rank))
        };
        assert_eq!(number(0), Some(7));
        assert_eq!(number(1), Some(6));
        assert_eq!(number(2), Some(5));
        // The identity order keeps the parts in their geometric sequence.
        assert_eq!(page_number_for_position(0, 0), 1);
        assert_eq!(page_number_for_position(4, 0), 5);
    }

    /// D2: a part's page number is its rank among the SURVIVORS, so deleting a
    /// part renumbers everything after it without touching `order`.
    #[test]
    fn deleting_a_part_renumbers_the_survivors() {
        let order = [0_usize, 1, 2, 3];
        // Drop the second part: the two below it move one page up.
        let deleted = [false, true, false, false];
        assert_eq!(kept_count(&deleted), 3);
        assert_eq!(survivor_rank(&order, &deleted, 0), Some(0));
        assert_eq!(survivor_rank(&order, &deleted, 1), None);
        assert_eq!(survivor_rank(&order, &deleted, 2), Some(1));
        assert_eq!(survivor_rank(&order, &deleted, 3), Some(2));
        // Page 5 (index 4) keeps its index for the FIRST survivor.
        assert_eq!(page_number_for_position(4, 0), 5);
        assert_eq!(page_number_for_position(4, 1), 6);

        // Ranks follow `order`, not the geometric index: reversed parts with the
        // first two dropped leave the bottom part as page 1 of the pair.
        let reversed = [3_usize, 2, 1, 0];
        let dropped = [true, true, false, false];
        assert_eq!(survivor_rank(&reversed, &dropped, 3), Some(0));
        assert_eq!(survivor_rank(&reversed, &dropped, 2), Some(1));
        assert_eq!(survivor_rank(&reversed, &dropped, 0), None);
        // An out-of-range part has no rank instead of panicking.
        assert_eq!(survivor_rank(&reversed, &dropped, 9), None);
    }

    #[test]
    fn apply_choice_deletes_and_keeps_the_position_for_an_undelete() {
        let mut order = vec![2_usize, 0, 1];
        let mut deleted = alive(3);
        apply_choice(&mut order, &mut deleted, 0, PartChoice::Delete);
        assert_eq!(deleted, vec![true, false, false]);
        // The deleted part kept its own position, so the survivors just renumber.
        assert_eq!(order, vec![2, 0, 1]);
        assert_eq!(survivor_rank(&order, &deleted, 1), Some(0));
        assert_eq!(survivor_rank(&order, &deleted, 2), Some(1));

        // Un-deleting it by picking the rank it would naturally take restores the
        // layout exactly, with no swap.
        apply_choice(&mut order, &mut deleted, 0, PartChoice::Rank(2));
        assert_eq!(deleted, alive(3));
        assert_eq!(order, vec![2, 0, 1]);
    }

    #[test]
    fn apply_choice_undeletes_and_then_swaps_to_the_requested_rank() {
        let mut order = vec![0_usize, 1, 2];
        let mut deleted = vec![false, true, false];
        // Part 1 is dropped; asking for rank 0 must bring it back AND put it in
        // front of the part that holds rank 0 today (part 0).
        apply_choice(&mut order, &mut deleted, 1, PartChoice::Rank(0));
        assert_eq!(deleted, alive(3));
        assert_eq!(order, vec![1, 0, 2]);
        assert_eq!(survivor_rank(&order, &deleted, 1), Some(0));
        assert_eq!(survivor_rank(&order, &deleted, 0), Some(1));
    }

    #[test]
    fn apply_choice_swaps_two_survivors_by_rank_not_by_position() {
        // Part 1 is dropped, so ranks 0 and 1 belong to parts 0 and 2.
        let mut order = vec![0_usize, 1, 2];
        let mut deleted = vec![false, true, false];
        apply_choice(&mut order, &mut deleted, 0, PartChoice::Rank(1));
        // Positions 0 and 2 were exchanged; the dropped part did not move.
        assert_eq!(order, vec![2, 1, 0]);
        assert_eq!(deleted, vec![false, true, false]);
        assert_eq!(survivor_rank(&order, &deleted, 2), Some(0));
        assert_eq!(survivor_rank(&order, &deleted, 0), Some(1));
    }

    #[test]
    fn apply_choice_ignores_impossible_arguments() {
        let mut order = vec![0_usize, 1];
        let mut deleted = alive(2);
        // Out-of-range part, unreachable rank, and mismatched arrays are no-ops,
        // never panics.
        apply_choice(&mut order, &mut deleted, 9, PartChoice::Delete);
        apply_choice(&mut order, &mut deleted, 0, PartChoice::Rank(7));
        apply_choice(&mut order, &mut [false], 0, PartChoice::Delete);
        assert_eq!(order, vec![0, 1]);
        assert_eq!(deleted, alive(2));
    }

    /// D4, the picker's safety-critical rule: a wheel notch can NEVER select
    /// "Delete", at any rank, in either direction, with any number of survivors.
    #[test]
    fn a_wheel_notch_can_never_select_delete() {
        // At the LAST rank the wrap goes back to the first one, not to Delete.
        assert_eq!(wheel_choice(Some(2), 3, 1), Some(PartChoice::Rank(0)));
        assert_eq!(wheel_choice(Some(0), 3, -1), Some(PartChoice::Rank(2)));
        // Both directions in the middle of the range.
        assert_eq!(wheel_choice(Some(0), 3, 1), Some(PartChoice::Rank(1)));
        assert_eq!(wheel_choice(Some(2), 3, -1), Some(PartChoice::Rank(1)));
        // A single survivor has nowhere to step: the notch selects nothing
        // rather than falling off the end of the list onto Delete.
        assert_eq!(wheel_choice(Some(0), 1, 1), None);
        assert_eq!(wheel_choice(Some(0), 1, -1), None);
        assert_eq!(wheel_choice(Some(0), 1, 7), None);
        // An ALREADY deleted part holds no rank: the wheel leaves it alone, so
        // un-deleting stays a deliberate click just as deleting is.
        assert_eq!(wheel_choice(None, 2, 1), None);
        assert_eq!(wheel_choice(None, 2, -3), None);
        assert_eq!(wheel_choice(None, 0, 1), None);
        // A notch that lands back where it started changes nothing.
        assert_eq!(wheel_choice(Some(1), 3, 0), None);
        assert_eq!(wheel_choice(Some(1), 3, 3), None);
        // Exhaustive over a small range: every result is a Rank inside 0..kept.
        for kept in 1..=4_usize {
            for rank in 0..kept {
                for steps in -5..=5_i32 {
                    match wheel_choice(Some(rank), kept, steps) {
                        Some(PartChoice::Rank(next)) => assert!(next < kept),
                        Some(PartChoice::Delete) => {
                            panic!("the wheel selected Delete at {rank}/{kept} by {steps} steps")
                        }
                        None => {}
                    }
                }
            }
        }
    }

    #[test]
    fn cycle_rank_wraps_inside_the_survivors() {
        assert_eq!(cycle_rank(0, 3, 1), 1);
        assert_eq!(cycle_rank(2, 3, 1), 0);
        assert_eq!(cycle_rank(0, 3, -1), 2);
        assert_eq!(cycle_rank(1, 3, -4), 0);
        // Degenerate inputs are no-ops.
        assert_eq!(cycle_rank(1, 3, 0), 1);
        assert_eq!(cycle_rank(4, 0, 3), 4);
        // An out-of-range start is reduced into range first.
        assert_eq!(cycle_rank(7, 3, 1), 2);
        assert_eq!(cycle_rank(usize::MAX, 3, 1), (usize::MAX % 3) + 1);
    }
}
