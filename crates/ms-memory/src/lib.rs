/*
FILE OVERVIEW: crates/ms-memory/src/lib.rs
Project-wide image cache memory policy.

Main items:
- `MemoryProfile`: user-selected memory usage profile persisted in `user_config.json`.
- `MemoryManager`: small shared runtime handle for querying and hot-applying the profile.
- `current_memory_availability`: the OS physical-memory probe (total + available) — the
  workspace's RAM owner: `/proc/meminfo` on Linux and Android, `GlobalMemoryStatusEx` on
  Windows, `sysctlbyname` + mach `host_statistics64` on macOS (no process), `None` elsewhere.
- `MemoryPressure` / `MemoryBudget`: pressure classification and profile-derived budgets.
- `CacheResourceInfo` and eviction request/report types: typed policy boundary for cache owners;
  `CacheEvictionReport::append` is the one owner of summing per-owner reports.

Notes:
This crate does not own pixels, `TextureHandle`s, or tab state. Cache owners keep storage local and
may use the pure policy functions here to choose reconstructable least-recently-used resources.
The binary mounts it under its historical module path with `pub use ms_memory as memory_manager;`
(`src/lib.rs`), so every call site keeps writing `crate::memory_manager::…`.
*/

#![warn(clippy::all)]

// `MemoryProfile::label()` returns a localized profile name, so the crate needs the
// `ms-i18n` macros. `#[macro_export]` + `$crate::` expansion means one crate-root
// `#[macro_use]` is enough, mirroring `src/lib.rs`.
#[macro_use]
extern crate ms_i18n;

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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
/// memory. On Linux and Android the values come from `/proc/meminfo`
/// (`MemAvailable` / `MemTotal`); on Windows from `GlobalMemoryStatusEx`
/// (`ullAvailPhys` / `ullTotalPhys`); on macOS from mach `host_statistics64`
/// (free+inactive+speculative pages, an approximation of reclaimable memory) plus
/// `sysctlbyname("hw.memsize")` (total). Returns `None` when the underlying source
/// cannot be read or parsed (logged), and on every other target, which the caller
/// treats as "memory pressure unknown". Never spawns a process: a file read, one
/// Win32 call or a few syscalls.
#[must_use]
pub fn current_memory_availability() -> Option<MemoryAvailability> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        read_proc_meminfo_availability()
    }
    #[cfg(windows)]
    {
        read_windows_availability()
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_availability()
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", windows, target_os = "macos")))]
    {
        None
    }
}

/// Path of the kernel memory report on Linux and Android.
#[cfg(any(target_os = "linux", target_os = "android"))]
const PROC_MEMINFO: &str = "/proc/meminfo";

/// Reads and parses [`PROC_MEMINFO`]. `None` (logged) when the file cannot be read
/// or lacks `MemTotal` / `MemAvailable` (kernels before 3.14 have no `MemAvailable`).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_proc_meminfo_availability() -> Option<MemoryAvailability> {
    let text = match std::fs::read_to_string(PROC_MEMINFO) {
        Ok(text) => text,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("[memory] cannot read {PROC_MEMINFO}: {err}"));
            return None;
        }
    };
    let parsed = parse_meminfo_availability(&text);
    if parsed.is_none() {
        ms_log::runtime_log::log_warn(format!("[memory] {PROC_MEMINFO} has no parseable MemTotal / MemAvailable line"));
    }
    parsed
}

/// Parses a `/proc/meminfo` text into total and available bytes. `None` unless both
/// `MemTotal:` and `MemAvailable:` are present with a numeric kB value. Pure.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn parse_meminfo_availability(text: &str) -> Option<MemoryAvailability> {
    let mut total_kib = None;
    let mut available_kib = None;
    for line in text.lines() {
        if let Some(value) = parse_meminfo_kib(line, "MemTotal:") {
            total_kib = Some(value);
        } else if let Some(value) = parse_meminfo_kib(line, "MemAvailable:") {
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

/// Parses the kB value of one `/proc/meminfo` line that starts with `key`. Pure.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn parse_meminfo_kib(line: &str, key: &str) -> Option<u64> {
    line.strip_prefix(key)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
}

/// Reads total and available physical memory through `GlobalMemoryStatusEx`.
/// `None` (logged with the Win32 error) when the call fails.
#[cfg(windows)]
fn read_windows_availability() -> Option<MemoryAvailability> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        // `dwLength` must be the struct size; a fixed-size Win32 struct always fits in u32.
        dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).unwrap_or(u32::MAX),
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: `status` is a writable, properly sized `MEMORYSTATUSEX` with `dwLength` set as
    // the API requires; the call only writes into it.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 {
        // SAFETY: reads the calling thread's last-error value; no preconditions.
        let error = unsafe { GetLastError() };
        ms_log::runtime_log::log_warn(format!("[memory] GlobalMemoryStatusEx failed (Win32 error {error})"));
        return None;
    }
    Some(MemoryAvailability { available_bytes: status.ullAvailPhys, total_bytes: status.ullTotalPhys })
}

/// Reads macOS physical memory without spawning a process: total from
/// `sysctlbyname("hw.memsize")`, available from mach `host_statistics64(HOST_VM_INFO64)`
/// page counts times the kernel VM page size (`vm_page_size`), combined by
/// [`macos_available_bytes`]. `None` (logged) when either call fails, so the caller keeps
/// treating pressure as unknown instead of acting on a wrong number. A few syscalls: cheap
/// enough for the 1 s pressure poll without a cache.
#[cfg(target_os = "macos")]
fn read_macos_availability() -> Option<MemoryAvailability> {
    let total_bytes = macos_memsize()?;
    let mut stats = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `stats` is writable storage for one `vm_statistics64`, and `count` holds its size in
    // `integer_t` units as the API requires; the kernel writes at most `count` units.
    let status = unsafe { libc::host_statistics64(macos_host_port(), libc::HOST_VM_INFO64, stats.as_mut_ptr().cast::<libc::integer_t>(), &mut count) };
    if status != libc::KERN_SUCCESS {
        ms_log::runtime_log::log_warn(format!("[memory] host_statistics64(HOST_VM_INFO64) failed (kern_return {status})"));
        return None;
    }
    // SAFETY: the call succeeded, so the kernel filled the struct (it was zero-initialized
    // beforehand, so even a shorter answer leaves only zero fields, all valid integers).
    let stats = unsafe { stats.assume_init() };
    // SAFETY: `vm_page_size` is a libsystem global initialized before `main`; reading it has no
    // side effects.
    let page_size = u64::try_from(unsafe { libc::vm_page_size }).ok()?;
    let available_bytes = macos_available_bytes(
        u64::from(stats.free_count),
        u64::from(stats.inactive_count),
        u64::from(stats.speculative_count),
        page_size,
    )?;
    Some(MemoryAvailability { available_bytes, total_bytes })
}

/// The host port for `host_statistics64`, obtained once per process: every
/// `mach_host_self()` call adds a user reference to the send right, so calling it on each
/// 1 s poll would grow the reference count without bound (libc has no
/// `mach_port_deallocate` to drop it).
#[cfg(target_os = "macos")]
fn macos_host_port() -> libc::mach_port_t {
    static HOST_PORT: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    // libc deprecates its mach bindings in favour of the `mach2` crate; the workspace adds no
    // crate for one call, and the symbol itself is a stable part of libsystem.
    #[expect(deprecated, reason = "libc points to the mach2 crate; one stable libsystem call does not justify a new dependency")]
    // SAFETY: `mach_host_self` has no preconditions and returns a send right to the host port.
    let fetch = || unsafe { libc::mach_host_self() };
    *HOST_PORT.get_or_init(fetch)
}

/// Total physical memory from `sysctlbyname("hw.memsize")` (a 64-bit byte count); `None`
/// (logged) when the call fails.
#[cfg(target_os = "macos")]
fn macos_memsize() -> Option<u64> {
    let mut value: u64 = 0;
    let mut len: libc::size_t = std::mem::size_of::<u64>();
    // SAFETY: the name is a NUL-terminated literal; `value` is writable for `len` bytes and
    // `len` holds that size; no new value is written (null `newp`, length 0).
    let rc = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&raw mut value).cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len != std::mem::size_of::<u64>() || value == 0 {
        ms_log::runtime_log::log_warn(format!("[memory] sysctl hw.memsize failed ({}; len {len})", std::io::Error::last_os_error()));
        return None;
    }
    Some(value)
}

/// Approximates macOS available memory from mach VM page counts.
///
/// macOS exposes no single "available memory" counter, so this mirrors the
/// meaning of Linux `MemAvailable` (reclaimable memory) by summing the pages that
/// can be reclaimed under pressure: free + inactive + speculative, multiplied by
/// the VM page size (what `vm_stat` reports as free / inactive / speculative). Wired /
/// active / compressed pages are excluded because they are not readily reclaimable.
///
/// Uses checked arithmetic; on the (physically unreachable) overflow it logs a
/// warning and returns `None`, as it does for a zero page size. Pure otherwise.
#[cfg(any(target_os = "macos", test))]
fn macos_available_bytes(free: u64, inactive: u64, speculative: u64, page_size: u64) -> Option<u64> {
    if page_size == 0 {
        ms_log::runtime_log::log_warn("[memory] macOS VM page size is 0; reporting unknown");
        return None;
    }
    let bytes = free.checked_add(inactive).and_then(|pages| pages.checked_add(speculative)).and_then(|pages| pages.checked_mul(page_size));
    if bytes.is_none() {
        ms_log::runtime_log::log_warn("[memory] macOS page count * page size overflowed u64; reporting unknown");
    }
    bytes
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

impl CacheEvictionReport {
    /// Adds `other` (one more cache owner's report) to this running total: its resources are
    /// appended in order and its byte estimate is added with saturation, so a `u64::MAX`
    /// ("everything") estimate never wraps. The one owner of summing per-owner reports.
    pub fn append(&mut self, other: Self) {
        self.estimated_freed_bytes = self.estimated_freed_bytes.saturating_add(other.estimated_freed_bytes);
        self.resources.extend(other.resources);
    }
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
    fn eviction_report_append_keeps_order_and_saturates_bytes() {
        let mut total = CacheEvictionReport {
            resources: vec![resource("a", CacheResourceKind::PageLinearGpu, Some(0))],
            estimated_freed_bytes: u64::MAX - 1,
        };
        total.append(CacheEvictionReport {
            resources: vec![resource("b", CacheResourceKind::CleanOverlayGpu, Some(1)), resource("c", CacheResourceKind::PreviewGpu, None)],
            estimated_freed_bytes: 5,
        });
        let ids: Vec<&str> = total.resources.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(total.estimated_freed_bytes, u64::MAX);
        total.append(CacheEvictionReport::default());
        assert_eq!(total.resources.len(), 3);
        assert_eq!(total.estimated_freed_bytes, u64::MAX);
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

    #[test]
    fn parses_linux_meminfo_kib_values() {
        assert_eq!(
            parse_meminfo_kib("MemAvailable:   12345 kB", "MemAvailable:"),
            Some(12_345)
        );
        assert_eq!(parse_meminfo_kib("SwapFree: 1 kB", "MemAvailable:"), None);
    }

    /// Head of a real `/proc/meminfo` (Linux and Android share the format).
    const MEMINFO_SAMPLE: &str = "MemTotal:       65536000 kB\n\
MemFree:         1000000 kB\n\
MemAvailable:   32768000 kB\n\
Buffers:          123456 kB\n";

    #[test]
    fn parses_proc_meminfo_total_and_available() {
        assert_eq!(
            parse_meminfo_availability(MEMINFO_SAMPLE),
            Some(MemoryAvailability { available_bytes: 32_768_000 * 1024, total_bytes: 65_536_000 * 1024 })
        );
    }

    #[test]
    fn proc_meminfo_without_mem_available_is_unknown() {
        // Kernels before 3.14 report no MemAvailable: unknown, never "0 free".
        assert_eq!(parse_meminfo_availability("MemTotal: 1024 kB\nMemFree: 512 kB\n"), None);
        assert_eq!(parse_meminfo_availability(""), None);
        assert_eq!(parse_meminfo_availability("MemTotal: abc kB\nMemAvailable: 1 kB\n"), None);
    }

    #[test]
    fn macos_available_is_free_inactive_speculative_pages() {
        // (10000 free + 5000 inactive + 2000 speculative) * 16 KiB pages.
        assert_eq!(macos_available_bytes(10_000, 5_000, 2_000, 16_384), Some(17_000 * 16_384));
        assert_eq!(macos_available_bytes(0, 0, 0, 4_096), Some(0));
    }

    #[test]
    fn macos_available_rejects_zero_page_size_and_overflow() {
        assert_eq!(macos_available_bytes(1, 1, 1, 0), None);
        assert_eq!(macos_available_bytes(u64::MAX, 1, 0, 4_096), None);
        assert_eq!(macos_available_bytes(u64::MAX / 2, 0, 0, 4_096), None);
    }
}
