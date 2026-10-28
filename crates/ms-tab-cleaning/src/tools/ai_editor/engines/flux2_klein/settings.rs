/*
File: cleaning/tools/ai_editor/engines/flux2_klein/settings.rs

Purpose:
Everything the FLUX.2 klein engine persists to its VARIANT's settings file
(`config::flux2_klein_settings_path`) and the wire enums those fields are spelled in:
the dtype vocabulary, the two memory steps («Видеопамять» / «Оперативная память»), the
source mode that decides whether the three model paths are typed by hand or derived from
the download directory, the one-time migration of the retired memory placement, and the
load/save pair that moves the whole struct through a worker thread.

Main responsibilities:
- own `Flux2KleinSettings`, its defaults, its clamping (`normalized`) and the JSON
  body it puts on the wire (`params`, which also names the variant's shared prompt library,
  the precision order and — download mode only — the encoder source; the persisted «Нет»
  stash `uncached_prompt` never travels); the «Постобработка» values are the shared
  `ms_region_postprocess::PostprocessSettings`, embedded FLATTENED (`postprocess`) so the
  file keeps its historic flat keys, and they never reach the wire (the host applies them);
- own the dtype vocabulary and the persisted step ids (`step_choice`);
- migrate a document written before the step axes existed (`legacy_memory_steps`,
  which owns the placement -> step-pair table; the backend refuses those fields);
- derive the effective model paths from the source mode AND the variant, and in download
  mode (both variants come from the app's catalog) from the encoder source
  (`encoder_source`, persisted as the historic `uncensored_text_encoder` bool; a switch maps
  the encoder format through `closest_offered`, `set_encoder_source`), the two weight formats
  (`transformer_format` / `text_encoder_format`, `Flux2WeightFormat` wire ids; bf16 keeps the
  existing folders) and the probed bf16 transformer layout; build the `*_formats` candidate
  lists of `params` from the runtime `installed_formats` (`format_candidates`, filled by the
  worker probe `probe_flux2_installed_formats`);
- read the settings file, moving a damaged one aside instead of ever overwriting it
  (`load_flux2_settings_from`, over the shared `read_settings_file`), and write it back
  (`save_flux2_settings`, over the shared `write_settings_json`);
- persist the runtime switch (`runtime`: PyTorch / Stable-Diffusion.cpp, absent = PyTorch) and
  the engine's sd.cpp options (`sdcpp`: the shared `SdCppRunOptions`, never sent to the backend).

There is ONE FILE PER VARIANT and the document records which one it is: 9B keeps
`flux2_klein_settings.json` byte-for-byte, 4B gets its own name, `load_flux2_settings`
stamps the owning variant over whatever it read, and `save_flux2_settings` writes back to
the file the document itself names.

Key structures:
- `Flux2KleinSettings`, `Flux2EffectivePaths`, `Flux2Dtype`, `Flux2SourceMode`
- `Flux2InstalledFormats` (runtime only, never persisted), `Flux2WeightSelection`
- `Flux2LoadedDocument`, `Flux2SettingsLoad` (its `problem` is the shared `SettingsProblem`)

Key functions:
- `load_flux2_settings()`, `load_flux2_settings_from()`, `save_flux2_settings()`,
  `settings_from_json()`, `legacy_memory_steps()`
- `Flux2KleinSettings::{encoder_source, set_encoder_source, weight_format, format_candidates,
  effective_paths, normalized, runtime, set_runtime}`,
  `probe_flux2_installed_formats()` (worker only)
- `normalize_source_lang()`, `source_lang_title()`

Notes:
Items are `pub(super)` so the module root and the sibling submodules of `flux2_klein`
can reach them; `use super::*;` pulls in the module root's imports and the other
submodules' re-exported items. `normalized()` is the ONLY value ever put on the wire.
*/

use super::*;
use ms_config::{Flux2EncoderSource, Flux2WeightAxis, Flux2WeightFormat, flux2_prompt_cache_precision_order};
use ms_sysprobe::ai_models::external::{ExternalModelStatus, external_model_status};
use ms_sysprobe::ai_models::external_catalog::{Flux2SpecRole, flux2_klein_spec_role, flux2_klein_transformer_layout, flux2_klein_weights_path};

// ---------------------------------------------------------------------------------------
// Wire enums
// ---------------------------------------------------------------------------------------

/// Compute precision of the pipeline. Both names are technical identifiers and stay
/// literal in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2Dtype {
    Bfloat16,
    Float16,
}

impl Flux2Dtype {
    pub(super) fn wire(self) -> &'static str {
        match self {
            Flux2Dtype::Bfloat16 => "bfloat16",
            Flux2Dtype::Float16 => "float16",
        }
    }

    pub(super) fn from_wire(value: &str) -> Self {
        match value.trim() {
            "float16" => Flux2Dtype::Float16,
            _ => Flux2Dtype::Bfloat16,
        }
    }

    pub(super) fn all() -> [Self; 2] {
        [Flux2Dtype::Bfloat16, Flux2Dtype::Float16]
    }
}

/// The retired memory fields of a document written before the step axes, with the values
/// every such document was CREATED with. A document whose memory fields all still read as
/// these (absent or equal) was never touched by the user, which is what decides its
/// migration ([`legacy_memory_steps`]).
const LEGACY_DEFAULT_PLACEMENT: &str = "full_gpu";
const LEGACY_MEMORY_FLAGS_AND_DEFAULTS: [(&str, bool); 6] = [
    ("low_cpu_mem_usage", false),
    ("vae_tiling", false),
    ("vae_slicing", false),
    ("unload_transformer_before_vae", false),
    ("unload_text_encoder_after_encode", false),
    ("text_encoder_fp8", false),
];

/// The step pair a document written before the step axes migrates to, or `None` when it
/// needs no migration.
///
/// Trigger: the document carries NEITHER `vram_step` NOR `ram_step` but does carry a
/// `placement` string. A document that has either step field is never re-migrated and its
/// legacy keys are ignored, which is what makes the migration idempotent.
///
/// A document whose seven memory fields are all still the SHIPPED DEFAULTS — `placement`
/// `full_gpu` and each of the six flags absent or at its default — migrates to «Авто» /
/// «Авто»: such a file is indistinguishable from one the user never opened the memory
/// section of, and the strict (Максимум скорости, Держать модели в памяти) pair the table
/// gives `full_gpu` would refuse to run on a card it does not fit. Every other document
/// follows the placement mapping table, owned here (the backend refuses the retired fields
/// since protocol 14 and keeps no copy): `full_gpu` -> (max, keep); `encoder_cpu` -> (phase, keep) with
/// `unload_transformer_before_vae` on — its derived value `true` when absent — else
/// (max, keep); `model_cpu_offload` -> (phase, keep); `sequential_cpu_offload` -> (min, keep);
/// anything else -> (auto, auto).
#[must_use]
pub(super) fn legacy_memory_steps(value: &Value) -> Option<StepChoice> {
    if value.get("vram_step").is_some() || value.get("ram_step").is_some() {
        return None;
    }
    let placement = value.get("placement")?.as_str()?.trim();
    let flag = |name: &str| value.get(name).and_then(Value::as_bool);
    let untouched = placement == LEGACY_DEFAULT_PLACEMENT
        && LEGACY_MEMORY_FLAGS_AND_DEFAULTS
            .iter()
            .all(|(name, default)| value.get(*name).is_none() || flag(name) == Some(*default));
    if untouched {
        return Some(StepChoice::default());
    }
    let keep = |vram| StepChoice { vram, ram: RamStep::Keep };
    Some(match placement {
        "full_gpu" => keep(VramStep::Max),
        // The flag's legacy default followed the placement: `true` everywhere but `full_gpu`.
        "encoder_cpu" if flag("unload_transformer_before_vae").unwrap_or(true) => keep(VramStep::Phase),
        "encoder_cpu" => keep(VramStep::Max),
        "model_cpu_offload" => keep(VramStep::Phase),
        "sequential_cpu_offload" => keep(VramStep::Min),
        _ => StepChoice::default(),
    })
}

// ---------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------

/// Where the three model components come from: the user's own paths, or the copy this
/// panel downloads.
///
/// The two are MODES and never mix. In [`Flux2SourceMode::Download`] the three effective
/// paths are DERIVED from the models directory and the encoder toggle, and the manual
/// fields are never written to — a user who has hand-built a model tree keeps it, and
/// flipping back restores his configuration exactly. That is why a finished download does
/// NOT write its answer into the settings: derivation configures the engine continuously
/// instead of once, and a write-back would silently overwrite paths the user can switch
/// back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2SourceMode {
    /// The user points at his own model parts. The three path rows are drawn and their
    /// values are what a run uses.
    Manual,
    /// The program downloads the model. The path rows are not drawn, and the effective
    /// paths come from [`config::flux2_klein_dir`] and the encoder toggle.
    Download,
}

impl Flux2SourceMode {
    /// Stable token persisted in the engine's settings file.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Download => "download",
        }
    }

    /// Reads a persisted token, falling back to [`FLUX2_DEFAULT_SOURCE_MODE`] for anything
    /// unrecognized — INCLUDING an absent field, which is what every settings file written
    /// before this switch existed looks like. Those files carry hand-entered paths, so the
    /// fallback must be `Manual`: loading them as `Download` would replace a working
    /// configuration with an empty download block and read as a broken update.
    pub(super) fn from_wire(value: &str) -> Self {
        match value.trim() {
            "download" => Self::Download,
            "manual" => Self::Manual,
            _ => FLUX2_DEFAULT_SOURCE_MODE,
        }
    }

    /// The localized caption of this mode's toggle button.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Manual => t!("cleaning.tools.flux2_klein.source_mode_manual"),
            Self::Download => t!("cleaning.tools.flux2_klein.source_mode_download"),
        }
    }

    /// The hover explaining what choosing this mode does.
    pub(super) fn hint(self) -> &'static str {
        match self {
            Self::Manual => t!("cleaning.tools.flux2_klein.source_mode_manual_hint"),
            Self::Download => t!("cleaning.tools.flux2_klein.source_mode_download_hint"),
        }
    }

    /// Both modes in picker order.
    pub(super) fn all() -> [Self; 2] {
        [Self::Manual, Self::Download]
    }
}

/// The mode a settings file without the field loads as. See
/// [`Flux2SourceMode::from_wire`] for why it cannot be `Download`.
pub(super) const FLUX2_DEFAULT_SOURCE_MODE: Flux2SourceMode = Flux2SourceMode::Manual;

/// The three model paths a run actually uses, after the source mode has been applied.
///
/// THE single answer to "which three paths does this engine use". Everything that needs
/// them — the wire `params`, the run gate, the prompt-cache gates, the paths shown in the
/// download body — goes through [`Flux2KleinSettings::effective_paths`], because two places
/// computing it is exactly how the two modes drift apart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2EffectivePaths {
    pub(super) text_encoder: String,
    pub(super) transformer: String,
    pub(super) vae: String,
}

/// Which weight formats of each component have their catalog files installed, and which
/// bf16 transformer layout is in use, as the last worker probe saw them
/// ([`probe_flux2_installed_formats`]). Runtime state only: never persisted, and an unprobed
/// (default) value means "nothing known to be installed" with the `Primary` layout, which
/// leaves each candidate list holding just the selected format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2InstalledFormats {
    transformer: Vec<Flux2WeightFormat>,
    stock_text_encoder: Vec<Flux2WeightFormat>,
    uncensored_text_encoder: Vec<Flux2WeightFormat>,
    /// The bf16 transformer layout the catalog's one rule chose
    /// (`external_catalog::flux2_klein_transformer_layout`): `LegacyDiffusers` keeps a 9B
    /// install made by the backend's former download in use instead of refetching 18 GB.
    pub(super) transformer_layout: Flux2TransformerLayout,
}

impl Flux2InstalledFormats {
    /// Whether `format` of `axis` (from encoder `source`; the transformer ignores it) was
    /// found installed.
    #[must_use]
    pub(super) fn contains(&self, axis: Flux2WeightAxis, source: Flux2EncoderSource, format: Flux2WeightFormat) -> bool {
        self.formats(axis, source).contains(&format)
    }

    /// Marks `format` of `axis` (from encoder `source`; the transformer ignores it) as
    /// installed (idempotent).
    pub(super) fn insert(&mut self, axis: Flux2WeightAxis, source: Flux2EncoderSource, format: Flux2WeightFormat) {
        let formats = match (axis, source) {
            (Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock | Flux2EncoderSource::Uncensored) => &mut self.transformer,
            (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock) => &mut self.stock_text_encoder,
            (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored) => &mut self.uncensored_text_encoder,
        };
        if !formats.contains(&format) {
            formats.push(format);
        }
    }

    fn formats(&self, axis: Flux2WeightAxis, source: Flux2EncoderSource) -> &[Flux2WeightFormat] {
        match (axis, source) {
            (Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock | Flux2EncoderSource::Uncensored) => &self.transformer,
            (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock) => &self.stock_text_encoder,
            (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored) => &self.uncensored_text_encoder,
        }
    }
}

/// The catalog spec that holds `variant`'s component `axis` in `format` (from encoder
/// `source`) in the bf16 transformer `layout` — the spec `flux2_klein_package_specs` puts in the
/// bundle for that selection, found by its catalog role, so the legacy-layout rule is the
/// catalog's and is never restated here. `None` for a format that is not offered.
fn flux2_component_spec(
    variant: Flux2Variant,
    axis: Flux2WeightAxis,
    source: Flux2EncoderSource,
    format: Flux2WeightFormat,
    layout: Flux2TransformerLayout,
) -> Option<&'static ExternalModelSpec> {
    let (transformer, text_encoder) = match axis {
        Flux2WeightAxis::Transformer => (format, Flux2WeightFormat::Bf16),
        Flux2WeightAxis::TextEncoder => (Flux2WeightFormat::Bf16, format),
    };
    flux2_klein_package_specs(variant, transformer, layout, source, text_encoder)
        .into_iter()
        .find(|spec| match flux2_klein_spec_role(spec.id) {
            Some(Flux2SpecRole::Weights { axis: role_axis, format: role_format, .. }) => role_axis == axis && role_format == format,
            Some(Flux2SpecRole::LegacyTransformer { .. } | Flux2SpecRole::SingleFileTransformer { .. }) => axis == Flux2WeightAxis::Transformer,
            Some(Flux2SpecRole::Base { .. } | Flux2SpecRole::TextEncoderConfig { .. }) | None => false,
        })
}

/// Probes which offered weight formats of `variant` have their catalog spec INSTALLED
/// (`external_model_status` = `Installed`: marker present, every file of exact size) under
/// `side_models_root`, per component and per encoder source, and which bf16 transformer layout
/// is in use (`flux2_klein_transformer_layout`; the bf16 transformer counts as installed in
/// THAT layout). Stat-only, never hashes, but still filesystem I/O: WORKER THREADS ONLY.
#[must_use]
pub(super) fn probe_flux2_installed_formats(side_models_root: &Path, variant: Flux2Variant) -> Flux2InstalledFormats {
    let layout = flux2_klein_transformer_layout(side_models_root, variant);
    let mut installed = Flux2InstalledFormats { transformer_layout: layout, ..Flux2InstalledFormats::default() };
    // The transformer ignores the source: it is probed once, as `Stock`.
    let mut choices = vec![(Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock)];
    choices.extend(Flux2EncoderSource::offered_for(variant).iter().map(|&source| (Flux2WeightAxis::TextEncoder, source)));
    for (axis, source) in choices {
        for &format in Flux2WeightFormat::offered_for(variant, axis, source) {
            let Some(spec) = flux2_component_spec(variant, axis, source, format, layout) else {
                continue;
            };
            if external_model_status(side_models_root, spec) == ExternalModelStatus::Installed {
                installed.insert(axis, source, format);
            }
        }
    }
    installed
}

/// What the catalog bundle and the derived weight paths follow: the two weight formats in
/// effect and the encoder source. The engine compares it across a frame to notice a change
/// made anywhere in the panel (a picker, a format row, the uncensored toggle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Flux2WeightSelection {
    pub(super) transformer: Flux2WeightFormat,
    pub(super) source: Flux2EncoderSource,
    pub(super) text_encoder: Flux2WeightFormat,
}

/// Everything the tool persists to THIS VARIANT's settings file
/// ([`config::flux2_klein_settings_path`]).
///
/// `#[serde(default)]` so a file written by an older build keeps loading; the wire
/// value is always `normalized()`, never this struct as edited.
// `PartialEq`: the shared `SettingsSaver` detects a change by value (its save debounce).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Flux2KleinSettings {
    /// `Flux2Variant::wire()` — which checkpoint this document describes.
    ///
    /// There is one settings FILE per variant, so this is not what selects the model; it
    /// records what the file is ABOUT, so a document hand-copied between the two cannot
    /// silently claim the other one's model paths. [`load_flux2_settings`] stamps the
    /// owning variant over whatever it reads, and an absent field reads as `9b` — every
    /// file written before the 4B engine existed describes the 9B model.
    pub(super) variant: String,
    /// `Flux2SourceMode::wire()` — whether the paths below are used at all.
    ///
    /// Defaults to `manual`, and a file written before this field existed loads as manual:
    /// such a file carries hand-entered paths, and loading it as `download` would replace a
    /// working configuration with an empty download block.
    pub(super) source_mode: String,
    /// Directory of the Qwen3 text encoder, in MANUAL mode.
    ///
    /// Never written to by the download: in download mode the effective encoder is derived
    /// ([`Flux2KleinSettings::effective_paths`]) and this keeps whatever the user typed, so
    /// flipping the mode back restores his configuration exactly.
    pub(super) text_encoder_path: String,
    /// «Расцензуренный энкодер»: the persisted spelling of the encoder SOURCE
    /// ([`Flux2KleinSettings::encoder_source`]; no separate field, no migration).
    ///
    /// It selects which encoder the download mode runs and the catalog bundle fetches (its
    /// files live in their own folders, so both sources may be installed at once and
    /// switching costs nothing when the other one is there). Inert for a variant that offers
    /// no uncensored encoder (4B). It never travels inside `params`: the derived encoder path
    /// already names the chosen files.
    pub(super) uncensored_text_encoder: bool,
    /// Either a `.safetensors` file or a diffusers directory.
    pub(super) transformer_path: String,
    /// VAE directory or `.safetensors` file.
    pub(super) vae_path: String,
    /// «Веса трансформера»: the `Flux2WeightFormat` wire id (`bf16`, `q8_0`, ...) of the
    /// transformer the DOWNLOAD mode runs (both variants come from the app's catalog).
    ///
    /// Persisted as the wire id so a hand-edited file stays readable; an absent field reads as
    /// `bf16` (every file written before the formats existed), and an unknown id or one the
    /// variant does not offer reads as `bf16` too ([`Flux2KleinSettings::weight_format`]).
    /// Inert in manual mode (the picked path is sent as-is and the backend derives its format).
    pub(super) transformer_format: String,
    /// «Веса текстового энкодера»: the same, for the text encoder of the selected source
    /// (offered per source: the uncensored 9B encoder has no `q5_k_m`; a source switch maps the
    /// stored id through `Flux2WeightFormat::closest_offered`, [`Flux2KleinSettings::set_encoder_source`]).
    /// A GGUF encoder excludes `text_encoder_fp8` (D8): `normalized()` sends fp8 off while a GGUF
    /// encoder is in effect.
    pub(super) text_encoder_format: String,
    /// Which formats of each component have their catalog files INSTALLED, and the bf16
    /// transformer layout in use — runtime state, never persisted (`serde(skip)`), filled by
    /// the engine from a worker probe ([`probe_flux2_installed_formats`]). It decides the
    /// `*_formats` candidate lists of `params` ([`Flux2KleinSettings::format_candidates`]) and
    /// the derived bf16 transformer path; it lives in the settings value because that value is
    /// what every request builder receives, and the GUI thread that builds some of them must
    /// not stat the disk.
    #[serde(skip)]
    pub(super) installed_formats: Flux2InstalledFormats,
    /// The ENGLISH prompt, i.e. the one actually sent to the backend.
    ///
    /// Never legitimately empty: an empty prompt blocks a run, so both `Default` and
    /// [`settings_from_json`] substitute [`FLUX2_DEFAULT_PROMPT`] for a blank one.
    pub(super) prompt: String,
    /// The optional user-language prompt the translator reads from.
    pub(super) source_prompt: String,
    /// The «Нет» row of the prompt-library picker: the last prompt no preset of the library
    /// holds, kept so that choosing a preset and then «Нет» brings the user's own text back
    /// (`track_uncached`). Persisted, never sent (`params` leaves it out); absent reads as empty.
    pub(super) uncached_prompt: String,
    /// Whether the translate-into-English row is shown at all.
    pub(super) translate_prompt: bool,
    /// `MtService::key()` of the machine translator used by that row.
    pub(super) mt_service: String,
    /// Source language code of `source_prompt` (`"auto"` by default).
    pub(super) source_lang: String,
    pub(super) steps: u32,
    pub(super) guidance_scale: f32,
    pub(super) strength: f32,
    /// Sent only when `use_seed` is set; otherwise the wire value is `null`.
    pub(super) seed: u64,
    pub(super) use_seed: bool,
    /// «Видеопамять»: the `VramStep` wire id (`auto` / `max` / `phase` / `half` / `min`).
    ///
    /// Persisted as the wire id so a hand-edited file is readable; `normalized()` maps an
    /// unknown id to `auto`. What a step MEANS is the backend's (`tiers.py`); the user's
    /// choice is never rewritten by a run — the backend learns floors instead.
    pub(super) vram_step: String,
    /// «Оперативная память»: the `RamStep` wire id (`auto` / `keep` / `run` / `min`).
    /// Same rules as `vram_step`.
    pub(super) ram_step: String,
    pub(super) dtype: String,
    /// VAE decode in tiles. The one memory flag a run may write back: the backend's
    /// out-of-memory recovery of the decode switches it on (`apply_backend_flags`).
    pub(super) vae_tiling: bool,
    /// Quantize the text encoder to fp8. Defaults to `false`: it trades embedding quality
    /// for memory, and that trade is the user's alone.
    pub(super) text_encoder_fp8: bool,
    /// «Энкодер на CPU»: the text encoder runs on the CPU and obeys only the RAM step, never
    /// the VRAM one. It runs once per changed prompt (its result is cached), so the CPU costs
    /// almost nothing. Default `true`, also for a file written before the field existed (the
    /// struct-level `#[serde(default)]`). Sent as `encoder_on_cpu` in every `params`, the
    /// forecast's included, so the forecast reflects it; honouring it is the backend's.
    pub(super) encoder_on_cpu: bool,
    /// Growth of the painted mask contour, pixels (`0..=FLUX2_DILATE_MAX`): the run dilates
    /// the painted mask by it (an ellipse, on the run worker) into the LATENT mask the model
    /// sees. Ignored in a whole-region run, which has no contour to grow.
    pub(super) mask_dilate_px: u32,
    /// «Постобработка»: feather and colour match, applied by the HOST to the raw run answer
    /// (`AiEngine::postprocess_settings`), never sent to the backend. FLATTENED, so its keys
    /// (`mask_feather_px`, `color_match`, `color_match_local_pct`, `color_match_scale_px`) are
    /// the very top-level keys this document always had, at the same place; its own
    /// `#[serde(default)]` reads every absent one as the shared default.
    #[serde(flatten)]
    pub(super) postprocess: PostprocessSettings,
    /// «Внимание только к маске»: the prompt acts only on the masked area, so the model
    /// cannot apply the instruction to a matching object OUTSIDE the mask.
    ///
    /// The user's persisted CHOICE, kept whatever the mask is. It is EFFECTIVE only with a
    /// painted mask: a whole-region run has no mask to confine the prompt to, so the wire
    /// value is this AND not `whole_region` ([`Flux2KleinSettings::to_params`]). Default
    /// `true`, also for a file written before the field existed (the struct-level
    /// `#[serde(default)]`).
    pub(super) text_attention_in_mask: bool,
    /// The local runtime a run uses: `ImageEditRuntime::wire()` (`pytorch` / `sdcpp`), the
    /// switch at the top of the panel. A file without the field (every file written before
    /// Stable-Diffusion.cpp existed) and an unknown id read as PyTorch ([`Self::runtime`]).
    /// An unknown id (a newer build's runtime) is rewritten as `pytorch` by the next save
    /// (`normalized`): the file stays loadable, the other build's choice is not kept.
    pub(super) runtime: String,
    /// The Stable-Diffusion.cpp options of this engine («Stable-Diffusion.cpp» section): the
    /// shared `SdCppRunOptions`, nested under its own key; absent reads as its defaults.
    pub(super) sdcpp: SdCppRunOptions,
}

impl Default for Flux2KleinSettings {
    fn default() -> Self {
        Self {
            // The historic engine: `Default` is what a settings file that carries no
            // variant loads as, and every such file describes the 9B model.
            variant: Flux2Variant::Klein9B.wire().to_string(),
            // Manual: an installation that predates the switch has hand-entered paths, and
            // so does a user who has never opened the download block.
            source_mode: FLUX2_DEFAULT_SOURCE_MODE.wire().to_string(),
            text_encoder_path: String::new(),
            // The official encoder is the default: it is the one the model ships with,
            // and the uncensored one is a deliberate choice with its own gated repository.
            uncensored_text_encoder: false,
            transformer_path: String::new(),
            vae_path: String::new(),
            // bf16 on both axes: the format the model ships in, and the only one an install
            // made before the formats existed has on disk.
            transformer_format: Flux2WeightFormat::Bf16.wire().to_string(),
            text_encoder_format: Flux2WeightFormat::Bf16.wire().to_string(),
            installed_formats: Flux2InstalledFormats::default(),
            // Not empty: an empty prompt is the one value the run gate refuses outright,
            // so the default is the job this tool exists for.
            prompt: FLUX2_DEFAULT_PROMPT.to_string(),
            source_prompt: String::new(),
            uncached_prompt: String::new(),
            translate_prompt: false,
            mt_service: MtService::Google.key().to_string(),
            source_lang: "auto".to_string(),
            // The user's checkpoint is distilled: four steps at guidance 1.0 is the
            // configuration it was trained to answer at, not a speed compromise.
            steps: 4,
            guidance_scale: 1.0,
            strength: 1.0,
            seed: 0,
            use_seed: false,
            // «Авто» on both axes: the backend picks the fastest step pair that fits this
            // machine, so a fresh install never starts on a step its card cannot hold.
            vram_step: VramStep::Auto.wire().to_string(),
            ram_step: RamStep::Auto.wire().to_string(),
            dtype: Flux2Dtype::Bfloat16.wire().to_string(),
            vae_tiling: false,
            // Never defaulted on: quantizing the encoder is a quality trade the user
            // makes deliberately.
            text_encoder_fp8: false,
            // On: the encoder runs once per changed prompt, so the CPU barely costs time, and
            // keeping it off the GPU leaves the video memory to the model that runs every step.
            encoder_on_cpu: true,
            mask_dilate_px: 16,
            // The one owner of the post-processing defaults is `ms_region_postprocess`.
            postprocess: PostprocessSettings::default(),
            // On: a prompt that may also act outside the mask can edit a matching object
            // the user never selected, and painting a mask says "only here".
            text_attention_in_mask: true,
            // PyTorch: the only runtime that existed before, so nothing changes for a user who
            // never touches the switch.
            runtime: ImageEditRuntime::PyTorch.wire().to_string(),
            sdcpp: SdCppRunOptions::default(),
        }
    }
}

impl Flux2KleinSettings {
    /// Returns a copy with every field forced into its supported range: unknown
    /// step/dtype/service values fall back to their defaults, numeric ranges are
    /// clamped, and non-finite floats are replaced by the default. This is the ONLY
    /// value ever put on the wire.
    #[must_use]
    pub(super) fn normalized(&self) -> Self {
        let defaults = Self::default();
        let clamp_f32 = |value: f32, min: f32, max: f32, fallback: f32| {
            if value.is_finite() {
                value.clamp(min, max)
            } else {
                fallback
            }
        };
        let variant = self.variant();
        Self {
            variant: variant.wire().to_string(),
            source_mode: Flux2SourceMode::from_wire(&self.source_mode)
                .wire()
                .to_string(),
            text_encoder_path: self.text_encoder_path.trim().to_string(),
            // Forced off for a variant that offers no uncensored encoder: the panel never
            // draws the toggle there, so a hand-edited settings file is the only way it could
            // arrive, and it must not select files nothing publishes.
            uncensored_text_encoder: self.encoder_source() == Flux2EncoderSource::Uncensored,
            transformer_path: self.transformer_path.trim().to_string(),
            vae_path: self.vae_path.trim().to_string(),
            // An unknown id, or one this variant does not offer, normalizes to bf16.
            transformer_format: self.weight_format(Flux2WeightAxis::Transformer).wire().to_string(),
            text_encoder_format: self.weight_format(Flux2WeightAxis::TextEncoder).wire().to_string(),
            installed_formats: self.installed_formats.clone(),
            prompt: self.prompt.trim().to_string(),
            source_prompt: self.source_prompt.clone(),
            uncached_prompt: self.uncached_prompt.clone(),
            translate_prompt: self.translate_prompt,
            mt_service: MtService::from_key(&self.mt_service)
                .unwrap_or(MtService::Google)
                .key()
                .to_string(),
            source_lang: normalize_source_lang(&self.source_lang),
            steps: self.steps.clamp(FLUX2_STEPS_MIN, FLUX2_STEPS_MAX),
            guidance_scale: clamp_f32(
                self.guidance_scale,
                FLUX2_GUIDANCE_MIN,
                FLUX2_GUIDANCE_MAX,
                defaults.guidance_scale,
            ),
            strength: clamp_f32(
                self.strength,
                FLUX2_STRENGTH_MIN,
                FLUX2_STRENGTH_MAX,
                defaults.strength,
            ),
            seed: self.seed,
            use_seed: self.use_seed,
            vram_step: self.step_choice().vram.wire().to_string(),
            ram_step: self.step_choice().ram.wire().to_string(),
            dtype: Flux2Dtype::from_wire(&self.dtype).wire().to_string(),
            vae_tiling: self.vae_tiling,
            // D8: fp8 quantization and a GGUF encoder exclude each other (the backend refuses
            // the pair by name). Only the wire value is forced; the stored choice survives,
            // so switching back to bf16 brings it back.
            text_encoder_fp8: self.text_encoder_fp8 && !self.gguf_text_encoder_in_effect(),
            encoder_on_cpu: self.encoder_on_cpu,
            mask_dilate_px: self.mask_dilate_px.min(FLUX2_DILATE_MAX),
            postprocess: self.postprocess.normalized(),
            text_attention_in_mask: self.text_attention_in_mask,
            runtime: self.runtime().wire().to_string(),
            sdcpp: self.sdcpp.normalized(),
        }
    }

    /// The runtime a run uses; an absent or unknown persisted id reads as PyTorch.
    #[must_use]
    pub(super) fn runtime(&self) -> ImageEditRuntime {
        ImageEditRuntime::from_wire(&self.runtime).unwrap_or_default()
    }

    /// Stores `runtime`; returns `true` when it changed (a save is owed).
    pub(super) fn set_runtime(&mut self, runtime: ImageEditRuntime) -> bool {
        let changed = self.runtime() != runtime || self.runtime != runtime.wire();
        self.runtime = runtime.wire().to_string();
        changed
    }

    /// The memory step pair, an unknown persisted id reading as «Авто».
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

    /// The active source mode.
    pub(super) fn source_mode(&self) -> Flux2SourceMode {
        Flux2SourceMode::from_wire(&self.source_mode)
    }

    /// The checkpoint this document describes. Anything unrecognized reads as 9B.
    pub(super) fn variant(&self) -> Flux2Variant {
        Flux2Variant::from_wire(&self.variant)
    }

    /// The encoder source in effect: `Uncensored` when the persisted toggle asks for it AND
    /// the variant offers it ([`Flux2EncoderSource::offered_for`]), else `Stock` — the flag is
    /// inert for 4B rather than pointing a path at files nothing publishes.
    #[must_use]
    pub(super) fn encoder_source(&self) -> Flux2EncoderSource {
        let offered = Flux2EncoderSource::offered_for(self.variant());
        if self.uncensored_text_encoder && offered.contains(&Flux2EncoderSource::Uncensored) {
            Flux2EncoderSource::Uncensored
        } else {
            Flux2EncoderSource::Stock
        }
    }

    /// The weight selection in effect ([`Flux2WeightSelection`]).
    #[must_use]
    pub(super) fn weight_selection(&self) -> Flux2WeightSelection {
        Flux2WeightSelection {
            transformer: self.weight_format(Flux2WeightAxis::Transformer),
            source: self.encoder_source(),
            text_encoder: self.weight_format(Flux2WeightAxis::TextEncoder),
        }
    }

    /// Stores `source` and maps the stored encoder format onto what that source offers
    /// (`Flux2WeightFormat::closest_offered`: the format itself, else the next smaller quant,
    /// else bf16). Returns `Some((from, to))` when the stored format had to change, so the
    /// panel can say so; `None` when it was kept. An unknown stored id reads as `bf16`.
    pub(super) fn set_encoder_source(&mut self, source: Flux2EncoderSource) -> Option<(Flux2WeightFormat, Flux2WeightFormat)> {
        self.uncensored_text_encoder = source == Flux2EncoderSource::Uncensored;
        let from = Flux2WeightFormat::from_wire(&self.text_encoder_format).unwrap_or(Flux2WeightFormat::Bf16);
        let to = from.closest_offered(Flux2WeightFormat::offered_for(self.variant(), Flux2WeightAxis::TextEncoder, self.encoder_source()));
        self.text_encoder_format = to.wire().to_string();
        (to != from).then_some((from, to))
    }

    /// The selected weight format of `axis`: the persisted id when this document's variant
    /// offers it for the current encoder source ([`Flux2WeightFormat::offered_for`]), else
    /// `Bf16` — an absent, unknown or not-offered id never reaches a path or the wire.
    #[must_use]
    pub(super) fn weight_format(&self, axis: Flux2WeightAxis) -> Flux2WeightFormat {
        let stored = match axis {
            Flux2WeightAxis::Transformer => &self.transformer_format,
            Flux2WeightAxis::TextEncoder => &self.text_encoder_format,
        };
        Flux2WeightFormat::from_wire(stored)
            .filter(|format| Flux2WeightFormat::offered_for(self.variant(), axis, self.encoder_source()).contains(format))
            .unwrap_or(Flux2WeightFormat::Bf16)
    }

    /// Whether the two weight-format settings are IN EFFECT: download mode (both variants'
    /// packages come from the catalog). In manual mode they are inert and nothing of them is
    /// sent.
    #[must_use]
    pub(super) fn weight_formats_in_effect(&self) -> bool {
        self.source_mode() == Flux2SourceMode::Download
    }

    /// Whether the run uses a GGUF text encoder chosen through the format setting (D8: it
    /// excludes `text_encoder_fp8`). A `.gguf` file picked in MANUAL mode is not detected
    /// here — the backend derives its format from the container and refuses fp8 with it by
    /// name, which is never a silent fallback.
    #[must_use]
    pub(super) fn gguf_text_encoder_in_effect(&self) -> bool {
        self.weight_formats_in_effect() && self.weight_format(Flux2WeightAxis::TextEncoder).is_gguf()
    }

    /// The catalog path of `format` on `axis` for this document's variant and encoder source,
    /// in the probed bf16 transformer layout (`installed_formats.transformer_layout`,
    /// `Primary` before the probe): the bf16 folder or single file, or the GGUF file. Pure path
    /// arithmetic, no disk access.
    ///
    /// Every offered format resolves (pinned by the catalog's own tests and by
    /// `every_offered_format_has_a_catalog_path` here); an unresolvable one yields an EMPTY
    /// path, which the run gate refuses visibly instead of running some other file.
    fn catalog_weights_path(&self, axis: Flux2WeightAxis, format: Flux2WeightFormat) -> String {
        flux2_klein_weights_path(&config::side_models_dir(), self.variant(), axis, self.encoder_source(), format, self.installed_formats.transformer_layout)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// The `*_formats` candidate list of `axis`: every format offered for the current encoder
    /// source whose catalog files are installed (`installed_formats`), plus the selected one
    /// whatever its state, each with its catalog path, ordered best quality first (the order of
    /// [`Flux2WeightFormat::offered_for`]). The selected entry's path is exactly the
    /// component's effective path, as the wire contract requires.
    ///
    /// Empty unless the format settings are in effect ([`Self::weight_formats_in_effect`]).
    #[must_use]
    pub(super) fn format_candidates(&self, axis: Flux2WeightAxis) -> Vec<(Flux2WeightFormat, String)> {
        if !self.weight_formats_in_effect() {
            return Vec::new();
        }
        let selected = self.weight_format(axis);
        let source = self.encoder_source();
        Flux2WeightFormat::offered_for(self.variant(), axis, source)
            .iter()
            .copied()
            .filter(|&format| format == selected || self.installed_formats.contains(axis, source, format))
            .map(|format| (format, self.catalog_weights_path(axis, format)))
            .collect()
    }

    /// THE three paths a run uses, after the source mode has been applied.
    ///
    /// Every consumer goes through here — the wire `params`, the run gate, the prompt-cache
    /// gates, the read-only paths the download body shows — so the two modes cannot drift
    /// apart in one of them.
    ///
    /// In `Manual` these are the user's trimmed fields. In `Download` they are DERIVED from
    /// the models directory and the encoder toggle and the manual fields are not read at
    /// all, which is what lets a hand-built configuration survive a download untouched.
    ///
    /// In `Download` every path is derived under THIS DOCUMENT'S VARIANT
    /// ([`Flux2KleinSettings::variant`]), which is what keeps the two engines from
    /// overwriting each other's copy of the model.
    ///
    /// Both variants' packages come from the app's catalog: the transformer and the encoder
    /// follow their selected weight format and the encoder source
    /// (`external_catalog::flux2_klein_weights_path`): the bf16 folder or single file (the 9B
    /// transformer in the probed layout: the BFL single file, or the legacy `transformer/`
    /// folder of an install made by the backend's former download), the uncensored folder, or
    /// the GGUF file under `transformer_gguf/` / `text_encoder*_gguf/`. Every bf16 path is the
    /// one an existing install already has, so it stays valid. The VAE has no format axis.
    ///
    /// The backend finds the tokenizer, the scheduler and the component configs from these
    /// paths: it probes each supplied path and ITS PARENT, so the package root is reached from
    /// any of them (`modules/ai_backend/inpaint/flux2_klein/components.py`).
    #[must_use]
    pub(super) fn effective_paths(&self) -> Flux2EffectivePaths {
        match self.source_mode() {
            Flux2SourceMode::Manual => Flux2EffectivePaths {
                text_encoder: self.text_encoder_path.trim().to_string(),
                transformer: self.transformer_path.trim().to_string(),
                vae: self.vae_path.trim().to_string(),
            },
            Flux2SourceMode::Download => Flux2EffectivePaths {
                text_encoder: self.catalog_weights_path(Flux2WeightAxis::TextEncoder, self.weight_format(Flux2WeightAxis::TextEncoder)),
                transformer: self.catalog_weights_path(Flux2WeightAxis::Transformer, self.weight_format(Flux2WeightAxis::Transformer)),
                vae: config::flux2_klein_vae_dir(self.variant()).to_string_lossy().to_string(),
            },
        }
    }

    /// Builds the `params` object of a generation request (and, through the builder they
    /// share, of every query about one).
    ///
    /// `self` must already be `normalized()`. `seed` is `null` unless the user pinned
    /// one, which is what the backend expects for "pick a fresh seed".
    ///
    /// `whole_region` is NOT a setting and is therefore passed in rather than read from
    /// `self`: it is derived from the painted mask by [`mask_for_run`], which builds the
    /// buffer and the flag together. It does not replace the mask — the blob still carries
    /// one, and the backend refuses `whole_region = true` unless that mask is solid.
    ///
    /// Every path that only ASKS the backend something (`.status`, the prompt-cache calls)
    /// passes `false`: no mask exists for those, and `true` would make
    /// the backend apply the mode's parameter overrides — and log them — on a polling path.
    ///
    /// `text_attention_in_mask` goes out as its EFFECTIVE value — the setting AND a painted
    /// mask (`!whole_region`) — so the wire never claims a mode the run cannot have. The
    /// `large_models.profiles` forecast uses [`Flux2KleinSettings::to_profiles_params`]
    /// instead.
    #[must_use]
    pub(super) fn to_params(&self, whole_region: bool) -> Value {
        self.params_for(whole_region, self.text_attention_in_mask && !whole_region)
    }

    /// The `params` of a `large_models.profiles` forecast. Like `to_params(false)` — a
    /// forecast is never a whole-region request — except that `text_attention_in_mask` is
    /// effective only while `mask_painted`, the host's answer of the last drawn frame: the
    /// backend counts the attention mask exactly when the flag is on, and a run over an
    /// empty mask will not have it.
    #[must_use]
    pub(super) fn to_profiles_params(&self, mask_painted: bool) -> Value {
        self.params_for(false, self.text_attention_in_mask && mask_painted)
    }

    /// The one builder behind [`Flux2KleinSettings::to_params`] and
    /// [`Flux2KleinSettings::to_profiles_params`]; `text_attention_in_mask` is the
    /// already-effective wire value.
    ///
    /// The memory steps always travel (`vram_step` / `ram_step`, the v13 two-axis fields that
    /// win over everything legacy), and none of the retired placement fields does.
    fn params_for(&self, whole_region: bool, text_attention_in_mask: bool) -> Value {
        // The EFFECTIVE paths, never the raw fields: in download mode the fields hold the
        // user's own configuration and must not reach the backend.
        let paths = self.effective_paths();
        let mut params = json!({
            "text_encoder_path": paths.text_encoder,
            "transformer_path": paths.transformer,
            "vae_path": paths.vae,
            "prompt": self.prompt,
            "steps": self.steps,
            "guidance_scale": self.guidance_scale,
            "strength": self.strength,
            "seed": if self.use_seed { json!(self.seed) } else { Value::Null },
            "vram_step": self.vram_step,
            "ram_step": self.ram_step,
            "dtype": self.dtype,
            "vae_tiling": self.vae_tiling,
            "text_encoder_fp8": self.text_encoder_fp8,
            "encoder_on_cpu": self.encoder_on_cpu,
            "whole_region": whole_region,
            // No `mask_dilate_px` and none of the `postprocess` keys (protocol 17 refuses all
            // five by name): the dilation is already in the mask the blob carries
            // (`run_flux2_klein_pass`), and feather / colour match are the host's.
            "text_attention_in_mask": text_attention_in_mask,
            // Pinned, not a setting: see `FLUX2_MAX_SEQ`. The field stays on the wire
            // because the backend still reads it.
            "max_sequence_length": FLUX2_MAX_SEQ,
            // Protocol 23: the model's shared prompt-cache library, and the precision order its
            // entries are preferred in (Rust owns it; the backend treats the ids as opaque).
            "prompt_library": self.variant().prompt_library_id(),
            "prompt_cache_precision_order": flux2_prompt_cache_precision_order(),
        });
        // The encoder SOURCE is known only in download mode (the catalog names the files);
        // a hand-picked encoder has none, and the backend then records and prefers none.
        if self.source_mode() == Flux2SourceMode::Download
            && let Some(map) = params.as_object_mut()
        {
            map.insert("text_encoder_source".to_string(), json!(self.encoder_source().wire()));
        }
        // The weight formats (protocol 21, D5) travel only while their settings are in
        // effect: download mode, either variant. Manual mode sends nothing of them (the
        // backend then derives each format from the file's container).
        if self.weight_formats_in_effect()
            && let Some(map) = params.as_object_mut()
        {
            for (axis, format_key, list_key) in [
                (Flux2WeightAxis::Transformer, "transformer_format", "transformer_formats"),
                (Flux2WeightAxis::TextEncoder, "text_encoder_format", "text_encoder_formats"),
            ] {
                let candidates: Vec<Value> = self
                    .format_candidates(axis)
                    .into_iter()
                    .map(|(format, path)| json!({ "id": format.wire(), "path": path }))
                    .collect();
                map.insert(format_key.to_string(), json!(self.weight_format(axis).wire()));
                map.insert(list_key.to_string(), Value::Array(candidates));
            }
        }
        params
    }
}

/// Maps a persisted language code onto the shared MT source-language list, falling
/// back to `"auto"` for anything the list does not carry.
pub(super) fn normalize_source_lang(code: &str) -> String {
    let lowered = code.trim().to_ascii_lowercase();
    if MT_SOURCE_LANGUAGES.iter().any(|lang| lang.code == lowered) {
        lowered
    } else {
        "auto".to_string()
    }
}

/// Localized title of a source-language code, or the raw code when it is unknown.
pub(super) fn source_lang_title(code: &str) -> String {
    MT_SOURCE_LANGUAGES
        .iter()
        .find(|lang: &&MtLanguage| lang.code == code)
        .map_or_else(|| code.to_string(), |lang| lang.title().to_string())
}

/// A settings document as read, before the variant stamp: the settings and whether the
/// retired memory placement was migrated into them ([`legacy_memory_steps`]).
#[derive(Debug, Clone)]
pub(super) struct Flux2LoadedDocument {
    pub(super) settings: Flux2KleinSettings,
    /// The document carried the retired placement fields and was migrated: the engine
    /// writes it back once, without them.
    pub(super) migrated: bool,
}

/// The outcome of the settings load the engine applies on the GUI thread.
#[derive(Debug, Clone)]
pub(super) struct Flux2SettingsLoad {
    pub(super) settings: Flux2KleinSettings,
    pub(super) migrated: bool,
    pub(super) problem: Option<SettingsProblem>,
}

impl Flux2SettingsLoad {
    /// Whether the engine must not write the settings file this session.
    pub(super) fn save_blocked(&self) -> bool {
        matches!(self.problem, Some(SettingsProblem::SaveBlocked(_)))
    }
}

/// Reads `variant`'s settings file. Runs on a worker thread.
///
/// See [`load_flux2_settings_from`]; the aside name is stamped with the current Unix time.
pub(super) fn load_flux2_settings(variant: Flux2Variant) -> Flux2SettingsLoad {
    load_flux2_settings_from(&config::flux2_klein_settings_path(variant), variant, unix_now_secs())
}

/// Reads the settings file at `path`, which belongs to `variant`.
///
/// The file rules are the shared `read_settings_file`: a missing file is the normal first
/// run (defaults); a file that is not a readable settings document is NEVER silently
/// replaced — it is moved aside to the first free `<stem>.malformed-<unix_secs>[-<n>].json`
/// beside it and defaults load, or, when that fails (or the file cannot be read at all),
/// defaults load with saving BLOCKED. Both cases are logged and reported in
/// [`Flux2SettingsLoad::problem`] for the panel. A readable document is migrated
/// ([`settings_from_json`]) and the migration is logged.
///
/// The returned document always carries `variant`, whatever the file said: the file that
/// was read is the one that BELONGS to this engine, so a document copied over from the
/// other variant must not keep claiming to describe the other model's paths.
pub(super) fn load_flux2_settings_from(path: &Path, variant: Flux2Variant, unix_secs: u64) -> Flux2SettingsLoad {
    let read = read_settings_file(path, unix_secs, FLUX2_LOG_LABEL, settings_from_json);
    let (settings, migrated) = match read.document {
        Some(document) => {
            if document.migrated {
                ms_log::runtime_log::log_info(format!(
                    "[cleaning] FLUX.2 klein settings {}: the retired memory placement was migrated to vram_step={} ram_step={}",
                    path.display(),
                    document.settings.vram_step,
                    document.settings.ram_step
                ));
            }
            (document.settings, document.migrated)
        }
        None => (Flux2KleinSettings::default(), false),
    };
    let mut load = Flux2SettingsLoad { settings, migrated, problem: read.problem };
    // Stamps the owning variant over whatever the document said.
    load.settings.variant = variant.wire().to_string();
    load
}

/// Deserializes a settings document and migrates the retired memory placement.
///
/// Split out of the loader so the migration is testable without a file on disk.
///
/// # Errors
/// The `serde_json` error when the value is not a settings object (or a field has the wrong
/// type): the loader then treats the file as damaged and moves it aside, instead of
/// loading defaults over it and overwriting it on the next save.
pub(super) fn settings_from_json(value: &Value) -> Result<Flux2LoadedDocument, serde_json::Error> {
    // serde also builds a struct from a JSON ARRAY (field by position): `["9b", "download"]`
    // would load as a variant and a source mode. Only an object is a settings document; anything
    // else is damaged and moved aside.
    if !value.is_object() {
        return Err(<serde_json::Error as serde::de::Error>::custom("the settings document is not a JSON object"));
    }
    let mut settings: Flux2KleinSettings = serde_json::from_value(value.clone())?;
    // An ABSENT variant is what every file written before the 4B engine existed looks
    // like, and serde's `#[serde(default)]` already answers `9b` for it. A PRESENT but
    // unrecognized token is normalized here for the same reason every other wire enum is:
    // a hand-edited value must not reach a path derivation as an unknown literal.
    settings.variant = Flux2Variant::from_wire(&settings.variant).wire().to_string();
    // The retired placement fields (`placement`, `low_cpu_mem_usage`, `vae_slicing`,
    // `unload_transformer_before_vae`, `unload_text_encoder_after_encode`) map to no field
    // any more; serde drops them, and only `legacy_memory_steps` still reads them, once.
    let migration = legacy_memory_steps(value);
    if let Some(choice) = migration {
        settings.set_step_choice(choice);
    }
    // A file written before the prompt had a default — or one the user emptied — loads
    // with the default prompt rather than with a blank field. A blank prompt is not a
    // usable state: it blocks the run gate, so preserving it would only mean a tool that
    // refuses to start until the user guesses what to type. Serde's `#[serde(default)]`
    // covers the ABSENT key; only the present-but-blank case needs this.
    if settings.prompt.trim().is_empty() {
        settings.prompt = FLUX2_DEFAULT_PROMPT.to_string();
    }
    // A file written by a build that still had the «Работа без маски» checkbox carries a
    // `whole_region` key that no longer maps to a field; older files likewise carry
    // `max_sequence_length` (pinned to `FLUX2_MAX_SEQ` now) and `brush_radius` (the brush
    // belongs to the host's `MaskBrush`). No migration is needed for any of them, and none
    // may be added: the struct does NOT use `deny_unknown_fields`, so serde drops such keys
    // silently, the document loads with every other setting intact, and the next save simply
    // writes it out without them. The mode `whole_region` used to hold is derived from the
    // painted mask now (`mask_for_run`), so there is nothing left for a stored value to mean.
    Ok(Flux2LoadedDocument { settings, migrated: migration.is_some() })
}

/// Writes `settings` into the file of the variant the document itself names.
///
/// The variant is read from `settings` rather than passed in, so the document and the
/// file it lands in can never disagree: [`load_flux2_settings`] stamps the owning variant
/// on load and the engine stamps it at construction, which makes that field the single
/// answer to "whose file is this".
///
/// # Errors
/// Returns a user-facing message when the data directory cannot be created, when the
/// settings cannot be serialized, or when the write fails.
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
pub(super) fn save_flux2_settings(settings: &Flux2KleinSettings) -> Result<(), String> {
    if cfg!(test) {
        ms_log::runtime_log::log_info(format!("[cleaning] {FLUX2_LOG_LABEL} settings not written: a test build never writes the user's settings file"));
        return Ok(());
    }
    write_settings_json(&config::flux2_klein_settings_path(settings.variant()), settings).map_err(|err| match err {
        SettingsWriteError::CreateDir(err) => tf!("cleaning.settings_io.create_dir_error", err = err),
        SettingsWriteError::Serialize(err) => tf!("cleaning.tools.flux2_klein.serialize_settings_error", err = err),
        SettingsWriteError::Write(err) => tf!("cleaning.tools.flux2_klein.write_settings_error", err = err),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_sysprobe::ai_models::external_catalog::flux2_klein_legacy_transformer_spec;

    /// A test build never writes the user's real settings file (the saver's teardown flush
    /// can reach the product writer from any test): it answers `Ok` and leaves the file alone.
    #[test]
    fn a_test_build_never_writes_the_settings_file() {
        let settings = Flux2KleinSettings::default();
        let path = config::flux2_klein_settings_path(settings.variant());
        let before = std::fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        assert!(save_flux2_settings(&settings).is_ok());
        assert_eq!(std::fs::metadata(&path).and_then(|meta| meta.modified()).ok(), before);
    }

    #[test]
    fn defaults_match_the_distilled_checkpoint_and_start_on_auto() {
        let settings = Flux2KleinSettings::default();
        assert_eq!(settings.steps, 4);
        assert!((settings.guidance_scale - 1.0).abs() < f32::EPSILON);
        assert!((settings.strength - 1.0).abs() < f32::EPSILON);
        assert!(!settings.use_seed);
        assert_eq!(settings.mask_dilate_px, 16);
        assert_eq!(settings.postprocess, PostprocessSettings::default(), "the shared post-processing defaults");
        assert_eq!(settings.step_choice(), StepChoice::default(), "«Авто» / «Авто»");
        // Quantizing the text encoder is never defaulted on.
        assert!(!settings.text_encoder_fp8);
        let partial = loaded_settings(&json!({}));
        assert_eq!(partial.steps, 4);
        assert_eq!(partial.vram_step, "auto");
        assert_eq!(partial.ram_step, "auto");
    }

    /// The runtime switch: a file without the field (every file older than sd.cpp) and an
    /// unknown id read as PyTorch; the sd.cpp options nest under `sdcpp` and default sensibly;
    /// neither ever reaches the backend's `params`.
    #[test]
    fn the_runtime_defaults_to_pytorch_and_never_travels() {
        assert_eq!(loaded_settings(&json!({})).runtime(), ImageEditRuntime::PyTorch);
        assert_eq!(loaded_settings(&json!({ "runtime": "quantum" })).runtime(), ImageEditRuntime::PyTorch);
        let sdcpp = loaded_settings(&json!({ "runtime": "sdcpp", "sdcpp": { "vae_tiling": true } }));
        assert_eq!(sdcpp.runtime(), ImageEditRuntime::SdCpp);
        assert!(sdcpp.sdcpp.vae_tiling);
        assert!(sdcpp.sdcpp.text_encoder_on_cpu, "an absent sd.cpp field keeps its default");
        assert_eq!(sdcpp.normalized().runtime, "sdcpp");
        let mut settings = Flux2KleinSettings::default();
        assert!(settings.set_runtime(ImageEditRuntime::SdCpp));
        assert!(!settings.set_runtime(ImageEditRuntime::SdCpp), "an unchanged runtime owes no save");
        let params = settings.normalized().to_params(false);
        assert!(params.get("runtime").is_none() && params.get("sdcpp").is_none(), "{params}");
        let saved = serde_json::to_value(&settings).expect("serializable");
        assert_eq!(saved["runtime"], json!("sdcpp"));
    }

    #[test]
    fn normalized_clamps_every_range() {
        let mut settings = Flux2KleinSettings {
            steps: 999,
            guidance_scale: f32::NAN,
            strength: 0.0,
            mask_dilate_px: 999,
            postprocess: PostprocessSettings { mask_feather_px: 999, color_match: true, color_match_local_pct: 101, color_match_scale_px: 97 },
            vram_step: "turbo".to_string(),
            ram_step: " keep ".to_string(),
            dtype: "nonsense".to_string(),
            mt_service: "nonsense".to_string(),
            source_lang: "nonsense".to_string(),
            ..Flux2KleinSettings::default()
        };
        settings.text_encoder_path = "  /models/qwen3  ".to_string();
        let norm = settings.normalized();
        assert_eq!(norm.steps, FLUX2_STEPS_MAX);
        assert!((norm.guidance_scale - 1.0).abs() < f32::EPSILON);
        assert!((norm.strength - FLUX2_STRENGTH_MIN).abs() < f32::EPSILON);
        assert_eq!(norm.mask_dilate_px, FLUX2_DILATE_MAX);
        // The post-processing ranges are `ms_region_postprocess`'s; `normalized()` delegates.
        assert_eq!(norm.postprocess, settings.postprocess.normalized());
        assert_eq!(norm.postprocess.mask_feather_px, ms_region_postprocess::FEATHER_MAX_PX);
        assert_eq!(norm.vram_step, "auto", "an unknown step id normalizes to «Авто»");
        assert_eq!(norm.ram_step, "keep", "a padded known id is kept");
        assert_eq!(norm.dtype, "bfloat16");
        assert_eq!(norm.mt_service, "google");
        assert_eq!(norm.source_lang, "auto");
        assert_eq!(norm.text_encoder_path, "/models/qwen3");
    }

    /// The run carries the two step ids and none of the retired placement fields.
    #[test]
    fn params_carry_the_steps_and_none_of_the_retired_fields() {
        let mut settings = runnable_settings();
        assert!(settings.set_step_choice(StepChoice { vram: VramStep::Half, ram: RamStep::Run }));
        assert!(!settings.set_step_choice(StepChoice { vram: VramStep::Half, ram: RamStep::Run }), "no change, no save");
        for params in [settings.normalized().to_params(false), settings.normalized().to_profiles_params(true)] {
            assert_eq!(params["vram_step"], json!("half"));
            assert_eq!(params["ram_step"], json!("run"));
            assert!(params["vae_tiling"].is_boolean());
            assert!(params["text_encoder_fp8"].is_boolean());
            for retired in ["placement", "profile", "low_cpu_mem_usage", "vae_slicing", "unload_transformer_before_vae", "unload_text_encoder_after_encode"] {
                assert!(params.get(retired).is_none(), "`{retired}` must not reach the wire");
            }
        }
    }

    /// The placement mapping table, row by row, for documents that are NOT the untouched default.
    #[test]
    fn the_legacy_placement_migrates_by_the_mapping_table() {
        let pair = |vram, ram| Some(StepChoice { vram, ram });
        let cases = [
            // `full_gpu` with any non-default flag is an explicit choice: the strict pair.
            (json!({ "placement": "full_gpu", "vae_tiling": true }), pair(VramStep::Max, RamStep::Keep)),
            (json!({ "placement": "full_gpu", "low_cpu_mem_usage": true }), pair(VramStep::Max, RamStep::Keep)),
            (json!({ "placement": "encoder_cpu", "unload_transformer_before_vae": false }), pair(VramStep::Max, RamStep::Keep)),
            (json!({ "placement": "encoder_cpu", "unload_transformer_before_vae": true }), pair(VramStep::Phase, RamStep::Keep)),
            // Absent flag: derived as `placement != full_gpu`, i.e. `true` here.
            (json!({ "placement": "encoder_cpu" }), pair(VramStep::Phase, RamStep::Keep)),
            (json!({ "placement": "model_cpu_offload" }), pair(VramStep::Phase, RamStep::Keep)),
            (json!({ "placement": "sequential_cpu_offload", "vae_slicing": true }), pair(VramStep::Min, RamStep::Keep)),
            (json!({ "placement": " sequential_cpu_offload " }), pair(VramStep::Min, RamStep::Keep)),
            // An unknown placement: «Авто» / «Авто».
            (json!({ "placement": "quantum" }), pair(VramStep::Auto, RamStep::Auto)),
        ];
        for (document, expected) in cases {
            assert_eq!(legacy_memory_steps(&document), expected, "{document}");
            let migrated = settings_from_json(&document).expect("a valid document");
            assert!(migrated.migrated, "{document}");
            assert_eq!(Some(migrated.settings.step_choice()), expected, "{document}");
        }
    }

    /// Q1: a document whose seven memory fields are all still the shipped defaults was
    /// never touched (the default `full_gpu` cannot be told apart from an explicit
    /// «Максимум скорости»), and migrates to «Авто» / «Авто» rather than to the strict pair.
    #[test]
    fn an_untouched_default_document_migrates_to_auto() {
        let shipped_defaults = json!({
            "placement": "full_gpu",
            "low_cpu_mem_usage": false,
            "vae_tiling": false,
            "vae_slicing": false,
            "unload_transformer_before_vae": false,
            "unload_text_encoder_after_encode": false,
            "text_encoder_fp8": false,
            "prompt": "a red balloon"
        });
        assert_eq!(legacy_memory_steps(&shipped_defaults), Some(StepChoice::default()));
        // A file older than some of the flags carries fewer keys: absent reads as default.
        assert_eq!(legacy_memory_steps(&json!({ "placement": "full_gpu", "steps": 4 })), Some(StepChoice::default()));
        // Any ONE flag away from its default is an explicit choice.
        for (flag, _) in LEGACY_MEMORY_FLAGS_AND_DEFAULTS {
            let mut touched = shipped_defaults.clone();
            touched[flag] = json!(true);
            assert_eq!(
                legacy_memory_steps(&touched),
                Some(StepChoice { vram: VramStep::Max, ram: RamStep::Keep }),
                "`{flag}` changed: the table decides"
            );
        }
        let migrated = settings_from_json(&shipped_defaults).expect("valid");
        assert!(migrated.migrated);
        assert_eq!(migrated.settings.step_choice(), StepChoice::default());
        assert_eq!(migrated.settings.prompt, "a red balloon", "the migration costs nothing else");
    }

    /// A document that has either step field is never re-migrated (steps win over a stale
    /// placement), and a migrated document written back is stable.
    #[test]
    fn the_migration_is_idempotent_and_steps_win() {
        let both = json!({ "placement": "sequential_cpu_offload", "vram_step": "half", "ram_step": "run" });
        assert_eq!(legacy_memory_steps(&both), None);
        let loaded = settings_from_json(&both).expect("valid");
        assert!(!loaded.migrated);
        assert_eq!(loaded.settings.step_choice(), StepChoice { vram: VramStep::Half, ram: RamStep::Run });
        let one = json!({ "placement": "sequential_cpu_offload", "ram_step": "min" });
        assert_eq!(legacy_memory_steps(&one), None, "one step field is enough to stop the migration");
        assert_eq!(loaded_settings(&one).step_choice(), StepChoice { vram: VramStep::Auto, ram: RamStep::Min });

        let first = settings_from_json(&json!({ "placement": "model_cpu_offload" })).expect("valid");
        assert!(first.migrated);
        let written = serde_json::to_value(&first.settings).expect("serialize");
        assert!(written.get("placement").is_none());
        let second = settings_from_json(&written).expect("valid");
        assert!(!second.migrated, "a written-back document is never migrated again");
        assert_eq!(second.settings.step_choice(), first.settings.step_choice());
        // No placement at all: nothing to migrate.
        assert_eq!(legacy_memory_steps(&json!({ "steps": 4 })), None);
        // An unknown step id in the file reads as «Авто».
        assert_eq!(loaded_settings(&json!({ "vram_step": "warp" })).step_choice().vram, VramStep::Auto);
    }

    /// A damaged file is MOVED ASIDE, never overwritten; a missing one is a first run; a
    /// readable one loads (and is migrated). Scratch files live under the OS temp dir.
    #[test]
    fn a_damaged_settings_file_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("ms_flux2_settings_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let path = dir.join("flux2_klein_settings.json");

        let missing = load_flux2_settings_from(&path, Flux2Variant::Klein4B, 7);
        assert!(missing.problem.is_none() && !missing.migrated);
        assert_eq!(missing.settings.variant(), Flux2Variant::Klein4B, "the owning variant is stamped");

        // `["9b", "download"]` is valid JSON that serde would read field by position.
        for damaged in ["{ not json", "", "[1, 2]", "[\"9b\", \"download\"]", "\"text\"", "{\"steps\": \"many\"}"] {
            std::fs::write(&path, damaged).expect("write the damaged file");
            let load = load_flux2_settings_from(&path, Flux2Variant::Klein9B, 42);
            let aside = malformed_aside_path(&path, 42, 0);
            assert_eq!(load.problem, Some(SettingsProblem::MovedAside(aside.clone())), "{damaged:?}");
            assert!(!load.save_blocked());
            assert!(!path.exists(), "the damaged file left its place");
            assert_eq!(std::fs::read_to_string(&aside).expect("the aside copy"), damaged, "kept byte for byte");
            std::fs::remove_file(&aside).expect("remove the aside copy");
        }
        assert_eq!(malformed_aside_path(&path, 42, 0).file_name().and_then(|n| n.to_str()), Some("flux2_klein_settings.malformed-42.json"));
        assert_eq!(malformed_aside_path(&path, 42, 3).file_name().and_then(|n| n.to_str()), Some("flux2_klein_settings.malformed-42-3.json"));

        std::fs::write(&path, r#"{ "placement": "encoder_cpu", "prompt": "a cat" }"#).expect("write a legacy file");
        let legacy = load_flux2_settings_from(&path, Flux2Variant::Klein9B, 1);
        assert!(legacy.problem.is_none());
        assert!(legacy.migrated, "the engine writes it back once");
        assert_eq!(legacy.settings.step_choice(), StepChoice { vram: VramStep::Phase, ram: RamStep::Keep });
        std::fs::remove_file(&path).expect("remove the legacy file");

        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    #[test]
    fn normalization_never_rewrites_a_guidance_the_checkpoint_ignores() {
        // A distilled checkpoint makes `guidance_scale` inert, and the panel closes the
        // control for it — but the VALUE is the user's and the backend owns the run's
        // semantics, so nothing here may clamp it to 1.0, drop it from `params` or reset
        // it. A user who switches to a checkpoint that is not distilled has to find his
        // setting where he left it; `normalized()` deliberately knows nothing about the
        // flag and only enforces the range.
        let settings = Flux2KleinSettings {
            guidance_scale: 3.5,
            ..Flux2KleinSettings::default()
        };
        let norm = settings.normalized();
        assert!((norm.guidance_scale - 3.5).abs() < f32::EPSILON);
        assert_eq!(
            norm.to_params(false)["guidance_scale"],
            Value::from(3.5_f32),
            "the value travels on the wire unchanged; the backend decides what it means"
        );
    }

    #[test]
    fn params_omit_the_seed_unless_pinned() {
        let settings = Flux2KleinSettings::default().normalized();
        assert_eq!(settings.to_params(false)["seed"], Value::Null);
        let pinned = Flux2KleinSettings {
            use_seed: true,
            seed: 42,
            ..Flux2KleinSettings::default()
        }
        .normalized();
        assert_eq!(pinned.to_params(false)["seed"], json!(42));
        // The distilled checkpoint has no negative prompt and must never grow a field
        // for one.
        assert!(pinned.to_params(false).get("negative_prompt").is_none());
    }

    /// The mode is not a setting any more, so the only thing left to pin about the wire
    /// is that the flag the caller derived is the flag that reaches the backend.
    #[test]
    fn the_derived_mode_is_what_reaches_the_wire() {
        for whole_region in [false, true] {
            let params = runnable_settings().normalized().to_params(whole_region);
            assert_eq!(params["whole_region"], json!(whole_region));
        }
    }

    /// Protocol 17: the dilation travels inside the mask blob and the post-processing is the
    /// host's, so none of the five retired mask/post keys reaches the backend — not in a run
    /// (either mode) and not in a forecast. The backend refuses each of them by name.
    #[test]
    fn no_mask_or_post_key_reaches_the_wire() {
        let normalized = runnable_settings().normalized();
        let all = [normalized.to_params(false), normalized.to_params(true), normalized.to_profiles_params(true), normalized.to_profiles_params(false)];
        for params in all {
            for retired in ["mask_dilate_px", "mask_feather_px", "color_match", "color_match_local_pct", "color_match_scale_px"] {
                assert!(params.get(retired).is_none(), "`{retired}` must not reach the wire: {params}");
            }
        }
    }

    /// `text_attention_in_mask` travels as its EFFECTIVE value: the setting AND a painted
    /// mask. A whole-region run has no mask to confine the prompt to, so it always sends
    /// `false`, and the user's persisted choice is untouched by that.
    #[test]
    fn text_attention_in_mask_is_sent_only_when_effective() {
        for (setting, whole_region, expected) in
            [(true, false, true), (true, true, false), (false, false, false), (false, true, false)]
        {
            let mut settings = runnable_settings();
            settings.text_attention_in_mask = setting;
            let params = settings.normalized().to_params(whole_region);
            assert_eq!(params["text_attention_in_mask"], json!(expected), "setting {setting}, whole {whole_region}");
            assert_eq!(settings.text_attention_in_mask, setting, "building params never rewrites the choice");
        }
        for (setting, painted, expected) in
            [(true, true, true), (true, false, false), (false, true, false), (false, false, false)]
        {
            let mut settings = runnable_settings();
            settings.text_attention_in_mask = setting;
            let params = settings.normalized().to_profiles_params(painted);
            assert_eq!(params["text_attention_in_mask"], json!(expected), "setting {setting}, painted {painted}");
            assert_eq!(params["whole_region"], json!(false), "a forecast is never a whole-region request");
        }
    }

    /// The flag defaults ON, for a fresh document AND for one written before the field
    /// existed, and `normalized()` carries it through unchanged in both states.
    #[test]
    fn text_attention_in_mask_defaults_on_and_survives_normalization() {
        assert!(Flux2KleinSettings::default().text_attention_in_mask);
        let old: Flux2KleinSettings =
            serde_json::from_value(json!({ "prompt": "remove the text" })).expect("an old document still loads");
        assert!(old.text_attention_in_mask, "a document without the field reads as on");
        let off = Flux2KleinSettings { text_attention_in_mask: false, ..Flux2KleinSettings::default() };
        assert!(!off.normalized().text_attention_in_mask);
        let round: Flux2KleinSettings =
            serde_json::from_str(&serde_json::to_string(&off).expect("serializes")).expect("round-trips");
        assert!(!round.text_attention_in_mask, "an explicit `false` is kept");
    }

    /// The post-processing values live in the flattened `postprocess` block, but the FILE keeps
    /// the flat keys every older build wrote: an old document loads its values from them, a
    /// document without them reads the shared defaults, and saving writes the same flat keys
    /// (with the same values) back — no nested object, no renamed key.
    #[test]
    fn an_old_flat_post_processing_document_loads_its_values_and_saves_the_same_keys() {
        let old = json!({
            "prompt": "remove the text",
            "mask_dilate_px": 9,
            "mask_feather_px": 5,
            "color_match": false,
            "color_match_local_pct": 35,
            "color_match_scale_px": 48
        });
        let loaded = loaded_settings(&old);
        assert_eq!(loaded.mask_dilate_px, 9);
        assert_eq!(
            loaded.postprocess,
            PostprocessSettings { mask_feather_px: 5, color_match: false, color_match_local_pct: 35, color_match_scale_px: 48 },
            "every flat key lands in the flattened block"
        );
        let written = serde_json::to_value(&loaded).expect("serialize");
        let object = written.as_object().expect("the settings serialize as an object");
        assert!(object.get("postprocess").is_none(), "the block is flattened, never nested");
        for key in ["mask_dilate_px", "mask_feather_px", "color_match", "color_match_local_pct", "color_match_scale_px"] {
            assert_eq!(object.get(key), old.get(key), "`{key}` is written back flat with its value");
        }
        let without = loaded_settings(&json!({ "prompt": "remove the text" }));
        assert_eq!(without.postprocess, PostprocessSettings::default(), "absent keys read the shared defaults");
        let partial = loaded_settings(&json!({ "color_match": false }));
        assert_eq!(partial.postprocess, PostprocessSettings { color_match: false, ..PostprocessSettings::default() }, "one key does not cost the others");
        // A wrong type inside the flattened block is still a damaged document (moved aside).
        assert!(settings_from_json(&json!({ "mask_feather_px": "wide" })).is_err());
    }

    /// «Энкодер на CPU» defaults ON — for a fresh document AND for one written before the
    /// field existed — survives normalization and a round trip, and travels as
    /// `encoder_on_cpu` in the run `params` AND in the forecast's.
    #[test]
    fn encoder_on_cpu_defaults_on_and_reaches_the_run_and_the_forecast() {
        assert!(Flux2KleinSettings::default().encoder_on_cpu);
        let old = loaded_settings(&json!({ "prompt": "remove the text", "vram_step": "half" }));
        assert!(old.encoder_on_cpu, "a document without the field reads as on");
        for flag in [true, false] {
            let settings = Flux2KleinSettings { encoder_on_cpu: flag, ..runnable_settings() };
            let round: Flux2KleinSettings =
                serde_json::from_str(&serde_json::to_string(&settings).expect("serializes")).expect("round-trips");
            assert_eq!(round.encoder_on_cpu, flag, "an explicit value is kept");
            let normalized = settings.normalized();
            assert_eq!(normalized.encoder_on_cpu, flag);
            for whole_region in [false, true] {
                assert_eq!(normalized.to_params(whole_region)["encoder_on_cpu"], json!(flag), "run params");
            }
            for painted in [false, true] {
                assert_eq!(normalized.to_profiles_params(painted)["encoder_on_cpu"], json!(flag), "forecast params");
            }
        }
    }

    /// A settings file written by an older build carries keys nothing maps to any more:
    /// `whole_region` (the mode is derived from the mask), `max_sequence_length` (pinned to
    /// [`FLUX2_MAX_SEQ`]) and `brush_radius` (the brush belongs to the host). The struct does
    /// not use `deny_unknown_fields`, so the document must load with every other setting
    /// intact rather than falling back to defaults.
    #[test]
    fn a_settings_file_carrying_removed_fields_still_loads() {
        let old = json!({
            "text_encoder_path": "/a",
            "transformer_path": "/b",
            "vae_path": "/c",
            "placement": "sequential_cpu_offload",
            "whole_region": true,
            "max_sequence_length": 128,
            "brush_radius": 64,
            "mask_dilate_px": 8
        });
        let migrated = loaded_settings(&old);
        assert_eq!(migrated.text_encoder_path, "/a", "the removed keys must not cost the rest");
        assert_eq!(migrated.transformer_path, "/b");
        assert_eq!(migrated.vae_path, "/c");
        assert_eq!(migrated.mask_dilate_px, 8);
        // The neighbouring placement migration still runs on such a document.
        assert_eq!(migrated.step_choice(), StepChoice { vram: VramStep::Min, ram: RamStep::Keep });
        // A stale `max_sequence_length` cannot lower what goes on the wire any more.
        assert_eq!(
            migrated.normalized().to_params(false)["max_sequence_length"],
            json!(FLUX2_MAX_SEQ)
        );

        // The same document without those keys loads identically: they mean nothing now.
        let mut without = old.clone();
        if let Some(map) = without.as_object_mut() {
            map.remove("whole_region");
            map.remove("max_sequence_length");
            map.remove("brush_radius");
        }
        let plain = loaded_settings(&without);
        assert_eq!(
            serde_json::to_value(&migrated).expect("serialize"),
            serde_json::to_value(&plain).expect("serialize"),
            "a stale removed key may not change a single loaded setting"
        );
    }

    /// The toggle is a persisted setting, and it must NOT leak into `params`: the derived
    /// encoder path already names the chosen files.
    #[test]
    fn the_encoder_toggle_persists_but_never_enters_the_generation_params() {
        let settings = Flux2KleinSettings {
            uncensored_text_encoder: true,
            ..runnable_settings()
        };
        let stored = serde_json::to_value(&settings).expect("settings serialize");
        assert_eq!(stored["uncensored_text_encoder"], json!(true));
        let reloaded = loaded_settings(&stored);
        assert!(reloaded.uncensored_text_encoder);
        assert!(settings.normalized().uncensored_text_encoder);

        let params = settings.normalized().to_params(false);
        assert!(
            params.get("uncensored").is_none() && params.get("uncensored_text_encoder").is_none(),
            "the toggle selects a path, it is not a parameter of the request"
        );
    }

    // -----------------------------------------------------------------------------------
    // Source mode: manual paths vs. the downloaded copy
    // -----------------------------------------------------------------------------------

    /// The mode must survive the settings file, and — the case that matters for an update —
    /// a file that predates the field must load as MANUAL.
    #[test]
    fn the_source_mode_round_trips_and_an_absent_field_loads_as_manual() {
        for mode in Flux2SourceMode::all() {
            assert_eq!(Flux2SourceMode::from_wire(mode.wire()), mode);
        }
        // The exact tokens, spelled out: they are what sits in the user's file.
        assert_eq!(Flux2SourceMode::from_wire("manual"), Flux2SourceMode::Manual);
        assert_eq!(
            Flux2SourceMode::from_wire("download"),
            Flux2SourceMode::Download
        );

        let settings = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            ..runnable_settings()
        };
        let stored = serde_json::to_value(&settings).expect("settings serialize");
        assert_eq!(stored["source_mode"], json!("download"));
        assert_eq!(
            loaded_settings(&stored).source_mode(),
            Flux2SourceMode::Download
        );

        // A settings file written before the switch existed carries hand-entered paths.
        // Loading it as `download` would hide them behind an empty download block and read
        // as an update that broke the install, so the fallback is not negotiable.
        let mut legacy = stored.clone();
        legacy
            .as_object_mut()
            .expect("the document is an object")
            .remove("source_mode");
        assert!(legacy.get("source_mode").is_none());
        assert_eq!(
            loaded_settings(&legacy).source_mode(),
            Flux2SourceMode::Manual,
            "a file without the field must load as manual"
        );
        // So must an unrecognised token and the compiled-in default.
        assert_eq!(Flux2SourceMode::from_wire("auto"), Flux2SourceMode::Manual);
        assert_eq!(Flux2SourceMode::from_wire(""), Flux2SourceMode::Manual);
        assert_eq!(FLUX2_DEFAULT_SOURCE_MODE, Flux2SourceMode::Manual);
        assert_eq!(
            Flux2KleinSettings::default().source_mode(),
            Flux2SourceMode::Manual
        );
    }

    /// The whole point of the mode: in download mode the paths are DERIVED and the manual
    /// fields are neither read nor written.
    #[test]
    fn download_mode_derives_the_paths_and_never_reads_the_manual_fields() {
        // The configuration from the user's screenshot: a hand-built tree, including a
        // hand-picked uncensored encoder somewhere else entirely.
        let hand_made = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Manual.wire().to_string(),
            text_encoder_path: "/home/u/sd_models/qwen3-uncensored".to_string(),
            transformer_path: "/home/u/sd_models/flux2.safetensors".to_string(),
            vae_path: "/home/u/sd_models/vae".to_string(),
            ..runnable_settings()
        };
        let manual = hand_made.effective_paths();
        assert_eq!(manual.text_encoder, "/home/u/sd_models/qwen3-uncensored");
        assert_eq!(manual.transformer, "/home/u/sd_models/flux2.safetensors");
        assert_eq!(manual.vae, "/home/u/sd_models/vae");

        // Flip to download: the effective paths become the derived ones and NONE of the
        // three hand-made values survives into them.
        let downloaded = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            ..hand_made.clone()
        };
        let derived = downloaded.effective_paths();
        assert_eq!(derived.transformer, package_path(Flux2Variant::Klein9B, "flux-2-klein-9b.safetensors"));
        assert_eq!(derived.vae, config::flux2_klein_vae_dir(Flux2Variant::Klein9B).to_string_lossy());
        assert_eq!(
            derived.text_encoder,
            package_path(Flux2Variant::Klein9B, "text_encoder"),
            "the toggle is off, so the official encoder is derived"
        );
        for hand in [&manual.text_encoder, &manual.transformer, &manual.vae] {
            assert!(
                ![&derived.text_encoder, &derived.transformer, &derived.vae]
                    .into_iter()
                    .any(|d| d == hand),
                "a manual path leaked into the derived set"
            );
        }

        // The encoder toggle selects the derived encoder, and only it.
        let uncensored = Flux2KleinSettings {
            uncensored_text_encoder: true,
            ..downloaded.clone()
        };
        let derived_uncensored = uncensored.effective_paths();
        assert_eq!(derived_uncensored.text_encoder, package_path(Flux2Variant::Klein9B, "text_encoder_uncensored"));
        assert_ne!(derived_uncensored.text_encoder, derived.text_encoder);
        assert_eq!(derived_uncensored.transformer, derived.transformer);
        assert_eq!(derived_uncensored.vae, derived.vae);

        // The manual fields are untouched throughout, so flipping back restores exactly
        // what the user had.
        assert_eq!(
            uncensored.text_encoder_path,
            "/home/u/sd_models/qwen3-uncensored"
        );
        let back = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Manual.wire().to_string(),
            ..uncensored
        };
        assert_eq!(back.effective_paths(), manual);
    }

    /// A run must send the paths its MODE prescribes — the single place the two modes could
    /// drift, so it is pinned on the WIRE rather than on the accessor.
    #[test]
    fn a_run_sends_the_paths_its_mode_prescribes() {
        let hand_made = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Manual.wire().to_string(),
            text_encoder_path: "/home/u/sd_models/qwen3".to_string(),
            transformer_path: "/home/u/sd_models/flux2.safetensors".to_string(),
            vae_path: "/home/u/sd_models/vae".to_string(),
            ..runnable_settings()
        };
        let manual_params = hand_made.normalized().to_params(false);
        assert_eq!(
            manual_params["text_encoder_path"],
            json!("/home/u/sd_models/qwen3")
        );
        assert_eq!(
            manual_params["transformer_path"],
            json!("/home/u/sd_models/flux2.safetensors")
        );
        assert_eq!(manual_params["vae_path"], json!("/home/u/sd_models/vae"));

        let downloaded = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            uncensored_text_encoder: true,
            ..hand_made.clone()
        };
        let derived_params = downloaded.normalized().to_params(false);
        assert_eq!(derived_params["text_encoder_path"], json!(package_path(Flux2Variant::Klein9B, "text_encoder_uncensored")));
        assert_eq!(derived_params["transformer_path"], json!(package_path(Flux2Variant::Klein9B, "flux-2-klein-9b.safetensors")));
        assert_eq!(
            derived_params["vae_path"],
            json!(config::flux2_klein_vae_dir(Flux2Variant::Klein9B).to_string_lossy())
        );
        // And the user's own paths never reach the backend in that mode.
        assert!(
            !derived_params.to_string().contains("/home/u/sd_models/"),
            "a manual path reached the wire in download mode"
        );

        // The run gate answers about the EFFECTIVE paths too: blank manual fields must not
        // block a run in download mode, where the derived ones are non-empty.
        let empty_fields = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            text_encoder_path: String::new(),
            transformer_path: String::new(),
            vae_path: String::new(),
            ..runnable_settings()
        };
        assert!(
            flux2_run_block_reason(&empty_fields, None, None).is_none(),
            "download mode derives its paths, so blank manual fields must not block a run"
        );
        // In manual mode the same blank fields do block it.
        let manual_blank = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Manual.wire().to_string(),
            ..empty_fields
        };
        assert!(flux2_run_block_reason(&manual_blank, None, None).is_some());
    }

    /// The 9B engine keeps the file it always had, byte for byte in name and in shape: a
    /// user with a live `flux2_klein_settings.json` full of hand-entered paths must get no
    /// migration, no reset and no new file.
    #[test]
    fn the_9b_settings_file_and_its_shape_are_unchanged() {
        assert_eq!(
            config::flux2_klein_settings_path(Flux2Variant::Klein9B),
            config::data_dir().join("flux2_klein_settings.json"),
            "the historic settings path must not move"
        );

        // Every field the previous build persisted that is still a setting. A field that
        // disappears from this list silently drops a user setting on the next save. The five
        // retired memory-placement fields left it on purpose: they migrate into the two step
        // axes once (`legacy_memory_steps`) and are written out without them.
        const HISTORIC_KEYS: [&str; 21] = [
            "source_mode",
            "text_encoder_path",
            "uncensored_text_encoder",
            "transformer_path",
            "vae_path",
            "prompt",
            "source_prompt",
            "translate_prompt",
            "mt_service",
            "source_lang",
            "steps",
            "guidance_scale",
            "strength",
            "seed",
            "use_seed",
            "dtype",
            "vae_tiling",
            "text_encoder_fp8",
            "mask_dilate_px",
            "mask_feather_px",
            "color_match",
        ];
        let document =
            serde_json::to_value(Flux2KleinSettings::default()).expect("serialize the defaults");
        let object = document.as_object().expect("the settings serialize as an object");
        for key in HISTORIC_KEYS {
            assert!(object.contains_key(key), "the settings file lost `{key}`");
        }
        let added: Vec<&str> = object
            .keys()
            .map(String::as_str)
            .filter(|key| !HISTORIC_KEYS.contains(key))
            .collect();
        // Key order (`serde_json` without `preserve_order`). All are ADDITIVE: the variant
        // came with the second engine, «Внимание только к маске» with the pinned run
        // options, the two step axes with the large-model manager and «Энкодер на CPU» with
        // the encoder placement, the two local colour-match values with the local match, the
        // two weight formats with the GGUF quants (the encoder source stays the historic
        // `uncensored_text_encoder` bool, no new key), the «Нет» stash with the shared prompt
        // library, the runtime switch and the sd.cpp options with Stable-Diffusion.cpp; a
        // document without any of them still loads (`#[serde(default)]`). The runtime
        // `installed_formats` is never written (`serde(skip)`).
        assert_eq!(
            added,
            ["color_match_local_pct", "color_match_scale_px", "encoder_on_cpu", "ram_step", "runtime", "sdcpp", "text_attention_in_mask", "text_encoder_format", "transformer_format", "uncached_prompt", "variant", "vram_step"],
            "only the variant, the run option, the two step axes, the encoder placement, the local colour match, the two weight formats, the «Нет» stash, the runtime and the sd.cpp options were added"
        );
        for retired in ["placement", "low_cpu_mem_usage", "vae_slicing", "unload_transformer_before_vae", "unload_text_encoder_after_encode"] {
            assert!(!object.contains_key(retired), "the retired `{retired}` must not be written back");
        }

        // A document written by the previous build carries no `variant` and must load as
        // the 9B engine with every value the user typed intact.
        let legacy = loaded_settings(&json!({
            "source_mode": "manual",
            "text_encoder_path": "/home/u/sd_models/qwen3",
            "transformer_path": "/home/u/sd_models/flux2.safetensors",
            "vae_path": "/home/u/sd_models/vae",
            "prompt": "a red balloon",
            "steps": 6,
        }));
        assert_eq!(legacy.variant(), Flux2Variant::Klein9B);
        assert_eq!(legacy.text_encoder_path, "/home/u/sd_models/qwen3");
        assert_eq!(legacy.transformer_path, "/home/u/sd_models/flux2.safetensors");
        assert_eq!(legacy.vae_path, "/home/u/sd_models/vae");
        assert_eq!(legacy.prompt, "a red balloon");
        assert_eq!(legacy.steps, 6);
        // And it is written back into the file it came from, never into the 4B one.
        assert_eq!(
            config::flux2_klein_settings_path(legacy.variant()),
            config::flux2_klein_settings_path(Flux2Variant::Klein9B)
        );
    }

    /// A document that names the 4B checkpoint derives ITS paths, and the two engines'
    /// derived sets never overlap — a shared one would make each download overwrite the
    /// other's model.
    #[test]
    fn the_variant_keys_the_derived_paths_and_the_settings_file() {
        let mut per_variant = Vec::new();
        for variant in Flux2Variant::all() {
            let settings = Flux2KleinSettings {
                variant: variant.wire().to_string(),
                source_mode: Flux2SourceMode::Download.wire().to_string(),
                ..runnable_settings()
            };
            assert_eq!(settings.variant(), variant);
            let derived = settings.effective_paths();
            for path in [&derived.text_encoder, &derived.transformer, &derived.vae] {
                assert!(
                    path.contains(variant.dir_name()),
                    "`{path}` is not under the {} model directory",
                    variant.dir_name()
                );
            }
            per_variant.push(derived);
        }
        assert_ne!(per_variant[0], per_variant[1]);
        // An unknown token in a hand-edited file reads as 9B rather than as a third model.
        let bogus = Flux2KleinSettings {
            variant: "klein-42b".to_string(),
            ..Flux2KleinSettings::default()
        };
        assert_eq!(bogus.variant(), Flux2Variant::Klein9B);
        assert_eq!(bogus.normalized().variant, "9b");
    }

    /// The 4B checkpoint has no uncensored encoder published for it and the backend refuses
    /// the pairing, so the panel never draws the toggle — and a hand-edited settings file
    /// that sets the flag anyway must not reach a path or the wire with it.
    #[test]
    fn the_4b_variant_never_carries_the_uncensored_encoder() {
        assert_eq!(Flux2EncoderSource::offered_for(Flux2Variant::Klein4B), &[Flux2EncoderSource::Stock]);
        let forced = Flux2KleinSettings {
            variant: Flux2Variant::Klein4B.wire().to_string(),
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            uncensored_text_encoder: true,
            ..runnable_settings()
        };
        assert_eq!(
            forced.encoder_source(),
            Flux2EncoderSource::Stock,
            "the flag is inert for a variant that has no uncensored encoder"
        );
        assert!(
            !forced.normalized().uncensored_text_encoder,
            "the refused pairing must never reach the wire"
        );
        assert_eq!(
            forced.effective_paths().text_encoder,
            package_path(Flux2Variant::Klein4B, "text_encoder"),
            "the derived encoder stays the official one"
        );
        // The 9B engine is unaffected: there the flag still selects the other directory.
        let allowed = Flux2KleinSettings {
            variant: Flux2Variant::Klein9B.wire().to_string(),
            ..forced
        };
        assert_eq!(allowed.encoder_source(), Flux2EncoderSource::Uncensored);
        assert!(allowed.normalized().uncensored_text_encoder);
        assert_eq!(allowed.effective_paths().text_encoder, package_path(Flux2Variant::Klein9B, "text_encoder_uncensored"));
    }

    // -----------------------------------------------------------------------------------
    // Weight formats and encoder source (catalog packages, protocol 21)
    // -----------------------------------------------------------------------------------

    /// `name` (a `/`-separated relative path) inside `variant`'s package root, as a string.
    fn package_path(variant: Flux2Variant, name: &str) -> String {
        name.split('/').fold(config::flux2_klein_dir(variant), |path, part| path.join(part)).to_string_lossy().to_string()
    }

    /// A document of `variant` in download mode with the given formats and encoder source.
    fn download_settings(variant: Flux2Variant, source: Flux2EncoderSource, transformer: Flux2WeightFormat, text_encoder: Flux2WeightFormat) -> Flux2KleinSettings {
        Flux2KleinSettings {
            variant: variant.wire().to_string(),
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            uncensored_text_encoder: source == Flux2EncoderSource::Uncensored,
            transformer_format: transformer.wire().to_string(),
            text_encoder_format: text_encoder.wire().to_string(),
            ..runnable_settings()
        }
    }

    /// A 4B document in download mode with the given formats.
    fn catalog_settings(transformer: Flux2WeightFormat, text_encoder: Flux2WeightFormat) -> Flux2KleinSettings {
        download_settings(Flux2Variant::Klein4B, Flux2EncoderSource::Stock, transformer, text_encoder)
    }

    /// The catalog path of a component, as the catalog itself answers it.
    fn catalog_path(variant: Flux2Variant, axis: Flux2WeightAxis, source: Flux2EncoderSource, format: Flux2WeightFormat, layout: Flux2TransformerLayout) -> String {
        flux2_klein_weights_path(&config::side_models_dir(), variant, axis, source, format, layout)
            .expect("every offered format resolves")
            .to_string_lossy()
            .to_string()
    }

    /// The (axis, source) pairs `variant` offers, the transformer once (it ignores the source).
    fn component_choices(variant: Flux2Variant) -> Vec<(Flux2WeightAxis, Flux2EncoderSource)> {
        let mut choices = vec![(Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock)];
        choices.extend(Flux2EncoderSource::offered_for(variant).iter().map(|&source| (Flux2WeightAxis::TextEncoder, source)));
        choices
    }

    /// A file written before the formats existed reads bf16 on both axes; a persisted id
    /// round-trips; an unknown id, or one the variant does not offer for the current source,
    /// normalizes to bf16.
    #[test]
    fn an_old_document_reads_bf16_and_a_bad_format_id_normalizes_to_bf16() {
        let old = loaded_settings(&json!({ "variant": "4b", "prompt": "remove the text" }));
        for axis in Flux2WeightAxis::all() {
            assert_eq!(old.weight_format(axis), Flux2WeightFormat::Bf16, "{axis:?}");
        }
        assert_eq!(old.normalized().transformer_format, "bf16");
        assert_eq!(old.normalized().text_encoder_format, "bf16");

        let chosen = catalog_settings(Flux2WeightFormat::Q4KM, Flux2WeightFormat::Q6K);
        let reloaded = loaded_settings(&serde_json::to_value(&chosen).expect("serialize"));
        assert_eq!(reloaded.weight_format(Flux2WeightAxis::Transformer), Flux2WeightFormat::Q4KM);
        assert_eq!(reloaded.weight_format(Flux2WeightAxis::TextEncoder), Flux2WeightFormat::Q6K);

        let bad = Flux2KleinSettings {
            transformer_format: "Q8_0".to_string(),
            text_encoder_format: "iq2_xs".to_string(),
            ..chosen.clone()
        };
        assert_eq!(bad.normalized().transformer_format, "bf16", "ids are lowercase wire ids");
        assert_eq!(bad.normalized().text_encoder_format, "bf16", "an unknown id");
        // 9B offers the GGUF quants too: the same ids are kept there.
        let nine = Flux2KleinSettings { variant: Flux2Variant::Klein9B.wire().to_string(), ..chosen };
        assert_eq!(nine.weight_format(Flux2WeightAxis::Transformer), Flux2WeightFormat::Q4KM);
        assert_eq!(nine.normalized().text_encoder_format, "q6_k");
        // …except where the uncensored encoder offers no such quant: `q5_k_m` reads as bf16.
        let uncensored = download_settings(Flux2Variant::Klein9B, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Bf16, Flux2WeightFormat::Q5KM);
        assert_eq!(uncensored.weight_format(Flux2WeightAxis::TextEncoder), Flux2WeightFormat::Bf16);
    }

    /// An old 9B settings file with the toggle on and no format keys — every install made by
    /// the backend's former download of the uncensored encoder — runs the uncensored bf16
    /// encoder from `text_encoder_uncensored/`, exactly the folder that download wrote, and
    /// sends it as the selected bf16 candidate.
    #[test]
    fn an_old_9b_uncensored_document_runs_the_uncensored_bf16_folder() {
        let old = loaded_settings(&json!({ "source_mode": "download", "uncensored_text_encoder": true, "prompt": "remove the text" }));
        assert_eq!(old.variant(), Flux2Variant::Klein9B);
        assert_eq!(old.encoder_source(), Flux2EncoderSource::Uncensored);
        assert_eq!(old.weight_format(Flux2WeightAxis::TextEncoder), Flux2WeightFormat::Bf16);
        let paths = old.effective_paths();
        assert_eq!(paths.text_encoder, package_path(Flux2Variant::Klein9B, "text_encoder_uncensored"));
        let params = old.normalized().to_params(false);
        assert_eq!(params["text_encoder_format"], json!("bf16"));
        assert_eq!(params["text_encoder_formats"], json!([{ "id": "bf16", "path": paths.text_encoder }]));
    }

    /// bf16 in download mode derives EXACTLY the folders an existing install has, so it stays
    /// valid without a re-download: 4B's diffusers folders, 9B's single file (or the legacy
    /// `transformer/` folder once the probe found that layout), and both encoder folders.
    #[test]
    fn bf16_catalog_paths_are_the_existing_folders() {
        let four = catalog_settings(Flux2WeightFormat::Bf16, Flux2WeightFormat::Bf16).effective_paths();
        assert_eq!(four.transformer, package_path(Flux2Variant::Klein4B, "transformer"));
        assert_eq!(four.text_encoder, package_path(Flux2Variant::Klein4B, "text_encoder"));
        assert_eq!(four.vae, config::flux2_klein_vae_dir(Flux2Variant::Klein4B).to_string_lossy());

        let mut nine = download_settings(Flux2Variant::Klein9B, Flux2EncoderSource::Stock, Flux2WeightFormat::Bf16, Flux2WeightFormat::Bf16);
        assert_eq!(nine.effective_paths().transformer, package_path(Flux2Variant::Klein9B, "flux-2-klein-9b.safetensors"), "before the probe: the download target");
        nine.installed_formats.transformer_layout = Flux2TransformerLayout::LegacyDiffusers;
        let legacy = nine.effective_paths();
        assert_eq!(legacy.transformer, package_path(Flux2Variant::Klein9B, "transformer"), "the backend's former download layout");
        assert_eq!(legacy.text_encoder, package_path(Flux2Variant::Klein9B, "text_encoder"), "the layout moves only the bf16 transformer");
        // A GGUF transformer ignores the layout.
        nine.transformer_format = Flux2WeightFormat::Q80.wire().to_string();
        assert_eq!(nine.effective_paths().transformer, package_path(Flux2Variant::Klein9B, "transformer_gguf/flux-2-klein-9b-Q8_0.gguf"));
    }

    /// Every offered (variant, component, source, format, layout) points its component at the
    /// catalog's path and leaves the other component and the VAE alone.
    #[test]
    fn every_offered_format_has_a_catalog_path() {
        for variant in Flux2Variant::all() {
            for layout in [Flux2TransformerLayout::Primary, Flux2TransformerLayout::LegacyDiffusers] {
                for (axis, source) in component_choices(variant) {
                    for &format in Flux2WeightFormat::offered_for(variant, axis, source) {
                        let (transformer, text_encoder) = match axis {
                            Flux2WeightAxis::Transformer => (format, Flux2WeightFormat::Bf16),
                            Flux2WeightAxis::TextEncoder => (Flux2WeightFormat::Bf16, format),
                        };
                        let mut settings = download_settings(variant, source, transformer, text_encoder);
                        settings.installed_formats.transformer_layout = layout;
                        let paths = settings.effective_paths();
                        let (this, other, other_axis) = match axis {
                            Flux2WeightAxis::Transformer => (&paths.transformer, &paths.text_encoder, Flux2WeightAxis::TextEncoder),
                            Flux2WeightAxis::TextEncoder => (&paths.text_encoder, &paths.transformer, Flux2WeightAxis::Transformer),
                        };
                        let case = format!("{variant:?} {axis:?} {source:?} {format:?} {layout:?}");
                        assert_eq!(*this, catalog_path(variant, axis, source, format, layout), "{case}");
                        assert!(this.contains(variant.dir_name()), "{case}: {this}");
                        assert_eq!(this.ends_with(".gguf"), format.is_gguf(), "{case}: {this}");
                        assert_eq!(*other, catalog_path(variant, other_axis, source, Flux2WeightFormat::Bf16, layout), "{case}: the other component is untouched");
                        assert_eq!(paths.vae, config::flux2_klein_vae_dir(variant).to_string_lossy(), "{case}: the VAE has no format axis");
                    }
                }
            }
        }
    }

    /// Switching the encoder source keeps an encoder format the new source offers and maps one
    /// it does not onto the next smaller quant (`closest_offered`), reporting the change; the
    /// transformer format is never touched.
    #[test]
    fn switching_the_encoder_source_maps_the_encoder_format_to_the_closest_offered() {
        let mut settings = download_settings(Flux2Variant::Klein9B, Flux2EncoderSource::Stock, Flux2WeightFormat::Q6K, Flux2WeightFormat::Q5KM);
        assert_eq!(settings.weight_format(Flux2WeightAxis::TextEncoder), Flux2WeightFormat::Q5KM);
        assert_eq!(
            settings.set_encoder_source(Flux2EncoderSource::Uncensored),
            Some((Flux2WeightFormat::Q5KM, Flux2WeightFormat::Q4KM)),
            "the uncensored encoder has no Q5_K_M: the next smaller quant"
        );
        assert_eq!(settings.encoder_source(), Flux2EncoderSource::Uncensored);
        assert!(settings.uncensored_text_encoder);
        assert_eq!(settings.text_encoder_format, "q4_k_m");
        assert_eq!(settings.weight_format(Flux2WeightAxis::Transformer), Flux2WeightFormat::Q6K, "the transformer is not the encoder");
        assert_eq!(settings.set_encoder_source(Flux2EncoderSource::Stock), None, "Q4_K_M is offered for the stock encoder too");
        assert_eq!(settings.text_encoder_format, "q4_k_m");
        assert_eq!(settings.set_encoder_source(Flux2EncoderSource::Uncensored), None);
        // An unknown stored id reads as bf16, which every source offers.
        settings.text_encoder_format = "iq2_xs".to_string();
        assert_eq!(settings.set_encoder_source(Flux2EncoderSource::Stock), None, "bf16 is offered: nothing to report");
        assert_eq!(settings.text_encoder_format, "bf16");
        // The encoder path follows the source.
        let stock = settings.effective_paths().text_encoder;
        settings.set_encoder_source(Flux2EncoderSource::Uncensored);
        assert_eq!(stock, package_path(Flux2Variant::Klein9B, "text_encoder"));
        assert_eq!(settings.effective_paths().text_encoder, package_path(Flux2Variant::Klein9B, "text_encoder_uncensored"));
    }

    /// Manual mode sends none of the four format fields (a picked `.gguf` path travels as-is
    /// and the backend derives its format); download mode of EITHER variant sends them.
    #[test]
    fn manual_mode_sends_no_format_fields_and_download_mode_of_both_variants_does() {
        let format_keys = ["transformer_format", "transformer_formats", "text_encoder_format", "text_encoder_formats"];
        let manual = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Manual.wire().to_string(),
            transformer_path: "/home/u/models/flux-2-klein-4b-Q4_K_M.gguf".to_string(),
            text_encoder_path: "/home/u/models/FLUX.2-klein-4B/text_encoder_gguf/Qwen3-4B-Q8_0.gguf".to_string(),
            ..catalog_settings(Flux2WeightFormat::Q4KM, Flux2WeightFormat::Q80)
        };
        assert!(manual.format_candidates(Flux2WeightAxis::TextEncoder).is_empty());
        let manual_params = manual.normalized().to_params(false);
        assert_eq!(manual_params["transformer_path"], json!("/home/u/models/flux-2-klein-4b-Q4_K_M.gguf"));
        assert_eq!(manual_params["text_encoder_path"], json!("/home/u/models/FLUX.2-klein-4B/text_encoder_gguf/Qwen3-4B-Q8_0.gguf"));
        for params in [manual_params, manual.normalized().to_profiles_params(false)] {
            for key in format_keys {
                assert!(params.get(key).is_none(), "`{key}` must not be sent: {params}");
            }
        }

        let nine = download_settings(Flux2Variant::Klein9B, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Q80, Flux2WeightFormat::Q6K).normalized();
        for params in [nine.to_params(false), nine.to_profiles_params(true)] {
            assert_eq!(params["transformer_format"], json!("q8_0"));
            assert_eq!(params["transformer_formats"], json!([{ "id": "q8_0", "path": params["transformer_path"] }]));
            assert_eq!(params["text_encoder_format"], json!("q6_k"));
            assert_eq!(params["text_encoder_formats"], json!([{ "id": "q6_k", "path": params["text_encoder_path"] }]));
            assert_eq!(
                params["text_encoder_path"],
                json!(package_path(Flux2Variant::Klein9B, "text_encoder_uncensored_gguf/flux2-klein-9b-uncensored-q6_k.gguf"))
            );
        }
    }

    /// Download mode sends the selected id and the candidates: the installed formats of the
    /// current encoder source plus the selected one, best quality first, each with its
    /// catalog path, the selected entry's path equal to the component path. Ids match the wire
    /// grammar.
    #[test]
    fn catalog_download_mode_sends_the_selected_format_and_its_candidates() {
        let path4 = |axis, format| catalog_path(Flux2Variant::Klein4B, axis, Flux2EncoderSource::Stock, format, Flux2TransformerLayout::Primary);
        // Nothing probed yet: each list holds the selected format alone.
        let fresh = catalog_settings(Flux2WeightFormat::Bf16, Flux2WeightFormat::Bf16).normalized();
        let params = fresh.to_params(false);
        assert_eq!(params["transformer_format"], json!("bf16"));
        assert_eq!(params["transformer_formats"], json!([{ "id": "bf16", "path": params["transformer_path"] }]));
        assert_eq!(params["text_encoder_format"], json!("bf16"));
        assert_eq!(params["text_encoder_formats"], json!([{ "id": "bf16", "path": params["text_encoder_path"] }]));

        let mut settings = catalog_settings(Flux2WeightFormat::Q4KM, Flux2WeightFormat::Q6K);
        for format in [Flux2WeightFormat::Q4KM, Flux2WeightFormat::Bf16, Flux2WeightFormat::Q80] {
            settings.installed_formats.insert(Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock, format);
        }
        settings.installed_formats.insert(Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, Flux2WeightFormat::Bf16);
        // An installed format of the OTHER source is not a candidate of this one.
        settings.installed_formats.insert(Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Q80);
        let normalized = settings.normalized();
        for params in [normalized.to_params(false), normalized.to_params(true), normalized.to_profiles_params(true)] {
            assert_eq!(params["transformer_format"], json!("q4_k_m"));
            assert_eq!(
                params["transformer_formats"],
                json!([
                    { "id": "bf16", "path": path4(Flux2WeightAxis::Transformer, Flux2WeightFormat::Bf16) },
                    { "id": "q8_0", "path": path4(Flux2WeightAxis::Transformer, Flux2WeightFormat::Q80) },
                    { "id": "q4_k_m", "path": params["transformer_path"] },
                ]),
                "installed formats, best first, insertion order irrelevant"
            );
            assert_eq!(params["text_encoder_format"], json!("q6_k"));
            assert_eq!(
                params["text_encoder_formats"],
                json!([
                    { "id": "bf16", "path": path4(Flux2WeightAxis::TextEncoder, Flux2WeightFormat::Bf16) },
                    { "id": "q6_k", "path": params["text_encoder_path"] },
                ]),
                "the selected format is a candidate even before its files are installed"
            );
            for list in ["transformer_formats", "text_encoder_formats"] {
                for entry in params[list].as_array().expect("a list") {
                    let id = entry["id"].as_str().expect("a string id");
                    assert!((1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'), "{id}");
                }
            }
        }
    }

    /// D8: a GGUF encoder in effect sends fp8 off; the stored choice is kept, and nothing
    /// else (a GGUF transformer, manual mode) touches the flag.
    #[test]
    fn a_gguf_encoder_excludes_fp8_on_the_wire_only() {
        let gguf_encoder = Flux2KleinSettings { text_encoder_fp8: true, ..catalog_settings(Flux2WeightFormat::Bf16, Flux2WeightFormat::Q80) };
        assert!(gguf_encoder.gguf_text_encoder_in_effect());
        assert!(!gguf_encoder.normalized().text_encoder_fp8);
        assert_eq!(gguf_encoder.normalized().to_params(false)["text_encoder_fp8"], json!(false));
        assert!(gguf_encoder.text_encoder_fp8, "the user's choice survives for bf16");

        let back_to_bf16 = Flux2KleinSettings { text_encoder_format: "bf16".to_string(), ..gguf_encoder.clone() };
        assert!(back_to_bf16.normalized().text_encoder_fp8);
        let gguf_transformer = Flux2KleinSettings { text_encoder_fp8: true, ..catalog_settings(Flux2WeightFormat::Q4KM, Flux2WeightFormat::Bf16) };
        assert!(gguf_transformer.normalized().text_encoder_fp8, "only the ENCODER format excludes fp8");
        let manual = Flux2KleinSettings { source_mode: Flux2SourceMode::Manual.wire().to_string(), ..gguf_encoder.clone() };
        assert!(manual.normalized().text_encoder_fp8, "the format setting is inert in manual mode");
        let nine = Flux2KleinSettings { variant: Flux2Variant::Klein9B.wire().to_string(), ..gguf_encoder };
        assert!(!nine.normalized().text_encoder_fp8, "9B offers GGUF encoders too");
    }

    /// The installed set is runtime state: never written to the file, and an empty side-models
    /// root probes as nothing installed with the `Primary` layout. The probe root is a unique
    /// temp path that is never created, so nothing is left behind.
    #[test]
    fn installed_formats_are_never_persisted_and_an_empty_root_probes_empty() {
        let mut settings = catalog_settings(Flux2WeightFormat::Bf16, Flux2WeightFormat::Bf16);
        settings.installed_formats.insert(Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock, Flux2WeightFormat::Q80);
        settings.installed_formats.transformer_layout = Flux2TransformerLayout::LegacyDiffusers;
        let written = serde_json::to_value(&settings).expect("serialize");
        assert!(written.get("installed_formats").is_none());
        assert!(written.get("transformer_layout").is_none());
        assert_eq!(loaded_settings(&written).installed_formats, Flux2InstalledFormats::default());

        let root = std::env::temp_dir().join(format!("ms_flux2_probe_absent_{}", std::process::id()));
        assert!(!root.exists());
        for variant in Flux2Variant::all() {
            assert_eq!(probe_flux2_installed_formats(&root, variant), Flux2InstalledFormats::default(), "{variant:?}");
        }
        assert!(!root.exists(), "the probe never creates anything");
    }

    /// The probe's spec lookup is the catalog's bundle rule: the bf16 transformer is the
    /// legacy spec in the legacy layout, every other component the download spec, and a
    /// format that is not offered has none.
    #[test]
    fn the_probed_spec_of_each_component_is_the_bundle_spec() {
        let legacy = Flux2TransformerLayout::LegacyDiffusers;
        let spec = |axis, source, format, layout| flux2_component_spec(Flux2Variant::Klein9B, axis, source, format, layout).map(|spec| spec.id);
        let t = Flux2WeightAxis::Transformer;
        assert_eq!(spec(t, Flux2EncoderSource::Stock, Flux2WeightFormat::Bf16, legacy), flux2_klein_legacy_transformer_spec(Flux2Variant::Klein9B).map(|spec| spec.id));
        for (axis, source) in component_choices(Flux2Variant::Klein9B) {
            for &format in Flux2WeightFormat::offered_for(Flux2Variant::Klein9B, axis, source) {
                assert_eq!(
                    spec(axis, source, format, Flux2TransformerLayout::Primary),
                    flux2_klein_spec(Flux2Variant::Klein9B, axis, source, format).map(|spec| spec.id),
                    "{axis:?} {source:?} {format:?}"
                );
            }
        }
        assert_eq!(spec(Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Q5KM, Flux2TransformerLayout::Primary), None);
    }

    /// Protocol 23: every `params` names the variant's shared prompt library and the precision
    /// order; the encoder source travels in download mode only (a hand-picked encoder has none),
    /// and the «Нет» stash never travels.
    #[test]
    fn params_carry_the_prompt_library_fields_and_never_the_stash() {
        for variant in Flux2Variant::all() {
            let manual = Flux2KleinSettings {
                variant: variant.wire().to_string(),
                uncached_prompt: "my own text".to_string(),
                ..runnable_settings()
            }
            .normalized();
            let params = manual.to_params(false);
            assert_eq!(params["prompt_library"], json!(variant.prompt_library_id()));
            assert_eq!(params["prompt_cache_precision_order"], json!(flux2_prompt_cache_precision_order()));
            assert!(params.get("text_encoder_source").is_none(), "manual mode names no source");
            assert!(!params.to_string().contains("my own text"), "the stash is never sent");
            assert_eq!(manual.to_profiles_params(true)["prompt_library"], json!(variant.prompt_library_id()));
        }
        let download = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            uncensored_text_encoder: true,
            ..runnable_settings()
        }
        .normalized();
        assert_eq!(download.to_params(false)["text_encoder_source"], json!("uncensored"));
        let four_b = Flux2KleinSettings { variant: Flux2Variant::Klein4B.wire().to_string(), ..download };
        assert_eq!(four_b.normalized().to_params(false)["text_encoder_source"], json!("stock"), "4B offers the stock encoder only");
    }

    /// The «Нет» stash round-trips through the settings file, and a file written before it
    /// existed loads with an empty one.
    #[test]
    fn the_uncached_prompt_round_trips_and_defaults_to_empty() {
        let settings = Flux2KleinSettings { uncached_prompt: "  spaced text \n".to_string(), ..runnable_settings() };
        let document = serde_json::to_value(&settings).expect("serialize the settings");
        assert_eq!(loaded_settings(&document).uncached_prompt, "  spaced text \n", "kept verbatim");
        let old = loaded_settings(&json!({ "prompt": "a prompt" }));
        assert!(old.uncached_prompt.is_empty());
    }
}
