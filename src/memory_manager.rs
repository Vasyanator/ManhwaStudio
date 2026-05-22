/*
FILE OVERVIEW: src/memory_manager.rs
Project-wide image cache memory policy.

Main items:
- `MemoryProfile`: user-selected memory usage profile persisted in `user_config.json`.
- `MemoryManager`: small shared runtime handle for querying and hot-applying the profile.
- `MemoryPressure` / `MemoryBudget`: pressure classification and profile-derived budgets.
- `CacheResourceInfo` and eviction request/report types: typed policy boundary for cache owners.

Notes:
This module does not own pixels, `TextureHandle`s, or tab state. Cache owners keep storage local and
may use the pure policy functions here to choose reconstructable least-recently-used resources.
*/

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader};
use std::sync::RwLock;

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryProfile {
    Minimal,
    Low,
    #[default]
    Medium,
    Maximum,
}

impl MemoryProfile {
    pub const ALL: [Self; 4] = [Self::Minimal, Self::Low, Self::Medium, Self::Maximum];

    #[must_use]
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::Maximum => "maximum",
        }
    }

    #[must_use]
    pub fn from_config_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "maximum" => Some(Self::Maximum),
            _ => None,
        }
    }

    #[must_use]
    pub fn display_name_ru(self) -> &'static str {
        match self {
            Self::Minimal => "Минимум",
            Self::Low => "Низкое",
            Self::Medium => "Среднее",
            Self::Maximum => "Максимум",
        }
    }
}

#[derive(Debug)]
pub struct MemoryManager {
    profile: RwLock<MemoryProfile>,
}

impl MemoryManager {
    #[must_use]
    pub fn new(profile: MemoryProfile) -> Self {
        Self {
            profile: RwLock::new(profile),
        }
    }

    #[must_use]
    pub fn profile(&self) -> MemoryProfile {
        self.profile
            .read()
            .map(|guard| *guard)
            .unwrap_or_else(|poisoned| *poisoned.into_inner())
    }

    pub fn set_profile(&self, profile: MemoryProfile) {
        match self.profile.write() {
            Ok(mut guard) => *guard = profile,
            Err(poisoned) => *poisoned.into_inner() = profile,
        }
    }

    #[must_use]
    pub fn budget(&self) -> MemoryBudget {
        MemoryBudget::for_profile(self.profile())
    }
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new(MemoryProfile::default())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryPressure {
    Normal,
    Soft,
    Hard,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    pub profile: MemoryProfile,
    pub source_page_cpu_cache_bytes: u64,
    pub ocr_page_cpu_cache_bytes: u64,
    pub visible_neighbor_pages: usize,
    pub keep_linear_gpu_outside_window: bool,
    pub keep_clean_cpu_snapshots_until_critical: bool,
}

impl MemoryBudget {
    #[must_use]
    pub fn for_profile(profile: MemoryProfile) -> Self {
        match profile {
            MemoryProfile::Minimal => Self {
                profile,
                source_page_cpu_cache_bytes: 0,
                ocr_page_cpu_cache_bytes: 128 * MIB,
                visible_neighbor_pages: 1,
                keep_linear_gpu_outside_window: false,
                keep_clean_cpu_snapshots_until_critical: false,
            },
            MemoryProfile::Low => Self {
                profile,
                source_page_cpu_cache_bytes: 512 * MIB,
                ocr_page_cpu_cache_bytes: 256 * MIB,
                visible_neighbor_pages: 1,
                keep_linear_gpu_outside_window: true,
                keep_clean_cpu_snapshots_until_critical: true,
            },
            MemoryProfile::Medium => Self {
                profile,
                source_page_cpu_cache_bytes: 1536 * MIB,
                ocr_page_cpu_cache_bytes: 512 * MIB,
                visible_neighbor_pages: 2,
                keep_linear_gpu_outside_window: true,
                keep_clean_cpu_snapshots_until_critical: true,
            },
            MemoryProfile::Maximum => Self {
                profile,
                source_page_cpu_cache_bytes: 4 * GIB,
                ocr_page_cpu_cache_bytes: GIB,
                visible_neighbor_pages: 4,
                keep_linear_gpu_outside_window: true,
                keep_clean_cpu_snapshots_until_critical: true,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAvailability {
    pub available_bytes: u64,
    pub total_bytes: u64,
}

#[must_use]
pub fn current_memory_availability() -> Option<MemoryAvailability> {
    #[cfg(target_os = "linux")]
    {
        read_linux_meminfo_availability()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_linux_meminfo_availability() -> Option<MemoryAvailability> {
    let file = File::open("/proc/meminfo").ok()?;
    let mut total_kib = None;
    let mut available_kib = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Some(value) = parse_meminfo_kib(&line, "MemTotal:") {
            total_kib = Some(value);
        } else if let Some(value) = parse_meminfo_kib(&line, "MemAvailable:") {
            available_kib = Some(value);
        }
        if total_kib.is_some() && available_kib.is_some() {
            break;
        }
    }
    Some(MemoryAvailability {
        available_bytes: available_kib?.saturating_mul(1024),
        total_bytes: total_kib?.saturating_mul(1024),
    })
}

#[cfg(target_os = "linux")]
fn parse_meminfo_kib(line: &str, key: &str) -> Option<u64> {
    line.strip_prefix(key)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
}

#[derive(Debug, Clone, Copy)]
struct PressureThresholds {
    soft_percent: u64,
    hard_percent: u64,
    critical_percent: u64,
    soft_bytes: u64,
    hard_bytes: u64,
    critical_bytes: u64,
}

impl PressureThresholds {
    fn for_profile(profile: MemoryProfile) -> Self {
        match profile {
            MemoryProfile::Minimal => Self {
                soft_percent: 25,
                hard_percent: 15,
                critical_percent: 8,
                soft_bytes: 4 * GIB,
                hard_bytes: 2 * GIB,
                critical_bytes: GIB,
            },
            MemoryProfile::Low => Self {
                soft_percent: 22,
                hard_percent: 13,
                critical_percent: 7,
                soft_bytes: 3584 * MIB,
                hard_bytes: 1792 * MIB,
                critical_bytes: 896 * MIB,
            },
            MemoryProfile::Medium => Self {
                soft_percent: 20,
                hard_percent: 12,
                critical_percent: 6,
                soft_bytes: 3 * GIB,
                hard_bytes: 1536 * MIB,
                critical_bytes: 768 * MIB,
            },
            MemoryProfile::Maximum => Self {
                soft_percent: 15,
                hard_percent: 10,
                critical_percent: 5,
                soft_bytes: 2 * GIB,
                hard_bytes: 1280 * MIB,
                critical_bytes: 640 * MIB,
            },
        }
    }
}

#[must_use]
pub fn classify_memory_pressure(
    profile: MemoryProfile,
    availability: MemoryAvailability,
) -> MemoryPressure {
    if availability.total_bytes == 0 {
        return MemoryPressure::Normal;
    }

    let thresholds = PressureThresholds::for_profile(profile);
    if below_threshold(
        availability,
        thresholds.critical_percent,
        thresholds.critical_bytes,
    ) {
        MemoryPressure::Critical
    } else if below_threshold(availability, thresholds.hard_percent, thresholds.hard_bytes) {
        MemoryPressure::Hard
    } else if below_threshold(availability, thresholds.soft_percent, thresholds.soft_bytes) {
        MemoryPressure::Soft
    } else {
        MemoryPressure::Normal
    }
}

fn below_threshold(availability: MemoryAvailability, percent: u64, bytes: u64) -> bool {
    availability.available_bytes < bytes
        || u128::from(availability.available_bytes) * 100
            < u128::from(availability.total_bytes) * u128::from(percent)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheResourceKind {
    PageLinearGpu,
    PageNearestGpu,
    SourcePageCpu,
    CleanOverlayGpu,
    CleanOverlayCpu,
    DetectorMaskGpu,
    CleaningMaskGpu,
    TypingMaskGpu,
    TextOverlayGpu,
    PreviewGpu,
    OcrPageCpu,
}

impl CacheResourceKind {
    pub const ALL: [Self; 11] = [
        Self::PageLinearGpu,
        Self::PageNearestGpu,
        Self::SourcePageCpu,
        Self::CleanOverlayGpu,
        Self::CleanOverlayCpu,
        Self::DetectorMaskGpu,
        Self::CleaningMaskGpu,
        Self::TypingMaskGpu,
        Self::TextOverlayGpu,
        Self::PreviewGpu,
        Self::OcrPageCpu,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheReloadCost {
    Cheap,
    DecodeFromDisk,
    RebuildFromModel,
    Expensive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheResourceInfo {
    pub id: String,
    pub kind: CacheResourceKind,
    pub page_idx: Option<usize>,
    pub estimated_bytes: u64,
    pub last_used_frame: u64,
    pub reload_cost: CacheReloadCost,
    pub dirty: bool,
    pub visible: bool,
    pub reconstructable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEvictionRequest {
    pub profile: MemoryProfile,
    pub pressure: MemoryPressure,
    pub target_free_bytes: u64,
    pub pinned_pages: BTreeSet<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheEvictionReport {
    pub resources: Vec<CacheResourceInfo>,
    pub estimated_freed_bytes: u64,
}

#[cfg(test)]
#[must_use]
pub fn pinned_page_window(
    current_page: usize,
    page_count: usize,
    before: usize,
    after: usize,
) -> BTreeSet<usize> {
    if page_count == 0 {
        return BTreeSet::new();
    }
    let start = current_page.saturating_sub(before);
    let end = current_page
        .saturating_add(after)
        .min(page_count.saturating_sub(1));
    (start..=end).collect()
}

#[must_use]
pub fn select_eviction_candidates(
    resources: &[CacheResourceInfo],
    request: &CacheEvictionRequest,
) -> CacheEvictionReport {
    if request.pressure == MemoryPressure::Normal || request.target_free_bytes == 0 {
        return CacheEvictionReport {
            resources: Vec::new(),
            estimated_freed_bytes: 0,
        };
    }

    let mut candidates: Vec<_> = resources
        .iter()
        .filter(|resource| is_evictable(resource, request))
        .cloned()
        .collect();

    candidates.sort_by(|left, right| {
        eviction_priority(left.kind, request)
            .cmp(&eviction_priority(right.kind, request))
            .then_with(|| left.last_used_frame.cmp(&right.last_used_frame))
            .then_with(|| left.reload_cost.cmp(&right.reload_cost))
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut selected = Vec::new();
    let mut freed = 0_u64;
    for candidate in candidates {
        freed = freed.saturating_add(candidate.estimated_bytes);
        selected.push(candidate);
        if freed >= request.target_free_bytes {
            break;
        }
    }

    CacheEvictionReport {
        resources: selected,
        estimated_freed_bytes: freed,
    }
}

fn is_evictable(resource: &CacheResourceInfo, request: &CacheEvictionRequest) -> bool {
    if resource.dirty || !resource.reconstructable || resource.visible {
        return false;
    }
    if resource
        .page_idx
        .is_some_and(|page_idx| request.pinned_pages.contains(&page_idx))
    {
        return false;
    }

    match resource.kind {
        CacheResourceKind::PageLinearGpu => {
            request.profile == MemoryProfile::Minimal
                || request.pressure == MemoryPressure::Critical
        }
        CacheResourceKind::CleanOverlayCpu => request.pressure == MemoryPressure::Critical,
        CacheResourceKind::PageNearestGpu
        | CacheResourceKind::SourcePageCpu
        | CacheResourceKind::CleanOverlayGpu
        | CacheResourceKind::DetectorMaskGpu
        | CacheResourceKind::CleaningMaskGpu
        | CacheResourceKind::TypingMaskGpu
        | CacheResourceKind::TextOverlayGpu
        | CacheResourceKind::PreviewGpu
        | CacheResourceKind::OcrPageCpu => true,
    }
}

fn eviction_priority(kind: CacheResourceKind, request: &CacheEvictionRequest) -> u8 {
    debug_assert!(CacheResourceKind::ALL.contains(&kind));
    match kind {
        CacheResourceKind::PageNearestGpu => 0,
        CacheResourceKind::PreviewGpu => 1,
        CacheResourceKind::DetectorMaskGpu
        | CacheResourceKind::CleaningMaskGpu
        | CacheResourceKind::TypingMaskGpu => 2,
        CacheResourceKind::TextOverlayGpu => 3,
        CacheResourceKind::CleanOverlayGpu => 4,
        CacheResourceKind::SourcePageCpu => 5,
        CacheResourceKind::OcrPageCpu => 6,
        CacheResourceKind::PageLinearGpu
            if request.profile == MemoryProfile::Minimal
                || request.pressure == MemoryPressure::Critical =>
        {
            7
        }
        CacheResourceKind::CleanOverlayCpu if request.pressure == MemoryPressure::Critical => 8,
        CacheResourceKind::PageLinearGpu | CacheResourceKind::CleanOverlayCpu => u8::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(id: &str, kind: CacheResourceKind, page_idx: Option<usize>) -> CacheResourceInfo {
        CacheResourceInfo {
            id: id.to_string(),
            kind,
            page_idx,
            estimated_bytes: 10,
            last_used_frame: 10,
            reload_cost: CacheReloadCost::Cheap,
            dirty: false,
            visible: false,
            reconstructable: true,
        }
    }

    #[test]
    fn memory_profile_order_matches_usage_levels() {
        assert!(MemoryProfile::Minimal < MemoryProfile::Low);
        assert!(MemoryProfile::Low < MemoryProfile::Medium);
        assert!(MemoryProfile::Medium < MemoryProfile::Maximum);
        assert_eq!(MemoryProfile::default(), MemoryProfile::Medium);
    }

    #[test]
    fn pressure_threshold_boundaries_are_below_not_equal() {
        assert_eq!(
            classify_memory_pressure(
                MemoryProfile::Medium,
                MemoryAvailability {
                    available_bytes: 3 * GIB,
                    total_bytes: 15 * GIB
                }
            ),
            MemoryPressure::Normal
        );
        assert_eq!(
            classify_memory_pressure(
                MemoryProfile::Medium,
                MemoryAvailability {
                    available_bytes: (3 * GIB).saturating_sub(1),
                    total_bytes: 15 * GIB
                }
            ),
            MemoryPressure::Soft
        );
        assert_eq!(
            classify_memory_pressure(
                MemoryProfile::Medium,
                MemoryAvailability {
                    available_bytes: 1536 * MIB,
                    total_bytes: 12_800 * MIB,
                }
            ),
            MemoryPressure::Soft
        );
        assert_eq!(
            classify_memory_pressure(
                MemoryProfile::Medium,
                MemoryAvailability {
                    available_bytes: (1536 * MIB).saturating_sub(1),
                    total_bytes: 12_800 * MIB,
                }
            ),
            MemoryPressure::Hard
        );
        assert_eq!(
            classify_memory_pressure(
                MemoryProfile::Medium,
                MemoryAvailability {
                    available_bytes: 768 * MIB,
                    total_bytes: 12_800 * MIB,
                }
            ),
            MemoryPressure::Hard
        );
        assert_eq!(
            classify_memory_pressure(
                MemoryProfile::Medium,
                MemoryAvailability {
                    available_bytes: (768 * MIB).saturating_sub(1),
                    total_bytes: 12_800 * MIB,
                }
            ),
            MemoryPressure::Critical
        );
    }

    #[test]
    fn eviction_order_excludes_dirty_and_non_reconstructable_resources() {
        let mut dirty = resource("dirty", CacheResourceKind::PageNearestGpu, Some(1));
        dirty.dirty = true;
        let mut permanent = resource("permanent", CacheResourceKind::PreviewGpu, None);
        permanent.reconstructable = false;
        let source_cpu = resource("source-cpu", CacheResourceKind::SourcePageCpu, Some(9));
        let nearest = resource("nearest", CacheResourceKind::PageNearestGpu, Some(8));
        let preview = resource("preview", CacheResourceKind::PreviewGpu, None);
        let resources = vec![source_cpu, dirty, preview, permanent, nearest];

        let report = select_eviction_candidates(
            &resources,
            &CacheEvictionRequest {
                profile: MemoryProfile::Medium,
                pressure: MemoryPressure::Hard,
                target_free_bytes: 30,
                pinned_pages: BTreeSet::new(),
            },
        );

        let ids: Vec<_> = report
            .resources
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(ids, vec!["nearest", "preview", "source-cpu"]);
        assert_eq!(report.estimated_freed_bytes, 30);
    }

    #[test]
    fn page_window_pinning_protects_current_neighbors() {
        let pinned_pages = pinned_page_window(5, 10, 1, 2);
        assert_eq!(pinned_pages, BTreeSet::from([4, 5, 6, 7]));

        let resources = vec![
            resource("pinned-before", CacheResourceKind::PageNearestGpu, Some(4)),
            resource("pinned-current", CacheResourceKind::PageNearestGpu, Some(5)),
            resource("outside", CacheResourceKind::PageNearestGpu, Some(8)),
        ];
        let report = select_eviction_candidates(
            &resources,
            &CacheEvictionRequest {
                profile: MemoryProfile::Medium,
                pressure: MemoryPressure::Soft,
                target_free_bytes: 10,
                pinned_pages,
            },
        );

        let ids: Vec<_> = report
            .resources
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(ids, vec!["outside"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_meminfo_kib_values() {
        assert_eq!(
            parse_meminfo_kib("MemAvailable:   12345 kB", "MemAvailable:"),
            Some(12_345)
        );
        assert_eq!(parse_meminfo_kib("SwapFree: 1 kB", "MemAvailable:"), None);
    }
}
