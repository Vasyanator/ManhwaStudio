/*
File: ai_editor/mod.rs

Purpose:
The «ИИ-редактор области» cleaning tool: the first consumer of the on-canvas region-editing
framework (`../region_edit_v2/`). It owns a `RegionFrame` with TWO mask layers, drives it from
`CleaningTool::draw_overlay_ui`, and performs the two actions the frame is not allowed to
perform itself — running the consumer and merging its result into the clean overlay.

In step 1 the consumer is a labelled PLACEHOLDER (`stub.rs`): there is no AI backend here, no
IPC method and no model. The tool exists so the whole path — paint, process, preview, apply,
undo — runs for real while the framework is still young.

Main responsibilities:
- own the frame and hand it the dock-panel rects and the page count every frame
- turn a `FrameOutcome` into work: process, apply, cancel, clear
- refuse a result whose size is not exactly the frame rect (D7), with a message and a log
- draw the two panel bodies: the compact brush controls and the main region panel

Key structures:
- `AiEditorTool`: the `CleaningTool` implementation
- `StubLayer`: what one mask layer means — its preview tint, its placeholder fill, its name
- `ApplyError`: why a pending result could not be merged into the clean overlay
- `CaptureError`: why the clean-overlay pixels under the frame could not be captured

Key functions:
- `AiEditorTool::run_stub()`, `apply_result()`: the two `&mut CanvasView` actions
- `capture_base()`: the clean base under the frame; transparent ONLY for a page with no overlay
- `check_result_fits()`: the D7 size check, pure and unit-tested

Notes:
`block_canvas_zoom()` stays `false` (D5): that flag also disables the clean-overlay undo
shortcuts for the whole session. Blocking is precise instead — `captures_canvas_pointer` over
the frame's hitbox and `block_canvas_drag_scroll_on_primary` while a frame drag is in flight.
Canvas drag-scroll additionally needs Space held (`canvas/scene.rs`), so mask painting can
never scroll the page out from under the brush.
Design and the decisions behind it: `dev-docs/region_edit_v2_plan.md`.
*/

mod stub;

use super::base::{CleaningTool, StrokePoint, capture_overlay_chunk, overlay_rect_to_scene_rect};
use super::region_edit_v2::frame::{FrameHost, FrameLock, RegionFrame};
use super::region_edit_v2::geometry::{FrameConstraints, SizeViolation};
use super::region_edit_v2::layers::ResultLayer;
use crate::canvas::{CanvasView, OverlayRectPx};
use crate::project::ProjectData;
use crate::widgets::WheelSlider;
use eframe::egui;
use egui::{Color32, Pos2};

/// Size requirements the PLACEHOLDER consumer imposes on the frame.
///
/// They are the shape a real inpainting model imposes — a latent-friendly multiple, a useful
/// shortest side, a memory ceiling and an aspect ceiling — with values chosen so the frame's
/// snapping and its red "invalid size" state can be exercised. They are handed to
/// `RegionFrame::new` once, at construction: the frame has no setter for them, and a step-2
/// consumer that must switch constraints at run time adds one together with its caller
/// (`dev-docs/region_edit_v2_plan.md` §11).
const STUB_CONSTRAINTS: FrameConstraints = FrameConstraints {
    multiple: 8,
    min_side: 64,
    max_area: Some(4 * 1024 * 1024),
    max_aspect: Some(8.0),
};

/// Smallest and largest brush radius, in region pixels, the compact panel offers.
///
/// `MaskBrush` clamps to its own range anyway, so these bound the SLIDER, not the brush; they
/// are kept equal to that range so the slider cannot present a value the brush would refuse.
const BRUSH_RADIUS_MIN_PX: usize = 1;
const BRUSH_RADIUS_MAX_PX: usize = 200;

/// What one mask layer of this tool means.
///
/// The three facts live together because they must not drift apart: the preview the user
/// paints with, the colour the placeholder writes for it, and the name both panels show. The
/// LENGTH of `AI_EDITOR_LAYERS` is also the frame's layer count.
#[derive(Debug, Clone, Copy)]
struct StubLayer {
    /// Catalog key of the layer's name. Resolved at draw time, never cached, so the name
    /// follows a language switch.
    name_key: &'static str,
    /// Colour of the layer's translucent preview inside the frame.
    tint: Color32,
    /// Colour the placeholder writes into the result where this layer is painted.
    fill: Color32,
}

/// The two mask layers of the step-1 tool, in painting order: a later layer wins where two
/// overlap, in the preview and in the placeholder result alike.
const AI_EDITOR_LAYERS: [StubLayer; 2] = [
    StubLayer {
        name_key: "cleaning.tools.area_editor.layer_white",
        tint: Color32::from_rgb(90, 180, 255),
        fill: Color32::WHITE,
    },
    StubLayer {
        name_key: "cleaning.tools.area_editor.layer_grey",
        tint: Color32::from_rgb(255, 150, 80),
        fill: Color32::from_rgb(128, 128, 128),
    },
];

/// Localized name of layer `idx`, or its index when the tool is asked about a layer it does
/// not define (unreachable while the frame is built from `AI_EDITOR_LAYERS`).
#[must_use]
fn layer_name(idx: usize) -> String {
    AI_EDITOR_LAYERS.get(idx).map_or_else(
        || (idx + 1).to_string(),
        |layer| ms_i18n::lookup(layer.name_key).unwrap_or(layer.name_key).to_string(),
    )
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
/// A capture failure is a real error, never a silent fallback: the ONLY case that legitimately
/// yields a transparent base is a page with no clean overlay allocated at all, which is that
/// page's true clean state (`dev-docs/region_edit_v2_plan.md` §11). Every other outcome would
/// mean applying the result wipes real clean-overlay pixels with transparency, unnoticed.
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

/// A line the tool shows under its main panel's status line.
#[derive(Debug, Clone)]
struct ToolMessage {
    /// Already localized text. Stored resolved because it carries run-time numbers; it is
    /// replaced on the next action, so a language switch cannot leave it stale for long.
    text: String,
    /// Whether it reports a failure, which is the only thing that decides its colour.
    error: bool,
}

/// The step-1 area editor: an on-canvas `RegionFrame` plus a placeholder consumer.
#[derive(Debug)]
pub struct AiEditorTool {
    frame: RegionFrame,
    /// Dock-panel rects of THIS frame, handed over by the tab before `draw_overlay_ui`. They
    /// are cut out of the viewport so the frame never hides behind a panel.
    panel_rects: Vec<egui::Rect>,
    message: Option<ToolMessage>,
}

impl Default for AiEditorTool {
    fn default() -> Self {
        Self {
            frame: RegionFrame::new(STUB_CONSTRAINTS, &AI_EDITOR_LAYERS.map(|layer| layer.tint)),
            panel_rects: Vec::new(),
            message: None,
        }
    }
}

impl AiEditorTool {
    /// Whether the mask may still be edited: no result waits and nothing is running.
    ///
    /// Editing the mask under a pending result would make that result describe a mask that no
    /// longer exists, which is the same rule the frame applies to painting.
    #[must_use]
    fn mask_editable(&self) -> bool {
        match self.frame.lock() {
            FrameLock::Free | FrameLock::MaskPainted => true,
            FrameLock::ResultPending | FrameLock::Processing => false,
        }
    }

    /// Records a message the main panel shows as ordinary text.
    fn report_info(&mut self, text: String) {
        self.message = Some(ToolMessage { text, error: false });
    }

    /// Records a failure: `text` is what the user reads, `detail` is the technical
    /// reason that goes to the log alone.
    ///
    /// The two are separate on purpose. `detail` is an English `Display` of a typed error and
    /// must not leak into a message shown under a French interface, while the user-facing
    /// half must not carry indices and buffer lengths nobody outside the code can act on.
    fn report_error(&mut self, text: String, detail: &dyn std::fmt::Display) {
        crate::runtime_log::log_warn(format!("[cleaning/ai_editor] {text} | {detail}"));
        self.message = Some(ToolMessage { text, error: true });
    }

    /// The clean-overlay pixels under `rect` of page `page_idx`.
    ///
    /// A page whose clean overlay has not been allocated yet has no clean pixels at all, so the
    /// base is fully transparent — that is the page's true state, not a stand-in, and it is the
    /// only case in which a transparent base is produced. Once an overlay EXISTS, a capture
    /// that fails or comes back the wrong size is an error: substituting transparency there
    /// would make the applied result erase real clean pixels with nothing, silently.
    ///
    /// # Errors
    /// [`CaptureError::NotMapped`] when the page is not laid out or the region does not map
    /// onto its overlay, and [`CaptureError::ChunkSize`] when the captured chunk is not exactly
    /// the region size. (`build_stub_result` re-checks the chunk against the MASK STACK's size;
    /// this check is against the frame's rectangle and carries the page index into the log.)
    fn capture_base(canvas: &CanvasView, page_idx: usize, rect: OverlayRectPx) -> Result<egui::ColorImage, CaptureError> {
        let not_mapped = CaptureError::NotMapped {
            page: page_idx,
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
        };
        let Some([overlay_w, overlay_h]) = canvas.overlay_size(page_idx) else {
            return Ok(egui::ColorImage::filled([rect.w, rect.h], Color32::TRANSPARENT));
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
        Ok(chunk)
    }

    /// Runs the placeholder consumer and hands the frame its result.
    ///
    /// Synchronous on purpose: the placeholder is one image clone plus one pass per mask layer
    /// over the region, with no decode, no file and no network, so it cannot block the GUI
    /// thread (D9). The real consumer will run on a worker and report through a channel.
    fn run_stub(&mut self, canvas: &CanvasView) {
        let (Some(page_idx), Some(rect)) = (self.frame.page_idx(), self.frame.rect_px()) else {
            self.report_error(
                t!("cleaning.tools.area_editor.error_no_frame").to_string(),
                &"the frame has no page anchor or no rectangle",
            );
            return;
        };
        let base = match Self::capture_base(canvas, page_idx, rect) {
            Ok(base) => base,
            Err(error) => {
                self.report_error(tf!("cleaning.tools.area_editor.error_no_overlay", page = page_idx + 1), &error);
                return;
            }
        };
        let fills = AI_EDITOR_LAYERS.map(|layer| layer.fill);
        match stub::build_stub_result(&base, self.frame.masks(), &fills) {
            Ok(image) => {
                self.frame.set_result(Some(ResultLayer::new(image)));
                self.report_info(t!("cleaning.tools.area_editor.processed_status").to_string());
            }
            Err(error) => self.report_error(
                t!("cleaning.tools.area_editor.error_process_failed").to_string(),
                &error,
            ),
        }
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

    /// Draws the brush row of the compact panel: radius, paint/erase, undo and clear.
    fn draw_brush_controls(&mut self, ui: &mut egui::Ui) {
        let mut radius = self.frame.brush_mut().radius_px();
        if ui
            .add(
                WheelSlider::new(&mut radius, BRUSH_RADIUS_MIN_PX..=BRUSH_RADIUS_MAX_PX)
                    .text(t!("cleaning.common.size_label")),
            )
            .changed()
        {
            // The setter answers whether it changed anything after clamping; the slider is
            // rebuilt from the brush next frame either way, so the answer has no reader here.
            self.frame.brush_mut().set_radius_px(radius);
        }

        let mut erase = self.frame.erase();
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut erase, false, t!("cleaning.tools.area_editor.brush_paint_button"));
            ui.selectable_value(&mut erase, true, t!("cleaning.tools.area_editor.brush_erase_button"))
                .on_hover_text(t!("cleaning.tools.area_editor.brush_erase_hint"));
        });
        self.frame.set_erase(erase);
    }

    /// Draws the mask-layer picker of the compact panel.
    fn draw_layer_picker(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("cleaning.tools.area_editor.mask_layer_label"));
        let mut active = self.frame.masks().active();
        ui.horizontal_wrapped(|ui| {
            for idx in 0..self.frame.masks().layer_count() {
                ui.selectable_value(&mut active, idx, layer_name(idx));
            }
        });
        self.frame.masks_mut().set_active(active);
    }

    /// Draws the two mask actions of the compact panel: undo one stroke, erase everything.
    fn draw_mask_actions(&mut self, ui: &mut egui::Ui) {
        let editable = self.mask_editable();
        let has_mask = !self.frame.masks().is_empty();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    editable && has_mask,
                    egui::Button::new(t!("cleaning.tools.area_editor.undo_stroke_button")),
                )
                .clicked()
                && !self.frame.masks_mut().undo()
            {
                self.report_info(t!("cleaning.tools.area_editor.nothing_to_undo").to_string());
            }
            if ui
                .add_enabled(
                    editable && has_mask,
                    egui::Button::new(t!("cleaning.region_frame.button.clear_mask")),
                )
                .clicked()
            {
                self.frame.masks_mut().clear_all();
            }
        });
    }

    /// Draws «Применить» and «Отменить» — the two actions that resolve a pending result.
    ///
    /// The captions and the enablement come from the frame, and the presses go back through
    /// `RegionFrame::request_*`, so this panel and the frame's own button row can never
    /// disagree about what is allowed or perform two different things.
    fn draw_result_actions(&mut self, ui: &mut egui::Ui) {
        let enabled = self.frame.buttons();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(enabled.apply, egui::Button::new(t!("cleaning.region_frame.button.apply")))
                .clicked()
            {
                self.frame.request_apply();
            }
            if ui
                .add_enabled(enabled.cancel, egui::Button::new(t!("cleaning.region_frame.button.cancel")))
                .clicked()
            {
                self.frame.request_cancel();
            }
        });
    }

    /// Draws the frame's page, its rectangle and the active size requirements.
    fn draw_geometry_section(&self, ui: &mut egui::Ui) {
        let (Some(page_idx), Some(rect)) = (self.frame.page_idx(), self.frame.rect_px()) else {
            return;
        };
        ui.label(tf!(
            "cleaning.tools.area_editor.frame_geometry",
            page = page_idx + 1,
            x = rect.x,
            y = rect.y,
            w = rect.w,
            h = rect.h
        ));
        let constraints = self.frame.constraints();
        ui.small(tf!(
            "cleaning.tools.area_editor.constraint_multiple",
            multiple = constraints.multiple,
            min_side = constraints.min_side
        ));
        if let Some(max_area) = constraints.max_area {
            ui.small(tf!("cleaning.tools.area_editor.constraint_max_area", area = max_area));
        }
        if let Some(max_aspect) = constraints.max_aspect {
            ui.small(tf!("cleaning.tools.area_editor.constraint_max_aspect", aspect = max_aspect));
        }
        if let Some(violation) = self.frame.size_violation() {
            let text = match violation {
                SizeViolation::NotMultiple => t!("cleaning.tools.area_editor.violation_multiple"),
                SizeViolation::TooSmall => t!("cleaning.tools.area_editor.violation_min_side"),
                SizeViolation::AreaTooLarge => t!("cleaning.tools.area_editor.violation_max_area"),
                SizeViolation::AspectTooSteep => t!("cleaning.tools.area_editor.violation_aspect"),
            };
            ui.colored_label(ui.visuals().error_fg_color, text);
        }
    }

    /// Draws one row per mask layer with its painted-pixel count.
    fn draw_layers_section(&self, ui: &mut egui::Ui) {
        let masks = self.frame.masks();
        for idx in 0..masks.layer_count() {
            ui.small(tf!(
                "cleaning.tools.area_editor.layer_row",
                name = layer_name(idx),
                count = masks.layer_set_px(idx)
            ));
        }
    }
}

impl CleaningTool for AiEditorTool {
    fn tool_id(&self) -> &'static str {
        "ai_editor"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.area_editor.title")
    }

    /// The step-1 placeholder runs locally: it needs no Python backend and no Torch.
    fn pytorch_required(&self) -> bool {
        false
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        // The frame keeps its placement (`reset` does), but nothing it held may survive a tool
        // switch: an unapplied result would come back as a preview over a page the user has
        // meanwhile edited.
        self.frame.reset();
        self.message = None;
    }

    /// The compact part of the tool's interface, in «Выбранный инструмент».
    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.draw_brush_controls(ui);
        ui.separator();
        self.draw_layer_picker(ui);
        self.draw_mask_actions(ui);
        ui.separator();
        ui.small(t!("cleaning.tools.area_editor.main_panel_hint"));
    }

    fn wants_main_panel(&self) -> bool {
        true
    }

    /// The main part, in the «Редактор области» dock panel.
    ///
    /// It runs inside `CanvasView::draw` and therefore mutates only the tool: every action
    /// raises a flag the frame consumes at the top of the next pass, re-checked against the
    /// frame's own enablement table.
    ///
    /// «Применить» and «Отменить» are repeated here on purpose. A frame holding a pending
    /// result is LOCKED, and its own button row is only as wide as the frame is on screen, so
    /// on a zoomed-out strip the user could otherwise reach neither and would have no way to
    /// resolve the result at all.
    fn draw_main_panel(&mut self, ui: &mut egui::Ui) {
        ui.small(t!("cleaning.tools.area_editor.placeholder_notice"));
        ui.separator();
        self.draw_geometry_section(ui);
        ui.separator();
        self.draw_layers_section(ui);
        ui.separator();
        if ui
            .add_enabled(
                self.frame.buttons().process,
                egui::Button::new(t!("cleaning.tools.area_editor.process_button")),
            )
            .on_hover_text(t!("cleaning.tools.area_editor.process_hint"))
            .clicked()
        {
            self.frame.request_process();
        }
        self.draw_result_actions(ui);
        ui.label(self.frame.status_text());
        if let Some(message) = self.message.as_ref() {
            if message.error {
                ui.colored_label(ui.visuals().error_fg_color, &message.text);
            } else {
                ui.small(&message.text);
            }
        }
    }

    fn set_panel_rects(&mut self, rects: &[egui::Rect]) {
        self.panel_rects.clear();
        self.panel_rects.extend_from_slice(rects);
    }

    /// The frame's whole per-frame pass, plus whatever it asked for.
    ///
    /// This is the only hook that owns the context, the canvas and the project at once, which
    /// is why the pass lives here rather than in `draw_cursor` (§10.1 of the design).
    fn draw_overlay_ui(&mut self, ctx: &egui::Context, canvas: &mut CanvasView, project: &ProjectData) {
        let host = FrameHost {
            panel_rects: &self.panel_rects,
            page_count: project.pages.len(),
        };
        let outcome = self.frame.update(ctx, canvas, host);
        if outcome.clear_mask_requested {
            self.frame.masks_mut().clear_all();
        }
        if outcome.cancel_requested {
            self.frame.set_processing(false);
            self.frame.set_result(None);
            self.report_info(t!("cleaning.tools.area_editor.cancelled_status").to_string());
        }
        // Process before apply: the two are mutually exclusive by `FrameButtons`, and running
        // first keeps the order the buttons sit in.
        if outcome.process_requested {
            self.run_stub(canvas);
        }
        if outcome.apply_requested {
            self.apply_result(canvas);
        }
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

    /// Shift+wheel resizes the brush, through the frame's own `MaskBrush`.
    ///
    /// This hook covers the pointer OUTSIDE the frame only: `tab.rs::handle_active_tool_wheel`
    /// drops the event while the canvas pointer is occluded, and the frame occludes its own
    /// hitbox. Over the frame the identical gesture is handled inside the frame's pass.
    fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.frame.brush_mut().handle_wheel(delta_y, modifiers)
    }

    /// The region editor's brush-size shortcuts `-` / `=` / `+`, for the pointer OUTSIDE the
    /// frame — `tab.rs::handle_active_tool_hotkeys` is gated on the same occlusion test as the
    /// wheel, so over the frame the shortcuts are handled inside the frame's pass instead.
    ///
    /// The tab repaints on a `true`, which is what makes the new radius show up in the brush
    /// ring immediately.
    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        self.frame.brush_mut().handle_size_shortcuts(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: usize, y: usize, w: usize, h: usize) -> OverlayRectPx {
        OverlayRectPx { x, y, w, h }
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

    /// Every layer the tool defines must have a name the catalog can resolve, and the frame is
    /// built with exactly one preview tint per layer.
    #[test]
    fn every_layer_has_a_name_and_a_tint() {
        let tool = AiEditorTool::default();
        assert_eq!(tool.frame.masks().layer_count(), AI_EDITOR_LAYERS.len());
        for idx in 0..AI_EDITOR_LAYERS.len() {
            assert!(!layer_name(idx).is_empty());
        }
    }

    /// D5, pinned. `block_canvas_zoom()` does not only block zooming: `tab.rs` refuses the
    /// clean-overlay Ctrl+Z / Ctrl+Shift+Z shortcuts for any tool that returns `true`
    /// (`handle_history_hotkeys`, `src/tabs/cleaning/tab.rs:1744-1753`) and the zoom shortcuts
    /// with it (`zoom_by_shortcut` / `reset_zoom_shortcut`, `:888-908`). Ten of the twelve
    /// registered tools DO override it to `true`, so copying a sibling is the likely edit —
    /// and this tool lives on the canvas for the WHOLE editing session, so inheriting `true`
    /// would kill canvas zoom and clean-overlay undo for the session with nothing else failing.
    #[test]
    fn the_area_editor_never_blocks_canvas_zoom_or_the_undo_shortcuts() {
        let tool = AiEditorTool::default();
        assert!(!tool.block_canvas_zoom(), "D5: blocking is precise, never the whole canvas");
        assert!(!tool.block_canvas_zoom_on_ctrl_primary(), "the same reasoning, for the Ctrl+drag zoom");
        // Blocking is precise instead: only a live frame gesture stops canvas drag-scroll.
        assert!(!tool.block_canvas_drag_scroll_on_primary(), "an idle frame blocks nothing");
    }

    /// A transparent base is the page's TRUE clean state only while the page has no overlay
    /// allocated. Once one exists, substituting transparency for a failed capture would make
    /// apply erase real clean pixels with nothing, with no message and no log.
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
        let mut tool = AiEditorTool::default();
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
        let mut tool = AiEditorTool::default();
        assert!(tool.mask_editable());
        tool.frame.set_processing(true);
        assert!(!tool.mask_editable());
        tool.frame.set_processing(false);
        tool.frame
            .set_result(Some(ResultLayer::new(egui::ColorImage::filled([2, 2], Color32::WHITE))));
        assert!(!tool.mask_editable());
    }
}
