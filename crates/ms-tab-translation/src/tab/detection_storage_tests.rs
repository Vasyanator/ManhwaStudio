//! Tests of the `text_detection` disk storage (`tab/detection_storage.rs`): unreadable pages are
//! reported, Save keeps protected pages, stored file names map to their page (desktop-only).

use std::collections::HashSet;
use std::fs;

use super::{TextDetectorPageResult, TextDetectorRect};
use super::detection_storage::{
    load_text_detection_storage, save_text_detection_storage, stored_detection_file_page,
    text_detection_blocks_file_path, text_detection_mask_file_path,
};

fn page(blocks: usize) -> TextDetectorPageResult {
    let rect = TextDetectorRect::from_xyxy(0.0, 0.0, 1.0, 1.0).expect("valid rect");
    TextDetectorPageResult { source_size: [2, 2], blocks: vec![rect; blocks], mask_size: [2, 2], mask_alpha: vec![0, 255, 255, 0] }
}

/// Pages whose stored files exist but cannot be read (bad JSON, an undecodable mask) are
/// reported as failed, never loaded as empty; a readable page loads.
#[test]
fn unreadable_stored_pages_are_reported_not_emptied() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let storage = dir.path();
    save_text_detection_storage(storage, &[(0, page(1)), (1, page(1)), (2, page(1))], &HashSet::new())?;
    fs::write(text_detection_blocks_file_path(storage, 1), "{ not json").map_err(|err| err.to_string())?;
    fs::write(text_detection_mask_file_path(storage, 2), b"not a png").map_err(|err| err.to_string())?;
    let load = load_text_detection_storage(storage, &[0, 1, 2, 3]);
    assert_eq!(load.pages.iter().map(|(idx, _)| *idx).collect::<Vec<_>>(), vec![0]);
    assert_eq!(load.failed_pages, vec![1, 2]);
    Ok(())
}

/// Save removes every other stored page but leaves the files of a protected (unreadable)
/// page exactly as they are.
#[test]
fn save_keeps_the_files_of_protected_pages() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let storage = dir.path();
    save_text_detection_storage(storage, &[(0, page(1)), (1, page(1))], &HashSet::new())?;
    fs::write(text_detection_blocks_file_path(storage, 1), "{ not json").map_err(|err| err.to_string())?;
    let protected = HashSet::from([1]);
    let (saved, failed) = save_text_detection_storage(storage, &[(5, page(2))], &protected)?;
    assert_eq!((saved, failed), (1, 0));
    assert!(!text_detection_blocks_file_path(storage, 0).exists(), "a stale page is removed");
    assert_eq!(fs::read_to_string(text_detection_blocks_file_path(storage, 1)).map_err(|err| err.to_string())?, "{ not json");
    assert!(text_detection_mask_file_path(storage, 1).exists(), "the protected mask stays");
    assert!(text_detection_blocks_file_path(storage, 5).exists());
    Ok(())
}

#[test]
fn stored_file_names_map_to_their_page() {
    assert_eq!(stored_detection_file_page("00012_blocks.json"), Some(12));
    assert_eq!(stored_detection_file_page("00003_mask.png"), Some(3));
    assert_eq!(stored_detection_file_page("custom_mask.png"), None);
    assert_eq!(stored_detection_file_page("_mask.png"), None);
}
