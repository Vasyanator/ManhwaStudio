/*
File: tab/input.rs

Purpose:
The «Текст» tab's side of the unified input system (`ms-input`): what the tab declares (its
commands, modes, axes, pointer gestures and their targets), which contexts it publishes each
frame, the binding texts its bottom hint, panels and mask tab show, and the ONE dispatch of this
frame's fired commands and axes, run by `TypingTabState::draw` BEFORE `canvas.draw` with the
selected layer as the argument.

Key items:
- the `ActionId` / `TargetId` / `ContextId` constants of the set (ids are persistence keys in
  `user_config` -> `Hotkeys`: never rename them);
- `declarations()`: the tab's `Declarations`, registered once by the studio through
  `StudioTab::input_declarations`;
- `TypingInputState` + `contexts_for`: the pure context rule behind `StudioTab::input_contexts`;
- `TypingInputLabels` / `TypingInputLabelCache`: binding texts, rebuilt only when the registry
  revision or the UI locale changes;
- `TypingTabState::run_input_frame`: the dispatch;
- `nudge_step_px`, `nudge_keys_held`, `pointer_button`, `layer_menu_popup`:
  helpers of the dispatch and of the per-widget gesture sites.

Notes:
- No handler here checks keyboard focus: key chords never match while a text field has focus
  (`ms-input` P1), and commands never fire while a popup is open.
- Pointer gestures on layers, the layout editor and the mask resolve through
  `InputFrame::gesture_for` at their widgets (`draw_page.rs`, `selection_rasters.rs`,
  `vector_transform.rs`, `panels.rs` + `layout_editor.rs`, `mask.rs`); the create rectangle is a
  canvas-wide gesture the router claims and the canvas captures (`create_upload.rs`).
- Shadowing: `typing.layer.rotate` (Sub) takes over Ctrl+wheel from the `view.zoom` axis only
  while a rotatable layer is selected and the pointer is on the canvas; Ctrl+= / Z+wheel / Ctrl+drag
  keep zooming. `typing.font_size` (Tab) takes over Shift+wheel from egui's horizontal scroll while
  it has a target, and yields to the mask brush (`brush.size`, Sub) while the mask is edited.
*/

use ms_canvas::{CanvasHintHelp, CanvasHintRow};
use ms_config::app_tab::AppTab;
use ms_i18n::LocaleTag;
use ms_input::{
    Accept, ActionId, ActionMeta, ActionSpec, AxisSource, AxisSpec, Binding, Button, Chord, CommandSpec, Condition, ContextId, Declarations, GestureKind,
    GestureSpec, HitPhase, InputFrame, InputRegistry, ModeEffect, ModeSpec, ModifierKey, MouseButton, ReleasePolicy, Sample, Scope, TargetId, TargetSpec,
    TouchSource, view,
};
use ms_tools::mask_brush::BRUSH_ACTIVE;

use super::{TypingTabState, TypingTextOverlayLayer, canvas_page_view, context_menu_popup};
use super::{arrow_nudge_step_px, egui, Pos2};

// --- Contexts -----------------------------------------------------------------------------------

/// Context: the vector-line layout editor is open (any sub-mode).
pub(crate) const LAYOUT_EDITOR: ContextId = ContextId("typing.layout_editor");
/// Context: the layout editor is in its Editing sub-mode (points and frame on the canvas).
pub(crate) const LAYOUT_EDITING: ContextId = ContextId("typing.layout_editor.editing");
/// Context: the clipping-mask tab is open (the mask brush owns the canvas).
pub(crate) const MASK_EDIT: ContextId = ContextId("typing.mask_edit");
/// Context: a layer (text / image overlay or raster) is selected and the layer tools are live
/// (no mask editing, no layout editor).
pub(crate) const LAYER_SELECTED: ContextId = ContextId("typing.layer_selected");
/// Context: the selected layer is an OVERLAY — a text layer or an image overlay (the code's
/// "overlay"), never a raster. Published together with [`LAYER_SELECTED`].
pub(crate) const LAYER_SELECTED_TEXT: ContextId = ContextId("typing.layer_selected.text");
/// Context: the selected layer may be rotated by the wheel (it is not in a transform mode, whose
/// handles own its geometry). Published together with [`LAYER_SELECTED`].
pub(crate) const LAYER_ROTATABLE: ContextId = ContextId("typing.layer_selected.rotatable");
/// Context: a create rectangle may start (nothing loading, no create editor or create job, no mask
/// editing, no layout editing).
pub(crate) const CREATE_AVAILABLE: ContextId = ContextId("typing.create_available");
/// Context: the quick font-size axis has a target: the create panel in create mode, or the edit
/// panel of a selected TEXT layer.
pub(crate) const FONT_SIZE_TARGET: ContextId = ContextId("typing.font_size_target");

// --- Tab actions --------------------------------------------------------------------------------

/// Command: show / hide the clean overlay (default C).
pub(crate) const TOGGLE_CLEAN: ActionId = ActionId("typing.toggle_clean");
/// Command: toggle the centering assist (default H).
pub(crate) const TOGGLE_CENTERING_ASSIST: ActionId = ActionId("typing.toggle_centering_assist");
/// Mode: held, a drag on the canvas selects a text area (default Shift). Referenced by
/// [`CREATE_RECT`]; releasing it mid-drag commits the rectangle.
pub(crate) const CREATE_MODE: ActionId = ActionId("typing.create_mode");
/// Gesture (canvas-wide): select a text area — LMB drag under [`CREATE_MODE`], or a finger
/// long-press drag.
pub(crate) const CREATE_RECT: ActionId = ActionId("typing.create_rect");
/// Axis: quick font size of the create panel / the selected text layer (default Shift+wheel).
pub(crate) const FONT_SIZE: ActionId = ActionId("typing.font_size");

// --- Selected-layer actions ---------------------------------------------------------------------

/// Command: auto-type the selected text layer over the bubble under it (default V).
pub(crate) const LAYER_AUTOTYPE: ActionId = ActionId("typing.layer.autotype");
/// Command: delete the selected overlay (default Delete).
pub(crate) const LAYER_DELETE: ActionId = ActionId("typing.layer.delete");
/// Command: duplicate the selected text layer (default Ctrl+D).
pub(crate) const LAYER_DUPLICATE: ActionId = ActionId("typing.layer.duplicate");
/// Command: scale the selected layer up by 10 % (default = / Shift+= / +).
pub(crate) const LAYER_SCALE_UP: ActionId = ActionId("typing.layer.scale_up");
/// Command: scale the selected layer down by 10 % (default -).
pub(crate) const LAYER_SCALE_DOWN: ActionId = ActionId("typing.layer.scale_down");
/// Command: reset the selected layer's scale (default 0).
pub(crate) const LAYER_SCALE_RESET: ActionId = ActionId("typing.layer.scale_reset");
/// Commands: nudge the selected layer by one page pixel (default arrows, auto-repeat).
pub(crate) const LAYER_NUDGE_LEFT: ActionId = ActionId("typing.layer.nudge_left");
pub(crate) const LAYER_NUDGE_RIGHT: ActionId = ActionId("typing.layer.nudge_right");
pub(crate) const LAYER_NUDGE_UP: ActionId = ActionId("typing.layer.nudge_up");
pub(crate) const LAYER_NUDGE_DOWN: ActionId = ActionId("typing.layer.nudge_down");
/// Commands: nudge the selected layer by [`NUDGE_FAST_STEP_PX`] page pixels (default Shift+arrows).
pub(crate) const LAYER_NUDGE_FAST_LEFT: ActionId = ActionId("typing.layer.nudge_fast_left");
pub(crate) const LAYER_NUDGE_FAST_RIGHT: ActionId = ActionId("typing.layer.nudge_fast_right");
pub(crate) const LAYER_NUDGE_FAST_UP: ActionId = ActionId("typing.layer.nudge_fast_up");
pub(crate) const LAYER_NUDGE_FAST_DOWN: ActionId = ActionId("typing.layer.nudge_fast_down");
/// Axis: rotate the selected layer by 2° per wheel notch (default Ctrl+wheel).
pub(crate) const LAYER_ROTATE: ActionId = ActionId("typing.layer.rotate");

/// The nudge commands in `[left, right, up, down]` order (plain, then fast) — the slot order of
/// `mesh_geometry::arrow_nudge_step_px`.
pub(crate) const NUDGE_PLAIN: [ActionId; 4] = [LAYER_NUDGE_LEFT, LAYER_NUDGE_RIGHT, LAYER_NUDGE_UP, LAYER_NUDGE_DOWN];
pub(crate) const NUDGE_FAST: [ActionId; 4] = [LAYER_NUDGE_FAST_LEFT, LAYER_NUDGE_FAST_RIGHT, LAYER_NUDGE_FAST_UP, LAYER_NUDGE_FAST_DOWN];
/// Page pixels one fast nudge moves (one plain nudge moves 1); the step `arrow_nudge_step_px` applies.
pub(crate) const NUDGE_FAST_STEP_PX: f32 = 5.0;

// --- Layer gestures -----------------------------------------------------------------------------

/// Gesture: select a layer (LMB click on it).
pub(crate) const LAYER_SELECT: ActionId = ActionId("typing.layer.select");
/// Gesture: open a layer's menu (RMB click on it; it also selects the layer).
pub(crate) const LAYER_MENU: ActionId = ActionId("typing.layer.menu");
/// Gesture: move a layer (LMB drag on its body).
pub(crate) const LAYER_MOVE: ActionId = ActionId("typing.layer.move");
/// Gesture: resize the centering-assist frame (LMB drag on a frame corner).
pub(crate) const LAYER_RESIZE_CENTERING: ActionId = ActionId("typing.layer.resize_centering");
/// Gesture: change a text layer's width (LMB drag on a width-guide tick).
pub(crate) const LAYER_SET_WIDTH: ActionId = ActionId("typing.layer.set_width");
/// Gesture: drag a deformation handle of a layer in transform mode.
pub(crate) const LAYER_DEFORM: ActionId = ActionId("typing.layer.deform");
/// Gesture: a deformation-brush stroke on a layer in transform mode.
pub(crate) const LAYER_DEFORM_BRUSH: ActionId = ActionId("typing.layer.deform_brush");
/// Gesture: rotate a layer with its rotation handle.
pub(crate) const LAYER_ROTATE_DRAG: ActionId = ActionId("typing.layer.rotate_drag");

/// Target: a corner of the selected text layer's centering-assist frame.
pub(crate) const TARGET_CENTERING_CORNER: TargetId = TargetId("typing.layer.centering_corner");
/// Target: a width-guide end tick of the selected text layer.
pub(crate) const TARGET_WIDTH_TICK: TargetId = TargetId("typing.layer.width_tick");
/// Target: a deformation handle of a layer in transform mode (overlay or raster).
pub(crate) const TARGET_DEFORM_HANDLE: TargetId = TargetId("typing.layer.deform_handle");
/// Target: the inside of a layer in a brush transform mode.
pub(crate) const TARGET_BRUSH: TargetId = TargetId("typing.layer.brush");
/// Target: the rotation handle of the selected layer.
pub(crate) const TARGET_ROTATE_HANDLE: TargetId = TargetId("typing.layer.rotate_handle");
/// Target: a layer's body (its grab area).
pub(crate) const TARGET_BODY: TargetId = TargetId("typing.layer.body");

// --- Layout editor ------------------------------------------------------------------------------

/// Command: delete the last point of the active vector line (default Delete).
pub(crate) const LAYOUT_DELETE_POINT: ActionId = ActionId("typing.layout.delete_point");
/// Mode: held, a click / drag on the last point of the active line appends a point (default Shift).
pub(crate) const LAYOUT_EXTEND_MODE: ActionId = ActionId("typing.layout.extend_mode");
/// Gesture: place the first point of an empty line (LMB click in the frame).
pub(crate) const LAYOUT_ADD_POINT: ActionId = ActionId("typing.layout.add_point");
/// Gesture: append a point after the last one ([extend] + LMB click / drag on the last point).
pub(crate) const LAYOUT_APPEND_POINT: ActionId = ActionId("typing.layout.append_point");
/// Gesture: move a line point (LMB drag on it).
pub(crate) const LAYOUT_MOVE_POINT: ActionId = ActionId("typing.layout.move_point");
/// Gesture: resize the layout frame (LMB drag on one of its eight handles).
pub(crate) const LAYOUT_RESIZE_FRAME: ActionId = ActionId("typing.layout.resize_frame");

/// Target: a layout-frame resize handle.
pub(crate) const TARGET_LAYOUT_HANDLE: TargetId = TargetId("typing.layout.handle");
/// Target: the last point of the active line.
pub(crate) const TARGET_LAYOUT_LAST_POINT: TargetId = TargetId("typing.layout.last_point");
/// Target: any point of the active line.
pub(crate) const TARGET_LAYOUT_POINT: TargetId = TargetId("typing.layout.point");
/// Target: the inside of the layout frame.
pub(crate) const TARGET_LAYOUT_FRAME: TargetId = TargetId("typing.layout.frame");

// --- Clipping mask ------------------------------------------------------------------------------

/// Mode: held, the paint button erases (default Shift).
pub(crate) const MASK_ERASE_MODE: ActionId = ActionId("typing.mask.erase_mode");
/// Gesture: paint the mask (LMB drag; a press alone paints a dot).
pub(crate) const MASK_PAINT: ActionId = ActionId("typing.mask.paint");
/// Gesture: erase the mask (RMB drag, or LMB drag under [`MASK_ERASE_MODE`]).
pub(crate) const MASK_ERASE: ActionId = ActionId("typing.mask.erase");
/// Gesture: flood-fill an area by colour (LMB click, fill mode).
pub(crate) const MASK_FILL: ActionId = ActionId("typing.mask.fill");
/// Target: a page under the open mask tab.
pub(crate) const TARGET_MASK_PAGE: TargetId = TargetId("typing.mask.page");

const SECTION_TAB: &str = "typing.input.section_tab";
const SECTION_LAYER: &str = "typing.input.section_layer";
const SECTION_LAYOUT: &str = "typing.input.section_layout";
const SECTION_MASK: &str = "typing.input.section_mask";

const TAB: Scope = Scope::Tab(AppTab::Typing);
const SUB: Scope = Scope::Sub(Some(AppTab::Typing));

/// The declared targets with their priorities (higher wins among the hit candidates of one
/// widget): a centering corner over a width tick over a deformation handle over the brush area
/// over the rotation handle over the body; a frame handle over the last point over any point over
/// the frame inside.
const TARGETS: [(TargetId, u16); 11] = [
    (TARGET_CENTERING_CORNER, 95),
    (TARGET_WIDTH_TICK, 90),
    (TARGET_DEFORM_HANDLE, 85),
    (TARGET_BRUSH, 80),
    (TARGET_ROTATE_HANDLE, 75),
    (TARGET_BODY, 60),
    (TARGET_LAYOUT_HANDLE, 90),
    (TARGET_LAYOUT_LAST_POINT, 75),
    (TARGET_LAYOUT_POINT, 70),
    (TARGET_LAYOUT_FRAME, 20),
    (TARGET_MASK_PAGE, 10),
];

fn meta(id: ActionId, title_key: &'static str, section_key: &'static str, scope: Scope, when: Vec<Condition>, defaults: Vec<Binding>) -> ActionMeta {
    ActionMeta { id, title_key, section_key, scope, when, defaults }
}

fn key(key: egui::Key) -> Button {
    Button::Key(key)
}

fn shift() -> Button {
    Button::Modifier(ModifierKey::Shift)
}

fn single(buttons: Vec<Button>) -> Binding {
    Binding::command(Chord::new(buttons))
}

fn command(meta: ActionMeta, repeat: bool) -> ActionSpec {
    ActionSpec::Command(CommandSpec { meta, accepts: Vec::new(), repeat })
}

fn mode(meta: ActionMeta, effect: ModeEffect) -> ActionSpec {
    ActionSpec::Mode(ModeSpec { meta, effect, on_release_mid_gesture: ReleasePolicy::Commit, beats: Vec::new(), param: None })
}

fn gesture(meta: ActionMeta, target: TargetId, accepts: Vec<Accept>) -> ActionSpec {
    ActionSpec::Gesture(GestureSpec { meta, target, accepts })
}

fn mouse(chord: Chord, button: MouseButton, kind: GestureKind) -> Binding {
    Binding::gesture(chord, Button::Mouse(button), kind)
}

fn left_drag() -> Vec<Binding> {
    vec![mouse(Chord::default(), MouseButton::Left, GestureKind::Drag)]
}

/// The Typing tab's input declarations: its tab commands, the create mode and rectangle, the
/// quick font-size axis, the selected-layer commands and rotation axis, the layer gestures and
/// targets, the layout-editor and clipping-mask sub-contexts, and the exclusive context groups the
/// conflict check needs.
#[must_use]
pub fn declarations() -> Declarations {
    use ModifierKey::Command as Cmd;
    let selected = || vec![Condition::Context(LAYER_SELECTED)];
    let selected_text = || vec![Condition::Context(LAYER_SELECTED), Condition::Context(LAYER_SELECTED_TEXT)];
    let layout = || vec![Condition::Context(LAYOUT_EDITING)];
    let mask = || vec![Condition::Context(MASK_EDIT)];
    let nudge = |id: ActionId, title_key: &'static str, arrow: egui::Key, fast: bool| {
        let buttons = if fast { vec![shift(), key(arrow)] } else { vec![key(arrow)] };
        command(meta(id, title_key, SECTION_LAYER, SUB, selected(), vec![single(buttons)]), true)
    };
    let layer_gesture = |id: ActionId, title_key: &'static str, target: TargetId, defaults: Vec<Binding>| gesture(meta(id, title_key, SECTION_LAYER, TAB, Vec::new(), defaults), target, Vec::new());
    let accepts_extend = || vec![Accept { mode: LAYOUT_EXTEND_MODE, sample: Sample::AtPress }];
    let extend = || Chord::default().with_modes([LAYOUT_EXTEND_MODE]);
    let actions = vec![
        command(meta(TOGGLE_CLEAN, "typing.input.toggle_clean_title", SECTION_TAB, TAB, vec![Condition::NotContext(LAYOUT_EDITOR)], vec![single(vec![key(egui::Key::C)])]), false),
        command(
            meta(TOGGLE_CENTERING_ASSIST, "typing.input.toggle_centering_assist_title", SECTION_TAB, TAB, vec![Condition::NotContext(LAYOUT_EDITOR)], vec![single(vec![key(egui::Key::H)])]),
            false,
        ),
        mode(
            meta(CREATE_MODE, "typing.input.create_mode_title", SECTION_TAB, TAB, vec![Condition::NotContext(MASK_EDIT), Condition::NotContext(LAYOUT_EDITING)], vec![Binding::mode([shift()])]),
            ModeEffect::OnDragStart,
        ),
        gesture(
            meta(
                CREATE_RECT,
                "typing.input.create_rect_title",
                SECTION_TAB,
                TAB,
                vec![Condition::Context(CREATE_AVAILABLE), Condition::PointerOnView],
                vec![
                    mouse(Chord::default().with_modes([CREATE_MODE]), MouseButton::Left, GestureKind::Drag),
                    Binding::gesture(Chord::default(), Button::Touch(TouchSource::LongPressDrag), GestureKind::Drag),
                ],
            ),
            view::SURFACE,
            Vec::new(),
        ),
        ActionSpec::Axis(AxisSpec {
            meta: meta(
                FONT_SIZE,
                "typing.input.font_size_title",
                SECTION_TAB,
                TAB,
                vec![Condition::Context(FONT_SIZE_TARGET), Condition::PointerOnView],
                // Some backends turn Shift+wheel into a horizontal wheel; both drive the axis.
                vec![Binding::axis(AxisSource::WheelY, Chord::new([shift()])), Binding::axis(AxisSource::WheelX, Chord::new([shift()]))],
            ),
        }),
        command(meta(LAYER_AUTOTYPE, "typing.input.layer_autotype_title", SECTION_LAYER, SUB, selected_text(), vec![single(vec![key(egui::Key::V)])]), false),
        command(meta(LAYER_DELETE, "typing.input.layer_delete_title", SECTION_LAYER, SUB, selected_text(), vec![single(vec![key(egui::Key::Delete)])]), false),
        command(
            meta(LAYER_DUPLICATE, "typing.input.layer_duplicate_title", SECTION_LAYER, SUB, selected_text(), vec![single(vec![Button::Modifier(Cmd), key(egui::Key::D)])]),
            false,
        ),
        command(
            meta(
                LAYER_SCALE_UP,
                "typing.input.layer_scale_up_title",
                SECTION_LAYER,
                SUB,
                selected(),
                vec![single(vec![key(egui::Key::Equals)]), single(vec![shift(), key(egui::Key::Equals)]), single(vec![key(egui::Key::Plus)])],
            ),
            false,
        ),
        command(meta(LAYER_SCALE_DOWN, "typing.input.layer_scale_down_title", SECTION_LAYER, SUB, selected(), vec![single(vec![key(egui::Key::Minus)])]), false),
        command(meta(LAYER_SCALE_RESET, "typing.input.layer_scale_reset_title", SECTION_LAYER, SUB, selected(), vec![single(vec![key(egui::Key::Num0)])]), false),
        nudge(LAYER_NUDGE_LEFT, "typing.input.layer_nudge_left_title", egui::Key::ArrowLeft, false),
        nudge(LAYER_NUDGE_RIGHT, "typing.input.layer_nudge_right_title", egui::Key::ArrowRight, false),
        nudge(LAYER_NUDGE_UP, "typing.input.layer_nudge_up_title", egui::Key::ArrowUp, false),
        nudge(LAYER_NUDGE_DOWN, "typing.input.layer_nudge_down_title", egui::Key::ArrowDown, false),
        nudge(LAYER_NUDGE_FAST_LEFT, "typing.input.layer_nudge_fast_left_title", egui::Key::ArrowLeft, true),
        nudge(LAYER_NUDGE_FAST_RIGHT, "typing.input.layer_nudge_fast_right_title", egui::Key::ArrowRight, true),
        nudge(LAYER_NUDGE_FAST_UP, "typing.input.layer_nudge_fast_up_title", egui::Key::ArrowUp, true),
        nudge(LAYER_NUDGE_FAST_DOWN, "typing.input.layer_nudge_fast_down_title", egui::Key::ArrowDown, true),
        ActionSpec::Axis(AxisSpec {
            meta: meta(
                LAYER_ROTATE,
                "typing.input.layer_rotate_title",
                SECTION_LAYER,
                SUB,
                vec![Condition::Context(LAYER_ROTATABLE), Condition::PointerOnView],
                vec![Binding::axis(AxisSource::WheelY, Chord::new([Button::Modifier(Cmd)]))],
            ),
        }),
        layer_gesture(LAYER_SELECT, "typing.input.layer_select_title", TARGET_BODY, vec![mouse(Chord::default(), MouseButton::Left, GestureKind::Click)]),
        layer_gesture(LAYER_MENU, "typing.input.layer_menu_title", TARGET_BODY, vec![mouse(Chord::default(), MouseButton::Right, GestureKind::Click)]),
        layer_gesture(LAYER_MOVE, "typing.input.layer_move_title", TARGET_BODY, left_drag()),
        layer_gesture(LAYER_RESIZE_CENTERING, "typing.input.layer_resize_centering_title", TARGET_CENTERING_CORNER, left_drag()),
        layer_gesture(LAYER_SET_WIDTH, "typing.input.layer_set_width_title", TARGET_WIDTH_TICK, left_drag()),
        layer_gesture(LAYER_DEFORM, "typing.input.layer_deform_title", TARGET_DEFORM_HANDLE, left_drag()),
        layer_gesture(LAYER_DEFORM_BRUSH, "typing.input.layer_deform_brush_title", TARGET_BRUSH, left_drag()),
        layer_gesture(LAYER_ROTATE_DRAG, "typing.input.layer_rotate_drag_title", TARGET_ROTATE_HANDLE, left_drag()),
        command(meta(LAYOUT_DELETE_POINT, "typing.input.layout_delete_point_title", SECTION_LAYOUT, SUB, layout(), vec![single(vec![key(egui::Key::Delete)])]), false),
        mode(meta(LAYOUT_EXTEND_MODE, "typing.input.layout_extend_mode_title", SECTION_LAYOUT, SUB, layout(), vec![Binding::mode([shift()])]), ModeEffect::Immediate),
        gesture(
            meta(LAYOUT_ADD_POINT, "typing.input.layout_add_point_title", SECTION_LAYOUT, SUB, layout(), vec![mouse(Chord::default(), MouseButton::Left, GestureKind::Click)]),
            TARGET_LAYOUT_FRAME,
            accepts_extend(),
        ),
        gesture(
            meta(
                LAYOUT_APPEND_POINT,
                "typing.input.layout_append_point_title",
                SECTION_LAYOUT,
                SUB,
                layout(),
                vec![mouse(extend(), MouseButton::Left, GestureKind::Click), mouse(extend(), MouseButton::Left, GestureKind::Drag)],
            ),
            TARGET_LAYOUT_LAST_POINT,
            Vec::new(),
        ),
        gesture(meta(LAYOUT_MOVE_POINT, "typing.input.layout_move_point_title", SECTION_LAYOUT, SUB, layout(), left_drag()), TARGET_LAYOUT_POINT, accepts_extend()),
        gesture(meta(LAYOUT_RESIZE_FRAME, "typing.input.layout_resize_frame_title", SECTION_LAYOUT, SUB, layout(), left_drag()), TARGET_LAYOUT_HANDLE, Vec::new()),
        mode(meta(MASK_ERASE_MODE, "typing.input.mask_erase_mode_title", SECTION_MASK, SUB, mask(), vec![Binding::mode([shift()])]), ModeEffect::OnDragStart),
        gesture(meta(MASK_PAINT, "typing.input.mask_paint_title", SECTION_MASK, SUB, mask(), left_drag()), TARGET_MASK_PAGE, Vec::new()),
        gesture(
            meta(
                MASK_ERASE,
                "typing.input.mask_erase_title",
                SECTION_MASK,
                SUB,
                mask(),
                vec![mouse(Chord::default(), MouseButton::Right, GestureKind::Drag), mouse(Chord::default().with_modes([MASK_ERASE_MODE]), MouseButton::Left, GestureKind::Drag)],
            ),
            TARGET_MASK_PAGE,
            Vec::new(),
        ),
        gesture(meta(MASK_FILL, "typing.input.mask_fill_title", SECTION_MASK, SUB, mask(), vec![mouse(Chord::default(), MouseButton::Left, GestureKind::Click)]), TARGET_MASK_PAGE, Vec::new()),
    ];
    let targets = TARGETS.iter().map(|&(id, priority)| TargetSpec { id, priority, canvas_wide: false }).collect();
    Declarations {
        actions,
        targets,
        fixed: Vec::new(),
        // At most one of each group holds at a time (by construction in `contexts_for` and the
        // studio's brush rule): the layer tools, the layout editor and the mask never run
        // together. `MASK_EDIT` and `BRUSH_ACTIVE` are published TOGETHER, so they sit in two
        // different groups.
        exclusive_contexts: vec![vec![LAYER_SELECTED, LAYOUT_EDITING, BRUSH_ACTIVE], vec![LAYER_SELECTED, LAYOUT_EDITING, MASK_EDIT]],
    }
}

/// One scale command of the selected layer (`selection_rasters.rs::scale_selected_layer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypingScaleStep {
    /// ×1.1.
    Up,
    /// ÷1.1.
    Down,
    /// Back to 1.
    Reset,
}

/// What the context rule reads of the tab's state (a pure snapshot, built by
/// `TypingTabState::input_state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TypingInputState {
    pub(super) mask_panel_open: bool,
    pub(super) layout_editor_active: bool,
    pub(super) layout_editor_editing: bool,
    pub(super) overlay_selected: bool,
    pub(super) raster_selected: bool,
    /// The selected layer (overlay or raster) is in a transform mode.
    pub(super) selection_in_transform_mode: bool,
    /// `TypingTextOverlayLayer::touch_create_selection_available`.
    pub(super) create_available: bool,
    /// `TypingTopPanelState::font_size_wheel_target`.
    pub(super) font_size_wheel_target: bool,
}

/// The contexts the tab publishes for `state` (its own; the canvas's and the brush's are added by
/// `StudioTab::input_contexts`). The layer contexts hold only while no mask is edited and no layout
/// editor is open, which is what makes the declared exclusive groups true.
#[must_use]
pub(super) fn contexts_for(state: TypingInputState) -> Vec<ContextId> {
    let mut contexts = Vec::new();
    if state.mask_panel_open {
        contexts.push(MASK_EDIT);
    }
    if state.layout_editor_active {
        contexts.push(LAYOUT_EDITOR);
        // The editor's canvas pass does not run while the mask is edited.
        if state.layout_editor_editing && !state.mask_panel_open {
            contexts.push(LAYOUT_EDITING);
        }
    }
    let layer_tools = !state.mask_panel_open && !state.layout_editor_active;
    if layer_tools && (state.overlay_selected || state.raster_selected) {
        contexts.push(LAYER_SELECTED);
        if state.overlay_selected {
            contexts.push(LAYER_SELECTED_TEXT);
        }
        if !state.selection_in_transform_mode {
            contexts.push(LAYER_ROTATABLE);
        }
    }
    if !state.mask_panel_open && !state.layout_editor_editing && state.create_available {
        contexts.push(CREATE_AVAILABLE);
    }
    if layer_tools && state.font_size_wheel_target {
        contexts.push(FONT_SIZE_TARGET);
    }
    contexts
}

/// The binding texts the tab's panels, layer menus and mask tab show (alternatives joined by
/// " / ", from `InputRegistry::label`). Owned copies, pushed to each owner when they change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TypingInputLabels {
    pub(crate) layer_select: String,
    pub(crate) layer_menu: String,
    pub(crate) toggle_clean: String,
    pub(crate) toggle_centering_assist: String,
    pub(crate) autotype: String,
    pub(crate) mask_paint: String,
    pub(crate) mask_erase: String,
    pub(crate) mask_fill: String,
}

impl TypingInputLabels {
    /// The labels of `registry`'s current bindings.
    #[must_use]
    pub(crate) fn from_registry(registry: &InputRegistry) -> Self {
        Self {
            layer_select: registry.label(LAYER_SELECT),
            layer_menu: registry.label(LAYER_MENU),
            toggle_clean: registry.label(TOGGLE_CLEAN),
            toggle_centering_assist: registry.label(TOGGLE_CENTERING_ASSIST),
            autotype: registry.label(LAYER_AUTOTYPE),
            mask_paint: registry.label(MASK_PAINT),
            mask_erase: registry.label(MASK_ERASE),
            mask_fill: registry.label(MASK_FILL),
        }
    }
}

/// The canvas bottom-hint rows of the tab, every binding text read from `registry`. Order is
/// canonical; the select-text-area row carries the animated "?" help (three-column grid mode).
#[must_use]
pub(super) fn typing_hint_rows(registry: &InputRegistry) -> Vec<CanvasHintRow> {
    let nudge = |ids: [ActionId; 4]| {
        let [left, right, up, down] = ids.map(|id| registry.label(id));
        tf!("canvas.bottom_hint.typing.nudge_bindings", left = left, up = up, down = down, right = right)
    };
    vec![
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.select_text_area_label"), registry.label(CREATE_RECT))
            .with_help(CanvasHintHelp::Animation(ms_gifs::typing::SELECT_TEXT_AREA)),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.quick_font_size_label"), registry.label(FONT_SIZE)),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.rotate_label"), registry.label(LAYER_ROTATE)),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.scale_label"),
            tf!(
                "canvas.bottom_hint.typing.scale_bindings",
                up = registry.label(LAYER_SCALE_UP),
                down = registry.label(LAYER_SCALE_DOWN),
                reset = registry.label(LAYER_SCALE_RESET)
            ),
        ),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.nudge_label"), nudge(NUDGE_PLAIN)),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.nudge_fast_label"), nudge(NUDGE_FAST)),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.autotype_label"), registry.label(LAYER_AUTOTYPE)),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.delete_label"), registry.label(LAYER_DELETE)),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.duplicate_label"), registry.label(LAYER_DUPLICATE)),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.toggle_clean_label"), registry.label(TOGGLE_CLEAN)),
        CanvasHintRow::new(t!("canvas.bottom_hint.typing.toggle_centering_assist_label"), registry.label(TOGGLE_CENTERING_ASSIST)),
    ]
}

/// The tab's binding texts, rebuilt only when the registry revision or the UI locale changed.
#[derive(Debug, Default)]
pub(super) struct TypingInputLabelCache {
    built_for: Option<(u64, LocaleTag)>,
    hint_rows: Vec<CanvasHintRow>,
    labels: TypingInputLabels,
}

impl TypingInputLabelCache {
    /// Rebuilds from `registry` when its revision or the active UI locale changed since the last
    /// build. Returns whether it rebuilt (the owners of the labels must then be updated).
    pub(super) fn refresh(&mut self, registry: &InputRegistry) -> bool {
        let key = (registry.revision(), ms_i18n::active_locale());
        if self.built_for.as_ref() == Some(&key) {
            return false;
        }
        self.hint_rows = typing_hint_rows(registry);
        self.labels = TypingInputLabels::from_registry(registry);
        self.built_for = Some(key);
        true
    }

    /// The bottom-hint rows of the last build.
    #[must_use]
    pub(super) fn hint_rows(&self) -> &[CanvasHintRow] {
        &self.hint_rows
    }

    /// The panel / menu / mask labels of the last build.
    #[must_use]
    pub(super) fn labels(&self) -> &TypingInputLabels {
        &self.labels
    }
}

/// The page-pixel step of this frame's nudge commands: `fired` answers whether a command fired.
/// Plain commands move 1 px, fast ones [`NUDGE_FAST_STEP_PX`]; opposite directions cancel.
#[must_use]
pub(super) fn nudge_step_px(fired: impl Fn(ActionId) -> bool) -> [f32; 2] {
    arrow_nudge_step_px(NUDGE_PLAIN.map(&fired), NUDGE_FAST.map(&fired))
}

/// Whether the button that fired a nudge command is still held (`InputFrame::command_held`): the
/// end of a keyboard move session (`move_layer.rs::drive_layer_move_settle`). A held arrow keeps
/// ONE move session open across the OS repeat delay, whatever the nudge commands are bound to.
#[must_use]
pub(super) fn nudge_keys_held(input: &InputFrame) -> bool {
    NUDGE_PLAIN.iter().chain(NUDGE_FAST.iter()).any(|id| input.command_held(*id))
}

/// The egui pointer button of a binding's mouse button (`None` for an extra button egui has no
/// name for).
#[must_use]
pub(super) fn pointer_button(button: MouseButton) -> Option<egui::PointerButton> {
    match button {
        MouseButton::Left => Some(egui::PointerButton::Primary),
        MouseButton::Right => Some(egui::PointerButton::Secondary),
        MouseButton::Middle => Some(egui::PointerButton::Middle),
        MouseButton::Extra(1) => Some(egui::PointerButton::Extra1),
        MouseButton::Extra(2) => Some(egui::PointerButton::Extra2),
        MouseButton::Extra(_) => None,
    }
}

/// The layer context menu of `response` for this frame, or `None` when it must not be built.
///
/// Opened by [`LAYER_MENU`] on the layer body (`hit`: the pointer is on the layer, not only on its
/// widget's bounding rect), or by a finger long-press lift (`touch_menu_at`, anchored there). A
/// secondary click the binding does not own (another modifier held, the menu rebound elsewhere, the
/// pointer off the layer) skips the popup for this frame, so it neither opens nor stays open; a
/// binding on another button opens it at the pointer (the helper opens by itself only on a
/// secondary click). Returns `(opened_by_binding, popup)`.
pub(super) fn layer_menu_popup<'r>(input: &InputFrame, response: &'r egui::Response, hit: bool, touch_menu_at: Option<Pos2>) -> Option<(bool, egui::Popup<'r>)> {
    let owned = input.gesture_for(response, &[(TARGET_BODY, hit)]).is_some_and(|gesture| gesture.action == LAYER_MENU && gesture.phase == HitPhase::Clicked);
    if response.secondary_clicked() && !response.long_touched() && !owned {
        return None;
    }
    let binding_anchor = (owned && !response.secondary_clicked()).then(|| response.interact_pointer_pos().or_else(|| response.hover_pos())).flatten();
    Some((owned, context_menu_popup(response, touch_menu_at.or(binding_anchor))))
}

impl TypingTextOverlayLayer {
    /// Records the mouse button of a layer pointer gesture that starts now (`GestureHit::button`):
    /// its drag continues, and its move session settles, by that button.
    pub(super) fn begin_layer_gesture(&mut self, button: MouseButton) {
        if let Some(pointer) = pointer_button(button) {
            self.layer_gesture_button = pointer;
        }
    }

    /// Whether the pointer button of the current (or last) layer gesture is held.
    #[must_use]
    pub(super) fn layer_gesture_button_down(&self, ctx: &egui::Context) -> bool {
        let button = self.layer_gesture_button;
        ctx.input(|input| input.pointer.button_down(button))
    }

    /// Whether the selected layer (overlay or raster) is in a transform mode.
    #[must_use]
    pub(super) fn selection_in_transform_mode(&self) -> bool {
        let overlay = self.selected_overlay_idx.is_some() && self.transform_mode_overlay_idx == self.selected_overlay_idx;
        let raster = self.selected_raster_idx.is_some() && self.transform_mode_raster_idx == self.selected_raster_idx;
        overlay || raster
    }

    /// The page of the selected layer (overlay or raster), if any.
    #[must_use]
    pub(super) fn selected_layer_page(&self) -> Option<usize> {
        match self.selected_overlay_idx {
            Some(idx) => self.overlays.get(idx).map(|overlay| overlay.page_idx),
            None => self.selected_raster_idx.and(self.selected_raster_page),
        }
    }
}

impl TypingTabState {
    /// The state the context rule reads (see [`contexts_for`]).
    #[must_use]
    pub(super) fn input_state(&self) -> TypingInputState {
        let layers = &self.text_overlays;
        TypingInputState {
            mask_panel_open: self.mask_layer.is_panel_open(),
            layout_editor_active: layers.layout_editor_active(),
            layout_editor_editing: layers.layout_editor_editing_active(),
            overlay_selected: layers.has_selected_overlay(),
            raster_selected: layers.has_selected_raster(),
            selection_in_transform_mode: layers.selection_in_transform_mode(),
            create_available: layers.touch_create_selection_available(),
            font_size_wheel_target: self.top_panel.font_size_wheel_target(),
        }
    }

    /// Refreshes the binding texts and pushes them to their owners when they changed, then sets
    /// this frame's bottom hint.
    pub(super) fn refresh_input_labels(&mut self, registry: &InputRegistry) {
        if self.input_labels.refresh(registry) {
            let labels = self.input_labels.labels();
            self.top_panel.set_input_labels(labels.clone());
            self.text_overlays.input_labels = labels.clone();
            self.mask_layer.set_input_labels(labels.clone());
        }
        self.set_bottom_hint(Some(ms_canvas::CanvasBottomHint { rows: self.input_labels.hint_rows().to_vec() }));
    }

    /// Runs this frame's Typing commands and axes, once, before `canvas.draw`. Selection commands
    /// act on the selected layer and only while its page is drawn (the canvas' previous-frame page
    /// rects give the view). Returns whether anything changed (the caller repaints).
    pub(super) fn run_input_frame(&mut self, ctx: &egui::Context, input: &InputFrame) -> bool {
        let fired = |id: ActionId| input.fired(id).is_some();
        let mut changed = false;
        if fired(TOGGLE_CLEAN) {
            // The panel owns the value after its one-shot seed from the canvas: seed first, so a
            // toggle on the very first frame is not overwritten by the seed.
            self.top_panel.sync_clean_overlays_visible_from_canvas(self.canvas.clean_overlays_visible());
            self.top_panel.toggle_clean_overlays_visible();
            changed = true;
        }
        if fired(TOGGLE_CENTERING_ASSIST) {
            self.top_panel.toggle_centering_assist();
            changed = true;
        }
        let view_id = self.canvas.view_id();
        // Axes report one sample per source; a rebinding may feed one axis from several.
        let font_steps = input.axis_samples_for_view(FONT_SIZE, view_id).fold(0_i32, |sum, sample| sum.saturating_add(sample.steps));
        if font_steps != 0 {
            changed |= if self.text_overlays.has_selected_overlay() {
                self.top_panel.adjust_selected_text_overlay_font_size_by_wheel_steps(font_steps)
            } else {
                self.top_panel.adjust_create_font_size_by_wheel_steps(font_steps)
            };
        }
        if fired(LAYOUT_DELETE_POINT) {
            changed |= self.text_overlays.layout_editor_delete_last_point(ctx);
        }
        // Everything below acts on the selected layer, on the page it lives on.
        let Some(view) = self.text_overlays.selected_layer_page().and_then(|page_idx| canvas_page_view(&self.canvas, page_idx)) else {
            return changed;
        };
        let strict_pixel_movement = self.top_panel.strict_pixel_movement();
        let layers = &mut self.text_overlays;
        // Rotation follows the continuous `delta` (trackpad point deltas rotate smoothly, a wheel's
        // line events by whole notches); the font size keeps whole `steps` (it walks a size list).
        let mut rotate = input.axis_samples_for_view(LAYER_ROTATE, view_id).map(|sample| sample.delta).peekable();
        if rotate.peek().is_some() {
            changed |= layers.rotate_selected_layer_by_wheel(ctx, view, rotate.sum());
        }
        if fired(LAYER_AUTOTYPE) {
            layers.trigger_selected_overlay_auto_typing(ctx, view, self.top_panel.auto_typing_settings());
            changed = true;
        }
        let scale = if fired(LAYER_SCALE_RESET) {
            Some(TypingScaleStep::Reset)
        } else if fired(LAYER_SCALE_UP) {
            Some(TypingScaleStep::Up)
        } else if fired(LAYER_SCALE_DOWN) {
            Some(TypingScaleStep::Down)
        } else {
            None
        };
        if let Some(step) = scale {
            changed |= layers.scale_selected_layer(step);
        }
        let step_page_px = nudge_step_px(fired);
        if step_page_px != [0.0, 0.0] {
            changed |= layers.nudge_selected_layer(step_page_px, view, strict_pixel_movement);
        }
        // Structural changes last: they shift overlay indices. Applied before the page pass, so the
        // frame paints the result (no blank frame) and no gesture of this frame holds a stale index.
        if fired(LAYER_DELETE)
            && let Some(idx) = layers.selected_overlay_idx
        {
            layers.remove_overlay(idx);
            changed = true;
        } else if fired(LAYER_DUPLICATE)
            && let Some(idx) = layers.selected_overlay_idx
        {
            changed |= layers.duplicate_overlay(idx).is_some();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ms_input::{ActionKind, ActiveSet, MatchEvent, MatchState};

    use super::*;

    /// The studio's shared sets plus the Typing declarations, as the studio registers them.
    fn registry() -> InputRegistry {
        let mut declarations = Declarations::default();
        declarations.extend(view::declarations());
        declarations.extend(ms_canvas::input::declarations());
        declarations.extend(ms_tools::mask_brush::declarations());
        declarations.extend(super::declarations());
        match InputRegistry::new(declarations) {
            Ok(registry) => registry,
            Err(err) => panic!("the typing declarations must be valid: {err}"),
        }
    }

    fn typing_env(contexts: &[ContextId]) -> ActiveSet {
        let all: BTreeSet<ContextId> = contexts.iter().copied().collect();
        // The pointer on the canvas view: that view also takes the focus-less zoom commands.
        ActiveSet { active_tab: Some(AppTab::Typing), contexts: all, pointer_on_view: true, view_focus: true, ..ActiveSet::default() }
    }

    fn press(state: &mut MatchState, registry: &InputRegistry, env: &ActiveSet, button: Button) -> Vec<ActionId> {
        state
            .step(registry, &MatchEvent::Press { button, repeat: false }, env)
            .into_iter()
            .filter_map(|outcome| match outcome {
                ms_input::MatchOutcome::Fired(fired) => Some(fired.action),
                ms_input::MatchOutcome::SequencePending => None,
            })
            .collect()
    }

    /// Pins every Typing action: id, kind, scope and default label in English, in declaration
    /// order. Ids are persistence keys. Also pins that no default binding of the tab conflicts with
    /// the view set, the canvas set or the shared brush set.
    #[test]
    fn typing_declarations_golden() {
        use ActionKind::{Axis, Command, Gesture, Mode};
        let _locale = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        ms_i18n::set_locale(&LocaleTag::english()).expect("embedded English catalog installs");
        let registry = registry();
        let ctrl = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" };
        let minus = egui::Key::Minus.symbol_or_name();
        let expected: Vec<(&str, ActionKind, Scope, String)> = vec![
            ("typing.toggle_clean", Command, TAB, "C".to_owned()),
            ("typing.toggle_centering_assist", Command, TAB, "H".to_owned()),
            ("typing.create_mode", Mode, TAB, "Shift".to_owned()),
            ("typing.create_rect", Gesture, TAB, "Shift+LMB drag / Long press and drag".to_owned()),
            ("typing.font_size", Axis, TAB, "Shift+Wheel / Shift+Horizontal wheel".to_owned()),
            ("typing.layer.autotype", Command, SUB, "V".to_owned()),
            ("typing.layer.delete", Command, SUB, "Del".to_owned()),
            ("typing.layer.duplicate", Command, SUB, format!("{ctrl}+D")),
            ("typing.layer.scale_up", Command, SUB, "= / Shift+= / +".to_owned()),
            ("typing.layer.scale_down", Command, SUB, minus.to_owned()),
            ("typing.layer.scale_reset", Command, SUB, "0".to_owned()),
            ("typing.layer.nudge_left", Command, SUB, "←".to_owned()),
            ("typing.layer.nudge_right", Command, SUB, "→".to_owned()),
            ("typing.layer.nudge_up", Command, SUB, "↑".to_owned()),
            ("typing.layer.nudge_down", Command, SUB, "↓".to_owned()),
            ("typing.layer.nudge_fast_left", Command, SUB, "Shift+←".to_owned()),
            ("typing.layer.nudge_fast_right", Command, SUB, "Shift+→".to_owned()),
            ("typing.layer.nudge_fast_up", Command, SUB, "Shift+↑".to_owned()),
            ("typing.layer.nudge_fast_down", Command, SUB, "Shift+↓".to_owned()),
            ("typing.layer.rotate", Axis, SUB, format!("{ctrl}+Wheel")),
            ("typing.layer.select", Gesture, TAB, "LMB click".to_owned()),
            ("typing.layer.menu", Gesture, TAB, "RMB click".to_owned()),
            ("typing.layer.move", Gesture, TAB, "LMB drag".to_owned()),
            ("typing.layer.resize_centering", Gesture, TAB, "LMB drag".to_owned()),
            ("typing.layer.set_width", Gesture, TAB, "LMB drag".to_owned()),
            ("typing.layer.deform", Gesture, TAB, "LMB drag".to_owned()),
            ("typing.layer.deform_brush", Gesture, TAB, "LMB drag".to_owned()),
            ("typing.layer.rotate_drag", Gesture, TAB, "LMB drag".to_owned()),
            ("typing.layout.delete_point", Command, SUB, "Del".to_owned()),
            ("typing.layout.extend_mode", Mode, SUB, "Shift".to_owned()),
            ("typing.layout.add_point", Gesture, SUB, "LMB click".to_owned()),
            ("typing.layout.append_point", Gesture, SUB, "Shift+LMB click / Shift+LMB drag".to_owned()),
            ("typing.layout.move_point", Gesture, SUB, "LMB drag".to_owned()),
            ("typing.layout.resize_frame", Gesture, SUB, "LMB drag".to_owned()),
            ("typing.mask.erase_mode", Mode, SUB, "Shift".to_owned()),
            ("typing.mask.paint", Gesture, SUB, "LMB drag".to_owned()),
            ("typing.mask.erase", Gesture, SUB, "RMB drag / Shift+LMB drag".to_owned()),
            ("typing.mask.fill", Gesture, SUB, "LMB click".to_owned()),
        ];
        let actual: Vec<(&str, ActionKind, Scope, String)> = registry
            .actions()
            .filter(|action| action.meta.id.0.starts_with("typing."))
            .map(|action| (action.meta.id.0, action.kind, action.meta.scope, registry.label(action.meta.id)))
            .collect();
        assert_eq!(actual, expected);
        let conflicts: Vec<_> = registry.conflicts().conflicts.iter().filter(|conflict| conflict.a.0.starts_with("typing.") || conflict.b.is_some_and(|b| b.0.starts_with("typing."))).collect();
        assert!(conflicts.is_empty(), "the typing defaults conflict: {conflicts:?}");
        // The one intended shadow of the selection axes: Ctrl+wheel rotate over the view zoom axis.
        assert!(
            registry.shadows_for(LAYER_ROTATE).iter().any(|shadow| shadow.inner == LAYER_ROTATE && shadow.outer == view::ZOOM),
            "typing.layer.rotate must shadow view.zoom"
        );
    }

    /// The bottom hint spells only registry labels: eleven rows in canonical order, the first with
    /// the help animation; the scale row lists the real (unmodified) keys.
    #[test]
    fn the_hint_rows_come_from_the_registry() {
        let _locale = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        ms_i18n::set_locale(&LocaleTag::english()).expect("embedded English catalog installs");
        let rows = typing_hint_rows(&registry());
        assert_eq!(rows.len(), 11);
        assert!(rows[0].help.is_some());
        assert!(rows[1..].iter().all(|row| row.help.is_none()));
        assert_eq!(rows[0].keys, "Shift+LMB drag / Long press and drag");
        let minus = egui::Key::Minus.symbol_or_name();
        assert_eq!(rows[3].keys, format!("= / Shift+= / +, {minus}, 0"));
        assert_eq!(rows[4].keys, "←, ↑, ↓, →");
        assert_eq!(rows[5].keys, "Shift+←, Shift+↑, Shift+↓, Shift+→");
        assert_eq!(rows[6].keys, "V");
    }

    /// The selection contexts follow the layer tools: none while the mask is edited or the layout
    /// editor is open; `.text` only for an overlay; `.rotatable` only outside transform modes. The
    /// font-size target likewise needs the layer tools.
    #[test]
    fn contexts_for_the_selection_axes() {
        let raster = TypingInputState { raster_selected: true, ..TypingInputState::default() };
        assert_eq!(contexts_for(raster), vec![LAYER_SELECTED, LAYER_ROTATABLE]);
        let text = TypingInputState { overlay_selected: true, font_size_wheel_target: true, ..TypingInputState::default() };
        assert_eq!(contexts_for(text), vec![LAYER_SELECTED, LAYER_SELECTED_TEXT, LAYER_ROTATABLE, FONT_SIZE_TARGET]);
        let transformed = TypingInputState { selection_in_transform_mode: true, ..text };
        assert_eq!(contexts_for(transformed), vec![LAYER_SELECTED, LAYER_SELECTED_TEXT, FONT_SIZE_TARGET]);
        let masked = TypingInputState { mask_panel_open: true, ..text };
        assert_eq!(contexts_for(masked), vec![MASK_EDIT]);
        let layout = TypingInputState { layout_editor_active: true, layout_editor_editing: true, create_available: true, ..text };
        assert_eq!(contexts_for(layout), vec![LAYOUT_EDITOR, LAYOUT_EDITING]);
        let preview = TypingInputState { layout_editor_editing: false, ..layout };
        assert_eq!(contexts_for(preview), vec![LAYOUT_EDITOR, CREATE_AVAILABLE]);
        let creating = TypingInputState { create_available: true, font_size_wheel_target: true, ..TypingInputState::default() };
        assert_eq!(contexts_for(creating), vec![CREATE_AVAILABLE, FONT_SIZE_TARGET]);
    }

    /// Ctrl+wheel is the rotate axis while a rotatable layer is selected and the view zoom
    /// otherwise; Z+wheel keeps zooming with a layer selected; Shift+wheel is the font size with a
    /// target, the brush size while the mask is edited.
    #[test]
    fn the_selection_axes_shadow_only_their_chords() {
        let registry = registry();
        let mut state = MatchState::default();
        let selected = typing_env(&[LAYER_SELECTED, LAYER_ROTATABLE]);
        let idle = typing_env(&[]);
        press(&mut state, &registry, &selected, Button::Modifier(ModifierKey::Command));
        assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &selected).map(|axis| axis.action), Some(LAYER_ROTATE));
        assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &idle).map(|axis| axis.action), Some(view::ZOOM));
        state.step(&registry, &MatchEvent::Release { button: Button::Modifier(ModifierKey::Command) }, &selected);
        press(&mut state, &registry, &selected, key(egui::Key::Z));
        assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &selected).map(|axis| axis.action), Some(view::ZOOM), "Z+wheel zooms with a layer selected");
        state.step(&registry, &MatchEvent::Release { button: key(egui::Key::Z) }, &selected);
        press(&mut state, &registry, &selected, shift());
        let font = typing_env(&[FONT_SIZE_TARGET]);
        assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &font).map(|axis| axis.action), Some(FONT_SIZE));
        let mut masked = typing_env(&[MASK_EDIT]);
        masked.contexts.insert(BRUSH_ACTIVE);
        assert_eq!(state.match_axis(&registry, AxisSource::WheelY, &masked).map(|axis| axis.action), Some(ms_tools::mask_brush::BRUSH_SIZE));
        let off_view = ActiveSet { pointer_on_view: false, ..font };
        assert_ne!(state.match_axis(&registry, AxisSource::WheelY, &off_view).map(|axis| axis.action), Some(FONT_SIZE), "Shift+wheel over a panel is not the font size");
    }

    /// Ctrl+= keeps zooming with a layer selected (only the wheel is shadowed); plain = scales the
    /// layer; Delete is the layer's or the layout editor's by context, with exact modifiers.
    #[test]
    fn selection_commands_fire_by_context_with_exact_modifiers() {
        let registry = registry();
        let selected = typing_env(&[LAYER_SELECTED, LAYER_SELECTED_TEXT, LAYER_ROTATABLE]);
        let mut state = MatchState::default();
        assert_eq!(press(&mut state, &registry, &selected, key(egui::Key::Equals)), vec![LAYER_SCALE_UP]);
        state.step(&registry, &MatchEvent::Release { button: key(egui::Key::Equals) }, &selected);
        press(&mut state, &registry, &selected, Button::Modifier(ModifierKey::Command));
        assert_eq!(press(&mut state, &registry, &selected, key(egui::Key::Equals)), vec![view::ZOOM_IN]);
        assert!(press(&mut state, &registry, &selected, key(egui::Key::Delete)).is_empty(), "Ctrl+Delete deletes nothing");
        let mut state = MatchState::default();
        assert_eq!(press(&mut state, &registry, &selected, key(egui::Key::Delete)), vec![LAYER_DELETE]);
        let mut state = MatchState::default();
        assert_eq!(press(&mut state, &registry, &typing_env(&[LAYOUT_EDITOR, LAYOUT_EDITING]), key(egui::Key::Delete)), vec![LAYOUT_DELETE_POINT]);
        let mut state = MatchState::default();
        press(&mut state, &registry, &selected, shift());
        assert_eq!(press(&mut state, &registry, &selected, key(egui::Key::ArrowLeft)), vec![LAYER_NUDGE_FAST_LEFT], "Shift+arrow is the fast nudge");
    }

    /// Each nudge command moves its own direction: 1 px plain, 5 px fast; opposite ones cancel.
    #[test]
    fn nudge_distance_per_command() {
        let expected: [(ActionId, [f32; 2]); 8] = [
            (LAYER_NUDGE_LEFT, [-1.0, 0.0]),
            (LAYER_NUDGE_RIGHT, [1.0, 0.0]),
            (LAYER_NUDGE_UP, [0.0, -1.0]),
            (LAYER_NUDGE_DOWN, [0.0, 1.0]),
            (LAYER_NUDGE_FAST_LEFT, [-NUDGE_FAST_STEP_PX, 0.0]),
            (LAYER_NUDGE_FAST_RIGHT, [NUDGE_FAST_STEP_PX, 0.0]),
            (LAYER_NUDGE_FAST_UP, [0.0, -NUDGE_FAST_STEP_PX]),
            (LAYER_NUDGE_FAST_DOWN, [0.0, NUDGE_FAST_STEP_PX]),
        ];
        for (id, step) in expected {
            assert_eq!(nudge_step_px(|fired| fired == id), step, "{id:?}");
        }
        assert_eq!(nudge_step_px(|fired| fired == LAYER_NUDGE_LEFT || fired == LAYER_NUDGE_RIGHT), [0.0, 0.0]);
        assert_eq!(nudge_step_px(|fired| fired == LAYER_NUDGE_RIGHT || fired == LAYER_NUDGE_FAST_DOWN), [1.0, NUDGE_FAST_STEP_PX]);
    }

    /// The declared priorities decide what a press on overlapping layer parts drives, and only the
    /// left button starts a layer drag.
    #[test]
    fn layer_gesture_candidates_resolve_by_priority() {
        let registry = registry();
        let state = MatchState::default();
        let env = typing_env(&[LAYER_SELECTED, LAYER_SELECTED_TEXT, LAYER_ROTATABLE]);
        let resolve = |button: MouseButton, targets: &[TargetId]| state.match_gesture(&registry, &Button::Mouse(button), GestureKind::Drag, targets, &env).map(|matched| matched.action);
        let all = [TARGET_BODY, TARGET_ROTATE_HANDLE, TARGET_BRUSH, TARGET_DEFORM_HANDLE, TARGET_WIDTH_TICK, TARGET_CENTERING_CORNER];
        assert_eq!(resolve(MouseButton::Left, &all), Some(LAYER_RESIZE_CENTERING));
        assert_eq!(resolve(MouseButton::Left, &all[..5]), Some(LAYER_SET_WIDTH));
        assert_eq!(resolve(MouseButton::Left, &all[..4]), Some(LAYER_DEFORM));
        assert_eq!(resolve(MouseButton::Left, &all[..3]), Some(LAYER_DEFORM_BRUSH));
        assert_eq!(resolve(MouseButton::Left, &all[..2]), Some(LAYER_ROTATE_DRAG));
        assert_eq!(resolve(MouseButton::Left, &all[..1]), Some(LAYER_MOVE));
        assert_eq!(resolve(MouseButton::Right, &all), None, "a right drag moves nothing");
        assert_eq!(resolve(MouseButton::Middle, &all), None, "a middle drag moves nothing");
        let layout = typing_env(&[LAYOUT_EDITOR, LAYOUT_EDITING]);
        let mut state = MatchState::default();
        let layout_targets = [TARGET_LAYOUT_FRAME, TARGET_LAYOUT_POINT, TARGET_LAYOUT_LAST_POINT];
        let layout_drag = |state: &MatchState| state.match_gesture(&registry, &Button::Mouse(MouseButton::Left), GestureKind::Drag, &layout_targets, &layout).map(|matched| matched.action);
        assert_eq!(layout_drag(&state), Some(LAYOUT_MOVE_POINT), "a plain drag on the last point moves it");
        press(&mut state, &registry, &layout, shift());
        assert_eq!(layout_drag(&state), Some(LAYOUT_APPEND_POINT), "Shift+drag on the last point appends");
        assert_eq!(
            state.match_gesture(&registry, &Button::Mouse(MouseButton::Left), GestureKind::Drag, &layout_targets[..2], &layout).map(|matched| matched.action),
            Some(LAYOUT_MOVE_POINT),
            "Shift+drag on another point still moves it"
        );
    }

    /// The mask: a plain left drag paints, Shift+left or a right drag erases, a left click fills.
    #[test]
    fn mask_gestures_paint_erase_and_fill() {
        let registry = registry();
        let mut env = typing_env(&[MASK_EDIT]);
        env.contexts.insert(BRUSH_ACTIVE);
        let mut state = MatchState::default();
        let resolve = |state: &MatchState, button: MouseButton, kind: GestureKind| state.match_gesture(&registry, &Button::Mouse(button), kind, &[TARGET_MASK_PAGE], &env).map(|matched| matched.action);
        assert_eq!(resolve(&state, MouseButton::Left, GestureKind::Drag), Some(MASK_PAINT));
        assert_eq!(resolve(&state, MouseButton::Right, GestureKind::Drag), Some(MASK_ERASE));
        assert_eq!(resolve(&state, MouseButton::Left, GestureKind::Click), Some(MASK_FILL));
        press(&mut state, &registry, &env, shift());
        assert_eq!(resolve(&state, MouseButton::Left, GestureKind::Drag), Some(MASK_ERASE));
    }

    #[test]
    fn mouse_buttons_map_to_egui_pointer_buttons() {
        assert_eq!(pointer_button(MouseButton::Left), Some(egui::PointerButton::Primary));
        assert_eq!(pointer_button(MouseButton::Right), Some(egui::PointerButton::Secondary));
        assert_eq!(pointer_button(MouseButton::Extra(2)), Some(egui::PointerButton::Extra2));
        assert_eq!(pointer_button(MouseButton::Extra(7)), None);
    }
}
