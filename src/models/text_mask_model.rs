/*
FILE OVERVIEW: src/models/text_mask_model.rs
Shared runtime model for text-detector masks, synchronized between tabs.

Main items:
- `TextMaskPage`: per-page mask payload (source size + mask size + alpha + optional detector blocks).
- `TextMaskModel`: mutable shared store with monotonic `revision`.
- `edit_page_mask`: in-place editable mask API with lazy page allocation.

Usage pattern:
- Translation tab writes page masks (and, when available, the detector blocks that
  describe the same detection) after detector/load events.
- Other tabs read masks by page index and track `revision` to refresh local caches.
  Autoclean reads `TextMaskPage.blocks` to build a box-based mask candidate.
*/

use std::collections::HashMap;

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
