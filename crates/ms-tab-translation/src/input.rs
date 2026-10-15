/*
FILE OVERVIEW: crates/ms-tab-translation/src/input.rs
Every input action of the «Перевод» tab as `ms_input` declarations, the contexts the tab
publishes, and the binding texts its hints show. The one owner of "which input does what" in this
crate: no other file reads a key, a modifier or the wheel.

Key items:
- action ids (`DELETE_BUBBLE`, `CREATE_BUBBLE`, ..., `OCR_QUICK_RECT`, `LINES_DELETE`, ...): the
  persistence keys of the user's overrides (`user_config` -> `Hotkeys`); never rename one.
- contexts (`LINES_EDIT`, `MASK_EDIT`, `ADV_REC_OPEN`, `ADV_REC_BRUSH`) and `contexts` (which of
  them, plus `ms_tools::mask_brush::BRUSH_ACTIVE`, hold for given tab facts).
- targets (`TARGET_LINE_HANDLE`, `TARGET_LINE`, `TARGET_LINES_PAGE`, `TARGET_MASK_PAGE`,
  `TARGET_ADV_REC_IMAGE`, `TARGET_BUBBLE_CARD`) of the tab's own targeted gestures.
- `declarations`: the whole set, in declaration order.
- `recent_char_rank`: the recent-character rank a command / gesture received as a parameter.
- `TranslationInputLabels`: this frame's binding texts (registry labels, alternatives joined by
  " / ") and the bottom-hint rows built from them.

Notes:
- Canvas-wide selections (quick / advanced OCR, image crop) are `Drag` gestures on
  `ms_input::view::SURFACE`: the router claims them at the press and the canvas raises its capture
  area; `tab/canvas_input.rs` drives them from `InputFrame::claimed`.
- Bubble commands carry the conditions that used to be the tab's "block canvas bubble hotkeys"
  check (detector edit modes, advanced recognition open); the router never fires them there.
*/

use ms_canvas::CanvasHintRow;
use ms_config::app_tab::AppTab;
use ms_input::{
    Accept, ActionId, ActionMeta, ActionSpec, Binding, Button, Chord, CommandSpec, Condition, ContextId, Declarations, GestureKind, GestureSpec,
    InputRegistry, ModeEffect, ModeSpec, ModifierKey, MouseButton, ReleasePolicy, Sample, Scope, TargetId, TargetSpec, TouchSource, view,
};
use ms_tools::mask_brush::BRUSH_ACTIVE;

/// Command: delete the selected bubble (default Delete).
pub const DELETE_BUBBLE: ActionId = ActionId("translation.bubble.delete_selected");
/// Command: create a text bubble under the pointer (default T); accepts a recent character.
pub const CREATE_BUBBLE: ActionId = ActionId("translation.bubble.create_at_cursor");
/// Command: create a hint bubble under the pointer (default H).
pub const CREATE_HINT_BUBBLE: ActionId = ActionId("translation.bubble.create_hint_at_cursor");
/// Command: create an empty external-image bubble under the pointer (default Q).
pub const CREATE_IMAGE_BUBBLE: ActionId = ActionId("translation.bubble.create_image_at_cursor");
/// Command: copy the selected bubble's original (unbound by default).
pub const COPY_BUBBLE_ORIGINAL: ActionId = ActionId("translation.bubble.copy_original");
/// Command: copy the selected bubble's translation (unbound by default).
pub const COPY_BUBBLE_TRANSLATION: ActionId = ActionId("translation.bubble.copy_translation");
/// Command: paste over the selected bubble's original (unbound by default).
pub const PASTE_BUBBLE_ORIGINAL: ActionId = ActionId("translation.bubble.paste_original");
/// Command: paste over the selected bubble's translation (unbound by default).
pub const PASTE_BUBBLE_TRANSLATION: ActionId = ActionId("translation.bubble.paste_translation");
/// Command: toggle the bubbles panel (default P).
pub const TOGGLE_BUBBLES_PANEL: ActionId = ActionId("translation.panel.bubbles.toggle");
/// Command: toggle the OCR panel (default S).
pub const TOGGLE_OCR_PANEL: ActionId = ActionId("translation.panel.ocr.toggle");
/// Command: toggle the composition panel (default K).
pub const TOGGLE_COMPOSITION_PANEL: ActionId = ActionId("translation.panel.composition.toggle");
/// Command: toggle the machine-translation panel (default M).
pub const TOGGLE_MT_PANEL: ActionId = ActionId("translation.panel.machine_translation.toggle");
/// Command: toggle the text-detector panel (default D).
pub const TOGGLE_DETECTOR_PANEL: ActionId = ActionId("translation.panel.detector.toggle");

/// Mode: quick OCR selection (default Shift).
pub const OCR_QUICK_MODE: ActionId = ActionId("translation.ocr.quick_mode");
/// Mode: advanced OCR selection (default Alt); beats the quick mode.
pub const OCR_ADVANCED_MODE: ActionId = ActionId("translation.ocr.advanced_mode");
/// Mode: image-bubble page-crop selection (default Shift+Q); beats both OCR modes.
pub const IMAGE_CROP_MODE: ActionId = ActionId("translation.image.crop_mode");
/// Parameter modes: the recent character of rank N (default digit N), N = 1..=6.
pub const RECENT_CHARS: [ActionId; 6] = [
    ActionId("translation.recent_char.1"),
    ActionId("translation.recent_char.2"),
    ActionId("translation.recent_char.3"),
    ActionId("translation.recent_char.4"),
    ActionId("translation.recent_char.5"),
    ActionId("translation.recent_char.6"),
];
/// Mode: a Translation brush (detector mask edit, advanced-recognition brush) erases instead of
/// painting (default Shift). Beats the quick OCR mode, so Shift in a brush never arms OCR.
pub const BRUSH_ERASE_MODE: ActionId = ActionId("translation.brush.erase_mode");

/// Gesture: quick OCR of a dragged rect (Shift + LMB drag).
pub const OCR_QUICK_RECT: ActionId = ActionId("translation.ocr.quick_rect");
/// Gesture: quick OCR of a rect dragged by a finger after a long press, with no key held. Its own
/// action (not an alternative of `OCR_QUICK_RECT`) because it has its own conditions: off in the
/// detector edit modes, where a finger long-press drag pans.
pub const OCR_QUICK_RECT_TOUCH: ActionId = ActionId("translation.ocr.quick_rect_touch");
/// Gesture: advanced recognition of a dragged rect (Alt + LMB drag).
pub const OCR_ADVANCED_RECT: ActionId = ActionId("translation.ocr.advanced_rect");
/// Gesture: page crop of an image bubble from a dragged rect (Shift+Q + LMB drag).
pub const IMAGE_CROP_RECT: ActionId = ActionId("translation.image.crop_rect");
/// Gesture: the bubble card context menu of the bubbles panel (RMB click).
pub const BUBBLE_CARD_MENU: ActionId = ActionId("translation.bubble_card.menu");

/// Command (lines edit): delete the selected detected line (default Delete).
pub const LINES_DELETE: ActionId = ActionId("translation.lines.delete");
/// Command (lines edit): add a line in the middle of the current page (default N).
pub const LINES_NEW: ActionId = ActionId("translation.lines.new");
/// Gesture (lines edit): resize the selected line by a handle (LMB drag).
pub const LINES_RESIZE: ActionId = ActionId("translation.lines.resize");
/// Gesture (lines edit): move a line (LMB drag; selects it).
pub const LINES_MOVE: ActionId = ActionId("translation.lines.move");
/// Gesture (lines edit): select a line (LMB click).
pub const LINES_SELECT: ActionId = ActionId("translation.lines.select");
/// Gesture (lines edit): clear the line selection (LMB click on the page outside every line).
pub const LINES_DESELECT: ActionId = ActionId("translation.lines.deselect");
/// Gesture (mask edit): paint the text mask (LMB drag).
pub const MASK_PAINT: ActionId = ActionId("translation.mask.paint");
/// Gesture (mask edit): erase the text mask (RMB drag, or LMB drag under the erase mode).
pub const MASK_ERASE: ActionId = ActionId("translation.mask.erase");
/// Gesture (advanced recognition, brush off): select an area for quick OCR (LMB drag).
pub const ADV_REC_SELECT_RECT: ActionId = ActionId("translation.adv_rec.select_rect");
/// Gesture (advanced recognition, brush on): paint OCR guides (LMB drag).
pub const ADV_REC_PAINT: ActionId = ActionId("translation.adv_rec.paint");
/// Gesture (advanced recognition, brush on): erase OCR guides (RMB drag, or LMB under the erase
/// mode).
pub const ADV_REC_ERASE: ActionId = ActionId("translation.adv_rec.erase");

/// Context: the text-detector lines edit mode is on.
pub const LINES_EDIT: ContextId = ContextId("translation.detector.lines_edit");
/// Context: the text-detector mask edit mode is on.
pub const MASK_EDIT: ContextId = ContextId("translation.detector.mask_edit");
/// Context: the advanced-recognition window is open (loading or showing a crop).
pub const ADV_REC_OPEN: ContextId = ContextId("translation.adv_rec.open");
/// Context: the advanced-recognition window's brush is on.
pub const ADV_REC_BRUSH: ContextId = ContextId("translation.adv_rec.brush");

/// Target: a handle of the selected detected line.
pub const TARGET_LINE_HANDLE: TargetId = TargetId("translation.line.handle");
/// Target: a detected line's rect.
pub const TARGET_LINE: TargetId = TargetId("translation.line");
/// Target: a page in the lines edit mode (outside every line).
pub const TARGET_LINES_PAGE: TargetId = TargetId("translation.lines.page");
/// Target: a page in the mask edit mode.
pub const TARGET_MASK_PAGE: TargetId = TargetId("translation.mask.page");
/// Target: the crop preview of the advanced-recognition window.
pub const TARGET_ADV_REC_IMAGE: TargetId = TargetId("translation.adv_rec.image");
/// Target: a card of the bubbles panel.
pub const TARGET_BUBBLE_CARD: TargetId = TargetId("translation.bubble_card");

/// The tab's targets with their priorities (a handle over the line it resizes, a line over the
/// page holding it).
const TARGETS: [(TargetId, u16); 6] = [
    (TARGET_LINE_HANDLE, 50),
    (TARGET_LINE, 40),
    (TARGET_BUBBLE_CARD, 30),
    (TARGET_MASK_PAGE, 20),
    (TARGET_ADV_REC_IMAGE, 20),
    (TARGET_LINES_PAGE, 5),
];

const SECTION_BUBBLES: &str = "app.hotkey.bubbles_category";
const SECTION_BUBBLE_TEXT: &str = "translation.tab.bubbles_title";
const SECTION_PANELS: &str = "translation.tab.hotkey_scope_panels";
const SECTION_OCR: &str = "app.hotkey.ocr_category";
const SECTION_RECENT: &str = "translation.input.section_recent_characters";
const SECTION_LINES: &str = "translation.input.section_lines_edit";
const SECTION_MASK: &str = "translation.input.section_mask_edit";
const SECTION_ADV_REC: &str = "translation.input.section_adv_rec";

/// The display titles of the recent-character modes, one literal key each (the i18n orphan test
/// needs every key spelled out).
const RECENT_CHAR_TITLES: [&str; 6] = [
    "translation.input.recent_char_1_title",
    "translation.input.recent_char_2_title",
    "translation.input.recent_char_3_title",
    "translation.input.recent_char_4_title",
    "translation.input.recent_char_5_title",
    "translation.input.recent_char_6_title",
];

/// The default digit of each recent-character rank.
const RECENT_CHAR_KEYS: [egui::Key; 6] = [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5, egui::Key::Num6];

const TR: Scope = Scope::Tab(AppTab::Translation);
const SUB: Scope = Scope::Sub(Some(AppTab::Translation));
const WINDOW: Scope = Scope::Window(Some(AppTab::Translation));

fn meta(id: ActionId, title_key: &'static str, section_key: &'static str, scope: Scope, when: Vec<Condition>, defaults: Vec<Binding>) -> ActionMeta {
    ActionMeta { id, title_key, section_key, scope, when, defaults }
}

/// The conditions every bubble command shares: never in a detector edit mode or while the
/// advanced-recognition window is open (those own the keyboard and the pointer).
fn bubble_gates() -> Vec<Condition> {
    vec![Condition::NotContext(LINES_EDIT), Condition::NotContext(MASK_EDIT), Condition::NotContext(ADV_REC_OPEN)]
}

fn key(key: egui::Key) -> Vec<Binding> {
    vec![Binding::command(Chord::new([Button::Key(key)]))]
}

fn command(meta: ActionMeta, accepts: Vec<Accept>) -> ActionSpec {
    ActionSpec::Command(CommandSpec { meta, accepts, repeat: false })
}

/// A mode of `meta` whose effect starts with a drag and whose release mid-gesture commits it.
fn mode(meta: ActionMeta, beats: Vec<ActionId>, param: Option<u8>) -> ActionSpec {
    ActionSpec::Mode(ModeSpec { meta, effect: ModeEffect::OnDragStart, on_release_mid_gesture: ReleasePolicy::Commit, beats, param })
}

fn gesture(meta: ActionMeta, target: TargetId, accepts: Vec<Accept>) -> ActionSpec {
    ActionSpec::Gesture(GestureSpec { meta, target, accepts })
}

fn left_drag(chord: Chord) -> Binding {
    Binding::gesture(chord, Button::Mouse(MouseButton::Left), GestureKind::Drag)
}

fn finger_drag(chord: Chord) -> Binding {
    Binding::gesture(chord, Button::Touch(TouchSource::LongPressDrag), GestureKind::Drag)
}

fn under(modes: &[ActionId]) -> Chord {
    Chord::default().with_modes(modes.iter().copied())
}

/// The recent-character parameter modes as accepted parameters sampled `sample`.
fn recent_char_accepts(sample: Sample) -> Vec<Accept> {
    RECENT_CHARS.iter().map(|&mode| Accept { mode, sample }).collect()
}

/// Every input action of the Translation tab, its targets and its exclusive context group, in
/// declaration order: bubble commands, panel toggles, selection and parameter modes, canvas-wide
/// selections and the card menu, the brush erase mode, the lines / mask edit set, the
/// advanced-recognition window set.
#[must_use]
pub fn declarations() -> Declarations {
    let shift = Button::Modifier(ModifierKey::Shift);
    let alt = Button::Modifier(ModifierKey::Alt);
    let left_click = || vec![Binding::gesture(Chord::default(), Button::Mouse(MouseButton::Left), GestureKind::Click)];
    // A brush paints on a drag and dabs on a click (the dab sits at the press point).
    let paint_strokes = || vec![left_drag(Chord::default()), Binding::gesture(Chord::default(), Button::Mouse(MouseButton::Left), GestureKind::Click)];
    let erase_strokes = || {
        vec![
            Binding::gesture(Chord::default(), Button::Mouse(MouseButton::Right), GestureKind::Drag),
            Binding::gesture(Chord::default(), Button::Mouse(MouseButton::Right), GestureKind::Click),
            left_drag(under(&[BRUSH_ERASE_MODE])),
            Binding::gesture(under(&[BRUSH_ERASE_MODE]), Button::Mouse(MouseButton::Left), GestureKind::Click),
        ]
    };
    let lines = || vec![Condition::Context(LINES_EDIT)];
    let mask = || vec![Condition::Context(MASK_EDIT)];
    let mut actions = vec![
        command(meta(DELETE_BUBBLE, "app.hotkey.delete_bubble", SECTION_BUBBLES, TR, bubble_gates(), key(egui::Key::Delete)), Vec::new()),
        command(meta(CREATE_BUBBLE, "app.hotkey.create_bubble", SECTION_BUBBLES, TR, bubble_gates(), key(egui::Key::T)), recent_char_accepts(Sample::AtPress)),
        command(meta(CREATE_HINT_BUBBLE, "app.hotkey.create_hint_bubble", SECTION_BUBBLES, TR, bubble_gates(), key(egui::Key::H)), Vec::new()),
        command(meta(CREATE_IMAGE_BUBBLE, "translation.input.create_image_title", SECTION_BUBBLES, TR, bubble_gates(), key(egui::Key::Q)), Vec::new()),
        command(meta(COPY_BUBBLE_ORIGINAL, "translation.tab.hotkey_copy_original", SECTION_BUBBLE_TEXT, TR, bubble_gates(), Vec::new()), Vec::new()),
        command(meta(COPY_BUBBLE_TRANSLATION, "translation.tab.hotkey_copy_translation", SECTION_BUBBLE_TEXT, TR, bubble_gates(), Vec::new()), Vec::new()),
        command(meta(PASTE_BUBBLE_ORIGINAL, "translation.tab.hotkey_paste_original", SECTION_BUBBLE_TEXT, TR, bubble_gates(), Vec::new()), Vec::new()),
        command(meta(PASTE_BUBBLE_TRANSLATION, "translation.tab.hotkey_paste_translation", SECTION_BUBBLE_TEXT, TR, bubble_gates(), Vec::new()), Vec::new()),
        command(meta(TOGGLE_BUBBLES_PANEL, "translation.tab.hotkey_open_bubbles", SECTION_PANELS, TR, Vec::new(), key(egui::Key::P)), Vec::new()),
        command(meta(TOGGLE_OCR_PANEL, "translation.tab.hotkey_open_ocr", SECTION_PANELS, TR, Vec::new(), key(egui::Key::S)), Vec::new()),
        command(meta(TOGGLE_COMPOSITION_PANEL, "translation.tab.hotkey_open_composition", SECTION_PANELS, TR, Vec::new(), key(egui::Key::K)), Vec::new()),
        command(meta(TOGGLE_MT_PANEL, "translation.tab.hotkey_open_mt", SECTION_PANELS, TR, Vec::new(), key(egui::Key::M)), Vec::new()),
        command(meta(TOGGLE_DETECTOR_PANEL, "translation.tab.hotkey_open_text_detector", SECTION_PANELS, TR, Vec::new(), key(egui::Key::D)), Vec::new()),
        mode(meta(OCR_QUICK_MODE, "translation.tab.hotkey_quick_recognition", SECTION_OCR, TR, Vec::new(), vec![Binding::mode([shift.clone()])]), Vec::new(), None),
        mode(meta(OCR_ADVANCED_MODE, "translation.tab.hotkey_advanced_recognition", SECTION_OCR, TR, Vec::new(), vec![Binding::mode([alt])]), vec![OCR_QUICK_MODE], None),
        mode(
            meta(IMAGE_CROP_MODE, "translation.input.crop_mode_title", SECTION_BUBBLES, TR, Vec::new(), vec![Binding::mode([shift.clone(), Button::Key(egui::Key::Q)])]),
            vec![OCR_QUICK_MODE, OCR_ADVANCED_MODE],
            None,
        ),
    ];
    for (rank, ((id, title_key), digit)) in RECENT_CHARS.iter().zip(RECENT_CHAR_TITLES).zip(RECENT_CHAR_KEYS).enumerate() {
        // `rank` < 6 (three arrays of six), so the parameter is 1..=6 and always fits.
        let param = u8::try_from(rank + 1).ok();
        actions.push(mode(meta(*id, title_key, SECTION_RECENT, TR, Vec::new(), vec![Binding::mode([Button::Key(digit)])]), Vec::new(), param));
    }
    let latest_recent_char = || recent_char_accepts(Sample::LatestDuringGesture);
    actions.extend([
        gesture(
            meta(
                OCR_QUICK_RECT,
                "translation.input.quick_rect_title",
                SECTION_OCR,
                TR,
                vec![Condition::NotContext(ADV_REC_OPEN), Condition::NotContext(MASK_EDIT)],
                vec![left_drag(under(&[OCR_QUICK_MODE]))],
            ),
            view::SURFACE,
            latest_recent_char(),
        ),
        gesture(
            meta(
                OCR_QUICK_RECT_TOUCH,
                "translation.input.quick_rect_touch_title",
                SECTION_OCR,
                TR,
                vec![Condition::NotContext(ADV_REC_OPEN), Condition::NotContext(MASK_EDIT), Condition::NotContext(LINES_EDIT)],
                vec![finger_drag(Chord::default())],
            ),
            view::SURFACE,
            latest_recent_char(),
        ),
        gesture(
            meta(
                OCR_ADVANCED_RECT,
                "translation.input.advanced_rect_title",
                SECTION_OCR,
                TR,
                vec![Condition::NotContext(ADV_REC_OPEN)],
                vec![left_drag(under(&[OCR_ADVANCED_MODE])), finger_drag(under(&[OCR_ADVANCED_MODE]))],
            ),
            view::SURFACE,
            latest_recent_char(),
        ),
        gesture(
            meta(IMAGE_CROP_RECT, "translation.input.crop_rect_title", SECTION_BUBBLES, TR, Vec::new(), vec![left_drag(under(&[IMAGE_CROP_MODE])), finger_drag(under(&[IMAGE_CROP_MODE]))]),
            view::SURFACE,
            latest_recent_char(),
        ),
        gesture(
            meta(
                BUBBLE_CARD_MENU,
                "translation.input.bubble_card_menu_title",
                SECTION_BUBBLE_TEXT,
                TR,
                Vec::new(),
                vec![Binding::gesture(Chord::default(), Button::Mouse(MouseButton::Right), GestureKind::Click)],
            ),
            TARGET_BUBBLE_CARD,
            Vec::new(),
        ),
        mode(
            meta(BRUSH_ERASE_MODE, "translation.input.erase_mode_title", SECTION_MASK, SUB, vec![Condition::Context(BRUSH_ACTIVE)], vec![Binding::mode([shift])]),
            vec![OCR_QUICK_MODE],
            None,
        ),
        command(meta(LINES_DELETE, "translation.input.lines_delete_title", SECTION_LINES, SUB, lines(), key(egui::Key::Delete)), Vec::new()),
        command(meta(LINES_NEW, "translation.input.lines_new_title", SECTION_LINES, SUB, lines(), key(egui::Key::N)), Vec::new()),
        gesture(meta(LINES_RESIZE, "translation.input.lines_resize_title", SECTION_LINES, SUB, lines(), vec![left_drag(Chord::default())]), TARGET_LINE_HANDLE, Vec::new()),
        gesture(meta(LINES_MOVE, "translation.input.lines_move_title", SECTION_LINES, SUB, lines(), vec![left_drag(Chord::default())]), TARGET_LINE, Vec::new()),
        gesture(meta(LINES_SELECT, "translation.input.lines_select_title", SECTION_LINES, SUB, lines(), left_click()), TARGET_LINE, Vec::new()),
        gesture(meta(LINES_DESELECT, "translation.input.lines_deselect_title", SECTION_LINES, SUB, lines(), left_click()), TARGET_LINES_PAGE, Vec::new()),
        gesture(meta(MASK_PAINT, "translation.input.mask_paint_title", SECTION_MASK, SUB, mask(), paint_strokes()), TARGET_MASK_PAGE, Vec::new()),
        gesture(meta(MASK_ERASE, "translation.input.mask_erase_title", SECTION_MASK, SUB, mask(), erase_strokes()), TARGET_MASK_PAGE, Vec::new()),
        gesture(
            meta(
                ADV_REC_SELECT_RECT,
                "translation.input.adv_rec_select_title",
                SECTION_ADV_REC,
                WINDOW,
                vec![Condition::Context(ADV_REC_OPEN), Condition::NotContext(ADV_REC_BRUSH)],
                vec![left_drag(Chord::default())],
            ),
            TARGET_ADV_REC_IMAGE,
            Vec::new(),
        ),
        gesture(
            meta(
                ADV_REC_PAINT,
                "translation.input.adv_rec_paint_title",
                SECTION_ADV_REC,
                WINDOW,
                vec![Condition::Context(ADV_REC_OPEN), Condition::Context(ADV_REC_BRUSH)],
                paint_strokes(),
            ),
            TARGET_ADV_REC_IMAGE,
            Vec::new(),
        ),
        gesture(
            meta(ADV_REC_ERASE, "translation.input.adv_rec_erase_title", SECTION_ADV_REC, WINDOW, vec![Condition::Context(ADV_REC_OPEN), Condition::Context(ADV_REC_BRUSH)], erase_strokes()),
            TARGET_ADV_REC_IMAGE,
            Vec::new(),
        ),
    ]);
    let targets = TARGETS.iter().map(|&(id, priority)| TargetSpec { id, priority, canvas_wide: false }).collect();
    // The two detector edit modes exclude each other (`set_text_detector_edit_*_mode`).
    Declarations { actions, targets, fixed: Vec::new(), exclusive_contexts: vec![vec![LINES_EDIT, MASK_EDIT]] }
}

/// The tab facts the published contexts follow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextFacts {
    pub lines_edit: bool,
    pub mask_edit: bool,
    pub adv_rec_open: bool,
    pub adv_rec_brush: bool,
}

/// The contexts the tab publishes for `facts`, in a fixed order: the detector edit mode, the
/// advanced-recognition window (and its brush), then `BRUSH_ACTIVE` while either brush is on.
#[must_use]
pub fn contexts(facts: ContextFacts) -> Vec<ContextId> {
    let mut contexts = Vec::with_capacity(4);
    if facts.lines_edit {
        contexts.push(LINES_EDIT);
    }
    if facts.mask_edit {
        contexts.push(MASK_EDIT);
    }
    if facts.adv_rec_open {
        contexts.push(ADV_REC_OPEN);
    }
    if facts.adv_rec_brush {
        contexts.push(ADV_REC_BRUSH);
    }
    if facts.mask_edit || facts.adv_rec_brush {
        contexts.push(BRUSH_ACTIVE);
    }
    contexts
}

/// The recent-character rank (1..=6) among the accepted `params` of a fired command or claimed
/// gesture: the lowest held rank, `None` with no digit held.
#[must_use]
pub fn recent_char_rank(params: &[ActionId]) -> Option<usize> {
    RECENT_CHARS.iter().position(|id| params.contains(id)).map(|idx| idx + 1)
}

/// This frame's binding texts of the tab's hints, from the registry (alternatives joined by
/// " / ", "not assigned" when unbound). Rebuilt every frame by the host: a dozen short strings,
/// no locale or revision staleness to manage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranslationInputLabels {
    pub create_bubble: String,
    /// The recent-character ranks as a group ("1-6" by default).
    pub recent_chars: String,
    /// The binding of each recent-character rank, rank 1 first (the recent-character cards).
    pub recent_char_keys: Vec<String>,
    pub create_hint_bubble: String,
    pub create_image_bubble: String,
    pub delete_bubble: String,
    pub quick_rect: String,
    pub advanced_rect: String,
    pub crop_rect: String,
    pub lines_delete: String,
    pub lines_new: String,
    pub mask_paint: String,
    pub mask_erase: String,
    pub adv_rec_select: String,
    pub adv_rec_paint: String,
    pub adv_rec_erase: String,
    /// Panel toggles: `None` while unbound, so the panel title shows no "[...]" suffix.
    pub bubbles_panel: Option<String>,
    pub ocr_panel: Option<String>,
    pub composition_panel: Option<String>,
    pub machine_translation_panel: Option<String>,
    pub text_detector_panel: Option<String>,
}

impl TranslationInputLabels {
    /// Reads every label from `registry`.
    #[must_use]
    pub fn from_registry(registry: &InputRegistry) -> Self {
        let bound = |id: ActionId| (!registry.bindings(id).is_empty()).then(|| registry.label(id));
        // Quick OCR is two actions (mouse, finger); its hints name both, joined like alternatives.
        let quick: Vec<String> = [OCR_QUICK_RECT, OCR_QUICK_RECT_TOUCH].into_iter().filter_map(bound).collect();
        let quick_rect = if quick.is_empty() { registry.label(OCR_QUICK_RECT) } else { quick.join(ms_input::label::ALTERNATIVE_SEPARATOR) };
        Self {
            create_bubble: registry.label(CREATE_BUBBLE),
            recent_chars: registry.param_group_label(&RECENT_CHARS),
            recent_char_keys: RECENT_CHARS.iter().map(|&id| registry.label(id)).collect(),
            create_hint_bubble: registry.label(CREATE_HINT_BUBBLE),
            create_image_bubble: registry.label(CREATE_IMAGE_BUBBLE),
            delete_bubble: registry.label(DELETE_BUBBLE),
            quick_rect,
            advanced_rect: registry.label(OCR_ADVANCED_RECT),
            crop_rect: registry.label(IMAGE_CROP_RECT),
            lines_delete: registry.label(LINES_DELETE),
            lines_new: registry.label(LINES_NEW),
            mask_paint: registry.label(MASK_PAINT),
            mask_erase: registry.label(MASK_ERASE),
            adv_rec_select: registry.label(ADV_REC_SELECT_RECT),
            adv_rec_paint: registry.label(ADV_REC_PAINT),
            adv_rec_erase: registry.label(ADV_REC_ERASE),
            bubbles_panel: bound(TOGGLE_BUBBLES_PANEL),
            ocr_panel: bound(TOGGLE_OCR_PANEL),
            composition_panel: bound(TOGGLE_COMPOSITION_PANEL),
            machine_translation_panel: bound(TOGGLE_MT_PANEL),
            text_detector_panel: bound(TOGGLE_DETECTOR_PANEL),
        }
    }

    /// The Translation bottom-hint rows (`canvas.bottom_hint.translation.*`), every key text from
    /// these labels. `lines_edit` adds the two line-edit commands at the end. No row carries help,
    /// so the hint stays two-column.
    #[must_use]
    pub fn hint_rows(&self, lines_edit: bool) -> Vec<CanvasHintRow> {
        let with_recent = |keys: &str| tf!("canvas.bottom_hint.translation.with_recent_char_keys", digits = self.recent_chars, keys = keys);
        let mut rows = vec![
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_bubble_label"), self.create_bubble.clone()),
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_bubble_recent_char_label"), with_recent(&self.create_bubble)),
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.ocr_bubble_label"), self.quick_rect.clone()),
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.ocr_bubble_recent_char_label"), with_recent(&self.quick_rect)),
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.advanced_ocr_label"), self.advanced_rect.clone()),
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_image_bubble_label"), self.create_image_bubble.clone()),
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_image_bubble_crop_label"), self.crop_rect.clone()),
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_hint_bubble_label"), self.create_hint_bubble.clone()),
            CanvasHintRow::new(t!("canvas.bottom_hint.translation.delete_bubble_label"), self.delete_bubble.clone()),
        ];
        if lines_edit {
            rows.push(CanvasHintRow::new(t!("canvas.bottom_hint.translation.delete_line_label"), self.lines_delete.clone()));
            rows.push(CanvasHintRow::new(t!("canvas.bottom_hint.translation.new_line_label"), self.lines_new.clone()));
        }
        rows
    }
}

#[cfg(test)]
mod tests;
