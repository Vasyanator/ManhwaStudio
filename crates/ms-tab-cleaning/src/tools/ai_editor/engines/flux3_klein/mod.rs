/*
File: cleaning/tools/ai_editor/engines/flux3_klein/mod.rs

Purpose:
The FLUX.3 klein ENGINE of the «ИИ-редактор области» tool. The HOST owns the on-canvas frame —
the rectangle, the painted mask stack, the pending result and Применить/Отменить. This MODULE
owns everything model-specific: the persisted parameters (model source, the DiT and text-encoder
weight formats, prompt, steps, seed, memory steps, expert switches), the session's extra
reference images, the `inpaint.flux3_klein*` wire contracts, the catalog bundle a selection
needs (downloaded by the app's model catalog, not by the backend), the worker threads and the
engine's progress bar. It implements `region_edit_v2::engine::AiEngine`
and never touches `CanvasView`, `ProjectData` or the frame.

This file is the MODULE ROOT and holds no logic: this header, the module declarations and the
constants shared below. Every child is declared as `mod x; use x::*;` and starts with
`use super::*;`, so a file here names any item of the module without a `super::sibling::`
path (the FLUX.2 klein layout). `Flux3KleinEngine` is the one `pub` item.

How a region is edited (the backend's `inpaint/flux3_klein/region.py`):
- FLUX.3 has no mask input. The REGION is reference #0 and the output is the region's size;
  the user's extra references (up to `FLUX3_MAX_EXTRA_REFERENCES`) follow as #1 and up, and a
  marks reference (`RunMarks::Reference`) is appended last as one more of them.
- The painted mask is "permission to change". The backend validates it and answers the RAW
  generated region (`EnginePoll::DoneRaw`); the HOST composites it inside the mask (feather +
  colour match, `ms_region_postprocess`, live settings from `postprocess_settings()`), so
  everything outside stays byte-identical. An EMPTY mask is the whole-region mode, derived by
  the shared `mask_for_run`, exactly as for FLUX.2.
- The size rule (multiple of 16, side >= 128, area <= 4 MP, aspect <= 4:1) is declared through
  `constraints()` and decided by `region_edit_v2::geometry::check_size`, the one owner of
  validity; the run path re-checks the actual region through the shared `region_size_refusal`.

Submodules:
- `settings.rs`: `Flux3KleinSettings` (persisted), its vocabularies (the two weight formats are
  `ms_config::{Flux3DitFormat, Flux3EncoderFormat}`), `normalized()`, the wire `params` (with
  every offered format candidate), `effective_root()` and the settings-file load/save.
- `references.rs`: the session's extra reference images, their capacity and the worker that
  reads a picked file into a wire PNG.
- `status.rs`: the `.status` answer (`Flux3Status`) and the model verdict derived from it.
- `wire.rs`: the run call and its request/response contract, `.status`, `.unload`, and the
  engine's `.prompt_cache.*` method family and header (`FLUX3_PROMPT_CACHE_METHODS`,
  `flux3_prompt_cache_header`).
- `decisions.rs`: the PURE decisions the panel renders (readiness line, run gate, seed field,
  stage names, run outcome line, the catalog bundle, the format-row captions, the prompt
  library's gate context `flux3_cache_context` and encoder caption `flux3_encoder_caption`).
- `engine/`: `Flux3KleinEngine`, its `AiEngine` impl and its long operations.
- `ui/`: the parameter panel.
- `test_support.rs`: `cfg(test)` fixtures.

IPC (`backend_ipc::protocol`, PROTOCOL_VERSION 24, `modules/ai_backend/ipc/PROTOCOL.md` §5.4):
`inpaint.flux3_klein` (streaming, cancelled mid-run through `CallHandle::cancel`),
`.status`, `.unload`, `.prompt_cache.{build (streaming), list, export, import}` and the
forecast `large_models.profiles` with `model: "flux3_klein"`.

Prompt library (the FLUX.2 klein UX over the shared `../shared/prompt_cache*.rs`): one library
`flux3_klein` for every encoder format, named presets keyed by their exact text, TEXT encoder
mode only (in `vl_refs` the encoder also reads the images, so every run encodes and
«Кэшировать» is closed with that reason). `.status` answers `prompt_cached` about the prompt
that travelled with it; `Some(true)` in TEXT mode waives the text encoder in the run gate (its
weights may be absent), and the backend's forecast leaves it out unless it is resident.

Download: the package is pinned in the app's catalog
(`ms_sysprobe::ai_models::external_catalog::flux3_klein_package_specs`: base + the root DiT
config for bf16 / GGUF + DiT(format) + encoder(format)) and fetched by the engine's `OcrModelDownloadController` (resumable, access
preflight of the gated BFL repository, adoption of an existing tree by hash, verify), drawn by
the shared `draw_ocr_model_download`. The backend downloads nothing for FLUX.3.
*/
use crate::tools::region_edit_v2::engine::{AiEngine, EnginePoll, EngineRunRequest, EngineSection, MarksMode, MarksSupport, MaskLayerSpec, RawRegionResult, RunMarks, RunOptionsCtx, region_size_refusal};
// The post-process settings the host applies to a raw run answer (one owner of defaults, ranges, UI).
use ms_region_postprocess::PostprocessSettings;
use ms_backend_ipc as backend_ipc;
use ms_canvas::OverlayRectPx;
use ms_config as config;
// The two weight-format vocabularies and the prompt library's id and precision order (one owner:
// `ms_config::flux3_weight_format`).
use ms_config::{FLUX3_PROMPT_LIBRARY_ID, Flux3DitFormat, Flux3EncoderFormat, flux3_prompt_cache_precision_order, flux3_prompt_cache_precision_rank};
// The catalog package: its pinned specs per format and the files' paths under the package root,
// and the explicit-download controller and panel block shared with the Translation tab.
use ms_sysprobe::ai_models::external::ExternalModelSpec;
use ms_sysprobe::ai_models::external_catalog::{FLUX3_KLEIN_BFL_REPO, flux3_klein_dit_relative_path, flux3_klein_dit_spec, flux3_klein_encoder_relative_path, flux3_klein_encoder_spec, flux3_klein_package_specs};
use ms_tab_translation::ocr_model_download::{OcrModelDownloadController, OcrModelDownloadState, OcrModelPanelSnapshot, bundle_key};
use ms_tab_translation::panels::ocr_model_download::{ModelDownloadAction, draw_ocr_model_download};
use crate::tools::base::RegionEditToolBase;
use crate::tools::region_edit_v2::geometry::{AspectLimit, FrameConstraints};
use ms_local_ai::backend_health::ai_backend_offline_error;
// The one client of `large_models.*`: the step vocabulary and the forecast question.
use ms_local_ai::large_models::{LIMITS_EDIT_DEBOUNCE, LargeModelStep, ProfilesRequest, RamStep, StepChoice, VramStep, format_gib};
// The axes widget and its engine-side state, shared with FLUX.2 klein.
use crate::tools::ai_editor::large_model_resources::{ResourcesAction, ResourcesController, ResourcesView, draw_large_model_resources, forecast_line};
// The model-agnostic parts of the large-model engines: streamed progress, the Hugging Face token
// row, the catalog-download decisions, the settings-file rules, the mask -> working-mode rule and
// the prompt library (controller, gates, cache line and its drawing).
use super::shared::*;
use ms_widgets::{HfAccessHint, WheelComboBox, WheelSlider};
use eframe::egui;
use egui::Color32;
// The one wire PNG codec of the cleaning tab.
use crate::tools::region_png::{encode_color_image_png_rgba, encode_mask_png_l8};
use ms_thread as thread;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use web_time::{Duration, Instant};

// Persisted parameters, their vocabularies, the wire `params` and the settings file IO.
mod settings;
use settings::*;
// The session's extra reference images and the worker that reads them.
mod references;
use references::*;
// The `.status` answer and the model verdict derived from it.
mod status;
use status::*;
// The run, `.status` and `.unload` calls and the run's request/response contract.
mod wire;
use wire::*;
// The pure decisions the panel renders.
mod decisions;
use decisions::*;
// The engine itself: its state, its `AiEngine` contract and its long operations.
mod engine;
pub use engine::Flux3KleinEngine;
use engine::Flux3PickerPurpose;
// The parameter panel.
mod ui;
use ui::*;
#[cfg(test)]
mod test_support;
#[cfg(test)]
use test_support::*;

// ---------------------------------------------------------------------------------------
// Limits (mirrored by the backend's `inpaint/flux3_klein/wire.py`)
// ---------------------------------------------------------------------------------------

/// Every side handed to the model must be a multiple of it (`REGION_SIDE_MULTIPLE`).
const FLUX3_SELECTION_MULTIPLE: usize = 16;
/// Shortest accepted side of a region, pixels (`REGION_MIN_SIDE`).
const FLUX3_MIN_SELECTION_PX: usize = 128;
/// Largest accepted region AREA, 4 MP (`REGION_MAX_PIXELS`): within it the region is the
/// model's reference #0 unchanged, so the output is pixel-aligned with the page.
const FLUX3_MAX_SELECTION_AREA_PX2: u64 = 4 * 1024 * 1024;
/// Steepest accepted ratio between the long and the short side (`REGION_MAX_ASPECT`); with the
/// area cap it bounds the longest side at 4096 px.
const FLUX3_MAX_SELECTION_ASPECT: f32 = 4.0;

/// User references a run may carry beside the region (`MAX_EXTRA_REFERENCES`: the model takes
/// ten images and the region is always the first).
const FLUX3_MAX_EXTRA_REFERENCES: usize = 9;

const FLUX3_STEPS_MIN: u32 = 1;
const FLUX3_STEPS_MAX: u32 = 64;
/// The package's own sampler default (`config.json` `steps`); the model is distilled and has
/// no guidance or negative prompt.
const FLUX3_DEFAULT_STEPS: u32 = 16;

/// Preview tint of the engine's single mask layer (the edit-permission area). Opaque on
/// purpose: `MaskStack` scales the alpha itself.
const FLUX3_MASK_TINT: Color32 = Color32::from_rgb(120, 220, 160);

/// The engine's name in log lines (a log token, never shown to the user).
const FLUX3_LOG_LABEL: &str = "FLUX.3 klein";

/// The adapter name of the forecast (`large_models.profiles` `model`). A wire identifier.
const FLUX3_LARGE_MODEL: &str = "flux3_klein";

/// The stable engine id: a widget-id stem and a log token, never shown.
const FLUX3_ENGINE_ID: &str = "flux3_klein";

/// The gated Hugging Face repository of the BFL package (the base, the native DiTs and the bf16
/// encoder), as the catalog pins it. Named in the download block with its licence page.
const FLUX3_REPO: &str = FLUX3_KLEIN_BFL_REPO;

/// The prompt a fresh settings file starts from, and the one substituted for a blank one: an
/// empty prompt blocks the run. A literal, like FLUX.2 klein's: a stored and transmitted value
/// is never localized (`dev-docs/i18n_exclusions.md` §A5); the user simply overwrites it in
/// any language, since the model's encoder is multilingual.
const FLUX3_DEFAULT_PROMPT: &str = "Remove any text and sound effects, and restore the background underneath them";

/// Generation may first load ~20 GB of weights; the PER-FRAME timeout of the streaming run
/// (restarted on each progress frame).
const FLUX3_RUN_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
/// `.status` and `.unload` are bookkeeping calls.
const FLUX3_QUERY_TIMEOUT: Duration = Duration::from_secs(60);
