/*
FILE HEADER (cleaning/tools/ai_editor/engines/sdxl/mod.rs)

Purpose:
The «SDXL Inpaint» ENGINE of the «ИИ-редактор области» tool. The HOST owns the on-canvas
frame — the rectangle, the painted mask stack, the pending result and Применить/Отменить.
This MODULE owns everything model-specific: the two channel modes and their parameters, the
persistence of both parameter sets, the streaming wire contract, the live latent preview and
the worker thread. It implements `super::super::engine::AiEngine` and never touches
`CanvasView`, `ProjectData` or the frame.

This file is the MODULE ROOT and holds no logic: this header, the module declarations, the
imports the submodules share through `use super::*;`, and the constants below. Every item
lives in a submodule and is re-exported here at module level, so the module PATH is what
callers name regardless of which file an item sits in.

TWO MODES, ONE BACKEND METHOD. `inpaint.sdxl` takes a `mode` field and behaves as two
different pipelines behind it, which is why the mode is a PARAMETER of one engine rather than
two picker entries:
- `nine_channel`: a dedicated 9-channel inpainting UNet. The clean masked image travels as
  its own conditioning channel, so the default denoise is 1.0.
- `four_channel`: an ordinary 4-channel SDXL checkpoint. The hole is prefilled by LaMa first
  — which removes the text from the context — so a moderate denoise (0.75) is enough. The
  `lama_model` field rides along ONLY in this mode; the 9-channel pipeline has no prefill
  step and would not know the field.

The LaMa prefill catalog is NOT duplicated here: `super::lama` owns it, and this engine reads
the LaMa-v2 SUBSET of it (`lama_v2_model_catalog`). The MPE checkpoint is deliberately absent
from that view — it is a different architecture in a different directory, and the backend
resolves the `lama_model` name inside `Torch/LaMa/models`.

The mask means "REGENERATE what is under it" — the inpaint meaning, carried by the same
yellow the mask-inpaint editor uses. The mask is therefore MANDATORY (`allows_empty_mask()`
is unconditionally `false`): SDXL inpainting has no whole-region mode the way FLUX.2 klein
does, so the host disables «Обработать» rather than sending a request that could only answer
with its own input.

The request shape — the method, the header ints, the blob layout and the streamed progress
frames — belongs to the backend and travels under `PROTOCOL_VERSION`: changing any of it is a
protocol change, not an engine-local one.

Submodules (each re-exported into this root with `use <name>::*;`):
- `settings.rs`: `SdxlMode`, `SdxlSettings`, `SdxlPersisted`, `SdxlRunConfig`, the file IO
  and the save gate.
- `progress.rs`: the progress state shared between the worker and the panel, and the
  progress bar plus live latent preview drawn from it.
- `wire.rs`: the streaming run call, the unload call, the request header of each mode and
  the PNG/L8 blob encoding.
- `engine.rs`: `SdxlEngine`, its `AiEngine` contract and its parameter panel.

Key items:
- `SdxlEngine`: the `AiEngine` implementation and its wiring.
- `SdxlMode`: the channel mode, which decides both the defaults and the wire params.
- `SdxlSettings` / `SdxlPersisted`: everything persisted to
  `config::sdxl_inpaint_settings_path()`. Both the file and its field names are FIXED by
  on-disk compatibility: an existing `sdxl_inpaint_settings.json` must keep loading unchanged.

Notes:
- The answer of a run is PIXELS and nothing else: `EnginePoll::Done` carries a `ColorImage`
  of exactly the frame rectangle, and merging it into the clean overlay is the HOST's job.
- Nothing here blocks the GUI thread: the settings load/save, the unload call and the run
  itself all live on workers, and `poll` only drains their channels.
- `inpaint.sdxl` STREAMS: every diffusion step sends a `progress` frame carrying `step` /
  `total` and, when the backend produced one, a latent preview PNG in the frame's blob. The
  worker publishes them into the shared progress under a GENERATION stamp, so a cancelled
  run's detached worker can no longer drive the bar of whatever runs next.
- A run cannot be stopped backend-side: `cancel` detaches the answer and the backend finishes
  the pass.
*/
use super::super::engine::{AiEngine, EnginePoll, EngineRunRequest, EngineSection, MaskLayerSpec};
use super::lama::{
    LamaModelSpec, default_lama_model_filename, ensure_lama_model_for_external,
    lama_v2_model_catalog,
};
// The engines' shared run-path re-check of `FrameConstraints`, so this engine's refusal
// wording is the host's own.
use super::region_size_refusal;
use crate::backend_ipc::{self, CallError};
use crate::canvas::OverlayRectPx;
use crate::config;
use crate::tabs::cleaning::tools::region_edit_v2::geometry::FrameConstraints;
use crate::tabs::translation::backend_health::ai_backend_offline_error;
use crate::widgets::{WheelComboBox, WheelSlider, WheelSpinBox};
use eframe::egui;
use egui::Color32;
use image::{ColorType, ImageEncoder};
use ms_thread as thread;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use web_time::Duration;

// The channel modes, the persisted parameter sets and their file IO.
mod settings;
use settings::*;
// The shared progress state and the bar plus latent preview drawn from it.
mod progress;
use progress::*;
// The streaming run call, the unload call and the blob encoding they carry.
mod wire;
use wire::*;
// The engine itself: its state, its `AiEngine` contract and its parameter panel.
mod engine;
// The one item the picker needs; a private glob would not carry it out of the module.
pub use engine::SdxlEngine;

// ---------------------------------------------------------------------------------------
// Limits and shared constants
// ---------------------------------------------------------------------------------------

/// Per-call timeout of `inpaint.sdxl` and `inpaint.sdxl.unload`.
///
/// Twenty minutes: SDXL diffusion may run its whole step schedule on the CPU, and the first
/// call of a session also loads the checkpoint.
const SDXL_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Both sides of the region must be whole multiples of this.
///
/// The SDXL VAE downscales by 8, so a side that is not a multiple of 8 cannot round-trip.
/// The grid is the ONLY constraint this engine puts on the frame: the pipeline caps neither
/// the area nor the aspect ratio, so `constraints()` leaves both `None` (unlike FLUX.2
/// klein, which caps both). A test pins that.
const SDXL_SELECTION_MULTIPLE: usize = 8;
/// Shortest accepted side. It is exactly what the grid step already implies: with a multiple
/// of 8 there is no valid smaller side, so this states the same rule rather than adding one.
const SDXL_MIN_SELECTION_PX: usize = 8;

/// Preview tint of the engine's single mask layer, i.e. of the area to REGENERATE.
///
/// The inpaint yellow of the mask-inpaint editor (`tools/base.rs`, the «Жёлтая: удаление»
/// legend and its mask preview), so a mask painted here reads the same as it always did.
/// Opaque on purpose: `MaskStack` scales the alpha itself, and a tint that arrived already
/// translucent would be darkened twice.
const SDXL_MASK_TINT: Color32 = Color32::from_rgb(255, 220, 0);

/// Diffusers samplers offered in the panel.
///
/// Literal wire tokens, never localized: the backend maps each name to a concrete scheduler
/// configuration, so a translated spelling would name no scheduler at all. Keep in sync with
/// the service.
const SDXL_SAMPLERS: [&str; 8] = [
    "Euler",
    "Euler a",
    "DPM++ 2M",
    "DPM++ 2M Karras",
    "DPM++ SDE Karras",
    "DDIM",
    "UniPC",
    "Heun",
];

/// Sentinel `seed` value meaning "draw a fresh random seed for every run".
///
/// The BACKEND's contract, which is why the field stays signed: any non-negative value is a
/// fixed seed and reproduces the same image, `-1` is the only value that does not.
const SDXL_RANDOM_SEED: i64 = -1;

// Parameter ranges offered by the panel.
const SDXL_STEPS_MIN: u32 = 1;
const SDXL_STEPS_MAX: u32 = 150;
const SDXL_CFG_MIN: f32 = 1.0;
const SDXL_CFG_MAX: f32 = 20.0;
const SDXL_DENOISE_MIN: f32 = 0.0;
const SDXL_DENOISE_MAX: f32 = 1.0;
const SDXL_MASK_DILATION_MIN: u32 = 0;
const SDXL_MASK_DILATION_MAX: u32 = 64;
const SDXL_MASK_BLUR_MIN: u32 = 0;
const SDXL_MASK_BLUR_MAX: u32 = 64;

/// Widest the live latent preview is drawn, in points. A preview wider than this is scaled
/// down to fit; a narrower one is never blown up.
const SDXL_PREVIEW_MAX_WIDTH_PT: f32 = 360.0;
