/*
FILE HEADER (cleaning/tools/ai_editor/engines/lama/mod.rs)

Purpose:
The «Lama» ENGINE of the «ИИ-редактор области» tool. The HOST owns the on-canvas frame —
the rectangle, the painted mask stack, the pending result and Применить/Отменить. This
MODULE owns everything model-specific: the four-entry model catalog, the parameters of the
two backend methods and their persistence, the background model-presence scan, the unload
call, the wire contracts and the worker thread. It implements
`super::super::engine::AiEngine` and never touches `CanvasView`, `ProjectData` or the frame.

This file is the MODULE ROOT and holds no logic: this header, the module declarations, the
imports the submodules share through `use super::*;`, and the constants below. Every item
lives in a submodule and is re-exported here at module level, so the module PATH is what
callers name regardless of which file an item sits in.

The mask means "REMOVE what is under it" — the inpaint meaning, the inverse of FLUX.2
klein's "you MAY change what is under it". The mask is therefore MANDATORY
(`allows_empty_mask()` is unconditionally `false`): with nothing painted there is nothing to
remove, and the host disables «Обработать» rather than sending a request that could only
answer with the region it was given. The single mask layer is tinted with the shared removal-mask
tint `ms_theme::canvas::MASK_TINT`, the same yellow as the mask-inpaint editor
(`tools/base.rs`), so the colour means the same thing in both places.

FOUR MODELS, TWO BACKEND METHODS, ONE BUTTON. `inpaint.lama_v2` and `inpaint.lama_mpe` are
genuinely different architectures (different generator, different forward arity, different
weight layout, different model directory), so the backend keeps both methods and this side
dispatches: every catalog entry carries the method it runs and therefore which parameter set
it takes. Both request shapes belong to the backend and travel under `PROTOCOL_VERSION`:
changing either is a protocol change, not an engine-local one.

- `best.ckpt`, `lama_large_512px.ckpt`, `anime-manga-big-lama.pt` -> `inpaint.lama_v2`,
  parameters `refine` / `n_iters` / `max_scales` / `px_budget`, models in `Torch/LaMa/models`.
- `inpainting_lama_mpe.ckpt` -> `inpaint.lama_mpe`, parameter `inpaint_size`, model in
  `Torch/LaMa_MPE`.

REFINE IS NOT UNIVERSAL. The backend refuses its refine pass on a TorchScript `.pt`
checkpoint outright, so `LamaModelSpec::supports_refine` is a CONSTANT of the catalog entry
rather than something re-derived from the file extension at a call site. When the selected
entry does not support it the refine controls are disabled with a tooltip that says why, and
`effective_refine` — the one place the flag is decided — keeps `refine: true` off the wire.

Submodules (each re-exported into this root with `use <name>::*;`):
- `catalog.rs`: the four `LamaModelSpec` entries, the `LamaMethod` dispatch and the
  ensure-before-run path. Also the ONLY items this module exports outside `ai_editor`: the
  SDXL 4-channel prefill picker shares the LaMa-v2 subset of the catalog.
- `scan.rs`: the background presence scan of both model directories and the ✓/«скачать»
  status line it feeds.
- `settings.rs`: `LamaSettings` — the selected model and every parameter — plus its file IO
  and the save gate.
- `wire.rs`: the run and unload calls, the request header of each method and the PNG/L8 blob
  encoding.
- `engine.rs`: `LamaEngine`, its `AiEngine` contract and its parameter panel.

Key items:
- `LamaEngine`: the `AiEngine` implementation and its wiring.
- `LamaModelSpec` / `LamaMethod` / `lama_model_catalog` / `lama_v2_model_catalog`: the
  catalog and its two views. The v2 view exists because the SDXL 4-channel prefill can run
  only those three checkpoints — it calls `inpaint.sdxl` with a `lama_model` name the
  backend resolves inside `Torch/LaMa/models`, a path the MPE checkpoint is not on.
- `LamaSettings`: everything persisted to `config::lama_engine_settings_path()`, loaded and
  saved on worker threads and driven from `AiEngine::poll`.

Notes:
- The answer of a run is PIXELS and nothing else: `EnginePoll::Done` carries a `ColorImage`
  of exactly the frame rectangle, and merging it into the clean overlay is the HOST's job.
- Nothing here blocks the GUI thread: the settings load/save, the model scan, the unload
  call and the run itself all live on workers, and `poll` only drains their channels.
- There is no progress reporting: `inpaint.lama_v2` / `inpaint.lama_mpe` are plain
  request/response calls with no streamed frames, so the panel shows a status line and the
  engine answers `Running` until the worker replies. For the same reason a run cannot be
  cancelled backend-side — the plain `call` never exposes the request id `Client::cancel`
  needs — so `cancel` detaches the answer and the backend finishes the pass.
*/
use super::super::engine::{AiEngine, EnginePoll, EngineRunRequest, EngineSection, MaskLayerSpec};
// The engines' shared run-path re-check of `FrameConstraints`, so this engine's refusal
// wording is the host's own.
use super::region_size_refusal;
use ms_sysprobe::ai_models;
use ms_backend_ipc::{self as backend_ipc, CallError};
use ms_canvas::OverlayRectPx;
use ms_config as config;
use crate::tools::region_edit_v2::geometry::FrameConstraints;
use ms_tab_translation::backend_health::ai_backend_offline_error;
use ms_widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use image::{ColorType, ImageEncoder};
use ms_thread as thread;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use web_time::Duration;

// The four model entries, the method each one runs and the ensure-before-run path.
mod catalog;
// Re-exported one level up, to `engines`, and no further: the SDXL engine's 4-channel
// prefill picker is the only consumer outside this module.
pub(super) use catalog::{
    LamaModelSpec, default_lama_model_filename, ensure_lama_model_for_external,
    lama_v2_model_catalog,
};
use catalog::*;
// The background presence scan of both model directories.
mod scan;
use scan::*;
// The persisted selection and parameters, and their file IO.
mod settings;
use settings::*;
// The run/unload calls and the blob encoding they carry.
mod wire;
use wire::*;
// The engine itself: its state, its `AiEngine` contract and its parameter panel.
mod engine;
// The one item the picker needs; a private glob would not carry it out of the module.
pub use engine::LamaEngine;

// ---------------------------------------------------------------------------------------
// Limits and shared constants
// ---------------------------------------------------------------------------------------

/// Per-call timeout of the two inpaint methods. Model warmup plus one pass can take a
/// while on first use, which is what the five minutes are for.
const LAMA_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Both sides of the region must be whole multiples of this.
///
/// The grid is the ONLY constraint this engine puts on the frame: neither backend method
/// caps the area or the aspect ratio, so `constraints()` leaves both `None` (unlike FLUX.2
/// klein, which caps both). A test pins that.
const LAMA_SELECTION_MULTIPLE: usize = 8;
/// Shortest accepted side. It is exactly what the grid step already implies: with a
/// multiple of 8 there is no valid smaller side, so this states the same rule rather than
/// adding one.
const LAMA_MIN_SELECTION_PX: usize = 8;

// Refine parameter ranges of `inpaint.lama_v2`, as offered by the panel.
const LAMA_N_ITERS_MIN: u8 = 5;
const LAMA_N_ITERS_MAX: u8 = 50;
const LAMA_MAX_SCALES_MIN: u8 = 1;
const LAMA_MAX_SCALES_MAX: u8 = 5;
const LAMA_PX_BUDGET_MIN: u32 = 500_000;
const LAMA_PX_BUDGET_MAX: u32 = 4_000_000;
/// Working resolution of `inpaint.lama_mpe`. The backend clamps to the same window, so
/// this range is the UI's half of one rule rather than a second one.
const LAMA_INPAINT_SIZE_MIN: u32 = 512;
const LAMA_INPAINT_SIZE_MAX: u32 = 4096;
