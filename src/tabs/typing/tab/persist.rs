/*
File: tab/persist.rs

Purpose:
Text-overlay persistence for the typing tab: reconciling live overlay MODEL state
into the shared `LayerDoc` and flushing/saving it to the on-disk `layers.json`
staging payload. Covers background save-job polling, save requests/spawning,
per-page and whole-doc text flushes, pixel-rounding of overlay positions, and the
DEFERRED-save policy that decides WHEN an edit is written.

Deferred-save policy (see `MODULE_README.md` "Contracts and invariants"):
an EDIT calls `mark_placement_save_dirty` and writes nothing; the write happens at a
flush point — selection change, page change, tab leave, idle debounce, or app exit.
STRUCTURAL changes (deletions) bypass this and write eagerly.

A flush point may retire its dirty state ONLY once a write is genuinely dispatched:
`request_overlay_placement_save` reports that through `PlacementSaveDispatch`, and
`flush_text_layers` through `Result<TypingTextFlushOutcome, TypingTextFlushError>`.

Key functions:
- mark_placement_save_dirty() / has_pending_placement_save() / clear_placement_save_dirty()
- discard_pending_placement_save(): drop everything unwritten (DISCARD path only)
- flush_placement_save_if_dirty(): in-session flush, detached writer
- drive_placement_save_debounce(): per-frame idle timer + the repaint that makes it fire
- flush_placement_save_on_page_change(): page-crossing flush
- flush_text_layers_if_dirty(): INLINE flush for tab leave / exit (barrier-safe)
- request_overlay_placement_save() / spawn_overlay_placement_save(): the writer itself
- flush_text_layers(): whole-doc flush for save-to-project (returns OWNED pages)

Notes:
Extracted verbatim from `tab.rs`. Methods keep their original visibility
(`pub(super)` default; `flush_text_layers` stays `pub`). `use super::*;` pulls in
the parent module's types and imports. Struct/enum definitions and the rest of the
big `impl TypingTextOverlayLayer` block remain in `tab.rs`; these methods reach the
private items that stay there as descendants of module `tab`.
*/

use super::*;

impl TypingTextOverlayLayer {
    pub(super) fn poll_save_jobs(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(rx) = self.save_rx.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    t!("typing.persist.text_info_channel_error").to_string(),
                )),
            }
        };

        let Some(recv_result) = recv_result else {
            return false;
        };

        self.save_rx = None;
        match recv_result {
            Ok(Ok(())) => {
                crate::trace_log!(cat::PERSIST, "overlay_placement_save result=ok");
                // Our own overlay write to `layers.json` / PNGs completed. The MODEL change already
                // routed through the shared doc (bumping its version, so the PS tab re-projects); this
                // job only persisted it to disk, so there is nothing more to signal cross-tab.
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::PERSIST, "overlay_placement_save result=err err={}", err);
                self.set_create_error(ctx, err);
            }
        }

        if self.save_requested_while_busy {
            self.save_requested_while_busy = false;
            self.spawn_overlay_placement_save();
        }
        true
    }

    /// Records that an EDIT changed a text layer, WITHOUT writing anything.
    ///
    /// The write happens later, at a flush point (`flush_placement_save_if_dirty` /
    /// `flush_text_layers_if_dirty`). Re-marking while already dirty RESTARTS the idle window (by
    /// clearing the seed, which the next frame re-seeds), so a continuous gesture — a drag re-marking
    /// every frame — keeps pushing the deadline out and writes ONCE on settle instead of per frame.
    ///
    /// Use this for edits only. STRUCTURAL changes (deletions) must keep saving EAGERLY through
    /// `dispatch_structural_placement_save`: their durability must not depend on a flush point being
    /// reached, or a deleted layer could resurrect from stale on-disk state.
    pub(super) fn mark_placement_save_dirty(&mut self) {
        self.placement_save_dirty = true;
        self.placement_save_dirty_since_s = None;
    }

    /// Whether any text-layer write is still owed to the disk.
    ///
    /// Covers all THREE axes:
    /// - `placement_save_dirty` — geometry/placement edits marked but not yet flushed;
    /// - `edit_render_data_dirty` — a completed edit render whose `render_data` is not yet persisted;
    /// - `save_requested_while_busy` — a flush that already ran but could only PARK behind an in-flight
    ///   save/create/edit render. It writes nothing until `poll_save_jobs` / `poll_edit_overlay_jobs`
    ///   re-fires it, so until then the edit is exactly as unwritten as a dirty flag: leaving it out
    ///   let a parked write be dropped by an exit or tab-leave flush that saw "nothing pending".
    ///
    /// The three have separate clear rules but mean the same thing to the writer
    /// (`request_overlay_placement_save` persists whatever is live), so flush points consult them
    /// through this one helper rather than deciding per flag.
    #[must_use]
    pub(super) fn has_pending_placement_save(&self) -> bool {
        self.placement_save_dirty || self.edit_render_data_dirty || self.save_requested_while_busy
    }

    /// Clears both dirty axes and the idle window. Call only when the pending edit has been WRITTEN
    /// (dispatched to the save pipeline) or has become moot (chapter reset, or an eager save that
    /// already persisted it) — never before a write attempt, or a failed attempt would leave the edit
    /// looking clean forever.
    ///
    /// Deliberately does NOT clear `save_requested_while_busy`: that flag is the only record of a
    /// parked write, and `poll_save_jobs` / `poll_edit_overlay_jobs` own its lifetime. Dropping it here
    /// would silently cancel a re-fire — including one requested by an EAGER structural save.
    pub(super) fn clear_placement_save_dirty(&mut self) {
        self.placement_save_dirty = false;
        self.placement_save_dirty_since_s = None;
        self.edit_render_data_dirty = false;
    }

    /// DROPS all pending text-save state without writing: both dirty axes, the idle window, AND the
    /// parked re-fire. DISCARD path only (`app.rs::start_exit_cleanup`).
    ///
    /// Unlike `clear_placement_save_dirty` this also cancels `save_requested_while_busy`, because on
    /// the discard path a re-fire is not a lost write to protect but an unwanted one: it would run
    /// against a staging dir that is being deleted, and (with the saver already shut down) take
    /// `enqueue_page_text_save`'s synchronous fallback, re-creating that dir with the discarded edits.
    pub(super) fn discard_pending_placement_save(&mut self) {
        self.clear_placement_save_dirty();
        self.save_requested_while_busy = false;
    }

    /// Writes a deferred edit if one is pending, via the normal detached placement-save worker.
    ///
    /// This is the in-session flush point used by focus loss, page change, and the idle debounce. It is
    /// NOT sufficient at app exit: `request_overlay_placement_save` detaches a `thread::spawn` that
    /// would race the layer-saver barrier. Exit and tab-leave use `flush_text_layers_if_dirty`, which
    /// enqueues inline on the calling thread.
    ///
    /// The dirty state is cleared ONLY once the write is genuinely owned by the save pipeline
    /// (`Started` or `Parked`). On `NotWired` nothing was written and the state stays dirty, so the
    /// next flush point retries; clearing there would mark the edit saved forever — the debounce would
    /// stop arming its repaint, `has_pending_placement_save` would go false, and tab-leave/exit would
    /// no longer retry, which at exit is silent data loss (the barrier cannot cover a job that was
    /// never enqueued).
    pub(super) fn flush_placement_save_if_dirty(&mut self, reason: TypingSaveFlushReason) {
        if !self.has_pending_placement_save() {
            return;
        }
        let dispatch = self.request_overlay_placement_save();
        match dispatch {
            PlacementSaveDispatch::Started | PlacementSaveDispatch::Parked => {
                self.clear_placement_save_dirty();
                crate::trace_log!(
                    cat::PERSIST,
                    "deferred text save flushed reason={} dispatch={}",
                    reason.as_trace_str(),
                    dispatch.as_trace_str()
                );
            }
            PlacementSaveDispatch::NotWired => {
                crate::trace_log!(
                    cat::PERSIST,
                    "deferred text save NOT dispatched (persistence not wired), staying dirty reason={}",
                    reason.as_trace_str()
                );
            }
        }
    }

    /// Dispatches an EAGER structural save (a deletion) and settles the deferred-edit state with it.
    ///
    /// Structural writes are never deferred: a deletion's durability must not wait on a flush point
    /// being reached, or the layer could resurrect from the stale on-disk set. The dispatch persists
    /// the whole live text state, so it also satisfies any pending deferred edit — but only if it was
    /// really dispatched, hence the same clear-after-dispatch rule the deferred flush points follow.
    ///
    /// `what` is a short English description of the structural change, used only for the diagnostic log.
    pub(super) fn dispatch_structural_placement_save(&mut self, what: &str) {
        match self.request_overlay_placement_save() {
            PlacementSaveDispatch::Started | PlacementSaveDispatch::Parked => {
                self.clear_placement_save_dirty();
            }
            PlacementSaveDispatch::NotWired => {
                crate::runtime_log::log_warn(format!(
                    "[typing] {what} was not persisted: no staging layers dir / shared document is \
                     wired, so nothing was written and the on-disk set is unchanged."
                ));
            }
        }
    }

    /// Per-frame idle-debounce tick: seeds the window for a fresh mark, flushes once the window has
    /// been open for `PLACEMENT_SAVE_IDLE_DEBOUNCE_SECS`, and otherwise schedules the wake that makes
    /// the deadline reachable.
    ///
    /// The `request_repaint_after` is load-bearing, not an optimization: egui does not draw frames
    /// while idle, so with no pending input NOTHING would call this again and the debounce would never
    /// fire — the edit would sit unwritten until the user happened to interact. This is the only thing
    /// that makes a walk-away edit crash-recoverable from the `_unsaved` staging dir.
    pub(super) fn drive_placement_save_debounce(&mut self, ctx: &egui::Context) {
        let now_s = ctx.input(|i| i.time);
        let (window_start_s, should_flush) = placement_save_debounce_tick(
            self.has_pending_placement_save(),
            self.placement_save_dirty_since_s,
            now_s,
        );
        self.placement_save_dirty_since_s = window_start_s;
        if should_flush {
            self.flush_placement_save_if_dirty(TypingSaveFlushReason::Idle);
            return;
        }
        if let Some(start) = window_start_s {
            let remaining = (PLACEMENT_SAVE_IDLE_DEBOUNCE_SECS - (now_s - start)).max(0.0);
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(remaining));
        }
    }

    /// Flushes a deferred edit when the canvas's derived current page changes.
    ///
    /// The typing canvas is a continuous scroll strip, so the page is derived per frame and changes as
    /// the user scrolls; a flush per page crossed is cheap and is a genuine focus loss for the layer
    /// being edited. The FIRST observation only seeds `last_page_idx` — without that guard a
    /// freshly-loaded chapter would flush against an uninitialized page on its first frame.
    pub(super) fn flush_placement_save_on_page_change(&mut self, page_idx: usize) {
        let changed = self.last_page_idx.is_some_and(|last| last != page_idx);
        self.last_page_idx = Some(page_idx);
        if changed {
            self.flush_placement_save_if_dirty(TypingSaveFlushReason::PageChange);
        }
    }

    /// Flushes a deferred edit INLINE on the calling thread, for the two points where the detached
    /// worker path is unsafe.
    ///
    /// Routes through `flush_text_layers` (which enqueues each resident page to the coalescing saver on
    /// THIS thread) rather than `request_overlay_placement_save` (which detaches a `thread::spawn`).
    /// At exit that distinction is the whole point: the detached writer has no quiescence handle, so it
    /// would race `on_exit`'s layer-saver barrier and the edit could be lost. Enqueuing inline puts the
    /// job in the saver's FIFO BEFORE the barrier runs, so the barrier covers it.
    ///
    /// Does no PNG encoding on this thread (the saver worker does that), so it is safe at a tab switch.
    ///
    /// The dirty state is cleared by `flush_text_layers` itself, and only when the flush actually ran
    /// and every resident page enqueued. A flush that could not run (no dir/doc, poisoned lock) leaves
    /// it dirty so a later flush point retries instead of treating the edit as saved.
    pub(super) fn flush_text_layers_if_dirty(&mut self, reason: TypingSaveFlushReason) {
        if !self.has_pending_placement_save() {
            return;
        }
        match self.flush_text_layers() {
            Ok(outcome) => {
                crate::trace_log!(
                    cat::PERSIST,
                    "deferred text save flushed inline reason={} owned_pages={} failed_pages={}",
                    reason.as_trace_str(),
                    outcome.owned_pages.len(),
                    outcome.failed_pages
                );
            }
            Err(err) => {
                // Left dirty on purpose. At a tab switch a later flush point retries; at EXIT there is
                // no later point, so this line is the only record that the edit did not reach the disk.
                crate::runtime_log::log_warn(format!(
                    "[typing] deferred text flush (reason={}) could not run: {err}. \
                     The pending text edit stays dirty and was NOT written.",
                    reason.as_trace_str()
                ));
            }
        }
    }

    /// Dispatches a placement save for the CURRENT live state and reports whether the save pipeline
    /// took ownership of the write. See [`PlacementSaveDispatch`]: only `Started`/`Parked` mean the
    /// write will happen, so only they may retire a caller's dirty state.
    ///
    /// Wiring is checked BEFORE the busy check so `Parked` cannot be a lie: `spawn_overlay_placement_save`
    /// silently returns when no dir/doc is wired, so a request parked without this check could re-fire
    /// straight into that return and drop the write with the dirty flag already cleared.
    #[must_use]
    pub(super) fn request_overlay_placement_save(&mut self) -> PlacementSaveDispatch {
        if !self.text_persistence_wired() {
            return PlacementSaveDispatch::NotWired;
        }
        if self.save_rx.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.edit_render_rx.is_some()
        {
            self.save_requested_while_busy = true;
            return PlacementSaveDispatch::Parked;
        }
        self.spawn_overlay_placement_save();
        PlacementSaveDispatch::Started
    }

    /// Whether text persistence has a destination at all: a staging `layers/` dir AND the shared doc.
    ///
    /// Both are wired once when the chapter loader starts (`ensure_loader_started` / `set_layer_doc`)
    /// and are never unset for the session, so a `true` here stays true — which is what lets a parked
    /// request assume it will still have somewhere to write when it re-fires.
    #[must_use]
    fn text_persistence_wired(&self) -> bool {
        self.layers_primary_dir.is_some() && self.layer_doc.is_some()
    }

    /// Syncs every text overlay's full MODEL state (geometry + deform + grouping + mask_clip) from the
    /// local runtimes into its shared-doc Text node, grouped per page so each resident page is synced
    /// once. Returns the set of pages that carry text (callers flush exactly those). The render image +
    /// `render_data` are pushed into the doc by `set_text_render` at render time, so this only needs to
    /// reconcile the placement/grouping fields that drag/group edits change.
    pub(super) fn sync_overlay_state_into_doc(&mut self) -> std::collections::BTreeSet<usize> {
        let mut pages_with_text: std::collections::BTreeSet<usize> =
            std::collections::BTreeSet::new();
        #[allow(clippy::type_complexity)]
        let mut state_by_page: HashMap<
            usize,
            Vec<(
                String,
                crate::models::layer_model::manifest::TransformRec,
                Option<crate::models::layer_model::manifest::DeformRec>,
                Option<u32>,
                Option<bool>,
            )>,
        > = HashMap::new();
        for overlay in &self.overlays {
            if overlay.kind != TypingOverlayKind::Text {
                continue;
            }
            pages_with_text.insert(overlay.page_idx);
            let transform = overlay.transform_rec();
            let deform = overlay.deform_mesh.as_ref().map(|m| {
                crate::models::layer_model::manifest::DeformRec {
                    cols: m.cols,
                    rows: m.rows,
                    points_px: m.points_px.clone(),
                }
            });
            state_by_page.entry(overlay.page_idx).or_default().push((
                overlay.uid.clone(),
                transform,
                deform,
                u32::try_from(overlay.layer_idx).ok(),
                Some(overlay.mask_clip_enabled),
            ));
        }
        for (page_idx, states) in state_by_page {
            self.route_to_doc(page_idx, |doc| {
                for (uid, transform, deform, layer_idx, mask_clip) in &states {
                    doc.set_transform(page_idx, uid, *transform);
                    if let Some(node) = doc.node_mut(page_idx, uid) {
                        node.deform = deform.clone();
                        node.text_layer_idx = *layer_idx;
                        if let crate::models::layer_model::layer_doc::NodeBody::Text {
                            mask_clip: mc,
                            ..
                        } = &mut node.body
                        {
                            *mc = *mask_clip;
                        }
                    }
                }
            });
        }
        pages_with_text
    }

    /// Synchronously flushes ONE page's CURRENT doc text to the staging `layers/` dir. Used right before
    /// creating a raster on `page_idx` so the staged page reflects the doc (including a deleted-last-text
    /// page → present-but-empty), preventing `add_page_raster`/`ensure_page_staged` from re-seeding stale
    /// committed text. A no-op if no doc/dir is wired or the page is not resident. The page is flushed
    /// even when it has zero text — `write_page_text_payload` keeps a previously-existing page
    /// present-but-empty, making the deletion durable. (`flush_page_text` for an empty page does no PNG
    /// IO, so this is cheap on the UI thread.)
    pub(super) fn flush_target_page_text_to_staging(&mut self, page_idx: usize) {
        let Some(layers_dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback_dir = self.layers_fallback_dir.clone();
        // Reconcile the local overlay placement into the doc first (so the flush writes current state),
        // then flush only the target page.
        self.sync_overlay_state_into_doc();
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(mut guard) = doc.lock() else {
            return;
        };
        // INTENTIONALLY SYNCHRONOUS (not enqueued): the caller spawns a worker that immediately reads
        // this page's on-disk staging `layers.json` via `add_page_raster`. The anti-resurrection
        // contract requires the page to be PRESENT (possibly empty) on disk BEFORE that read. An async
        // enqueue would race the worker (it could read stale committed text before the enqueued write
        // lands), resurrecting a deleted-last-text overlay. We cannot barrier on the GUI thread, and
        // the empty-page case does no PNG IO, so a direct synchronous flush is both correct and cheap.
        if let Err(err) = guard.flush_page_text(page_idx, &layers_dir, fallback_dir.as_deref()) {
            crate::runtime_log::log_warn(format!(
                "[typing] flush target page {page_idx} text before raster create: {err}"
            ));
        }
    }

    /// Spawns the detached placement-save worker for the current live state.
    ///
    /// Callers must have established that persistence is wired (`text_persistence_wired`) — the
    /// early returns below are the last line of defence, and they LOG rather than return silently:
    /// reaching them means a caller believed a write was dispatched when none was.
    pub(super) fn spawn_overlay_placement_save(&mut self) {
        // Text persistence is now owned by the shared doc: route each text overlay's MODEL state into
        // the doc, then flush the doc's INLINE v3 text payload to `layers.json` (staging `layers/`
        // dir). Nothing writes `text_info.json` anymore — the doc is the sole text writer, mirroring
        // how rasters persist.
        let Some(layers_dir) = self.layers_primary_dir.clone() else {
            crate::runtime_log::log_warn(
                "[typing] placement save skipped: no staging layers dir is wired, nothing was written",
            );
            return;
        };
        let fallback_dir = self.layers_fallback_dir.clone();
        let pages_with_text = self.sync_overlay_state_into_doc();

        // Flush the doc's text payload on a worker thread (PNG re-encode is off the UI thread). The
        // doc lock is shared via the Arc; `flush_page_text` writes only text nodes, leaving rasters
        // untouched on disk.
        let Some(doc) = self.layer_doc.clone() else {
            crate::runtime_log::log_warn(
                "[typing] placement save skipped: no shared layer document is wired, nothing was written",
            );
            return;
        };
        let pages: Vec<usize> = pages_with_text.into_iter().collect();
        crate::trace_log!(
            cat::PERSIST,
            "spawn_overlay_placement_save pages={:?}",
            pages
        );
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        thread::spawn(move || {
            let result = (|| {
                let mut guard = doc.lock().map_err(|_| "doc lock poisoned".to_string())?;
                for page_idx in pages {
                    // ASYNC: enqueue to the coalescing saver (falls back to sync flush when no saver).
                    // This is the placement autosave; no reader depends on it landing synchronously,
                    // and the save-to-project / app-close barriers guarantee durability.
                    guard.enqueue_page_text_save(page_idx, &layers_dir, fallback_dir.as_deref())?;
                }
                Ok(())
            })();
            let _ = tx.send(result);
        });
        self.save_rx = Some(rx);
    }

    /// Synchronously flushes text into the staging `layers/` dir for EVERY page the shared doc has
    /// resident (not just pages with typing-tab overlays), so staging is text-complete for every page
    /// the session loaded — including deletions and pages only PS visited (which load text into the doc
    /// too). `outcome.owned_pages` is the set of OWNED text pages (the doc-resident pages flushed) and
    /// keeps its exact meaning: the save-to-project merge replaces those pages wholesale (authoritative,
    /// incl. deletions) and PRESERVES committed text for pages NOT in this set (never loaded this
    /// session → the session doesn't own their text, so a raster-only PS edit must not drop their
    /// committed text). Mirrors the PS editor's `flush_layers`; best-effort per page.
    ///
    /// Also settles the DEFERRED-save state, but only on a fully successful run: this flush is a
    /// superset of whatever a deferred edit was waiting to write, so the eager callers
    /// (save-to-project, the page-op quiesce) make the next flush point a cheap no-op — while a partial
    /// or impossible flush leaves the edit dirty to be retried.
    ///
    /// # Errors
    /// Returns [`TypingTextFlushError`] when the flush could not run AT ALL: no staging dir, no shared
    /// doc, or a poisoned doc lock. An `Ok` outcome with an empty `owned_pages` means the flush RAN and
    /// there were simply no resident pages — the two were indistinguishable while this returned a bare
    /// set, so a deferred flush point could not tell a failed dispatch from an empty one and cleared
    /// its dirty state either way.
    pub fn flush_text_layers(
        &mut self,
    ) -> Result<TypingTextFlushOutcome, TypingTextFlushError> {
        let _persist_span = crate::trace_scope!(cat::PERSIST, "flush_text_layers");
        let mut outcome = TypingTextFlushOutcome::default();
        let Some(layers_dir) = self.layers_primary_dir.clone() else {
            return Err(TypingTextFlushError::NoLayersDir);
        };
        let fallback_dir = self.layers_fallback_dir.clone();
        // Push the live overlay MODEL state into the doc first (geometry/group/mask-clip edits), then
        // flush EVERY resident doc page's text — not only pages with overlays loaded in this tab.
        self.sync_overlay_state_into_doc();
        let Some(doc) = self.layer_doc.clone() else {
            return Err(TypingTextFlushError::NoLayerDoc);
        };
        let Ok(mut guard) = doc.lock() else {
            return Err(TypingTextFlushError::DocLockPoisoned);
        };
        for page_idx in guard.resident_pages() {
            // ASYNC: enqueue each resident page's text to the coalescing saver (PNG encode off the GUI
            // thread). The save-to-project merge worker barriers the saver BEFORE reading the staging
            // `layers.json`, so every enqueued page is on disk before the merge — the FIFO channel +
            // barrier give the same ordering the old synchronous flush did. A page is marked OWNED on a
            // successful ENQUEUE; save-to-project removes barrier-reported write failures before the
            // merge, so they preserve committed text. An enqueue failure leaves it unowned. With no
            // saver, `enqueue_page_text_save` falls back to a synchronous flush, also correct.
            match guard.enqueue_page_text_save(page_idx, &layers_dir, fallback_dir.as_deref()) {
                Ok(()) => {
                    outcome.owned_pages.insert(page_idx);
                }
                Err(err) => {
                    outcome.failed_pages += 1;
                    crate::runtime_log::log_warn(format!(
                        "[typing] flush text page {page_idx} to layers.json failed: {err}"
                    ));
                }
            }
        }
        drop(guard);
        // This flush persists EVERY resident page's text, which is a superset of whatever a deferred
        // edit was waiting to write, so a fully successful run satisfies any pending deferral. Clearing
        // here means the eager callers (save-to-project, the page-op quiesce) also settle the deferred
        // state, making a later flush point a cheap no-op instead of a redundant second write.
        //
        // Cleared only AFTER the writes were dispatched, and only when EVERY resident page enqueued: an
        // early return above wrote nothing, and a partial failure did not persist the live state, so in
        // both cases the edit must stay dirty for the next flush point to retry.
        if outcome.failed_pages == 0 {
            self.clear_placement_save_dirty();
        }
        crate::trace_log!(
            cat::PERSIST,
            "flush_text_layers owned_pages={} failed_pages={}",
            outcome.owned_pages.len(),
            outcome.failed_pages
        );
        Ok(outcome)
    }

    pub(super) fn round_all_overlay_positions_to_pixels(&mut self) {
        let mut changed_indices = Vec::new();
        for (idx, overlay) in self.overlays.iter_mut().enumerate() {
            let previous_center = overlay.center_page_px;
            overlay.center_page_px = [
                overlay.center_page_px[0].round(),
                overlay.center_page_px[1].round(),
            ];
            if overlay.center_page_px != previous_center {
                changed_indices.push(idx);
            }
        }
        if changed_indices.is_empty() {
            return;
        }
        for idx in changed_indices {
            self.mark_overlay_geometry_changed(idx, false);
        }
        // EDIT (pixel-rounding of placements): deferred to the next flush point.
        self.mark_placement_save_dirty();
    }
}
