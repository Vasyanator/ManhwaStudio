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
            Self::Minimal => t!("memory.profile.minimal"),
            Self::Low => t!("memory.profile.low"),
            Self::Medium => t!("memory.profile.medium"),
            Self::Maximum => t!("memory.profile.maximum"),
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

    /// Total-bytes budget for the clean-overlay undo history, in COMPRESSED
    /// (zstd) bytes, scaled by the active memory profile.
    ///
    /// This bounds the sum of retained `RasterDiff` payloads on the undo stack
    /// (see `CleanOverlaysModel`). It is independent of the source-page cache
    /// budget: undo history is user-editable state, not a reconstructable cache,
    /// so it gets its own cap. A single edit larger than the whole budget is
    /// still retained (there is always at least one undoable step), so this is a
    /// soft target for the accumulated history rather than a hard per-edit limit.
    #[must_use]
    pub fn clean_overlay_undo_bytes(self) -> u64 {
        match self.profile {
            MemoryProfile::Minimal => 64 * MIB,
            MemoryProfile::Low => 128 * MIB,
            MemoryProfile::Medium => 256 * MIB,
            MemoryProfile::Maximum => 512 * MIB,
        }
    }

    /// [`Self::clean_overlay_undo_bytes`] as `usize` for the history engine's
    /// budget API. Saturates to `usize::MAX` on the (unreachable for these small
    /// caps) 32-bit overflow, never panicking.
    #[must_use]
    pub fn clean_overlay_undo_bytes_usize(self) -> usize {
        usize::try_from(self.clean_overlay_undo_bytes()).unwrap_or(usize::MAX)
    }

    /// Total-bytes budget for the PS-editor per-page undo history, in COMPRESSED
    /// (zstd) bytes, scaled by the active memory profile.
    ///
    /// Bounds the sum of retained `RasterDiff` payloads on the PS editor's brush
    /// undo stack (see `tabs::ps_editor::edit_op`). Sibling of
    /// [`Self::clean_overlay_undo_bytes`] and uses the same tiers: undo history is
    /// user-editable state, not a reconstructable cache, so it gets its own cap. A
    /// single edit larger than the whole budget is still retained (there is always
    /// at least one undoable step), so this is a soft target for the accumulated
    /// history rather than a hard per-edit limit.
    #[must_use]
    pub fn ps_editor_undo_bytes(self) -> u64 {
        match self.profile {
            MemoryProfile::Minimal => 64 * MIB,
            MemoryProfile::Low => 128 * MIB,
            MemoryProfile::Medium => 256 * MIB,
            MemoryProfile::Maximum => 512 * MIB,
        }
    }

    /// [`Self::ps_editor_undo_bytes`] as `usize` for the history engine's budget
    /// API. Saturates to `usize::MAX` on the (unreachable for these small caps)
    /// 32-bit overflow, never panicking.
    #[must_use]
    pub fn ps_editor_undo_bytes_usize(self) -> usize {
        usize::try_from(self.ps_editor_undo_bytes()).unwrap_or(usize::MAX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAvailability {
    pub available_bytes: u64,
    pub total_bytes: u64,
}

/// Reports physical memory availability for pressure/eviction decisions.
///
/// Returns both currently available (reclaimable) memory and total physical
/// memory. On Linux the values come from `/proc/meminfo` (`MemAvailable` /
/// `MemTotal`); on macOS from `vm_stat` (free+inactive+speculative pages, an
/// approximation of reclaimable memory) plus `sysctl -n hw.memsize` (total).
/// Returns `None` when the underlying source cannot be read or parsed, which the
/// caller treats as "memory pressure unknown".
#[must_use]
pub fn current_memory_availability() -> Option<MemoryAvailability> {
    #[cfg(target_os = "linux")]
    {
        read_linux_meminfo_availability()
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_availability()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

/// TTL for the macOS memory-probe cache. Unlike Linux `/proc/meminfo` (a
/// microsecond read), each macOS probe fork+execs `sysctl` and `vm_stat` (several
/// ms each). 750ms is short enough to keep the 1s pressure poll and eviction
/// decisions responsive, yet long enough to collapse bursts (e.g. per-page cache
/// promotion) into a single probe per window.
#[cfg(target_os = "macos")]
const MACOS_MEM_CACHE_TTL: std::time::Duration = std::time::Duration::from_millis(750);

/// Cached macOS physical-memory availability.
///
/// Wraps [`probe_macos_availability`] in a short-TTL in-process cache (see
/// [`MACOS_MEM_CACHE_TTL`]) so rapid successive callers reuse a recent value
/// instead of each spawning `sysctl`/`vm_stat`. Same return contract as the raw
/// probe: `None` means "availability unknown". Cache lives ONLY on macOS; the
/// Linux/Windows paths call their probes directly and are unaffected.
#[cfg(target_os = "macos")]
fn read_macos_availability() -> Option<MemoryAvailability> {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    // Module-local TTL cache: a justified, bounded exception to the "no global
    // mutable state" rule (a pure timed cache, not shared logic state). A poisoned
    // lock falls back to a fresh probe rather than panicking.
    static CACHE: OnceLock<Mutex<Option<(Instant, Option<MemoryAvailability>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some((stamped_at, value)) = guard.as_ref()
        && stamped_at.elapsed() < MACOS_MEM_CACHE_TTL
    {
        return *value;
    }
    // Probe WITHOUT holding the lock: never fork+exec under a Mutex.
    let value = probe_macos_availability();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), value));
    }
    value
}

/// Reads macOS physical memory availability via `vm_stat` (available) and
/// `sysctl -n hw.memsize` (total). Returns `None` if either short command fails
/// or its output cannot be parsed, so the caller keeps treating pressure as
/// unknown instead of acting on a wrong number.
///
/// This is the raw (uncached) probe; callers go through
/// [`read_macos_availability`], which caches the result for a short TTL.
#[cfg(target_os = "macos")]
fn probe_macos_availability() -> Option<MemoryAvailability> {
    use std::process::Command;

    // Total physical memory: `hw.memsize` is the authoritative byte count.
    let memsize_out = Command::new("sysctl").args(["-n", "hw.memsize"]).output().ok()?;
    if !memsize_out.status.success() {
        crate::runtime_log::log_warn("[memory] `sysctl -n hw.memsize` exited with failure status");
        return None;
    }
    let total_bytes = parse_macos_sysctl_u64(&String::from_utf8_lossy(&memsize_out.stdout))?;

    // Available (reclaimable) memory: approximated from `vm_stat` page counts.
    let vm_out = Command::new("vm_stat").output().ok()?;
    if !vm_out.status.success() {
        crate::runtime_log::log_warn("[memory] `vm_stat` exited with failure status");
        return None;
    }
    let available_bytes = parse_vm_stat_available_bytes(&String::from_utf8_lossy(&vm_out.stdout))?;

    Some(MemoryAvailability {
        available_bytes,
        total_bytes,
    })
}

/// Parses the single-number output of `sysctl -n <key>` into `u64`.
/// Returns `None` on empty/garbled output.
#[cfg(any(target_os = "macos", test))]
fn parse_macos_sysctl_u64(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

/// Extracts the page size in bytes from the `vm_stat` header line, which ends
/// with "(page size of N bytes)". Returns `None` if the marker is absent or the
/// number cannot be parsed.
#[cfg(any(target_os = "macos", test))]
fn parse_vm_stat_page_size(line: &str) -> Option<u64> {
    line.split("page size of")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
}

/// Parses a `vm_stat` "Pages <name>:" row into its page count, stripping the
/// trailing '.' that `vm_stat` appends to every count. Returns `None` when the
/// prefix does not match or the number cannot be parsed.
#[cfg(any(target_os = "macos", test))]
fn parse_vm_stat_pages(line: &str, key: &str) -> Option<u64> {
    line.strip_prefix(key)?
        .trim()
        .trim_end_matches('.')
        .parse::<u64>()
        .ok()
}

/// Approximates macOS available memory from `vm_stat` output.
///
/// macOS exposes no single "available memory" counter, so this mirrors the
/// meaning of Linux `MemAvailable` (reclaimable memory) by summing the pages that
/// can be reclaimed under pressure: free + inactive + speculative, multiplied by
/// the page size from the `vm_stat` header. Wired/active/compressed pages are
/// excluded because they are not readily reclaimable.
///
/// Uses checked arithmetic; on the (physically unreachable) overflow it logs a
/// warning and returns `None`. Returns `None` if the page size or any required
/// page count is missing or unparseable, so the caller keeps pressure "unknown".
#[cfg(any(target_os = "macos", test))]
fn parse_vm_stat_available_bytes(text: &str) -> Option<u64> {
    let mut page_size: Option<u64> = None;
    let mut free: Option<u64> = None;
    let mut inactive: Option<u64> = None;
    let mut speculative: Option<u64> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if page_size.is_none()
            && let Some(size) = parse_vm_stat_page_size(line)
        {
            page_size = Some(size);
        }
        if let Some(v) = parse_vm_stat_pages(line, "Pages free:") {
            free = Some(v);
        } else if let Some(v) = parse_vm_stat_pages(line, "Pages inactive:") {
            inactive = Some(v);
        } else if let Some(v) = parse_vm_stat_pages(line, "Pages speculative:") {
            speculative = Some(v);
        }
    }

    let page_size = page_size?;
    // free + inactive + speculative ≈ pages reclaimable under memory pressure.
    let pages = free?.checked_add(inactive?)?.checked_add(speculative?)?;
    match pages.checked_mul(page_size) {
        Some(bytes) => Some(bytes),
        None => {
            crate::runtime_log::log_warn(
                "[memory] vm_stat page count * page size overflowed u64; reporting unknown",
            );
            None
        }
    }
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

    /// Realistic `vm_stat` capture (16 KiB pages). The parser must strip the
    /// trailing '.', read the page size from the header, and sum
    /// free+inactive+speculative pages.
    const VM_STAT_SAMPLE: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                               10000.\n\
Pages active:                            234567.\n\
Pages inactive:                            5000.\n\
Pages speculative:                         2000.\n\
Pages throttled:                              0.\n\
Pages wired down:                        333333.\n\
Pages purgeable:                          44444.\n\
\"Translation faults\":                1234567890.\n\
Pages copy-on-write:                   12345678.\n";

    #[test]
    fn parses_macos_vm_stat_available_bytes() {
        // (10000 free + 5000 inactive + 2000 speculative) * 16384 = 278_528_000.
        assert_eq!(
            parse_vm_stat_available_bytes(VM_STAT_SAMPLE),
            Some(17_000 * 16_384)
        );
    }

    #[test]
    fn macos_vm_stat_page_size_parsed_from_header() {
        assert_eq!(
            parse_vm_stat_page_size("Mach Virtual Memory Statistics: (page size of 4096 bytes)"),
            Some(4096)
        );
        assert_eq!(parse_vm_stat_page_size("Pages free: 1."), None);
    }

    #[test]
    fn macos_vm_stat_bad_input_returns_none() {
        // Missing header/page size and missing required rows must not panic.
        assert_eq!(parse_vm_stat_available_bytes(""), None);
        assert_eq!(
            parse_vm_stat_available_bytes("Pages free: 10.\nPages inactive: 5.\n"),
            None,
            "no page-size header => unknown"
        );
    }

    #[test]
    fn parses_macos_sysctl_memsize() {
        assert_eq!(parse_macos_sysctl_u64("  17179869184\n"), Some(17_179_869_184));
        assert_eq!(parse_macos_sysctl_u64("garbage"), None);
        assert_eq!(parse_macos_sysctl_u64(""), None);
    }
}
