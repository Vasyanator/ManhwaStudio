/*
FILE HEADER (cleaning/tools/ai_editor/engines/flux2_klein.rs)

Purpose:
The FLUX.2 klein ENGINE of the «ИИ-редактор области» tool. The HOST owns the on-canvas
frame — the rectangle, the painted mask stack, the pending result and Применить/Отменить.
This file owns everything model-specific: the parameters and their persistence, the memory
presets, the RAM/VRAM forecast, the prompt-cache library, the request/response wire
contracts, the OOM recovery, the worker thread and its own progress bar. It implements
`super::super::engine::AiEngine` and never touches `CanvasView`, `ProjectData` or the frame.

The mask means "you MAY change what is under it" — the inverse of the mask-inpaint tools'
"remove what is under it". Everything outside it must survive the round trip untouched,
which is why the engine declares exactly ONE mask layer and puts its bytes on the wire
verbatim.

Working modes (`Flux2KleinSettings::whole_region`, a MODE and not a memory profile,
which is why no `MemoryPreset` owns it):
- off (default): the painted mask decides, and `allows_empty_mask()` is `false`, so the
  host's own frame button refuses a run with nothing painted.
- on: the WHOLE selected region is regenerated and no painting is required, which is what
  `allows_empty_mask() == true` tells the host. The request still carries a mask, a SOLID
  one built by `mask_for_run`, because the backend refuses `whole_region = true` unless the
  mask really is uniformly 255. The host's painted mask is never consulted in this mode, so
  it comes back untouched the moment the switch is cleared. `mask_dilate_px` is ignored
  backend-side here (the slider is faded to say so); `mask_feather_px` keeps working and is
  what softens the join between the regenerated region and the page.

Selection contract (checked twice, on purpose):
- `constraints()` declares multiple of 16, shortest side >= 128 px, area <= 1 MP, aspect not
  steeper than 8:1. The frame snaps and validates against them, so a rectangle that reaches
  `start` has already satisfied every one of them.
- The run path re-validates the ACTUAL region size (`region_block_reason`, on the worker as
  well) and refuses with a named reason: a host that hands over a region of another size
  gets an explanation instead of a request the backend would reject.

Key items:
- `Flux2KleinEngine`: the `AiEngine` implementation and its wiring.
- `Flux2KleinSettings`: everything persisted to `flux2_klein_settings.json`
  (`config::flux2_klein_settings_path`), loaded/saved on worker threads.
  `normalized()` is the ONLY value ever put on the wire.
- `Flux2SessionState`: the run channel and the per-RUN undo stack. The painted mask is NOT
  here — it belongs to the host's `MaskStack` and arrives in `EngineRunRequest::masks`.
- `MemoryPreset`: four built-in placement/VAE/text-encoder configurations plus `Custom`,
  which is never chosen by hand — it is what `detect` reports when the seven fields a
  preset owns match no preset.
- `Flux2Status` / `Flux2Estimate`: the `.status` component catalog and the backend's
  own VRAM/RAM forecast. The forecast is COMPUTED BY THE BACKEND; this file only
  displays it. Its geometry input is the region SIZE, so `set_region` arms it on a
  settled size change and never on a move — see that method for the full rule.

IPC (`backend_ipc::protocol`):
- `inpaint.flux2_klein` — streaming. Header `{image_len, mask_len, params}`, blob
  `region.png ++ mask.png` (mask L8, exactly the region size). Response header
  `{image_len, oom_recovered, applied{...}}`, blob = RGB PNG of exactly the region
  size, validated by STRICT equality before use (`image_len` is REQUIRED; an answer
  without it is refused). Progress frames carry `phase` (`load`/`generate`), `step`,
  `total`, `label` and no preview blob. The request goes out through `begin_call`, so
  its id is known and a cancel can stop it with `CallHandle::cancel`.
- The backend may RECOVER from an out-of-memory failure during the VAE decode by
  retrying it with the transformer unloaded (and, if needed, VAE tiling/slicing on).
  The five memory flags it actually used come back in `applied`; this side writes them
  into the settings and saves them, so the next run takes the cheap path immediately,
  and says so in the engine's status line when `oom_recovered` is set. A partial `applied`
  object is ignored wholesale.
- `.status`, `.estimate`, `.unload` — one-shot. `.status` and `.estimate` carry the
  normalized `params`: both are questions ABOUT the paths in the request, and a
  backend that receives none answers about the paths of its last successful
  generation, i.e. about nothing until one has run. `.status` additionally answers
  `prompt_cached` — whether the embeddings of the prompt IN THE REQUEST are already
  held — and the field is optional: a backend that omits it reads as "not known",
  never as "not cached". It also answers `text_encoder_available`, which is a DIFFERENT
  question from `available`: a run whose prompt is cached needs no encoder, so
  `available` stays true while this is false.
- GENERATING WITHOUT A TEXT ENCODER is supported and is the reason the prompt-cache
  library exists: the denoise and the VAE decode never look at the encoder, so a
  `.msprompt` carried to a machine that never downloaded the 16 GB Qwen3 is enough. The
  run gate therefore waives the encoder path when `.status` says the prompt is cached,
  and only then. What the absence costs is stated where it happens: a warning line beside
  the cache status ("only ready caches work"), a disabled «Кэшировать»/«Сохранить кэш»
  (the two operations that must ENCODE, refused backend-side anyway), the family shown on
  every library row (a machine with no encoder has no ACTIVE family, so the listing spans
  all of them), and a one-off notice after a load whose `encoder_verified` came back
  false — the file's own metadata was taken on trust because nothing local could compare
  the fingerprint. Загрузить/Экспорт/Импорт keep working throughout.
- `.prompt_cache.*` — the prompt-cache LIBRARY, six methods carrying the normalized
  `params` plus their own fields. `build` is STREAMING (the ~16 GB Qwen3 encoder takes
  ~106 s to read) and drives the same progress bar as a generation, which is why the
  two can never run at once. `list` answers the ACTIVE encoder family (empty when no
  encoder is installed, and the listing then spans every family) and the saved entries
  (`name`, its own `family`, `prompt`, `created_at`); `save`/`load` take a `name`; `export` takes a
  `name` and a `path`; `import` takes a `path` — all of them BESIDE `params` at the top
  level of the header, which is where the backend reads them from, and never `overwrite`,
  so a name already taken comes back as an explicit error. The library itself lives
  backend-side
  (`prompt_cache/`, one folder per encoder family) — this side works with NAMES and
  never builds a path into it. An imported file of a foreign family is stored under
  that family and reported as such; it does not appear in this family's listing and
  the backend refuses to load it, which is expected and is surfaced as a warning.

Contracts:
- The GUI thread never blocks: settings I/O, every IPC call, the native file pickers,
  the machine translation of the prompt and even the one-frame cancel write all run on
  `ms_thread::spawn` workers and `AiEngine::poll` only drains channels.
- `poll` is called every frame whether the parameter panel is visible or not; nothing
  `draw_parameters` does may be a precondition of it. The reverse is not true: the intents
  the panel raises (re-query the catalog, start a translation, open a picker) are folded
  back at the end of `draw_parameters` itself, so a click acts on the frame it happened in.
- ONE progress bar serves every run of the engine, so it is claimed by GENERATION: a run
  takes the next number when it starts, and a write from an older one — including the
  terminal "the bar is done" — is dropped. A cancel and a moved frame both retire the
  current generation and cancel the request behind it. The bar lives in this engine's own
  panel: there is no shared progress vocabulary between engines
  (`dev-docs/region_edit_v2_plan.md` §13.2 D13).
- The prompt sent to the backend is the ENGLISH field. The optional second field plus
  the Google/Yandex/DeepL picker only fill it in, reusing the translation tab's own
  dispatcher (`translate_texts_via_translator`) instead of a second copy of it. It is
  never empty: an empty prompt blocks the run, so `FLUX2_DEFAULT_PROMPT` is substituted
  both for a fresh settings file and for one whose prompt is missing or blank.
- `.status` and `.prompt_cache.list` are re-queried through the SAME one-shot arming as
  the memory forecast (`status_wanted`, `prompt_cache_list_wanted`): a change re-arms
  the flag, and at most one query is ever in flight, so editing the prompt cannot turn
  a keystroke into a request. A `prompt_cached` answer is shown only while the prompt it
  was asked about still equals the one in the field.
- The answer of a run is PIXELS and nothing else (D12): `EnginePoll::Done` carries a
  `ColorImage` of exactly the frame rectangle. Merging it into the clean overlay is the
  HOST's job; this file never writes `CleanOverlaysModel` storage and never touches
  `CanvasView`.
- The model is distilled: 4 steps and `guidance_scale = 1.0` are the defaults and
  there is no negative prompt — do not add a field for one.
*/
use super::super::engine::{AiEngine, EnginePoll, EngineRunRequest, EngineSection, MaskLayerSpec};
use crate::backend_ipc::{self, CallError};
use crate::canvas::OverlayRectPx;
use crate::config;
use crate::tabs::cleaning::tools::base::RegionEditToolBase;
use crate::tabs::cleaning::tools::region_edit_v2::geometry::FrameConstraints;
use crate::tabs::translation::backend_health::ai_backend_offline_error;
use crate::tabs::translation::machine_translation::{MtService, translate_texts_via_translator};
use crate::tabs::translation::panels::machine_translation::{MT_SOURCE_LANGUAGES, MtLanguage};
use crate::widgets::{SeedSpinBox, WheelComboBox, WheelSlider};
use eframe::egui;
use egui::Color32;
use image::{ColorType, ImageEncoder};
use ms_thread as thread;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use web_time::Duration;

// ---------------------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------------------

/// The VAE stride: every side handed to the model must be a multiple of it.
const FLUX2_SELECTION_MULTIPLE: usize = 16;
/// Shortest accepted side of a region, pixels. Below it the model has no context to
/// work from and answers with noise.
const FLUX2_MIN_SELECTION_PX: usize = 128;
/// Largest accepted region AREA (1 MP). The latent budget is the real constraint, and
/// it is an area, not a side.
const FLUX2_MAX_SELECTION_AREA_PX2: usize = 1_048_576;
/// Steepest accepted ratio between the long and the short side of a region.
const FLUX2_MAX_SELECTION_ASPECT: f32 = 8.0;

const FLUX2_STEPS_MIN: u32 = 1;
const FLUX2_STEPS_MAX: u32 = 50;
const FLUX2_GUIDANCE_MIN: f32 = 1.0;
const FLUX2_GUIDANCE_MAX: f32 = 10.0;
const FLUX2_STRENGTH_MIN: f32 = 0.25;
const FLUX2_STRENGTH_MAX: f32 = 1.0;
const FLUX2_DILATE_MAX: u32 = 64;
const FLUX2_FEATHER_MAX: u32 = 32;
const FLUX2_MAX_SEQ_MIN: u32 = 64;
const FLUX2_MAX_SEQ_MAX: u32 = 512;
const FLUX2_BRUSH_MIN: u32 = 1;
const FLUX2_BRUSH_MAX: u32 = 256;

/// Generation may load ~20 GB of weights before the first step; allow a wide window.
const FLUX2_RUN_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
/// `.status` / `.estimate` / `.unload` are cheap bookkeeping calls.
const FLUX2_QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest number of pre-run region images kept for «Вернуть». A megapixel region is
/// ~4 MB, so an unbounded stack would grow without limit over a long session; the
/// oldest entry is dropped instead.
const FLUX2_UNDO_LIMIT: usize = 8;

/// Breakdown key of the denoising-loop peak in an `.estimate` answer.
const FLUX2_BREAKDOWN_PEAK_DENOISE: &str = "peak_denoise";
/// Breakdown key of the VAE-decode peak in an `.estimate` answer. The forecast's VRAM
/// figure is the LARGEST of the peaks, not their sum.
const FLUX2_BREAKDOWN_PEAK_DECODE: &str = "peak_decode";
/// Breakdown key of the prompt-encoding peak in an `.estimate` answer.
///
/// The text encoder is resident only while the prompt is encoded, so that phase is a
/// peak of its own rather than a term added to the others. A backend that does not
/// report it simply leaves the line out of the tooltip.
const FLUX2_BREAKDOWN_PEAK_ENCODE: &str = "peak_encode";

/// Painter opacity of a control that is still live but currently has no effect.
const FLUX2_FADED_CONTROL_OPACITY: f32 = 0.45;

/// Horizontal room left for the «Сохранить кэш» button beside the name field, points.
/// The field takes whatever is left, so the button never wraps onto its own line.
const FLUX2_CACHE_NAME_BUTTON_RESERVE: f32 = 140.0;

/// Preview tint of the engine's single mask layer, i.e. of the edit-permission area.
///
/// Opaque on purpose: `MaskStack` scales the alpha itself so the mask stays translucent
/// over the artwork, and a tint that arrived already translucent would be darkened twice.
const FLUX2_MASK_TINT: Color32 = Color32::from_rgb(80, 200, 255);

/// The prompt a fresh settings file starts from, and the one substituted for an
/// absent or blank prompt in an existing one.
///
/// It is a LITERAL and stays untranslated on purpose, for the same reason every other
/// wire value in this file does (`dev-docs/i18n_exclusions.md` §A5: a value that
/// doubles as stored and transmitted content is never localized). The model reads
/// English; the field it fills is the ENGLISH one, and the optional user-language
/// field with its translator row exists precisely so the user never has to write here
/// in English by hand. An empty prompt blocks a run outright, so a usable default is
/// strictly better than an empty field the user must guess how to fill.
const FLUX2_DEFAULT_PROMPT: &str =
    "Remove any text and sound effects, and restore the background underneath them";

/// Extension of a saved prompt-cache file, without the dot.
///
/// A file-format identifier, not prose: it is what the file is NAMED on disk and what
/// the open dialog filters on, so it is the same in every language. Only the human
/// caption of the filter is localized.
const FLUX2_PROMPT_CACHE_EXTENSION: &str = "msprompt";

/// Colour of a status line reporting a good state.
///
/// The studio UI has no shared semantic-colour table; the cleaning subtree names its
/// tones per file, and these three are the ones it already uses — this green is the
/// mask editor's «sample area» legend colour (`tools/base.rs`), the amber is what the
/// memory forecast and the watermark tool already warn in, and the red is the shared
/// error tone of `RegionEditToolBase::draw_ui_hint`. Naming them here keeps one meaning
/// per colour inside this file instead of four bare literals.
const FLUX2_STATUS_OK_COLOR: Color32 = Color32::from_rgb(90, 255, 130);
/// Colour of a status line reporting a state the user should act on.
const FLUX2_STATUS_WARN_COLOR: Color32 = Color32::from_rgb(255, 170, 60);
/// Colour of a status line reporting a failure.
const FLUX2_STATUS_ERROR_COLOR: Color32 = Color32::from_rgb(255, 120, 120);

// ---------------------------------------------------------------------------------------
// Wire enums
// ---------------------------------------------------------------------------------------

/// Where the pipeline's modules live during a run. Wire values are the persisted
/// identity and are literals by design (`dev-docs/i18n_exclusions.md` §A5: a value that
/// doubles as stored content is never localized).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flux2Placement {
    FullGpu,
    EncoderCpu,
    ModelCpuOffload,
    SequentialCpuOffload,
}

impl Flux2Placement {
    /// The value put on the wire; also the value persisted in the settings file.
    fn wire(self) -> &'static str {
        match self {
            Flux2Placement::FullGpu => "full_gpu",
            Flux2Placement::EncoderCpu => "encoder_cpu",
            Flux2Placement::ModelCpuOffload => "model_cpu_offload",
            Flux2Placement::SequentialCpuOffload => "sequential_cpu_offload",
        }
    }

    /// Parses a persisted/wire value, falling back to the default placement so a
    /// hand-edited settings file cannot push an unknown mode onto the backend.
    fn from_wire(value: &str) -> Self {
        match value.trim() {
            "encoder_cpu" => Flux2Placement::EncoderCpu,
            "model_cpu_offload" => Flux2Placement::ModelCpuOffload,
            "sequential_cpu_offload" => Flux2Placement::SequentialCpuOffload,
            _ => Flux2Placement::FullGpu,
        }
    }

    fn label(self) -> &'static str {
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

    fn all() -> [Self; 4] {
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
enum Flux2Dtype {
    Bfloat16,
    Float16,
}

impl Flux2Dtype {
    fn wire(self) -> &'static str {
        match self {
            Flux2Dtype::Bfloat16 => "bfloat16",
            Flux2Dtype::Float16 => "float16",
        }
    }

    fn from_wire(value: &str) -> Self {
        match value.trim() {
            "float16" => Flux2Dtype::Float16,
            _ => Flux2Dtype::Bfloat16,
        }
    }

    fn all() -> [Self; 2] {
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
struct MemoryPresetValues {
    placement: Flux2Placement,
    low_cpu_mem_usage: bool,
    vae_tiling: bool,
    vae_slicing: bool,
    unload_transformer_before_vae: bool,
    unload_text_encoder_after_encode: bool,
    text_encoder_fp8: bool,
}

/// A built-in memory profile: one named combination of placement, VAE flags and
/// text-encoder handling.
///
/// `Custom` is never selectable — it is what [`MemoryPreset::detect`] reports when the
/// seven fields match no preset, so editing any of them below silently moves the picker
/// to «Пользовательский» instead of leaving a lie on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryPreset {
    MaxSpeed,
    Balanced,
    MinRam,
    MinVram,
    Custom,
}

impl MemoryPreset {
    /// The four selectable presets, in the order they are offered.
    fn selectable() -> [Self; 4] {
        [
            MemoryPreset::MaxSpeed,
            MemoryPreset::Balanced,
            MemoryPreset::MinRam,
            MemoryPreset::MinVram,
        ]
    }

    fn label(self) -> &'static str {
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
    fn values(self) -> Option<MemoryPresetValues> {
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
    fn detect(settings: &Flux2KleinSettings) -> Self {
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
    fn apply(self, settings: &mut Flux2KleinSettings) -> bool {
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

/// Everything the tool persists to `flux2_klein_settings.json`.
///
/// `#[serde(default)]` so a file written by an older build keeps loading; the wire
/// value is always `normalized()`, never this struct as edited.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Flux2KleinSettings {
    /// Directory of the Qwen3 text encoder.
    text_encoder_path: String,
    /// Either a `.safetensors` file or a diffusers directory.
    transformer_path: String,
    /// VAE directory or `.safetensors` file.
    vae_path: String,
    /// The ENGLISH prompt, i.e. the one actually sent to the backend.
    ///
    /// Never legitimately empty: an empty prompt blocks a run, so both `Default` and
    /// [`settings_from_json`] substitute [`FLUX2_DEFAULT_PROMPT`] for a blank one.
    prompt: String,
    /// The optional user-language prompt the translator reads from.
    source_prompt: String,
    /// Whether the translate-into-English row is shown at all.
    translate_prompt: bool,
    /// `MtService::key()` of the machine translator used by that row.
    mt_service: String,
    /// Source language code of `source_prompt` (`"auto"` by default).
    source_lang: String,
    steps: u32,
    guidance_scale: f32,
    strength: f32,
    /// Sent only when `use_seed` is set; otherwise the wire value is `null`.
    seed: u64,
    use_seed: bool,
    placement: String,
    dtype: String,
    low_cpu_mem_usage: bool,
    vae_tiling: bool,
    vae_slicing: bool,
    /// Move the transformer off the GPU before the VAE decode.
    ///
    /// The decode peaks ON TOP of a resident transformer, which is the most common
    /// source of an out-of-memory failure; unloading first trades a short reload for
    /// that peak. Its default follows the placement — `false` under `full_gpu`, `true`
    /// everywhere else — which is applied in `load_flux2_settings` for a settings file
    /// written before the field existed (serde's per-field default cannot read
    /// `placement`).
    unload_transformer_before_vae: bool,
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
    unload_text_encoder_after_encode: bool,
    /// Quantize the text encoder to fp8.
    ///
    /// Defaults to `false` in every preset and in `Default`: it trades embedding quality
    /// for memory, and it only helps while the encoder is actually resident — once
    /// `unload_text_encoder_after_encode` is on, the peak is set by the transformer and
    /// this flag no longer moves it.
    text_encoder_fp8: bool,
    /// Edit the WHOLE selected region instead of a painted mask.
    ///
    /// A working MODE, not a memory profile, which is why it is deliberately absent
    /// from [`MemoryPresetValues`]: choosing a preset must never turn it on or off.
    /// When it is set the tool still sends a mask — a SOLID one, every byte `255`,
    /// exactly the region size — because that is the contract the backend validates
    /// (`whole_region = true` with a non-solid mask is refused). The user's painted
    /// mask is left untouched in the session so that clearing the checkbox brings it
    /// back; see [`Flux2SessionState::mask_for_run`].
    ///
    /// The backend IGNORES `mask_dilate_px` in this mode (there is no contour to grow)
    /// while `mask_feather_px` keeps working and softens the region's join to the page.
    whole_region: bool,
    mask_dilate_px: u32,
    mask_feather_px: u32,
    color_match: bool,
    max_sequence_length: u32,
    /// Brush radius of the mask painter, in region pixels. UI-only, never sent.
    ///
    /// Nothing reads it since the brush became the HOST's: the frame's `MaskBrush` is shared by
    /// every engine and owns the live radius. The field is kept rather than deleted because the
    /// file it lives in is the user's and dropping it would silently discard a value they chose;
    /// remove it, with `FLUX2_BRUSH_MIN`/`FLUX2_BRUSH_MAX` and its clamp, once the host round has
    /// decided where a shared brush radius is persisted.
    brush_radius: u32,
}

impl Default for Flux2KleinSettings {
    fn default() -> Self {
        Self {
            text_encoder_path: String::new(),
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
            // transformer already sits on the card, so it lands in host memory the
            // pipeline has just vacated: measured 17.2 GiB RSS while the card holds
            // 18.4 GiB. Keeping it turns a NEW prompt from ~116 s into 6.0 s, so paying
            // 16.4 GiB of otherwise idle host memory for that is the right default.
            unload_transformer_before_vae: false,
            unload_text_encoder_after_encode: false,
            // Never defaulted on: quantizing the encoder is a quality trade the user
            // makes deliberately.
            text_encoder_fp8: false,
            // Off by default: painting the permitted area is the tool's normal flow, and
            // a settings file written before this field existed must keep that flow.
            whole_region: false,
            mask_dilate_px: 16,
            // 12 px, not 6: with a correct ramp (its width IS `mask_feather_px`) 6 px still
            // leaves a visible step — measured +9.8% excess gradient on the mask contour
            // against +1.1% at 12 px, while 12 px still keeps 81-84% of the edit. 16 px is
            // cleaner again (+0.3%) but gives up too much of it.
            mask_feather_px: 12,
            color_match: true,
            max_sequence_length: FLUX2_MAX_SEQ_MAX,
            brush_radius: 24,
        }
    }
}

impl Flux2KleinSettings {
    /// Returns a copy with every field forced into its supported range: unknown
    /// placement/dtype/service values fall back to their defaults, numeric ranges are
    /// clamped, and non-finite floats are replaced by the default. This is the ONLY
    /// value ever put on the wire.
    #[must_use]
    fn normalized(&self) -> Self {
        let defaults = Self::default();
        let clamp_f32 = |value: f32, min: f32, max: f32, fallback: f32| {
            if value.is_finite() {
                value.clamp(min, max)
            } else {
                fallback
            }
        };
        Self {
            text_encoder_path: self.text_encoder_path.trim().to_string(),
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
            whole_region: self.whole_region,
            mask_dilate_px: self.mask_dilate_px.min(FLUX2_DILATE_MAX),
            mask_feather_px: self.mask_feather_px.min(FLUX2_FEATHER_MAX),
            color_match: self.color_match,
            max_sequence_length: self
                .max_sequence_length
                .clamp(FLUX2_MAX_SEQ_MIN, FLUX2_MAX_SEQ_MAX),
            brush_radius: self.brush_radius.clamp(FLUX2_BRUSH_MIN, FLUX2_BRUSH_MAX),
        }
    }

    /// Builds the `params` object of a generation or estimate request.
    ///
    /// `self` must already be `normalized()`. `seed` is `null` unless the user pinned
    /// one, which is what the backend expects for "pick a fresh seed".
    ///
    /// `whole_region` travels with the request but does NOT replace the mask: the blob
    /// still carries one, and the backend refuses `whole_region = true` unless that mask
    /// is solid. [`Flux2SessionState::mask_for_run`] is what makes it so.
    #[must_use]
    fn to_params(&self) -> Value {
        json!({
            "text_encoder_path": self.text_encoder_path,
            "transformer_path": self.transformer_path,
            "vae_path": self.vae_path,
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
            "whole_region": self.whole_region,
            "mask_dilate_px": self.mask_dilate_px,
            "mask_feather_px": self.mask_feather_px,
            "color_match": self.color_match,
            "max_sequence_length": self.max_sequence_length,
        })
    }
}

/// Maps a persisted language code onto the shared MT source-language list, falling
/// back to `"auto"` for anything the list does not carry.
fn normalize_source_lang(code: &str) -> String {
    let lowered = code.trim().to_ascii_lowercase();
    if MT_SOURCE_LANGUAGES.iter().any(|lang| lang.code == lowered) {
        lowered
    } else {
        "auto".to_string()
    }
}

/// Localized title of a source-language code, or the raw code when it is unknown.
fn source_lang_title(code: &str) -> String {
    MT_SOURCE_LANGUAGES
        .iter()
        .find(|lang: &&MtLanguage| lang.code == code)
        .map_or_else(|| code.to_string(), |lang| lang.title().to_string())
}

// ---------------------------------------------------------------------------------------
// Backend answers
// ---------------------------------------------------------------------------------------

/// One entry of the `.status` component catalog.
#[derive(Debug, Clone, Default)]
struct Flux2Component {
    path: String,
    /// `exists` for a path component, `found` for the tokenizer/scheduler.
    present: bool,
    size_bytes: u64,
}

impl Flux2Component {
    /// Reads a component entry, accepting either the `exists` or the `found` spelling
    /// of "is it there" so the tokenizer/scheduler entries parse with the same code.
    fn parse(value: Option<&Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let present = value
            .get("exists")
            .or_else(|| value.get("found"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Self {
            path: value
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            present,
            size_bytes: value
                .get("size_bytes")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        }
    }
}

/// The `.status` answer: whether a run can start at all, plus the host's memory.
#[derive(Debug, Clone, Default)]
struct Flux2Status {
    available: bool,
    reason: String,
    text_encoder: Flux2Component,
    transformer: Flux2Component,
    vae: Flux2Component,
    tokenizer: Flux2Component,
    scheduler: Flux2Component,
    /// Total device VRAM in bytes, `0` when the backend did not report one. Shown
    /// beside the forecast's "free" figure, which alone says nothing about headroom.
    vram_total: u64,
    /// Total host RAM in bytes, `0` when unknown. Same role as `vram_total`.
    ram_total: u64,
    loaded: bool,
    device: String,
    /// Whether the backend already holds the embeddings of the prompt this answer was
    /// asked about, so a run can skip the ~16 GB text encoder entirely.
    ///
    /// `None` when the backend did not report the field at all — an older build, or one
    /// whose prompt-cache methods do not exist yet. That is NOT the same as `Some(false)`
    /// and must not be shown as "not cached": the honest answer there is "not known".
    prompt_cached: Option<bool>,
    /// Whether a text encoder is present ON THIS MACHINE for the paths this answer was
    /// asked about. Reported separately from `available`, because a run whose prompt is
    /// already cached needs no encoder at all: `available` stays `true` while this is
    /// `false`, and only the operations that must ENCODE (a new prompt, `.build`, a
    /// `.save` that has to name the encoder) are refused.
    ///
    /// An empty path and a path that does not exist are the same `false` — the second is
    /// what a settings file carried over from another machine looks like.
    ///
    /// `None` is "not known": a backend that predates the field, or no answer yet. It must
    /// not read as `Some(false)`, which is what the warning line and the encode gates act on.
    text_encoder_available: Option<bool>,
}

/// The `.estimate` answer: the backend's own forecast for the current parameters.
///
/// Every figure here is COMPUTED BY THE BACKEND; this side only formats it.
#[derive(Debug, Clone, Default)]
struct Flux2Estimate {
    vram_bytes: u64,
    ram_bytes: u64,
    vram_free: u64,
    ram_free: u64,
    fits: bool,
    /// `(component, bytes)` pairs in KEY order, not in the backend's: `serde_json` is
    /// built without `preserve_order`, so an object parses into a `BTreeMap`. Nothing
    /// may depend on the position of an entry — the per-phase peaks are looked up by name.
    /// The keys are backend identifiers and stay literal.
    breakdown: Vec<(String, u64)>,
}

/// Live progress shared between the run worker and the engine's own panel.
///
/// ONE instance is owned by the engine and reused by every run, so it is claimed by
/// GENERATION: [`begin_progress_generation`] hands the next number to a starting run
/// and every later write from an older one — including its terminal `active = false` —
/// is dropped by [`update_progress`]. Without that, a cancelled worker finishing a
/// minute later would erase the bar of the run that replaced it and leave a bare
/// spinner until that run ended.
#[derive(Default)]
struct Flux2Progress {
    /// The run that currently owns every other field. `0` = no run has started yet,
    /// which no worker can ever carry.
    generation: u64,
    active: bool,
    phase: String,
    step: u64,
    total: u64,
    label: String,
    /// IPC id of the in-flight request of `generation`, published by the worker as
    /// soon as the request is on the wire and taken by a cancel, which needs it to
    /// stop the backend instead of merely dropping the answer.
    cancel_id: Option<u64>,
}

/// Memory flags the backend ACTUALLY used for a finished run.
///
/// They may differ from what was requested: an out-of-memory failure during the VAE
/// decode is recovered by retrying it with cheaper settings, and the recovered values
/// come back here so the next run starts from them instead of hitting the same wall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Flux2AppliedFlags {
    unload_transformer_before_vae: bool,
    vae_tiling: bool,
    vae_slicing: bool,
    unload_text_encoder_after_encode: bool,
    text_encoder_fp8: bool,
}

/// One finished generation: the regenerated region plus what the backend reports about
/// how it got there.
struct Flux2RunOutcome {
    image: egui::ColorImage,
    /// The backend hit an out-of-memory failure during the VAE decode and recovered
    /// from it without re-running the denoising.
    oom_recovered: bool,
    /// Present when the backend reported the flags it ended up using.
    applied: Option<Flux2AppliedFlags>,
}

/// Message the run worker sends back. `source` is the region the run started from and
/// becomes the undo entry.
struct Flux2JobResult {
    source: egui::ColorImage,
    result: Result<Flux2RunOutcome, String>,
}

/// One saved entry of the prompt-cache LIBRARY, as reported by `.prompt_cache.list`.
///
/// The library lives backend-side (a `prompt_cache/` directory next to `fonts/`, split
/// into one folder per encoder FAMILY); this side never builds a path into it and works
/// with names alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Flux2PromptCacheEntry {
    /// The entry's identity in the library, and what `.save`/`.load`/`.export` name.
    name: String,
    /// The encoder family the entry itself belongs to, empty when the backend did not
    /// report one. Every entry carries its own, because a listing made on a machine with
    /// NO encoder spans every family in the library — see [`Flux2PromptCacheList::family`].
    /// It is display-only: the wire identifies an entry by NAME alone.
    family: String,
    /// The prompt the entry was built from. Shown on hover, so a name like "sfx" can
    /// still be checked against what it actually encodes.
    prompt: String,
    /// When it was created, already formatted for display — see
    /// [`format_prompt_cache_created`]. Empty when the backend reported nothing usable.
    created: String,
}

impl Flux2PromptCacheEntry {
    /// The row caption: the bare name, or `<family> / <name>` while the listing spans more
    /// than one family (`show_family`) and the entry actually reports one.
    ///
    /// The family is never part of the identity sent to the backend — the wire names an
    /// entry by `name` alone — so this exists only to keep the user from mistaking another
    /// encoder's cache for one of their own.
    fn label(&self, show_family: bool) -> String {
        if !show_family || self.family.is_empty() {
            return self.name.clone();
        }
        tf!(
            "cleaning.tools.flux2_klein.prompt_cache_entry_with_family",
            family = self.family,
            name = self.name
        )
    }
}

/// The `.prompt_cache.list` answer: the ACTIVE encoder family plus the saved entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Flux2PromptCacheList {
    /// Name of the ACTIVE encoder family, i.e. the one the current settings select.
    ///
    /// Empty means there is none — either the backend did not report one, or (the case
    /// this field now has to distinguish) no encoder is installed at all, and the listing
    /// then spans EVERY family in the library. The rows are shown with their own
    /// [`Flux2PromptCacheEntry::family`] in that case, so the user can see that what they
    /// are looking at is not only "their" caches.
    family: String,
    /// The backend's `text_encoder_available` for the paths this listing was asked about;
    /// `None` when it did not report the field. Same three-state rule as
    /// [`Flux2Status::text_encoder_available`], and the fallback source for the warning
    /// line when no `.status` answer carries one.
    text_encoder_available: Option<bool>,
    entries: Vec<Flux2PromptCacheEntry>,
}

/// A finished `.prompt_cache.load`: the prompt the entry was built from, and how much of
/// the entry's identity the backend could actually check.
///
/// `encoder_verified` is the backend's own three-state answer: `Some(true)` the encoder
/// fingerprint in the file was compared against the encoder on disk, `Some(false)` there
/// is no local encoder to compare against and the file's metadata was taken on trust (the
/// format marker, the version, the sequence length, the dtype and the fp8 flag are checked
/// in both cases), `None` the backend did not say. Only `Some(false)` is reported to the
/// user, and only once, as the outcome of that load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Flux2PromptCacheLoad {
    /// The prompt the entry encodes, trimmed. Empty means the entry carried none, which is
    /// refused rather than applied.
    prompt: String,
    encoder_verified: Option<bool>,
}

/// What a finished prompt-cache worker did. One enum for all five operations because at
/// most one of them is ever in flight, so they share a single channel.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Flux2PromptCacheOutcome {
    /// The embeddings of the current prompt now live in the backend's live cache.
    Built,
    /// They were stored in the library under this name.
    Saved(String),
    /// A library entry was loaded.
    Loaded(Flux2PromptCacheLoad),
    /// A library entry was written to this file.
    Exported(PathBuf),
    /// A file was taken into the library.
    Imported {
        /// The entry's name in the library, empty when the backend did not report one.
        name: String,
        /// Whether it landed in the CURRENT encoder family. `None` when the answer did
        /// not say and nothing local could decide it — no warning is shown then, because
        /// inventing one would be a guess either way.
        family_matches: Option<bool>,
    },
}

/// The one prompt-cache control the user pressed in a frame.
///
/// An enum rather than five booleans: only one operation can run at a time (they share
/// the backend's pipeline and the one channel), and this makes that a property of the
/// type instead of a rule the UI has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flux2PromptCacheAction {
    Build,
    Save,
    Load,
    Export,
    Import,
}

/// Which of the five prompt-cache controls may be used right now.
///
/// A named struct with a free constructor rather than five expressions inline in the UI,
/// for the same reason [`flux2_run_block_reason`] is a free function: the gates are the
/// contract worth testing, and a [`Flux2PanelCtx`] cannot be built outside a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Flux2PromptCacheGates {
    build: bool,
    save: bool,
    load: bool,
    export: bool,
    import: bool,
}

/// Decides which prompt-cache controls are live.
///
/// `prompt_cached` is the three-state answer from `.status` for the CURRENT prompt
/// (`None` = not known yet, or a backend that does not report the field). `busy` covers
/// both a prompt-cache operation and a generation already in flight, because the two
/// share the one progress bar and the backend's one pipeline. `name` is the save-name
/// field and `has_selection` says whether the library combo points at an entry.
///
/// Building needs the encoder on disk and something to encode. Saving additionally needs
/// a cache that actually exists — `None` is not a promise that one does — and a name to
/// store it under. Loading and exporting act on a listed entry, so both need a selection;
/// importing needs neither an entry nor an encoder, because the file supplies everything.
///
/// `text_encoder_available` is the backend's own answer for the configured path
/// ([`Flux2Status::text_encoder_available`]). It closes BUILD and SAVE on top of the local
/// path check, because those two are the only library operations that need the encoder:
/// a build encodes, and a save has to name the encoder that produced the entry, so the
/// backend refuses both outright. `None` — a backend that does not report the field —
/// leaves the decision to the path check alone, which is what this gate has always used.
fn flux2_prompt_cache_gates(
    settings: &Flux2KleinSettings,
    prompt_cached: Option<bool>,
    text_encoder_available: Option<bool>,
    name: &str,
    has_selection: bool,
    backend_available: bool,
    busy: bool,
) -> Flux2PromptCacheGates {
    let ready = backend_available && !busy;
    // A configured path the backend cannot find is the same situation as no path at all —
    // that is exactly what a settings file copied from another machine looks like.
    let encoder_present =
        !settings.text_encoder_path.trim().is_empty() && text_encoder_available != Some(false);
    let encodable = !settings.prompt.trim().is_empty() && encoder_present;
    Flux2PromptCacheGates {
        build: ready && encodable,
        save: ready && encodable && prompt_cached == Some(true) && !name.trim().is_empty(),
        load: ready && has_selection,
        export: ready && has_selection,
        import: ready,
    }
}

/// Decides the three-state prompt-cache line from the last `.status` answer.
///
/// `asked_about` is the trimmed prompt that answer was asked about; `current_prompt` is
/// what the field holds now. The answer counts only while the two still agree — otherwise
/// it describes a prompt the user has already typed away from, and "not known" is the only
/// honest report. A free function so this rule can be tested without a live tool.
fn prompt_cache_state_for(
    status: Option<&Flux2Status>,
    asked_about: Option<&str>,
    current_prompt: &str,
) -> Option<bool> {
    let status = status?;
    if asked_about? != current_prompt.trim() {
        return None;
    }
    status.prompt_cached
}

/// Formats the creation time of a library entry for display.
///
/// The backend writes an ISO-8601 UTC string (`created_at`), which is used verbatim; a
/// Unix timestamp in seconds is accepted too and rendered in the machine's local time,
/// because a timestamp is the other shape this field is written in and a wrong "no date"
/// would be indistinguishable from a genuinely missing one. Anything else — and a
/// timestamp outside the representable range — yields an empty string, which the UI shows
/// as "no date" rather than as a wrong one.
fn format_prompt_cache_created(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| {
                // Python's `time.time()` is a FLOAT, so an integer-only read would drop
                // every timestamp the backend actually sends. Cast justification: the
                // guard keeps the value finite and inside ±1e15 s, which is orders of
                // magnitude below `i64`'s range, so the truncation can neither wrap nor
                // lose the whole-second part. `f64` has no fallible conversion to `i64`.
                let seconds = number.as_f64()?;
                (seconds.is_finite() && seconds.abs() < 1e15).then(|| seconds.trunc() as i64)
            })
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .map(|utc| {
                utc.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default(),
        Some(_) | None => String::new(),
    }
}

// ---------------------------------------------------------------------------------------
// Per-session state
// ---------------------------------------------------------------------------------------

/// What one poll of the run channel found.
///
/// `Done` and `Failed` are TERMINAL and are produced exactly once: the receiver is dropped
/// before either is returned, so the next poll answers `Idle`.
enum Flux2RunPoll {
    /// No run has been started, or the last one has already been reported.
    Idle,
    /// A run is still in flight.
    Running,
    /// The run finished; the image is exactly the region the run started from.
    Done(egui::ColorImage),
    /// The run failed, with an already-localized message.
    Failed(String),
}

/// State of the engine's runs: the channel the worker answers on and the region images
/// that preceded the runs already reported.
///
/// The painted MASK is deliberately not here and must not come back: the host's `MaskStack`
/// owns it and hands its bytes over in `EngineRunRequest::masks`.
#[derive(Default)]
struct Flux2SessionState {
    /// Region images from before each finished run, most recent last, bounded by
    /// [`FLUX2_UNDO_LIMIT`].
    undo_stack: Vec<egui::ColorImage>,
    run_rx: Option<Receiver<Flux2JobResult>>,
}

impl Flux2SessionState {
    /// Drops the run history. The caller is responsible for the run in flight
    /// ([`Self::cancel_run`]) — dropping the receiver alone would leave the backend
    /// generating an image nothing can receive.
    fn clear(&mut self) {
        self.undo_stack.clear();
    }

    /// Starts a run on a worker thread. A second run is refused while one is in flight.
    ///
    /// `mask` is the L8 buffer that actually goes on the wire — the host's painted layer,
    /// or a solid one under `whole_region` — and `mask_size` is the region size both it and
    /// `region` describe. The pair is validated by the caller before it gets here.
    ///
    /// # Errors
    /// Returns the localized "already running" message when a run is still in flight.
    fn start_run(
        &mut self,
        region: egui::ColorImage,
        mask: Vec<u8>,
        mask_size: [usize; 2],
        settings: &Flux2KleinSettings,
        progress: &Arc<Mutex<Flux2Progress>>,
    ) -> Result<(), String> {
        if self.run_rx.is_some() {
            return Err(t!("cleaning.mask_editor.processing_already_running_status").to_string());
        }
        let settings = settings.normalized();
        // Claimed here rather than on the worker: the claim then happens in the order
        // the user pressed the button, whatever order the threads start in.
        let generation = begin_progress_generation(progress);
        let progress = Arc::clone(progress);
        let (tx, rx) = mpsc::channel::<Flux2JobResult>();
        thread::spawn(move || {
            let result =
                run_flux2_klein(&region, &mask, mask_size, &settings, &progress, generation);
            let _ = tx.send(Flux2JobResult {
                source: region,
                result,
            });
        });
        self.run_rx = Some(rx);
        Ok(())
    }

    /// Abandons the run in flight: its answer is discarded, its progress generation is
    /// retired so it can neither move nor stop the bar of whatever runs next, and the
    /// backend is told to stop instead of finishing a generation nobody will see.
    /// Returns `true` when there was a run to abandon.
    ///
    /// A no-op detail that matters: `run_rx` is dropped first, so a result that lands
    /// between the two statements is discarded rather than reported as a finished run.
    ///
    /// During the short window before the request reaches the wire (the region and mask
    /// are still being encoded) there is no id to cancel yet; the run is detached all
    /// the same and its answer is dropped, the backend simply finishes it.
    fn cancel_run(&mut self, progress: &Arc<Mutex<Flux2Progress>>) -> bool {
        if self.run_rx.is_none() {
            return false;
        }
        self.run_rx = None;
        if let Some(id) = retire_progress_generation(progress) {
            spawn_flux2_cancel(id);
        }
        true
    }

    /// Polls the run channel and reports what it found, writing the user-facing line for
    /// the engine's own status slot into `status`.
    ///
    /// A finished run also writes the memory flags the backend actually used back into
    /// `settings` (setting `settings_changed`, which the caller turns into a background
    /// save), so a run that had to recover from an out-of-memory failure makes the next
    /// one take the cheap path from the start.
    fn poll_run(
        &mut self,
        settings: &mut Flux2KleinSettings,
        settings_changed: &mut bool,
        status: &mut Option<String>,
    ) -> Flux2RunPoll {
        let Some(rx) = self.run_rx.as_ref() else {
            return Flux2RunPoll::Idle;
        };
        match rx.try_recv() {
            Ok(job) => {
                self.run_rx = None;
                match job.result {
                    Ok(outcome) => {
                        self.push_undo(job.source);
                        if let Some(applied) = outcome.applied {
                            *settings_changed |= apply_backend_flags(settings, applied);
                        }
                        *status = Some(if outcome.oom_recovered {
                            t!("cleaning.tools.flux2_klein.oom_recovered_status").to_string()
                        } else {
                            t!("cleaning.mask_editor.processing_done_status").to_string()
                        });
                        Flux2RunPoll::Done(outcome.image)
                    }
                    Err(err) => {
                        let message = tf!("cleaning.mask_editor.processing_error", err = err);
                        *status = Some(message.clone());
                        Flux2RunPoll::Failed(message)
                    }
                }
            }
            Err(TryRecvError::Empty) => Flux2RunPoll::Running,
            Err(TryRecvError::Disconnected) => {
                self.run_rx = None;
                let message =
                    t!("cleaning.mask_editor.processing_thread_crashed_error").to_string();
                *status = Some(message.clone());
                Flux2RunPoll::Failed(message)
            }
        }
    }

    /// Pushes one pre-run region image onto the undo stack, dropping the oldest entry
    /// once [`FLUX2_UNDO_LIMIT`] is reached.
    ///
    /// The bound is what keeps a long session from accumulating megabytes of history
    /// nobody will walk back to; the entries lost are always the oldest.
    fn push_undo(&mut self, image: egui::ColorImage) {
        if self.undo_stack.len() >= FLUX2_UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(image);
    }
}

/// The L8 mask a run actually puts on the wire, region-sized either way.
///
/// `painted` is the host's mask layer, exactly `w * h` bytes. With `whole_region` set the
/// model may change every pixel, and the backend proves that by REQUIRING a solid mask
/// alongside the flag — so a fresh all-`255` buffer is built here rather than the painted
/// one being overwritten. That is the whole point of building it: the host's layer keeps
/// whatever the user painted, and clearing the switch brings their work back untouched.
fn mask_for_run(painted: &[u8], whole_region: bool) -> Vec<u8> {
    if whole_region {
        vec![255u8; painted.len()]
    } else {
        painted.to_vec()
    }
}

/// Copies the flags the backend actually used into `settings`. Returns `true` when
/// anything changed, i.e. when a background save is owed.
///
/// The memory preset is deliberately NOT re-pinned here: if the recovered combination
/// matches no preset, the picker moves itself to «Пользовательский», which is the
/// honest report of what is now in effect.
fn apply_backend_flags(settings: &mut Flux2KleinSettings, applied: Flux2AppliedFlags) -> bool {
    let mut changed = false;
    for (field, value) in [
        (
            &mut settings.unload_transformer_before_vae,
            applied.unload_transformer_before_vae,
        ),
        (&mut settings.vae_tiling, applied.vae_tiling),
        (&mut settings.vae_slicing, applied.vae_slicing),
        (
            &mut settings.unload_text_encoder_after_encode,
            applied.unload_text_encoder_after_encode,
        ),
        (&mut settings.text_encoder_fp8, applied.text_encoder_fp8),
    ] {
        if *field != value {
            *field = value;
            changed = true;
        }
    }
    changed
}

// ---------------------------------------------------------------------------------------
// File pickers
// ---------------------------------------------------------------------------------------

/// What a running native file dialog is picking a path for.
///
/// Five of the variants fill in a model path; the last two carry a prompt-cache entry in
/// or out of the library and do NOT touch the settings — they start an IPC call instead
/// (see [`Flux2KleinEngine::poll_picker`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flux2PickerPurpose {
    TextEncoderDir,
    TransformerFile,
    TransformerDir,
    VaeFile,
    VaeDir,
    /// Destination of `inpaint.flux2_klein.prompt_cache.export`.
    PromptCacheExport,
    /// Source of `inpaint.flux2_klein.prompt_cache.import`.
    PromptCacheImport,
}

/// Spawns the blocking native file dialog for `purpose` on a worker thread.
///
/// `None` means the user cancelled. Never called on the GUI thread's own stack: `rfd`
/// blocks until the dialog closes.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_flux2_picker(purpose: Flux2PickerPurpose) -> Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
    thread::spawn(move || {
        let picked = match purpose {
            Flux2PickerPurpose::TextEncoderDir
            | Flux2PickerPurpose::TransformerDir
            | Flux2PickerPurpose::VaeDir => rfd::FileDialog::new().pick_folder(),
            Flux2PickerPurpose::TransformerFile | Flux2PickerPurpose::VaeFile => {
                rfd::FileDialog::new()
                    .add_filter(
                        t!("cleaning.tools.flux2_klein.weights_files_filter"),
                        &["safetensors", "sft", "gguf"],
                    )
                    .pick_file()
            }
            Flux2PickerPurpose::PromptCacheExport => rfd::FileDialog::new()
                .set_title(t!(
                    "cleaning.tools.flux2_klein.prompt_cache_export_dialog_title"
                ))
                .add_filter(
                    t!("cleaning.tools.flux2_klein.prompt_cache_files_filter"),
                    &[FLUX2_PROMPT_CACHE_EXTENSION],
                )
                // The extension is part of the file's identity, so the suggested name
                // already carries it; the stem is a plain identifier and stays literal.
                .set_file_name(format!("prompt.{FLUX2_PROMPT_CACHE_EXTENSION}"))
                .save_file(),
            Flux2PickerPurpose::PromptCacheImport => rfd::FileDialog::new()
                .set_title(t!(
                    "cleaning.tools.flux2_klein.prompt_cache_import_dialog_title"
                ))
                .add_filter(
                    t!("cleaning.tools.flux2_klein.prompt_cache_files_filter"),
                    &[FLUX2_PROMPT_CACHE_EXTENSION],
                )
                .pick_file(),
        };
        let _ = tx.send(picked);
    });
    rx
}

/// Web fallback: the browser build has no native file dialog (`rfd` is native-only),
/// so the pick resolves immediately as cancelled and the dropped capability is logged.
#[cfg(target_arch = "wasm32")]
fn spawn_flux2_picker(_purpose: Flux2PickerPurpose) -> Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
    crate::runtime_log::log_warn("[cleaning] FLUX.2 klein file picker unavailable on web build");
    let _ = tx.send(None);
    rx
}

// ---------------------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------------------

/// The FLUX.2 klein engine of the «ИИ-редактор области» tool.
///
/// Everything here is engine-owned state; the frame rectangle, the painted mask and the
/// pending result belong to the host and reach this type only through `AiEngine`.
pub struct Flux2KleinEngine {
    session: Flux2SessionState,
    /// The frame rectangle the forecast and the status line describe, `None` while the tool
    /// has no frame. Published by the host through [`AiEngine::set_region`].
    region: Option<OverlayRectPx>,
    /// A region SIZE change is waiting for the frame's geometry to settle before it arms the
    /// forecast. It cannot be re-derived by comparing rectangles on the settling frame: the
    /// rectangle usually does not change on the frame the pointer is released, so the size
    /// change would be forgotten and the forecast would never be re-armed.
    region_resize_pending: bool,
    /// User-facing line about the last run (started, finished, recovered, failed,
    /// cancelled), shown in this engine's own panel under the progress bar.
    run_status: Option<String>,
    settings: Flux2KleinSettings,
    settings_rx: Option<Receiver<Flux2KleinSettings>>,
    settings_loaded: bool,
    dirty: bool,
    save_rx: Option<Receiver<()>>,
    /// Component catalog; `None` until the first `.status` answer.
    status: Option<Flux2Status>,
    status_rx: Option<Receiver<Result<Flux2Status, String>>>,
    status_error: Option<String>,
    /// Arms exactly ONE `.status` query, so a failing query cannot spawn a thread per
    /// frame. Re-armed whenever the PROMPT changes too: the answer's `prompt_cached`
    /// is about the prompt that travelled with the question.
    status_wanted: bool,
    /// The trimmed prompt the query currently in flight was asked about, moved into
    /// `status_prompt` when its answer lands.
    status_query_prompt: Option<String>,
    /// The trimmed prompt `status` describes. The cache line is shown only while it
    /// still equals the prompt in the field — otherwise the answer is about a prompt
    /// the user has already edited away from, and the honest state is "not known yet".
    status_prompt: Option<String>,
    /// The one prompt-cache operation (build / save / load / export / import) that may
    /// be in flight. All five share the channel because only one can run at a time.
    prompt_cache_rx: Option<Receiver<Result<Flux2PromptCacheOutcome, String>>>,
    /// User-facing outcome of the last prompt-cache operation, shown under the buttons.
    prompt_cache_status: Option<String>,
    /// A warning about the LAST prompt-cache operation, shown beside its status line:
    /// "the imported entry belongs to another encoder family", or "the encoder fingerprint
    /// of the loaded entry was not compared". Cleared when the next operation starts, so it
    /// is a remark about a result and never a standing banner.
    prompt_cache_warning: Option<String>,
    /// The library listing of the current encoder family; `None` until `.list` answers.
    prompt_cache_library: Option<Flux2PromptCacheList>,
    prompt_cache_list_rx: Option<Receiver<Result<Flux2PromptCacheList, String>>>,
    prompt_cache_list_error: Option<String>,
    /// Same one-shot arming as `status_wanted`; re-armed after a save or an import and
    /// whenever the encoder path changes, since the family — and with it the whole
    /// listing — follows that path.
    prompt_cache_list_wanted: bool,
    /// Name of the library entry the combo points at. Kept as a NAME rather than an
    /// index so a refreshed listing cannot silently move the selection to another entry.
    prompt_cache_selected: Option<String>,
    /// Buffer of the "save under this name" field.
    prompt_cache_name_input: String,
    /// Entry an export dialog was opened for, captured when the dialog started so a
    /// selection changed while it was open cannot export the wrong entry.
    prompt_cache_export_name: Option<String>,
    estimate: Option<Flux2Estimate>,
    estimate_rx: Option<Receiver<Result<Flux2Estimate, String>>>,
    estimate_error: Option<String>,
    /// Same one-shot arming as `status_wanted`; re-armed whenever a parameter changed and,
    /// for the geometry, only by a SETTLED region SIZE change ([`AiEngine::set_region`]), so
    /// the forecast follows the controls without flooding the IPC from scrolling or dragging.
    estimate_wanted: bool,
    unload_rx: Option<Receiver<Result<(), String>>>,
    unload_status: Option<String>,
    translate_rx: Option<Receiver<Result<String, String>>>,
    translate_status: Option<String>,
    picker_rx: Option<Receiver<Option<PathBuf>>>,
    picker: Option<Flux2PickerPurpose>,
    progress: Arc<Mutex<Flux2Progress>>,
    ai_backend_available: bool,
}

impl Default for Flux2KleinEngine {
    fn default() -> Self {
        let mut engine = Self {
            session: Flux2SessionState::default(),
            region: None,
            region_resize_pending: false,
            run_status: None,
            settings: Flux2KleinSettings::default(),
            settings_rx: None,
            settings_loaded: false,
            dirty: false,
            save_rx: None,
            status: None,
            status_rx: None,
            status_error: None,
            status_wanted: true,
            status_query_prompt: None,
            status_prompt: None,
            prompt_cache_rx: None,
            prompt_cache_status: None,
            prompt_cache_warning: None,
            prompt_cache_library: None,
            prompt_cache_list_rx: None,
            prompt_cache_list_error: None,
            prompt_cache_list_wanted: true,
            prompt_cache_selected: None,
            prompt_cache_name_input: String::new(),
            prompt_cache_export_name: None,
            estimate: None,
            estimate_rx: None,
            estimate_error: None,
            estimate_wanted: false,
            unload_rx: None,
            unload_status: None,
            translate_rx: None,
            translate_status: None,
            picker_rx: None,
            picker: None,
            progress: Arc::new(Mutex::new(Flux2Progress::default())),
            ai_backend_available: false,
        };
        engine.request_settings_load();
        engine
    }
}

impl Flux2KleinEngine {
    /// Reads the settings file on a worker thread (never on the GUI thread).
    fn request_settings_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(load_flux2_settings());
        });
    }

    /// Applies a finished settings load. A disconnected channel keeps the in-memory
    /// defaults and unblocks saving.
    fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(settings) => {
                self.settings = settings;
                self.settings_loaded = true;
                self.settings_rx = None;
                self.estimate_wanted = true;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.settings_loaded = true;
                self.settings_rx = None;
            }
        }
    }

    /// Writes dirty settings on a worker thread, at most one save in flight, and never
    /// before the initial load finished (which would clobber the file).
    ///
    /// It is driven from [`AiEngine::poll`] and from nowhere else, which is why the host must
    /// poll the selected engine EVERY frame, panel visible or not: without that call nothing
    /// ever writes the settings file and every model path, memory preset and prompt is lost on
    /// exit, with no error anywhere.
    fn poll_and_maybe_save(&mut self) {
        if let Some(rx) = self.save_rx.as_ref() {
            match rx.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => self.save_rx = None,
                Err(TryRecvError::Empty) => return,
            }
        }
        if !settings_save_due(self.dirty, self.settings_loaded, self.save_rx.is_some()) {
            return;
        }
        self.dirty = false;
        let settings = self.settings.clone();
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(err) = save_flux2_settings(&settings) {
                crate::runtime_log::log_warn(format!(
                    "[cleaning] failed to save FLUX.2 klein settings: {err}"
                ));
            }
            let _ = tx.send(());
        });
    }

    /// Polls the `.status` query and arms a new one when it is wanted, the backend is
    /// reachable and nothing else is running.
    fn poll_and_maybe_query_status(&mut self) {
        if let Some(rx) = self.status_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(status)) => {
                    self.status = Some(status);
                    self.status_error = None;
                    self.status_rx = None;
                    // The answer describes the prompt that travelled with the question,
                    // which may already be several keystrokes behind the field.
                    self.status_prompt = self.status_query_prompt.take();
                }
                Ok(Err(err)) => {
                    self.status_error = Some(err);
                    self.status_rx = None;
                    self.status_query_prompt = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.status_rx = None;
                    self.status_query_prompt = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.status_wanted
            && self.ai_backend_available
            && self.status_rx.is_none()
            && self.session.run_rx.is_none()
            // A prompt-cache build holds the backend for ~106 s; asking it about the
            // catalog meanwhile only queues a call behind that.
            && self.prompt_cache_rx.is_none()
        {
            self.status_wanted = false;
            // The catalog is a question ABOUT the paths in the settings, so they go
            // with the query; without them the backend answers about whatever it used
            // last, i.e. about nothing until a generation has succeeded. The PROMPT
            // travels for the same reason: `prompt_cached` is an answer about one
            // specific prompt, not about the tool in general.
            let params = self.settings.normalized().to_params();
            self.status_query_prompt = Some(self.settings.prompt.trim().to_string());
            let (tx, rx) = mpsc::channel();
            self.status_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(fetch_flux2_status(&params));
            });
        }
    }

    /// Whether the backend holds the embeddings of the prompt CURRENTLY in the field.
    ///
    /// `None` means "not known yet": no `.status` answer, an answer about a different
    /// prompt (the user has typed since), or a backend that does not report the field.
    /// A neutral line is shown for all three — reporting "not cached" for a question
    /// nobody has answered yet would be a guess, and the wrong one most of the time.
    fn prompt_cache_state(&self) -> Option<bool> {
        prompt_cache_state_for(
            self.status.as_ref(),
            self.status_prompt.as_deref(),
            &self.settings.prompt,
        )
    }

    /// Polls the `.estimate` query and sends a new one when something armed
    /// `estimate_wanted`. `region` is the frame rectangle's size; without a frame there is
    /// nothing to forecast and the arming is kept for the next frame that has one.
    fn poll_and_maybe_query_estimate(&mut self, region: Option<[usize; 2]>) {
        if let Some(rx) = self.estimate_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(estimate)) => {
                    self.estimate = Some(estimate);
                    self.estimate_error = None;
                    self.estimate_rx = None;
                }
                Ok(Err(err)) => {
                    self.estimate_error = Some(err);
                    self.estimate_rx = None;
                }
                Err(TryRecvError::Disconnected) => self.estimate_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let Some([width, height]) = region else {
            return;
        };
        if !self.estimate_wanted
            || !self.ai_backend_available
            || self.estimate_rx.is_some()
            || self.session.run_rx.is_some()
        {
            return;
        }
        self.estimate_wanted = false;
        let params = self.settings.normalized().to_params();
        let (tx, rx) = mpsc::channel();
        self.estimate_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(fetch_flux2_estimate(&params, width, height));
        });
    }

    /// Drains the unload channel into the status line under the parameter section.
    fn poll_unload(&mut self) {
        let Some(rx) = self.unload_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.unload_rx = None;
                self.unload_status =
                    Some(t!("cleaning.tools.flux2_klein.unload_done_status").to_string());
                // The pipeline is gone, so the resident/device fields of the catalog
                // are stale.
                self.status_wanted = true;
            }
            Ok(Err(err)) => {
                self.unload_rx = None;
                self.unload_status = Some(tf!("cleaning.inpaint.unload_error", err = err));
            }
            Err(TryRecvError::Disconnected) => self.unload_rx = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Drains the prompt-translation channel into the English prompt field.
    fn poll_translate(&mut self) {
        let Some(rx) = self.translate_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(text)) => {
                self.translate_rx = None;
                self.settings.prompt = text;
                self.dirty = true;
                // A different prompt: whatever the catalog last said about the cache is
                // now about the previous one.
                self.status_wanted = true;
                self.translate_status =
                    Some(t!("cleaning.tools.flux2_klein.translate_done_status").to_string());
            }
            Ok(Err(err)) => {
                self.translate_rx = None;
                self.translate_status =
                    Some(tf!("cleaning.tools.flux2_klein.translate_error", err = err));
            }
            Err(TryRecvError::Disconnected) => self.translate_rx = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Starts one machine-translation request for the user-language prompt.
    ///
    /// The call BLOCKS on the network (DeepL additionally self-throttles), so it runs
    /// on its own worker and the GUI only polls the channel.
    fn start_translate(&mut self) {
        if self.translate_rx.is_some() {
            return;
        }
        let source = self.settings.source_prompt.trim().to_string();
        if source.is_empty() {
            self.translate_status =
                Some(t!("cleaning.tools.flux2_klein.translate_empty_error").to_string());
            return;
        }
        let service = MtService::from_key(&self.settings.mt_service).unwrap_or(MtService::Google);
        let source_lang = normalize_source_lang(&self.settings.source_lang);
        let (tx, rx) = mpsc::channel();
        self.translate_rx = Some(rx);
        self.translate_status =
            Some(t!("cleaning.tools.flux2_klein.translate_running_status").to_string());
        thread::spawn(move || {
            let _ = tx.send(translate_prompt_to_english(service, &source_lang, source));
        });
    }

    /// Starts a native file dialog for `purpose`; at most one dialog at a time.
    fn start_picker(&mut self, purpose: Flux2PickerPurpose) {
        if self.picker_rx.is_some() {
            return;
        }
        self.picker_rx = Some(spawn_flux2_picker(purpose));
        self.picker = Some(purpose);
    }

    /// Folds a finished file pick into the matching model-path field, or — for the two
    /// prompt-cache purposes — starts the IPC call the dialog was opened for.
    ///
    /// A cancelled dialog is a no-op in both cases: nothing is written and no call goes
    /// out.
    fn poll_picker(&mut self) {
        let Some(rx) = self.picker_rx.as_ref() else {
            return;
        };
        let picked = match rx.try_recv() {
            Ok(picked) => picked,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        self.picker_rx = None;
        let Some(purpose) = self.picker.take() else {
            return;
        };
        let Some(path) = picked else {
            return;
        };
        let value = path.to_string_lossy().to_string();
        match purpose {
            Flux2PickerPurpose::TextEncoderDir => self.settings.text_encoder_path = value,
            Flux2PickerPurpose::TransformerFile | Flux2PickerPurpose::TransformerDir => {
                self.settings.transformer_path = value;
            }
            Flux2PickerPurpose::VaeFile | Flux2PickerPurpose::VaeDir => {
                self.settings.vae_path = value;
            }
            // These two carry a library entry in or out through a FILE: nothing is
            // persisted and the settings are left exactly as they were.
            Flux2PickerPurpose::PromptCacheExport => {
                self.start_prompt_cache_export(path);
                return;
            }
            Flux2PickerPurpose::PromptCacheImport => {
                self.start_prompt_cache_import(path);
                return;
            }
        }
        self.dirty = true;
        self.status_wanted = true;
        self.estimate_wanted = true;
        // The encoder family — and therefore the whole library listing — is decided by
        // the model paths, so a new path invalidates the list just as it does the catalog.
        self.prompt_cache_list_wanted = true;
    }

    /// Starts the prompt-cache build on a worker thread.
    ///
    /// Streaming, and it claims the shared progress bar exactly as a generation does —
    /// reading the ~16 GB Qwen3 encoder takes ~106 s and the user needs to see it move.
    /// Claiming the generation here, on the GUI thread, is what makes a cancel and a moved
    /// frame able to retire and stop it, just like a run.
    fn start_prompt_cache_build(&mut self) {
        if self.prompt_cache_rx.is_some() {
            return;
        }
        let header = self.prompt_cache_header(&[]);
        let generation = begin_progress_generation(&self.progress);
        let progress = Arc::clone(&self.progress);
        let (tx, rx) = mpsc::channel();
        self.prompt_cache_rx = Some(rx);
        self.prompt_cache_status =
            Some(t!("cleaning.tools.flux2_klein.prompt_cache_building_status").to_string());
        // A warning belongs to the operation that produced it, not to the next one.
        self.prompt_cache_warning = None;
        thread::spawn(move || {
            let result = build_flux2_prompt_cache(header, &progress, generation);
            let _ = tx.send(result);
        });
    }

    /// Stores the current prompt's cache in the library under the name in the field.
    fn start_prompt_cache_save(&mut self) {
        let name = self.prompt_cache_name_input.trim().to_string();
        if name.is_empty() {
            return;
        }
        let header = self.prompt_cache_header(&[("name", json!(name.clone()))]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_saving_status"),
            move || save_flux2_prompt_cache(header).map(|()| Flux2PromptCacheOutcome::Saved(name)),
        );
    }

    /// Loads the selected library entry into the backend's live cache.
    fn start_prompt_cache_load(&mut self) {
        let Some(name) = self.prompt_cache_selected.clone() else {
            return;
        };
        let header = self.prompt_cache_header(&[("name", json!(name))]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_loading_status"),
            move || load_flux2_prompt_cache(header).map(Flux2PromptCacheOutcome::Loaded),
        );
    }

    /// Writes the entry the export dialog was opened for to `path`.
    fn start_prompt_cache_export(&mut self, path: PathBuf) {
        let Some(name) = self.prompt_cache_export_name.take() else {
            return;
        };
        let header = self.prompt_cache_header(&[
            ("name", json!(name)),
            ("path", json!(path.to_string_lossy())),
        ]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_exporting_status"),
            move || {
                export_flux2_prompt_cache(header).map(|()| Flux2PromptCacheOutcome::Exported(path))
            },
        );
    }

    /// Takes the file at `path` into the library.
    ///
    /// No `name` is sent: the backend then names the entry after the file's own stem,
    /// which is what the user just picked and therefore recognises.
    fn start_prompt_cache_import(&mut self, path: PathBuf) {
        let family = self.prompt_cache_family().to_string();
        let header = self.prompt_cache_header(&[("path", json!(path.to_string_lossy()))]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_importing_status"),
            move || import_flux2_prompt_cache(header, &family),
        );
    }

    /// Builds a prompt-cache request header: the normalized settings under `params`
    /// (which is what identifies the encoder family) plus the operation's own fields.
    fn prompt_cache_header(&self, extra: &[(&str, Value)]) -> Value {
        flux2_prompt_cache_header(&self.settings.normalized(), extra)
    }

    /// The encoder family the library listing describes, empty when it is not known yet.
    fn prompt_cache_family(&self) -> &str {
        self.prompt_cache_library
            .as_ref()
            .map_or("", |library| library.family.as_str())
    }

    /// Runs one prompt-cache `job` on a worker thread, showing `running` meanwhile.
    ///
    /// Refuses to start a second operation while one is in flight — the backend holds one
    /// pipeline and one library, and the buttons are gated on the same condition.
    fn start_prompt_cache_job<F>(&mut self, running: &str, job: F)
    where
        F: FnOnce() -> Result<Flux2PromptCacheOutcome, String> + Send + 'static,
    {
        if self.prompt_cache_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.prompt_cache_rx = Some(rx);
        self.prompt_cache_status = Some(running.to_string());
        // A warning belongs to the operation that produced it, not to the next one.
        self.prompt_cache_warning = None;
        thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    /// Polls the library listing and arms a new `.list` query when one is wanted.
    ///
    /// Same one-shot arming as `.status`, and for the same reason: a failing query must
    /// not spawn a thread per frame.
    fn poll_and_maybe_query_prompt_cache_list(&mut self) {
        if let Some(rx) = self.prompt_cache_list_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(library)) => {
                    self.prompt_cache_list_rx = None;
                    self.prompt_cache_list_error = None;
                    // A selection that the refreshed listing no longer contains is
                    // dropped rather than left pointing at a deleted entry.
                    if let Some(selected) = self.prompt_cache_selected.as_ref()
                        && !library.entries.iter().any(|entry| &entry.name == selected)
                    {
                        self.prompt_cache_selected = None;
                    }
                    self.prompt_cache_library = Some(library);
                }
                Ok(Err(err)) => {
                    self.prompt_cache_list_rx = None;
                    self.prompt_cache_list_error = Some(err);
                }
                Err(TryRecvError::Disconnected) => self.prompt_cache_list_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.prompt_cache_list_wanted
            && self.ai_backend_available
            && self.prompt_cache_list_rx.is_none()
            && self.prompt_cache_rx.is_none()
            && self.session.run_rx.is_none()
        {
            self.prompt_cache_list_wanted = false;
            let header = self.prompt_cache_header(&[]);
            let (tx, rx) = mpsc::channel();
            self.prompt_cache_list_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(list_flux2_prompt_caches(header));
            });
        }
    }

    /// Drains the prompt-cache channel into the status line under the prompt field.
    ///
    /// A finished BUILD re-arms `.status`, which is what turns the line above the buttons
    /// green. A finished LOAD additionally writes the prompt the file was built from into
    /// the field, so the user sees exactly what they loaded, and persists it.
    fn poll_prompt_cache(&mut self) {
        let Some(rx) = self.prompt_cache_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.prompt_cache_rx = None;
                self.prompt_cache_status = Some(self.apply_prompt_cache_outcome(outcome));
            }
            Ok(Err(err)) => {
                self.prompt_cache_rx = None;
                self.prompt_cache_status =
                    Some(tf!("cleaning.tools.flux2_klein.prompt_cache_error", err = err));
            }
            Err(TryRecvError::Disconnected) => {
                self.prompt_cache_rx = None;
                self.prompt_cache_status =
                    Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Applies one finished prompt-cache operation and returns the line to show for it.
    ///
    /// Save and import both change the library, so both re-arm `.list`: the combo must
    /// show the new entry without the user reopening the panel. A load whose
    /// `encoder_verified` came back `false` additionally raises the one-off notice that the
    /// entry's encoder identity was taken on trust.
    fn apply_prompt_cache_outcome(&mut self, outcome: Flux2PromptCacheOutcome) -> String {
        match outcome {
            Flux2PromptCacheOutcome::Built => {
                // The live cache changed, so the catalog's `prompt_cached` is stale.
                self.status_wanted = true;
                t!("cleaning.tools.flux2_klein.prompt_cache_built_status").to_string()
            }
            Flux2PromptCacheOutcome::Saved(name) => {
                self.prompt_cache_list_wanted = true;
                // The entry the user has just created is the one they will act on next.
                self.prompt_cache_selected = Some(name.clone());
                tf!(
                    "cleaning.tools.flux2_klein.prompt_cache_saved_status",
                    name = name
                )
            }
            // An entry that carries no prompt is refused rather than half-applied: an
            // empty prompt would block the run gate, and silently clearing the field would
            // look like the tool had lost the user's text.
            Flux2PromptCacheOutcome::Loaded(loaded) if loaded.prompt.is_empty() => {
                t!("cleaning.tools.flux2_klein.prompt_cache_load_empty_error").to_string()
            }
            Flux2PromptCacheOutcome::Loaded(loaded) => {
                self.settings.prompt = loaded.prompt;
                self.dirty = true;
                self.status_wanted = true;
                self.estimate_wanted = true;
                // The load SUCCEEDED and everything checkable was checked; what could not
                // be checked is the encoder's own fingerprint, because there is no local
                // encoder to compare it against. That is a one-off remark about this
                // operation — it rides the same warning slot as the foreign-family notice
                // and is cleared by the next operation — not a standing alarm.
                if loaded.encoder_verified == Some(false) {
                    self.prompt_cache_warning = Some(
                        t!("cleaning.tools.flux2_klein.prompt_cache_unverified_encoder_warning")
                            .to_string(),
                    );
                }
                t!("cleaning.tools.flux2_klein.prompt_cache_loaded_status").to_string()
            }
            Flux2PromptCacheOutcome::Exported(path) => tf!(
                "cleaning.tools.flux2_klein.prompt_cache_exported_status",
                path = path.display()
            ),
            Flux2PromptCacheOutcome::Imported {
                name,
                family_matches,
            } => {
                self.prompt_cache_list_wanted = true;
                // A foreign entry was still imported — into ITS family's folder, so it is
                // not lost — but it will not appear in this family's list and the backend
                // will refuse to load it. Saying so is the whole point: the alternative is
                // a successful import the user then cannot find anywhere.
                if family_matches == Some(false) {
                    self.prompt_cache_warning = Some(
                        t!("cleaning.tools.flux2_klein.prompt_cache_import_foreign_warning")
                            .to_string(),
                    );
                } else if !name.is_empty() {
                    self.prompt_cache_selected = Some(name.clone());
                }
                tf!(
                    "cleaning.tools.flux2_klein.prompt_cache_imported_status",
                    name = name
                )
            }
        }
    }

    /// Size requirements this engine imposes on the frame rectangle, in source page pixels.
    ///
    /// Not a `const`: `max_area` is a `u64` and the limit is declared in `usize`, and a
    /// saturating widening is not available in a constant. Saturating rather than panicking
    /// keeps the comparison monotonic on a target where the two widths differ.
    fn frame_constraints() -> FrameConstraints {
        FrameConstraints {
            multiple: FLUX2_SELECTION_MULTIPLE,
            min_side: FLUX2_MIN_SELECTION_PX,
            max_area: Some(u64::try_from(FLUX2_MAX_SELECTION_AREA_PX2).unwrap_or(u64::MAX)),
            max_aspect: Some(FLUX2_MAX_SELECTION_ASPECT),
        }
    }

    /// Region size the forecast and the status line describe, `None` without a frame.
    fn region_size(&self) -> Option<[usize; 2]> {
        self.region.map(|rect| [rect.w, rect.h])
    }
}

/// Whether two frame rectangles describe the same region.
///
/// `OverlayRectPx` derives no `PartialEq`, and adding one to the canvas types for this
/// engine alone is not this module's call.
fn same_region(a: Option<&OverlayRectPx>, b: Option<&OverlayRectPx>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// Whether two frame rectangles describe the same region SIZE.
///
/// Separate from [`same_region`] because the memory forecast is a question about the SIZE
/// alone: a rectangle that only moved forecasts identically, and asking the backend again
/// would be a round trip per scrolled pixel.
fn same_region_size(a: Option<&OverlayRectPx>, b: Option<&OverlayRectPx>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.w == b.w && a.h == b.h,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

impl AiEngine for Flux2KleinEngine {
    fn id(&self) -> &'static str {
        "flux2_klein"
    }

    fn title(&self) -> String {
        t!("cleaning.tools.flux2_klein.title").to_string()
    }

    fn section(&self) -> EngineSection {
        EngineSection::WithPrompt
    }

    fn requires_torch(&self) -> bool {
        true
    }

    fn constraints(&self) -> FrameConstraints {
        Self::frame_constraints()
    }

    /// ONE layer: the area the model is allowed to change. A second layer would have no
    /// meaning on the wire — the request carries exactly one mask.
    fn mask_layers(&self) -> Vec<MaskLayerSpec> {
        vec![MaskLayerSpec {
            tint: FLUX2_MASK_TINT,
            label_key: "cleaning.tools.flux2_klein.mask_heading",
        }]
    }

    /// «Работа без маски» is exactly this flag: with it set the whole region is
    /// regenerated and there is nothing to paint, so the host must stop demanding a
    /// non-empty mask. The host re-reads this whenever the parameters change.
    fn allows_empty_mask(&self) -> bool {
        self.settings.whole_region
    }

    fn draw_parameters(&mut self, ui: &mut egui::Ui) {
        let mut settings_changed = false;
        let mut want_status = false;
        let mut want_estimate = false;
        let mut unload_requested = false;
        let mut translate_requested = false;
        let mut prompt_cache_action: Option<Flux2PromptCacheAction> = None;
        let mut picker_requested: Option<Flux2PickerPurpose> = None;
        // Read before the destructure below borrows `settings` apart: the cache state is
        // derived from `status` AND `settings`, so it cannot be computed while both are
        // held by the context.
        let prompt_cache_state = self.prompt_cache_state();
        let region = self.region_size();
        {
            let Self {
                settings,
                status,
                status_error,
                estimate,
                estimate_error,
                unload_status,
                translate_status,
                translate_rx,
                estimate_rx,
                prompt_cache_rx,
                prompt_cache_status,
                prompt_cache_warning,
                prompt_cache_library,
                prompt_cache_list_rx,
                prompt_cache_list_error,
                prompt_cache_selected,
                prompt_cache_name_input,
                progress,
                ai_backend_available,
                run_status,
                ..
            } = self;
            let mut panel = Flux2PanelCtx {
                settings,
                status: status.as_ref(),
                status_error: status_error.as_deref(),
                estimate: estimate.as_ref(),
                estimate_error: estimate_error.as_deref(),
                unload_status,
                translate_status: translate_status.as_deref(),
                translate_busy: translate_rx.is_some(),
                estimate_busy: estimate_rx.is_some(),
                prompt_cache_state,
                prompt_cache_status: prompt_cache_status.as_deref(),
                prompt_cache_warning: prompt_cache_warning.as_deref(),
                prompt_cache_library: prompt_cache_library.as_ref(),
                prompt_cache_list_error: prompt_cache_list_error.as_deref(),
                prompt_cache_list_busy: prompt_cache_list_rx.is_some(),
                prompt_cache_busy: prompt_cache_rx.is_some(),
                prompt_cache_selected,
                prompt_cache_name_input,
                progress,
                run_status: run_status.as_deref(),
                ai_backend_available: *ai_backend_available,
                settings_changed: &mut settings_changed,
                want_status: &mut want_status,
                want_estimate: &mut want_estimate,
                unload_requested: &mut unload_requested,
                translate_requested: &mut translate_requested,
                prompt_cache_action: &mut prompt_cache_action,
                picker_requested: &mut picker_requested,
            };
            panel.draw(ui, region);
        }

        if settings_changed {
            self.dirty = true;
            // Any parameter can move the memory forecast, so it is re-armed as a whole
            // rather than diffed field by field.
            self.estimate_wanted = true;
        }
        if want_status {
            self.status_wanted = true;
        }
        if want_estimate {
            self.estimate_wanted = true;
        }
        if translate_requested {
            self.start_translate();
        }
        match prompt_cache_action {
            Some(Flux2PromptCacheAction::Build) => self.start_prompt_cache_build(),
            Some(Flux2PromptCacheAction::Save) => self.start_prompt_cache_save(),
            Some(Flux2PromptCacheAction::Load) => self.start_prompt_cache_load(),
            // The dialog is opened here and the entry is captured with it, so a selection
            // changed while it is open cannot redirect the export to another entry.
            Some(Flux2PromptCacheAction::Export) => {
                self.prompt_cache_export_name = self.prompt_cache_selected.clone();
                self.start_picker(Flux2PickerPurpose::PromptCacheExport);
            }
            Some(Flux2PromptCacheAction::Import) => {
                self.start_picker(Flux2PickerPurpose::PromptCacheImport);
            }
            None => {}
        }
        if let Some(purpose) = picker_requested {
            self.start_picker(purpose);
        }
        if unload_requested && self.unload_rx.is_none() {
            let (tx, rx) = mpsc::channel();
            self.unload_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(unload_flux2_klein());
            });
            self.unload_status =
                Some(t!("cleaning.tools.flux2_klein.unload_requested_status").to_string());
        }
    }

    /// Only the engine's own half of the gate — the model paths and the prompt. The
    /// rectangle is already validated against [`Self::constraints`] by the frame, and the
    /// non-empty-mask rule is [`Self::allows_empty_mask`].
    fn run_block_reason(&self) -> Option<String> {
        flux2_run_block_reason(&self.settings, self.prompt_cache_state())
    }

    /// Validates the host's request and starts the run.
    ///
    /// The size guarantees of `EngineRunRequest` are CHECKED rather than trusted: a request
    /// whose region or mask does not match `rect_px` would otherwise be encoded onto the
    /// wire and rejected by the backend with a message about the protocol instead of about
    /// the region. The user-facing refusal names the region; the numbers go to the log.
    ///
    /// # Errors
    /// Returns a localized message when a run is already in flight, when the region or the
    /// masks do not match `rect_px`, or when the region size breaks a model constraint.
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String> {
        let EngineRunRequest {
            page_idx,
            rect_px,
            region,
            masks,
        } = request;
        let size = [rect_px.w, rect_px.h];
        let expected_bytes = rect_px.w.saturating_mul(rect_px.h);
        let mask_ok = masks.len() == 1 && masks[0].len() == expected_bytes;
        if region.size != size || !mask_ok {
            crate::runtime_log::log_warn(format!(
                "[cleaning] FLUX.2 klein run request does not match the frame: page {page_idx}, rect {}x{} at ({}, {}), region {}x{}, {} mask layer(s) of {:?} bytes, expected {expected_bytes}",
                rect_px.w,
                rect_px.h,
                rect_px.x,
                rect_px.y,
                region.size[0],
                region.size[1],
                masks.len(),
                masks.iter().map(Vec::len).collect::<Vec<_>>()
            ));
            return Err(t!("cleaning.region.invalid_selection_size_error").to_string());
        }
        // Re-validated here as well as on the worker: the frame snaps to the same
        // constraints, but a host that hands over another size must be told which rule it
        // broke rather than have the backend refuse the blob.
        if let Some(reason) = region_block_reason(size) {
            return Err(reason);
        }
        // The painted layer is never overwritten: under `whole_region` a solid buffer is
        // built beside it, which is what the backend requires alongside the flag.
        let mask = mask_for_run(&masks[0], self.settings.whole_region);
        self.session
            .start_run(region, mask, size, &self.settings, &self.progress)?;
        // A run may have to load weights it did not have; the catalog and the forecast are
        // stale afterwards.
        self.status_wanted = true;
        self.estimate_wanted = true;
        self.run_status =
            Some(t!("cleaning.mask_editor.processing_background_status").to_string());
        Ok(())
    }

    /// Drains every channel the engine owns and reports the run.
    ///
    /// Called whether the parameter panel is drawn or not, which is why all the polling
    /// lives here: a finished run must land even while the panel is closed.
    fn poll(&mut self, ctx: &egui::Context) -> EnginePoll {
        self.poll_settings_load();
        self.poll_and_maybe_query_status();
        self.poll_unload();
        self.poll_translate();
        self.poll_prompt_cache();
        self.poll_and_maybe_query_prompt_cache_list();
        self.poll_picker();

        let mut settings_changed = false;
        let run = self.session.poll_run(
            &mut self.settings,
            &mut settings_changed,
            &mut self.run_status,
        );
        if settings_changed {
            self.dirty = true;
            self.estimate_wanted = true;
        }

        let region = self.region_size();
        self.poll_and_maybe_query_estimate(region);
        self.poll_and_maybe_save();

        match run {
            Flux2RunPoll::Idle => EnginePoll::Idle,
            Flux2RunPoll::Running => {
                // The progress bar moves on backend frames, not on user input, so the GUI
                // has to be woken even when nothing is touched.
                ctx.request_repaint();
                EnginePoll::Running
            }
            Flux2RunPoll::Done(image) => EnginePoll::Done(image),
            Flux2RunPoll::Failed(err) => EnginePoll::Failed(err),
        }
    }

    fn cancel(&mut self) {
        if self.session.cancel_run(&self.progress) {
            self.run_status =
                Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
        }
    }

    fn set_backend_available(&mut self, available: bool) {
        self.ai_backend_available = available;
    }

    /// Deliberately empty: the run button is the host's `AiButton` with
    /// `AiRequirement::Torch`, which resolves the runtime's presence itself. Storing a
    /// second copy of that answer here could only ever disagree with it.
    fn set_torch_available(&mut self, _available: bool) {}

    /// Publishes the frame rectangle, and arms the memory forecast only on a SETTLED SIZE
    /// change.
    ///
    /// The rectangle itself is stored unconditionally, so the status line keeps printing the
    /// live size while the user drags. The `.estimate` round trip is what is gated, on two
    /// rules that are one bug each if dropped:
    /// - the forecast depends on the SIZE alone, so a pure position change arms nothing —
    ///   the host pushes this every frame and the frame's keep-in-view clamp moves the
    ///   rectangle whenever the canvas is scrolled, which used to re-arm the forecast
    ///   continuously;
    /// - a resize drag publishes a new size every rendered frame, so the change is held in
    ///   `region_resize_pending` until `geometry_settled` — one request when the handle is
    ///   released, no matter how many sizes the drag passed through.
    ///
    /// The cancel-and-clear half stays keyed to the rectangle's IDENTITY: a rectangle that
    /// moved describes another part of the page, so a run in flight would answer about the
    /// previous one and the pre-run images kept for chaining runs stop describing anything.
    /// In practice it fires only on a deliberate move — a frame holding a run or a result is
    /// locked and neither the user nor the keep-in-view clamp moves it — but the engine
    /// cannot see the frame's lock and must not depend on it.
    fn set_region(&mut self, region: Option<OverlayRectPx>, geometry_settled: bool) {
        if !same_region_size(self.region.as_ref(), region.as_ref()) {
            self.region_resize_pending = true;
        }
        if !same_region(self.region.as_ref(), region.as_ref()) {
            self.region = region;
            if self.session.cancel_run(&self.progress) {
                self.run_status =
                    Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
            }
            self.session.clear();
        }
        if self.region_resize_pending && geometry_settled {
            self.region_resize_pending = false;
            self.estimate_wanted = true;
        }
    }
}

// ---------------------------------------------------------------------------------------
// Parameter panel body
// ---------------------------------------------------------------------------------------

/// Everything the parameter panel may read or mutate, borrowed for exactly one frame.
///
/// It exists so the panel can be split into small methods instead of one function with a
/// dozen locals, and so the intents the controls raise (re-query the catalog, open a
/// picker, start a translation) travel back to the engine as plain flags rather than as
/// work done from inside a widget closure.
struct Flux2PanelCtx<'a> {
    settings: &'a mut Flux2KleinSettings,
    status: Option<&'a Flux2Status>,
    status_error: Option<&'a str>,
    estimate: Option<&'a Flux2Estimate>,
    estimate_error: Option<&'a str>,
    unload_status: &'a mut Option<String>,
    translate_status: Option<&'a str>,
    translate_busy: bool,
    estimate_busy: bool,
    /// Three-state answer for the prompt currently in the field: `Some(true)` cached,
    /// `Some(false)` not cached, `None` not known yet.
    prompt_cache_state: Option<bool>,
    prompt_cache_status: Option<&'a str>,
    prompt_cache_warning: Option<&'a str>,
    prompt_cache_library: Option<&'a Flux2PromptCacheList>,
    prompt_cache_list_error: Option<&'a str>,
    prompt_cache_list_busy: bool,
    prompt_cache_busy: bool,
    prompt_cache_selected: &'a mut Option<String>,
    prompt_cache_name_input: &'a mut String,
    progress: &'a Arc<Mutex<Flux2Progress>>,
    /// The engine's own line about the last run, drawn under the progress bar.
    run_status: Option<&'a str>,
    ai_backend_available: bool,
    /// Set when a control changed a persisted value.
    settings_changed: &'a mut bool,
    /// Set when the component catalog should be re-queried.
    want_status: &'a mut bool,
    /// Set when the memory forecast should be re-queried.
    want_estimate: &'a mut bool,
    /// Set when the user asked to unload the backend pipeline.
    unload_requested: &'a mut bool,
    /// Set when the user pressed the "translate into English" arrow.
    translate_requested: &'a mut bool,
    /// The one prompt-cache control the user pressed this frame, if any.
    prompt_cache_action: &'a mut Option<Flux2PromptCacheAction>,
    /// At most one file dialog request per frame.
    picker_requested: &'a mut Option<Flux2PickerPurpose>,
}

impl Flux2PanelCtx<'_> {
    /// Draws the whole «Редактор области» body for this engine: its progress bar and run
    /// status, the prompt block, the parameters, and the working-mode switch.
    ///
    /// No scroll area and no run button: the panel that hosts this body owns its scrolling,
    /// and «Обработать» / «Применить» / «Отменить» belong to the frame and its host
    /// (`dev-docs/region_edit_v2_plan.md` §13.1).
    ///
    /// `region` is the frame rectangle's size, `None` while the tool has no frame; the
    /// controls that describe a region are then simply not drawn.
    fn draw(&mut self, ui: &mut egui::Ui, region: Option<[usize; 2]>) {
        draw_flux2_progress_ui(ui, self.progress);
        if let Some(status) = self.run_status {
            ui.small(status);
        }
        self.draw_prompt(ui);
        self.draw_params(ui, region);
        self.draw_mode_controls(ui);
    }

    /// Draws the prompt block: the optional user-language field with its translator
    /// row above, then the English field that is actually sent.
    fn draw_prompt(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        *self.settings_changed |= ui
            .checkbox(
                &mut self.settings.translate_prompt,
                t!("cleaning.tools.flux2_klein.translate_prompt_label"),
            )
            .changed();

        if self.settings.translate_prompt {
            ui.label(t!("cleaning.tools.flux2_klein.source_prompt_label"));
            *self.settings_changed |= ui
                .add(
                    egui::TextEdit::multiline(&mut self.settings.source_prompt)
                        .id_salt("cleaning_flux2_klein_source_prompt")
                        .hint_text(t!("cleaning.tools.flux2_klein.source_prompt_hint"))
                        .desired_rows(2),
                )
                .changed();
            ui.horizontal_wrapped(|ui| {
                ui.label(t!("cleaning.tools.flux2_klein.mt_service_label"));
                let mut service = MtService::from_key(&self.settings.mt_service)
                    .unwrap_or(MtService::Google);
                WheelComboBox::from_id_salt("cleaning_flux2_klein_mt_service")
                    .selected_text(service.title())
                    .show_ui(ui, |ui| {
                        for candidate in MtService::all() {
                            ui.selectable_value(&mut service, *candidate, candidate.title());
                        }
                    });
                if service.key() != self.settings.mt_service {
                    self.settings.mt_service = service.key().to_string();
                    *self.settings_changed = true;
                }

                ui.label(t!("cleaning.tools.flux2_klein.source_lang_label"));
                let mut lang = normalize_source_lang(&self.settings.source_lang);
                WheelComboBox::from_id_salt("cleaning_flux2_klein_source_lang")
                    .selected_text(source_lang_title(&lang))
                    .show_ui(ui, |ui| {
                        for candidate in MT_SOURCE_LANGUAGES {
                            ui.selectable_value(
                                &mut lang,
                                candidate.code.to_string(),
                                candidate.title(),
                            );
                        }
                    });
                if lang != self.settings.source_lang {
                    self.settings.source_lang = lang;
                    *self.settings_changed = true;
                }

                let can_translate =
                    !self.translate_busy && !self.settings.source_prompt.trim().is_empty();
                // A glyph, not prose: the caption is the tooltip.
                let arrow = ui
                    .add_enabled(can_translate, egui::Button::new("↓"))
                    .on_hover_text(t!("cleaning.tools.flux2_klein.translate_button_tooltip"))
                    .on_disabled_hover_text(t!(
                        "cleaning.tools.flux2_klein.translate_button_disabled_tooltip"
                    ));
                if arrow.clicked() {
                    *self.translate_requested = true;
                }
                if self.translate_busy {
                    ui.spinner();
                    ui.ctx().request_repaint();
                }
            });
            if let Some(status) = self.translate_status {
                ui.small(status);
            }
        }

        ui.label(t!("cleaning.tools.flux2_klein.prompt_label"));
        let prompt_edited = ui
            .add(
                egui::TextEdit::multiline(&mut self.settings.prompt)
                    .id_salt("cleaning_flux2_klein_prompt")
                    .hint_text(t!("cleaning.tools.flux2_klein.prompt_hint"))
                    .desired_rows(3),
            )
            .changed();
        *self.settings_changed |= prompt_edited;
        if prompt_edited {
            // `prompt_cached` is an answer about ONE prompt, so an edited field makes the
            // catalog stale in exactly the way a changed model path does. Re-arming the
            // one-shot flag (rather than firing a query here) is what keeps a keystroke
            // from becoming a request: at most one query is ever in flight, and the flag
            // simply stays armed until it returns — the same discipline the memory
            // forecast uses.
            *self.want_status = true;
        }
        self.draw_prompt_cache(ui);
    }

    /// Draws the prompt-cache block directly under the English prompt field: the cache
    /// line, «Кэшировать», the save-under-a-name row, and the library row (the saved
    /// entries of the current encoder family plus load / export / import).
    ///
    /// The line is the reason the rest is there: encoding a new prompt costs a ~106 s read
    /// of the 16 GB Qwen3 encoder, while a cached one costs ~6 s, so whether THIS prompt
    /// is cached is the single fact that decides how long the next run takes.
    fn draw_prompt_cache(&mut self, ui: &mut egui::Ui) {
        match self.prompt_cache_state {
            Some(true) => {
                ui.colored_label(
                    FLUX2_STATUS_OK_COLOR,
                    t!("cleaning.tools.flux2_klein.prompt_cached_status"),
                );
            }
            Some(false) => {
                ui.colored_label(
                    FLUX2_STATUS_WARN_COLOR,
                    t!("cleaning.tools.flux2_klein.prompt_not_cached_status"),
                );
            }
            // Not known yet — see `Flux2KleinEngine::prompt_cache_state`. A neutral line,
            // never the warning one: the answer is outstanding, not negative.
            None => {
                ui.small(t!("cleaning.tools.flux2_klein.prompt_cache_unknown_status"));
            }
        }

        let text_encoder_available = self.text_encoder_available();
        // Only a positive `false` counts: "not known" must neither warn nor close a button.
        let encoder_missing = text_encoder_available == Some(false);
        // A WARNING, not an error: without an encoder the tool still generates from ready
        // caches, and only encoding a new prompt is closed. The amber of "not cached" says
        // exactly that — something is limited, nothing is broken.
        if encoder_missing {
            ui.colored_label(
                FLUX2_STATUS_WARN_COLOR,
                t!("cleaning.tools.flux2_klein.text_encoder_missing_warning"),
            );
        }

        let gates = flux2_prompt_cache_gates(
            self.settings,
            self.prompt_cache_state,
            text_encoder_available,
            self.prompt_cache_name_input,
            self.prompt_cache_selected.is_some(),
            self.ai_backend_available,
            self.prompt_cache_busy,
        );
        // The two encode-only controls need their own explanation when it is the missing
        // encoder that closed them: the generic tooltip tells the user to fill in a path,
        // which is precisely what they cannot do on this machine.
        let build_disabled_tooltip = if encoder_missing {
            t!("cleaning.tools.flux2_klein.prompt_cache_encoder_missing_disabled_tooltip")
        } else {
            t!("cleaning.tools.flux2_klein.prompt_cache_build_disabled_tooltip")
        };
        let save_disabled_tooltip = if encoder_missing {
            t!("cleaning.tools.flux2_klein.prompt_cache_encoder_missing_disabled_tooltip")
        } else {
            t!("cleaning.tools.flux2_klein.prompt_cache_save_disabled_tooltip")
        };
        // Collected from the rows below and written once at the end: every row needs
        // `self`, and an `Option` assignment keeps "at most one action per frame" a
        // property of the code rather than a convention.
        let mut action: Option<Flux2PromptCacheAction> = None;

        ui.horizontal_wrapped(|ui| {
            if prompt_cache_button(
                ui,
                gates.build,
                t!("cleaning.tools.flux2_klein.prompt_cache_build_button"),
                t!("cleaning.tools.flux2_klein.prompt_cache_build_tooltip"),
                build_disabled_tooltip,
            ) {
                action = Some(Flux2PromptCacheAction::Build);
            }
            if self.prompt_cache_busy {
                ui.spinner();
                ui.ctx().request_repaint();
            }
        });

        // Saving asks for a NAME, in the same shape the watermark library and the typing
        // presets ask for one: an inline field beside the button that commits it, not a
        // modal of this tool's own.
        ui.horizontal(|ui| {
            let field_width = (ui.available_width() - FLUX2_CACHE_NAME_BUTTON_RESERVE).max(80.0);
            ui.add(
                egui::TextEdit::singleline(self.prompt_cache_name_input)
                    .id_salt("cleaning_flux2_klein_prompt_cache_name")
                    .hint_text(t!("cleaning.tools.flux2_klein.prompt_cache_name_hint"))
                    .desired_width(field_width),
            );
            if prompt_cache_button(
                ui,
                gates.save,
                t!("cleaning.tools.flux2_klein.prompt_cache_save_button"),
                t!("cleaning.tools.flux2_klein.prompt_cache_save_tooltip"),
                save_disabled_tooltip,
            ) {
                action = Some(Flux2PromptCacheAction::Save);
            }
        });

        ui.horizontal_wrapped(|ui| {
            self.draw_prompt_cache_library_combo(ui);
            for (enabled, caption, tooltip, disabled_tooltip, requested) in [
                (
                    gates.load,
                    t!("cleaning.tools.flux2_klein.prompt_cache_load_button"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_load_tooltip"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_load_disabled_tooltip"),
                    Flux2PromptCacheAction::Load,
                ),
                (
                    gates.export,
                    t!("cleaning.tools.flux2_klein.prompt_cache_export_button"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_export_tooltip"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_export_disabled_tooltip"),
                    Flux2PromptCacheAction::Export,
                ),
                (
                    gates.import,
                    t!("cleaning.tools.flux2_klein.prompt_cache_import_button"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_import_tooltip"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_import_disabled_tooltip"),
                    Flux2PromptCacheAction::Import,
                ),
            ] {
                if prompt_cache_button(ui, enabled, caption, tooltip, disabled_tooltip) {
                    action = Some(requested);
                }
            }
            if self.prompt_cache_list_busy {
                ui.spinner();
                ui.ctx().request_repaint();
            }
        });

        if let Some(action) = action {
            *self.prompt_cache_action = Some(action);
        }
        if let Some(error) = self.prompt_cache_list_error {
            ui.colored_label(
                FLUX2_STATUS_ERROR_COLOR,
                tf!(
                    "cleaning.tools.flux2_klein.prompt_cache_list_error",
                    err = error
                ),
            );
        }
        if let Some(status) = self.prompt_cache_status {
            ui.small(status);
        }
        if let Some(warning) = self.prompt_cache_warning {
            ui.colored_label(FLUX2_STATUS_WARN_COLOR, warning);
        }
    }

    /// Whether a text encoder is installed on this machine, as the backend reports it.
    ///
    /// `.status` is the primary source because it is re-queried on every settings change;
    /// the library listing answers the same question and is the fallback, so the warning
    /// does not disappear while a status query is outstanding. `None` from both means the
    /// answer is unknown and nothing is claimed.
    fn text_encoder_available(&self) -> Option<bool> {
        self.status
            .and_then(|status| status.text_encoder_available)
            .or_else(|| {
                self.prompt_cache_library
                    .and_then(|library| library.text_encoder_available)
            })
    }

    /// Draws the picker of saved library entries.
    ///
    /// The selection is a NAME, so a refreshed listing can neither move it to a different
    /// entry nor keep one that has disappeared. An empty library (or one not listed yet)
    /// shows a placeholder and offers no rows — the load and export buttons are gated on
    /// the selection, so nothing can act on it.
    ///
    /// **With no active family the listing spans the whole library**, so each row is
    /// labelled with the family it belongs to: the user is then looking at other encoders'
    /// caches as well as their own, and a bare name would hide that. The wire still
    /// identifies an entry by NAME alone, so the family is display-only — and a name
    /// present in two families is refused by the backend rather than resolved at random.
    fn draw_prompt_cache_library_combo(&mut self, ui: &mut egui::Ui) {
        let entries: &[Flux2PromptCacheEntry] = self
            .prompt_cache_library
            .map_or(&[], |library| library.entries.as_slice());
        // An empty top-level family is the backend saying that none is active — which is
        // the case in which entries of several families share one listing.
        let show_family = self
            .prompt_cache_library
            .is_some_and(|library| library.family.is_empty());
        let selected_text = match self.prompt_cache_selected.as_deref() {
            Some(name) => entries
                .iter()
                .find(|entry| entry.name == name)
                .map_or_else(|| name.to_string(), |entry| entry.label(show_family)),
            None if entries.is_empty() => {
                t!("cleaning.tools.flux2_klein.prompt_cache_library_empty").to_string()
            }
            None => t!("cleaning.tools.flux2_klein.prompt_cache_library_none").to_string(),
        };
        let mut picked: Option<String> = None;
        WheelComboBox::from_id_salt("cleaning_flux2_klein_prompt_cache_library")
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                for entry in entries {
                    let selected = self.prompt_cache_selected.as_deref() == Some(entry.name.as_str());
                    // The prompt an entry encodes is what tells two similar names apart,
                    // and it is far too long for a combo row — so it lives on hover,
                    // together with the creation date.
                    if ui
                        .selectable_label(selected, entry.label(show_family))
                        .on_hover_text(prompt_cache_entry_tooltip(entry, show_family))
                        .clicked()
                    {
                        picked = Some(entry.name.clone());
                    }
                }
            });
        if let Some(name) = picked {
            *self.prompt_cache_selected = Some(name);
        }
    }

    /// Draws the collapsed-by-default parameter section: model paths, generation
    /// parameters, the memory preset with its forecast, the nested advanced section,
    /// and the backend catalog/unload controls.
    fn draw_params(&mut self, ui: &mut egui::Ui, region: Option<[usize; 2]>) {
        let settings = &mut *self.settings;
        let changed = &mut *self.settings_changed;
        let want_status = &mut *self.want_status;
        let want_estimate = &mut *self.want_estimate;
        let unload_requested = &mut *self.unload_requested;
        let picker_requested = &mut *self.picker_requested;
        let unload_status = &mut *self.unload_status;
        let status = self.status;
        let status_error = self.status_error;
        let estimate = self.estimate;
        let estimate_error = self.estimate_error;
        let estimate_busy = self.estimate_busy;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            "cleaning_flux2_klein_params",
            t!("cleaning.tools.flux2_klein.params_heading"),
            false,
            |ui| {
                draw_path_row(
                    ui,
                    t!("cleaning.tools.flux2_klein.text_encoder_path_label"),
                    "cleaning_flux2_klein_text_encoder_path",
                    &mut settings.text_encoder_path,
                    &[(
                        "📁",
                        t!("cleaning.tools.flux2_klein.browse_folder_tooltip"),
                        Flux2PickerPurpose::TextEncoderDir,
                    )],
                    changed,
                    picker_requested,
                );
                draw_path_row(
                    ui,
                    t!("cleaning.tools.flux2_klein.transformer_path_label"),
                    "cleaning_flux2_klein_transformer_path",
                    &mut settings.transformer_path,
                    &[
                        (
                            "📄",
                            t!("cleaning.tools.flux2_klein.browse_file_tooltip"),
                            Flux2PickerPurpose::TransformerFile,
                        ),
                        (
                            "📁",
                            t!("cleaning.tools.flux2_klein.browse_folder_tooltip"),
                            Flux2PickerPurpose::TransformerDir,
                        ),
                    ],
                    changed,
                    picker_requested,
                );
                draw_path_row(
                    ui,
                    t!("cleaning.tools.flux2_klein.vae_path_label"),
                    "cleaning_flux2_klein_vae_path",
                    &mut settings.vae_path,
                    &[
                        (
                            "📄",
                            t!("cleaning.tools.flux2_klein.browse_file_tooltip"),
                            Flux2PickerPurpose::VaeFile,
                        ),
                        (
                            "📁",
                            t!("cleaning.tools.flux2_klein.browse_folder_tooltip"),
                            Flux2PickerPurpose::VaeDir,
                        ),
                    ],
                    changed,
                    picker_requested,
                );

                ui.separator();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.steps, FLUX2_STEPS_MIN..=FLUX2_STEPS_MAX)
                            .text(t!("cleaning.common.steps_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.flux2_klein.steps_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(
                            &mut settings.guidance_scale,
                            FLUX2_GUIDANCE_MIN..=FLUX2_GUIDANCE_MAX,
                        )
                        .text("Guidance"),
                    )
                    .on_hover_text(t!("cleaning.tools.flux2_klein.guidance_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(
                            &mut settings.strength,
                            FLUX2_STRENGTH_MIN..=FLUX2_STRENGTH_MAX,
                        )
                        .text(t!("cleaning.tools.flux2_klein.strength_label")),
                    )
                    .changed();
                ui.horizontal(|ui| {
                    *changed |= ui
                        .checkbox(
                            &mut settings.use_seed,
                            t!("cleaning.tools.flux2_klein.fixed_seed_label"),
                        )
                        .changed();
                    if settings.use_seed {
                        *changed |= SeedSpinBox::new(&mut settings.seed).draw(ui).changed();
                    }
                });

                ui.separator();
                draw_preset_row(ui, settings, changed);
                draw_estimate_ui(ui, estimate, estimate_error, estimate_busy, status);
                if ui
                    .small_button(t!("cleaning.tools.flux2_klein.refresh_estimate_button"))
                    .clicked()
                {
                    *want_estimate = true;
                }

                draw_advanced_section(ui, settings, changed);

                ui.separator();
                draw_status_ui(ui, status, status_error, region);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .small_button(t!("cleaning.tools.flux2_klein.refresh_status_button"))
                        .clicked()
                    {
                        *want_status = true;
                    }
                    if ui
                        .small_button(t!("cleaning.tools.flux2_klein.unload_button"))
                        .clicked()
                    {
                        *unload_requested = true;
                    }
                });
                if let Some(text) = unload_status.as_ref() {
                    ui.small(text);
                }
            },
        );
    }

    /// Draws the working-mode switch: «Работа без маски», which decides whether the
    /// painted mask is consulted at all.
    ///
    /// The brush itself is NOT here: the radius, paint/erase, clear and fill belong to the
    /// host's «Выбранный инструмент» panel, which drives the frame's one `MaskBrush` for
    /// every engine. What stays is the one control that changes the MEANING of the mask —
    /// with the switch on, the host is told to accept an empty mask
    /// ([`Flux2KleinEngine::allows_empty_mask`]) and the painted layer is never read, so it
    /// survives the mode byte for byte.
    fn draw_mode_controls(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label(t!("cleaning.tools.flux2_klein.mask_heading"));
        *self.settings_changed |= ui
            .checkbox(
                &mut self.settings.whole_region,
                t!("cleaning.tools.flux2_klein.whole_region_label"),
            )
            .on_hover_text(t!("cleaning.tools.flux2_klein.whole_region_hint"))
            .changed();
        if self.settings.whole_region {
            ui.small(t!("cleaning.tools.flux2_klein.whole_region_mask_hint"));
            return;
        }
        ui.small(t!("cleaning.tools.flux2_klein.mask_hint"));
    }
}

/// The first reason a run cannot start that this ENGINE can see, or `None` when it can.
///
/// It covers the model paths and the prompt, and nothing else. The two checks that used to
/// live here belong to the host now and must not be duplicated: the region size is
/// validated by the frame against [`Flux2KleinEngine::constraints`] (and again on the worker
/// by [`region_block_reason`], which is what protects the wire), and "is anything painted?"
/// is [`Flux2KleinEngine::allows_empty_mask`], which the frame's own process button honours.
///
/// `prompt_cached` is the three-state `.status` answer for the prompt in the field. **A
/// cached prompt waives the text encoder**: the denoise and the VAE decode never look at
/// it, so a `.msprompt` carried to a machine that never downloaded the 16 GB Qwen3 is
/// enough to run, and blocking there would hide a run that works. The waiver needs
/// `Some(true)` and nothing weaker: `None` means the answer is outstanding — or that the
/// backend does not report the field at all, and such a backend cannot generate without an
/// encoder either, so an enabled button would only offer a run that always fails.
/// The transformer, the VAE, the tokenizer and the scheduler are never waived; the backend
/// makes the final decision either way (`_first_unavailable_reason`) and this gate exists
/// to explain it before the click, not to duplicate it.
///
/// A free function rather than a method for the same reason [`region_block_reason`] is
/// one — the gate is the contract worth testing, and it must be testable without an engine
/// instance, which owns channels and worker state.
fn flux2_run_block_reason(
    settings: &Flux2KleinSettings,
    prompt_cached: Option<bool>,
) -> Option<String> {
    // Trimmed here rather than through `normalized()`: this runs on every frame of an
    // open panel, and the only thing `normalized()` would add for these four fields is
    // the trim — at the price of rebuilding the whole settings struct.
    let encoder_waived = prompt_cached == Some(true);
    if settings.transformer_path.trim().is_empty() || settings.vae_path.trim().is_empty() {
        // Two messages, because naming a path the run does not need would send the user
        // looking for a 16 GB download they have already worked around.
        return Some(if encoder_waived {
            t!("cleaning.tools.flux2_klein.model_paths_required_error").to_string()
        } else {
            t!("cleaning.tools.flux2_klein.paths_required_error").to_string()
        });
    }
    if !encoder_waived && settings.text_encoder_path.trim().is_empty() {
        return Some(t!("cleaning.tools.flux2_klein.paths_required_error").to_string());
    }
    if settings.prompt.trim().is_empty() {
        return Some(t!("cleaning.tools.flux2_klein.prompt_required_error").to_string());
    }
    None
}

/// Validates a REGION SIZE against the model's hard constraints, naming the first one
/// it breaks. Shared by the run gate and the worker, so the two cannot disagree.
fn region_block_reason(region: [usize; 2]) -> Option<String> {
    let [w, h] = region;
    if w == 0 || h == 0 {
        return Some(t!("cleaning.region.invalid_selection_size_error").to_string());
    }
    if w % FLUX2_SELECTION_MULTIPLE != 0 || h % FLUX2_SELECTION_MULTIPLE != 0 {
        return Some(tf!(
            "cleaning.tools.flux2_klein.region_multiple_error",
            mult = FLUX2_SELECTION_MULTIPLE,
            w = w,
            h = h
        ));
    }
    if w.min(h) < FLUX2_MIN_SELECTION_PX {
        return Some(tf!(
            "cleaning.region.min_selection_error",
            min = FLUX2_MIN_SELECTION_PX,
            w = w,
            h = h
        ));
    }
    if w.saturating_mul(h) > FLUX2_MAX_SELECTION_AREA_PX2 {
        return Some(tf!(
            "cleaning.region.max_selection_area_error",
            max = FLUX2_MAX_SELECTION_AREA_PX2,
            w = w,
            h = h
        ));
    }
    let long = w.max(h);
    let short = w.min(h).max(1);
    // Same rearrangement as `RegionEditToolBase::check_selection_limits`, so the two
    // gates accept exactly the same set of regions.
    if (long as f32) > (FLUX2_MAX_SELECTION_ASPECT * short as f32).floor() {
        return Some(tf!(
            "cleaning.region.max_aspect_error",
            ratio = format!("{FLUX2_MAX_SELECTION_ASPECT:.0}"),
            w = w,
            h = h
        ));
    }
    None
}

// ---------------------------------------------------------------------------------------
// UI helpers
// ---------------------------------------------------------------------------------------

/// Draws one editable model-path row: a label, a text field, and one browse button per
/// `(glyph, tooltip, purpose)` entry. Returns nothing; `changed` and `picker_requested`
/// carry the outcome.
fn draw_path_row(
    ui: &mut egui::Ui,
    label: &str,
    id_salt: &'static str,
    value: &mut String,
    buttons: &[(&'static str, &str, Flux2PickerPurpose)],
    changed: &mut bool,
    picker_requested: &mut Option<Flux2PickerPurpose>,
) {
    ui.label(label);
    ui.horizontal(|ui| {
        *changed |= ui
            .add(
                egui::TextEdit::singleline(value)
                    .id_salt(id_salt)
                    .desired_width(ui.available_width() - 64.0),
            )
            .changed();
        for (glyph, tooltip, purpose) in buttons {
            // Glyph buttons: an icon is not prose, so the caption stays literal and the
            // localized text lives in the tooltip.
            if ui.small_button(*glyph).on_hover_text(*tooltip).clicked() {
                *picker_requested = Some(*purpose);
            }
        }
    });
}

/// Draws one prompt-cache button and reports whether it was clicked.
///
/// Every one of the five is disabled for a reason the user cannot see from the button
/// itself (no cache yet, no selection, an unreachable backend), so a disabled tooltip is
/// not optional here — it is the only place that reason is stated.
fn prompt_cache_button(
    ui: &mut egui::Ui,
    enabled: bool,
    caption: &str,
    tooltip: &str,
    disabled_tooltip: &str,
) -> bool {
    ui.add_enabled(enabled, egui::Button::new(caption))
        .on_hover_text(tooltip)
        .on_disabled_hover_text(disabled_tooltip)
        .clicked()
}

/// Hover text of one library row: the prompt the entry encodes, when it was made, and —
/// while the listing spans several families — which family it belongs to.
///
/// `show_family` is the combo's own decision (no active family), so the row and its
/// tooltip can never disagree about whether the family is on screen.
fn prompt_cache_entry_tooltip(entry: &Flux2PromptCacheEntry, show_family: bool) -> String {
    let head = if entry.created.is_empty() {
        entry.prompt.clone()
    } else {
        tf!(
            "cleaning.tools.flux2_klein.prompt_cache_entry_tooltip",
            prompt = entry.prompt,
            created = entry.created
        )
    };
    if !show_family || entry.family.is_empty() {
        return head;
    }
    let family = tf!(
        "cleaning.tools.flux2_klein.prompt_cache_entry_family",
        family = entry.family
    );
    if head.is_empty() {
        return family;
    }
    format!("{head}\n{family}")
}

/// Draws the memory-preset picker. `Пользовательский` is shown when the current values
/// match no preset but is never offered as a choice.
fn draw_preset_row(ui: &mut egui::Ui, settings: &mut Flux2KleinSettings, changed: &mut bool) {
    let active = MemoryPreset::detect(settings);
    let mut picked: Option<MemoryPreset> = None;
    ui.horizontal(|ui| {
        ui.label(t!("cleaning.tools.flux2_klein.preset_label"));
        WheelComboBox::from_id_salt("cleaning_flux2_klein_preset")
            .selected_text(active.label())
            .show_ui(ui, |ui| {
                for preset in MemoryPreset::selectable() {
                    if ui
                        .selectable_label(preset == active, preset.label())
                        .clicked()
                    {
                        picked = Some(preset);
                    }
                }
            });
    });
    if let Some(preset) = picked {
        *changed |= preset.apply(settings);
    }
}

/// Draws the backend's memory forecast, warning visibly when it does not fit.
fn draw_estimate_ui(
    ui: &mut egui::Ui,
    estimate: Option<&Flux2Estimate>,
    error: Option<&str>,
    busy: bool,
    status: Option<&Flux2Status>,
) {
    if busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.small(t!("cleaning.tools.flux2_klein.estimate_running_status"));
        });
        ui.ctx().request_repaint();
    }
    if let Some(error) = error {
        ui.small(tf!("cleaning.tools.flux2_klein.estimate_error", err = error));
    }
    let Some(estimate) = estimate else {
        if !busy && error.is_none() {
            ui.small(t!("cleaning.tools.flux2_klein.estimate_unknown_status"));
        }
        return;
    };
    // The free figures are what `fits` was actually computed against; the totals come
    // from `.status` and turn "9.8 free" into a proportion the user can judge. They are
    // dropped, not printed as "0.0", while `.status` has not answered — that call can
    // fail on its own, and «из 0,0 ГиБ» would be a lie rather than a missing figure.
    let vram_total = status.map_or(0, |s| s.vram_total);
    let ram_total = status.map_or(0, |s| s.ram_total);
    let line = if vram_total > 0 && ram_total > 0 {
        tf!(
            "cleaning.tools.flux2_klein.estimate_status",
            vram = format_gib(estimate.vram_bytes),
            vram_free = format_gib(estimate.vram_free),
            vram_total = format_gib(vram_total),
            ram = format_gib(estimate.ram_bytes),
            ram_free = format_gib(estimate.ram_free),
            ram_total = format_gib(ram_total)
        )
    } else {
        tf!(
            "cleaning.tools.flux2_klein.estimate_status_no_totals",
            vram = format_gib(estimate.vram_bytes),
            vram_free = format_gib(estimate.vram_free),
            ram = format_gib(estimate.ram_bytes),
            ram_free = format_gib(estimate.ram_free)
        )
    };
    // The line stays one short sentence; the per-phase peaks and the full per-component
    // breakdown live in its tooltip, because the forecast is max(prompt encoding,
    // denoise, decode) and which of the phases dominates is the actionable part.
    let tooltip = estimate_tooltip(estimate);
    if estimate.fits {
        ui.small(line).on_hover_text(tooltip);
    } else {
        ui.colored_label(FLUX2_STATUS_WARN_COLOR, line)
            .on_hover_text(tooltip);
        ui.colored_label(
            FLUX2_STATUS_WARN_COLOR,
            t!("cleaning.tools.flux2_klein.estimate_does_not_fit_warning"),
        );
    }
}

/// Builds the hover text of the forecast line: the per-phase peaks first, in pipeline
/// order (prompt encoding, denoise, VAE decode — the VRAM figure is the LARGEST of them,
/// not their sum), then every other breakdown entry the backend reported.
///
/// Breakdown keys are backend identifiers, so they stay literal; their captions and
/// the unit around them come from the locale, so every figure on this screen is
/// labelled in the same unit the figures are actually computed in (gibibytes).
fn estimate_tooltip(estimate: &Flux2Estimate) -> String {
    let peaks = split_estimate_peaks(estimate);
    let mut lines = Vec::<String>::new();
    if let Some(bytes) = peaks.encode {
        lines.push(tf!(
            "cleaning.tools.flux2_klein.estimate_peak_encode",
            size = format_gib(bytes)
        ));
    }
    if let Some(bytes) = peaks.denoise {
        lines.push(tf!(
            "cleaning.tools.flux2_klein.estimate_peak_denoise",
            size = format_gib(bytes)
        ));
    }
    if let Some(bytes) = peaks.decode {
        lines.push(tf!(
            "cleaning.tools.flux2_klein.estimate_peak_decode",
            size = format_gib(bytes)
        ));
    }
    for (key, bytes) in peaks.others {
        lines.push(tf!(
            "cleaning.tools.flux2_klein.estimate_breakdown_entry",
            key = key,
            size = format_gib(bytes)
        ));
    }
    if lines.is_empty() {
        return t!("cleaning.tools.flux2_klein.estimate_no_breakdown_status").to_string();
    }
    lines.join("\n")
}

/// A breakdown split into its per-phase peaks and everything else, borrowed from the
/// [`Flux2Estimate`] it was read from.
///
/// A named struct rather than a tuple: three `Option<u64>` fields in a row are exactly
/// the shape where a positional swap survives review unnoticed.
#[derive(Debug, Default, PartialEq, Eq)]
struct Flux2EstimatePeaks<'a> {
    /// Peak of the prompt-encoding phase, `None` when the backend did not report one
    /// (a build predating the phase-wise forecast).
    encode: Option<u64>,
    denoise: Option<u64>,
    decode: Option<u64>,
    /// Every breakdown entry that is not one of the peaks above, in the order the
    /// backend's object parsed into (key order — see [`Flux2Estimate::breakdown`]).
    others: Vec<(&'a str, u64)>,
}

/// Splits a breakdown into its per-phase peaks and the remaining entries, which is the
/// order the tooltip lists them in. Each peak is `None` when the backend did not report
/// it, and none of them is repeated among `others`.
///
/// Separate from [`estimate_tooltip`] because the ORDER and the de-duplication are the
/// contract worth testing, while the rendered text depends on a loaded locale catalog.
fn split_estimate_peaks(estimate: &Flux2Estimate) -> Flux2EstimatePeaks<'_> {
    let peak = |name: &str| {
        estimate
            .breakdown
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, bytes)| *bytes)
    };
    const PEAK_KEYS: [&str; 3] = [
        FLUX2_BREAKDOWN_PEAK_ENCODE,
        FLUX2_BREAKDOWN_PEAK_DENOISE,
        FLUX2_BREAKDOWN_PEAK_DECODE,
    ];
    let others = estimate
        .breakdown
        .iter()
        .filter(|(key, _)| !PEAK_KEYS.contains(&key.as_str()))
        .map(|(key, bytes)| (key.as_str(), *bytes))
        .collect();
    Flux2EstimatePeaks {
        encode: peak(FLUX2_BREAKDOWN_PEAK_ENCODE),
        denoise: peak(FLUX2_BREAKDOWN_PEAK_DENOISE),
        decode: peak(FLUX2_BREAKDOWN_PEAK_DECODE),
        others,
    }
}

/// Draws the component catalog from `.status` plus the current region size.
fn draw_status_ui(
    ui: &mut egui::Ui,
    status: Option<&Flux2Status>,
    error: Option<&str>,
    region: Option<[usize; 2]>,
) {
    // No frame on the canvas yet: there is no region to report, and printing `0x0` would
    // read as a broken selection rather than as an absent one.
    if let Some([w, h]) = region {
        ui.small(tf!(
            "cleaning.tools.flux2_klein.region_size_status",
            w = w,
            h = h
        ));
    }
    if let Some(error) = error {
        ui.colored_label(
            FLUX2_STATUS_ERROR_COLOR,
            tf!("cleaning.tools.flux2_klein.status_error", err = error),
        );
    }
    let Some(status) = status else {
        ui.small(t!("cleaning.tools.flux2_klein.status_unknown_status"));
        return;
    };
    if !status.available && !status.reason.is_empty() {
        ui.colored_label(FLUX2_STATUS_WARN_COLOR, status.reason.as_str());
    }
    for (label, component) in [
        (
            t!("cleaning.tools.flux2_klein.component_text_encoder"),
            &status.text_encoder,
        ),
        (
            t!("cleaning.tools.flux2_klein.component_transformer"),
            &status.transformer,
        ),
        (t!("cleaning.tools.flux2_klein.component_vae"), &status.vae),
        (
            t!("cleaning.tools.flux2_klein.component_tokenizer"),
            &status.tokenizer,
        ),
        (
            t!("cleaning.tools.flux2_klein.component_scheduler"),
            &status.scheduler,
        ),
    ] {
        let mark = if component.present { "✓" } else { "✗" };
        let line = if component.size_bytes > 0 {
            tf!(
                "cleaning.tools.flux2_klein.component_sized_status",
                mark = mark,
                name = label,
                size = format_gib(component.size_bytes)
            )
        } else {
            tf!(
                "cleaning.tools.flux2_klein.component_status",
                mark = mark,
                name = label
            )
        };
        let response = ui.small(line);
        // The path the BACKEND resolved, which is the only way to tell a typo in the
        // field above from a genuinely missing file.
        if !component.path.is_empty() {
            response.on_hover_text(component.path.as_str());
        }
    }
    if !status.device.is_empty() {
        ui.small(tf!(
            "cleaning.tools.flux2_klein.device_status",
            device = status.device
        ));
    }
    if status.loaded {
        ui.small(t!("cleaning.tools.flux2_klein.pipeline_loaded_status"));
    }
}

/// Draws the nested advanced section: placement, dtype, VAE flags, text-encoder memory
/// handling and mask shaping.
fn draw_advanced_section(
    ui: &mut egui::Ui,
    settings: &mut Flux2KleinSettings,
    changed: &mut bool,
) {
    RegionEditToolBase::draw_region_editor_collapsible_section(
        ui,
        "cleaning_flux2_klein_advanced",
        t!("cleaning.tools.flux2_klein.advanced_heading"),
        false,
        |ui| {
            ui.horizontal(|ui| {
                ui.label(t!("cleaning.tools.flux2_klein.placement_label"));
                let mut placement = Flux2Placement::from_wire(&settings.placement);
                WheelComboBox::from_id_salt("cleaning_flux2_klein_placement")
                    .selected_text(placement.label())
                    .show_ui(ui, |ui| {
                        for candidate in Flux2Placement::all() {
                            ui.selectable_value(&mut placement, candidate, candidate.label());
                        }
                    });
                if placement.wire() != settings.placement {
                    settings.placement = placement.wire().to_string();
                    *changed = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label(t!("cleaning.tools.flux2_klein.dtype_label"));
                let mut dtype = Flux2Dtype::from_wire(&settings.dtype);
                WheelComboBox::from_id_salt("cleaning_flux2_klein_dtype")
                    .selected_text(dtype.wire())
                    .show_ui(ui, |ui| {
                        for candidate in Flux2Dtype::all() {
                            // The dtype names are technical identifiers, not prose.
                            ui.selectable_value(&mut dtype, candidate, candidate.wire());
                        }
                    });
                if dtype.wire() != settings.dtype {
                    settings.dtype = dtype.wire().to_string();
                    *changed = true;
                }
            });
            *changed |= ui
                .checkbox(
                    &mut settings.low_cpu_mem_usage,
                    t!("cleaning.tools.flux2_klein.low_cpu_mem_usage_label"),
                )
                .changed();
            *changed |= ui.checkbox(&mut settings.vae_tiling, "VAE tiling").changed();
            *changed |= ui
                .checkbox(&mut settings.vae_slicing, "VAE slicing")
                .changed();
            *changed |= ui
                .checkbox(
                    &mut settings.unload_transformer_before_vae,
                    t!("cleaning.tools.flux2_klein.unload_before_vae_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.unload_before_vae_hint"))
                .changed();
            *changed |= ui
                .checkbox(
                    &mut settings.unload_text_encoder_after_encode,
                    t!("cleaning.tools.flux2_klein.unload_text_encoder_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.unload_text_encoder_hint"))
                .changed();
            // fp8 only moves the peak while the encoder is still resident: once it is
            // unloaded right after the prompt is encoded, the peak belongs to the
            // transformer and quantizing the encoder buys nothing. The control is FADED
            // to say so and stays live — the user may be about to turn the unloading off
            // again, and a disabled checkbox would hide the setting instead of
            // explaining it (`egui-docs/02-painting.md`: `set_opacity` is a painter
            // property, so nothing here moves).
            let pointless = settings.unload_text_encoder_after_encode;
            let saved_opacity = ui.opacity();
            if pointless {
                ui.set_opacity(saved_opacity * FLUX2_FADED_CONTROL_OPACITY);
            }
            let fp8 = ui
                .checkbox(
                    &mut settings.text_encoder_fp8,
                    t!("cleaning.tools.flux2_klein.text_encoder_fp8_label"),
                )
                .on_hover_text(if pointless {
                    t!("cleaning.tools.flux2_klein.text_encoder_fp8_hint_pointless")
                } else {
                    t!("cleaning.tools.flux2_klein.text_encoder_fp8_hint")
                });
            if pointless {
                ui.set_opacity(saved_opacity);
            }
            *changed |= fp8.changed();
            // Growing the mask contour has no meaning when the mask is the whole region:
            // the backend IGNORES `mask_dilate_px` under `whole_region`, so the slider is
            // faded to say so. It stays live, like the fp8 checkbox above — the user may
            // be about to clear the switch — and `set_opacity` is a painter property, so
            // nothing moves (`egui-docs/02-painting.md`). Feathering is NOT faded: it
            // still softens how the regenerated region joins the rest of the page.
            let dilate_ignored = settings.whole_region;
            let saved_opacity = ui.opacity();
            if dilate_ignored {
                ui.set_opacity(saved_opacity * FLUX2_FADED_CONTROL_OPACITY);
            }
            let dilate = ui.add(
                WheelSlider::new(&mut settings.mask_dilate_px, 0..=FLUX2_DILATE_MAX)
                    .text(t!("cleaning.common.mask_expand_label")),
            );
            let dilate = if dilate_ignored {
                dilate.on_hover_text(t!("cleaning.tools.flux2_klein.mask_expand_hint_ignored"))
            } else {
                dilate
            };
            if dilate_ignored {
                ui.set_opacity(saved_opacity);
            }
            *changed |= dilate.changed();
            *changed |= ui
                .add(
                    WheelSlider::new(&mut settings.mask_feather_px, 0..=FLUX2_FEATHER_MAX)
                        .text(t!("cleaning.tools.flux2_klein.mask_feather_label")),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.mask_feather_hint"))
                .changed();
            *changed |= ui
                .checkbox(
                    &mut settings.color_match,
                    t!("cleaning.tools.flux2_klein.color_match_label"),
                )
                .changed();
            *changed |= ui
                .add(
                    WheelSlider::new(
                        &mut settings.max_sequence_length,
                        FLUX2_MAX_SEQ_MIN..=FLUX2_MAX_SEQ_MAX,
                    )
                    .text("Max tokens"),
                )
                .changed();
        },
    );
}

/// Formats a byte count as GIBIBYTES (2^30 bytes) with one decimal.
///
/// The unit itself lives in the locale template — «ГиБ» / `GiB` / `Gio` — so it can be
/// translated, and every template that consumes this function must name that unit and
/// not a decimal gigabyte.
fn format_gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn lock_progress(progress: &Mutex<Flux2Progress>) -> MutexGuard<'_, Flux2Progress> {
    match progress.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// Claims the shared progress for a run that is about to start and returns ITS
/// generation, which the worker must carry into every later write.
///
/// Called on the GUI thread before the worker is spawned, so two runs started in a row
/// are ordered by construction rather than by whichever thread wins the lock.
fn begin_progress_generation(progress: &Mutex<Flux2Progress>) -> u64 {
    let mut guard = lock_progress(progress);
    // Wrapping, not saturating: a saturated counter would stop being unique and let a
    // stale worker write into a live run again. 2^64 runs is not a reachable session.
    guard.generation = guard.generation.wrapping_add(1);
    guard.active = true;
    guard.phase = "load".to_string();
    guard.step = 0;
    guard.total = 0;
    guard.label = t!("cleaning.tools.flux2_klein.preparing_status").to_string();
    guard.cancel_id = None;
    guard.generation
}

/// Retires the current generation: the bar disappears at once and every later write
/// from the abandoned worker is ignored.
///
/// Returns the IPC id of the abandoned request when it had already reached the wire,
/// so the caller can cancel it backend-side instead of leaving it computing.
fn retire_progress_generation(progress: &Mutex<Flux2Progress>) -> Option<u64> {
    let mut guard = lock_progress(progress);
    guard.generation = guard.generation.wrapping_add(1);
    guard.active = false;
    guard.cancel_id.take()
}

/// Applies `update` to the shared progress only while `generation` still owns it; a
/// write from a retired or superseded run is dropped.
fn update_progress(
    progress: &Mutex<Flux2Progress>,
    generation: u64,
    update: impl FnOnce(&mut Flux2Progress),
) {
    let mut guard = lock_progress(progress);
    if guard.generation != generation {
        return;
    }
    update(&mut guard);
}

/// Asks the backend to stop request `id`, on a worker thread.
///
/// The cancel frame is a socket write behind the client's writer lock, which another
/// thread may be holding for a multi-megabyte request blob — never taken on the GUI
/// thread. A cancel for a finished id is a no-op on the backend.
fn spawn_flux2_cancel(id: u64) {
    thread::spawn(move || {
        let client = match backend_ipc::shared_client() {
            Ok(client) => client,
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[cleaning] FLUX.2 klein cancel could not reach the backend: {err}"
                ));
                return;
            }
        };
        if let Err(err) = client.cancel(id) {
            crate::runtime_log::log_warn(format!(
                "[cleaning] FLUX.2 klein cancel of request {id} failed: {err}"
            ));
        }
    });
}

/// Draws the single progress bar shared by the load and generate phases.
fn draw_flux2_progress_ui(ui: &mut egui::Ui, progress: &Mutex<Flux2Progress>) {
    let (active, phase, step, total, label) = {
        let guard = lock_progress(progress);
        (
            guard.active,
            guard.phase.clone(),
            guard.step,
            guard.total,
            guard.label.clone(),
        )
    };
    if !active {
        return;
    }
    // Cast justification: both are small counters (steps, or module counts during a
    // load), far below the f32 integer-exact range.
    let fraction = if total > 0 {
        (step as f32 / total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let text = if phase == "load" {
        tf!(
            "cleaning.tools.flux2_klein.load_progress_status",
            label = label,
            step = step,
            total = total
        )
    } else if total > 0 {
        tf!("cleaning.common.step_progress_status", step = step, total = total)
    } else {
        label.clone()
    };
    ui.add(egui::ProgressBar::new(fraction).text(text));
    // Nothing else drives repaints while the worker runs, so the bar would freeze.
    ui.ctx().request_repaint();
}

// ---------------------------------------------------------------------------------------
// Worker passes
// ---------------------------------------------------------------------------------------

/// Runs one FLUX.2 klein edit pass and returns the regenerated region plus what the
/// backend reports about how it got there.
///
/// `settings` must already be `normalized()`. `mask` is the L8 edit-permission mask in
/// region coordinates and must be exactly `mask_size[0] * mask_size[1]` bytes matching
/// `image.size`. `generation` is the progress generation claimed by `start_run`: every
/// write into `progress`, including the terminal one that always clears the bar before
/// returning, is dropped once a newer run (or a cancel) has retired it.
///
/// # Errors
/// Returns a user-facing message when the region violates the model's size contract,
/// when the mask is empty or the wrong size, when the backend fails or is unreachable,
/// when the response is missing or contradicts its declared length, or when the
/// returned PNG is not exactly the region size.
fn run_flux2_klein(
    image: &egui::ColorImage,
    mask: &[u8],
    mask_size: [usize; 2],
    settings: &Flux2KleinSettings,
    progress: &Arc<Mutex<Flux2Progress>>,
    generation: u64,
) -> Result<Flux2RunOutcome, String> {
    let outcome = run_flux2_klein_pass(image, mask, mask_size, settings, progress, generation);
    // The bar is cleared on EVERY exit, including the early validation refusals above
    // the IPC call, because `start_run` raised it before the worker even started.
    update_progress(progress, generation, |state| {
        state.active = false;
        state.cancel_id = None;
    });
    outcome
}

/// The body of one run, without the progress bookkeeping [`run_flux2_klein`] wraps it
/// in. Same contract and same errors; split out only so no early return can leave the
/// bar raised.
fn run_flux2_klein_pass(
    image: &egui::ColorImage,
    mask: &[u8],
    mask_size: [usize; 2],
    settings: &Flux2KleinSettings,
    progress: &Arc<Mutex<Flux2Progress>>,
    generation: u64,
) -> Result<Flux2RunOutcome, String> {
    if image.size != mask_size {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    let (width, height) = (image.size[0], image.size[1]);
    if mask.len() != width.saturating_mul(height) {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    // Re-checked on the worker, not only in the button gate: the gate is UI state and
    // a run can be started from a session whose region was re-derived by ratio.
    if let Some(reason) = region_block_reason(image.size) {
        return Err(reason);
    }
    if !mask.iter().any(|value| *value > 0) {
        return Err(t!("cleaning.tools.flux2_klein.empty_mask_error").to_string());
    }

    let image_png = encode_color_image_png_rgba(image)?;
    let mask_png = encode_mask_png_l8(mask, width, height)?;
    let header = json!({
        "image_len": image_png.len(),
        "mask_len": mask_png.len(),
        "params": settings.to_params(),
    });
    let blob = concat_image_mask(&image_png, &mask_png);

    let stream_result = flux2_stream_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN,
        header,
        &blob,
        |id| update_progress(progress, generation, |state| state.cancel_id = Some(id)),
        |phase, step, total, label| {
            update_progress(progress, generation, |state| {
                state.phase = phase;
                state.step = step;
                state.total = total;
                state.label = label;
            });
        },
    );

    let (response_header, out_bytes) = stream_result?;
    if out_bytes.is_empty() {
        return Err(t!("cleaning.inpaint.no_png_result_error").to_string());
    }
    // Declared length is validated with STRICT equality before the bytes are used, so
    // a truncated or padded frame is rejected instead of decoded into garbage. The
    // field is REQUIRED: an answer without it is a protocol violation, not a licence
    // to skip the check.
    let declared = response_header
        .get("image_len")
        .and_then(Value::as_u64)
        .and_then(|declared| usize::try_from(declared).ok())
        .ok_or_else(|| t!("cleaning.inpaint.no_png_result_error").to_string())?;
    if declared != out_bytes.len() {
        return Err(tf!(
            "cleaning.tools.flux2_klein.blob_length_error",
            declared = declared,
            actual = out_bytes.len()
        ));
    }
    let out_rgba = image::load_from_memory(&out_bytes)
        .map_err(|err| tf!("cleaning.inpaint.corrupt_png_error", err = err))?
        .to_rgba8();
    let (out_w, out_h) = (out_rgba.width() as usize, out_rgba.height() as usize);
    if out_w != width || out_h != height {
        return Err(tf!(
            "cleaning.inpaint.unexpected_size_error",
            out_w = out_w,
            out_h = out_h,
            width = width,
            height = height
        ));
    }
    Ok(Flux2RunOutcome {
        image: egui::ColorImage::from_rgba_unmultiplied([out_w, out_h], out_rgba.as_raw()),
        oom_recovered: response_header
            .get("oom_recovered")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        applied: parse_applied_flags(&response_header),
    })
}

/// Reads the `applied` object of a generation response.
///
/// All FIVE memory flags must be present: `None` when the backend reported no `applied`
/// at all (an older build) and also when it reported only some of them, so the tool's
/// own settings are then left exactly as the user set them rather than half-overwritten.
fn parse_applied_flags(header: &Value) -> Option<Flux2AppliedFlags> {
    let applied = header.get("applied")?;
    let flag = |name: &str| applied.get(name).and_then(Value::as_bool);
    Some(Flux2AppliedFlags {
        unload_transformer_before_vae: flag("unload_transformer_before_vae")?,
        vae_tiling: flag("vae_tiling")?,
        vae_slicing: flag("vae_slicing")?,
        unload_text_encoder_after_encode: flag("unload_text_encoder_after_encode")?,
        text_encoder_fp8: flag("text_encoder_fp8")?,
    })
}

/// Streaming call to `method`. Each `progress` frame carries `phase`/`step`/`total`/
/// `label` in the header and no preview blob — the same shape for a generation and for a
/// prompt-cache build, which is why both go through here.
///
/// `on_started` receives the IPC id of the request as soon as it is on the wire; the
/// progress state keeps it so a cancel can stop the work backend-side. This is why the call is
/// built from `begin_call` + `wait_streaming` rather than from the `call_streaming`
/// shorthand, which never exposes the id.
fn flux2_stream_call<S, F>(
    method: &'static str,
    header: Value,
    blob: &[u8],
    on_started: S,
    mut on_progress: F,
) -> Result<(Value, Vec<u8>), String>
where
    S: FnOnce(u64),
    F: FnMut(String, u64, u64, String),
{
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let handle = client
        .begin_call(method, header, blob)
        .map_err(|err| map_flux2_call_error(CallError::Transport(err)))?;
    on_started(handle.id());
    handle
        .wait_streaming(
            |progress_header, _preview_blob| {
                let phase = progress_header
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("generate")
                    .to_string();
                let step = progress_header
                    .get("step")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let total = progress_header
                    .get("total")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let label = progress_header
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                on_progress(phase, step, total, label);
            },
            FLUX2_RUN_TIMEOUT,
        )
        .map_err(map_flux2_call_error)
}

/// Builds the `.status` request header.
///
/// The model paths MUST travel with the query: when `params` is missing the backend
/// answers about the paths of its LAST SUCCESSFUL generation instead, which are empty
/// until one has run. An empty header therefore makes the component panel report
/// "nothing is configured" for exactly the paths the user has just entered — and the
/// panel exists to tell them whether those paths are usable.
fn flux2_status_header(params: &Value) -> Value {
    json!({ "params": params })
}

/// Queries `.status` for the component catalog and the host's memory figures.
///
/// `params` must come from a `normalized()` settings value; empty paths are allowed
/// and are what the backend reports as "not configured".
fn fetch_flux2_status(params: &Value) -> Result<Flux2Status, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_STATUS,
            flux2_status_header(params),
            &[],
            FLUX2_QUERY_TIMEOUT,
        )
        .map_err(map_flux2_call_error)?;
    Ok(parse_flux2_status(&header))
}

/// Parses a `.status` answer. Every field is optional: a backend that reports less
/// than the full catalog degrades to "not present" rather than to an error.
fn parse_flux2_status(header: &Value) -> Flux2Status {
    let components = header.get("components");
    let component = |name: &str| Flux2Component::parse(components.and_then(|c| c.get(name)));
    let memory = header.get("memory");
    let memory_u64 = |name: &str| {
        memory
            .and_then(|m| m.get(name))
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    Flux2Status {
        available: header
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        reason: header
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        text_encoder: component("text_encoder"),
        transformer: component("transformer"),
        vae: component("vae"),
        tokenizer: component("tokenizer"),
        scheduler: component("scheduler"),
        vram_total: memory_u64("vram_total"),
        ram_total: memory_u64("ram_total"),
        loaded: header
            .get("loaded")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        device: header
            .get("device")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // Deliberately NOT defaulted to `false`: a backend that does not know about the
        // prompt cache must read as "unknown", never as "your prompt is not cached".
        prompt_cached: header.get("prompt_cached").and_then(Value::as_bool),
        // Same rule, and it matters more here: `Some(false)` gates the encode buttons and
        // raises a warning, so a backend that never reports the field must not be read as
        // "you have no encoder".
        text_encoder_available: header.get("text_encoder_available").and_then(Value::as_bool),
    }
}

/// Asks the backend to forecast the memory cost of one run at `width` x `height`.
fn fetch_flux2_estimate(
    params: &Value,
    width: usize,
    height: usize,
) -> Result<Flux2Estimate, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_ESTIMATE,
            json!({
                "params": params,
                "region_width": width,
                "region_height": height,
            }),
            &[],
            FLUX2_QUERY_TIMEOUT,
        )
        .map_err(map_flux2_call_error)?;
    Ok(parse_flux2_estimate(&header))
}

/// Parses an `.estimate` answer. The breakdown comes out in key order (see
/// [`Flux2Estimate::breakdown`]), so it must be read by name, never by position.
fn parse_flux2_estimate(header: &Value) -> Flux2Estimate {
    let u64_field = |name: &str| header.get(name).and_then(Value::as_u64).unwrap_or_default();
    let breakdown = header
        .get("breakdown")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.as_u64().unwrap_or_default()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Flux2Estimate {
        vram_bytes: u64_field("vram_bytes"),
        ram_bytes: u64_field("ram_bytes"),
        vram_free: u64_field("vram_free"),
        ram_free: u64_field("ram_free"),
        fits: header.get("fits").and_then(Value::as_bool).unwrap_or(false),
        breakdown,
    }
}

/// Releases the resident FLUX.2 klein pipeline on the backend.
fn unload_flux2_klein() -> Result<(), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_UNLOAD,
            json!({}),
            &[],
            FLUX2_QUERY_TIMEOUT,
        )
        .map_err(map_flux2_call_error)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Prompt cache
// ---------------------------------------------------------------------------------------

/// Builds the request header of a prompt-cache call: the normalized settings under
/// `params`, and the operation's own fields (`name`, `path`, or neither) BESIDE it at the
/// top level, which is where the backend reads them from.
///
/// The settings travel with EVERY one of the six, not only with the build: they carry the
/// text-encoder path, which is what decides the encoder FAMILY the library is split by. A
/// `.list` without them would describe some other family's entries — and the backend
/// refuses the call outright rather than guessing one.
///
/// `overwrite` is never sent: it defaults to `false` backend-side, so a name already taken
/// comes back as an explicit error the user is shown, instead of silently replacing a
/// cache that cost a 16 GB encoder read to build.
///
/// `settings` must already be `normalized()`.
#[must_use]
fn flux2_prompt_cache_header(settings: &Flux2KleinSettings, extra: &[(&str, Value)]) -> Value {
    let mut header = json!({ "params": settings.to_params() });
    // `json!` above always builds an object; the guard keeps this total rather than
    // relying on that from a distance.
    if let Some(map) = header.as_object_mut() {
        for (key, value) in extra {
            map.insert((*key).to_string(), value.clone());
        }
    }
    header
}

/// Encodes the prompt in `params` and leaves the embeddings in the backend's live cache.
///
/// Streaming, because reading the ~16 GB Qwen3 encoder takes ~106 s: the progress frames
/// have the same shape as a generation's and drive the same bar. `generation` is the
/// progress generation claimed on the GUI thread; every write is dropped once a newer run
/// — or a cancel — has retired it, and the bar is cleared on EVERY exit.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable.
fn build_flux2_prompt_cache(
    header: Value,
    progress: &Arc<Mutex<Flux2Progress>>,
    generation: u64,
) -> Result<Flux2PromptCacheOutcome, String> {
    let outcome = flux2_stream_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_BUILD,
        header,
        &[],
        |id| update_progress(progress, generation, |state| state.cancel_id = Some(id)),
        |phase, step, total, label| {
            update_progress(progress, generation, |state| {
                state.phase = phase;
                state.step = step;
                state.total = total;
                state.label = label;
            });
        },
    )
    .map(|(_header, _blob)| Flux2PromptCacheOutcome::Built);
    update_progress(progress, generation, |state| {
        state.active = false;
        state.cancel_id = None;
    });
    outcome
}

/// Lists the library entries of the encoder family `params` identifies.
///
/// # Errors
/// Returns a user-facing message when the backend fails, does not know the method, or is
/// unreachable.
fn list_flux2_prompt_caches(header: Value) -> Result<Flux2PromptCacheList, String> {
    let response = flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LIST,
        header,
    )?;
    Ok(parse_flux2_prompt_cache_list(&response))
}

/// Parses a `.prompt_cache.list` answer.
///
/// Every field is optional: an answer that reports less than the full record degrades to
/// an entry with empty extras rather than to an error, and an entry with no NAME is
/// dropped outright — the name is the only field the other four methods can act on.
///
/// The top-level `family` and `text_encoder_available` are read the same way, and an
/// EMPTY `family` is kept as such: on a machine with no encoder it is the backend's way of
/// saying that no family is active and the entries come from all of them.
fn parse_flux2_prompt_cache_list(header: &Value) -> Flux2PromptCacheList {
    let entries = header
        .get("entries")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name").and_then(Value::as_str)?.trim();
                    if name.is_empty() {
                        return None;
                    }
                    Some(Flux2PromptCacheEntry {
                        name: name.to_string(),
                        // Each entry names its own family, which is the only thing that
                        // keeps a library-wide listing (no encoder installed) unambiguous.
                        family: item
                            .get("family")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        prompt: item
                            .get("prompt")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        // `created_at` is the backend's spelling (an ISO-8601 UTC
                        // string); `created` is accepted as well so a shorter spelling
                        // does not silently read as "no date".
                        created: format_prompt_cache_created(
                            item.get("created_at").or_else(|| item.get("created")),
                        ),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Flux2PromptCacheList {
        // An EMPTY family is a fact, not a gap: it is how the backend reports that no
        // encoder is installed and the listing therefore spans every family.
        family: header
            .get("family")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        text_encoder_available: header.get("text_encoder_available").and_then(Value::as_bool),
        entries,
    }
}

/// Stores the live cache of `params.prompt` in the library under `params.name`.
///
/// # Errors
/// Returns the backend's own message — which is what reports a name already taken — or a
/// transport message when it is unreachable.
fn save_flux2_prompt_cache(header: Value) -> Result<(), String> {
    flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_SAVE,
        header,
    )
    .map(|_response| ())
}

/// Loads library entry `params.name` into the live cache.
///
/// # Errors
/// Returns the backend's own message — which is what reports an entry of a different
/// encoder family — or a transport message when it is unreachable.
fn load_flux2_prompt_cache(header: Value) -> Result<Flux2PromptCacheLoad, String> {
    let response = flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_LOAD,
        header,
    )?;
    Ok(parse_flux2_prompt_cache_load(&response))
}

/// Reads a `.prompt_cache.load` answer: the prompt the entry was built from, trimmed, and
/// whether the encoder's fingerprint was actually compared.
///
/// An empty prompt means the answer carried none, which the caller refuses rather than
/// writing into the field — a blank prompt would block the run gate.
///
/// `encoder_verified` is `None` when the backend did not report it (an older build, which
/// only ever verified). It is NOT read as `false`: the notice it drives says the file's
/// metadata was taken on trust, and inventing that would be a false alarm.
fn parse_flux2_prompt_cache_load(header: &Value) -> Flux2PromptCacheLoad {
    Flux2PromptCacheLoad {
        prompt: header
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        encoder_verified: header.get("encoder_verified").and_then(Value::as_bool),
    }
}

/// Writes library entry `params.name` to the file `params.path`.
///
/// # Errors
/// Returns the backend's own message, or a transport message when it is unreachable.
fn export_flux2_prompt_cache(header: Value) -> Result<(), String> {
    flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_EXPORT,
        header,
    )
    .map(|_response| ())
}

/// Takes the file `params.path` into the library.
///
/// `current_family` is the family the tool currently lists, used only to decide whether
/// the import landed outside it when the backend does not say so itself.
///
/// # Errors
/// Returns the backend's own message, or a transport message when it is unreachable.
fn import_flux2_prompt_cache(
    header: Value,
    current_family: &str,
) -> Result<Flux2PromptCacheOutcome, String> {
    let response = flux2_prompt_cache_call(
        backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_PROMPT_CACHE_IMPORT,
        header,
    )?;
    Ok(parse_flux2_prompt_cache_import(&response, current_family))
}

/// Reads a `.prompt_cache.import` answer.
///
/// `family_matches` is decided in this order: the backend's own `family_matches` flag if
/// it reported one, then the `foreign` spelling of the same fact, then a comparison of the
/// reported `family` against `current_family` when both are known. When none of the three
/// applies the answer is `None` — "not known" — and no warning is shown, because a guess
/// here would either hide a lost entry or accuse the backend of losing one it did not.
fn parse_flux2_prompt_cache_import(header: &Value, current_family: &str) -> Flux2PromptCacheOutcome {
    let family = header
        .get("family")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let family_matches = header
        .get("family_matches")
        .and_then(Value::as_bool)
        .or_else(|| header.get("foreign").and_then(Value::as_bool).map(|foreign| !foreign))
        .or_else(|| {
            (!family.is_empty() && !current_family.is_empty()).then(|| family == current_family)
        });
    Flux2PromptCacheOutcome::Imported {
        name: header
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        family_matches,
    }
}

/// One-shot prompt-cache call, returning the response header.
///
/// # Errors
/// Returns the backend's own message for a refused request — a name already taken, an
/// entry of another encoder family, or "unknown method", which is what a backend without
/// the prompt-cache handlers answers — or the offline message when it cannot be reached at
/// all. None of them is a panic and none is swallowed.
fn flux2_prompt_cache_call(method: &'static str, header: Value) -> Result<Value, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (response, _blob) = client
        .call(method, header, &[], FLUX2_QUERY_TIMEOUT)
        .map_err(map_flux2_call_error)?;
    Ok(response)
}

/// Translates one prompt into English through the translation tab's own dispatcher.
///
/// BLOCKING: runs only on a worker thread. `source_lang` is an MT language code
/// (`"auto"` is accepted); the target is always `"en"`, which every backend maps to
/// its own wire spelling itself.
///
/// # Errors
/// Returns the provider's message when the request fails, and a dedicated message when
/// the provider answered with no usable text at all.
fn translate_prompt_to_english(
    service: MtService,
    source_lang: &str,
    text: String,
) -> Result<String, String> {
    let results = translate_texts_via_translator(service, source_lang, "en", vec![text])?;
    match results.into_iter().next() {
        Some(Ok(translated)) if !translated.trim().is_empty() => Ok(translated),
        Some(Ok(_)) | None => {
            Err(t!("cleaning.tools.flux2_klein.translate_empty_result_error").to_string())
        }
        Some(Err(err)) => Err(err),
    }
}

fn map_flux2_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Encoding and settings I/O
// ---------------------------------------------------------------------------------------

/// Concatenates the region PNG and the mask PNG into one request blob. The receiver
/// splits it by the `image_len` / `mask_len` header fields.
fn concat_image_mask(image_png: &[u8], mask_png: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(image_png.len() + mask_png.len());
    blob.extend_from_slice(image_png);
    blob.extend_from_slice(mask_png);
    blob
}

fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let (width, height) = (image.size[0], image.size[1]);
    let width_u32 = u32::try_from(width)
        .map_err(|_| t!("cleaning.png.image_width_too_large_error").to_string())?;
    let height_u32 = u32::try_from(height)
        .map_err(|_| t!("cleaning.png.image_height_too_large_error").to_string())?;
    let mut raw = Vec::<u8>::with_capacity(width.saturating_mul(height).saturating_mul(4));
    for px in &image.pixels {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        raw.extend_from_slice(&[r, g, b, a]);
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, width_u32, height_u32, ColorType::Rgba8.into())
        .map_err(|err| tf!("cleaning.png.encode_image_error", err = err))?;
    Ok(out)
}

/// Encodes the L8 edit-permission mask. `mask` must be exactly `width * height` bytes.
fn encode_mask_png_l8(mask: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let width_u32 = u32::try_from(width)
        .map_err(|_| t!("cleaning.png.mask_width_too_large_error").to_string())?;
    let height_u32 = u32::try_from(height)
        .map_err(|_| t!("cleaning.png.mask_height_too_large_error").to_string())?;
    if mask.len() != width.saturating_mul(height) {
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(mask, width_u32, height_u32, ColorType::L8.into())
        .map_err(|err| tf!("cleaning.png.encode_mask_error", err = err))?;
    Ok(out)
}

/// Reads the settings file, falling back to defaults on any error (a missing file is
/// the normal first-run case, and a corrupt one must not block the tool).
fn load_flux2_settings() -> Flux2KleinSettings {
    let path = config::flux2_klein_settings_path();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return Flux2KleinSettings::default(),
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Flux2KleinSettings::default();
    };
    settings_from_json(&value)
}

/// Deserializes a settings document and migrates the placement-dependent memory flags.
///
/// Split out of [`load_flux2_settings`] so the migration itself is testable without a
/// settings file on disk: the file I/O has no contract worth asserting, this does.
///
/// A document that is not a settings object at all deserializes to `Default`, which is
/// the same "a corrupt file must not block the tool" rule the caller applies.
fn settings_from_json(value: &Value) -> Flux2KleinSettings {
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
    // which is exactly what serde's `#[serde(default)]` already produces. The same holds
    // for `whole_region`: it depends on no sibling field, so a file written before it
    // existed loads with the painted-mask flow the user had, and a file that carries it
    // keeps their choice.
    settings
}

/// Writes the settings file.
///
/// # Errors
/// Returns a user-facing message when the data directory cannot be created, when the
/// settings cannot be serialized, or when the write fails.
/// Whether a settings save must be started right now.
///
/// `dirty` is raised by every parameter change and by the OOM recovery that rewrites the
/// economy settings itself. `settings_loaded` gates it because the initial load runs on its own
/// worker: saving the in-memory DEFAULTS before that load lands would overwrite the user's file
/// with defaults, which is a silent data loss rather than a visible failure.
/// `save_in_flight` keeps at most one writer on the file at a time.
#[must_use]
fn settings_save_due(dirty: bool, settings_loaded: bool, save_in_flight: bool) -> bool {
    dirty && settings_loaded && !save_in_flight
}

fn save_flux2_settings(settings: &Flux2KleinSettings) -> Result<(), String> {
    let path = config::flux2_klein_settings_path();
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
    use crate::tabs::cleaning::tools::region_edit_v2::geometry::check_size;

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
        assert_eq!(settings.max_sequence_length, 512);
        // The default placement is `full_gpu`, where the transformer stays put...
        assert!(!settings.unload_transformer_before_vae);
        // ...and the encoder is kept too: loaded LAST, it occupies host memory the
        // pipeline has already vacated, and keeping it turns a new prompt into ~6 s.
        assert!(!settings.unload_text_encoder_after_encode);
        // Quantizing the text encoder is never defaulted on.
        assert!(!settings.text_encoder_fp8);
    }

    #[test]
    fn presets_pin_the_unload_flags_and_never_the_fp8_one() {
        let mut settings = Flux2KleinSettings::default();
        MemoryPreset::MaxSpeed.apply(&mut settings);
        assert!(!settings.unload_transformer_before_vae);
        // No preset releases the encoder: measured, keeping it costs host memory the
        // pipeline no longer needs and saves ~110 s on every new prompt.
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
    fn applied_flags_are_written_back_and_can_unpin_the_preset() {
        let mut settings = Flux2KleinSettings::default();
        MemoryPreset::MaxSpeed.apply(&mut settings);
        assert_eq!(MemoryPreset::detect(&settings), MemoryPreset::MaxSpeed);
        let recovered = Flux2AppliedFlags {
            unload_transformer_before_vae: true,
            vae_tiling: true,
            vae_slicing: false,
            unload_text_encoder_after_encode: true,
            text_encoder_fp8: true,
        };
        assert!(apply_backend_flags(&mut settings, recovered));
        assert!(settings.unload_transformer_before_vae);
        assert!(settings.vae_tiling);
        assert!(settings.unload_text_encoder_after_encode);
        assert!(settings.text_encoder_fp8);
        // Applying the same values twice owes no second save.
        assert!(!apply_backend_flags(&mut settings, recovered));
        // `full_gpu` plus the recovered flags matches no preset any more.
        assert_eq!(MemoryPreset::detect(&settings), MemoryPreset::Custom);
    }

    #[test]
    fn applied_flags_parse_only_when_complete() {
        let full = json!({
            "image_len": 4,
            "oom_recovered": true,
            "applied": {
                "unload_transformer_before_vae": true,
                "vae_tiling": true,
                "vae_slicing": false,
                "unload_text_encoder_after_encode": true,
                "text_encoder_fp8": false
            }
        });
        let parsed = parse_applied_flags(&full).expect("complete applied object");
        assert!(parsed.unload_transformer_before_vae);
        assert!(parsed.vae_tiling);
        assert!(!parsed.vae_slicing);
        assert!(parsed.unload_text_encoder_after_encode);
        assert!(!parsed.text_encoder_fp8);
        // A backend that reports no `applied` (or a partial one) must leave the user's
        // own settings alone rather than half-overwrite them.
        assert!(parse_applied_flags(&json!({ "image_len": 4 })).is_none());
        assert!(
            parse_applied_flags(&json!({ "applied": { "vae_tiling": true } })).is_none(),
            "a partial applied object is not applied at all"
        );
        // The three OLD flags alone are a partial object now that there are five: an
        // answer that reports nothing about the text encoder must not be half-applied.
        assert!(
            parse_applied_flags(&json!({
                "applied": {
                    "unload_transformer_before_vae": true,
                    "vae_tiling": true,
                    "vae_slicing": false
                }
            }))
            .is_none(),
            "an `applied` object missing the text-encoder flags is not applied at all"
        );
    }

    #[test]
    fn estimate_tooltip_leads_with_the_phase_peaks() {
        let estimate = Flux2Estimate {
            breakdown: vec![
                ("transformer".to_string(), 8_000_000_000),
                (FLUX2_BREAKDOWN_PEAK_DECODE.to_string(), 6_000_000_000),
                (FLUX2_BREAKDOWN_PEAK_DENOISE.to_string(), 9_000_000_000),
                (FLUX2_BREAKDOWN_PEAK_ENCODE.to_string(), 17_000_000_000),
            ],
            ..Flux2Estimate::default()
        };
        // The three peaks lead, and none of them is repeated among the remaining
        // entries. Asserted on the split rather than on the rendered text: every line
        // comes from a locale template, and a unit test runs without a loaded catalog.
        let peaks = split_estimate_peaks(&estimate);
        assert_eq!(peaks.encode, Some(17_000_000_000));
        assert_eq!(peaks.denoise, Some(9_000_000_000));
        assert_eq!(peaks.decode, Some(6_000_000_000));
        assert_eq!(peaks.others, vec![("transformer", 8_000_000_000)]);

        let tooltip = estimate_tooltip(&estimate);
        assert_eq!(tooltip.lines().count(), 4);
        // A backend that reported nothing still gets a line, not an empty tooltip.
        assert!(!estimate_tooltip(&Flux2Estimate::default()).is_empty());
        assert_eq!(
            split_estimate_peaks(&Flux2Estimate::default()),
            Flux2EstimatePeaks::default()
        );
    }

    #[test]
    fn a_breakdown_without_the_encode_peak_still_renders() {
        // The prompt-encoding phase is reported by a NEWER backend than the one that
        // introduced the other two peaks; an answer without it must lose exactly that
        // line and keep everything else.
        let estimate = Flux2Estimate {
            breakdown: vec![
                (FLUX2_BREAKDOWN_PEAK_DECODE.to_string(), 6_000_000_000),
                (FLUX2_BREAKDOWN_PEAK_DENOISE.to_string(), 9_000_000_000),
            ],
            ..Flux2Estimate::default()
        };
        let peaks = split_estimate_peaks(&estimate);
        assert!(peaks.encode.is_none());
        assert_eq!(peaks.denoise, Some(9_000_000_000));
        assert_eq!(estimate_tooltip(&estimate).lines().count(), 2);
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
            max_sequence_length: 1,
            brush_radius: 0,
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
        assert_eq!(norm.max_sequence_length, FLUX2_MAX_SEQ_MIN);
        assert_eq!(norm.brush_radius, FLUX2_BRUSH_MIN);
        assert_eq!(norm.placement, "full_gpu");
        assert_eq!(norm.dtype, "bfloat16");
        assert_eq!(norm.mt_service, "google");
        assert_eq!(norm.source_lang, "auto");
        assert_eq!(norm.text_encoder_path, "/models/qwen3");
        assert!(norm.to_params()["unload_transformer_before_vae"].is_boolean());
        assert!(norm.to_params()["unload_text_encoder_after_encode"].is_boolean());
        assert!(norm.to_params()["text_encoder_fp8"].is_boolean());
    }

    #[test]
    fn params_omit_the_seed_unless_pinned() {
        let settings = Flux2KleinSettings::default().normalized();
        assert_eq!(settings.to_params()["seed"], Value::Null);
        let pinned = Flux2KleinSettings {
            use_seed: true,
            seed: 42,
            ..Flux2KleinSettings::default()
        }
        .normalized();
        assert_eq!(pinned.to_params()["seed"], json!(42));
        // The distilled checkpoint has no negative prompt and must never grow a field
        // for one.
        assert!(pinned.to_params().get("negative_prompt").is_none());
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

    #[test]
    fn region_block_reason_enforces_every_limit() {
        assert!(region_block_reason([512, 512]).is_none());
        assert!(region_block_reason([0, 512]).is_some(), "empty region");
        assert!(region_block_reason([510, 512]).is_some(), "not a multiple of 16");
        assert!(region_block_reason([112, 512]).is_some(), "below the min side");
        assert!(region_block_reason([1024, 1040]).is_some(), "over 1 MP");
        assert!(region_block_reason([128, 1040]).is_some(), "steeper than 8:1");
        assert!(region_block_reason([128, 1024]).is_none(), "exactly 8:1 is allowed");
        // The frame validates a rectangle against `constraints()`, the worker against
        // `region_block_reason`. The two must accept exactly the same regions, or the
        // frame would offer a size the run then refuses.
        let constraints = Flux2KleinEngine::frame_constraints();
        for size in [
            [512usize, 512],
            [510, 512],
            [112, 512],
            [1024, 1040],
            [128, 1040],
            [128, 1024],
        ] {
            assert_eq!(
                check_size(size[0], size[1], &constraints).is_some(),
                region_block_reason(size).is_some(),
                "{size:?}"
            );
        }
    }

    #[test]
    fn blob_concat_orders_image_then_mask() {
        let image_png = b"IMAGE".to_vec();
        let mask_png = b"MASK".to_vec();
        let blob = concat_image_mask(&image_png, &mask_png);
        assert_eq!(blob.len(), image_png.len() + mask_png.len());
        assert_eq!(&blob[..image_png.len()], image_png.as_slice());
        assert_eq!(&blob[image_png.len()..], mask_png.as_slice());
    }

    #[test]
    fn mask_encodes_one_byte_per_pixel() {
        let mask = vec![255u8; 32 * 16];
        let png = encode_mask_png_l8(&mask, 32, 16).expect("encode mask");
        let decoded = image::load_from_memory(&png).expect("decode mask").to_luma8();
        assert_eq!(decoded.dimensions(), (32, 16));
        assert!(encode_mask_png_l8(&mask, 32, 15).is_err(), "length mismatch must fail");
    }

    /// The engine owns no brush any more: the host's `MaskStack` paints, and the engine's
    /// only remaining duty is to put the bytes it is handed on the wire unchanged.
    #[test]
    fn the_host_mask_reaches_the_wire_verbatim() {
        // A band across a 64x64 region — the shape a stroke leaves.
        let mut painted = vec![0u8; 64 * 64];
        for x in 6..40 {
            painted[10 * 64 + x] = 255;
        }
        let sent = mask_for_run(&painted, false);
        assert_eq!(sent, painted, "the painted layer travels byte for byte");
        // The bytes that actually reach the wire are checked, not the intention.
        let png = encode_mask_png_l8(&sent, 64, 64).expect("encode the painted mask");
        let decoded = image::load_from_memory(&png).expect("decode").to_luma8();
        assert_eq!(decoded.dimensions(), (64, 64));
        assert_eq!(decoded.get_pixel(0, 0).0[0], 0, "outside the band nothing may change");
        assert_eq!(decoded.get_pixel(10, 10).0[0], 255, "inside the band the model may paint");
    }

    /// The undo entries describe the rectangle they were captured from, so a frame that
    /// moves or resizes invalidates them. `same_region` is what decides that, because
    /// `OverlayRectPx` derives no `PartialEq` to lean on.
    #[test]
    fn a_moved_frame_is_a_new_region_and_drops_the_undo_history() {
        let rect = |x, y, w, h| OverlayRectPx { x, y, w, h };
        assert!(same_region(None, None));
        assert!(same_region(Some(&rect(1, 2, 64, 64)), Some(&rect(1, 2, 64, 64))));
        assert!(
            !same_region(Some(&rect(1, 2, 64, 64)), Some(&rect(1, 3, 64, 64))),
            "a move is a new region even at the same size"
        );
        assert!(
            !same_region(Some(&rect(1, 2, 64, 64)), Some(&rect(1, 2, 64, 80))),
            "a resize is a new region"
        );
        assert!(!same_region(Some(&rect(1, 2, 64, 64)), None));

        let mut session = Flux2SessionState::default();
        session.push_undo(egui::ColorImage::filled([2, 2], Color32::WHITE));
        session.clear();
        assert!(session.undo_stack.is_empty());
    }

    #[test]
    fn status_parses_both_present_spellings() {
        let header = json!({
            "available": true,
            "reason": "",
            "components": {
                "text_encoder": { "path": "/a", "exists": true, "size_bytes": 1024 },
                "tokenizer": { "found": true, "path": "/b" }
            },
            "memory": { "vram_total": 16, "ram_total": 32 },
            "loaded": true,
            "device": "cuda:0"
        });
        let status = parse_flux2_status(&header);
        assert!(status.available);
        assert!(status.text_encoder.present);
        assert_eq!(status.text_encoder.size_bytes, 1024);
        assert!(status.tokenizer.present);
        assert!(!status.vae.present, "a missing component reads as absent");
        assert_eq!(status.vram_total, 16);
        assert_eq!(status.device, "cuda:0");
    }

    #[test]
    fn estimate_parses_the_breakdown() {
        let header = json!({
            "vram_bytes": 10_000_000_000u64,
            "ram_bytes": 2_000_000_000u64,
            "vram_free": 15_000_000_000u64,
            "ram_free": 20_000_000_000u64,
            "fits": true,
            "breakdown": { "transformer": 8_000_000_000u64 }
        });
        let estimate = parse_flux2_estimate(&header);
        assert!(estimate.fits);
        assert_eq!(estimate.vram_bytes, 10_000_000_000);
        assert_eq!(estimate.breakdown.len(), 1);
        assert_eq!(estimate.breakdown[0].0, "transformer");
        // An empty answer must degrade, not panic.
        let empty = parse_flux2_estimate(&json!({}));
        assert!(!empty.fits);
        assert!(empty.breakdown.is_empty());
    }

    #[test]
    fn status_request_carries_the_model_paths() {
        let settings = Flux2KleinSettings {
            text_encoder_path: "  /models/qwen3  ".to_string(),
            transformer_path: "/models/flux2.safetensors".to_string(),
            vae_path: "/models/vae".to_string(),
            ..Flux2KleinSettings::default()
        }
        .normalized();
        let header = flux2_status_header(&settings.to_params());
        let params = header
            .get("params")
            .expect("`.status` without `params` makes the backend answer about the paths of the last successful run, i.e. about nothing");
        assert_eq!(params["text_encoder_path"], json!("/models/qwen3"));
        assert_eq!(
            params["transformer_path"],
            json!("/models/flux2.safetensors")
        );
        assert_eq!(params["vae_path"], json!("/models/vae"));
        // Paths the user has not filled in yet still travel, as empty strings: that is
        // exactly the question the component panel asks.
        let empty = flux2_status_header(&Flux2KleinSettings::default().normalized().to_params());
        assert_eq!(empty["params"]["text_encoder_path"], json!(""));
    }

    #[test]
    fn a_stale_run_cannot_touch_the_progress_of_the_next_one() {
        let progress = Mutex::new(Flux2Progress::default());
        let first = begin_progress_generation(&progress);
        update_progress(&progress, first, |state| {
            state.cancel_id = Some(7);
            state.step = 3;
        });
        // Cancel: the id is handed out so the backend can be stopped, and the bar goes.
        assert_eq!(retire_progress_generation(&progress), Some(7));
        assert!(!lock_progress(&progress).active);

        let second = begin_progress_generation(&progress);
        assert_ne!(first, second, "each run gets its own generation");
        // The abandoned worker keeps reporting and eventually finishes. None of that
        // may reach the run that replaced it — least of all its `active = false`.
        update_progress(&progress, first, |state| {
            state.step = 99;
            state.active = false;
        });
        let guard = lock_progress(&progress);
        assert!(guard.active, "a stale worker must not erase the live bar");
        assert_eq!(guard.step, 0, "a stale worker must not move the live bar");
    }

    /// The debounced settings saver must be ARMED by a settings change and by nothing else.
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

    /// The engine keeps a change pending rather than dropping it while a save cannot run yet,
    /// so nothing is lost between the parameter edit and the initial load landing.
    #[test]
    fn a_change_made_before_the_initial_load_is_kept_pending() {
        let mut engine = Flux2KleinEngine { settings_loaded: false, dirty: true, ..Default::default() };
        engine.poll_and_maybe_save();
        assert!(engine.dirty, "the change must survive until it can be written");
        assert!(engine.save_rx.is_none(), "and no writer may have been started");
    }

    #[test]
    fn the_undo_stack_is_bounded() {
        let mut session = Flux2SessionState::default();
        for index in 0..(FLUX2_UNDO_LIMIT + 3) {
            // A distinguishable one-pixel entry: the width encodes the push order.
            session.push_undo(egui::ColorImage::filled([index + 1, 1], Color32::WHITE));
        }
        assert_eq!(session.undo_stack.len(), FLUX2_UNDO_LIMIT);
        // The oldest entries are the ones dropped, so the most recent run is always
        // the one «Вернуть» restores.
        assert_eq!(
            session.undo_stack[FLUX2_UNDO_LIMIT - 1].size,
            [FLUX2_UNDO_LIMIT + 3, 1]
        );
        assert_eq!(session.undo_stack[0].size, [4, 1]);
    }

    /// A frame rectangle for the `set_region` rules below.
    fn region_rect(x: usize, y: usize, w: usize, h: usize) -> Option<OverlayRectPx> {
        Some(OverlayRectPx { x, y, w, h })
    }

    /// An engine whose forecast is disarmed and which already knows a 128x128 frame at the
    /// origin, so each rule below starts from a settled, quiet state.
    fn engine_with_settled_region() -> Flux2KleinEngine {
        let mut engine = Flux2KleinEngine::default();
        engine.set_region(region_rect(0, 0, 128, 128), true);
        engine.estimate_wanted = false;
        engine.region_resize_pending = false;
        engine
    }

    /// The host pushes the rectangle EVERY frame, and the frame's keep-in-view clamp moves it
    /// whenever the canvas is scrolled — so a position change must cost nothing. It used to
    /// arm one `.estimate` round trip per scrolled frame.
    #[test]
    fn a_position_only_change_does_not_arm_the_forecast() {
        let mut engine = engine_with_settled_region();
        engine.set_region(region_rect(64, 32, 128, 128), true);
        assert!(!engine.estimate_wanted, "the forecast depends on the size alone");
        assert!(
            engine.region.is_some_and(|rect| rect.x == 64 && rect.y == 32),
            "the rectangle itself must still follow the frame, so the status line stays live"
        );
    }

    /// A resize drag publishes a new size on every rendered frame; querying the backend on
    /// each of them would be one IPC round trip per frame.
    #[test]
    fn a_size_change_while_the_geometry_is_unsettled_does_not_arm_the_forecast() {
        let mut engine = engine_with_settled_region();
        engine.set_region(region_rect(0, 0, 192, 128), false);
        assert!(!engine.estimate_wanted, "nothing may be queried mid-gesture");
        assert!(
            engine.region.is_some_and(|rect| rect.w == 192),
            "the dragged size must still be displayed"
        );
    }

    /// The rule the user asked for: one forecast per finished resize.
    #[test]
    fn a_settled_size_change_arms_the_forecast_exactly_once() {
        let mut engine = engine_with_settled_region();
        engine.set_region(region_rect(0, 0, 192, 128), true);
        assert!(engine.estimate_wanted, "a settled resize must re-arm the forecast");
        engine.estimate_wanted = false;
        // The host keeps pushing the same rectangle every frame afterwards.
        engine.set_region(region_rect(0, 0, 192, 128), true);
        assert!(!engine.estimate_wanted, "the arming must not repeat on the next frames");
    }

    /// The sizes a drag passes through are one change, not many: the pending flag survives
    /// the release frame, on which the rectangle usually no longer moves at all.
    #[test]
    fn the_sizes_of_one_drag_arm_the_forecast_once_on_release() {
        let mut engine = engine_with_settled_region();
        engine.set_region(region_rect(0, 0, 160, 128), false);
        engine.set_region(region_rect(0, 0, 192, 128), false);
        assert!(!engine.estimate_wanted, "still mid-drag");
        // The release frame carries the same rectangle as the last dragged one.
        engine.set_region(region_rect(0, 0, 192, 128), true);
        assert!(engine.estimate_wanted, "the finished resize must arm the forecast");
        engine.estimate_wanted = false;
        engine.set_region(region_rect(0, 0, 192, 128), true);
        assert!(!engine.estimate_wanted, "and exactly once");
    }

    /// The setter runs every frame with an unchanged rectangle for as long as the user does
    /// nothing at all.
    #[test]
    fn an_unchanged_rectangle_arms_nothing() {
        let mut engine = engine_with_settled_region();
        for _ in 0..3 {
            engine.set_region(region_rect(0, 0, 128, 128), true);
        }
        assert!(!engine.estimate_wanted, "an idle frame must be silent");
    }

    /// Regression guard for the fix above: gating the GEOMETRY must not disarm the forecast
    /// as a whole. Everything that is not the rectangle still arms it — here the settings
    /// load, which carries the memory preset, the placement and the model paths.
    #[test]
    fn a_settings_change_still_arms_the_forecast() {
        let (tx, rx) = mpsc::channel();
        assert!(tx.send(runnable_settings()).is_ok(), "the test channel must accept the settings");
        let mut engine = Flux2KleinEngine {
            settings_rx: Some(rx),
            estimate_wanted: false,
            ..Flux2KleinEngine::default()
        };
        engine.poll_settings_load();
        assert!(engine.settings_loaded, "the load must have landed");
        assert!(engine.estimate_wanted, "loaded settings describe another forecast");
    }

    /// The host promises that the region and every mask buffer are exactly `rect_px`.
    /// The engine CHECKS that promise: a mismatch would otherwise be encoded and refused
    /// by the backend with a message about the protocol instead of about the region.
    #[test]
    fn a_run_request_that_does_not_match_the_frame_is_refused() {
        let mut engine = Flux2KleinEngine {
            settings: runnable_settings(),
            ..Flux2KleinEngine::default()
        };
        let rect = OverlayRectPx {
            x: 0,
            y: 0,
            w: 128,
            h: 128,
        };
        let region = || egui::ColorImage::filled([128, 128], Color32::WHITE);
        let mask = || vec![255u8; 128 * 128];
        let request = |rect_px, region, masks| EngineRunRequest {
            page_idx: 3,
            rect_px,
            region,
            masks,
        };

        assert!(
            engine
                .start(request(
                    rect,
                    egui::ColorImage::filled([64, 128], Color32::WHITE),
                    vec![mask()]
                ))
                .is_err(),
            "a region of another size"
        );
        assert!(
            engine.start(request(rect, region(), Vec::new())).is_err(),
            "no mask layer at all"
        );
        assert!(
            engine
                .start(request(rect, region(), vec![mask(), mask()]))
                .is_err(),
            "more layers than the one this engine declares"
        );
        assert!(
            engine
                .start(request(rect, region(), vec![vec![255u8; 128 * 64]]))
                .is_err(),
            "a mask of the wrong length"
        );
        // A rectangle that breaks a model constraint is refused too, even when the region
        // and the mask agree with it: `region_block_reason` is the wire's last guard.
        let steep = OverlayRectPx {
            x: 0,
            y: 0,
            w: 120,
            h: 128,
        };
        assert!(
            engine
                .start(request(
                    steep,
                    egui::ColorImage::filled([120, 128], Color32::WHITE),
                    vec![vec![255u8; 120 * 128]]
                ))
                .is_err(),
            "120 is not a multiple of 16"
        );
    }

    /// Settings that pass every gate except the one under test, so a block reason a
    /// test observes can only be the one it is asking about.
    fn runnable_settings() -> Flux2KleinSettings {
        Flux2KleinSettings {
            text_encoder_path: "/models/qwen3".to_string(),
            transformer_path: "/models/flux2.safetensors".to_string(),
            vae_path: "/models/vae".to_string(),
            prompt: "a clean background".to_string(),
            ..Flux2KleinSettings::default()
        }
    }

    #[test]
    fn whole_region_is_off_by_default_and_survives_the_wire() {
        let settings = Flux2KleinSettings::default();
        assert!(
            !settings.whole_region,
            "the painted-mask flow is the default one"
        );
        let params = Flux2KleinSettings {
            whole_region: true,
            ..runnable_settings()
        }
        .normalized()
        .to_params();
        assert_eq!(params["whole_region"], json!(true));
        // The flag never replaces the mask parameters: feathering still reaches the
        // backend and still means what it means.
        assert_eq!(params["mask_feather_px"], json!(12));
    }

    #[test]
    fn a_settings_file_without_whole_region_loads_it_off() {
        // A document written before the field existed: no `whole_region` key at all.
        let old = json!({
            "text_encoder_path": "/a",
            "transformer_path": "/b",
            "vae_path": "/c",
            "placement": "sequential_cpu_offload",
            "mask_dilate_px": 8
        });
        let migrated = settings_from_json(&old);
        assert!(
            !migrated.whole_region,
            "an old file must keep the painted-mask flow"
        );
        // The neighbouring migration still works and is not disturbed by the new field.
        assert!(migrated.unload_transformer_before_vae);
        assert_eq!(migrated.mask_dilate_px, 8);

        // A file that carries the field keeps whatever the user chose.
        let carried = settings_from_json(&json!({ "whole_region": true }));
        assert!(carried.whole_region);
    }

    #[test]
    fn whole_region_belongs_to_no_memory_preset() {
        // Applying a preset must not touch the mode in either direction: it is how the
        // tool works, not a memory profile.
        for preset in MemoryPreset::selectable() {
            let mut on = Flux2KleinSettings {
                whole_region: true,
                ..Flux2KleinSettings::default()
            };
            preset.apply(&mut on);
            assert!(on.whole_region, "{preset:?} must not clear the mode");

            let mut off = Flux2KleinSettings::default();
            preset.apply(&mut off);
            assert!(!off.whole_region, "{preset:?} must not set the mode");
        }
        // ...and it must not move the picker either: `detect` compares the seven fields
        // a preset owns, and this is not one of them.
        let mut settings = Flux2KleinSettings::default();
        MemoryPreset::Balanced.apply(&mut settings);
        let before = MemoryPreset::detect(&settings);
        settings.whole_region = true;
        assert_eq!(
            MemoryPreset::detect(&settings),
            before,
            "the mode must not push the picker to «Пользовательский»"
        );
    }

    /// The empty-mask rule LEFT the engine's gate in the port: the host's frame enforces
    /// it and asks the engine only whether an empty mask is meaningful at all. Everything
    /// else the gate used to refuse is refused exactly as before.
    #[test]
    fn an_empty_mask_blocks_a_run_only_while_the_mode_is_off() {
        let mut engine = Flux2KleinEngine {
            settings: runnable_settings(),
            ..Flux2KleinEngine::default()
        };
        assert!(
            !engine.allows_empty_mask(),
            "nothing painted and no whole-region mode: the host must refuse the run"
        );
        engine.settings.whole_region = true;
        assert!(
            engine.allows_empty_mask(),
            "whole-region mode needs no painted mask"
        );
        assert!(
            engine.run_block_reason().is_none(),
            "with paths and a prompt the engine's own half of the gate is clear"
        );

        // Every other gate still applies in the new mode.
        engine.settings.prompt = "   ".to_string();
        assert!(engine.run_block_reason().is_some(), "a blank prompt still blocks");
        engine.settings = Flux2KleinSettings {
            whole_region: true,
            vae_path: String::new(),
            ..runnable_settings()
        };
        assert!(engine.run_block_reason().is_some(), "a missing path still blocks");
    }

    #[test]
    fn a_cached_prompt_lets_a_run_start_without_a_text_encoder() {
        let no_encoder = Flux2KleinSettings {
            text_encoder_path: String::new(),
            ..runnable_settings()
        };
        // The whole point of the prompt-cache library: the denoise and the VAE decode
        // never look at the encoder, so a ready embedding is enough to run.
        assert!(
            flux2_run_block_reason(&no_encoder, Some(true)).is_none(),
            "a cached prompt must waive the encoder path"
        );
        // Without a cache the encoder is required again, and the message names all three
        // paths because all three are genuinely needed then.
        for state in [None, Some(false)] {
            let reason = flux2_run_block_reason(&no_encoder, state)
                .expect("no encoder and no cache must be refused");
            assert_eq!(
                reason,
                t!("cleaning.tools.flux2_klein.paths_required_error"),
                "{state:?} must be refused with the three-path message"
            );
        }
        // `Some(true)` and nothing weaker: a backend that never reports `prompt_cached`
        // cannot generate without an encoder either, so an enabled button there would only
        // offer a run that always fails.

        // The waiver covers the encoder ALONE. The transformer and the VAE are still
        // required, and their message must not send the user after the 16 GB encoder they
        // have just worked around.
        for broken in [
            Flux2KleinSettings {
                transformer_path: "  ".to_string(),
                ..no_encoder.clone()
            },
            Flux2KleinSettings {
                vae_path: String::new(),
                ..no_encoder.clone()
            },
        ] {
            let reason = flux2_run_block_reason(&broken, Some(true))
                .expect("a missing transformer or VAE still blocks the run");
            assert_eq!(
                reason,
                t!("cleaning.tools.flux2_klein.model_paths_required_error")
            );
        }
        // And every other gate keeps working with the encoder waived.
        let no_prompt = Flux2KleinSettings {
            prompt: "   ".to_string(),
            ..no_encoder.clone()
        };
        assert!(flux2_run_block_reason(&no_prompt, Some(true)).is_some());
    }

    #[test]
    fn whole_region_sends_a_solid_mask_and_keeps_the_painted_one() {
        // The layer the host paints and hands over: a real stroke, not a full region.
        let mut painted = vec![0u8; 32 * 16];
        for x in 2..14 {
            painted[4 * 32 + x] = 255;
        }
        let painted_px = painted.iter().filter(|value| **value > 0).count();
        assert!(painted_px > 0 && painted_px < painted.len(), "a real stroke");

        let sent = mask_for_run(&painted, true);
        // The bytes that actually reach the wire are checked, not the intention: the
        // buffer is encoded and decoded back exactly as `run_flux2_klein_pass` does it.
        let png = encode_mask_png_l8(&sent, 32, 16).expect("encode the solid mask");
        let decoded = image::load_from_memory(&png).expect("decode").to_luma8();
        assert_eq!(decoded.dimensions(), (32, 16));
        assert!(
            decoded.pixels().all(|pixel| pixel.0[0] == 255),
            "the backend refuses whole_region unless every mask byte is 255"
        );

        // The solid buffer is built BESIDE the host's layer, never through it, so clearing
        // the switch brings the user's work back exactly as it was.
        assert_eq!(mask_for_run(&painted, false), painted);
    }

    #[test]
    fn the_default_prompt_fills_a_new_and_a_blank_settings_file() {
        // A fresh install: the field is usable the moment the tool opens, because an
        // empty prompt is the one value the run gate refuses.
        assert_eq!(Flux2KleinSettings::default().prompt, FLUX2_DEFAULT_PROMPT);
        assert!(!FLUX2_DEFAULT_PROMPT.trim().is_empty());

        // A settings file written before the prompt had a default: the key is ABSENT.
        let absent = settings_from_json(&json!({ "placement": "full_gpu", "steps": 4 }));
        assert_eq!(absent.prompt, FLUX2_DEFAULT_PROMPT);
        // A file whose prompt is present but blank — whitespace included — is the same
        // unusable state and gets the same substitution.
        for blank in ["", "   ", "\n\t "] {
            let migrated = settings_from_json(&json!({ "prompt": blank }));
            assert_eq!(
                migrated.prompt, FLUX2_DEFAULT_PROMPT,
                "a blank prompt ({blank:?}) must not survive the load"
            );
        }
        // A file that carries a real prompt keeps the user's own text, verbatim.
        let carried = settings_from_json(&json!({ "prompt": "a red balloon" }));
        assert_eq!(carried.prompt, "a red balloon");
        // And the default one passes the run gate it exists for.
        let settings = Flux2KleinSettings {
            text_encoder_path: "/models/qwen3".to_string(),
            transformer_path: "/models/flux2.safetensors".to_string(),
            vae_path: "/models/vae".to_string(),
            ..Flux2KleinSettings::default()
        };
        assert!(flux2_run_block_reason(&settings, None).is_none());
    }

    #[test]
    fn the_status_request_carries_the_prompt() {
        // `prompt_cached` is an answer ABOUT one prompt, so the prompt has to travel with
        // the question exactly as the model paths do.
        let settings = Flux2KleinSettings {
            prompt: "  remove the sfx  ".to_string(),
            ..Flux2KleinSettings::default()
        }
        .normalized();
        let header = flux2_status_header(&settings.to_params());
        assert_eq!(header["params"]["prompt"], json!("remove the sfx"));
    }

    #[test]
    fn prompt_cached_is_three_state() {
        let cached = parse_flux2_status(&json!({ "available": true, "prompt_cached": true }));
        assert_eq!(cached.prompt_cached, Some(true));
        let not_cached = parse_flux2_status(&json!({ "prompt_cached": false }));
        assert_eq!(not_cached.prompt_cached, Some(false));
        // A backend that does not know about the prompt cache at all must read as
        // "unknown", never as "your prompt is not cached".
        let silent = parse_flux2_status(&json!({ "available": true }));
        assert_eq!(silent.prompt_cached, None);

        // And the answer only counts while it is about the prompt in the field.
        let status = parse_flux2_status(&json!({ "prompt_cached": true }));
        assert_eq!(
            prompt_cache_state_for(Some(&status), Some("a prompt"), "  a prompt  "),
            Some(true),
            "the comparison is on the trimmed prompt, which is what was sent"
        );
        assert_eq!(
            prompt_cache_state_for(Some(&status), Some("a prompt"), "a different prompt"),
            None,
            "an answer about an older prompt must read as unknown"
        );
        assert_eq!(
            prompt_cache_state_for(Some(&status), None, "a prompt"),
            None,
            "no answer has landed yet"
        );
        assert_eq!(prompt_cache_state_for(None, Some("a prompt"), "a prompt"), None);
    }

    /// Settings that pass every prompt-cache gate except the one under test.
    fn cacheable_settings() -> Flux2KleinSettings {
        Flux2KleinSettings {
            text_encoder_path: "/models/qwen3".to_string(),
            prompt: "remove the sfx".to_string(),
            ..Flux2KleinSettings::default()
        }
    }

    #[test]
    fn prompt_cache_gates_name_every_blocking_condition() {
        let ready = cacheable_settings();
        let all_open =
            flux2_prompt_cache_gates(&ready, Some(true), Some(true), "entry", true, true, false);
        assert_eq!(
            all_open,
            Flux2PromptCacheGates {
                build: true,
                save: true,
                load: true,
                export: true,
                import: true
            }
        );

        // An unreachable backend closes every one of them: none can be served locally.
        let offline =
            flux2_prompt_cache_gates(&ready, Some(true), Some(true), "entry", true, false, false);
        assert_eq!(
            offline,
            Flux2PromptCacheGates {
                build: false,
                save: false,
                load: false,
                export: false,
                import: false
            }
        );
        // So does an operation already in flight — one pipeline, one progress bar.
        let busy =
            flux2_prompt_cache_gates(&ready, Some(true), Some(true), "entry", true, true, true);
        assert_eq!(busy, offline);

        // An empty prompt or a missing encoder blocks building and saving, and nothing
        // else: loading, exporting and importing do not encode anything.
        for broken in [
            Flux2KleinSettings {
                prompt: "   ".to_string(),
                ..cacheable_settings()
            },
            Flux2KleinSettings {
                text_encoder_path: String::new(),
                ..cacheable_settings()
            },
        ] {
            let gates =
                flux2_prompt_cache_gates(&broken, Some(true), Some(true), "entry", true, true, false);
            assert!(!gates.build);
            assert!(!gates.save);
            assert!(gates.load && gates.export && gates.import);
        }

        // Saving needs a cache that EXISTS. `None` is "not known yet" and is not a
        // promise that one does, so it blocks saving exactly as `Some(false)` does.
        for state in [None, Some(false)] {
            let gates =
                flux2_prompt_cache_gates(&ready, state, Some(true), "entry", true, true, false);
            assert!(!gates.save, "{state:?} must not offer a save");
            assert!(gates.build, "{state:?} still allows building one");
        }
        // …and a name to store it under.
        for name in ["", "   "] {
            let gates =
                flux2_prompt_cache_gates(&ready, Some(true), Some(true), name, true, true, false);
            assert!(!gates.save, "an empty name ({name:?}) is not accepted");
        }

        // Loading and exporting act on a listed entry; importing does not.
        let no_selection =
            flux2_prompt_cache_gates(&ready, Some(true), Some(true), "entry", false, true, false);
        assert!(!no_selection.load);
        assert!(!no_selection.export);
        assert!(no_selection.import);
    }

    #[test]
    fn a_missing_local_encoder_closes_only_the_two_operations_that_encode() {
        // The settings still NAME an encoder — this is exactly a settings file carried
        // over from another machine, where the path is filled in and points nowhere.
        let stale_path = cacheable_settings();
        let gates =
            flux2_prompt_cache_gates(&stale_path, Some(true), Some(false), "entry", true, true, false);
        assert!(
            !gates.build,
            "there is nothing on this machine to encode the prompt with"
        );
        assert!(
            !gates.save,
            "a saved entry has to name the encoder that produced it"
        );
        // Everything that only moves ready files around keeps working — that is what makes
        // an encoder-less machine usable at all.
        assert!(gates.load, "a ready cache can still be loaded");
        assert!(gates.export, "copying a file out needs no encoder");
        assert!(gates.import, "the imported file supplies everything");

        // `None` is "not known" and must not close anything by itself: the local path
        // check stays the only rule, exactly as before the field existed.
        let unknown =
            flux2_prompt_cache_gates(&stale_path, Some(true), None, "entry", true, true, false);
        assert!(unknown.build && unknown.save);
    }

    #[test]
    fn prompt_cache_headers_carry_the_settings_and_the_operation_fields() {
        let settings = cacheable_settings().normalized();
        let plain = flux2_prompt_cache_header(&settings, &[]);
        // The settings travel with EVERY prompt-cache call: the encoder path is what
        // decides the family the library is split by, and the backend refuses the call
        // outright without one.
        assert_eq!(plain["params"]["text_encoder_path"], json!("/models/qwen3"));
        assert_eq!(plain["params"]["prompt"], json!("remove the sfx"));
        assert!(plain.get("name").is_none());

        // `name` and `path` sit BESIDE `params`, not inside it: that is where the backend
        // reads them from (`_require_non_empty_str(header, ...)`).
        let named = flux2_prompt_cache_header(&settings, &[("name", json!("sfx"))]);
        assert_eq!(named["name"], json!("sfx"));
        assert!(named["params"].get("name").is_none());
        assert_eq!(named["params"]["text_encoder_path"], json!("/models/qwen3"));

        let exported = flux2_prompt_cache_header(
            &settings,
            &[("name", json!("sfx")), ("path", json!("/tmp/a.msprompt"))],
        );
        assert_eq!(exported["name"], json!("sfx"));
        assert_eq!(exported["path"], json!("/tmp/a.msprompt"));
        // `overwrite` is never sent: a name already taken must come back as an explicit
        // error, never silently replace a cache that cost a 16 GB encoder read.
        assert!(exported.get("overwrite").is_none());
    }

    #[test]
    fn the_library_listing_drops_entries_without_a_name() {
        let header = json!({
            "family": "qwen3-4b",
            "entries": [
                {
                    "name": "sfx",
                    "prompt": "remove the sfx",
                    "created_at": "2024-05-01T09:00:00Z",
                    "size_bytes": 4096
                },
                { "name": "  ", "prompt": "nameless" },
                { "prompt": "no name key at all" },
                { "name": "bare" }
            ]
        });
        let library = parse_flux2_prompt_cache_list(&header);
        assert_eq!(library.family, "qwen3-4b");
        // The name is the only field `.save`/`.load`/`.export` can act on, so an entry
        // without one is not listed at all rather than shown as an unusable row.
        assert_eq!(library.entries.len(), 2);
        assert_eq!(library.entries[0].name, "sfx");
        assert_eq!(library.entries[0].prompt, "remove the sfx");
        assert_eq!(library.entries[0].created, "2024-05-01T09:00:00Z");
        // The shorter `created` spelling is accepted too, so a rename on the wire does
        // not silently turn every entry into "no date".
        let shorter = parse_flux2_prompt_cache_list(&json!({
            "entries": [{ "name": "sfx", "created": 1_700_000_000u64 }]
        }));
        assert!(!shorter.entries[0].created.is_empty());
        // A record reporting nothing but its name still lists, with empty extras.
        assert_eq!(library.entries[1].name, "bare");
        assert!(library.entries[1].prompt.is_empty());
        assert!(library.entries[1].created.is_empty());
        // An empty answer degrades instead of failing.
        assert_eq!(
            parse_flux2_prompt_cache_list(&json!({})),
            Flux2PromptCacheList::default()
        );
    }

    #[test]
    fn a_created_timestamp_is_read_in_every_shape_the_backend_may_send() {
        // A ready-made string is shown verbatim.
        assert_eq!(
            format_prompt_cache_created(Some(&json!("  2024-05-01 12:00  "))),
            "2024-05-01 12:00"
        );
        // Python's `time.time()` is a float, so both integer and fractional Unix
        // timestamps have to render.
        assert!(!format_prompt_cache_created(Some(&json!(1_700_000_000u64))).is_empty());
        assert_eq!(
            format_prompt_cache_created(Some(&json!(1_700_000_000.75f64))),
            format_prompt_cache_created(Some(&json!(1_700_000_000u64))),
            "the fractional second must not change the rendered minute"
        );
        // Anything unusable is "no date", never a wrong one.
        assert!(format_prompt_cache_created(None).is_empty());
        assert!(format_prompt_cache_created(Some(&Value::Null)).is_empty());
        assert!(format_prompt_cache_created(Some(&json!(1e300))).is_empty());
    }

    #[test]
    fn an_import_reports_a_foreign_family_however_the_backend_spells_it() {
        let matched = parse_flux2_prompt_cache_import(
            &json!({ "name": "sfx", "family_matches": true }),
            "qwen3-4b",
        );
        assert_eq!(
            matched,
            Flux2PromptCacheOutcome::Imported {
                name: "sfx".to_string(),
                family_matches: Some(true)
            }
        );
        // The `foreign` spelling of the same fact is the inverse.
        let foreign = parse_flux2_prompt_cache_import(
            &json!({ "name": "sfx", "foreign": true }),
            "qwen3-4b",
        );
        assert_eq!(
            foreign,
            Flux2PromptCacheOutcome::Imported {
                name: "sfx".to_string(),
                family_matches: Some(false)
            }
        );
        // With neither flag, the reported family is compared against the listed one.
        let compared =
            parse_flux2_prompt_cache_import(&json!({ "name": "sfx", "family": "t5" }), "qwen3-4b");
        assert_eq!(
            compared,
            Flux2PromptCacheOutcome::Imported {
                name: "sfx".to_string(),
                family_matches: Some(false)
            }
        );
        // Nothing to compare: "not known", so no warning is invented in either direction.
        for header in [json!({ "name": "sfx" }), json!({ "name": "sfx", "family": "t5" })] {
            let unknown = parse_flux2_prompt_cache_import(&header, "");
            assert_eq!(
                unknown,
                Flux2PromptCacheOutcome::Imported {
                    name: "sfx".to_string(),
                    family_matches: None
                }
            );
        }
    }

    #[test]
    fn a_loaded_entry_answers_with_its_trimmed_prompt() {
        assert_eq!(
            parse_flux2_prompt_cache_load(&json!({ "prompt": "  remove the sfx  " })).prompt,
            "remove the sfx"
        );
        // An answer carrying no prompt yields an empty string, which the caller shows as
        // an error instead of clearing the user's field.
        assert!(parse_flux2_prompt_cache_load(&json!({})).prompt.is_empty());
        assert!(
            parse_flux2_prompt_cache_load(&json!({ "prompt": "   " }))
                .prompt
                .is_empty()
        );
        assert!(
            parse_flux2_prompt_cache_load(&json!({ "prompt": 7 }))
                .prompt
                .is_empty()
        );
    }

    #[test]
    fn a_load_says_whether_the_encoder_fingerprint_was_actually_compared() {
        let verified =
            parse_flux2_prompt_cache_load(&json!({ "prompt": "sfx", "encoder_verified": true }));
        assert_eq!(verified.encoder_verified, Some(true));
        let trusted =
            parse_flux2_prompt_cache_load(&json!({ "prompt": "sfx", "encoder_verified": false }));
        assert_eq!(trusted.encoder_verified, Some(false));
        // A backend that does not report the field only ever verified, so its silence must
        // read as "not known" and raise no notice — never as "taken on trust".
        let silent = parse_flux2_prompt_cache_load(&json!({ "prompt": "sfx" }));
        assert_eq!(silent.encoder_verified, None);
        // Neither does a value of the wrong shape.
        let wrong =
            parse_flux2_prompt_cache_load(&json!({ "prompt": "sfx", "encoder_verified": "no" }));
        assert_eq!(wrong.encoder_verified, None);
    }

    #[test]
    fn the_status_reports_the_encoder_separately_from_availability() {
        // The pair this whole feature turns on: a run is available BECAUSE the prompt is
        // cached, while no encoder exists on the machine at all.
        let cached_without_encoder = parse_flux2_status(&json!({
            "available": true,
            "prompt_cached": true,
            "text_encoder_available": false
        }));
        assert!(cached_without_encoder.available);
        assert_eq!(cached_without_encoder.text_encoder_available, Some(false));
        assert_eq!(cached_without_encoder.prompt_cached, Some(true));

        let installed = parse_flux2_status(&json!({ "text_encoder_available": true }));
        assert_eq!(installed.text_encoder_available, Some(true));
        // Same three-state rule as `prompt_cached`: a backend that predates the field must
        // not be read as "you have no encoder", which would raise a false warning and close
        // the two encode buttons.
        let silent = parse_flux2_status(&json!({ "available": true }));
        assert_eq!(silent.text_encoder_available, None);
        assert_eq!(
            parse_flux2_status(&json!({ "text_encoder_available": "yes" })).text_encoder_available,
            None
        );
    }

    #[test]
    fn a_listing_without_an_active_family_names_the_family_of_every_entry() {
        // What an encoder-less machine gets: no active family, entries from all of them.
        let library = parse_flux2_prompt_cache_list(&json!({
            "family": "",
            "directory": "/root/prompt_cache",
            "text_encoder_available": false,
            "entries": [
                { "name": "sfx", "family": "qwen3-4b-aabbccdd", "prompt": "remove the sfx" },
                { "name": "bg", "family": "qwen3-8b-11223344" }
            ]
        }));
        assert!(
            library.family.is_empty(),
            "an empty family is the backend saying none is active"
        );
        assert_eq!(library.text_encoder_available, Some(false));
        assert_eq!(library.entries[0].family, "qwen3-4b-aabbccdd");

        // The row shows which family it came from, so the user can see that the list is
        // not only "their" caches. The rendered text depends on the active catalog (a unit
        // test runs with none, where `t!`/`tf!` answer the key itself), so what is asserted
        // here is the BRANCH: with a family to show, the caption is no longer the bare name,
        // and the tooltip is no longer the one that omits it. The placeholders of the
        // templates themselves are guarded by the catalog test below.
        let entry = &library.entries[0];
        assert_ne!(entry.label(true), entry.name);
        assert_ne!(
            prompt_cache_entry_tooltip(entry, true),
            prompt_cache_entry_tooltip(entry, false)
        );
        // With one active family the name stands alone: the family is the same for every
        // row and would be pure noise.
        assert_eq!(entry.label(false), "sfx");
        // An entry that reports no family of its own is shown by name, never by a made-up
        // prefix — and its tooltip cannot change either.
        let anonymous = parse_flux2_prompt_cache_list(&json!({ "entries": [{ "name": "sfx" }] }));
        assert!(anonymous.entries[0].family.is_empty());
        assert_eq!(anonymous.entries[0].label(true), "sfx");
        assert_eq!(
            prompt_cache_entry_tooltip(&anonymous.entries[0], true),
            prompt_cache_entry_tooltip(&anonymous.entries[0], false)
        );
        // And a backend that reports neither flag says nothing about the encoder.
        assert_eq!(anonymous.text_encoder_available, None);
    }

    #[test]
    fn every_catalog_keeps_the_placeholders_a_library_row_is_built_from() {
        // A translation that drops `{family}` or `{name}` compiles and passes the
        // key-existence test, while leaving the user unable to tell whose cache a row is —
        // which is the entire reason the family is on the row at all.
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            let entry = |key: &str| -> String {
                catalog
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("locale `{tag}` lacks the key `{key}`"))
                    .to_owned()
            };
            let row = entry("cleaning.tools.flux2_klein.prompt_cache_entry_with_family");
            assert!(
                row.contains("{family}") && row.contains("{name}"),
                "locale `{tag}`: a library row names both the family and the entry, `{row}` does not"
            );
            let hover = entry("cleaning.tools.flux2_klein.prompt_cache_entry_family");
            assert!(
                hover.contains("{family}"),
                "locale `{tag}`: the hover line carries the family, `{hover}` does not"
            );
        }
    }

    #[test]
    fn gib_formatting_is_one_decimal() {
        assert_eq!(format_gib(0), "0.0");
        assert_eq!(format_gib(1024 * 1024 * 1024), "1.0");
    }
}
