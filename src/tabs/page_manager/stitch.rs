/*
File: tabs/page_manager/stitch.rs

Purpose:
The "stitch pages" window (Layer 3 of `dev-docs/stitch_pages_plan.md`): a modal
`egui::Window` that shows every selected page as a draggable rectangle on a
zoomable board, offers the quick arrangements / fit modes / background of the
layout core, and emits `PageOpKind::Stitch` when the user confirms.

Key structures:
- StitchDialogState: the dialog's own state (placements, camera, settings, drag).
- BoardDrag: what the current primary/middle drag on the board is doing.

Key functions:
- PageManagerTabState::draw_stitch_dialog(): the per-frame window.
- build_stitch_op(): placements + background -> the engine request (pure).
- hit_test(): topmost placement under a world point (pure).
- layout_error_message(): StitchLayoutError -> localized user text.

Notes:
All layout math lives in `stitch_layout.rs`; this file only draws and routes
input. Page pixels come from the bounded preview cache of `thumbs.rs` (a decode
never happens on the GUI thread), and the previews are only ever a VIEW: the
stitched result is composed by the engine from the untouched originals.
*/

use std::collections::HashMap;

use eframe::egui;

use crate::app::PageImageInfo;
use crate::page_ops::{PageOpKind, StitchPlacement};
use crate::project::ProjectData;
use crate::tabs::ps_editor::viewport::{PsViewport, ViewTransform};

use super::stitch_layout::{
    self, CrossAlign, EditPlacement, FitMode, LayoutKind, SnapAxis, SnapKind, SnapResult,
    StitchLayoutError, WorldRect,
};
use super::thumbs::{PREVIEW_CACHE_CAPACITY, PREVIEW_LONG_SIDE_PX, PreviewState};
use super::{PageManagerAction, PageManagerTabState};

/// Snap radius of a page drag, in SCREEN points. Divided by the zoom before it
/// reaches [`stitch_layout::snap_drag`], so the magnet feels equally strong at
/// any zoom level.
const SNAP_RADIUS_POINTS: f64 = 9.0;

/// Zoom step handed to [`PsViewport::handle_input`] per wheel notch.
///
/// `raw_wheel_delta` is unit-dependent (`Point` / `Line` / `Page`), so its
/// magnitude must never be used as a distance (`egui-docs/03-input.md`). Only
/// its sign is read here and this fixed step is applied instead, which makes one
/// notch mean the same zoom change on every platform.
const WHEEL_ZOOM_STEP: f32 = 100.0;

/// Hard bound applied to a placement coordinate while dragging, in canvas px.
///
/// Far beyond the engine's 40 000 px canvas budget (an out-of-budget layout is
/// refused by `normalize`, not clamped), but finite, so a wild drag can never
/// produce a non-finite or overflowing coordinate.
const COORD_LIMIT_PX: f64 = 4_000_000.0;

/// Maximum number of pages a preview decode is REQUESTED for at once.
///
/// Defined from the preview LRU's own capacity, not restated: asking for more
/// previews than the cache can hold would evict and re-decode them every frame.
/// A page that misses out is drawn as a numbered placeholder — but if its
/// texture happens to still be cached it is drawn, so a rank swap during a pan
/// does not blink the image away.
const MAX_LIVE_PREVIEWS: usize = PREVIEW_CACHE_CAPACITY;

/// Board padding when the camera is fit to the layout, as a fraction of the
/// bounding box (0.06 = 6% of empty space around it).
const FIT_MARGIN: f32 = 0.06;

/// State of the "stitch pages" window.
///
/// `pages` is the selection snapshot taken when the window opened; it is
/// re-validated against the current page count on every frame, because
/// `clamp_selection` may silently drop indices after a reload.
pub(super) struct StitchDialogState {
    /// Source page indices in the CURRENT page order, ascending.
    pages: Vec<usize>,
    /// One placement per entry of `pages`, in the same order. Empty until every
    /// page's pixel size is known (see `PageManagerTabState::page_pixel_size`).
    placements: Vec<EditPlacement>,
    /// Cross-axis alignment of the quick arrangements and of the fit modes.
    align: CrossAlign,
    /// Currently selected fit mode.
    fit_mode: FitMode,
    /// Opaque background color (sRGB), used when `transparent` is false.
    background: [u8; 3],
    /// Whether the uncovered canvas stays fully transparent.
    transparent: bool,
    /// Board camera.
    viewport: PsViewport,
    /// Whether the camera has already been fit to the initial layout.
    camera_fitted: bool,
    /// The drag currently in progress on the board, if any.
    drag: Option<BoardDrag>,
    /// Last localized layout error (a refused fit mode), shown under the strip.
    fit_error: Option<String>,
}

impl StitchDialogState {
    /// Fresh dialog for the given current page indices (ascending, >= 2).
    pub(super) fn new(pages: Vec<usize>) -> Self {
        Self {
            pages,
            placements: Vec::new(),
            align: CrossAlign::Center,
            fit_mode: FitMode::Fill,
            background: [255, 255, 255],
            transparent: false,
            viewport: PsViewport::default(),
            camera_fitted: false,
            drag: None,
            fit_error: None,
        }
    }
}

/// What the pointer is doing on the board while a button is held.
#[derive(Debug, Clone, Copy)]
enum BoardDrag {
    /// Moving the camera (middle button, or the primary button on empty space).
    Pan,
    /// Moving one placed page.
    Page {
        /// Index into `StitchDialogState::placements`.
        index: usize,
        /// Pointer position relative to the page's top-left corner, world px.
        /// Keeping it fixed is what makes the page follow the cursor exactly.
        grab: [f64; 2],
        /// Snap result of the last frame, so the guides can be painted.
        snap: Option<SnapResult>,
    },
}

/// Straight (non-premultiplied) RGBA background for the engine request.
#[must_use]
fn background_rgba(color: [u8; 3], transparent: bool) -> [u8; 4] {
    if transparent {
        [0, 0, 0, 0]
    } else {
        [color[0], color[1], color[2], 255]
    }
}

/// Rounds a world coordinate to a whole canvas pixel.
///
/// `NaN` (which has no meaningful position) becomes `0`; every other value,
/// infinities included, is clamped to `±COORD_LIMIT_PX`, so a drag can never
/// produce an unrepresentable position.
#[must_use]
fn round_to_canvas_px(value: f64) -> i64 {
    if value.is_nan() {
        return 0;
    }
    let clamped = value.round().clamp(-COORD_LIMIT_PX, COORD_LIMIT_PX);
    // Guarded above: `clamped` is finite and well inside the i64 range.
    clamped as i64
}

/// Index of the topmost placement covering `world`, or `None` for empty space.
///
/// Placements are painted in list order (later entries on top), so the search
/// runs backwards and the visually topmost page wins a stack of overlaps.
#[must_use]
fn hit_test(placements: &[EditPlacement], world: [f64; 2]) -> Option<usize> {
    placements.iter().enumerate().rev().find_map(|(index, placement)| {
        let rect = placement.rect().to_world();
        let inside = world[0] >= rect.min_x
            && world[0] < rect.max_x
            && world[1] >= rect.min_y
            && world[1] < rect.max_y;
        inside.then_some(index)
    })
}

/// Picks the indices of the `max` highest-scoring entries of `scores`.
///
/// Ties and equal scores resolve towards the lower index, so a static camera
/// produces a stable selection frame after frame. Non-positive scores are
/// eligible only while fewer than `max` entries have been picked.
#[must_use]
fn top_scored_indices(scores: &[f64], max: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|a, b| {
        scores[*b]
            .partial_cmp(&scores[*a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });
    order.truncate(max);
    order.sort_unstable();
    order
}

/// Normalizes a COPY of the placements and reports the canvas they would produce.
///
/// The single validation gate of the dialog: the confirm button is enabled
/// exactly when this succeeds, and the size label shows the canvas it returns.
///
/// # Errors
/// [`StitchLayoutError::Empty`] when fewer than two pages remain, plus any error
/// raised by [`stitch_layout::normalize`].
fn normalized_stitch_layout(
    placements: &[EditPlacement],
) -> Result<(Vec<EditPlacement>, stitch_layout::CanvasSize), StitchLayoutError> {
    if placements.len() < 2 {
        return Err(StitchLayoutError::Empty);
    }
    let mut normalized = placements.to_vec();
    let size = stitch_layout::normalize(&mut normalized)?;
    Ok((normalized, size))
}

/// Builds the engine request from the current placements.
///
/// The placements are normalized on a COPY, so the editing coordinates (which
/// may be negative while the user drags) are never mutated by a mere preview of
/// the result.
///
/// # Errors
/// Any [`StitchLayoutError`] raised by [`stitch_layout::normalize`], plus
/// [`StitchLayoutError::Empty`] when fewer than two pages remain.
fn build_stitch_op(
    placements: &[EditPlacement],
    background: [u8; 4],
) -> Result<PageOpKind, StitchLayoutError> {
    let (normalized, size) = normalized_stitch_layout(placements)?;
    let placements = normalized
        .iter()
        .map(|placement| {
            let (page_idx, crop, scale, dx, dy) = placement.engine_fields();
            StitchPlacement {
                page_idx,
                crop,
                scale,
                dx,
                dy,
            }
        })
        .collect();
    Ok(PageOpKind::Stitch {
        placements,
        width: size.width,
        height: size.height,
        background,
    })
}

/// What a page draws when it has no usable preview texture this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderKind {
    /// A decode was requested for this page and has not landed yet.
    Loading,
    /// The page is outside [`MAX_LIVE_PREVIEWS`]: nothing is loading and nothing
    /// ever will be, so only the numbered badge identifies it.
    Capped,
    /// The decode failed, or produced a texture that cannot be sampled.
    Failed,
}

/// Classifies the placeholder a page falls back to.
///
/// `requested` is whether the board asked for this page's preview this frame. It
/// is the ONLY thing that separates "still loading" from "not allowed a preview":
/// both surface as [`PreviewState::Pending`], and calling the second one
/// «Загрузка…» would promise an image that never arrives.
#[must_use]
fn placeholder_kind(preview: &PreviewState, requested: bool) -> PlaceholderKind {
    match preview {
        PreviewState::Pending if requested => PlaceholderKind::Loading,
        PreviewState::Pending => PlaceholderKind::Capped,
        // A `Ready` that reaches a placeholder had an unusable texture size.
        PreviewState::Failed | PreviewState::Ready { .. } => PlaceholderKind::Failed,
    }
}

/// Localized caption painted over a placeholder, or `None` when the numbered
/// badge already says everything there is to say.
#[must_use]
fn placeholder_caption(preview: &PreviewState, requested: bool) -> Option<&'static str> {
    match placeholder_kind(preview, requested) {
        PlaceholderKind::Loading => Some(t!("page_manager.stitch_dialog.preview_loading")),
        PlaceholderKind::Capped => None,
        PlaceholderKind::Failed => Some(t!("page_manager.stitch_dialog.preview_failed")),
    }
}

/// Localized caption of a fit mode.
///
/// The `t!` macro only accepts a string literal, so the mapping is an exhaustive
/// match instead of a key table: adding a `FitMode` variant must not compile
/// until it has a caption.
#[must_use]
fn fit_mode_label(mode: FitMode) -> &'static str {
    match mode {
        FitMode::Fill => t!("page_manager.stitch_dialog.fit_fill_radio"),
        FitMode::ScaleToSmaller => t!("page_manager.stitch_dialog.fit_scale_smaller_radio"),
        FitMode::ScaleToLarger => t!("page_manager.stitch_dialog.fit_scale_larger_radio"),
        FitMode::Crop => t!("page_manager.stitch_dialog.fit_crop_radio"),
    }
}

/// Maps a layout error to the localized message shown to the user.
///
/// The `StitchLayoutError` `Display` texts are technical (log/English); this is
/// the single place that turns them into UI strings.
#[must_use]
fn layout_error_message(error: StitchLayoutError) -> String {
    match error {
        StitchLayoutError::Empty => t!("page_manager.stitch_dialog.empty_error").to_string(),
        StitchLayoutError::CanvasTooLarge { width, height } => tf!(
            "page_manager.stitch_dialog.canvas_too_large_error",
            width = width,
            height = height,
            max_side = stitch_layout::MAX_CANVAS_SIDE_PX,
            max_pixels = stitch_layout::MAX_CANVAS_PIXELS
        ),
        StitchLayoutError::FitNotAvailable => {
            t!("page_manager.stitch_dialog.fit_not_available_error").to_string()
        }
        StitchLayoutError::ScaleOutOfRange { page_idx, scale } => tf!(
            "page_manager.stitch_dialog.scale_out_of_range_error",
            page = page_idx + 1,
            scale = format!("{scale:.3}"),
            max_scale = stitch_layout::MAX_PLACEMENT_SCALE
        ),
        StitchLayoutError::DegeneratePage { page_idx } => tf!(
            "page_manager.stitch_dialog.degenerate_page_error",
            page = page_idx + 1
        ),
    }
}

impl PageManagerTabState {
    /// Draws the "stitch pages" window. Returns the state to keep, or `None`
    /// when the dialog closed this frame (confirmed, cancelled, or invalidated).
    ///
    /// `page_infos` supplies authoritative page geometry; pages missing from it
    /// fall back to the thumbnail/preview probe, and the board stays in its
    /// "loading" state until every selected page has a size.
    pub(super) fn draw_stitch_dialog(
        &mut self,
        ctx: &egui::Context,
        mut state: StitchDialogState,
        project: &ProjectData,
        page_infos: &HashMap<usize, PageImageInfo>,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) -> Option<StitchDialogState> {
        // `clamp_selection` drops out-of-range indices every frame, so a reload
        // can shrink the selection under an open dialog. Re-validate here, not
        // only when the window opened.
        let page_count = project.pages.len();
        if state.pages.len() < 2 || state.pages.iter().any(|idx| *idx >= page_count) {
            self.error_message =
                Some(t!("page_manager.stitch_dialog.selection_lost_error").to_string());
            return None;
        }

        self.seed_stitch_placements(&mut state, project, page_infos);

        let mut keep_open = true;
        let mut close_clicked = false;
        let mut confirm_clicked = false;
        egui::Window::new(t!("page_manager.stitch_dialog.title"))
            .id(egui::Id::new("page_manager_stitch_dialog"))
            .open(&mut keep_open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(1040.0, 720.0))
            .min_width(760.0)
            .min_height(520.0)
            .show(ctx, |ui| {
                egui::Panel::top("page_manager_stitch_settings").show(ui, |ui| {
                    draw_stitch_settings(ui, &mut state);
                });
                egui::Panel::bottom("page_manager_stitch_actions").show(ui, |ui| {
                    draw_stitch_actions(
                        ui,
                        &state,
                        op_in_progress,
                        &mut confirm_clicked,
                        &mut close_clicked,
                    );
                });
                egui::CentralPanel::default().show(ui, |ui| {
                    self.draw_stitch_board(ui, &mut state, project);
                });
            });

        if confirm_clicked {
            let background = background_rgba(state.background, state.transparent);
            match build_stitch_op(&state.placements, background) {
                Ok(op) => {
                    actions.push(PageManagerAction::RequestOp(op));
                    return None;
                }
                Err(error) => {
                    // Confirm is disabled while the layout is invalid, so this can
                    // only be a race with the very frame the layout became invalid.
                    state.fit_error = Some(layout_error_message(error));
                }
            }
        }
        if !keep_open || close_clicked {
            return None;
        }
        Some(state)
    }

    /// Fills `state.placements` once every selected page has a known pixel size.
    ///
    /// Pages whose size is still unknown get a thumbnail request (the thumbnail
    /// worker reports the full image dimensions), so the dialog converges without
    /// ever decoding anything on the GUI thread.
    fn seed_stitch_placements(
        &mut self,
        state: &mut StitchDialogState,
        project: &ProjectData,
        page_infos: &HashMap<usize, PageImageInfo>,
    ) {
        if !state.placements.is_empty() {
            return;
        }
        let mut sizes: Vec<[u32; 2]> = Vec::with_capacity(state.pages.len());
        let mut missing = false;
        for &idx in &state.pages {
            match self.page_pixel_size(idx, project, page_infos) {
                Some(size) => sizes.push(size),
                None => {
                    missing = true;
                    // Ask the thumbnail worker for this page; its reply carries the
                    // full image dimensions we are waiting for.
                    self.thumbs
                        .request_thumb_if_needed(&project.pages[idx].path, self.generation);
                }
            }
        }
        if missing {
            return;
        }
        state.placements = state
            .pages
            .iter()
            .zip(sizes)
            .map(|(&page_idx, size)| EditPlacement::new(page_idx, size))
            .collect();
        // The initial layout is the plan's default: a row in ascending page-index
        // order, centred on the cross axis, nothing resampled.
        stitch_layout::arrange_row(&mut state.placements, state.align);
    }

    /// Draws the board: camera input, page dragging with snapping, and painting.
    fn draw_stitch_board(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut StitchDialogState,
        project: &ProjectData,
    ) {
        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::CornerRadius::ZERO, ui.visuals().extreme_bg_color);

        if state.placements.is_empty() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                t!("page_manager.stitch_dialog.loading_sizes"),
                egui::FontId::proportional(15.0),
                ui.visuals().weak_text_color(),
            );
            return;
        }

        self.handle_stitch_board_input(ui, state, rect, &response);
        let view = state.viewport.transform(rect);
        self.paint_stitch_board(ui, state, project, rect, &view);
    }

    /// Applies wheel zoom, panning, and page dragging for this frame.
    fn handle_stitch_board_input(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut StitchDialogState,
        rect: egui::Rect,
        response: &egui::Response,
    ) {
        // Fit the whole layout on the first frame that has a real rect, so the
        // default view is strongly zoomed out even for 18 000 px ribbons.
        if !state.camera_fitted && rect.width() > 1.0 && rect.height() > 1.0 {
            fit_camera_to_layout(&mut state.viewport, rect, &state.placements);
            state.camera_fitted = true;
        }

        // Wheel: sign only (see WHEEL_ZOOM_STEP), anchored on the cursor.
        let wheel_y = if response.hovered() {
            ui.ctx()
                .input(|input| crate::input_util::raw_wheel_delta(input).y)
        } else {
            0.0
        };
        let wheel_for_zoom = if wheel_y > 0.0 {
            WHEEL_ZOOM_STEP
        } else if wheel_y < 0.0 {
            -WHEEL_ZOOM_STEP
        } else {
            0.0
        };
        let anchor = response.hover_pos().filter(|pos| rect.contains(*pos));
        state
            .viewport
            .handle_input(rect, anchor, wheel_for_zoom, egui::Vec2::ZERO);

        if response.drag_stopped() {
            state.drag = None;
        }
        if response.drag_started() {
            let hit_view = state.viewport.transform(rect);
            state.drag = Some(start_board_drag(state, response, &hit_view));
        }
        if !response.dragged() {
            state.drag = None;
            return;
        }
        match state.drag {
            Some(BoardDrag::Pan) | None => {
                state.viewport.handle_input(
                    rect,
                    anchor,
                    0.0,
                    response.drag_delta(),
                );
            }
            Some(BoardDrag::Page { index, grab, .. }) => {
                let view = state.viewport.transform(rect);
                let snap = drag_page(state, index, grab, response, &view);
                if snap.is_some() {
                    // The warning described the layout as it was BEFORE this move
                    // (a refused fit mode may now be available, or the other way
                    // round), so it must not outlive the layout that produced it.
                    state.fit_error = None;
                }
                state.drag = Some(BoardDrag::Page { index, grab, snap });
            }
        }
    }

    /// Paints the canvas background, every placed page, and the snap guides.
    fn paint_stitch_board(
        &mut self,
        ui: &mut egui::Ui,
        state: &StitchDialogState,
        project: &ProjectData,
        rect: egui::Rect,
        view: &ViewTransform,
    ) {
        let painter = ui.painter_at(rect);
        let visuals = ui.visuals().clone();

        // The canvas the engine would produce: the bounding box of every page.
        if let Some(bbox) = stitch_layout::bounding_box(&state.placements) {
            let canvas = world_rect_to_screen(view, bbox_to_world(bbox));
            let fill = if state.transparent {
                // No checkerboard: at a strongly zoomed-out board it would cost
                // thousands of shapes per frame. A neutral tone plus the
                // «Прозрачный» checkbox states the same thing for free.
                egui::Color32::from_gray(48)
            } else {
                egui::Color32::from_rgb(
                    state.background[0],
                    state.background[1],
                    state.background[2],
                )
            };
            painter.rect_filled(canvas, egui::CornerRadius::ZERO, fill);
            painter.rect_stroke(
                canvas,
                egui::CornerRadius::ZERO,
                egui::Stroke::new(1.0, visuals.widgets.noninteractive.fg_stroke.color),
                egui::StrokeKind::Outside,
            );
        }

        // Only the pages that occupy the most screen area get a real preview:
        // the preview LRU is small on purpose (see MAX_LIVE_PREVIEWS).
        let screen_rects: Vec<egui::Rect> = state
            .placements
            .iter()
            .map(|placement| world_rect_to_screen(view, placement.rect().to_world()))
            .collect();
        let scores: Vec<f64> = screen_rects
            .iter()
            .map(|screen| {
                let visible = screen.intersect(rect);
                if visible.is_negative() {
                    0.0
                } else {
                    f64::from(visible.width().max(0.0)) * f64::from(visible.height().max(0.0))
                }
            })
            .collect();
        let live = top_scored_indices(&scores, MAX_LIVE_PREVIEWS);

        let dragged = match state.drag {
            Some(BoardDrag::Page { index, .. }) => Some(index),
            Some(BoardDrag::Pan) | None => None,
        };
        for (index, placement) in state.placements.iter().enumerate() {
            let screen = screen_rects[index];
            if !screen.intersects(rect) {
                continue;
            }
            let path = &project.pages[placement.page_idx].path;
            // Outside the cap a decode is never REQUESTED (that is what keeps the
            // small LRU from thrashing), but a texture that is still cached is
            // drawn anyway — and read without touching LRU order, so a capped page
            // cannot evict a live one.
            let requested = live.contains(&index);
            let preview = if requested {
                self.thumbs
                    .request_preview_if_needed(path, PREVIEW_LONG_SIDE_PX, self.generation);
                self.thumbs.preview_state(path)
            } else {
                self.thumbs.preview_state_cached(path)
            };
            match preview {
                // A degenerate texture would sample garbage over the whole quad,
                // so it is treated as a failed decode rather than painted.
                PreviewState::Ready { texture, size, .. }
                    if size.x > 0.0 && size.y > 0.0 =>
                {
                    painter.image(texture, screen, crop_uv(placement), egui::Color32::WHITE);
                }
                PreviewState::Ready { .. } | PreviewState::Pending | PreviewState::Failed => {
                    painter.rect_filled(
                        screen,
                        egui::CornerRadius::ZERO,
                        visuals.widgets.noninteractive.weak_bg_fill,
                    );
                    let caption = placeholder_caption(&preview, requested);
                    if let Some(caption) = caption {
                        painter.text(
                            screen.center(),
                            egui::Align2::CENTER_CENTER,
                            caption,
                            egui::FontId::proportional(13.0),
                            visuals.weak_text_color(),
                        );
                    }
                }
            }
            let stroke = if dragged == Some(index) {
                visuals.selection.stroke
            } else {
                egui::Stroke::new(1.0, visuals.widgets.inactive.fg_stroke.color)
            };
            painter.rect_stroke(
                screen,
                egui::CornerRadius::ZERO,
                stroke,
                egui::StrokeKind::Inside,
            );
            paint_page_badge(&painter, &visuals, screen, placement.page_idx + 1);
        }

        if let Some(BoardDrag::Page {
            index,
            snap: Some(snap),
            ..
        }) = state.drag
        {
            paint_snap_guides(&painter, state, index, &snap, view, rect);
        }
    }
}

/// Draws the settings strip: quick arrangements, cross-axis alignment, fit
/// mode, and the canvas background.
fn draw_stitch_settings(ui: &mut egui::Ui, state: &mut StitchDialogState) {
    let seeded = !state.placements.is_empty();
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("page_manager.stitch_dialog.arrange_label"));
        if ui
            .add_enabled(
                seeded,
                egui::Button::new(t!("page_manager.stitch_dialog.arrange_row_button")),
            )
            .clicked()
        {
            arrange_stitch_layout(state, stitch_layout::arrange_row);
        }
        if ui
            .add_enabled(
                seeded,
                egui::Button::new(t!("page_manager.stitch_dialog.arrange_column_button")),
            )
            .clicked()
        {
            arrange_stitch_layout(state, stitch_layout::arrange_column);
        }
        ui.separator();
        ui.label(t!("page_manager.stitch_dialog.align_label"));
        let mut align = state.align;
        ui.radio_value(
            &mut align,
            CrossAlign::Start,
            t!("page_manager.stitch_dialog.align_start_radio"),
        );
        ui.radio_value(
            &mut align,
            CrossAlign::Center,
            t!("page_manager.stitch_dialog.align_center_radio"),
        );
        ui.radio_value(
            &mut align,
            CrossAlign::End,
            t!("page_manager.stitch_dialog.align_end_radio"),
        );
        if align != state.align {
            state.align = align;
            apply_stitch_fit(state, state.fit_mode);
        }
    });

    ui.add_space(4.0);
    let free_layout = matches!(
        stitch_layout::layout_kind(&state.placements),
        LayoutKind::Free
    );
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("page_manager.stitch_dialog.fit_label"));
        let mut mode = state.fit_mode;
        // «Fill» means "nothing resampled or cut", so clicking it must be able to
        // reset the pixels even when it is ALREADY the selected mode — otherwise a
        // layout that reached `Fill` with scales still applied has no way back.
        let mut refill_clicked = false;
        ui.add_enabled_ui(seeded, |ui| {
            if ui
                .radio_value(&mut mode, FitMode::Fill, fit_mode_label(FitMode::Fill))
                .clicked()
            {
                refill_clicked = true;
            }
        });
        // The three resampling/cropping modes only have a defined meaning for a
        // pure row or a pure column: a free arrangement has no single cross axis
        // to fit, so it can only be filled with background.
        ui.add_enabled_ui(seeded && !free_layout, |ui| {
            for candidate in [FitMode::ScaleToSmaller, FitMode::ScaleToLarger, FitMode::Crop] {
                ui.radio_value(&mut mode, candidate, fit_mode_label(candidate))
                    .on_disabled_hover_text(t!(
                        "page_manager.stitch_dialog.fit_free_disabled_tooltip"
                    ));
            }
        });
        if mode != state.fit_mode || refill_clicked {
            apply_stitch_fit(state, mode);
        }
    });

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("page_manager.stitch_dialog.background_label"));
        ui.add_enabled_ui(!state.transparent, |ui| {
            ui.color_edit_button_srgb(&mut state.background);
        });
        ui.checkbox(
            &mut state.transparent,
            t!("page_manager.stitch_dialog.background_transparent_checkbox"),
        );
        ui.separator();
        ui.add(
            egui::Label::new(
                egui::RichText::new(t!("page_manager.stitch_dialog.board_hint")).weak(),
            )
            .selectable(false),
        );
    });
    if let Some(message) = state.fit_error.as_ref() {
        ui.colored_label(ui.visuals().warn_fg_color, message);
    }
    ui.add_space(4.0);
}

/// Draws the bottom strip: resulting canvas size, the "applied immediately"
/// warning, and the confirm / cancel buttons.
fn draw_stitch_actions(
    ui: &mut egui::Ui,
    state: &StitchDialogState,
    op_in_progress: bool,
    confirm_clicked: &mut bool,
    close_clicked: &mut bool,
) {
    // The canvas is derived from the placements alone, so the strip does not need
    // the whole engine request — only whether the layout is valid and how big it is.
    let layout = normalized_stitch_layout(&state.placements);
    ui.add_space(6.0);
    match &layout {
        Ok((_, size)) => {
            ui.label(tf!(
                "page_manager.stitch_dialog.canvas_size_label",
                width = size.width,
                height = size.height
            ));
        }
        Err(error) => {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                layout_error_message(*error),
            );
        }
    }
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(egui::RichText::new(
            t!("page_manager.stitch_dialog.apply_warning"),
        ))
        .wrap(),
    );
    ui.add(
        egui::Label::new(tf!(
            "page_manager.stitch_dialog.trash_note",
            dir = super::dialogs::PAGE_OP_TRASH_DIR
        ))
        .wrap(),
    );
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !op_in_progress && layout.is_ok(),
                egui::Button::new(t!("page_manager.stitch_dialog.confirm_button")),
            )
            .clicked()
        {
            *confirm_clicked = true;
        }
        if ui.button(t!("page_manager.dialog.cancel_button")).clicked() {
            *close_clicked = true;
        }
    });
    ui.add_space(6.0);
}

/// Runs a quick arrangement and puts the layout into the [`FitMode::Fill`] state
/// the settings strip then reports.
///
/// `arrange` only writes positions — crops and scales survive it — so the pixels
/// are reset FIRST. Without that reset the radio would claim «nothing resampled»
/// while the pages still carried the scale of a previous fit mode.
fn arrange_stitch_layout(
    state: &mut StitchDialogState,
    arrange: fn(&mut [EditPlacement], CrossAlign),
) {
    state.placements.iter_mut().for_each(EditPlacement::reset_pixels);
    arrange(&mut state.placements, state.align);
    state.fit_mode = FitMode::Fill;
    state.fit_error = None;
    state.camera_fitted = false;
}

/// Applies `mode` to the layout, recording a localized message if the layout
/// core refuses it. On refusal the mode falls back to [`FitMode::Fill`], which
/// is always available.
fn apply_stitch_fit(state: &mut StitchDialogState, mode: FitMode) {
    match stitch_layout::apply_fit(&mut state.placements, mode, state.align) {
        Ok(()) => {
            state.fit_mode = mode;
            state.fit_error = None;
        }
        Err(error) => {
            state.fit_error = Some(layout_error_message(error));
            state.fit_mode = FitMode::Fill;
            // `apply_fit` already reset crops and scales; re-running Fill cannot
            // fail and leaves the layout in the state the radio now claims.
            if stitch_layout::apply_fit(&mut state.placements, FitMode::Fill, state.align).is_err()
            {
                state.placements.iter_mut().for_each(EditPlacement::reset_pixels);
            }
        }
    }
    state.camera_fitted = false;
}

/// Decides what a freshly started drag does: pan the board, or move a page.
fn start_board_drag(
    state: &StitchDialogState,
    response: &egui::Response,
    view: &ViewTransform,
) -> BoardDrag {
    // Only the primary button moves a page; every other button (middle, and a
    // stray secondary drag) pans, so a right-drag can never displace a page.
    if !response.dragged_by(egui::PointerButton::Primary) {
        return BoardDrag::Pan;
    }
    let Some(screen) = response.interact_pointer_pos() else {
        return BoardDrag::Pan;
    };
    let world = view.screen_to_world(screen);
    let point = [f64::from(world.x), f64::from(world.y)];
    match hit_test(&state.placements, point) {
        Some(index) => {
            let rect = state.placements[index].rect();
            BoardDrag::Page {
                index,
                grab: [point[0] - i64_to_f64(rect.x), point[1] - i64_to_f64(rect.y)],
                snap: None,
            }
        }
        // A primary drag on empty space pans, mirroring the other canvases.
        None => BoardDrag::Pan,
    }
}

/// Moves the dragged page to the pointer, snapped to the other pages.
///
/// Returns the snap result so the caller can paint the guides that fired.
fn drag_page(
    state: &mut StitchDialogState,
    index: usize,
    grab: [f64; 2],
    response: &egui::Response,
    view: &ViewTransform,
) -> Option<SnapResult> {
    let screen = response.interact_pointer_pos()?;
    let world = view.screen_to_world(screen);
    let [width, height] = state.placements.get(index)?.placed_size();
    let candidate = WorldRect::from_min_size(
        f64::from(world.x) - grab[0],
        f64::from(world.y) - grab[1],
        f64::from(width),
        f64::from(height),
    );
    let others: Vec<WorldRect> = state
        .placements
        .iter()
        .enumerate()
        .filter(|(other, _)| *other != index)
        .map(|(_, placement)| placement.rect().to_world())
        .collect();
    // The magnet is specified in screen points, so it must shrink with the zoom
    // to stay the same distance on screen at any magnification.
    let zoom = f64::from(view.zoom.max(f32::EPSILON));
    let snap = stitch_layout::snap_drag(candidate, &others, SNAP_RADIUS_POINTS / zoom);
    let placed = snap.snapped_rect(candidate);
    let placement = state.placements.get_mut(index)?;
    placement.dx = round_to_canvas_px(placed.min_x);
    placement.dy = round_to_canvas_px(placed.min_y);
    Some(snap)
}

/// Fits the camera so the whole layout is visible with a small margin.
fn fit_camera_to_layout(
    viewport: &mut PsViewport,
    rect: egui::Rect,
    placements: &[EditPlacement],
) {
    let Some(bbox) = stitch_layout::bounding_box(placements) else {
        return;
    };
    let world = bbox_to_world(bbox);
    let width = world.width().max(1.0);
    let height = world.height().max(1.0);
    let zoom_x = f64::from(rect.width()) / width;
    let zoom_y = f64::from(rect.height()) / height;
    let zoom = (zoom_x.min(zoom_y) * f64::from(1.0 - FIT_MARGIN)).max(f64::from(f32::MIN_POSITIVE));
    // `set_camera` clamps the zoom into the viewport's own range, so a layout
    // larger than the widest zoom-out simply stays partly off-screen.
    viewport.set_camera(
        f64_to_f32(zoom),
        egui::Vec2::new(
            f64_to_f32(world.center_x()),
            f64_to_f32(world.center_y()),
        ),
    );
}

/// Paints the snap guides that fired, spanning the dragged page and the page it
/// snapped to, so the user sees WHICH edge the magnet caught.
fn paint_snap_guides(
    painter: &egui::Painter,
    state: &StitchDialogState,
    index: usize,
    snap: &SnapResult,
    view: &ViewTransform,
    board: egui::Rect,
) {
    let dragged = state.placements[index].rect().to_world();
    // `SnapGuide::other` indexes the `others` slice, which skipped the dragged
    // page; shift it back into the placement list.
    let resolve = |other: usize| -> usize {
        if other >= index { other + 1 } else { other }
    };
    for guide in [snap.x, snap.y].into_iter().flatten() {
        let partner = state
            .placements
            .get(resolve(guide.other))
            .map(|placement| placement.rect().to_world());
        let color = match guide.kind {
            SnapKind::Adjacency => egui::Color32::from_rgb(120, 220, 140),
            SnapKind::Alignment => egui::Color32::from_rgb(240, 180, 80),
        };
        let stroke = egui::Stroke::new(1.5, color);
        match guide.axis {
            SnapAxis::X => {
                let (mut lo, mut hi) = (dragged.min_y, dragged.max_y);
                if let Some(other) = partner {
                    lo = lo.min(other.min_y);
                    hi = hi.max(other.max_y);
                }
                let x = view
                    .world_to_screen(egui::pos2(f64_to_f32(guide.position), 0.0))
                    .x;
                let y0 = view.world_to_screen(egui::pos2(0.0, f64_to_f32(lo))).y;
                let y1 = view.world_to_screen(egui::pos2(0.0, f64_to_f32(hi))).y;
                painter.line_segment(
                    [
                        egui::pos2(x, y0.max(board.top())),
                        egui::pos2(x, y1.min(board.bottom())),
                    ],
                    stroke,
                );
            }
            SnapAxis::Y => {
                let (mut lo, mut hi) = (dragged.min_x, dragged.max_x);
                if let Some(other) = partner {
                    lo = lo.min(other.min_x);
                    hi = hi.max(other.max_x);
                }
                let y = view
                    .world_to_screen(egui::pos2(0.0, f64_to_f32(guide.position)))
                    .y;
                let x0 = view.world_to_screen(egui::pos2(f64_to_f32(lo), 0.0)).x;
                let x1 = view.world_to_screen(egui::pos2(f64_to_f32(hi), 0.0)).x;
                painter.line_segment(
                    [
                        egui::pos2(x0.max(board.left()), y),
                        egui::pos2(x1.min(board.right()), y),
                    ],
                    stroke,
                );
            }
        }
    }
}

/// Paints the 1-based page number in the corner of a placed page.
fn paint_page_badge(
    painter: &egui::Painter,
    visuals: &egui::Visuals,
    screen: egui::Rect,
    number: usize,
) {
    let anchor = screen.left_top() + egui::vec2(4.0, 4.0);
    let text = painter.layout_no_wrap(
        number.to_string(),
        egui::FontId::proportional(13.0),
        visuals.strong_text_color(),
    );
    let badge = egui::Rect::from_min_size(anchor, text.size() + egui::vec2(8.0, 4.0));
    painter.rect_filled(
        badge,
        egui::CornerRadius::same(3),
        egui::Color32::from_black_alpha(160),
    );
    painter.galley(badge.min + egui::vec2(4.0, 2.0), text, visuals.strong_text_color());
}

/// UV rect of a placement's crop inside its own page image.
fn crop_uv(placement: &EditPlacement) -> egui::Rect {
    let page_w = u32_to_f32(placement.page_size[0].max(1));
    let page_h = u32_to_f32(placement.page_size[1].max(1));
    let [x, y, w, h] = placement.crop;
    egui::Rect::from_min_max(
        egui::pos2(u32_to_f32(x) / page_w, u32_to_f32(y) / page_h),
        egui::pos2(
            u32_to_f32(x.saturating_add(w)) / page_w,
            u32_to_f32(y.saturating_add(h)) / page_h,
        ),
    )
}

/// Screen rect of a world rect.
fn world_rect_to_screen(view: &ViewTransform, world: WorldRect) -> egui::Rect {
    egui::Rect::from_min_max(
        view.world_to_screen(egui::pos2(
            f64_to_f32(world.min_x),
            f64_to_f32(world.min_y),
        )),
        view.world_to_screen(egui::pos2(
            f64_to_f32(world.max_x),
            f64_to_f32(world.max_y),
        )),
    )
}

/// The layout bounding box as a float rect.
fn bbox_to_world(bbox: stitch_layout::BoundingBox) -> WorldRect {
    WorldRect::from_min_size(
        i64_to_f64(bbox.min_x),
        i64_to_f64(bbox.min_y),
        i64_to_f64(bbox.max_x - bbox.min_x),
        i64_to_f64(bbox.max_y - bbox.min_y),
    )
}

/// Widening conversion of a canvas coordinate. Exact for every value the layout
/// core can produce (`|v| <= COORD_LIMIT_PX`, far below 2^53).
fn i64_to_f64(value: i64) -> f64 {
    // `as` is the only conversion available here; f64 represents every i64 up to
    // 2^53 exactly, and canvas coordinates never leave that range.
    value as f64
}

/// Narrowing conversion used for painting only: screen geometry is f32 in egui,
/// and a rounding error below one screen point is invisible.
fn f64_to_f32(value: f64) -> f32 {
    value as f32
}

/// Widening conversion of a pixel count for UV math. Exact below 2^24 px, and a
/// larger page's UV error is far below one texel.
fn u32_to_f32(value: u32) -> f32 {
    value as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(page_idx: usize, size: [u32; 2], dx: i64, dy: i64) -> EditPlacement {
        let mut placement = EditPlacement::new(page_idx, size);
        placement.dx = dx;
        placement.dy = dy;
        placement
    }

    #[test]
    fn background_rgba_makes_transparent_fully_clear() {
        assert_eq!(background_rgba([10, 20, 30], false), [10, 20, 30, 255]);
        assert_eq!(background_rgba([10, 20, 30], true), [0, 0, 0, 0]);
    }

    #[test]
    fn round_to_canvas_px_is_total() {
        assert_eq!(round_to_canvas_px(1.4), 1);
        assert_eq!(round_to_canvas_px(-1.5), -2);
        assert_eq!(round_to_canvas_px(f64::NAN), 0);
        assert_eq!(round_to_canvas_px(f64::INFINITY), 4_000_000);
        assert_eq!(round_to_canvas_px(f64::NEG_INFINITY), -4_000_000);
        assert_eq!(round_to_canvas_px(1e18), 4_000_000);
    }

    #[test]
    fn hit_test_prefers_the_topmost_page() {
        let placements = vec![
            placement(0, [100, 100], 0, 0),
            placement(1, [100, 100], 50, 50),
        ];
        assert_eq!(hit_test(&placements, [10.0, 10.0]), Some(0));
        // Overlap: the later entry is painted on top and must win.
        assert_eq!(hit_test(&placements, [60.0, 60.0]), Some(1));
        assert_eq!(hit_test(&placements, [400.0, 400.0]), None);
        // The rect is half-open: the far edge belongs to the next pixel.
        assert_eq!(hit_test(&placements, [100.0, 10.0]), None);
    }

    #[test]
    fn top_scored_indices_keeps_the_largest_and_stays_sorted() {
        let scores = [1.0, 9.0, 3.0, 7.0];
        assert_eq!(top_scored_indices(&scores, 2), vec![1, 3]);
        assert_eq!(top_scored_indices(&scores, 10), vec![0, 1, 2, 3]);
        assert_eq!(top_scored_indices(&scores, 0), Vec::<usize>::new());
        // Equal scores resolve towards the lower index, so the choice is stable.
        assert_eq!(top_scored_indices(&[5.0, 5.0, 5.0], 2), vec![0, 1]);
    }

    #[test]
    fn build_stitch_op_normalizes_to_a_zero_origin() {
        let placements = vec![
            placement(3, [100, 200], -40, -10),
            placement(1, [100, 200], 60, 90),
        ];
        let op = build_stitch_op(&placements, [1, 2, 3, 4]).expect("layout fits the budget");
        match op {
            PageOpKind::Stitch {
                placements,
                width,
                height,
                background,
            } => {
                assert_eq!(background, [1, 2, 3, 4]);
                assert_eq!(width, 200);
                assert_eq!(height, 300);
                assert_eq!(placements[0].page_idx, 3);
                assert_eq!((placements[0].dx, placements[0].dy), (0, 0));
                assert_eq!((placements[1].dx, placements[1].dy), (100, 100));
                assert_eq!(placements[0].crop, [0, 0, 100, 200]);
                assert!((placements[0].scale - 1.0).abs() < f32::EPSILON);
            }
            other => panic!("expected a Stitch op, got {other:?}"),
        }
        // The source list is never mutated by building the request.
        assert_eq!(placements[0].dx, -40);
    }

    #[test]
    fn build_stitch_op_refuses_fewer_than_two_pages() {
        let placements = vec![placement(0, [10, 10], 0, 0)];
        assert_eq!(
            build_stitch_op(&placements, [0, 0, 0, 0]),
            Err(StitchLayoutError::Empty)
        );
        assert_eq!(
            build_stitch_op(&[], [0, 0, 0, 0]),
            Err(StitchLayoutError::Empty)
        );
    }

    #[test]
    fn placeholder_kind_separates_capped_from_loading() {
        // Both are `Pending`; only the request flag tells them apart.
        assert_eq!(
            placeholder_kind(&PreviewState::Pending, true),
            PlaceholderKind::Loading
        );
        assert_eq!(
            placeholder_kind(&PreviewState::Pending, false),
            PlaceholderKind::Capped
        );
        // A capped page never promises an image it will never get.
        assert_eq!(placeholder_caption(&PreviewState::Pending, false), None);
        assert!(placeholder_caption(&PreviewState::Pending, true).is_some());
    }

    #[test]
    fn placeholder_kind_reports_a_failure_even_when_capped() {
        assert_eq!(
            placeholder_kind(&PreviewState::Failed, false),
            PlaceholderKind::Failed
        );
        // A `Ready` only reaches a placeholder with an unusable texture size.
        let degenerate = PreviewState::Ready {
            texture: egui::TextureId::Managed(0),
            size: egui::Vec2::ZERO,
            full_size: None,
        };
        assert_eq!(placeholder_kind(&degenerate, true), PlaceholderKind::Failed);
    }

    #[test]
    fn arranging_resets_the_pixels_the_fill_mode_claims() {
        let mut state = StitchDialogState::new(vec![0, 1]);
        state.placements = vec![placement(0, [100, 200], 0, 0), placement(1, [100, 200], 0, 0)];
        // A previous fit left page 0 scaled and cropped.
        state.placements[0].scale = 0.5;
        state.placements[0].crop = [0, 50, 100, 100];
        state.fit_mode = FitMode::Crop;
        state.fit_error = Some("stale".to_string());

        arrange_stitch_layout(&mut state, stitch_layout::arrange_row);

        assert_eq!(state.fit_mode, FitMode::Fill);
        assert_eq!(state.fit_error, None);
        for placed in &state.placements {
            assert!((placed.scale - 1.0).abs() < f32::EPSILON);
            assert_eq!(placed.crop, [0, 0, 100, 200]);
        }
        // Untouched pixels also mean the row is packed from the full sizes.
        assert_eq!((state.placements[1].dx, state.placements[1].dy), (100, 0));
    }

    #[test]
    fn crop_uv_maps_the_cropped_region() {
        let mut placed = EditPlacement::new(0, [200, 100]);
        placed.crop = [50, 25, 100, 50];
        let uv = crop_uv(&placed);
        assert!((uv.min.x - 0.25).abs() < 1e-6);
        assert!((uv.min.y - 0.25).abs() < 1e-6);
        assert!((uv.max.x - 0.75).abs() < 1e-6);
        assert!((uv.max.y - 0.75).abs() < 1e-6);
    }
}
