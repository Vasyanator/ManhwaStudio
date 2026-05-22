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

Notes:
- Workers must stay non-blocking for the GUI thread.
- Errors are reported through `runtime_log` and returned channels, not ignored.
*/

use super::helpers::build_overlay_prepared_tiles;
use super::settings::{save_canvas_settings_to_project_file, save_canvas_settings_to_user_file};
use super::types::{CanvasSettingsSaveRequest, OverlayPrepareRequest, OverlayPrepareResult};
use crate::models::clean_overlays_model::{CleanOverlaysModel, save_overlay_snapshots_to};
use crate::runtime_log;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(super) fn spawn_overlay_prepare_thread() -> (
    Sender<Option<OverlayPrepareRequest>>,
    Receiver<OverlayPrepareResult>,
    JoinHandle<()>,
) {
    let (tx_req, rx_req) = mpsc::channel::<Option<OverlayPrepareRequest>>();
    let (tx_res, rx_res) = mpsc::channel::<OverlayPrepareResult>();
    let handle = thread::spawn(move || {
        while let Ok(msg) = rx_req.recv() {
            let Some(request) = msg else {
                break;
            };
            let tiles = build_overlay_prepared_tiles(request.image.as_ref());
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

/// Spawns a background thread that saves dirty clean-overlay pages to the unsaved staging
/// directory every 30 seconds if there are any pending changes.
///
/// The thread keeps running until the `Arc` holding `model` is dropped (all strong counts go to
/// zero), at which point `model.lock()` will fail and the loop exits cleanly.
pub fn spawn_overlay_autosave_thread(
    model: Arc<Mutex<CleanOverlaysModel>>,
    unsaved_clean_layers_dir: PathBuf,
) -> JoinHandle<()> {
    thread::spawn(move || {
        const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);
        loop {
            thread::sleep(AUTOSAVE_INTERVAL);
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
            if let Err(err) = save_overlay_snapshots_to(&unsaved_clean_layers_dir, &snapshots) {
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
