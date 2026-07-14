/*
File: src/canvas/workers.rs

Purpose:
Background worker helpers for the canvas subsystem.

Main responsibilities:
- spawn the clean-overlay prepare worker;
- spawn the canvas settings saver worker;
- spawn the overlay autosave worker (saves dirty overlays to unsaved folder every 30 s);
- keep thread/bootstrap code out of `mod.rs` and runtime buckets.

Key functions:
- spawn_overlay_prepare_thread()
- spawn_canvas_settings_saver_thread()
- spawn_overlay_autosave_thread()
- build_overlay_prepared_tiles_parallel()
- save_overlay_snapshots_parallel()

Notes:
- Workers must stay non-blocking for the GUI thread.
- Errors are reported through `runtime_log` and returned channels, not ignored.
- Overlay tiling and overlay-autosave PNG encoding are CPU-bound and run on the global rayon
  pool from inside these already-dedicated worker threads (no nested private pools). The parallel
  paths reproduce the exact tile layout / file set of their sequential references.
*/

use super::OVERLAY_TILE_SIDE;
use super::helpers::rgba_from_overlay_tile;
use super::settings::{save_canvas_settings_to_project_file, save_canvas_settings_to_user_file};
use super::types::{
    CanvasSettingsSaveRequest, OverlayPrepareRequest, OverlayPrepareResult, OverlayPreparedTile,
};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::runtime_log;
use image::RgbaImage;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use ms_thread::{self as thread, JoinHandle};
use web_time::Duration;

pub(super) fn spawn_overlay_prepare_thread() -> (
    Sender<Option<OverlayPrepareRequest>>,
    Receiver<OverlayPrepareResult>,
    JoinHandle<()>,
) {
    let (tx_req, rx_req) = mpsc::channel::<Option<OverlayPrepareRequest>>();
    let (tx_res, rx_res) = mpsc::channel::<OverlayPrepareResult>();
    let handle = thread::spawn(move || {
        // This is already a dedicated background worker thread, so using the global rayon pool
        // inside it parallelizes one page's tiles without nesting a private pool.
        while let Ok(msg) = rx_req.recv() {
            let Some(request) = msg else {
                break;
            };
            let tiles = build_overlay_prepared_tiles_parallel(request.image.as_ref());
            let result = OverlayPrepareResult {
                page_idx: request.page_idx,
                job_id: request.job_id,
                size: request.image.size,
                tiles,
            };
            if tx_res.send(result).is_err() {
                break;
            }
        }
    });
    (tx_req, rx_res, handle)
}

/// Tiles `image` into GPU-upload tiles, copying each tile's pixels in parallel via the global
/// rayon pool.
///
/// Produces the same tile collection a straightforward sequential builder would: an
/// `OVERLAY_TILE_SIDE` grid walked in row-major order, with `tile_idx` matching position,
/// `origin_px` / `size_px` covering each (possibly partial edge) tile, and byte-identical
/// premultiplied RGBA produced by `rgba_from_overlay_tile`. The test module's inline sequential
/// oracle (`parallel_tiling_matches_sequential_reference`) pins this equivalence. Tiles are
/// independent: the grid covers disjoint
/// rectangular sub-regions of the source, each tile reads its own (non-overlapping) source
/// rectangle from the shared read-only `image` and writes only its own freshly allocated buffer,
/// so the per-tile work has no shared mutable state. Returns an empty `Vec` for a zero-sized image.
fn build_overlay_prepared_tiles_parallel(image: &egui::ColorImage) -> Vec<OverlayPreparedTile> {
    let w = image.size[0];
    let h = image.size[1];
    if w == 0 || h == 0 {
        return Vec::new();
    }
    // Build the tile grid sequentially first (cheap, no pixel copies). This reproduces the exact
    // boundary math and row-major ordering of the sequential reference so consumers that index by
    // `tile_idx` see an identical layout.
    let mut grid: Vec<([usize; 2], [usize; 2])> = Vec::new();
    let mut y = 0usize;
    while y < h {
        let mut x = 0usize;
        while x < w {
            let tw = (w - x).min(OVERLAY_TILE_SIDE);
            let th = (h - y).min(OVERLAY_TILE_SIDE);
            grid.push(([x, y], [tw, th]));
            x += OVERLAY_TILE_SIDE;
        }
        y += OVERLAY_TILE_SIDE;
    }
    // Copy each tile's pixels in parallel. `par_iter().enumerate()` preserves input order on
    // `collect`, so the resulting `Vec` stays in the same row-major order and `tile_idx` matches
    // the position, identical to the sequential builder.
    grid.par_iter()
        .enumerate()
        .map(|(tile_idx, &(origin_px, size_px))| OverlayPreparedTile {
            tile_idx,
            origin_px,
            size_px,
            rgba: rgba_from_overlay_tile(image, origin_px[0], origin_px[1], size_px[0], size_px[1]),
        })
        .collect()
}

/// Spawns a background thread that saves dirty clean-overlay pages to the unsaved staging
/// directory every 30 seconds if there are any pending changes.
///
/// `shutdown` lets the owner stop and join the worker before a structural page transaction. The
/// worker checks it at most once a second, so shutdown never leaves a stale autosave writer racing
/// a page-index remap.
pub fn spawn_overlay_autosave_thread(
    model: Arc<Mutex<CleanOverlaysModel>>,
    unsaved_clean_layers_dir: PathBuf,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);
        loop {
            // Sleep in bounded slices so teardown can join promptly instead of waiting 30 seconds.
            for _ in 0..AUTOSAVE_INTERVAL.as_secs() {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(Duration::from_secs(1));
            }
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            let snapshots = {
                let Ok(mut locked) = model.lock() else {
                    // Model mutex poisoned or dropped — exit.
                    break;
                };
                if !locked.has_unsaved_overlay_changes() {
                    continue;
                }
                locked.take_dirty_save_snapshots()
            };
            if snapshots.is_empty() {
                continue;
            }
            if let Err(err) = save_overlay_snapshots_parallel(
                &unsaved_clean_layers_dir,
                &snapshots,
                Some(shutdown.as_ref()),
            )
            {
                runtime_log::log_error(format!(
                    "[canvas::autosave] failed to autosave dirty overlays; path={}; error={err}",
                    unsaved_clean_layers_dir.display()
                ));
                if let Ok(mut locked) = model.lock() {
                    locked.restore_dirty_save_indexes(snapshots.iter().map(|(idx, _, _)| *idx));
                }
            } else {
                runtime_log::log_info(format!(
                    "[canvas::autosave] dirty overlays saved to {}",
                    unsaved_clean_layers_dir.display()
                ));
            }
        }
    })
}

/// Encodes dirty overlay snapshots to `dir/<stem>.png`, checking cancellation between pages.
///
/// Each entry is `(page_idx, file_stem, rgba_image)`. The directory is created once up front and
/// every snapshot is written to `dir/<stem>.png` via `image::RgbaImage::save`. A shutdown abort is
/// reported as an error so the caller restores all dirty indexes, including pages not yet visited.
///
/// # Errors
/// Returns the first per-page encode/write failure (deterministically the lowest `page_idx` among
/// failures), with the page index and target path attached as context. A failure is propagated as a
/// real error, never silently dropped.
fn save_overlay_snapshots_parallel(
    dir: &Path,
    snapshots: &[(usize, String, Arc<RgbaImage>)],
    shutdown: Option<&AtomicBool>,
) -> anyhow::Result<()> {
    if snapshots.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|err| {
        anyhow::anyhow!(
            "failed to create overlay autosave directory {}: {err}",
            dir.display()
        )
    })?;
    // Encode pages in stable order so cancellation can be observed between individual files.
    let mut failures = Vec::new();
    for (page_idx, stem, image) in snapshots {
        // Cancellation is checked between pages. Encoding one page may still use the image
        // crate's internal parallel work, but shutdown never waits for the rest of the pass.
        if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(anyhow::anyhow!("overlay autosave cancelled during shutdown"));
        }
        let dst = dir.join(format!("{stem}.png"));
        match image.save(&dst) {
            Ok(()) => {}
            Err(err) => failures.push((
                *page_idx,
                anyhow::anyhow!(
                    "failed to encode overlay page {page_idx} to {}: {err}",
                    dst.display()
                ),
            )),
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    // Deterministic order independent of thread scheduling: report the lowest failing page index.
    failures.sort_by_key(|(page_idx, _)| *page_idx);
    let (_, err) = failures.swap_remove(0);
    Err(err)
}

pub(super) fn spawn_canvas_settings_saver_thread()
-> (Sender<Option<CanvasSettingsSaveRequest>>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Option<CanvasSettingsSaveRequest>>();
    let handle = thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let Some(mut latest) = first else {
                break;
            };
            while let Ok(next) = rx.try_recv() {
                let Some(request) = next else {
                    return;
                };
                latest = request;
            }
            if let Err(err) = save_canvas_settings_to_project_file(
                &latest.project_settings_file,
                &latest.snapshot,
            ) {
                runtime_log::log_error(format!(
                    "[canvas::settings] failed to persist project canvas settings; path={}; error={err}",
                    latest.project_settings_file.display()
                ));
            }
            if let Err(err) =
                save_canvas_settings_to_user_file(&latest.user_settings_file, &latest.snapshot)
            {
                runtime_log::log_error(format!(
                    "[canvas::settings] failed to persist user canvas settings; path={}; error={err}",
                    latest.user_settings_file.display()
                ));
            }
        }
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::helpers::rgba_from_overlay_tile;
    use crate::models::clean_overlays_model::save_overlay_snapshots_to;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Self-contained sequential reference for `build_overlay_prepared_tiles_parallel`.
    ///
    /// Walks the same `OVERLAY_TILE_SIDE` grid in row-major order and copies each tile's pixels via
    /// `rgba_from_overlay_tile`, so the parallel builder can be checked for byte-and-order identity
    /// (count, `tile_idx`, `origin_px`, `size_px`, bytes) against a known-correct serial baseline.
    /// Returns an empty `Vec` for a zero-sized image, matching the parallel path.
    fn sequential_overlay_tiles_reference(image: &egui::ColorImage) -> Vec<OverlayPreparedTile> {
        let w = image.size[0];
        let h = image.size[1];
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let mut tiles = Vec::new();
        let mut tile_idx = 0usize;
        let mut y = 0usize;
        while y < h {
            let mut x = 0usize;
            while x < w {
                let tw = (w - x).min(OVERLAY_TILE_SIDE);
                let th = (h - y).min(OVERLAY_TILE_SIDE);
                tiles.push(OverlayPreparedTile {
                    tile_idx,
                    origin_px: [x, y],
                    size_px: [tw, th],
                    rgba: rgba_from_overlay_tile(image, x, y, tw, th),
                });
                tile_idx += 1;
                x += OVERLAY_TILE_SIDE;
            }
            y += OVERLAY_TILE_SIDE;
        }
        tiles
    }

    /// Creates a fresh unique temp directory for a test and returns its path.
    ///
    /// Avoids a `tempfile` dev-dependency; the directory lives under the OS temp dir and is
    /// removed by the caller at the end of the test.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "manhwastudio_workers_test_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Builds a deterministic test overlay where each pixel encodes its coordinates, so a wrong
    /// tile origin/stride would surface as mismatched bytes.
    fn make_test_overlay(w: usize, h: usize) -> egui::ColorImage {
        let mut raw = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                // Channel values derived from coordinates; alpha kept opaque so premultiplied and
                // straight representations stay stable for byte comparison.
                let r = u8::try_from(x % 256).unwrap_or(0);
                let g = u8::try_from(y % 256).unwrap_or(0);
                let b = u8::try_from((x + y) % 256).unwrap_or(0);
                raw.extend_from_slice(&[r, g, b, 255]);
            }
        }
        egui::ColorImage::from_rgba_premultiplied([w, h], &raw)
    }

    #[test]
    fn parallel_tiling_matches_sequential_reference() {
        // Larger than one tile in both axes plus partial edge tiles to exercise boundary math.
        let cases = [
            [1usize, 1usize],
            [OVERLAY_TILE_SIDE, OVERLAY_TILE_SIDE],
            [OVERLAY_TILE_SIDE + 7, OVERLAY_TILE_SIDE + 13],
            [OVERLAY_TILE_SIDE * 2 + 1, OVERLAY_TILE_SIDE + 1],
        ];
        for [w, h] in cases {
            let image = make_test_overlay(w, h);
            let seq = sequential_overlay_tiles_reference(&image);
            let par = build_overlay_prepared_tiles_parallel(&image);
            assert_eq!(
                seq.len(),
                par.len(),
                "tile count differs for {w}x{h}: seq={} par={}",
                seq.len(),
                par.len()
            );
            for (s, p) in seq.iter().zip(par.iter()) {
                assert_eq!(s.tile_idx, p.tile_idx, "tile_idx differs for {w}x{h}");
                assert_eq!(s.origin_px, p.origin_px, "origin differs for {w}x{h}");
                assert_eq!(s.size_px, p.size_px, "size differs for {w}x{h}");
                assert_eq!(s.rgba, p.rgba, "tile bytes differ for {w}x{h}");
            }
        }
    }

    #[test]
    fn parallel_tiling_empty_for_zero_sized_image() {
        for size in [[0usize, 4usize], [4, 0], [0, 0]] {
            let image = egui::ColorImage::from_rgba_premultiplied(size, &[]);
            assert!(build_overlay_prepared_tiles_parallel(&image).is_empty());
        }
    }

    #[test]
    fn parallel_png_encode_matches_sequential_files() {
        let snapshots: Vec<(usize, String, Arc<RgbaImage>)> = (0..4)
            .map(|i| {
                let w = 8 + u32::try_from(i).unwrap_or(0);
                let h = 6 + u32::try_from(i).unwrap_or(0);
                let mut img = RgbaImage::new(w, h);
                for (x, y, px) in img.enumerate_pixels_mut() {
                    let r =
                        u8::try_from((x + y + u32::try_from(i).unwrap_or(0)) % 256).unwrap_or(0);
                    *px = image::Rgba([
                        r,
                        u8::try_from(x % 256).unwrap_or(0),
                        u8::try_from(y % 256).unwrap_or(0),
                        255,
                    ]);
                }
                (i, format!("{:03}", i + 1), Arc::new(img))
            })
            .collect();

        let seq_dir = unique_temp_dir("encode_seq");
        let par_dir = unique_temp_dir("encode_par");
        save_overlay_snapshots_to(&seq_dir, &snapshots).expect("sequential encode must succeed");
        save_overlay_snapshots_parallel(&par_dir, &snapshots, None)
            .expect("parallel encode must succeed");

        for (_, stem, _) in &snapshots {
            let seq_path = seq_dir.join(format!("{stem}.png"));
            let par_path = par_dir.join(format!("{stem}.png"));
            assert!(seq_path.exists(), "sequential file missing: {stem}");
            assert!(par_path.exists(), "parallel file missing: {stem}");
            let seq_bytes = std::fs::read(&seq_path).expect("read sequential png");
            let par_bytes = std::fs::read(&par_path).expect("read parallel png");
            assert_eq!(
                seq_bytes, par_bytes,
                "encoded PNG bytes differ for stem {stem}"
            );
            // The written file must be a valid, decodable PNG with the same pixels.
            let decoded = image::open(&par_path)
                .expect("decode parallel png")
                .to_rgba8();
            let original = snapshots
                .iter()
                .find(|(_, s, _)| s == stem)
                .map(|(_, _, img)| img.clone())
                .expect("snapshot present");
            assert_eq!(decoded.dimensions(), original.dimensions());
            assert_eq!(decoded.as_raw(), original.as_raw());
        }

        let _ = std::fs::remove_dir_all(&seq_dir);
        let _ = std::fs::remove_dir_all(&par_dir);
    }

    #[test]
    fn parallel_png_encode_empty_is_ok_and_writes_nothing() {
        let dir = unique_temp_dir("encode_empty");
        save_overlay_snapshots_parallel(&dir, &[], None).expect("empty encode must succeed");
        // Matches sequential: no directory created, no files written for an empty snapshot set.
        assert!(
            !dir.exists(),
            "no directory should be created for empty input"
        );
    }

    /// A write failure must propagate as `Err` with context, never be silently dropped or
    /// downgraded to `Ok`. Triggered deterministically by pointing `dir` at an EXISTING REGULAR
    /// FILE, so `create_dir_all(dir)` fails (the path exists but is not a directory) before any
    /// PNG is written. The non-empty snapshot set ensures the early `Ok` empty-input path is not
    /// taken.
    #[test]
    fn parallel_png_encode_propagates_create_dir_failure() {
        let base = unique_temp_dir("encode_err");
        std::fs::create_dir_all(&base).expect("create test base dir");
        // A real file standing where the output directory is expected.
        let file_as_dir = base.join("not_a_dir");
        std::fs::write(&file_as_dir, b"occupied").expect("create blocking file");

        let mut img = RgbaImage::new(4, 4);
        for px in img.pixels_mut() {
            *px = image::Rgba([1, 2, 3, 255]);
        }
        let snapshots: Vec<(usize, String, Arc<RgbaImage>)> =
            vec![(7, "001".to_string(), Arc::new(img))];

        let err = save_overlay_snapshots_parallel(&file_as_dir, &snapshots, None)
            .expect_err("create_dir_all over an existing file must fail, not return Ok");
        // The error must carry the offending path as diagnostic context, not be an opaque failure.
        let msg = err.to_string();
        assert!(
            msg.contains(&file_as_dir.display().to_string()),
            "error must include the target directory path; got: {msg}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A per-page encode/write failure (not a missing parent directory) must also propagate as an
    /// `Err` carrying the failing page index and target path. Triggered deterministically by
    /// pre-creating a subdirectory at the exact `dir/<stem>.png` target so `RgbaImage::save` cannot
    /// write a file there. `create_dir_all(dir)` succeeds (the parent is a real directory), so this
    /// exercises the parallel per-tile failure-collection path rather than the up-front guard.
    #[test]
    fn parallel_png_encode_propagates_per_page_write_failure() {
        let dir = unique_temp_dir("encode_page_err");
        std::fs::create_dir_all(&dir).expect("create output dir");
        // Occupy the destination file path with a directory so the PNG save fails.
        std::fs::create_dir_all(dir.join("002.png")).expect("create blocking dir at png target");

        let mut img = RgbaImage::new(4, 4);
        for px in img.pixels_mut() {
            *px = image::Rgba([9, 8, 7, 255]);
        }
        // page_idx 5 maps to stem "002"; only this page targets the blocked path.
        let snapshots: Vec<(usize, String, Arc<RgbaImage>)> =
            vec![(5, "002".to_string(), Arc::new(img))];

        let err = save_overlay_snapshots_parallel(&dir, &snapshots, None)
            .expect_err("saving over an existing directory path must fail, not return Ok");
        let msg = err.to_string();
        assert!(
            msg.contains("page 5"),
            "error must include the failing page index; got: {msg}"
        );
        assert!(
            msg.contains("002.png"),
            "error must include the failing target path; got: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
