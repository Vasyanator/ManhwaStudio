/*
FILE OVERVIEW: src/models/clean_overlays_model.rs
Shared runtime model for clean overlays and action-side page cache.

Main items:
- `OverlayDelta`: incremental overlay/visibility changes for canvas subscribers.
- `OverlayHistoryEntry`: one committed overlay edit stored as a minimal rectangular RGBA delta.
- `CleanOverlaysModel`: shared storage for per-page clean overlays and cached source pages.

Core behavior:
- Keeps clean overlays in two synchronized CPU forms:
  `ColorImage` for canvas/UI sync and `image::RgbaImage` for export/save paths that must not depend
  on display textures or lose unsaved edits.
- Can accept a prebuilt overlay pair (`Arc<RgbaImage>` + `ColorImage`) from background workers,
  so expensive conversion work does not have to happen on the GUI thread.
- Stores optional source-page cache (`image::RgbaImage`) in the model, so heavy action images are
  shared between tabs instead of duplicated per-view.
- Bounds that source-page cache by explicit byte/item policy, LRU order, and optional page-window
  pins so project-wide memory policy can trim reconstructable decoded pages without affecting
  dirty overlay data.
- Keeps pages with no clean layer virtual (`None`) and treats fully transparent loaded clean layers
  as absent when they were not user edits, avoiding full transparent CPU images until tools
  materialize the page.
- Keeps undo/redo history for cleaning commits as per-page signed channel deltas, so history does
  not duplicate full images and redo branch is discarded after a new head commit.
- Supports "cache pages immediately" mode via `cache_pages_enabled`; when disabled, page cache can
  still be populated lazily for specific pages when needed by tools.
*/

use egui::ColorImage;
use image::RgbaImage;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, MemoryBudget, MemoryPressure, MemoryProfile,
};

/// Threshold below which LRU eviction of page cache entries is triggered (2 GiB).
const PAGE_CACHE_EVICT_FREE_RAM_THRESHOLD: u64 = 2 * 1024 * 1024 * 1024;
const OVERLAY_HISTORY_LIMIT: usize = 128;
const DEFAULT_PAGE_CACHE_BYTE_LIMIT: u64 = 512 * 1024 * 1024;
const DEFAULT_PAGE_CACHE_ITEM_LIMIT: usize = 64;
const LOW_PAGE_CACHE_ITEM_LIMIT: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageCacheWindow {
    pub center_idx: usize,
    pub radius: usize,
}

#[derive(Debug, Clone)]
pub struct PageCachePolicy {
    pub byte_limit: Option<u64>,
    pub item_limit: Option<usize>,
    pub pinned_window: Option<PageCacheWindow>,
}

impl Default for PageCachePolicy {
    fn default() -> Self {
        Self {
            byte_limit: Some(DEFAULT_PAGE_CACHE_BYTE_LIMIT),
            item_limit: Some(DEFAULT_PAGE_CACHE_ITEM_LIMIT),
            pinned_window: None,
        }
    }
}

impl PageCachePolicy {
    #[must_use]
    pub fn for_profile(profile: MemoryProfile, page_count: usize) -> Self {
        let budget = MemoryBudget::for_profile(profile);
        match profile {
            MemoryProfile::Minimal => Self {
                byte_limit: Some(budget.source_page_cpu_cache_bytes),
                item_limit: Some(0),
                pinned_window: None,
            },
            MemoryProfile::Low => Self {
                byte_limit: Some(budget.source_page_cpu_cache_bytes),
                item_limit: Some(LOW_PAGE_CACHE_ITEM_LIMIT.min(page_count)),
                pinned_window: None,
            },
            MemoryProfile::Medium => Self {
                byte_limit: Some(budget.source_page_cpu_cache_bytes),
                item_limit: Some(DEFAULT_PAGE_CACHE_ITEM_LIMIT.min(page_count)),
                pinned_window: None,
            },
            MemoryProfile::Maximum => Self {
                byte_limit: Some(budget.source_page_cpu_cache_bytes),
                item_limit: Some(page_count),
                pinned_window: None,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct OverlayDelta {
    pub revision: u64,
    pub visibility: Option<bool>,
    pub changed: Vec<(usize, Option<ColorImage>)>,
}

#[derive(Debug, Clone)]
struct OverlayHistoryEntry {
    page_idx: usize,
    origin_px: [usize; 2],
    size_px: [usize; 2],
    rgba_deltas: Vec<[i16; 4]>,
}

#[derive(Debug)]
pub struct CleanOverlaysModel {
    overlays: Vec<Option<ColorImage>>,
    overlay_rgba_cache: Vec<Option<Arc<RgbaImage>>>,
    page_cache: Vec<Option<Arc<RgbaImage>>>,
    /// LRU access order for `page_cache` entries; least recently used index is at the front.
    page_cache_lru: Vec<usize>,
    page_cache_last_used: Vec<u64>,
    page_cache_clock: u64,
    page_cache_policy: PageCachePolicy,
    cache_pages_enabled: bool,
    basenames: Vec<String>,
    sizes: Vec<[usize; 2]>,
    visible: bool,
    updates_lock: usize,
    revision: u64,
    dirty_indexes: HashSet<usize>,
    save_dirty_indexes: HashSet<usize>,
    has_project_unsaved_changes: bool,
    visibility_dirty: bool,
    undo_history: Vec<OverlayHistoryEntry>,
    redo_history: Vec<OverlayHistoryEntry>,
}

#[allow(dead_code)]
impl CleanOverlaysModel {
    pub fn new_from_pages(pages: &[PathBuf]) -> Self {
        let mut sorted_pages = pages.to_vec();
        sorted_pages.sort_by_key(|a| numeric_first_key(a));

        let mut basenames = Vec::with_capacity(sorted_pages.len());
        for page in &sorted_pages {
            let basename = page
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            basenames.push(basename);
        }

        Self {
            overlays: vec![None; sorted_pages.len()],
            overlay_rgba_cache: vec![None; sorted_pages.len()],
            page_cache: vec![None; sorted_pages.len()],
            page_cache_lru: Vec::new(),
            page_cache_last_used: vec![0; sorted_pages.len()],
            page_cache_clock: 0,
            page_cache_policy: PageCachePolicy::default(),
            cache_pages_enabled: false,
            basenames,
            sizes: vec![[0, 0]; sorted_pages.len()],
            visible: true,
            updates_lock: 0,
            revision: 1,
            dirty_indexes: HashSet::new(),
            save_dirty_indexes: HashSet::new(),
            has_project_unsaved_changes: false,
            visibility_dirty: true,
            undo_history: Vec::new(),
            redo_history: Vec::new(),
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn count(&self) -> usize {
        self.overlays.len()
    }

    pub fn get(&self, idx: usize) -> Option<&ColorImage> {
        self.overlays.get(idx).and_then(|x| x.as_ref())
    }

    pub fn overlay_rgba(&self, idx: usize) -> Option<Arc<RgbaImage>> {
        self.overlay_rgba_cache
            .get(idx)
            .and_then(|item| item.as_ref())
            .cloned()
    }

    pub fn is_overlay_virtual_absent(&self, idx: usize) -> bool {
        idx < self.overlays.len()
            && self.overlays[idx].is_none()
            && self.overlay_rgba_cache[idx].is_none()
            && !self.save_dirty_indexes.contains(&idx)
    }

    pub fn save_snapshots(&self) -> Vec<(String, Arc<RgbaImage>)> {
        let mut out = Vec::new();
        for (idx, cached) in self.overlay_rgba_cache.iter().enumerate() {
            let Some(image) = cached.as_ref() else {
                continue;
            };
            if image.width() == 0 || image.height() == 0 {
                continue;
            }
            let stem = self
                .basenames
                .get(idx)
                .and_then(|n| Path::new(n).file_stem().and_then(|s| s.to_str()))
                .unwrap_or("overlay")
                .to_string();
            out.push((stem, Arc::clone(image)));
        }
        out
    }

    pub fn cache_pages_enabled(&self) -> bool {
        self.cache_pages_enabled
    }

    pub fn set_cache_pages_enabled(&mut self, enabled: bool) {
        self.cache_pages_enabled = enabled;
        if !enabled {
            self.evict_page_cache_lru_until(u64::MAX, &BTreeSet::new(), false);
        }
    }

    pub fn set_memory_profile(&mut self, profile: MemoryProfile) {
        self.set_page_cache_policy(PageCachePolicy::for_profile(profile, self.page_cache.len()));
    }

    pub fn set_page_cache_policy(&mut self, policy: PageCachePolicy) {
        self.page_cache_policy = policy;
        self.enforce_page_cache_policy();
    }

    pub fn configure_page_cache_limits(
        &mut self,
        byte_limit: Option<u64>,
        item_limit: Option<usize>,
    ) {
        self.page_cache_policy.byte_limit = byte_limit;
        self.page_cache_policy.item_limit = item_limit;
        self.enforce_page_cache_policy();
    }

    pub fn pin_page_cache_window(&mut self, center_idx: usize, radius: usize) {
        self.page_cache_policy.pinned_window = Some(PageCacheWindow { center_idx, radius });
        self.enforce_page_cache_policy();
    }

    pub fn clear_page_cache_window_pin(&mut self) {
        self.page_cache_policy.pinned_window = None;
        self.enforce_page_cache_policy();
    }

    pub fn page_cache_policy(&self) -> &PageCachePolicy {
        &self.page_cache_policy
    }

    pub fn page_cache_estimated_bytes(&self) -> u64 {
        self.page_cache
            .iter()
            .filter_map(|item| item.as_deref())
            .map(rgba_image_bytes)
            .sum()
    }

    pub fn memory_usage_snapshot(&self) -> Vec<CacheResourceInfo> {
        let mut out = Vec::new();
        for (idx, image) in self.page_cache.iter().enumerate() {
            let Some(image) = image.as_deref() else {
                continue;
            };
            out.push(CacheResourceInfo {
                id: format!("source-page-cpu:{idx}"),
                kind: CacheResourceKind::SourcePageCpu,
                page_idx: Some(idx),
                estimated_bytes: rgba_image_bytes(image),
                last_used_frame: self.page_cache_last_used.get(idx).copied().unwrap_or(0),
                reload_cost: CacheReloadCost::DecodeFromDisk,
                dirty: false,
                visible: self.is_page_cache_pinned(
                    idx,
                    self.page_cache_policy.pinned_window,
                    &BTreeSet::new(),
                ),
                reconstructable: true,
            });
        }
        for (idx, image) in self.overlay_rgba_cache.iter().enumerate() {
            let Some(image) = image.as_deref() else {
                continue;
            };
            let dirty = self.save_dirty_indexes.contains(&idx);
            out.push(CacheResourceInfo {
                id: format!("clean-overlay-cpu:{idx}"),
                kind: CacheResourceKind::CleanOverlayCpu,
                page_idx: Some(idx),
                estimated_bytes: rgba_image_bytes(image),
                last_used_frame: u64::MAX,
                reload_cost: if dirty {
                    CacheReloadCost::Expensive
                } else {
                    CacheReloadCost::DecodeFromDisk
                },
                dirty,
                visible: false,
                reconstructable: !dirty,
            });
        }
        out
    }

    pub fn evict_cache(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let mut report = CacheEvictionReport {
            resources: Vec::new(),
            estimated_freed_bytes: 0,
        };
        let target_bytes = match request.pressure {
            MemoryPressure::Normal if request.target_free_bytes == 0 => 0,
            MemoryPressure::Normal => request.target_free_bytes,
            MemoryPressure::Soft => request.target_free_bytes,
            MemoryPressure::Hard => request
                .target_free_bytes
                .max(self.page_cache_estimated_bytes() / 2),
            MemoryPressure::Critical => u64::MAX,
        };
        let protected_pages = self
            .combined_protected_pages(self.page_cache_policy.pinned_window, &request.pinned_pages);
        self.evict_page_cache_lru_until_with_report(
            target_bytes,
            &protected_pages,
            true,
            &mut report,
        );
        report
    }

    pub fn cached_page_rgba(&mut self, idx: usize) -> Option<Arc<RgbaImage>> {
        let image = self
            .page_cache
            .get(idx)
            .and_then(|item| item.as_ref())
            .cloned();
        if image.is_some() {
            self.lru_touch(idx);
        }
        image
    }

    pub fn has_cached_page_rgba(&self, idx: usize) -> bool {
        self.page_cache
            .get(idx)
            .and_then(|item| item.as_ref())
            .is_some()
    }

    pub fn store_cached_page_rgba(&mut self, idx: usize, image: RgbaImage) -> bool {
        self.store_cached_page_rgba_arc(idx, Arc::new(image))
    }

    pub fn store_cached_page_rgba_arc(&mut self, idx: usize, image: Arc<RgbaImage>) -> bool {
        if idx >= self.page_cache.len() {
            return false;
        }
        self.page_cache[idx] = Some(image);
        self.lru_touch(idx);
        self.enforce_page_cache_policy();
        self.maybe_evict_page_cache();
        true
    }

    /// Mark `idx` as most recently used in the LRU order.
    fn lru_touch(&mut self, idx: usize) {
        self.page_cache_clock = self.page_cache_clock.saturating_add(1);
        if let Some(last_used) = self.page_cache_last_used.get_mut(idx) {
            *last_used = self.page_cache_clock;
        }
        self.page_cache_lru.retain(|&i| i != idx);
        self.page_cache_lru.push(idx);
    }

    /// If free system RAM is below the threshold, evict the least recently used cached pages
    /// until memory pressure is relieved.
    fn maybe_evict_page_cache(&mut self) {
        if free_memory_bytes() >= PAGE_CACHE_EVICT_FREE_RAM_THRESHOLD {
            return;
        }
        let protected_pages =
            self.combined_protected_pages(self.page_cache_policy.pinned_window, &BTreeSet::new());
        self.evict_page_cache_lru_until(u64::MAX, &protected_pages, true);
    }

    fn enforce_page_cache_policy(&mut self) {
        let protected_pages =
            self.combined_protected_pages(self.page_cache_policy.pinned_window, &BTreeSet::new());
        if let Some(item_limit) = self.page_cache_policy.item_limit {
            while self.page_cache_item_count() > item_limit {
                if self
                    .evict_one_page_cache_lru(&protected_pages, true)
                    .is_none()
                {
                    break;
                }
            }
        }
        if let Some(byte_limit) = self.page_cache_policy.byte_limit {
            while self.page_cache_estimated_bytes() > byte_limit {
                if self
                    .evict_one_page_cache_lru(&protected_pages, true)
                    .is_none()
                {
                    break;
                }
            }
        }
    }

    fn page_cache_item_count(&self) -> usize {
        self.page_cache.iter().filter(|item| item.is_some()).count()
    }

    fn evict_page_cache_lru_until(
        &mut self,
        bytes_to_free: u64,
        protected_pages: &BTreeSet<usize>,
        respect_policy_pins: bool,
    ) -> u64 {
        let mut report = CacheEvictionReport::default();
        self.evict_page_cache_lru_until_with_report(
            bytes_to_free,
            protected_pages,
            respect_policy_pins,
            &mut report,
        );
        report.estimated_freed_bytes
    }

    fn evict_page_cache_lru_until_with_report(
        &mut self,
        bytes_to_free: u64,
        protected_pages: &BTreeSet<usize>,
        respect_policy_pins: bool,
        report: &mut CacheEvictionReport,
    ) {
        while bytes_to_free == u64::MAX || report.estimated_freed_bytes < bytes_to_free {
            let Some(evicted) = self.evict_one_page_cache_lru(protected_pages, respect_policy_pins)
            else {
                break;
            };
            report.estimated_freed_bytes = report
                .estimated_freed_bytes
                .saturating_add(evicted.estimated_bytes);
            report.resources.push(evicted);
        }
    }

    fn evict_one_page_cache_lru(
        &mut self,
        protected_pages: &BTreeSet<usize>,
        respect_policy_pins: bool,
    ) -> Option<CacheResourceInfo> {
        let mut skipped = Vec::new();
        while let Some(idx) = self.page_cache_lru.first().copied() {
            self.page_cache_lru.remove(0);
            if self
                .page_cache
                .get(idx)
                .and_then(|item| item.as_ref())
                .is_none()
            {
                continue;
            }
            if self.is_page_cache_pinned(idx, None, protected_pages)
                || (respect_policy_pins
                    && self.is_page_cache_pinned(
                        idx,
                        self.page_cache_policy.pinned_window,
                        protected_pages,
                    ))
            {
                skipped.push(idx);
                continue;
            }
            let Some(slot) = self.page_cache.get_mut(idx) else {
                continue;
            };
            let Some(image) = slot.take() else {
                continue;
            };
            for skipped_idx in skipped {
                self.page_cache_lru.push(skipped_idx);
            }
            return Some(CacheResourceInfo {
                id: format!("source-page-cpu:{idx}"),
                kind: CacheResourceKind::SourcePageCpu,
                page_idx: Some(idx),
                estimated_bytes: rgba_image_bytes(&image),
                last_used_frame: self.page_cache_last_used.get(idx).copied().unwrap_or(0),
                reload_cost: CacheReloadCost::DecodeFromDisk,
                dirty: false,
                visible: false,
                reconstructable: true,
            });
        }
        for skipped_idx in skipped {
            self.page_cache_lru.push(skipped_idx);
        }
        None
    }

    fn combined_protected_pages(
        &self,
        page_window: Option<PageCacheWindow>,
        protected_pages: &BTreeSet<usize>,
    ) -> BTreeSet<usize> {
        let mut out = protected_pages.clone();
        if let Some(window) = page_window {
            let start = window.center_idx.saturating_sub(window.radius);
            let end = window
                .center_idx
                .saturating_add(window.radius)
                .min(self.page_cache.len().saturating_sub(1));
            out.extend(start..=end);
        }
        out
    }

    fn is_page_cache_pinned(
        &self,
        idx: usize,
        window: Option<PageCacheWindow>,
        protected_pages: &BTreeSet<usize>,
    ) -> bool {
        if protected_pages.contains(&idx) {
            return true;
        }
        let Some(window) = window else {
            return false;
        };
        idx.abs_diff(window.center_idx) <= window.radius
    }

    pub fn replace(&mut self, idx: usize, img: &ColorImage) {
        let mut img = img.clone();
        if idx >= self.overlays.len() || img.size[0] == 0 || img.size[1] == 0 {
            return;
        }
        let [known_w, known_h] = self.sizes[idx];
        if known_w > 0 && known_h > 0 && img.size != [known_w, known_h] {
            img = resize_nearest(&img, known_w, known_h);
        }
        if self.sizes[idx][0] == 0 || self.sizes[idx][1] == 0 {
            self.sizes[idx] = img.size;
        }
        let rgba = Arc::new(color_image_to_rgba(&img));
        self.apply_overlay_snapshot(idx, img, rgba, true);
    }

    pub fn ensure_overlay(&mut self, idx: usize, size: [usize; 2]) -> bool {
        let Some(target_size) = self.normalized_overlay_size(idx, size) else {
            return false;
        };
        if self.ensure_overlay_storage(idx, target_size) {
            self.mark_dirty(idx);
        }
        true
    }

    // Parameters represent distinct required inputs with no natural grouping.
    #[allow(clippy::too_many_arguments)]
    pub fn replace_region(
        &mut self,
        idx: usize,
        size: [usize; 2],
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        chunk: &ColorImage,
    ) -> bool {
        if w == 0 || h == 0 || chunk.size[0] == 0 || chunk.size[1] == 0 {
            return false;
        }
        let Some(target_size) = self.normalized_overlay_size(idx, size) else {
            return false;
        };
        self.ensure_overlay_storage(idx, target_size);
        let x0 = x.min(target_size[0]);
        let y0 = y.min(target_size[1]);
        let x1 = x.saturating_add(w).min(target_size[0]);
        let y1 = y.saturating_add(h).min(target_size[1]);
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        let target_w = x1.saturating_sub(x0);
        let target_h = y1.saturating_sub(y0);
        let history_entry = self.build_region_history_entry(idx, x0, y0, target_w, target_h, chunk);
        let Some(overlay) = self.overlays.get_mut(idx).and_then(|item| item.as_mut()) else {
            return false;
        };
        blit_scaled_chunk_color_image(overlay, x0, y0, target_w, target_h, chunk);
        let Some(cache) = self
            .overlay_rgba_cache
            .get_mut(idx)
            .and_then(|item| item.as_mut())
        else {
            return false;
        };
        let rgba = Arc::make_mut(cache);
        blit_scaled_chunk_rgba(rgba, x0, y0, target_w, target_h, chunk);
        if let Some(entry) = history_entry {
            self.push_undo_history(entry);
        }
        self.mark_dirty(idx);
        true
    }

    pub fn replace_from_rgba(&mut self, idx: usize, mut image: RgbaImage) {
        if idx >= self.overlays.len() || image.width() == 0 || image.height() == 0 {
            return;
        }
        let [known_w, known_h] = self.sizes[idx];
        if known_w > 0
            && known_h > 0
            && (image.width() as usize != known_w || image.height() as usize != known_h)
        {
            image = image::imageops::resize(
                &image,
                known_w as u32,
                known_h as u32,
                image::imageops::FilterType::Nearest,
            );
        }
        let size = [image.width() as usize, image.height() as usize];
        if self.sizes[idx][0] == 0 || self.sizes[idx][1] == 0 {
            self.sizes[idx] = size;
        }
        let color_image = ColorImage::from_rgba_unmultiplied(size, image.as_raw());
        self.apply_overlay_snapshot(idx, color_image, Arc::new(image), true);
    }

    pub fn replace_prepared_overlay(
        &mut self,
        idx: usize,
        image: Arc<RgbaImage>,
        color_image: ColorImage,
    ) {
        self.replace_prepared_overlay_impl(idx, image, color_image, true);
    }

    pub fn load_prepared_overlay(
        &mut self,
        idx: usize,
        image: Arc<RgbaImage>,
        color_image: ColorImage,
    ) {
        self.replace_prepared_overlay_impl(idx, image, color_image, false);
    }

    fn replace_prepared_overlay_impl(
        &mut self,
        idx: usize,
        image: Arc<RgbaImage>,
        color_image: ColorImage,
        record_history: bool,
    ) {
        if idx >= self.overlays.len() || color_image.size[0] == 0 || color_image.size[1] == 0 {
            return;
        }
        if !record_history && rgba_is_fully_transparent(image.as_ref()) {
            if self.sizes[idx][0] == 0 || self.sizes[idx][1] == 0 {
                self.sizes[idx] = color_image.size;
            }
            let had_materialized_overlay =
                self.overlays[idx].is_some() || self.overlay_rgba_cache[idx].is_some();
            self.overlays[idx] = None;
            self.overlay_rgba_cache[idx] = None;
            if had_materialized_overlay {
                self.mark_runtime_changed(idx);
            }
            return;
        }
        let [known_w, known_h] = self.sizes[idx];
        if known_w > 0 && known_h > 0 && color_image.size != [known_w, known_h] {
            match Arc::try_unwrap(image) {
                Ok(mut image) => {
                    image = image::imageops::resize(
                        &image,
                        known_w as u32,
                        known_h as u32,
                        image::imageops::FilterType::Nearest,
                    );
                    let color =
                        ColorImage::from_rgba_unmultiplied([known_w, known_h], image.as_raw());
                    self.apply_overlay_snapshot(idx, color, Arc::new(image), record_history);
                }
                Err(shared) => {
                    let resized = image::imageops::resize(
                        shared.as_ref(),
                        known_w as u32,
                        known_h as u32,
                        image::imageops::FilterType::Nearest,
                    );
                    let color =
                        ColorImage::from_rgba_unmultiplied([known_w, known_h], resized.as_raw());
                    self.apply_overlay_snapshot(idx, color, Arc::new(resized), record_history);
                }
            }
            return;
        }
        if self.sizes[idx][0] == 0 || self.sizes[idx][1] == 0 {
            self.sizes[idx] = color_image.size;
        }
        self.apply_overlay_snapshot(idx, color_image, image, record_history);
    }

    pub fn clear(&mut self, idx: usize) {
        if idx >= self.overlays.len() {
            return;
        }
        let [w, h] = self.sizes[idx];
        if w == 0 || h == 0 {
            self.overlays[idx] = None;
            self.overlay_rgba_cache[idx] = None;
        } else {
            self.apply_overlay_snapshot(
                idx,
                transparent_overlay(w, h),
                Arc::new(RgbaImage::new(w as u32, h as u32)),
                true,
            );
            return;
        }
        self.mark_dirty(idx);
    }

    pub fn can_undo_overlay_history(&self) -> bool {
        !self.undo_history.is_empty()
    }

    pub fn can_redo_overlay_history(&self) -> bool {
        !self.redo_history.is_empty()
    }

    pub fn undo_overlay_history(&mut self) -> bool {
        let Some(entry) = self.undo_history.pop() else {
            return false;
        };
        if !self.apply_history_entry(&entry, HistoryDirection::Undo) {
            self.undo_history.push(entry);
            return false;
        }
        self.redo_history.push(entry);
        true
    }

    pub fn redo_overlay_history(&mut self) -> bool {
        let Some(entry) = self.redo_history.pop() else {
            return false;
        };
        if !self.apply_history_entry(&entry, HistoryDirection::Redo) {
            self.redo_history.push(entry);
            return false;
        }
        self.undo_history.push(entry);
        true
    }

    pub fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.visibility_dirty = true;
        self.bump_revision_unless_locked();
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn lock_updates(&mut self) {
        self.updates_lock = self.updates_lock.saturating_add(1);
    }

    pub fn unlock_updates(&mut self) {
        if self.updates_lock > 0 {
            self.updates_lock -= 1;
        }
    }

    pub fn updates_locked(&self) -> bool {
        self.updates_lock > 0
    }

    pub fn take_delta(&mut self, known_revision: u64) -> Option<OverlayDelta> {
        if known_revision == self.revision {
            return None;
        }

        let mut changed: Vec<(usize, Option<ColorImage>)> = Vec::new();
        let mut indexes: Vec<usize> = self.dirty_indexes.drain().collect();
        indexes.sort_unstable();
        for idx in indexes {
            let item = self.overlays.get(idx).cloned().unwrap_or(None);
            changed.push((idx, item));
        }

        let visibility = if self.visibility_dirty {
            self.visibility_dirty = false;
            Some(self.visible)
        } else {
            None
        };

        Some(OverlayDelta {
            revision: self.revision,
            visibility,
            changed,
        })
    }

    pub fn save_all(&self, clean_layers_dir: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(clean_layers_dir)?;
        for (stem, image) in self.save_snapshots() {
            let dst = clean_layers_dir.join(format!("{stem}.png"));
            image.save(&dst)?;
        }
        Ok(())
    }

    /// Saves only the pages that have been modified since the last `save_dirty_to` call
    /// (tracked via `save_dirty_indexes`). Clears autosave dirty tracking after writing.
    /// The destination directory is created if it does not yet exist.
    pub fn save_dirty_to(&mut self, dir: &Path) -> anyhow::Result<()> {
        let snapshots = self.take_dirty_save_snapshots();
        if let Err(err) = save_overlay_snapshots_to(dir, &snapshots) {
            self.restore_dirty_save_indexes(snapshots.iter().map(|(idx, _, _)| *idx));
            return Err(err);
        }
        Ok(())
    }

    /// Returns true when there are overlay pages modified since the last `save_dirty_to`.
    pub fn has_unsaved_overlay_changes(&self) -> bool {
        !self.save_dirty_indexes.is_empty()
    }

    pub fn has_project_unsaved_changes(&self) -> bool {
        self.has_project_unsaved_changes
    }

    pub fn mark_saved_to_project(&mut self) {
        self.has_project_unsaved_changes = false;
    }

    pub fn take_dirty_save_snapshots(&mut self) -> Vec<(usize, String, Arc<RgbaImage>)> {
        let dirty: Vec<usize> = self.save_dirty_indexes.drain().collect();
        dirty
            .into_iter()
            .filter_map(|idx| {
                let image = self.overlay_rgba_cache.get(idx).and_then(|x| x.as_ref())?;
                if image.width() == 0 || image.height() == 0 {
                    return None;
                }
                let stem = self
                    .basenames
                    .get(idx)
                    .and_then(|n| std::path::Path::new(n).file_stem().and_then(|s| s.to_str()))
                    .unwrap_or("overlay")
                    .to_string();
                Some((idx, stem, Arc::clone(image)))
            })
            .collect()
    }

    pub fn restore_dirty_save_indexes<I>(&mut self, indexes: I)
    where
        I: IntoIterator<Item = usize>,
    {
        self.save_dirty_indexes.extend(indexes);
    }

    fn mark_dirty(&mut self, idx: usize) {
        self.dirty_indexes.insert(idx);
        self.save_dirty_indexes.insert(idx);
        self.has_project_unsaved_changes = true;
        self.bump_revision_unless_locked();
    }

    fn mark_runtime_changed(&mut self, idx: usize) {
        self.dirty_indexes.insert(idx);
        self.bump_revision_unless_locked();
    }

    fn bump_revision_unless_locked(&mut self) {
        if self.updates_lock == 0 {
            self.revision = self.revision.saturating_add(1);
        }
    }

    fn normalized_overlay_size(&mut self, idx: usize, size: [usize; 2]) -> Option<[usize; 2]> {
        if idx >= self.overlays.len() || size[0] == 0 || size[1] == 0 {
            return None;
        }
        let [known_w, known_h] = self.sizes[idx];
        let target = if known_w > 0 && known_h > 0 {
            [known_w, known_h]
        } else {
            size
        };
        if self.sizes[idx] == [0, 0] {
            self.sizes[idx] = target;
        }
        Some(target)
    }

    fn ensure_overlay_storage(&mut self, idx: usize, size: [usize; 2]) -> bool {
        let overlay_reset = self
            .overlays
            .get(idx)
            .and_then(|item| item.as_ref())
            .is_none_or(|image| image.size != size);
        if overlay_reset {
            self.overlays[idx] = Some(transparent_overlay(size[0], size[1]));
        }

        let cache_reset = self
            .overlay_rgba_cache
            .get(idx)
            .and_then(|item| item.as_ref())
            .is_none_or(|image| {
                image.width() as usize != size[0] || image.height() as usize != size[1]
            });
        if cache_reset {
            let rgba = self.overlays[idx]
                .as_ref()
                .filter(|image| image.size == size)
                .map(color_image_to_rgba)
                .unwrap_or_else(|| RgbaImage::new(size[0] as u32, size[1] as u32));
            self.overlay_rgba_cache[idx] = Some(Arc::new(rgba));
        }

        overlay_reset
    }

    fn apply_overlay_snapshot(
        &mut self,
        idx: usize,
        color_image: ColorImage,
        rgba_image: Arc<RgbaImage>,
        record_history: bool,
    ) {
        if record_history
            && let Some(entry) = self.build_full_image_history_entry(
                idx,
                self.overlays.get(idx).and_then(|item| item.as_ref()),
                &color_image,
            )
        {
            self.push_undo_history(entry);
        }
        self.overlay_rgba_cache[idx] = Some(rgba_image);
        self.overlays[idx] = Some(color_image);
        if record_history {
            self.mark_dirty(idx);
        } else {
            self.mark_runtime_changed(idx);
        }
    }

    fn build_full_image_history_entry(
        &self,
        idx: usize,
        before: Option<&ColorImage>,
        after: &ColorImage,
    ) -> Option<OverlayHistoryEntry> {
        let width = after.size[0];
        let height = after.size[1];
        if width == 0 || height == 0 {
            return None;
        }
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut changed = false;
        for y in 0..height {
            for x in 0..width {
                let idx_flat = y.saturating_mul(width).saturating_add(x);
                let before_px = before
                    .and_then(|image| image.pixels.get(idx_flat))
                    .copied()
                    .unwrap_or(egui::Color32::TRANSPARENT);
                let Some(after_px) = after.pixels.get(idx_flat).copied() else {
                    continue;
                };
                if before_px != after_px {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                    changed = true;
                }
            }
        }
        if !changed {
            return None;
        }
        let rect_w = max_x.saturating_sub(min_x).saturating_add(1);
        let rect_h = max_y.saturating_sub(min_y).saturating_add(1);
        let mut rgba_deltas = Vec::with_capacity(rect_w.saturating_mul(rect_h));
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let idx_flat = y.saturating_mul(width).saturating_add(x);
                let before_px = before
                    .and_then(|image| image.pixels.get(idx_flat))
                    .copied()
                    .unwrap_or(egui::Color32::TRANSPARENT);
                let after_px = after
                    .pixels
                    .get(idx_flat)
                    .copied()
                    .unwrap_or(egui::Color32::TRANSPARENT);
                rgba_deltas.push(color_delta(before_px, after_px));
            }
        }
        Some(OverlayHistoryEntry {
            page_idx: idx,
            origin_px: [min_x, min_y],
            size_px: [rect_w, rect_h],
            rgba_deltas,
        })
    }

    fn build_region_history_entry(
        &self,
        idx: usize,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        chunk: &ColorImage,
    ) -> Option<OverlayHistoryEntry> {
        if width == 0 || height == 0 || chunk.size[0] == 0 || chunk.size[1] == 0 {
            return None;
        }
        let before = self.overlays.get(idx).and_then(|item| item.as_ref());
        let mut min_x = width;
        let mut min_y = height;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut changed = false;
        for local_y in 0..height {
            let src_y = (local_y * chunk.size[1] / height).min(chunk.size[1] - 1);
            for local_x in 0..width {
                let src_x = (local_x * chunk.size[0] / width).min(chunk.size[0] - 1);
                let before_px = color_at_or_transparent(before, x + local_x, y + local_y);
                let after_px = color_at_chunk(chunk, src_x, src_y);
                if before_px != after_px {
                    min_x = min_x.min(local_x);
                    min_y = min_y.min(local_y);
                    max_x = max_x.max(local_x);
                    max_y = max_y.max(local_y);
                    changed = true;
                }
            }
        }
        if !changed {
            return None;
        }
        let rect_w = max_x.saturating_sub(min_x).saturating_add(1);
        let rect_h = max_y.saturating_sub(min_y).saturating_add(1);
        let mut rgba_deltas = Vec::with_capacity(rect_w.saturating_mul(rect_h));
        for local_y in min_y..=max_y {
            let src_y = (local_y * chunk.size[1] / height).min(chunk.size[1] - 1);
            for local_x in min_x..=max_x {
                let src_x = (local_x * chunk.size[0] / width).min(chunk.size[0] - 1);
                let before_px = color_at_or_transparent(before, x + local_x, y + local_y);
                let after_px = color_at_chunk(chunk, src_x, src_y);
                rgba_deltas.push(color_delta(before_px, after_px));
            }
        }
        Some(OverlayHistoryEntry {
            page_idx: idx,
            origin_px: [x + min_x, y + min_y],
            size_px: [rect_w, rect_h],
            rgba_deltas,
        })
    }

    fn push_undo_history(&mut self, entry: OverlayHistoryEntry) {
        self.undo_history.push(entry);
        if self.undo_history.len() > OVERLAY_HISTORY_LIMIT {
            let overflow = self.undo_history.len() - OVERLAY_HISTORY_LIMIT;
            self.undo_history.drain(0..overflow);
        }
        self.redo_history.clear();
    }

    fn apply_history_entry(
        &mut self,
        entry: &OverlayHistoryEntry,
        direction: HistoryDirection,
    ) -> bool {
        let Some(size) = self.sizes.get(entry.page_idx).copied() else {
            return false;
        };
        if size[0] == 0 || size[1] == 0 {
            return false;
        }
        self.ensure_overlay_storage(entry.page_idx, size);
        let Some(overlay) = self
            .overlays
            .get_mut(entry.page_idx)
            .and_then(|item| item.as_mut())
        else {
            return false;
        };
        let Some(cache) = self
            .overlay_rgba_cache
            .get_mut(entry.page_idx)
            .and_then(|item| item.as_mut())
        else {
            return false;
        };
        let rect_w = entry.size_px[0];
        let rect_h = entry.size_px[1];
        if rect_w == 0 || rect_h == 0 {
            return false;
        }
        let expected_len = rect_w.saturating_mul(rect_h);
        if entry.rgba_deltas.len() != expected_len {
            return false;
        }
        let overlay_w = overlay.size[0];
        let overlay_h = overlay.size[1];
        let raw = Arc::make_mut(cache).as_mut();
        for local_y in 0..rect_h {
            for local_x in 0..rect_w {
                let dst_x = entry.origin_px[0] + local_x;
                let dst_y = entry.origin_px[1] + local_y;
                if dst_x >= overlay_w || dst_y >= overlay_h {
                    return false;
                }
                let delta_idx = local_y.saturating_mul(rect_w).saturating_add(local_x);
                let Some(delta) = entry.rgba_deltas.get(delta_idx).copied() else {
                    return false;
                };
                let pixel_idx = dst_y.saturating_mul(overlay_w).saturating_add(dst_x);
                let Some(pixel) = overlay.pixels.get_mut(pixel_idx) else {
                    return false;
                };
                let raw_idx = pixel_idx.saturating_mul(4);
                if raw_idx.saturating_add(3) >= raw.len() {
                    return false;
                }
                let next = apply_color_delta(*pixel, delta, direction);
                *pixel = next;
                write_color32_as_straight_rgba(raw, raw_idx, next);
            }
        }
        self.mark_dirty(entry.page_idx);
        true
    }
}

#[derive(Clone, Copy)]
enum HistoryDirection {
    Undo,
    Redo,
}

fn numeric_first_key(path: &Path) -> (u8, String, u8, String) {
    let base = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ext_weight = match ext.as_str() {
        "png" => 0,
        "jpg" | "jpeg" => 1,
        _ => 2,
    };
    if stem.chars().all(|c| c.is_ascii_digit()) && !stem.is_empty() {
        let num = stem.parse::<u64>().unwrap_or(0);
        return (
            0,
            format!("{num:020}"),
            ext_weight,
            base.to_ascii_lowercase(),
        );
    }
    (
        1,
        stem.to_ascii_lowercase(),
        ext_weight,
        base.to_ascii_lowercase(),
    )
}

fn transparent_overlay(w: usize, h: usize) -> ColorImage {
    ColorImage::filled([w, h], egui::Color32::TRANSPARENT)
}

pub fn save_overlay_snapshots_to(
    dir: &Path,
    snapshots: &[(usize, String, Arc<RgbaImage>)],
) -> anyhow::Result<()> {
    if snapshots.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    for (_, stem, image) in snapshots {
        let dst = dir.join(format!("{stem}.png"));
        image.save(&dst)?;
    }
    Ok(())
}

fn color_image_to_rgba(image: &ColorImage) -> image::RgbaImage {
    let mut raw = Vec::with_capacity(image.pixels.len() * 4);
    for px in &image.pixels {
        raw.extend_from_slice(&px.to_srgba_unmultiplied());
    }
    image::RgbaImage::from_raw(image.size[0] as u32, image.size[1] as u32, raw)
        .unwrap_or_else(|| image::RgbaImage::new(image.size[0] as u32, image.size[1] as u32))
}

fn rgba_image_bytes(image: &RgbaImage) -> u64 {
    u64::from(image.width())
        .saturating_mul(u64::from(image.height()))
        .saturating_mul(4)
}

fn rgba_is_fully_transparent(image: &RgbaImage) -> bool {
    image.as_raw().chunks_exact(4).all(|pixel| pixel[3] == 0)
}

fn write_color32_as_straight_rgba(raw: &mut [u8], raw_idx: usize, color: egui::Color32) {
    if raw_idx.saturating_add(3) >= raw.len() {
        return;
    }
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    raw[raw_idx] = r;
    raw[raw_idx + 1] = g;
    raw[raw_idx + 2] = b;
    raw[raw_idx + 3] = a;
}

fn color_at_or_transparent(image: Option<&ColorImage>, x: usize, y: usize) -> egui::Color32 {
    let Some(image) = image else {
        return egui::Color32::TRANSPARENT;
    };
    if x >= image.size[0] || y >= image.size[1] {
        return egui::Color32::TRANSPARENT;
    }
    let idx = y.saturating_mul(image.size[0]).saturating_add(x);
    image
        .pixels
        .get(idx)
        .copied()
        .unwrap_or(egui::Color32::TRANSPARENT)
}

fn color_at_chunk(image: &ColorImage, x: usize, y: usize) -> egui::Color32 {
    let idx = y.saturating_mul(image.size[0]).saturating_add(x);
    image
        .pixels
        .get(idx)
        .copied()
        .unwrap_or(egui::Color32::TRANSPARENT)
}

fn color_delta(before: egui::Color32, after: egui::Color32) -> [i16; 4] {
    [
        i16::from(after.r()) - i16::from(before.r()),
        i16::from(after.g()) - i16::from(before.g()),
        i16::from(after.b()) - i16::from(before.b()),
        i16::from(after.a()) - i16::from(before.a()),
    ]
}

fn apply_color_delta(
    color: egui::Color32,
    delta: [i16; 4],
    direction: HistoryDirection,
) -> egui::Color32 {
    let sign = match direction {
        HistoryDirection::Undo => -1,
        HistoryDirection::Redo => 1,
    };
    egui::Color32::from_rgba_premultiplied(
        apply_channel_delta(color.r(), delta[0], sign),
        apply_channel_delta(color.g(), delta[1], sign),
        apply_channel_delta(color.b(), delta[2], sign),
        apply_channel_delta(color.a(), delta[3], sign),
    )
}

fn apply_channel_delta(channel: u8, delta: i16, sign: i16) -> u8 {
    let value = i16::from(channel) + delta.saturating_mul(sign);
    let clamped = value.clamp(0, i16::from(u8::MAX));
    u8::try_from(clamped).unwrap_or(u8::MAX)
}

fn blit_scaled_chunk_color_image(
    dst: &mut ColorImage,
    target_x: usize,
    target_y: usize,
    target_w: usize,
    target_h: usize,
    chunk: &ColorImage,
) {
    if target_w == 0 || target_h == 0 || chunk.size[0] == 0 || chunk.size[1] == 0 {
        return;
    }
    let dst_w = dst.size[0];
    let dst_h = dst.size[1];
    for y in 0..target_h {
        let src_y = (y * chunk.size[1] / target_h).min(chunk.size[1] - 1);
        let dst_y = target_y + y;
        if dst_y >= dst_h {
            break;
        }
        for x in 0..target_w {
            let src_x = (x * chunk.size[0] / target_w).min(chunk.size[0] - 1);
            let dst_x = target_x + x;
            if dst_x >= dst_w {
                break;
            }
            let src_idx = src_y.saturating_mul(chunk.size[0]).saturating_add(src_x);
            let dst_idx = dst_y.saturating_mul(dst_w).saturating_add(dst_x);
            if let (Some(src_px), Some(dst_px)) =
                (chunk.pixels.get(src_idx), dst.pixels.get_mut(dst_idx))
            {
                *dst_px = *src_px;
            }
        }
    }
}

fn blit_scaled_chunk_rgba(
    dst: &mut RgbaImage,
    target_x: usize,
    target_y: usize,
    target_w: usize,
    target_h: usize,
    chunk: &ColorImage,
) {
    if target_w == 0 || target_h == 0 || chunk.size[0] == 0 || chunk.size[1] == 0 {
        return;
    }
    let dst_w = dst.width() as usize;
    let dst_h = dst.height() as usize;
    let raw = dst.as_mut();
    for y in 0..target_h {
        let src_y = (y * chunk.size[1] / target_h).min(chunk.size[1] - 1);
        let dst_y = target_y + y;
        if dst_y >= dst_h {
            break;
        }
        for x in 0..target_w {
            let src_x = (x * chunk.size[0] / target_w).min(chunk.size[0] - 1);
            let dst_x = target_x + x;
            if dst_x >= dst_w {
                break;
            }
            let src_idx = src_y.saturating_mul(chunk.size[0]).saturating_add(src_x);
            let Some(src_px) = chunk.pixels.get(src_idx) else {
                continue;
            };
            let dst_idx = dst_y
                .saturating_mul(dst_w)
                .saturating_add(dst_x)
                .saturating_mul(4);
            if dst_idx.saturating_add(3) >= raw.len() {
                continue;
            }
            write_color32_as_straight_rgba(raw, dst_idx, *src_px);
        }
    }
}

fn resize_nearest(src: &ColorImage, dst_w: usize, dst_h: usize) -> ColorImage {
    if src.size[0] == 0 || src.size[1] == 0 || dst_w == 0 || dst_h == 0 {
        return ColorImage::filled([dst_w.max(1), dst_h.max(1)], egui::Color32::TRANSPARENT);
    }
    let src_w = src.size[0];
    let src_h = src.size[1];
    let mut out = ColorImage::filled([dst_w, dst_h], egui::Color32::TRANSPARENT);
    for y in 0..dst_h {
        let sy = y.saturating_mul(src_h) / dst_h;
        for x in 0..dst_w {
            let sx = x.saturating_mul(src_w) / dst_w;
            let sidx = sy.saturating_mul(src_w).saturating_add(sx);
            let didx = y.saturating_mul(dst_w).saturating_add(x);
            if let (Some(src_px), Some(dst_px)) = (src.pixels.get(sidx), out.pixels.get_mut(didx)) {
                *dst_px = *src_px;
            }
        }
    }
    out
}

/// Returns the amount of free (available) RAM in bytes.
/// Returns `u64::MAX` on platforms or errors where the value cannot be determined.
fn free_memory_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        use std::io::{BufRead, BufReader};
        if let Ok(file) = std::fs::File::open("/proc/meminfo") {
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("MemAvailable:") {
                    if let Some(kb_str) = rest.split_whitespace().next()
                        && let Ok(kb) = kb_str.parse::<u64>()
                    {
                        return kb * 1024;
                    }
                    break;
                }
            }
        }
        u64::MAX
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut stat = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            dwMemoryLoad: 0,
            ullTotalPhys: 0,
            ullAvailPhys: 0,
            ullTotalPageFile: 0,
            ullAvailPageFile: 0,
            ullTotalVirtual: 0,
            ullAvailVirtual: 0,
            ullAvailExtendedVirtual: 0,
        };
        // SAFETY: stat is properly initialized with correct dwLength.
        if unsafe { GlobalMemoryStatusEx(&mut stat) } != 0 {
            stat.ullAvailPhys
        } else {
            u64::MAX
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        u64::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_page_model() -> CleanOverlaysModel {
        CleanOverlaysModel::new_from_pages(&[PathBuf::from("001.png")])
    }

    fn multi_page_model(count: usize) -> CleanOverlaysModel {
        let pages = (0..count)
            .map(|idx| PathBuf::from(format!("{idx:03}.png")))
            .collect::<Vec<_>>();
        CleanOverlaysModel::new_from_pages(&pages)
    }

    #[test]
    fn color_image_to_rgba_writes_straight_alpha_rgb() {
        let mut image = ColorImage::filled([1, 1], egui::Color32::TRANSPARENT);
        image.pixels[0] = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 128);

        let rgba = color_image_to_rgba(&image);

        assert_eq!(rgba.as_raw().as_slice(), &[255, 255, 255, 128]);
    }

    #[test]
    fn replace_region_updates_save_cache_with_straight_alpha_rgb() {
        let mut model = single_page_model();
        let chunk = ColorImage::filled(
            [1, 1],
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 128),
        );

        assert!(model.replace_region(0, [1, 1], 0, 0, 1, 1, &chunk));

        let Some(rgba) = model.overlay_rgba(0) else {
            panic!("overlay rgba cache was not populated");
        };
        assert_eq!(rgba.as_raw().as_slice(), &[255, 255, 255, 128]);
    }

    #[test]
    fn overlay_history_keeps_straight_alpha_save_cache_after_redo() {
        let mut model = single_page_model();
        let chunk = ColorImage::filled(
            [1, 1],
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 128),
        );

        assert!(model.replace_region(0, [1, 1], 0, 0, 1, 1, &chunk));
        assert!(model.undo_overlay_history());
        assert!(model.redo_overlay_history());

        let Some(color_image) = model.get(0) else {
            panic!("overlay color image was not populated");
        };
        assert_eq!(
            color_image.pixels[0].to_srgba_unmultiplied(),
            [255, 255, 255, 128]
        );
        let Some(rgba) = model.overlay_rgba(0) else {
            panic!("overlay rgba cache was not populated");
        };
        assert_eq!(rgba.as_raw().as_slice(), &[255, 255, 255, 128]);
    }

    #[test]
    fn loaded_transparent_overlay_stays_virtual_absent() {
        let mut model = single_page_model();
        let rgba = Arc::new(RgbaImage::new(2, 2));
        let color = ColorImage::filled([2, 2], egui::Color32::TRANSPARENT);

        model.load_prepared_overlay(0, rgba, color);

        assert!(model.is_overlay_virtual_absent(0));
        assert!(model.get(0).is_none());
        assert!(model.overlay_rgba(0).is_none());
        assert!(model.save_snapshots().is_empty());
    }

    #[test]
    fn edited_empty_overlay_materializes_and_is_dirty() {
        let mut model = single_page_model();

        assert!(model.ensure_overlay(0, [2, 2]));

        assert!(!model.is_overlay_virtual_absent(0));
        assert!(model.get(0).is_some());
        assert!(model.overlay_rgba(0).is_some());
        assert!(model.has_unsaved_overlay_changes());
    }

    #[test]
    fn page_cache_item_limit_evicts_lru_but_keeps_pinned_window() {
        let mut model = multi_page_model(4);
        model.set_page_cache_policy(PageCachePolicy {
            byte_limit: None,
            item_limit: Some(2),
            pinned_window: Some(PageCacheWindow {
                center_idx: 0,
                radius: 0,
            }),
        });

        for idx in 0..4 {
            assert!(model.store_cached_page_rgba(idx, RgbaImage::new(1, 1)));
        }

        assert!(model.has_cached_page_rgba(0));
        assert!(!model.has_cached_page_rgba(1));
        assert!(!model.has_cached_page_rgba(2));
        assert!(model.has_cached_page_rgba(3));
    }

    #[test]
    fn cache_eviction_request_reports_source_page_bytes() {
        let mut model = multi_page_model(3);
        model.set_page_cache_policy(PageCachePolicy {
            byte_limit: None,
            item_limit: None,
            pinned_window: None,
        });
        assert!(model.store_cached_page_rgba(0, RgbaImage::new(2, 2)));
        assert!(model.store_cached_page_rgba(1, RgbaImage::new(2, 2)));
        assert!(model.cached_page_rgba(0).is_some());

        let mut request = CacheEvictionRequest {
            profile: MemoryProfile::Medium,
            pressure: MemoryPressure::Soft,
            target_free_bytes: 16,
            pinned_pages: BTreeSet::new(),
        };
        request.pinned_pages.insert(1);
        let report = model.evict_cache(&request);

        assert_eq!(report.estimated_freed_bytes, 16);
        assert_eq!(report.resources.len(), 1);
        assert_eq!(report.resources[0].page_idx, Some(0));
        assert!(!model.has_cached_page_rgba(0));
        assert!(model.has_cached_page_rgba(1));
    }
}
