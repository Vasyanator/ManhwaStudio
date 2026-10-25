/*
File: tools/watermark_removal/cleaning_tool.rs

Purpose:
The `CleaningTool` implementation of `WatermarkRemovalTool`: the tab-facing hooks (draw,
input, panels, canvas overlays, teardown).

Key items:
- `impl CleaningTool for WatermarkRemovalTool`

Notes:
The trait is declared in `tools/base.rs`.
*/
use crate::tools::base::{CleaningTool, RegionEditToolBase, RegionOverlayFrame, StrokePoint};
use crate::tools::watermark_library_window::{ARM_CANCEL_RGB, LibraryPanelContext};
use crate::input::CleaningLabels;
use ms_canvas::CanvasView;
use ms_input::{ContextId, InputFrame};
use ms_models::SessionCapabilities;
use ms_project::ProjectData;
use eframe::egui;
use egui::Color32;
use ms_thread as thread;
use std::sync::mpsc::self;
use std::sync::Arc;
use super::{CHAPTER_MAX_TEMPLATE_SIDE, WatermarkEditorCtx, WatermarkMode, WatermarkRemovalTool};
use super::backend::unload_watermark_model;


impl CleaningTool for WatermarkRemovalTool {
    fn title(&self) -> &'static str {
        t!("cleaning.tools.watermark.title")
    }

    /// Always `false`: the Torch requirement of this tool lives INSIDE it.
    ///
    /// Only the two network modes need Torch, and «По главе (точное вычитание)» needs neither
    /// backend nor Torch nor weights — so the picker BUTTON must never be AI-gated. Following
    /// the mode here made the persisted default (`mask_only`) disable the button on a fresh
    /// Torch-less install, hiding the one mode that would have worked; and because the same
    /// answer feeds `ensure_active_tool_available` every frame, picking a network mode
    /// force-deselected the tool. The gate is therefore drawn where it belongs: the two
    /// network entries of the mode picker and the run button (`draw_mode_picker`,
    /// `draw_actions`), both explaining themselves on hover.
    fn pytorch_required(&self) -> bool {
        false
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.region_base.cancel_selection();
        self.session.clear();
        // A canvas reservation belongs to the tool that is drawing: nothing cancels it while
        // this tool is not active, and a selection made after coming back would otherwise be
        // cut out as a library crop the user no longer expects.
        self.library_arm = None;
        // ORDER MATTERS: the tab calls `finish_stroke()` before `deactivate()`, so switching
        // tools mid-drag completes a DIVERTED selection one statement before the reservation
        // dies. Syncing here is what drops that orphan rect — without it the next activation
        // would find a rect nothing armed and cut it out for a reservation that is gone.
        self.sync_selection_diversion();
        // The chapter catalog and any job in flight deliberately survive: a chapter-wide
        // pass takes minutes and must not be thrown away by a stray tool switch.
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.region_base.draw_ui_hint(ui);
        if self.effective_mode() == WatermarkMode::Chapter {
            ui.small(t!("cleaning.tools.watermark.chapter.description_hint"));
            // Reachable without an open region editor: an entry can be built from reference
            // crops alone, with no chapter and no selection involved. A TOGGLE drawn pressed
            // while the panel is open — a dock tab has no close affordance by design, so this
            // button and its twin in the region editor are the whole story.
            if ui
                .add(
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.library_manage_button"))
                        .selected(self.library_window.is_open()),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.library_manage_hint"))
                .clicked()
            {
                self.library_window.toggle();
            }
        } else {
            ui.small(t!("cleaning.tools.watermark.description_hint"));
            ui.small(t!("cleaning.tools.watermark.download_hint"));
            ui.small(t!("cleaning.tools.watermark.experimental_warning"));
        }
        // A chapter job outlives its editor window, so its progress belongs here too.
        if let Some((done, total, label)) = self.chapter.progress.clone() {
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.job_progress",
                label = label,
                done = done,
                total = total
            ));
        }
        if let Some(status) = self.chapter.status.as_ref() {
            ui.small(status);
        }
    }

    fn input_contexts(&self) -> Vec<ContextId> {
        self.region_base.input_contexts()
    }

    /// `cleaning.region.cancel` (published only while no editor window is open) drops the
    /// selection and the editor session it would have opened.
    fn apply_input(&mut self, input: &InputFrame) -> bool {
        if RegionEditToolBase::cancel_fired(input) {
            self.region_base.cancel_selection();
            self.session.clear();
            return true;
        }
        false
    }

    fn set_input_labels(&mut self, labels: &Arc<CleaningLabels>) {
        self.region_base.set_input_labels(labels);
    }

    fn set_ai_backend_available(&mut self, available: bool) {
        self.ai_backend_available = available;
    }

    fn set_ai_backend_torch_available(&mut self, available: bool) {
        self.ai_backend_torch_available = available;
    }

    fn set_session_capabilities(&mut self, caps: SessionCapabilities) {
        self.series_watermark_library = caps.series_watermark_library;
    }

    /// The «Библиотека знаков» panel follows the toggle the two library buttons drive. Both
    /// buttons belong to the chapter mode, which a single-image session does not offer, so the
    /// panel is never shown there.
    fn wants_library_panel(&self) -> bool {
        self.library_window.is_open() && self.series_watermark_library
    }

    /// Draws the library screen into its dock panel.
    ///
    /// The intake shares the tool's OWN ring measurement and footprint limit, so the library
    /// and the chapter mode cannot disagree about either. The jobs behind it are polled from
    /// `draw_overlay_ui`, not from here: this body does not run while the panel is hidden.
    fn draw_library_panel(&mut self, ui: &mut egui::Ui) {
        let sample_params = self.settings.normalized().chapter_sample_params();
        // Rebuilt every frame rather than cached: the catalog holds a handful of marks, and a
        // cached copy would be a second answer to "which entries is this chapter hunting"
        // that could disagree with the catalog after a scan rebuilt it.
        let selected_entries = self.chapter_library_selection();
        let saved_entries = self.chapter_saved_entries();
        let labels = self.region_base.labels();
        let arm_select_hint = tf!(
            "cleaning.tools.watermark.chapter.library_arm_select_hint",
            armed = labels.select_rect_armed,
            select_mode = labels.select_mode
        );
        let context = LibraryPanelContext {
            sample_params,
            max_side: CHAPTER_MAX_TEMPLATE_SIDE,
            selected_entries: &selected_entries,
            saved_entries: &saved_entries,
            armed: self.library_arm.as_deref(),
            chapter_busy: self.chapter.busy(),
            arm_select_hint: &arm_select_hint,
        };
        // The dock body may not touch the catalog or the canvas itself; what it asks for is
        // consumed by `draw_overlay_ui` later in this same frame.
        if let Some(request) = self.library_window.draw_panel_body(ui, &context) {
            self.library_panel_request = Some(request);
        }
    }

    fn wants_primary_stroke(&self, _point: StrokePoint) -> bool {
        // A click that ends an eyedropper sampling belongs to the colour control, not to the
        // canvas: letting it through would start a selection drag under the picked pixel.
        // BOTH predicates are needed. The colour widget clears `eyedropper_active` on the very
        // frame the terminating click lands, and the dock body (where it is drawn) runs before
        // this hook, so on that frame the first flag already reads false — only
        // `primary_click_consumed_this_frame` still names the click. The typing tab guards the
        // same pair for the same reason (`ms-tab-typing/src/panel/create_state.rs`).
        !self.library_window.eyedropper_owns_primary_click()
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        self.region_base.begin_selection(canvas, point);
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, _from: StrokePoint, to: StrokePoint) {
        self.region_base.update_selection(canvas, to);
    }

    fn stroke_end(&mut self, canvas: &mut CanvasView) {
        self.region_base.end_selection(canvas);
    }

    /// A cancelled selection claim (a second finger, focus loss) drops only the drag in flight.
    fn stroke_cancel(&mut self, _canvas: &mut CanvasView) {
        self.region_base.abort_selection_drag();
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        input: &InputFrame,
    ) {
        self.poll_settings_load();
        self.poll_and_maybe_query_status();
        self.poll_unload();
        self.poll_chapter_job(canvas);
        // The library panel's workers are polled HERE, not from its dock body: the body does
        // not run while the panel is hidden, so a job finishing after the user toggled it shut
        // would never be collected. This hook runs every frame the tool is active.
        self.poll_library_capture();
        // The capture lane has its OWN channel, so `WatermarkLibraryWindow::busy` cannot see
        // it and cannot ask for the repaint it needs. Without this the worker's answer — and
        // the row or the status it produces — waits for whatever input happens to draw the
        // next frame, which after a finished drag may be nothing at all: the user lets go of
        // the mouse and the application goes quiet holding an answer it has not painted.
        if self.library_capture_rx.is_some() {
            ctx.request_repaint();
        }
        self.library_window.poll(ctx);
        // A reservation may only outlive the surface that made it for as long as that surface
        // is the one the user is looking at: the red «Отменить выделение» is the ONLY way to
        // call it off, so a reservation drawn on no screen is one the user cannot cancel. The
        // panel decides (it owns the screen); the tool owns the reservation and drops it. This
        // runs BEFORE `run_library_panel_request` below, so a reservation the body asked for
        // this very frame is not judged against the screen it was made on.
        self.drop_reservation_without_a_screen();
        if self.library_window.take_changed() {
            // The panel wrote to disk, so the chapter mode's own copy of the entry list is
            // stale. The panel body already drew this frame (the dock runs inside
            // `CanvasView::draw`), so its flag is consumed in the same frame it was raised.
            self.chapter.library_requested = true;
        }
        // The library picker fills itself the first time the chapter mode is drawn and
        // whenever an entry was written, never on a frame that already has a job running.
        let chapter_mode = self.effective_mode() == WatermarkMode::Chapter;
        if chapter_mode
            && !self.chapter.busy()
            && (self.chapter.library_requested || !self.chapter.library_loaded)
        {
            self.request_chapter_library();
        }

        let mut settings_changed = false;
        let mut want_status = false;
        let mut unload_requested = false;
        let mut chapter_request = None;
        {
            let Self {
                region_base,
                session,
                settings,
                status,
                unload_status,
                progress,
                ai_backend_available,
                ai_backend_torch_available,
                chapter,
                library_window,
                series_watermark_library,
                ..
            } = self;
            let mut editor_ctx = WatermarkEditorCtx {
                session,
                settings,
                status: status.as_ref(),
                unload_status,
                progress,
                ai_backend_available: *ai_backend_available,
                ai_backend_torch_available: *ai_backend_torch_available,
                library_panel_open: library_window.is_open(),
                series_watermark_library: *series_watermark_library,
                chapter,
                settings_changed: &mut settings_changed,
                want_status: &mut want_status,
                unload_requested: &mut unload_requested,
                chapter_request: &mut chapter_request,
            };
            region_base.draw_overlay_ui(
                RegionOverlayFrame { ctx, canvas, project, input, window_title: t!("cleaning.tools.watermark.title") },
                |editor| {
                    if editor.status.is_none() {
                        editor.status =
                            Some(t!("cleaning.tools.watermark.editor_hint_status").to_string());
                    }
                },
                |ui, editor| editor_ctx.draw_body(ui, editor),
            );
        }

        if settings_changed {
            self.dirty = true;
        }
        if want_status {
            self.status_wanted = true;
        }
        if let Some(request) = chapter_request {
            self.start_chapter_request(request, canvas, project);
        }
        // The dock body drew earlier in this same frame (it runs inside `CanvasView::draw`),
        // so whatever it asked for is acted on here, where the canvas and the project exist.
        if let Some(request) = self.library_panel_request.take() {
            self.run_library_panel_request(request, canvas, project);
        }
        // An armed drag never reached the region editor: `RegionEditToolBase` parked its rect
        // instead of loading it (`set_selection_diverted`), and this is where the tool collects
        // it. Taking the reservation before the capture starts means a capture that fails does
        // not leave the next selection silently spoken for; taking the RECT first means a rect
        // whose reservation died in the meantime is discarded here rather than stranded.
        if let Some((page_idx, rect)) = self.region_base.take_diverted_selection()
            && let Some(arm) = self.take_reservation_for_diverted(page_idx, rect)
        {
            self.start_library_capture(arm, page_idx, rect, canvas, project);
        }
        self.report_selection_refusal();
        // Last word on the diversion: whatever this frame did to the reservation — armed it,
        // cancelled it, consumed it, watched it expire — the flag now says the same thing.
        self.sync_selection_diversion();
        if unload_requested && self.unload_rx.is_none() {
            let (tx, rx) = mpsc::channel();
            self.unload_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(unload_watermark_model());
            });
            self.unload_status =
                Some(t!("cleaning.tools.watermark.unload_requested_status").to_string());
        }
        // The editor is gone (applied or cancelled): drop its per-session state so
        // the next region starts with an empty undo stack and no stale mask.
        if !self.region_base.has_open_editor() && self.session.scroll_id.is_some() {
            self.session.clear();
        }
        self.poll_and_maybe_save();
        // LAST, so every mutation this frame made to what the panel shows is covered: the dock
        // body already drew (it runs inside `CanvasView::draw`), so a status or a refusal row
        // recorded anywhere above needs a frame of its own to be seen at all.
        self.library_window.request_repaint_if_dirty(ctx);
    }

    /// Paints the selection cursor, in the reservation's own colour while one is live.
    ///
    /// An armed drag does something completely different from an ordinary one — it is cut out
    /// for the library and never opens the region editor — and until this existed the canvas
    /// looked identical either way: same crosshair, same black-and-white band. The only signal
    /// was the red «Отменить выделение» button, which lives on the dock panel, i.e. not where
    /// the user is looking while dragging. The accent is the CANCEL BUTTON'S own colour, so the
    /// canvas and the panel say the same thing in the same colour.
    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        if self.library_arm.is_some() {
            self.region_base.draw_cursor_tinted(
                ui,
                canvas,
                pointer_scene_pos,
                Color32::from_rgb(ARM_CANCEL_RGB[0], ARM_CANCEL_RGB[1], ARM_CANCEL_RGB[2]),
            );
            return;
        }
        self.region_base.draw_cursor(ui, canvas, pointer_scene_pos);
    }

}
