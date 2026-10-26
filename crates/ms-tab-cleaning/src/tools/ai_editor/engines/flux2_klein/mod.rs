/*
FILE HEADER (cleaning/tools/ai_editor/engines/flux2_klein/mod.rs)

Purpose:
The FLUX.2 klein ENGINE of the «ИИ-редактор области» tool. The HOST owns the on-canvas
frame — the rectangle, the painted mask stack, the pending result and Применить/Отменить.
This MODULE owns everything model-specific: the parameters and their persistence, the two
memory steps («Видеопамять» / «Оперативная память») and their large-model forecast, the
prompt-cache library, the request/response wire
contracts, the OOM recovery, the worker thread and its own progress bar. It implements
`region_edit_v2::engine::AiEngine` and never touches `CanvasView`, `ProjectData` or the frame.

This file is the MODULE ROOT and holds no logic at all: this header, the module
declarations, and the constants shared by everything below — the selection limits, the
parameter ranges, the mask tint and the default prompt (status lines use `ms_theme::status`). Every
other item was lifted verbatim into the submodules listed below and is re-exported here at
module level, so the module PATH is unchanged and a submodule may name any item of this
module regardless of which file it now lives in. `Flux2KleinEngine` is the one `pub` item
and is re-exported explicitly, because a private glob would not carry it out of the module.

The mask means "you MAY change what is under it" — the inverse of the mask-inpaint tools'
"remove what is under it". Everything outside it must survive the round trip untouched,
which is why the engine declares exactly ONE mask layer. What goes on the wire is the
LATENT mask: that layer grown by `mask_dilate_px` (cv2-exact ellipse,
`ms_region_postprocess::dilate_ellipse`, on the run worker); the host's composite keeps the
pixels outside the PAINTED mask byte-identical whatever the model did there.

TWO ENGINES, ONE IMPLEMENTATION (`Flux2Variant`, declared in `ms-config`, which owns every
fact that keys a path, a persisted document or an id; what a variant offers is
`ms_config::flux2_weight_format`; its caption is this module's own
`variant_presentation.rs`): FLUX.2 klein 9B and FLUX.2 klein 4B are separate entries of the
picker built from this same module, parameterized by the variant. The variant keys the model
directory, the derived component paths, the engine id, the picker caption and the settings
FILE (9B keeps `flux2_klein_settings.json` unchanged); nothing else on the wire changes,
because every method is keyed by the three component paths. BOTH packages are pinned in the
app's catalog (`external_catalog::flux2_klein_package_specs`): the download block offers the
two weight formats («Веса трансформера» / «Веса текстового энкодера», bf16 or a GGUF quant),
for 9B the encoder SOURCE («Расцензуренный энкодер»; 4B offers only the stock encoder, so the
toggle is not drawn there and the flag is inert), the Hugging Face token row while a gated
spec of the bundle (BFL 9B, the uncensored encoder) is not installed, and ONE pinned bundle
download through the engine's `OcrModelDownloadController` (which also adopts an existing
unmarked download once — the backend's former 9B download included). The backend keeps ONE
pipeline and ONE text encoder resident, keyed by those paths, so selecting the other variant
unloads the previous one — the panel says so and this side orchestrates nothing.

Working modes are DERIVED from the painted mask, not chosen: there is no switch and no
persisted field, and `mask_for_run` is the only place the decision is made.
- something painted: only what is under the mask may change. The painted buffer goes on
  the wire verbatim and `whole_region` travels as `false`.
- nothing painted: the WHOLE selected region is regenerated. The request keeps its shape —
  a mask is still sent, a SOLID one built by `mask_for_run`, because the backend refuses
  `whole_region = true` unless the mask really is uniformly 255. `mask_dilate_px` is
  ignored then (there is no contour to grow: the latent mask is the solid one); the host's
  feather keeps working and is what softens the join between the regenerated region and
  the page, while its colour match is off (no ring outside the mask to match against).
`allows_empty_mask()` is therefore unconditionally `true`: an empty mask is a legal run
rather than a refusal, and the host is what tells the user so (its own green hint under
«Обработать»).

Selection contract (checked twice, on purpose):
- `constraints()` declares multiple of 16, shortest side >= 128 px, area <= 1 MP, aspect not
  steeper than 8:1. The frame snaps and validates against them, so a rectangle that reaches
  `start` has already satisfied every one of them.
- The run path re-validates the ACTUAL region size (`region_block_reason`, on the worker as
  well) and refuses with a named reason: a host that hands over a region of another size
  gets an explanation instead of a request the backend would reject.

Panel layout (`Flux2PanelCtx::draw`, and its ORDER is the design):
progress bar -> run status -> the PROMPT block (the English field, the one compact line
about its cache state, and two toggles that unfold the translator and the prompt-cache
library) -> «Сила изменения» -> the READINESS line -> three sibling collapsible sections,
«Установка модели» / «Память и скорость» / «Для экспертов» -> the mask note -> the shared
«Постобработка» section (`../shared/postprocess_section.rs`), docked at the bottom.
Everything a user touches per edit is above the folds; everything set once per machine is
inside them. There is no wrapper section around the whole body and no section nested in
another. «Установка модели» is the only one built from `CollapsingState` rather than
`RegionEditToolBase::draw_region_editor_collapsible_section`, because it has to be opened
from outside itself: by «Установить» on the readiness line, and once by the first `Missing`
verdict that lands (never by `default_open`, which on the first frame would only ever see
the `Unknown` that precedes the first `.status` answer). That one-shot opening yields to
the user: a fold he has moved by hand is never forced again — see `flux2_install_seed`.

Submodules (each re-exported into this root with `use <name>::*;`):
- `engine/`: the engine itself. `mod.rs` holds `Flux2KleinEngine`, its `AiEngine` impl and
  the polling that keeps its derived state current; `actions.rs` holds the `start_*` /
  `poll_*` pair of every long operation (component actions, the HF token, the translator,
  the pickers and the prompt-cache jobs); the catalog download is the engine's controller.
- `ui/`: the parameter panel. `mod.rs` is the body and its ORDER; `install.rs`,
  `components.rs`, `advanced.rs` and `mask_hint.rs` draw the blocks it calls into.
- `decisions.rs`: the PURE decisions the panel renders — `flux2_readiness_line`,
  `flux2_prompt_cache_line`, `flux2_run_block_reason`, `region_block_reason` — kept apart
  from the drawing so they can be asserted without a frame.
- `settings.rs`: `Flux2KleinSettings` and everything persisted with it — the dtype
  vocabulary, the two step ids, the one-time migration of the retired memory placement,
  the source mode and the effective-path derivation, plus the settings file IO (a damaged
  file is moved aside, never overwritten).
- `status.rs`: the `.status` component catalog (`Flux2Status`) and every verdict derived
  from it — `Flux2ModelReadiness`, the merged component rows, the install-fold seed and
  `flux2_pipeline_busy`.
- `prompt_cache.rs`: the saved `.msprompt` library — its shapes, its operation gates and
  the whole `.prompt_cache.*` wire block.
- The memory forecast is NOT a file here: it is `large_models.profiles`, asked through
  `ms_local_ai::large_models::ProfilesQuery` (latest wins, one in flight) and drawn by the
  shared `ai_editor::large_model_resources` widget.
- `run_outcome.rs`: what one run answers with — the raw region, the run's mask and latent
  mask, the `applied.vae_tiling` flag, a degraded run's effective steps.
- `session.rs`: the run channel and the pickers.
- `wire.rs`: the generation run (including the latent-mask dilation), the `.status` and
  `.component_action` calls and the image/mask blob encoding.
- The MODEL-AGNOSTIC parts are not files here: the progress state of all three long
  operations with its generation guard, rate window, frame parser and streaming call, the
  Hugging Face token controls and their drawing, the settings-file
  quarantine and save backoff, and `mask_for_run` are `../shared/` (`use super::shared::*`),
  and the forecast / caps / learned-floor state is `ai_editor::large_model_resources`'
  `ResourcesController`.
- `variant_presentation.rs`: the `Flux2VariantPresentation` extension trait — the picker
  caption of a variant, which belongs to this module rather than to the configuration crate
  that declares the enum.
- `test_support.rs`: `cfg(test)` only — the fixtures every `mod tests` here is built from.

Key items:
- `Flux2KleinEngine`: the `AiEngine` implementation and its wiring.
- `flux2_readiness_line` / `flux2_prompt_cache_line` / `flux2_component_rows`: the panel's
  decisions, kept as PURE functions with unit tests. What a line says, which fact wins the
  one line the prompt block has for it, and how a component's two halves are joined are
  contracts; the drawing code only renders their answers.
- `Flux2KleinSettings`: everything persisted to this VARIANT's settings file
  (`config::flux2_klein_settings_path`), loaded/saved on worker threads. The file records
  the variant it belongs to; an absent field reads as 9B, and the loader stamps the
  OWNING variant so a hand-copied file cannot lie about which model its paths describe.
  `normalized()` is the ONLY value ever put on the wire. The prompt token budget is
  NOT among them: `FLUX2_MAX_SEQ` is pinned at 512, which was already the maximum
  the backend accepts, and the length is part of the prompt-cache key — lowering it
  invalidated every entry of the saved `.msprompt` library at once.
- `Flux2ModelReadiness` / `flux2_model_readiness`: THE answer to "is the model
  installed on this machine", derived from the `.status` presence catalog together
  with `effective_paths` and the source mode. Three-state on purpose: `Unknown` (no
  answer yet) never blocks a run, `Missing` blocks and names every missing component,
  `Ready` passes. It is what keeps the refusal LOCAL in download mode, where the
  derived paths are never empty and the backend's untranslated «Путь ... не найден»
  used to be the first thing the user saw. The catalog it reads is an answer ABOUT
  THREE PATHS and counts only while it still describes them (`flux2_status_for_paths`,
  the exact counterpart of `prompt_cache_state_for` for the prompt half of the same
  answer); a path edit both invalidates it — back to `Unknown`, which never blocks —
  and re-arms the query (`note_settings_changed`).
- `Flux2SessionState`: the run channel. The painted mask is NOT
  here — it belongs to the host's `MaskStack` and arrives in `EngineRunRequest::masks`.
- The memory steps (`vram_step` / `ram_step`, `StepChoice`): what a step MEANS is the
  backend's (`runtime/large_models/tiers.py`); this side persists the ids and shows the
  `large_models.profiles` forecast for them. A run never rewrites the user's steps — the
  backend learns a floor instead, shown and resettable in «Память и скорость».
- `Flux2Status`: the `.status` component catalog. The forecast is COMPUTED BY THE BACKEND
  (`large_models.profiles`); its geometry input is the region SIZE, so `set_region` arms it
  on a settled size change and never on a move — see that method for the full rule.

IPC (`backend_ipc::protocol`):
- `inpaint.flux2_klein` — streaming. Header `{image_len, mask_len, params}`, blob
  `region.png ++ mask.png [++ reference.png]` (mask = the LATENT mask, L8 0/255, exactly the
  region size; `params` carries none of `mask_dilate_px`, `mask_feather_px`, `color_match`,
  `color_match_local_pct`, `color_match_scale_px`, which protocol 17 refuses by name). Response header
  `{image_len, oom_recovered, applied{...}, degraded, degrade_reason, *_step_effective}`,
  blob = RGB PNG of the RAW decode of exactly the region size (outside the mask a VAE round
  trip, not the original bytes), validated by STRICT equality before use (`image_len` is REQUIRED; an answer
  without it is refused). Progress frames carry `phase` (`load`/`generate`), `step`,
  `total`, `label` and no preview blob. The request goes out through `begin_call`, so
  its id is known and a cancel can stop it with `CallHandle::cancel`.
- The backend may RECOVER from an out-of-memory failure (the decode retried with VAE
  tiling, or the run degraded to a more economical step pair). Of `applied` this side reads
  ONLY `vae_tiling` (tolerant of the five-flag v13 and the one-flag v14 object) and writes
  it back; the user's steps are never rewritten — the backend learns a floor. A degraded
  run names the effective steps in the engine's status line.
- `.status`, `.unload` — one-shot; `large_models.profiles` (the forecast) likewise. All
  carry the normalized `params`: they are questions ABOUT the paths in the request, and a
  backend that receives none answers about the paths of its last successful
  generation, i.e. about nothing until one has run. `.status` additionally answers
  `prompt_cached` — whether the embeddings of the prompt IN THE REQUEST are already
  held — and the field is optional: a backend that omits it reads as "not known",
  never as "not cached". It also answers `text_encoder_available`, which is a DIFFERENT
  question from `available`: a run whose prompt is cached needs no encoder, so
  `available` stays true while this is false. And it answers `guidance_supported`, whose
  ABSENCE means SUPPORTED — the inverse default of the two above, because that is what
  every backend older than the field reports and what the control did before it existed.
- GENERATING WITHOUT A TEXT ENCODER is supported and is the reason the prompt-cache
  library exists: the denoise and the VAE decode never look at the encoder, so a
  `.msprompt` carried to a machine that never downloaded the 16 GB Qwen3 is enough. The
  run gate therefore waives the encoder path when `.status` says the prompt is cached,
  and only then. What the absence costs is stated where it happens: the one line under the
  prompt takes the warning ("only ready caches work") unless the prompt is already cached,
  in which case the affirmative wins because the run works either way
  (`flux2_prompt_cache_line`), a disabled «Кэшировать»/«Сохранить кэш»
  (the two operations that must ENCODE, refused backend-side anyway), the family shown on
  every library row (a machine with no encoder has no ACTIVE family, so the listing spans
  all of them), and a one-off notice after a load whose `encoder_verified` came back
  false — the file's own metadata was taken on trust because nothing local could compare
  the fingerprint. Загрузить/Экспорт/Импорт keep working throughout.
- `.prompt_cache.*` — the prompt-cache LIBRARY, six methods carrying the normalized
  `params` plus their own fields. `build` is STREAMING (reading the Qwen3 encoder takes
  far longer than a call may block for) and drives the same progress bar as a generation, which is why the
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
- THE MODEL SOURCE IS A MODE (`source_mode`, persisted, defaulting to `manual`): either
  the user's own three paths, or the copy this panel downloads. Exactly one body is drawn
  under the switch at the top of «Установка модели».
  `Flux2KleinSettings::effective_paths` is the ONE answer to "which three paths does a run
  use" and every consumer goes through it. In download mode the paths are DERIVED from
  `config::flux2_klein_dir(variant)` and the encoder toggle and the manual fields are never read
  or written, so a hand-built model tree survives a download and flipping back restores it
  exactly. That is why a finished download does not write its paths into the settings.
- THE DOWNLOAD is not a backend method (the backend's `.download.*` were retired at protocol
  22): in download mode the engine's `OcrModelDownloadController` fetches the pinned catalog
  bundle of the selection (`flux2_catalog_bundle`), checks Hugging Face access to its gated
  repositories in Rust (`ms_sysprobe::ai_models::hf_access`) and adopts files already on disk.
  The token is the process-wide `ms_sysprobe::hf_token` global (OS secret store), is NOT a
  setting of this engine, and must never be logged; this panel only edits it (the token row).
- `.component_action` — STREAMING, one per-component load / unload / move / warm-up.
  Header: `component`, `action` and the normalized `params`; the answer repeats the
  `.status` `components` block as it stands after the action. It claims the SAME progress
  bar as a generation and a `.prompt_cache.build`, so the three are mutually exclusive.
  The wire contract is `dev-docs/flux2_component_residency.md`.

Contracts:
- PER-COMPONENT RESIDENCY is reported by `.status` and rendered in the SAME list as the
  presence catalog — one row per component carrying presence, size, residency and the
  backend's action buttons together: `components[<name>].residency` is one of `not_loaded` / `ram` / `gpu` /
  `offloaded` / `mixed` — five and not three, because accelerate's offload leaves the
  parameters on `meta` with the bytes in a host map, and a load can leave a component
  genuinely split. An ABSENT `components` means NOT KNOWN and never "not loaded"
  (`components_busy` then says the service could not take its lock), a residency literal
  this build does not know leaves the row stateless with the literal on hover, and an
  unknown ACTION is dropped from that row. Which actions are possible is the SERVICE's
  decision and travels in `actions`: this side renders that list and never re-derives it,
  because the rule depends on the accelerate hooks, the pipeline-wide model cache key and
  the memory guard, none of which the UI can see. The transformer and the VAE load and
  unload TOGETHER for that cache-key reason, and their hovers say so.
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
  (`dev-docs/region_edit_v2_plan.md` §13.2 D13). THREE operations claim it — a generation,
  `.prompt_cache.build` and `.component_action` — and every gate that means "wait for the
  current operation" reads `flux2_pipeline_busy` over all three rather than one receiver.
  The RUN gate reads `non_run_pipeline_busy` instead — the same rule with the generation
  taken out, so a run cannot report itself as the reason it cannot start while the other two
  genuinely block one. The bar has an optional SECOND level (`file_step`/`file_total`/
  `file_label`, a checkpoint read of a load), so a frame without them renders the overall
  bar alone rather than a second bar frozen at zero; its read SPEED is derived here from
  those frames by the shared `RateEstimator`. The catalog download has its own progress in
  its block (`draw_ocr_model_download`).
- The prompt sent to the backend is the ENGLISH field. The optional second field plus
  the Google/Yandex/DeepL picker only fill it in, reusing the translation tab's own
  dispatcher (`translate_texts_via_translator`, paced by the same process-wide MT request
  limiter as a Translation run) instead of a second copy of it. It is
  never empty: an empty prompt blocks the run, so `FLUX2_DEFAULT_PROMPT` is substituted
  both for a fresh settings file and for one whose prompt is missing or blank.
- `.status` and `.prompt_cache.list` are re-queried through the SAME one-shot arming as
  the memory forecast (`status_wanted`, `prompt_cache_list_wanted`): a change re-arms
  the flag, and at most one query is ever in flight, so editing the prompt cannot turn
  a keystroke into a request. A `prompt_cached` answer is shown only while the prompt it
  was asked about still equals the one in the field.
- The answer of a run is RAW PIXELS plus the run facts (D12, raw-result amendment):
  `EnginePoll::DoneRaw` carries a `RawRegionResult` — the decode, exactly the frame
  rectangle, the mask the run used, the latent mask as the colour-match sample mask and
  `whole_region`. The HOST post-processes it (composite onto the unmarked region, feather,
  colour match) with `AiEngine::postprocess_settings`, i.e. this engine's persisted
  «Постобработка» values (`Flux2KleinSettings::postprocess`, flattened into the settings
  file under the keys it always had), re-deriving a pending result whenever they change.
  Merging it into the clean overlay is the HOST's job too; this module never writes
  `CleanOverlaysModel` storage and never touches `CanvasView`.
- The model is distilled: 4 steps and `guidance_scale = 1.0` are the defaults and
  there is no negative prompt — do not add a field for one. Whether guidance can do
  anything at all is the BACKEND's answer, not an assumption of this side: a checkpoint
  that declares `"is_distilled": true` makes diffusers switch classifier-free guidance off,
  and `.status` reports that as `guidance_supported: false`, on which the panel closes the
  control (`flux2_guidance_supported`). The field is OPTIONAL and an absent one means
  SUPPORTED — the shipped klein checkpoints are distilled, but the paths are the user's and
  may point at one that is not.
*/
use crate::tools::region_edit_v2::engine::{AiEngine, EnginePoll, EngineRunRequest, EngineSection, MarksMode, MarksSupport, MaskLayerSpec, RawRegionResult, RunMarks, RunOptionsCtx};
// The host-side post-process: its persisted settings (flattened into this engine's document)
// and the latent-mask dilation the run worker applies before the mask goes on the wire.
use ms_region_postprocess::{PostprocessSettings, dilate_ellipse};
use ms_backend_ipc as backend_ipc;
use ms_canvas::OverlayRectPx;
use ms_config as config;
use ms_config::{Flux2EncoderSource, Flux2Variant, Flux2WeightAxis, Flux2WeightFormat};
// The catalog package of both variants: its pinned specs per weight format and encoder source,
// and the explicit-download controller and panel block shared with the Translation tab (the
// SDXL engine is the other cleaning consumer).
use ms_sysprobe::ai_models::external::ExternalModelSpec;
use ms_sysprobe::ai_models::external_catalog::{Flux2TransformerLayout, flux2_klein_package_specs, flux2_klein_spec};
use ms_tab_translation::ocr_model_download::{JobKind, OcrModelDownloadController, OcrModelDownloadState, OcrModelPanelSnapshot, bundle_key};
use ms_tab_translation::panels::ocr_model_download::{ModelDownloadAction, draw_ocr_model_download};
use crate::tools::base::RegionEditToolBase;
use crate::tools::region_edit_v2::geometry::{AspectLimit, FrameConstraints};
use ms_local_ai::backend_health::ai_backend_offline_error;
// The one client of `large_models.*`: the step vocabulary, the forecast query and the caps.
use ms_local_ai::large_models::{LIMITS_EDIT_DEBOUNCE, LargeModelStep, ProfilesRequest, RamStep, StepChoice, VramStep, format_gib};
// The axes widget the large-model cleaning engines share (FLUX.2 and FLUX.3 klein).
use crate::tools::ai_editor::large_model_resources::{ResourcesAction, ResourcesController, ResourcesView, draw_encoder_formats, draw_large_model_resources, forecast_line};
// The model-agnostic parts the large-model engines share: streamed progress, the Hugging Face
// download, the settings-file rules and the mask -> working-mode rule.
use super::shared::*;
use ms_tab_translation::machine_translation::{MtService, translate_texts_via_translator};
use ms_tab_translation::panels::machine_translation::{MT_SOURCE_LANGUAGES, MtLanguage};
use ms_widgets::{SeedSpinBox, WheelComboBox, WheelSlider};
use eframe::egui;
use egui::Color32;
// The wire PNG encoders, shared by `wire.rs` (through its `use super::*`) and the tests.
use crate::tools::region_png::{encode_color_image_png_rgba, encode_mask_png_l8};
use ms_thread as thread;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use web_time::{Duration, Instant};

// The picker caption of a `Flux2Variant` — the enum's presentation half, which an extension
// trait supplies because the enum itself is declared in `ms-config`.
mod variant_presentation;
use variant_presentation::*;
// Persisted parameters, the wire vocabulary they are spelled in, and their file IO.
mod settings;
use settings::*;
// The `.status` component catalog and every verdict derived from it.
mod status;
use status::*;
// The saved `.msprompt` library, its gates and the `.prompt_cache.*` wire block.
mod prompt_cache;
use prompt_cache::*;
// What one run answers with: the region, the written-back memory flag, a degraded run.
mod run_outcome;
use run_outcome::*;
// The run channel, the raw-result hand-off and the file pickers.
mod session;
use session::*;
// The run, status and component-action calls, and the blob encoding a run carries.
mod wire;
use wire::*;
// The engine itself: its state, its `AiEngine` contract and its long operations.
mod engine;
// The one item this module exports; the module path is unchanged by the split.
pub use engine::Flux2KleinEngine;
// The parameter panel: its body, its blocks and everything they draw.
mod ui;
use ui::*;
// The pure line/verdict decisions the panel renders, and their tests.
mod decisions;
use decisions::*;
// The fixtures every `mod tests` in this directory is built from.
#[cfg(test)]
mod test_support;
#[cfg(test)]
use test_support::*;

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
/// Largest growth of the painted mask contour into the latent mask, pixels
/// (`mask_dilate_px`). The feather and colour-match ranges are `ms_region_postprocess`'s.
const FLUX2_DILATE_MAX: u32 = 64;

/// Horizontal room left for the «Сохранить кэш» button beside the name field, points.
/// The field takes whatever is left, so the button never wraps onto its own line.
const FLUX2_CACHE_NAME_BUTTON_RESERVE: f32 = 140.0;

/// Preview tint of the engine's single mask layer, i.e. of the edit-permission area.
///
/// Opaque on purpose: `MaskStack` scales the alpha itself so the mask stays translucent
/// over the artwork, and a tint that arrived already translucent would be darkened twice.
const FLUX2_MASK_TINT: Color32 = Color32::from_rgb(80, 200, 255);

/// The engine's name in log lines (a log token, never shown to the user).
const FLUX2_LOG_LABEL: &str = "FLUX.2 klein";

/// The adapter name this engine's forecast is asked under (`large_models.profiles`
/// `model`). A wire identifier, never localized; the backend answers with the registration
/// the request's dtype selects (`flux2_klein` / `flux2_klein.float16`).
const FLUX2_LARGE_MODEL: &str = "flux2_klein";

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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_vae_tiling_is_written_back_and_the_steps_never_are() {
        let mut settings = Flux2KleinSettings::default();
        assert!(settings.set_step_choice(StepChoice { vram: VramStep::Max, ram: RamStep::Keep }));
        let recovered = Flux2AppliedFlags { vae_tiling: Some(true) };
        assert!(apply_backend_flags(&mut settings, recovered));
        assert!(settings.vae_tiling);
        // Applying the same value twice owes no second save.
        assert!(!apply_backend_flags(&mut settings, recovered));
        // An answer without the flag changes nothing.
        assert!(!apply_backend_flags(&mut settings, Flux2AppliedFlags { vae_tiling: None }));
        assert_eq!(
            settings.step_choice(),
            StepChoice { vram: VramStep::Max, ram: RamStep::Keep },
            "the user's steps survive a recovered run: the backend learns a floor instead"
        );
        assert!(!settings.text_encoder_fp8, "fp8 is the user's alone, never written back");
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
        let (sent, whole_region) = mask_for_run(&painted);
        assert_eq!(sent, painted, "the painted layer travels byte for byte");
        assert!(!whole_region, "something is painted, so only that may change");
        // The bytes that actually reach the wire are checked, not the intention.
        let png = encode_mask_png_l8(&sent, 64, 64).expect("encode the painted mask");
        let decoded = image::load_from_memory(&png).expect("decode").to_luma8();
        assert_eq!(decoded.dimensions(), (64, 64));
        assert_eq!(decoded.get_pixel(0, 0).0[0], 0, "outside the band nothing may change");
        assert_eq!(decoded.get_pixel(10, 10).0[0], 255, "inside the band the model may paint");
    }

    #[test]
    fn the_default_prompt_fills_a_new_and_a_blank_settings_file() {
        // A fresh install: the field is usable the moment the tool opens, because an
        // empty prompt is the one value the run gate refuses.
        assert_eq!(Flux2KleinSettings::default().prompt, FLUX2_DEFAULT_PROMPT);
        assert!(!FLUX2_DEFAULT_PROMPT.trim().is_empty());

        // A settings file written before the prompt had a default: the key is ABSENT.
        let absent = loaded_settings(&json!({ "placement": "full_gpu", "steps": 4 }));
        assert_eq!(absent.prompt, FLUX2_DEFAULT_PROMPT);
        // A file whose prompt is present but blank — whitespace included — is the same
        // unusable state and gets the same substitution.
        for blank in ["", "   ", "\n\t "] {
            let migrated = loaded_settings(&json!({ "prompt": blank }));
            assert_eq!(
                migrated.prompt, FLUX2_DEFAULT_PROMPT,
                "a blank prompt ({blank:?}) must not survive the load"
            );
        }
        // A file that carries a real prompt keeps the user's own text, verbatim.
        let carried = loaded_settings(&json!({ "prompt": "a red balloon" }));
        assert_eq!(carried.prompt, "a red balloon");
        // And the default one passes the run gate it exists for.
        let settings = Flux2KleinSettings {
            text_encoder_path: "/models/qwen3".to_string(),
            transformer_path: "/models/flux2.safetensors".to_string(),
            vae_path: "/models/vae".to_string(),
            ..Flux2KleinSettings::default()
        };
        assert!(flux2_run_block_reason(&settings, None, None).is_none());
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

}
