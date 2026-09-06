/*
File: tabs/ps_editor/tools/mod.rs

Purpose:
Tool subsystem for the PS-like editor. Defines the `PsTool` trait, the per-frame interaction
context, and the dirty-region/outcome types. The editor owns a `Vec<Box<dyn PsTool>>` so new
tools can be added without touching the tab orchestration.

Key structures:
- `PsToolId`: stable identity for the active-tool selector and hotkeys.
- `PsToolSection`: toolbar grouping (brushes / selection / manipulation) for the tool selector.
- `PsToolContext`: mutable per-frame access to the layer stack, selection, pointer state (both mouse
  buttons plus the frame's pointer delta), and the frame's keyboard modifiers / gesture-control keys
  (Esc, Backspace).
- `ToolOutcome`: what changed this frame (dirty rect in the ACTIVE LAYER's own pixels, selection
  change).
- `PsHotkeyRow`: one already-localized (action, keys) row of the tab's shortcut panel.
- `PsTool`: trait every tool implements (interaction, overlay drawing, options UI, shortcut list).

Key functions:
- `PsToolContext::ensure_selection`: lazily allocates the page-sized selection mask.
- `PsToolContext::normalize_selection`: drops an all-zero mask so "nothing selected" is `None`.
- `normalize_selection_slot`: the pure core of the above, testable without a `PsToolContext`.
- `PsTool::reset` / `PsTool::freeze`: the two lifecycle hooks the tab uses to keep a multi-frame
  gesture from outliving its context (tool/page switch) or from dying on a suppressed frame.
- `PsTool::gesture_in_flight`: whether an ACCEPTED gesture is still running, which is what lets the
  tab keep drawing a preview the user dragged over a floating panel.
- `PsTool::hotkey_rows`: the tool's own shortcut inventory for the «Горячие клавиши» panel.

Notes:
Tools never touch GPU textures, files, or shared models. They mutate the in-memory layer stack and
selection only; the tab translates `ToolOutcome::dirty` into tile re-uploads.

Hint TEXT belongs in `hotkey_rows`, never in `options_ui`: the shortcut panel renders it as a
two-column grid, while `options_ui` is reserved for controls that actually change a parameter.
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

/// Inclusive dirty rectangle in the ACTIVE LAYER's own pixel coordinates.
///
/// Tools report the region they modified so the tab can re-upload only the affected tiles. The
/// space is layer-local, not page-local: a `TiledTexture` is sized to its layer's image and the
/// undo diff is cut from that same buffer, so a transformed raster's rect must NOT be mapped into
/// page pixels on the way here.
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
    /// Region of the ACTIVE LAYER's own pixel grid whose pixels changed, if any.
    pub dirty: Option<DirtyRect>,
    /// Set when the selection mask changed and its overlay must be refreshed.
    pub selection_changed: bool,
}

/// One row of the «Горячие клавиши» panel: what the shortcut does, and which keys trigger it.
///
/// Both fields are ALREADY LOCALIZED (`t!`/`tf!`) by the tool that built the row — the panel only
/// lays them out, so a tool that needs a language-dependent key name stays the single owner of it.
#[derive(Debug, Clone)]
pub struct PsHotkeyRow {
    /// Localized description of what the shortcut does.
    pub action: String,
    /// Localized key combination, e.g. `"Ctrl+D"` or `"Shift + колесо"`.
    pub keys: String,
}

impl PsHotkeyRow {
    /// Builds a row from an already-localized action description and key combination.
    ///
    /// Both halves must be non-empty: the panel renders a two-column grid, so an empty half leaves
    /// a blank cell, and that only ever happens through an authoring or translation bug (the unit
    /// tests in this module assert it).
    #[must_use]
    pub fn new(action: impl Into<String>, keys: impl Into<String>) -> Self {
        Self { action: action.into(), keys: keys.into() }
    }
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
    /// Secondary (right) mouse button held this frame.
    ///
    /// Present for the brush's Alt + right-drag size/hardness HUD. It is deliberately the raw
    /// button state and not a `Response::dragged()`: this canvas senses `click_and_drag`, so
    /// `dragged()` is delayed by the click/drag ambiguity and `drag_delta()` stays zero until it
    /// resolves — the same reason the tab's own pan gate reads `middle_down`.
    pub secondary_down: bool,
    /// Pointer movement this frame in SCREEN px (`PointerState::delta`), for gestures measured in
    /// drag distance rather than in canvas position.
    pub pointer_delta: egui::Vec2,
    /// Keyboard modifiers as of this frame. Selection tools sample them at press to pick the
    /// combination mode, and read `alt` live to switch into straight-segment mode; the brush reads
    /// `shift` for its axis constraint and `alt` to stand aside for the tab's eyedropper.
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
/// renders cursor/preview decorations in screen space. `has_options` + `options_ui` are the
/// «Выбранный инструмент» panel's pair: the first is REQUIRED and decides whether the second is
/// called at all, so a parameterless tool implements only the first.
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
    /// Whether this tool has any parameter the «Выбранный инструмент» panel can show.
    ///
    /// REQUIRED, with no default body, for the same reason as [`PsTool::hotkey_rows`]: only the
    /// tool knows whether it owns a parameter, so a new tool must be forced by the compiler to
    /// answer. It is the panel's ONLY question — a `false` makes it print
    /// `ps_editor.active_tool.no_options` and never call `options_ui`, so a tool answering `false`
    /// need not implement `options_ui` at all.
    ///
    /// Answer `true` only for a control that CHANGES a parameter. A label describing a key or a
    /// drag is a [`PsHotkeyRow`], not an option (see this module's `MODULE_README.md`).
    fn has_options(&self) -> bool;

    /// Draws the tool's own parameter controls in the «Выбранный инструмент» panel.
    ///
    /// Called only while [`PsTool::has_options`] answers `true`, which is why the default is a
    /// no-op: a parameterless tool (the transform and deform gizmos) states that once in
    /// `has_options` instead of carrying an empty implementation here.
    fn options_ui(&mut self, _ui: &mut egui::Ui) {}

    /// The tool's OWN shortcuts and gestures, already localized, for the «Горячие клавиши» panel.
    ///
    /// REQUIRED on purpose — there is deliberately no default body. A tool is the only place that
    /// knows which keys and mouse gestures it interprets, so a new tool must be forced by the
    /// compiler to state them; a defaulted empty list would silently ship an undocumented tool.
    /// Returning an empty `Vec` is the explicit way to say "this tool has no shortcuts".
    ///
    /// Rebuilt on every call so a runtime language switch is reflected. Tab-level shortcuts (tool
    /// letters, Ctrl+D, undo/redo, pan, zoom) belong to the tab, NOT here: a tool must not list a
    /// key it does not itself handle.
    fn hotkey_rows(&self) -> Vec<PsHotkeyRow>;

    /// Whether the tool currently holds an unfinished gesture whose START was already accepted.
    ///
    /// REQUIRED, with no default body, for the same reason as [`PsTool::hotkey_rows`] and
    /// [`PsTool::has_options`]: only the tool knows what "a gesture" means for it, so a new tool
    /// must be forced by the compiler to answer. `true` from the frame the tool accepted the start
    /// until the frame the gesture commits or is abandoned; `false` on an idle tool. A gesture that
    /// legitimately continues with the button UP (the lasso's pending polygon) is still in flight.
    ///
    /// The tab asks this to decide whether [`PsTool::draw_overlay`] may keep following a pointer
    /// that has moved over a floating dock panel (`../mod.rs`, `overlay_pointer`): a preview may
    /// not APPEAR under a panel, but one dragged there by a running gesture must stay visible.
    /// It must therefore NOT report `true` for a press the tool itself refused — that press holds
    /// the button down just like an accepted one, and reporting it would put the preview back under
    /// the panel in exactly the case this method exists to fix.
    fn gesture_in_flight(&self) -> bool;

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
    /// A gesture that already changed PIXELS cannot be undone by this hook — it receives no layer
    /// access — so the tab commits such a gesture before abandoning it (`BrushTool`'s
    /// `end_stroke_for_commit`, run by `PsEditorTabState::commit_brush_stroke_before_abandon`).
    /// A tool whose gesture only builds an overlay needs nothing of the sort.
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

    /// The tools the tab registers in `PsEditorTabState::default` (`../mod.rs`), in the same order.
    /// Kept as a fixture so `hotkey_rows` is exercised for every SHIPPED tool, not just the ones a
    /// test happened to name; a new tool must be added here as well as to the tab.
    fn registered_tools() -> Vec<Box<dyn PsTool>> {
        vec![
            Box::new(select::SelectTool::new(select::SelectMode::Rect)),
            Box::new(select::SelectTool::new(select::SelectMode::Lasso)),
            Box::new(brush::BrushTool::default()),
            Box::new(transform::TransformTool::default()),
            Box::new(deform::DeformTool::default()),
        ]
    }

    /// Every shipped tool must produce well-formed shortcut rows: the panel lays them out as a
    /// two-column grid, so a blank half is an authoring bug (a forgotten argument) or a translation
    /// bug (an empty catalog value) and must not reach the UI.
    ///
    /// The reference catalog is installed first on purpose: `t!` falls back to the KEY text on a
    /// miss, which is never empty, so without a real catalog an empty translation would slip past.
    #[test]
    fn every_tool_reports_well_formed_hotkey_rows() {
        let _guard = crate::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

        for tool in registered_tools() {
            let id = tool.id();
            for row in tool.hotkey_rows() {
                assert!(!row.action.trim().is_empty(), "{id:?}: a hotkey row has an empty action");
                assert!(
                    !row.keys.trim().is_empty(),
                    "{id:?}: hotkey row {:?} has empty keys",
                    row.action
                );
            }
        }
    }

    /// The lasso understands three keys the rectangular marquee does not (Alt straight segments,
    /// the Alt-release anchor, and Backspace/Delete), so its row list must be the longer one. This
    /// pins the mode-dependence that `SelectTool::hotkey_rows` exists to express.
    #[test]
    fn the_lasso_documents_more_shortcuts_than_the_rect_marquee() {
        let rect = select::SelectTool::new(select::SelectMode::Rect).hotkey_rows().len();
        let lasso = select::SelectTool::new(select::SelectMode::Lasso).hotkey_rows().len();
        assert_eq!(lasso, rect + 3, "the lasso adds exactly its three extra key rows");
    }
}
