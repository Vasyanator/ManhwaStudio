/*
FILE OVERVIEW: crates/ms-models/src/text_mask_model.rs
Shared runtime model for text-detector masks, synchronized between tabs.

Main items:
- `TextMaskPage`: per-page mask payload (source size + mask size + alpha + optional detector blocks).
- `TextMaskModel`: mutable shared store with monotonic `revision`.
- `edit_page_mask`: in-place editable mask API with lazy page allocation.
- `merge_region`: replaces one source-pixel rect of a page's mask with a region detection,
  keeping everything outside the rect (brush edits included), and takes the caller's merged box
  set for the page.
- `replace_mask_region`: the one mask rule of that replacement, also applied by the Translation
  tab to its own detector result store.
- `RegionMergeError`: why a region could not be merged.

Usage pattern:
- Translation tab writes page masks (and, when available, the detector blocks that
  describe the same detection) after detector/load events.
- Other tabs read masks by page index and track `revision` to refresh local caches.
  Autoclean reads `TextMaskPage.blocks` to build a box-based mask candidate.
*/

use std::collections::HashMap;

/// Why a region could not be merged into a page mask (`replace_mask_region`, `merge_region`).
/// Nothing is changed when one is returned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegionMergeError {
    /// The rect `[x1, y1, x2, y2]` (exclusive far edges) is empty or not inside the source page.
    #[error("region rect {rect:?} is empty or outside the {source_size:?} source page")]
    RectOutsidePage { rect: [u32; 4], source_size: [u32; 2] },
    /// The region mask is neither empty nor exactly one byte per rect pixel.
    #[error("region mask has {len} bytes, expected 0 or {expected}")]
    RegionMaskSize { len: usize, expected: usize },
    /// The page mask does not hold one byte per pixel of its declared size.
    #[error("page mask has {len} bytes, expected {expected} for a {mask_size:?} mask")]
    PageMaskSize { len: usize, expected: usize, mask_size: [u32; 2] },
}

/// Pixel count of a `[w, h]` size, `None` on overflow.
fn pixel_count(size: [u32; 2]) -> Option<usize> {
    usize::try_from(u64::from(size[0]) * u64::from(size[1])).ok()
}

/// Checks that `rect` (`[x1, y1, x2, y2]`, exclusive far edges) is non-empty and inside a page
/// of `source_size`.
fn check_region_rect(rect: [u32; 4], source_size: [u32; 2]) -> Result<(), RegionMergeError> {
    let [x1, y1, x2, y2] = rect;
    if x1 < x2 && y1 < y2 && x2 <= source_size[0] && y2 <= source_size[1] {
        Ok(())
    } else {
        Err(RegionMergeError::RectOutsidePage { rect, source_size })
    }
}

/// Replaces the part of a page mask that covers the source-pixel `rect` with `region_alpha`.
///
/// `mask_alpha` is the page mask (`mask_size`, one byte per pixel) of a `source_size` page; the
/// mask may be a scaled raster of the page. `rect` is `[x1, y1, x2, y2]` in source pixels with
/// exclusive far edges, and `region_alpha` is the region's mask at the rect's size (`x2 - x1` by
/// `y2 - y1`), or empty for "no text in the region". Every mask pixel whose centre maps into
/// `rect` takes the region's value at that point (nearest sample; nonzero -> 255, else 0), so
/// the old mask inside the rect is cleared and the region's mask is the only content there;
/// every other pixel is unchanged.
///
/// # Errors
/// [`RegionMergeError`] when the rect is empty / outside the page or a buffer length does not
/// match its size; `mask_alpha` is then untouched.
pub fn replace_mask_region(
    mask_alpha: &mut [u8],
    mask_size: [u32; 2],
    source_size: [u32; 2],
    rect: [u32; 4],
    region_alpha: &[u8],
) -> Result<(), RegionMergeError> {
    check_region_rect(rect, source_size)?;
    let [x1, y1, x2, y2] = rect;
    let region_w = u64::from(x2 - x1);
    let region_len = pixel_count([x2 - x1, y2 - y1]).unwrap_or(usize::MAX);
    if !region_alpha.is_empty() && region_alpha.len() != region_len {
        return Err(RegionMergeError::RegionMaskSize { len: region_alpha.len(), expected: region_len });
    }
    let mask_len = pixel_count(mask_size).unwrap_or(usize::MAX);
    if mask_alpha.len() != mask_len {
        return Err(RegionMergeError::PageMaskSize { len: mask_alpha.len(), expected: mask_len, mask_size });
    }
    let [mask_w, mask_h] = mask_size.map(u64::from);
    if mask_w == 0 || mask_h == 0 {
        return Ok(());
    }
    // Source coordinate of a mask pixel's centre: floor((2m + 1) * source / (2 * mask)). The mask
    // pixels whose centre can fall into [a, b) lie in [floor(a * mask / source),
    // ceil(b * mask / source)); each candidate is then checked exactly. The products are done in
    // u128: every factor is below 2^33, so no product can overflow (in u64, `(2m + 1) * source`
    // could for sides near `u32::MAX`). Every result is at most a u32 side, so it fits u64.
    let to_source = |m: u64, source: u32, mask: u64| {
        let value = ((2 * u128::from(m) + 1) * u128::from(source)) / (2 * u128::from(mask));
        u64::try_from(value).unwrap_or(u64::MAX)
    };
    let mask_range = |a: u32, b: u32, source: u32, mask: u64| {
        let start = (u128::from(a) * u128::from(mask)) / u128::from(source);
        let end = (u128::from(b) * u128::from(mask)).div_ceil(u128::from(source)).min(u128::from(mask));
        (u64::try_from(start).unwrap_or(u64::MAX), u64::try_from(end).unwrap_or(0))
    };
    // `check_region_rect` guarantees `0 <= x1 < x2 <= source_w`, so `source_w > 0` (same for y).
    let (mx_start, mx_end) = mask_range(x1, x2, source_size[0], mask_w);
    let (my_start, my_end) = mask_range(y1, y2, source_size[1], mask_h);
    for my in my_start..my_end {
        let sy = to_source(my, source_size[1], mask_h);
        if sy < u64::from(y1) || sy >= u64::from(y2) {
            continue;
        }
        for mx in mx_start..mx_end {
            let sx = to_source(mx, source_size[0], mask_w);
            if sx < u64::from(x1) || sx >= u64::from(x2) {
                continue;
            }
            let value = if region_alpha.is_empty() {
                0
            } else {
                let region_idx = (sy - u64::from(y1)) * region_w + (sx - u64::from(x1));
                // In range: sx/sy lie inside the rect and the region length was checked above.
                let region_value = usize::try_from(region_idx).ok().and_then(|idx| region_alpha.get(idx)).copied().unwrap_or(0);
                if region_value == 0 { 0 } else { 255 }
            };
            // In range: mx < mask_w, my < mask_h and the mask length was checked above.
            if let Some(pixel) = usize::try_from(my * mask_w + mx).ok().and_then(|idx| mask_alpha.get_mut(idx)) {
                *pixel = value;
            }
        }
    }
    Ok(())
}

/// The box set a region merge leaves on a page: the caller's merged `blocks`, unless the page's
/// boxes were unknown while its mask had content (a hand-edited mask keeps unknown boxes).
/// Empty sets are stored as `None`, as everywhere in the model.
fn merged_blocks(old: Option<&Vec<[i32; 4]>>, mask_was_empty: bool, blocks: Option<Vec<[i32; 4]>>) -> Option<Vec<[i32; 4]>> {
    if old.is_none() && !mask_was_empty {
        return None;
    }
    blocks.filter(|blocks| !blocks.is_empty())
}

/// Per-page detector mask payload shared between tabs.
///
/// `source_size` is the source-page pixel size `[w, h]`; `mask_size` is the mask
/// raster size `[w, h]` (may differ from `source_size` when the detector returned a
/// downscaled mask). `mask_alpha` is a single-channel `mask_w * mask_h` buffer.
///
/// `blocks` are the detector text boxes as `[x1, y1, x2, y2]` covering integer rects
/// in **source-page pixel space** — the same space as `source_size`, NOT `mask_size`.
/// Consumers that operate in mask space must scale blocks by the `source -> mask`
/// transform themselves. `blocks` is `None` when no detector boxes are known for the
/// page (e.g. a mask produced purely by manual editing); it never implies "empty box
/// set" versus "unknown" beyond that distinction: `Some(vec![])` is not produced by
/// this model. The boxes describe the same detection the mask represents; a manual
/// mask edit invalidates them (see `edit_page_mask`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMaskPage {
    pub source_size: [u32; 2],
    pub mask_size: [u32; 2],
    pub mask_alpha: Vec<u8>,
    pub blocks: Option<Vec<[i32; 4]>>,
}

#[derive(Debug, Default)]
pub struct TextMaskModel {
    pages: HashMap<usize, TextMaskPage>,
    revision: u64,
}

impl TextMaskModel {
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            revision: 1,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn page(&self, page_idx: usize) -> Option<&TextMaskPage> {
        self.pages.get(&page_idx)
    }

    /// Every stored page with its index, in no particular order.
    pub fn pages(&self) -> impl Iterator<Item = (usize, &TextMaskPage)> {
        self.pages.iter().map(|(page_idx, page)| (*page_idx, page))
    }

    /// Replaces the source-pixel `rect` of page `page_idx` with a region detection: inside the
    /// rect the CURRENT mask (brush edits included) is replaced by `region_alpha`
    /// (`replace_mask_region`); outside the rect nothing changes. Bumps the revision.
    ///
    /// `blocks` is the page's WHOLE box set after the merge, as the caller's own detector store
    /// holds it (source pixels; the Translation tab passes its finalized blocks, so both stores
    /// agree). It is stored unless the page's boxes are unknown (`None`) while its mask has
    /// content: such a mask was hand-edited and its boxes stay unknown. A missing page, or one
    /// stored for another `source_size` (the page image changed), starts from an empty
    /// source-size mask. The mask is changed in place (no copy of the page).
    ///
    /// # Errors
    /// [`RegionMergeError`] from the rect / buffer checks; the model is then unchanged.
    pub fn merge_region(
        &mut self,
        page_idx: usize,
        source_size: [u32; 2],
        rect: [u32; 4],
        region_alpha: &[u8],
        blocks: Option<Vec<[i32; 4]>>,
    ) -> Result<(), RegionMergeError> {
        check_region_rect(rect, source_size)?;
        if let Some(page) = self.pages.get_mut(&page_idx).filter(|page| page.source_size == source_size && !page.mask_alpha.is_empty()) {
            let mask_was_empty = page.mask_alpha.iter().all(|&px| px == 0);
            // Validated before any write: an error leaves the page untouched.
            replace_mask_region(&mut page.mask_alpha, page.mask_size, source_size, rect, region_alpha)?;
            page.blocks = merged_blocks(page.blocks.as_ref(), mask_was_empty, blocks);
            self.revision = self.revision.saturating_add(1);
            return Ok(());
        }
        // No reusable raster (missing, stale or mask-less page): a fresh source-size mask.
        {
            let len = pixel_count(source_size)
                .ok_or(RegionMergeError::PageMaskSize { len: 0, expected: usize::MAX, mask_size: source_size })?;
            let mut mask_alpha = vec![0u8; len];
            replace_mask_region(&mut mask_alpha, source_size, source_size, rect, region_alpha)?;
            // A stale page's boxes describe another image; a mask-less page's boxes are kept by
            // the rule below like any other page's.
            let old_blocks = self.pages.get(&page_idx).filter(|page| page.source_size == source_size).and_then(|page| page.blocks.clone());
            let next_blocks = merged_blocks(old_blocks.as_ref(), true, blocks);
            self.pages.insert(page_idx, TextMaskPage { source_size, mask_size: source_size, mask_alpha, blocks: next_blocks });
        }
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Replaces the whole mask payload for `page_idx` without any detector boxes.
    ///
    /// Equivalent to `set_page_with_blocks(.., None)`. Used by mask writers that have no
    /// detector box set for the page (e.g. the cleaning-side disk mask loader).
    pub fn set_page(
        &mut self,
        page_idx: usize,
        source_size: [u32; 2],
        mask_size: [u32; 2],
        mask_alpha: Vec<u8>,
    ) {
        self.set_page_with_blocks(page_idx, source_size, mask_size, mask_alpha, None);
    }

    /// Replaces the whole mask payload for `page_idx`, including detector boxes.
    ///
    /// `blocks` are the detector text boxes (`[x1, y1, x2, y2]`, source-page pixel
    /// space) that describe the same detection as `mask_alpha`, or `None` when no
    /// boxes are known. Callers must pass the boxes matching what the mask actually
    /// contains (raw detector boxes for a raw glyph mask; resolved/expanded/merged
    /// boxes for a mask rasterized from them). The write is skipped (no revision bump)
    /// when the stored page is byte-for-byte identical, blocks included.
    pub fn set_page_with_blocks(
        &mut self,
        page_idx: usize,
        source_size: [u32; 2],
        mask_size: [u32; 2],
        mask_alpha: Vec<u8>,
        blocks: Option<Vec<[i32; 4]>>,
    ) {
        let blocks = blocks.filter(|blocks| !blocks.is_empty());
        let next = TextMaskPage {
            source_size,
            mask_size,
            mask_alpha,
            blocks,
        };
        if self.pages.get(&page_idx).is_some_and(|prev| prev == &next) {
            return;
        }
        self.pages.insert(page_idx, next);
        self.revision = self.revision.saturating_add(1);
    }

    /// Edits the mask for `page_idx` in place, allocating the page lazily.
    ///
    /// The closure receives `(mask_alpha, mask_w, mask_h)` and returns `true` when it
    /// mutated the buffer. On a reported change the page's detector `blocks` are
    /// invalidated to `None` — a hand-edited mask no longer matches the detector boxes,
    /// so autoclean must fall back to the mask itself rather than a stale box set — and
    /// the revision is bumped. When the closure returns `false` the page (including its
    /// `blocks`) is left untouched and no revision bump occurs.
    pub fn edit_page_mask<F>(
        &mut self,
        page_idx: usize,
        source_size: [u32; 2],
        mask_size: [u32; 2],
        edit: F,
    ) -> bool
    where
        F: FnOnce(&mut [u8], usize, usize) -> bool,
    {
        let mask_w = usize::try_from(mask_size[0]).ok().unwrap_or(0);
        let mask_h = usize::try_from(mask_size[1]).ok().unwrap_or(0);
        if mask_w == 0 || mask_h == 0 {
            return false;
        }
        let expected_len = mask_w.saturating_mul(mask_h);
        let mut staged_mask = self
            .pages
            .get(&page_idx)
            .map(|page| page.mask_alpha.clone())
            .unwrap_or_else(|| vec![0u8; expected_len]);
        staged_mask.resize(expected_len, 0u8);
        if !edit(&mut staged_mask, mask_w, mask_h) {
            return false;
        }
        // Commit every field together only after the closure confirms a mutation.
        self.pages.insert(
            page_idx,
            TextMaskPage {
                source_size,
                mask_size,
                mask_alpha: staged_mask,
                blocks: None,
            },
        );
        self.revision = self.revision.saturating_add(1);
        true
    }

    pub fn remove_page(&mut self, page_idx: usize) {
        if self.pages.remove(&page_idx).is_some() {
            self.revision = self.revision.saturating_add(1);
        }
    }

    pub fn clear_all(&mut self) {
        if self.pages.is_empty() {
            return;
        }
        self.pages.clear();
        self.revision = self.revision.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_page_stores_blocks() {
        let mut model = TextMaskModel::new();
        let blocks = vec![[1, 2, 3, 4], [10, 20, 30, 40]];
        model.set_page_with_blocks(0, [64, 64], [64, 64], vec![0u8; 64 * 64], Some(blocks.clone()));
        let page = model.page(0).expect("page must exist after set_page");
        assert_eq!(page.blocks.as_deref(), Some(blocks.as_slice()));
    }

    #[test]
    fn set_page_without_blocks_stores_none() {
        let mut model = TextMaskModel::new();
        model.set_page(0, [8, 8], [8, 8], vec![0u8; 8 * 8]);
        assert_eq!(model.page(0).and_then(|p| p.blocks.clone()), None);
    }

    #[test]
    fn set_page_normalizes_empty_blocks_to_none() {
        let mut model = TextMaskModel::new();
        model.set_page_with_blocks(0, [8, 8], [8, 8], vec![0u8; 8 * 8], Some(Vec::new()));
        assert_eq!(model.page(0).and_then(|page| page.blocks.as_ref()), None);
    }

    #[test]
    fn edit_page_mask_mutating_closure_clears_blocks() {
        let mut model = TextMaskModel::new();
        model.set_page_with_blocks(0, [8, 8], [8, 8], vec![0u8; 8 * 8], Some(vec![[0, 0, 4, 4]]));
        let rev_before = model.revision();
        // A mutating edit reports a change, so the detector boxes must be dropped.
        let changed = model.edit_page_mask(0, [8, 8], [8, 8], |mask, _w, _h| {
            mask[0] = 255;
            true
        });
        assert!(changed);
        assert_eq!(model.page(0).and_then(|p| p.blocks.clone()), None);
        assert!(model.revision() > rev_before);
    }

    #[test]
    fn edit_page_mask_non_mutating_closure_keeps_blocks() {
        let mut model = TextMaskModel::new();
        let blocks = vec![[0, 0, 4, 4]];
        model.set_page_with_blocks(0, [8, 8], [8, 8], vec![0u8; 8 * 8], Some(blocks.clone()));
        let rev_before = model.revision();
        // A no-op edit reports no change, so the boxes stay and the revision is stable.
        let changed = model.edit_page_mask(0, [8, 8], [8, 8], |_mask, _w, _h| false);
        assert!(!changed);
        assert_eq!(model.page(0).and_then(|p| p.blocks.clone()), Some(blocks));
        assert_eq!(model.revision(), rev_before);
    }

    /// 4x4 page, region `[1, 1, 3, 3]`: inside takes the region mask, outside is untouched.
    #[test]
    fn replace_mask_region_replaces_inside_and_keeps_outside() {
        let mut mask = vec![255u8; 16];
        replace_mask_region(&mut mask, [4, 4], [4, 4], [1, 1, 3, 3], &[0, 9, 0, 0]).expect("valid merge");
        // Rows: untouched, [255, 0, 255, 255], [255, 0, 0, 255], untouched.
        let expected = vec![255, 255, 255, 255, 255, 0, 255, 255, 255, 0, 0, 255, 255, 255, 255, 255];
        assert_eq!(mask, expected);
    }

    #[test]
    fn replace_mask_region_with_empty_region_clears_the_rect() {
        let mut mask = vec![255u8; 9];
        replace_mask_region(&mut mask, [3, 3], [3, 3], [0, 0, 3, 1], &[]).expect("valid merge");
        assert_eq!(mask, vec![0, 0, 0, 255, 255, 255, 255, 255, 255]);
    }

    /// A half-resolution mask (2x2 for a 4x4 page): mask pixel (1, 1) has its centre at source
    /// (2, 2), inside `[2, 2, 4, 4]`; the other three stay.
    #[test]
    fn replace_mask_region_maps_a_scaled_mask_by_pixel_centres() {
        let mut mask = vec![7u8; 4];
        replace_mask_region(&mut mask, [2, 2], [4, 4], [2, 2, 4, 4], &[1, 1, 1, 1]).expect("valid merge");
        assert_eq!(mask, vec![7, 7, 7, 255]);
    }

    #[test]
    fn replace_mask_region_rejects_bad_rects_and_buffers_untouched() {
        let mut mask = vec![5u8; 16];
        assert!(matches!(
            replace_mask_region(&mut mask, [4, 4], [4, 4], [0, 0, 5, 4], &[]),
            Err(RegionMergeError::RectOutsidePage { .. })
        ));
        assert!(matches!(
            replace_mask_region(&mut mask, [4, 4], [4, 4], [2, 2, 2, 4], &[]),
            Err(RegionMergeError::RectOutsidePage { .. })
        ));
        assert!(matches!(
            replace_mask_region(&mut mask, [4, 4], [4, 4], [0, 0, 2, 2], &[1, 2, 3]),
            Err(RegionMergeError::RegionMaskSize { len: 3, expected: 4 })
        ));
        assert!(matches!(
            replace_mask_region(&mut mask[..15], [4, 4], [4, 4], [0, 0, 2, 2], &[]),
            Err(RegionMergeError::PageMaskSize { len: 15, expected: 16, .. })
        ));
        assert_eq!(mask, vec![5u8; 16]);
    }

    /// The model merge keeps the mask outside the rect, stores the caller's merged box set and
    /// bumps the revision.
    #[test]
    fn merge_region_replaces_inside_keeps_outside_and_bumps_revision() {
        let mut model = TextMaskModel::new();
        let mut mask = vec![0u8; 64];
        mask[0] = 255;
        mask[6 * 8 + 6] = 255;
        model.set_page_with_blocks(0, [8, 8], [8, 8], mask, Some(vec![[0, 0, 2, 2], [5, 5, 7, 7]]));
        let revision = model.revision();
        model.merge_region(0, [8, 8], [4, 4, 8, 8], &[255; 16], Some(vec![[0, 0, 2, 2], [4, 4, 6, 6]])).expect("valid merge");
        let page = model.page(0).expect("page kept");
        assert_eq!(page.mask_alpha[0], 255, "outside the rect untouched");
        assert!(page.mask_alpha[4 * 8 + 4..4 * 8 + 8].iter().all(|&px| px == 255), "inside replaced");
        assert_eq!(page.mask_alpha[3 * 8 + 3], 0);
        assert_eq!(page.blocks, Some(vec![[0, 0, 2, 2], [4, 4, 6, 6]]));
        assert!(model.revision() > revision);
    }

    #[test]
    fn merge_region_creates_a_missing_page_at_the_source_size() {
        let mut model = TextMaskModel::new();
        model.merge_region(3, [4, 2], [2, 0, 4, 2], &[255, 0, 0, 255], Some(vec![[2, 0, 3, 1]])).expect("valid merge");
        let page = model.page(3).expect("page created");
        assert_eq!((page.source_size, page.mask_size), ([4, 2], [4, 2]));
        assert_eq!(page.mask_alpha, vec![0, 0, 255, 0, 0, 0, 0, 255]);
        assert_eq!(page.blocks, Some(vec![[2, 0, 3, 1]]));
    }

    /// A hand-edited page (mask content, unknown boxes) keeps unknown boxes; an empty region
    /// result clears the rect.
    #[test]
    fn merge_region_keeps_unknown_boxes_of_a_hand_edited_mask() {
        let mut model = TextMaskModel::new();
        model.set_page(0, [4, 4], [4, 4], vec![255u8; 16]);
        model.merge_region(0, [4, 4], [0, 0, 2, 4], &[], Some(vec![[0, 0, 1, 1]])).expect("valid merge");
        let page = model.page(0).expect("page kept");
        assert_eq!(page.blocks, None);
        assert_eq!(page.mask_alpha, vec![0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn merge_region_error_leaves_the_model_unchanged() {
        let mut model = TextMaskModel::new();
        model.set_page(0, [4, 4], [4, 4], vec![255u8; 16]);
        let revision = model.revision();
        let before = model.page(0).cloned();
        assert!(model.merge_region(0, [4, 4], [0, 0, 2, 2], &[1, 2], None).is_err());
        assert_eq!(model.page(0).cloned(), before);
        assert_eq!(model.revision(), revision);
    }

    #[test]
    fn edit_page_mask_non_mutating_dimension_change_leaves_page_untouched() {
        let mut model = TextMaskModel::new();
        model.set_page_with_blocks(
            0,
            [8, 8],
            [8, 8],
            vec![7u8; 8 * 8],
            Some(vec![[0, 0, 4, 4]]),
        );
        let page_before = model.page(0).cloned().expect("page must exist");
        let revision_before = model.revision();

        let changed = model.edit_page_mask(0, [16, 12], [4, 6], |_mask, _w, _h| false);

        assert!(!changed);
        assert_eq!(model.page(0), Some(&page_before));
        assert_eq!(model.revision(), revision_before);
    }
}
