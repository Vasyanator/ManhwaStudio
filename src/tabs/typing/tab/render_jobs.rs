/*
File: tab/render_jobs.rs

Purpose:
Background render/job orchestration for the typing tab on `TypingTextOverlayLayer`:
edit-render token bookkeeping, project loader + eager v3 migration workers, create /
raster / raster-effects / edit-overlay job polling and dispatch, and the shape-variant
preview lifecycle (render, poll, draw, apply).

Notes:
Extracted verbatim from `tab.rs`. Methods are `pub(super)` so `tab.rs` and sibling
submodules of `tab` can use them. `use super::*;` pulls in the parent module's types and
imports. Struct/enum definitions and the rest of the big `impl TypingTextOverlayLayer`
block remain in `tab.rs`; these methods reach the private items that stay there as
descendants of module `tab`.
*/

use super::*;

impl TypingTextOverlayLayer {
    pub(super) fn next_edit_render_token(&mut self) -> u64 {
        self.edit_render_next_token = self.edit_render_next_token.wrapping_add(1);
        self.edit_render_latest_token
            .store(self.edit_render_next_token, Ordering::Release);
        self.edit_render_next_token
    }

    pub(super) fn cancel_active_edit_overlay_render(&mut self) {
        self.next_edit_render_token();
        self.edit_render_rx = None;
    }

    pub(super) fn set_page_count(&mut self, page_count: usize) {
        self.page_count = page_count;
    }

    pub(super) fn set_clean_overlays_model(&mut self, model: Option<Arc<Mutex<CleanOverlaysModel>>>) {
        self.clean_overlays_model = model;
    }

    pub(super) fn ensure_loader_started(&mut self, project: &ProjectData) {
        let project_dir = project.project_dir.clone();
        if self.loaded_project_dir.as_ref() == Some(&project_dir) {
            return;
        }
        if self.loading_project_dir.as_ref() == Some(&project_dir) {
            return;
        }

        self.overlays.clear();
        self.pending_upload_indices.clear();
        self.pending_upload_set.clear();
        self.last_load_error = None;
        self.create_selection = None;
        self.create_editor = None;
        self.create_render_state = None;
        self.create_status_error = None;
        self.save_rx = None;
        self.save_requested_while_busy = false;
        self.migration_rx = None;
        self.pending_migration = None;
        self.export_rx = None;
        self.export_status = TypingExportUiStatus::Hidden;
        self.cancel_active_edit_overlay_render();
        self.edit_render_data_dirty = false;
        self.last_selected_overlay_idx = None;
        self.selected_overlay_idx = None;
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
        self.drag_has_changes = false;
        self.auto_typing_job = None;
        self.auto_typing_debug_visual = None;
        self.auto_typing_next_token = 0;
        self.loaded_project_dir = None;
        self.loaded_text_images_dir = None;

        // Text overlays now live in the chapter's `layers/` folder. Saves go to the unsaved
        // staging `layers/` dir; reads prefer it, then the committed `layers/` dir. Chapters that
        // predate this move still keep their overlays under the legacy `text_images/` folder, so
        // that is used as the committed read source until the next save migrates them into
        // `layers/`. Page masks are a separate store and stay under `text_images/`.
        let unsaved_layers_dir = project.paths.unsaved_layers_dir.clone();
        let main_layers_dir = project.paths.layers_dir.clone();
        let legacy_text_images_dir = project.paths.text_images_dir.clone();

        // Capture the dirs used to read read-only PS raster layers (Task B) and force a reload of
        // the raster cache for the current page on this project (re)load.
        self.layers_primary_dir = Some(unsaved_layers_dir.clone());
        self.layers_fallback_dir = Some(main_layers_dir.clone());
        self.raster_layers_by_page.clear();
        self.bands_by_page.clear();

        // Committed (non-staging) read source: migrated chapters have `text_info.json` under
        // `layers/`; older ones only under the legacy `text_images/` dir. Used as the save-time
        // fallback for locating original image PNGs.
        let committed_read_dir = if main_layers_dir.join(TEXT_INFO_FILE_NAME).is_file() {
            main_layers_dir.clone()
        } else if legacy_text_images_dir.join(TEXT_INFO_FILE_NAME).is_file() {
            legacy_text_images_dir.clone()
        } else {
            main_layers_dir.clone()
        };

        // Saves always go to the unsaved staging dir.
        self.text_images_save_dir = Some(unsaved_layers_dir.clone());
        // The committed dir is a read fallback: original image PNGs may still live only there
        // (including a legacy `text_images/` for not-yet-migrated chapters).
        self.text_images_fallback_dir = Some(committed_read_dir.clone());

        // Loading reads `text_info.json` from the first candidate that has it, and resolves each
        // overlay PNG from that dir then every later one. The order — unsaved staging, committed
        // `layers/`, legacy `text_images/` — means an old chapter's PNGs are still found after its
        // metadata has migrated into `layers/`.
        let candidate_dirs = [unsaved_layers_dir, main_layers_dir, legacy_text_images_dir];
        let primary_idx = candidate_dirs
            .iter()
            .position(|d| d.join(TEXT_INFO_FILE_NAME).is_file())
            .unwrap_or(0);
        let primary_load_dir = candidate_dirs[primary_idx].clone();
        let fallback_load_dirs: Vec<PathBuf> = candidate_dirs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != primary_idx)
            .map(|(_, d)| d.clone())
            .collect();

        let page_paths = project
            .pages
            .iter()
            .map(|page| (page.idx, page.path.clone()))
            .collect::<Vec<_>>();
        // Cache page image paths for lazy page-pixel-size resolution (legacy overlay uv→px), and drop
        // any stale sizes from a previous project.
        self.page_image_paths = page_paths.iter().cloned().collect();
        self.page_sizes_px.clear();
        let (tx, rx) = mpsc::channel::<TypingOverlayLoadResponse>();
        let project_dir_for_thread = project_dir.clone();
        let primary_load_dir_for_thread = primary_load_dir.clone();
        thread::spawn(move || {
            let page_sizes = load_typing_page_sizes(&page_paths);
            let fallback_refs: Vec<&Path> = fallback_load_dirs.iter().map(PathBuf::as_path).collect();
            let result = load_typing_overlays_from_dir(
                &primary_load_dir_for_thread,
                &fallback_refs,
                &page_sizes,
            );
            let _ = tx.send((project_dir_for_thread, result));
        });
        self.loading_project_dir = Some(project_dir);
        self.loading_text_images_dir = Some(primary_load_dir);
        self.loading_rx = Some(rx);

        // EAGER one-shot migration: if this is a legacy chapter (a `text_info.json` not yet fully
        // inlined into v3 `layers.json`), convert the WHOLE chapter to v3 on disk once, in the
        // background. Pixels are preserved by renaming the overlay PNGs; `text_info.json` becomes
        // `.bak` LAST. RECORD the request now (cheap detection); it is STARTED only after the initial
        // overlay load completes (`poll_loader`), so the migration never races the loader on the
        // overlay PNGs it renames.
        use crate::models::layer_model::migrate;
        self.pending_migration = None;
        let committed_layers = project.paths.layers_dir.clone();
        let legacy_text_images = project.paths.text_images_dir.clone();
        if migrate::chapter_needs_migration(&committed_layers, &legacy_text_images).is_some() {
            let page_paths = project
                .pages
                .iter()
                .map(|page| (page.idx, page.path.clone()))
                .collect::<Vec<_>>();
            self.pending_migration = Some((
                committed_layers,
                legacy_text_images,
                project.paths.unsaved_layers_dir.clone(),
                page_paths,
            ));
        }
    }

    /// Spawns the eager chapter-migration worker for the `pending_migration` request captured at open.
    /// Called once the initial overlay load has completed, so the migration (which renames overlay
    /// PNGs) does not race the loader. The result is polled by `poll_migration`.
    pub(super) fn start_pending_migration(&mut self) {
        use crate::models::layer_model::migrate;
        if self.migration_rx.is_some() {
            return;
        }
        let Some((committed_layers, legacy_text_images, unsaved_layers, page_paths)) =
            self.pending_migration.take()
        else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let page_sizes = load_typing_page_sizes(&page_paths);
            let result = migrate::migrate_chapter_to_v3(
                &committed_layers,
                &legacy_text_images,
                Some(&unsaved_layers),
                &page_sizes,
            );
            let _ = tx.send(result);
        });
        self.migration_rx = Some(rx);
    }

    /// Polls the eager migration worker. On success, evicts the migrated doc pages so both tabs
    /// re-project the v3 data; drops the page caches so they reload. Returns true when it completed.
    pub(super) fn poll_migration(&mut self) -> bool {
        let Some(rx) = self.migration_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.migration_rx = None;
                match result {
                    Ok(report) => {
                        if !report.migrated_pages.is_empty() {
                            crate::runtime_log::log_info(format!(
                                "[migrate] chapter migrated to v3: {} overlays, {} PNGs renamed, {} missing, {} pages; backup {:?}",
                                report.migrated_overlays,
                                report.renamed_pngs,
                                report.missing_pngs,
                                report.migrated_pages.len(),
                                report.backup_path
                            ));
                            // Evict the migrated pages from the shared doc so both tabs re-project the
                            // v3 inline data (the doc version bump drives their per-frame reproject).
                            if let Some(doc) = self.layer_doc.clone()
                                && let Ok(mut guard) = doc.lock()
                            {
                                for &page in &report.migrated_pages {
                                    guard.evict_page(page);
                                }
                            }
                            // Drop the local per-page caches so they reload the migrated bands/text.
                            for &page in &report.migrated_pages {
                                self.bands_by_page.remove(&page);
                                self.raster_layers_by_page.remove(&page);
                            }
                        }
                    }
                    Err(err) => crate::runtime_log::log_warn(format!(
                        "[migrate] chapter migration failed (will retry on next open, text_info.json intact): {err}"
                    )),
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.migration_rx = None;
                crate::runtime_log::log_warn("[migrate] migration worker disconnected");
                true
            }
        }
    }

    pub(super) fn poll_loader(&mut self) -> bool {
        let Some(rx) = self.loading_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok((project_dir, result)) => {
                self.loading_rx = None;
                self.loading_project_dir = None;
                self.loaded_project_dir = Some(project_dir);
                match result {
                    Ok(decoded) => {
                        self.loaded_text_images_dir = self.loading_text_images_dir.take();
                        // MERGE by (uid, page) instead of wholesale-replace, so doc-created runtimes
                        // (materialized by an early `sync_from_doc` on a MIGRATED chapter, whose loader
                        // returns an empty set) are NOT wiped on loader completion. See
                        // `merge_loaded_overlays`. Only the merged-in entries are (re)queued for upload;
                        // doc-created runtimes keep whatever upload state `sync_from_doc` gave them.
                        let touched = merge_loaded_overlays(&mut self.overlays, decoded);
                        for idx in touched {
                            self.queue_overlay_texture_upload(idx);
                        }
                        self.export_rx = None;
                        self.export_status = TypingExportUiStatus::Hidden;
                        self.last_load_error = None;
                        self.cancel_active_edit_overlay_render();
                        self.edit_render_data_dirty = false;
                        self.last_selected_overlay_idx = None;
                        self.selected_overlay_idx = None;
                        self.transform_mode_overlay_idx = None;
                        self.drag_state = None;
                        self.drag_has_changes = false;
                        self.auto_typing_job = None;
                        self.auto_typing_debug_visual = None;
                    }
                    Err(err) => {
                        // Do NOT wholesale-clear `overlays` (same class as the merge fix): on a CORRUPT
                        // / unreadable `text_info.json` the doc-created runtimes are authoritative and
                        // must survive — clearing would wipe text the user is editing. Just record the
                        // error and log it; keep the existing runtimes + their upload queue intact.
                        self.loading_text_images_dir = None;
                        self.loaded_text_images_dir = None;
                        crate::runtime_log::log_warn(format!(
                            "[typing] overlay load failed (keeping doc-created runtimes): {err}"
                        ));
                        self.export_rx = None;
                        self.export_status = TypingExportUiStatus::Hidden;
                        self.last_load_error = Some(err);
                        self.cancel_active_edit_overlay_render();
                        self.edit_render_data_dirty = false;
                    }
                }
                // The initial overlay load is done reading `text_info.json` + the overlay PNGs, so it is
                // now safe to start the eager migration (which renames those PNGs) without a race.
                self.start_pending_migration();
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.loading_rx = None;
                self.loading_project_dir = None;
                self.loading_text_images_dir = None;
                self.loaded_text_images_dir = None;
                self.last_load_error =
                    Some(t!("typing.render.text_info_load_error").to_string());
                self.cancel_active_edit_overlay_render();
                self.edit_render_data_dirty = false;
                self.last_selected_overlay_idx = None;
                self.selected_overlay_idx = None;
                self.transform_mode_overlay_idx = None;
                self.drag_state = None;
                self.drag_has_changes = false;
                self.auto_typing_job = None;
                self.auto_typing_debug_visual = None;
                self.pending_upload_indices.clear();
                self.pending_upload_set.clear();
                self.export_rx = None;
                self.export_status = TypingExportUiStatus::Hidden;
                true
            }
        }
    }

    pub(super) fn poll_create_overlay_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(state) = self.create_render_state.as_ref() else {
                return false;
            };
            match state.rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    t!("typing.render.create_overlay_channel_error").to_string(),
                )),
            }
        };

        let Some(recv_result) = recv_result else {
            return false;
        };
        self.create_render_state = None;

        match recv_result {
            Ok(Ok(decoded)) => {
                crate::trace_log!(
                    cat::SYNC,
                    "create_overlay_render result=ok uid={} kind={:?} size={}x{} warnings={}",
                    decoded.uid,
                    decoded.kind,
                    decoded.size_px[0],
                    decoded.size_px[1],
                    decoded.warnings.len()
                );
                if !decoded.warnings.is_empty() {
                    self.set_create_warning(ctx, decoded.warnings.join("; "));
                }
                self.insert_runtime_overlay(decoded);
                self.request_overlay_placement_save();
                true
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::SYNC, "create_overlay_render result=err err={}", err);
                self.set_create_error(ctx, err);
                true
            }
        }
    }

    /// Drops the cached raster layers + bands for `page_idx` so they reload from disk (authoritative)
    /// on the next `ensure_raster_layers_for_page`.
    pub(super) fn invalidate_raster_cache_for_page(&mut self, page_idx: usize) {
        self.raster_layers_by_page.remove(&page_idx);
        self.bands_by_page.remove(&page_idx);
        // Evict the page from the shared doc too, so the next `ensure_raster_layers_for_page`
        // reloads it from disk (where a worker just wrote a new raster) and re-projects.
        if let Some(doc) = &self.layer_doc
            && let Ok(mut guard) = doc.lock()
        {
            guard.evict_page(page_idx);
        }
        // Drop this page's raster texture-generation cache so re-projected nodes re-upload cleanly.
        self.raster_texture_generations
            .retain(|(p, _), _| *p != page_idx);
    }

    /// Polls the "create raster from external image" worker. On success the page's raster cache is
    /// reloaded from disk and the new raster is selected; the cross-tab revision is bumped (PS picks
    /// it up). Mirrors `poll_create_overlay_jobs`.
    pub(super) fn poll_create_raster_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(state) = self.create_raster_state.as_ref() else {
                return false;
            };
            match state.rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err(t!("typing.render.create_raster_channel_error").to_string()))
                }
            }
        };
        let Some(recv_result) = recv_result else {
            return false;
        };
        self.create_raster_state = None;
        match recv_result {
            Ok(created) => {
                self.invalidate_raster_cache_for_page(created.page_idx);
                self.pending_select_raster_uid = Some((created.page_idx, created.uid));
                true
            }
            Err(err) => {
                self.set_create_error(ctx, err);
                true
            }
        }
    }

    /// Applies a scale/rotation edit from the image panel to a raster layer (by uid), persisting the
    /// transform. Rotation arrives in degrees (panel space) and is converted to radians.
    pub(super) fn apply_raster_transform_edit(
        &mut self,
        page_idx: usize,
        uid: &str,
        user_scale: f32,
        rotation_deg: f32,
    ) {
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|ls| ls.iter_mut().find(|l| l.uid == uid))
        else {
            return;
        };
        layer.transform.scale = user_scale.clamp(0.05, 20.0);
        layer.transform.rotation = rotation_deg.to_radians();
        let transform = layer.transform;
        self.persist_raster_transform(page_idx, uid, transform);
    }

    /// Applies an effects edit (non-destructive) from the image panel to a raster: updates the
    /// transform, then spawns a worker that renders the effects chain from the ORIGINAL base image.
    /// `poll_raster_effects_jobs` writes the rendered PNG (or clears it) and persists the chain via
    /// `update_raster_effects`, leaving the base untouched so the effects stay reversible.
    pub(super) fn apply_raster_effects_edit(
        &mut self,
        page_idx: usize,
        uid: &str,
        render_data_json: &Value,
        user_scale: f32,
        rotation_deg: f32,
    ) {
        self.apply_raster_transform_edit(page_idx, uid, user_scale, rotation_deg);
        if self.raster_effects_state.is_some() {
            // A render is already in flight: stash the latest request (superseding any older
            // pending one) so `poll_raster_effects_jobs` reapplies it once the current render
            // finishes. Otherwise this edit would be silently lost — e.g. effecting a second
            // raster right after a first, leaving the second without its effects on save.
            self.pending_raster_effects = Some((
                page_idx,
                uid.to_string(),
                render_data_json.clone(),
                user_scale,
                rotation_deg,
            ));
            return;
        }
        let effects: Vec<Value> = render_data_json
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(layer) = self
            .raster_layers_by_page
            .get(&page_idx)
            .and_then(|ls| ls.iter().find(|l| l.uid == uid))
        else {
            return;
        };
        let base_file = layer.base_file.clone();
        let uid = uid.to_string();
        let primary = self.layers_primary_dir.clone();
        let fallback = self.layers_fallback_dir.clone();
        // Prefer the resident doc's in-memory base pixels: a freshly created raster (e.g. a cut)
        // may not be flushed to disk yet, so the disk-only load would fail. Clone the pixels and
        // drop the guard BEFORE spawning so the lock is never held across the worker thread.
        let base_in_memory: Option<ColorImage> = self
            .layer_doc
            .clone()
            .and_then(|doc| doc.lock().ok().and_then(|g| g.raster_base_image(page_idx, &uid)));
        let (tx, rx) = mpsc::channel::<Result<TypingRasterEffectsResult, String>>();
        thread::spawn(move || {
            let _ = tx.send(render_raster_effects(
                page_idx,
                uid,
                base_file,
                primary,
                fallback,
                effects,
                base_in_memory,
            ));
        });
        self.raster_effects_state = Some(rx);
    }

    /// Polls the non-destructive raster-effects worker: swaps the cached display image, persists the
    /// chain (`update_raster_effects` — base untouched), and bumps the cross-tab revision.
    pub(super) fn poll_raster_effects_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv = {
            let Some(rx) = self.raster_effects_state.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err(t!("typing.render.raster_effects_channel_error").to_string()))
                }
            }
        };
        let Some(recv) = recv else {
            return false;
        };
        self.raster_effects_state = None;
        let result = match recv {
            Ok(r) => r,
            Err(err) => {
                self.set_create_error(ctx, err);
                return true;
            }
        };

        // Route the rendered display image + effects chain to the shared doc (the source of truth),
        // then re-project. `set_effects` bumps the node generation, so `sync_from_doc` re-uploads
        // the raster texture. Falls back to mutating the local cache directly if no doc is wired.
        let routed = {
            let page = result.page_idx;
            let uid = result.uid.clone();
            let effects = result.effects.clone();
            let display = result.display_image.clone();
            self.route_to_doc(page, move |doc| doc.set_effects(page, &uid, effects, display))
        };
        if !routed
            && let Some(layer) = self
                .raster_layers_by_page
                .get_mut(&result.page_idx)
                .and_then(|ls| ls.iter_mut().find(|l| l.uid == result.uid))
        {
            layer.image = result.display_image.clone();
            layer.effects = result.effects.clone();
            layer.texture = None; // size may have changed → re-upload on next draw
        }
        if let Some(dir) = self.layers_primary_dir.clone() {
            let rendered = if result.effects.is_empty() {
                None
            } else {
                Some(&result.display_image)
            };
            let fallback = self.layers_fallback_dir.clone();
            // ASYNC: route the effects persist through the doc's effects-only saver path (targeted
            // single-raster RMW, PNG encode off-thread). Falls back to a direct synchronous
            // `update_raster_effects` when no doc/saver is wired. The save-to-project / app-close
            // barriers guarantee the enqueued effects land before merge/exit.
            let effects_persist = self
                .layer_doc
                .as_ref()
                .and_then(|doc| {
                    doc.lock().ok().map(|guard| {
                        guard.enqueue_raster_effects(
                            result.page_idx,
                            &dir,
                            fallback.as_deref(),
                            &result.uid,
                            &result.effects,
                            rendered,
                        )
                    })
                })
                .unwrap_or_else(|| {
                    crate::models::layer_model::persist::update_raster_effects(
                        &dir,
                        result.page_idx,
                        &result.uid,
                        &result.effects,
                        rendered,
                        fallback.as_deref(),
                    )
                });
            if let Err(err) = effects_persist {
                crate::runtime_log::log_warn(format!("[typing] persist raster effects: {err}"));
            }
        }
        // Reapply an edit that arrived while this render was in flight, so the last requested
        // effects (e.g. on a second raster) are not lost. `raster_effects_state` is now `None`,
        // so this spawns a fresh render instead of re-stashing.
        if let Some((page_idx, uid, render_data_json, user_scale, rotation_deg)) =
            self.pending_raster_effects.take()
        {
            self.apply_raster_effects_edit(
                page_idx,
                &uid,
                &render_data_json,
                user_scale,
                rotation_deg,
            );
        }
        true
    }

    pub(super) fn poll_edit_overlay_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(rx) = self.edit_render_rx.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    t!("typing.render.edit_overlay_channel_error")
                        .to_string(),
                )),
            }
        };
        let Some(recv_result) = recv_result else {
            return false;
        };

        self.edit_render_rx = None;
        let mut repainted = false;
        match recv_result {
            Ok(Ok(Some(result))) => {
                crate::trace_log!(
                    cat::SYNC,
                    "edit_overlay_render result=ok token={} overlay_idx={} image_effects={} size={}x{} warnings={}",
                    result.token,
                    result.overlay_idx,
                    result.is_image_effects,
                    result.size_px[0],
                    result.size_px[1],
                    result.warnings.len()
                );
                if !result.warnings.is_empty() {
                    self.set_create_warning(ctx, result.warnings.join("; "));
                }
                repainted |= self.apply_edit_overlay_render_result(result);
            }
            Ok(Ok(None)) => {
                crate::trace_log!(cat::SYNC, "edit_overlay_render result=none (skipped/cancelled)");
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::SYNC, "edit_overlay_render result=err err={}", err);
                self.set_create_error(ctx, err);
                repainted = true;
            }
        }

        if self.edit_render_rx.is_none()
            && self.save_requested_while_busy
            && self.save_rx.is_none()
            && self.create_render_state.is_none()
        {
            self.save_requested_while_busy = false;
            self.spawn_overlay_placement_save();
            repainted = true;
        }

        repainted
    }

    pub(super) fn apply_edit_overlay_render_result(&mut self, result: TypingEditOverlayResult) -> bool {
        if self.edit_render_latest_token.load(Ordering::Acquire) != result.token {
            return false;
        }
        {
            let Some(overlay) = self.overlays.get_mut(result.overlay_idx) else {
                return false;
            };
            if result.is_image_effects {
                // У image-эффектов имя показываемого файла может смениться (исходник <-> `_fx`),
                // поэтому идентичность оверлея проверяем по виду, а не по имени файла.
                if overlay.kind != TypingOverlayKind::Image {
                    return false;
                }
                overlay.file_name = result.file_name;
                overlay.original_file_name = result.image_original_file_name;
            } else if overlay.file_name != result.file_name {
                return false;
            }

            overlay.user_scale = result.user_scale.clamp(0.05, 20.0);
            overlay.angle_deg = normalize_angle_deg(result.rotation_deg);
            overlay.render_data_json = Some(result.render_data_json.clone());
            overlay.size_px = result.size_px;
            overlay.source_rgba = result.rgba.clone();
        }
        // Route a TEXT overlay's re-render (render_data + rendered image) to the shared doc, the
        // source of truth for text MODEL state, then re-project. Image-effect overlays are not doc
        // nodes (they are local image overlays), so they keep only the local runtime mutation above.
        if !result.is_image_effects
            && let Some(overlay) = self.overlays.get(result.overlay_idx)
            && overlay.kind == TypingOverlayKind::Text
            && overlay.size_px[0] > 0
            && overlay.size_px[1] > 0
            && result.rgba.len() == result.size_px[0] * result.size_px[1] * 4
        {
            let page_idx = overlay.page_idx;
            let uid = overlay.uid.clone();
            let render_data = result.render_data_json.clone();
            let image =
                ColorImage::from_rgba_unmultiplied(result.size_px, result.rgba.as_slice());
            self.route_to_doc(page_idx, move |doc| {
                doc.set_text_render(page_idx, &uid, render_data, image);
            });
        }
        // A vector-transform re-render (settle/reset) just changed the overlay's baked pixels and warp;
        // drop the cached un-warped base so the Phase-3b live preview re-derives it from the fresh state
        // on the next drag (settle ⇒ re-render un-warped; reset ⇒ reuse the fresh un-warped source_rgba).
        if self.transform_mode_kind == TypingTransformModeKind::Vector
            && self.transform_mode_overlay_idx == Some(result.overlay_idx)
        {
            self.vector_transform_base = None;
        }
        self.mark_overlay_pixels_dirty(result.overlay_idx);
        self.edit_render_data_dirty = true;
        true
    }

    pub(super) fn queue_selected_overlay_edit_request(
        &mut self,
        ctx: &egui::Context,
        request: TypingOverlayEditRequest,
    ) {
        match request {
            TypingOverlayEditRequest::ImageTransform {
                target,
                user_scale,
                rotation_deg,
            } => match target {
                TypingEditTarget::Raster { page_idx, uid } => {
                    self.apply_raster_transform_edit(page_idx, &uid, user_scale, rotation_deg);
                }
                TypingEditTarget::Overlay(overlay_idx) => {
                    if self.selected_overlay_idx != Some(overlay_idx) {
                        return;
                    }
                    {
                        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
                            return;
                        };
                        if overlay.kind != TypingOverlayKind::Image {
                            return;
                        }
                        overlay.user_scale = user_scale.clamp(0.05, 20.0);
                        overlay.angle_deg = normalize_angle_deg(rotation_deg);
                    }
                    self.mark_overlay_geometry_changed(overlay_idx, false);
                    self.request_overlay_placement_save();
                }
            },
            TypingOverlayEditRequest::ImageEffects {
                target,
                render_data_json,
                user_scale,
                rotation_deg,
            } => match target {
                TypingEditTarget::Raster { page_idx, uid } => {
                    self.apply_raster_effects_edit(
                        page_idx,
                        &uid,
                        &render_data_json,
                        user_scale,
                        rotation_deg,
                    );
                }
                TypingEditTarget::Overlay(overlay_idx) => {
                    // Re-render пишет в неподтверждённую staging-папку.
                    let Some(text_images_dir) = self.text_images_save_dir.clone() else {
                        self.set_create_error(
                            ctx,
                            t!("typing.render.text_images_dir_missing_image_error"),
                        );
                        return;
                    };
                    if self.selected_overlay_idx != Some(overlay_idx) {
                        return;
                    }
                    let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
                        return;
                    };
                    if overlay.kind != TypingOverlayKind::Image {
                        return;
                    }
                    overlay.user_scale = user_scale.clamp(0.05, 20.0);
                    overlay.angle_deg = normalize_angle_deg(rotation_deg);

                    let edit_request = TypingEditImageEffectsRequest {
                        token: 0,
                        latest_token: Arc::clone(&self.edit_render_latest_token),
                        overlay_idx,
                        file_name: overlay.file_name.clone(),
                        original_file_name: overlay.original_file_name.clone(),
                        text_images_dir,
                        fallback_text_images_dir: self.text_images_fallback_dir.clone(),
                        user_scale: overlay.user_scale,
                        rotation_deg: overlay.angle_deg,
                        render_data_json,
                    };

                    self.start_edit_image_effects_render_job(edit_request);
                }
            },
            TypingOverlayEditRequest::Text {
                overlay_idx,
                render_params,
                render_data_json,
                user_scale,
                rotation_deg,
            } => {
                let render_params = *render_params;
                // Re-render writes to the unsaved staging dir.
                let Some(text_images_dir) = self.text_images_save_dir.clone() else {
                    self.set_create_error(
                        ctx,
                        t!("typing.errors.text_images_dir_missing_overlay"),
                    );
                    return;
                };
                if self.selected_overlay_idx != Some(overlay_idx) {
                    return;
                }
                let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
                    return;
                };
                if overlay.kind != TypingOverlayKind::Text {
                    return;
                }
                overlay.user_scale = user_scale.clamp(0.05, 20.0);
                overlay.angle_deg = normalize_angle_deg(rotation_deg);

                let edit_request = TypingEditOverlayRequest {
                    token: 0,
                    latest_token: Arc::clone(&self.edit_render_latest_token),
                    overlay_idx,
                    file_name: overlay.file_name.clone(),
                    text_images_dir,
                    user_scale: overlay.user_scale,
                    rotation_deg: overlay.angle_deg,
                    render_params,
                    render_data_json,
                    font_provider: Arc::clone(&self.font_provider),
                };

                self.start_edit_overlay_render_job(edit_request);
            }
        }
    }

    /// Updates the tab-side font source used by background render workers. Called
    /// each frame from the canvas hook so a font reload in the panel propagates here.
    pub(in crate::tabs::typing) fn set_font_provider(&mut self, provider: Arc<dyn FontProvider>) {
        self.font_provider = provider;
    }

    pub(super) fn start_edit_overlay_render_job(&mut self, mut request: TypingEditOverlayRequest) {
        request.token = self.next_edit_render_token();
        let preempted = self.edit_render_rx.is_some();
        crate::trace_log!(
            cat::SYNC,
            "edit_overlay_render dispatch kind=text token={} overlay_idx={} scale={:.3} rot={:.1} preempted_prev={}",
            request.token,
            request.overlay_idx,
            request.user_scale,
            request.rotation_deg,
            preempted
        );
        let (tx, rx) = mpsc::channel::<Result<Option<TypingEditOverlayResult>, String>>();
        thread::spawn(move || {
            let result = render_and_store_edited_overlay(request);
            let _ = tx.send(result);
        });
        self.edit_render_rx = Some(rx);
    }

    pub(super) fn start_edit_image_effects_render_job(&mut self, mut request: TypingEditImageEffectsRequest) {
        request.token = self.next_edit_render_token();
        let preempted = self.edit_render_rx.is_some();
        crate::trace_log!(
            cat::SYNC,
            "edit_overlay_render dispatch kind=image_effects token={} overlay_idx={} scale={:.3} rot={:.1} preempted_prev={}",
            request.token,
            request.overlay_idx,
            request.user_scale,
            request.rotation_deg,
            preempted
        );
        let (tx, rx) = mpsc::channel::<Result<Option<TypingEditOverlayResult>, String>>();
        thread::spawn(move || {
            let result = render_and_store_image_effects_overlay(request);
            let _ = tx.send(result);
        });
        self.edit_render_rx = Some(rx);
    }

    pub(super) fn start_shape_variant_preview_if_available(
        &mut self,
        ctx: &egui::Context,
        overlay_idx: usize,
        origin: Pos2,
    ) {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            self.shape_variant_preview = None;
            return;
        };
        let Some(render_data_json) = overlay.render_data_json.as_ref() else {
            self.shape_variant_preview = None;
            return;
        };
        let Some(base_params) = text_render_params_from_render_data(render_data_json) else {
            self.shape_variant_preview = None;
            return;
        };
        let overlay_kind = overlay.kind;
        let overlay_size_px = overlay.size_px;
        if !shape_variant_preview_available(overlay_kind) {
            self.shape_variant_preview = None;
            return;
        }

        let variants = build_shape_variant_grid(&base_params);
        let dark_checkerboard = use_dark_shape_variant_checkerboard(base_params.text_color);
        let menu_id = self.next_shape_variant_preview_id();
        let cancel_render = Arc::new(AtomicBool::new(false));
        let worker_cancel_render = Arc::clone(&cancel_render);
        let fonts = Arc::clone(&self.font_provider);
        let (tx, rx) = mpsc::channel::<Result<TypingShapeVariantPreviewResult, String>>();
        thread::spawn(move || {
            if worker_cancel_render.load(Ordering::Relaxed) {
                return;
            }
            let tiles = render_shape_variant_preview_tiles(
                base_params,
                variants,
                &fonts,
                &worker_cancel_render,
            );
            if worker_cancel_render.load(Ordering::Relaxed) {
                return;
            }
            let _ = tx.send(Ok(TypingShapeVariantPreviewResult { menu_id, tiles }));
        });

        let slot_size = shape_variant_slot_size(overlay_size_px);
        let screen_rect = ctx.content_rect();
        self.shape_variant_preview = Some(TypingShapeVariantPreviewState {
            menu_id,
            overlay_idx,
            origin,
            menu_rect: None,
            place_above: origin.y >= screen_rect.center().y,
            dark_checkerboard,
            slot_size,
            gap_px: TEXT_SHAPE_VARIANT_TILE_GAP_PX,
            padding_px: TEXT_SHAPE_VARIANT_PANEL_PADDING_PX,
            cancel_render,
            rx,
            tiles: None,
        });
    }

    pub(super) fn poll_shape_variant_preview(&mut self, ctx: &egui::Context) {
        if !ctx.any_popup_open() {
            self.shape_variant_preview = None;
            return;
        }
        let Some(state) = self.shape_variant_preview.as_mut() else {
            return;
        };
        let Ok(message) = state.rx.try_recv() else {
            return;
        };
        match message {
            Ok(result) if result.menu_id == state.menu_id => {
                state.tiles = Some(result.tiles);
                ctx.request_repaint();
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!(
                    "ERROR typing::shape_variant_preview overlay_idx={} err={}",
                    state.overlay_idx, err
                );
                self.shape_variant_preview = None;
            }
        }
    }

    pub(super) fn update_shape_variant_preview_menu_rect(&mut self, overlay_idx: usize, menu_rect: Rect) {
        let Some(state) = self.shape_variant_preview.as_mut() else {
            return;
        };
        if state.overlay_idx == overlay_idx {
            state.menu_rect = Some(menu_rect);
        }
    }

    pub(super) fn draw_shape_variant_preview(&mut self, ctx: &egui::Context) -> Option<TypingShapeVariant> {
        if !ctx.any_popup_open() {
            self.shape_variant_preview = None;
            return None;
        }
        let state = self.shape_variant_preview.as_mut()?;
        if self.selected_overlay_idx != Some(state.overlay_idx) {
            self.shape_variant_preview = None;
            return None;
        }
        let tiles = state.tiles.as_mut()?;
        if tiles.is_empty() {
            return None;
        }

        for tile in tiles.iter_mut().filter(|tile| tile.texture.is_none()) {
            let Some(rgba) = tile.rgba.take() else {
                continue;
            };
            let image = ColorImage::from_rgba_unmultiplied(tile.size_px, rgba.as_slice());
            tile.texture = Some(ctx.load_texture(
                format!(
                    "typing_shape_variant_{}_{}_{}",
                    state.menu_id, tile.variant.row, tile.variant.col
                ),
                image,
                TextureOptions::LINEAR,
            ));
        }

        let panel_size = shape_variant_panel_size(state.slot_size, state.gap_px, state.padding_px);
        let screen_rect = ctx.content_rect();
        let anchor_rect = state
            .menu_rect
            .unwrap_or_else(|| Rect::from_min_size(state.origin, Vec2::ZERO));
        let mut pos =
            shape_variant_panel_pos(anchor_rect, panel_size, screen_rect, state.place_above);
        pos.x = pos.x.clamp(
            screen_rect.left(),
            (screen_rect.right() - panel_size.x).max(screen_rect.left()),
        );
        pos.y = pos.y.clamp(
            screen_rect.top(),
            (screen_rect.bottom() - panel_size.y).max(screen_rect.top()),
        );

        let mut clicked_variant = None;
        egui::Area::new(Id::new(("typing_shape_variant_preview", state.menu_id)))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                ui.set_min_size(panel_size);
                let panel_rect = Rect::from_min_size(ui.min_rect().min, panel_size);
                paint_shape_variant_checkerboard(
                    ui.painter(),
                    panel_rect,
                    8.0,
                    state.dark_checkerboard,
                );

                for tile in tiles.iter() {
                    let Some(texture) = tile.texture.as_ref() else {
                        continue;
                    };
                    let slot_min = Pos2::new(
                        panel_rect.left()
                            + state.padding_px
                            + tile.variant.col as f32 * (state.slot_size.x + state.gap_px),
                        panel_rect.top()
                            + state.padding_px
                            + tile.variant.row as f32 * (state.slot_size.y + state.gap_px),
                    );
                    let slot_rect = Rect::from_min_size(slot_min, state.slot_size);
                    let response = ui.interact(
                        slot_rect,
                        Id::new((
                            "typing_shape_variant_tile",
                            state.menu_id,
                            tile.variant.row,
                            tile.variant.col,
                        )),
                        Sense::click(),
                    );
                    let scale = if response.hovered() { 1.06 } else { 1.0 };
                    let draw_size = fit_size_to_box(tile.size_px, state.slot_size * scale);
                    let draw_rect = Rect::from_center_size(slot_rect.center(), draw_size);
                    ui.painter().image(
                        texture.id(),
                        draw_rect,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    if response.hovered() {
                        ui.painter().rect_stroke(
                            draw_rect.expand(3.0),
                            6.0,
                            Stroke::new(2.0, Color32::WHITE),
                            egui::StrokeKind::Outside,
                        );
                    }
                    if response.clicked() {
                        clicked_variant = Some(tile.variant.clone());
                    }
                }
            });

        clicked_variant
    }

    pub(super) fn apply_shape_variant_to_overlay(&mut self, ctx: &egui::Context, variant: TypingShapeVariant) {
        let Some(overlay_idx) = self.selected_overlay_idx else {
            return;
        };
        let Some(text_images_dir) = self.text_images_save_dir.clone() else {
            self.set_create_error(
                ctx,
                t!("typing.errors.text_images_dir_missing_overlay"),
            );
            return;
        };
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return;
        };
        if overlay.kind != TypingOverlayKind::Text {
            return;
        }
        let Some(current_render_data) = overlay.render_data_json.as_ref() else {
            return;
        };
        let Some((render_params, render_data_json)) =
            build_shape_variant_apply_payload(current_render_data, &variant)
        else {
            return;
        };

        let edit_request = TypingEditOverlayRequest {
            token: 0,
            latest_token: Arc::clone(&self.edit_render_latest_token),
            overlay_idx,
            file_name: overlay.file_name.clone(),
            text_images_dir,
            user_scale: overlay.user_scale,
            rotation_deg: overlay.angle_deg,
            render_params,
            render_data_json,
            font_provider: Arc::clone(&self.font_provider),
        };
        self.shape_variant_preview = None;
        self.start_edit_overlay_render_job(edit_request);
    }
}
