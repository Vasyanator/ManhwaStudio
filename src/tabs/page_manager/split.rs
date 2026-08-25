/*
File: tabs/page_manager/split.rs

Purpose:
The "split page" window (Layer 2 of `dev-docs/split_page_plan.md`): an
`egui::Window` that shows ONE selected page on a zoomable, pannable board with
parallel cut lines (all horizontal XOR all vertical), an order picker per
resulting part, and a confirm that emits `PageOpKind::Split`.

Key structures:
- SplitDialogState: the dialog's own state (axis, cuts, part order, camera).

Key functions:
- PageManagerTabState::draw_split_dialog(): the per-frame window.
- build_split_op(): state -> the engine request (pure).
- order_widget_rects(): where every part's order picker is placed (pure).
- dragged_cut_value(): a handle drag delta -> the new cut coordinate (pure).
- layout_error_message(): SplitLayoutError -> localized user text.

Notes:
All cut/part/order math lives in `split_layout.rs`; this file only draws and
routes input. Board world coordinates are SOURCE pixels, so a cut is stored at
full source precision no matter how coarse the preview is — the preview from the
bounded cache of `thumbs.rs` is only ever a VIEW, and the engine cuts the
untouched original. No decode ever happens on the GUI thread.
*/

use std::collections::HashMap;

use eframe::egui;

use crate::app::PageImageInfo;
use crate::page_ops::{PageOpKind, SplitAxis};
use crate::project::ProjectData;
use crate::tabs::ps_editor::viewport::{PsViewport, ViewTransform};
use crate::widgets::{WheelComboBox, combo_popup_open};

use super::split_layout::{self, SplitLayoutError, SplitPart};
use super::thumbs::{PreviewState, SPLIT_PREVIEW_LONG_SIDE_PX};
use super::{PageManagerAction, PageManagerTabState};

/// Zoom step handed to [`PsViewport::handle_input`] per wheel notch.
///
/// `raw_wheel_delta` is unit-dependent (`Point` / `Line` / `Page`), so its
/// magnitude must never be used as a distance (`egui-docs/03-input.md`). Only its
/// sign is read here and this fixed step is applied instead, which makes one
/// notch mean the same zoom change on every platform. Same rationale and value
/// as the stitch board's constant.
const WHEEL_ZOOM_STEP: f32 = 100.0;

/// Long side of the cut-line grab handle, in SCREEN points (the short side is
/// [`HANDLE_THICKNESS_POINTS`]). Screen-constant on purpose: the handle must stay
/// equally grabbable at any zoom.
const HANDLE_LENGTH_POINTS: f32 = 54.0;
/// Short side of the grab handle, in screen points.
const HANDLE_THICKNESS_POINTS: f32 = 18.0;
/// Diameter of the "delete this line" button, in screen points.
const DELETE_BUTTON_POINTS: f32 = 16.0;
/// Gap between the grab handle and the delete button, in screen points.
const DELETE_BUTTON_GAP_POINTS: f32 = 6.0;
/// Width of a part's order picker, in screen points.
const ORDER_WIDGET_WIDTH_POINTS: f32 = 104.0;
/// Height of a part's order picker, in screen points.
const ORDER_WIDGET_HEIGHT_POINTS: f32 = 22.0;
/// Inset of the order picker from the visible corner of its part, in screen points.
const ORDER_WIDGET_MARGIN_POINTS: f32 = 6.0;
/// Smallest gap between two consecutive order pickers, in screen points.
///
/// Together with the widget's own size it forms the picker PITCH — the distance
/// the sequence advances from one part to the next (see [`order_widget_rects`]).
const ORDER_WIDGET_GAP_POINTS: f32 = 4.0;

/// Hard bound applied to a world coordinate before it becomes a pixel index.
///
/// Far beyond any page size the engine accepts, but finite, so a wild drag at a
/// tiny zoom can never produce a non-finite or out-of-range cut coordinate.
const COORD_LIMIT_PX: f32 = 1.0e8;

/// State of the "split page" window.
///
/// `page_idx` is the selection snapshot taken when the window opened; it is
/// re-validated against the current page count on every frame, because
/// `clamp_selection` may silently drop it after a reload.
///
/// Invariant maintained by every mutation here: `order.len() == cuts.len() + 1`
/// and `order` is a permutation of `0..order.len()` (see `split_layout`).
pub(super) struct SplitDialogState {
    /// Index of the page being cut, in the CURRENT page order.
    page_idx: usize,
    /// The page's pixel size, once the thumbnail/preview probe has reported it.
    page_size: Option<[u32; 2]>,
    /// Orientation of every cut line.
    axis: SplitAxis,
    /// Cut coordinates along the cut axis, in SOURCE pixels, ascending.
    cuts: Vec<u32>,
    /// `order[k]` = page position of geometric part `k` (see `split_layout`).
    order: Vec<usize>,
    /// Board camera.
    viewport: PsViewport,
    /// Whether the camera has already been fit to the page.
    camera_fitted: bool,
    /// Cut coordinate captured at the last right-click, for the board's
    /// "add a line here" menu item. Captured on `secondary_clicked`, BEFORE the
    /// menu opens, because the pointer has moved by the time the closure runs.
    context_cut: Option<u32>,
}

impl SplitDialogState {
    /// Fresh dialog for the given current page index. Starts with horizontal
    /// cut lines and no cut, i.e. one part covering the whole page.
    pub(super) fn new(page_idx: usize) -> Self {
        Self {
            page_idx,
            page_size: None,
            axis: SplitAxis::Horizontal,
            cuts: Vec::new(),
            order: split_layout::default_order(1),
            viewport: PsViewport::default(),
            camera_fitted: false,
            context_cut: None,
        }
    }

    /// The page extent along the current cut axis, in source pixels; `0` while
    /// the page size is unknown.
    #[must_use]
    fn extent(&self) -> u32 {
        self.page_size.map_or(0, |size| axis_extent(self.axis, size))
    }

    /// Switches the cut orientation, dropping every existing cut.
    ///
    /// The cuts are coordinates on the OTHER axis, so they have no meaning after
    /// the switch; keeping them would silently move every line. A no-op when the
    /// axis is unchanged, so the user's work survives a redundant click.
    fn set_axis(&mut self, axis: SplitAxis) {
        if self.axis == axis {
            return;
        }
        self.axis = axis;
        self.cuts.clear();
        self.order = split_layout::default_order(1);
        self.camera_fitted = false;
    }
}

/// The page extent a cut coordinate is measured along: the height for horizontal
/// lines, the width for vertical ones.
#[must_use]
fn axis_extent(axis: SplitAxis, page_size: [u32; 2]) -> u32 {
    match axis {
        SplitAxis::Horizontal => page_size[1],
        SplitAxis::Vertical => page_size[0],
    }
}

/// The component of a world point that a cut coordinate is measured on.
#[must_use]
fn axis_coord(axis: SplitAxis, world: egui::Pos2) -> f32 {
    match axis {
        SplitAxis::Horizontal => world.y,
        SplitAxis::Vertical => world.x,
    }
}

/// World-space rect of one part of the page: the part's own interval along the
/// cut axis, the full page across it.
#[must_use]
fn part_world_rect(axis: SplitAxis, part: SplitPart, page_size: [u32; 2]) -> egui::Rect {
    let origin = u32_to_f32(part.origin);
    let size = u32_to_f32(part.size);
    match axis {
        SplitAxis::Horizontal => egui::Rect::from_min_size(
            egui::pos2(0.0, origin),
            egui::vec2(u32_to_f32(page_size[0]), size),
        ),
        SplitAxis::Vertical => egui::Rect::from_min_size(
            egui::pos2(origin, 0.0),
            egui::vec2(size, u32_to_f32(page_size[1])),
        ),
    }
}

/// Screen rects of every part's order picker, indexed by GEOMETRIC part.
///
/// `parts_screen[k]` is part `k`'s full screen rect (it may lie partly or wholly
/// outside `board`); the answer is `None` only for a part with no visible area at
/// all. Every VISIBLE part gets a picker of the full fixed `size`, whatever the
/// zoom — a part 3 pt wide on screen still gets a readable widget, which is the
/// whole point: on a webtoon ribbon at fit zoom the page is a few dozen points
/// wide and a picker sized to the part would never appear.
///
/// Placement rule:
/// - ACROSS the cut axis the picker hugs its part's visible trailing edge (the
///   right edge for horizontal cuts, the top edge for vertical ones) and is then
///   clamped into the board, so a picker wider than its own part simply overhangs
///   the page instead of disappearing.
/// - ALONG the cut axis the pickers form a strictly ordered, non-overtaking
///   sequence: each one starts at its part's visible leading edge and is then
///   pushed to at least `pitch` past its predecessor, pulled back inside the
///   board from the tail, and pushed back inside it from the head.
///
/// `pitch` is `size along the axis + ORDER_WIDGET_GAP_POINTS`, reduced to
/// `(available - size along the axis) / (visible parts - 1)` — the widest spacing
/// whose chain still ends inside the board — when that many pickers do not fit at
/// full pitch. Because the pickers are drawn in geometric order, each one keeps a
/// leading strip of `pitch` uncovered by its successor, so **no picker is ever
/// hidden under another** — at any zoom and any number of cuts.
#[must_use]
fn order_widget_rects(
    axis: SplitAxis,
    parts_screen: &[egui::Rect],
    board: egui::Rect,
    size: egui::Vec2,
) -> Vec<Option<egui::Rect>> {
    let mut placed: Vec<Option<egui::Rect>> = vec![None; parts_screen.len()];
    // Parts are contiguous along the cut axis, so the visible ones are a
    // contiguous run and keeping only them preserves their geometric order.
    let visible: Vec<(usize, egui::Rect)> = parts_screen
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
            let clipped = part.intersect(board);
            clipped.is_positive().then_some((index, clipped))
        })
        .collect();
    if visible.is_empty() {
        return placed;
    }

    let (extent_along, board_min, board_max) = match axis {
        SplitAxis::Horizontal => (size.y, board.top(), board.bottom()),
        SplitAxis::Vertical => (size.x, board.left(), board.right()),
    };
    let available = (board_max - board_min - ORDER_WIDGET_MARGIN_POINTS * 2.0).max(0.0);
    // The chain spans `(n - 1) * pitch + extent`, so this is the largest pitch
    // that still lands the LAST picker inside the board. Infallible conversion
    // for any realistic part count; a saturating one would merely shrink the
    // pitch further, never make the layout wrong. Floored at 1 pt: a zero pitch
    // would stack every picker on one point, which is exactly the "hidden under
    // one another" state this rule forbids.
    let steps = u32::try_from(visible.len().saturating_sub(1)).unwrap_or(u32::MAX);
    let pitch = if steps == 0 {
        extent_along + ORDER_WIDGET_GAP_POINTS
    } else {
        (extent_along + ORDER_WIDGET_GAP_POINTS)
            .min(((available - extent_along) / u32_to_f32(steps)).max(1.0))
    };
    let first_allowed = board_min + ORDER_WIDGET_MARGIN_POINTS;
    let last_allowed = (board_max - extent_along - ORDER_WIDGET_MARGIN_POINTS).max(first_allowed);

    let mut along: Vec<f32> = Vec::with_capacity(visible.len());
    for (order, (_, clipped)) in visible.iter().enumerate() {
        let ideal = match axis {
            SplitAxis::Horizontal => clipped.top() + ORDER_WIDGET_MARGIN_POINTS,
            SplitAxis::Vertical => clipped.right() - ORDER_WIDGET_MARGIN_POINTS - size.x,
        };
        let previous = order.checked_sub(1).and_then(|prev| along.get(prev).copied());
        along.push(previous.map_or(ideal, |value| ideal.max(value + pitch)));
    }
    // Pull the tail back inside the board, keeping the pitch: the last picker
    // must stay clickable even when its part ends past the board edge.
    for index in (0..along.len()).rev() {
        let next = along.get(index + 1).copied();
        let limit = next.map_or(last_allowed, |value| last_allowed.min(value - pitch));
        if let Some(slot) = along.get_mut(index) {
            *slot = slot.min(limit);
        }
    }
    // ... then push the head back in. This is what saves the VERTICAL case, where
    // a picker anchored to a part narrower than itself starts left of the board.
    // The trailing `min` only bites on a board too small to hold the chain even
    // at the 1 pt pitch floor (hundreds of parts): staying inside the board wins
    // over staying distinct, because a picker painted outside it is not merely
    // covered, it is clipped away entirely.
    for index in 0..along.len() {
        let previous = index.checked_sub(1).and_then(|prev| along.get(prev).copied());
        let lower = previous.map_or(first_allowed, |value| first_allowed.max(value + pitch));
        if let Some(slot) = along.get_mut(index) {
            *slot = slot.max(lower).min(last_allowed);
        }
    }

    for (order, (index, clipped)) in visible.iter().enumerate() {
        let Some(&along_pos) = along.get(order) else {
            continue;
        };
        let rect = match axis {
            SplitAxis::Horizontal => {
                let low = board.left() + ORDER_WIDGET_MARGIN_POINTS;
                let high = (board.right() - ORDER_WIDGET_MARGIN_POINTS - size.x).max(low);
                let x = (clipped.right() - ORDER_WIDGET_MARGIN_POINTS - size.x).clamp(low, high);
                egui::Rect::from_min_size(egui::pos2(x, along_pos), size)
            }
            SplitAxis::Vertical => {
                let low = board.top() + ORDER_WIDGET_MARGIN_POINTS;
                let high = (board.bottom() - ORDER_WIDGET_MARGIN_POINTS - size.y).max(low);
                let y = (clipped.top() + ORDER_WIDGET_MARGIN_POINTS).clamp(low, high);
                egui::Rect::from_min_size(egui::pos2(along_pos, y), size)
            }
        };
        if let Some(slot) = placed.get_mut(*index) {
            *slot = Some(rect);
        }
    }
    placed
}

/// Order-picker rects for the dialog's current state and camera, indexed by
/// geometric part.
///
/// Empty while the page is uncut: one part is nothing to order. Shared by the
/// drawing code and by the board's wheel guard, which must know where the
/// pickers are BEFORE it decides whether the wheel belongs to the camera.
#[must_use]
fn split_order_widget_rects(
    state: &SplitDialogState,
    board: egui::Rect,
    view: &ViewTransform,
    page_size: [u32; 2],
) -> Vec<Option<egui::Rect>> {
    let axis = state.axis;
    let parts = split_layout::parts(axis_extent(axis, page_size), &state.cuts);
    if parts.len() < 2 {
        return Vec::new();
    }
    let parts_screen: Vec<egui::Rect> = parts
        .iter()
        .map(|part| view.world_rect_to_screen(part_world_rect(axis, *part, page_size)))
        .collect();
    order_widget_rects(
        axis,
        &parts_screen,
        board,
        egui::vec2(ORDER_WIDGET_WIDTH_POINTS, ORDER_WIDGET_HEIGHT_POINTS),
    )
}

/// Screen rect of a cut line's grab handle: centred on the line, and on the
/// VIEWPORT CENTRE along it.
///
/// The along-line position is not state: the handle is simply always drawn at
/// the centre of the board, which is what makes it "slide along its line and
/// return to the middle" for free — dragging it along the line has no effect
/// because there is nothing along the line to store.
#[must_use]
fn handle_rect(axis: SplitAxis, board: egui::Rect, line_screen: f32) -> egui::Rect {
    match axis {
        SplitAxis::Horizontal => egui::Rect::from_center_size(
            egui::pos2(board.center().x, line_screen),
            egui::vec2(HANDLE_LENGTH_POINTS, HANDLE_THICKNESS_POINTS),
        ),
        SplitAxis::Vertical => egui::Rect::from_center_size(
            egui::pos2(line_screen, board.center().y),
            egui::vec2(HANDLE_THICKNESS_POINTS, HANDLE_LENGTH_POINTS),
        ),
    }
}

/// Screen rect of a cut line's delete button, just past the end of its handle.
#[must_use]
fn delete_rect(axis: SplitAxis, handle: egui::Rect) -> egui::Rect {
    let offset = DELETE_BUTTON_GAP_POINTS + DELETE_BUTTON_POINTS * 0.5;
    let center = match axis {
        SplitAxis::Horizontal => egui::pos2(handle.right() + offset, handle.center().y),
        SplitAxis::Vertical => egui::pos2(handle.center().x, handle.bottom() + offset),
    };
    egui::Rect::from_center_size(
        center,
        egui::vec2(DELETE_BUTTON_POINTS, DELETE_BUTTON_POINTS),
    )
}

/// Rounds a world coordinate to a whole source pixel.
///
/// `NaN` (which has no meaningful position) becomes `0`; every other value,
/// infinities included, is clamped into `0..=COORD_LIMIT_PX`, so a drag at any
/// zoom always yields a representable pixel index. The caller still clamps the
/// result into the page through [`split_layout::clamp_cut`].
#[must_use]
fn round_world_to_px(value: f32) -> u32 {
    if value.is_nan() {
        return 0;
    }
    let clamped = value.round().clamp(0.0, COORD_LIMIT_PX);
    // Guarded above: `clamped` is finite, non-negative and far below u32::MAX.
    clamped as u32
}

/// New cut coordinate after its handle moved by `delta` screen points.
///
/// `line_screen` is where the cut is drawn this frame; the handle is moved
/// RELATIVE to that, never to the pointer's absolute position. That is what
/// preserves the grab offset: reading `interact_pointer_pos` instead would snap
/// the line to the pointer on the first dragged frame, a jump of up to half the
/// handle's thickness — ~300 SOURCE pixels on an 18 000 px ribbon at fit zoom.
/// It also keeps the handle stateless: only the perpendicular coordinate is read
/// back, so the along-line movement still has no effect.
#[must_use]
fn dragged_cut_value(
    axis: SplitAxis,
    view: &ViewTransform,
    line_screen: egui::Pos2,
    delta: f32,
) -> u32 {
    let moved = match axis {
        SplitAxis::Horizontal => egui::pos2(line_screen.x, line_screen.y + delta),
        SplitAxis::Vertical => egui::pos2(line_screen.x + delta, line_screen.y),
    };
    round_world_to_px(axis_coord(axis, view.screen_to_world(moved)))
}

/// Widening conversion of a pixel count into the board's world/screen space.
/// Exact below 2^24 px, far above any page the engine accepts.
#[must_use]
fn u32_to_f32(value: u32) -> f32 {
    value as f32
}

/// Builds the engine request from the current state.
///
/// # Errors
/// Any [`SplitLayoutError`] raised by [`split_layout::validate`], plus
/// [`SplitLayoutError::PageTooSmall`] while the page size is still unknown
/// (extent `0`), which is also what keeps the confirm button disabled then.
fn build_split_op(state: &SplitDialogState) -> Result<PageOpKind, SplitLayoutError> {
    let extent = state.extent();
    split_layout::validate(extent, &state.cuts, &state.order)?;
    Ok(PageOpKind::Split {
        page_idx: state.page_idx,
        axis: state.axis,
        cuts: state.cuts.clone(),
        order: state.order.clone(),
    })
}

/// Maps a layout error to the localized message shown to the user.
///
/// The `SplitLayoutError` `Display` texts are technical (log/English); this is
/// the single place that turns them into UI strings. Cut indices are reported
/// 1-based, matching what the user counts on the board.
#[must_use]
fn layout_error_message(error: SplitLayoutError) -> String {
    match error {
        SplitLayoutError::PageTooSmall { extent } => tf!(
            "page_manager.split_dialog.page_too_small_error",
            extent = extent
        ),
        SplitLayoutError::NoCuts => {
            t!("page_manager.split_dialog.no_cuts_error").to_string()
        }
        SplitLayoutError::CutOutsidePage {
            index,
            value,
            extent,
        } => tf!(
            "page_manager.split_dialog.cut_outside_page_error",
            index = index + 1,
            value = value,
            extent = extent
        ),
        SplitLayoutError::CutsNotIncreasing { index } => tf!(
            "page_manager.split_dialog.cuts_not_increasing_error",
            index = index + 1
        ),
        SplitLayoutError::OrderNotPermutation { .. } => {
            t!("page_manager.split_dialog.order_invalid_error").to_string()
        }
    }
}

/// Localized caption of a cut orientation.
///
/// The `t!` macro only accepts a string literal, so the mapping is an exhaustive
/// match instead of a key table: adding a `SplitAxis` variant must not compile
/// until it has a caption.
#[must_use]
fn axis_label(axis: SplitAxis) -> &'static str {
    match axis {
        SplitAxis::Horizontal => t!("page_manager.split_dialog.axis_horizontal_radio"),
        SplitAxis::Vertical => t!("page_manager.split_dialog.axis_vertical_radio"),
    }
}

impl PageManagerTabState {
    /// Draws the "split page" window. Returns the state to keep, or `None` when
    /// the dialog closed this frame (confirmed, cancelled, or invalidated).
    ///
    /// `page_infos` supplies authoritative page geometry; a page missing from it
    /// falls back to the thumbnail/preview probe, and the board stays in its
    /// "loading" state until the size is known.
    pub(super) fn draw_split_dialog(
        &mut self,
        ctx: &egui::Context,
        mut state: SplitDialogState,
        project: &ProjectData,
        page_infos: &HashMap<usize, PageImageInfo>,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) -> Option<SplitDialogState> {
        // `clamp_selection` drops out-of-range indices every frame, so a reload
        // can invalidate the page under an open dialog. Re-validate here, not
        // only when the window opened.
        if state.page_idx >= project.pages.len() {
            self.error_message =
                Some(t!("page_manager.split_dialog.selection_lost_error").to_string());
            return None;
        }
        if state.page_size.is_none() {
            state.page_size = self.page_pixel_size(state.page_idx, project, page_infos);
        }
        // `page_pixel_size` answers from `page_infos` alone, so the page size can
        // be known while the preview decode has FAILED (the page file was replaced
        // or became unreadable after it was first loaded). The board then shows a
        // grey rectangle and the user would be cutting a page they cannot see —
        // with an operation that applies immediately and is not undone by
        // discarding unsaved changes. Peeked, not touched: the LRU order stays the
        // board's business.
        let preview_failed = matches!(
            self.thumbs
                .preview_state_cached(&project.pages[state.page_idx].path),
            PreviewState::Failed
        );

        let mut keep_open = true;
        let mut close_clicked = false;
        let mut confirm_clicked = false;
        egui::Window::new(t!("page_manager.split_dialog.title"))
            .id(egui::Id::new("page_manager_split_dialog"))
            .open(&mut keep_open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(1040.0, 720.0))
            .min_width(760.0)
            .min_height(520.0)
            .show(ctx, |ui| {
                egui::Panel::top("page_manager_split_settings").show(ui, |ui| {
                    draw_split_settings(ui, &mut state);
                });
                egui::Panel::bottom("page_manager_split_actions").show(ui, |ui| {
                    draw_split_actions(
                        ui,
                        &state,
                        op_in_progress,
                        preview_failed,
                        &mut confirm_clicked,
                        &mut close_clicked,
                    );
                });
                egui::CentralPanel::default().show(ui, |ui| {
                    self.draw_split_board(ui, &mut state, project);
                });
            });

        if confirm_clicked {
            match build_split_op(&state) {
                Ok(op) => {
                    actions.push(PageManagerAction::RequestOp(op));
                    return None;
                }
                Err(error) => {
                    // Confirm is disabled while the layout is invalid, so this can
                    // only be a race with the very frame it became invalid.
                    self.error_message = Some(layout_error_message(error));
                }
            }
        }
        if !keep_open || close_clicked {
            return None;
        }
        Some(state)
    }

    /// Draws the board: camera input, the page preview, the cut lines with their
    /// handles, and the per-part order pickers.
    fn draw_split_board(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut SplitDialogState,
        project: &ProjectData,
    ) {
        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::CornerRadius::ZERO, ui.visuals().extreme_bg_color);

        let Some(page_size) = state.page_size else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                t!("page_manager.split_dialog.loading_size"),
                egui::FontId::proportional(15.0),
                ui.visuals().weak_text_color(),
            );
            // The size arrives through the thumbnail worker; ask for it once per
            // frame until it does (the request is deduplicated by the runtime).
            self.thumbs
                .request_thumb_if_needed(&project.pages[state.page_idx].path, self.generation);
            return;
        };

        // Where the order pickers sit RIGHT NOW: the camera has not moved yet this
        // frame, so these are exactly the rects the user is pointing at, and the
        // wheel guard below can tell "over a picker" from "over the board".
        // (On the very first frame the camera is not fitted yet, but a fresh
        // dialog has no cut and therefore no picker, so the answer is empty
        // either way.)
        let picker_rects =
            split_order_widget_rects(state, rect, &state.viewport.transform(rect), page_size);
        self.handle_split_board_input(ui, state, rect, &response, page_size, &picker_rects);
        let view = state.viewport.transform(rect);
        self.paint_split_page(ui, state, project, rect, &view, page_size);
        draw_cut_lines(ui, state, rect, &view, page_size);
        draw_order_widgets(ui, state, rect, &view, page_size);
        board_context_menu(state, &response, &view, page_size);
    }

    /// Applies the first-frame fit, wheel zoom, and panning for this frame.
    ///
    /// `picker_rects` are this frame's order-picker rects (see
    /// [`split_order_widget_rects`]); the wheel over any of them belongs to the
    /// picker, never to the camera.
    fn handle_split_board_input(
        &mut self,
        ui: &mut egui::Ui,
        state: &mut SplitDialogState,
        rect: egui::Rect,
        response: &egui::Response,
        page_size: [u32; 2],
        picker_rects: &[Option<egui::Rect>],
    ) {
        // Fit the whole page on the first frame that has a real rect, so even an
        // 18 000 px ribbon opens fully visible.
        if !state.camera_fitted && rect.width() > 1.0 && rect.height() > 1.0 {
            // Infallible on every target this project builds for (32/64-bit
            // usize); a hypothetical 16-bit build would merely fit a clamped size.
            let width = usize::try_from(page_size[0]).unwrap_or(usize::MAX);
            let height = usize::try_from(page_size[1]).unwrap_or(usize::MAX);
            state.viewport.fit_page(rect, [width, height]);
            state.camera_fitted = true;
        }

        // Wheel: sign only (see WHEEL_ZOOM_STEP), anchored on the cursor.
        //
        // The board must not zoom while the wheel belongs to an order picker, and
        // that is true for a CLOSED picker too, not only an open popup: a
        // click-only widget over this `click_and_drag` board yields
        // `click: picker, drag: board`, and egui's `hovered` set is the UNION of
        // the two, so `response.hovered()` is true under a picker. The picker's
        // own wheel step consumes only `smooth_scroll_delta`, while
        // `raw_wheel_delta` reads `input.events` — so without this rect test one
        // notch would zoom the board AND silently swap two parts
        // (`egui-docs/03-input.md`, `egui-docs/04-widgets.md` §2).
        let over_picker = response.hover_pos().is_some_and(|pos| {
            picker_rects
                .iter()
                .flatten()
                .any(|picker| picker.contains(pos))
        });
        let wheel_y = if response.hovered() && !over_picker && !combo_popup_open(ui.ctx()) {
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

        // Any drag that reaches the board missed every cut handle — the handles
        // are interacted AFTER this rect, so within the layer they win the
        // pointer — which means a board drag is always a pan, whatever button it
        // uses. `drag_delta` is zero unless the board is actually dragged.
        state
            .viewport
            .handle_input(rect, anchor, 0.0, response.drag_delta());
    }

    /// Paints the page preview (or its placeholder) into the page's world rect.
    fn paint_split_page(
        &mut self,
        ui: &mut egui::Ui,
        state: &SplitDialogState,
        project: &ProjectData,
        rect: egui::Rect,
        view: &ViewTransform,
        page_size: [u32; 2],
    ) {
        let painter = ui.painter_at(rect);
        let visuals = ui.visuals().clone();
        let page_rect = view.world_rect_to_screen(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(u32_to_f32(page_size[0]), u32_to_f32(page_size[1])),
        ));

        let path = &project.pages[state.page_idx].path;
        self.thumbs
            .request_preview_if_needed(path, SPLIT_PREVIEW_LONG_SIDE_PX, self.generation);
        let preview = self.thumbs.preview_state(path);
        match preview {
            // A degenerate texture would sample garbage over the whole quad, so
            // it is treated as a failed decode rather than painted.
            PreviewState::Ready { texture, size, .. } if size.x > 0.0 && size.y > 0.0 => {
                painter.image(
                    texture,
                    page_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            PreviewState::Ready { .. } | PreviewState::Pending | PreviewState::Failed => {
                painter.rect_filled(
                    page_rect,
                    egui::CornerRadius::ZERO,
                    visuals.widgets.noninteractive.weak_bg_fill,
                );
                let caption = match preview {
                    PreviewState::Failed => t!("page_manager.split_dialog.preview_failed"),
                    PreviewState::Pending | PreviewState::Ready { .. } => {
                        t!("page_manager.split_dialog.preview_loading")
                    }
                };
                painter.text(
                    page_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    caption,
                    egui::FontId::proportional(13.0),
                    visuals.weak_text_color(),
                );
            }
        }
        painter.rect_stroke(
            page_rect,
            egui::CornerRadius::ZERO,
            egui::Stroke::new(1.0, visuals.widgets.inactive.fg_stroke.color),
            egui::StrokeKind::Inside,
        );
    }
}

/// Draws every cut line with its grab handle and delete button, and applies the
/// drag / deletion the user performed this frame.
///
/// The line spans the WHOLE board, not just the page image, so a line stays
/// visible while the page is panned off-centre. Deletions are collected and
/// applied after the loop: mutating `state.cuts` while iterating it is how the
/// launcher's equivalent code ended up with an index-shifting bug.
fn draw_cut_lines(
    ui: &mut egui::Ui,
    state: &mut SplitDialogState,
    board: egui::Rect,
    view: &ViewTransform,
    page_size: [u32; 2],
) {
    let axis = state.axis;
    let extent = axis_extent(axis, page_size);
    let painter = ui.painter_at(board);
    let line_color = egui::Color32::from_rgb(255, 79, 68);
    let handle_fill = egui::Color32::from_rgb(190, 28, 28);
    let grip_color = egui::Color32::from_rgb(250, 235, 235);

    let mut dragged: Option<(usize, u32)> = None;
    let mut to_delete: Vec<usize> = Vec::new();
    for (index, &cut) in state.cuts.iter().enumerate() {
        let world = match axis {
            SplitAxis::Horizontal => egui::pos2(0.0, u32_to_f32(cut)),
            SplitAxis::Vertical => egui::pos2(u32_to_f32(cut), 0.0),
        };
        let screen = view.world_to_screen(world);
        let line_screen = axis_coord(axis, screen);
        let (from, to) = match axis {
            SplitAxis::Horizontal => (
                egui::pos2(board.left(), line_screen),
                egui::pos2(board.right(), line_screen),
            ),
            SplitAxis::Vertical => (
                egui::pos2(line_screen, board.top()),
                egui::pos2(line_screen, board.bottom()),
            ),
        };
        painter.line_segment([from, to], egui::Stroke::new(1.5, line_color));

        let handle = handle_rect(axis, board, line_screen);
        let handle_response = ui
            .interact(
                handle,
                ui.id().with(("split_cut_handle", index)),
                egui::Sense::drag(),
            )
            .on_hover_cursor(match axis {
                SplitAxis::Horizontal => egui::CursorIcon::ResizeVertical,
                SplitAxis::Vertical => egui::CursorIcon::ResizeHorizontal,
            })
            .on_hover_text(t!("page_manager.split_dialog.handle_tooltip"));
        // Only the perpendicular coordinate is state: the pointer's position
        // ALONG the line is deliberately ignored, which is what makes the handle
        // snap back to the viewport centre.
        if handle_response.dragged() {
            let delta = match axis {
                SplitAxis::Horizontal => handle_response.drag_delta().y,
                SplitAxis::Vertical => handle_response.drag_delta().x,
            };
            dragged = Some((index, dragged_cut_value(axis, view, screen, delta)));
        }
        painter.rect_filled(handle, egui::CornerRadius::same(8), handle_fill);
        paint_handle_grip(&painter, axis, handle, grip_color);

        let delete = delete_rect(axis, handle);
        let delete_response = ui
            .interact(
                delete,
                ui.id().with(("split_cut_delete", index)),
                egui::Sense::click(),
            )
            .on_hover_text(t!("page_manager.split_dialog.delete_tooltip"));
        let radius = DELETE_BUTTON_POINTS * 0.5;
        painter.circle_filled(delete.center(), radius, egui::Color32::from_rgb(200, 30, 30));
        painter.circle_stroke(
            delete.center(),
            radius,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 150, 150)),
        );
        paint_cross(&painter, delete, grip_color);
        if delete_response.clicked() {
            to_delete.push(index);
        }
    }

    if let Some((index, value)) = dragged {
        // The immutable borrow of `cuts` ends with the call, so the write below
        // is free to take a mutable one.
        let clamped = split_layout::clamp_cut(extent, &state.cuts, index, value);
        if let Some(slot) = state.cuts.get_mut(index) {
            *slot = clamped;
        }
    }
    for index in to_delete.into_iter().rev() {
        split_layout::remove_cut(&mut state.cuts, &mut state.order, index);
    }
}

/// Paints the two grip lines that mark a handle as draggable.
fn paint_handle_grip(
    painter: &egui::Painter,
    axis: SplitAxis,
    handle: egui::Rect,
    color: egui::Color32,
) {
    let stroke = egui::Stroke::new(1.5, color);
    let center = handle.center();
    for offset in [-3.0_f32, 3.0] {
        let (from, to) = match axis {
            SplitAxis::Horizontal => (
                egui::pos2(center.x - 9.0, center.y + offset),
                egui::pos2(center.x + 9.0, center.y + offset),
            ),
            SplitAxis::Vertical => (
                egui::pos2(center.x + offset, center.y - 9.0),
                egui::pos2(center.x + offset, center.y + 9.0),
            ),
        };
        painter.line_segment([from, to], stroke);
    }
}

/// Paints the "x" of a delete button as two segments (no glyph, so it needs no
/// font coverage and no i18n exemption).
fn paint_cross(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.5, color);
    let inset = rect.shrink(rect.width() * 0.3);
    painter.line_segment([inset.left_top(), inset.right_bottom()], stroke);
    painter.line_segment([inset.right_top(), inset.left_bottom()], stroke);
}

/// Draws the order picker of every visible part, in its top-right corner, and
/// applies the swap the user picked.
fn draw_order_widgets(
    ui: &mut egui::Ui,
    state: &mut SplitDialogState,
    board: egui::Rect,
    view: &ViewTransform,
    page_size: [u32; 2],
) {
    let page_idx = state.page_idx;
    // One entry per geometric part; empty while the page is uncut, because one
    // part is nothing to order yet.
    let picker_rects = split_order_widget_rects(state, board, view, page_size);
    let count = picker_rects.len();
    if count < 2 {
        return;
    }
    let mut swap: Option<(usize, usize)> = None;
    for (part_index, picker_rect) in picker_rects.iter().enumerate() {
        let Some(rect) = *picker_rect else {
            continue;
        };
        let Some(mut selected) = state.order.get(part_index).copied() else {
            continue;
        };
        // `new_child` places the picker at an absolute rect WITHOUT advancing the
        // board's cursor (`Ui::place` does the same internally, but only accepts a
        // `Widget`, which `WheelComboBox::show_index` is not).
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        // Item `i` is "take page position i", whose resulting page number does
        // not depend on the current order — so the same caption serves both the
        // list and the selected text (which is item `order[part]`).
        let response = WheelComboBox::from_id_salt(("page_manager_split_order", part_index))
            .width(ORDER_WIDGET_WIDTH_POINTS)
            .show_index(&mut child, &mut selected, count, |i| {
                tf!(
                    "page_manager.split_dialog.order_item",
                    position = i + 1,
                    page = split_layout::page_number_for_position(page_idx, i)
                )
            });
        if response.changed() {
            swap = Some((part_index, selected));
        }
    }
    if let Some((part_index, position)) = swap {
        split_layout::swap_positions(&mut state.order, part_index, position);
    }
}

/// Right-click menu of the board: adds a cut line where the user clicked.
///
/// The world position is captured on `secondary_clicked` and stored, because by
/// the time the menu closure runs the pointer has already moved to the menu item
/// (the same order the launcher's ribbon menu uses).
fn board_context_menu(
    state: &mut SplitDialogState,
    response: &egui::Response,
    view: &ViewTransform,
    page_size: [u32; 2],
) {
    let axis = state.axis;
    let extent = axis_extent(axis, page_size);
    if response.secondary_clicked()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let world = view.screen_to_world(pointer);
        state.context_cut = Some(round_world_to_px(axis_coord(axis, world)));
    }
    let candidate = state.context_cut;
    let mut requested: Option<u32> = None;
    response.context_menu(|ui| {
        let enabled = candidate.is_some_and(|value| value > 0 && value < extent);
        if ui
            .add_enabled(
                enabled,
                egui::Button::new(t!("page_manager.split_dialog.add_cut_here_menu")),
            )
            .clicked()
        {
            requested = candidate;
            ui.close();
        }
    });
    if let Some(value) = requested {
        // A refused insert (the click landed on a page edge or on an existing
        // line) is a deliberate no-op: there is nothing to add and nothing the
        // user needs to be told.
        split_layout::insert_cut(extent, &mut state.cuts, &mut state.order, value);
    }
}

/// Draws the settings strip: cut orientation, the "add a line" button, and the
/// board hint.
fn draw_split_settings(ui: &mut egui::Ui, state: &mut SplitDialogState) {
    let extent = state.extent();
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("page_manager.split_dialog.axis_label"));
        let mut axis = state.axis;
        for candidate in [SplitAxis::Horizontal, SplitAxis::Vertical] {
            ui.radio_value(&mut axis, candidate, axis_label(candidate));
        }
        state.set_axis(axis);

        ui.separator();
        let suggestion = split_layout::suggest_cut(extent, &state.cuts);
        if ui
            .add_enabled(
                suggestion.is_some(),
                egui::Button::new(t!("page_manager.split_dialog.add_cut_button")),
            )
            .on_disabled_hover_text(t!("page_manager.split_dialog.add_cut_disabled_tooltip"))
            .clicked()
            && let Some(value) = suggestion
        {
            // `suggest_cut` only proposes a coordinate strictly inside a part of
            // at least 2 px, so this insert cannot be refused.
            split_layout::insert_cut(extent, &mut state.cuts, &mut state.order, value);
        }
    });
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(egui::RichText::new(t!("page_manager.split_dialog.board_hint")).weak())
            .selectable(false)
            .wrap(),
    );
    ui.add_space(4.0);
}

/// Draws the bottom strip: the resulting part count, the validation message, the
/// "applied immediately" warning, and the confirm / cancel buttons.
///
/// `preview_failed` disables the confirm and explains why: a split is immediate
/// and irreversible, so it is never offered over a page the board could not
/// render.
fn draw_split_actions(
    ui: &mut egui::Ui,
    state: &SplitDialogState,
    op_in_progress: bool,
    preview_failed: bool,
    confirm_clicked: &mut bool,
    close_clicked: &mut bool,
) {
    let validation = split_layout::validate(state.extent(), &state.cuts, &state.order);
    ui.add_space(6.0);
    // While the page size is still being probed the extent is 0, which validates
    // as "too small to cut" — a true statement about an unknown page and a
    // confusing one for the user, so the strip reports the wait instead.
    if state.page_size.is_none() {
        ui.label(t!("page_manager.split_dialog.loading_size"));
    } else {
        match validation {
            Ok(()) => {
                ui.label(tf!(
                    "page_manager.split_dialog.parts_label",
                    count = split_layout::part_count(&state.cuts)
                ));
            }
            Err(error) => {
                ui.colored_label(ui.visuals().warn_fg_color, layout_error_message(error));
            }
        }
    }
    if preview_failed {
        ui.add(
            egui::Label::new(
                egui::RichText::new(t!("page_manager.split_dialog.preview_failed_error"))
                    .color(ui.visuals().warn_fg_color),
            )
            .wrap(),
        );
    }
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(egui::RichText::new(t!(
            "page_manager.split_dialog.apply_warning"
        )))
        .wrap(),
    );
    ui.add(
        egui::Label::new(tf!(
            "page_manager.split_dialog.trash_note",
            dir = super::dialogs::PAGE_OP_TRASH_DIR
        ))
        .wrap(),
    );
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !op_in_progress && !preview_failed && validation.is_ok(),
                egui::Button::new(t!("page_manager.split_dialog.confirm_button")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_extent_picks_the_cut_axis() {
        assert_eq!(axis_extent(SplitAxis::Horizontal, [800, 6000]), 6000);
        assert_eq!(axis_extent(SplitAxis::Vertical, [800, 6000]), 800);
    }

    #[test]
    fn part_world_rect_spans_the_page_across_the_cut_axis() {
        let part = SplitPart {
            origin: 100,
            size: 250,
        };
        let horizontal = part_world_rect(SplitAxis::Horizontal, part, [800, 6000]);
        assert_eq!(horizontal.min, egui::pos2(0.0, 100.0));
        assert_eq!(horizontal.max, egui::pos2(800.0, 350.0));
        let vertical = part_world_rect(SplitAxis::Vertical, part, [800, 6000]);
        assert_eq!(vertical.min, egui::pos2(100.0, 0.0));
        assert_eq!(vertical.max, egui::pos2(350.0, 6000.0));
    }

    /// The window's own default board: `default_size(1040, 720)` minus the top
    /// and bottom strips.
    fn default_board() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1024.0, 540.0))
    }

    fn picker_size() -> egui::Vec2 {
        egui::vec2(ORDER_WIDGET_WIDTH_POINTS, ORDER_WIDGET_HEIGHT_POINTS)
    }

    /// Screen rects of `cuts.len() + 1` parts exactly as the board paints them at
    /// the first-frame fit zoom.
    fn fitted_part_rects(
        axis: SplitAxis,
        board: egui::Rect,
        page_size: [u32; 2],
        cuts: &[u32],
    ) -> Vec<egui::Rect> {
        let mut viewport = PsViewport::default();
        viewport.fit_page(
            board,
            [
                usize::try_from(page_size[0]).unwrap_or(usize::MAX),
                usize::try_from(page_size[1]).unwrap_or(usize::MAX),
            ],
        );
        let view = viewport.transform(board);
        split_layout::parts(axis_extent(axis, page_size), cuts)
            .into_iter()
            .map(|part| view.world_rect_to_screen(part_world_rect(axis, part, page_size)))
            .collect()
    }

    /// The invariant C2 exists for: every placed picker stays inside the board
    /// and strictly advances past its predecessor along the cut axis, so the one
    /// drawn later can never cover it completely.
    fn assert_every_picker_is_reachable(
        rects: &[Option<egui::Rect>],
        board: egui::Rect,
        axis: SplitAxis,
    ) {
        let placed: Vec<egui::Rect> = rects.iter().flatten().copied().collect();
        for rect in &placed {
            assert!(board.contains_rect(*rect), "{rect:?} escapes the board");
            // Float-tolerant: the rect is built from a clamped corner, so its
            // size can differ from the constant by an ulp.
            let drift = (rect.size() - picker_size()).abs();
            assert!(
                drift.x < 0.01 && drift.y < 0.01,
                "the picker lost its fixed size: {rect:?}"
            );
        }
        for pair in placed.windows(2) {
            let [before, after] = pair else {
                continue;
            };
            let advance = match axis {
                SplitAxis::Horizontal => after.top() - before.top(),
                SplitAxis::Vertical => after.left() - before.left(),
            };
            assert!(
                advance > 0.0,
                "picker {after:?} does not advance past {before:?}"
            );
        }
    }

    /// C2: the app's primary content. An 18 000 px ribbon is 23 pt wide at the
    /// window's default fit zoom — far narrower than the picker — and every part
    /// must still get one, or the parts cannot be reordered at all.
    #[test]
    fn every_part_of_a_ribbon_gets_a_picker_at_fit_zoom() {
        let board = default_board();
        for page_size in [[800_u32, 8_000_u32], [800, 18_000]] {
            let parts =
                fitted_part_rects(SplitAxis::Horizontal, board, page_size, &[1_000, 2_000]);
            let rects = order_widget_rects(SplitAxis::Horizontal, &parts, board, picker_size());
            assert_eq!(rects.len(), 3);
            assert!(
                rects.iter().all(Option::is_some),
                "a visible part was left without a picker at {page_size:?}"
            );
            assert_every_picker_is_reachable(&rects, board, SplitAxis::Horizontal);
        }
    }

    /// The same for VERTICAL cuts, where the picker is wider than its own part:
    /// it overhangs the page instead of vanishing, and the sequence stays inside
    /// the board.
    #[test]
    fn vertical_parts_narrower_than_the_picker_still_get_one() {
        let board = default_board();
        let parts = fitted_part_rects(SplitAxis::Vertical, board, [800, 18_000], &[200, 500]);
        let rects = order_widget_rects(SplitAxis::Vertical, &parts, board, picker_size());
        assert_eq!(rects.len(), 3);
        assert!(rects.iter().all(Option::is_some));
        assert_every_picker_is_reachable(&rects, board, SplitAxis::Vertical);
    }

    /// A part reduced to a sliver by the zoom keeps its picker, and two slivers
    /// never end up on top of each other.
    #[test]
    fn slivers_keep_their_pickers_and_do_not_collide() {
        let board = default_board();
        let parts = [
            egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(200.0, 3.0)),
            egui::Rect::from_min_size(egui::pos2(10.0, 13.0), egui::vec2(200.0, 3.0)),
        ];
        let rects = order_widget_rects(SplitAxis::Horizontal, &parts, board, picker_size());
        assert!(rects.iter().all(Option::is_some));
        assert_every_picker_is_reachable(&rects, board, SplitAxis::Horizontal);
    }

    /// More pickers than the board can hold at full pitch: they compress instead
    /// of piling up, and every one keeps a leading strip of its own.
    #[test]
    fn many_parts_compress_instead_of_stacking() {
        let board = default_board();
        let parts: Vec<egui::Rect> = (0..40)
            .map(|index| {
                let top = 20.0 + u32_to_f32(index) * 2.0;
                egui::Rect::from_min_size(egui::pos2(10.0, top), egui::vec2(300.0, 2.0))
            })
            .collect();
        let rects = order_widget_rects(SplitAxis::Horizontal, &parts, board, picker_size());
        assert_eq!(rects.iter().flatten().count(), 40);
        assert_every_picker_is_reachable(&rects, board, SplitAxis::Horizontal);
    }

    #[test]
    fn a_part_entirely_off_the_board_has_no_picker() {
        let board = default_board();
        let parts = [
            egui::Rect::from_min_size(egui::pos2(1_500.0, 900.0), egui::vec2(200.0, 200.0)),
            egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(200.0, 200.0)),
        ];
        let rects = order_widget_rects(SplitAxis::Horizontal, &parts, board, picker_size());
        assert_eq!(rects.first().copied().flatten(), None);
        assert!(rects.get(1).copied().flatten().is_some());
    }

    /// C3: the handle must move the line by the pointer's DELTA. Reading the
    /// absolute pointer position instead discarded the grab offset, which on a
    /// ribbon at fit zoom is hundreds of source pixels.
    #[test]
    fn dragging_a_handle_applies_the_delta_and_never_jumps() {
        let board = default_board();
        let mut viewport = PsViewport::default();
        viewport.fit_page(board, [800, 18_000]);
        let view = viewport.transform(board);
        let cut = 9_000_u32;
        let line = view.world_to_screen(egui::pos2(0.0, u32_to_f32(cut)));
        // The frame the drag starts on carries no movement yet: wherever inside
        // the handle the user grabbed, the line must stay put.
        assert_eq!(
            dragged_cut_value(SplitAxis::Horizontal, &view, line, 0.0),
            cut
        );
        // Half the handle's thickness — the size of the old jump — applied as a
        // delta is ~300 source pixels at this zoom.
        let moved = dragged_cut_value(SplitAxis::Horizontal, &view, line, 9.0);
        let expected = cut + round_world_to_px(9.0 / view.zoom);
        assert!(
            moved.abs_diff(expected) <= 1,
            "expected about {expected} px, got {moved} px"
        );
        assert!(moved > cut + 250, "a 9 pt drag must be ~309 source px");
    }

    #[test]
    fn handle_sits_on_the_viewport_centre_along_its_line() {
        let board = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 300.0));
        let horizontal = handle_rect(SplitAxis::Horizontal, board, 120.0);
        assert_eq!(horizontal.center(), egui::pos2(200.0, 120.0));
        assert_eq!(horizontal.width(), HANDLE_LENGTH_POINTS);
        let vertical = handle_rect(SplitAxis::Vertical, board, 120.0);
        assert_eq!(vertical.center(), egui::pos2(120.0, 150.0));
        assert_eq!(vertical.height(), HANDLE_LENGTH_POINTS);
    }

    #[test]
    fn delete_button_follows_the_end_of_the_handle() {
        let board = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 300.0));
        let handle = handle_rect(SplitAxis::Horizontal, board, 120.0);
        let delete = delete_rect(SplitAxis::Horizontal, handle);
        assert!(delete.left() > handle.right());
        assert_eq!(delete.center().y, handle.center().y);
        let vertical_handle = handle_rect(SplitAxis::Vertical, board, 120.0);
        let vertical_delete = delete_rect(SplitAxis::Vertical, vertical_handle);
        assert!(vertical_delete.top() > vertical_handle.bottom());
        assert_eq!(vertical_delete.center().x, vertical_handle.center().x);
    }

    #[test]
    fn round_world_to_px_is_total() {
        assert_eq!(round_world_to_px(12.4), 12);
        assert_eq!(round_world_to_px(12.6), 13);
        assert_eq!(round_world_to_px(-40.0), 0);
        assert_eq!(round_world_to_px(f32::NAN), 0);
        assert_eq!(round_world_to_px(f32::INFINITY), 100_000_000);
        assert_eq!(round_world_to_px(f32::NEG_INFINITY), 0);
    }

    #[test]
    fn switching_the_axis_drops_the_cuts() {
        let mut state = SplitDialogState::new(3);
        state.page_size = Some([800, 6000]);
        state.cuts = vec![1000, 2000];
        state.order = vec![2, 0, 1];
        // A redundant set keeps the user's work.
        state.set_axis(SplitAxis::Horizontal);
        assert_eq!(state.cuts, vec![1000, 2000]);
        // A real switch drops it: the coordinates belong to the other axis.
        state.set_axis(SplitAxis::Vertical);
        assert!(state.cuts.is_empty());
        assert_eq!(state.order, vec![0]);
        assert_eq!(state.extent(), 800);
    }

    #[test]
    fn build_split_op_emits_the_engine_request() {
        let mut state = SplitDialogState::new(4);
        state.page_size = Some([800, 6000]);
        state.cuts = vec![2000, 4000];
        state.order = vec![2, 1, 0];
        match build_split_op(&state) {
            Ok(PageOpKind::Split {
                page_idx,
                axis,
                cuts,
                order,
            }) => {
                assert_eq!(page_idx, 4);
                assert_eq!(axis, SplitAxis::Horizontal);
                assert_eq!(cuts, vec![2000, 4000]);
                assert_eq!(order, vec![2, 1, 0]);
            }
            other => panic!("expected a Split op, got {other:?}"),
        }
    }

    #[test]
    fn build_split_op_refuses_an_uncut_page() {
        let mut state = SplitDialogState::new(0);
        state.page_size = Some([800, 6000]);
        assert_eq!(build_split_op(&state), Err(SplitLayoutError::NoCuts));
        // Unknown page size: extent 0, which is refused as "too small".
        let unknown = SplitDialogState::new(0);
        assert_eq!(
            build_split_op(&unknown),
            Err(SplitLayoutError::PageTooSmall { extent: 0 })
        );
    }
}
