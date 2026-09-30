/*
File: crates/ms-canvas/src/workers.rs

Purpose:
Background worker helpers for the canvas subsystem.

Main responsibilities:
- spawn the clean-overlay prepare worker;
- spawn the canvas settings saver worker;
- spawn the overlay autosave worker (saves dirty overlays to the unsaved folder when the project's
  `AutosaveGate` is due — every 30 s without a gate) and own its quiescence handle
  `OverlayAutosaveControl` (pause / flush-and-stop / stop-now);
- keep thread/bootstrap code out of `mod.rs` and runtime buckets.

Key functions:
- spawn_overlay_prepare_thread()
- spawn_canvas_settings_saver_thread()
- spawn_overlay_autosave_thread()
- OverlayAutosaveControl::{pause_blocking, request_flush_and_stop, request_stop_now}
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
use ms_models::autosave_gate::AutosaveGate;
use ms_models::clean_overlays_model::{CleanOverlaysModel, OverlaySaveSnapshot};
use ms_log::runtime_log;
#[cfg(test)]
use image::RgbaImage;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use ms_thread::{self as thread, JoinHandle};
use web_time::{Duration, Instant};

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

/// Lifecycle request observed by the overlay autosave worker (see [`OverlayAutosaveControl`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutosaveMode {
    /// Periodic passes every interval.
    Run,
    /// Run one final full pass as soon as no pause is held, then exit.
    FlushAndStop,
    /// Exit without writing; an in-flight pass is cancelled between pages and its snapshots
    /// are restored to the model's dirty set.
    StopNow,
}

/// Shared state of [`OverlayAutosaveControl`], always read and written under its mutex.
#[derive(Debug)]
struct AutosaveState {
    mode: AutosaveMode,
    /// Number of live [`OverlayAutosavePauseGuard`]s; no pass STARTS while it is non-zero.
    pause_depth: u32,
    /// True from the moment a pass is admitted (dirty set about to be taken) until its writes
    /// and restores are finished. Set under the same lock that checks `pause_depth`, so a
    /// returned `pause_blocking` proves no pass is running or can start.
    writing: bool,
}

/// Quiescence handle shared between the clean-overlay autosave worker and its owner.
///
/// Contract:
/// - [`Self::pause_blocking`] returns only when no autosave pass is running, and no pass starts
///   until every returned guard is dropped. Callers that take the model's dirty set themselves
///   (save-to-project) hold a guard across take → write → merge so an autosave pass can never
///   land pages in the staging dir after the merge removed it.
/// - [`Self::request_flush_and_stop`] makes the worker write every dirty page once (waiting for
///   pauses to lift) and exit; [`Self::request_stop_now`] makes it exit without writing.
///   `StopNow` wins over `FlushAndStop`. Both are non-blocking; the owner joins the thread.
///
/// A poisoned state mutex is recovered (`PoisonError::into_inner`): the state is plain data whose
/// invariants hold between statements, and a stuck control would either block a save forever or
/// leave a writer running into a deleted staging dir.
#[derive(Debug)]
pub struct OverlayAutosaveControl {
    state: Mutex<AutosaveState>,
    cv: Condvar,
}

/// RAII pause of the overlay autosave; dropping it resumes the worker (see
/// [`OverlayAutosaveControl::pause_blocking`]).
#[derive(Debug)]
#[must_use = "the autosave resumes as soon as the guard is dropped"]
pub struct OverlayAutosavePauseGuard {
    control: Arc<OverlayAutosaveControl>,
}

impl Drop for OverlayAutosavePauseGuard {
    fn drop(&mut self) {
        let mut state = self.control.lock_state();
        state.pause_depth = state.pause_depth.saturating_sub(1);
        drop(state);
        self.control.cv.notify_all();
    }
}

impl OverlayAutosaveControl {
    /// Creates a control in `Run` mode with no pause held.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AutosaveState { mode: AutosaveMode::Run, pause_depth: 0, writing: false }),
            cv: Condvar::new(),
        })
    }

    fn lock_state(&self) -> MutexGuard<'_, AutosaveState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Pauses the autosave and BLOCKS until any in-flight pass has finished (at most one page
    /// encode per dirty page). Must never be called on the GUI thread. Returns immediately when
    /// the worker has already exited. Pauses nest; the worker resumes when the last guard drops.
    pub fn pause_blocking(self: &Arc<Self>) -> OverlayAutosavePauseGuard {
        let mut state = self.lock_state();
        state.pause_depth = state.pause_depth.saturating_add(1);
        while state.writing {
            state = self.cv.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        drop(state);
        OverlayAutosavePauseGuard { control: Arc::clone(self) }
    }

    /// Requests one final full pass and exit (non-discard teardown). Non-blocking; ignored when
    /// `StopNow` was already requested.
    pub fn request_flush_and_stop(&self) {
        let mut state = self.lock_state();
        if state.mode == AutosaveMode::Run {
            state.mode = AutosaveMode::FlushAndStop;
        }
        drop(state);
        self.cv.notify_all();
    }

    /// Requests an exit without writing (page operations, discard). Non-blocking; an in-flight
    /// pass is cancelled between pages and restores its snapshots to the dirty set.
    pub fn request_stop_now(&self) {
        self.lock_state().mode = AutosaveMode::StopNow;
        self.cv.notify_all();
    }

    fn stop_now_requested(&self) -> bool {
        self.lock_state().mode == AutosaveMode::StopNow
    }

    /// Blocks until a pass is due (per `schedule`) and admitted, marking `writing = true` under the
    /// lock. Returns `None` when the worker must exit, `Some(true)` for the final (flush-and-stop)
    /// pass. `schedule` is consulted only while no pause is held, so a due window is never consumed
    /// by a pass that cannot run.
    fn wait_for_pass(&self, schedule: &mut PassSchedule) -> Option<bool> {
        let mut state = self.lock_state();
        loop {
            let now = Instant::now();
            match state.mode {
                AutosaveMode::StopNow => return None,
                AutosaveMode::FlushAndStop if state.pause_depth == 0 => {
                    state.writing = true;
                    return Some(true);
                }
                AutosaveMode::Run if state.pause_depth == 0 && schedule.take_due(now) => {
                    state.writing = true;
                    return Some(false);
                }
                AutosaveMode::FlushAndStop | AutosaveMode::Run => {}
            }
            // While paused the deadline is irrelevant (it may already be overdue, which would make the
            // remainder zero and spin): wait a full slice — the pause guard's drop notifies us.
            let wait = if state.pause_depth > 0 { MAX_WAIT_SLICE } else { schedule.wait_hint(now) };
            state = self.cv.wait_timeout(state, wait).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Ends the pass admitted by `wait_for_pass` and wakes pausers.
    fn finish_pass(&self) {
        self.lock_state().writing = false;
        self.cv.notify_all();
    }
}

/// Upper bound on one control wait: notifications make every request prompt, the bound protects
/// against a lost wakeup and makes the worker notice an autosave-gate epoch moved by another writer.
const MAX_WAIT_SLICE: Duration = Duration::from_secs(1);

/// Default period between clean-overlay autosave passes when no autosave gate is installed.
const OVERLAY_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);

/// When the overlay autosave worker runs a regular (non-final) pass.
#[derive(Debug)]
enum PassSchedule {
    /// Fixed period (no autosave gate): a pass every `interval`, measured from the previous pass.
    Interval { interval: Duration, next_pass: Instant },
    /// The project's autosave gate: a pass is due when the gate's flush epoch moved past the epoch
    /// of this worker's last pass. The model's dirty set is the held state.
    Gate { gate: Arc<AutosaveGate>, seen_epoch: u64 },
}

impl PassSchedule {
    /// Builds the schedule: gate-driven when `gate` is present, else the fixed default interval.
    fn new(gate: Option<Arc<AutosaveGate>>, interval: Duration) -> Self {
        match gate {
            Some(gate) => {
                let seen_epoch = gate.poll();
                Self::Gate { gate, seen_epoch }
            }
            None => Self::Interval { interval, next_pass: Instant::now() + interval },
        }
    }

    /// Returns whether a pass is due at `now` and, if so, CONSUMES it (the next interval starts /
    /// the gate epoch is recorded as seen). Called only when the pass will be admitted.
    fn take_due(&mut self, now: Instant) -> bool {
        match self {
            Self::Interval { interval, next_pass } => {
                if now < *next_pass {
                    return false;
                }
                *next_pass = now + *interval;
                true
            }
            Self::Gate { gate, seen_epoch } => {
                let epoch = gate.poll();
                if epoch == *seen_epoch {
                    return false;
                }
                *seen_epoch = epoch;
                true
            }
        }
    }

    /// How long to wait before re-checking `take_due`: bounded to `[1 ms, MAX_WAIT_SLICE]` so an
    /// overdue deadline can never produce a zero-timeout spin.
    fn wait_hint(&self, now: Instant) -> Duration {
        let remaining = match self {
            Self::Interval { next_pass, .. } => next_pass.saturating_duration_since(now),
            Self::Gate { gate, .. } => gate.wait_deadline().unwrap_or(MAX_WAIT_SLICE),
        };
        remaining.clamp(Duration::from_millis(1), MAX_WAIT_SLICE)
    }
}

/// Spawns a background thread that saves dirty clean-overlay pages to the unsaved staging
/// directory.
///
/// With `gate` (the project's `AutosaveGate`) a pass runs when the gate is due (interval since the
/// first pending action, action threshold, or `force_flush`); the model's dirty set is the held
/// state. With `None` a pass runs every 30 seconds (immediate-mode fallback for tools and tests).
/// Either way a pass with nothing dirty writes nothing.
///
/// `control` is the owner's quiescence handle ([`OverlayAutosaveControl`]): page operations and
/// the discard path `request_stop_now` + join; non-discard teardown `request_flush_and_stop` +
/// join (always writes every dirty page, gate or not); save-to-project holds a pause guard across
/// its own take/write/merge.
pub fn spawn_overlay_autosave_thread(
    model: Arc<Mutex<CleanOverlaysModel>>,
    unsaved_clean_layers_dir: PathBuf,
    control: Arc<OverlayAutosaveControl>,
    gate: Option<Arc<AutosaveGate>>,
) -> JoinHandle<()> {
    spawn_overlay_autosave_thread_with_schedule(model, unsaved_clean_layers_dir, control, PassSchedule::new(gate, OVERLAY_AUTOSAVE_INTERVAL))
}

/// [`spawn_overlay_autosave_thread`] with an explicit schedule (tests use a long interval or their
/// own gate).
fn spawn_overlay_autosave_thread_with_schedule(
    model: Arc<Mutex<CleanOverlaysModel>>,
    unsaved_clean_layers_dir: PathBuf,
    control: Arc<OverlayAutosaveControl>,
    mut schedule: PassSchedule,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Some(final_pass) = control.wait_for_pass(&mut schedule) {
            let outcome = run_overlay_autosave_pass(&model, &unsaved_clean_layers_dir, &control);
            control.finish_pass();
            if final_pass || outcome == AutosavePassOutcome::ModelGone {
                return;
            }
        }
    })
}

/// Result of one autosave pass that decides whether the worker keeps running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutosavePassOutcome {
    Continue,
    /// The model mutex is poisoned: the worker exits (nothing trustworthy left to save).
    ModelGone,
}

/// Takes the model's dirty snapshots and writes them to `dir`, restoring them to the dirty set
/// on failure or `StopNow` cancellation. Runs with `writing = true` held by the caller.
fn run_overlay_autosave_pass(
    model: &Arc<Mutex<CleanOverlaysModel>>,
    dir: &Path,
    control: &OverlayAutosaveControl,
) -> AutosavePassOutcome {
    let snapshots = {
        let Ok(mut locked) = model.lock() else {
            runtime_log::log_error("[canvas::autosave] clean-overlay model lock poisoned; autosave worker exits");
            return AutosavePassOutcome::ModelGone;
        };
        if !locked.has_unsaved_overlay_changes() {
            return AutosavePassOutcome::Continue;
        }
        locked.take_dirty_save_snapshots()
    };
    if snapshots.is_empty() {
        return AutosavePassOutcome::Continue;
    }
    if let Err(err) = save_overlay_snapshots_parallel(dir, &snapshots, Some(&|| control.stop_now_requested()), Some(model)) {
        runtime_log::log_error(format!(
            "[canvas::autosave] failed to autosave dirty overlays; path={}; error={err}",
            dir.display()
        ));
        if let Ok(mut locked) = model.lock() {
            locked.restore_dirty_save_snapshots(&snapshots);
        }
    } else {
        runtime_log::log_info(format!("[canvas::autosave] dirty overlays saved to {}", dir.display()));
    }
    AutosavePassOutcome::Continue
}

/// Encodes dirty overlay snapshots to `dir/<stem>.png`, checking `cancelled` between pages.
///
/// The directory is created once up front and every snapshot is written to `dir/<stem>.png`
/// via `image::RgbaImage::save`. A shutdown abort is reported as an error so the caller
/// restores all dirty snapshots, including pages not yet visited.
///
/// When `model` is provided, each page is guarded against a concurrent
/// `CleanOverlaysModel::detach_page_overlay` via the snapshot's detach generation: a page
/// detached before its write is skipped, and a page detached DURING its (lock-free) write has
/// the just-written file removed again, so an in-flight autosave can never resurrect a
/// detached clean layer.
///
/// # Errors
/// Returns the first per-page encode/write failure (deterministically the lowest `page_idx`
/// among failures), with the page index and target path attached as context. A failure is
/// propagated as a real error, never silently dropped.
fn save_overlay_snapshots_parallel(
    dir: &Path,
    snapshots: &[OverlaySaveSnapshot],
    cancelled: Option<&dyn Fn() -> bool>,
    model: Option<&Mutex<CleanOverlaysModel>>,
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
    // Returns whether the snapshot is still current; a poisoned model lock counts as
    // stale so a broken model never gates a write open.
    let snapshot_is_current = |snapshot: &OverlaySaveSnapshot| -> bool {
        model.is_none_or(|model| {
            model
                .lock()
                .map(|guard| guard.overlay_snapshot_is_current(snapshot))
                .unwrap_or(false)
        })
    };
    // Encode pages in stable order so cancellation can be observed between individual files.
    let mut failures = Vec::new();
    for snapshot in snapshots {
        // Cancellation is checked between pages. Encoding one page may still use the image
        // crate's internal parallel work, but shutdown never waits for the rest of the pass.
        if cancelled.is_some_and(|is_cancelled| is_cancelled()) {
            return Err(anyhow::anyhow!("overlay autosave cancelled during shutdown"));
        }
        // Cheap pre-write skip: the page was detached after the snapshot was taken.
        if !snapshot_is_current(snapshot) {
            continue;
        }
        let dst = dir.join(snapshot.file_name());
        match snapshot.image.save(&dst) {
            Ok(()) => {
                // Post-write reconcile: a detach racing the encode above already bumped the
                // generation; remove the stale file (already-trashed/missing is fine).
                if !snapshot_is_current(snapshot) {
                    match std::fs::remove_file(&dst) {
                        Ok(())  => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => runtime_log::log_warn(format!(
                            "[canvas::autosave] could not remove stale overlay written during detach: {}: {err}",
                            dst.display()
                        )),
                    }
                }
            }
            Err(err) => failures.push((
                snapshot.page_idx,
                anyhow::anyhow!(
                    "failed to encode overlay page {} to {}: {err}",
                    snapshot.page_idx,
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
    use crate::helpers::rgba_from_overlay_tile;
    use ms_models::clean_overlays_model::save_overlay_snapshots_to;
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
        let snapshots: Vec<OverlaySaveSnapshot> = (0..4)
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
                OverlaySaveSnapshot {
                    page_idx: i,
                    stem: format!("{:03}", i + 1),
                    image: Arc::new(img),
                    generation: 0,
                }
            })
            .collect();

        let seq_dir = unique_temp_dir("encode_seq");
        let par_dir = unique_temp_dir("encode_par");
        save_overlay_snapshots_to(&seq_dir, &snapshots).expect("sequential encode must succeed");
        save_overlay_snapshots_parallel(&par_dir, &snapshots, None, None)
            .expect("parallel encode must succeed");

        for OverlaySaveSnapshot { stem, .. } in &snapshots {
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
                .find(|snapshot| &snapshot.stem == stem)
                .map(|snapshot| snapshot.image.clone())
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
        save_overlay_snapshots_parallel(&dir, &[], None, None).expect("empty encode must succeed");
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
        let snapshots = vec![OverlaySaveSnapshot {
            page_idx: 7,
            stem: "001".to_string(),
            image: Arc::new(img),
            generation: 0,
        }];

        let err = save_overlay_snapshots_parallel(&file_as_dir, &snapshots, None, None)
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
        let snapshots = vec![OverlaySaveSnapshot {
            page_idx: 5,
            stem: "002".to_string(),
            image: Arc::new(img),
            generation: 0,
        }];

        let err = save_overlay_snapshots_parallel(&dir, &snapshots, None, None)
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

    /// The autosave writer must not persist a snapshot whose page was detached after the
    /// snapshot was taken (deterministic replay of the take -> detach -> write race).
    #[test]
    fn parallel_encode_skips_snapshots_of_detached_pages() {
        let dir = unique_temp_dir("encode_detached");
        let model = Mutex::new(CleanOverlaysModel::new_from_pages(&[PathBuf::from(
            "001.png",
        )]));
        let snapshots = {
            let mut guard = model.lock().expect("model lock");
            let mut img = RgbaImage::new(4, 4);
            for px in img.pixels_mut() {
                *px = image::Rgba([255, 255, 255, 255]);
            }
            guard.replace_from_rgba(0, img);
            guard.take_dirty_save_snapshots()
        };
        assert_eq!(snapshots.len(), 1);
        assert!(model.lock().expect("model lock").detach_page_overlay(0));

        save_overlay_snapshots_parallel(&dir, &snapshots, None, Some(&model))
            .expect("guarded encode must succeed");
        assert!(
            !dir.join("001.png").exists(),
            "a detached page's snapshot must not be written"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Builds a one-page model whose page 0 (stem `001`) is dirty.
    fn dirty_one_page_model() -> Arc<Mutex<CleanOverlaysModel>> {
        let mut model = CleanOverlaysModel::new_from_pages(&[PathBuf::from("001.png")]);
        let mut img = RgbaImage::new(4, 4);
        for px in img.pixels_mut() {
            *px = image::Rgba([10, 20, 30, 255]);
        }
        model.replace_from_rgba(0, img);
        assert!(model.has_unsaved_overlay_changes());
        Arc::new(Mutex::new(model))
    }

    /// Short wait used to observe that something did NOT happen while blocked.
    const NEGATIVE_WAIT: Duration = Duration::from_millis(150);

    /// An interval schedule whose first pass is already due.
    fn due_now() -> PassSchedule {
        PassSchedule::Interval { interval: Duration::from_secs(3600), next_pass: Instant::now() }
    }

    fn test_gate(interval: Duration, action_threshold: u32) -> Arc<AutosaveGate> {
        Arc::new(AutosaveGate::with_policy_fn(move || ms_config::autosave_policy::AutosavePolicy { interval, action_threshold }))
    }

    /// One committed edit of page 0 (a full-page snapshot apply = one `mark_dirty` = one action).
    fn commit_edit(model: &Mutex<CleanOverlaysModel>, value: u8) {
        let mut img = RgbaImage::new(4, 4);
        for px in img.pixels_mut() {
            *px = image::Rgba([value, value, value, 255]);
        }
        model.lock().expect("model lock").replace_from_rgba(0, img);
    }

    /// With a gate, the worker holds the dirty set (no pass) until the gate is due: one action of a
    /// threshold of two writes nothing; the second closes the window and the page is written.
    #[test]
    fn gated_worker_passes_only_when_the_gate_is_due() {
        let dir = unique_temp_dir("autosave_gated");
        let gate = test_gate(Duration::from_secs(3600), 2);
        let model = Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&[PathBuf::from("001.png")])));
        model.lock().expect("model lock").set_autosave_gate(Some(Arc::clone(&gate)));
        let control = OverlayAutosaveControl::new();
        let handle = spawn_overlay_autosave_thread(Arc::clone(&model), dir.clone(), Arc::clone(&control), Some(Arc::clone(&gate)));
        commit_edit(&model, 10);
        thread::sleep(NEGATIVE_WAIT);
        assert!(!dir.join("001.png").exists(), "a pass ran before the gate was due");
        assert!(model.lock().expect("model lock").has_unsaved_overlay_changes(), "the dirty set is the held state");
        commit_edit(&model, 20);
        let start = Instant::now();
        while !dir.join("001.png").exists() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(dir.join("001.png").exists(), "the due gate did not trigger a pass");
        control.request_stop_now();
        handle.join().expect("autosave thread");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `FlushAndStop` writes the held dirty set even though the gate is not due.
    #[test]
    fn gated_flush_and_stop_writes_held_pages() {
        let dir = unique_temp_dir("autosave_gated_flush");
        let gate = test_gate(Duration::from_secs(3600), 1000);
        let model = dirty_one_page_model();
        let control = OverlayAutosaveControl::new();
        let handle = spawn_overlay_autosave_thread(Arc::clone(&model), dir.clone(), Arc::clone(&control), Some(gate));
        thread::sleep(NEGATIVE_WAIT);
        assert!(!dir.join("001.png").exists(), "no pass while the gate is idle");
        control.request_flush_and_stop();
        handle.join().expect("autosave thread");
        assert!(dir.join("001.png").exists(), "flush-and-stop must write the held page");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `pause_blocking` must not return while a pass is admitted (`writing`), and must return
    /// as soon as that pass finishes.
    #[test]
    fn pause_waits_for_in_flight_pass() {
        let control = OverlayAutosaveControl::new();
        assert_eq!(control.wait_for_pass(&mut due_now()), Some(false), "a due pass is admitted");
        let (tx, rx) = mpsc::channel();
        let pauser = {
            let control = Arc::clone(&control);
            thread::spawn(move || {
                let guard = control.pause_blocking();
                tx.send(()).expect("test receiver alive");
                drop(guard);
            })
        };
        assert!(rx.recv_timeout(NEGATIVE_WAIT).is_err(), "pause returned while a pass was writing");
        control.finish_pass();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok(), "pause did not return after the pass ended");
        pauser.join().expect("pauser thread");
    }

    /// While a pause guard is held no pass is admitted, even an overdue one; dropping the guard
    /// admits it.
    #[test]
    fn held_pause_blocks_pass_admission() {
        let control = OverlayAutosaveControl::new();
        let guard = control.pause_blocking();
        let (tx, rx) = mpsc::channel();
        let waiter = {
            let control = Arc::clone(&control);
            thread::spawn(move || {
                let admitted = control.wait_for_pass(&mut due_now());
                tx.send(admitted).expect("test receiver alive");
                control.finish_pass();
            })
        };
        assert!(rx.recv_timeout(NEGATIVE_WAIT).is_err(), "a pass was admitted under a held pause");
        drop(guard);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).ok(), Some(Some(false)));
        waiter.join().expect("waiter thread");
    }

    /// `FlushAndStop` writes every dirty page exactly once and exits, but only after a held
    /// pause is released.
    #[test]
    fn flush_and_stop_writes_once_after_pause_lifts() {
        let dir = unique_temp_dir("autosave_flush_stop");
        let model = dirty_one_page_model();
        let control = OverlayAutosaveControl::new();
        let guard = control.pause_blocking();
        let handle = spawn_overlay_autosave_thread_with_schedule(
            Arc::clone(&model),
            dir.clone(),
            Arc::clone(&control),
            PassSchedule::new(None, Duration::from_secs(3600)),
        );
        control.request_flush_and_stop();
        thread::sleep(NEGATIVE_WAIT);
        assert!(!handle.is_finished(), "flush-and-stop must wait for the pause to lift");
        assert!(!dir.join("001.png").exists(), "nothing may be written under a pause");
        drop(guard);
        handle.join().expect("autosave thread");
        assert!(dir.join("001.png").exists(), "the final pass must write the dirty page");
        assert!(!model.lock().expect("model lock").has_unsaved_overlay_changes());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `StopNow` exits without writing and leaves the dirty set intact; it also wins over an
    /// earlier `FlushAndStop`.
    #[test]
    fn stop_now_exits_without_writing() {
        let dir = unique_temp_dir("autosave_stop_now");
        let model = dirty_one_page_model();
        let control = OverlayAutosaveControl::new();
        let guard = control.pause_blocking();
        let handle = spawn_overlay_autosave_thread_with_schedule(
            Arc::clone(&model),
            dir.clone(),
            Arc::clone(&control),
            PassSchedule::new(None, Duration::from_secs(3600)),
        );
        control.request_flush_and_stop();
        control.request_stop_now();
        drop(guard);
        handle.join().expect("autosave thread");
        assert!(!dir.exists(), "stop-now must not write");
        assert!(model.lock().expect("model lock").has_unsaved_overlay_changes());
    }

    /// A pass cancelled by `StopNow` (observed between pages) restores its taken snapshots to
    /// the model's dirty set instead of losing them.
    #[test]
    fn stop_now_mid_pass_restores_dirty_snapshots() {
        let dir = unique_temp_dir("autosave_stop_mid_pass");
        let model = dirty_one_page_model();
        let control = OverlayAutosaveControl::new();
        assert_eq!(control.wait_for_pass(&mut due_now()), Some(false));
        // The stop arrives after the pass was admitted, i.e. while it is `writing`.
        control.request_stop_now();
        let outcome = run_overlay_autosave_pass(&model, &dir, &control);
        control.finish_pass();
        assert_eq!(outcome, AutosavePassOutcome::Continue);
        assert!(!dir.join("001.png").exists(), "a cancelled pass must not write the page");
        assert!(
            model.lock().expect("model lock").has_unsaved_overlay_changes(),
            "cancelled snapshots must be restored to the dirty set"
        );
        assert_eq!(control.wait_for_pass(&mut due_now()), None, "the worker exits after stop-now");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
