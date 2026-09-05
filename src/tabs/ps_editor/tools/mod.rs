/*
File: tabs/ps_editor/tools/mod.rs

Purpose:
Tool subsystem for the PS-like editor. Defines the `PsTool` trait, the per-frame interaction
context, and the dirty-region/outcome types. The editor owns a `Vec<Box<dyn PsTool>>` so new
tools can be added without touching the tab orchestration.

Key structures:
- `PsToolId`: stable identity for the active-tool selector and hotkeys.
- `PsToolSection`: toolbar grouping (brushes / selection / manipulation) for the tool selector.
- `PsToolContext`: mutable per-frame access to the layer stack, selection, pointer state, and the
  frame's keyboard modifiers / gesture-control keys (Esc, Backspace).
- `ToolOutcome`: what changed this frame (dirty image rect, selection change, repaint request).
- `PsTool`: trait every tool implements (interaction, overlay drawing, options UI).

Key functions:
- `PsToolContext::ensure_selection`: lazily allocates the page-sized selection mask.
- `PsToolContext::normalize_selection`: drops an all-zero mask so "nothing selected" is `None`.
- `normalize_selection_slot`: the pure core of the above, testable without a `PsToolContext`.
- `PsTool::reset` / `PsTool::freeze`: the two lifecycle hooks the tab uses to keep a multi-frame
  gesture from outliving its context (tool/page switch) or from dying on a suppressed frame.

Notes:
Tools never touch GPU textures, files, or shared models. They mutate the in-memory layer stack and
selection only; the tab translates `ToolOutcome::dirty` into tile re-uploads.
*/

pub mod brush;
pub mod deform;
pub mod select;
pub mod transform;

use super::layers::LayerStack;
use super::selection::Selection;
use super::viewport::ViewTransform;
use eframe::egui;
use egui::Pos2;

/// Stable identifier for each tool, used by the toolbar and hotkeys.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PsToolId {
    SelectRect,
    SelectLasso,
    Brush,
    Transform,
    Deform,
}

/// Toolbar grouping for the tool selector.
///
/// The toolbar renders one heading per section, in [`PsToolSection::ORDER`], and lists the tools
/// whose [`PsToolId::section`] matches. Adding a tool therefore only requires assigning it a
/// section; the toolbar layout needs no edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsToolSection {
    Brushes,
    Selection,
    Manipulation,
}

impl PsToolSection {
    /// Sections in toolbar order, top to bottom.
    pub const ORDER: [PsToolSection; 3] = [
        PsToolSection::Brushes,
        PsToolSection::Selection,
        PsToolSection::Manipulation,
    ];

    /// Localized section heading for the toolbar.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            PsToolSection::Brushes => t!("ps_editor.toolbar.section_brushes"),
            PsToolSection::Selection => t!("ps_editor.toolbar.section_selection"),
            PsToolSection::Manipulation => t!("ps_editor.toolbar.section_manipulation"),
        }
    }
}

impl PsToolId {
    /// Which toolbar section this tool belongs to.
    #[must_use]
    pub fn section(self) -> PsToolSection {
        match self {
            PsToolId::Brush => PsToolSection::Brushes,
            PsToolId::SelectRect | PsToolId::SelectLasso => PsToolSection::Selection,
            PsToolId::Transform | PsToolId::Deform => PsToolSection::Manipulation,
        }
    }
}

/// Inclusive dirty rectangle in image pixel coordinates.
///
/// Tools report the region they modified so the tab can re-upload only the affected tiles.
#[derive(Debug, Clone, Copy)]
pub struct DirtyRect {
    pub min_x: usize,
    pub min_y: usize,
    pub max_x: usize,
    pub max_y: usize,
}

/// Per-frame result of a tool interaction.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolOutcome {
    /// Image-space region whose pixels changed (active layer), if any.
    pub dirty: Option<DirtyRect>,
    /// Set when the selection mask changed and its overlay must be refreshed.
    pub selection_changed: bool,
}

/// Mutable per-frame context handed to the active tool.
pub struct PsToolContext<'a> {
    pub page_size: [usize; 2],
    /// Pointer position in image pixel coordinates (fractional), or `None` when unavailable.
    pub pointer_image: Option<Pos2>,
    /// True when the pointer is inside the viewport rect this frame.
    pub pointer_in_viewport: bool,
    pub primary_pressed: bool,
    pub primary_down: bool,
    pub primary_released: bool,
    /// Keyboard modifiers as of this frame. Selection tools sample them at press to pick the
    /// combination mode, and read `alt` live to switch into straight-segment mode.
    pub modifiers: egui::Modifiers,
    /// Escape was pressed this frame: cancel any in-progress gesture without touching the selection.
    pub cancel_pressed: bool,
    /// Backspace or Delete was pressed this frame: drop the last placed point of an in-progress path.
    pub remove_point_pressed: bool,
    /// The frame's image↔screen transform, for tools that hit-test screen-space handles.
    pub view: ViewTransform,
    pub stack: &'a mut LayerStack,
    /// The page selection. Tools may create it on demand via `ensure_selection`.
    pub selection: &'a mut Option<Selection>,
}

impl PsToolContext<'_> {
    /// Returns a mutable selection, creating an empty page-sized one if absent.
    pub fn ensure_selection(&mut self) -> &mut Selection {
        if self.selection.is_none() {
            *self.selection = Some(Selection::empty(self.page_size[0], self.page_size[1]));
        }
        self.selection
            .as_mut()
            .expect("selection was just created above")
    }

    /// Drops an all-zero mask so "nothing selected" is `None`, never `Some(empty)`.
    ///
    /// `ensure_selection` allocates a page-sized mask eagerly, so a gesture that selects no pixels
    /// would otherwise leave a phantom selection: the marquee hides itself (`!any()`) while the
    /// brush, which only tests `Option::is_some`, silently refuses to paint. Every commit path
    /// must call this.
    pub fn normalize_selection(&mut self) {
        normalize_selection_slot(self.selection);
    }
}

/// Enforces the "a selection is `Some` only when it selects at least one pixel" invariant on a
/// selection slot: an all-zero mask becomes `None`, a non-empty one is left untouched.
///
/// Split out of [`PsToolContext::normalize_selection`] so the rule can be exercised without a
/// `LayerStack` or a `ViewTransform`; the tab enforces the same invariant through
/// `PsEditorTabState::set_selection`.
pub fn normalize_selection_slot(selection: &mut Option<Selection>) {
    if selection.as_ref().is_some_and(|s| !s.any()) {
        *selection = None;
    }
}

/// Contract every editor tool implements.
///
/// `interact` runs once per frame with pointer/button state already resolved. `draw_overlay`
/// renders cursor/preview decorations in screen space. `options_ui` draws the tool's own option
/// controls in the tool panel.
///
/// A tool that keeps state ACROSS frames (an outline, a drag anchor, a stroke position) must also
/// honor the two lifecycle hooks below: `reset` when its gesture's context is gone, `freeze` on a
/// frame the tab did not route to it.
pub trait PsTool {
    fn id(&self) -> PsToolId;
    fn title(&self) -> &'static str;
    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome;
    fn draw_overlay(
        &self,
        painter: &egui::Painter,
        view: &ViewTransform,
        pointer_image: Option<Pos2>,
    );
    fn options_ui(&mut self, ui: &mut egui::Ui);

    /// Abandons any in-progress gesture; the tool must be safe to use again immediately.
    ///
    /// Called by the tab whenever the gesture's context is gone: the tool is switched away from
    /// (`PsEditorTabState::set_active_tool`), the page is switched (`request_page`), or Esc is
    /// pressed on a frame whose input is suppressed. Without it a gesture that ends with the
    /// button UP (a pending polygon) survives both transitions and commits later, with a
    /// combination mode sampled minutes ago and — across a page switch — the wrong page's
    /// coordinates. Must not touch the page selection: an abandoned gesture leaves the committed
    /// selection exactly as it was.
    ///
    /// The default is empty, for tools whose state is rebuilt from scratch each frame.
    fn reset(&mut self) {}

    /// Called once per frame whose canvas input the tab did NOT route to this tool (a canvas pan
    /// or a text-layer drag is in progress).
    ///
    /// The tool must FREEZE, not abort: a suppressed frame hides the button events, so the next
    /// routed frame can see "button up, no release event" and mistake a swallowed release for an
    /// interrupted drag. A tool that keeps a multi-frame outline records the suppression here and
    /// treats that missing release as a real release when input resumes.
    ///
    /// The default is empty, so a tool whose gesture cannot be damaged by a suppressed frame keeps
    /// its current behavior while the canvas pans.
    fn freeze(&mut self) {}

    /// Downcast hook for the brush tool so the tab can forward wheel/size gestures.
    ///
    /// Default returns `None`; only `brush::BrushTool` overrides it. This keeps brush-specific
    /// input handling out of the generic tool dispatch without a full `Any` downcast.
    fn as_brush_mut(&mut self) -> Option<&mut brush::BrushTool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page-sized mask with a single pixel set, so `any()` is true.
    fn one_pixel(w: usize, h: usize) -> Selection {
        let mut sel = Selection::empty(w, h);
        sel.set_rect(0, 0, 1, 1);
        assert!(sel.any(), "the fixture must select at least one pixel");
        sel
    }

    #[test]
    fn normalize_drops_an_all_zero_mask() {
        let mut slot = Some(Selection::empty(8, 8));
        normalize_selection_slot(&mut slot);
        assert!(slot.is_none(), "an all-zero mask must never survive as Some");
    }

    #[test]
    fn normalize_keeps_a_non_empty_mask() {
        let mut slot = Some(one_pixel(8, 8));
        normalize_selection_slot(&mut slot);
        assert!(slot.is_some_and(|s| s.any()), "a real selection must be left alone");
    }

    #[test]
    fn normalize_leaves_none_alone() {
        let mut slot: Option<Selection> = None;
        normalize_selection_slot(&mut slot);
        assert!(slot.is_none());
    }
}
