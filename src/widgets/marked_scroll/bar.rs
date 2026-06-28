/*
File: src/widgets/marked_scroll/bar.rs

Purpose:
Vertical scrollbar rendering and handle dragging, ported from egui's internal
`ScrollArea` bar block so the widget can inject marks between the track
background and the handle.

PORT SOURCE: egui 0.33.3, `src/containers/scroll_area.rs`, the per-axis bar
block of `ScrollArea::show_viewport_dyn` (roughly lines 1200-1443). That code is
private, so the geometry (`calculate_handle_rect`), handle drag math, floating
opacity/width logic, and track/handle painting are reproduced here for the
vertical axis only. egui is MIT/Apache-2.0; keep this in sync when bumping egui.

Differences from the upstream block:
- vertical axis only; no horizontal branch, no stick-to-end / momentum (the host
  `ScrollArea` engine still owns wheel, drag-to-scroll, momentum and clipping);
- the handle drag anchor is kept in `ctx` temp data instead of the engine's
  private `State`; only the resulting offset is written back by the caller;
- marks are painted between the track background and the handle.

Key items:
- BarConfig / BarInputs: configuration and per-frame geometry inputs.
- BarResult: the dragged offset (if any) and the resolved `BarGeometry`.
- run_vertical_bar(): interaction + layered painting (track -> marks -> handle)
  plus gutter items.
*/

use super::gutter::{GutterItem, paint_gutter_items};
use super::marks::{BarGeometry, ScrollMark, paint_mark};
use egui::{Id, Rangef, Rect, Sense, Ui, lerp, pos2, remap_clamp};

/// Behavioral configuration of the bar for one frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BarConfig {
    /// Thin bar that expands on hover (egui floating bars). When false the bar
    /// is always full width and fully opaque.
    pub floating: bool,
    /// When true, typed-fill marks fade together with the floating track
    /// background; when false they stay fully opaque regardless of the bar.
    pub marks_follow_bar_opacity: bool,
}

/// Per-frame geometry inputs taken from the host `ScrollArea` output.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BarInputs {
    /// Stable id of the host scroll area (used to namespace interaction state).
    pub base_id: Id,
    /// Full-width bar column (width = `bar_width`) over the viewport Y span.
    pub track_rect: Rect,
    /// Optional gutter column to the left of the track.
    pub gutter_rect: Option<Rect>,
    /// Total scrollable content length along the axis.
    pub content_size_y: f32,
    /// Visible viewport length along the axis.
    pub viewport_y: f32,
    /// Current scroll offset along the axis.
    pub offset_y: f32,
}

/// Output of one bar frame: the new offset if the user dragged, and the geometry
/// used for marks/gutter (so callers can reuse the same projection).
#[derive(Clone, Copy, Debug)]
pub(crate) struct BarResult {
    pub new_offset_y: Option<f32>,
    pub geometry: BarGeometry,
}

/// Runs interaction and layered painting for the vertical bar.
///
/// Paints, in order, the track background, all `marks` (ascending layer, under
/// the handle), the handle, then `gutter_items` left of the track. Returns the
/// dragged offset (clamped to `[0, max_offset]`) when the handle was moved this
/// frame; the caller is responsible for writing it back into the engine state.
pub(crate) fn run_vertical_bar(
    ui: &Ui,
    cfg: &BarConfig,
    inputs: &BarInputs,
    marks: Vec<ScrollMark>,
    gutter_items: Vec<GutterItem>,
) -> BarResult {
    let scroll_style = ui.spacing().scroll;
    let full = inputs.track_rect;
    let max_offset = (inputs.content_size_y - inputs.viewport_y).max(0.0);
    let scrollable = max_offset > 0.0;

    // Floating bars expand from `floating_width` to `bar_width` on hover.
    let is_hovering = ui.rect_contains_pointer(full);
    let width = if cfg.floating {
        let hover_t = ui
            .ctx()
            .animate_bool_responsive(inputs.base_id.with("mscroll_bar_hover"), is_hovering);
        lerp(
            scroll_style.floating_width..=scroll_style.bar_width,
            hover_t,
        )
    } else {
        scroll_style.bar_width
    };
    let cross = Rangef::new(full.right() - width, full.right());
    let visible_bar =
        Rect::from_min_max(pos2(cross.min, full.top()), pos2(cross.max, full.bottom()));

    // Handle geometry (egui `calculate_handle_rect`, vertical axis).
    let track_len = full.height();
    let handle_size = if inputs.content_size_y > 0.0 {
        (inputs.viewport_y / inputs.content_size_y * track_len)
            .clamp(scroll_style.handle_min_length, track_len)
    } else {
        track_len
    };
    let travel_top = full.top();
    let travel_bottom = full.bottom() - handle_size;
    let handle_top_for = |offset: f32| -> f32 {
        if scrollable {
            remap_clamp(offset, 0.0..=max_offset, travel_top..=travel_bottom)
        } else {
            travel_top
        }
    };
    let handle_rect_for = |offset: f32| -> Rect {
        let top = handle_top_for(offset);
        Rect::from_min_max(pos2(cross.min, top), pos2(cross.max, top + handle_size))
    };

    let handle_rect = handle_rect_for(inputs.offset_y);

    // Interaction: drag the handle (or click the track to jump). The drag anchor
    // (pointer offset within the handle) is kept in ctx temp data, mirroring the
    // engine's private `scroll_start_offset_from_top_left`.
    let sense = if scrollable {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let response = ui.interact(visible_bar, inputs.base_id.with("mscroll_bar"), sense);
    let anchor_id = inputs.base_id.with("mscroll_drag_anchor");
    let mut new_offset_y = None;
    if let Some(pointer) = response.interact_pointer_pos() {
        if scrollable {
            let anchor = ui.ctx().data_mut(|data| {
                if let Some(stored) = data.get_temp::<f32>(anchor_id) {
                    stored
                } else {
                    let anchor = if handle_rect.contains(pointer) {
                        pointer.y - handle_rect.top()
                    } else {
                        // Center the handle on the pointer when clicking the track.
                        let new_top =
                            (pointer.y - handle_size / 2.0).clamp(travel_top, travel_bottom);
                        pointer.y - new_top
                    };
                    data.insert_temp(anchor_id, anchor);
                    anchor
                }
            });
            let new_top = pointer.y - anchor;
            let offset = if (travel_bottom - travel_top).abs() < f32::EPSILON {
                0.0
            } else {
                remap_clamp(new_top, travel_top..=travel_bottom, 0.0..=max_offset)
            };
            new_offset_y = Some(offset.clamp(0.0, max_offset));
        }
    } else {
        ui.ctx().data_mut(|data| data.remove::<f32>(anchor_id));
    }

    let resolved_offset = new_offset_y.unwrap_or(inputs.offset_y);
    let handle_rect = handle_rect_for(resolved_offset);

    // Handle color from interaction state (egui widget visuals).
    let hovering_handle = response.hovered()
        && ui.input(|input| {
            input
                .pointer
                .latest_pos()
                .is_some_and(|p| handle_rect.contains(p))
        });
    let pointer_down = response.is_pointer_button_down_on();
    let (handle_color, corner_radius) = {
        let widgets = &ui.visuals().widgets;
        let visuals = if pointer_down {
            &widgets.active
        } else if hovering_handle {
            &widgets.hovered
        } else {
            &widgets.inactive
        };
        let color = if scroll_style.foreground_color {
            visuals.fg_stroke.color
        } else {
            visuals.bg_fill
        };
        (color, visuals.corner_radius)
    };
    let extreme_bg = ui.visuals().extreme_bg_color;

    // Opacities (egui floating bars). Non-floating bars are fully opaque.
    let (handle_opacity, background_opacity) = if cfg.floating {
        let interacting = response.hovered() || response.dragged();
        let handle_opacity = if interacting {
            scroll_style.interact_handle_opacity
        } else {
            let outer_t = ui
                .ctx()
                .animate_bool_responsive(inputs.base_id.with("mscroll_outer_hover"), is_hovering);
            lerp(
                scroll_style.dormant_handle_opacity..=scroll_style.active_handle_opacity,
                outer_t,
            )
        };
        let background_opacity = if interacting {
            scroll_style.interact_background_opacity
        } else if is_hovering {
            scroll_style.active_background_opacity
        } else {
            scroll_style.dormant_background_opacity
        };
        (handle_opacity, background_opacity)
    } else {
        (1.0, 1.0)
    };

    let geometry = BarGeometry {
        track_rect: visible_bar,
        content_size: inputs.content_size_y,
        viewport: inputs.viewport_y,
        offset: resolved_offset,
    };

    // Layered painting: track background -> marks (under handle) -> handle.
    let painter = ui.painter();
    painter.rect_filled(
        visible_bar,
        corner_radius,
        extreme_bg.gamma_multiply(background_opacity),
    );

    let mut marks = marks;
    marks.sort_by_key(|mark| mark.layer);
    let mark_opacity = if cfg.marks_follow_bar_opacity {
        background_opacity
    } else {
        1.0
    };
    for mark in marks {
        paint_mark(painter, mark, &geometry, mark_opacity);
    }

    painter.rect_filled(
        handle_rect,
        corner_radius,
        handle_color.gamma_multiply(handle_opacity),
    );

    // Build-time gutter items, projected onto the same geometry, left of the track.
    if let Some(gutter_rect) = inputs.gutter_rect {
        paint_gutter_items(painter, gutter_rect, &geometry, gutter_items);
    }

    BarResult {
        new_offset_y,
        geometry,
    }
}
