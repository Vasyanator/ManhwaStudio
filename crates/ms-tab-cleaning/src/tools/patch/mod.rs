/*
File: tabs/cleaning/tools/patch/mod.rs

Purpose:
The cleaning tab's host for the «Заплатка» (patch) tool. The tool itself — selection, gesture,
ROI geometry, the membrane solve and the outline painting — lives in `ms_tools::patch`; this
file is the adapter that drives it against the cleaning tab's canvas and stores its answer in the
clean overlay.

Main responsibilities:
- Implement `CleaningTool` by forwarding every hook to `PatchToolCore`.
- Implement `PatchHost`: the canvas geometry the gesture is projected with, the region loads, and
  the commit into the clean overlay as ONE undo step.
- Own the shared region-loader thread and the buffers one job retains between its load and its
  store.

Key structures:
- `PatchTool`: the `CleaningTool` implementation; a core plus this host's state.
- `PatchLoader`: the shared region loader and the job it currently has in flight.
- `CleaningPatchHost`: the short-lived `PatchHost` view over canvas, project and loader.
- `ApplyError`: why a finished chunk may not be written into the clean overlay.

Key functions:
- `build_overlay_chunk()`: the STORE step — the solved colours turned into clean-overlay pixels.
- `check_chunk_fits()`: the bounds guard run against the LIVE overlay before the write.

Notes:
Why TWO region loads: the membrane needs the COMPOSITED destination (page + clean overlay), while
`overlay_pixel_for_final_color` needs the ORIGINAL page pixel under it as its `base`. The core
asks for the first; the second is this host's, because the store step is the only consumer of it.
`RegionLoadRequest` answers one or the other depending on whether an `overlay_chunk` is supplied,
so the same loader is asked twice for the same rectangle; both answers are derived from one
rectangle and therefore cannot disagree on size.

This tool is NOT a region editor: no floating window, no main dock panel. It rides the tab's
ordinary stroke pipeline, which is why `captures_canvas_pointer()` and `block_canvas_zoom()` both
stay `false` — see the tests at the bottom of this file for what each of them would break.
*/
use super::base::{
    BrushToolBase, CleaningTool, RegionLoadRequest, RegionLoadResult, StrokePoint,
    extract_overlay_chunk, overlay_pixel_for_final_color, scene_pos_to_overlay_pos,
    spawn_region_loader_thread,
};
use super::region_edit_v2::frame::page_source_size;
use super::region_edit_v2::geometry::usable_viewport_for;
use ms_canvas::{CanvasView, OverlayRectPx};
use ms_project::ProjectData;
use ms_tools::patch::{
    PagePixelProjection, PatchBuffer, PatchCommit, PatchHost, PatchHostError, PatchInputError,
    PatchRegionPoll, PatchRegionRequest, PatchToolCore, check_roi_buffer,
};
use eframe::egui;
use egui::{Color32, Pos2};
use ms_thread::JoinHandle;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

/// Log prefix of this host. Also the reason a log line says which surface a patch came from.
const LOG_TAG: &str = "cleaning/patch";

/// `egui::Id` source of the bare layer the selection outline is painted on.
const OUTLINE_LAYER_ID: &str = "cleaning_patch_outline";

/// Why a finished chunk may not be written into the clean overlay.
///
/// The check exists because `CanvasView::replace_overlay_region_px` REPAIRS instead of refusing:
/// it clips a target rectangle that no longer fits the overlay and then nearest-rescales the
/// whole chunk into what is left, overwriting alpha wholesale. The same reason
/// `ai_editor::check_result_fits` and `region_edit_v2/frame.rs`'s D7 exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum ApplyError {
    /// The solved chunk does not have exactly the ROI's size.
    #[error("the solved chunk is {chunk_w}x{chunk_h}, the ROI is {roi_w}x{roi_h}")]
    ChunkSize {
        chunk_w: usize,
        chunk_h: usize,
        roi_w: usize,
        roi_h: usize,
    },
    /// The ROI does not lie inside the page's LIVE clean overlay.
    #[error("the ROI {x};{y} {w}x{h} does not fit the {overlay_w}x{overlay_h} clean overlay")]
    OutOfBounds {
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        overlay_w: usize,
        overlay_h: usize,
    },
}

/// Whether `chunk_size` may be written into `roi` of an overlay of `overlay_size`.
///
/// `overlay_size` is `None` while the page has no clean overlay allocated yet; the write then
/// creates one at the page size and there is nothing to bounds-check against, so only the size
/// equality is enforced. Run against the overlay as it is AT COMMIT TIME, not as it was when the
/// job started: the solve is off-thread and the page may have been undone, cleared or re-laid out
/// in between.
///
/// # Errors
/// [`ApplyError::ChunkSize`] when the chunk is not exactly the ROI size, and
/// [`ApplyError::OutOfBounds`] when the ROI leaves the existing overlay.
fn check_chunk_fits(
    chunk_size: [usize; 2],
    roi: OverlayRectPx,
    overlay_size: Option<[usize; 2]>,
) -> Result<(), ApplyError> {
    if chunk_size != [roi.w, roi.h] {
        return Err(ApplyError::ChunkSize {
            chunk_w: chunk_size[0],
            chunk_h: chunk_size[1],
            roi_w: roi.w,
            roi_h: roi.h,
        });
    }
    if let Some([overlay_w, overlay_h]) = overlay_size
        && (roi.x.saturating_add(roi.w) > overlay_w || roi.y.saturating_add(roi.h) > overlay_h)
    {
        return Err(ApplyError::OutOfBounds {
            x: roi.x,
            y: roi.y,
            w: roi.w,
            h: roi.h,
            overlay_w,
            overlay_h,
        });
    }
    Ok(())
}

/// Turns one solved patch into the clean-overlay chunk that is written over the ROI.
///
/// THE store step, and the only thing about this tool that is cleaning-specific: the core stops
/// at `(rgb, coverage)` because how those pixels are stored — over which backdrop, with which
/// alpha — is a property of the clean-overlay model, not of the patch maths.
///
/// `overlay_chunk` is the overlay AS IT IS under the ROI and is what the result starts from: a
/// pixel the patch does not cover must keep the surrounding cleaning work instead of being erased
/// by the commit, which replaces the whole ROI. `page` is the ORIGINAL page under the ROI, the
/// `base` `overlay_pixel_for_final_color` is defined against.
///
/// # Errors
/// [`PatchInputError::BufferSize`] naming the first of `page` / `overlay_chunk` whose size
/// disagrees with the ROI, and [`PatchInputError::SolvedLength`] when the solved buffers do not
/// cover it. Nothing is substituted for a buffer whose size cannot be explained: an
/// all-transparent stand-in for the overlay chunk would erase every pre-existing cleaning pixel
/// of the ROI on commit.
fn build_overlay_chunk(
    roi: OverlayRectPx,
    page: &egui::ColorImage,
    overlay_chunk: egui::ColorImage,
    rgb: &[[u8; 3]],
    coverage: &[f32],
) -> Result<egui::ColorImage, PatchInputError> {
    let count = roi.w.saturating_mul(roi.h);
    check_roi_buffer(PatchBuffer::Page, page.size, page.pixels.len(), roi)?;
    check_roi_buffer(
        PatchBuffer::OverlayChunk,
        overlay_chunk.size,
        overlay_chunk.pixels.len(),
        roi,
    )?;
    if rgb.len() != count || coverage.len() != count {
        return Err(PatchInputError::SolvedLength {
            rgb: rgb.len(),
            coverage: coverage.len(),
            want: count,
        });
    }

    // Start from the overlay that is already there: a pixel the patch does not cover must keep
    // the surrounding cleaning work instead of being erased by the commit.
    let mut chunk = overlay_chunk;
    // Every one of the four buffers is exactly `count` long by the checks above, so the zip
    // visits each ROI pixel exactly once and no pixel can be skipped over a length disagreement.
    let per_pixel = chunk
        .pixels
        .iter_mut()
        .zip(page.pixels.iter())
        .zip(rgb.iter())
        .zip(coverage.iter());
    for (((target, base), final_rgb), coverage) in per_pixel {
        if *coverage <= 0.0 {
            continue;
        }
        *target = overlay_pixel_for_final_color(
            *base,
            Color32::from_rgb(final_rgb[0], final_rgb[1], final_rgb[2]),
            *coverage,
        );
    }
    Ok(chunk)
}

/// Reports a host-side failure to the log, in the same shape `PatchToolCore` uses.
///
/// The core shows the sentence this host returns and deliberately does not log it a second time,
/// so the technical context has to be recorded here.
fn log_failure(text: &str, detail: &dyn std::fmt::Display) {
    ms_log::runtime_log::log_warn(format!("[{LOG_TAG}] {text} | {detail}"));
}

/// The region loads of ONE patch job, and everything retained between them and the store step.
#[derive(Debug)]
struct InFlightLoad {
    composite_job_id: u64,
    page_job_id: u64,
    /// The clean-overlay pixels currently under the ROI; preserved where coverage is zero.
    overlay_chunk: egui::ColorImage,
    /// The composited destination, taken out when it is handed to the core.
    composite: Option<egui::ColorImage>,
    /// The ORIGINAL page pixels under the ROI, retained until the store step consumes them.
    page: Option<egui::ColorImage>,
    /// Whether the composite has already been handed to the core.
    ///
    /// After that moment this load owes the loader thread nothing and exists only to carry `page`
    /// to the store step, so a worker that dies from then on must NOT fail the load — dropping it
    /// would throw away the finished patch's backdrop. `composite` alone cannot say this: it is
    /// `None` both before the composite arrives and after it has been handed over.
    composite_delivered: bool,
}

impl InFlightLoad {
    /// Whether the death of the loader worker still FAILS this load.
    ///
    /// Only until the composite has been handed to the core. From then on the load owes the worker
    /// nothing — it exists solely to carry the retained page region to the store step — and failing
    /// it would drop a patch that is already solved.
    fn fails_on_worker_death(&self) -> bool {
        !self.composite_delivered
    }
}

/// The shared region loader and the job it currently has in flight.
///
/// Owns the loader THREAD: its contract is "send `None` and join, or the thread and the page it
/// decoded outlive the tool", honoured by `Drop`.
struct PatchLoader {
    next_job_id: u64,
    in_flight: Option<InFlightLoad>,
    load_tx: Sender<Option<RegionLoadRequest>>,
    load_rx: Receiver<RegionLoadResult>,
    loader_handle: Option<JoinHandle<()>>,
}

impl Default for PatchLoader {
    fn default() -> Self {
        let (load_tx, load_rx, loader_handle) = spawn_region_loader_thread();
        Self {
            next_job_id: 0,
            in_flight: None,
            load_tx,
            load_rx,
            loader_handle: Some(loader_handle),
        }
    }
}

impl Drop for PatchLoader {
    /// Stops the shared region loader.
    ///
    /// Its ownership contract: send `None` and join, or the thread and the page it decoded
    /// outlive the tool.
    fn drop(&mut self) {
        // A failed send means the worker already exited, which is the state this asks for; a
        // failed join means it panicked, and there is no recovery available inside `drop`.
        let _ = self.load_tx.send(None);
        if let Some(handle) = self.loader_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Photoshop-style patch tool: select, drag onto a source, get a colour-adapted copy.
///
/// A thin adapter. Every decision about the gesture, the geometry and the solve lives in
/// `PatchToolCore`; what is here is the cleaning tab's canvas, its project and its clean overlay.
pub struct PatchTool {
    core: PatchToolCore,
    loader: PatchLoader,
    /// The dock-panel rectangles this frame, cut out of the viewport before the outline is
    /// painted so it never lands on a panel.
    panel_rects: Vec<egui::Rect>,
}

impl Default for PatchTool {
    fn default() -> Self {
        Self {
            core: PatchToolCore::new(LOG_TAG, OUTLINE_LAYER_ID),
            loader: PatchLoader::default(),
            panel_rects: Vec::new(),
        }
    }
}

/// The `PatchHost` view the core is driven through for one hook.
///
/// Short-lived on purpose: it borrows the canvas, the project and the loader for exactly the call
/// that needs them. `project` is `None` in the hooks the tab hands no project — only
/// `start_region_load` needs it, and it is called from `draw_overlay_ui`, which has one.
struct CleaningPatchHost<'a> {
    canvas: &'a mut CanvasView,
    project: Option<&'a ProjectData>,
    loader: &'a mut PatchLoader,
    panel_rects: &'a [egui::Rect],
}

impl PatchHost for CleaningPatchHost<'_> {
    fn page_source_size(&self, page_idx: usize) -> Option<[usize; 2]> {
        page_source_size(self.canvas, page_idx, self.canvas.zoom())
    }

    fn page_projection(&self, page_idx: usize) -> Option<PagePixelProjection> {
        // The page-pixel space the selection lives in is the CLEAN OVERLAY's, so its size is what
        // the outline is projected with — the same space `scene_pos_to_overlay_pos` answers in.
        Some(PagePixelProjection {
            scene_rect: self.canvas.page_scene_rect(page_idx)?,
            pixel_size: self.canvas.overlay_size(page_idx)?,
        })
    }

    fn usable_viewport(&self) -> Option<egui::Rect> {
        let viewport = self.canvas.visible_scene_rect()?;
        // The dock panels are cut out of the viewport, so the outline never paints over one.
        let usable = usable_viewport_for(viewport, viewport, self.panel_rects);
        usable.is_positive().then_some(usable)
    }

    fn scene_pos_to_page_pos(&self, page_idx: usize, scene_pos: Pos2) -> Option<(f32, f32)> {
        let pos = scene_pos_to_overlay_pos(self.canvas, page_idx, scene_pos)?;
        Some((pos.x, pos.y))
    }

    fn ensure_page_pixels(&mut self, page_idx: usize, scene_pos: Pos2) {
        // A refusal only means the page is not laid out yet, which the core's page-position
        // guard then reports as "no gesture".
        let _ = BrushToolBase::ensure_overlay_under_point(
            self.canvas,
            StrokePoint {
                page_idx,
                scene_pos,
                modifiers: super::base::StrokeModifiers::default(),
            },
        );
    }

    /// Sends BOTH region requests and retains the clean-overlay chunk for the store step.
    ///
    /// Nothing is decoded here: the GUI thread only copies the ROI-sized chunk it already holds
    /// in memory and hands two rectangles to the loader thread.
    fn start_region_load(&mut self, request: &PatchRegionRequest) -> Result<(), PatchHostError> {
        let page_idx = request.page_idx;
        let Some(project) = self.project else {
            return Err(PatchHostError {
                user_message: t!("tools.patch.error_region_load").to_string(),
                detail: "a region load was started from a hook that is handed no project"
                    .to_string(),
            });
        };
        let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) else {
            return Err(PatchHostError {
                user_message: tf!("tools.patch.error_page_missing", page = page_idx + 1),
                detail: format!("page {page_idx} is not in the project"),
            });
        };
        let page_path = page.path.clone();
        let Some(overlay) = self.canvas.overlay_image(page_idx) else {
            return Err(PatchHostError {
                user_message: tf!("cleaning.tools.patch.error_no_overlay", page = page_idx + 1),
                detail: format!("page {page_idx} has no clean overlay allocated"),
            });
        };
        let overlay_chunk = extract_overlay_chunk(overlay, request.roi);

        let composite_job_id = self.loader.next_job_id;
        let page_job_id = self.loader.next_job_id.saturating_add(1);
        self.loader.next_job_id = self.loader.next_job_id.saturating_add(2);

        let composite_request = RegionLoadRequest {
            job_id: composite_job_id,
            page_idx,
            source_rect: request.roi,
            source_size: request.source_size,
            page_path: page_path.clone(),
            overlay_chunk: Some(overlay_chunk.clone()),
            shared_overlays_model: self.canvas.clean_overlays_model_handle(),
        };
        // `overlay_chunk: None` asks the same loader for the page WITHOUT the clean overlay —
        // the `base` argument `overlay_pixel_for_final_color` is defined against.
        let page_request = RegionLoadRequest {
            job_id: page_job_id,
            page_idx,
            source_rect: request.roi,
            source_size: request.source_size,
            page_path,
            overlay_chunk: None,
            shared_overlays_model: self.canvas.clean_overlays_model_handle(),
        };
        if self.loader.load_tx.send(Some(composite_request)).is_err()
            || self.loader.load_tx.send(Some(page_request)).is_err()
        {
            return Err(PatchHostError {
                user_message: t!("tools.patch.error_region_load").to_string(),
                detail: "the region loader worker is gone".to_string(),
            });
        }

        self.loader.in_flight = Some(InFlightLoad {
            composite_job_id,
            page_job_id,
            overlay_chunk,
            composite: None,
            page: None,
            composite_delivered: false,
        });
        Ok(())
    }

    /// Drains the loader and answers `Ready` once BOTH halves have arrived.
    ///
    /// The original page half is not handed over: it is retained for `commit_patch`, which is its
    /// only consumer.
    ///
    /// The core calls this on every frame, including when it awaits nothing (`PatchHost` doc), and
    /// that is the point: with no job in flight the drain simply DROPS what arrives, which is how
    /// an abandoned job's decoded page region is released promptly instead of sitting in the
    /// channel until the next job.
    fn poll_region_load(&mut self) -> PatchRegionPoll {
        let mut failure: Option<String> = None;
        let mut disconnected = false;
        loop {
            match self.loader.load_rx.try_recv() {
                Ok(result) => {
                    let Some(pending) = self.loader.in_flight.as_mut() else {
                        continue;
                    };
                    let slot = if result.job_id == pending.composite_job_id {
                        &mut pending.composite
                    } else if result.job_id == pending.page_job_id {
                        &mut pending.page
                    } else {
                        // An answer to an abandoned job; dropping it is what makes a cancel free.
                        continue;
                    };
                    match result.image {
                        Ok(image) => *slot = Some(image),
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if let Some(error) = failure {
            self.loader.in_flight = None;
            return PatchRegionPoll::Failed(error);
        }
        if disconnected {
            // Only a load that still OWES the core its composite is failed by the worker's death.
            // Once the composite has been handed over, what is left is the retained page region the
            // store step needs, and dropping it here would lose a patch that is already solved.
            let awaiting = self
                .loader
                .in_flight
                .as_ref()
                .is_some_and(InFlightLoad::fails_on_worker_death);
            if awaiting {
                self.loader.in_flight = None;
                return PatchRegionPoll::Failed(
                    "the region loader worker died with a job in flight".to_string(),
                );
            }
            return PatchRegionPoll::Pending;
        }
        let Some(pending) = self.loader.in_flight.as_mut() else {
            return PatchRegionPoll::Pending;
        };
        if pending.page.is_none() {
            return PatchRegionPoll::Pending;
        }
        match pending.composite.take() {
            Some(composite) => {
                pending.composite_delivered = true;
                PatchRegionPoll::Ready(composite)
            }
            None => PatchRegionPoll::Pending,
        }
    }

    fn discard_region_load(&mut self) {
        self.loader.in_flight = None;
    }

    /// Writes one finished patch into the clean overlay as ONE undo step.
    fn commit_patch(&mut self, patch: &PatchCommit<'_>) -> Result<(), String> {
        let Some(load) = self.loader.in_flight.take() else {
            let text = t!("cleaning.tools.patch.error_apply").to_string();
            log_failure(&text, &"the original page region was not retained for the store step");
            return Err(text);
        };
        let Some(page) = load.page else {
            let text = t!("cleaning.tools.patch.error_apply").to_string();
            log_failure(&text, &"the original page region never arrived");
            return Err(text);
        };
        let chunk =
            match build_overlay_chunk(patch.roi, &page, load.overlay_chunk, patch.rgb, patch.coverage)
            {
                Ok(chunk) => chunk,
                Err(error) => {
                    let text = error.user_message();
                    log_failure(&text, &error);
                    return Err(text);
                }
            };
        // Bounds are re-checked against the overlay AS IT IS NOW, not as it was when the job
        // started: the solve ran off-thread, and `replace_overlay_region_px` clips a rectangle
        // that no longer fits and nearest-rescales the chunk into the remainder instead of
        // refusing.
        if let Err(error) = check_chunk_fits(
            chunk.size,
            patch.roi,
            self.canvas.overlay_size(patch.page_idx),
        ) {
            let text = t!("cleaning.tools.patch.error_overlay_changed").to_string();
            log_failure(&text, &error);
            return Err(text);
        }
        // `replace_overlay_region_px` syncs the shared model and records ONE region diff there
        // (`CleanOverlaysModel::replace_region`), so the whole patch is a single Ctrl+Z and
        // nothing further has to be published.
        if self
            .canvas
            .replace_overlay_region_px(patch.page_idx, patch.roi, &chunk)
        {
            Ok(())
        } else {
            let text = t!("cleaning.tools.patch.error_apply").to_string();
            log_failure(
                &text,
                &format!("the canvas refused the overlay write for page {}", patch.page_idx),
            );
            Err(text)
        }
    }
}

impl CleaningTool for PatchTool {
    fn tool_id(&self) -> &'static str {
        "patch"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.patch.title")
    }

    fn deactivate(&mut self, canvas: &mut CanvasView) {
        let mut host = CleaningPatchHost {
            canvas,
            project: None,
            loader: &mut self.loader,
            panel_rects: &self.panel_rects,
        };
        self.core.deactivate(&mut host);
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.core.draw_ui(ui);
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        let mut host = CleaningPatchHost {
            canvas,
            project: None,
            loader: &mut self.loader,
            panel_rects: &self.panel_rects,
        };
        self.core
            .stroke_begin(&mut host, point.page_idx, point.scene_pos);
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, _from: StrokePoint, to: StrokePoint) {
        let host = CleaningPatchHost {
            canvas,
            project: None,
            loader: &mut self.loader,
            panel_rects: &self.panel_rects,
        };
        self.core.stroke_update(&host, to.page_idx, to.scene_pos);
    }

    fn stroke_end(&mut self, _canvas: &mut CanvasView) {
        self.core.stroke_end();
    }

    fn set_space_pan_active(&mut self, active: bool) {
        self.core.set_space_pan_active(active);
    }

    fn space_pan_active(&self) -> bool {
        self.core.space_pan_active()
    }

    fn block_canvas_drag_scroll_on_primary(&self) -> bool {
        !self.core.space_pan_active()
    }

    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        self.core.on_escape(ctx)
    }

    fn set_panel_rects(&mut self, rects: &[egui::Rect]) {
        self.panel_rects.clear();
        self.panel_rects.extend_from_slice(rects);
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        // The only place in the frame where the pointer state is available (`../../tab.rs` runs
        // this after all stroke dispatch), and therefore the only place a parked `stroke_end` that
        // no `stroke_begin` claimed can be judged. `primary_released` is checked as well as
        // `primary_down`, so a release followed by a fresh press inside one frame still counts as
        // the release it was.
        let released =
            ctx.input(|input| !input.pointer.primary_down() || input.pointer.primary_released());
        let mut host = CleaningPatchHost {
            canvas,
            project: Some(project),
            loader: &mut self.loader,
            panel_rects: &self.panel_rects,
        };
        self.core.draw_overlay_ui(ctx, &mut host, released);
    }

    /// Never `true`.
    ///
    /// `block_canvas_zoom` also disables the clean-overlay Ctrl+Z / Ctrl+Shift+Z shortcuts
    /// (`tab.rs::handle_history_hotkeys`), which is acceptable for a modal editor window but not
    /// for a surface that lives on the canvas for a whole session.
    fn block_canvas_zoom(&self) -> bool {
        false
    }

    /// Never `true`.
    ///
    /// The tab ORs this flag into `canvas_pointer_occluded` and then refuses every stroke, key
    /// and cursor callback over the captured area — and this tool drives its entire gesture
    /// through those callbacks.
    fn captures_canvas_pointer(&self, _pointer_pos: Pos2) -> bool {
        false
    }

    fn ensure_hover_overlay(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        // Same reason as in `stroke_begin`: the selection's coordinate space is the overlay's.
        // A page that is not laid out yet simply gets no overlay this frame.
        let _ = BrushToolBase::ensure_overlay_under_point(canvas, point);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `block_canvas_zoom` must stay `false`.
    ///
    /// The flag also disables the clean-overlay Ctrl+Z / Ctrl+Shift+Z shortcuts
    /// (`tab.rs::handle_history_hotkeys`), and this tool's selection lives on the canvas for a
    /// whole session — undo has to keep working the entire time.
    #[test]
    fn canvas_zoom_is_never_blocked() {
        assert!(!PatchTool::default().block_canvas_zoom());
    }

    /// `captures_canvas_pointer` must stay `false`.
    ///
    /// The tab ORs it into `canvas_pointer_occluded` and then calls `finish_stroke()` and
    /// returns, so a capturing tool receives no `stroke_begin`/`stroke_update`/`stroke_end`, no
    /// `on_key_event` and no `draw_cursor` — and this tool's entire gesture rides those.
    #[test]
    fn the_canvas_pointer_is_never_captured() {
        let tool = PatchTool::default();
        for pos in [egui::pos2(0.0, 0.0), egui::pos2(500.0, 500.0)] {
            assert!(!tool.captures_canvas_pointer(pos));
        }
    }

    /// The commit refuses a chunk that does not fit the LIVE overlay instead of letting
    /// `replace_overlay_region_px` clip the rectangle and nearest-rescale the chunk into it.
    #[test]
    fn a_chunk_that_does_not_fit_the_live_overlay_is_refused() {
        let roi = OverlayRectPx {
            x: 10,
            y: 10,
            w: 20,
            h: 20,
        };
        assert!(check_chunk_fits([20, 20], roi, Some([200, 200])).is_ok());
        // No overlay allocated yet: the write creates one, so only the size equality applies.
        assert!(check_chunk_fits([20, 20], roi, None).is_ok());
        assert!(matches!(
            check_chunk_fits([19, 20], roi, Some([200, 200])),
            Err(ApplyError::ChunkSize { .. })
        ));
        // The page shrank while the solve ran.
        assert!(matches!(
            check_chunk_fits([20, 20], roi, Some([25, 200])),
            Err(ApplyError::OutOfBounds { .. })
        ));
        assert!(matches!(
            check_chunk_fits([20, 20], roi, Some([200, 25])),
            Err(ApplyError::OutOfBounds { .. })
        ));
    }

    /// A load whose composite has been delivered survives the loader worker's death; one that is
    /// still waiting for it does not.
    ///
    /// The core polls the host on EVERY frame, including while the solve runs, so this predicate is
    /// what keeps a `Disconnected` channel from sweeping up the page region the store step needs.
    #[test]
    fn only_a_load_still_owing_its_composite_is_failed_by_a_dead_worker() {
        let mut load = InFlightLoad {
            composite_job_id: 0,
            page_job_id: 1,
            overlay_chunk: egui::ColorImage::filled([2, 2], Color32::TRANSPARENT),
            composite: None,
            page: None,
            composite_delivered: false,
        };
        assert!(
            load.fails_on_worker_death(),
            "a load the core is still waiting on must be failed"
        );
        load.page = Some(egui::ColorImage::filled([2, 2], Color32::WHITE));
        load.composite_delivered = true;
        assert!(
            !load.fails_on_worker_death(),
            "once the composite is handed over the retained page must survive"
        );
    }

    /// One well-formed store step: an 8x8 ROI whose middle 2x2 block is fully covered.
    fn store_input() -> (OverlayRectPx, egui::ColorImage, egui::ColorImage, Vec<[u8; 3]>, Vec<f32>) {
        let roi = OverlayRectPx {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        };
        let count = roi.w * roi.h;
        let mut coverage = vec![0.0f32; count];
        for y in 3..5 {
            for x in 3..5 {
                coverage[y * roi.w + x] = 1.0;
            }
        }
        (
            roi,
            egui::ColorImage::filled([8, 8], Color32::WHITE),
            egui::ColorImage::filled([8, 8], Color32::TRANSPARENT),
            vec![[10, 20, 30]; count],
            coverage,
        )
    }

    /// The store step writes only what the patch covers, and solves each written pixel against
    /// the ORIGINAL page under it.
    ///
    /// A pixel with zero coverage keeps the existing clean-overlay pixel: the commit replaces the
    /// whole ROI, so anything else would erase the surrounding cleaning work.
    #[test]
    fn the_store_step_writes_only_the_covered_pixels() {
        let (roi, page, overlay_chunk, rgb, coverage) = store_input();
        let chunk = build_overlay_chunk(roi, &page, overlay_chunk, &rgb, &coverage)
            .expect("the fixture is well formed");
        assert_eq!(chunk.size, [roi.w, roi.h]);
        assert_eq!(
            chunk.pixels[0],
            Color32::TRANSPARENT,
            "an uncovered pixel keeps the existing clean-overlay pixel"
        );
        let covered = chunk.pixels[3 * roi.w + 3];
        assert_eq!(
            covered,
            overlay_pixel_for_final_color(Color32::WHITE, Color32::from_rgb(10, 20, 30), 1.0),
            "a covered pixel is solved against the ORIGINAL page under it"
        );
        assert_eq!(covered.a(), 255, "full coverage commits a DENSE overlay pixel");
    }

    /// A malformed store input is REFUSED, never substituted.
    ///
    /// The clean-overlay chunk is the dangerous one: a transparent stand-in for it would erase
    /// every pre-existing cleaning pixel of the ROI the moment the chunk is written.
    #[test]
    fn a_malformed_store_input_is_refused_and_never_yields_a_chunk() {
        let (roi, page, overlay_chunk, rgb, coverage) = store_input();
        assert!(build_overlay_chunk(roi, &page, overlay_chunk, &rgb, &coverage).is_ok());

        // The clean-overlay chunk disagrees with the ROI.
        let (roi, page, _, rgb, coverage) = store_input();
        let error = build_overlay_chunk(
            roi,
            &page,
            egui::ColorImage::filled([1, 1], Color32::TRANSPARENT),
            &rgb,
            &coverage,
        )
        .expect_err("a malformed clean-overlay chunk must be refused, not substituted");
        assert!(matches!(
            error,
            PatchInputError::BufferSize {
                buffer: PatchBuffer::OverlayChunk,
                got_w: 1,
                got_h: 1,
                want_w: 8,
                want_h: 8,
                ..
            }
        ));
        assert!(!error.user_message().is_empty(), "the refusal carries a user-facing sentence");

        // The original page region disagrees with the ROI.
        let (roi, _, overlay_chunk, rgb, coverage) = store_input();
        let error = build_overlay_chunk(
            roi,
            &egui::ColorImage::filled([9, 8], Color32::WHITE),
            overlay_chunk,
            &rgb,
            &coverage,
        )
        .expect_err("a malformed original page region must be refused, not substituted");
        assert!(matches!(
            error,
            PatchInputError::BufferSize {
                buffer: PatchBuffer::Page,
                ..
            }
        ));
        assert!(!error.user_message().is_empty());

        // The solved buffers do not cover the ROI.
        let (roi, page, overlay_chunk, _, coverage) = store_input();
        let error = build_overlay_chunk(roi, &page, overlay_chunk, &[[0, 0, 0]], &coverage)
            .expect_err("a short solution must be refused");
        assert!(matches!(error, PatchInputError::SolvedLength { .. }));
        assert!(!error.user_message().is_empty());
    }
}
