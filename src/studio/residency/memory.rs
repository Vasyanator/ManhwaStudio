/*
File: src/studio/residency/memory.rs

Purpose:
Memory side of `PageResidency`: the per-frame source-texture trim, the source-page GPU eviction
driven by `ms-memory` policy, and the throttled memory-pressure poll. The window of pages to keep
and the tab-owned GPU caches are NOT known here: the caller (`StudioApp`) passes the window in and
evicts its tabs' caches itself.

Key structures:
- `PressureTick`: a due pressure poll that found non-Normal pressure.

Key functions:
- `PageResidency::trim_gpu`, `PageResidency::poll_pressure`, `PageResidency::evict_for_pressure`
  (per-owner reports merged with `CacheEvictionReport::append`, their one owner).

Notes:
Eviction drops only GPU handles; `PageImageInfo` (page geometry) is never evicted, so canvas
layout stays stable, and every evicted texture is reconstructable from the kept tile bytes.
*/

use super::PageResidency;
use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, MemoryBudget, MemoryPressure, MemoryProfile, classify_memory_pressure,
    current_memory_availability, select_eviction_candidates,
};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::runtime_log;
use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use web_time::{Duration, Instant};

const MEMORY_PRESSURE_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const SOFT_PRESSURE_TARGET_FREE_BYTES: u64 = 256 * 1024 * 1024;
const HARD_PRESSURE_TARGET_FREE_BYTES: u64 = 768 * 1024 * 1024;

/// A due memory-pressure poll that classified the system as under pressure (never `Normal`).
/// `profile` is the memory profile the classification used, so the eviction request built from
/// it matches the decision exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::studio) struct PressureTick {
    profile: MemoryProfile,
    pressure: MemoryPressure,
}

impl PressureTick {
    /// The eviction request for this tick: free `pressure_target_free_bytes(pressure)` while
    /// keeping `pinned_pages` (the active tab's page window) resident.
    pub(in crate::studio) fn eviction_request(self, pinned_pages: BTreeSet<usize>) -> CacheEvictionRequest {
        CacheEvictionRequest {
            profile: self.profile,
            pressure: self.pressure,
            target_free_bytes: pressure_target_free_bytes(self.pressure),
            pinned_pages,
        }
    }
}

impl PageResidency {
    /// The memory budget of the current profile (the profile can change at runtime from the
    /// settings tab, so callers read it per frame).
    pub(in crate::studio) fn memory_budget(&self) -> MemoryBudget {
        self.memory_manager.budget()
    }

    /// Per-frame source-texture trim after the active tab drew. `active_pages` is the active tab's
    /// page window for `budget.visible_neighbor_pages`; `pixel_inspection` says whether that tab
    /// currently draws NEAREST tiles. Drops nearest textures outside the inspected window, linear
    /// textures outside the window unless the budget keeps them, and while inspecting also runs a
    /// soft source-page eviction pinned to the window.
    pub(in crate::studio) fn trim_gpu(&mut self, budget: MemoryBudget, active_pages: HashSet<usize>, pixel_inspection: bool) {
        for (page_idx, page_texture) in &mut self.textures {
            if !pixel_inspection || !active_pages.contains(page_idx) {
                page_texture.drop_nearest_textures();
            }
            if !budget.keep_linear_gpu_outside_window && !active_pages.contains(page_idx) {
                page_texture.drop_linear_textures();
            }
        }
        if !pixel_inspection {
            return;
        }
        let request = CacheEvictionRequest {
            profile: budget.profile,
            pressure: MemoryPressure::Soft,
            target_free_bytes: u64::MAX,
            pinned_pages: active_pages.into_iter().collect::<BTreeSet<_>>(),
        };
        // The report only describes what was dropped; this trim runs every frame while pixel
        // inspection is active, so it is deliberately not logged (the pressure path logs its own).
        let _ = self.evict_source_page_gpu_cache(&request);
    }

    /// Throttled memory-pressure poll (at most once per `MEMORY_PRESSURE_POLL_INTERVAL`).
    /// Records the classified pressure, logs a change, and returns a [`PressureTick`] when the
    /// poll was due and the pressure is not `Normal`; `None` otherwise.
    pub(in crate::studio) fn poll_pressure(&mut self, now: Instant) -> Option<PressureTick> {
        if self
            .last_memory_pressure_poll
            .is_some_and(|last| now.duration_since(last) < MEMORY_PRESSURE_POLL_INTERVAL)
        {
            return None;
        }
        self.last_memory_pressure_poll = Some(now);

        let profile = self.memory_manager.profile();
        let pressure = current_memory_availability()
            .map(|availability| classify_memory_pressure(profile, availability))
            .unwrap_or(MemoryPressure::Normal);
        let previous_pressure = self.memory_pressure;
        self.memory_pressure = pressure;
        if pressure != previous_pressure {
            runtime_log::log_info(format!(
                "[memory] pressure changed from {previous_pressure:?} to {pressure:?}"
            ));
        }
        if pressure == MemoryPressure::Normal {
            return None;
        }
        Some(PressureTick { profile, pressure })
    }

    /// Pressure eviction of what the residency owns, in this order: source-page GPU textures, then
    /// the shared page / overlay caches of `overlays` (a poisoned lock is logged and skipped).
    /// Returns the merged report; the caller appends its tabs' caches and logs the total.
    pub(in crate::studio) fn evict_for_pressure(
        &mut self,
        request: &CacheEvictionRequest,
        overlays: &Arc<Mutex<CleanOverlaysModel>>,
    ) -> CacheEvictionReport {
        let mut report = self.evict_source_page_gpu_cache(request);
        match overlays.lock() {
            Ok(mut model) => {
                report.append(model.evict_cache(request));
            }
            Err(_) => runtime_log::log_warn(
                "[memory] failed to lock CleanOverlaysModel while evicting page cache",
            ),
        }
        report
    }

    fn source_page_gpu_memory_snapshot(
        &self,
        active_pages: &HashSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        let mut resources = Vec::new();
        for (page_idx, page_texture) in &self.textures {
            let visible = active_pages.contains(page_idx);
            let linear_bytes = page_texture.estimated_linear_gpu_bytes();
            if linear_bytes > 0 {
                resources.push(CacheResourceInfo {
                    id: format!("source-page-linear-{page_idx}"),
                    kind: CacheResourceKind::PageLinearGpu,
                    page_idx: Some(*page_idx),
                    estimated_bytes: linear_bytes,
                    last_used_frame: page_texture.linear_last_used_frame,
                    reload_cost: CacheReloadCost::Cheap,
                    dirty: false,
                    visible,
                    reconstructable: true,
                });
            }
            let nearest_bytes = page_texture.estimated_nearest_gpu_bytes();
            if nearest_bytes > 0 {
                resources.push(CacheResourceInfo {
                    id: format!("source-page-nearest-{page_idx}"),
                    kind: CacheResourceKind::PageNearestGpu,
                    page_idx: Some(*page_idx),
                    estimated_bytes: nearest_bytes,
                    last_used_frame: page_texture.nearest_last_used_frame,
                    reload_cost: CacheReloadCost::Cheap,
                    dirty: false,
                    visible,
                    reconstructable: true,
                });
            }
        }
        resources
    }

    fn evict_source_page_gpu_cache(
        &mut self,
        request: &CacheEvictionRequest,
    ) -> CacheEvictionReport {
        let resources = self.source_page_gpu_memory_snapshot(
            &request.pinned_pages.iter().copied().collect::<HashSet<_>>(),
        );
        let report = select_eviction_candidates(&resources, request);
        for resource in &report.resources {
            let Some(page_idx) = resource.page_idx else {
                continue;
            };
            let Some(page_texture) = self.textures.get_mut(&page_idx) else {
                continue;
            };
            match resource.kind {
                CacheResourceKind::PageNearestGpu => page_texture.drop_nearest_textures(),
                CacheResourceKind::PageLinearGpu => page_texture.drop_linear_textures(),
                CacheResourceKind::SourcePageCpu
                | CacheResourceKind::CleanOverlayGpu
                | CacheResourceKind::CleanOverlayCpu
                | CacheResourceKind::DetectorMaskGpu
                | CacheResourceKind::CleaningMaskGpu
                | CacheResourceKind::TypingMaskGpu
                | CacheResourceKind::TextOverlayGpu
                | CacheResourceKind::PreviewGpu
                | CacheResourceKind::OcrPageCpu => {}
            }
        }
        report
    }
}

const fn pressure_target_free_bytes(pressure: MemoryPressure) -> u64 {
    match pressure {
        MemoryPressure::Normal => 0,
        MemoryPressure::Soft => SOFT_PRESSURE_TARGET_FREE_BYTES,
        MemoryPressure::Hard => HARD_PRESSURE_TARGET_FREE_BYTES,
        MemoryPressure::Critical => u64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::PressureTick;
    use crate::memory_manager::{MemoryPressure, MemoryProfile};
    use std::collections::BTreeSet;

    /// The request keeps the classification's profile and pressure and scales its byte target
    /// with the pressure level.
    #[test]
    fn pressure_tick_builds_the_request_of_its_classification() {
        let pinned = BTreeSet::from([3usize, 4]);
        let soft = PressureTick { profile: MemoryProfile::Medium, pressure: MemoryPressure::Soft }.eviction_request(pinned.clone());
        assert_eq!(soft.profile, MemoryProfile::Medium);
        assert_eq!(soft.pressure, MemoryPressure::Soft);
        assert_eq!(soft.target_free_bytes, 256 * 1024 * 1024);
        assert_eq!(soft.pinned_pages, pinned);
        let hard = PressureTick { profile: MemoryProfile::Minimal, pressure: MemoryPressure::Hard }.eviction_request(BTreeSet::new());
        assert_eq!(hard.target_free_bytes, 768 * 1024 * 1024);
        let critical = PressureTick { profile: MemoryProfile::Minimal, pressure: MemoryPressure::Critical }.eviction_request(BTreeSet::new());
        assert_eq!(critical.target_free_bytes, u64::MAX);
    }
}
