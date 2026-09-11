/*
File: cleaning/tools/ai_editor/engines/flux2_klein/settings.rs

Purpose:
Everything the FLUX.2 klein engine persists to its VARIANT's settings file
(`config::flux2_klein_settings_path`) and the wire enums those fields are spelled in:
the placement/dtype vocabulary, the memory presets built out of it, the source mode
that decides whether the three model paths are typed by hand or derived from the
download directory, and the load/save pair that moves the whole struct through a
worker thread.

Main responsibilities:
- own `Flux2KleinSettings`, its defaults, its clamping (`normalized`) and the JSON
  body it puts on the wire (`params`);
- own the placement/dtype/preset vocabulary and the preset <-> field mapping;
- derive the effective model paths from the source mode AND the variant;
- read and write the settings file, and decide when a save is due.

There is ONE FILE PER VARIANT and the document records which one it is: 9B keeps
`flux2_klein_settings.json` byte-for-byte, 4B gets its own name, `load_flux2_settings`
stamps the owning variant over whatever it read, and `save_flux2_settings` writes back to
the file the document itself names.

Key structures:
- `Flux2KleinSettings`, `Flux2EffectivePaths`
- `MemoryPreset`, `MemoryPresetValues`, `Flux2Placement`, `Flux2Dtype`
- `Flux2SourceMode`

Key functions:
- `load_flux2_settings()`, `save_flux2_settings()`, `settings_from_json()`,
  `settings_save_due()`
- `normalize_source_lang()`, `source_lang_title()`

Notes:
Items are `pub(super)` so the module root and the sibling submodules of `flux2_klein`
can reach them; `use super::*;` pulls in the module root's imports and the other
submodules' re-exported items. `normalized()` is the ONLY value ever put on the wire.
*/

use super::*;

// ---------------------------------------------------------------------------------------
// Wire enums
// ---------------------------------------------------------------------------------------

/// Where the pipeline's modules live during a run. Wire values are the persisted
/// identity and are literals by design (`dev-docs/i18n_exclusions.md` §A5: a value that
/// doubles as stored content is never localized).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2Placement {
    FullGpu,
    EncoderCpu,
    ModelCpuOffload,
    SequentialCpuOffload,
}

impl Flux2Placement {
    /// The value put on the wire; also the value persisted in the settings file.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Flux2Placement::FullGpu => "full_gpu",
            Flux2Placement::EncoderCpu => "encoder_cpu",
            Flux2Placement::ModelCpuOffload => "model_cpu_offload",
            Flux2Placement::SequentialCpuOffload => "sequential_cpu_offload",
        }
    }

    /// Parses a persisted/wire value, falling back to the default placement so a
    /// hand-edited settings file cannot push an unknown mode onto the backend.
    pub(super) fn from_wire(value: &str) -> Self {
        match value.trim() {
            "encoder_cpu" => Flux2Placement::EncoderCpu,
            "model_cpu_offload" => Flux2Placement::ModelCpuOffload,
            "sequential_cpu_offload" => Flux2Placement::SequentialCpuOffload,
            _ => Flux2Placement::FullGpu,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Flux2Placement::FullGpu => t!("cleaning.tools.flux2_klein.placement_full_gpu"),
            Flux2Placement::EncoderCpu => t!("cleaning.tools.flux2_klein.placement_encoder_cpu"),
            Flux2Placement::ModelCpuOffload => {
                t!("cleaning.tools.flux2_klein.placement_model_cpu_offload")
            }
            Flux2Placement::SequentialCpuOffload => {
                t!("cleaning.tools.flux2_klein.placement_sequential_cpu_offload")
            }
        }
    }

    pub(super) fn all() -> [Self; 4] {
        [
            Flux2Placement::FullGpu,
            Flux2Placement::EncoderCpu,
            Flux2Placement::ModelCpuOffload,
            Flux2Placement::SequentialCpuOffload,
        ]
    }
}

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

/// The seven settings fields a [`MemoryPreset`] owns, as one comparable value.
///
/// A named struct rather than a tuple because equality between "what the preset says"
/// and "what the settings hold" is the whole mechanism behind
/// [`MemoryPreset::detect`], and a seven-slot positional tuple makes a swapped pair of
/// booleans invisible at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MemoryPresetValues {
    pub(super) placement: Flux2Placement,
    pub(super) low_cpu_mem_usage: bool,
    pub(super) vae_tiling: bool,
    pub(super) vae_slicing: bool,
    pub(super) unload_transformer_before_vae: bool,
    pub(super) unload_text_encoder_after_encode: bool,
    pub(super) text_encoder_fp8: bool,
}

/// A built-in memory profile: one named combination of placement, VAE flags and
/// text-encoder handling.
///
/// `Custom` is never selectable — it is what [`MemoryPreset::detect`] reports when the
/// seven fields match no preset, so editing any of them below silently moves the picker
/// to «Пользовательский» instead of leaving a lie on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MemoryPreset {
    MaxSpeed,
    Balanced,
    MinRam,
    MinVram,
    Custom,
}

impl MemoryPreset {
    /// The four selectable presets, in the order they are offered.
    pub(super) fn selectable() -> [Self; 4] {
        [
            MemoryPreset::MaxSpeed,
            MemoryPreset::Balanced,
            MemoryPreset::MinRam,
            MemoryPreset::MinVram,
        ]
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            MemoryPreset::MaxSpeed => t!("cleaning.tools.flux2_klein.preset_max_speed"),
            MemoryPreset::Balanced => t!("cleaning.tools.flux2_klein.preset_balanced"),
            MemoryPreset::MinRam => t!("cleaning.tools.flux2_klein.preset_min_ram"),
            MemoryPreset::MinVram => t!("cleaning.tools.flux2_klein.preset_min_vram"),
            MemoryPreset::Custom => t!("cleaning.tools.flux2_klein.preset_custom"),
        }
    }

    /// The seven fields a preset owns: placement, `low_cpu_mem_usage`, VAE tiling, VAE
    /// slicing, whether the transformer is unloaded before the VAE decode, whether the
    /// text encoder is unloaded right after the prompt is encoded, and whether that
    /// encoder is quantized to fp8. `Custom` owns none, so it answers `None` and cannot
    /// be applied.
    ///
    /// `text_encoder_fp8` is `false` in EVERY preset on purpose: it trades embedding
    /// quality for memory, and that trade is the user's to make, never a preset's.
    pub(super) fn values(self) -> Option<MemoryPresetValues> {
        match self {
            MemoryPreset::MaxSpeed => Some(MemoryPresetValues {
                placement: Flux2Placement::FullGpu,
                low_cpu_mem_usage: false,
                vae_tiling: false,
                vae_slicing: false,
                unload_transformer_before_vae: false,
                unload_text_encoder_after_encode: false,
                text_encoder_fp8: false,
            }),
            MemoryPreset::Balanced => Some(MemoryPresetValues {
                placement: Flux2Placement::EncoderCpu,
                low_cpu_mem_usage: false,
                vae_tiling: true,
                vae_slicing: false,
                unload_transformer_before_vae: true,
                unload_text_encoder_after_encode: false,
                text_encoder_fp8: false,
            }),
            MemoryPreset::MinRam => Some(MemoryPresetValues {
                placement: Flux2Placement::EncoderCpu,
                low_cpu_mem_usage: true,
                vae_tiling: true,
                vae_slicing: false,
                unload_transformer_before_vae: true,
                unload_text_encoder_after_encode: false,
                text_encoder_fp8: false,
            }),
            // `low_cpu_mem_usage` joins the VRAM profile because sequential offload
            // streams every module through host RAM: loading without it spikes RAM as
            // well, which defeats the point of the profile on a small machine.
            MemoryPreset::MinVram => Some(MemoryPresetValues {
                placement: Flux2Placement::SequentialCpuOffload,
                low_cpu_mem_usage: true,
                vae_tiling: true,
                vae_slicing: true,
                unload_transformer_before_vae: true,
                unload_text_encoder_after_encode: false,
                text_encoder_fp8: false,
            }),
            MemoryPreset::Custom => None,
        }
    }

    /// Reports which preset `settings` currently equals, or `Custom` when none does.
    pub(super) fn detect(settings: &Flux2KleinSettings) -> Self {
        let current = MemoryPresetValues {
            placement: Flux2Placement::from_wire(&settings.placement),
            low_cpu_mem_usage: settings.low_cpu_mem_usage,
            vae_tiling: settings.vae_tiling,
            vae_slicing: settings.vae_slicing,
            unload_transformer_before_vae: settings.unload_transformer_before_vae,
            unload_text_encoder_after_encode: settings.unload_text_encoder_after_encode,
            text_encoder_fp8: settings.text_encoder_fp8,
        };
        Self::selectable()
            .into_iter()
            .find(|preset| preset.values() == Some(current))
            .unwrap_or(MemoryPreset::Custom)
    }

    /// Writes the preset's seven fields into `settings`. Returns `true` if anything
    /// changed. `Custom` is a no-op: it has no values of its own.
    pub(super) fn apply(self, settings: &mut Flux2KleinSettings) -> bool {
        let Some(values) = self.values() else {
            return false;
        };
        let mut changed = false;
        if settings.placement != values.placement.wire() {
            settings.placement = values.placement.wire().to_string();
            changed = true;
        }
        for (field, value) in [
            (&mut settings.low_cpu_mem_usage, values.low_cpu_mem_usage),
            (&mut settings.vae_tiling, values.vae_tiling),
            (&mut settings.vae_slicing, values.vae_slicing),
            (
                &mut settings.unload_transformer_before_vae,
                values.unload_transformer_before_vae,
            ),
            (
                &mut settings.unload_text_encoder_after_encode,
                values.unload_text_encoder_after_encode,
            ),
            (&mut settings.text_encoder_fp8, values.text_encoder_fp8),
        ] {
            if *field != value {
                *field = value;
                changed = true;
            }
        }
        changed
    }
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

/// Everything the tool persists to THIS VARIANT's settings file
/// ([`config::flux2_klein_settings_path`]).
///
/// `#[serde(default)]` so a file written by an older build keeps loading; the wire
/// value is always `normalized()`, never this struct as edited.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Use the UNCENSORED text encoder rather than the official one.
    ///
    /// A normal persisted setting and not a global: it selects which encoder the model
    /// download fetches AND which of the two managed directories `text_encoder_path`
    /// points at. Both encoders may sit on disk at once, so flipping it when the other one
    /// is already there repoints the path and downloads nothing
    /// ([`flux2_text_encoder_path_after_toggle`]). It never travels inside `params`: the
    /// download methods carry it as their own top-level `uncensored` field.
    pub(super) uncensored_text_encoder: bool,
    /// Either a `.safetensors` file or a diffusers directory.
    pub(super) transformer_path: String,
    /// VAE directory or `.safetensors` file.
    pub(super) vae_path: String,
    /// The ENGLISH prompt, i.e. the one actually sent to the backend.
    ///
    /// Never legitimately empty: an empty prompt blocks a run, so both `Default` and
    /// [`settings_from_json`] substitute [`FLUX2_DEFAULT_PROMPT`] for a blank one.
    pub(super) prompt: String,
    /// The optional user-language prompt the translator reads from.
    pub(super) source_prompt: String,
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
    pub(super) placement: String,
    pub(super) dtype: String,
    pub(super) low_cpu_mem_usage: bool,
    pub(super) vae_tiling: bool,
    pub(super) vae_slicing: bool,
    /// Move the transformer off the GPU before the VAE decode.
    ///
    /// The decode peaks ON TOP of a resident transformer, which is the most common
    /// source of an out-of-memory failure; unloading first trades a short reload for
    /// that peak. Its default follows the placement — `false` under `full_gpu`, `true`
    /// everywhere else — which is applied in `load_flux2_settings` for a settings file
    /// written before the field existed (serde's per-field default cannot read
    /// `placement`).
    pub(super) unload_transformer_before_vae: bool,
    /// Drop the Qwen3 text encoder from memory as soon as the prompt is encoded.
    ///
    /// The encoder is ~16 GB and is needed exactly ONCE per generation, while the
    /// transformer that follows it is ~18 GB: holding both at the same time fits
    /// neither the device nor the host, and the failure mode is the kernel's OOM killer
    /// rather than a catchable exception. Unloading costs a re-read from disk on the
    /// next NEW prompt (tens of seconds); repeating the same prompt is free, because the
    /// backend caches the embeddings. Its default follows the placement — `false` under
    /// `full_gpu`, `true` everywhere else — applied in `load_flux2_settings` for a file
    /// written before the field existed, exactly like the flag above.
    pub(super) unload_text_encoder_after_encode: bool,
    /// Quantize the text encoder to fp8.
    ///
    /// Defaults to `false` in every preset and in `Default`: it trades embedding quality
    /// for memory, and it only helps while the encoder is actually resident — once
    /// `unload_text_encoder_after_encode` is on, the peak is set by the transformer and
    /// this flag no longer moves it.
    pub(super) text_encoder_fp8: bool,
    pub(super) mask_dilate_px: u32,
    pub(super) mask_feather_px: u32,
    pub(super) color_match: bool,
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
            // Not empty: an empty prompt is the one value the run gate refuses outright,
            // so the default is the job this tool exists for.
            prompt: FLUX2_DEFAULT_PROMPT.to_string(),
            source_prompt: String::new(),
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
            placement: Flux2Placement::FullGpu.wire().to_string(),
            dtype: Flux2Dtype::Bfloat16.wire().to_string(),
            low_cpu_mem_usage: false,
            vae_tiling: false,
            vae_slicing: false,
            // Nothing is unloaded by default. The encoder is loaded LAST, after the
            // transformer already sits on the card, so it lands in host memory the pipeline
            // has just vacated. Keeping it there is what turns a NEW prompt from a full
            // re-read of the encoder into a single encode pass, and the host memory it
            // costs is memory nothing else wants at that moment. The backend now loads a
            // TRUNCATED encoder (28 of 36 decoder layers, no `lm_head`), so both the
            // residency and the read are about a fifth smaller than they were; the exact
            // figures that used to stand here were measured on the full model and no
            // replacement has been measured on real hardware yet.
            unload_transformer_before_vae: false,
            unload_text_encoder_after_encode: false,
            // Never defaulted on: quantizing the encoder is a quality trade the user
            // makes deliberately.
            text_encoder_fp8: false,
            mask_dilate_px: 16,
            // 12 px, not 6: with a correct ramp (its width IS `mask_feather_px`) 6 px still
            // leaves a visible step — measured +9.8% excess gradient on the mask contour
            // against +1.1% at 12 px, while 12 px still keeps 81-84% of the edit. 16 px is
            // cleaner again (+0.3%) but gives up too much of it.
            mask_feather_px: 12,
            color_match: true,
        }
    }
}

impl Flux2KleinSettings {
    /// Returns a copy with every field forced into its supported range: unknown
    /// placement/dtype/service values fall back to their defaults, numeric ranges are
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
            // Forced off for a variant that has no uncensored encoder published for it:
            // the backend refuses that pair outright, and the panel never draws the toggle
            // there, so a hand-edited settings file is the only way it could arrive.
            uncensored_text_encoder: self.uncensored_text_encoder
                && variant.supports_uncensored_encoder(),
            transformer_path: self.transformer_path.trim().to_string(),
            vae_path: self.vae_path.trim().to_string(),
            prompt: self.prompt.trim().to_string(),
            source_prompt: self.source_prompt.clone(),
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
            placement: Flux2Placement::from_wire(&self.placement).wire().to_string(),
            dtype: Flux2Dtype::from_wire(&self.dtype).wire().to_string(),
            low_cpu_mem_usage: self.low_cpu_mem_usage,
            vae_tiling: self.vae_tiling,
            vae_slicing: self.vae_slicing,
            unload_transformer_before_vae: self.unload_transformer_before_vae,
            unload_text_encoder_after_encode: self.unload_text_encoder_after_encode,
            text_encoder_fp8: self.text_encoder_fp8,
            mask_dilate_px: self.mask_dilate_px.min(FLUX2_DILATE_MAX),
            mask_feather_px: self.mask_feather_px.min(FLUX2_FEATHER_MAX),
            color_match: self.color_match,
        }
    }

    /// The active source mode.
    pub(super) fn source_mode(&self) -> Flux2SourceMode {
        Flux2SourceMode::from_wire(&self.source_mode)
    }

    /// The checkpoint this document describes. Anything unrecognized reads as 9B.
    pub(super) fn variant(&self) -> Flux2Variant {
        Flux2Variant::from_wire(&self.variant)
    }

    /// Whether the «Расцензуренный энкодер» toggle is in effect right now.
    ///
    /// The persisted flag AND the variant's capability: only the 9B checkpoint has an
    /// uncensored encoder published for it, so the flag is inert for the other one rather
    /// than pointing a path at a directory nothing fills.
    pub(super) fn uncensored_encoder_active(&self) -> bool {
        self.uncensored_text_encoder && self.variant().supports_uncensored_encoder()
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
    /// The derived encoder is a SIBLING of the downloaded `tokenizer/` and `scheduler/`
    /// directories under the variant's own root, which is what the backend's component search
    /// needs: it probes each supplied path and ITS PARENT (`component_search_roots` /
    /// `component_probe_order` in `modules/ai_backend/inpaint/flux2_klein.py`), so the repo
    /// root is reached from any of the three and both encoder directories resolve the
    /// tokenizer.
    #[must_use]
    pub(super) fn effective_paths(&self) -> Flux2EffectivePaths {
        match self.source_mode() {
            Flux2SourceMode::Manual => Flux2EffectivePaths {
                text_encoder: self.text_encoder_path.trim().to_string(),
                transformer: self.transformer_path.trim().to_string(),
                vae: self.vae_path.trim().to_string(),
            },
            Flux2SourceMode::Download => {
                let variant = self.variant();
                Flux2EffectivePaths {
                    text_encoder: config::flux2_klein_text_encoder_dir(
                        variant,
                        self.uncensored_encoder_active(),
                    )
                    .to_string_lossy()
                    .to_string(),
                    transformer: config::flux2_klein_transformer_dir(variant)
                        .to_string_lossy()
                        .to_string(),
                    vae: config::flux2_klein_vae_dir(variant)
                        .to_string_lossy()
                        .to_string(),
                }
            }
        }
    }

    /// Builds the `params` object of a generation or estimate request.
    ///
    /// `self` must already be `normalized()`. `seed` is `null` unless the user pinned
    /// one, which is what the backend expects for "pick a fresh seed".
    ///
    /// `whole_region` is NOT a setting and is therefore passed in rather than read from
    /// `self`: it is derived from the painted mask by [`mask_for_run`], which builds the
    /// buffer and the flag together. It does not replace the mask — the blob still carries
    /// one, and the backend refuses `whole_region = true` unless that mask is solid.
    ///
    /// Every path that only ASKS the backend something (`.status`, `.estimate`, the
    /// prompt-cache calls) passes `false`: no mask exists for those, and `true` would make
    /// the backend apply the mode's parameter overrides — and log them — on a polling path.
    #[must_use]
    pub(super) fn to_params(&self, whole_region: bool) -> Value {
        // The EFFECTIVE paths, never the raw fields: in download mode the fields hold the
        // user's own configuration and must not reach the backend.
        let paths = self.effective_paths();
        json!({
            "text_encoder_path": paths.text_encoder,
            "transformer_path": paths.transformer,
            "vae_path": paths.vae,
            "prompt": self.prompt,
            "steps": self.steps,
            "guidance_scale": self.guidance_scale,
            "strength": self.strength,
            "seed": if self.use_seed { json!(self.seed) } else { Value::Null },
            "placement": self.placement,
            "dtype": self.dtype,
            "low_cpu_mem_usage": self.low_cpu_mem_usage,
            "vae_tiling": self.vae_tiling,
            "vae_slicing": self.vae_slicing,
            "unload_transformer_before_vae": self.unload_transformer_before_vae,
            "unload_text_encoder_after_encode": self.unload_text_encoder_after_encode,
            "text_encoder_fp8": self.text_encoder_fp8,
            "whole_region": whole_region,
            "mask_dilate_px": self.mask_dilate_px,
            "mask_feather_px": self.mask_feather_px,
            "color_match": self.color_match,
            // Pinned, not a setting: see `FLUX2_MAX_SEQ`. The field stays on the wire
            // because the backend still reads it.
            "max_sequence_length": FLUX2_MAX_SEQ,
        })
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

/// Reads `variant`'s settings file, falling back to defaults on any error (a missing file
/// is the normal first-run case, and a corrupt one must not block the tool).
///
/// The returned document always carries `variant`, whatever the file said: the file that
/// was read is the one that BELONGS to this engine, so a document copied over from the
/// other variant must not keep claiming to describe the other model's paths.
pub(super) fn load_flux2_settings(variant: Flux2Variant) -> Flux2KleinSettings {
    let path = config::flux2_klein_settings_path(variant);
    let mut settings = match fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(value) => settings_from_json(&value),
            Err(_) => Flux2KleinSettings::default(),
        },
        Err(_) => Flux2KleinSettings::default(),
    };
    settings.variant = variant.wire().to_string();
    settings
}

/// Deserializes a settings document and migrates the placement-dependent memory flags.
///
/// Split out of [`load_flux2_settings`] so the migration itself is testable without a
/// settings file on disk: the file I/O has no contract worth asserting, this does.
///
/// A document that is not a settings object at all deserializes to `Default`, which is
/// the same "a corrupt file must not block the tool" rule the caller applies.
pub(super) fn settings_from_json(value: &Value) -> Flux2KleinSettings {
    let has_boolean = |name: &str| value.get(name).is_some_and(Value::is_boolean);
    let has_unload_transformer = has_boolean("unload_transformer_before_vae");
    let has_unload_text_encoder = has_boolean("unload_text_encoder_after_encode");
    let mut settings: Flux2KleinSettings =
        serde_json::from_value(value.clone()).unwrap_or_default();
    // `unload_transformer_before_vae` defaults from the PLACEMENT, which a serde
    // per-field default cannot read: it sees one field at a time and never its siblings.
    // A file written before the flag existed gets it derived here — `false` under
    // `full_gpu` (nothing is unloaded, everything already fits), `true` for every
    // economical placement — while a file that carries it keeps the user's choice.
    // An ABSENT variant is what every file written before the 4B engine existed looks
    // like, and serde's `#[serde(default)]` already answers `9b` for it. A PRESENT but
    // unrecognized token is normalized here for the same reason every other wire enum is:
    // a hand-edited value must not reach a path derivation as an unknown literal.
    settings.variant = Flux2Variant::from_wire(&settings.variant).wire().to_string();
    let economical = Flux2Placement::from_wire(&settings.placement) != Flux2Placement::FullGpu;
    if !has_unload_transformer {
        settings.unload_transformer_before_vae = economical;
    }
    // The ENCODER flag does not follow the placement: the encoder is loaded last, after
    // the transformer already sits on the card, so it lands in host memory the pipeline
    // has just vacated and there is nothing to save by dropping it. `false` everywhere.
    if !has_unload_text_encoder {
        settings.unload_text_encoder_after_encode = false;
    }
    // A file written before the prompt had a default — or one the user emptied — loads
    // with the default prompt rather than with a blank field. A blank prompt is not a
    // usable state: it blocks the run gate, so preserving it would only mean a tool that
    // refuses to start until the user guesses what to type. Serde's `#[serde(default)]`
    // covers the ABSENT key; only the present-but-blank case needs this.
    if settings.prompt.trim().is_empty() {
        settings.prompt = FLUX2_DEFAULT_PROMPT.to_string();
    }
    // `text_encoder_fp8` needs no migration: its default is `false` for EVERY placement,
    // which is exactly what serde's `#[serde(default)]` already produces.
    //
    // A file written by a build that still had the «Работа без маски» checkbox carries a
    // `whole_region` key that no longer maps to a field; older files likewise carry
    // `max_sequence_length` (pinned to `FLUX2_MAX_SEQ` now) and `brush_radius` (the brush
    // belongs to the host's `MaskBrush`). No migration is needed for any of them, and none
    // may be added: the struct does NOT use `deny_unknown_fields`, so serde drops such keys
    // silently, the document loads with every other setting intact, and the next save simply
    // writes it out without them. The mode `whole_region` used to hold is derived from the
    // painted mask now (`mask_for_run`), so there is nothing left for a stored value to mean.
    settings
}

/// Whether a settings save must be started right now.
///
/// `dirty` is raised by every parameter change and by the OOM recovery that rewrites the
/// economy settings itself. `settings_loaded` gates it because the initial load runs on its own
/// worker: saving the in-memory DEFAULTS before that load lands would overwrite the user's file
/// with defaults, which is a silent data loss rather than a visible failure.
/// `save_in_flight` keeps at most one writer on the file at a time.
#[must_use]
pub(super) fn settings_save_due(dirty: bool, settings_loaded: bool, save_in_flight: bool) -> bool {
    dirty && settings_loaded && !save_in_flight
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
pub(super) fn save_flux2_settings(settings: &Flux2KleinSettings) -> Result<(), String> {
    let path = config::flux2_klein_settings_path(settings.variant());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    }
    let raw = serde_json::to_string_pretty(settings)
        .map_err(|err| tf!("cleaning.tools.flux2_klein.serialize_settings_error", err = err))?;
    fs::write(&path, raw)
        .map_err(|err| tf!("cleaning.tools.flux2_klein.write_settings_error", err = err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_wire_roundtrip() {
        for placement in Flux2Placement::all() {
            assert_eq!(Flux2Placement::from_wire(placement.wire()), placement);
        }
        assert_eq!(
            Flux2Placement::from_wire("bogus"),
            Flux2Placement::FullGpu,
            "an unknown placement must fall back to the default"
        );
    }

    #[test]
    fn defaults_match_the_distilled_checkpoint() {
        let settings = Flux2KleinSettings::default();
        assert_eq!(settings.steps, 4);
        assert!((settings.guidance_scale - 1.0).abs() < f32::EPSILON);
        assert!((settings.strength - 1.0).abs() < f32::EPSILON);
        assert!(!settings.use_seed);
        assert_eq!(settings.mask_dilate_px, 16);
        assert_eq!(settings.mask_feather_px, 12);
        assert!(settings.color_match);
        // The default placement is `full_gpu`, where the transformer stays put...
        assert!(!settings.unload_transformer_before_vae);
        // ...and the encoder is kept too: loaded LAST, it occupies host memory the
        // pipeline has already vacated, and keeping it spares every new prompt a full
        // re-read of the encoder.
        assert!(!settings.unload_text_encoder_after_encode);
        // Quantizing the text encoder is never defaulted on.
        assert!(!settings.text_encoder_fp8);
    }

    #[test]
    fn presets_pin_the_unload_flags_and_never_the_fp8_one() {
        let mut settings = Flux2KleinSettings::default();
        MemoryPreset::MaxSpeed.apply(&mut settings);
        assert!(!settings.unload_transformer_before_vae);
        // No preset releases the encoder: keeping it costs host memory the pipeline no
        // longer needs and saves a full encoder re-read on every new prompt.
        assert!(!settings.unload_text_encoder_after_encode);
        for preset in [
            MemoryPreset::Balanced,
            MemoryPreset::MinRam,
            MemoryPreset::MinVram,
        ] {
            let mut settings = Flux2KleinSettings::default();
            preset.apply(&mut settings);
            assert!(
                settings.unload_transformer_before_vae,
                "every economical preset unloads before the VAE decode"
            );
            assert!(
                !settings.unload_text_encoder_after_encode,
                "no preset drops the text encoder: it is loaded last, into host memory \
                 the pipeline has already vacated"
            );
        }
        // fp8 is a quality trade and belongs to the user, so NO preset turns it on —
        // including the one whose whole purpose is the smallest VRAM footprint.
        for preset in MemoryPreset::selectable() {
            let mut settings = Flux2KleinSettings {
                text_encoder_fp8: true,
                ..Flux2KleinSettings::default()
            };
            preset.apply(&mut settings);
            assert!(
                !settings.text_encoder_fp8,
                "{:?} must clear fp8, not carry it",
                preset
            );
        }
    }

    #[test]
    fn the_unload_flags_migrate_from_the_placement_and_fp8_does_not() {
        // A file written before either flag existed: `full_gpu` unloads nothing …
        let old_full_gpu = json!({ "placement": "full_gpu", "steps": 4 });
        let migrated = settings_from_json(&old_full_gpu);
        assert!(!migrated.unload_transformer_before_vae);
        assert!(!migrated.unload_text_encoder_after_encode);
        assert!(!migrated.text_encoder_fp8);
        // … while every economical placement unloads the TRANSFORMER, which `Default`
        // alone (it only knows the `full_gpu` case) would have got wrong. The encoder
        // flag is `false` regardless of placement — see `settings_from_json`.
        for placement in ["encoder_cpu", "model_cpu_offload", "sequential_cpu_offload"] {
            let migrated = settings_from_json(&json!({ "placement": placement }));
            assert!(
                migrated.unload_transformer_before_vae,
                "{placement} must unload the transformer"
            );
            assert!(
                !migrated.unload_text_encoder_after_encode,
                "{placement} must KEEP the text encoder: it is loaded last, into host \
                 memory the pipeline has vacated"
            );
            assert!(
                !migrated.text_encoder_fp8,
                "{placement} must not silently quantize the encoder"
            );
        }
        // A file that CARRIES the flags keeps the user's own choice, migration or not.
        let explicit = settings_from_json(&json!({
            "placement": "encoder_cpu",
            "unload_transformer_before_vae": false,
            "unload_text_encoder_after_encode": false,
            "text_encoder_fp8": true
        }));
        assert!(!explicit.unload_transformer_before_vae);
        assert!(!explicit.unload_text_encoder_after_encode);
        assert!(explicit.text_encoder_fp8);
    }

    #[test]
    fn partial_json_uses_defaults() {
        let settings: Flux2KleinSettings =
            serde_json::from_str("{}").expect("deserialize empty object");
        assert_eq!(settings.steps, 4);
        assert_eq!(settings.placement, "full_gpu");
    }

    #[test]
    fn normalized_clamps_every_range() {
        let mut settings = Flux2KleinSettings {
            steps: 999,
            guidance_scale: f32::NAN,
            strength: 0.0,
            mask_dilate_px: 999,
            mask_feather_px: 999,
            placement: "nonsense".to_string(),
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
        assert_eq!(norm.mask_feather_px, FLUX2_FEATHER_MAX);
        assert_eq!(norm.placement, "full_gpu");
        assert_eq!(norm.dtype, "bfloat16");
        assert_eq!(norm.mt_service, "google");
        assert_eq!(norm.source_lang, "auto");
        assert_eq!(norm.text_encoder_path, "/models/qwen3");
        assert!(norm.to_params(false)["unload_transformer_before_vae"].is_boolean());
        assert!(norm.to_params(false)["unload_text_encoder_after_encode"].is_boolean());
        assert!(norm.to_params(false)["text_encoder_fp8"].is_boolean());
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

    #[test]
    fn presets_are_detected_back_from_their_own_values() {
        for preset in MemoryPreset::selectable() {
            let mut settings = Flux2KleinSettings::default();
            preset.apply(&mut settings);
            assert_eq!(MemoryPreset::detect(&settings), preset);
        }
    }

    #[test]
    fn a_hand_edited_combination_reports_custom() {
        let mut settings = Flux2KleinSettings::default();
        MemoryPreset::Balanced.apply(&mut settings);
        settings.vae_slicing = !settings.vae_slicing;
        assert_eq!(MemoryPreset::detect(&settings), MemoryPreset::Custom);
        // The two new fields are owned by the preset too, so toggling either of them
        // alone must move the picker off the preset just as the VAE flags do.
        for flip in [
            |s: &mut Flux2KleinSettings| {
                s.unload_text_encoder_after_encode = !s.unload_text_encoder_after_encode;
            },
            |s: &mut Flux2KleinSettings| s.text_encoder_fp8 = !s.text_encoder_fp8,
        ] {
            let mut settings = Flux2KleinSettings::default();
            MemoryPreset::Balanced.apply(&mut settings);
            flip(&mut settings);
            assert_eq!(MemoryPreset::detect(&settings), MemoryPreset::Custom);
        }
        // `Custom` owns no values, so applying it must not touch anything.
        let before = settings.clone();
        assert!(!MemoryPreset::Custom.apply(&mut settings));
        assert_eq!(before.placement, settings.placement);
        assert_eq!(before.vae_slicing, settings.vae_slicing);
    }

    /// The settings saver must be ARMED by a settings change and by nothing else.
    ///
    /// This is the whole persistence guarantee of the engine: `dirty` is the only signal a
    /// parameter change leaves behind, and `poll_and_maybe_save` — which runs inside
    /// `AiEngine::poll`, i.e. once per frame while the tool is active — is the only writer.
    /// The two guards beside it are equally load-bearing: saving before the initial load
    /// landed would overwrite the user's file with the in-memory defaults, and a second writer
    /// would race the first on the same path.
    #[test]
    fn the_settings_saver_is_armed_by_a_settings_change_and_only_then() {
        assert!(settings_save_due(true, true, false), "a changed setting must be written");
        assert!(!settings_save_due(false, true, false), "nothing changed: no write");
        assert!(
            !settings_save_due(true, false, false),
            "a write before the initial load lands would clobber the file with the defaults"
        );
        assert!(!settings_save_due(true, true, true), "at most one writer on the file at a time");
    }

    /// The mode is not a setting any more, so the only thing left to pin about the wire
    /// is that the flag the caller derived is the flag that reaches the backend.
    #[test]
    fn the_derived_mode_is_what_reaches_the_wire() {
        for whole_region in [false, true] {
            let params = runnable_settings().normalized().to_params(whole_region);
            assert_eq!(params["whole_region"], json!(whole_region));
            // The mode never replaces the mask parameters: feathering still reaches the
            // backend and still means what it means in both.
            assert_eq!(params["mask_feather_px"], json!(12));
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
        let migrated = settings_from_json(&old);
        assert_eq!(migrated.text_encoder_path, "/a", "the removed keys must not cost the rest");
        assert_eq!(migrated.transformer_path, "/b");
        assert_eq!(migrated.vae_path, "/c");
        assert_eq!(migrated.mask_dilate_px, 8);
        // The neighbouring placement migration still runs on such a document.
        assert!(migrated.unload_transformer_before_vae);
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
        let plain = settings_from_json(&without);
        assert_eq!(
            serde_json::to_value(&migrated).expect("serialize"),
            serde_json::to_value(&plain).expect("serialize"),
            "a stale removed key may not change a single loaded setting"
        );
    }

    /// The toggle is a persisted setting, and it must NOT leak into `params`: the download
    /// methods carry it as their own top-level field.
    #[test]
    fn the_encoder_toggle_persists_but_never_enters_the_generation_params() {
        let settings = Flux2KleinSettings {
            uncensored_text_encoder: true,
            ..runnable_settings()
        };
        let stored = serde_json::to_value(&settings).expect("settings serialize");
        assert_eq!(stored["uncensored_text_encoder"], json!(true));
        let reloaded = settings_from_json(&stored);
        assert!(reloaded.uncensored_text_encoder);
        assert!(settings.normalized().uncensored_text_encoder);

        let params = settings.normalized().to_params(false);
        assert!(
            params.get("uncensored").is_none() && params.get("uncensored_text_encoder").is_none(),
            "the toggle belongs to the download request, not to `params`"
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
            settings_from_json(&stored).source_mode(),
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
            settings_from_json(&legacy).source_mode(),
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
        assert_eq!(
            derived.transformer,
            config::flux2_klein_transformer_dir(Flux2Variant::Klein9B).to_string_lossy()
        );
        assert_eq!(derived.vae, config::flux2_klein_vae_dir(Flux2Variant::Klein9B).to_string_lossy());
        assert_eq!(
            derived.text_encoder,
            config::flux2_klein_text_encoder_dir(Flux2Variant::Klein9B, false).to_string_lossy(),
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
        assert_eq!(
            derived_uncensored.text_encoder,
            config::flux2_klein_text_encoder_dir(Flux2Variant::Klein9B, true).to_string_lossy()
        );
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
        assert_eq!(
            derived_params["text_encoder_path"],
            json!(config::flux2_klein_text_encoder_dir(Flux2Variant::Klein9B, true).to_string_lossy())
        );
        assert_eq!(
            derived_params["transformer_path"],
            json!(config::flux2_klein_transformer_dir(Flux2Variant::Klein9B).to_string_lossy())
        );
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

        // Every field the previous build persisted, and the ONE the variant added. A field
        // that disappears from this list silently drops a user setting on the next save.
        const HISTORIC_KEYS: [&str; 26] = [
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
            "placement",
            "dtype",
            "low_cpu_mem_usage",
            "vae_tiling",
            "vae_slicing",
            "unload_transformer_before_vae",
            "unload_text_encoder_after_encode",
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
        assert_eq!(
            added,
            ["variant"],
            "the variant is the only field the second engine added to the document"
        );

        // A document written by the previous build carries no `variant` and must load as
        // the 9B engine with every value the user typed intact.
        let legacy = settings_from_json(&json!({
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
        assert!(!Flux2Variant::Klein4B.supports_uncensored_encoder());
        let forced = Flux2KleinSettings {
            variant: Flux2Variant::Klein4B.wire().to_string(),
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            uncensored_text_encoder: true,
            ..runnable_settings()
        };
        assert!(
            !forced.uncensored_encoder_active(),
            "the flag is inert for a variant that has no uncensored encoder"
        );
        assert!(
            !forced.normalized().uncensored_text_encoder,
            "the refused pairing must never reach the wire"
        );
        assert_eq!(
            forced.effective_paths().text_encoder,
            config::flux2_klein_text_encoder_dir(Flux2Variant::Klein4B, false).to_string_lossy(),
            "the derived encoder stays the official one"
        );
        // The 9B engine is unaffected: there the flag still selects the other directory.
        let allowed = Flux2KleinSettings {
            variant: Flux2Variant::Klein9B.wire().to_string(),
            ..forced
        };
        assert!(allowed.uncensored_encoder_active());
        assert!(allowed.normalized().uncensored_text_encoder);
        assert_eq!(
            allowed.effective_paths().text_encoder,
            config::flux2_klein_text_encoder_dir(Flux2Variant::Klein9B, true).to_string_lossy()
        );
    }
}
