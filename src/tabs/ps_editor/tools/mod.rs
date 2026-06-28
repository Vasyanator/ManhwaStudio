/*
File: tabs/ps_editor/tools/mod.rs

Purpose:
Tool subsystem for the PS-like editor. Defines the `PsTool` trait, the per-frame interaction
context, and the dirty-region/outcome types. The editor owns a `Vec<Box<dyn PsTool>>` so new
tools can be added without touching the tab orchestration.

Key structures:
- `PsToolId`: stable identity for the active-tool selector and hotkeys.
- `PsToolContext`: mutable per-frame access to the layer stack, selection, and pointer state.
- `ToolOutcome`: what changed this frame (dirty image rect, selection change, repaint request).
- `PsTool`: trait every tool implements (interaction, overlay drawing, options UI).

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
}

/// Contract every editor tool implements.
///
/// `interact` runs once per frame with pointer/button state already resolved. `draw_overlay`
/// renders cursor/preview decorations in screen space. `options_ui` draws the tool's own option
/// controls in the tool panel.
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

    /// Downcast hook for the brush tool so the tab can forward wheel/size gestures.
    ///
    /// Default returns `None`; only `brush::BrushTool` overrides it. This keeps brush-specific
    /// input handling out of the generic tool dispatch without a full `Any` downcast.
    fn as_brush_mut(&mut self) -> Option<&mut brush::BrushTool> {
        None
    }
}
