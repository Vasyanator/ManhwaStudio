/*
File: src/window_geometry.rs

Purpose:
Owns WHERE the program's OS window opens and where it is left: the user's primary-monitor
choice, the last known geometry of the studio window, and the runtime plumbing that keeps
both in `user_config.json`.

Main responsibilities:
- decode/encode the self-versioned `Window` section of `user_config.json`;
- plan the startup geometry for a `ViewportBuilder` BEFORE any window exists;
- resolve a stored monitor key against the monitors winit reports at runtime, degrading to
  the largest monitor with an explicit logged reason;
- publish the live monitor list for the settings UI;
- observe the live window each frame and persist changes through a coalescing writer thread.

Key structures:
- `MonitorKey`: synthetic monitor identity (name + physical rect + DPI scale).
- `WindowRect`: window geometry in zoom-independent logical pixels.
- `WindowSettings`: serde mirror of the `Window` config section.
- `MonitorSnapshot`: process-wide mirror of the monitors the live window can see.
- `GeometrySnapshot`: one sampled window state, and the fold rule two of them combine under.
- `WindowGeometryTracker`: per-frame observer, relocation driver and owner of the writer thread.

Key functions:
- `window_settings_from_user_settings`, `plan_startup_placement`, `apply_placement`
- `resolve_monitor`, `largest_monitor_index`, `should_relocate`
- `refresh_monitors`, `monitor_snapshot`, `persist_preferred_monitor`, `request_relocation`
- `update_window_section`, `persist_geometry`, `spawn_geometry_saver`

Notes:
DURABILITY. Geometry reaches the disk through `config_saver::ConfigSaver`, the shared debouncing
writer thread: the tracker only compares each frame's sample with the last one it handed over, so
once a sample is queued the saver is the last owner of it. A failed write therefore holds its
sample and retries it with a capped backoff, `flush_and_join` makes the final attempt, and a
sample that is lost anyway is logged as lost. `GeometrySnapshot::coalesce` folds field by field,
matching `persist_geometry`'s rule that a `None` field means "not measurable", never "forget it".


ORDERING PROBLEM. A second winit `EventLoop` cannot be created
(`winit-0.30.13/src/event_loop.rs:115-119`), so the monitor list is unknown until the window
exists. Startup therefore works off the STORED monitor rect (`Window.monitor` /
`Window.auto_monitor`), and the live list is only used at runtime — to refresh
`auto_monitor`, to feed the settings UI, and to move the window once when it did not open on
the requested monitor.

UNITS. Everything persisted here is in *logical pixels*: physical pixels divided by the
monitor's DPI scale factor, independent of the egui zoom factor (`General.ui_scale_percent`).
That is exactly what `ViewportBuilder::with_position` / `with_inner_size` consume, because
egui-winit turns them into `LogicalPosition`/`LogicalSize` multiplied by
`Context::zoom_factor()` (`egui-winit-0.35.0/src/lib.rs:2063-2087`) and the builder is
consumed while the context is still at zoom 1.0 — the `run_native` creator closure that
applies the UI scale runs only after the window has been created. At runtime the same numbers
are recovered from `ViewportInfo` (reported in points = physical / (zoom * scale)) by
multiplying by `Context::zoom_factor()`. `MonitorKey` is the one exception: it stores what
winit reports, i.e. PHYSICAL pixels, plus the scale needed to convert it.

WAYLAND. The compositor owns window placement there: `with_position` is ignored and
`ViewportInfo::outer_rect` is always `None` (`egui-0.35.0/src/data/input/viewport_info.rs:52-66`).
This module detects that (no outer rect / `Window::outer_position()` errors), persists no
geometry, never relocates, and says so in the settings UI instead of pretending to work.

Native-only: winit windows and OS monitors do not exist in the web build.
*/

use crate::config;
use crate::config_saver::{self, ConfigSaver, SaverLabels};
use crate::runtime_log;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use ms_thread as thread;
use winit::window::Window;

/// Top-level `user_config.json` key owned by this module.
///
/// A dedicated section, not a sub-object of the panel-layout section: the two are written by
/// different owners with different lifecycles (window geometry is sampled from the OS window,
/// the dock layout from user edits), and each carries its own `version`.
pub const WINDOW_SECTION_KEY: &str = "Window";

/// Schema version of the `Window` section. The config file has no global version, so the
/// section carries its own (precedent: `fonts_data.rs`).
pub const WINDOW_SECTION_VERSION: u32 = 1;

/// How often the live monitor list is re-enumerated. `available_monitors()` is a display-server
/// round trip, so it must not run per frame.
const MONITOR_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Smallest window size worth persisting, in logical pixels. Anything below is treated as a
/// transient/garbage sample and dropped.
const MIN_PERSISTED_SIZE: f32 = 200.0;

/// Coordinate magnitude beyond which a persisted geometry is considered corrupt.
const MAX_PERSISTED_COORD: f32 = 65_536.0;

/// Default DPI scale used when a stored monitor key predates the `scale` field.
fn default_monitor_scale() -> f64 {
    1.0
}

/// Typed failures of the `Window` config section writers.
#[derive(Debug, thiserror::Error)]
pub enum WindowGeometryError {
    /// The read-modify-write transaction on `user_config.json` failed. The payload is the
    /// full `anyhow` chain, already suitable for a log line.
    #[error("failed to update the '{WINDOW_SECTION_KEY}' section of user_config.json: {0}")]
    Persist(String),
}

/// A rectangle in zoom-independent logical pixels (see the file header's UNITS note).
///
/// Used both for the persisted window geometry and for a monitor's area converted out of
/// physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct WindowRect {
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub w: f32,
    #[serde(default)]
    pub h: f32,
}

impl WindowRect {
    /// Whether the rectangle is finite, big enough to be a real window and inside the
    /// coordinate range any real desktop can produce. Insane values are dropped on load
    /// rather than fed to the window system.
    #[must_use]
    pub fn is_sane(&self) -> bool {
        [self.x, self.y, self.w, self.h].iter().all(|v| v.is_finite())
            && self.w >= MIN_PERSISTED_SIZE
            && self.h >= MIN_PERSISTED_SIZE
            && self.x.abs() <= MAX_PERSISTED_COORD
            && self.y.abs() <= MAX_PERSISTED_COORD
            && self.w <= MAX_PERSISTED_COORD
            && self.h <= MAX_PERSISTED_COORD
    }

    /// Center point of the rectangle, as `(x, y)`.
    #[must_use]
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    /// Whether `point` lies inside the rectangle (min-inclusive, max-exclusive).
    #[must_use]
    pub fn contains(&self, point: (f32, f32)) -> bool {
        point.0 >= self.x
            && point.0 < self.x + self.w
            && point.1 >= self.y
            && point.1 < self.y + self.h
    }
}

/// Synthetic identity of one monitor.
///
/// Neither winit nor any OS offers a stable monitor id, so the identity is the tuple
/// (`name`, physical `x`/`y`/`w`/`h`) and it is re-resolved against the live monitor list on
/// every start (see [`resolve_monitor`]). `scale` is NOT part of the identity: it is the DPI
/// factor needed to convert this monitor's physical rect into the logical units the rest of
/// this module uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorKey {
    /// Human-readable connector/monitor name as reported by the OS (`"DP-1"`, `"HDMI-A-2"`).
    /// `None` when the platform does not expose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Left edge in physical pixels, in the virtual-desktop coordinate space.
    #[serde(default)]
    pub x: i32,
    /// Top edge in physical pixels, in the virtual-desktop coordinate space.
    #[serde(default)]
    pub y: i32,
    /// Width in physical pixels.
    #[serde(default)]
    pub w: u32,
    /// Height in physical pixels.
    #[serde(default)]
    pub h: u32,
    /// DPI scale factor of this monitor (physical / logical).
    #[serde(default = "default_monitor_scale")]
    pub scale: f64,
}

impl Default for MonitorKey {
    fn default() -> Self {
        Self {
            name: None,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            scale: default_monitor_scale(),
        }
    }
}

impl MonitorKey {
    /// Whether the key describes a usable monitor (non-empty area, sane scale).
    #[must_use]
    pub fn is_sane(&self) -> bool {
        self.w > 0 && self.h > 0 && self.scale.is_finite() && self.scale > 0.0
    }

    /// Screen area in physical pixels; the ordering key of "the largest monitor".
    #[must_use]
    pub fn area(&self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }

    /// Full identity match: same name AND same physical rect.
    #[must_use]
    pub fn same_identity(&self, other: &Self) -> bool {
        self.name == other.name && self.same_geometry(other)
    }

    /// Same physical rect, ignoring the name (a monitor renamed by a driver update).
    #[must_use]
    pub fn same_geometry(&self, other: &Self) -> bool {
        self.x == other.x && self.y == other.y && self.w == other.w && self.h == other.h
    }

    /// The monitor's area converted from physical pixels into the logical units used for
    /// window placement.
    #[must_use]
    pub fn logical_rect(&self) -> WindowRect {
        // `is_sane` guarantees a positive finite scale; a corrupt key that slipped through
        // would divide by a non-positive number, so fall back to 1:1 instead.
        let scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };
        let to_logical = |value: f64| -> f32 {
            let scaled = value / scale;
            // f64 -> f32 of a screen coordinate: the value range is far inside f32 precision,
            // and an infinite result is filtered by `WindowRect::is_sane` at the use sites.
            scaled as f32
        };
        WindowRect {
            x: to_logical(f64::from(self.x)),
            y: to_logical(f64::from(self.y)),
            w: to_logical(f64::from(self.w)),
            h: to_logical(f64::from(self.h)),
        }
    }
}

/// Serde mirror of the `Window` section of `user_config.json`.
///
/// Every field is optional so a partial, older or hand-edited section still yields its known
/// keys; unknown/unsane values are dropped by [`sanitize`](WindowSettings::sanitize).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowSettings {
    /// Schema version; see [`WINDOW_SECTION_VERSION`]. `None` means "written before the field
    /// existed" and is treated as version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The monitor the user explicitly chose in the settings. `None` = "auto".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorKey>,
    /// The largest monitor observed during the last run. Written by the program, not the user;
    /// it is what makes "start on the largest monitor" work before any window exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_monitor: Option<MonitorKey>,
    /// Last known geometry of the studio window while it was neither maximized, minimized nor
    /// fullscreen. Position is the OUTER top-left, size is the INNER size — the pair
    /// `with_position` / `with_inner_size` expect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<WindowRect>,
    /// Whether the studio window was left maximized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximized: Option<bool>,
}

impl WindowSettings {
    /// Drops values that cannot describe a real window or monitor, so a corrupt config can
    /// never be fed to the window system.
    fn sanitize(&mut self) {
        if self.main.is_some_and(|rect| !rect.is_sane()) {
            runtime_log::log_warn(
                "[window-geometry] ignoring an out-of-range stored window rect from user_config.json",
            );
            self.main = None;
        }
        if self.monitor.as_ref().is_some_and(|key| !key.is_sane()) {
            runtime_log::log_warn(
                "[window-geometry] ignoring a corrupt stored primary-monitor key from user_config.json",
            );
            self.monitor = None;
        }
        if self.auto_monitor.as_ref().is_some_and(|key| !key.is_sane()) {
            self.auto_monitor = None;
        }
    }

    /// Decodes the section from a `serde_json::Value`, falling back to defaults on any error.
    ///
    /// A malformed section must never block startup: it is logged and treated as absent, and
    /// the next write replaces it with a well-formed one.
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let mut settings = match serde_json::from_value::<Self>(value.clone()) {
            Ok(settings) => settings,
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[window-geometry] malformed '{WINDOW_SECTION_KEY}' section in user_config.json, \
                     using defaults; error={err}"
                ));
                Self::default()
            }
        };
        if settings.version.is_some_and(|v| v > WINDOW_SECTION_VERSION) {
            runtime_log::log_warn(format!(
                "[window-geometry] '{WINDOW_SECTION_KEY}' section version {:?} is newer than the \
                 supported {WINDOW_SECTION_VERSION}; known fields are still used",
                settings.version
            ));
        }
        settings.sanitize();
        settings
    }

    /// Encodes the section back to a `serde_json::Value`, always stamping the current version.
    ///
    /// A serialization failure is impossible for this plain data (no maps with non-string keys,
    /// no non-finite floats survive `sanitize`); if it ever happened, an empty object is
    /// returned so the caller writes a well-formed — if empty — section instead of panicking.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut stamped = self.clone();
        stamped.version = Some(WINDOW_SECTION_VERSION);
        serde_json::to_value(&stamped).unwrap_or_else(|err| {
            runtime_log::log_error(format!(
                "[window-geometry] failed to serialize the '{WINDOW_SECTION_KEY}' section: {err}"
            ));
            Value::Object(Map::new())
        })
    }

    /// Whether the user made an explicit primary-monitor choice.
    #[must_use]
    pub fn has_explicit_monitor(&self) -> bool {
        self.monitor.is_some()
    }

    /// The monitor the program should open on, as far as the config alone can tell: the
    /// explicit choice, else the largest monitor seen during the previous run.
    #[must_use]
    pub fn target_monitor(&self) -> Option<&MonitorKey> {
        self.monitor.as_ref().or(self.auto_monitor.as_ref())
    }
}

/// Reads the `Window` section out of an already loaded `user_config.json` snapshot.
#[must_use]
pub fn window_settings_from_user_settings(user_settings: &Value) -> WindowSettings {
    user_settings
        .get(WINDOW_SECTION_KEY)
        .map_or_else(WindowSettings::default, WindowSettings::from_value)
}

/// Reads the `Window` section straight from `user_config.json`.
///
/// Window creation must NOT use the startup `user_settings` snapshot: `run_main` loads it once
/// and reuses it for every window of the session (the same trap `General.ui_scale_percent`
/// documents), so a monitor chosen in the studio would not reach the next window until a
/// restart. This is one small read per window creation, never per frame.
///
/// An unreadable or malformed config degrades to the defaults with a logged reason: a window
/// that cannot be placed from the config still opens where the window manager puts it.
#[must_use]
pub fn load_window_settings() -> WindowSettings {
    match config::load_raw_user_settings_for_startup() {
        Ok(settings) => window_settings_from_user_settings(&settings),
        Err(err) => {
            runtime_log::log_error(format!(
                "[window-geometry] failed to read user_config.json for the startup window \
                 placement; the window manager decides this time; error={err:#}"
            ));
            WindowSettings::default()
        }
    }
}

// ---------------------------------------------------------------------------------------
// Monitor resolution (pure)
// ---------------------------------------------------------------------------------------

/// Why the largest monitor was used instead of the requested one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorFallback {
    /// No monitor was requested at all — "auto" means "the largest one".
    NoPreference,
    /// A monitor was requested but is not among the connected ones (unplugged, or the
    /// desktop was rearranged).
    PreferredMissing,
}

/// Outcome of matching a stored monitor key against the live monitor list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorResolution {
    /// The window system reported no monitors; nothing can be decided.
    NoMonitors,
    /// The requested monitor was found at this index.
    Preferred(usize),
    /// The largest monitor at this index is used, for the given reason.
    Fallback {
        index: usize,
        reason: MonitorFallback,
    },
}

/// Index of the largest monitor by physical area, or `None` for an empty list.
///
/// Ties resolve to the FIRST such monitor so the choice is deterministic across runs.
#[must_use]
pub fn largest_monitor_index(monitors: &[MonitorKey]) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for (index, monitor) in monitors.iter().enumerate() {
        let area = monitor.area();
        if best.is_none_or(|(_, best_area)| area > best_area) {
            best = Some((index, area));
        }
    }
    best.map(|(index, _)| index)
}

/// Resolves a stored monitor key to an index in the live monitor list.
///
/// Matching precedence: full identity (name + rect), then name alone (a monitor moved in the
/// desktop arrangement), then rect alone (a driver renamed it). Anything else degrades to the
/// largest monitor with a [`MonitorFallback`] reason the caller is expected to log.
#[must_use]
pub fn resolve_monitor(preferred: Option<&MonitorKey>, monitors: &[MonitorKey]) -> MonitorResolution {
    let Some(largest) = largest_monitor_index(monitors) else {
        return MonitorResolution::NoMonitors;
    };
    let Some(preferred) = preferred else {
        return MonitorResolution::Fallback {
            index: largest,
            reason: MonitorFallback::NoPreference,
        };
    };
    if let Some(index) = monitors
        .iter()
        .position(|monitor| monitor.same_identity(preferred))
    {
        return MonitorResolution::Preferred(index);
    }
    if let Some(name) = preferred.name.as_deref().filter(|n| !n.trim().is_empty())
        && let Some(index) = monitors
            .iter()
            .position(|monitor| monitor.name.as_deref() == Some(name))
    {
        return MonitorResolution::Preferred(index);
    }
    if let Some(index) = monitors
        .iter()
        .position(|monitor| monitor.same_geometry(preferred))
    {
        return MonitorResolution::Preferred(index);
    }
    MonitorResolution::Fallback {
        index: largest,
        reason: MonitorFallback::PreferredMissing,
    }
}

// ---------------------------------------------------------------------------------------
// Startup placement (pure)
// ---------------------------------------------------------------------------------------

/// Whether a window restores the stored size or keeps the one its own builder declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizePolicy {
    /// The studio window: it owns the persisted geometry and restores it verbatim.
    RestoreStored,
    /// Every other window (today: the launcher): it only follows the monitor choice and
    /// keeps its own size.
    KeepDefault,
}

/// Geometry to feed into a `ViewportBuilder` before the window exists.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StartupPlacement {
    /// Outer position in logical pixels, or `None` to let the window system decide.
    pub position: Option<[f32; 2]>,
    /// Inner size in logical pixels, or `None` to keep the builder's own size.
    pub size: Option<[f32; 2]>,
    /// Whether the window should open maximized.
    pub maximized: bool,
}

/// Computes the startup geometry from the stored section alone — no monitor enumeration,
/// because none is possible before the window exists (see the file header).
///
/// `default_size` is the window's own inner size in logical pixels, used when centering on a
/// monitor. Rules, in order:
/// 1. `RestoreStored` + a stored rect that lies on the target monitor → restore it verbatim.
/// 2. A known target monitor → center the window on it (this is what "start on monitor X",
///    and "start on the largest monitor" when nothing was chosen, actually does).
/// 3. Nothing known (first run) → leave placement to the window system.
#[must_use]
pub fn plan_startup_placement(
    settings: &WindowSettings,
    default_size: [f32; 2],
    size_policy: SizePolicy,
) -> StartupPlacement {
    let maximized = settings.maximized.unwrap_or(true);
    let target = settings.target_monitor().map(MonitorKey::logical_rect);
    let stored = settings.main.filter(WindowRect::is_sane);

    if size_policy == SizePolicy::RestoreStored
        && let Some(rect) = stored
        && target.is_none_or(|monitor| monitor.contains(rect.center()))
    {
        return StartupPlacement {
            position: Some([rect.x, rect.y]),
            size: Some([rect.w, rect.h]),
            maximized,
        };
    }

    // The stored size is still the user's size even when the stored POSITION is stale
    // (the window's monitor went away, or the user picked a different one).
    let size = match size_policy {
        SizePolicy::RestoreStored => stored.map_or(default_size, |rect| [rect.w, rect.h]),
        SizePolicy::KeepDefault => default_size,
    };
    let position = target.map(|monitor| {
        [
            monitor.x + ((monitor.w - size[0]) * 0.5).max(0.0),
            monitor.y + ((monitor.h - size[1]) * 0.5).max(0.0),
        ]
    });
    StartupPlacement {
        position,
        size: match size_policy {
            SizePolicy::RestoreStored => stored.map(|rect| [rect.w, rect.h]),
            SizePolicy::KeepDefault => None,
        },
        maximized,
    }
}

/// Applies a planned placement to a viewport builder.
///
/// [`StartupPlacement::maximized`] is deliberately NOT applied here: Windows needs the
/// first-frame `ViewportCommand::Maximized` workaround instead of the builder flag
/// (see `studio_bootstrap.rs`), so each call site decides how to honor it.
#[must_use]
pub fn apply_placement(
    mut builder: egui::ViewportBuilder,
    placement: &StartupPlacement,
) -> egui::ViewportBuilder {
    if let Some(position) = placement.position {
        builder = builder.with_position(position);
    }
    if let Some(size) = placement.size {
        builder = builder.with_inner_size(size);
    }
    builder
}

// ---------------------------------------------------------------------------------------
// Relocation decision (pure)
// ---------------------------------------------------------------------------------------

/// Inputs of the "should the window be moved to the target monitor?" decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelocationInput {
    /// Whether the platform lets us read and set the window position at all (false on Wayland).
    pub position_supported: bool,
    /// Whether the user picked the target monitor explicitly.
    pub preference_explicit: bool,
    /// Whether the config carried a stored window rect for this session's startup.
    pub has_stored_geometry: bool,
    /// Index of the monitor the window currently occupies, if known.
    pub current_monitor: Option<usize>,
    /// Index of the monitor the window should occupy.
    pub target_monitor: usize,
}

/// Whether the window must be moved onto the target monitor after it has opened.
///
/// This is the SAFETY NET behind [`plan_startup_placement`], which already aims every window at
/// the target monitor: it fires only when the window landed somewhere else anyway, i.e. when
/// the window manager ignored the position hint (or nothing could be planned because this is a
/// first run). It is deliberately narrow, because "the window is not where we asked" and "the
/// window manager knows better than us" are indistinguishable from here: with an explicit user
/// choice the user's intent outranks the WM, and with no stored geometry nothing can be lost by
/// moving. In the remaining case — auto choice plus a geometry the user left behind — the WM's
/// decision is left alone rather than fought every session.
#[must_use]
pub fn should_relocate(input: RelocationInput) -> bool {
    if !input.position_supported {
        return false;
    }
    let Some(current) = input.current_monitor else {
        // Unknown current monitor: moving blindly could take the window away from where the
        // user can see it. Do nothing and let the next start use the stored placement.
        return false;
    };
    if current == input.target_monitor {
        return false;
    }
    input.preference_explicit || !input.has_stored_geometry
}

// ---------------------------------------------------------------------------------------
// Process-wide monitor mirror
// ---------------------------------------------------------------------------------------

/// The monitors the live window can see, as last observed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonitorSnapshot {
    /// All connected monitors, in the order winit reported them.
    pub monitors: Vec<MonitorKey>,
    /// Index of the monitor the window currently occupies, if the platform reports one.
    pub current: Option<usize>,
    /// Whether the window position can be read and set on this platform/session.
    pub position_supported: bool,
}

/// Process-wide mirror of [`MonitorSnapshot`].
///
/// A mirrored slot rather than plumbing: the settings widget is shared by the launcher and the
/// studio and is drawn far from whoever owns the winit window (precedent:
/// `ai_backend_capabilities.rs`). Written only by [`refresh_monitors`], read by the UI.
fn monitor_slot() -> &'static RwLock<Option<MonitorSnapshot>> {
    static SLOT: OnceLock<RwLock<Option<MonitorSnapshot>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Pending "move the window to this monitor" request raised by the settings UI.
///
/// The UI has no winit window handle, and the move needs a multi-frame un-maximize / move /
/// re-maximize sequence, so the request is parked here and executed by
/// [`WindowGeometryTracker`] on its next frame. A window without a tracker (the launcher)
/// simply never consumes it; the choice still applies at the next start.
fn relocation_request_slot() -> &'static Mutex<Option<MonitorKey>> {
    static SLOT: OnceLock<Mutex<Option<MonitorKey>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// The last observed monitor list, or `None` when no window has published one yet.
#[must_use]
pub fn monitor_snapshot() -> Option<MonitorSnapshot> {
    monitor_slot()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Asks the studio window to move onto `monitor` as soon as it draws its next frame.
pub fn request_relocation(monitor: MonitorKey) {
    *relocation_request_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(monitor);
}

/// Takes a pending relocation request, if any.
fn take_relocation_request() -> Option<MonitorKey> {
    relocation_request_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

/// Builds a [`MonitorKey`] from a winit monitor handle.
fn monitor_key_from_handle(handle: &winit::monitor::MonitorHandle) -> MonitorKey {
    let position = handle.position();
    let size = handle.size();
    MonitorKey {
        name: handle.name(),
        x: position.x,
        y: position.y,
        w: size.width,
        h: size.height,
        scale: handle.scale_factor(),
    }
}

/// Collects the monitor list of a live window.
fn collect_monitors(window: &Window) -> MonitorSnapshot {
    let monitors: Vec<MonitorKey> = window
        .available_monitors()
        .map(|handle| monitor_key_from_handle(&handle))
        .collect();
    let current = window
        .current_monitor()
        .map(|handle| monitor_key_from_handle(&handle))
        .and_then(|current| {
            monitors
                .iter()
                .position(|monitor| monitor.same_identity(&current))
        });
    MonitorSnapshot {
        monitors,
        current,
        // On Wayland the compositor never tells a client where its window is; winit reports
        // that as an error rather than a guess.
        position_supported: window.outer_position().is_ok(),
    }
}

/// Re-reads the monitor list from a live window, publishes it for the settings UI and, when it
/// changed, reconciles `Window.auto_monitor` on a worker thread.
///
/// Cheap enough for a window-creation call and for the tracker's throttled refresh; NOT cheap
/// enough for every frame (`available_monitors` is a display-server round trip).
pub fn refresh_monitors(window: Option<&Window>) -> Option<MonitorSnapshot> {
    let snapshot = collect_monitors(window?);
    let changed = {
        let mut slot = monitor_slot()
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let changed = slot.as_ref() != Some(&snapshot);
        if changed {
            *slot = Some(snapshot.clone());
        }
        changed
    };
    if changed {
        spawn_monitor_reconcile(snapshot.clone());
    }
    Some(snapshot)
}

/// Logs how the stored monitor choice resolves against `monitors` and refreshes
/// `Window.auto_monitor` (the "largest monitor" the next startup will place on).
///
/// Runs on a worker thread: it reads and writes `user_config.json`.
fn spawn_monitor_reconcile(snapshot: MonitorSnapshot) {
    let spawn_result = thread::Builder::new()
        .name("window-monitor-reconcile".to_string())
        .spawn(move || reconcile_monitors(&snapshot));
    if let Err(err) = spawn_result {
        runtime_log::log_error(format!(
            "[window-geometry] failed to spawn the monitor reconcile thread; the primary-monitor \
             choice will not be refreshed this run: {err}"
        ));
    }
}

/// Worker body of [`spawn_monitor_reconcile`].
fn reconcile_monitors(snapshot: &MonitorSnapshot) {
    let settings = match config::load_raw_user_settings_for_startup() {
        Ok(value) => window_settings_from_user_settings(&value),
        Err(err) => {
            runtime_log::log_error(format!(
                "[window-geometry] failed to read user_config.json while reconciling monitors; \
                 the primary-monitor choice is left untouched; error={err:#}"
            ));
            return;
        }
    };
    match resolve_monitor(settings.monitor.as_ref(), &snapshot.monitors) {
        MonitorResolution::NoMonitors => {
            runtime_log::log_warn(
                "[window-geometry] the window system reported no monitors; startup placement \
                 stays with the window manager",
            );
        }
        MonitorResolution::Preferred(index) => {
            runtime_log::log_info(format!(
                "[window-geometry] primary monitor resolved to #{index} ({})",
                describe_monitor(snapshot.monitors.get(index))
            ));
        }
        MonitorResolution::Fallback { index, reason } => match reason {
            MonitorFallback::NoPreference => {
                runtime_log::log_info(format!(
                    "[window-geometry] no primary monitor chosen; using the largest one, #{index} ({})",
                    describe_monitor(snapshot.monitors.get(index))
                ));
            }
            MonitorFallback::PreferredMissing => {
                runtime_log::log_warn(format!(
                    "[window-geometry] the chosen primary monitor ({}) is not connected; falling \
                     back to the largest one, #{index} ({}). The choice is kept in user_config.json \
                     in case the monitor comes back.",
                    describe_monitor(settings.monitor.as_ref()),
                    describe_monitor(snapshot.monitors.get(index))
                ));
            }
        },
    }

    let largest = largest_monitor_index(&snapshot.monitors).and_then(|i| snapshot.monitors.get(i));
    if let Some(largest) = largest
        && settings.auto_monitor.as_ref() != Some(largest)
        && let Err(err) = persist_auto_monitor(largest.clone())
    {
        runtime_log::log_error(format!("[window-geometry] {err}"));
    }
}

/// One-line monitor description for logs (never localized: log text, see
/// `dev-docs/i18n_exclusions.md`).
fn describe_monitor(monitor: Option<&MonitorKey>) -> String {
    monitor.map_or_else(
        || "unknown".to_string(),
        |monitor| {
            format!(
                "name={:?} rect={}x{}+{}+{} scale={}",
                monitor.name, monitor.w, monitor.h, monitor.x, monitor.y, monitor.scale
            )
        },
    )
}

// ---------------------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------------------

/// Runs `mutator` over the decoded `Window` section and writes it back.
///
/// Goes through `config::update_user_config_file`, the single locked read-modify-write border
/// for `user_config.json`: a malformed file is an error, never a reason to overwrite the whole
/// root. Fields the mutator does not touch (notably the user's `monitor` choice) survive
/// verbatim, so the geometry writer and the settings UI can write concurrently.
fn update_window_section(
    path: &Path,
    mutator: impl FnOnce(&mut WindowSettings),
) -> Result<(), WindowGeometryError> {
    config::update_user_config_file(path, |root| {
        let section = root.get(WINDOW_SECTION_KEY).cloned().unwrap_or(Value::Null);
        let mut settings = WindowSettings::from_value(&section);
        mutator(&mut settings);
        if let Some(object) = root.as_object_mut() {
            object.insert(WINDOW_SECTION_KEY.to_owned(), settings.to_value());
        }
        Ok(())
    })
    .map_err(|err| WindowGeometryError::Persist(format!("{err:#}")))
}

/// Persists the user's explicit primary-monitor choice (`None` = auto / largest).
///
/// Synchronous and small; called from the settings widget, which persists every other setting
/// the same way.
pub fn persist_preferred_monitor(monitor: Option<MonitorKey>) -> Result<(), WindowGeometryError> {
    update_window_section(&config::user_config_path(), |settings| {
        settings.monitor = monitor;
    })
}

/// Persists the largest monitor observed this run.
fn persist_auto_monitor(monitor: MonitorKey) -> Result<(), WindowGeometryError> {
    update_window_section(&config::user_config_path(), |settings| {
        settings.auto_monitor = Some(monitor);
    })
}

/// One sampled state of the live window, as handed to the writer thread.
#[derive(Debug, Clone, PartialEq)]
pub struct GeometrySnapshot {
    /// Restored (non-maximized) geometry, or `None` when the window is maximized/minimized/
    /// fullscreen or the platform hides its position.
    pub rect: Option<WindowRect>,
    /// Maximized state, or `None` when the platform does not report it.
    pub maximized: Option<bool>,
}

impl GeometrySnapshot {
    /// Whether this snapshot carries anything worth writing.
    #[must_use]
    fn is_empty(&self) -> bool {
        self.rect.is_none() && self.maximized.is_none()
    }
}

impl config_saver::SaverPayload for GeometrySnapshot {
    /// Folds a newer sample over this one FIELD BY FIELD: a `None` field of `newer` means "not
    /// measurable in that sample", not "forget it", exactly as [`persist_geometry`] reads it.
    ///
    /// Replacing the whole snapshot instead would make the fold disagree with the write step —
    /// a rect sampled before the user maximized the window would be dropped by the maximize
    /// sample that follows it, even though writing the two in order keeps it. That matters most
    /// for a rect a FAILED write still owes the disk: it is the only copy left in the process.
    fn coalesce(&mut self, newer: Self) {
        if newer.rect.is_some() {
            self.rect = newer.rect;
        }
        if newer.maximized.is_some() {
            self.maximized = newer.maximized;
        }
    }
}

impl config_saver::SaverError for WindowGeometryError {
    /// Every failure of this section is transient by assumption — a locked, busy, momentarily
    /// unwritable or unreadable `user_config.json` is the common case, and a permanent one only
    /// costs the capped retries of one session.
    fn is_retryable(&self) -> bool {
        match self {
            Self::Persist(_) => true,
        }
    }
}

/// Persists one sampled geometry into the config file at `path`, leaving the fields it does not
/// own untouched.
fn persist_geometry(path: &Path, snapshot: &GeometrySnapshot) -> Result<(), WindowGeometryError> {
    update_window_section(path, |settings| {
        // A `None` rect means "not measurable right now" (maximized, or Wayland), NOT "forget
        // the restored geometry" — overwriting it would lose the size the user actually chose.
        if let Some(rect) = snapshot.rect {
            settings.main = Some(rect);
        }
        if let Some(maximized) = snapshot.maximized {
            settings.maximized = Some(maximized);
        }
    })
}

/// Log strings of the geometry writer thread.
const GEOMETRY_SAVER_LABELS: SaverLabels = SaverLabels {
    tag: "[window-geometry]",
    subject: "the window geometry",
    thread_name: "window-geometry-saver",
};

/// Spawns the writer thread that owns every geometry write.
///
/// Coalescing, retry and shutdown policy live in [`config_saver`]: a burst of samples is
/// debounced into ONE write (dragging a window costs one write per pause, not one per frame), a
/// failed write holds its sample and retries it with a capped backoff instead of dropping it,
/// and the shutdown makes the final attempt.
fn spawn_geometry_saver() -> ConfigSaver<GeometrySnapshot> {
    ConfigSaver::spawn(GEOMETRY_SAVER_LABELS, persist_geometry)
}

// ---------------------------------------------------------------------------------------
// Runtime tracker
// ---------------------------------------------------------------------------------------

/// Multi-frame state of a window relocation.
///
/// Un-maximize, move and re-maximize are three separate window-system requests; issuing them
/// inside one frame makes X11 window managers apply them out of order, so each step gets its
/// own frame.
#[derive(Debug, Clone, PartialEq)]
enum RelocationStage {
    /// Nothing to do (also the state after the startup decision said "stay put").
    Idle,
    /// The startup decision has not been taken yet; it needs the monitor list.
    PendingStartupDecision,
    /// `set_maximized(false)` was issued; move on the next frame.
    Unmaximized {
        target: MonitorKey,
        restore_maximized: bool,
    },
    /// The window was moved; restore the maximized state on the next frame.
    Moved { restore_maximized: bool },
}

/// Observes the live studio window and keeps `user_config.json` in sync with it.
///
/// Owned by the window's `eframe::App` (today `StudioBootstrapApp`), one per window. All disk
/// work happens on the writer thread; the per-frame path only reads already-computed
/// `ViewportInfo` values and compares them with the last sample.
#[derive(Debug)]
pub struct WindowGeometryTracker {
    /// The debouncing writer thread that owns every write of this section.
    saver: ConfigSaver<GeometrySnapshot>,
    /// Last sample handed to the writer; the change filter that keeps idle frames silent.
    last_sent: Option<GeometrySnapshot>,
    /// When the monitor list was last enumerated.
    last_monitor_refresh: Option<Instant>,
    /// The primary-monitor choice this session started with.
    preferred: Option<MonitorKey>,
    /// Whether that choice was the user's explicit one.
    preference_explicit: bool,
    /// Whether the config carried a window rect at startup.
    had_stored_geometry: bool,
    /// Current step of a relocation, if any.
    relocation: RelocationStage,
    /// Set once the "this session cannot report window positions" warning has been logged.
    warned_position_unsupported: bool,
}

impl WindowGeometryTracker {
    /// Creates a tracker seeded from the settings the window was opened with.
    #[must_use]
    pub fn new(settings: &WindowSettings) -> Self {
        Self {
            saver: spawn_geometry_saver(),
            last_sent: None,
            last_monitor_refresh: None,
            preferred: settings.target_monitor().cloned(),
            preference_explicit: settings.has_explicit_monitor(),
            had_stored_geometry: settings.main.is_some(),
            relocation: RelocationStage::PendingStartupDecision,
            warned_position_unsupported: false,
        }
    }

    /// Per-frame observation. Cheap: no syscalls except the throttled monitor enumeration.
    ///
    /// `window` is `Option` because eframe reports `None` for headless runs (tests); the
    /// tracker then degrades to geometry-only tracking with no monitor awareness.
    pub fn observe(&mut self, ctx: &egui::Context, window: Option<&Window>) {
        self.refresh_monitors_if_due(window);
        self.drive_relocation(ctx, window);
        self.sample_and_send(ctx);
    }

    /// Re-enumerates monitors at most every [`MONITOR_REFRESH_INTERVAL`].
    fn refresh_monitors_if_due(&mut self, window: Option<&Window>) {
        let now = Instant::now();
        let due = self
            .last_monitor_refresh
            .is_none_or(|last| now.duration_since(last) >= MONITOR_REFRESH_INTERVAL);
        if !due {
            return;
        }
        self.last_monitor_refresh = Some(now);
        refresh_monitors(window);
    }

    /// Advances the relocation state machine by one step per frame.
    fn drive_relocation(&mut self, ctx: &egui::Context, window: Option<&Window>) {
        let Some(window) = window else {
            return;
        };
        // A choice made in the settings this session outranks whatever the startup decision
        // concluded, and re-arms the machine.
        if let Some(requested) = take_relocation_request() {
            self.preferred = Some(requested);
            self.preference_explicit = true;
            self.relocation = RelocationStage::PendingStartupDecision;
        }
        match std::mem::replace(&mut self.relocation, RelocationStage::Idle) {
            RelocationStage::Idle => {}
            RelocationStage::PendingStartupDecision => self.decide_relocation(ctx, window),
            RelocationStage::Unmaximized {
                target,
                restore_maximized,
            } => {
                Self::move_to_monitor(window, &target);
                self.relocation = RelocationStage::Moved { restore_maximized };
                ctx.request_repaint();
            }
            RelocationStage::Moved { restore_maximized } => {
                if restore_maximized {
                    window.set_maximized(true);
                }
            }
        }
    }

    /// Decides once whether the window has to be moved onto the target monitor.
    fn decide_relocation(&mut self, ctx: &egui::Context, window: &Window) {
        let Some(snapshot) = monitor_snapshot() else {
            // No monitor list yet: retry on the next frame after a refresh.
            self.relocation = RelocationStage::PendingStartupDecision;
            return;
        };
        if !snapshot.position_supported {
            if !self.warned_position_unsupported {
                self.warned_position_unsupported = true;
                runtime_log::log_warn(
                    "[window-geometry] this session does not expose window positions (Wayland): \
                     the compositor decides which monitor the window opens on, and no geometry is \
                     persisted",
                );
            }
            return;
        }
        let resolution = resolve_monitor(self.preferred.as_ref(), &snapshot.monitors);
        let target_index = match resolution {
            MonitorResolution::NoMonitors => return,
            MonitorResolution::Preferred(index) | MonitorResolution::Fallback { index, .. } => index,
        };
        let Some(target) = snapshot.monitors.get(target_index) else {
            return;
        };
        let decision = RelocationInput {
            position_supported: snapshot.position_supported,
            preference_explicit: self.preference_explicit,
            has_stored_geometry: self.had_stored_geometry,
            current_monitor: snapshot.current,
            target_monitor: target_index,
        };
        if !should_relocate(decision) {
            return;
        }
        runtime_log::log_info(format!(
            "[window-geometry] moving the window onto monitor #{target_index} ({}); it opened on \
             {:?} instead",
            describe_monitor(Some(target)),
            snapshot.current
        ));
        let restore_maximized = window.is_maximized();
        if restore_maximized {
            window.set_maximized(false);
            self.relocation = RelocationStage::Unmaximized {
                target: target.clone(),
                restore_maximized,
            };
        } else {
            Self::move_to_monitor(window, target);
            self.relocation = RelocationStage::Moved {
                restore_maximized: false,
            };
        }
        ctx.request_repaint();
    }

    /// Centers the window on `target` using physical pixels (winit's own unit here, so no
    /// point/logical conversion can go wrong).
    fn move_to_monitor(window: &Window, target: &MonitorKey) {
        let outer = window.outer_size();
        // A window wider/taller than the monitor lands at the monitor's origin (saturating
        // subtraction), and the coordinate arithmetic saturates rather than wrapping into a
        // position no display covers.
        let offset_x = i32::try_from(target.w.saturating_sub(outer.width) / 2).unwrap_or(0);
        let offset_y = i32::try_from(target.h.saturating_sub(outer.height) / 2).unwrap_or(0);
        window.set_outer_position(winit::dpi::PhysicalPosition::new(
            target.x.saturating_add(offset_x),
            target.y.saturating_add(offset_y),
        ));
    }

    /// Samples the window state and forwards it to the writer thread when it changed.
    fn sample_and_send(&mut self, ctx: &egui::Context) {
        let snapshot = sample_geometry(ctx);
        if snapshot.is_empty() || self.last_sent.as_ref() == Some(&snapshot) {
            return;
        }
        self.last_sent = Some(snapshot.clone());
        self.saver.store(snapshot);
    }

    /// Writes the pending sample and joins the writer thread. Called from the app's `on_exit`,
    /// so the geometry of the last moments before closing is not lost inside the debounce — and
    /// so a sample an earlier write failed on gets its final attempt while the process still
    /// holds it. Idempotent.
    pub fn flush_and_join(&mut self) {
        self.saver.flush_and_join();
    }
}

/// Reads the current window geometry out of the frame's `ViewportInfo`.
///
/// Free of syscalls: egui-winit already computed these values for this frame. Position/size are
/// converted from egui points into the zoom-independent logical pixels this module persists
/// (see the file header's UNITS note). Maximized, minimized and fullscreen windows report no
/// rect: their geometry is not the one to restore.
fn sample_geometry(ctx: &egui::Context) -> GeometrySnapshot {
    let (outer_rect, inner_rect, maximized, minimized, fullscreen) = ctx.input(|input| {
        let viewport = input.viewport();
        (
            viewport.outer_rect,
            viewport.inner_rect,
            viewport.maximized,
            viewport.minimized,
            viewport.fullscreen,
        )
    });
    let zoom = ctx.zoom_factor();
    let hidden_state = maximized.unwrap_or(false)
        || minimized.unwrap_or(false)
        || fullscreen.unwrap_or(false);
    let rect = if hidden_state {
        None
    } else {
        outer_rect.zip(inner_rect).and_then(|(outer, inner)| {
            let rect = WindowRect {
                x: outer.min.x * zoom,
                y: outer.min.y * zoom,
                w: inner.width() * zoom,
                h: inner.height() * zoom,
            };
            rect.is_sane().then_some(rect)
        })
    };
    GeometrySnapshot { rect, maximized }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_saver::SaverPayload;
    use crate::config_saver::test_harness::{FAST_TIMING, LoopHarness, NO_TIMER_TIMING};

    fn monitor(name: &str, x: i32, y: i32, w: u32, h: u32) -> MonitorKey {
        MonitorKey {
            name: Some(name.to_owned()),
            x,
            y,
            w,
            h,
            scale: 1.0,
        }
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> WindowRect {
        WindowRect { x, y, w, h }
    }

    #[test]
    fn largest_monitor_prefers_the_biggest_area_and_the_first_on_a_tie() {
        let monitors = vec![
            monitor("HDMI-1", 0, 0, 1920, 1080),
            monitor("DP-1", 1920, 0, 3840, 2160),
            monitor("DP-2", 5760, 0, 3840, 2160),
        ];
        assert_eq!(largest_monitor_index(&monitors), Some(1));
        assert_eq!(largest_monitor_index(&[]), None);
    }

    #[test]
    fn resolve_monitor_matches_the_stored_key() {
        let monitors = vec![
            monitor("HDMI-1", 0, 0, 1920, 1080),
            monitor("DP-1", 1920, 0, 3840, 2160),
        ];
        let stored = monitor("HDMI-1", 0, 0, 1920, 1080);
        assert_eq!(
            resolve_monitor(Some(&stored), &monitors),
            MonitorResolution::Preferred(0)
        );
    }

    #[test]
    fn resolve_monitor_matches_by_name_when_the_arrangement_moved() {
        let monitors = vec![
            monitor("HDMI-1", 3840, 0, 1920, 1080),
            monitor("DP-1", 0, 0, 3840, 2160),
        ];
        let stored = monitor("HDMI-1", 0, 0, 1920, 1080);
        assert_eq!(
            resolve_monitor(Some(&stored), &monitors),
            MonitorResolution::Preferred(0)
        );
    }

    #[test]
    fn resolve_monitor_matches_by_geometry_when_the_name_changed() {
        let monitors = vec![monitor("DP-3", 0, 0, 1920, 1080)];
        let stored = monitor("HDMI-1", 0, 0, 1920, 1080);
        assert_eq!(
            resolve_monitor(Some(&stored), &monitors),
            MonitorResolution::Preferred(0)
        );
    }

    #[test]
    fn resolve_monitor_falls_back_to_the_largest_when_the_stored_one_is_gone() {
        let monitors = vec![
            monitor("HDMI-1", 0, 0, 1920, 1080),
            monitor("DP-1", 1920, 0, 3840, 2160),
        ];
        let stored = monitor("VGA-1", -1920, 0, 1280, 1024);
        assert_eq!(
            resolve_monitor(Some(&stored), &monitors),
            MonitorResolution::Fallback {
                index: 1,
                reason: MonitorFallback::PreferredMissing,
            }
        );
    }

    #[test]
    fn resolve_monitor_without_a_preference_picks_the_largest() {
        let monitors = vec![
            monitor("HDMI-1", 0, 0, 1920, 1080),
            monitor("DP-1", 1920, 0, 3840, 2160),
        ];
        assert_eq!(
            resolve_monitor(None, &monitors),
            MonitorResolution::Fallback {
                index: 1,
                reason: MonitorFallback::NoPreference,
            }
        );
    }

    #[test]
    fn resolve_monitor_reports_an_empty_monitor_list() {
        assert_eq!(resolve_monitor(None, &[]), MonitorResolution::NoMonitors);
        let stored = monitor("DP-1", 0, 0, 3840, 2160);
        assert_eq!(
            resolve_monitor(Some(&stored), &[]),
            MonitorResolution::NoMonitors
        );
    }

    #[test]
    fn monitor_logical_rect_divides_by_the_dpi_scale() {
        let mut key = monitor("DP-1", 3840, 0, 3840, 2160);
        key.scale = 2.0;
        assert_eq!(key.logical_rect(), rect(1920.0, 0.0, 1920.0, 1080.0));
    }

    #[test]
    fn startup_placement_restores_geometry_that_lies_on_the_target_monitor() {
        let settings = WindowSettings {
            monitor: Some(monitor("DP-1", 1920, 0, 3840, 2160)),
            main: Some(rect(2000.0, 100.0, 1400.0, 900.0)),
            maximized: Some(false),
            ..WindowSettings::default()
        };
        let placement = plan_startup_placement(&settings, [1400.0, 900.0], SizePolicy::RestoreStored);
        assert_eq!(placement.position, Some([2000.0, 100.0]));
        assert_eq!(placement.size, Some([1400.0, 900.0]));
        assert!(!placement.maximized);
    }

    #[test]
    fn startup_placement_recenters_geometry_left_on_another_monitor() {
        let settings = WindowSettings {
            monitor: Some(monitor("DP-1", 0, 0, 3840, 2160)),
            main: Some(rect(4000.0, 100.0, 1400.0, 900.0)),
            ..WindowSettings::default()
        };
        let placement = plan_startup_placement(&settings, [1400.0, 900.0], SizePolicy::RestoreStored);
        assert_eq!(placement.position, Some([(3840.0 - 1400.0) / 2.0, (2160.0 - 900.0) / 2.0]));
        assert_eq!(placement.size, Some([1400.0, 900.0]));
        // Nothing stored about the maximized state -> the historical default (maximized).
        assert!(placement.maximized);
    }

    #[test]
    fn startup_placement_uses_the_auto_monitor_when_nothing_was_chosen() {
        let settings = WindowSettings {
            auto_monitor: Some(monitor("DP-1", 1920, 0, 3840, 2160)),
            ..WindowSettings::default()
        };
        let placement = plan_startup_placement(&settings, [1400.0, 900.0], SizePolicy::RestoreStored);
        assert_eq!(
            placement.position,
            Some([1920.0 + (3840.0 - 1400.0) / 2.0, (2160.0 - 900.0) / 2.0])
        );
        assert_eq!(placement.size, None);
    }

    #[test]
    fn startup_placement_without_any_knowledge_leaves_placement_to_the_window_system() {
        let placement = plan_startup_placement(
            &WindowSettings::default(),
            [1400.0, 900.0],
            SizePolicy::RestoreStored,
        );
        assert_eq!(placement.position, None);
        assert_eq!(placement.size, None);
        assert!(placement.maximized);
    }

    #[test]
    fn startup_placement_keep_default_only_follows_the_monitor() {
        let settings = WindowSettings {
            monitor: Some(monitor("DP-1", 1920, 0, 3840, 2160)),
            main: Some(rect(2000.0, 100.0, 1400.0, 900.0)),
            ..WindowSettings::default()
        };
        let placement = plan_startup_placement(&settings, [1360.0, 860.0], SizePolicy::KeepDefault);
        assert_eq!(
            placement.position,
            Some([1920.0 + (3840.0 - 1360.0) / 2.0, (2160.0 - 860.0) / 2.0])
        );
        assert_eq!(placement.size, None);
    }

    #[test]
    fn startup_placement_centers_a_window_larger_than_the_monitor_at_its_origin() {
        let settings = WindowSettings {
            monitor: Some(monitor("VGA-1", 100, 50, 1024, 768)),
            main: Some(rect(4000.0, 4000.0, 2000.0, 1500.0)),
            ..WindowSettings::default()
        };
        let placement = plan_startup_placement(&settings, [1400.0, 900.0], SizePolicy::RestoreStored);
        assert_eq!(placement.position, Some([100.0, 50.0]));
    }

    #[test]
    fn relocation_is_skipped_when_positions_are_unsupported() {
        assert!(!should_relocate(RelocationInput {
            position_supported: false,
            preference_explicit: true,
            has_stored_geometry: false,
            current_monitor: Some(0),
            target_monitor: 1,
        }));
    }

    #[test]
    fn relocation_respects_an_explicit_choice_but_not_a_dragged_window() {
        let base = RelocationInput {
            position_supported: true,
            preference_explicit: true,
            has_stored_geometry: true,
            current_monitor: Some(0),
            target_monitor: 1,
        };
        assert!(should_relocate(base));
        assert!(!should_relocate(RelocationInput {
            preference_explicit: false,
            ..base
        }));
        // First run: no geometry the user could have chosen, so "largest monitor" applies.
        assert!(should_relocate(RelocationInput {
            preference_explicit: false,
            has_stored_geometry: false,
            ..base
        }));
        // Already there.
        assert!(!should_relocate(RelocationInput {
            current_monitor: Some(1),
            ..base
        }));
        // Unknown current monitor: never move blindly.
        assert!(!should_relocate(RelocationInput {
            current_monitor: None,
            ..base
        }));
    }

    #[test]
    fn settings_round_trip_through_json() {
        let settings = WindowSettings {
            version: Some(WINDOW_SECTION_VERSION),
            monitor: Some(monitor("DP-1", 1920, 0, 3840, 2160)),
            auto_monitor: Some(monitor("DP-1", 1920, 0, 3840, 2160)),
            main: Some(rect(2000.0, 100.0, 1400.0, 900.0)),
            maximized: Some(false),
        };
        let decoded = WindowSettings::from_value(&settings.to_value());
        assert_eq!(decoded, settings);
    }

    #[test]
    fn missing_and_broken_fields_degrade_to_defaults() {
        // Absent section.
        assert_eq!(
            window_settings_from_user_settings(&serde_json::json!({})),
            WindowSettings::default()
        );
        // Wrong types everywhere -> the whole section is unusable, but startup survives.
        let broken = serde_json::json!({"Window": {"monitor": "DP-1", "main": 42}});
        assert_eq!(
            window_settings_from_user_settings(&broken),
            WindowSettings::default()
        );
        // Partial objects keep what they can.
        let partial = serde_json::json!({"Window": {"maximized": false, "monitor": {"name": "DP-1", "w": 1920, "h": 1080}}});
        let decoded = window_settings_from_user_settings(&partial);
        assert_eq!(decoded.maximized, Some(false));
        assert_eq!(decoded.monitor.map(|m| (m.w, m.h, m.scale)), Some((1920, 1080, 1.0)));
        assert_eq!(decoded.main, None);
    }

    #[test]
    fn out_of_range_geometry_is_dropped_on_load() {
        let insane = serde_json::json!({
            "Window": {
                "main": {"x": 0.0, "y": 0.0, "w": 12.0, "h": 8.0},
                "monitor": {"name": "DP-1", "x": 0, "y": 0, "w": 0, "h": 0}
            }
        });
        let decoded = window_settings_from_user_settings(&insane);
        assert_eq!(decoded.main, None);
        assert_eq!(decoded.monitor, None);
    }

    #[test]
    fn a_newer_section_version_still_yields_its_known_fields() {
        let future = serde_json::json!({
            "Window": {"version": WINDOW_SECTION_VERSION + 1, "maximized": false}
        });
        assert_eq!(
            window_settings_from_user_settings(&future).maximized,
            Some(false)
        );
    }

    #[test]
    fn the_shipped_defaults_decode_to_the_documented_empty_state() {
        let defaults = config::user_config_defaults();
        let settings = window_settings_from_user_settings(&defaults);
        // The literal version in `config::user_config_defaults` must not drift from this
        // module's constant.
        assert_eq!(settings.version, Some(WINDOW_SECTION_VERSION));
        assert_eq!(settings.monitor, None);
        assert_eq!(settings.auto_monitor, None);
        assert_eq!(settings.main, None);
        // Historical behavior: both windows opened maximized before this section existed.
        assert_eq!(settings.maximized, Some(true));
    }

    /// A geometry sample carrying both fields.
    fn sample(w: f32, maximized: bool) -> GeometrySnapshot {
        GeometrySnapshot {
            rect: Some(rect(0.0, 0.0, w, 600.0)),
            maximized: Some(maximized),
        }
    }

    /// The transient failure the retry queue exists for.
    fn transient_failure() -> WindowGeometryError {
        WindowGeometryError::Persist("permission denied".to_owned())
    }

    #[test]
    fn coalescing_keeps_the_last_sample_of_a_burst_field_by_field() {
        let mut pending = sample(800.0, false);
        pending.coalesce(sample(900.0, false));
        pending.coalesce(sample(1000.0, false));
        assert_eq!(pending.rect, Some(rect(0.0, 0.0, 1000.0, 600.0)));
        // A maximize sample carries no rect: the restored geometry it says nothing about must
        // survive the fold, exactly as it survives `persist_geometry`.
        pending.coalesce(GeometrySnapshot {
            rect: None,
            maximized: Some(true),
        });
        assert_eq!(pending.rect, Some(rect(0.0, 0.0, 1000.0, 600.0)));
        assert_eq!(pending.maximized, Some(true));
    }

    #[test]
    fn a_failed_write_is_retried_until_it_succeeds() {
        // The regression: the tracker never re-sends a sample it has already handed over (it
        // only compares against it), so a sample dropped on a transient failure is gone from
        // the whole process — and a user who stops moving the window loses the session's
        // geometry for good.
        let harness = LoopHarness::start(FAST_TIMING, |attempt: u32| {
            if attempt <= 2 {
                Err(transient_failure())
            } else {
                Ok(())
            }
        });
        let snapshot = sample(1400.0, false);
        harness.store(snapshot.clone());

        // Two failures and the success that follows them, with no further sample from the GUI
        // thread — the user stopped moving the window.
        harness.await_attempt();
        harness.await_attempt();
        harness.await_attempt();

        harness.shutdown();
        assert_eq!(
            harness.join_and_take_attempts(),
            vec![snapshot.clone(), snapshot.clone(), snapshot]
        );
    }

    #[test]
    fn a_shutdown_makes_the_final_attempt_at_a_held_sample() {
        // `on_exit` is the last moment the process still holds the sample.
        let harness = LoopHarness::start(NO_TIMER_TIMING, |attempt: u32| {
            if attempt <= 1 {
                Err(transient_failure())
            } else {
                Ok(())
            }
        });
        let snapshot = sample(1400.0, false);
        harness.store(snapshot.clone());
        harness.await_attempt();

        harness.shutdown();
        harness.await_attempt();
        assert_eq!(
            harness.join_and_take_attempts(),
            vec![snapshot.clone(), snapshot]
        );
    }

    #[test]
    fn dropping_the_tracker_still_makes_the_final_attempt() {
        // A tracker dropped without `flush_and_join` disconnects the channel; the held sample
        // must not go with it.
        let mut harness = LoopHarness::start(NO_TIMER_TIMING, |attempt: u32| {
            if attempt <= 1 {
                Err(transient_failure())
            } else {
                Ok(())
            }
        });
        harness.store(sample(1400.0, false));
        harness.await_attempt();

        harness.disconnect();
        harness.await_attempt();
        assert_eq!(harness.join_and_take_attempts().len(), 2);
    }

    #[test]
    fn a_newer_sample_is_folded_over_the_held_one() {
        // The window was moved (rect sample), the write failed, then the user maximized it —
        // the maximize sample carries no rect, and the rect the failed write still owes must
        // ride along with it instead of being replaced by "unknown".
        let harness = LoopHarness::start(NO_TIMER_TIMING, |attempt: u32| {
            if attempt <= 1 {
                Err(transient_failure())
            } else {
                Ok(())
            }
        });
        harness.store(sample(1400.0, false));
        harness.await_attempt();

        harness.store(GeometrySnapshot {
            rect: None,
            maximized: Some(true),
        });
        harness.await_attempt();
        harness.shutdown();

        assert_eq!(
            harness.join_and_take_attempts(),
            vec![
                sample(1400.0, false),
                GeometrySnapshot {
                    rect: Some(rect(0.0, 0.0, 1400.0, 600.0)),
                    maximized: Some(true),
                },
            ]
        );
    }

    #[test]
    fn an_empty_sample_is_never_worth_a_write() {
        assert!(
            GeometrySnapshot {
                rect: None,
                maximized: None,
            }
            .is_empty()
        );
        assert!(
            !GeometrySnapshot {
                rect: None,
                maximized: Some(true),
            }
            .is_empty()
        );
    }
}
