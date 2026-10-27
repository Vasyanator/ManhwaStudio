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
  modes), which weight formats are selected (`dit_format` / `encoder_format`, vocabularies
  `ms_config::{Flux3DitFormat, Flux3EncoderFormat}`) and which step pair is persisted
  (`step_choice` / `set_step_choice`);
- build the `params` of a run, a `.status`, a forecast and a `.prompt_cache.*` call
  (`to_params`, with every offered format candidate and its absolute path under the root, the
  prompt-library id and the precision order); the «Постобработка» settings (`postprocess`,
  the shared `ms_region_postprocess::PostprocessSettings`) and the «Нет» stash
  (`uncached_prompt`) are persisted here but never sent;
- answer which encoder mode a call actually sends (`text_mode_in_effect`: the prompt cache
  serves TEXT mode only);
- load and save the settings file on worker threads (`load_flux3_settings`,
  `save_flux3_settings`).

Key structures:
- `Flux3KleinSettings`, `Flux3SourceMode`, `Flux3Fp8Compute`, `Flux3SettingsLoad`

Key functions:
- `Flux3KleinSettings::{normalized, effective_root, dit_format, encoder_format,
  text_mode_in_effect, to_params, step_choice, set_step_choice}`, `package_path()`
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
    /// The app's model catalog downloads the package into the models directory.
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
    /// `ae.safetensors`, `text_encoder/`, and a GGUF file at its catalog path under it,
    /// `transformer_gguf/` / `text_encoder_gguf/`). Never read or written in download mode.
    pub(super) model_root: String,
    /// `Flux3DitFormat::wire()`: the DiT the run loads AND the one the catalog bundle fetches.
    /// The native ids are the ones every earlier build persisted.
    pub(super) format: String,
    /// `Flux3EncoderFormat::wire()`: the text encoder the run loads and the bundle fetches.
    /// Absent in a document of an earlier build: reads as `bf16` (the shards it described).
    /// A GGUF encoder excludes `vl_refs` (D6): `normalized()` sends `text` while one is selected.
    pub(super) text_encoder_format: String,
    /// The prompt, sent as typed (trimmed). Never legitimately empty: the default fills a new
    /// and a blank document.
    pub(super) prompt: String,
    /// The «Нет» stash of the prompt library: the last prompt no preset holds, kept verbatim
    /// (`track_uncached`) so «Нет» can bring it back. Persisted, NEVER sent; absent in a
    /// document of an earlier build (reads as empty).
    pub(super) uncached_prompt: String,
    /// Flow-map steps, 1..=64.
    pub(super) steps: u32,
    /// A pinned seed, `None` = a fresh random one per run (the backend draws it and echoes it).
    pub(super) seed: Option<u64>,
    /// «Референсы и в текстовый энкодер»: `encoder_mode = vl_refs` (Qwen3-VL also sees the
    /// reference images); off = `text`, the reference behaviour. Only the bf16 encoder has the
    /// vision tower: with a GGUF encoder the stored choice is kept but `text` is sent.
    pub(super) vl_refs: bool,
    /// «Автоэнкодер в bfloat16»: `ae_dtype = bfloat16`; off = `float32`, the reference.
    pub(super) ae_bf16: bool,
    /// `Flux3Fp8Compute::wire()`; sent as chosen only for the fp8r format.
    pub(super) fp8_compute: String,
    /// One of [`FLUX3_REFERENCE_CAP_SIDES`]; `0` = no cap.
    pub(super) reference_max_side: u32,
    /// «Постобработка» — feather and colour match — that the HOST applies to the raw run answer
    /// (`AiEngine::postprocess_settings`); never on the wire. Flattened, so its four keys
    /// (`mask_feather_px`, `color_match`, `color_match_local_pct`, `color_match_scale_px`) stay
    /// top-level in the file exactly as before they moved into the shared struct; an absent
    /// key reads as the crate's default.
    #[serde(flatten)]
    pub(super) postprocess: PostprocessSettings,
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
            format: Flux3DitFormat::default().wire().to_string(),
            text_encoder_format: Flux3EncoderFormat::default().wire().to_string(),
            prompt: FLUX3_DEFAULT_PROMPT.to_string(),
            uncached_prompt: String::new(),
            steps: FLUX3_DEFAULT_STEPS,
            seed: None,
            vl_refs: false,
            ae_bf16: false,
            fp8_compute: Flux3Fp8Compute::Auto.wire().to_string(),
            reference_max_side: 0,
            postprocess: PostprocessSettings::default(),
            vram_step: VramStep::Auto.wire().to_string(),
            ram_step: RamStep::Auto.wire().to_string(),
            encoder_on_cpu: true,
        }
    }
}

impl Flux3KleinSettings {
    /// A copy with every field forced into its supported range: unknown tokens fall back to
    /// their defaults, numbers are clamped, text is trimmed, and `vl_refs` is off while a GGUF
    /// encoder is selected (D6: the GGUF file has no vision tower; the backend refuses the pair
    /// by name). The ONLY value put on the wire; never written back into the stored settings,
    /// so the stored `vl_refs` returns with the bf16 encoder.
    #[must_use]
    pub(super) fn normalized(&self) -> Self {
        let choice = self.step_choice();
        Self {
            source_mode: self.source_mode().wire().to_string(),
            model_root: self.model_root.trim().to_string(),
            format: self.dit_format().wire().to_string(),
            text_encoder_format: self.encoder_format().wire().to_string(),
            prompt: self.prompt.trim().to_string(),
            // Verbatim: the stash is compared with the typed text, never sent.
            uncached_prompt: self.uncached_prompt.clone(),
            steps: self.steps.clamp(FLUX3_STEPS_MIN, FLUX3_STEPS_MAX),
            seed: self.seed,
            vl_refs: self.vl_refs && !self.encoder_format().is_gguf(),
            ae_bf16: self.ae_bf16,
            fp8_compute: Flux3Fp8Compute::from_wire(&self.fp8_compute).wire().to_string(),
            reference_max_side: if FLUX3_REFERENCE_CAP_SIDES.contains(&self.reference_max_side) { self.reference_max_side } else { 0 },
            postprocess: self.postprocess.normalized(),
            vram_step: choice.vram.wire().to_string(),
            ram_step: choice.ram.wire().to_string(),
            encoder_on_cpu: self.encoder_on_cpu,
        }
    }

    /// The active source mode.
    pub(super) fn source_mode(&self) -> Flux3SourceMode {
        Flux3SourceMode::from_wire(&self.source_mode)
    }

    /// The selected DiT weight format; an unknown token reads as the default (`fp8r`).
    pub(super) fn dit_format(&self) -> Flux3DitFormat {
        Flux3DitFormat::from_wire(&self.format).unwrap_or_default()
    }

    /// The selected text-encoder weight format; an unknown or absent token reads as the
    /// default (`bf16`).
    pub(super) fn encoder_format(&self) -> Flux3EncoderFormat {
        Flux3EncoderFormat::from_wire(&self.text_encoder_format).unwrap_or_default()
    }

    /// Whether a call sends `encoder_mode = text` — the mode [`Self::normalized`] puts on the
    /// wire: «Референсы и в текстовый энкодер» off, or on with a GGUF encoder (D6). The prompt
    /// cache serves this mode only; in `vl_refs` the encoder also reads the images, so every run
    /// encodes and nothing is cached.
    #[must_use]
    pub(super) fn text_mode_in_effect(&self) -> bool {
        !self.vl_refs || self.encoder_format().is_gguf()
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
    /// Download: the derived `config::flux3_klein_dir()`, which is where the catalog bundle puts
    /// the files (its `Shared` dirs mirror it, pinned by a catalog test); Manual: the user's
    /// trimmed folder (empty until he picks one).
    #[must_use]
    pub(super) fn effective_root(&self) -> String {
        match self.source_mode() {
            Flux3SourceMode::Download => config::flux3_klein_dir().to_string_lossy().to_string(),
            Flux3SourceMode::Manual => self.model_root.trim().to_string(),
        }
    }

    /// Whether [`Self::effective_root`] may be sent to the backend: a non-empty ABSOLUTE path on
    /// this OS. The backend refuses a relative candidate path for the whole call (`wire._absolute`),
    /// so a relative or half-typed manual folder is decided locally instead
    /// (`Flux3ModelReadiness::RootNotAbsolute`) and no `.status` / forecast / run is asked.
    #[must_use]
    pub(super) fn has_usable_root(&self) -> bool {
        std::path::Path::new(&self.effective_root()).is_absolute()
    }

    /// `reference_max_pixels` on the wire: `None` without a cap.
    #[must_use]
    pub(super) fn reference_max_pixels(&self) -> Option<u64> {
        (self.reference_max_side > 0).then(|| u64::from(self.reference_max_side) * u64::from(self.reference_max_side))
    }

    /// The `params` object of a run (`whole_region` from `mask_for_run`) and — with
    /// `whole_region = false`, since no mask exists there — of a `.status` and a forecast.
    ///
    /// `self` must already be `normalized()`. Exactly the backend's accepted keys travel (the
    /// post-process settings are the host's and never do; the backend refuses them), and
    /// `fp8_compute` carries the choice only for the fp8r format (`auto` otherwise): the
    /// choice is meaningless for the other formats, and a pinned `scaled_mm` must not turn a
    /// bf16 run into a refusal.
    ///
    /// The format candidates (protocol 24, plan D7): `dit_formats` lists EVERY DiT format in
    /// quality order (`Flux3DitFormat::all`), a native id without a path (the backend locates
    /// its files under `source_root`), a GGUF id with the absolute path of its catalog file
    /// under the root; `text_encoder_formats` lists every GGUF encoder the same way (`bf16` is
    /// the root's `text_encoder/` and needs no entry). Sent in both source modes, whether the
    /// files exist or not: `.status` answers per-format presence from them. With an empty root
    /// (manual mode before a folder is picked) the paths are not absolute and the backend
    /// refuses them, so the engine asks nothing in that state.
    ///
    /// The prompt library (protocol 24, the FLUX.2 klein shapes of protocol 23): every call
    /// names the model's one library (`prompt_library` = `FLUX3_PROMPT_LIBRARY_ID`) and the
    /// precision order its entries are preferred in (`flux3_prompt_cache_precision_order`, the
    /// encoder format ids); the backend serves a cached prompt in TEXT mode only.
    #[must_use]
    pub(super) fn to_params(&self, whole_region: bool) -> Value {
        let format = self.dit_format();
        let fp8_compute = match format {
            Flux3DitFormat::Fp8r => Flux3Fp8Compute::from_wire(&self.fp8_compute),
            Flux3DitFormat::Bf16 | Flux3DitFormat::Nvfp4 | Flux3DitFormat::Q80 | Flux3DitFormat::Q6K | Flux3DitFormat::Q5KM | Flux3DitFormat::Q4KM => Flux3Fp8Compute::Auto,
        };
        let root = self.effective_root();
        let dit_formats: Vec<Value> = Flux3DitFormat::all()
            .iter()
            .map(|&candidate| {
                if candidate.is_gguf() {
                    json!({ "id": candidate.wire(), "path": package_path(&root, flux3_klein_dit_relative_path(candidate)) })
                } else {
                    json!({ "id": candidate.wire() })
                }
            })
            .collect();
        let text_encoder_formats: Vec<Value> = Flux3EncoderFormat::all()
            .iter()
            .filter(|candidate| candidate.is_gguf())
            .map(|&candidate| json!({ "id": candidate.wire(), "path": package_path(&root, flux3_klein_encoder_relative_path(candidate)) }))
            .collect();
        json!({
            "source_root": root,
            "format": format.wire(),
            "dit_formats": dit_formats,
            "text_encoder_format": self.encoder_format().wire(),
            "text_encoder_formats": text_encoder_formats,
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
            "whole_region": whole_region,
            "prompt_library": FLUX3_PROMPT_LIBRARY_ID,
            "prompt_cache_precision_order": flux3_prompt_cache_precision_order(),
        })
    }
}

/// `relative` (a catalog path, `/`-separated) joined onto the package `root` component by
/// component, so the result uses the platform's separator, as a string for the wire.
#[must_use]
pub(super) fn package_path(root: &str, relative: &str) -> String {
    relative.split('/').fold(PathBuf::from(root), |path, component| path.join(component)).to_string_lossy().to_string()
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
///
/// A test build of this crate writes NOTHING and answers `Ok(())`: the path is the user's real
/// data directory, and the shared saver's teardown flush can reach this function from any test
/// that leaves a loaded, dirty engine behind. `Ok` rather than an error because the saver reads
/// `Ok` as "nothing is owed any more"; an error would show a failure and retry with backoff
/// for a write that must never happen. `cfg!(test)` alone is enough here (no runtime latch as
/// in `ms-tab-typing`'s `persistence_suppressed_by_tests`): this function is private to
/// `ms-tab-cleaning`, the engines are built only by this crate's `engines/mod.rs`, and every
/// test that reaches them is one of this crate's own. Tests of real writing use an injected
/// writer or `write_settings_json` on a temp path.
pub(super) fn save_flux3_settings(settings: &Flux3KleinSettings) -> Result<(), String> {
    if cfg!(test) {
        ms_log::runtime_log::log_info(format!("[cleaning] {FLUX3_LOG_LABEL} settings not written: a test build never writes the user's settings file"));
        return Ok(());
    }
    write_settings_json(&config::flux3_klein_settings_path(), settings).map_err(|err| match err {
        SettingsWriteError::CreateDir(err) => tf!("cleaning.settings_io.create_dir_error", err = err),
        SettingsWriteError::Serialize(err) => tf!("cleaning.tools.flux3_klein.serialize_settings_error", err = err),
        SettingsWriteError::Write(err) => tf!("cleaning.tools.flux3_klein.write_settings_error", err = err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test build never writes the user's real settings file (the saver's teardown flush
    /// can reach the product writer from any test): it answers `Ok` and leaves the file alone.
    #[test]
    fn a_test_build_never_writes_the_settings_file() {
        let path = config::flux3_klein_settings_path();
        let before = std::fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        assert!(save_flux3_settings(&Flux3KleinSettings::default()).is_ok());
        assert_eq!(std::fs::metadata(&path).and_then(|meta| meta.modified()).ok(), before);
    }

    /// The defaults are the package's: fp8r, 16 steps, no guidance, a random seed, the
    /// encoder on the CPU, «Авто» on both axes, the reference AE and encoder mode.
    #[test]
    fn the_defaults_match_the_package_and_the_binding_decisions() {
        let settings = Flux3KleinSettings::default();
        assert_eq!(settings.dit_format(), Flux3DitFormat::Fp8r);
        assert_eq!(settings.encoder_format(), Flux3EncoderFormat::Bf16);
        assert_eq!(settings.steps, 16);
        assert_eq!(settings.seed, None);
        assert!(settings.encoder_on_cpu);
        assert!(!settings.vl_refs && !settings.ae_bf16);
        assert_eq!(settings.step_choice(), StepChoice { vram: VramStep::Auto, ram: RamStep::Auto });
        assert_eq!(settings.source_mode(), Flux3SourceMode::Download);
        assert_eq!(settings.postprocess, PostprocessSettings::default(), "the crate owns the post-process defaults");
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
        assert_eq!(loaded.postprocess, PostprocessSettings::default(), "a document without the post-process keys reads their defaults");
        assert_eq!(loaded.prompt, FLUX3_DEFAULT_PROMPT);
        assert_eq!(loaded.format, "fp8r");
        assert_eq!(loaded.encoder_format(), Flux3EncoderFormat::Bf16, "a document without the encoder format describes the shards");
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
            text_encoder_format: "q2_k".to_string(),
            prompt: "  edit  ".to_string(),
            steps: 500,
            fp8_compute: "magic".to_string(),
            reference_max_side: 333,
            postprocess: PostprocessSettings { mask_feather_px: 99, color_match: false, color_match_local_pct: 101, color_match_scale_px: 97 },
            vram_step: "warp".to_string(),
            ram_step: "".to_string(),
            ..Flux3KleinSettings::default()
        }
        .normalized();
        assert_eq!(wild.source_mode, "download");
        assert_eq!(wild.model_root, "/m/flux3");
        assert_eq!(wild.format, "fp8r");
        assert_eq!(wild.text_encoder_format, "bf16");
        assert_eq!(wild.prompt, "edit");
        assert_eq!(wild.steps, FLUX3_STEPS_MAX);
        assert_eq!(wild.fp8_compute, "auto");
        assert_eq!(wild.reference_max_side, 0);
        // The ranges are the crate's; `normalized()` delegates to it.
        assert_eq!(
            wild.postprocess,
            PostprocessSettings { mask_feather_px: ms_region_postprocess::FEATHER_MAX_PX, color_match: false, color_match_local_pct: ms_region_postprocess::LOCAL_PCT_MAX, color_match_scale_px: ms_region_postprocess::SCALE_MAX_PX }
        );
        assert_eq!((wild.vram_step.as_str(), wild.ram_step.as_str()), ("auto", "auto"));
        assert_eq!(Flux3KleinSettings { steps: 0, ..Flux3KleinSettings::default() }.normalized().steps, FLUX3_STEPS_MIN);
    }

    /// The params carry EXACTLY the backend's accepted keys (`FIELDS` in
    /// `modules/ai_backend/inpaint/flux3_klein/wire.py`, which refuses any other by name) with
    /// the types it checks: an extra key would turn every call into a refusal.
    #[test]
    fn the_params_carry_exactly_the_backend_fields() {
        const BACKEND_FIELDS: [&str; 18] = [
            "source_root", "format", "dit_formats", "text_encoder_format", "text_encoder_formats", "encoder_mode", "ae_dtype", "fp8_compute",
            "reference_max_pixels", "prompt", "steps", "seed", "vram_step", "ram_step", "encoder_on_cpu", "whole_region",
            "prompt_library", "prompt_cache_precision_order",
        ];
        let params = Flux3KleinSettings::default().normalized().to_params(false);
        let mut keys: Vec<&str> = params.as_object().expect("an object").keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = BACKEND_FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(params["encoder_on_cpu"], json!(true));
        assert_eq!(params["format"], json!("fp8r"));
        assert_eq!(params["text_encoder_format"], json!("bf16"));
        assert_eq!(params["encoder_mode"], json!("text"));
        assert_eq!(params["ae_dtype"], json!("float32"));
        assert_eq!(params["seed"], Value::Null, "no pinned seed: the backend draws one");
        assert_eq!(params["reference_max_pixels"], Value::Null);
        assert_eq!(params["whole_region"], json!(false));
        assert_eq!(params["steps"], json!(16));
        assert_eq!(params["prompt_library"], json!("flux3_klein"));
        assert_eq!(params["prompt_cache_precision_order"], json!(["bf16", "q8_0", "q4_k_m"]));
    }

    /// The «Нет» stash is persisted verbatim and never sent; a document of an earlier build
    /// (no field) loads with an empty stash.
    #[test]
    fn the_uncached_prompt_is_persisted_and_never_sent() {
        let settings = Flux3KleinSettings { uncached_prompt: "  my own text \n".to_string(), ..Flux3KleinSettings::default() };
        let text = serde_json::to_string(&settings).expect("serializes");
        let back = settings_from_json(&serde_json::from_str(&text).expect("json")).expect("loads");
        assert_eq!(back.uncached_prompt, "  my own text \n", "kept verbatim");
        assert_eq!(settings.normalized().uncached_prompt, settings.uncached_prompt, "normalizing never touches the stash");
        let params = settings.normalized().to_params(false);
        assert!(params.get("uncached_prompt").is_none(), "the stash never travels");
        let old = settings_from_json(&json!({ "prompt": "edit", "steps": 8 })).expect("loads");
        assert!(old.uncached_prompt.is_empty());
    }

    /// TEXT mode is what the wire carries: «Референсы и в текстовый энкодер» off, or on with a
    /// GGUF encoder (normalized to `text`, D6).
    #[test]
    fn the_text_mode_in_effect_follows_the_normalized_encoder_mode() {
        let text = Flux3KleinSettings::default();
        assert!(text.text_mode_in_effect());
        let vl = Flux3KleinSettings { vl_refs: true, ..Flux3KleinSettings::default() };
        assert!(!vl.text_mode_in_effect());
        let gguf = Flux3KleinSettings { vl_refs: true, text_encoder_format: "q4_k_m".to_string(), ..Flux3KleinSettings::default() };
        assert!(gguf.text_mode_in_effect());
        for settings in [text, vl, gguf] {
            let sent = settings.normalized().to_params(false)["encoder_mode"].clone();
            assert_eq!(sent == json!("text"), settings.text_mode_in_effect(), "{settings:?}");
        }
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
            ..Flux3KleinSettings::default()
        }
        .normalized();
        let params = settings.to_params(true);
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
        let settings = Flux3KleinSettings { seed: Some(42), vl_refs: true, reference_max_side: 512, postprocess: PostprocessSettings { mask_feather_px: 3, color_match: false, color_match_local_pct: 0, color_match_scale_px: 96 }, ..Flux3KleinSettings::default() };
        let text = serde_json::to_string(&settings).expect("serializes");
        let back = settings_from_json(&serde_json::from_str(&text).expect("json")).expect("loads");
        assert_eq!(back, settings);
    }

    /// A file written before the post-process settings moved into the shared struct (the four
    /// keys flat at the top level) loads its values, and a save writes exactly the same key set
    /// as that build did: the flattened struct changed the code, not the document.
    #[test]
    fn an_old_document_keeps_its_post_process_values_and_its_key_set() {
        let old = json!({
            "source_mode": "manual", "model_root": "/m/flux3", "format": "bf16", "prompt": "edit", "steps": 8, "seed": 7,
            "vl_refs": true, "ae_bf16": false, "fp8_compute": "auto", "reference_max_side": 768,
            "mask_feather_px": 5, "color_match": false, "color_match_local_pct": 30, "color_match_scale_px": 40,
            "vram_step": "auto", "ram_step": "auto", "encoder_on_cpu": false,
        });
        let loaded = settings_from_json(&old).expect("loads");
        assert_eq!(loaded.postprocess, PostprocessSettings { mask_feather_px: 5, color_match: false, color_match_local_pct: 30, color_match_scale_px: 40 });
        assert_eq!((loaded.steps, loaded.reference_max_side, loaded.seed), (8, 768, Some(7)));
        assert_eq!(loaded.dit_format(), Flux3DitFormat::Bf16, "a native format id of an earlier build is read unchanged");
        assert_eq!(loaded.encoder_format(), Flux3EncoderFormat::Bf16, "no encoder format: the shards it described");
        let mut saved = serde_json::to_value(&loaded).expect("serializes");
        // The keys this build adds; everything else is written back exactly as it was read.
        let added = saved.as_object_mut().expect("an object").remove("text_encoder_format");
        assert_eq!(added, Some(json!("bf16")));
        let stash = saved.as_object_mut().expect("an object").remove("uncached_prompt");
        assert_eq!(stash, Some(json!("")), "an earlier document has no «Нет» stash");
        let mut saved_keys: Vec<&str> = saved.as_object().expect("an object").keys().map(String::as_str).collect();
        saved_keys.sort_unstable();
        let mut old_keys: Vec<&str> = old.as_object().expect("an object").keys().map(String::as_str).collect();
        old_keys.sort_unstable();
        assert_eq!(saved_keys, old_keys);
        assert_eq!(saved, old, "every value is written back where the old build read it");
    }

    /// Every DiT format travels as a candidate in quality order, a native one without a path, a
    /// GGUF one with the absolute path of its catalog file under the root; the encoder lists
    /// only its GGUF candidates. Both source modes, whatever is selected.
    #[test]
    fn every_format_candidate_travels_with_its_path_under_the_root() {
        let manual_root = std::env::temp_dir().join("ms_flux3_candidates_fictional_root").to_string_lossy().to_string();
        let download = Flux3KleinSettings::default();
        let manual = Flux3KleinSettings { source_mode: "manual".to_string(), model_root: manual_root.clone(), format: "q4_k_m".to_string(), ..Flux3KleinSettings::default() };
        for (settings, root) in [(download, config::flux3_klein_dir().to_string_lossy().to_string()), (manual, manual_root)] {
            let params = settings.normalized().to_params(false);
            let dit = params["dit_formats"].as_array().expect("a list");
            let ids: Vec<&str> = dit.iter().map(|entry| entry["id"].as_str().expect("an id")).collect();
            let expected: Vec<&str> = Flux3DitFormat::all().iter().map(|format| format.wire()).collect();
            assert_eq!(ids, expected, "every DiT format, best first");
            for (entry, &format) in dit.iter().zip(Flux3DitFormat::all()) {
                if format.is_gguf() {
                    let path = entry["path"].as_str().expect("a GGUF candidate carries its path");
                    assert_eq!(path, package_path(&root, flux3_klein_dit_relative_path(format)));
                    assert!(Path::new(path).is_absolute(), "{path}");
                    assert!(Path::new(path).starts_with(&root), "{path}");
                } else {
                    assert!(entry.get("path").is_none(), "a native id has no path: {entry}");
                }
            }
            let encoders = params["text_encoder_formats"].as_array().expect("a list");
            let ids: Vec<&str> = encoders.iter().map(|entry| entry["id"].as_str().expect("an id")).collect();
            assert_eq!(ids, ["q8_0", "q4_k_m"], "the GGUF encoders only");
            for entry in encoders {
                let format = Flux3EncoderFormat::from_wire(entry["id"].as_str().expect("an id")).expect("a known id");
                assert_eq!(entry["path"].as_str(), Some(package_path(&root, flux3_klein_encoder_relative_path(format)).as_str()));
            }
            assert_eq!(params["source_root"], json!(root));
        }
    }

    /// D6: a GGUF encoder has no vision tower, so `vl_refs` is sent as `text` while one is
    /// selected; the stored choice survives and returns with the bf16 encoder.
    #[test]
    fn a_gguf_encoder_sends_the_text_mode_and_keeps_the_stored_choice() {
        let gguf = Flux3KleinSettings { vl_refs: true, text_encoder_format: "q8_0".to_string(), ..Flux3KleinSettings::default() };
        let normalized = gguf.normalized();
        assert!(!normalized.vl_refs);
        let params = normalized.to_params(false);
        assert_eq!(params["encoder_mode"], json!("text"));
        assert_eq!(params["text_encoder_format"], json!("q8_0"));
        assert!(gguf.vl_refs, "the stored choice is untouched");
        let bf16 = Flux3KleinSettings { text_encoder_format: "bf16".to_string(), ..gguf };
        assert_eq!(bf16.normalized().to_params(false)["encoder_mode"], json!("vl_refs"));
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
