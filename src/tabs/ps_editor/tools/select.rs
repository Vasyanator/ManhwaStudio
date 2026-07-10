/*
File: tabs/ps_editor/tools/select.rs

Purpose:
Selection tools for the PS-like editor: rectangular marquee and freehand lasso. Both build the
page `Selection` mask used to clip the brush. The same struct backs both tools, parameterized by
`SelectMode`.

Key structures:
- `SelectMode`: `Rect` or `Lasso`.
- `SelectTool`: in-progress drag/lasso state and the resulting commit on release.

Notes:
A click with no meaningful drag (tiny rectangle / fewer than three lasso points) clears the
selection, matching the "click to deselect" behavior of a marquee tool.
*/

use super::{PsTool, PsToolContext, PsToolId, ToolOutcome};
use crate::tabs::ps_editor::viewport::ViewTransform;
use eframe::egui;
use egui::{Color32, Pos2, Stroke};

/// Which selection shape a `SelectTool` instance builds.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SelectMode {
    Rect,
    Lasso,
}

/// Rectangular-marquee or freehand-lasso selection tool.
#[derive(Debug, Clone)]
pub struct SelectTool {
    mode: SelectMode,
    /// Rect anchor in image pixels (start of the drag).
    drag_start: Option<Pos2>,
    /// Sampled lasso vertices in image pixels.
    lasso_points: Vec<(f32, f32)>,
    /// Latest pointer position in image pixels (for preview + release commit).
    last_pointer: Option<Pos2>,
    dragging: bool,
}

impl SelectTool {
    #[must_use]
    pub fn new(mode: SelectMode) -> Self {
        Self {
            mode,
            drag_start: None,
            lasso_points: Vec::new(),
            last_pointer: None,
            dragging: false,
        }
    }

    fn reset_drag(&mut self) {
        self.drag_start = None;
        self.lasso_points.clear();
        self.dragging = false;
    }

    fn commit_rect(&self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        use crate::trace::cat;
        let mut outcome = ToolOutcome::default();
        if let (Some(start), Some(end)) = (self.drag_start, self.last_pointer) {
            ctx.ensure_selection().set_rect(
                start.x.round() as i32,
                start.y.round() as i32,
                end.x.round() as i32,
                end.y.round() as i32,
            );
            outcome.selection_changed = true;
            crate::trace_log!(
                cat::PS_EDITOR,
                "selection commit_rect from=({:.1},{:.1}) to=({:.1},{:.1}) any={}",
                start.x,
                start.y,
                end.x,
                end.y,
                ctx.selection.as_ref().is_some_and(|s| s.any())
            );
        }
        outcome
    }

    fn commit_lasso(&self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        use crate::trace::cat;
        let mut outcome = ToolOutcome::default();
        let points = self.lasso_points.len();
        ctx.ensure_selection().set_polygon(&self.lasso_points);
        outcome.selection_changed = true;
        crate::trace_log!(
            cat::PS_EDITOR,
            "selection commit_lasso points={} any={}",
            points,
            ctx.selection.as_ref().is_some_and(|s| s.any())
        );
        outcome
    }
}

impl PsTool for SelectTool {
    fn id(&self) -> PsToolId {
        match self.mode {
            SelectMode::Rect => PsToolId::SelectRect,
            SelectMode::Lasso => PsToolId::SelectLasso,
        }
    }

    fn title(&self) -> &'static str {
        match self.mode {
            SelectMode::Rect => t!("ps_editor.tools.rect_select_title"),
            SelectMode::Lasso => t!("ps_editor.tools.lasso_title"),
        }
    }

    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        if let Some(pointer) = ctx.pointer_image {
            self.last_pointer = Some(pointer);
        }

        // Begin a drag only when the press lands inside the viewport.
        if ctx.primary_pressed
            && ctx.pointer_in_viewport
            && let Some(pointer) = ctx.pointer_image
        {
            self.dragging = true;
            crate::trace_log!(
                crate::trace::cat::INPUT,
                "selection drag_begin mode={:?} at=({:.1},{:.1})",
                self.mode,
                pointer.x,
                pointer.y
            );
            match self.mode {
                SelectMode::Rect => self.drag_start = Some(pointer),
                SelectMode::Lasso => {
                    self.lasso_points.clear();
                    self.lasso_points.push((pointer.x, pointer.y));
                }
            }
        }

        if self.dragging && ctx.primary_down {
            if let (SelectMode::Lasso, Some(pointer)) = (self.mode, ctx.pointer_image) {
                // Sample sparsely to keep the polygon small.
                let far_enough = self
                    .lasso_points
                    .last()
                    .map(|&(lx, ly)| (lx - pointer.x).hypot(ly - pointer.y) >= 2.0)
                    .unwrap_or(true);
                if far_enough {
                    self.lasso_points.push((pointer.x, pointer.y));
                }
            }
            return ToolOutcome::default();
        }

        if self.dragging && ctx.primary_released {
            let outcome = match self.mode {
                SelectMode::Rect => self.commit_rect(ctx),
                SelectMode::Lasso => self.commit_lasso(ctx),
            };
            self.reset_drag();
            return outcome;
        }

        // Drag interrupted (button lost focus): drop in-progress state.
        if self.dragging && !ctx.primary_down {
            self.reset_drag();
        }
        ToolOutcome::default()
    }

    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        view: &ViewTransform,
        _pointer_image: Option<Pos2>,
    ) {
        if !self.dragging {
            return;
        }
        // Thin black-and-white dashed preview, matching the committed marquee (never blue).
        match self.mode {
            SelectMode::Rect => {
                if let (Some(start), Some(end)) = (self.drag_start, self.last_pointer) {
                    let rect = egui::Rect::from_two_pos(
                        view.world_to_screen(start),
                        view.world_to_screen(end),
                    );
                    let corners = [
                        rect.left_top(),
                        rect.right_top(),
                        rect.right_bottom(),
                        rect.left_bottom(),
                        rect.left_top(),
                    ];
                    draw_dashed_preview(painter, &corners);
                }
            }
            SelectMode::Lasso => {
                if self.lasso_points.len() >= 2 {
                    let pts: Vec<Pos2> = self
                        .lasso_points
                        .iter()
                        .map(|&(x, y)| view.world_to_screen(Pos2::new(x, y)))
                        .collect();
                    draw_dashed_preview(painter, &pts);
                }
            }
        }
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        match self.mode {
            SelectMode::Rect => {
                ui.label(t!("ps_editor.tools.rect_select_hint"));
            }
            SelectMode::Lasso => {
                ui.label(t!("ps_editor.tools.lasso_hint"));
            }
        }
        ui.label(t!("ps_editor.tools.select_clear_hint"));
    }
}

/// Draws a thin black-and-white dashed path (offset white over black) for the in-progress marquee.
fn draw_dashed_preview(painter: &egui::Painter, path: &[Pos2]) {
    let dash = 5.0;
    let gap = 5.0;
    let mut shapes = Vec::new();
    for segment in path.windows(2) {
        egui::Shape::dashed_line_many(
            segment,
            Stroke::new(1.0, Color32::BLACK),
            dash,
            gap,
            &mut shapes,
        );
        egui::Shape::dashed_line_many_with_offset(
            segment,
            Stroke::new(1.0, Color32::WHITE),
            &[dash],
            &[gap],
            dash,
            &mut shapes,
        );
    }
    painter.extend(shapes);
}
