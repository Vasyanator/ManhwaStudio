/*
File: crates/ms-tab-cleaning/src/text_mask_storage.rs

Purpose:
Reads one page's stored text-detector mask for the Cleaning tab (the display-cache load and
the quick text clean's disk fallback) WITH the sizes the store recorded, so a mask the
detector stored at a scaled size is accepted as what it is instead of being taken for a
source-sized mask of another image.

Key structures:
- `StoredTextMask`: the mask raster plus the source-page size and mask size it was stored for.

Key functions:
- `read_stored_text_mask()`: an adapter over the format's one reader.

Notes:
The storage format is owned by the Translation tab's detector
(`ms_tab_translation::text_detector::read_text_detection_page`, which documents the size rules);
this file only keeps the mask half of a page and binarizes it. Runs on worker threads only
(file I/O).
*/

use std::path::Path;

use ms_tab_translation::text_detector::read_text_detection_page;

/// One page's stored mask and the sizes the store recorded for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredTextMask {
    /// The source page's pixel size `[w, h]` the mask was stored for.
    pub source_size: [u32; 2],
    /// The mask raster's size `[w, h]`; may differ from `source_size` (a scaled mask).
    pub mask_size: [u32; 2],
    /// `mask_size[0] * mask_size[1]` bytes, binarized to 0 / 255.
    pub mask_alpha: Vec<u8>,
}

/// Reads the stored mask of `page_idx` from the detector store `dir`. Worker thread only.
///
/// `Ok(None)` when the page has no stored mask (no files, or blocks without a mask). The sizes
/// are the recorded ones (`read_text_detection_page`).
///
/// # Errors
/// The reader's technical message naming the file when the page's files exist but cannot be
/// read, record no valid sizes, or disagree with each other (a mismatched pair is never guessed at).
pub(crate) fn read_stored_text_mask(dir: &Path, page_idx: usize) -> Result<Option<StoredTextMask>, String> {
    let Some(page) = read_text_detection_page(dir, page_idx)? else {
        return Ok(None);
    };
    if page.mask_alpha.is_empty() {
        return Ok(None);
    }
    let mask_alpha = page.mask_alpha.into_iter().map(|px| if px > 0 { 255 } else { 0 }).collect();
    Ok(Some(StoredTextMask { source_size: page.source_size, mask_size: page.mask_size, mask_alpha }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use ms_tab_translation::text_detector::{text_detection_blocks_file_path, text_detection_mask_file_path};

    /// A unique scratch store under the OS temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ms_cleaning_text_mask_storage_{name}_{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create the scratch store");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(err) = std::fs::remove_dir_all(&self.0) {
                eprintln!("could not remove the scratch store {}: {err}", self.0.display());
            }
        }
    }

    fn write_mask(dir: &Path, page_idx: usize, w: u32, h: u32) {
        let mut mask = image::GrayImage::new(w, h);
        mask.put_pixel(1, 1, image::Luma([200]));
        mask.save(text_detection_mask_file_path(dir, page_idx)).expect("write the mask");
    }

    /// A mask the detector stored at half the page size keeps the page's source size, so the
    /// model does not take it for a mask of another (smaller) image.
    #[test]
    fn a_scaled_stored_mask_keeps_the_recorded_source_size() {
        let store = Scratch::new("scaled");
        write_mask(&store.0, 3, 100, 50);
        std::fs::write(text_detection_blocks_file_path(&store.0, 3), r#"{"page_idx":3,"source_size":[200,100],"blocks":[],"mask_size":[100,50],"mask_file":"00003_mask.png"}"#).expect("write the blocks file");
        let mask = read_stored_text_mask(&store.0, 3).expect("readable").expect("present");
        assert_eq!(mask.source_size, [200, 100]);
        assert_eq!(mask.mask_size, [100, 50]);
        assert_eq!(mask.mask_alpha.len(), 100 * 50);
        assert_eq!(mask.mask_alpha.get(100 + 1), Some(&255), "binarized");
    }

    /// No mask file: nothing stored. A mask with no blocks file (older store): its raster
    /// size stands for both. A blocks file without `mask_size`: the mask is source-sized.
    #[test]
    fn missing_files_follow_the_writer_rules() {
        let store = Scratch::new("legacy");
        assert_eq!(read_stored_text_mask(&store.0, 0), Ok(None));
        write_mask(&store.0, 0, 40, 30);
        let legacy = read_stored_text_mask(&store.0, 0).expect("readable").expect("present");
        assert_eq!((legacy.source_size, legacy.mask_size), ([40, 30], [40, 30]));
        std::fs::write(text_detection_blocks_file_path(&store.0, 0), r#"{"source_size":[40,30],"blocks":[]}"#).expect("write the blocks file");
        let full = read_stored_text_mask(&store.0, 0).expect("readable").expect("present");
        assert_eq!((full.source_size, full.mask_size), ([40, 30], [40, 30]));
    }

    /// A raster that does not match the recorded mask size, or a blocks file without a valid
    /// source size, is an error — never a guess.
    #[test]
    fn a_mismatched_or_unreadable_record_is_an_error() {
        let store = Scratch::new("mismatch");
        write_mask(&store.0, 1, 100, 50);
        std::fs::write(text_detection_blocks_file_path(&store.0, 1), r#"{"source_size":[200,100],"mask_size":[90,50]}"#).expect("write the blocks file");
        assert!(read_stored_text_mask(&store.0, 1).is_err());
        std::fs::write(text_detection_blocks_file_path(&store.0, 1), r#"{"source_size":[0,100]}"#).expect("write the blocks file");
        assert!(read_stored_text_mask(&store.0, 1).is_err());
        std::fs::write(text_detection_blocks_file_path(&store.0, 1), "{ broken").expect("write the blocks file");
        assert!(read_stored_text_mask(&store.0, 1).is_err());
    }
}
