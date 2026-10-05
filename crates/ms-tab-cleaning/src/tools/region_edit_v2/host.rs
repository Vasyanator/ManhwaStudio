/*
File: region_edit_v2/host.rs

Purpose:
The GENERIC host of the on-canvas region-editing framework: one `CleaningTool` that puts a
`RegionFrame` in front of a catalog of AI engines (`super::engine::AiEngine`). The host owns the
rectangle, the mask stack, the pending result and the apply path; an engine owns its
parameters, its settings file, its wire protocol and its worker threads. Everything that makes
one hosted tool differ from another — its id, its title, its egui id salts, its log tag and its
engine catalog — is a `HostSpec` the consumer tool declares (`../ai_editor/` is one).

Main responsibilities:
- own the frame and hand it the dock-panel rects and the page count every frame
- own the engine catalog, the selected engine, and the frame state that follows from it:
  mask layers (pushed on construction and on `select_engine`), and the size constraints and
  whether an empty mask is a legal run (both re-read every frame, `push_engine_rules`)
  (D15/D16)
- push the per-frame facts an engine may not cache: backend/Torch availability and the frame
  rectangle
- run one job: load the source region on a worker, hand it to the engine, poll it, and turn
  the answer into the frame's pending result or into a user-facing failure (D14)
- run mask generation into the selected mask layer through the same region load
- refuse a result whose size is not exactly the frame rect (D7), with a message and a log
- own the marks mode (how the marks layer reaches the engine), keep it inside the selected
  engine's `marks_support()` and package the marks into each run (`prepare_run_marks`)
- own the marks colour selector, and keep its eyedropper click from also painting

Key structures:
- `HostSpec`: what a consumer tool declares to get a host of its own
- `RegionEditHost`: the `CleaningTool` implementation
- `PendingLoad`: the region-load job in flight, and the rectangle it was started for
- `ApplyError`: why a pending result could not be merged into the clean overlay
- `CaptureError`: why the clean-overlay pixels under the frame could not be captured

Key functions:
- `RegionEditHost::new()`: builds the host for one `HostSpec`
- `draw_main_panel_top()` / `draw_main_panel()` / `draw_main_panel_bottom()`: the three
  sections of the «Редактор области» panel — engine progress (pinned), engine parameters
  (scrolled), host actions + engine run options (pinned)
- `run_options_ctx()`: the host facts handed to `AiEngine::draw_run_options`
- `start_run()`, `poll_region_load()`, `poll_engine()`: the three steps of one run
- `apply_result()`: the only `&mut CanvasView` action
- `select_engine()`: applies an engine's constraints and mask layers to the frame
- `capture_clean_overlay()`: the clean chunk that is composited over the page crop
- `check_result_fits()`: the D7 size check, pure and unit-tested
- `reconcile_marks_mode()`, `prepare_run_marks()`: the marks-mode rules, pure and unit-tested

Notes:
The `CleaningTool` panel entry points (`draw_ui`, `draw_main_panel` and its pinned
`draw_main_panel_top` / `draw_main_panel_bottom`) stay here as short dispatchers; the section
helpers they call (engine picker, brush, layers, mask generation and actions, the
`draw_host_actions` «Обработать» section) live in the sibling `host_panels.rs`, which is why the
fields and helpers those read are `pub(super)`: nothing else in the framework touches them. NOTHING here decodes an image on the GUI thread: the source region is produced by
the shared region loader worker of `base.rs` (`spawn_region_loader_thread`, reused rather than
copied, D10/D14), and the engine runs its own workers. `block_canvas_zoom()` stays `false` (D5):
that flag also disables the clean-overlay undo shortcuts for the whole session. Blocking is
precise instead — `captures_canvas_pointer` over the hitbox,
`block_canvas_drag_scroll_on_primary` only during a live gesture, and
`block_canvas_zoom_on_ctrl_primary` only while the frame wants Ctrl+drag for a marks rectangle.
Design and the decisions behind it: `dev-docs/region_edit_v2_plan.md` (§13).
*/

use super::engine::{
    AiEngine, EnginePoll, EngineRunRequest, MarksMode, MarksSupport, RunMarks, RunOptionsCtx, composite_marks_over,
};
use super::frame::{FrameHost, FrameLock, RegionFrame, page_source_size};
use super::geometry::FrameConstraints;
use super::layers::ResultLayer;
use crate::tools::base::{
    CleaningTool, RegionLoadRequest, RegionLoadResult, StrokePoint, capture_overlay_chunk, overlay_rect_to_scene_rect,
    spawn_region_loader_thread,
};
use crate::tools::mask_generation::{self, GeneratedMask, MaskGenerationPoll, MaskGenerationSpawner, MaskGenerationState, MaskSource};
use ms_canvas::{CanvasView, OverlayRectPx};
use ms_project::ProjectData;
use ms_widgets::{ColorPresets, PresetDefaults, ViewportColorSelector};
use eframe::egui;
use egui::Pos2;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;

/// Size requirements a frame carries while NO engine could be selected.
///
/// Reachable only when the spec's catalog returns no engine, which is a build mistake rather
/// than a runtime state — the constructor logs it. The values impose nothing, so the frame
/// stays grey and «Обработать» is refused for want of an engine, not for its size.
const NO_ENGINE_CONSTRAINTS: FrameConstraints = FrameConstraints::UNCONSTRAINED;

/// What a consumer tool declares to get a `RegionEditHost` of its own.
///
/// Exactly the parts of a hosted tool that differ between tools; everything else — the frame,
/// the run and mask-generation pipelines, the apply path and both panel bodies — is the host's
/// and is shared. A consumer keeps one `static` spec and builds its tool with
/// [`RegionEditHost::new`].
#[derive(Debug)]
pub(in crate::tools) struct HostSpec {
    /// `CleaningTool::tool_id`: stable, never shown to the user.
    pub tool_id: &'static str,
    /// `CleaningTool::title`. A function rather than a key because `t!` takes only a literal
    /// key, so the consumer's function is where that literal lives.
    pub title: fn() -> &'static str,
    /// Prefix of every log line this host writes, e.g. `"[cleaning/ai_editor]"`.
    pub log_tag: &'static str,
    /// `id_salt` of the mask-generation parameter section and of its source picker.
    ///
    /// Both captions are localized, so egui would otherwise derive their persistent ids from
    /// the translated text and lose the folded state and the open popup on a language switch
    /// (`egui-docs/05-ids-and-i18n.md` §2). One host instance draws each control once, so a
    /// constant per tool is enough — and it must differ BETWEEN tools, or two hosted tools
    /// would share one folded state. Changing a tool's value loses its stored state.
    pub mask_generation_section_salt: &'static str,
    pub mask_source_picker_salt: &'static str,
    /// Builds the tool's engines, in picker order; the engine at index 0 is selected on
    /// creation. Called once per host: engines hold live channels and worker state.
    pub catalog: fn() -> Vec<Box<dyn AiEngine>>,
}

/// Why a pending result could not be merged into the clean overlay.
///
/// Both variants exist because `CanvasView::replace_overlay_region_px` would otherwise
/// SUCCEED at something wrong: it nearest-rescales a chunk of the wrong size into the target
/// and clips a target that leaves the overlay, in both cases overwriting alpha wholesale
/// (D7). The tool refuses instead of letting either happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum ApplyError {
    /// The result does not have exactly the frame's size.
    #[error("the result is {result_w}x{result_h}, the frame region is {region_w}x{region_h}")]
    SizeMismatch {
        result_w: usize,
        result_h: usize,
        region_w: usize,
        region_h: usize,
    },
    /// The frame rectangle does not lie inside the page's clean overlay.
    #[error("the region {x};{y} {w}x{h} does not fit the {overlay_w}x{overlay_h} overlay")]
    OutOfBounds {
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        overlay_w: usize,
        overlay_h: usize,
    },
}

/// Whether `result_size` may be written into `rect` of an overlay of `overlay_size`.
///
/// `overlay_size` is `None` while the page has no clean overlay allocated yet; the write then
/// creates one at the page size and the bounds check has nothing to compare against, so only
/// the size equality is enforced.
///
/// # Errors
/// [`ApplyError::SizeMismatch`] when the result is not exactly the region size, and
/// [`ApplyError::OutOfBounds`] when the region leaves the existing overlay.
fn check_result_fits(
    result_size: [usize; 2],
    rect: OverlayRectPx,
    overlay_size: Option<[usize; 2]>,
) -> Result<(), ApplyError> {
    if result_size != [rect.w, rect.h] {
        return Err(ApplyError::SizeMismatch {
            result_w: result_size[0],
            result_h: result_size[1],
            region_w: rect.w,
            region_h: rect.h,
        });
    }
    if let Some([overlay_w, overlay_h]) = overlay_size
        && (rect.x.saturating_add(rect.w) > overlay_w || rect.y.saturating_add(rect.h) > overlay_h)
    {
        return Err(ApplyError::OutOfBounds {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            overlay_w,
            overlay_h,
        });
    }
    Ok(())
}

/// Why the clean-overlay pixels under the frame could not be captured.
///
/// A capture failure is a real error, never a silent fallback: a page whose clean overlay is
/// not allocated at all simply HAS no clean pixels, and the loader composites nothing over the
/// page crop for it. Every other outcome would mean handing the engine a region that does not
/// show what the user is looking at, unnoticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum CaptureError {
    /// The page is not laid out, or the region does not map onto its overlay.
    #[error("page {page} is not laid out, or the region {x};{y} {w}x{h} does not map onto its overlay")]
    NotMapped {
        page: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    },
    /// The captured chunk does not cover the region exactly.
    #[error("page {page}: the captured chunk is {chunk_w}x{chunk_h}, the region {x};{y} is {w}x{h}")]
    ChunkSize {
        page: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        chunk_w: usize,
        chunk_h: usize,
    },
}

/// The marks mode to use under `support`: `current` while the engine supports it, otherwise
/// the engine's preferred mode.
///
/// The per-frame half of the auto-switch rule (`select_engine` sets the preferred mode
/// outright): the user's choice stands as long as it stays legal, and an engine that stops
/// accepting it — a different API model picked in its own panel — moves it to its preference
/// rather than leaving a mode the run cannot honour.
#[must_use]
fn reconcile_marks_mode(current: MarksMode, support: MarksSupport) -> MarksMode {
    if support.supports(current) { current } else { support.preferred() }
}

/// Why the marks could not be packaged for a run. Each is a broken internal invariant (the
/// marks layer always has the frame's size), reported rather than papered over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum MarksError {
    /// The marks layer does not hold `4 * w * h` bytes for the region.
    #[error("the marks layer holds {len} bytes, a {w}x{h} region needs {expected}")]
    Shape { len: usize, w: usize, h: usize, expected: usize },
}

/// Packages the marks for one run: the region the engine edits and what travels beside it.
///
/// - nothing drawn (`marks_empty`): the region unchanged and `RunMarks::None`, whatever `mode`;
/// - `OverlayOnRegion`: the marks composited onto the region ("over", straight alpha), and
///   `RunMarks::None`;
/// - `SeparateReference`: the region unchanged, and a composited copy as `RunMarks::Reference`;
/// - `TransparentLayer`: the region unchanged, and the marks alone as `RunMarks::Layer`.
///
/// `mode` must already be one the engine supports (`reconcile_marks_mode`). `marks_rgba` is the
/// frame's straight-alpha marks layer, 4 bytes per region pixel.
///
/// # Errors
/// [`MarksError::Shape`] when `marks_rgba` is not exactly the region's size, so a layer of
/// another shape is never stretched over the picture.
fn prepare_run_marks(mode: MarksMode, region: egui::ColorImage, marks_rgba: &[u8], marks_empty: bool) -> Result<(egui::ColorImage, RunMarks), MarksError> {
    if marks_empty {
        return Ok((region, RunMarks::None));
    }
    let [w, h] = region.size;
    let shape = MarksError::Shape { len: marks_rgba.len(), w, h, expected: w.saturating_mul(h).saturating_mul(4) };
    match mode {
        MarksMode::OverlayOnRegion => {
            let composited = composite_marks_over(&region, marks_rgba).ok_or(shape)?;
            Ok((composited, RunMarks::None))
        }
        MarksMode::SeparateReference => {
            let reference = composite_marks_over(&region, marks_rgba).ok_or(shape)?;
            Ok((region, RunMarks::Reference(reference)))
        }
        MarksMode::TransparentLayer => {
            let (Ok(w32), Ok(h32)) = (u32::try_from(w), u32::try_from(h)) else {
                return Err(shape);
            };
            let layer = image::RgbaImage::from_raw(w32, h32, marks_rgba.to_vec())
                .filter(|layer| layer.as_raw().len() == marks_rgba.len())
                .ok_or(shape)?;
            Ok((region, RunMarks::Layer(layer)))
        }
    }
}

/// What a run that composited the marks INTO its region keeps, so the marks can be taken back
/// out of the engine's answer (`restore_untouched_marked_pixels`).
#[derive(Debug)]
struct MarksRestore {
    /// The region as loaded, without the marks.
    original: egui::ColorImage,
    /// The region exactly as the engine received it, marks composited.
    sent: egui::ColorImage,
}

/// Takes the user's marks back out of an engine's answer: every pixel the marks changed in the
/// SENT region (`sent != original`) that the engine returned UNTOUCHED (`result == sent`) is
/// restored to the original, unmarked pixel. Returns how many pixels were restored.
///
/// Marks are instructions, never content. An inpainting engine composites its output only
/// inside the mask and hands every other pixel back as it got it, so without this a mark drawn
/// outside the mask would be merged into the clean overlay by «Применить». A pixel the engine
/// CHANGED is kept: that is the engine's answer, whatever it replaced. A mark whose colour
/// equals the pixel under it changed nothing, so testing `sent != original` is the same as
/// testing the mark's alpha, without keeping a copy of the marks layer.
///
/// Changes nothing and returns `0` when the three images are not the same size — the caller
/// has already refused a result of the wrong size, so this is a guard, not a path.
fn restore_untouched_marked_pixels(result: &mut egui::ColorImage, original: &egui::ColorImage, sent: &egui::ColorImage) -> usize {
    if result.size != original.size || result.size != sent.size {
        return 0;
    }
    let mut restored = 0usize;
    for ((out, before), sent) in result.pixels.iter_mut().zip(&original.pixels).zip(&sent.pixels) {
        if sent != before && out == sent {
            *out = *before;
            restored += 1;
        }
    }
    restored
}

/// The fixed colour set the marks colour picker offers. Not persisted in this version: the
/// shipped palette is already a set of clearly distinct colours, which is what marks need.
#[must_use]
fn marks_color_presets() -> ColorPresets {
    ColorPresets::from_defaults(PresetDefaults::Palette)
}

/// A line the tool shows under its main panel's status line.
#[derive(Debug, Clone)]
pub(super) struct ToolMessage {
    /// Already localized text. Stored resolved because it carries run-time numbers; it is
    /// replaced on the next action, so a language switch cannot leave it stale for long.
    pub(super) text: String,
    /// Whether it reports a failure, which is the only thing that decides its colour.
    pub(super) error: bool,
}

/// What a finished region load is FOR.
///
/// Both consumers need exactly the same pixels — the page crop composited with the clean
/// overlay — and the loader is single-slot, so the purpose travels with the job instead of
/// being guessed from the tool's state when the answer lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadPurpose {
    /// The region goes to the selected engine (`hand_region_to_engine`).
    Run,
    /// The region goes to a backend mask detector (`start_mask_detection`).
    MaskGeneration,
}

/// The region-load job in flight, and what it was started for.
///
/// The rectangle is captured HERE rather than re-read from the frame when the load lands: the
/// frame is locked for the whole run, so the two cannot diverge, and carrying it makes the
/// size validation of the arriving region a comparison against the job's own contract.
#[derive(Debug, Clone, Copy)]
pub(super) struct PendingLoad {
    job_id: u64,
    page_idx: usize,
    rect: OverlayRectPx,
    purpose: LoadPurpose,
}

/// A hosted region-editing tool: an on-canvas `RegionFrame` in front of the engine catalog of
/// one [`HostSpec`].
///
/// The fields the panel bodies read are `pub(super)` because those bodies live in the sibling
/// `host_panels.rs`; no other framework file touches them.
pub struct RegionEditHost {
    /// What the consumer tool declared: its id, title, id salts, log tag and catalog.
    pub(super) spec: &'static HostSpec,
    pub(super) frame: RegionFrame,
    /// Every hosted engine, built once at construction: they hold live channels and worker
    /// state, so they are kept for the session rather than rebuilt per frame.
    pub(super) engines: Vec<Box<dyn AiEngine>>,
    /// Index into `engines` of the engine the panels and the run path address. Out of range
    /// only for an empty catalog, which every accessor treats as "no engine".
    pub(super) selected: usize,
    /// Dock-panel rects of THIS frame, handed over by the tab before `draw_overlay_ui`. They
    /// are cut out of the viewport so the frame never hides behind a panel.
    panel_rects: Vec<egui::Rect>,
    pub(super) message: Option<ToolMessage>,
    /// Last availability the tab pushed, forwarded to the selected engine every frame.
    pub(super) backend_available: bool,
    pub(super) torch_available: bool,
    /// The shared region loader (`base.rs`), reused rather than copied (D10/D14): it decodes
    /// the page off the GUI thread and composites the clean overlay over the crop.
    load_tx: Sender<Option<RegionLoadRequest>>,
    load_rx: Receiver<RegionLoadResult>,
    load_thread: Option<JoinHandle<()>>,
    next_job_id: u64,
    /// `Some` between «Обработать» and the moment the region reaches the engine. Clearing it
    /// abandons the job: the answer is then dropped on its job id.
    pub(super) pending_load: Option<PendingLoad>,
    /// Mask-generation parameters and the watermark catalog state, shared with the classic
    /// mask-inpaint editors through `crate::tools::mask_generation` — the sources, their
    /// requirement rules and the detection call have exactly one implementation.
    pub(super) mask_generation: MaskGenerationState,
    /// How a detection is started. Always `mask_generation::spawn_mask_generation` in the
    /// product; the field exists so this host's unit tests can drive `start_mask_detection`
    /// itself without the real detector, which performs a backend round trip and, for the
    /// Torch sources, a model download into the runtime data root — neither belongs in a unit
    /// test. A test must opt OUT explicitly; the default is always the real spawner.
    ///
    /// Injection rather than a `#[cfg(test)]` override inside `mask_generation.rs`: an override
    /// there would have to be reconfigurable, i.e. mutable process- or thread-global state,
    /// which this project forbids (`CLAUDE.md` §5), and it would be invisible at the call site.
    /// This field is per-instance, needs no crate feature — `ms-tab-cleaning` has none — and
    /// leaves `start_mask_detection` with ONE code path, so the test drives exactly what ships.
    spawn_detection: MaskGenerationSpawner,
    /// `Some` while a detector is running on a worker. The frame is `Processing` for as long,
    /// so its rectangle and its mask cannot move under the job.
    pub(super) mask_generation_rx: Option<Receiver<Result<GeneratedMask, String>>>,
    /// Raised by «Сгенерировать маску» in the compact panel and consumed by the next
    /// `draw_overlay_ui`. A panel body may mutate only the tool, and starting the job needs
    /// the canvas and the project — the same reason «Обработать» goes through the frame.
    pub(super) generate_mask_requested: bool,
    /// How the marks layer reaches the selected engine. Always one the engine supports: set to
    /// its preferred mode on `select_engine`, kept legal every frame by `push_engine_rules`.
    pub(super) marks_mode: MarksMode,
    /// The marks colour widget of the compact panel; it owns the eyedropper state.
    pub(super) marks_color_selector: ViewportColorSelector,
    /// The colour cells the marks picker offers (`marks_color_presets`). Edits live for the
    /// session only.
    pub(super) marks_presets: ColorPresets,
    /// Whether the compact panel drew the marks colour selector in THIS frame. Painting is
    /// suppressed for the eyedropper only then: a selector that is not drawn cannot sample or
    /// end its sampling, and must not keep the brush disabled from behind a closed panel.
    pub(super) marks_selector_drawn: bool,
    /// `Some` while a run that composited the marks into its region is in flight: what
    /// `accept_result` needs to take them back out of the answer. Dropped with the run.
    marks_restore: Option<MarksRestore>,
}

impl std::fmt::Debug for RegionEditHost {
    /// Hand-written: `Box<dyn AiEngine>` is not `Debug`, and an engine's state is pixel and
    /// channel data that would drown the frame's own fields anyway.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegionEditHost")
            .field("tool_id", &self.spec.tool_id)
            .field("frame", &self.frame)
            .field("engines", &self.engines.iter().map(|engine| engine.id()).collect::<Vec<_>>())
            .field("selected", &self.selected)
            .field("pending_load", &self.pending_load)
            .field("generating_mask", &self.mask_generation_rx.is_some())
            .finish()
    }
}

impl RegionEditHost {
    /// Builds the host of the tool `spec` describes: its engines, a frame shaped by the first of
    /// them, and its own region-loader worker (joined on drop).
    ///
    /// An empty catalog is logged and yields a host that can paint a mask but run nothing.
    #[must_use]
    pub(in crate::tools) fn new(spec: &'static HostSpec) -> Self {
        let engines = (spec.catalog)();
        let (constraints, layers, marks_mode) = match engines.first() {
            Some(engine) => (engine.constraints(), engine.mask_layers(), engine.marks_support().preferred()),
            None => {
                ms_log::runtime_log::log_error(format!(
                    "{} the engine catalog is empty: the tool can paint a mask but can run nothing",
                    spec.log_tag
                ));
                (NO_ENGINE_CONSTRAINTS, Vec::new(), MarksSupport::OVERLAY_ONLY.preferred())
            }
        };
        let (load_tx, load_rx, load_thread) = spawn_region_loader_thread();
        Self {
            spec,
            frame: RegionFrame::new(constraints, &layers),
            engines,
            selected: 0,
            panel_rects: Vec::new(),
            message: None,
            backend_available: false,
            torch_available: false,
            load_tx,
            load_rx,
            load_thread: Some(load_thread),
            next_job_id: 1,
            pending_load: None,
            mask_generation: MaskGenerationState::default(),
            spawn_detection: mask_generation::spawn_mask_generation,
            mask_generation_rx: None,
            generate_mask_requested: false,
            marks_mode,
            marks_color_selector: ViewportColorSelector::default(),
            marks_presets: marks_color_presets(),
            marks_selector_drawn: false,
            marks_restore: None,
        }
    }
}

impl Drop for RegionEditHost {
    /// Stops the region loader worker. Without it the thread outlives the tool and holds the
    /// last decoded page — tens of megabytes — for the rest of the session.
    fn drop(&mut self) {
        // The worker is already gone if the send fails; there is nothing to report and nothing
        // to recover, so the error is deliberately discarded here and only here.
        let _ = self.load_tx.send(None);
        if let Some(thread) = self.load_thread.take() {
            let _ = thread.join();
        }
    }
}

impl RegionEditHost {
    /// The selected engine, or `None` when the catalog is empty.
    #[must_use]
    pub(super) fn engine(&self) -> Option<&dyn AiEngine> {
        self.engines.get(self.selected).map(AsRef::as_ref)
    }

    /// The selected engine, mutably. `None` when the catalog is empty.
    fn engine_mut(&mut self) -> Option<&mut (dyn AiEngine + 'static)> {
        self.engines.get_mut(self.selected).map(AsMut::as_mut)
    }

    /// The host facts the engine's pinned run options depend on this frame.
    ///
    /// `mask_painted` is the MASK stack's answer, never the marks': it is exactly the question
    /// «Обработать» and an engine's working mode ask (FLUX.2 klein's whole-region mode is "no
    /// mask pixel painted"), and the marks never act as a permission mask.
    pub(super) fn run_options_ctx(&self) -> RunOptionsCtx {
        RunOptionsCtx { mask_painted: !self.frame.masks().is_empty() }
    }

    /// Why the selected engine says a switch is unsafe right now; `None` when it is free.
    ///
    /// A SECOND gate beside the frame lock, and an independent one: the lock covers a run,
    /// this covers work an engine owns that no frame state describes — FLUX.2 klein's model
    /// download, whose free-space budget is its own and would be doubled by starting the
    /// other checkpoint's download beside it. Both the picker and [`Self::select_engine`]
    /// read it, so closing the control and refusing the switch cannot drift apart.
    #[must_use]
    pub(super) fn switch_block_reason(&self) -> Option<String> {
        self.engine().and_then(AiEngine::switch_block_reason)
    }

    /// Whether the mask may still be edited: no result waits and nothing is running.
    ///
    /// Editing the mask under a pending result would make that result describe a mask that no
    /// longer exists, which is the same rule the frame applies to painting.
    #[must_use]
    pub(super) fn mask_editable(&self) -> bool {
        match self.frame.lock() {
            FrameLock::Free | FrameLock::MaskPainted => true,
            FrameLock::ResultPending | FrameLock::Processing => false,
        }
    }

    /// Records a message the main panel shows as ordinary text.
    pub(super) fn report_info(&mut self, text: String) {
        self.message = Some(ToolMessage { text, error: false });
    }

    /// Records a failure: `text` is what the user reads, `detail` is the technical
    /// reason that goes to the log alone.
    ///
    /// The two are separate on purpose. `detail` is an English `Display` of a typed error and
    /// must not leak into a message shown under a French interface, while the user-facing
    /// half must not carry indices and buffer lengths nobody outside the code can act on.
    fn report_error(&mut self, text: String, detail: &dyn std::fmt::Display) {
        ms_log::runtime_log::log_warn(format!("{} {text} | {detail}", self.spec.log_tag));
        self.message = Some(ToolMessage { text, error: true });
    }

    /// Selects engine `idx` and re-shapes the frame around it (D15/D16).
    ///
    /// Refused while the frame is not free: engines declare different mask layers, so the
    /// switch would discard painted work. The picker is disabled in that state, and this guard
    /// is the same rule stated where it is enforceable.
    ///
    /// The new constraints are applied WITHOUT resizing the frame: a rectangle the new engine
    /// refuses turns the frame red and blocks «Обработать», which is the designed way for the
    /// user to see that this engine wants a different size.
    pub(super) fn select_engine(&mut self, idx: usize) {
        if idx == self.selected || idx >= self.engines.len() || !self.frame.lock().is_free() {
            return;
        }
        // The engine's own refusal, stated where it is enforceable for the same reason the
        // frame lock is: the picker is already closed while it stands, but a switch must not
        // depend on a control having been drawn this frame.
        if self.switch_block_reason().is_some() {
            return;
        }
        // A run cannot be in flight (the frame would be locked), but a load job might have been
        // abandoned by a cancel that has not been polled yet; dropping it here keeps its answer
        // from reaching the newly selected engine.
        self.pending_load = None;
        self.selected = idx;
        self.message = None;
        let Some(engine) = self.engines.get(idx) else {
            return;
        };
        let constraints = engine.constraints();
        let layers = engine.mask_layers();
        let allows_empty = engine.allows_empty_mask();
        // A switch always starts from the new engine's own preference; the user's choice
        // under the previous engine says nothing about what this one handles best.
        self.marks_mode = engine.marks_support().preferred();
        self.frame.set_constraints(constraints);
        self.frame.set_mask_layers(&layers);
        self.frame.set_allows_empty_mask(allows_empty);
    }

    /// The marks modes the selected engine accepts; the default set with no engine.
    #[must_use]
    pub(super) fn marks_support(&self) -> MarksSupport {
        self.engine().map_or(MarksSupport::OVERLAY_ONLY, AiEngine::marks_support)
    }

    /// Re-reads the answers of the selected engine that may change with its own parameters —
    /// `allows_empty_mask()` and `constraints()` into the frame, `marks_support()` into the
    /// marks mode — and pushes the eyedropper's claim on the next click. Pass step 1, every frame.
    ///
    /// `set_constraints` re-validates and never resizes, so pushing an unchanged declaration
    /// every frame is free and a changed one turns a refused rectangle red. Mask layers are
    /// deliberately NOT re-pushed: `set_mask_layers` re-creates the stack and refuses on a
    /// locked frame, so it stays a `select_engine`-time push.
    fn push_engine_rules(&mut self) {
        let allows_empty = self.engine().is_some_and(AiEngine::allows_empty_mask);
        self.frame.set_allows_empty_mask(allows_empty);
        let constraints = self.engine().map_or(NO_ENGINE_CONSTRAINTS, AiEngine::constraints);
        self.frame.set_constraints(constraints);
        let support = self.marks_support();
        let mode = reconcile_marks_mode(self.marks_mode, support);
        if mode != self.marks_mode {
            ms_log::runtime_log::log_info(format!(
                "{} the engine no longer accepts marks as {:?}; switched to its preferred {:?}",
                self.spec.log_tag, self.marks_mode, mode
            ));
            self.marks_mode = mode;
        }
        // The panel body ran earlier this frame; only a selector it actually drew can hold the
        // click, and the flag is consumed so a closed panel releases the brush next frame.
        let drawn = std::mem::take(&mut self.marks_selector_drawn);
        if !drawn {
            // A selector that was not drawn (mask mode, a hidden panel) cannot finish a sampling;
            // left pending it would eat the first click after it reappears.
            self.cancel_marks_eyedropper();
        }
        let selector = &self.marks_color_selector;
        self.frame.set_painting_suppressed(drawn && (selector.eyedropper_active() || selector.primary_click_consumed_this_frame()));
    }

    /// Ends a pending marks-colour sampling, as Escape would, and puts back the colour the
    /// sampling previews overwrote. A no-op when nothing is pending.
    pub(super) fn cancel_marks_eyedropper(&mut self) {
        if let Some(color) = self.marks_color_selector.cancel_eyedropper() {
            self.frame.set_marks_color(color);
        }
    }

    /// The clean-overlay pixels under `rect` of page `page_idx`, or `None` when the page has
    /// no clean overlay allocated at all.
    ///
    /// `None` is the page's TRUE clean state, not a stand-in: the loader then composites
    /// nothing over the page crop, which is exactly right. Once an overlay EXISTS, a capture
    /// that fails or comes back the wrong size is an error — handing the engine the bare page
    /// there would hide every clean edit the user already made.
    ///
    /// # Errors
    /// [`CaptureError::NotMapped`] when the page is not laid out or the region does not map
    /// onto its overlay, and [`CaptureError::ChunkSize`] when the captured chunk is not exactly
    /// the region size.
    fn capture_clean_overlay(
        canvas: &CanvasView,
        page_idx: usize,
        rect: OverlayRectPx,
    ) -> Result<Option<egui::ColorImage>, CaptureError> {
        let not_mapped = CaptureError::NotMapped {
            page: page_idx,
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
        };
        let Some([overlay_w, overlay_h]) = canvas.overlay_size(page_idx) else {
            return Ok(None);
        };
        let page_rect = canvas.page_scene_rect(page_idx).ok_or(not_mapped)?;
        let scene_rect = overlay_rect_to_scene_rect(page_rect, overlay_w, overlay_h, rect).ok_or(not_mapped)?;
        let chunk = capture_overlay_chunk(canvas, page_idx, scene_rect).ok_or(not_mapped)?;
        if chunk.size != [rect.w, rect.h] {
            return Err(CaptureError::ChunkSize {
                page: page_idx,
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: rect.h,
                chunk_w: chunk.size[0],
                chunk_h: chunk.size[1],
            });
        }
        Ok(Some(chunk))
    }

    /// Step 1 of a run: refuses what the selected engine refuses, then queues the region load
    /// (D14). The load mechanics themselves are [`Self::request_region`], shared with mask
    /// generation.
    fn start_run(&mut self, canvas: &CanvasView, project: &ProjectData) {
        let Some(engine) = self.engine() else {
            self.report_error(
                t!("cleaning.tools.area_editor.error_no_engine").to_string(),
                &"the engine catalog is empty",
            );
            return;
        };
        if let Some(reason) = engine.run_block_reason() {
            let detail = format!("engine {} refused the run", engine.id());
            self.report_error(reason, &detail);
            return;
        }
        self.request_region(canvas, project, LoadPurpose::Run);
    }

    /// Step 1 of a mask generation: asks the loader worker for the same source region a run
    /// would get, so the detector sees the page WITH the user's clean edits (D14).
    ///
    /// Every refusal the button already encodes is restated here, where it is enforceable: a
    /// panel must not be able to start a job by having been drawn one frame late. Nothing is
    /// decoded or encoded on the GUI thread — the load is a worker and so is the detection.
    fn start_mask_generation(&mut self, canvas: &CanvasView, project: &ProjectData) {
        if let Some((text, detail)) = self.mask_generation_block_reason() {
            self.report_error(text, &detail);
            return;
        }
        self.request_region(canvas, project, LoadPurpose::MaskGeneration);
    }

    /// Why «Сгенерировать маску» must refuse right now: `(the sentence the user reads, the
    /// technical detail for the log)`, or `None` while a generation may start.
    ///
    /// ONE rule, read by both the control and the action, so a greyed-out button and a refused
    /// click can never disagree — the same pairing `switch_block_reason` gives the engine
    /// picker. The order runs most-specific first: an unplaced frame, then a job already in
    /// flight, then a frame holding work, then the source's own unavailability.
    #[must_use]
    pub(super) fn mask_generation_block_reason(&self) -> Option<(String, String)> {
        if !self.frame.is_placed() {
            return Some((
                t!("cleaning.tools.area_editor.error_no_frame").to_string(),
                "the frame has no page anchor or no rectangle".to_string(),
            ));
        }
        if self.mask_generation_rx.is_some() || self.pending_load.is_some() {
            return Some((
                t!("cleaning.mask_editor.background_op_running_status").to_string(),
                "a region load or a detection is already in flight".to_string(),
            ));
        }
        if !self.mask_editable() {
            return Some((
                t!("cleaning.tools.area_editor.generate_mask_locked_hint").to_string(),
                "a result waits or the engine is running".to_string(),
            ));
        }
        let source = self.mask_generation.params.source;
        if !source.is_available(self.backend_available, self.torch_available) {
            return Some((
                mask_generation::generate_button_hover_text(source, self.backend_available, self.torch_available),
                format!("mask source {source:?} is unavailable (backend: {}, torch: {})", self.backend_available, self.torch_available),
            ));
        }
        None
    }

    /// Queues one region load for `purpose` and locks the frame for its duration.
    ///
    /// Shared by the run and by mask generation because both need the identical pixels and the
    /// identical guarantees: the frame is marked as processing immediately, so the rectangle the
    /// answer is validated against cannot move under the job. Nothing is decoded here — the GUI
    /// thread only captures the clean-overlay chunk it already has in memory.
    fn request_region(&mut self, canvas: &CanvasView, project: &ProjectData, purpose: LoadPurpose) {
        let (Some(page_idx), Some(rect)) = (self.frame.page_idx(), self.frame.rect_px()) else {
            self.report_error(
                t!("cleaning.tools.area_editor.error_no_frame").to_string(),
                &"the frame has no page anchor or no rectangle",
            );
            return;
        };
        let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) else {
            self.report_error(
                tf!("cleaning.tools.area_editor.error_page_missing", page = page_idx + 1),
                &format!("page {page_idx} is not in the project"),
            );
            return;
        };
        let page_path = page.path.clone();
        let Some(source_size) = page_source_size(canvas, page_idx, canvas.zoom()) else {
            self.report_error(
                tf!("cleaning.tools.area_editor.error_page_missing", page = page_idx + 1),
                &format!("page {page_idx} is not laid out, so its source size is unknown"),
            );
            return;
        };
        let overlay_chunk = match Self::capture_clean_overlay(canvas, page_idx, rect) {
            Ok(chunk) => chunk,
            Err(error) => {
                self.report_error(tf!("cleaning.tools.area_editor.error_no_overlay", page = page_idx + 1), &error);
                return;
            }
        };

        let job_id = self.next_job_id;
        self.next_job_id = self.next_job_id.saturating_add(1);
        let request = RegionLoadRequest {
            job_id,
            page_idx,
            source_rect: rect,
            source_size,
            page_path,
            overlay_chunk,
            shared_overlays_model: canvas.clean_overlays_model_handle(),
        };
        if self.load_tx.send(Some(request)).is_err() {
            self.report_error(
                t!("cleaning.tools.area_editor.error_region_load").to_string(),
                &"the region loader worker is gone",
            );
            return;
        }
        self.pending_load = Some(PendingLoad { job_id, page_idx, rect, purpose });
        self.frame.set_processing(true);
        self.report_info(t!("cleaning.tools.area_editor.loading_region_status").to_string());
    }

    /// Step 2: drains the loader and hands a finished region to the engine.
    ///
    /// A result whose job id is not the awaited one belongs to an abandoned run and is dropped.
    /// A dead worker is reported once, against the job it stranded, rather than leaving the
    /// frame locked on a run that can never finish.
    fn poll_region_load(&mut self) {
        loop {
            match self.load_rx.try_recv() {
                Ok(result) => {
                    if self.pending_load.map(|pending| pending.job_id) != Some(result.job_id) {
                        continue;
                    }
                    let Some(pending) = self.pending_load.take() else {
                        continue;
                    };
                    match result.image {
                        Ok(region) => match pending.purpose {
                            LoadPurpose::Run => self.hand_region_to_engine(pending, region),
                            LoadPurpose::MaskGeneration => self.start_mask_detection(pending, region),
                        },
                        Err(error) => {
                            self.frame.set_processing(false);
                            self.report_error(t!("cleaning.tools.area_editor.error_region_load").to_string(), &error);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.pending_load.take().is_some() {
                        self.frame.set_processing(false);
                        self.report_error(
                            t!("cleaning.tools.area_editor.error_region_load").to_string(),
                            &"the region loader worker died with a job in flight",
                        );
                    }
                    break;
                }
            }
        }
    }

    /// Validates a loaded region against the job's rectangle and starts the engine run.
    ///
    /// Every size the trait promises an engine is checked HERE, because the loader derives its
    /// crop from the decoded FILE and a page file whose dimensions disagree with the overlay
    /// yields a region of a different size. Handing that on would make the engine refuse a
    /// request the host was supposed to guarantee.
    fn hand_region_to_engine(&mut self, pending: PendingLoad, region: egui::ColorImage) {
        let rect = pending.rect;
        let expected = [rect.w, rect.h];
        if region.size != expected {
            self.frame.set_processing(false);
            self.report_error(
                t!("cleaning.tools.area_editor.error_region_size").to_string(),
                &format!(
                    "the loaded region is {}x{}, the frame region is {}x{}",
                    region.size[0], region.size[1], rect.w, rect.h
                ),
            );
            return;
        }
        let masks = self.frame.masks();
        let mask_bytes = rect.w.saturating_mul(rect.h);
        if masks.size() != (rect.w, rect.h) {
            let (mask_w, mask_h) = masks.size();
            self.frame.set_processing(false);
            self.report_error(
                t!("cleaning.tools.area_editor.error_region_size").to_string(),
                &format!("the mask stack is {mask_w}x{mask_h}, the frame region is {}x{}", rect.w, rect.h),
            );
            return;
        }
        let masks: Vec<Vec<u8>> = (0..masks.layer_count()).map(|idx| masks.bytes(idx).to_vec()).collect();
        if masks.iter().any(|layer| layer.len() != mask_bytes) {
            self.frame.set_processing(false);
            self.report_error(
                t!("cleaning.tools.area_editor.error_region_size").to_string(),
                &format!("a mask layer does not hold {mask_bytes} bytes: {:?}", masks.iter().map(Vec::len).collect::<Vec<_>>()),
            );
            return;
        }

        // The mode is legal by `push_engine_rules`; re-checked here because a run must never
        // hand an engine a form it refuses, and the fallback is logged rather than silent.
        let support = self.marks_support();
        let mode = reconcile_marks_mode(self.marks_mode, support);
        if mode != self.marks_mode {
            ms_log::runtime_log::log_warn(format!(
                "{} run requested with marks as {:?}, which the engine does not accept; sending {:?}",
                self.spec.log_tag, self.marks_mode, mode
            ));
        }
        let frame_masks = self.frame.masks();
        let marks_empty = frame_masks.marks_is_empty();
        // Overlay mode puts the marks INTO the picture the engine edits; the unmarked region is
        // kept so `accept_result` can take every untouched mark back out of the answer.
        let original = (mode == MarksMode::OverlayOnRegion && !marks_empty).then(|| region.clone());
        let (region, marks) = match prepare_run_marks(mode, region, frame_masks.marks_rgba(), marks_empty) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.frame.set_processing(false);
                // An internal invariant broke (the marks layer always has the frame's size): say
                // so, rather than blame the loaded region.
                self.report_error(t!("cleaning.tools.area_editor.error_marks_shape").to_string(), &error);
                return;
            }
        };
        let restore = original.map(|original| MarksRestore { original, sent: region.clone() });
        let request = EngineRunRequest {
            page_idx: pending.page_idx,
            rect_px: rect,
            region,
            masks,
            marks,
        };
        let outcome = match self.engine_mut() {
            Some(engine) => engine.start(request),
            None => Err(t!("cleaning.tools.area_editor.error_no_engine").to_string()),
        };
        match outcome {
            Ok(()) => {
                self.marks_restore = restore;
                self.report_info(t!("cleaning.tools.area_editor.run_started_status").to_string());
            }
            Err(reason) => {
                self.frame.set_processing(false);
                self.report_error(reason, &"the engine refused the run request");
            }
        }
    }

    /// Step 2 of a mask generation: hands the loaded region to the detector worker.
    ///
    /// The frame stays `Processing` across the handover, so the mask cannot be painted and no
    /// run can start while a detector is about to overwrite the active layer. The worker is
    /// started through `self.spawn_detection`, which is the real spawner everywhere except in
    /// this module's own tests.
    fn start_mask_detection(&mut self, pending: PendingLoad, region: egui::ColorImage) {
        let rect = pending.rect;
        if region.size != [rect.w, rect.h] {
            self.frame.set_processing(false);
            self.report_error(
                t!("cleaning.tools.area_editor.error_region_size").to_string(),
                &format!(
                    "the loaded region is {}x{}, the frame region is {}x{}",
                    region.size[0], region.size[1], rect.w, rect.h
                ),
            );
            return;
        }
        let params = self.mask_generation.params;
        if params.source == MaskSource::Watermark {
            // The detection may download network code and weights; re-query the catalog once
            // it ends, so the ✓/«скачать» marks stop lying.
            self.mask_generation.rearm_watermark_catalog();
        }
        // Through the stored spawner, never `mask_generation::spawn_mask_generation` by name:
        // the indirection is this host's only test seam for the detection (see the field).
        self.mask_generation_rx = Some((self.spawn_detection)(
            region,
            params,
            Arc::clone(&self.mask_generation.watermark_progress),
        ));
        self.report_info(t!("cleaning.mask_editor.generating_mask_status").to_string());
    }

    /// Step 3 of a mask generation: drains the detector and writes its answer into the frame.
    ///
    /// Runs every frame, panel visible or not — the same rule the engine poll follows: a panel
    /// the user closed mid-detection must not strand the job. A running detector repaints, so
    /// the streaming watermark progress bar advances without any input.
    fn poll_mask_generation(&mut self, ctx: &egui::Context) {
        match mask_generation::poll_mask_generation(&mut self.mask_generation_rx) {
            MaskGenerationPoll::Idle => {}
            MaskGenerationPoll::Running => ctx.request_repaint(),
            MaskGenerationPoll::Done(mask) => self.accept_generated_mask(mask),
            MaskGenerationPoll::Failed(text) => {
                self.frame.set_processing(false);
                self.report_error(text, &"the mask-generation worker reported a failure");
            }
        }
    }

    /// Writes a finished detection into the SELECTED mask layer of the frame's stack.
    ///
    /// It goes through `MaskStack::set_active_from_alpha`, which snapshots the layer first, so
    /// the whole generated mask is one undo step and erases exactly like a brush stroke — the
    /// host never touches a layer's pixels itself. Which layer receives it is the user's
    /// choice in the layer picker, and what the mask MEANS there is the engine's business: the
    /// detector only marks the text or the watermark it found.
    ///
    /// A mask whose size is not exactly the frame rectangle is REFUSED rather than rescaled,
    /// for the same reason a result is (D7): the stack and the region share one stride.
    fn accept_generated_mask(&mut self, mask: GeneratedMask) {
        self.frame.set_processing(false);
        let Some(rect) = self.frame.rect_px() else {
            self.report_error(
                t!("cleaning.tools.area_editor.error_no_frame").to_string(),
                &"a generated mask arrived for a frame that has no rectangle",
            );
            return;
        };
        let size = mask.size();
        if size != [rect.w, rect.h] {
            self.report_error(
                tf!("cleaning.mask_editor.mask_gen_size_error", mask_w = size[0], mask_h = size[1], expected_w = rect.w, expected_h = rect.h),
                &"the detector answered about a region of a different size",
            );
            return;
        }
        let (stack_w, stack_h) = self.frame.masks().size();
        if !self.frame.masks_mut().set_active_from_alpha(mask.alpha()) {
            self.report_error(
                tf!("cleaning.mask_editor.mask_gen_size_error", mask_w = size[0], mask_h = size[1], expected_w = stack_w, expected_h = stack_h),
                &format!("the mask stack is {stack_w}x{stack_h} and refused a {}x{} mask", size[0], size[1]),
            );
            return;
        }
        self.report_info(t!("cleaning.mask_editor.mask_generated_status").to_string());
    }

    /// Step 3: polls the selected engine and turns a terminal answer into frame state.
    ///
    /// Runs EVERY frame, panel visible or not: an engine drains its channels here, so skipping
    /// it would strand a finished run — and, for FLUX.2 klein, would also stop the settings
    /// saver that runs inside `poll`, silently losing model paths and prompts.
    fn poll_engine(&mut self, ctx: &egui::Context) {
        let Some(engine) = self.engine_mut() else {
            return;
        };
        let poll = engine.poll(ctx);
        match poll {
            // A run that is still going, or none at all: the frame's own lock already says so.
            EnginePoll::Idle | EnginePoll::Running => {}
            EnginePoll::Done(image) => self.accept_result(image),
            EnginePoll::Failed(reason) => {
                self.marks_restore = None;
                self.frame.set_processing(false);
                self.report_error(reason, &"the engine reported a failed run");
            }
        }
    }

    /// Stores a finished run as the frame's pending result, or refuses it on size (D7).
    ///
    /// The check is against the frame's CURRENT rectangle, which is the one the run started
    /// with — the frame is locked for the whole run — so a mismatch means the engine answered
    /// about something else and the result must not be applied anywhere.
    ///
    /// A run that composited the marks into its region gets them taken back out first
    /// (`restore_untouched_marked_pixels`): marks are instructions, never content.
    fn accept_result(&mut self, mut image: egui::ColorImage) {
        self.frame.set_processing(false);
        let restore = self.marks_restore.take();
        let Some(rect) = self.frame.rect_px() else {
            self.report_error(
                t!("cleaning.tools.area_editor.error_no_frame").to_string(),
                &"a result arrived for a frame that has no rectangle",
            );
            return;
        };
        if image.size != [rect.w, rect.h] {
            self.report_error(
                t!("cleaning.tools.area_editor.error_size_mismatch").to_string(),
                &format!(
                    "the engine answered {}x{}, the frame region is {}x{}",
                    image.size[0], image.size[1], rect.w, rect.h
                ),
            );
            return;
        }
        if let Some(restore) = restore {
            let restored = restore_untouched_marked_pixels(&mut image, &restore.original, &restore.sent);
            ms_log::runtime_log::log_info(format!(
                "{} took the marks back out of the result: {restored} untouched marked pixel(s) restored",
                self.spec.log_tag
            ));
        }
        self.frame.set_result(Some(ResultLayer::new(image)));
        self.report_info(t!("cleaning.tools.area_editor.result_ready_status").to_string());
    }

    /// Abandons everything in flight: the engine's run, a mask detection, the region load that
    /// precedes either, and a queued generation request.
    ///
    /// Dropping `mask_generation_rx` is what cancels a detection: the worker cannot be stopped
    /// mid-call, so its answer is discarded on arrival instead of reaching a mask the user has
    /// meanwhile taken back.
    fn cancel_run(&mut self) {
        self.pending_load = None;
        self.marks_restore = None;
        self.mask_generation_rx = None;
        self.generate_mask_requested = false;
        if let Some(engine) = self.engine_mut() {
            engine.cancel();
        }
        self.frame.set_processing(false);
        self.frame.set_result(None);
    }

    /// Merges the pending result into the clean overlay and releases the frame.
    ///
    /// The size check runs FIRST and refuses rather than rescales (D7). A refusal leaves the
    /// result pending, so the user can cancel it or resize nothing and try again.
    fn apply_result(&mut self, canvas: &mut CanvasView) {
        let (Some(page_idx), Some(rect)) = (self.frame.page_idx(), self.frame.rect_px()) else {
            self.report_error(
                t!("cleaning.tools.area_editor.error_no_frame").to_string(),
                &"the frame has no page anchor or no rectangle",
            );
            return;
        };
        let Some(result) = self.frame.result() else {
            self.report_error(
                t!("cleaning.tools.area_editor.error_no_result").to_string(),
                &"apply was requested with no pending result",
            );
            return;
        };
        if let Err(error) = check_result_fits(result.size(), rect, canvas.overlay_size(page_idx)) {
            self.report_error(
                t!("cleaning.tools.area_editor.error_size_mismatch").to_string(),
                &error,
            );
            return;
        }
        if !canvas.replace_overlay_region_px(page_idx, rect, result.image()) {
            self.report_error(
                tf!("cleaning.tools.area_editor.error_apply_failed", page = page_idx + 1),
                &format!("replace_overlay_region_px refused page {page_idx}, region {rect:?}"),
            );
            return;
        }
        self.frame.reset();
        self.report_info(t!("cleaning.tools.area_editor.applied_status").to_string());
    }
}

impl CleaningTool for RegionEditHost {
    fn tool_id(&self) -> &'static str {
        self.spec.tool_id
    }

    fn title(&self) -> &'static str {
        (self.spec.title)()
    }

    /// Whatever the SELECTED engine needs. The tab gates the tool button on this, so a tool
    /// whose current engine wants Torch is offered exactly like the Torch tools beside it.
    fn pytorch_required(&self) -> bool {
        self.engine().is_some_and(AiEngine::requires_torch)
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        // The frame keeps its placement (`reset` does), but nothing it held may survive a tool
        // switch: an unapplied result would come back as a preview over a page the user has
        // meanwhile edited, and a run in flight would answer about a rectangle nobody sees.
        self.cancel_run();
        self.frame.reset();
        self.message = None;
        // The compact panel is not drawn for an inactive tool, so a pending sampling ends here.
        self.cancel_marks_eyedropper();
    }

    /// The compact part of the tool's interface, in «Выбранный инструмент» (§13.1).
    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        if self.draw_engine_picker(ui) {
            ui.separator();
        }
        self.draw_brush_controls(ui);
        self.draw_layer_picker(ui);
        self.draw_mask_generation(ui);
        self.draw_mask_actions(ui);
        self.draw_marks_mode(ui);
        self.draw_mask_summary(ui);
        ui.separator();
        ui.small(t!("cleaning.tools.area_editor.main_panel_hint"));
    }

    fn wants_main_panel(&self) -> bool {
        true
    }

    /// The scrolled middle of the «Редактор области» dock panel: the selected engine's own
    /// parameters (§13.1). Its progress is pinned above it (`draw_main_panel_top`) and the
    /// host actions under it (`draw_main_panel_bottom`), both outside the scroll.
    ///
    /// It runs inside `CanvasView::draw` and therefore mutates only the tool: every action
    /// raises a flag the frame consumes at the top of the next pass, re-checked against the
    /// frame's own enablement table.
    fn draw_main_panel(&mut self, ui: &mut egui::Ui) {
        match self.engine_mut() {
            Some(engine) => engine.draw_parameters(ui),
            None => {
                ui.colored_label(ui.visuals().error_fg_color, t!("cleaning.tools.area_editor.error_no_engine"));
            }
        }
    }

    /// The pinned top of the main panel: the selected engine's own progress (D13 — the engine
    /// owns it, the host only decides where it sits). Nothing without an engine.
    fn draw_main_panel_top(&mut self, ui: &mut egui::Ui) {
        if let Some(engine) = self.engine_mut() {
            engine.draw_progress(ui);
        }
    }

    /// The pinned bottom of the main panel: the host's «Обработать» section and, under it,
    /// the engine's per-run options with the facts they depend on (`run_options_ctx`).
    fn draw_main_panel_bottom(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        self.draw_host_actions(ui);
        let ctx = self.run_options_ctx();
        if let Some(engine) = self.engine_mut() {
            engine.draw_run_options(ui, ctx);
        }
    }

    fn set_ai_backend_available(&mut self, available: bool) {
        self.backend_available = available;
    }

    fn set_ai_backend_torch_available(&mut self, available: bool) {
        self.torch_available = available;
    }

    fn set_panel_rects(&mut self, rects: &[egui::Rect]) {
        self.panel_rects.clear();
        self.panel_rects.extend_from_slice(rects);
    }

    /// The frame's whole per-frame pass, the engine's per-frame pushes, and one step of the
    /// run in flight.
    ///
    /// This is the only hook that owns the context, the canvas and the project at once, which
    /// is why the pass lives here rather than in `draw_cursor` (§10.1 of the design).
    ///
    /// The order is load-bearing:
    /// 1. `allows_empty_mask` and `constraints` are read back BEFORE the pass, because the
    ///    engine may derive either from a parameter the user changed in this same frame's panel
    ///    body, which ran earlier inside `CanvasView::draw` (an engine whose model picker
    ///    decides its size rules, say). A stale copy would silently block or allow a run
    ///    (§13.5), or validate the rectangle against the previous model's rules.
    /// 2. the frame pass settles the rectangle, so the `set_region` push below carries THIS
    ///    frame's rectangle — pushing it after a run has started would look like a moved frame
    ///    to an engine that treats that as "a different image" and cancels the run.
    /// 3. the outcome is acted on, then the load and the engine are polled, so a run started
    ///    this frame gets its first poll in the next one, never inside its own start.
    fn draw_overlay_ui(&mut self, ctx: &egui::Context, canvas: &mut CanvasView, project: &ProjectData) {
        self.push_engine_rules();

        let host = FrameHost {
            panel_rects: &self.panel_rects,
            page_count: project.pages.len(),
        };
        let outcome = self.frame.update(ctx, canvas, host);

        // `drag_active()` is the frame's own answer to "is a gesture in flight?" — a move
        // drag, a resize drag or a mask stroke — and it is read AFTER the pass, so the frame
        // on which the pointer was released already reports settled. A paint stroke cannot
        // change the rectangle at all, but it is deliberately included: an engine must be
        // able to treat "settled" as "the user's hand is off the frame".
        let settled = !self.frame.drag_active();
        let (backend, torch, region) = (self.backend_available, self.torch_available, self.frame.rect_px());
        if let Some(engine) = self.engine_mut() {
            engine.set_backend_available(backend);
            engine.set_torch_available(torch);
            engine.set_region(region, settled);
        }

        if outcome.clear_mask_requested {
            self.frame.masks_mut().clear_all();
        }
        if outcome.cancel_requested {
            self.cancel_run();
            self.report_info(t!("cleaning.tools.area_editor.cancelled_status").to_string());
        }
        // Process before apply: the two are mutually exclusive by `FrameButtons`, and running
        // first keeps the order the buttons sit in.
        if outcome.process_requested {
            self.start_run(canvas, project);
        }
        if outcome.apply_requested {
            self.apply_result(canvas);
        }
        // Last of the queued actions: `cancel_run` clears the flag, so a cancel and a generate
        // clicked in the same frame resolve as the cancel.
        if self.generate_mask_requested {
            self.generate_mask_requested = false;
            self.start_mask_generation(canvas, project);
        }

        self.poll_region_load();
        self.poll_mask_generation(ctx);
        self.poll_engine(ctx);
    }

    /// The frame's hitbox swallows canvas input; nothing outside it does.
    fn captures_canvas_pointer(&self, pointer_pos: Pos2) -> bool {
        self.frame.captures_pointer(pointer_pos)
    }

    /// Only a live move/resize drag blocks canvas drag-scroll (D5), and canvas drag-scroll
    /// needs Space held anyway, so painting is never affected.
    fn block_canvas_drag_scroll_on_primary(&self) -> bool {
        self.frame.drag_active()
    }

    /// This tool never takes a canvas stroke: every gesture it has belongs to the frame's own
    /// `egui::Area`, which senses it through a `Response`.
    fn wants_primary_stroke(&self, _point: StrokePoint) -> bool {
        false
    }

    /// Ctrl/Cmd+drag over the frame body draws a marks rectangle in marks mode, so the canvas
    /// zoom drag is blocked exactly then — never as a constant (see
    /// `RegionFrame::blocks_ctrl_primary_zoom`); mask mode keeps the canvas zoom.
    fn block_canvas_zoom_on_ctrl_primary(&self) -> bool {
        self.frame.blocks_ctrl_primary_zoom()
    }

    /// Shift+wheel resizes the CURRENT brush (mask or marks), through the frame's `MaskBrush`.
    ///
    /// This hook covers the pointer OUTSIDE the frame only: `tab.rs::handle_active_tool_wheel`
    /// drops the event while the canvas pointer is occluded, and the frame occludes its own
    /// hitbox. Over the frame the identical gesture is handled inside the frame's pass.
    fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.frame.active_brush_mut().handle_wheel(delta_y, modifiers)
    }

    /// The region editor's brush-size shortcuts `-` / `=` / `+`, for the pointer OUTSIDE the
    /// frame — `tab.rs::handle_active_tool_hotkeys` is gated on the same occlusion test as the
    /// wheel, so over the frame the shortcuts are handled inside the frame's pass instead.
    ///
    /// The tab repaints on a `true`, which is what makes the new radius show up in the brush
    /// ring immediately.
    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        self.frame.active_brush_mut().handle_size_shortcuts(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::engine::{EngineSection, MaskLayerSpec};
    use super::super::frame::PaintTarget;
    use super::super::geometry::SizeViolation;
    use crate::tools::mask_generation::{MaskGenerationParams, WatermarkProgress};
    use egui::Color32;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Mutex;

    fn rect(x: usize, y: usize, w: usize, h: usize) -> OverlayRectPx {
        OverlayRectPx { x, y, w, h }
    }

    /// The spec every host test builds from. Its catalog is two idle [`RecordingEngine`]s, not
    /// a real consumer's engines: the host is generic, a real engine would start settings
    /// workers the tests do not need, and the switch tests need a second engine to switch to.
    /// Like every real engine today, each declares ONE mask layer.
    static TEST_SPEC: HostSpec = HostSpec {
        tool_id: "region_edit_host_test",
        title: test_title,
        log_tag: "[cleaning/region_edit_host_test]",
        mask_generation_section_salt: "cleaning_region_edit_host_test_mask_generation_section",
        mask_source_picker_salt: "cleaning_region_edit_host_test_mask_source_picker",
        catalog: test_catalog,
    };

    fn test_title() -> &'static str {
        "region edit host test"
    }

    fn test_catalog() -> Vec<Box<dyn AiEngine>> {
        vec![Box::new(RecordingEngine::idle()), Box::new(RecordingEngine::idle())]
    }

    /// A host over [`TEST_SPEC`], as the product builds one over a consumer's spec.
    fn test_host() -> RegionEditHost {
        RegionEditHost::new(&TEST_SPEC)
    }

    /// The detector a test installs into [`RegionEditHost::spawn_detection`] in place of the real
    /// one: it starts no worker, so no backend round trip and no model download happens — the
    /// default source is `ComicTextDetector`, whose first real call downloads weights into the
    /// runtime data root, which for a test binary is whatever directory it was launched from.
    ///
    /// It answers a full mask of exactly the size of the region it was handed, so a test can
    /// prove that the region reached the spawner rather than only that some receiver was
    /// stored. `params` and `progress` are named by the [`MaskGenerationSpawner`] signature and
    /// have no stub behaviour to drive.
    fn stub_detection_spawner(
        image: egui::ColorImage,
        _params: MaskGenerationParams,
        _progress: Arc<Mutex<WatermarkProgress>>,
    ) -> Receiver<Result<GeneratedMask, String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let alpha = vec![255u8; image.size[0] * image.size[1]];
        // The receiver is alive in this scope, so the send cannot fail.
        tx.send(Ok(GeneratedMask::from_parts_for_test(image.size, alpha))).expect("the stub answer is queued");
        rx
    }

    #[test]
    fn a_result_of_exactly_the_region_size_is_accepted() {
        assert_eq!(
            check_result_fits([64, 32], rect(10, 20, 64, 32), Some([200, 300])),
            Ok(())
        );
    }

    /// The check exists because `replace_overlay_region_px` would nearest-rescale instead of
    /// refusing, silently stretching the result over the region (D7).
    #[test]
    fn a_result_of_the_wrong_size_is_refused() {
        assert_eq!(
            check_result_fits([63, 32], rect(0, 0, 64, 32), Some([200, 300])),
            Err(ApplyError::SizeMismatch {
                result_w: 63,
                result_h: 32,
                region_w: 64,
                region_h: 32,
            })
        );
    }

    #[test]
    fn a_region_that_leaves_the_overlay_is_refused() {
        assert_eq!(
            check_result_fits([64, 32], rect(180, 0, 64, 32), Some([200, 300])),
            Err(ApplyError::OutOfBounds {
                x: 180,
                y: 0,
                w: 64,
                h: 32,
                overlay_w: 200,
                overlay_h: 300,
            })
        );
    }

    /// A page with no overlay yet has no bounds to check against; the write allocates one at
    /// the page size, so only the size equality is enforced.
    #[test]
    fn without_an_overlay_only_the_size_is_checked() {
        assert_eq!(check_result_fits([64, 32], rect(9000, 0, 64, 32), None), Ok(()));
        assert!(check_result_fits([64, 33], rect(0, 0, 64, 32), None).is_err());
    }

    /// The frame is built around the engine that is selected at construction, so the very
    /// first painted stroke already goes into the layers that engine asked for.
    #[test]
    fn the_frame_starts_shaped_by_the_selected_engine() {
        let tool = test_host();
        let engine = tool.engine().expect("the catalog is not empty");
        assert_eq!(tool.frame.masks().layer_count(), engine.mask_layers().len());
        assert_eq!(tool.frame.constraints().multiple, engine.constraints().multiple);
        assert_eq!(tool.frame.constraints().min_side, engine.constraints().min_side);
    }

    /// D15: the picker may not switch engines while the frame holds work, because engines
    /// declare different mask layers and the switch re-creates the stack.
    #[test]
    fn a_locked_frame_refuses_an_engine_switch() {
        let mut tool = test_host();
        let first = tool.selected;
        tool.frame.set_processing(true);
        tool.select_engine(first + 1);
        assert_eq!(tool.selected, first, "a processing frame must keep its engine");
        tool.frame.set_processing(false);
        tool.frame
            .set_result(Some(ResultLayer::new(egui::ColorImage::filled([2, 2], Color32::WHITE))));
        tool.select_engine(first + 1);
        assert_eq!(tool.selected, first, "a frame holding a result must keep its engine too");
    }

    /// The engine's answer is the only thing that decides whether an empty mask may run, and
    /// it is re-read rather than cached: FLUX.2 klein derives it from a checkbox in its own
    /// panel (§13.5), so a copy taken at the switch would go stale on the next click.
    #[test]
    fn the_empty_mask_rule_follows_the_selected_engine() {
        let mut tool = test_host();
        let allows = tool.engine().is_some_and(AiEngine::allows_empty_mask);
        tool.frame.set_allows_empty_mask(allows);
        // The frame relaxes only on demand; without the push it refuses an empty-mask run.
        tool.frame.set_allows_empty_mask(false);
        assert!(!tool.frame.buttons().process, "an empty mask blocks the run until an engine allows it");
    }

    /// An engine may derive its size rules from its own parameters (a model picker), so the
    /// host re-reads them every frame. The push re-validates and never resizes: the rectangle
    /// the user placed stays, turns red, and the mask stack is left alone.
    #[test]
    fn the_size_rules_follow_the_selected_engine_every_frame() {
        let grid = Rc::new(Cell::new(1));
        let mut tool = test_host();
        tool.engines = vec![Box::new(RecordingEngine {
            calls: Rc::new(RefCell::new(EngineCalls::default())),
            answer: EnginePoll::Idle,
            switch_block: None,
            grid: Rc::clone(&grid),
            marks: Rc::new(Cell::new(MarksSupport::OVERLAY_ONLY)),
        })];
        tool.selected = 0;
        tool.frame.place_for_test(0, rect(0, 0, 12, 20));
        tool.push_engine_rules();
        assert_eq!(tool.frame.size_violation(), None);

        grid.set(8);
        tool.push_engine_rules();
        assert_eq!(tool.frame.size_violation(), Some(SizeViolation::NotMultiple));
        let r = tool.frame.rect_px().expect("the frame stays placed");
        assert_eq!((r.w, r.h), (12, 20), "a new rule never resizes the rectangle");
        assert_eq!(tool.frame.masks().layer_count(), 1, "nor re-creates the mask stack");
    }

    /// The tab gates the tool button on this, so it must describe the engine that would
    /// actually run rather than the tool as a category.
    #[test]
    fn the_torch_requirement_comes_from_the_selected_engine() {
        let tool = test_host();
        let expected = tool.engine().is_some_and(AiEngine::requires_torch);
        assert_eq!(tool.pytorch_required(), expected);
    }

    /// D5, pinned. `block_canvas_zoom()` does not only block zooming: `tab.rs` refuses the
    /// clean-overlay Ctrl+Z / Ctrl+Shift+Z shortcuts for any tool that returns `true`
    /// (`handle_history_hotkeys`) and the zoom shortcuts with it. Ten of the twelve registered
    /// tools DO override it to `true`, so copying a sibling is the likely edit — and this tool
    /// lives on the canvas for the WHOLE editing session, so inheriting `true` would kill
    /// canvas zoom and clean-overlay undo for the session with nothing else failing.
    #[test]
    fn the_area_editor_never_blocks_canvas_zoom_or_the_undo_shortcuts() {
        let tool = test_host();
        assert!(!tool.block_canvas_zoom(), "D5: blocking is precise, never the whole canvas");
        assert!(!tool.block_canvas_zoom_on_ctrl_primary(), "the same reasoning, for the Ctrl+drag zoom");
        // Blocking is precise instead: only a live frame gesture stops canvas drag-scroll.
        assert!(!tool.block_canvas_drag_scroll_on_primary(), "an idle frame blocks nothing");
    }

    /// A capture error must name the page and the region: the loader composites the clean
    /// overlay over the page crop, so a silently skipped chunk would hand the model a region
    /// without the user's existing clean edits.
    #[test]
    fn a_capture_error_names_the_page_and_the_region() {
        let error = CaptureError::NotMapped { page: 4, x: 10, y: 20, w: 64, h: 32 };
        let text = error.to_string();
        assert!(text.contains("page 4") && text.contains("10;20") && text.contains("64x32"), "{text}");
        let error = CaptureError::ChunkSize { page: 4, x: 10, y: 20, w: 64, h: 32, chunk_w: 63, chunk_h: 32 };
        let text = error.to_string();
        assert!(text.contains("page 4") && text.contains("63x32") && text.contains("64x32"), "{text}");
    }

    /// The panel repeats the frame's own actions, so it must queue them rather than perform
    /// them: its body runs inside `CanvasView::draw` and owns no `&mut CanvasView`.
    #[test]
    fn the_panel_can_queue_the_actions_that_resolve_a_pending_result() {
        let mut tool = test_host();
        assert!(!tool.frame.buttons().apply, "nothing to apply on a fresh frame");
        tool.frame
            .set_result(Some(ResultLayer::new(egui::ColorImage::filled([2, 2], Color32::WHITE))));
        let buttons = tool.frame.buttons();
        assert!(buttons.apply && buttons.cancel, "both must be offered while a result waits");
        assert!(!buttons.process, "a second run must not be able to replace the pending result");
        tool.frame.request_apply();
        tool.frame.request_cancel();
    }

    /// The mask may not be edited while a result waits or work runs: the mask then describes
    /// work already handed over.
    #[test]
    fn the_mask_is_not_editable_while_work_is_held() {
        let mut tool = test_host();
        assert!(tool.mask_editable());
        tool.frame.set_processing(true);
        assert!(!tool.mask_editable());
        tool.frame.set_processing(false);
        tool.frame
            .set_result(Some(ResultLayer::new(egui::ColorImage::filled([2, 2], Color32::WHITE))));
        assert!(!tool.mask_editable());
    }

    /// Cancelling must release the frame AND abandon the load job, or the region that is still
    /// being decoded would arrive later and start a run the user already stopped.
    #[test]
    fn cancelling_abandons_the_pending_region_load() {
        let mut tool = test_host();
        tool.pending_load = Some(PendingLoad { job_id: 7, page_idx: 0, rect: rect(0, 0, 64, 64), purpose: LoadPurpose::Run });
        tool.frame.set_processing(true);
        tool.cancel_run();
        assert!(tool.pending_load.is_none());
        assert_eq!(tool.frame.lock(), FrameLock::Free);
    }

    /// A stale answer — one from a job that was abandoned — must be dropped on its id rather
    /// than started, and must not disturb the frame.
    #[test]
    fn a_region_from_an_abandoned_job_is_ignored() {
        let mut tool = test_host();
        tool.pending_load = Some(PendingLoad { job_id: 2, page_idx: 0, rect: rect(0, 0, 64, 64), purpose: LoadPurpose::Run });
        // The loader is a real worker here; feed the tool the answer of job 1 directly.
        tool.hand_region_to_engine(
            PendingLoad { job_id: 1, page_idx: 0, rect: rect(0, 0, 4, 4), purpose: LoadPurpose::Run },
            egui::ColorImage::filled([8, 8], Color32::WHITE),
        );
        assert!(
            tool.message.as_ref().is_some_and(|message| message.error),
            "a region of the wrong size must be refused, never rescaled"
        );
        assert!(tool.frame.result().is_none());
    }

    /// A generated mask lands in the layer the user SELECTED, not in layer 0, and only there:
    /// which layer the detection fills is the layer picker's answer, and the engines that
    /// declare two layers give the two different meanings.
    #[test]
    fn a_generated_mask_fills_the_selected_layer_only() {
        let mut tool = test_host();
        tool.frame.place_for_test(0, rect(0, 0, 5, 3));
        let layers = tool.frame.masks().layer_count();
        assert!(layers >= 1, "every engine declares at least one mask layer");
        let target = layers - 1;
        tool.frame.masks_mut().set_active(target);

        let mut alpha = vec![0u8; 15];
        // Row 2, column 1: a transposed write would land somewhere else.
        alpha[2 * 5 + 1] = 255;
        tool.accept_generated_mask(GeneratedMask::from_parts_for_test([5, 3], alpha));

        assert_eq!(tool.frame.masks().layer_set_px(target), 1);
        assert_eq!(tool.frame.masks().bytes(target)[2 * 5 + 1], 255);
        for idx in 0..layers {
            if idx != target {
                assert_eq!(tool.frame.masks().layer_set_px(idx), 0, "layer {idx} must be untouched");
            }
        }
        assert!(tool.message.as_ref().is_some_and(|message| !message.error), "success is reported");
        assert_eq!(tool.frame.lock(), FrameLock::MaskPainted, "the frame is released into its painted state");
    }

    /// The fill is undoable exactly like a stroke: one `undo` restores what the brush had left.
    #[test]
    fn a_generated_mask_is_undoable_like_a_stroke() {
        let mut tool = test_host();
        tool.frame.place_for_test(0, rect(0, 0, 4, 4));
        tool.accept_generated_mask(GeneratedMask::from_parts_for_test([4, 4], vec![255u8; 16]));
        let active = tool.frame.masks().active();
        assert_eq!(tool.frame.masks().layer_set_px(active), 16);
        assert!(tool.frame.masks_mut().undo());
        assert!(tool.frame.masks().is_empty(), "undo takes the whole generated mask back");
    }

    /// A detection answer of the wrong size is REFUSED, never rescaled: the stack and the
    /// region share one stride (D7's rule, applied to the mask).
    #[test]
    fn a_generated_mask_of_the_wrong_size_is_refused() {
        let mut tool = test_host();
        tool.frame.place_for_test(0, rect(0, 0, 5, 3));
        tool.accept_generated_mask(GeneratedMask::from_parts_for_test([3, 5], vec![255u8; 15]));
        assert!(tool.message.as_ref().is_some_and(|message| message.error));
        assert!(tool.frame.masks().is_empty(), "nothing may be written from a mask of the wrong shape");
    }

    /// Every state that must refuse a generation does, with its own reason, in the documented
    /// order. This is the rule «Сгенерировать маску» is drawn from AND the one a click is
    /// checked against, so the button and the action cannot disagree.
    #[test]
    fn generation_is_refused_in_every_blocked_state() {
        let mut tool = test_host();
        // Backend and Torch present, so nothing but the frame's own state can block.
        tool.backend_available = true;
        tool.torch_available = true;

        // 1. No frame yet.
        assert_eq!(
            tool.mask_generation_block_reason().map(|(text, _)| text),
            Some(t!("cleaning.tools.area_editor.error_no_frame").to_string())
        );

        tool.frame.place_for_test(0, rect(0, 0, 8, 8));
        assert!(tool.mask_generation_block_reason().is_none(), "a placed, free frame may generate");

        // 2. A job already in flight.
        tool.pending_load = Some(PendingLoad { job_id: 1, page_idx: 0, rect: rect(0, 0, 8, 8), purpose: LoadPurpose::MaskGeneration });
        assert_eq!(
            tool.mask_generation_block_reason().map(|(text, _)| text),
            Some(t!("cleaning.mask_editor.background_op_running_status").to_string())
        );
        tool.pending_load = None;

        // 3. A result waiting: the mask then describes work already handed over, and
        //    overwriting it would make the pending result describe a mask that is gone.
        tool.frame
            .set_result(Some(ResultLayer::new(egui::ColorImage::filled([8, 8], Color32::WHITE))));
        assert!(!tool.mask_editable());
        assert_eq!(
            tool.mask_generation_block_reason().map(|(text, _)| text),
            Some(t!("cleaning.tools.area_editor.generate_mask_locked_hint").to_string())
        );
        tool.frame.set_result(None);

        // 4. The source itself unavailable — reported with the source's own reason, never a
        //    silent fallback to another detector.
        tool.backend_available = false;
        assert_eq!(
            tool.mask_generation_block_reason().map(|(text, _)| text),
            Some(t!("cleaning.mask_editor.backend_unavailable_status").to_string())
        );
    }

    /// Cancelling abandons a detection in flight: the worker cannot be stopped mid-call, so
    /// dropping the receiver is what keeps its answer from reaching a mask the user took back.
    #[test]
    fn cancelling_abandons_a_running_detection() {
        let mut tool = test_host();
        tool.frame.place_for_test(0, rect(0, 0, 8, 8));
        let (tx, rx) = std::sync::mpsc::channel::<Result<GeneratedMask, String>>();
        tool.mask_generation_rx = Some(rx);
        tool.generate_mask_requested = true;
        tool.frame.set_processing(true);

        tool.cancel_run();

        assert!(tool.mask_generation_rx.is_none());
        assert!(!tool.generate_mask_requested, "a queued request must not survive a cancel");
        assert_eq!(tool.frame.lock(), FrameLock::Free);
        // The worker's answer now has nowhere to go, which is the point.
        assert!(tx.send(Ok(GeneratedMask::from_parts_for_test([8, 8], vec![255u8; 64]))).is_err());
    }

    /// A region loaded for a detection must NOT reach the engine, and vice versa: the purpose
    /// travels with the job because the loader has one slot for both consumers.
    #[test]
    fn a_load_carries_what_it_was_started_for() {
        let mut tool = test_host();
        // Opt out of the real detector; everything else is the production path.
        tool.spawn_detection = stub_detection_spawner;
        tool.frame.place_for_test(0, rect(0, 0, 4, 4));
        tool.backend_available = true;
        tool.torch_available = true;
        let pending = PendingLoad { job_id: 1, page_idx: 0, rect: rect(0, 0, 4, 4), purpose: LoadPurpose::MaskGeneration };
        tool.frame.set_processing(true);
        tool.start_mask_detection(pending, egui::ColorImage::filled([4, 4], Color32::WHITE));

        assert!(tool.mask_generation_rx.is_some(), "the detector worker was started");
        assert!(tool.frame.result().is_none(), "a detection never produces a result layer");
        assert_eq!(tool.frame.lock(), FrameLock::Processing, "the frame stays locked for the detection");
        // The stored receiver is the spawner's own, and it carries a mask of exactly the region
        // that was handed over: the loaded region travelled THROUGH the detection path.
        let answer = tool.mask_generation_rx.as_ref().expect("the receiver is stored").recv().expect("the stub answered");
        assert_eq!(answer.expect("the stub answers a mask").size(), [4, 4]);
    }

    /// What one `RecordingEngine` was asked to do, shared with the test that installed it.
    #[derive(Debug, Default)]
    struct EngineCalls {
        polls: usize,
        cancels: usize,
        backend: Option<bool>,
        torch: Option<bool>,
        region: Option<Option<OverlayRectPx>>,
        /// What the last `set_region` push reported about the frame's gesture state.
        geometry_settled: Option<bool>,
        started: Option<(usize, usize, usize)>,
        /// The region and the marks of the last started run.
        run_region: Option<egui::ColorImage>,
        run_marks: Option<RunMarks>,
        /// How often each panel section of the engine was drawn.
        parameter_draws: usize,
        progress_draws: usize,
        /// The context of every `draw_run_options` call, in order.
        run_options: Vec<RunOptionsCtx>,
    }

    /// A minimal `AiEngine` that records what the host does to it and answers whatever the
    /// test queued. It exists because the host's contract with an engine is entirely about
    /// CALL ORDER and per-frame pushes, which no real engine can observe for us.
    struct RecordingEngine {
        calls: Rc<RefCell<EngineCalls>>,
        answer: EnginePoll,
        /// What this engine answers to [`AiEngine::switch_block_reason`]. `None` is the
        /// trait default and the state every other test wants.
        switch_block: Option<String>,
        /// The grid its `constraints()` declare, shared so a test can change the declaration
        /// the way a real engine's own parameter would. `1` everywhere else.
        grid: Rc<Cell<usize>>,
        /// What its `marks_support()` answers, shared for the same reason as `grid`. The
        /// trait default (overlay only) everywhere else.
        marks: Rc<Cell<MarksSupport>>,
    }

    impl RecordingEngine {
        /// An engine whose call record nobody reads, answering `Idle` and refusing no switch.
        fn idle() -> Self {
            Self {
                calls: Rc::new(RefCell::new(EngineCalls::default())),
                answer: EnginePoll::Idle,
                switch_block: None,
                grid: Rc::new(Cell::new(1)),
                marks: Rc::new(Cell::new(MarksSupport::OVERLAY_ONLY)),
            }
        }

        /// An idle engine whose marks support the test controls through the returned cell.
        fn with_marks(support: MarksSupport, calls: &Rc<RefCell<EngineCalls>>) -> (Self, Rc<Cell<MarksSupport>>) {
            let marks = Rc::new(Cell::new(support));
            let engine = Self {
                calls: Rc::clone(calls),
                answer: EnginePoll::Idle,
                switch_block: None,
                grid: Rc::new(Cell::new(1)),
                marks: Rc::clone(&marks),
            };
            (engine, marks)
        }
    }

    impl AiEngine for RecordingEngine {
        fn id(&self) -> &'static str {
            "recording"
        }
        fn title(&self) -> String {
            "recording".to_string()
        }
        fn section(&self) -> EngineSection {
            EngineSection::WithoutPrompt
        }
        fn requires_torch(&self) -> bool {
            false
        }
        fn constraints(&self) -> FrameConstraints {
            FrameConstraints { multiple: self.grid.get(), min_side: 1, ..FrameConstraints::UNCONSTRAINED }
        }
        fn mask_layers(&self) -> Vec<MaskLayerSpec> {
            vec![MaskLayerSpec { tint: Color32::RED, label_key: "cleaning.region_frame.status.free" }]
        }
        fn allows_empty_mask(&self) -> bool {
            false
        }
        fn draw_parameters(&mut self, _ui: &mut egui::Ui) {
            self.calls.borrow_mut().parameter_draws += 1;
        }
        fn draw_progress(&mut self, _ui: &mut egui::Ui) {
            self.calls.borrow_mut().progress_draws += 1;
        }
        fn draw_run_options(&mut self, _ui: &mut egui::Ui, ctx: RunOptionsCtx) {
            self.calls.borrow_mut().run_options.push(ctx);
        }
        fn run_block_reason(&self) -> Option<String> {
            None
        }
        fn switch_block_reason(&self) -> Option<String> {
            self.switch_block.clone()
        }
        fn marks_support(&self) -> MarksSupport {
            self.marks.get()
        }
        fn start(&mut self, request: EngineRunRequest) -> Result<(), String> {
            let mut calls = self.calls.borrow_mut();
            calls.started = Some((request.page_idx, request.region.size[0], request.masks.len()));
            calls.run_region = Some(request.region);
            calls.run_marks = Some(request.marks);
            Ok(())
        }
        fn poll(&mut self, _ctx: &egui::Context) -> EnginePoll {
            self.calls.borrow_mut().polls += 1;
            std::mem::replace(&mut self.answer, EnginePoll::Idle)
        }
        fn cancel(&mut self) {
            self.calls.borrow_mut().cancels += 1;
        }
        fn set_backend_available(&mut self, available: bool) {
            self.calls.borrow_mut().backend = Some(available);
        }
        fn set_torch_available(&mut self, available: bool) {
            self.calls.borrow_mut().torch = Some(available);
        }
        fn set_region(&mut self, region: Option<OverlayRectPx>, geometry_settled: bool) {
            let mut calls = self.calls.borrow_mut();
            calls.region = Some(region);
            calls.geometry_settled = Some(geometry_settled);
        }
    }

    /// Installs a single `RecordingEngine` answering `answer`, and returns the tool plus the
    /// shared call record.
    fn tool_with_recording_engine(answer: EnginePoll) -> (RegionEditHost, Rc<RefCell<EngineCalls>>) {
        let calls = Rc::new(RefCell::new(EngineCalls::default()));
        let mut tool = test_host();
        tool.engines = vec![Box::new(RecordingEngine {
            calls: Rc::clone(&calls),
            answer,
            switch_block: None,
            grid: Rc::new(Cell::new(1)),
            marks: Rc::new(Cell::new(MarksSupport::OVERLAY_ONLY)),
        })];
        tool.selected = 0;
        (tool, calls)
    }

    /// Two engines can hold two independent multi-gigabyte downloads, so an engine that owns
    /// one closes the picker while it runs. The gate lives in `select_engine`, not only in
    /// the drawing code, because a switch must not depend on a control having been drawn.
    #[test]
    fn an_engine_that_calls_a_switch_unsafe_refuses_it() {
        let calls = Rc::new(RefCell::new(EngineCalls::default()));
        let busy = |reason: Option<&str>| RecordingEngine {
            calls: Rc::clone(&calls),
            answer: EnginePoll::Idle,
            switch_block: reason.map(str::to_string),
            grid: Rc::new(Cell::new(1)),
            marks: Rc::new(Cell::new(MarksSupport::OVERLAY_ONLY)),
        };
        let mut tool = test_host();
        tool.engines = vec![Box::new(busy(Some("downloading"))), Box::new(busy(None))];
        tool.selected = 0;

        // The reason the picker puts on the disabled tooltip — never an empty refusal.
        assert_eq!(tool.switch_block_reason().as_deref(), Some("downloading"));
        tool.select_engine(1);
        assert_eq!(tool.selected, 0, "a switch away from a downloading engine must be refused");

        // And the gate is a state, not a latch: it lifts as soon as the engine says so.
        tool.engines[0] = Box::new(busy(None));
        assert!(tool.switch_block_reason().is_none());
        tool.select_engine(1);
        assert_eq!(tool.selected, 1);
    }

    /// The regression this whole round exists to prevent: an engine that is never polled never
    /// drains its channels — a finished run is stranded, and FLUX.2 klein's settings saver,
    /// which lives inside `poll`, never writes, silently losing model paths and prompts on
    /// exit. The host polls once per frame, panel visible or not.
    #[test]
    fn the_selected_engine_is_polled_on_every_frame() {
        let ctx = egui::Context::default();
        let (mut tool, calls) = tool_with_recording_engine(EnginePoll::Idle);
        for _ in 0..3 {
            tool.poll_engine(&ctx);
        }
        assert_eq!(calls.borrow().polls, 3, "one poll per frame, unconditionally");
    }

    /// The three sections of the main panel reach the engine's three draw methods, each
    /// exactly once — the progress is drawn ONLY in the pinned top, never again inside the
    /// scrolled parameters.
    #[test]
    fn each_main_panel_section_draws_its_own_engine_part() {
        let ctx = egui::Context::default();
        let (mut tool, calls) = tool_with_recording_engine(EnginePoll::Idle);
        ctx.run_ui(egui::RawInput::default(), |ui| {
            tool.draw_main_panel_top(ui);
            tool.draw_main_panel(ui);
            tool.draw_main_panel_bottom(ui);
        })
        .drop_without_applying_deltas();
        let calls = calls.borrow();
        assert_eq!((calls.progress_draws, calls.parameter_draws, calls.run_options.len()), (1, 1, 1));
    }

    /// `mask_painted` is the MASK stack's answer: marks alone leave it `false` (they never
    /// permit an edit), and one painted mask pixel turns it `true`.
    #[test]
    fn the_run_options_learn_whether_a_mask_is_painted() {
        let ctx = egui::Context::default();
        let (mut tool, calls) = tool_with_recording_engine(EnginePoll::Idle);
        tool.frame.place_for_test(0, rect(0, 0, 4, 4));
        let draw = |tool: &mut RegionEditHost| {
            ctx.run_ui(egui::RawInput::default(), |ui| tool.draw_main_panel_bottom(ui))
                .drop_without_applying_deltas();
        };
        draw(&mut tool);
        tool.frame.masks_mut().paint_marks_segment((1, 1), (1, 1), 1, Color32::RED, false);
        draw(&mut tool);
        tool.frame.masks_mut().paint_segment((2, 2), (2, 2), 1, false);
        draw(&mut tool);
        let painted: Vec<bool> = calls.borrow().run_options.iter().map(|ctx| ctx.mask_painted).collect();
        assert_eq!(painted, vec![false, false, true]);
    }

    /// A failed run releases the frame and is reported to the user; it must not leave the
    /// frame locked on work that is over.
    #[test]
    fn a_failed_run_releases_the_frame_and_is_reported() {
        let ctx = egui::Context::default();
        let (mut tool, _calls) = tool_with_recording_engine(EnginePoll::Failed("boom".to_string()));
        tool.frame.set_processing(true);
        tool.poll_engine(&ctx);
        assert_eq!(tool.frame.lock(), FrameLock::Free);
        let message = tool.message.as_ref().expect("a failure must be shown");
        assert!(message.error && message.text == "boom", "{message:?}");
    }

    /// «Отменить» must reach the ENGINE, not only the frame: a run left going would keep a
    /// worker and a backend job alive and would answer about a frame the user released.
    #[test]
    fn cancelling_reaches_the_engine() {
        let (mut tool, calls) = tool_with_recording_engine(EnginePoll::Running);
        tool.frame.set_processing(true);
        tool.cancel_run();
        assert_eq!(calls.borrow().cancels, 1);
        assert_eq!(tool.frame.lock(), FrameLock::Free);
    }

    /// D7 on the ENGINE's answer: a result that is not exactly the frame rectangle never
    /// becomes a pending result, because applying it would nearest-rescale it over the page.
    #[test]
    fn an_engine_answer_of_the_wrong_size_is_refused() {
        let mut tool = test_host();
        tool.frame.set_processing(true);
        tool.accept_result(egui::ColorImage::filled([8, 8], Color32::WHITE));
        assert!(tool.frame.result().is_none(), "an unplaced frame has no rectangle to match");
        assert!(tool.message.as_ref().is_some_and(|message| message.error));
    }

    // -----------------------------------------------------------------------------------
    // The marks mode
    // -----------------------------------------------------------------------------------

    /// Every mode with the reference and the layer, preferring `preferred`.
    fn all_modes(preferred: MarksMode) -> MarksSupport {
        MarksSupport::new(preferred)
            .with(MarksMode::OverlayOnRegion)
            .with(MarksMode::SeparateReference)
            .with(MarksMode::TransparentLayer)
    }

    /// The user's choice stands while it is supported; an unsupported one becomes the
    /// engine's preferred mode.
    #[test]
    fn the_marks_mode_stays_inside_the_engines_support() {
        let layer_first = all_modes(MarksMode::TransparentLayer);
        assert_eq!(reconcile_marks_mode(MarksMode::SeparateReference, layer_first), MarksMode::SeparateReference);
        let reference_only = MarksSupport::new(MarksMode::SeparateReference);
        assert_eq!(reconcile_marks_mode(MarksMode::TransparentLayer, reference_only), MarksMode::SeparateReference);
        assert_eq!(reconcile_marks_mode(MarksMode::TransparentLayer, MarksSupport::OVERLAY_ONLY), MarksMode::OverlayOnRegion);
    }

    /// A 2x1 region (white, grey) and a marks layer with an opaque red mark on pixel 0.
    fn region_and_marks() -> (egui::ColorImage, Vec<u8>) {
        let region = egui::ColorImage::new([2, 1], vec![Color32::WHITE, Color32::from_rgb(100, 100, 100)]);
        let marks = vec![255, 0, 0, 255, 0, 0, 0, 0];
        (region, marks)
    }

    /// Each mode packages the marks the way the contract says, and an empty layer sends the
    /// region untouched whatever the mode.
    #[test]
    fn the_marks_are_packaged_per_mode() {
        for mode in MarksMode::ALL {
            let (region, marks) = region_and_marks();
            let (sent, extra) = prepare_run_marks(mode, region.clone(), &marks, true).expect("an empty layer always packages");
            assert_eq!(sent, region, "{mode:?}: nothing drawn, nothing changed");
            assert!(matches!(extra, RunMarks::None), "{mode:?}");
        }

        let (region, marks) = region_and_marks();
        let (sent, extra) = prepare_run_marks(MarksMode::OverlayOnRegion, region.clone(), &marks, false).expect("overlay");
        assert_eq!(sent.pixels, vec![Color32::RED, region.pixels[1]], "composited onto the region");
        assert!(matches!(extra, RunMarks::None));

        let (sent, extra) = prepare_run_marks(MarksMode::SeparateReference, region.clone(), &marks, false).expect("reference");
        assert_eq!(sent, region, "the edited region stays clean");
        let RunMarks::Reference(reference) = extra else { panic!("a reference was expected") };
        assert_eq!(reference.pixels, vec![Color32::RED, region.pixels[1]]);

        let (sent, extra) = prepare_run_marks(MarksMode::TransparentLayer, region.clone(), &marks, false).expect("layer");
        assert_eq!(sent, region, "the edited region stays clean");
        let RunMarks::Layer(layer) = extra else { panic!("a layer was expected") };
        assert_eq!((layer.width(), layer.height()), (2, 1));
        assert_eq!(layer.as_raw(), &marks, "the straight-alpha bytes travel as they are");

        for mode in MarksMode::ALL {
            let (region, marks) = region_and_marks();
            assert!(prepare_run_marks(mode, region, &marks[..4], false).is_err(), "{mode:?}: a layer of the wrong size is refused");
        }
    }

    /// A switch starts from the new engine's preference; afterwards the user's choice stands
    /// until the engine stops supporting it, and is then moved to the preferred mode.
    #[test]
    fn the_marks_mode_follows_engine_switches_and_support_changes() {
        let calls = Rc::new(RefCell::new(EngineCalls::default()));
        let (overlay_engine, _) = RecordingEngine::with_marks(MarksSupport::OVERLAY_ONLY, &calls);
        let (flexible, support) = RecordingEngine::with_marks(all_modes(MarksMode::TransparentLayer), &calls);
        let mut tool = test_host();
        tool.engines = vec![Box::new(overlay_engine), Box::new(flexible)];
        tool.selected = 0;
        tool.marks_mode = MarksMode::OverlayOnRegion;

        tool.select_engine(1);
        assert_eq!(tool.marks_mode, MarksMode::TransparentLayer, "the new engine's preference");
        tool.marks_mode = MarksMode::SeparateReference;
        tool.push_engine_rules();
        assert_eq!(tool.marks_mode, MarksMode::SeparateReference, "a supported choice stands");

        support.set(MarksSupport::new(MarksMode::TransparentLayer).with(MarksMode::OverlayOnRegion));
        tool.push_engine_rules();
        assert_eq!(tool.marks_mode, MarksMode::TransparentLayer, "an unsupported choice falls back to the preference");

        tool.select_engine(0);
        assert_eq!(tool.marks_mode, MarksMode::OverlayOnRegion);
    }

    /// The run hands the engine the marks in the selected mode, and the frame's own layers are
    /// not consumed by it.
    #[test]
    fn a_run_carries_the_marks_in_the_selected_mode() {
        let calls = Rc::new(RefCell::new(EngineCalls::default()));
        let (engine, _) = RecordingEngine::with_marks(all_modes(MarksMode::TransparentLayer), &calls);
        let mut tool = test_host();
        tool.engines = vec![Box::new(engine)];
        tool.selected = 0;
        tool.marks_mode = MarksMode::TransparentLayer;
        tool.frame.place_for_test(0, rect(0, 0, 4, 4));
        tool.frame.masks_mut().paint_marks_segment((1, 1), (1, 1), 1, Color32::RED, false);
        tool.frame.set_processing(true);

        let region = egui::ColorImage::filled([4, 4], Color32::WHITE);
        tool.hand_region_to_engine(PendingLoad { job_id: 1, page_idx: 0, rect: rect(0, 0, 4, 4), purpose: LoadPurpose::Run }, region.clone());

        let calls = calls.borrow();
        assert_eq!(calls.run_region.as_ref(), Some(&region), "a layer-mode run keeps the region clean");
        let Some(RunMarks::Layer(layer)) = calls.run_marks.as_ref() else { panic!("the marks travel as a layer: {:?}", calls.run_marks) };
        assert_eq!(layer.get_pixel(1, 1).0, [255, 0, 0, 255], "the mark the frame holds");
        assert!(!tool.frame.masks().marks_is_empty(), "the frame keeps its marks");
    }

    /// The default (overlay-only) engine gets the marks composited into its region.
    #[test]
    fn a_default_engine_gets_the_marks_inside_the_region() {
        let calls = Rc::new(RefCell::new(EngineCalls::default()));
        let (engine, _) = RecordingEngine::with_marks(MarksSupport::OVERLAY_ONLY, &calls);
        let mut tool = test_host();
        tool.engines = vec![Box::new(engine)];
        tool.selected = 0;
        tool.marks_mode = MarksMode::OverlayOnRegion;
        tool.frame.place_for_test(0, rect(0, 0, 4, 4));
        tool.frame.masks_mut().paint_marks_segment((0, 0), (0, 0), 1, Color32::RED, false);
        tool.frame.set_processing(true);

        tool.hand_region_to_engine(
            PendingLoad { job_id: 1, page_idx: 0, rect: rect(0, 0, 4, 4), purpose: LoadPurpose::Run },
            egui::ColorImage::filled([4, 4], Color32::WHITE),
        );
        let calls = calls.borrow();
        let sent = calls.run_region.as_ref().expect("the run started");
        assert_eq!(sent.pixels[0], Color32::RED, "the mark is part of the picture");
        assert_eq!(sent.pixels[3 * 4 + 3], Color32::WHITE, "an unmarked pixel is untouched");
        assert!(matches!(calls.run_marks, Some(RunMarks::None)));
    }

    /// Marks are instructions, never content: a marked pixel the engine handed back untouched
    /// gets the original pixel back, a pixel the engine changed is kept, and an unmarked pixel
    /// is never touched.
    #[test]
    fn untouched_marked_pixels_are_restored_from_the_original() {
        let gray = Color32::from_rgb(100, 100, 100);
        let green = Color32::from_rgb(0, 200, 0);
        // Pixel 0: marked, untouched. 1: marked, repainted by the engine. 2: unmarked,
        // repainted. 3: unmarked, untouched. 4: marked with the colour already under it.
        let original = egui::ColorImage::new([5, 1], vec![Color32::WHITE, Color32::WHITE, gray, gray, Color32::RED]);
        let sent = egui::ColorImage::new([5, 1], vec![Color32::RED, Color32::RED, gray, gray, Color32::RED]);
        let mut result = egui::ColorImage::new([5, 1], vec![Color32::RED, green, green, gray, Color32::RED]);
        let restored = restore_untouched_marked_pixels(&mut result, &original, &sent);
        assert_eq!(result.pixels, vec![Color32::WHITE, green, green, gray, Color32::RED]);
        assert_eq!(restored, 1);

        let mut wrong = egui::ColorImage::new([4, 1], vec![Color32::RED; 4]);
        assert_eq!(restore_untouched_marked_pixels(&mut wrong, &original, &sent), 0, "a size mismatch changes nothing");
        assert_eq!(wrong.pixels, vec![Color32::RED; 4]);
    }

    /// End to end through the host: an overlay-mode run sends the marks inside the region, and
    /// the accepted result carries none of the marks the engine did not paint over.
    #[test]
    fn an_overlay_run_never_merges_untouched_marks_into_the_result() {
        let calls = Rc::new(RefCell::new(EngineCalls::default()));
        let (engine, _) = RecordingEngine::with_marks(MarksSupport::OVERLAY_ONLY, &calls);
        let mut tool = test_host();
        tool.engines = vec![Box::new(engine)];
        tool.selected = 0;
        tool.marks_mode = MarksMode::OverlayOnRegion;
        tool.frame.place_for_test(0, rect(0, 0, 4, 4));
        tool.frame.masks_mut().paint_marks_boxes(&[(0, 0, 2, 1)], Color32::RED, false);
        tool.frame.set_processing(true);
        tool.hand_region_to_engine(
            PendingLoad { job_id: 1, page_idx: 0, rect: rect(0, 0, 4, 4), purpose: LoadPurpose::Run },
            egui::ColorImage::filled([4, 4], Color32::WHITE),
        );
        let mut answer = calls.borrow().run_region.clone().expect("the run started");
        assert_eq!(answer.pixels[0], Color32::RED, "the engine saw the marks");
        // The engine repaints pixel 1 and leaves pixel 0 as it got it.
        answer.pixels[1] = Color32::BLUE;
        tool.accept_result(answer);

        let result = tool.frame.result().expect("the result is pending").image();
        assert_eq!(result.pixels[0], Color32::WHITE, "an untouched mark is taken back out");
        assert_eq!(result.pixels[1], Color32::BLUE, "what the engine painted stays");
        assert!(tool.marks_restore.is_none(), "the restore data goes with the run");
    }

    /// A sampling the selector can no longer finish is cancelled, and the colour it started
    /// from comes back.
    #[test]
    fn an_undrawn_selector_cancels_its_eyedropper() {
        let mut tool = test_host();
        tool.frame.set_paint_target(PaintTarget::Marks);
        let start = tool.frame.marks_color();
        // Nothing drew the selector this frame, so nothing is pending and nothing changes.
        tool.push_engine_rules();
        assert_eq!(tool.frame.marks_color(), start);
        assert!(!tool.marks_color_selector.eyedropper_active());
    }

    /// The mode switch only ever happens in marks mode; the idle host keeps the canvas zoom,
    /// and so does a marks-mode frame whose body was never hovered.
    #[test]
    fn the_marks_mode_does_not_block_the_canvas_zoom_by_itself() {
        let mut tool = test_host();
        tool.frame.set_paint_target(PaintTarget::Marks);
        assert!(!tool.block_canvas_zoom_on_ctrl_primary(), "not hovered, nothing in flight");
        assert!(!tool.block_canvas_zoom(), "D5 still holds in marks mode");
    }

    /// The wheel and the size shortcuts outside the frame resize the brush IN USE.
    #[test]
    fn the_wheel_resizes_the_active_brush() {
        let mut tool = test_host();
        let shift = egui::Modifiers { shift: true, ..egui::Modifiers::NONE };
        tool.frame.set_paint_target(PaintTarget::Marks);
        let marks_before = tool.frame.active_brush_mut().radius_px();
        assert!(tool.on_wheel_event(400.0, shift));
        let marks_after = tool.frame.active_brush_mut().radius_px();
        assert!(marks_after > marks_before, "{marks_before} -> {marks_after}");
        tool.frame.set_paint_target(PaintTarget::Mask);
        assert_eq!(tool.frame.active_brush_mut().radius_px(), ms_tools::MaskBrush::default().radius_px(), "the mask brush is untouched");
    }
}
