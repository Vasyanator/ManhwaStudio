//! Tests of the `text_detection` storage format (`text_detector/storage.rs`): the page reader's
//! size rules, the older mask-only page, and the write -> read round trip.

use std::fs;
use std::path::Path;

use super::{read_text_detection_page, text_detection_blocks_file_path, text_detection_mask_file_path, write_text_detection_page};
use crate::text_detector::{TextDetectorPageResult, TextDetectorRect};

fn write_mask(dir: &Path, page_idx: usize, w: u32, h: u32) -> Result<(), String> {
    let mut mask = image::GrayImage::new(w, h);
    mask.put_pixel(1, 1, image::Luma([200]));
    mask.save(text_detection_mask_file_path(dir, page_idx)).map_err(|err| err.to_string())
}

/// A page written by the writer reads back with the same sizes, blocks and mask bytes.
#[test]
fn a_written_page_reads_back_unchanged() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let rect = TextDetectorRect::from_xyxy(1.5, 2.0, 10.25, 12.0).ok_or("valid rect")?;
    let page = TextDetectorPageResult { source_size: [4, 2], blocks: vec![rect], mask_size: [2, 1], mask_alpha: vec![0, 255] };
    write_text_detection_page(dir.path(), 7, &page)?;
    let read = read_text_detection_page(dir.path(), 7)?.ok_or("present")?;
    assert_eq!((read.source_size, read.mask_size), ([4, 2], [2, 1]));
    assert_eq!(read.mask_alpha, vec![0, 255]);
    assert_eq!(read.blocks.len(), 1);
    assert_eq!(read.blocks.first().map(|r| (r.x1, r.y1, r.x2, r.y2)), Some((1.5, 2.0, 10.25, 12.0)));
    Ok(())
}

/// A mask the detector stored at half the page size keeps the page's source size, so a reader
/// does not take it for a mask of another (smaller) image.
#[test]
fn a_scaled_stored_mask_keeps_the_recorded_source_size() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    write_mask(dir.path(), 3, 100, 50)?;
    fs::write(text_detection_blocks_file_path(dir.path(), 3), r#"{"page_idx":3,"source_size":[200,100],"blocks":[],"mask_size":[100,50],"mask_file":"00003_mask.png"}"#).map_err(|err| err.to_string())?;
    let page = read_text_detection_page(dir.path(), 3)?.ok_or("present")?;
    assert_eq!((page.source_size, page.mask_size), ([200, 100], [100, 50]));
    assert_eq!(page.mask_alpha.len(), 100 * 50);
    assert_eq!(page.mask_alpha.get(100 + 1), Some(&200), "raw gray bytes");
    Ok(())
}

/// No files: nothing stored. A mask with no blocks file (older store): its raster size stands
/// for both. A blocks file without `mask_size`: the mask is source-sized.
#[test]
fn missing_files_follow_the_writer_rules() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    assert!(read_text_detection_page(dir.path(), 0)?.is_none());
    write_mask(dir.path(), 0, 40, 30)?;
    let legacy = read_text_detection_page(dir.path(), 0)?.ok_or("present")?;
    assert_eq!((legacy.source_size, legacy.mask_size), ([40, 30], [40, 30]));
    assert!(legacy.blocks.is_empty());
    fs::write(text_detection_blocks_file_path(dir.path(), 0), r#"{"source_size":[40,30],"blocks":[]}"#).map_err(|err| err.to_string())?;
    let full = read_text_detection_page(dir.path(), 0)?.ok_or("present")?;
    assert_eq!((full.source_size, full.mask_size), ([40, 30], [40, 30]));
    assert_eq!(full.mask_alpha.len(), 40 * 30);
    Ok(())
}

/// A raster that does not match the recorded mask size, a blocks file without a valid source
/// size, malformed JSON or a non-object document is an error — never a guess, never "empty".
#[test]
fn a_mismatched_or_unreadable_record_is_an_error() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    write_mask(dir.path(), 1, 100, 50)?;
    let blocks = text_detection_blocks_file_path(dir.path(), 1);
    for body in [r#"{"source_size":[200,100],"mask_size":[90,50]}"#, r#"{"source_size":[0,100]}"#, "{ broken", "[1, 2]"] {
        fs::write(&blocks, body).map_err(|err| err.to_string())?;
        assert!(read_text_detection_page(dir.path(), 1).is_err(), "{body}");
    }
    Ok(())
}

/// A `mask_file` that is not a plain file name (a path, `..`, absolute, a drive prefix) fails
/// the page instead of reading outside the store; a plain other name is followed.
#[test]
fn a_mask_file_outside_the_store_fails_the_page() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let store = dir.path().join("store");
    fs::create_dir_all(&store).map_err(|err| err.to_string())?;
    // A valid mask OUTSIDE the store that a crafted record would point at.
    write_mask(dir.path(), 4, 2, 2)?;
    let blocks = text_detection_blocks_file_path(&store, 4);
    for name in ["../00004_mask.png", "sub/x.png", "sub\\x.png", "/etc/passwd", "C:x.png", "..", "."] {
        let body = serde_json::json!({ "source_size": [2, 2], "mask_size": [2, 2], "mask_file": name }).to_string();
        fs::write(&blocks, body).map_err(|err| err.to_string())?;
        assert!(read_text_detection_page(&store, 4).is_err(), "{name}");
    }
    let mut custom = image::GrayImage::new(2, 2);
    custom.put_pixel(0, 0, image::Luma([255]));
    custom.save(store.join("custom.png")).map_err(|err| err.to_string())?;
    fs::write(&blocks, r#"{"source_size":[2,2],"mask_size":[2,2],"mask_file":"custom.png"}"#).map_err(|err| err.to_string())?;
    let page = read_text_detection_page(&store, 4)?.ok_or("present")?;
    assert_eq!(page.mask_alpha, vec![255, 0, 0, 0]);
    Ok(())
}
