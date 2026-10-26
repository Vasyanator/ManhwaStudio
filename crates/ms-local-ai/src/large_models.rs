/*
FILE OVERVIEW: crates/ms-local-ai/src/large_models.rs
The ONE Rust client of the backend's large-model pool (`large_models.*`, protocol v13,
`modules/ai_backend/ipc/PROTOCOL.md` §5.10): the wire types, their strict parsers, the five
calls, the step vocabulary with its localized labels and hints, and the two latest-wins
helpers the UI drives (`ProfilesQuery`, `LimitsWriter`).

Main types:
- `VramStep` / `RamStep` (the two step axes, behind the `LargeModelStep` trait), `StepChoice`.
- `ProfilesRequest` -> `LargeModelProfiles` (`SelectedPlan`, `Forecast`, `AxisRows`,
  `StepRow`, `LearnedFloor`, `ProbedLimits`, `FormatRow`, `Suggestion`).
- `Limits`, `CapChange`, `LargePoolStatus` / `LargeModelEntry` (the pushed health part).
- `RefusalReason`, `FailureKind`, `Resource`, `SuggestionKind`: backend machine codes mapped
  onto typed enums, because `ms-i18n` resolves only `&'static` keys.
- `LargeModelsError` (typed; `user_message()` is the localized text).
- `ProfilesQuery`: at most one `profiles` request in flight, latest wins, GUI polls only.
- `LimitsWriter`: debounced, axis-scoped latest-wins `limits.set` (an untouched axis travels as
  `Keep`; a pending edit is delivered on a worker when the writer is dropped).

Main functions:
- `parse_profiles`, `parse_pool_status`, `parse_limits` (pure, strict: an unknown step id
  or a missing required field is `Malformed`, never a silent zero).
- `fetch_profiles`, `set_limits`, `unload`, `reset_learned` (blocking IPC: WORKER THREADS
  ONLY), `limits_set_header`, `profiles_request_header`.
- `format_gib`, `format_duration`: the byte / seconds formatters every large-model surface
  shares.

Notes:
- Step SEMANTICS are the backend's (`runtime/large_models/tiers.py`); this file mirrors only
  the wire ids and their presentation.
- GUI-free: no egui. Every call that touches the backend blocks and must run on a worker;
  `ProfilesQuery::poll` / `LimitsWriter::poll` only `try_recv`.
- The caps are persisted by the BACKEND (`large_models.limits.set` -> `device_service.py`);
  this side never writes `General.large_models_*_cap_bytes`.
*/

use ms_backend_ipc::{self as backend_ipc, CallError};
use serde_json::{Map, Value, json};
use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use web_time::{Duration, Instant};

/// Timeout of every `large_models.*` call. `profiles` reads device memory through Torch on
/// a GPU device; the others are bookkeeping.
const LARGE_MODELS_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Quiet time after the last cap edit before [`LimitsWriter`] sends it.
pub const LIMITS_EDIT_DEBOUNCE: Duration = Duration::from_millis(400);

/// Bytes in one GiB (2^30).
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

// ---------------------------------------------------------------------------------------
// Step vocabulary
// ---------------------------------------------------------------------------------------

/// Which of the two step axes a [`LargeModelStep`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// «Видеопамять».
    Vram,
    /// «Оперативная память».
    Ram,
}

/// The shared surface of [`VramStep`] and [`RamStep`]: wire id, presentation and axis.
///
/// It exists so the row/axis types and the parsers are written once for both axes.
pub trait LargeModelStep: Copy + Eq + fmt::Debug + Send + 'static {
    /// The axis this step lives on.
    const AXIS: Axis;
    /// The concrete steps of the axis, fastest first (the order the pickers list them in).
    const CONCRETE: &'static [Self];
    /// The request-only «Авто» value of this axis.
    const AUTO: Self;
    /// The wire id (`tiers.py` `VramStep` / `RamStep` value).
    fn wire(self) -> &'static str;
    /// Parses a wire id; `None` for an id this build does not know.
    fn from_wire(value: &str) -> Option<Self>;
    /// The localized short label.
    fn label(self) -> &'static str;
    /// The localized hover explaining what the step does.
    fn hint(self) -> &'static str;
}

/// «Видеопамять»: how much of the card the model may occupy, fastest first; `Auto` = the
/// first that fits (a request value only — a plan never holds it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VramStep {
    #[default]
    Auto,
    Max,
    Phase,
    Half,
    Min,
}

impl LargeModelStep for VramStep {
    const AXIS: Axis = Axis::Vram;
    const CONCRETE: &'static [Self] = &[Self::Max, Self::Phase, Self::Half, Self::Min];
    const AUTO: Self = Self::Auto;

    fn wire(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Max => "max",
            Self::Phase => "phase",
            Self::Half => "half",
            Self::Min => "min",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "max" => Some(Self::Max),
            "phase" => Some(Self::Phase),
            "half" => Some(Self::Half),
            "min" => Some(Self::Min),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => t!("large_models.step.auto_label"),
            Self::Max => t!("large_models.vram.max_label"),
            Self::Phase => t!("large_models.vram.phase_label"),
            Self::Half => t!("large_models.vram.half_label"),
            Self::Min => t!("large_models.vram.min_label"),
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Auto => t!("large_models.step.auto_hint"),
            Self::Max => t!("large_models.vram.max_hint"),
            Self::Phase => t!("large_models.vram.phase_hint"),
            Self::Half => t!("large_models.vram.half_hint"),
            Self::Min => t!("large_models.vram.min_hint"),
        }
    }
}

/// «Оперативная память»: where the parts not on the card live, fastest first; `Auto` = the
/// first that fits (a request value only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RamStep {
    #[default]
    Auto,
    Keep,
    Run,
    Min,
}

impl LargeModelStep for RamStep {
    const AXIS: Axis = Axis::Ram;
    const CONCRETE: &'static [Self] = &[Self::Keep, Self::Run, Self::Min];
    const AUTO: Self = Self::Auto;

    fn wire(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Keep => "keep",
            Self::Run => "run",
            Self::Min => "min",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "keep" => Some(Self::Keep),
            "run" => Some(Self::Run),
            "min" => Some(Self::Min),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => t!("large_models.step.auto_label"),
            Self::Keep => t!("large_models.ram.keep_label"),
            Self::Run => t!("large_models.ram.run_label"),
            Self::Min => t!("large_models.ram.min_label"),
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Auto => t!("large_models.step.auto_hint"),
            Self::Keep => t!("large_models.ram.keep_hint"),
            Self::Run => t!("large_models.ram.run_hint"),
            Self::Min => t!("large_models.ram.min_hint"),
        }
    }
}

/// A position on both axes, as the user chose it (either may be `Auto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct StepChoice {
    pub vram: VramStep,
    pub ram: RamStep,
}

// ---------------------------------------------------------------------------------------
// Machine codes
// ---------------------------------------------------------------------------------------

/// Why a plan was refused (`planner.py` `REASON_*`). An unknown code is kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalReason {
    BudgetUnknown,
    DeviceShort,
    HostShort,
    StepsUnsupported,
    FormatUnavailable,
    Other(String),
}

impl RefusalReason {
    /// Maps a backend code; never fails (an unknown one becomes `Other`).
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "budget_unknown" => Self::BudgetUnknown,
            "device_short" => Self::DeviceShort,
            "host_short" => Self::HostShort,
            "steps_unsupported" => Self::StepsUnsupported,
            "format_unavailable" => Self::FormatUnavailable,
            other => Self::Other(other.to_string()),
        }
    }

    /// The localized sentence the panel shows for this refusal.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::BudgetUnknown => t!("large_models.refusal.budget_unknown_error").to_string(),
            Self::DeviceShort => t!("large_models.refusal.device_short_error").to_string(),
            Self::HostShort => t!("large_models.refusal.host_short_error").to_string(),
            Self::StepsUnsupported => t!("large_models.refusal.steps_unsupported_error").to_string(),
            Self::FormatUnavailable => t!("large_models.refusal.format_unavailable_error").to_string(),
            Self::Other(code) => tf!("large_models.refusal.other_error", reason = code),
        }
    }
}

/// A memory failure the backend learned from (`degrade.py` `FailureKind`): the reason of a
/// learned floor and of a degraded run. An unknown code is kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    DeviceOom,
    DeviceShort,
    HostShort,
    SysmemSpill,
    Other(String),
}

impl FailureKind {
    /// Maps a backend code; never fails (an unknown one becomes `Other`).
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "device_oom" => Self::DeviceOom,
            "device_short" => Self::DeviceShort,
            "host_short" => Self::HostShort,
            "sysmem_spill" => Self::SysmemSpill,
            other => Self::Other(other.to_string()),
        }
    }

    /// The localized short phrase for this failure.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::DeviceOom => t!("large_models.failure.device_oom_label").to_string(),
            Self::DeviceShort => t!("large_models.failure.device_short_label").to_string(),
            Self::HostShort => t!("large_models.failure.host_short_label").to_string(),
            Self::SysmemSpill => t!("large_models.failure.sysmem_spill_label").to_string(),
            Self::Other(code) => tf!("large_models.failure.other_label", reason = code),
        }
    }
}

/// The resource a refused plan ran shortest of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    Device,
    Host,
    Other(String),
}

impl Resource {
    fn from_wire(value: &str) -> Self {
        match value {
            "device" => Self::Device,
            "host" => Self::Host,
            other => Self::Other(other.to_string()),
        }
    }
}

/// What a backend suggestion proposes (`planner.py` `Suggestion.kind`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionKind {
    /// Another weight format fits (`"format"`).
    Format,
    /// The pair fits without the user's caps (`"limits"`).
    RaiseLimits,
    /// The named step pair fits (`"steps"`).
    Steps,
    /// A kind this build does not know; shown nowhere actionable.
    Other(String),
}

impl SuggestionKind {
    fn from_wire(value: &str) -> Self {
        match value {
            "format" => Self::Format,
            "limits" => Self::RaiseLimits,
            "steps" => Self::Steps,
            other => Self::Other(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// A failed `large_models.*` call or an answer this build cannot read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LargeModelsError {
    /// The backend is not reachable (no client, or a transport failure).
    Offline,
    /// The backend ran the method and refused it; the backend's own message.
    Call(String),
    /// The answer does not have the documented shape; names the field.
    Malformed(String),
    /// The worker running the call ended without an answer (it panicked).
    WorkerLost,
}

impl LargeModelsError {
    /// The localized text a panel shows for this error.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Offline => t!("large_models.error.offline_error").to_string(),
            Self::Call(message) => tf!("large_models.error.call_error", err = message),
            Self::Malformed(message) => tf!("large_models.error.malformed_error", err = message),
            Self::WorkerLost => t!("large_models.error.worker_lost_error").to_string(),
        }
    }

    fn from_call(err: CallError) -> Self {
        match err {
            CallError::Transport(_) => Self::Offline,
            CallError::Error(message) | CallError::Interrupted(message) => Self::Call(message),
        }
    }

    fn malformed(path: &str, expected: &str) -> Self {
        Self::Malformed(format!("{path}: expected {expected}"))
    }
}

impl fmt::Display for LargeModelsError {
    /// Technical (log) form; the UI uses [`LargeModelsError::user_message`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline => write!(f, "the AI backend is offline"),
            Self::Call(message) => write!(f, "backend error: {message}"),
            Self::Malformed(message) => write!(f, "malformed answer: {message}"),
            Self::WorkerLost => write!(f, "the worker ended without an answer"),
        }
    }
}

impl std::error::Error for LargeModelsError {}

// ---------------------------------------------------------------------------------------
// Answer shapes
// ---------------------------------------------------------------------------------------

/// Bytes other idle models must give up before a plan can be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimBytes {
    pub device_bytes: u64,
    pub host_bytes: u64,
}

/// One forecast (`manager.py` `_forecast_dict`). `host_peak` is the ABSOLUTE peak of the
/// backend process (baseline RSS included, `forecast.py`), not its growth.
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    pub device_peak: u64,
    pub host_peak: u64,
    pub peak_phase: String,
    /// First run from an empty process, at the cold disk rate.
    pub load_cold_s: f64,
    /// Reads of a repeated run, at the page-cache rate.
    pub load_warm_s: f64,
    pub per_step_s: f64,
    /// First run end to end (cold load + phases + moves).
    pub total_s: f64,
    pub calibrated: bool,
    pub device_calibrated: bool,
    /// The baseline RSS `host_peak` includes (the backend process without this model's
    /// weights; `service.py` `_reservation_of` reads the same figure back from the idle
    /// phase). Optional and additive on the wire (`host_baseline`); `None` from a backend
    /// that does not report it. Needed to ADD two forecasts' host peaks without counting
    /// the process twice.
    pub host_baseline: Option<u64>,
}

impl Forecast {
    /// Seconds of a REPEATED run: the first-run total with the cold load swapped for the
    /// warm one. Never negative.
    #[must_use]
    pub fn warm_total_s(&self) -> f64 {
        (self.total_s - self.load_cold_s + self.load_warm_s).max(0.0)
    }
}

/// What the requested pair resolves to, exactly as an admission would plan it.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedPlan {
    pub requested: StepChoice,
    /// The concrete VRAM step planned; `None` when refused.
    pub vram: Option<VramStep>,
    /// The concrete RAM step planned; `None` when refused.
    pub ram: Option<RamStep>,
    pub fits: bool,
    pub needs_reclaim: Option<ReclaimBytes>,
    pub reason: Option<RefusalReason>,
    pub shortest_resource: Option<Resource>,
    /// The plan's forecast, or the closest attempt's for a refusal.
    pub forecast: Option<Forecast>,
}

/// One picker row: a concrete step planned with the other axis' selected step.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRow<S> {
    pub id: S,
    pub fits: bool,
    /// The higher step whose plan this one repeats on this card.
    pub same_as: Option<S>,
    /// Faster than the learned floor.
    pub below_floor: bool,
    /// `device_peak` on the VRAM axis, `host_peak` on the RAM axis.
    pub peak: Option<u64>,
    pub total_s: Option<f64>,
}

/// One axis of a `profiles` answer.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisRows<S> {
    /// `false` on a host run device for the VRAM axis: no rows, no recommendation.
    pub applicable: bool,
    pub steps: Vec<StepRow<S>>,
    /// What `Auto` on this axis resolves to with the other axis fixed.
    pub recommended: Option<S>,
    pub floor: Option<S>,
}

/// The learned step floor of one model / format / device class.
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedFloor {
    /// The registration the floor belongs to (what `learned.reset` takes).
    pub model: String,
    pub format: Option<String>,
    pub vram: Option<VramStep>,
    pub ram: Option<RamStep>,
    pub reason: FailureKind,
}

/// The pool's hard caps (bytes); `None` = no cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    pub vram_cap: Option<u64>,
    pub ram_cap: Option<u64>,
}

/// One field of a `limits.set` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapChange {
    /// Field absent: the cap in force is kept.
    Keep,
    /// `null`: the cap is removed.
    Clear,
    /// The new cap in bytes.
    Set(u64),
}

impl CapChange {
    /// `Set` for a cap, `Clear` for "no cap".
    #[must_use]
    pub fn from_cap(cap: Option<u64>) -> Self {
        cap.map_or(Self::Clear, Self::Set)
    }
}

/// The caps in force plus the budgets the backend probed for a `profiles` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProbedLimits {
    pub caps: Limits,
    pub vram_total: Option<u64>,
    pub vram_free: Option<u64>,
    pub vram_reserve: Option<u64>,
    pub ram_total: Option<u64>,
    pub ram_free: Option<u64>,
    pub ram_cgroup_limit: Option<u64>,
    pub ram_reserve: Option<u64>,
}

/// One weight format of the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatRow {
    pub id: String,
    pub available: bool,
    pub reason: Option<String>,
    pub dit_bytes: Option<u64>,
    pub compute_path: Option<String>,
    pub quality_rank: Option<i64>,
    /// What `Auto` on both axes resolves to in this format; `None` = nothing fits.
    pub fits_best: Option<StepChoice>,
}

/// A user action that would make the request fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub kind: SuggestionKind,
    /// The pair that fits.
    pub choice: Option<StepChoice>,
    /// The format to switch to (`Format` only).
    pub format: Option<String>,
}

/// The `large_models.profiles` answer (`ProfilesReport.to_dict()` plus FLUX.2's additive
/// `encoder` and `encoder_formats`).
#[derive(Debug, Clone, PartialEq)]
pub struct LargeModelProfiles {
    /// The registration planned (e.g. `flux2_klein.float16` for a float16 request).
    pub model: String,
    pub format: Option<String>,
    pub selected: SelectedPlan,
    pub vram: AxisRows<VramStep>,
    pub ram: AxisRows<RamStep>,
    pub learned_floor: Option<LearnedFloor>,
    pub limits: ProbedLimits,
    pub formats: Vec<FormatRow>,
    pub suggestions: Vec<Suggestion>,
    /// The text encoder's own plan (FLUX.2); `None` when it will not encode or the model has
    /// no encoder.
    pub encoder: Option<SelectedPlan>,
    /// The text encoder's weight-format rows (FLUX.2, same row shape as `formats`), best
    /// first; empty when the backend does not report them (older backends, FLUX.3).
    pub encoder_formats: Vec<FormatRow>,
}

/// One loaded large-model handle (health `model_manager.large_models[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeModelEntry {
    pub model: String,
    pub key: String,
    pub format: Option<String>,
    pub device: Option<String>,
    /// Effective steps of the last finished run.
    pub vram_step: Option<VramStep>,
    pub ram_step: Option<RamStep>,
    pub device_bytes: u64,
    pub host_bytes: u64,
    pub in_use: bool,
    pub degraded: bool,
    pub degrade_reason: Option<FailureKind>,
}

/// The pool's state as the pushed health snapshot (or `large_models.status`) reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LargePoolStatus {
    pub entries: Vec<LargeModelEntry>,
    pub limits: Limits,
}

// ---------------------------------------------------------------------------------------
// Strict field readers
// ---------------------------------------------------------------------------------------

/// A JSON object with the path it was reached by, for `Malformed` messages.
struct Obj<'a> {
    map: &'a Map<String, Value>,
    path: String,
}

impl<'a> Obj<'a> {
    fn of(value: &'a Value, path: &str) -> Result<Self, LargeModelsError> {
        value
            .as_object()
            .map(|map| Self { map, path: path.to_string() })
            .ok_or_else(|| LargeModelsError::malformed(path, "an object"))
    }

    fn field(&self, key: &str) -> String {
        if self.path.is_empty() { key.to_string() } else { format!("{}.{key}", self.path) }
    }

    /// The value under `key`; absent and `null` both read as `None`.
    fn opt(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|value| !value.is_null())
    }

    fn req(&self, key: &str) -> Result<&'a Value, LargeModelsError> {
        self.opt(key).ok_or_else(|| LargeModelsError::malformed(&self.field(key), "a value"))
    }

    fn child(&self, key: &str) -> Result<Obj<'a>, LargeModelsError> {
        Obj::of(self.req(key)?, &self.field(key))
    }

    fn opt_child(&self, key: &str) -> Result<Option<Obj<'a>>, LargeModelsError> {
        self.opt(key).map(|value| Obj::of(value, &self.field(key))).transpose()
    }

    fn bool(&self, key: &str) -> Result<bool, LargeModelsError> {
        self.req(key)?.as_bool().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "a boolean"))
    }

    /// A boolean that defaults when absent (used only where the shape documents a default).
    fn bool_or(&self, key: &str, default: bool) -> Result<bool, LargeModelsError> {
        match self.opt(key) {
            None => Ok(default),
            Some(value) => value.as_bool().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "a boolean")),
        }
    }

    fn u64(&self, key: &str) -> Result<u64, LargeModelsError> {
        self.req(key)?.as_u64().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "an unsigned integer"))
    }

    fn opt_u64(&self, key: &str) -> Result<Option<u64>, LargeModelsError> {
        self.opt(key)
            .map(|value| value.as_u64().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "an unsigned integer")))
            .transpose()
    }

    fn opt_i64(&self, key: &str) -> Result<Option<i64>, LargeModelsError> {
        self.opt(key)
            .map(|value| value.as_i64().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "an integer")))
            .transpose()
    }

    fn f64(&self, key: &str) -> Result<f64, LargeModelsError> {
        self.req(key)?.as_f64().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "a number"))
    }

    fn opt_f64(&self, key: &str) -> Result<Option<f64>, LargeModelsError> {
        self.opt(key)
            .map(|value| value.as_f64().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "a number")))
            .transpose()
    }

    fn str(&self, key: &str) -> Result<&'a str, LargeModelsError> {
        self.req(key)?.as_str().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "a string"))
    }

    fn opt_str(&self, key: &str) -> Result<Option<&'a str>, LargeModelsError> {
        self.opt(key)
            .map(|value| value.as_str().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "a string")))
            .transpose()
    }

    /// An optional step id; an id this build does not know is `Malformed`, never `None`.
    fn opt_step<S: LargeModelStep>(&self, key: &str) -> Result<Option<S>, LargeModelsError> {
        self.opt_str(key)?
            .map(|id| S::from_wire(id).ok_or_else(|| LargeModelsError::Malformed(format!("{}: unknown step id {id:?}", self.field(key)))))
            .transpose()
    }

    fn step<S: LargeModelStep>(&self, key: &str) -> Result<S, LargeModelsError> {
        self.opt_step(key)?.ok_or_else(|| LargeModelsError::malformed(&self.field(key), "a step id"))
    }

    fn array(&self, key: &str) -> Result<&'a Vec<Value>, LargeModelsError> {
        self.req(key)?.as_array().ok_or_else(|| LargeModelsError::malformed(&self.field(key), "an array"))
    }
}

// ---------------------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------------------

/// Parses a `large_models.profiles` answer header.
///
/// # Errors
/// `Malformed` (naming the field) for a missing required field, a value of the wrong type or
/// a step id this build does not know — never a silent zero.
pub fn parse_profiles(header: &Value) -> Result<LargeModelProfiles, LargeModelsError> {
    let root = Obj::of(header, "")?;
    let formats = root
        .array("formats")?
        .iter()
        .enumerate()
        .map(|(index, row)| parse_format_row(&Obj::of(row, &format!("formats[{index}]"))?))
        .collect::<Result<Vec<_>, _>>()?;
    // Additive key of the FLUX.2 adapter: absent or `null` (older backends, FLUX.3) reads as
    // no rows; present, it is parsed as strictly as `formats`.
    let encoder_formats = match root.opt("encoder_formats") {
        None => Vec::new(),
        Some(_) => root
            .array("encoder_formats")?
            .iter()
            .enumerate()
            .map(|(index, row)| parse_format_row(&Obj::of(row, &format!("encoder_formats[{index}]"))?))
            .collect::<Result<Vec<_>, _>>()?,
    };
    let suggestions = root
        .array("suggestions")?
        .iter()
        .enumerate()
        .map(|(index, item)| parse_suggestion(&Obj::of(item, &format!("suggestions[{index}]"))?))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LargeModelProfiles {
        model: root.str("model")?.to_string(),
        format: root.opt_str("format")?.map(str::to_string),
        selected: parse_selected(&root.child("selected")?)?,
        vram: parse_axis::<VramStep>(&root.child("vram")?, "device_peak")?,
        ram: parse_axis::<RamStep>(&root.child("ram")?, "host_peak")?,
        learned_floor: root.opt_child("learned_floor")?.map(|floor| parse_floor(&floor)).transpose()?,
        limits: parse_probed_limits(&root.child("limits")?)?,
        formats,
        suggestions,
        // Additive key of the FLUX.2 adapter: absent for a model without a text encoder.
        encoder: root.opt_child("encoder")?.map(|encoder| parse_selected(&encoder)).transpose()?,
        encoder_formats,
    })
}

fn parse_selected(obj: &Obj<'_>) -> Result<SelectedPlan, LargeModelsError> {
    let forecast = obj.opt_child("forecast")?.map(|forecast| parse_forecast(&forecast)).transpose()?;
    let needs_reclaim = obj
        .opt_child("needs_reclaim")?
        .map(|reclaim| -> Result<ReclaimBytes, LargeModelsError> {
            Ok(ReclaimBytes { device_bytes: reclaim.u64("device_bytes")?, host_bytes: reclaim.u64("host_bytes")? })
        })
        .transpose()?;
    Ok(SelectedPlan {
        requested: StepChoice { vram: obj.step("vram_requested")?, ram: obj.step("ram_requested")? },
        vram: obj.opt_step("vram")?,
        ram: obj.opt_step("ram")?,
        fits: obj.bool("fits")?,
        needs_reclaim,
        reason: obj.opt_str("reason")?.map(RefusalReason::from_wire),
        shortest_resource: obj.opt_str("shortest_resource")?.map(Resource::from_wire),
        forecast,
    })
}

fn parse_forecast(obj: &Obj<'_>) -> Result<Forecast, LargeModelsError> {
    let time = obj.child("time")?;
    Ok(Forecast {
        device_peak: obj.u64("device_peak")?,
        host_peak: obj.u64("host_peak")?,
        peak_phase: obj.str("peak_phase")?.to_string(),
        load_cold_s: time.f64("load_cold_s")?,
        load_warm_s: time.f64("load_warm_s")?,
        per_step_s: time.f64("per_step_s")?,
        total_s: time.f64("total_s")?,
        calibrated: obj.bool("calibrated")?,
        device_calibrated: obj.bool("device_calibrated")?,
        host_baseline: obj.opt_u64("host_baseline")?,
    })
}

/// One axis; `peak_key` is `device_peak` (VRAM rows) or `host_peak` (RAM rows).
fn parse_axis<S: LargeModelStep>(obj: &Obj<'_>, peak_key: &str) -> Result<AxisRows<S>, LargeModelsError> {
    let steps = obj
        .array("steps")?
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let row = Obj::of(row, &format!("{}[{index}]", obj.field("steps")))?;
            Ok(StepRow {
                id: row.step::<S>("id")?,
                fits: row.bool("fits")?,
                same_as: row.opt_step::<S>("same_as")?,
                below_floor: row.bool("below_floor")?,
                peak: row.opt_u64(peak_key)?,
                total_s: row.opt_f64("total_s")?,
            })
        })
        .collect::<Result<Vec<_>, LargeModelsError>>()?;
    Ok(AxisRows {
        // Documented as always present since v13; an older answer without it meant "applies".
        applicable: obj.bool_or("applicable", true)?,
        steps,
        recommended: obj.opt_step::<S>("recommended")?,
        floor: obj.opt_step::<S>("floor")?,
    })
}

fn parse_floor(obj: &Obj<'_>) -> Result<LearnedFloor, LargeModelsError> {
    Ok(LearnedFloor {
        model: obj.str("model")?.to_string(),
        format: obj.opt_str("format")?.map(str::to_string),
        vram: obj.opt_step("vram")?,
        ram: obj.opt_step("ram")?,
        reason: FailureKind::from_wire(obj.str("reason")?),
    })
}

/// Parses a `{vram_cap, ram_cap}` object (`null` = no cap).
///
/// # Errors
/// `Malformed` when it is not an object or a cap is not an unsigned integer.
pub fn parse_limits(value: &Value) -> Result<Limits, LargeModelsError> {
    let obj = Obj::of(value, "limits")?;
    Ok(Limits { vram_cap: obj.opt_u64("vram_cap")?, ram_cap: obj.opt_u64("ram_cap")? })
}

fn parse_probed_limits(obj: &Obj<'_>) -> Result<ProbedLimits, LargeModelsError> {
    Ok(ProbedLimits {
        caps: Limits { vram_cap: obj.opt_u64("vram_cap")?, ram_cap: obj.opt_u64("ram_cap")? },
        vram_total: obj.opt_u64("vram_total")?,
        vram_free: obj.opt_u64("vram_free")?,
        vram_reserve: obj.opt_u64("vram_reserve")?,
        ram_total: obj.opt_u64("ram_total")?,
        ram_free: obj.opt_u64("ram_free")?,
        ram_cgroup_limit: obj.opt_u64("ram_cgroup_limit")?,
        ram_reserve: obj.opt_u64("ram_reserve")?,
    })
}

fn parse_steps_pair(obj: &Obj<'_>) -> Result<StepChoice, LargeModelsError> {
    Ok(StepChoice { vram: obj.step("vram")?, ram: obj.step("ram")? })
}

fn parse_format_row(obj: &Obj<'_>) -> Result<FormatRow, LargeModelsError> {
    Ok(FormatRow {
        id: obj.str("id")?.to_string(),
        available: obj.bool("available")?,
        reason: obj.opt_str("reason")?.map(str::to_string),
        dit_bytes: obj.opt_u64("dit_bytes")?,
        compute_path: obj.opt_str("compute_path")?.map(str::to_string),
        quality_rank: obj.opt_i64("quality_rank")?,
        fits_best: obj.opt_child("fits_best")?.map(|pair| parse_steps_pair(&pair)).transpose()?,
    })
}

fn parse_suggestion(obj: &Obj<'_>) -> Result<Suggestion, LargeModelsError> {
    let payload = obj.opt_child("payload")?;
    let choice = match &payload {
        Some(payload) if payload.opt("vram").is_some() || payload.opt("ram").is_some() => Some(parse_steps_pair(payload)?),
        Some(_) | None => None,
    };
    Ok(Suggestion {
        kind: SuggestionKind::from_wire(obj.str("kind")?),
        choice,
        format: payload.as_ref().map(|payload| payload.opt_str("format")).transpose()?.flatten().map(str::to_string),
    })
}

/// The error a health part carries instead of its value (`{"status":"error","error":..}`).
fn health_part_error(value: &Value) -> Option<LargeModelsError> {
    (value.get("status").and_then(Value::as_str) == Some("error")).then(|| {
        LargeModelsError::Call(value.get("error").and_then(Value::as_str).unwrap_or_default().to_string())
    })
}

/// Parses the pool part of the health snapshot (`model_manager.large_models` +
/// `model_manager.limits`), or the `large_models.status` answer's two fields.
///
/// # Errors
/// `Call` when either part is the snapshot's `{"status":"error"}` placeholder (the backend
/// could not build it), `Malformed` for any other shape violation.
pub fn parse_pool_status(large_models: &Value, limits: &Value) -> Result<LargePoolStatus, LargeModelsError> {
    if let Some(err) = health_part_error(large_models).or_else(|| health_part_error(limits)) {
        return Err(err);
    }
    let entries = large_models
        .as_array()
        .ok_or_else(|| LargeModelsError::malformed("large_models", "an array"))?
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let entry = Obj::of(entry, &format!("large_models[{index}]"))?;
            Ok(LargeModelEntry {
                model: entry.str("model")?.to_string(),
                key: entry.str("key")?.to_string(),
                format: entry.opt_str("format")?.map(str::to_string),
                device: entry.opt_str("device")?.map(str::to_string),
                vram_step: entry.opt_step("vram_step")?,
                ram_step: entry.opt_step("ram_step")?,
                device_bytes: entry.u64("device_bytes")?,
                host_bytes: entry.u64("host_bytes")?,
                in_use: entry.bool("in_use")?,
                degraded: entry.bool("degraded")?,
                degrade_reason: entry.opt_str("degrade_reason")?.map(FailureKind::from_wire),
            })
        })
        .collect::<Result<Vec<_>, LargeModelsError>>()?;
    Ok(LargePoolStatus { entries, limits: parse_limits(limits)? })
}

// ---------------------------------------------------------------------------------------
// Requests and calls (blocking: worker threads only)
// ---------------------------------------------------------------------------------------

/// One `large_models.profiles` question.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfilesRequest {
    /// The adapter name (`flux2_klein`).
    pub model: String,
    /// The model's own request params (its inpaint method's `params` object).
    pub params: Value,
    /// `[width, height]` of the generated region, pixels.
    pub region: [u32; 2],
    /// Denoising steps; `None` = the model's default.
    pub steps: Option<u32>,
    /// Weight format; `None` = the best available.
    pub format: Option<String>,
    pub choice: StepChoice,
    /// `[width, height]` of every reference image the run conditions on, in the model's
    /// own order (FLUX.3 klein: the region itself first, then each user reference). Empty
    /// for a model without references (FLUX.2), and then `shape.references` is not sent.
    pub references: Vec<[u32; 2]>,
}

/// The request header of a `profiles` call (`PROTOCOL.md` §5.10).
///
/// `shape.references` (`[{w, h}]`) is emitted only when [`ProfilesRequest::references`] is
/// non-empty, so the FLUX.2 request is byte-for-byte what it was before the field existed.
#[must_use]
pub fn profiles_request_header(req: &ProfilesRequest) -> Value {
    let mut shape = json!({ "region_width": req.region[0], "region_height": req.region[1] });
    if let (Some(steps), Some(map)) = (req.steps, shape.as_object_mut()) {
        map.insert("steps".to_string(), json!(steps));
    }
    if !req.references.is_empty()
        && let Some(map) = shape.as_object_mut()
    {
        let references: Vec<Value> = req.references.iter().map(|[w, h]| json!({ "w": w, "h": h })).collect();
        map.insert("references".to_string(), Value::Array(references));
    }
    json!({
        "model": req.model,
        "params": req.params,
        "shape": shape,
        "format": req.format,
        "vram_step": req.choice.vram.wire(),
        "ram_step": req.choice.ram.wire(),
    })
}

fn call(method: &str, header: Value) -> Result<Value, LargeModelsError> {
    let client = backend_ipc::shared_client().map_err(|_| LargeModelsError::Offline)?;
    client
        .call(method, header, &[], LARGE_MODELS_CALL_TIMEOUT)
        .map(|(header, _blob)| header)
        .map_err(LargeModelsError::from_call)
}

/// Asks the backend for the step pickers of one request. Blocking: WORKER THREAD ONLY.
///
/// # Errors
/// `Offline`, `Call` (the backend refused, e.g. an unconfigured model path) or `Malformed`
/// (logged with the field).
pub fn fetch_profiles(req: &ProfilesRequest) -> Result<LargeModelProfiles, LargeModelsError> {
    let header = call(backend_ipc::protocol::METHOD_LARGE_MODELS_PROFILES, profiles_request_header(req))?;
    parse_profiles(&header).inspect_err(|err| {
        ms_log::runtime_log::log_warn(format!("[large_models] profiles answer for model={} unreadable: {err}", req.model));
    })
}

/// The request header of a `limits.set` call: `Keep` leaves the field out, `Clear` sends
/// `null`, `Set` the byte count.
#[must_use]
pub fn limits_set_header(vram: CapChange, ram: CapChange) -> Value {
    let mut header = Map::new();
    for (field, change) in [("vram_cap_bytes", vram), ("ram_cap_bytes", ram)] {
        match change {
            CapChange::Keep => {}
            CapChange::Clear => {
                header.insert(field.to_string(), Value::Null);
            }
            CapChange::Set(bytes) => {
                header.insert(field.to_string(), json!(bytes));
            }
        }
    }
    Value::Object(header)
}

/// Sets the pool's caps; returns the limits now in force. Blocking: WORKER THREAD ONLY.
///
/// # Errors
/// `Offline`, `Call` (e.g. the backend could not store them; nothing changed) or
/// `Malformed`.
pub fn set_limits(vram: CapChange, ram: CapChange) -> Result<Limits, LargeModelsError> {
    let header = call(backend_ipc::protocol::METHOD_LARGE_MODELS_LIMITS_SET, limits_set_header(vram, ram))?;
    let limits = parse_limits(header.get("limits").unwrap_or(&Value::Null))?;
    ms_log::runtime_log::log_info(format!("[large_models] limits set: vram_cap={:?} ram_cap={:?}", limits.vram_cap, limits.ram_cap));
    Ok(limits)
}

/// Drops the idle handles (all, or `model`'s); returns the residency keys dropped.
/// Blocking: WORKER THREAD ONLY.
///
/// # Errors
/// `Offline`, `Call` or `Malformed`.
pub fn unload(model: Option<&str>) -> Result<Vec<String>, LargeModelsError> {
    let header = call(backend_ipc::protocol::METHOD_LARGE_MODELS_UNLOAD, json!({ "model": model }))?;
    let keys = header
        .get("unloaded")
        .and_then(Value::as_array)
        .ok_or_else(|| LargeModelsError::malformed("unloaded", "an array"))?
        .iter()
        .map(|key| key.as_str().map(str::to_string).ok_or_else(|| LargeModelsError::malformed("unloaded[]", "a string")))
        .collect::<Result<Vec<_>, _>>()?;
    ms_log::runtime_log::log_info(format!("[large_models] unload model={model:?} dropped={keys:?}"));
    Ok(keys)
}

/// Forgets the learned step floors (all, or `model`'s); returns how many entries.
/// Blocking: WORKER THREAD ONLY.
///
/// # Errors
/// `Offline`, `Call` or `Malformed`.
pub fn reset_learned(model: Option<&str>) -> Result<u32, LargeModelsError> {
    let header = call(backend_ipc::protocol::METHOD_LARGE_MODELS_LEARNED_RESET, json!({ "model": model }))?;
    let cleared = header
        .get("cleared")
        .and_then(Value::as_u64)
        .ok_or_else(|| LargeModelsError::malformed("cleared", "an unsigned integer"))?;
    ms_log::runtime_log::log_info(format!("[large_models] learned floors reset model={model:?} cleared={cleared}"));
    Ok(u32::try_from(cleared).unwrap_or(u32::MAX))
}

// ---------------------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------------------

/// Formats a byte count as GIBIBYTES (2^30 bytes) with one decimal. The unit lives in the
/// locale template that consumes this, which must name GiB, not a decimal gigabyte.
#[must_use]
pub fn format_gib(bytes: u64) -> String {
    // Cast justification: display-only; above 2^53 bytes the f64 rounding is far below the
    // one decimal shown.
    let gib = bytes as f64 / GIB;
    format!("{gib:.1}")
}

/// Formats a forecast duration coarsely: seconds, minutes, or hours and minutes. A
/// non-finite or negative input reads as zero seconds.
#[must_use]
pub fn format_duration(seconds: f64) -> String {
    let seconds = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
    // Cast justification: the value is finite, non-negative and bounded to 1e9 before the
    // cast, so the conversion neither saturates nor wraps.
    let whole = seconds.min(1.0e9).round() as u64;
    if whole >= 3600 {
        return tf!("large_models.time.hours_label", hours = whole / 3600, minutes = (whole % 3600) / 60);
    }
    if whole >= 60 {
        return tf!("large_models.time.minutes_label", minutes = (whole + 30) / 60);
    }
    tf!("large_models.time.seconds_label", seconds = whole.max(1))
}

// ---------------------------------------------------------------------------------------
// ProfilesQuery: one request in flight, latest wins
// ---------------------------------------------------------------------------------------

/// The function a [`ProfilesQuery`] runs on its worker (injectable for tests).
pub type ProfilesFetcher = Arc<dyn Fn(&ProfilesRequest) -> Result<LargeModelProfiles, LargeModelsError> + Send + Sync>;

/// What makes two `profiles` questions the same: everything the request carries plus the
/// query's invalidation epoch. `params` is the JSON text — deterministic, because
/// `serde_json` is built without `preserve_order` (its maps are sorted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilesKey {
    model: String,
    params: String,
    region: [u32; 2],
    steps: Option<u32>,
    format: Option<String>,
    choice: StepChoice,
    references: Vec<[u32; 2]>,
    epoch: u64,
}

impl ProfilesKey {
    /// The key of `req` under invalidation epoch `epoch`.
    #[must_use]
    pub fn new(req: &ProfilesRequest, epoch: u64) -> Self {
        Self {
            model: req.model.clone(),
            params: req.params.to_string(),
            region: req.region,
            steps: req.steps,
            format: req.format.clone(),
            choice: req.choice,
            references: req.references.clone(),
            epoch,
        }
    }
}

/// The request on the worker and the channel it answers on.
struct InFlight {
    key: ProfilesKey,
    rx: Receiver<Result<LargeModelProfiles, LargeModelsError>>,
}

/// Latest-wins `large_models.profiles` query for one panel.
///
/// At most ONE request is in flight. [`ProfilesQuery::request`] records the wanted request;
/// when nothing is in flight and no answer (or error) for that exact key is held, it is sent.
/// A request made while another is in flight waits; when the running one lands it is kept
/// (as a STALE answer the panel may show dimmed) and the wanted one is sent next — so a
/// dragged control costs at most two round trips, never one per frame. [`Self::poll`] only
/// `try_recv`s and is safe on the GUI thread.
pub struct ProfilesQuery {
    fetcher: ProfilesFetcher,
    epoch: u64,
    wanted: Option<ProfilesRequest>,
    in_flight: Option<InFlight>,
    latest: Option<(ProfilesKey, LargeModelProfiles)>,
    error: Option<(ProfilesKey, LargeModelsError)>,
}

impl fmt::Debug for ProfilesQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfilesQuery")
            .field("epoch", &self.epoch)
            .field("wanted", &self.wanted.is_some())
            .field("in_flight", &self.in_flight.is_some())
            .field("latest", &self.latest.is_some())
            .field("error", &self.error.as_ref().map(|(_, err)| err))
            .finish()
    }
}

impl Default for ProfilesQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl ProfilesQuery {
    /// A query that asks the backend ([`fetch_profiles`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_fetcher(Arc::new(fetch_profiles))
    }

    /// A query over a custom fetcher (tests).
    #[must_use]
    pub fn with_fetcher(fetcher: ProfilesFetcher) -> Self {
        Self { fetcher, epoch: 0, wanted: None, in_flight: None, latest: None, error: None }
    }

    /// Records `req` as the wanted question and sends it when nothing is in flight. An
    /// identical question (same key, no invalidation since) is never resent.
    pub fn request(&mut self, req: ProfilesRequest) {
        self.wanted = Some(req);
        self.dispatch();
    }

    /// Marks every held answer stale (the machine or the backend's knowledge changed): the
    /// wanted question is asked again.
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.dispatch();
    }

    /// Drains the worker; returns `true` when an answer (or an error) landed this call.
    /// Sends the wanted question next when it differs from what landed. GUI-safe.
    pub fn poll(&mut self) -> bool {
        let mut landed = false;
        if let Some(in_flight) = self.in_flight.as_ref() {
            let outcome = match in_flight.rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    ms_log::runtime_log::log_warn("[large_models] profiles worker ended without an answer");
                    Some(Err(LargeModelsError::WorkerLost))
                }
            };
            if let Some(result) = outcome
                && let Some(in_flight) = self.in_flight.take()
            {
                landed = true;
                match result {
                    Ok(profiles) => {
                        if self.error.as_ref().is_some_and(|(key, _)| *key == in_flight.key) {
                            self.error = None;
                        }
                        self.latest = Some((in_flight.key, profiles));
                    }
                    // The previous answer is KEPT: an error says nothing about the plan it
                    // described, and dropping it would blank the panel on a transient failure.
                    Err(err) => self.error = Some((in_flight.key, err)),
                }
            }
        }
        self.dispatch();
        landed
    }

    /// The most recent answer, possibly about an older question ([`Self::is_stale`]).
    #[must_use]
    pub fn latest(&self) -> Option<&LargeModelProfiles> {
        self.latest.as_ref().map(|(_, profiles)| profiles)
    }

    /// Whether the held answer is about another question than the wanted one.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        match (&self.latest, self.wanted_key()) {
            (Some((key, _)), Some(wanted)) => *key != wanted,
            (Some(_) | None, None) | (None, Some(_)) => false,
        }
    }

    /// Whether a request is on the worker.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The error of the WANTED question, if asking it failed.
    #[must_use]
    pub fn error(&self) -> Option<&LargeModelsError> {
        let wanted = self.wanted_key()?;
        self.error.as_ref().filter(|(key, _)| *key == wanted).map(|(_, err)| err)
    }

    fn wanted_key(&self) -> Option<ProfilesKey> {
        self.wanted.as_ref().map(|req| ProfilesKey::new(req, self.epoch))
    }

    fn dispatch(&mut self) {
        if self.in_flight.is_some() {
            return;
        }
        let Some(req) = self.wanted.clone() else {
            return;
        };
        let key = ProfilesKey::new(&req, self.epoch);
        let answered = self.latest.as_ref().is_some_and(|(held, _)| *held == key)
            || self.error.as_ref().is_some_and(|(held, _)| *held == key);
        if answered {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let fetcher = Arc::clone(&self.fetcher);
        ms_thread::spawn(move || {
            // A failed send means the query was dropped meanwhile: nobody wants the answer.
            let _ = tx.send(fetcher(&req));
        });
        self.in_flight = Some(InFlight { key, rx });
    }
}

// ---------------------------------------------------------------------------------------
// LimitsWriter: debounced, axis-scoped latest-wins `limits.set`
// ---------------------------------------------------------------------------------------

/// The function a [`LimitsWriter`] runs on its worker (injectable for tests).
pub type LimitsSetter = Arc<dyn Fn(CapChange, CapChange) -> Result<Limits, LargeModelsError> + Send + Sync>;

/// The caps one pending `limits.set` will carry: per axis, `Keep` (not touched) or the new
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingCaps {
    vram: CapChange,
    ram: CapChange,
    at: Instant,
}

/// Debounced, AXIS-SCOPED, latest-wins writer of the pool caps.
///
/// [`Self::edit`] records the newest change of ONE OR BOTH axes (`Keep` = this edit did not
/// touch the axis, so an earlier pending change of it survives); [`Self::poll`] sends the
/// merged pair once it has been quiet for [`LIMITS_EDIT_DEBOUNCE`] and nothing is in flight,
/// so a dragged spin box costs one `limits.set`, not one per frame. An untouched axis travels
/// as `Keep`: the backend merges atomically and answers the limits in force, so two editors
/// (the cleaning panel and the settings pane, each with its own writer) never overwrite each
/// other's axis with a stale value; each reconciles its display from the answer.
///
/// DROPPING a writer with a pending edit hands the edit to a worker (never blocking the
/// caller), so closing a panel inside the quiet time does not lose it. That send is
/// best-effort at process exit: a worker still waiting on the backend dies with the process.
pub struct LimitsWriter {
    setter: LimitsSetter,
    pending: Option<PendingCaps>,
    in_flight: Option<Receiver<Result<Limits, LargeModelsError>>>,
}

impl fmt::Debug for LimitsWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LimitsWriter")
            .field("pending", &self.pending.as_ref().map(|pending| (pending.vram, pending.ram)))
            .field("in_flight", &self.in_flight.is_some())
            .finish()
    }
}

impl Default for LimitsWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LimitsWriter {
    /// Delivers a pending edit on a worker instead of losing it with its owner.
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            ms_log::runtime_log::log_info(format!(
                "[large_models] limits writer dropped with a pending edit (vram={:?} ram={:?}); sending it now",
                pending.vram, pending.ram
            ));
            Self::spawn_send(&self.setter, pending);
        }
    }
}

impl LimitsWriter {
    /// A writer that calls the backend ([`set_limits`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_setter(Arc::new(set_limits))
    }

    /// A writer over a custom setter (tests).
    #[must_use]
    pub fn with_setter(setter: LimitsSetter) -> Self {
        Self { setter, pending: None, in_flight: None }
    }

    /// Records an edit made at `now`: `vram` / `ram` are the changes of that edit, `Keep` for
    /// an axis it did not touch (an earlier pending change of that axis is kept).
    pub fn edit(&mut self, vram: CapChange, ram: CapChange, now: Instant) {
        let merge = |new: CapChange, old: CapChange| if new == CapChange::Keep { old } else { new };
        let (old_vram, old_ram) = self.pending.map_or((CapChange::Keep, CapChange::Keep), |pending| (pending.vram, pending.ram));
        self.pending = Some(PendingCaps { vram: merge(vram, old_vram), ram: merge(ram, old_ram), at: now });
    }

    /// Whether an edit waits for its quiet time or a write is in flight.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.pending.is_some() || self.in_flight.is_some()
    }

    fn spawn_send(setter: &LimitsSetter, pending: PendingCaps) -> Receiver<Result<Limits, LargeModelsError>> {
        let (tx, rx) = mpsc::channel();
        let setter = Arc::clone(setter);
        ms_thread::spawn(move || {
            // A failed send means the writer was dropped meanwhile: nobody waits.
            let _ = tx.send(setter(pending.vram, pending.ram));
        });
        rx
    }

    /// Drains a landed write (returned once: the limits IN FORCE, both axes) and sends the
    /// pending edit when it is due. GUI-safe: never blocks.
    pub fn poll(&mut self, now: Instant) -> Option<Result<Limits, LargeModelsError>> {
        let mut landed = None;
        if let Some(rx) = self.in_flight.as_ref() {
            match rx.try_recv() {
                Ok(result) => landed = Some(result),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    ms_log::runtime_log::log_warn("[large_models] limits worker ended without an answer");
                    landed = Some(Err(LargeModelsError::WorkerLost));
                }
            }
            if landed.is_some() {
                self.in_flight = None;
            }
        }
        let due = self
            .pending
            .as_ref()
            .is_some_and(|pending| now.saturating_duration_since(pending.at) >= LIMITS_EDIT_DEBOUNCE);
        if self.in_flight.is_none()
            && due
            && let Some(pending) = self.pending.take()
        {
            self.in_flight = Some(Self::spawn_send(&self.setter, pending));
        }
        landed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const FIXTURE: &str = include_str!("../fixtures/large_models_profiles_v13.json");

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).expect("the v13 profiles fixture is valid JSON")
    }

    /// The pinned FLUX.2 v13 answer parses completely: both axes, a refusal row, the floor,
    /// the probed limits, a format row, a suggestion and the additive encoder object.
    #[test]
    fn parses_the_v13_flux2_fixture() {
        let profiles = parse_profiles(&fixture()).expect("the fixture parses");
        assert_eq!(profiles.model, "flux2_klein");
        assert_eq!(profiles.format.as_deref(), Some("bf16"));
        let selected = &profiles.selected;
        assert_eq!(selected.requested, StepChoice { vram: VramStep::Auto, ram: RamStep::Auto });
        assert_eq!(selected.vram, Some(VramStep::Phase));
        assert_eq!(selected.ram, Some(RamStep::Keep));
        assert!(selected.fits);
        assert_eq!(selected.needs_reclaim, Some(ReclaimBytes { device_bytes: 0, host_bytes: 1_073_741_824 }));
        let forecast = selected.forecast.as_ref().expect("a fitting plan carries its forecast");
        assert_eq!(forecast.device_peak, 14_495_514_624);
        assert!(!forecast.calibrated);
        assert!((forecast.warm_total_s() - (forecast.total_s - forecast.load_cold_s + forecast.load_warm_s)).abs() < 1e-9);

        assert!(profiles.vram.applicable);
        assert_eq!(profiles.vram.steps.len(), 4);
        let max = &profiles.vram.steps[0];
        assert_eq!(max.id, VramStep::Max);
        assert!(!max.fits, "the fixture's refusal row");
        assert!(max.below_floor);
        assert_eq!(profiles.vram.steps[3].same_as, Some(VramStep::Half));
        assert_eq!(profiles.vram.recommended, Some(VramStep::Phase));
        assert_eq!(profiles.vram.floor, Some(VramStep::Phase));
        assert_eq!(profiles.ram.steps.len(), 3);
        assert_eq!(profiles.ram.steps[0].peak, Some(30_064_771_072), "RAM rows read host_peak");

        let floor = profiles.learned_floor.as_ref().expect("the fixture has a floor");
        assert_eq!(floor.model, "flux2_klein");
        assert_eq!(floor.reason, FailureKind::DeviceOom);
        assert_eq!(profiles.limits.caps, Limits { vram_cap: None, ram_cap: Some(34_359_738_368) });
        assert_eq!(profiles.limits.vram_total, Some(25_753_026_560));
        assert_eq!(profiles.limits.ram_cgroup_limit, None);
        assert_eq!(profiles.formats.len(), 1);
        assert_eq!(profiles.formats[0].fits_best, Some(StepChoice { vram: VramStep::Phase, ram: RamStep::Keep }));
        assert_eq!(profiles.suggestions.len(), 1);
        assert_eq!(profiles.suggestions[0].kind, SuggestionKind::RaiseLimits);
        assert_eq!(profiles.suggestions[0].choice, Some(StepChoice { vram: VramStep::Max, ram: RamStep::Keep }));
        let encoder = profiles.encoder.as_ref().expect("the fixture carries the encoder plan");
        assert!(encoder.fits);
        assert_eq!(encoder.vram, Some(VramStep::Phase));
        assert_eq!(forecast.host_baseline, None, "the v13 fixture predates the optional baseline");
    }

    /// The optional `host_baseline` of a forecast is read when present; a malformed one is
    /// malformed, never ignored.
    #[test]
    fn the_forecast_baseline_is_optional() {
        let mut value = fixture();
        value["selected"]["forecast"]["host_baseline"] = json!(1_610_612_736u64);
        let profiles = parse_profiles(&value).expect("the fixture parses with a baseline");
        assert_eq!(profiles.selected.forecast.as_ref().and_then(|forecast| forecast.host_baseline), Some(1_610_612_736));
        value["selected"]["forecast"]["host_baseline"] = json!("lots");
        assert!(matches!(parse_profiles(&value), Err(LargeModelsError::Malformed(_))));
    }

    /// An unknown step id is a malformed answer naming the field, never a silent fallback.
    #[test]
    fn an_unknown_step_id_is_malformed() {
        let mut value = fixture();
        value["vram"]["steps"][1]["id"] = json!("turbo");
        match parse_profiles(&value) {
            Err(LargeModelsError::Malformed(message)) => assert!(message.contains("vram.steps[1].id"), "{message}"),
            other => panic!("expected Malformed, got {other:?}"),
        }
        let mut value = fixture();
        value["selected"]["vram_requested"] = json!("fast");
        assert!(matches!(parse_profiles(&value), Err(LargeModelsError::Malformed(_))));
    }

    /// A missing REQUIRED field is malformed; a missing OPTIONAL one is accepted.
    #[test]
    fn required_and_optional_fields() {
        let mut value = fixture();
        value["selected"].as_object_mut().expect("object").remove("fits");
        assert!(matches!(parse_profiles(&value), Err(LargeModelsError::Malformed(m)) if m.contains("selected.fits")));

        let mut value = fixture();
        value["selected"]["forecast"].as_object_mut().expect("object").remove("device_peak");
        assert!(matches!(parse_profiles(&value), Err(LargeModelsError::Malformed(_))));

        let mut value = fixture();
        let root = value.as_object_mut().expect("object");
        root.remove("encoder");
        root.insert("learned_floor".to_string(), Value::Null);
        value["limits"].as_object_mut().expect("object").remove("vram_free");
        value["vram"].as_object_mut().expect("object").remove("applicable");
        let profiles = parse_profiles(&value).expect("optional fields may be absent");
        assert!(profiles.encoder.is_none());
        assert!(profiles.learned_floor.is_none());
        assert_eq!(profiles.limits.vram_free, None);
        assert!(profiles.vram.applicable, "an absent `applicable` reads as applicable");

        let mut value = fixture();
        value["encoder"] = Value::Null;
        assert!(parse_profiles(&value).expect("encoder: null").encoder.is_none());
    }

    /// `encoder_formats` is additive: absent or `null` reads as no rows, present rows parse
    /// like `formats`, and a malformed row is `Malformed` naming its index.
    #[test]
    fn encoder_formats_are_additive_and_strict() {
        assert!(parse_profiles(&fixture()).expect("fixture").encoder_formats.is_empty(), "the v13 fixture has none");
        let mut value = fixture();
        value["encoder_formats"] = Value::Null;
        assert!(parse_profiles(&value).expect("encoder_formats: null").encoder_formats.is_empty());

        let mut value = fixture();
        let row = value["formats"][0].clone();
        let mut gguf = row.clone();
        gguf["id"] = json!("q4_k_m");
        gguf["fits_best"] = Value::Null;
        value["encoder_formats"] = json!([row, gguf]);
        let profiles = parse_profiles(&value).expect("encoder rows parse");
        assert_eq!(profiles.encoder_formats.len(), 2);
        assert_eq!(profiles.encoder_formats[0], profiles.formats[0], "same row shape as `formats`");
        assert_eq!(profiles.encoder_formats[1].id, "q4_k_m");
        assert_eq!(profiles.encoder_formats[1].fits_best, None);

        value["encoder_formats"][1].as_object_mut().expect("object").remove("available");
        match parse_profiles(&value) {
            Err(LargeModelsError::Malformed(message)) => assert!(message.contains("encoder_formats[1].available"), "{message}"),
            other => panic!("expected Malformed, got {other:?}"),
        }
        let mut value = fixture();
        value["encoder_formats"] = json!({});
        assert!(matches!(parse_profiles(&value), Err(LargeModelsError::Malformed(m)) if m.contains("encoder_formats")));
    }

    /// A refused selection carries its reason and resource as typed codes.
    #[test]
    fn a_refusal_reads_its_codes() {
        let mut value = fixture();
        value["selected"]["fits"] = json!(false);
        value["selected"]["vram"] = Value::Null;
        value["selected"]["ram"] = Value::Null;
        value["selected"]["reason"] = json!("host_short");
        value["selected"]["shortest_resource"] = json!("host");
        let selected = parse_profiles(&value).expect("parses").selected;
        assert!(!selected.fits);
        assert_eq!(selected.vram, None);
        assert_eq!(selected.reason, Some(RefusalReason::HostShort));
        assert_eq!(selected.shortest_resource, Some(Resource::Host));
        assert_eq!(RefusalReason::from_wire("brand_new"), RefusalReason::Other("brand_new".to_string()));
        assert_eq!(FailureKind::from_wire("sysmem_spill"), FailureKind::SysmemSpill);
    }

    #[test]
    fn step_wire_ids_round_trip() {
        for step in [VramStep::Auto, VramStep::Max, VramStep::Phase, VramStep::Half, VramStep::Min] {
            assert_eq!(VramStep::from_wire(step.wire()), Some(step));
        }
        for step in [RamStep::Auto, RamStep::Keep, RamStep::Run, RamStep::Min] {
            assert_eq!(RamStep::from_wire(step.wire()), Some(step));
        }
        assert_eq!(VramStep::from_wire("keep"), None);
        assert_eq!(RamStep::from_wire("phase"), None);
        assert_eq!(VramStep::CONCRETE.len(), 4);
        assert_eq!(RamStep::CONCRETE.len(), 3);
    }

    /// `Keep` leaves the field out, `Clear` sends `null`, `Set` the bytes.
    #[test]
    fn limits_header_encodes_keep_clear_and_set() {
        assert_eq!(limits_set_header(CapChange::Keep, CapChange::Keep), json!({}));
        assert_eq!(
            limits_set_header(CapChange::Clear, CapChange::Set(8)),
            json!({ "vram_cap_bytes": null, "ram_cap_bytes": 8 })
        );
        let header = limits_set_header(CapChange::Keep, CapChange::Clear);
        assert!(header.get("vram_cap_bytes").is_none());
        assert_eq!(header["ram_cap_bytes"], Value::Null);
        assert_eq!(CapChange::from_cap(None), CapChange::Clear);
        assert_eq!(CapChange::from_cap(Some(3)), CapChange::Set(3));
    }

    #[test]
    fn profiles_header_carries_shape_and_steps() {
        let req = ProfilesRequest {
            model: "flux2_klein".to_string(),
            params: json!({ "dtype": "bfloat16" }),
            region: [512, 768],
            steps: Some(4),
            format: None,
            choice: StepChoice { vram: VramStep::Half, ram: RamStep::Auto },
            references: Vec::new(),
        };
        let header = profiles_request_header(&req);
        assert_eq!(header["model"], json!("flux2_klein"));
        assert_eq!(header["shape"], json!({ "region_width": 512, "region_height": 768, "steps": 4 }));
        assert_eq!(header["format"], Value::Null);
        assert_eq!(header["vram_step"], json!("half"));
        assert_eq!(header["ram_step"], json!("auto"));
        assert!(header["shape"].get("references").is_none(), "no references: the shape is FLUX.2's, unchanged");
        let with_references = profiles_request_header(&ProfilesRequest { references: vec![[512, 768], [300, 200]], ..req.clone() });
        assert_eq!(with_references["shape"]["references"], json!([{ "w": 512, "h": 768 }, { "w": 300, "h": 200 }]));
        let without_steps = profiles_request_header(&ProfilesRequest { steps: None, ..req });
        assert!(without_steps["shape"].get("steps").is_none());
    }

    /// The key covers every request field and the epoch, and the params text is stable.
    #[test]
    fn the_cache_key_covers_the_request_and_the_epoch() {
        let req = ProfilesRequest {
            model: "flux2_klein".to_string(),
            params: json!({ "b": 1, "a": 2 }),
            region: [512, 512],
            steps: Some(4),
            format: None,
            choice: StepChoice::default(),
            references: Vec::new(),
        };
        let reordered = ProfilesRequest { params: json!({ "a": 2, "b": 1 }), ..req.clone() };
        assert_eq!(ProfilesKey::new(&req, 0), ProfilesKey::new(&reordered, 0), "object key order is irrelevant");
        assert_ne!(ProfilesKey::new(&req, 0), ProfilesKey::new(&req, 1));
        let other_choice = ProfilesRequest { choice: StepChoice { vram: VramStep::Min, ram: RamStep::Auto }, ..req.clone() };
        assert_ne!(ProfilesKey::new(&req, 0), ProfilesKey::new(&other_choice, 0));
        let other_region = ProfilesRequest { region: [512, 640], ..req.clone() };
        assert_ne!(ProfilesKey::new(&req, 0), ProfilesKey::new(&other_region, 0));
        let other_references = ProfilesRequest { references: vec![[512, 512]], ..req.clone() };
        assert_ne!(ProfilesKey::new(&req, 0), ProfilesKey::new(&other_references, 0));
    }

    /// A fetcher whose calls are recorded and released one by one through a gate channel.
    struct GatedFetcher {
        calls: Arc<Mutex<Vec<ProfilesRequest>>>,
        gate: mpsc::Sender<Result<LargeModelProfiles, LargeModelsError>>,
    }

    fn gated_query() -> (ProfilesQuery, GatedFetcher) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (gate_tx, gate_rx) = mpsc::channel::<Result<LargeModelProfiles, LargeModelsError>>();
        let gate_rx = Arc::new(Mutex::new(gate_rx));
        let recorded = Arc::clone(&calls);
        let fetcher: ProfilesFetcher = Arc::new(move |req: &ProfilesRequest| {
            recorded.lock().expect("calls lock").push(req.clone());
            gate_rx
                .lock()
                .expect("gate lock")
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or(Err(LargeModelsError::WorkerLost))
        });
        (ProfilesQuery::with_fetcher(fetcher), GatedFetcher { calls, gate: gate_tx })
    }

    fn request(width: u32) -> ProfilesRequest {
        ProfilesRequest {
            model: "flux2_klein".to_string(),
            params: json!({}),
            region: [width, 512],
            steps: Some(4),
            format: None,
            choice: StepChoice::default(),
            references: Vec::new(),
        }
    }

    fn answer(model: &str) -> LargeModelProfiles {
        let mut profiles = parse_profiles(&fixture()).expect("fixture");
        profiles.model = model.to_string();
        profiles
    }

    /// Polls until an answer lands (the worker is a real thread).
    fn poll_until_landed(query: &mut ProfilesQuery) {
        for _ in 0..2000 {
            if query.poll() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("no answer landed");
    }

    /// Waits until the fetcher has been called `n` times.
    fn wait_calls(fetcher: &GatedFetcher, n: usize) {
        for _ in 0..2000 {
            if fetcher.calls.lock().expect("calls lock").len() >= n {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the fetcher was not called {n} times");
    }

    /// A then B while A is in flight: A lands and is kept STALE, B is sent exactly once and
    /// ends up final.
    #[test]
    fn a_request_made_in_flight_waits_and_wins() {
        let (mut query, fetcher) = gated_query();
        query.request(request(512));
        assert!(query.is_busy());
        query.request(request(640));
        query.request(request(640));
        wait_calls(&fetcher, 1);
        fetcher.gate.send(Ok(answer("a"))).expect("gate");
        poll_until_landed(&mut query);
        assert_eq!(query.latest().map(|p| p.model.as_str()), Some("a"));
        assert!(query.is_stale(), "A answers an older question than B");
        assert!(query.is_busy(), "B was dispatched as soon as A landed");
        wait_calls(&fetcher, 2);
        fetcher.gate.send(Ok(answer("b"))).expect("gate");
        poll_until_landed(&mut query);
        assert_eq!(query.latest().map(|p| p.model.as_str()), Some("b"));
        assert!(!query.is_stale());
        assert!(!query.is_busy());
        let calls = fetcher.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 2, "B is sent once, however often it was requested");
        assert_eq!(calls[1].region, [640, 512]);
    }

    /// An identical request is not resent; `invalidate` resends it.
    #[test]
    fn identical_requests_are_not_resent_and_invalidate_resends() {
        let (mut query, fetcher) = gated_query();
        query.request(request(512));
        wait_calls(&fetcher, 1);
        fetcher.gate.send(Ok(answer("a"))).expect("gate");
        poll_until_landed(&mut query);
        query.request(request(512));
        assert!(!query.is_busy(), "the same question is already answered");
        query.invalidate();
        assert!(query.is_busy(), "an invalidation asks again");
        assert!(query.is_stale(), "the held answer predates the invalidation");
        wait_calls(&fetcher, 2);
        fetcher.gate.send(Ok(answer("a2"))).expect("gate");
        poll_until_landed(&mut query);
        assert_eq!(fetcher.calls.lock().expect("calls lock").len(), 2);
        assert!(!query.is_stale());
    }

    /// An error is surfaced for the wanted question and the previous answer is kept.
    #[test]
    fn an_error_is_surfaced_and_the_previous_answer_kept() {
        let (mut query, fetcher) = gated_query();
        query.request(request(512));
        wait_calls(&fetcher, 1);
        fetcher.gate.send(Ok(answer("a"))).expect("gate");
        poll_until_landed(&mut query);
        query.request(request(640));
        wait_calls(&fetcher, 2);
        fetcher.gate.send(Err(LargeModelsError::Call("paths missing".to_string()))).expect("gate");
        poll_until_landed(&mut query);
        assert_eq!(query.error(), Some(&LargeModelsError::Call("paths missing".to_string())));
        assert_eq!(query.latest().map(|p| p.model.as_str()), Some("a"), "the last good answer stays");
        assert!(query.is_stale());
        assert!(!query.is_busy(), "a failed question is not retried by itself");
        // Going back to the answered question clears the error view.
        query.request(request(512));
        assert!(query.error().is_none());
        assert!(!query.is_stale());
    }

    /// Polls `writer` until a write lands.
    fn poll_until_written(writer: &mut LimitsWriter, now: Instant) -> Result<Limits, LargeModelsError> {
        for _ in 0..2000 {
            if let Some(landed) = writer.poll(now) {
                return landed;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("no write landed");
    }

    /// A fake backend: merges each `limits.set` atomically into one shared state, exactly as
    /// `LargeModelManager.set_limits` does, and records every call.
    /// Every `(vram, ram)` pair a fake backend was called with.
    type RecordedCalls = Arc<Mutex<Vec<(CapChange, CapChange)>>>;

    fn merging_backend() -> (Arc<Mutex<Limits>>, RecordedCalls, LimitsSetter) {
        let state = Arc::new(Mutex::new(Limits::default()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (shared, recorded) = (Arc::clone(&state), Arc::clone(&calls));
        let setter: LimitsSetter = Arc::new(move |vram, ram| {
            recorded.lock().expect("calls lock").push((vram, ram));
            let mut limits = shared.lock().expect("state lock");
            let apply = |cap: &mut Option<u64>, change: CapChange| match change {
                CapChange::Keep => {}
                CapChange::Clear => *cap = None,
                CapChange::Set(bytes) => *cap = Some(bytes),
            };
            apply(&mut limits.vram_cap, vram);
            apply(&mut limits.ram_cap, ram);
            Ok(*limits)
        });
        (state, calls, setter)
    }

    /// The writer waits for the quiet time, merges the edits per axis, sends once, and
    /// reports once.
    #[test]
    fn the_limits_writer_debounces_and_sends_the_latest_edit() {
        let (_state, calls, setter) = merging_backend();
        let mut writer = LimitsWriter::with_setter(setter);
        let t0 = Instant::now();
        writer.edit(CapChange::Set(10), CapChange::Keep, t0);
        writer.edit(CapChange::Set(20), CapChange::Keep, t0 + Duration::from_millis(50));
        writer.edit(CapChange::Keep, CapChange::Clear, t0 + Duration::from_millis(100));
        assert!(writer.poll(t0 + Duration::from_millis(300)).is_none());
        assert!(calls.lock().expect("calls lock").is_empty(), "still inside the quiet time");
        assert!(writer.is_busy());
        let due = t0 + Duration::from_millis(100) + LIMITS_EDIT_DEBOUNCE;
        assert_eq!(poll_until_written(&mut writer, due), Ok(Limits { vram_cap: Some(20), ram_cap: None }));
        assert_eq!(
            *calls.lock().expect("calls lock"),
            vec![(CapChange::Set(20), CapChange::Clear)],
            "the newest VRAM value and the later RAM change, merged, in one call"
        );
        assert!(!writer.is_busy());
        assert!(writer.poll(due).is_none(), "a result is reported once");
    }

    /// Two editors with their own writers: a VRAM-only edit then a RAM-only edit keep BOTH,
    /// because each sends `Keep` for the axis it did not touch.
    #[test]
    fn two_writers_editing_different_axes_keep_both_edits() {
        let (state, calls, setter) = merging_backend();
        let mut cleaning = LimitsWriter::with_setter(Arc::clone(&setter));
        let mut settings = LimitsWriter::with_setter(setter);
        let t0 = Instant::now();
        cleaning.edit(CapChange::Set(8), CapChange::Keep, t0);
        let due = t0 + LIMITS_EDIT_DEBOUNCE;
        assert_eq!(poll_until_written(&mut cleaning, due), Ok(Limits { vram_cap: Some(8), ram_cap: None }));
        settings.edit(CapChange::Keep, CapChange::Set(16), t0);
        assert_eq!(
            poll_until_written(&mut settings, due),
            Ok(Limits { vram_cap: Some(8), ram_cap: Some(16) }),
            "the answer is the limits in force, which the editor reconciles from"
        );
        assert_eq!(*state.lock().expect("state lock"), Limits { vram_cap: Some(8), ram_cap: Some(16) });
        assert_eq!(calls.lock().expect("calls lock").len(), 2);
    }

    /// Dropping a writer inside the quiet time still delivers its edit (on a worker).
    #[test]
    fn dropping_a_writer_with_a_pending_edit_delivers_it() {
        let (state, calls, setter) = merging_backend();
        let mut writer = LimitsWriter::with_setter(setter);
        writer.edit(CapChange::Keep, CapChange::Set(32), Instant::now());
        drop(writer);
        for _ in 0..2000 {
            if !calls.lock().expect("calls lock").is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(*calls.lock().expect("calls lock"), vec![(CapChange::Keep, CapChange::Set(32))]);
        assert_eq!(state.lock().expect("state lock").ram_cap, Some(32));
        // A writer with nothing pending sends nothing on drop.
        let (_state, idle_calls, idle_setter) = merging_backend();
        drop(LimitsWriter::with_setter(idle_setter));
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(idle_calls.lock().expect("calls lock").is_empty());
    }

    /// The health pool part parses, and a part the backend failed to build is an error.
    #[test]
    fn pool_status_parses_and_error_parts_are_errors() {
        let large = json!([{
            "model": "flux2_klein", "key": "flux2_klein@cuda:0", "format": "bf16", "device": "cuda:0",
            "vram_step": "phase", "ram_step": "keep", "device_bytes": 9_000_000_000u64, "host_bytes": 1_000u64,
            "in_use": false, "degraded": true, "degrade_reason": "device_oom", "components": {}
        }]);
        let limits = json!({ "vram_cap": null, "ram_cap": 17_179_869_184u64 });
        let status = parse_pool_status(&large, &limits).expect("parses");
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].vram_step, Some(VramStep::Phase));
        assert_eq!(status.entries[0].degrade_reason, Some(FailureKind::DeviceOom));
        assert_eq!(status.limits, Limits { vram_cap: None, ram_cap: Some(17_179_869_184) });

        let failed = json!({ "status": "error", "error": "ledger busy" });
        assert_eq!(parse_pool_status(&failed, &limits), Err(LargeModelsError::Call("ledger busy".to_string())));
        assert!(matches!(parse_pool_status(&large, &failed), Err(LargeModelsError::Call(_))));
        assert!(matches!(parse_pool_status(&json!({}), &limits), Err(LargeModelsError::Malformed(_))));
    }

    #[test]
    fn gib_and_duration_formatting() {
        assert_eq!(format_gib(0), "0.0");
        assert_eq!(format_gib(1024 * 1024 * 1024), "1.0");
        assert_eq!(format_gib(3 * 512 * 1024 * 1024), "1.5");
        // Without a loaded catalog the key itself is returned: the branch is what is pinned.
        assert_eq!(format_duration(12.4), "large_models.time.seconds_label");
        assert_eq!(format_duration(f64::NAN), "large_models.time.seconds_label");
        assert_eq!(format_duration(125.0), "large_models.time.minutes_label");
        assert_eq!(format_duration(7300.0), "large_models.time.hours_label");
    }
}
