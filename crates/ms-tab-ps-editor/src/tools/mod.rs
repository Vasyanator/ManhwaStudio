/*
File: tabs/ps_editor/tools/mod.rs

Purpose:
Tool subsystem for the PS-like editor. Defines the `PsTool` trait, the per-frame interaction
context, and the dirty-region/outcome types. The editor owns a `Vec<Box<dyn PsTool>>` so new
tools can be added without touching the tab orchestration.

Key structures:
- `PsToolId`: stable identity for the active-tool selector and hotkeys.
- `PsToolSection`: toolbar grouping (brushes / selection / manipulation) for the tool selector.
- `PsToolContext`: mutable per-frame access to the layer stack, selection, the page index, pointer
  state (both mouse buttons plus the frame's pointer delta), and the frame's keyboard modifiers /
  gesture-control keys (Esc, Backspace).
- `ToolOutcome`: what changed this frame (dirty rect in the ACTIVE LAYER's own pixels, selection
  change).
- `PsHotkeyRow`: one already-localized (action, keys) row of the tab's shortcut panel.
- `PsToolAction` / `ToolRegionWrite`: the tool → tab out-channel, drained once per frame; the one
  capability today is "write this pixel rect into the active editable layer as ONE undo step".
- `PsToolOverlayCx`: the page/view geometry an on-canvas `draw_overlay_ui` pass is handed.
- `PsTool`: trait every tool implements (interaction, overlay drawing, options UI, shortcut list,
  and the defaulted region-tool hooks below).

Key functions:
- `PsToolContext::ensure_selection`: lazily allocates the page-sized selection mask.
- `PsToolContext::normalize_selection`: drops an all-zero mask so "nothing selected" is `None`.
- `normalize_selection_slot`: the pure core of the above, testable without a `PsToolContext`.
- `PsTool::reset` / `PsTool::freeze`: the two lifecycle hooks the tab uses to keep a multi-frame
  gesture from outliving its context (tool/page switch) or from dying on a suppressed frame.
- `PsTool::gesture_in_flight`: whether an ACCEPTED gesture is still running, which is what lets the
  tab keep drawing a preview the user dragged over a floating panel.
- `PsTool::hotkey_rows`: the tool's own shortcut inventory for the «Горячие клавиши» panel.
- `PsTool::poll_workers` / `PsTool::take_actions`: the two hooks a REGION tool needs — one for its
  own worker channels, one for the pixel writes it asks the tab to perform.
- `PsTool::set_panel_rects` / `wants_main_panel` / `draw_main_panel` / `draw_overlay_ui` /
  `captures_canvas_pointer`: the on-canvas surface hooks, all defaulted to inert.

Notes:
Tools never touch GPU textures, files, or shared models. They mutate the in-memory layer stack and
selection only; the tab translates `ToolOutcome::dirty` into tile re-uploads. A pixel commit that
must survive (an undo entry, a doc write, a `Клин` write-back) is REQUESTED through
[`PsToolAction`] and performed by the tab — a tool never records history and never touches the
shared models itself.

Hint TEXT belongs in `hotkey_rows`, never in `options_ui`: the shortcut panel renders it as a
two-column grid, while `options_ui` is reserved for controls that actually change a parameter.
*/

pub mod brush;
pub mod deform;
pub mod patch;
pub mod select;
pub mod transform;

use super::layers::LayerStack;
use super::selection::Selection;
use super::viewport::ViewTransform;
use eframe::egui;
use egui::{ColorImage, Pos2, Rect};

/// Stable identifier for each tool, used by the toolbar and hotkeys.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PsToolId {
    SelectRect,
    SelectLasso,
    Brush,
    Patch,
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
            // The patch tool is a RETOUCHING tool: it paints pixels into the active layer, so it
            // belongs beside the brush rather than in the selection section, even though its
            // gesture starts by drawing a selection of its own.
            PsToolId::Brush | PsToolId::Patch => PsToolSection::Brushes,
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

/// One pixel rect a tool asks the tab to write into the ACTIVE EDITABLE layer as a single
/// undoable step.
///
/// A tool may not perform this itself: an undo entry, the push to the shared `LayerDoc` and the
/// `Клин` write-back to `CleanOverlaysModel` are all tab-side (`../mod.rs`), and a tool sees
/// neither. It therefore describes the write and hands it over through [`PsToolAction`].
///
/// Geometry is in the ACTIVE LAYER's own pixel grid — the same space as [`DirtyRect`] and as the
/// undo diff — not in page pixels. `origin` is the rect's top-left corner; the rect's size is
/// `pixels.size`. A rect that PARTLY leaves the layer is clipped by the tab — a region tool works
/// in page pixels and its ROI legitimately reaches the page edge — while one that leaves it
/// entirely is refused and logged.
///
/// `pixels` are PREMULTIPLIED RGBA, the convention of `Layer::image` throughout this editor.
#[derive(Debug, Clone)]
pub struct ToolRegionWrite {
    /// Page the write belongs to. The tab drops the request when this is not the resident page —
    /// a worker result that outlived a page switch must not land on the new page's pixels.
    pub page_idx: usize,
    /// Top-left corner of the rect in the ACTIVE LAYER's own pixels.
    pub origin: [usize; 2],
    /// Premultiplied RGBA source pixels; its `size` is the rect's size.
    pub pixels: ColorImage,
    /// Per-pixel blend weight in `0..=255`, row-major and exactly `size[0] * size[1]` long.
    ///
    /// `None` means "fully opaque": the source replaces the destination. `Some` blends
    /// `dst = dst + (src - dst) * coverage / 255` per premultiplied channel, which is what a
    /// feathered region edit needs and what makes a coverage of 255 identical to `None`. A
    /// wrongly-sized buffer is REFUSED and logged by the tab, never silently padded.
    pub coverage: Option<Vec<u8>>,
    /// ALREADY LOCALIZED undo label, as `PsHotkeyRow`'s fields are. It reaches the user through
    /// `ActionHistory::peek_undo_label`, so the tool owns the wording.
    pub label: String,
}

/// A deferred request from a tool to its host tab, drained once per frame.
///
/// The tab's own layers-panel `PanelActions` is the same pattern one level up: a body that may not
/// perform an expensive or contract-bearing mutation in place parks it and lets the owner apply it
/// at a point where the borrows and the ordering are right.
///
/// One variant on purpose (§14: no speculative future-proofing). Add a variant only together with
/// the tool that raises it and the tab arm that performs it.
#[derive(Debug, Clone)]
pub enum PsToolAction {
    /// Commit a pixel rect into the active editable layer as ONE undo step.
    ///
    /// Raised today by `patch::PatchTool`, which solves its membrane off-thread and can therefore
    /// reach neither the undo stack nor the shared models from where the answer arrives. Add a
    /// second variant only together with the tool that raises it and the tab arm that performs it
    /// (`CLAUDE.md` §14).
    WriteRegion(ToolRegionWrite),
}

/// Page and view geometry handed to [`PsTool::draw_overlay_ui`] every frame.
///
/// It carries exactly what an on-canvas surface needs to place itself and to convert between the
/// two spaces it lives in, and nothing else:
/// * `viewport` — the canvas rect in SCREEN points. The canvas fills the whole program-tab area
///   and the dock panels float over it, so this is the outer bound an `egui::Area` must stay
///   inside; the panels themselves are cut out with the rects from [`PsTool::set_panel_rects`].
/// * `view` — this frame's image↔screen transform. On-canvas geometry is stored in PAGE pixels and
///   re-projected every frame; a stored screen rect drifts the moment the user pans or zooms.
/// * `page_size` / `page_idx` — which page the geometry belongs to and how far it extends, so a
///   surface can clamp itself to the page and can refuse to act after a page switch.
///
/// The layer stack is deliberately ABSENT: a pass that runs outside `interact` must not mutate
/// pixels behind the tab's back. Pixel work is requested through [`PsToolAction`], and a host that
/// needs the stack to BUILD that work parks the request until its next `interact`
/// (`patch::PatchTool`).
#[derive(Debug, Clone, Copy)]
pub struct PsToolOverlayCx {
    /// The canvas rect in screen points.
    pub viewport: Rect,
    /// This frame's image↔screen transform.
    pub view: ViewTransform,
    /// The resident page's size in page pixels.
    pub page_size: [usize; 2],
    /// The resident page's index.
    pub page_idx: usize,
}

/// Mutable per-frame context handed to the active tool.
pub struct PsToolContext<'a> {
    /// Index of the page being edited, i.e. `LayerStack::page_idx` of `stack`.
    ///
    /// A tool that hands work to a worker or queues a [`PsToolAction`] must stamp it with this:
    /// the result arrives frames later, possibly after a page switch, and only the page it was
    /// computed for may receive it. Tools that finish inside one frame ignore it.
    ///
    /// `dead_code` is allowed for the same reason as on [`PsToolAction::WriteRegion`]: the tab
    /// fills it on every frame and the unit tests read it, but none of the four shipped tools
    /// spans more than one frame's worth of work. Remove the attribute with the first tool that
    /// does.
    #[allow(dead_code)]
    pub page_idx: usize,
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
///
/// A REGION tool — one that owns a worker thread, an on-canvas surface and a dock panel — needs
/// more than one frame of input can express, and the block of DEFAULTED hooks at the end of the
/// trait is what it uses: `poll_workers` (its own channels), `take_actions` (the pixel commits it
/// asks the tab to perform), `set_panel_rects` / `wants_main_panel` / `draw_main_panel` /
/// `draw_overlay_ui` (its surface and its panel) and `captures_canvas_pointer` (standing the tab's
/// canvas input aside). All of them are inert by default, so the four shipped tools implement none.
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

    // ------------------------------------------------------------------------------------------
    // REGION-TOOL HOOKS
    //
    // Everything below is DEFAULTED to inert, so a tool that draws only into the canvas painter
    // (the four shipped ones) implements none of it and behaves exactly as before. A tool that
    // owns a worker thread, an on-canvas surface or a dock panel overrides what it needs.
    // ------------------------------------------------------------------------------------------

    /// Consumes finished results from the tool's OWN worker channels; called once per frame for
    /// EVERY tool, active or not, from `PsEditorTabState::poll_tools` (`../mod.rs`).
    ///
    /// The GUI thread must never block (`CLAUDE.md` §5), so a tool that computes anything
    /// expensive owns a `Sender`/`Receiver` pair and drains it here with `try_recv` — never inside
    /// `interact`, which runs under the canvas input gate and is skipped on a pan frame.
    ///
    /// It runs for INACTIVE tools on purpose: a job dispatched before a tool switch still finishes,
    /// and its result must be consumed rather than stranded in the channel. Pixel work it produces
    /// is queued through [`PsTool::take_actions`], not applied here.
    ///
    /// Returns `true` when the tool wants the next frame — a job is still in flight, or the poll
    /// changed something the user must see. The tab turns that into `Context::request_repaint`,
    /// which is what makes an off-thread result appear without any pointer movement.
    fn poll_workers(&mut self) -> bool {
        false
    }

    /// Hands the tab everything the tool wants COMMITTED this frame, leaving the tool's queue
    /// empty.
    ///
    /// This is the tool's only write path to pixels that must survive: an undo entry, the push to
    /// the shared `LayerDoc` and the `Клин` write-back to `CleanOverlaysModel` are tab-side, and a
    /// tool sees none of them. Queue an action from wherever the decision is made — `interact`,
    /// [`PsTool::poll_workers`] or [`PsTool::draw_overlay_ui`] — and the tab performs it at one
    /// fixed point in the frame.
    ///
    /// Drained from EVERY tool, so an action queued just before a tool switch is still performed.
    /// The default returns an empty `Vec`.
    fn take_actions(&mut self) -> Vec<PsToolAction> {
        Vec::new()
    }

    /// Rects of the dock panels drawn over the canvas THIS frame, in screen points.
    ///
    /// Pushed by the tab right after `PanelDock::end` and before the canvas is drawn, so a tool
    /// that PLACES something on the canvas can cut the floating panels out of the viewport and
    /// never park its surface underneath one. Tools that place nothing ignore it.
    ///
    /// Main-window rects only, by construction: a panel the user detached into a sub-window is
    /// never reported (`PanelDockOutput::drawn_panels`), so it can never blank out a region of
    /// this window.
    fn set_panel_rects(&mut self, _rects: &[egui::Rect]) {}

    /// Whether this tool wants its own dock panel shown right now.
    ///
    /// The tab declares that seventh tab on EVERY frame and drives only its `visible` flag from
    /// this answer, so a tool returning `false` costs one declaration and nothing else. The panel
    /// is captioned with [`PsTool::title`], because it belongs to the tool rather than to the tab.
    fn wants_main_panel(&self) -> bool {
        false
    }

    /// Body of the tool's own dock panel, drawn only while [`PsTool::wants_main_panel`] is `true`.
    ///
    /// Same rule as [`PsTool::options_ui`]: it may mutate the TOOL and nothing else. It runs inside
    /// the dock frame, before the canvas exists for this frame, so a button here raises a flag the
    /// tool acts on in its next [`PsTool::draw_overlay_ui`] or `interact`.
    ///
    /// This is for a panel a REGION tool needs for its own controls and status. Ordinary
    /// parameters still belong in `options_ui` and shortcuts still belong in `hotkey_rows`.
    fn draw_main_panel(&mut self, _ui: &mut egui::Ui) {}

    /// The tool's on-canvas pass, run once per frame for the ACTIVE tool only.
    ///
    /// Unlike [`PsTool::draw_overlay`] — a painter-only decoration on the canvas' own background
    /// layer — this hook owns an `egui::Context` and may therefore open its own `egui::Area`, sense
    /// pointer input in it, and draw through `Context::layer_painter`. It is where a region tool's
    /// controls, handles and dashed outlines live.
    ///
    /// It runs UNGATED by the canvas input gate: the tool's own `Area` occludes what it covers by
    /// z-order, and the tab asks [`PsTool::captures_canvas_pointer`] to stand its own canvas input
    /// aside. It runs only while a page is resident, because [`PsToolOverlayCx`] is page geometry;
    /// a worker result therefore must be consumed in [`PsTool::poll_workers`], which has no such
    /// condition and runs for inactive tools too.
    ///
    /// The layer stack is not reachable from here. Pixel work is queued through
    /// [`PsTool::take_actions`].
    fn draw_overlay_ui(&mut self, _ctx: &egui::Context, _cx: PsToolOverlayCx) {}

    /// Whether the tool's own on-canvas surface claims the pointer at `pointer_pos` (screen points).
    ///
    /// The tab ORs this into its canvas occlusion gate (`canvas_pointer_occluded`, `../mod.rs`), so
    /// a `true` withholds the wheel, the zoom anchor and the routing to `interact` — and hides the
    /// tool's own cursor preview, which is the same gate. Answer `true` only over the surface's
    /// real hit area, re-projected from PAGE pixels this frame; a stale screen rect drifts as soon
    /// as the user pans.
    ///
    /// It must stay as NARROW as possible, and it must never be widened into a modal block. The
    /// cleaning tab paid for that lesson: its equivalent whole-canvas flag (`block_canvas_zoom`)
    /// also disables the undo shortcuts, which is tolerable for a modal window and not for a
    /// surface that lives on the canvas for a whole session. This hook cannot do that here — the
    /// PS editor's undo/redo runs in `handle_hotkeys`, which never consults the canvas gate — and
    /// it must not grow a sibling that can.
    fn captures_canvas_pointer(&self, _pointer_pos: Pos2) -> bool {
        false
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
            Box::new(patch::PatchTool::default()),
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
        let _guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
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

    // ------------------------------------------------------------------------------------------
    // The REGION-TOOL hooks: defaulted to inert, and overridable.
    // ------------------------------------------------------------------------------------------

    /// A tool that implements nothing beyond the REQUIRED methods, to exercise the defaults.
    #[derive(Default)]
    struct InertTool;

    impl PsTool for InertTool {
        fn id(&self) -> PsToolId {
            PsToolId::Brush
        }
        fn title(&self) -> &'static str {
            "inert"
        }
        fn interact(&mut self, _ctx: &mut PsToolContext<'_>) -> ToolOutcome {
            ToolOutcome::default()
        }
        fn draw_overlay(
            &self,
            _painter: &egui::Painter,
            _view: &ViewTransform,
            _pointer_image: Option<Pos2>,
        ) {
        }
        fn has_options(&self) -> bool {
            false
        }
        fn hotkey_rows(&self) -> Vec<PsHotkeyRow> {
            Vec::new()
        }
        fn gesture_in_flight(&self) -> bool {
            false
        }
    }

    /// A tool that DOES own a panel, a worker and an out-channel, to prove each hook is reachable
    /// through `dyn PsTool` and really overrides the default.
    #[derive(Default)]
    struct RegionStubTool {
        panel_rects: Vec<Rect>,
        queued: Vec<PsToolAction>,
    }

    impl PsTool for RegionStubTool {
        fn id(&self) -> PsToolId {
            PsToolId::Brush
        }
        fn title(&self) -> &'static str {
            "region stub"
        }
        fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
            // The one thing `interact` is used for here: proving the page index reaches a tool, by
            // stamping the queued request with it exactly as a real region tool must.
            self.queued
                .push(PsToolAction::WriteRegion(ToolRegionWrite {
                    page_idx: ctx.page_idx,
                    origin: [0, 0],
                    pixels: egui::ColorImage::new([1, 1], vec![egui::Color32::RED]),
                    coverage: None,
                    label: "stub".to_string(),
                }));
            ToolOutcome::default()
        }
        fn draw_overlay(
            &self,
            _painter: &egui::Painter,
            _view: &ViewTransform,
            _pointer_image: Option<Pos2>,
        ) {
        }
        fn has_options(&self) -> bool {
            false
        }
        fn hotkey_rows(&self) -> Vec<PsHotkeyRow> {
            Vec::new()
        }
        fn gesture_in_flight(&self) -> bool {
            false
        }
        fn poll_workers(&mut self) -> bool {
            true
        }
        fn take_actions(&mut self) -> Vec<PsToolAction> {
            std::mem::take(&mut self.queued)
        }
        fn set_panel_rects(&mut self, rects: &[Rect]) {
            self.panel_rects.clear();
            self.panel_rects.extend_from_slice(rects);
        }
        fn wants_main_panel(&self) -> bool {
            true
        }
        fn captures_canvas_pointer(&self, pointer_pos: Pos2) -> bool {
            self.panel_rects.iter().any(|r| r.contains(pointer_pos))
        }
    }

    /// Every SHIPPED tool must be inert on the region hooks WHILE IDLE: adding them may not change
    /// the behaviour of the brush, the two marquees, the transform or the deform gizmo. A tool that
    /// silently started claiming the pointer or a dock panel would withhold canvas input and open a
    /// seventh panel for no reason.
    ///
    /// The patch tool is included rather than excused: it is a region tool, but it owns no dock
    /// panel, claims no pointer, and asks for a frame only while a job is actually in flight — so a
    /// freshly built one answers exactly as the others do. Its own file tests the busy answers.
    #[test]
    fn the_shipped_tools_leave_every_region_hook_inert() {
        let probe = Pos2::new(10.0, 10.0);
        for mut tool in registered_tools() {
            let id = tool.id();
            assert!(!tool.wants_main_panel(), "{id:?} must not ask for a dock panel");
            assert!(
                !tool.captures_canvas_pointer(probe),
                "{id:?} must not claim the canvas pointer"
            );
            assert!(!tool.poll_workers(), "{id:?} owns no worker channel");
            assert!(tool.take_actions().is_empty(), "{id:?} queues no tab action");
            // The push must be accepted and ignored, not merely accepted: a tool that stored the
            // rects without using them would still answer the two questions above the same way.
            tool.set_panel_rects(&[Rect::from_min_max(Pos2::ZERO, Pos2::new(50.0, 50.0))]);
            assert!(!tool.captures_canvas_pointer(probe));
            assert!(tool.take_actions().is_empty());
        }
    }

    /// A tool with nothing but the required methods gets the same inert answers — the defaults live
    /// in the trait, not in the shipped tools.
    #[test]
    fn a_tool_implementing_only_the_required_methods_is_inert() {
        let mut tool: Box<dyn PsTool> = Box::new(InertTool);
        assert!(!tool.wants_main_panel());
        assert!(!tool.captures_canvas_pointer(Pos2::new(1.0, 1.0)));
        assert!(!tool.poll_workers());
        assert!(tool.take_actions().is_empty());
    }

    /// The other half: each hook really is overridable through `dyn PsTool`, and
    /// `take_actions` DRAINS — a second call must not hand the tab the same commit twice.
    #[test]
    fn an_overriding_tool_is_reached_through_the_trait_object() {
        let mut tool: Box<dyn PsTool> = Box::new(RegionStubTool::default());
        assert!(tool.wants_main_panel());
        assert!(tool.poll_workers());

        let panel = Rect::from_min_max(Pos2::ZERO, Pos2::new(40.0, 40.0));
        tool.set_panel_rects(&[panel]);
        assert!(tool.captures_canvas_pointer(Pos2::new(10.0, 10.0)));
        assert!(!tool.captures_canvas_pointer(Pos2::new(100.0, 100.0)));

        let mut stack = LayerStack::new(
            7,
            [4, 4],
            egui::ColorImage::new([4, 4], vec![egui::Color32::TRANSPARENT; 16]),
            egui::ColorImage::new([4, 4], vec![egui::Color32::TRANSPARENT; 16]),
        );
        let mut selection = None;
        let mut ctx = PsToolContext {
            page_idx: 7,
            page_size: [4, 4],
            pointer_image: None,
            pointer_in_viewport: false,
            primary_pressed: false,
            primary_down: false,
            primary_released: false,
            secondary_down: false,
            pointer_delta: egui::Vec2::ZERO,
            modifiers: egui::Modifiers::default(),
            cancel_pressed: false,
            remove_point_pressed: false,
            view: ViewTransform {
                viewport_rect: Rect::from_min_max(Pos2::ZERO, Pos2::new(100.0, 100.0)),
                zoom: 1.0,
                center_world: egui::Vec2::new(2.0, 2.0),
            },
            stack: &mut stack,
            selection: &mut selection,
        };
        let _ = tool.interact(&mut ctx);

        let queued = tool.take_actions();
        assert_eq!(queued.len(), 1);
        let PsToolAction::WriteRegion(write) = &queued[0];
        assert_eq!(
            write.page_idx, 7,
            "the context must carry the page the stack is on, so a queued commit can name it"
        );
        assert!(
            tool.take_actions().is_empty(),
            "take_actions must DRAIN: a commit handed over twice would be applied twice"
        );
    }
}
