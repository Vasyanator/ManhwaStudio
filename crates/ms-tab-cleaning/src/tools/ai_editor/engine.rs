/*
File: ai_editor/engine.rs

Purpose:
The contract between the «ИИ-редактор области» HOST and the AI engines it hosts. The host
owns the on-canvas `RegionFrame` — the rectangle, the mask stack, the pending result, the
lock and Применить/Отменить; an engine owns everything model-specific — its parameters, its
settings file, its wire protocol, its worker thread and its own progress bar.

Key structures:
- `AiEngine`: the trait every engine implements
- `EngineSection`: which picker section («Без промпта» / «С промптом») an engine appears in
- `MaskLayerSpec`: one mask layer an engine wants painted (re-exported from the framework)
- `EngineRunRequest`: everything the host hands an engine to start one run
- `EnginePoll`: what one `poll` says about the run in flight

Notes:
An engine never sees `CanvasView`, `ProjectData`, the frame or an `egui::Context` outside
`poll`; its whole UI surface is a plain `&mut egui::Ui`. There is deliberately NO shared
progress vocabulary (`dev-docs/region_edit_v2_plan.md` §13.2 D13): engines disagree about
what progress even is, so each draws its own bar inside its own parameter panel and the host
learns only Running / Done / Failed. A run's answer is PIXELS and only pixels (D12).
*/

use super::super::region_edit_v2::geometry::FrameConstraints;
use ms_canvas::OverlayRectPx;
use eframe::egui;

/// Which picker section an engine appears in.
///
/// The sections are drawn in the order of this enum and a section with no engines is not
/// drawn at all. An engine belongs to exactly one section; an engine whose MODES straddle
/// both (FLUX.1 Fill) picks the section of its default mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineSection {
    /// Engines that need no text prompt at all.
    WithoutPrompt,
    /// Engines whose run is driven by a prompt.
    WithPrompt,
}

/// One mask layer an engine wants the user to paint.
///
/// Re-exported from the framework rather than redeclared: the host builds the frame's
/// `MaskStack` straight from what `mask_layers()` returns, so a second, engine-side twin of
/// the type could only ever drift from the one the frame consumes. Layers are declared in
/// painting order — a later layer wins where two overlap, in the preview and in the run
/// request alike — and the layer count is the LENGTH of `mask_layers()` and nothing else.
pub use super::super::region_edit_v2::layers::MaskLayerSpec;

/// Everything the host hands an engine to start one run.
///
/// The host guarantees the SIZES: `region` is exactly `rect_px.w * rect_px.h` pixels, and
/// `masks` holds exactly one L8 buffer per declared mask layer, each of exactly
/// `rect_px.w * rect_px.h` bytes. An engine still validates them — a request that fails the
/// check is refused by `AiEngine::start` rather than encoded onto the wire.
#[derive(Debug, Clone)]
pub struct EngineRunRequest {
    /// Index of the page the frame sits on, for logs and for a result the host must place
    /// back on the page it came from.
    pub page_idx: usize,
    /// The frame rectangle in SOURCE PAGE PIXELS — the unit
    /// `CanvasView::replace_overlay_region_px` consumes.
    pub rect_px: OverlayRectPx,
    /// Source page crop composited with the current clean overlay, exactly `rect_px` in
    /// size. Never the bare overlay chunk: a page with no overlay would otherwise hand the
    /// model pure transparency (`dev-docs/region_edit_v2_plan.md` §13.2 D14).
    pub region: egui::ColorImage,
    /// One L8 buffer per declared mask layer, in `mask_layers()` order, each
    /// `rect_px.w * rect_px.h` bytes and holding only `0` or `255`.
    pub masks: Vec<Vec<u8>>,
}

/// What one `poll` says about the run in flight.
///
/// `Done` and `Failed` are TERMINAL and are reported exactly once: the engine has dropped
/// the run by the time it returns either, and the next poll answers `Idle`. `Done` carries a
/// `ColorImage` of exactly the requested `rect_px` — the host refuses any other size rather
/// than rescaling it (D7).
#[derive(Debug, Clone)]
pub enum EnginePoll {
    /// No run has been started, or the last one has already been reported.
    Idle,
    /// A run is in flight; the engine's own panel shows how far it has got.
    Running,
    /// The run finished. The image is exactly `rect_px` in size.
    Done(egui::ColorImage),
    /// The run failed, with an already-localized message for the user.
    Failed(String),
}

/// One AI engine the «ИИ-редактор области» tool can host.
///
/// Ownership boundary: the engine owns its parameters, its persistence, its wire protocol,
/// its worker threads and its progress reporting; the host owns the rectangle, the mask
/// stack, the pending result and the apply path. An engine never touches `CanvasView`,
/// `ProjectData` or the frame, and never blocks the GUI thread — every call below must
/// return within a frame.
pub trait AiEngine {
    /// Stable identifier, used for widget ids and logs. Never shown to the user.
    fn id(&self) -> &'static str;

    /// Localized name for the engine picker, resolved at draw time.
    fn title(&self) -> String;

    /// Which picker section the engine appears in.
    fn section(&self) -> EngineSection;

    /// Whether the engine needs the PyTorch runtime, which gates its run button.
    fn requires_torch(&self) -> bool;

    /// Size requirements the engine imposes on the frame rectangle, in source page pixels.
    ///
    /// The host hands these to the frame, which snaps a DRAGGED rectangle to them and marks a
    /// rectangle that does not satisfy them as invalid — «Обработать» is refused while it does
    /// not, so a rect that reaches `start` has already satisfied every one of them. A frame
    /// that was valid for the previous engine and is not valid for this one simply turns red;
    /// it is never resized behind the user's back.
    fn constraints(&self) -> FrameConstraints;

    /// The mask layers the engine wants painted, in painting order. Must not be empty.
    fn mask_layers(&self) -> Vec<MaskLayerSpec>;

    /// Whether a run with every mask layer empty is meaningful — for FLUX.2 klein it is,
    /// because an empty mask IS its whole-region working mode. The host relaxes the frame's
    /// non-empty-mask rule exactly when this is `true`, tells the user so with its own
    /// green line under «Обработать», and re-reads the answer whenever the engine's
    /// parameters change, because an engine may make it depend on one of them.
    fn allows_empty_mask(&self) -> bool;

    /// Draws the engine's parameters, its own progress bar and its engine-specific status —
    /// the body of the left «Редактор области» panel.
    ///
    /// A plain `&mut Ui`: an engine never touches `CanvasView`, `ProjectData` or the frame.
    /// The panel may not be visible on a given frame, so nothing this method does may be a
    /// precondition of `poll`.
    fn draw_parameters(&mut self, ui: &mut egui::Ui);

    /// Why a run is refused right now, localized; `None` when it may start.
    ///
    /// Only the engine's OWN half of the gate: the rectangle is already validated against
    /// `constraints()` by the frame, and the non-empty-mask rule is `allows_empty_mask()`.
    fn run_block_reason(&self) -> Option<String>;

    /// Why switching AWAY from this engine is unsafe right now, localized; `None` when the
    /// picker may switch freely. Default: `None`.
    ///
    /// The host disables the engine picker while a reason stands and puts it on the
    /// disabled tooltip, so a refusal always says what it is waiting for.
    ///
    /// It exists because the frame lock — the picker's other gate — only covers a RUN. An
    /// engine may own work that no frame state describes: FLUX.2 klein's model download is
    /// a multi-gigabyte transfer with its own free-space budget, and nothing else would
    /// stop a user from starting a second one from the other engine and overcommitting the
    /// disk. An engine that owns no such work leaves the default.
    ///
    /// Reported, never enforced silently: this only closes a control, it does not abort
    /// anything, and it must stay cheap enough to call every frame.
    fn switch_block_reason(&self) -> Option<String> {
        None
    }

    /// Starts one run. The engine takes over `request` and reports through `poll`.
    ///
    /// # Errors
    /// Returns a localized message when a run is already in flight or when `request` fails
    /// the size checks the host is supposed to guarantee.
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String>;

    /// Called every frame while the tool is active, panel visible or not — this is where an
    /// engine drains its channels, so skipping it strands a finished run.
    fn poll(&mut self, ctx: &egui::Context) -> EnginePoll;

    /// Abandons the run in flight, if any. A no-op when nothing is running.
    fn cancel(&mut self);

    /// Tells the engine whether the AI backend process is reachable.
    fn set_backend_available(&mut self, available: bool);

    /// Tells the engine whether the PyTorch runtime is present.
    fn set_torch_available(&mut self, available: bool);

    /// Publishes the frame's current rectangle, `None` while the tool has no frame.
    ///
    /// Called every frame, like the two setters above. It exists because an engine's
    /// parameter panel legitimately depends on the region BEFORE a run — FLUX.2 klein asks
    /// the backend for a RAM/VRAM forecast of exactly this rectangle and prints its size —
    /// and neither `EngineRunRequest` (which only exists once a run starts) nor
    /// `draw_parameters` (which may not be drawn at all) can supply it. An engine that does
    /// not care leaves the body empty.
    ///
    /// `geometry_settled` is `false` while a frame gesture is in flight — a move drag, a
    /// resize drag or a mask stroke. The rectangle is THIS frame's either way and must keep
    /// being DISPLAYED live, but an engine must not START WORK off an unsettled geometry: a
    /// resize drag publishes a new rectangle on every rendered frame, so an engine that
    /// queried the backend on each of them would issue one request per frame. Defer such
    /// work to the first call that reports `true`.
    fn set_region(&mut self, region: Option<OverlayRectPx>, geometry_settled: bool);
}
