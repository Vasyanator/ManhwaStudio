//! Tests of the Translation input set: the declarations golden (ids are persistence keys of the
//! user's overrides), its validity and conflict-freedom next to every other declaration the
//! Translation canvas meets, the published contexts per tab state, the recent-character
//! parameter and the label-built hint rows.

use ms_config::app_tab::AppTab;
use ms_input::{ActionId, ActionKind, Declarations, InputRegistry, Scope, view};

use super::{
    ADV_REC_BRUSH, ADV_REC_OPEN, BRUSH_ERASE_MODE, ContextFacts, LINES_EDIT, MASK_EDIT, OCR_QUICK_MODE, RECENT_CHARS, TranslationInputLabels, contexts,
    declarations, recent_char_rank,
};
use ms_tools::mask_brush::BRUSH_ACTIVE;

const TR: Scope = Scope::Tab(AppTab::Translation);
const SUB: Scope = Scope::Sub(Some(AppTab::Translation));
const WINDOW: Scope = Scope::Window(Some(AppTab::Translation));

/// The Translation set together with everything else declared where the Translation canvas
/// runs: the shared view set, the canvas set and the shared brush set.
fn studio_like_registry() -> InputRegistry {
    let mut all: Declarations = view::declarations();
    all.extend(ms_canvas::input::declarations());
    all.extend(ms_tools::mask_brush::declarations());
    all.extend(declarations());
    InputRegistry::new(all).expect("the Translation declarations form a valid registry with the view, canvas and brush sets")
}

/// The golden: id, kind, scope and default label of every Translation action, in declaration
/// order. Ids are persistence keys: a change here is a change of the user's stored overrides.
#[test]
fn declarations_golden() {
    use ActionKind::{Command, Gesture, Mode};
    let _locale = crate::english_locale_for_test();
    let registry = studio_like_registry();
    let expected: Vec<(&str, ActionKind, Scope, &str)> = vec![
        ("translation.bubble.delete_selected", Command, TR, "Del"),
        ("translation.bubble.create_at_cursor", Command, TR, "T"),
        ("translation.bubble.create_hint_at_cursor", Command, TR, "H"),
        ("translation.bubble.create_image_at_cursor", Command, TR, "Q"),
        ("translation.bubble.copy_original", Command, TR, "no binding"),
        ("translation.bubble.copy_translation", Command, TR, "no binding"),
        ("translation.bubble.paste_original", Command, TR, "no binding"),
        ("translation.bubble.paste_translation", Command, TR, "no binding"),
        ("translation.panel.bubbles.toggle", Command, TR, "P"),
        ("translation.panel.ocr.toggle", Command, TR, "S"),
        ("translation.panel.composition.toggle", Command, TR, "K"),
        ("translation.panel.machine_translation.toggle", Command, TR, "M"),
        ("translation.panel.detector.toggle", Command, TR, "D"),
        ("translation.ocr.quick_mode", Mode, TR, "Shift"),
        ("translation.ocr.advanced_mode", Mode, TR, "Alt"),
        ("translation.image.crop_mode", Mode, TR, "Shift+Q"),
        ("translation.recent_char.1", Mode, TR, "1"),
        ("translation.recent_char.2", Mode, TR, "2"),
        ("translation.recent_char.3", Mode, TR, "3"),
        ("translation.recent_char.4", Mode, TR, "4"),
        ("translation.recent_char.5", Mode, TR, "5"),
        ("translation.recent_char.6", Mode, TR, "6"),
        ("translation.ocr.quick_rect", Gesture, TR, "Shift+LMB drag"),
        ("translation.ocr.quick_rect_touch", Gesture, TR, "Long press and drag"),
        ("translation.ocr.advanced_rect", Gesture, TR, "Alt+LMB drag / Alt+Long press and drag"),
        ("translation.image.crop_rect", Gesture, TR, "Shift+Q+LMB drag / Shift+Q+Long press and drag"),
        ("translation.bubble_card.menu", Gesture, TR, "RMB click"),
        ("translation.brush.erase_mode", Mode, SUB, "Shift"),
        ("translation.lines.delete", Command, SUB, "Del"),
        ("translation.lines.new", Command, SUB, "N"),
        ("translation.lines.resize", Gesture, SUB, "LMB drag"),
        ("translation.lines.move", Gesture, SUB, "LMB drag"),
        ("translation.lines.select", Gesture, SUB, "LMB click"),
        ("translation.lines.deselect", Gesture, SUB, "LMB click"),
        ("translation.mask.paint", Gesture, SUB, "LMB drag / LMB click"),
        ("translation.mask.erase", Gesture, SUB, "RMB drag / RMB click / Shift+LMB drag / Shift+LMB click"),
        ("translation.adv_rec.select_rect", Gesture, WINDOW, "LMB drag"),
        ("translation.adv_rec.paint", Gesture, WINDOW, "LMB drag / LMB click"),
        ("translation.adv_rec.erase", Gesture, WINDOW, "RMB drag / RMB click / Shift+LMB drag / Shift+LMB click"),
    ];
    let actual: Vec<(&str, ActionKind, Scope, String)> =
        declarations().actions.iter().map(|spec| (spec.meta().id.0, spec.kind(), spec.meta().scope, registry.label(spec.meta().id))).collect();
    let expected: Vec<(&str, ActionKind, Scope, String)> = expected.into_iter().map(|(id, kind, scope, label)| (id, kind, scope, label.to_owned())).collect();
    assert_eq!(actual, expected);
}

/// No default binding of the Translation set conflicts with another one on its level, also next
/// to the view, canvas and brush sets. The brush erase mode sharing Shift with the quick OCR mode
/// is a cross-level shadow (information), never a conflict.
#[test]
fn the_defaults_are_conflict_free() {
    let registry = studio_like_registry();
    assert!(registry.conflicts().conflicts.is_empty(), "the default bindings conflict: {:?}", registry.conflicts().conflicts);
}

/// The brush erase mode beats the quick OCR mode (Shift in a brush erases, it never arms OCR);
/// the advanced mode beats quick; the crop chord beats both.
#[test]
fn mode_precedence_is_declared() {
    let beats = |id: ActionId| -> Vec<ActionId> {
        match declarations().actions.into_iter().find(|spec| spec.meta().id == id) {
            Some(ms_input::ActionSpec::Mode(mode)) => mode.beats,
            other => panic!("{id:?} is not a declared mode: {other:?}"),
        }
    };
    assert_eq!(beats(BRUSH_ERASE_MODE), vec![OCR_QUICK_MODE]);
    assert_eq!(beats(super::OCR_ADVANCED_MODE), vec![OCR_QUICK_MODE]);
    assert_eq!(beats(super::IMAGE_CROP_MODE), vec![OCR_QUICK_MODE, super::OCR_ADVANCED_MODE]);
    let params: Vec<Option<u8>> = RECENT_CHARS
        .iter()
        .map(|id| match declarations().actions.into_iter().find(|spec| spec.meta().id == *id) {
            Some(ms_input::ActionSpec::Mode(mode)) => mode.param,
            other => panic!("{id:?} is not a declared mode: {other:?}"),
        })
        .collect();
    assert_eq!(params, (1..=6).map(Some).collect::<Vec<_>>());
}

/// Every tab state publishes exactly its contexts, `BRUSH_ACTIVE` while either brush is on.
#[test]
fn contexts_follow_the_tab_state() {
    assert!(contexts(ContextFacts::default()).is_empty());
    assert_eq!(contexts(ContextFacts { lines_edit: true, ..ContextFacts::default() }), vec![LINES_EDIT]);
    assert_eq!(contexts(ContextFacts { mask_edit: true, ..ContextFacts::default() }), vec![MASK_EDIT, BRUSH_ACTIVE]);
    assert_eq!(contexts(ContextFacts { adv_rec_open: true, ..ContextFacts::default() }), vec![ADV_REC_OPEN]);
    assert_eq!(contexts(ContextFacts { adv_rec_open: true, adv_rec_brush: true, ..ContextFacts::default() }), vec![ADV_REC_OPEN, ADV_REC_BRUSH, BRUSH_ACTIVE]);
}

/// The finger quick OCR is off in both detector edit modes (a finger long-press drag pans there)
/// and with the advanced window open; the mouse quick OCR only in the mask edit and that window.
#[test]
fn finger_quick_ocr_yields_to_the_edit_modes() {
    let when = |id: ActionId| declarations().actions.into_iter().find(|spec| spec.meta().id == id).map(|spec| spec.meta().when.clone()).expect("declared");
    let finger = when(super::OCR_QUICK_RECT_TOUCH);
    for context in [LINES_EDIT, MASK_EDIT, ADV_REC_OPEN] {
        assert!(finger.contains(&ms_input::Condition::NotContext(context)), "{context:?}");
    }
    assert!(!when(super::OCR_QUICK_RECT).contains(&ms_input::Condition::NotContext(LINES_EDIT)));
}

/// The rank is the lowest recent-character parameter received; other parameters are ignored.
#[test]
fn recent_char_rank_reads_the_lowest_held_rank() {
    assert_eq!(recent_char_rank(&[]), None);
    assert_eq!(recent_char_rank(&[RECENT_CHARS[0]]), Some(1));
    assert_eq!(recent_char_rank(&[RECENT_CHARS[5]]), Some(6));
    assert_eq!(recent_char_rank(&[RECENT_CHARS[4], RECENT_CHARS[2]]), Some(3));
    assert_eq!(recent_char_rank(&[OCR_QUICK_MODE]), None);
}

/// The hint rows spell no binding of their own: every key text comes from the registry, the
/// recent-character group reads "1-6" (all six ranks), and the lines edit adds its two rows.
#[test]
fn hint_rows_are_built_from_the_registry() {
    let _locale = crate::english_locale_for_test();
    let labels = TranslationInputLabels::from_registry(&studio_like_registry());
    assert_eq!(labels.recent_chars, "1-6");
    assert_eq!(labels.recent_char_keys, ["1", "2", "3", "4", "5", "6"]);
    assert_eq!(labels.ocr_panel.as_deref(), Some("S"));
    let keys = |rows: &[ms_canvas::CanvasHintRow]| rows.iter().map(|row| row.keys.clone()).collect::<Vec<_>>();
    let rows = labels.hint_rows(false);
    assert_eq!(
        keys(&rows),
        [
            "T",
            "1-6+T",
            "Shift+LMB drag / Long press and drag",
            "1-6+Shift+LMB drag / 1-6+Long press and drag",
            "Alt+LMB drag / Alt+Long press and drag",
            "Q",
            "Shift+Q+LMB drag / Shift+Q+Long press and drag",
            "H",
            "Del",
        ]
    );
    let lines_rows = labels.hint_rows(true);
    assert_eq!(keys(&lines_rows[rows.len()..]), ["Del", "N"]);
}

/// An unbound panel toggle shows no binding suffix (its label is `None`), every other label the
/// registry's "not assigned" text.
#[test]
fn unbound_actions_read_as_such() {
    let _locale = crate::english_locale_for_test();
    let mut registry = studio_like_registry();
    registry.set_bindings(super::TOGGLE_OCR_PANEL, Vec::new()).expect("a panel toggle may be unbound");
    registry.set_bindings(super::CREATE_IMAGE_BUBBLE, Vec::new()).expect("a command may be unbound");
    let labels = TranslationInputLabels::from_registry(&registry);
    assert_eq!(labels.ocr_panel, None);
    assert_eq!(labels.create_image_bubble, "no binding");
}
