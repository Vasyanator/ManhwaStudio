/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/storage.rs
The one owner of the per-chapter `text_detection` storage FORMAT: file names, the page reader
and the page writer. GUI-free; every function does file I/O and runs on worker threads only.

Format (one page `idx` in the store directory):
- `{idx:05}_blocks.json`: `{ page_idx, source_size: [w, h], blocks: [{x1, y1, x2, y2}],
  mask_size: [w, h], mask_file }`. `source_size` is the page in pixels (the space of the
  blocks); `mask_size` is the mask raster, which may be smaller than the page (a detector stores
  its mask at its own working scale); a missing `mask_size` means `source_size`. `mask_file` names
  the mask next to it: a plain file name only, any path fails the page; empty when the page
  has no mask (then the default name is probed).
- `{idx:05}_mask.png`: the mask raster, 8-bit gray.
- A mask with no blocks file (an older store) has no recorded source size: its raster size
  stands for both.

Key functions:
- `read_text_detection_page`: the one reader (Translation's storage load, Cleaning's mask
  reads through `ms-tab-cleaning` `text_mask_storage`).
- `write_text_detection_page`: the one writer (Translation's Save).
- `text_detection_blocks_file_path`, `text_detection_mask_file_path`, `stored_detection_file_page`.

Notes:
Directory policy (which pages to load, which stale files Save removes, protected pages, logging)
stays with the Translation tab (`tab/detection_storage.rs`).
*/

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{TextDetectorPageResult, TextDetectorRect};

/// `{page_idx:05}_blocks.json` in the detector store `dir`.
#[must_use]
pub fn text_detection_blocks_file_path(dir: &Path, page_idx: usize) -> PathBuf {
    dir.join(format!("{page_idx:05}_blocks.json"))
}

/// The default mask file name of `page_idx`: `{page_idx:05}_mask.png`.
fn text_detection_mask_file_name(page_idx: usize) -> String {
    format!("{page_idx:05}_mask.png")
}

/// `{page_idx:05}_mask.png` in the detector store `dir`.
#[must_use]
pub fn text_detection_mask_file_path(dir: &Path, page_idx: usize) -> PathBuf {
    dir.join(text_detection_mask_file_name(page_idx))
}

/// The page index a stored detection file name starts with (`{page_idx:05}_blocks.json` /
/// `{page_idx:05}_mask.png`), or `None` for any other name.
#[must_use]
pub fn stored_detection_file_page(name: &str) -> Option<usize> {
    let (index, _) = name.split_once('_')?;
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
}

/// Reads the stored detection of `page_idx` from the store `dir`. Worker thread only.
///
/// `Ok(None)` when the page has neither a blocks file nor a mask file. With a blocks file the
/// blocks come back in reading order and the sizes are the recorded ones; `mask_alpha` is the
/// raw gray raster (`mask_size[0] * mask_size[1]` bytes) or empty when no mask file exists. A
/// mask with no blocks file is an older store's page: no blocks, raster size for both sizes.
///
/// # Errors
/// A technical message naming the file when the blocks file cannot be read, is not a JSON
/// object or records no valid (positive) `source_size`, when an existing mask cannot be decoded
/// or is empty, or when the raster size differs from the recorded `mask_size`. An unreadable page
/// is never reported as empty, so a caller never treats (and later deletes) it as such.
pub fn read_text_detection_page(dir: &Path, page_idx: usize) -> Result<Option<TextDetectorPageResult>, String> {
    let blocks_path = text_detection_blocks_file_path(dir, page_idx);
    if !blocks_path.exists() {
        let mask_path = text_detection_mask_file_path(dir, page_idx);
        if !mask_path.exists() {
            return Ok(None);
        }
        let gray = read_mask(&mask_path)?;
        let raster_size = [gray.width(), gray.height()];
        return Ok(Some(TextDetectorPageResult {
            source_size: raster_size,
            blocks: Vec::new(),
            mask_size: raster_size,
            mask_alpha: gray.into_raw(),
        }));
    }

    let raw = fs::read_to_string(&blocks_path).map_err(|err| format!("{}: {err}", blocks_path.display()))?;
    let data = serde_json::from_str::<Value>(&raw).map_err(|err| format!("{}: {err}", blocks_path.display()))?;
    let Some(obj) = data.as_object() else {
        return Err(format!("{}: not a JSON object", blocks_path.display()));
    };

    let source_size = size_pair(obj.get("source_size"))
        .ok_or_else(|| tf!("translation.tab.invalid_source_size_error", path = blocks_path.display()))?;
    let mut blocks = Vec::<TextDetectorRect>::new();
    if let Some(items) = obj.get("blocks").and_then(Value::as_array) {
        for item in items {
            let x1 = item.get("x1").and_then(Value::as_f64);
            let y1 = item.get("y1").and_then(Value::as_f64);
            let x2 = item.get("x2").and_then(Value::as_f64);
            let y2 = item.get("y2").and_then(Value::as_f64);
            let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) else {
                continue;
            };
            // Narrowing is exact for what the writer stored (`f32` coordinates serialized by
            // serde_json); a hand-edited value only loses sub-f32 precision.
            let rect = TextDetectorRect::from_xyxy(x1 as f32, y1 as f32, x2 as f32, y2 as f32);
            if let Some(rect) = rect {
                blocks.push(rect);
            }
        }
    }
    ms_text_detect::blocks::sort_reading_order(&mut blocks);

    let mask_size = size_pair(obj.get("mask_size")).unwrap_or(source_size);
    let mask_file = match obj.get("mask_file").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => {
            // The name comes from a file on disk: only a plain file name inside the store is
            // accepted, so a crafted record can never read outside it. Anything else fails the
            // page (reported, its files kept by Save), never silently ignored.
            if !is_plain_file_name(name) {
                return Err(format!("{}: mask_file {name:?} is not a plain file name", blocks_path.display()));
            }
            dir.join(name)
        }
        None => text_detection_mask_file_path(dir, page_idx),
    };
    // A mask file that exists but cannot be decoded, or whose size differs from the recorded
    // one, fails the page: loading it as mask-less would let Save delete the file.
    let mask_alpha = if mask_file.exists() {
        let gray = read_mask(&mask_file)?;
        if gray.width() != mask_size[0] || gray.height() != mask_size[1] {
            return Err(format!(
                "{}: mask is {}x{}, the blocks file records {}x{}",
                mask_file.display(),
                gray.width(),
                gray.height(),
                mask_size[0],
                mask_size[1]
            ));
        }
        gray.into_raw()
    } else {
        Vec::new()
    };

    Ok(Some(TextDetectorPageResult { source_size, blocks, mask_size, mask_alpha }))
}

/// Whether `name` is a single plain file-name component: not absolute, no path separator of
/// any platform (`/`, `\\`), no drive prefix (`:`), and not `.` / `..`.
fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':']) && !Path::new(name).is_absolute()
}

/// Decodes a stored mask as 8-bit gray. Errors name the file when it cannot be decoded or has
/// a zero dimension.
fn read_mask(path: &Path) -> Result<image::GrayImage, String> {
    let gray = image::open(path).map_err(|err| format!("{}: {err}", path.display()))?.to_luma8();
    if gray.width() == 0 || gray.height() == 0 {
        return Err(format!("{}: the mask is empty", path.display()));
    }
    Ok(gray)
}

/// Writes `result` as page `page_idx` of the store `dir` (worker thread): the mask PNG when the
/// result has a well-formed mask (an old mask file is removed otherwise), then the blocks file.
///
/// # Errors
/// A technical message naming the file that could not be written or removed.
pub fn write_text_detection_page(dir: &Path, page_idx: usize, result: &TextDetectorPageResult) -> Result<(), String> {
    let mask_file_name = text_detection_mask_file_name(page_idx);
    let blocks_path = text_detection_blocks_file_path(dir, page_idx);
    let mask_path = dir.join(&mask_file_name);

    let expected_mask_len = usize::try_from(result.mask_size[0])
        .ok()
        .zip(usize::try_from(result.mask_size[1]).ok())
        .and_then(|(w, h)| w.checked_mul(h));
    let has_mask = !result.mask_alpha.is_empty()
        && result.mask_size[0] > 0
        && result.mask_size[1] > 0
        && expected_mask_len == Some(result.mask_alpha.len());

    if has_mask {
        let img = image::GrayImage::from_vec(result.mask_size[0], result.mask_size[1], result.mask_alpha.clone())
            .ok_or_else(|| tf!("translation.tab.invalid_mask_error", page_idx = page_idx))?;
        img.save_with_format(&mask_path, image::ImageFormat::Png)
            .map_err(|err| format!("{}: {err}", mask_path.display()))?;
    } else if mask_path.exists() {
        fs::remove_file(&mask_path).map_err(|err| format!("{}: {err}", mask_path.display()))?;
    }

    let blocks = result
        .blocks
        .iter()
        .map(|rect| {
            serde_json::json!({
                "x1": rect.x1,
                "y1": rect.y1,
                "x2": rect.x2,
                "y2": rect.y2
            })
        })
        .collect::<Vec<_>>();
    let payload = serde_json::json!({
        "page_idx": page_idx,
        "source_size": result.source_size,
        "blocks": blocks,
        "mask_size": result.mask_size,
        "mask_file": if has_mask { mask_file_name } else { String::new() },
    });
    let raw = serde_json::to_string_pretty(&payload).map_err(|err| err.to_string())?;
    fs::write(&blocks_path, raw).map_err(|err| format!("{}: {err}", blocks_path.display()))?;
    Ok(())
}

/// A `[w, h]` JSON pair of positive `u32`s (the first two items of the array).
fn size_pair(value: Option<&Value>) -> Option<[u32; 2]> {
    let arr = value?.as_array()?;
    let [w, h, ..] = arr.as_slice() else { return None };
    let w = u32::try_from(w.as_u64()?).ok()?;
    let h = u32::try_from(h.as_u64()?).ok()?;
    (w > 0 && h > 0).then_some([w, h])
}

#[cfg(test)]
mod tests;
