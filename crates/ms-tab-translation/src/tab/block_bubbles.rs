/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/block_bubbles.rs
The one path from "a detector block + its recognized text" to a bubble, shared by block OCR
(«Распознать блоки», one OCR request per detector block) and the NeuroLate detector mode with
"recognize text right away" (the block texts come with the page detection itself).

Main items:
- `block_uv_rect`: a block rectangle (page pixels) -> the page-normalized selection a block OCR
  task / bubble uses (clamped; degenerate blocks refused).
- `page_text_inserts`: a NeuroLate page result + its aligned block texts -> the bubble inserts
  (rect expanded like block OCR's, text finished by `ocr::finish_recognized_text`).
- `TranslationTabState::insert_recognized_bubble`: one recognized result -> one bubble at its
  selection (the canvas's `create_bubble_with_original_text_at_page_uv_rect`, the optional
  recent-character rank, the immediate flush) — used by the OCR `Recognized` arm.
- `TranslationTabState::create_bubbles_from_page_texts`: every insert of a page, through
  `insert_recognized_bubble`.

Notes:
GUI thread; string work only, no I/O. Detector blocks are never merged for page texts (each text
belongs to exactly one block); the expansion is the detector panel's `block_expand_px`.
*/

use ms_canvas::CanvasView;
use ms_project::ProjectData;

use super::{OcrPendingBubbleInsert, TranslationTabState};
use super::detection_mask::detector_expand_blocks;
use super::ocr_flow::build_bubble_original_text;
use crate::ocr::{OcrRecognizeResult, OcrTextFinish, finish_recognized_text};
use crate::text_detector::{TextDetectorPageResult, TextDetectorRect};

/// The page-normalized selection `[u1, v1, u2, v2]` of a block rectangle (source pixels) on a
/// page of `source_size`: clamped to the page; `None` when a side is not wider than 0.0001 (the
/// block-OCR rule for degenerate blocks).
pub(super) fn block_uv_rect(rect: &TextDetectorRect, source_size: [u32; 2]) -> Option<[f32; 4]> {
    // Page sides are far below 2^24, so the u32 -> f32 conversion is exact.
    let source_w = source_size[0].max(1) as f32;
    let source_h = source_size[1].max(1) as f32;
    let u1 = (rect.x1 / source_w).clamp(0.0, 1.0);
    let v1 = (rect.y1 / source_h).clamp(0.0, 1.0);
    let u2 = (rect.x2 / source_w).clamp(0.0, 1.0);
    let v2 = (rect.y2 / source_h).clamp(0.0, 1.0);
    ((u2 - u1) > 0.0001 && (v2 - v1) > 0.0001).then_some([u1, v1, u2, v2])
}

/// The bubbles a NeuroLate page detection with block texts makes: for every block of `result`
/// whose aligned entry of `texts` holds text, the block expanded by `block_expand_px` (as block
/// OCR expands it), its selection (`block_uv_rect`) and the text finished like an OCR request
/// of that block (`finish_recognized_text` with `finish`). Blocks without text, degenerate
/// blocks and texts without a block are skipped; a result whose finished text is blank too.
pub(super) fn page_text_inserts(
    page_idx: usize,
    result: &TextDetectorPageResult,
    texts: &[Option<String>],
    block_expand_px: i32,
    finish: &OcrTextFinish,
) -> Vec<(OcrPendingBubbleInsert, OcrRecognizeResult)> {
    // Page sides are far below 2^24, so the u32 -> f32 conversion is exact.
    let (source_w, source_h) = (result.source_size[0].max(1) as f32, result.source_size[1].max(1) as f32);
    result
        .blocks
        .iter()
        .zip(texts)
        .filter_map(|(rect, text)| {
            let text = text.as_deref()?;
            let expanded = detector_expand_blocks(std::slice::from_ref(rect), block_expand_px, source_w, source_h);
            let uv_rect = block_uv_rect(expanded.first()?, result.source_size)?;
            let recognized = finish_recognized_text(text, finish);
            if recognized.text.trim().is_empty() {
                return None;
            }
            let insert = OcrPendingBubbleInsert { page_idx, uv_rect, join_newlines: finish.join_newlines, recent_character_rank: None };
            Some((insert, recognized))
        })
        .collect()
}

impl TranslationTabState {
    /// Creates the bubble a recognized `result` makes at `insert`'s selection: the bubble's
    /// original text (`build_bubble_original_text`, honouring `insert.join_newlines`) at the
    /// selection, the recent-character rank when the insert carries one, then the immediate
    /// flush to the bubbles model. A blank text creates nothing. Returns whether a bubble was
    /// created.
    pub(super) fn insert_recognized_bubble(&mut self, canvas: &mut CanvasView, project: &ProjectData, insert: &OcrPendingBubbleInsert, result: &OcrRecognizeResult) -> bool {
        let original_text = build_bubble_original_text(result, insert.join_newlines);
        if original_text.trim().is_empty() || !canvas.create_bubble_with_original_text_at_page_uv_rect(insert.page_idx, insert.uv_rect, original_text) {
            return false;
        }
        if let Some(rank) = insert.recent_character_rank
            && !self.apply_recent_character_rank_to_latest_bubble(canvas, project, rank)
        {
            ms_log::runtime_log::log_info(format!("[translation] recent character rank {rank} not applied to the new bubble on page {}", insert.page_idx));
        }
        canvas.flush_pending_bubble_upserts_now(project);
        true
    }

    /// Creates the bubbles of a NeuroLate page detection's block texts (`page_text_inserts` with
    /// the OCR panel's text options and the detector panel's block expansion), each through
    /// [`Self::insert_recognized_bubble`]. Returns how many were created.
    pub(super) fn create_bubbles_from_page_texts(&mut self, canvas: &mut CanvasView, project: &ProjectData, page_idx: usize, result: &TextDetectorPageResult, texts: &[Option<String>]) -> usize {
        let options = &self.ocr_panel_options;
        let finish = OcrTextFinish {
            join_newlines: options.join_newlines,
            reflect_strings: options.reflect_strings,
            char_replacements: options.runtime_char_replacements(),
            fix_caps_lock: options.fix_caps_lock,
        };
        let inserts = page_text_inserts(page_idx, result, texts, self.text_detector_options.block_expand_px, &finish);
        let mut created = 0usize;
        for (insert, recognized) in &inserts {
            if self.insert_recognized_bubble(canvas, project, insert, recognized) {
                created += 1;
            }
        }
        ms_log::runtime_log::log_info(format!("[text-detector] NeuroLate page {page_idx}: {created} bubble(s) from {} block text(s)", inserts.len()));
        created
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::CharReplacementRule;

    fn rect(x1: f32, y1: f32, x2: f32, y2: f32) -> TextDetectorRect {
        TextDetectorRect::from_xyxy(x1, y1, x2, y2).unwrap_or_else(|| panic!("valid test rect {x1} {y1} {x2} {y2}"))
    }

    fn finish(join_newlines: bool) -> OcrTextFinish {
        OcrTextFinish {
            join_newlines,
            reflect_strings: false,
            char_replacements: vec![CharReplacementRule { targets: vec!["…".to_string()], replacement: "...".to_string() }],
            fix_caps_lock: true,
        }
    }

    #[test]
    fn block_uv_rect_clamps_and_refuses_degenerate_blocks() {
        assert_eq!(block_uv_rect(&rect(10.0, 20.0, 60.0, 120.0), [100, 200]), Some([0.1, 0.1, 0.6, 0.6]));
        assert_eq!(block_uv_rect(&rect(-10.0, 0.0, 200.0, 400.0), [100, 200]), Some([0.0, 0.0, 1.0, 1.0]));
        assert_eq!(block_uv_rect(&rect(150.0, 0.0, 160.0, 10.0), [100, 200]), None, "outside the page");
    }

    /// Each block with text becomes one insert at its expanded selection, with the text
    /// finished like an OCR result (substitutions, caps fix); blocks without text, and texts
    /// past the last block, make none.
    #[test]
    fn page_texts_become_finished_inserts_at_their_blocks() {
        let result = TextDetectorPageResult {
            source_size: [100, 200],
            blocks: vec![rect(10.0, 20.0, 30.0, 40.0), rect(50.0, 60.0, 70.0, 80.0), rect(10.0, 100.0, 30.0, 120.0)],
            mask_size: [0, 0],
            mask_alpha: Vec::new(),
        };
        let texts = vec![Some("HELLO THERE…".to_string()), None, Some("  ".to_string()), Some("orphan".to_string())];
        let inserts = page_text_inserts(3, &result, &texts, 5, &finish(true));
        assert_eq!(inserts.len(), 1, "blank and missing texts skipped, the orphan text has no block");
        let (insert, recognized) = &inserts[0];
        assert_eq!((insert.page_idx, insert.join_newlines, insert.recent_character_rank), (3, true, None));
        assert_eq!(insert.uv_rect, [0.05, 0.075, 0.35, 0.225], "expanded by 5 px, then normalized");
        assert_eq!(recognized.text, "Hello there...");
        assert_eq!(build_bubble_original_text(recognized, insert.join_newlines), "Hello there...");
    }
}
