/*
File: cleaning/tools/ai_editor/engines/flux3_klein/settings.rs

Purpose:
Everything the FLUX.3 klein engine persists (`config::flux3_klein_settings_path`), the wire
vocabularies those values are spelled in, the ONE builder of the wire `params` object, and the
settings-file load/save over the shared `settings_file` rules (a damaged file is moved aside,
never overwritten; a failed write is retried with backoff).

Main responsibilities:
- own `Flux3KleinSettings`, its defaults and `normalized()` (the only value put on the wire);
- answer which package root a call names (`effective_root`, the ONE answer for both source
  modes) and which step pair is persisted (`step_choice` / `set_step_choice`);
- build the `params` of a run, a `.status` and a forecast (`to_params`);
- load and save the settings file on worker threads (`load_flux3_settings`,
  `save_flux3_settings`).

Key structures:
- `Flux3KleinSettings`, `Flux3SourceMode`, `Flux3Format`, `Flux3Fp8Compute`, `Flux3SettingsLoad`

Key functions:
- `Flux3KleinSettings::{normalized, effective_root, to_params, step_choice, set_step_choice}`
- `settings_from_json()`, `load_flux3_settings()`, `load_flux3_settings_from()`, `save_flux3_settings()`

Notes:
The wire keys are exactly the backend's accepted set (`FIELDS` in
`modules/ai_backend/inpaint/flux3_klein/wire.py`), which refuses any other key by name; a test
pins the set. The struct does not use `deny_unknown_fields`, so a document written by a newer
build loads with the fields this one knows.
*/

use super::*;

/// Where the model package comes from.
///
/// MODES, never mixed: in `Download` the root is DERIVED (`config::flux3_klein_dir`) and the
/// manual field is never read or written, so a hand-picked package survives a download and
/// flipping back restores it exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux3SourceMode {
    /// The program downloads the package from Hugging Face into the models directory.
    Download,
    /// The user points at a package folder of his own.
    Manual,
}

impl Flux3SourceMode {
    /// Stable token persisted in the settings file.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Manual => "manual",
        }
    }

    /// Reads a persisted token; anything unrecognized (an absent field included) is the
    /// default, `Download`. Unlike FLUX.2 klein there is no older document whose hand-entered
    /// paths an absent field would have to protect.
    pub(super) fn from_wire(value: &str) -> Self {
        match value.trim() {
            "manual" => Self::Manual,
            _ => Self::Download,
        }
    }

    /// The localized caption of this mode's toggle button.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Download => t!("cleaning.tools.flux3_klein.source_mode_download"),
            Self::Manual => t!("cleaning.tools.flux3_klein.source_mode_manual"),
        }
    }

    /// The hover explaining what this mode does.
    pub(super) fn hint(self) -> &'static str {
        match self {
            Self::Download => t!("cleaning.tools.flux3_klein.source_mode_download_hint"),
            Self::Manual => t!("cleaning.tools.flux3_klein.source_mode_manual_hint"),
        }
    }

    /// Both modes in switch order.
    pub(super) fn all() -> [Self; 2] {
        [Self::Download, Self::Manual]
    }
}

/// How the DiT weights are stored (`WeightFormat`); the wire value is also what the download
/// fetches and what the forecast's format rows are called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux3Format {
    /// Full precision, the reference quality (11 GB DiT).
    Bf16,
    /// Row-wise fp8 (6.3 GB DiT), the default: near-reference quality at about half the size.
    Fp8r,
    /// 4-bit NVFP4 (5.3 GB DiT), the smallest.
    Nvfp4,
}

impl Flux3Format {
    /// The default weight format (`DEFAULT_FORMAT`).
    pub(super) const DEFAULT: Self = Self::Fp8r;

    /// Wire / persisted token.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::Bf16 => "bf16",
            Self::Fp8r => "fp8r",
            Self::Nvfp4 => "nvfp4",
        }
    }

    /// Reads a token; `None` for an unknown one.
    pub(super) fn from_wire(value: &str) -> Option<Self> {
        match value.trim() {
            "bf16" => Some(Self::Bf16),
            "fp8r" => Some(Self::Fp8r),
            "nvfp4" => Some(Self::Nvfp4),
            _ => None,
        }
    }

    /// The localized picker caption.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Bf16 => t!("cleaning.tools.flux3_klein.format_bf16_label"),
            Self::Fp8r => t!("cleaning.tools.flux3_klein.format_fp8r_label"),
            Self::Nvfp4 => t!("cleaning.tools.flux3_klein.format_nvfp4_label"),
        }
    }

    /// The localized hover of one picker row.
    pub(super) fn hint(self) -> &'static str {
        match self {
            Self::Bf16 => t!("cleaning.tools.flux3_klein.format_bf16_hint"),
            Self::Fp8r => t!("cleaning.tools.flux3_klein.format_fp8r_hint"),
            Self::Nvfp4 => t!("cleaning.tools.flux3_klein.format_nvfp4_hint"),
        }
    }

    /// Every format, in picker order.
    pub(super) fn all() -> [Self; 3] {
        [Self::Bf16, Self::Fp8r, Self::Nvfp4]
    }
}

/// The fp8r matmul path (`Fp8Compute`): meaningful for `Fp8r` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux3Fp8Compute {
    /// The backend probes the card and picks.
    Auto,
    /// Row-wise `_scaled_mm` (fast, needs hardware fp8); refused before load where it cannot run.
    ScaledMm,
    /// Dequantize to bf16 per forward (works everywhere, slower).
    Dequant,
}

impl Flux3Fp8Compute {
    /// Wire / persisted token.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ScaledMm => "scaled_mm",
            Self::Dequant => "dequant",
        }
    }

    /// Reads a token; anything unknown is `Auto`.
    pub(super) fn from_wire(value: &str) -> Self {
        match value.trim() {
            "scaled_mm" => Self::ScaledMm,
            "dequant" => Self::Dequant,
            _ => Self::Auto,
        }
    }

    /// The localized picker caption.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Auto => t!("cleaning.tools.flux3_klein.fp8_compute_auto"),
            Self::ScaledMm => t!("cleaning.tools.flux3_klein.fp8_compute_scaled_mm"),
            Self::Dequant => t!("cleaning.tools.flux3_klein.fp8_compute_dequant"),
        }
    }

    /// Every path, in picker order.
    pub(super) fn all() -> [Self; 3] {
        [Self::Auto, Self::ScaledMm, Self::Dequant]
    }
}

/// The «Ограничить референсы» choices: the longest-side budget of every reference as a square
/// side in pixels (`reference_max_pixels = side²`), `0` = no cap. Any other persisted value
/// normalizes to `0`.
pub(super) const FLUX3_REFERENCE_CAP_SIDES: [u32; 4] = [0, 512, 768, 1024];

/// Everything the engine persists to `config::flux3_klein_settings_path`.
///
/// `#[serde(default)]` so a document written by another build keeps loading; the wire value is
/// always `normalized()`, never this struct as edited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Flux3KleinSettings {
    /// `Flux3SourceMode::wire()`.
    pub(super) source_mode: String,
    /// The package folder of MANUAL mode (`model.safetensors` / `variants/<format>/`,
    /// `ae.safetensors`, `text_encoder/`). Never read or written in download mode.
    pub(super) model_root: String,
    /// `Flux3Format::wire()`: the DiT the run loads AND the one the download fetches.
    pub(super) format: String,
    /// The prompt, sent as typed (trimmed). Never legitimately empty: the default fills a new
    /// and a blank document.
    pub(super) prompt: String,
    /// Flow-map steps, 1..=64.
    pub(super) steps: u32,
    /// A pinned seed, `None` = a fresh random one per run (the backend draws it and echoes it).
    pub(super) seed: Option<u64>,
    /// «Референсы и в текстовый энкодер»: `encoder_mode = vl_refs` (Qwen3-VL also sees the
    /// reference images); off = `text`, the reference behaviour.
    pub(super) vl_refs: bool,
    /// «Автоэнкодер в bfloat16»: `ae_dtype = bfloat16`; off = `float32`, the reference.
    pub(super) ae_bf16: bool,
    /// `Flux3Fp8Compute::wire()`; sent as chosen only for the fp8r format.
    pub(super) fp8_compute: String,
    /// One of [`FLUX3_REFERENCE_CAP_SIDES`]; `0` = no cap.
    pub(super) reference_max_side: u32,
    /// Feather ramp of the composite, pixels inwards from the mask contour, 0..=32.
    pub(super) mask_feather_px: u32,
    /// Match the generated colours to the ring outside the mask (ineffective under a whole-region run).
    pub(super) color_match: bool,
    /// «Сила локальной подгонки», percent 0..=100: the low-frequency Lab mean/contrast match
    /// inside the mask the backend applies AFTER the global one; `0` = global match only.
    /// Effective only with `color_match` (and, like it, never under a whole-region run).
    /// Default 60, also for a document without the field.
    pub(super) color_match_local_pct: u32,
    /// «Масштаб локальной подгонки»: the Gaussian sigma, pixels, of the finest
    /// level of that local match's averaging (its blur radius), 8..=96.
    /// Default 24, also for a document without the field.
    pub(super) color_match_scale_px: u32,
    /// «Видеопамять»: the `VramStep` wire id; an unknown id reads as «Авто».
    pub(super) vram_step: String,
    /// «Оперативная память»: the `RamStep` wire id; an unknown id reads as «Авто».
    pub(super) ram_step: String,
    /// «Энкодер на CPU»: the text encoder runs on the CPU from RAM at every VRAM step. Default
    /// `true`, also for a document without the field. Sent in every `params`.
    pub(super) encoder_on_cpu: bool,
}

impl Default for Flux3KleinSettings {
    fn default() -> Self {
        Self {
            // Download: a fresh install has no package anywhere, and the download block is the
            // shortest way to one.
            source_mode: Flux3SourceMode::Download.wire().to_string(),
            model_root: String::new(),
            format: Flux3Format::DEFAULT.wire().to_string(),
            prompt: FLUX3_DEFAULT_PROMPT.to_string(),
            steps: FLUX3_DEFAULT_STEPS,
            seed: None,
            vl_refs: false,
            ae_bf16: false,
            fp8_compute: Flux3Fp8Compute::Auto.wire().to_string(),
            reference_max_side: 0,
            mask_feather_px: FLUX3_DEFAULT_FEATHER,
            color_match: true,
            color_match_local_pct: FLUX3_DEFAULT_COLOR_MATCH_LOCAL_PCT,
            color_match_scale_px: FLUX3_DEFAULT_COLOR_MATCH_SCALE,
            vram_step: VramStep::Auto.wire().to_string(),
            ram_step: RamStep::Auto.wire().to_string(),
            encoder_on_cpu: true,
        }
    }
}

impl Flux3KleinSettings {
    /// A copy with every field forced into its supported range: unknown tokens fall back to
    /// their defaults, numbers are clamped, text is trimmed. The ONLY value put on the wire.
    #[must_use]
    pub(super) fn normalized(&self) -> Self {
        let choice = self.step_choice();
        Self {
            source_mode: self.source_mode().wire().to_string(),
            model_root: self.model_root.trim().to_string(),
            format: self.format().wire().to_string(),
            prompt: self.prompt.trim().to_string(),
            steps: self.steps.clamp(FLUX3_STEPS_MIN, FLUX3_STEPS_MAX),
            seed: self.seed,
            vl_refs: self.vl_refs,
            ae_bf16: self.ae_bf16,
            fp8_compute: Flux3Fp8Compute::from_wire(&self.fp8_compute).wire().to_string(),
            reference_max_side: if FLUX3_REFERENCE_CAP_SIDES.contains(&self.reference_max_side) { self.reference_max_side } else { 0 },
            mask_feather_px: self.mask_feather_px.min(FLUX3_FEATHER_MAX),
            color_match: self.color_match,
            color_match_local_pct: self.color_match_local_pct.min(FLUX3_COLOR_MATCH_LOCAL_PCT_MAX),
            color_match_scale_px: self.color_match_scale_px.clamp(FLUX3_COLOR_MATCH_SCALE_MIN, FLUX3_COLOR_MATCH_SCALE_MAX),
            vram_step: choice.vram.wire().to_string(),
            ram_step: choice.ram.wire().to_string(),
            encoder_on_cpu: self.encoder_on_cpu,
        }
    }

    /// The active source mode.
    pub(super) fn source_mode(&self) -> Flux3SourceMode {
        Flux3SourceMode::from_wire(&self.source_mode)
    }

    /// The selected weight format; an unknown token reads as the default.
    pub(super) fn format(&self) -> Flux3Format {
        Flux3Format::from_wire(&self.format).unwrap_or(Flux3Format::DEFAULT)
    }

    /// The persisted step pair, an unknown id reading as «Авто».
    pub(super) fn step_choice(&self) -> StepChoice {
        StepChoice {
            vram: VramStep::from_wire(self.vram_step.trim()).unwrap_or(VramStep::Auto),
            ram: RamStep::from_wire(self.ram_step.trim()).unwrap_or(RamStep::Auto),
        }
    }

    /// Stores `choice`; returns `true` when anything changed (a save is owed).
    pub(super) fn set_step_choice(&mut self, choice: StepChoice) -> bool {
        let changed = self.vram_step != choice.vram.wire() || self.ram_step != choice.ram.wire();
        self.vram_step = choice.vram.wire().to_string();
        self.ram_step = choice.ram.wire().to_string();
        changed
    }

    /// THE package root a call names (`params.source_root`), after the source mode.
    ///
    /// Download: the derived `config::flux3_klein_dir()`, which is where `.download.start`
    /// puts the files; Manual: the user's trimmed folder (empty until he picks one).
    #[must_use]
    pub(super) fn effective_root(&self) -> String {
        match self.source_mode() {
            Flux3SourceMode::Download => config::flux3_klein_dir().to_string_lossy().to_string(),
            Flux3SourceMode::Manual => self.model_root.trim().to_string(),
        }
    }

    /// `reference_max_pixels` on the wire: `None` without a cap.
    #[must_use]
    pub(super) fn reference_max_pixels(&self) -> Option<u64> {
        (self.reference_max_side > 0).then(|| u64::from(self.reference_max_side) * u64::from(self.reference_max_side))
    }

    /// The `params` object of a run (`whole_region` from `mask_for_run`) and — with
    /// `whole_region = false`, since no mask exists there — of a `.status` and a forecast.
    ///
    /// `self` must already be `normalized()`. Exactly the backend's accepted keys travel, and
    /// `fp8_compute` carries the choice only for the fp8r format (`auto` otherwise): the
    /// choice is meaningless for the other formats, and a pinned `scaled_mm` must not turn a
    /// bf16 run into a refusal.
    #[must_use]
    pub(super) fn to_params(&self, whole_region: bool) -> Value {
        let format = self.format();
        let fp8_compute = match format {
            Flux3Format::Fp8r => Flux3Fp8Compute::from_wire(&self.fp8_compute),
            Flux3Format::Bf16 | Flux3Format::Nvfp4 => Flux3Fp8Compute::Auto,
        };
        json!({
            "source_root": self.effective_root(),
            "format": format.wire(),
            "encoder_mode": if self.vl_refs { "vl_refs" } else { "text" },
            "ae_dtype": if self.ae_bf16 { "bfloat16" } else { "float32" },
            "fp8_compute": fp8_compute.wire(),
            "reference_max_pixels": self.reference_max_pixels(),
            "prompt": self.prompt,
            "steps": self.steps,
            "seed": self.seed,
            "vram_step": self.vram_step,
            "ram_step": self.ram_step,
            "encoder_on_cpu": self.encoder_on_cpu,
            "mask_feather_px": self.mask_feather_px,
            "color_match": self.color_match,
            // Sent as stored, like `color_match`: the backend settles both under `whole_region`
            // and applies the local match only under the global one.
            "color_match_local_pct": self.color_match_local_pct,
            "color_match_scale_px": self.color_match_scale_px,
            "whole_region": whole_region,
        })
    }
}

/// The outcome of the settings load the engine applies on the GUI thread.
#[derive(Debug, Clone)]
pub(super) struct Flux3SettingsLoad {
    pub(super) settings: Flux3KleinSettings,
    pub(super) problem: Option<SettingsProblem>,
}

impl Flux3SettingsLoad {
    /// Whether the engine must not write the settings file this session.
    pub(super) fn save_blocked(&self) -> bool {
        matches!(self.problem, Some(SettingsProblem::SaveBlocked(_)))
    }
}

/// Reads the settings file. Runs on a worker thread.
pub(super) fn load_flux3_settings() -> Flux3SettingsLoad {
    load_flux3_settings_from(&config::flux3_klein_settings_path(), unix_now_secs())
}

/// Reads the settings file at `path` under the shared rules (`read_settings_file`): missing ->
/// defaults; damaged -> moved aside to `<stem>.malformed-<unix_secs>[-<n>].json` and defaults;
/// unreadable or unmovable -> defaults with saving BLOCKED. The problem is reported for the
/// panel either way.
pub(super) fn load_flux3_settings_from(path: &Path, unix_secs: u64) -> Flux3SettingsLoad {
    let read = read_settings_file(path, unix_secs, FLUX3_LOG_LABEL, settings_from_json);
    Flux3SettingsLoad { settings: read.document.unwrap_or_default(), problem: read.problem }
}

/// Deserializes a settings document; a blank prompt is replaced by the default one (an empty
/// prompt only blocks the run).
///
/// # Errors
/// A `serde_json` error when the value is not a JSON object or a field has the wrong type:
/// the loader then moves the file aside instead of overwriting it.
pub(super) fn settings_from_json(value: &Value) -> Result<Flux3KleinSettings, serde_json::Error> {
    // serde also builds a struct from a JSON ARRAY (field by position), which would read a
    // document of another shape as settings; only an object is one.
    if !value.is_object() {
        return Err(<serde_json::Error as serde::de::Error>::custom("the settings document is not a JSON object"));
    }
    let mut settings: Flux3KleinSettings = serde_json::from_value(value.clone())?;
    if settings.prompt.trim().is_empty() {
        settings.prompt = FLUX3_DEFAULT_PROMPT.to_string();
    }
    Ok(settings)
}

/// Writes `settings` to the settings file (worker thread).
///
/// # Errors
/// A user-facing message when the directory cannot be created, the settings cannot be
/// serialized, or the write fails.
pub(super) fn save_flux3_settings(settings: &Flux3KleinSettings) -> Result<(), String> {
    write_settings_json(&config::flux3_klein_settings_path(), settings).map_err(|err| match err {
        SettingsWriteError::CreateDir(err) => tf!("cleaning.settings_io.create_dir_error", err = err),
        SettingsWriteError::Serialize(err) => tf!("cleaning.tools.flux3_klein.serialize_settings_error", err = err),
        SettingsWriteError::Write(err) => tf!("cleaning.tools.flux3_klein.write_settings_error", err = err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults are the package's: fp8r, 16 steps, no guidance, a random seed, the
    /// encoder on the CPU, «Авто» on both axes, the reference AE and encoder mode.
    #[test]
    fn the_defaults_match_the_package_and_the_binding_decisions() {
        let settings = Flux3KleinSettings::default();
        assert_eq!(settings.format(), Flux3Format::Fp8r);
        assert_eq!(settings.steps, 16);
        assert_eq!(settings.seed, None);
        assert!(settings.encoder_on_cpu);
        assert!(!settings.vl_refs && !settings.ae_bf16);
        assert_eq!(settings.step_choice(), StepChoice { vram: VramStep::Auto, ram: RamStep::Auto });
        assert_eq!(settings.source_mode(), Flux3SourceMode::Download);
        assert_eq!(settings.mask_feather_px, 12);
        assert!(settings.color_match);
        assert_eq!((settings.color_match_local_pct, settings.color_match_scale_px), (60, 24));
        assert!(!settings.prompt.trim().is_empty());
        assert_eq!(settings.normalized(), settings, "the defaults are already normal");
    }

    /// An empty or foreign document loads with the defaults for every absent field — the
    /// encoder flag included — and a blank prompt is replaced; unknown keys are dropped.
    #[test]
    fn a_partial_document_loads_with_defaults_and_unknown_keys_are_dropped() {
        let loaded = settings_from_json(&json!({ "steps": 8, "prompt": "  ", "some_future_key": 1 })).expect("loads");
        assert_eq!(loaded.steps, 8);
        assert!(loaded.encoder_on_cpu, "a document without the flag keeps the encoder on the CPU");
        assert_eq!((loaded.color_match_local_pct, loaded.color_match_scale_px), (60, 24), "a document without the local match reads its defaults");
        assert_eq!(loaded.prompt, FLUX3_DEFAULT_PROMPT);
        assert_eq!(loaded.format, "fp8r");
        assert!(settings_from_json(&json!(["not", "an", "object"])).is_err());
        assert!(settings_from_json(&json!({ "steps": "many" })).is_err(), "a wrong type is a damaged file");
    }

    /// `normalized()` clamps every range and maps every unknown token to its default.
    #[test]
    fn normalized_clamps_every_range() {
        let wild = Flux3KleinSettings {
            source_mode: "elsewhere".to_string(),
            model_root: "  /m/flux3  ".to_string(),
            format: "int2".to_string(),
            prompt: "  edit  ".to_string(),
            steps: 500,
            fp8_compute: "magic".to_string(),
            reference_max_side: 333,
            mask_feather_px: 99,
            color_match_local_pct: 101,
            color_match_scale_px: 97,
            vram_step: "warp".to_string(),
            ram_step: "".to_string(),
            ..Flux3KleinSettings::default()
        }
        .normalized();
        assert_eq!(wild.source_mode, "download");
        assert_eq!(wild.model_root, "/m/flux3");
        assert_eq!(wild.format, "fp8r");
        assert_eq!(wild.prompt, "edit");
        assert_eq!(wild.steps, FLUX3_STEPS_MAX);
        assert_eq!(wild.fp8_compute, "auto");
        assert_eq!(wild.reference_max_side, 0);
        assert_eq!(wild.mask_feather_px, FLUX3_FEATHER_MAX);
        assert_eq!(wild.color_match_local_pct, FLUX3_COLOR_MATCH_LOCAL_PCT_MAX);
        assert_eq!(wild.color_match_scale_px, FLUX3_COLOR_MATCH_SCALE_MAX);
        let small = Flux3KleinSettings { color_match_local_pct: 0, color_match_scale_px: 0, ..Flux3KleinSettings::default() }.normalized();
        assert_eq!(small.color_match_local_pct, 0, "0 % is a valid value: the global match only");
        assert_eq!(small.color_match_scale_px, FLUX3_COLOR_MATCH_SCALE_MIN);
        assert_eq!((wild.vram_step.as_str(), wild.ram_step.as_str()), ("auto", "auto"));
        assert_eq!(Flux3KleinSettings { steps: 0, ..Flux3KleinSettings::default() }.normalized().steps, FLUX3_STEPS_MIN);
    }

    /// The params carry EXACTLY the backend's accepted keys (`FIELDS` in
    /// `modules/ai_backend/inpaint/flux3_klein/wire.py`, which refuses any other by name) with
    /// the types it checks: an extra key would turn every call into a refusal.
    #[test]
    fn the_params_carry_exactly_the_backend_fields() {
        const BACKEND_FIELDS: [&str; 17] = [
            "source_root", "format", "encoder_mode", "ae_dtype", "fp8_compute", "reference_max_pixels", "prompt", "steps", "seed",
            "vram_step", "ram_step", "encoder_on_cpu", "mask_feather_px", "color_match", "color_match_local_pct",
            "color_match_scale_px", "whole_region",
        ];
        let params = Flux3KleinSettings::default().normalized().to_params(false);
        let mut keys: Vec<&str> = params.as_object().expect("an object").keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = BACKEND_FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(params["encoder_on_cpu"], json!(true));
        assert_eq!(params["format"], json!("fp8r"));
        assert_eq!(params["encoder_mode"], json!("text"));
        assert_eq!(params["ae_dtype"], json!("float32"));
        assert_eq!(params["seed"], Value::Null, "no pinned seed: the backend draws one");
        assert_eq!(params["reference_max_pixels"], Value::Null);
        assert_eq!(params["whole_region"], json!(false));
        assert_eq!(params["steps"], json!(16));
        assert_eq!(params["color_match_local_pct"], json!(60));
        assert_eq!(params["color_match_scale_px"], json!(24));
    }

    /// Each expert switch reaches its wire value; the seed travels as a number when pinned.
    #[test]
    fn the_expert_switches_and_the_seed_reach_the_wire() {
        let settings = Flux3KleinSettings {
            format: "fp8r".to_string(),
            vl_refs: true,
            ae_bf16: true,
            fp8_compute: "dequant".to_string(),
            reference_max_side: 768,
            seed: Some(u64::MAX),
            encoder_on_cpu: false,
            color_match_local_pct: 35,
            color_match_scale_px: 48,
            ..Flux3KleinSettings::default()
        }
        .normalized();
        let params = settings.to_params(true);
        assert_eq!(params["color_match_local_pct"], json!(35), "sent as stored under whole_region too");
        assert_eq!(params["color_match_scale_px"], json!(48));
        assert_eq!(params["encoder_mode"], json!("vl_refs"));
        assert_eq!(params["ae_dtype"], json!("bfloat16"));
        assert_eq!(params["fp8_compute"], json!("dequant"));
        assert_eq!(params["reference_max_pixels"], json!(768 * 768));
        assert_eq!(params["seed"], json!(u64::MAX), "the whole 0..2^64-1 range travels");
        assert_eq!(params["encoder_on_cpu"], json!(false));
        assert_eq!(params["whole_region"], json!(true));
        // The fp8 path is the fp8r format's alone: a pinned choice never reaches another format.
        let bf16 = Flux3KleinSettings { format: "bf16".to_string(), fp8_compute: "scaled_mm".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(bf16.normalized().to_params(false)["fp8_compute"], json!("auto"));
        assert_eq!(bf16.normalized().fp8_compute, "scaled_mm", "the stored choice survives for when fp8r comes back");
    }

    /// The root follows the mode: derived in download mode (the manual field is never read),
    /// the user's own folder in manual mode.
    #[test]
    fn the_root_follows_the_source_mode() {
        let mut settings = Flux3KleinSettings { model_root: "/home/u/flux.3-klein".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(settings.effective_root(), config::flux3_klein_dir().to_string_lossy());
        settings.source_mode = Flux3SourceMode::Manual.wire().to_string();
        assert_eq!(settings.effective_root(), "/home/u/flux.3-klein");
        assert_eq!(settings.normalized().to_params(false)["source_root"], json!("/home/u/flux.3-klein"));
    }

    /// A round trip through the file shape keeps every field, the optional seed included.
    #[test]
    fn the_document_round_trips() {
        let settings = Flux3KleinSettings { seed: Some(42), vl_refs: true, reference_max_side: 512, color_match_local_pct: 0, color_match_scale_px: 96, ..Flux3KleinSettings::default() };
        let text = serde_json::to_string(&settings).expect("serializes");
        let back = settings_from_json(&serde_json::from_str(&text).expect("json")).expect("loads");
        assert_eq!(back, settings);
    }

    /// A damaged settings file is moved aside and the defaults load; the copy keeps its bytes.
    #[test]
    fn a_damaged_settings_file_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("ms_flux3_settings_{}_{}", std::process::id(), unix_now_secs()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("flux3_klein_settings.json");
        std::fs::write(&path, b"{ not json").expect("write");
        let load = load_flux3_settings_from(&path, 1_700_000_000);
        assert_eq!(load.settings, Flux3KleinSettings::default());
        let Some(SettingsProblem::MovedAside(aside)) = load.problem.clone() else {
            panic!("expected the file to be moved aside, got {:?}", load.problem);
        };
        assert!(!load.save_blocked());
        assert_eq!(std::fs::read(&aside).expect("aside copy"), b"{ not json");
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}
