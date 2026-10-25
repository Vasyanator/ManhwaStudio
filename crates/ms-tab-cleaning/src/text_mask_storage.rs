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
- `read_stored_text_mask()`: `{idx:05}_mask.png` + the sizes of `{idx:05}_blocks.json`.
- `text_detection_mask_file_path()`, `text_detection_blocks_file_path()`.

Notes:
The storage format is written and owned by the Translation tab (`ms-tab-translation`
`tab.rs`: `save_text_detection_page` / `load_text_detection_page`): `_blocks.json` records
`source_size` (the page in pixels, the space of the blocks) and `mask_size` (the mask raster,
which may be smaller than the page — a detector stores its mask at its own working scale);
a missing `mask_size` means `source_size`. Only those two keys are read here. A mask with no
blocks file (an older store) has no recorded source size: its raster size stands for both.
Runs on worker threads only (file I/O).
*/

use std::path::{Path, PathBuf};

use serde_json::Value;

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

/// `{page_idx:05}_mask.png` in the detector store `dir`.
pub(crate) fn text_detection_mask_file_path(dir: &Path, page_idx: usize) -> PathBuf {
    dir.join(format!("{page_idx:05}_mask.png"))
}

/// `{page_idx:05}_blocks.json` in the detector store `dir`.
pub(crate) fn text_detection_blocks_file_path(dir: &Path, page_idx: usize) -> PathBuf {
    dir.join(format!("{page_idx:05}_blocks.json"))
}

/// Reads the stored mask of `page_idx` from the detector store `dir`. Worker thread only.
///
/// `Ok(None)` when the page has no mask file. The sizes come from the page's blocks file when
/// it exists (see the file header); otherwise the raster's own size is used for both.
///
/// # Errors
/// A technical message naming the file when the mask cannot be decoded or is empty, when the
/// blocks file exists but cannot be read or records no valid `source_size`, or when the mask
/// raster's size differs from the recorded `mask_size` (a mismatched pair is never guessed at).
pub(crate) fn read_stored_text_mask(dir: &Path, page_idx: usize) -> Result<Option<StoredTextMask>, String> {
    let mask_path = text_detection_mask_file_path(dir, page_idx);
    if !mask_path.exists() {
        return Ok(None);
    }
    let luma = image::open(&mask_path).map_err(|err| format!("{}: {err}", mask_path.display()))?.to_luma8();
    let raster_size = [luma.width(), luma.height()];
    if raster_size[0] == 0 || raster_size[1] == 0 {
        return Err(format!("{}: the mask is empty", mask_path.display()));
    }
    let blocks_path = text_detection_blocks_file_path(dir, page_idx);
    let recorded = if blocks_path.exists() {
        let raw = std::fs::read_to_string(&blocks_path).map_err(|err| format!("{}: {err}", blocks_path.display()))?;
        let value = serde_json::from_str::<Value>(&raw).map_err(|err| format!("{}: {err}", blocks_path.display()))?;
        Some(recorded_sizes(&value).ok_or_else(|| format!("{}: no valid source_size", blocks_path.display()))?)
    } else {
        None
    };
    let (source_size, mask_size) = recorded.unwrap_or((raster_size, raster_size));
    if mask_size != raster_size {
        return Err(format!(
            "{}: the mask is {}x{}, the blocks file records {}x{}",
            mask_path.display(),
            raster_size[0],
            raster_size[1],
            mask_size[0],
            mask_size[1]
        ));
    }
    let mask_alpha = luma.into_raw().into_iter().map(|px| if px > 0 { 255 } else { 0 }).collect();
    Ok(Some(StoredTextMask { source_size, mask_size, mask_alpha }))
}

/// `(source_size, mask_size)` of a parsed blocks file: `None` without a valid nonzero
/// `source_size`; a missing or invalid `mask_size` means `source_size` (the writer's rule).
fn recorded_sizes(value: &Value) -> Option<([u32; 2], [u32; 2])> {
    let source_size = size_pair(value.get("source_size"))?;
    Some((source_size, size_pair(value.get("mask_size")).unwrap_or(source_size)))
}

/// A `[w, h]` JSON pair of positive `u32`s.
fn size_pair(value: Option<&Value>) -> Option<[u32; 2]> {
    let pair = value?.as_array()?;
    let [w, h] = pair.as_slice() else { return None };
    let w = u32::try_from(w.as_u64()?).ok()?;
    let h = u32::try_from(h.as_u64()?).ok()?;
    (w > 0 && h > 0).then_some([w, h])
}

#[cfg(test)]
mod tests {
    use super::*;

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
