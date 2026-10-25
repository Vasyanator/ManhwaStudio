//! Tests of the Translation canvas drivers: the claimed rect selection step (start on a page,
//! follow, commit / cancel, recent-character sampling), the 4x4 minimum, the lines edit hits,
//! the line move / resize geometry, and the contexts the tab state publishes per edit mode.

use eframe::egui::{Rect, pos2};
use ms_input::{ActionId, Button, ClaimedGesture, GestureKind, GesturePhase, MouseButton, ReleasePolicy, ViewId, view};

use super::{
    CanvasSelection, CanvasSelectionKind, LineEditHit, SelectionStep, advance_selection, is_committable, line_edit_hits, move_text_detector_rect,
    page_uv, resize_text_detector_rect,
};
use crate::input::{ADV_REC_OPEN, IMAGE_CROP_RECT, LINES_EDIT, MASK_EDIT, OCR_ADVANCED_RECT, OCR_QUICK_RECT, RECENT_CHARS};
use crate::tab::TranslationTabState;
use crate::text_detector::TextDetectorRect;

/// A claim of `action` on a test view in `phase`, pressed at (10, 10), now at `current`.
fn claim(action: ActionId, phase: GesturePhase, current: (f32, f32), params: Vec<ActionId>) -> ClaimedGesture {
    ClaimedGesture {
        action,
        target: view::SURFACE,
        view: ViewId(egui::Id::new("test.view")),
        phase,
        origin: pos2(10.0, 10.0),
        current: pos2(current.0, current.1),
        params,
        button: Button::Mouse(MouseButton::Left),
        kind: GestureKind::Drag,
    }
}

#[test]
fn a_claimed_selection_starts_follows_and_commits() {
    let mut slot = None;
    let started = claim(OCR_QUICK_RECT, GesturePhase::Started, (10.0, 10.0), vec![RECENT_CHARS[2]]);
    assert_eq!(advance_selection(&mut slot, Some(&started), true), SelectionStep::Started);
    assert_eq!(slot.map(|selection| (selection.kind, selection.recent_character_rank)), Some((CanvasSelectionKind::QuickOcr, Some(3))));

    // A released digit keeps the last rank; a newly held one replaces it.
    let moved = claim(OCR_QUICK_RECT, GesturePhase::Active, (40.0, 30.0), Vec::new());
    assert_eq!(advance_selection(&mut slot, Some(&moved), true), SelectionStep::Running);
    assert_eq!(slot.and_then(|selection| selection.recent_character_rank), Some(3));
    let digit = claim(OCR_QUICK_RECT, GesturePhase::Active, (50.0, 40.0), vec![RECENT_CHARS[0]]);
    assert_eq!(advance_selection(&mut slot, Some(&digit), true), SelectionStep::Running);
    assert_eq!(slot.and_then(|selection| selection.recent_character_rank), Some(1));

    let released = claim(OCR_QUICK_RECT, GesturePhase::Finished(ReleasePolicy::Commit), (60.0, 70.0), Vec::new());
    let SelectionStep::Commit(selection) = advance_selection(&mut slot, Some(&released), true) else {
        panic!("a release commits the selection");
    };
    assert_eq!(selection.rect(), Rect::from_two_pos(pos2(10.0, 10.0), pos2(60.0, 70.0)));
    assert_eq!(selection.recent_character_rank, Some(1));
    assert!(slot.is_none());
}

#[test]
fn a_selection_off_the_pages_never_starts() {
    let mut slot = None;
    let started = claim(OCR_ADVANCED_RECT, GesturePhase::Started, (10.0, 10.0), Vec::new());
    assert_eq!(advance_selection(&mut slot, Some(&started), false), SelectionStep::Idle);
    let moved = claim(OCR_ADVANCED_RECT, GesturePhase::Active, (40.0, 40.0), Vec::new());
    assert_eq!(advance_selection(&mut slot, Some(&moved), true), SelectionStep::Idle, "a refused start stays refused for the whole gesture");
    let released = claim(OCR_ADVANCED_RECT, GesturePhase::Finished(ReleasePolicy::Commit), (40.0, 40.0), Vec::new());
    assert_eq!(advance_selection(&mut slot, Some(&released), true), SelectionStep::Idle);
}

#[test]
fn a_cancelled_or_vanished_claim_drops_the_selection() {
    for end in [Some(claim(IMAGE_CROP_RECT, GesturePhase::Finished(ReleasePolicy::Cancel), (50.0, 50.0), Vec::new())), None] {
        let mut slot = None;
        let started = claim(IMAGE_CROP_RECT, GesturePhase::Started, (10.0, 10.0), Vec::new());
        assert_eq!(advance_selection(&mut slot, Some(&started), true), SelectionStep::Started);
        assert_eq!(slot.map(|selection| selection.kind), Some(CanvasSelectionKind::ImageCrop));
        assert_eq!(advance_selection(&mut slot, end.as_ref(), true), SelectionStep::Dropped);
        assert!(slot.is_none());
    }
}

/// A claim of another action on the view (the shared zoom drag) is not a selection.
#[test]
fn a_zoom_drag_claim_is_not_a_selection() {
    let mut slot = None;
    let zoom = claim(view::ZOOM_DRAG, GesturePhase::Started, (10.0, 10.0), Vec::new());
    assert_eq!(advance_selection(&mut slot, Some(&zoom), true), SelectionStep::Idle);
    assert!(slot.is_none());
    assert_eq!(CanvasSelectionKind::for_action(view::ZOOM_DRAG), None);
}

#[test]
fn only_a_rect_of_at_least_four_points_is_committed() {
    assert!(is_committable(Rect::from_min_max(pos2(0.0, 0.0), pos2(4.0, 4.0))));
    assert!(!is_committable(Rect::from_min_max(pos2(0.0, 0.0), pos2(3.9, 40.0))));
    assert!(!is_committable(Rect::from_min_max(pos2(0.0, 0.0), pos2(40.0, 3.9))));
    let click = CanvasSelection { kind: CanvasSelectionKind::QuickOcr, start: pos2(5.0, 5.0), current: pos2(5.0, 5.0), recent_character_rank: None };
    assert!(!is_committable(click.rect()));
}

#[test]
fn line_edit_hits_prefer_the_topmost_line_and_the_selected_handles() {
    let lines = [Some(Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 50.0))), None, Some(Rect::from_min_max(pos2(50.0, 25.0), pos2(150.0, 75.0)))];
    assert_eq!(line_edit_hits(pos2(60.0, 30.0), &lines, None), LineEditHit { handle: None, line: Some(2) }, "the last drawn line is on top");
    assert_eq!(line_edit_hits(pos2(10.0, 10.0), &lines, None), LineEditHit { handle: None, line: Some(0) });
    assert_eq!(line_edit_hits(pos2(300.0, 300.0), &lines, Some(0)), LineEditHit::default());
    // Handles exist only on the selected line, within 8 points: index 4 is the bottom-right.
    assert_eq!(line_edit_hits(pos2(104.0, 54.0), &lines, Some(0)).handle, Some((0, 4)));
    assert_eq!(line_edit_hits(pos2(104.0, 54.0), &lines, Some(2)).handle, None);
    assert_eq!(line_edit_hits(pos2(10.0, 10.0), &lines, Some(1)).handle, None, "a line without a scene rect has no handles");
}

#[test]
fn page_uv_is_normalized_and_clamped() {
    let page = Rect::from_min_max(pos2(100.0, 200.0), pos2(300.0, 600.0));
    assert_eq!(page_uv(page, pos2(200.0, 300.0)), pos2(0.5, 0.25));
    assert_eq!(page_uv(page, pos2(0.0, 900.0)), pos2(0.0, 1.0));
}

#[test]
fn a_moved_or_resized_line_stays_inside_the_page() {
    let rect = TextDetectorRect::from_xyxy(10.0, 10.0, 30.0, 20.0).expect("valid rect");
    let moved = move_text_detector_rect(rect, pos2(15.0, 15.0), pos2(1000.0, -50.0), 100.0, 100.0);
    assert_eq!((moved.x1, moved.y1, moved.x2, moved.y2), (80.0, 0.0, 100.0, 10.0));
    // Handle 0 (top-left) cannot cross the opposite edges: at least 4 px per side.
    let resized = resize_text_detector_rect(rect, pos2(90.0, 90.0), 0, 100.0, 100.0);
    assert_eq!((resized.x1, resized.y1, resized.x2, resized.y2), (26.0, 16.0, 30.0, 20.0));
}

/// The tab state publishes its edit mode, and the brush context with the mask edit.
#[test]
fn the_tab_state_publishes_its_edit_mode() {
    let mut state = TranslationTabState::default();
    assert!(state.input_contexts().is_empty());
    state.set_text_detector_edit_lines_mode(true);
    assert_eq!(state.input_contexts(), vec![LINES_EDIT]);
    state.set_text_detector_edit_mask_mode(true);
    assert_eq!(state.input_contexts(), vec![MASK_EDIT, ms_tools::mask_brush::BRUSH_ACTIVE], "the edit modes exclude each other");
    state.set_text_detector_edit_mask_mode(false);
    assert!(state.input_contexts().is_empty());
    assert!(!state.input_contexts().contains(&ADV_REC_OPEN));
}

mod line_edits_reach_the_model {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use eframe::egui::pos2;
    use ms_models::page_view::{PageImageInfo, SourcePageLoadState};
    use ms_models::text_mask_model::TextMaskModel;

    use crate::tab::{TextDetectorLineDragKind, TextDetectorLineDragState, TextDetectorLineSelection, TranslationTabState};
    use crate::tab::detection_mask::{detector_rects_to_blocks_px, rasterize_resolved_blocks};
    use crate::text_detector::{TextDetectorPageResult, TextDetectorRect};

    fn rect(x1: f32, y1: f32, x2: f32, y2: f32) -> TextDetectorRect {
        TextDetectorRect::from_xyxy(x1, y1, x2, y2).unwrap_or_else(|| panic!("valid test rect {x1} {y1} {x2} {y2}"))
    }

    fn tab_with_page(result: TextDetectorPageResult) -> (TranslationTabState, Arc<Mutex<TextMaskModel>>) {
        let mut tab = TranslationTabState::default();
        let model = Arc::new(Mutex::new(TextMaskModel::new()));
        tab.text_mask_model = Some(Arc::clone(&model));
        tab.text_detector_results.insert(0, result);
        tab.sync_text_mask_page_from_result(0);
        (tab, model)
    }

    fn model_blocks(model: &Arc<Mutex<TextMaskModel>>) -> Option<Vec<[i32; 4]>> {
        model.lock().expect("model lock").page(0).and_then(|page| page.blocks.clone())
    }

    fn drag_on_page_0() -> TextDetectorLineDragState {
        TextDetectorLineDragState {
            selection: TextDetectorLineSelection { page_idx: 0, line_idx: 0 },
            start_pointer_src: pos2(0.0, 0.0),
            start_rect: rect(0.0, 0.0, 1.0, 1.0),
            kind: TextDetectorLineDragKind::Move,
        }
    }

    /// A page with a detector mask: the edited lines become the model's boxes once the drag
    /// ends (not while it runs); the model keeps its mask.
    #[test]
    fn edited_lines_reach_the_model_once_the_drag_ends() {
        let (mut tab, model) = tab_with_page(TextDetectorPageResult { source_size: [6, 4], blocks: vec![rect(0.0, 0.0, 1.0, 1.0)], mask_size: [6, 4], mask_alpha: vec![255; 24] });
        tab.text_detector_results.get_mut(&0).expect("page").blocks[0] = rect(1.0, 1.0, 2.0, 2.0);
        tab.line_edited_pages.insert(0);
        tab.text_detector_line_drag_state = Some(drag_on_page_0());
        tab.flush_line_edits_to_text_mask_model();
        assert_eq!(model_blocks(&model), Some(vec![[0, 0, 1, 1]]), "not while the drag runs");
        tab.text_detector_line_drag_state = None;
        tab.flush_line_edits_to_text_mask_model();
        assert_eq!(model_blocks(&model), Some(vec![[1, 1, 2, 2]]));
        assert_eq!(model.lock().expect("model lock").page(0).map(|page| page.mask_alpha.clone()), Some(vec![255; 24]), "mask kept");
        assert!(tab.line_edited_pages.is_empty());
    }

    /// A hand-edited model mask (boxes unknown) keeps its boxes unknown after a line edit.
    #[test]
    fn a_hand_edited_mask_keeps_unknown_boxes() {
        let (mut tab, model) = tab_with_page(TextDetectorPageResult { source_size: [6, 4], blocks: vec![rect(0.0, 0.0, 1.0, 1.0)], mask_size: [6, 4], mask_alpha: vec![255; 24] });
        model.lock().expect("model lock").set_page(0, [6, 4], [6, 4], vec![0; 24]);
        tab.text_detector_line_selection = Some(TextDetectorLineSelection { page_idx: 0, line_idx: 0 });
        tab.delete_selected_text_detector_line();
        tab.flush_line_edits_to_text_mask_model();
        assert_eq!(model_blocks(&model), None);
        assert_eq!(model.lock().expect("model lock").page(0).map(|page| page.mask_alpha.clone()), Some(vec![0; 24]));
    }

    /// A legacy page (no stored mask) shows the raster of its resolved lines: after a line is
    /// deleted the model page is rebuilt from the remaining lines, boxes and raster alike.
    #[test]
    fn a_legacy_page_is_rebuilt_from_the_edited_lines() {
        let legacy = TextDetectorPageResult { source_size: [6, 4], blocks: vec![rect(0.0, 0.0, 1.0, 1.0), rect(4.0, 2.0, 6.0, 4.0)], mask_size: [6, 4], mask_alpha: Vec::new() };
        let (mut tab, model) = tab_with_page(legacy);
        tab.materialize_text_mask_page_from_blocks_if_missing(0);
        tab.text_detector_line_selection = Some(TextDetectorLineSelection { page_idx: 0, line_idx: 0 });
        tab.delete_selected_text_detector_line();
        tab.flush_line_edits_to_text_mask_model();
        let result = tab.text_detector_results.get(&0).expect("page");
        let (raster, resolved) = rasterize_resolved_blocks(result, &tab.text_detector_options).expect("remaining line raster");
        let model = model.lock().expect("model lock");
        let page = model.page(0).expect("model page");
        assert_eq!(page.blocks, detector_rects_to_blocks_px(&resolved));
        assert_eq!(page.mask_alpha, raster);
    }

    /// Dropping the line edit state of one page keeps another page's selection.
    #[test]
    fn dropping_line_state_touches_only_its_page() {
        let mut tab = TranslationTabState::default();
        tab.text_detector_line_selection = Some(TextDetectorLineSelection { page_idx: 1, line_idx: 0 });
        tab.text_detector_line_drag_state = Some(drag_on_page_0());
        tab.drop_line_edit_state_on_page(0);
        assert!(tab.text_detector_line_drag_state.is_none());
        assert!(tab.text_detector_line_selection.is_some());
        tab.drop_line_edit_state_on_page(1);
        assert!(tab.text_detector_line_selection.is_none());
    }

    /// A line on a page without a detector entry takes the page size the page loader knows; with
    /// the size unknown nothing is created (no disk read).
    #[test]
    fn a_new_page_entry_takes_the_known_page_size() {
        let mut tab = TranslationTabState::default();
        assert!(tab.create_text_detector_line_at_uv(3, pos2(0.5, 0.5)).is_err(), "size unknown");
        assert!(!tab.text_detector_results.contains_key(&3));
        let loading = PageImageInfo { width_px: 0, height_px: 0, load_state: SourcePageLoadState::Loading };
        let known = PageImageInfo { width_px: 600, height_px: 800, load_state: SourcePageLoadState::Available };
        tab.note_page_sizes(&HashMap::from([(3, loading)]));
        assert!(tab.create_text_detector_line_at_uv(3, pos2(0.5, 0.5)).is_err(), "still loading");
        tab.note_page_sizes(&HashMap::from([(3, known)]));
        tab.create_text_detector_line_at_uv(3, pos2(0.5, 0.5)).unwrap_or_else(|error| panic!("line created: {error}"));
        let page = tab.text_detector_results.get(&3).expect("page entry");
        assert_eq!(page.source_size, [600, 800]);
        assert_eq!(page.blocks.len(), 1);
        assert!(tab.line_edited_pages.contains(&3));
    }
}
