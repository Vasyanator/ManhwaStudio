/*
FILE OVERVIEW: src/models/clean_overlays_model.rs
Shared runtime model for clean overlays and action-side page cache.

Main items:
- `OverlayDelta`: incremental overlay/visibility changes for canvas subscribers.
- `CleanOverlayDiffOp`: one committed overlay edit as a reversible tiled+zstd `RasterDiff`
  (a `ms_actions::ReversibleAction` over `CleanOverlaysModel`).
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
- Keeps undo/redo history for cleaning commits via `ms_actions::ActionHistory<CleanOverlayDiffOp>`:
  each edit is stored as a tiled, zstd-compressed, reversible straight-RGBA delta (`RasterDiff`),
  bounded by a 128-step count cap AND a per-memory-profile COMPRESSED byte budget; the redo branch
  is discarded after a new head commit.
- Supports "cache pages immediately" mode via `cache_pages_enabled`; when disabled, page cache can
  still be populated lazily for specific pages when needed by tools.

Threading note:
- Undo apply/record works on the straight-RGBA cache and mirrors changed pixels back into the
  `ColorImage`. Region/brush construction (`replace_region`) is bounded (per-region capture +
  compress) and cheap enough for the interactive path. The FULL-PAGE construction path
  (`apply_overlay_snapshot`: clear / quick-clean / large region-editor apply) still scans and
  zstd-compresses the whole page synchronously on the caller's thread; for those discrete one-shot
  actions this matches the pre-existing full-page behavior. Moving full-page construction to a
  worker is a planned follow-up (Phase 2c).
*/

use egui::ColorImage;
use image::RgbaImage;
use ms_actions::{
    ActionHistory, ApplyDirection, DirtyRect, RasterDiff, RasterDiffError, ReversibleAction,
};
use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, MemoryBudget, MemoryPressure, MemoryProfile,
};

/// Threshold below which LRU eviction of page cache entries is triggered (2 GiB).
const PAGE_CACHE_EVICT_FREE_RAM_THRESHOLD: u64 = 2 * 1024 * 1024 * 1024;
const OVERLAY_HISTORY_LIMIT: usize = 128;
/// Tile edge (pixels) used to partition overlay `RasterDiff`s. Matches the 1024px
/// tiling already used elsewhere for dirty-tracking; a local const because the
/// existing constants are GPU-upload tile sizes not exported for reuse here.
const OVERLAY_HISTORY_TILE_SIDE: u32 = 1024;
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

/// Error raised while applying a `CleanOverlayDiffOp` to a `CleanOverlaysModel`.
///
/// Kept typed and panic-free: a missing/zero-sized target page and a raster
/// apply failure are the only failure modes, and both are surfaced to the caller
/// (which treats them as "nothing changed") rather than aborting.
#[derive(Debug)]
pub(crate) enum CleanOverlayDiffError {
    /// The target page index is out of range or has an unknown (zero) size, so the
    /// diff cannot be applied.
    PageUnavailable {
        /// The page index that could not be resolved.
        page_idx: usize,
    },
    /// The underlying `RasterDiff` operation failed (size mismatch, corrupt
    /// payload, ...).
    Raster(RasterDiffError),
}

impl fmt::Display for CleanOverlayDiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CleanOverlayDiffError::PageUnavailable { page_idx } => {
                write!(f, "clean overlay page {page_idx} is unavailable for undo/redo")
            }
            CleanOverlayDiffError::Raster(err) => write!(f, "raster diff failed: {err}"),
        }
    }
}

impl std::error::Error for CleanOverlayDiffError {}

impl From<RasterDiffError> for CleanOverlayDiffError {
    fn from(err: RasterDiffError) -> Self {
        CleanOverlayDiffError::Raster(err)
    }
}

/// One committed clean-overlay edit, stored as a reversible tiled+zstd delta.
///
/// The payload lives behind an `Arc` so `inverse()` (used on every undo) and the
/// redo path share it instead of deep-cloning the compressed tiles. `dir` selects
/// how [`RasterDiff::apply`] runs: `Forward` re-applies the edit (redo — adds the
/// delta), `Reverse` undoes it (subtracts the delta). A freshly RECORDED op always
/// has `dir == Forward`, matching the engine's semantics: `ActionHistory::undo`
/// runs `inverse()` (a `Reverse` op) to restore the pre-edit pixels, and `redo`
/// re-applies the original `Forward` op.
#[derive(Debug, Clone)]
pub(crate) struct CleanOverlayDiffOp {
    page_idx: usize,
    diff: Arc<RasterDiff>,
    dir: ApplyDirection,
    label: String,
}

impl ReversibleAction for CleanOverlayDiffOp {
    type Ctx = CleanOverlaysModel;
    type Err = CleanOverlayDiffError;

    fn apply(&mut self, ctx: &mut Self::Ctx) -> Result<(), Self::Err> {
        ctx.apply_raster_diff(self.page_idx, &self.diff, self.dir)
    }

    fn inverse(&self) -> Self {
        Self {
            page_idx: self.page_idx,
            diff: Arc::clone(&self.diff),
            dir: match self.dir {
                ApplyDirection::Forward => ApplyDirection::Reverse,
                ApplyDirection::Reverse => ApplyDirection::Forward,
            },
            label: self.label.clone(),
        }
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn weight(&self) -> usize {
        // Drives the history byte budget: the retained cost of this op is its
        // compressed tile payload (the `Arc`/String overhead is negligible).
        self.diff.compressed_len()
    }
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
    /// Unified undo/redo engine. Each entry is a reversible tiled+zstd overlay
    /// delta; bounded by `OVERLAY_HISTORY_LIMIT` steps and a per-profile
    /// compressed byte budget (see `set_memory_profile`).
    history: ActionHistory<CleanOverlayDiffOp>,
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
            // Start with the count cap and a default (Medium-profile) byte budget;
            // `set_memory_profile` re-tunes the budget once the profile is known.
            history: ActionHistory::with_weight_budget(
                OVERLAY_HISTORY_LIMIT,
                MemoryBudget::for_profile(MemoryProfile::default()).clean_overlay_undo_bytes_usize(),
            ),
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
        // Re-tune the undo-history byte budget; enforced immediately (may evict
        // oldest entries), but always keeps at least one undoable step.
        let budget = MemoryBudget::for_profile(profile).clean_overlay_undo_bytes_usize();
        self.history.set_weight_budget(Some(budget));
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
        // Capture the region's straight-RGBA pixels BEFORE mutating, so the
        // recorded diff can be reversed from live pixels (no second snapshot).
        let before_region = self.copy_region_rgba(idx, x0, y0, target_w, target_h);
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
        // Capture "after" from the same region and record the reversible delta.
        let after_region = self.copy_region_rgba(idx, x0, y0, target_w, target_h);
        self.record_region_diff(
            idx,
            [x0, y0],
            [target_w, target_h],
            target_size,
            &before_region,
            &after_region,
        );
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
        self.history.can_undo()
    }

    pub fn can_redo_overlay_history(&self) -> bool {
        self.history.can_redo()
    }

    pub fn undo_overlay_history(&mut self) -> bool {
        // Take-and-restore idiom: the ops' `Ctx` is `Self`, but `history` is a
        // field of `Self`, so `self.history.undo(self)` would double-borrow.
        // `mem::replace` moves the history out (cheap: it moves a VecDeque of
        // `Arc<RasterDiff>`, cloning no payloads) against a same-config empty
        // history, applies against `self`, then puts it back.
        let mut history = self.take_history();
        let result = history.undo(self);
        self.history = history;
        match result {
            Ok(changed) => changed,
            Err(err) => {
                eprintln!("Clean overlay undo failed: {err}");
                false
            }
        }
    }

    pub fn redo_overlay_history(&mut self) -> bool {
        // See `undo_overlay_history` for the take-and-restore rationale.
        let mut history = self.take_history();
        let result = history.redo(self);
        self.history = history;
        match result {
            Ok(changed) => changed,
            Err(err) => {
                eprintln!("Clean overlay redo failed: {err}");
                false
            }
        }
    }

    /// Move the undo history out of `self`, leaving an empty history that
    /// preserves the count limit and byte budget. Used only by the
    /// take-and-restore undo/redo idiom above; the caller MUST put a history
    /// back (normally the moved-out one) before returning.
    fn take_history(&mut self) -> ActionHistory<CleanOverlayDiffOp> {
        let limit = self.history.limit();
        let replacement = match self.history.weight_budget() {
            Some(budget) => ActionHistory::with_weight_budget(limit, budget),
            None => ActionHistory::new(limit),
        };
        mem::replace(&mut self.history, replacement)
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
        // Routed through the storage seam so the web build persists overlays into
        // its in-memory/IndexedDB store instead of the desktop filesystem.
        let store = crate::storage::storage();
        let dir_str = clean_layers_dir.to_string_lossy();
        store.create_dir_all(dir_str.as_ref())?;
        for (stem, image) in self.save_snapshots() {
            let dst = clean_layers_dir.join(format!("{stem}.png"));
            let dst_str = dst.to_string_lossy();
            // Encode straight-alpha RGBA to PNG in memory (default encoder params,
            // identical to `RgbaImage::save`) before handing bytes to storage.
            let mut buf = Vec::new();
            image.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)?;
            store.write(dst_str.as_ref(), &buf)?;
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
        // Record the reversible full-page delta BEFORE overwriting the cache, so
        // "before" is the current page and "after" is the incoming image.
        if record_history {
            self.record_full_image_diff(idx, rgba_image.as_ref());
        }
        self.overlay_rgba_cache[idx] = Some(rgba_image);
        self.overlays[idx] = Some(color_image);
        if record_history {
            self.mark_dirty(idx);
        } else {
            self.mark_runtime_changed(idx);
        }
    }

    /// Apply a reversible overlay delta to page `page_idx` in `dir`, updating BOTH
    /// synchronized representations in lockstep. This REPLACES the old
    /// `apply_history_entry` and is the only mutation path used by undo/redo.
    ///
    /// The straight-RGBA cache is the primary buffer the `RasterDiff` mutates; the
    /// `ColorImage` is then re-derived over the changed rects via
    /// `Color32::from_rgba_unmultiplied`, keeping the pair byte-consistent under
    /// the module's straight-vs-premultiplied convention
    /// (`to_srgba_unmultiplied(ColorImage) == rgba`). Marks the page dirty and
    /// bumps the revision, exactly as a forward edit does.
    ///
    /// # Errors
    /// - [`CleanOverlayDiffError::PageUnavailable`] if the page is out of range,
    ///   has zero size, or has no materialized cache.
    /// - [`CleanOverlayDiffError::Raster`] if the delta cannot be applied (image
    ///   size mismatch / corrupt payload).
    fn apply_raster_diff(
        &mut self,
        page_idx: usize,
        diff: &RasterDiff,
        dir: ApplyDirection,
    ) -> Result<(), CleanOverlayDiffError> {
        let Some(size) = self.sizes.get(page_idx).copied() else {
            return Err(CleanOverlayDiffError::PageUnavailable { page_idx });
        };
        if size[0] == 0 || size[1] == 0 {
            return Err(CleanOverlayDiffError::PageUnavailable { page_idx });
        }
        // Guarantee both representations exist at the page size before applying.
        self.ensure_overlay_storage(page_idx, size);
        let Some(image_size) = usize_pair_to_u32(size) else {
            return Err(CleanOverlayDiffError::PageUnavailable { page_idx });
        };

        // 1) Apply the delta to the straight-RGBA cache (primary buffer). A page
        //    resized since the edit surfaces as a `RasterDiff` size error here.
        let dirty = {
            let Some(cache) = self
                .overlay_rgba_cache
                .get_mut(page_idx)
                .and_then(|item| item.as_mut())
            else {
                return Err(CleanOverlayDiffError::PageUnavailable { page_idx });
            };
            let rgba = Arc::make_mut(cache);
            diff.apply(rgba.as_mut(), image_size, dir)?
        };

        // 2) Mirror the changed pixels into the ColorImage. `overlay_rgba_cache`
        //    (read) and `overlays` (write) are DIFFERENT fields, so these disjoint
        //    borrows coexist.
        let Some(rgba) = self
            .overlay_rgba_cache
            .get(page_idx)
            .and_then(|item| item.as_ref())
        else {
            return Err(CleanOverlayDiffError::PageUnavailable { page_idx });
        };
        let Some(overlay) = self
            .overlays
            .get_mut(page_idx)
            .and_then(|item| item.as_mut())
        else {
            return Err(CleanOverlayDiffError::PageUnavailable { page_idx });
        };
        sync_color_image_from_rgba(overlay, rgba.as_ref(), &dirty);

        self.mark_dirty(page_idx);
        Ok(())
    }

    /// Copy a region's straight-RGBA pixels out of the page cache, row-major over
    /// the `w`x`h` rect at `(x, y)`. Out-of-bounds or missing pixels read as fully
    /// transparent (zeros), matching the old delta's TRANSPARENT default.
    fn copy_region_rgba(&self, idx: usize, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
        let Some(image) = self.overlay_rgba_cache.get(idx).and_then(|item| item.as_ref()) else {
            return out;
        };
        let img_w = image.width() as usize;
        let img_h = image.height() as usize;
        let raw = image.as_raw();
        for row in 0..h {
            let src_y = y + row;
            if src_y >= img_h {
                break;
            }
            for col in 0..w {
                let src_x = x + col;
                if src_x >= img_w {
                    break;
                }
                let src_idx = (src_y * img_w + src_x) * 4;
                let dst_idx = (row * w + col) * 4;
                if let (Some(src), Some(dst)) = (
                    raw.get(src_idx..src_idx + 4),
                    out.get_mut(dst_idx..dst_idx + 4),
                ) {
                    dst.copy_from_slice(src);
                }
            }
        }
        out
    }

    /// Build and record a region overlay delta from region-local `before`/`after`
    /// straight-RGBA buffers. Skips recording when nothing changed (no-op edit) so
    /// `can_undo` stays false, matching the previous behavior.
    fn record_region_diff(
        &mut self,
        idx: usize,
        region_origin: [usize; 2],
        region_size: [usize; 2],
        image_size: [usize; 2],
        before: &[u8],
        after: &[u8],
    ) {
        let (Some(origin), Some(region), Some(dims)) = (
            usize_pair_to_u32(region_origin),
            usize_pair_to_u32(region_size),
            usize_pair_to_u32(image_size),
        ) else {
            return;
        };
        match RasterDiff::from_region_pixels(
            before,
            after,
            origin,
            region,
            dims,
            OVERLAY_HISTORY_TILE_SIDE,
        ) {
            Ok(diff) if diff.is_empty() => {}
            Ok(diff) => self.history.record(CleanOverlayDiffOp {
                page_idx: idx,
                diff: Arc::new(diff),
                dir: ApplyDirection::Forward,
                label: overlay_edit_label(),
            }),
            Err(err) => {
                eprintln!("Failed to build region clean overlay undo diff (page {idx}): {err}");
            }
        }
    }

    /// Build and record a full-page overlay delta between the current page cache
    /// and the incoming `after` image. Skips recording when nothing changed.
    fn record_full_image_diff(&mut self, idx: usize, after: &RgbaImage) {
        let w = after.width();
        let h = after.height();
        if w == 0 || h == 0 {
            return;
        }
        let after_bytes = after.as_raw();
        // "Before" is the current straight-RGBA page; a missing or mismatched page
        // is treated as fully transparent (parity with the old delta which
        // defaulted absent pixels to TRANSPARENT).
        let fallback: Vec<u8>;
        let before_bytes: &[u8] =
            match self.overlay_rgba_cache.get(idx).and_then(|item| item.as_ref()) {
                Some(image) if image.width() == w && image.height() == h => image.as_raw(),
                _ => {
                    fallback = vec![0u8; after_bytes.len()];
                    &fallback
                }
            };
        match RasterDiff::from_rgba(before_bytes, after_bytes, [w, h], OVERLAY_HISTORY_TILE_SIDE) {
            Ok(diff) if diff.is_empty() => {}
            Ok(diff) => self.history.record(CleanOverlayDiffOp {
                page_idx: idx,
                diff: Arc::new(diff),
                dir: ApplyDirection::Forward,
                label: overlay_edit_label(),
            }),
            Err(err) => {
                eprintln!("Failed to build full-page clean overlay undo diff (page {idx}): {err}");
            }
        }
    }
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
    // Routed through the storage seam (web build writes to its virtual store).
    let store = crate::storage::storage();
    let dir_str = dir.to_string_lossy();
    store.create_dir_all(dir_str.as_ref())?;
    for (_, stem, image) in snapshots {
        let dst = dir.join(format!("{stem}.png"));
        let dst_str = dst.to_string_lossy();
        // Encode straight-alpha RGBA to PNG in memory (default encoder params,
        // identical to `RgbaImage::save`) before handing bytes to storage.
        let mut buf = Vec::new();
        image.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)?;
        store.write(dst_str.as_ref(), &buf)?;
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

/// Convert a `[usize; 2]` pixel pair into a `[u32; 2]`, or `None` if either
/// component exceeds `u32` (unreachable for realistic image dimensions). Keeps the
/// raster-diff conversions free of lossy `as` casts.
fn usize_pair_to_u32(pair: [usize; 2]) -> Option<[u32; 2]> {
    Some([
        u32::try_from(pair[0]).ok()?,
        u32::try_from(pair[1]).ok()?,
    ])
}

/// Fixed label for clean-overlay undo entries (not currently surfaced in the UI;
/// present for history/logging parity with the unified action system).
fn overlay_edit_label() -> String {
    "clean overlay edit".to_string()
}

/// Re-derive the `ColorImage` pixels over the changed `dirty` rects from the
/// straight-RGBA cache, keeping the two representations byte-consistent.
///
/// Uses `Color32::from_rgba_unmultiplied`, the inverse of the
/// `to_srgba_unmultiplied` used to build the RGBA cache, so
/// `to_srgba_unmultiplied(result) == rgba` holds for every synced pixel (the
/// module's dual-representation invariant). Out-of-bounds rects are clamped, never
/// panicking.
fn sync_color_image_from_rgba(overlay: &mut ColorImage, rgba: &RgbaImage, dirty: &[DirtyRect]) {
    let ow = overlay.size[0];
    let oh = overlay.size[1];
    let iw = rgba.width() as usize;
    let ih = rgba.height() as usize;
    let raw = rgba.as_raw();
    for rect in dirty {
        let ox = rect.origin_px[0] as usize;
        let oy = rect.origin_px[1] as usize;
        let rw = rect.size_px[0] as usize;
        let rh = rect.size_px[1] as usize;
        for row in 0..rh {
            let y = oy + row;
            if y >= oh || y >= ih {
                break;
            }
            for col in 0..rw {
                let x = ox + col;
                if x >= ow || x >= iw {
                    break;
                }
                let src_idx = (y * iw + x) * 4;
                let Some(px) = raw.get(src_idx..src_idx + 4) else {
                    continue;
                };
                let color = egui::Color32::from_rgba_unmultiplied(px[0], px[1], px[2], px[3]);
                if let Some(dst) = overlay.pixels.get_mut(y * ow + x) {
                    *dst = color;
                }
            }
        }
    }
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

    fn solid_chunk(r: u8, g: u8, b: u8, a: u8) -> ColorImage {
        ColorImage::filled([1, 1], egui::Color32::from_rgba_unmultiplied(r, g, b, a))
    }

    #[test]
    fn region_edit_syncs_both_reps_and_round_trips() {
        let mut model = single_page_model();
        let chunk = solid_chunk(255, 255, 255, 255);
        assert!(model.get(0).is_none());

        // Forward edit mutates BOTH representations.
        assert!(model.replace_region(0, [2, 2], 0, 0, 1, 1, &chunk));
        let Some(color) = model.get(0) else {
            panic!("overlay color image was not populated");
        };
        assert_eq!(color.pixels[0].to_srgba_unmultiplied(), [255, 255, 255, 255]);
        let Some(rgba) = model.overlay_rgba(0) else {
            panic!("overlay rgba cache was not populated");
        };
        assert_eq!(&rgba.as_raw()[0..4], &[255, 255, 255, 255]);
        assert!(model.can_undo_overlay_history());

        // Undo restores BOTH to the pre-edit transparent pixels.
        assert!(model.undo_overlay_history());
        let Some(color) = model.get(0) else {
            panic!("overlay color image missing after undo");
        };
        assert_eq!(color.pixels[0], egui::Color32::TRANSPARENT);
        let Some(rgba) = model.overlay_rgba(0) else {
            panic!("overlay rgba cache missing after undo");
        };
        assert_eq!(&rgba.as_raw()[0..4], &[0, 0, 0, 0]);

        // Redo restores BOTH to the post-edit pixels.
        assert!(model.redo_overlay_history());
        let Some(color) = model.get(0) else {
            panic!("overlay color image missing after redo");
        };
        assert_eq!(color.pixels[0].to_srgba_unmultiplied(), [255, 255, 255, 255]);
        let Some(rgba) = model.overlay_rgba(0) else {
            panic!("overlay rgba cache missing after redo");
        };
        assert_eq!(&rgba.as_raw()[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn full_replace_snapshot_round_trips_both_reps() {
        let mut model = single_page_model();
        let mut white = RgbaImage::new(2, 2);
        for px in white.pixels_mut() {
            *px = image::Rgba([255, 255, 255, 255]);
        }
        model.replace_from_rgba(0, white);
        assert!(model.can_undo_overlay_history());

        assert!(model.undo_overlay_history());
        let Some(rgba) = model.overlay_rgba(0) else {
            panic!("overlay rgba cache missing after undo");
        };
        assert!(rgba.as_raw().iter().all(|&b| b == 0));
        let Some(color) = model.get(0) else {
            panic!("overlay color image missing after undo");
        };
        assert!(color.pixels.iter().all(|px| *px == egui::Color32::TRANSPARENT));

        assert!(model.redo_overlay_history());
        let Some(rgba) = model.overlay_rgba(0) else {
            panic!("overlay rgba cache missing after redo");
        };
        assert!(rgba.as_raw().iter().all(|&b| b == 255));
    }

    #[test]
    fn identical_region_edit_is_not_recorded() {
        let mut model = single_page_model();
        assert!(model.ensure_overlay(0, [2, 2]));
        let transparent = ColorImage::filled([2, 2], egui::Color32::TRANSPARENT);
        // Overwriting transparent with transparent changes nothing.
        assert!(model.replace_region(0, [2, 2], 0, 0, 2, 2, &transparent));
        assert!(!model.can_undo_overlay_history());
    }

    #[test]
    fn new_edit_truncates_redo_branch() {
        let mut model = single_page_model();
        let a = solid_chunk(255, 0, 0, 255);
        let b = solid_chunk(0, 255, 0, 255);
        let c = solid_chunk(0, 0, 255, 255);
        assert!(model.replace_region(0, [4, 4], 0, 0, 1, 1, &a));
        assert!(model.replace_region(0, [4, 4], 0, 0, 1, 1, &b));
        assert!(model.undo_overlay_history());
        assert!(model.can_redo_overlay_history());
        // A fresh commit abandons the redoable future.
        assert!(model.replace_region(0, [4, 4], 1, 1, 1, 1, &c));
        assert!(!model.can_redo_overlay_history());
    }

    #[test]
    fn tiny_weight_budget_evicts_oldest_but_keeps_one() {
        let mut model = single_page_model();
        // A budget far below any single compressed diff exercises eviction while
        // the `len > 1` guard keeps the newest step undoable.
        model.history.set_weight_budget(Some(4));
        for color in [
            solid_chunk(255, 0, 0, 255),
            solid_chunk(0, 255, 0, 255),
            solid_chunk(0, 0, 255, 255),
        ] {
            assert!(model.replace_region(0, [4, 4], 0, 0, 1, 1, &color));
        }
        assert!(model.can_undo_overlay_history());
        assert_eq!(model.history.undo_len(), 1);
        assert!(model.history.undo_weight() > 4);
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
