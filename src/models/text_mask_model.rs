/*
FILE OVERVIEW: src/models/text_mask_model.rs
Shared runtime model for text-detector masks, synchronized between tabs.

Main items:
- `TextMaskPage`: per-page mask payload (source size + mask size + alpha).
- `TextMaskModel`: mutable shared store with monotonic `revision`.
- `edit_page_mask`: in-place editable mask API with lazy page allocation.

Usage pattern:
- Translation tab writes page masks after detector/load events.
- Other tabs read masks by page index and track `revision` to refresh local caches.
*/

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMaskPage {
    pub source_size: [u32; 2],
    pub mask_size: [u32; 2],
    pub mask_alpha: Vec<u8>,
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

    pub fn set_page(
        &mut self,
        page_idx: usize,
        source_size: [u32; 2],
        mask_size: [u32; 2],
        mask_alpha: Vec<u8>,
    ) {
        let next = TextMaskPage {
            source_size,
            mask_size,
            mask_alpha,
        };
        if self.pages.get(&page_idx).is_some_and(|prev| prev == &next) {
            return;
        }
        self.pages.insert(page_idx, next);
        self.revision = self.revision.saturating_add(1);
    }

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
        let page = self.pages.entry(page_idx).or_insert_with(|| TextMaskPage {
            source_size,
            mask_size,
            mask_alpha: vec![0u8; expected_len],
        });
        if page.source_size != source_size || page.mask_size != mask_size {
            page.source_size = source_size;
            page.mask_size = mask_size;
            page.mask_alpha.resize(expected_len, 0u8);
        } else if page.mask_alpha.len() != expected_len {
            page.mask_alpha.resize(expected_len, 0u8);
        }
        if !edit(&mut page.mask_alpha, mask_w, mask_h) {
            return false;
        }
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
