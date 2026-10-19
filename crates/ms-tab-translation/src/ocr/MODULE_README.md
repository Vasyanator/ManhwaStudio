# Module: crates/ms-tab-translation/src/ocr

## Purpose
Child modules of `ocr.rs` (the Translation tab's OCR controller and worker). `ocr.rs` keeps the
controller, the command / event types, the worker loop and the routing of a recognize command;
an engine step that is large and self-contained lives here.

## Architecture
`ocr.rs::run_recognize_command` dispatches each request by engine. The NeuroLate engine
(`OcrEngine::NeuroLate`) goes to `publish_neurolate_recognize` in `ocr.rs`, which calls
`neurolate::run_neurolate_ocr` here, applies the shared post-OCR processing
(`apply_post_ocr_processing`) and publishes `WorkerEvent::RecognizeOk { result, region }`.
Children use the parent's private helpers (`crop_image_with_placement`, `encode_png`,
`assemble_line_ocr_result`, `PageImageCache`, `OcrFailure`) instead of copies.

## Files and submodules
- `neurolate.rs` (native only): one NeuroLate recognize — generation run, key read, crop with its
  page placement, ONE paid request (`MASK_AND_TEXT` only when the request carries an
  `OcrRegionWant` and the crop lies on a page, else `TEXT`), the block texts in the common
  reading order through `assemble_line_ocr_result`, and the region through
  `text_detector::neurolate::region_from_image`. Fixture-driven unit tests over
  `crates/ms-ai-api/fixtures/neurolate/`.

## Contracts and invariants
- A paid request is sent at most once: never retried here, and the tab skips block OCR's
  automatic retry for NeuroLate (`OcrEngine::retries_failed_block_request`).
- The wait runs under the controller's `GenerationTracker` run: the panel's Stop and the
  controller drop abandon it at once (`OcrFailure::Cancelled`); the detached request may still
  be charged.
- `region` in the result is `Some` exactly when the request wanted one; an unusable region is
  `Some(Err)` while the text is still published. The region conversion has one owner
  (`text_detector::neurolate`), never duplicated here.
- Text: blocks with an error, an EMPTY / FAILED status or no text add nothing; the join (line
  breaks, reflection, blank lines) is `assemble_line_ocr_result`'s.
- Worker thread only (credential store, PNG encode, HTTP wait).

## Editing map
- To change how a NeuroLate answer becomes text or which mode is sent, edit `neurolate.rs`.
- To change the answer -> mask + blocks conversion, edit `../text_detector/neurolate.rs`.
- To change when a selection asks for the region, edit
  `../text_detector/region_detect.rs` (`selection_detects`, `ocr_region_want`) and
  `../tab/detector_region.rs`.
