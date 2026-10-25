/*
File: crates/ms-tab-ps-editor/src/page_session.rs

Purpose:
Page session of the tab: camera hand-over, the background page loader, page persistence and the
active tool / selection setters.

Key functions:
- `current_page()`, `camera()`, `sync_view_from_canvas()` (pub): camera exchange with `CanvasView`.
- `ensure_loader()`, `request_page()`, `poll_loader()`: background base-layer decode for the active
  page.
- `persist_current_page()`, `flush_layers()`, `flush_layers_if_dirty()`,
  `has_deferred_layer_edits()` (pub): layer persistence.
- `set_active_tool()` (the only writer of `active_tool_idx`), `reset_active_tool()`,
  `set_selection()`.

Notes:
`set_selection` keeps the page selection `Some` only while it selects at least one pixel.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

impl PsEditorTabState {
    /// Page currently shown or being loaded (falls back to the requested page mid-load).
    pub fn current_page(&self) -> Option<usize> {
        self.active_page_idx.or(self.requested_page_idx)
    }

    /// Current camera as `(zoom, center_world)` in image-pixel space, for syncing the view back
    /// to `CanvasView` when leaving this tab.
    pub fn camera(&self) -> (f32, Vec2) {
        (self.viewport.zoom(), self.viewport.center_world())
    }

    /// Synchronizes this tab's view from the shared canvas world when the tab becomes active.
    ///
    /// Mirrors three things from `CanvasView` (best-effort, "в доступных пределах"):
    /// - the current page (loads it if different from the one shown);
    /// - the clean overlay (reloads the page when the shared model changed under us so the `Клин`
    ///   base layer reflects edits made on other tabs — raster layers are preserved across the
    ///   reload, the camera is re-applied below);
    /// - zoom + camera position, deferred via `pending_camera` until the page is loaded so the
    ///   async load's refit does not clobber it.
    ///
    /// `center_world` is the page-local source-pixel point to center; `None` keeps the fitted
    /// center and only applies the zoom.
    pub fn sync_view_from_canvas(
        &mut self,
        project: &ProjectData,
        page_idx: usize,
        zoom: f32,
        center_world: Option<Vec2>,
    ) {
        self.ensure_loader();
        let model_revision = self.overlay_model_revision();
        let page_changed = self.active_page_idx != Some(page_idx);
        let overlay_changed = model_revision != self.last_overlay_revision;
        if page_changed || overlay_changed {
            self.request_page(project, page_idx);
        }
        self.pending_camera = Some(CameraSync {
            page_idx,
            zoom,
            center_world,
        });
        self.apply_pending_camera();
    }

    /// Applies a pending synced camera once its target page is the loaded one.
    ///
    /// Deferred until no load is in flight: a reload (page change or clean-overlay refresh) calls
    /// `viewport.invalidate`, so applying before the reload settles would be overwritten by the
    /// post-load refit. Applying after `poll_loader` has cleared `pending_job_id` re-marks the
    /// camera initialized so the later `fit_page_if_needed` is a no-op.
    pub(crate) fn apply_pending_camera(&mut self) {
        let Some(sync) = self.pending_camera else {
            return;
        };
        if self.pending_job_id.is_some() || self.active_page_idx != Some(sync.page_idx) {
            return;
        }
        let Some(stack) = &self.stack else {
            return;
        };
        let size = stack.size();
        let (w, h) = (size[0] as f32, size[1] as f32);
        // Absent center -> page center; otherwise clamp the synced point into the page bounds.
        let center = sync.center_world.map_or_else(
            || Vec2::new(w * 0.5, h * 0.5),
            |c| Vec2::new(c.x.clamp(0.0, w), c.y.clamp(0.0, h)),
        );
        self.viewport.set_camera(sync.zoom, center);
        self.pending_camera = None;
    }

    /// Reads the shared clean-overlay model revision (0 when no model is bound).
    pub(crate) fn overlay_model_revision(&self) -> u64 {
        self.overlays_model
            .as_ref()
            .and_then(|model| model.lock().ok().map(|locked| locked.revision()))
            .unwrap_or(0)
    }

    /// Lazily starts the page loader worker once the model is known.
    pub(crate) fn ensure_loader(&mut self) {
        if self.loader.is_some() {
            return;
        }
        if let Some(model) = &self.overlays_model {
            self.loader = Some(spawn_page_loader_thread(Arc::clone(model)));
        }
    }

    /// Requests loading of `page_idx`, persisting the page being left first.
    pub(crate) fn request_page(&mut self, project: &ProjectData, page_idx: usize) {
        let Some(page) = project.pages.iter().find(|p| p.idx == page_idx) else {
            return;
        };
        // A brush stroke still in flight belongs to the page being LEFT, and its pixels are
        // already on that page's layer. Commit it FIRST — before the persist below, and long before
        // `reset_active_tool` at the end of this function — because the commit reads the resident
        // stack and the current `active_page_idx`, both of which are about to become the next
        // page's. This is the one abandonment path where the order is load-bearing.
        self.commit_brush_stroke_before_abandon(project);
        // Persist the page we are leaving (committed edits are already flushed; this catches any
        // model state not yet written). The new page reloads fresh from disk + the shared doc.
        self.persist_current_page(project);
        // Undo history is per-page-session: a recorded diff is only valid while its page's layer image
        // buffers are resident, and the new page rebuilds the stack from scratch. Drop it (and any
        // in-progress brush-stroke union) so an undo cannot apply to the wrong page's pixels.
        self.history.clear();
        self.brush_stroke_dirty = None;
        // Drop any in-progress gesture snapshots too, so a gesture straddling a page switch cannot
        // record an undo step against the wrong page.
        self.opacity_gesture = None;
        self.group_opacity_gesture = None;
        self.transform_gesture_before = None;
        self.deform_gesture_before = None;
        // Same reason for the tool's own gesture: an outline traced on the page being left holds
        // that page's image coordinates (and a combination mode sampled there), so it must never
        // reach the next page's selection.
        self.reset_active_tool(project);
        if self.loader.is_none() {
            return;
        }
        let job_id = self.next_job_id;
        self.next_job_id += 1;
        self.pending_job_id = Some(job_id);
        self.requested_page_idx = Some(page_idx);
        self.load_error = None;
        // The worker decodes the persisted user-layer payload off-thread, so it needs the layer dirs
        // and the FULL chapter page-size map (the doc's legacy ribbon migration requires every page's
        // aspect ratio). Capturing the page paths here keeps the borrow off `loader` (so we can call
        // `page_sizes_map(&mut self)`); re-borrow the loader after building the request.
        let page_path = page.path.clone();
        let unsaved_layers_dir = project.paths.unsaved_layers_dir.clone();
        let layers_dir = project.paths.layers_dir.clone();
        // Gate the legacy `text_images/` fallback ONCE per chapter, cached by the committed layers dir:
        // a migrated chapter (inline text present) decodes with `None`; an un-migrated one feeds the
        // legacy dir so its text reaches the doc. Recomputing on every page switch would re-parse
        // `layers.json` on the GUI thread — the cache keeps that off the hot path.
        let legacy_text_dir = if let Some((cached_layers_dir, cached_legacy_text_dir)) =
            &self.doc_legacy_text_dir_cache
            && cached_layers_dir == &project.paths.layers_dir
        {
            cached_legacy_text_dir.clone()
        } else {
            let gated = if ms_models::layer_model::migrate::manifest_has_inline_text(
                &project.paths.layers_dir,
            ) {
                None
            } else {
                Some(project.paths.text_images_dir.clone())
            };
            self.doc_legacy_text_dir_cache =
                Some((project.paths.layers_dir.clone(), gated.clone()));
            gated
        };
        let page_sizes = self.page_sizes_map(project);
        ms_log::trace_log!(
            cat::PERSIST,
            "page_load request job={} page={}",
            job_id,
            page_idx
        );
        let Some(loader) = &self.loader else {
            return;
        };
        let request = PageLoadRequest {
            job_id,
            page_idx,
            page_path,
            unsaved_layers_dir,
            layers_dir,
            legacy_text_dir,
            page_sizes,
        };
        // A failed send means the loader worker has exited, so this page will never load.
        if loader.request_tx.send(Some(request)).is_err() {
            ms_log::runtime_log::log_warn(format!(
                "[ps_editor] page load request not sent: loader worker has exited (job={job_id} page={page_idx})"
            ));
        }
    }

    /// Writes the current page's raster layers to the unsaved staging dir (`*_unsaved/layers/`).
    ///
    /// Base layers are skipped (they mirror `src/` and `clean_layers/`). Called when leaving a page
    /// and on an explicit project save; previously-visited pages were already written on their own
    /// page switch.
    ///
    /// This path is NOT redundant with the per-edit `route_to_doc` enqueue: it reads from the PS
    /// `self.stack` (not the doc) and carries the EXPLICIT `removed_uids` from `self.deleted_raster_uids`.
    /// The doc's `enqueue_page_save` passes an empty removed set, so it would PRESERVE a raster the PS
    /// editor deleted as "another tab's" — a deleted raster would resurrect on disk. So this builds an
    /// OWNED save job (raster part only — no effects reconcile, mirroring the sync `save_page_rasters`
    /// that preserves another tab's effects) with the explicit removed set and enqueues it through the
    /// saver handle (moving the PNG encode off-thread) while preserving the EXACT deletion contract.
    /// Falls back to the synchronous `save_page_rasters` when no saver is enabled.
    pub(crate) fn persist_current_page(&mut self, project: &ProjectData) {
        let Some(stack) = &self.stack else {
            return;
        };
        let page_idx = stack.page_idx();
        let _s = ms_log::trace_scope!(cat::PERSIST, "persist_current_page page={}", page_idx);
        // Owned raster layers for the async job. `effects` is left empty + `display_image` None so the
        // saver's effects-reconcile loop is a no-op for these — mirroring the sync `save_page_rasters`
        // here, which never reconciles effects (it PRESERVES another tab's on-disk chain on a non-dirty
        // raster). PS does not own the typing-tab mask-clip flag; `None` preserves the on-disk value.
        let owned_layers: Vec<saver::OwnedRasterLayer> = stack
            .layers()
            .iter()
            .filter(|layer| layer.kind == LayerKind::Raster)
            .map(|layer| saver::OwnedRasterLayer {
                uid: layer.uid.to_string(),
                name: layer.name.clone(),
                visible: layer.visible,
                opacity: layer.opacity,
                transform: transform_to_rec(layer.transform),
                deform: layer.deform.clone(),
                group_uid: layer
                    .group
                    .and_then(|gid| stack.group(gid).map(|g| g.uid.to_string())),
                // Always carried: PS clears its dirty flags at enqueue, so a failed write could not be
                // retried from them (bound: one active page per PS persist).
                base_image: Some(layer.image.clone()),
                image_size: layer.image.size,
                pixels_dirty: layer.pixels_dirty,
                mask_clip: None,
                display_image: None,
                effects: Vec::new(),
            })
            .collect();
        let groups: Vec<persist::GroupMeta> = stack
            .groups()
            .iter()
            .map(|g| persist::GroupMeta {
                uid: g.uid.to_string(),
                name: g.name.clone(),
                visible: g.visible,
                opacity: g.opacity,
                collapsed: g.collapsed,
            })
            .collect();
        let removed_uids: Vec<String> = self
            .deleted_raster_uids
            .get(&page_idx)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        let raster_count = owned_layers.len();
        let group_count = groups.len();
        let removed_count = removed_uids.len();

        // Try the off-thread path: capture the saver handle (clone, no doc lock held during the write).
        let saver = self.layer_doc.as_ref().and_then(|doc| {
            doc.lock().ok().and_then(|mut guard| {
                guard.saver_handle().map(|handle| {
                    let epoch = guard.next_save_epoch(page_idx, saver::SaveKind::Raster);
                    (handle, epoch)
                })
            })
        });
        let persist_result: Result<(), String> = if let Some((handle, raster_epoch)) = saver {
            handle.enqueue(saver::PageSaveJob {
                page_idx,
                layers_dir: project.paths.unsaved_layers_dir.clone(),
                fallback_dir: Some(project.paths.layers_dir.clone()),
                raster: Some(saver::RasterSavePart {
                    layers: owned_layers,
                    groups,
                    removed_uids,
                }),
                raster_epoch: Some(raster_epoch),
                text: None,
                text_epoch: None,
                effects: Vec::new(),
                structural: Vec::new(),
                structural_epoch: None,
            });
            // This raw-handle enqueue bypasses the doc's funnels, so it reports its own autosave
            // action — AFTER the send, like the funnels (the saver polls the gate before draining).
            if let Some(doc) = &self.layer_doc {
                match doc.lock() {
                    Ok(guard) => guard.note_autosave_action(),
                    // The job is queued regardless; an unreported action only delays the autosave
                    // to the gate's interval / next action (barriers still force it out).
                    Err(_) => ms_log::runtime_log::log_error(format!(
                        "[ps_editor] page {page_idx}: shared layer doc lock poisoned; autosave action not reported"
                    )),
                }
            }
            Ok(())
        } else {
            // No saver: synchronous fallback. Same transaction shape as the saver job, including the
            // committed `fallback_dir`: a page staging has no record of is seeded from committed first,
            // so deleting its last raster stages it PRESENT-but-EMPTY instead of leaving it absent
            // (absent means "use committed" to the loader and the merge, which would resurrect it).
            let outs: Vec<persist::RasterLayerOut> = owned_layers
                .iter()
                .map(saver::OwnedRasterLayer::as_out)
                .collect();
            persist::ManifestTxn::begin(&project.paths.unsaved_layers_dir).and_then(|mut txn| {
                txn.save_page_rasters(page_idx, &outs, &groups, &removed_uids, Some(&project.paths.layers_dir))?;
                txn.commit().map(|_wrote| ())
            })
        };

        match persist_result {
            Ok(()) => {
                ms_log::trace_log!(
                    cat::PERSIST,
                    "persist_current_page saved page={} rasters={} groups={} removed={}",
                    page_idx,
                    raster_count,
                    group_count,
                    removed_count
                );
                // The deletions are captured by the enqueued job (or already written); stop carrying
                // them so a later flush does not re-emit the now-dropped removed set.
                self.deleted_raster_uids.remove(&page_idx);
                // This page's persist is enqueued/written; the tab-switch flush is redundant for it.
                self.layers_dirty = false;
                // Base PNGs are (being) written: clear `pixels_dirty` so a later flush (e.g. on project
                // save) treats these rasters as clean and preserves a non-destructive effects chain
                // the typing tab added in the meantime, instead of rewriting the base and dropping it.
                if let Some(stack) = self.stack.as_mut() {
                    stack.mark_rasters_persisted();
                }
                // No cross-tab signal needed: this is pure persistence of state whose MODEL changes
                // already routed through the shared doc (bumping its version).
            }
            Err(err) => ms_log::runtime_log::log_error(format!(
                "[ps_editor] persist layers (page {page_idx}): {err}"
            )),
        }
    }

    /// Flushes the active page's raster layers to disk. Call before merging the unsaved staging
    /// folder into the project on "save to project".
    pub fn flush_layers(&mut self, project: &ProjectData) {
        let _s = ms_log::trace_scope!(cat::PERSIST, "flush_layers");
        self.persist_current_page(project);
    }

    /// Flushes the active page only when an `edit_doc_node` mutation deferred persistence since the
    /// last flush (tracked by `layers_dirty`; the per-edit `route_to_doc` path enqueues immediately
    /// and clears it). Used on a PS→other-tab switch so an unchanged page does not pay a redundant
    /// snapshot+enqueue every switch. `flush_layers` clears the flag via `persist_current_page`.
    pub fn flush_layers_if_dirty(&mut self, project: &ProjectData) {
        if !self.layers_dirty {
            return;
        }
        self.flush_layers(project);
    }

    /// True while an edit of the active page changed the shared document but its persistence (and
    /// with it the autosave gate's `note_action`) is still deferred to the next flush — e.g. a
    /// visibility / opacity change or a text drag routed through `edit_doc_node`. The single-image
    /// session counts this as unsaved work, since the gate's action count has not moved yet.
    /// Cleared by `flush_layers` (an enqueued or written page persist).
    #[must_use]
    pub fn has_deferred_layer_edits(&self) -> bool {
        self.layers_dirty
    }

    /// Records that a raster node was removed from the stack this session so the next page save
    /// drops it from the manifest (`save_page_rasters` otherwise preserves rasters it does not own)
    /// and the merge does not resurrect it. No-op for non-raster layers.
    pub(crate) fn record_raster_deletion(&mut self, layer_id: LayerId) {
        let Some(stack) = self.stack.as_ref() else {
            return;
        };
        let page_idx = stack.page_idx();
        let Some(layer) = stack.layer(layer_id) else {
            return;
        };
        if layer.kind != LayerKind::Raster {
            return;
        }
        let uid = layer.uid.to_string();
        self.deleted_raster_uids
            .entry(page_idx)
            .or_default()
            .insert(uid);
    }

    /// Drains finished load jobs, building a fresh stack for the matching page.
    pub(crate) fn poll_loader(&mut self) {
        let Some(loader) = &self.loader else {
            return;
        };
        let mut latest: Option<page_loader::PageLoadResult> = None;
        while let Ok(result) = loader.result_rx.try_recv() {
            latest = Some(result);
        }
        let Some(result) = latest else {
            return;
        };
        if Some(result.job_id) != self.pending_job_id {
            return;
        }
        self.pending_job_id = None;
        let _s = ms_log::trace_scope!(
            cat::PERSIST,
            "page_load complete job={} page={}",
            result.job_id,
            result.page_idx
        );
        match result.outcome {
            Ok(page) => {
                ms_log::trace_log!(
                    cat::SYNC,
                    "page_load base_ready page={} size=[{},{}]",
                    result.page_idx,
                    page.size[0],
                    page.size[1]
                );
                // Build a fresh stack with only the two base layers; the user raster layers are
                // materialized below by `sync_view_from_doc` from the shared `LayerDoc` (the post-
                // refactor source of truth), so there is NO separate `load_persisted_into_stack` decode
                // here — the raster PNGs were already decoded ONCE off-thread by the worker.
                let stack = LayerStack::new(result.page_idx, page.size, page.source, page.clean);
                self.stack = Some(stack);
                // Seed the loaded page's size from the freshly-built stack (authoritative) so a memoized
                // header read can't disagree with the worker's page-size map.
                if let Some(size) = self.stack.as_ref().map(|s| s.size()) {
                    self.page_sizes_px.insert(result.page_idx, size);
                }
                // Move the worker-decoded user-layer payload into the shared doc under a BRIEF lock
                // (no decode is performed here — the heavy PNG decode already ran lock-free on the
                // worker). `insert_decoded_page` is memoized: if the page was already resident (e.g. a
                // concurrent edit between request and insert), it discards the payload and keeps the
                // live in-memory page. If the worker's decode failed (`layers == None`), the page is
                // left un-inserted and the projection below shows just the base layers.
                if let Some(payload) = page.layers {
                    if let Some(doc) = &self.layer_doc
                        && let Ok(mut doc) = doc.lock()
                    {
                        doc.insert_decoded_page(result.page_idx, payload);
                    }
                } else {
                    ms_log::runtime_log::log_warn(format!(
                        "[ps_editor] page {} loaded without a layer payload (decode failed on worker)",
                        result.page_idx
                    ));
                }
                // Materialize the text-layer runtimes (pin / text-group metadata included) from the doc
                // page just inserted — the page loader's payload is PS's only disk read for this page —
                // then project the doc over the stack rasters / text / bands so both tabs read one model.
                self.materialize_text_runtime_from_doc(result.page_idx);
                self.active_page_idx = Some(result.page_idx);
                self.selection = None;
                // Panel selection keys on session ids that the new page's stack reuses; reset it.
                // `panel_primary` is only cleared here — `normalize_panel_primary` below re-seeds it
                // from the fresh stack's active layer, which is `Клин`.
                self.panel_selection.clear();
                self.panel_anchor = None;
                self.panel_primary = None;
                self.render_cache.clear();
                // A fresh page: forget prior per-node generations so the first projection uploads.
                self.node_generations
                    .retain(|(p, _), _| *p == result.page_idx);
                self.viewport.invalidate();
                self.last_overlay_revision = self.overlay_model_revision();
                self.load_error = None;
                // Project the shared doc over the freshly-loaded stack / text / bands.
                self.sync_view_from_doc(result.page_idx);
                // Seed the panel's primary row from the projected stack's active layer, so the page
                // opens with `Клин` selected. Called explicitly rather than left to
                // `sync_view_from_doc`'s own tail: that call returns early when the doc has no page
                // (a worker decode failure leaves the page un-inserted), and the page still opens
                // with its two base layers, which must still show an active one.
                self.normalize_panel_primary();
            }
            Err(err) => {
                ms_log::trace_log!(
                    cat::PERSIST,
                    "page_load failed page={} err={}",
                    result.page_idx,
                    err
                );
                ms_log::runtime_log::log_error(format!("[ps_editor] page load failed: {err}"));
                self.load_error = Some(err);
            }
        }
    }

    /// Index of the active tool in `self.tools` for a given id.
    pub(crate) fn tool_index(&self, id: PsToolId) -> Option<usize> {
        self.tools.iter().position(|tool| tool.id() == id)
    }

    pub(crate) fn active_tool_id(&self) -> PsToolId {
        self.tools[self.active_tool_idx].id()
    }

    /// Switches the active tool, abandoning the OUTGOING tool's in-progress gesture first.
    ///
    /// Every write to `active_tool_idx` must go through here (toolbar click and B/M/L/V hotkeys):
    /// a gesture that ends with the button UP — a pending lasso polygon — otherwise survives the
    /// switch and commits on some later frame, with a combination mode sampled long before. Out
    /// of range or already-active indices are ignored, so a caller may pass any candidate.
    ///
    /// A brush stroke in flight is COMMITTED before it is abandoned (see
    /// `commit_brush_stroke_before_abandon`): its pixels are already on the layer, so dropping them
    /// silently would leave them un-undoable. That is why this needs the project.
    pub(crate) fn set_active_tool(&mut self, idx: usize, project: &ProjectData) {
        if idx >= self.tools.len() || idx == self.active_tool_idx {
            return;
        }
        self.commit_brush_stroke_before_abandon(project);
        if let Some(outgoing) = self.tools.get_mut(self.active_tool_idx) {
            outgoing.reset();
        }
        self.active_tool_idx = idx;
    }

    /// Abandons the active tool's in-progress gesture, keeping the tool selected.
    ///
    /// Used on a page switch: the outline's image coordinates belong to the page being left, so
    /// committing it against the next page would paint a selection nowhere near where it was drawn.
    /// Also used for Esc pressed on a frame whose input was suppressed by a pan.
    ///
    /// A brush stroke that has painted is committed first, exactly as on a tool switch — Esc means
    /// "abandon the gesture", never "silently keep un-undoable pixels". Callers that change the
    /// PAGE must run the commit before the change (see `request_page`), because the commit targets
    /// the resident stack and `active_page_idx`.
    pub(crate) fn reset_active_tool(&mut self, project: &ProjectData) {
        self.commit_brush_stroke_before_abandon(project);
        if let Some(tool) = self.tools.get_mut(self.active_tool_idx) {
            tool.reset();
        }
    }

    /// Stores `selection` as the page selection, enforcing "a selection is `Some` only when it
    /// selects at least one pixel".
    ///
    /// An all-zero mask stored as `Some` is invisible (`Selection::any` is false, so the marquee
    /// draws nothing) yet still clips the brush, which only tests `Option::is_some` — the user
    /// sees a tool that silently refuses to paint until Ctrl+D. Reachable whenever a footprint
    /// falls entirely outside the page (a raster moved off-page, an off-page text row).
    pub(crate) fn set_selection(&mut self, selection: Selection) {
        self.selection = non_empty_selection(selection);
    }
}
