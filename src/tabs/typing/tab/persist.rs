/*
File: tab/persist.rs

Purpose:
Text-overlay persistence for the typing tab: reconciling live overlay MODEL state
into the shared `LayerDoc` and flushing/saving it to the on-disk `layers.json`
staging payload. Covers background save-job polling, save requests/spawning,
per-page and whole-doc text flushes, and pixel-rounding of overlay positions
(which triggers a placement save).

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

    pub(super) fn request_overlay_placement_save(&mut self) {
        if self.save_rx.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.edit_render_rx.is_some()
        {
            self.save_requested_while_busy = true;
            return;
        }
        self.spawn_overlay_placement_save();
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
            let transform = crate::models::layer_model::manifest::TransformRec {
                cx: overlay.center_page_px[0],
                cy: overlay.center_page_px[1],
                rotation: overlay.angle_deg.to_radians(),
                scale: overlay.user_scale,
            };
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

    pub(super) fn spawn_overlay_placement_save(&mut self) {
        // Text persistence is now owned by the shared doc: route each text overlay's MODEL state into
        // the doc, then flush the doc's INLINE v3 text payload to `layers.json` (staging `layers/`
        // dir). Nothing writes `text_info.json` anymore — the doc is the sole text writer, mirroring
        // how rasters persist.
        let Some(layers_dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback_dir = self.layers_fallback_dir.clone();
        let pages_with_text = self.sync_overlay_state_into_doc();

        // Flush the doc's text payload on a worker thread (PNG re-encode is off the UI thread). The
        // doc lock is shared via the Arc; `flush_page_text` writes only text nodes, leaving rasters
        // untouched on disk.
        let Some(doc) = self.layer_doc.clone() else {
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
    /// too). Returns the set of OWNED text pages (the doc-resident pages flushed): the save-to-project
    /// merge replaces those pages wholesale (authoritative, incl. deletions) and PRESERVES committed
    /// text for pages NOT in this set (never loaded this session → the session doesn't own their text,
    /// so a raster-only PS edit must not drop their committed text). Mirrors the PS editor's
    /// `flush_layers`; best-effort per page.
    pub fn flush_text_layers(&mut self) -> std::collections::HashSet<usize> {
        let _persist_span = crate::trace_scope!(cat::PERSIST, "flush_text_layers");
        let mut owned: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let Some(layers_dir) = self.layers_primary_dir.clone() else {
            return owned;
        };
        let fallback_dir = self.layers_fallback_dir.clone();
        // Push the live overlay MODEL state into the doc first (geometry/group/mask-clip edits), then
        // flush EVERY resident doc page's text — not only pages with overlays loaded in this tab.
        self.sync_overlay_state_into_doc();
        let Some(doc) = self.layer_doc.clone() else {
            return owned;
        };
        let Ok(mut guard) = doc.lock() else {
            return owned;
        };
        for page_idx in guard.resident_pages() {
            // ASYNC: enqueue each resident page's text to the coalescing saver (PNG encode off the GUI
            // thread). The save-to-project merge worker barriers the saver BEFORE reading the staging
            // `layers.json`, so every enqueued page is on disk before the merge — the FIFO channel +
            // barrier give the same ordering the old synchronous flush did. A page is marked OWNED on a
            // successful ENQUEUE (the barrier guarantees the write); an enqueue failure leaves it
            // unowned (fail-safe), so the merge preserves committed text rather than dropping it. With
            // no saver, `enqueue_page_text_save` falls back to a synchronous flush, also correct.
            match guard.enqueue_page_text_save(page_idx, &layers_dir, fallback_dir.as_deref()) {
                Ok(()) => {
                    owned.insert(page_idx);
                }
                Err(err) => crate::runtime_log::log_warn(format!(
                    "[typing] flush text page {page_idx} to layers.json failed: {err}"
                )),
            }
        }
        crate::trace_log!(cat::PERSIST, "flush_text_layers owned_pages={}", owned.len());
        owned
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
        self.request_overlay_placement_save();
    }
}
