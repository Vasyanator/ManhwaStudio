/*
File: crates/ms-tab-cleaning/src/input.rs

Purpose:
The «Клининг» tab's side of the unified input system (`ms-input`): every action the tab, its tools,
the on-canvas region frame and the legacy region-editor window declare, the contexts that switch
them on, the gesture targets of the frame and the window, and the binding-label snapshot the tool
hints are built from.

Key items:
- `declarations()`: the tab's `Declarations` (registered once by the studio through
  `StudioTab::input_declarations`).
- the `ContextId` constants (`TOOL_BRUSH`, `TOOL_ZAMAZKA`, `TOOL_STAMP`, `TOOL_PATCH`,
  `REGION_SELECT`, `REGION_ARMED`, `REGION_FRAME`, `EDITOR_WINDOW`) and the one exclusive group of
  the tool contexts plus the zamazka / stamp pair.
- the `ActionId` / `TargetId` constants (persistence keys: never rename an action id).
- `STROKE_ACTIONS` / `PRESS_ACTIONS`: the canvas-wide claims `canvas_input.rs` drives.
- `CleaningLabels`: the binding texts of the hints, rebuilt only when the registry revision or the
  UI locale changes, handed to the tools as an `Arc` snapshot.
- `editor_window_view_id`: the view identity of a legacy region-editor window.

Notes:
- Canvas-wide gestures are bindings on `ms_input::view::SURFACE`: the router claims them at the
  press and the canvas raises its capture `Area`, so no bubble or page row reacts. Strokes and
  selections are `Drag` bindings; the two right-button press actions (`PICK_COLOR`,
  `STAMP_SET_ANCHOR`) are `Click` bindings, claimed at the press and acted on at the claim's
  `Started` phase (the press itself), as the old secondary-press handling did.
- Scopes: the clean-overlay history is `Tab(Cleaning)`; tool modes, canvas gestures, the frame
  and the tool commands are `Sub(Some(Cleaning))`; the legacy window's brush gestures are
  `Window(Some(Cleaning))`. The frame's stroke undo (`FRAME_UNDO_STROKE`) therefore shadows the
  tab's Ctrl+Z while the frame is up, and the `rect_mode` Ctrl shadows only the Ctrl alternative of
  `view.zoom_modifier` for canvas drags (Z+drag keeps zooming in every tool).
*/

use std::sync::Arc;

use eframe::egui;
use egui::Id;
use ms_config::app_tab::AppTab;
use ms_i18n::LocaleTag;
use ms_input::{
    Accept, ActionId, ActionMeta, ActionSpec, AxisSource, AxisSpec, Binding, Button, Chord, CommandSpec, Condition, ContextId, Declarations, GestureKind,
    GestureSpec, InputRegistry, ModeEffect, ModeSpec, ModifierKey, MouseButton, ReleasePolicy, Sample, Scope, TargetId, TargetSpec, TouchSource, ViewId,
    view,
};
use ms_tools::mask_brush;

/// Context: the active tool paints on the canvas with a brush (Замазка, Штамп).
pub const TOOL_BRUSH: ContextId = ContextId("cleaning.tool.brush");
/// Context: «Замазка» is the active tool.
pub const TOOL_ZAMAZKA: ContextId = ContextId("cleaning.tool.zamazka");
/// Context: «Штамп» is the active tool.
pub const TOOL_STAMP: ContextId = ContextId("cleaning.tool.stamp");
/// Context: «Заплатка» is the active tool.
pub const TOOL_PATCH: ContextId = ContextId("cleaning.tool.patch");
/// Context: a region tool takes a canvas selection now (no legacy editor window is open).
pub const REGION_SELECT: ContextId = ContextId("cleaning.region.select");
/// Context: a region tool expects a selection (its selection diversion is armed): a plain drag
/// selects as well. Published only together with [`REGION_SELECT`].
pub const REGION_ARMED: ContextId = ContextId("cleaning.region.armed");
/// Context: a region-frame host tool is active (the on-canvas frame takes its own gestures).
pub const REGION_FRAME: ContextId = ContextId("cleaning.region_frame");
/// Context: a legacy region-editor window is open.
pub const EDITOR_WINDOW: ContextId = ContextId("cleaning.editor_window");

/// Command: undo the last committed clean-overlay edit.
pub const CLEAN_UNDO: ActionId = ActionId("cleaning.clean.undo");
/// Command: redo the last undone clean-overlay edit.
pub const CLEAN_REDO: ActionId = ActionId("cleaning.clean.redo");
/// Mode: a stroke erases while held (brush tools, the frame, the editor window).
pub const ERASE_MODE: ActionId = ActionId("cleaning.erase_mode");
/// Mode: a brush stroke draws a rectangle while held at its press (Замазка, Штамп).
pub const RECT_MODE: ActionId = ActionId("cleaning.rect_mode");
/// Mode: a drag selects a region while held (region tools).
pub const SELECT_MODE: ActionId = ActionId("cleaning.select_mode");
/// Mode: the wheel rotates the stamp while held.
pub const STAMP_ROTATE_MODE: ActionId = ActionId("cleaning.stamp.rotate_mode");
/// Gesture: a brush stroke on the page (accepts `ERASE_MODE` latest, `RECT_MODE` at the press).
pub const STROKE: ActionId = ActionId("cleaning.stroke");
/// Gesture (`Click`, acted on at the press): pick the colour under the pointer (Замазка).
pub const PICK_COLOR: ActionId = ActionId("cleaning.pick_color");
/// Gesture (`Click`, acted on at the press): set the clone-stamp source point (Штамп).
pub const STAMP_SET_ANCHOR: ActionId = ActionId("cleaning.stamp.set_anchor");
/// Axis: rotate the stamp, fine steps.
pub const STAMP_ROTATE: ActionId = ActionId("cleaning.stamp.rotate");
/// Axis: rotate the stamp, coarse steps.
pub const STAMP_ROTATE_COARSE: ActionId = ActionId("cleaning.stamp.rotate_coarse");
/// Gesture: select a region (rectangle) on the page.
pub const REGION_SELECT_RECT: ActionId = ActionId("cleaning.region.select_rect");
/// Gesture: select a region while a selection is expected (no selection mode needed).
pub const REGION_SELECT_RECT_ARMED: ActionId = ActionId("cleaning.region.select_rect_armed");
/// Command: cancel the region selection (not while a legacy editor window is open).
pub const REGION_CANCEL: ActionId = ActionId("cleaning.region.cancel");
/// Gesture: the patch drag — a new selection, or dragging the selection onto its source (the
/// patch core decides from the press point).
pub const PATCH_DRAG: ActionId = ActionId("cleaning.patch.drag");
/// Command: clear the patch selection / cancel the patch work in flight.
pub const PATCH_CANCEL: ActionId = ActionId("cleaning.patch.cancel");
/// Mode: a frame-body drag draws a marks rectangle while held at its press (marks mode).
pub const FRAME_MARKS_RECT_MODE: ActionId = ActionId("cleaning.frame.marks_rect_mode");
/// Gesture: paint the frame's mask (or marks) layer.
pub const FRAME_PAINT: ActionId = ActionId("cleaning.frame.paint");
/// Gesture: erase the frame's mask (or marks) layer, whatever the panel mode says.
pub const FRAME_ERASE: ActionId = ActionId("cleaning.frame.erase");
/// Gesture: draw a hollow marks rectangle on the frame body (marks mode).
pub const FRAME_MARKS_RECT: ActionId = ActionId("cleaning.frame.marks_rect");
/// Gesture: move the frame by its grip.
pub const FRAME_MOVE: ActionId = ActionId("cleaning.frame.move");
/// Gesture: resize the frame by a handle.
pub const FRAME_RESIZE: ActionId = ActionId("cleaning.frame.resize");
/// Command: undo the last mask / marks stroke of the frame.
pub const FRAME_UNDO_STROKE: ActionId = ActionId("cleaning.frame.undo_stroke");
/// Gesture: paint the mask in a legacy region-editor window.
pub const EDITOR_PAINT: ActionId = ActionId("cleaning.editor.paint");
/// Gesture: erase the mask in a legacy region-editor window.
pub const EDITOR_ERASE: ActionId = ActionId("cleaning.editor.erase");

/// Target: the frame body (paint / erase / marks rectangle).
pub const TARGET_FRAME_BODY: TargetId = TargetId("cleaning.frame.body");
/// Target: the frame's drag grip in its top strip.
pub const TARGET_FRAME_GRIP: TargetId = TargetId("cleaning.frame.grip");
/// Target: a frame resize handle.
pub const TARGET_FRAME_HANDLE: TargetId = TargetId("cleaning.frame.handle");
/// Target: the preview image of a legacy region-editor window.
pub const TARGET_EDITOR_IMAGE: TargetId = TargetId("cleaning.editor.image");

/// The canvas-wide claims that drive a tool STROKE (`CleaningTool::stroke_begin` / `update` /
/// `end`).
pub const STROKE_ACTIONS: [ActionId; 4] = [STROKE, PATCH_DRAG, REGION_SELECT_RECT, REGION_SELECT_RECT_ARMED];
/// The canvas-wide claims acted on once, at their press (`CleaningTool::on_page_press`).
pub const PRESS_ACTIONS: [ActionId; 2] = [PICK_COLOR, STAMP_SET_ANCHOR];

const SECTION_CLEAN: &str = "cleaning.input.section_clean";
const SECTION_BRUSH: &str = "cleaning.input.section_brush";
const SECTION_REGION: &str = "cleaning.input.section_region";
const SECTION_PATCH: &str = "cleaning.input.section_patch";
const SECTION_AREA_EDITOR: &str = "cleaning.input.section_area_editor";

const SUB: Scope = Scope::Sub(Some(AppTab::Cleaning));

fn meta(id: ActionId, title_key: &'static str, section_key: &'static str, scope: Scope, when: Vec<Condition>, defaults: Vec<Binding>) -> ActionMeta {
    ActionMeta { id, title_key, section_key, scope, when, defaults }
}

fn key(key: egui::Key) -> Button {
    Button::Key(key)
}

fn modifier(modifier: ModifierKey) -> Button {
    Button::Modifier(modifier)
}

fn mouse(button: MouseButton) -> Button {
    Button::Mouse(button)
}

fn mode(meta: ActionMeta) -> ActionSpec {
    ActionSpec::Mode(ModeSpec { meta, effect: ModeEffect::Immediate, on_release_mid_gesture: ReleasePolicy::Commit, beats: Vec::new(), param: None })
}

fn command(meta: ActionMeta) -> ActionSpec {
    ActionSpec::Command(CommandSpec { meta, accepts: Vec::new(), repeat: false })
}

fn gesture(meta: ActionMeta, target: TargetId, accepts: Vec<Accept>) -> ActionSpec {
    ActionSpec::Gesture(GestureSpec { meta, target, accepts })
}

/// A drag of `button` under `chord`.
fn drag(chord: Chord, button: MouseButton) -> Binding {
    Binding::gesture(chord, mouse(button), GestureKind::Drag)
}

/// A plain click of `button` (a canvas-wide press action).
fn click(button: MouseButton) -> Binding {
    Binding::gesture(Chord::default(), mouse(button), GestureKind::Click)
}

/// The tab's input declarations: the clean-overlay history (tab scope), the tool modes, canvas
/// gestures, axes and commands, the region frame's set (sub scope), the legacy editor window's
/// brush gestures (window scope), the frame / window targets and the exclusive context groups.
#[must_use]
pub fn declarations() -> Declarations {
    use MouseButton::{Left, Right};
    let command_key = |k: egui::Key| Chord::new([modifier(ModifierKey::Command), key(k)]);
    let tool = |context: ContextId| vec![Condition::Context(context), Condition::PointerOnView];
    let actions = vec![
        // The clean-overlay history. Off while a legacy editor window is open (it is modal over
        // the region it edits), and shadowed by the frame's stroke undo while the frame is up.
        command(meta(
            CLEAN_UNDO,
            "cleaning.input.clean_undo_title",
            SECTION_CLEAN,
            Scope::Tab(AppTab::Cleaning),
            vec![Condition::NotContext(EDITOR_WINDOW)],
            vec![Binding::command(command_key(egui::Key::Z))],
        )),
        command(meta(
            CLEAN_REDO,
            "cleaning.input.clean_redo_title",
            SECTION_CLEAN,
            Scope::Tab(AppTab::Cleaning),
            vec![Condition::NotContext(EDITOR_WINDOW)],
            vec![
                Binding::command(Chord::new([modifier(ModifierKey::Command), modifier(ModifierKey::Shift), key(egui::Key::Z)])),
                Binding::command(command_key(egui::Key::Y)),
            ],
        )),
        // Tool modes. `erase_mode` and `select_mode` share Shift and are never active together:
        // the selection mode needs `REGION_SELECT`, the erase mode excludes it.
        mode(meta(
            ERASE_MODE,
            "cleaning.input.erase_mode_title",
            SECTION_BRUSH,
            SUB,
            vec![Condition::NotContext(REGION_SELECT)],
            vec![Binding::mode([modifier(ModifierKey::Shift)])],
        )),
        mode(meta(RECT_MODE, "cleaning.input.rect_mode_title", SECTION_BRUSH, SUB, vec![Condition::Context(TOOL_BRUSH)], vec![Binding::mode([modifier(ModifierKey::Command)])])),
        mode(meta(SELECT_MODE, "cleaning.input.select_mode_title", SECTION_REGION, SUB, vec![Condition::Context(REGION_SELECT)], vec![Binding::mode([modifier(ModifierKey::Shift)])])),
        mode(meta(STAMP_ROTATE_MODE, "cleaning.input.stamp_rotate_mode_title", SECTION_BRUSH, SUB, vec![Condition::Context(TOOL_STAMP)], vec![Binding::mode([key(egui::Key::R)])])),
        // Canvas-wide gestures (claimed by the router on the canvas view).
        gesture(
            meta(STROKE, "cleaning.input.stroke_title", SECTION_BRUSH, SUB, tool(TOOL_BRUSH), vec![drag(Chord::default(), Left)]),
            view::SURFACE,
            vec![Accept { mode: ERASE_MODE, sample: Sample::LatestDuringGesture }, Accept { mode: RECT_MODE, sample: Sample::AtPress }],
        ),
        gesture(meta(PICK_COLOR, "cleaning.input.pick_color_title", SECTION_BRUSH, SUB, tool(TOOL_ZAMAZKA), vec![click(Right)]), view::SURFACE, Vec::new()),
        gesture(
            meta(STAMP_SET_ANCHOR, "cleaning.input.stamp_set_anchor_title", SECTION_BRUSH, SUB, tool(TOOL_STAMP), vec![click(Right)]),
            view::SURFACE,
            Vec::new(),
        ),
        ActionSpec::Axis(AxisSpec {
            meta: meta(
                STAMP_ROTATE,
                "cleaning.input.stamp_rotate_title",
                SECTION_BRUSH,
                SUB,
                tool(TOOL_STAMP),
                vec![Binding::axis(AxisSource::WheelY, Chord::default().with_modes([STAMP_ROTATE_MODE]))],
            ),
        }),
        ActionSpec::Axis(AxisSpec {
            meta: meta(
                STAMP_ROTATE_COARSE,
                "cleaning.input.stamp_rotate_coarse_title",
                SECTION_BRUSH,
                SUB,
                tool(TOOL_STAMP),
                vec![Binding::axis(AxisSource::WheelY, Chord::new([modifier(ModifierKey::Shift)]).with_modes([STAMP_ROTATE_MODE]))],
            ),
        }),
        gesture(
            meta(
                REGION_SELECT_RECT,
                "cleaning.input.region_select_rect_title",
                SECTION_REGION,
                SUB,
                tool(REGION_SELECT),
                vec![drag(Chord::default().with_modes([SELECT_MODE]), Left), Binding::gesture(Chord::default(), Button::Touch(TouchSource::LongPressDrag), GestureKind::Drag)],
            ),
            view::SURFACE,
            Vec::new(),
        ),
        gesture(
            meta(
                REGION_SELECT_RECT_ARMED,
                "cleaning.input.region_select_rect_armed_title",
                SECTION_REGION,
                SUB,
                vec![Condition::Context(REGION_SELECT), Condition::Context(REGION_ARMED), Condition::PointerOnView],
                vec![drag(Chord::default(), Left)],
            ),
            view::SURFACE,
            Vec::new(),
        ),
        command(meta(REGION_CANCEL, "cleaning.input.region_cancel_title", SECTION_REGION, SUB, vec![Condition::Context(REGION_SELECT)], vec![Binding::command(Chord::new([key(egui::Key::Escape)]))])),
        gesture(meta(PATCH_DRAG, "cleaning.input.patch_drag_title", SECTION_PATCH, SUB, tool(TOOL_PATCH), vec![drag(Chord::default(), Left)]), view::SURFACE, Vec::new()),
        command(meta(PATCH_CANCEL, "cleaning.input.patch_cancel_title", SECTION_PATCH, SUB, vec![Condition::Context(TOOL_PATCH)], vec![Binding::command(Chord::new([key(egui::Key::Escape)]))])),
        // The on-canvas region frame (targeted gestures, resolved from its own `Response`s).
        mode(meta(
            FRAME_MARKS_RECT_MODE,
            "cleaning.input.frame_marks_rect_mode_title",
            SECTION_AREA_EDITOR,
            SUB,
            vec![Condition::Context(REGION_FRAME)],
            vec![Binding::mode([modifier(ModifierKey::Command)])],
        )),
        gesture(
            meta(FRAME_PAINT, "cleaning.input.frame_paint_title", SECTION_AREA_EDITOR, SUB, vec![Condition::Context(REGION_FRAME)], vec![drag(Chord::default(), Left)]),
            TARGET_FRAME_BODY,
            Vec::new(),
        ),
        gesture(
            meta(
                FRAME_ERASE,
                "cleaning.input.frame_erase_title",
                SECTION_AREA_EDITOR,
                SUB,
                vec![Condition::Context(REGION_FRAME)],
                vec![drag(Chord::default().with_modes([ERASE_MODE]), Left), drag(Chord::default(), Right)],
            ),
            TARGET_FRAME_BODY,
            Vec::new(),
        ),
        gesture(
            meta(
                FRAME_MARKS_RECT,
                "cleaning.input.frame_marks_rect_title",
                SECTION_AREA_EDITOR,
                SUB,
                vec![Condition::Context(REGION_FRAME)],
                vec![drag(Chord::default().with_modes([FRAME_MARKS_RECT_MODE]), Left)],
            ),
            TARGET_FRAME_BODY,
            vec![Accept { mode: ERASE_MODE, sample: Sample::AtPress }],
        ),
        gesture(
            meta(FRAME_MOVE, "cleaning.input.frame_move_title", SECTION_AREA_EDITOR, SUB, vec![Condition::Context(REGION_FRAME)], vec![drag(Chord::default(), Left)]),
            TARGET_FRAME_GRIP,
            Vec::new(),
        ),
        gesture(
            meta(FRAME_RESIZE, "cleaning.input.frame_resize_title", SECTION_AREA_EDITOR, SUB, vec![Condition::Context(REGION_FRAME)], vec![drag(Chord::default(), Left)]),
            TARGET_FRAME_HANDLE,
            Vec::new(),
        ),
        command(meta(
            FRAME_UNDO_STROKE,
            "cleaning.input.frame_undo_stroke_title",
            SECTION_AREA_EDITOR,
            SUB,
            vec![Condition::Context(REGION_FRAME)],
            vec![Binding::command(command_key(egui::Key::Z))],
        )),
        // The legacy region-editor window (targeted gestures on its preview image).
        gesture(
            meta(EDITOR_PAINT, "cleaning.input.editor_paint_title", SECTION_AREA_EDITOR, Scope::Window(Some(AppTab::Cleaning)), vec![Condition::Context(EDITOR_WINDOW)], vec![drag(Chord::default(), Left)]),
            TARGET_EDITOR_IMAGE,
            Vec::new(),
        ),
        gesture(
            meta(
                EDITOR_ERASE,
                "cleaning.input.editor_erase_title",
                SECTION_AREA_EDITOR,
                Scope::Window(Some(AppTab::Cleaning)),
                vec![Condition::Context(EDITOR_WINDOW)],
                vec![drag(Chord::default().with_modes([ERASE_MODE]), Left), drag(Chord::default(), Right)],
            ),
            TARGET_EDITOR_IMAGE,
            Vec::new(),
        ),
    ];
    let targets = [(TARGET_FRAME_HANDLE, 90), (TARGET_FRAME_GRIP, 70), (TARGET_FRAME_BODY, 60), (TARGET_EDITOR_IMAGE, 50)]
        .into_iter()
        .map(|(id, priority)| TargetSpec { id, priority, canvas_wide: false })
        .collect();
    // One tool is active at a time, and of its states only one owns the canvas: brush painting,
    // the patch, a region selection, the frame or an open editor window. Zamazka and the stamp are
    // the two brush tools.
    let exclusive_contexts = vec![vec![TOOL_BRUSH, TOOL_PATCH, REGION_SELECT, REGION_FRAME, EDITOR_WINDOW], vec![TOOL_ZAMAZKA, TOOL_STAMP]];
    Declarations { actions, targets, fixed: Vec::new(), exclusive_contexts }
}

/// The view identity of the legacy region-editor window of the tool whose window id is
/// `window_id` (content-named, stable across `StudioApp` rebuilds).
#[must_use]
pub fn editor_window_view_id(window_id: &str) -> ViewId {
    ViewId(Id::new(("cleaning.editor_window", window_id)))
}

/// The modes of one stroke point: what the accepted parameter modes of the claimed gesture say
/// (`ERASE_MODE` latest, `RECT_MODE` at the press), or what is held now for a hover point.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrokeModes {
    /// The stroke erases (`ERASE_MODE`).
    pub erase: bool,
    /// The stroke draws a rectangle (`RECT_MODE`, sampled at the press).
    pub rect: bool,
}

impl StrokeModes {
    /// The modes carried by a claim's sampled parameters.
    #[must_use]
    pub fn from_params(params: &[ActionId]) -> Self {
        Self { erase: params.contains(&ERASE_MODE), rect: params.contains(&RECT_MODE) }
    }
}

/// The binding texts the cleaning hints are built from (registry labels: alternatives joined by
/// " / ", gesture labels carry their button and kind). Rebuilt by [`CleaningLabels::refresh`] only
/// when the registry revision or the UI locale changed; tools hold the last `Arc` snapshot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleaningLabels {
    pub stroke: String,
    pub erase_mode: String,
    pub rect_mode: String,
    pub pick_color: String,
    pub set_anchor: String,
    pub rotate: String,
    pub rotate_coarse: String,
    pub brush_size: String,
    pub brush_smaller: String,
    pub brush_larger: String,
    pub pan: String,
    pub select_mode: String,
    pub select_rect: String,
    pub select_rect_armed: String,
    pub region_cancel: String,
    pub patch_cancel: String,
    pub frame_erase: String,
    pub editor_paint: String,
    pub editor_erase: String,
}

impl CleaningLabels {
    /// All labels from `registry`.
    #[must_use]
    pub fn from_registry(registry: &InputRegistry) -> Self {
        Self {
            stroke: registry.label(STROKE),
            erase_mode: registry.label(ERASE_MODE),
            rect_mode: registry.label(RECT_MODE),
            pick_color: registry.label(PICK_COLOR),
            set_anchor: registry.label(STAMP_SET_ANCHOR),
            rotate: registry.label(STAMP_ROTATE),
            rotate_coarse: registry.label(STAMP_ROTATE_COARSE),
            brush_size: registry.label(mask_brush::BRUSH_SIZE),
            brush_smaller: registry.label(mask_brush::BRUSH_SMALLER),
            brush_larger: registry.label(mask_brush::BRUSH_LARGER),
            pan: registry.label(view::PAN),
            select_mode: registry.label(SELECT_MODE),
            select_rect: registry.label(REGION_SELECT_RECT),
            select_rect_armed: registry.label(REGION_SELECT_RECT_ARMED),
            region_cancel: registry.label(REGION_CANCEL),
            patch_cancel: registry.label(PATCH_CANCEL),
            frame_erase: registry.label(FRAME_ERASE),
            editor_paint: registry.label(EDITOR_PAINT),
            editor_erase: registry.label(EDITOR_ERASE),
        }
    }
}

/// The tab's cache of [`CleaningLabels`], keyed by the registry revision and the UI locale.
#[derive(Debug, Default)]
pub struct CleaningLabelsCache {
    built_for: Option<(u64, LocaleTag)>,
    labels: Arc<CleaningLabels>,
}

impl CleaningLabelsCache {
    /// The labels for `registry`, rebuilt when its revision or the active UI locale changed since
    /// the last call.
    pub fn refresh(&mut self, registry: &InputRegistry) -> &Arc<CleaningLabels> {
        let key = (registry.revision(), ms_i18n::active_locale());
        if self.built_for.as_ref() != Some(&key) {
            self.labels = Arc::new(CleaningLabels::from_registry(registry));
            self.built_for = Some(key);
        }
        &self.labels
    }
}

/// Headless input frames for the crate's tests: the studio's view set, the canvas set, the
/// shared brush set and this tab's declarations in one router, driven through `Context::run_ui`.
#[cfg(test)]
pub(crate) mod test_support {
    use eframe::egui;
    use egui::{Event, Modifiers, RawInput, Rect};
    use ms_config::app_tab::AppTab;
    use ms_input::{ContextId, Declarations, FrameEnv, InputFrame, InputRegistry, InputRouter};

    /// The registry the studio builds for this tab (minus the other tabs).
    pub(crate) fn registry() -> InputRegistry {
        let mut declarations = Declarations::default();
        declarations.extend(ms_input::view::declarations());
        declarations.extend(ms_canvas::input::declarations());
        declarations.extend(ms_tools::mask_brush::declarations());
        declarations.extend(super::declarations());
        match InputRegistry::new(declarations) {
            Ok(registry) => registry,
            Err(err) => panic!("the cleaning declarations must be valid: {err}"),
        }
    }

    /// One router over [`registry`] in one headless `egui::Context`.
    pub(crate) struct InputBench {
        pub(crate) ctx: egui::Context,
        pub(crate) router: InputRouter,
        time: f64,
        screen: Rect,
    }

    impl InputBench {
        pub(crate) fn new(screen: Rect) -> Self {
            Self { ctx: egui::Context::default(), router: InputRouter::new(registry()), time: 0.0, screen }
        }

        /// Runs one frame with `events` (a leading `ModifiersChanged(modifiers)` is added) and the
        /// Cleaning tab active under `contexts`; `body` runs inside the pass right after
        /// `begin_frame`, with the frame's result.
        pub(crate) fn frame<R>(&mut self, modifiers: Modifiers, events: Vec<Event>, contexts: &[ContextId], body: impl FnOnce(&mut egui::Ui, &InputFrame) -> R) -> R {
            self.time += 0.016;
            let mut all = Vec::with_capacity(events.len() + 1);
            all.push(Event::ModifiersChanged(modifiers));
            all.extend(events);
            let raw = RawInput { screen_rect: Some(self.screen), time: Some(self.time), events: all, ..RawInput::default() };
            let env = FrameEnv { active_tab: Some(AppTab::Cleaning), contexts: contexts.to_vec(), modal_open: false };
            let router = &mut self.router;
            let mut body = Some(body);
            let mut out = None;
            // Headless: no renderer applies the texture deltas, so they are dropped unapplied.
            self.ctx
                .run_ui(raw, |ui| {
                    let frame = router.begin_frame(ui.ctx(), &env).clone();
                    if let Some(body) = body.take() {
                        out = Some(body(ui, &frame));
                    }
                })
                .drop_without_applying_deltas();
            match out {
                Some(out) => out,
                None => panic!("run_ui must call the frame closure once"),
            }
        }
    }

    /// A key press (physical key set, as egui-winit delivers it).
    pub(crate) fn key_press(key: egui::Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers }
    }

    /// One wheel notch (a `Line` event) with `modifiers`.
    pub(crate) fn wheel_line(lines: f32, modifiers: Modifiers) -> Event {
        Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: egui::vec2(0.0, lines), phase: egui::TouchPhase::Move, modifiers }
    }
}

#[cfg(test)]
mod tests {
    use ms_input::{ActionKind, ActiveSet, Binding, Button, InputRegistry, MatchState, ModifierKey, MouseButton, Scope, view};

    use super::test_support::registry;
    use super::*;

    fn english() -> std::sync::MutexGuard<'static, ()> {
        let guard = match ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = ms_i18n::set_locale(&LocaleTag::english()) {
            panic!("embedded English catalog must load: {err}");
        }
        guard
    }

    /// The golden table of the tab's declarations: id, kind, scope and default label.
    #[test]
    fn declarations_golden() {
        let _locale = english();
        let registry = registry();
        let ctrl = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" };
        let sub = Scope::Sub(Some(AppTab::Cleaning));
        let window = Scope::Window(Some(AppTab::Cleaning));
        let tab = Scope::Tab(AppTab::Cleaning);
        let expected: Vec<(ActionId, ActionKind, Scope, String)> = vec![
            (CLEAN_UNDO, ActionKind::Command, tab, format!("{ctrl}+Z")),
            (CLEAN_REDO, ActionKind::Command, tab, format!("{ctrl}+Shift+Z / {ctrl}+Y")),
            (ERASE_MODE, ActionKind::Mode, sub, "Shift".to_owned()),
            (RECT_MODE, ActionKind::Mode, sub, ctrl.to_owned()),
            (SELECT_MODE, ActionKind::Mode, sub, "Shift".to_owned()),
            (STAMP_ROTATE_MODE, ActionKind::Mode, sub, "R".to_owned()),
            (STROKE, ActionKind::Gesture, sub, "LMB drag".to_owned()),
            (PICK_COLOR, ActionKind::Gesture, sub, "RMB click".to_owned()),
            (STAMP_SET_ANCHOR, ActionKind::Gesture, sub, "RMB click".to_owned()),
            (STAMP_ROTATE, ActionKind::Axis, sub, "R+Wheel".to_owned()),
            (STAMP_ROTATE_COARSE, ActionKind::Axis, sub, "R+Shift+Wheel".to_owned()),
            (REGION_SELECT_RECT, ActionKind::Gesture, sub, "Shift+LMB drag / Long press and drag".to_owned()),
            (REGION_SELECT_RECT_ARMED, ActionKind::Gesture, sub, "LMB drag".to_owned()),
            (REGION_CANCEL, ActionKind::Command, sub, "Esc".to_owned()),
            (PATCH_DRAG, ActionKind::Gesture, sub, "LMB drag".to_owned()),
            (PATCH_CANCEL, ActionKind::Command, sub, "Esc".to_owned()),
            (FRAME_MARKS_RECT_MODE, ActionKind::Mode, sub, ctrl.to_owned()),
            (FRAME_PAINT, ActionKind::Gesture, sub, "LMB drag".to_owned()),
            (FRAME_ERASE, ActionKind::Gesture, sub, "Shift+LMB drag / RMB drag".to_owned()),
            (FRAME_MARKS_RECT, ActionKind::Gesture, sub, format!("{ctrl}+LMB drag")),
            (FRAME_MOVE, ActionKind::Gesture, sub, "LMB drag".to_owned()),
            (FRAME_RESIZE, ActionKind::Gesture, sub, "LMB drag".to_owned()),
            (FRAME_UNDO_STROKE, ActionKind::Command, sub, format!("{ctrl}+Z")),
            (EDITOR_PAINT, ActionKind::Gesture, window, "LMB drag".to_owned()),
            (EDITOR_ERASE, ActionKind::Gesture, window, "Shift+LMB drag / RMB drag".to_owned()),
        ];
        let declared: Vec<ActionId> = declarations().actions.iter().map(|action| action.meta().id).collect();
        assert_eq!(declared, expected.iter().map(|(id, ..)| *id).collect::<Vec<_>>(), "declaration order");
        for (id, kind, scope, label) in expected {
            let spec = registry.spec(id).unwrap_or_else(|| panic!("{id:?} is declared"));
            assert_eq!(spec.kind(), kind, "{id:?}");
            assert_eq!(spec.meta().scope, scope, "{id:?}");
            assert_eq!(registry.label(id), label, "{id:?}");
        }
    }

    /// The defaults conflict with nothing in the studio's view, canvas and brush sets; the only
    /// overlaps are the intended cross-level shadows.
    #[test]
    fn the_defaults_have_no_conflict() {
        let registry = registry();
        assert!(registry.conflicts().conflicts.is_empty(), "{:?}", registry.conflicts().conflicts);
        let shadowed_by = |inner: ActionId| registry.shadows_for(inner).iter().map(|shadow| (shadow.inner, shadow.outer)).collect::<Vec<_>>();
        assert!(shadowed_by(FRAME_UNDO_STROKE).contains(&(FRAME_UNDO_STROKE, CLEAN_UNDO)), "the frame's Ctrl+Z shadows the clean undo");
        assert!(shadowed_by(STROKE).iter().any(|(_, outer)| *outer == view::ZOOM_DRAG), "Ctrl+drag (rect) shadows the Ctrl zoom drag");
    }

    /// Canvas-wide claims: every stroke is a `Drag`, every press action a `Click`, on
    /// `view.surface`.
    #[test]
    fn canvas_claims_are_surface_drags_and_clicks() {
        let registry = registry();
        let kinds = STROKE_ACTIONS.into_iter().map(|id| (id, GestureKind::Drag)).chain(PRESS_ACTIONS.into_iter().map(|id| (id, GestureKind::Click)));
        for (id, kind) in kinds {
            let Some(ActionSpec::Gesture(spec)) = registry.spec(id) else { panic!("{id:?} is a gesture") };
            assert_eq!(spec.target, view::SURFACE, "{id:?}");
            assert!(spec.meta.defaults.iter().all(|binding| matches!(binding, Binding::Gesture(g) if g.kind == kind)), "{id:?}");
        }
    }

    fn env(contexts: &[ContextId]) -> ActiveSet {
        ActiveSet { active_tab: Some(AppTab::Cleaning), contexts: contexts.iter().copied().collect(), pointer_on_view: true, view_focus: true, ..ActiveSet::default() }
    }

    fn surface_drag(registry: &InputRegistry, state: &MatchState, button: MouseButton, contexts: &[ContextId]) -> Option<ActionId> {
        surface_gesture(registry, state, button, GestureKind::Drag, contexts)
    }

    fn surface_gesture(registry: &InputRegistry, state: &MatchState, button: MouseButton, kind: GestureKind, contexts: &[ContextId]) -> Option<ActionId> {
        state.match_gesture(registry, &Button::Mouse(button), kind, &[view::SURFACE], &env(contexts)).map(|matched| matched.action)
    }

    fn held(buttons: &[Button]) -> MatchState {
        let mut state = MatchState::default();
        for button in buttons {
            state.step(&registry(), &ms_input::MatchEvent::Press { button: button.clone(), repeat: false }, &ActiveSet::default());
        }
        state
    }

    /// Per tool context, which canvas drag a press starts: the brush stroke (also under Ctrl: the
    /// rectangle), the zoom drag under Z in every tool, the region selection only under Shift
    /// unless armed, the patch drag.
    #[test]
    fn surface_drags_per_tool_context() {
        let registry = registry();
        let lmb = Button::Mouse(MouseButton::Left);
        let ctrl = Button::Modifier(ModifierKey::Command);
        let shift = Button::Modifier(ModifierKey::Shift);
        let z = Button::Key(egui::Key::Z);
        let brush = [TOOL_BRUSH, TOOL_ZAMAZKA, ms_tools::mask_brush::BRUSH_ACTIVE];
        assert_eq!(surface_drag(&registry, &held(std::slice::from_ref(&lmb)), MouseButton::Left, &brush), Some(STROKE));
        assert_eq!(surface_drag(&registry, &held(&[ctrl.clone(), lmb.clone()]), MouseButton::Left, &brush), Some(STROKE), "Ctrl+drag is the rectangle");
        assert_eq!(surface_drag(&registry, &held(&[shift.clone(), lmb.clone()]), MouseButton::Left, &brush), Some(STROKE), "Shift+drag erases");
        assert_eq!(surface_drag(&registry, &held(&[z.clone(), lmb.clone()]), MouseButton::Left, &brush), Some(view::ZOOM_DRAG), "Z+drag zooms");
        assert_eq!(surface_drag(&registry, &held(&[]), MouseButton::Right, &brush), None, "a right drag is no press action");
        assert_eq!(surface_gesture(&registry, &held(&[]), MouseButton::Right, GestureKind::Click, &brush), Some(PICK_COLOR));
        assert_eq!(surface_gesture(&registry, &held(&[]), MouseButton::Right, GestureKind::Click, &[TOOL_BRUSH, TOOL_STAMP]), Some(STAMP_SET_ANCHOR));

        let region = [REGION_SELECT];
        assert_eq!(surface_drag(&registry, &held(std::slice::from_ref(&lmb)), MouseButton::Left, &region), None, "a plain drag does not select");
        assert_eq!(surface_drag(&registry, &held(&[shift.clone(), lmb.clone()]), MouseButton::Left, &region), Some(REGION_SELECT_RECT));
        assert_eq!(surface_drag(&registry, &held(&[ctrl.clone(), lmb.clone()]), MouseButton::Left, &region), Some(view::ZOOM_DRAG));
        assert_eq!(surface_drag(&registry, &held(std::slice::from_ref(&lmb)), MouseButton::Left, &[REGION_SELECT, REGION_ARMED]), Some(REGION_SELECT_RECT_ARMED));
        assert_eq!(surface_drag(&registry, &held(&[z, lmb.clone()]), MouseButton::Left, &[REGION_SELECT, REGION_ARMED]), Some(view::ZOOM_DRAG));

        assert_eq!(surface_drag(&registry, &held(std::slice::from_ref(&lmb)), MouseButton::Left, &[TOOL_PATCH]), Some(PATCH_DRAG));
        assert_eq!(surface_drag(&registry, &held(&[lmb]), MouseButton::Left, &[REGION_FRAME]), None, "the frame host takes no canvas drag");
        assert_eq!(surface_drag(&registry, &held(&[ctrl]), MouseButton::Left, &[EDITOR_WINDOW]), Some(view::ZOOM_DRAG));
    }

    /// The labels snapshot is rebuilt only when the registry revision or the locale changes.
    #[test]
    fn labels_follow_the_registry_revision() {
        let _locale = english();
        let mut registry = registry();
        let mut cache = CleaningLabelsCache::default();
        let first = Arc::clone(cache.refresh(&registry));
        assert_eq!(first.erase_mode, "Shift");
        assert!(Arc::ptr_eq(&first, cache.refresh(&registry)), "same revision, same snapshot");
        if let Err(err) = registry.set_bindings(ERASE_MODE, vec![Binding::mode([Button::Key(egui::Key::E)])]) {
            panic!("a key is a valid mode chord: {err}");
        }
        let second = Arc::clone(cache.refresh(&registry));
        assert_eq!(second.erase_mode, "E");
        assert_eq!(second.frame_erase, "E+LMB drag / RMB drag", "a mode reference prints the mode's current chord");
    }
}
