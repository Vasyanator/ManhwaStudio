/*
File: tab/doc_layers.rs

Purpose:
Unified layer-document integration for the typing tab: projecting the shared
`LayerDoc` into this tab's per-page runtime state and routing edits back to it.
Covers band-Z reordering (text + raster interleaved), page-size resolution,
raster-layer loading, doc<->tab sync/route helpers, single-raster drawing, unified
hit-testing, per-frame canvas bookkeeping, layout-editor state queries, and the
GPU-cache snapshot/eviction methods on `TypingTextOverlayLayer`.

Notes:
Extracted verbatim from `tab.rs`. Methods are `pub(super)` so `tab.rs` and sibling
submodules of `tab` can use them. `use super::*;` pulls in the parent module's
types and imports. Struct/enum definitions and the rest of the big
`impl TypingTextOverlayLayer` block remain in `tab.rs`; these methods reach the
private items that stay there as descendants of module `tab`.
*/

use super::*;

impl TypingTextOverlayLayer {
    /// Stores the app-owned shared unified layer document (see `layer_doc`).
    pub(super) fn set_layer_doc(
        &mut self,
        doc: std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>,
    ) {
        self.layer_doc = Some(doc);
    }

    /// Flattens the page's unified bands (from `self.bands_by_page`) into one `BandRef` per node,
    /// bottom-to-top, expanding each `TextGroup` band into its member text overlays as `PinnedText`
    /// refs sub-ordered by ascending page-Y (lower on the page = lower in the stack), mirroring
    /// `draw_composite`'s tiebreak and the PS unified order. Used to move a SINGLE text within (or out
    /// of) its text group: once flattened, every text owns its own pinned band so it can be reordered
    /// independently. (This is the per-page on-demand pinning the guardrail allows; the `layer_idx`
    /// grouping axis is untouched for other pages.)
    pub(super) fn flatten_page_bands_to_refs(
        &self,
        page_idx: usize,
    ) -> Vec<crate::models::layer_model::persist::BandRef> {
        use crate::models::layer_model::ordering::Band;
        use crate::models::layer_model::persist;
        let Some(bands) = self.bands_by_page.get(&page_idx) else {
            return Vec::new();
        };
        let mut sorted: Vec<&Band> = bands.iter().collect();
        sorted.sort_by_key(|b| b.z());
        let mut order: Vec<persist::BandRef> = Vec::new();
        for band in sorted {
            match band {
                Band::Raster { uid, .. } => order.push(persist::BandRef::Raster(uid.clone())),
                Band::PinnedText { uid, .. } => {
                    order.push(persist::BandRef::PinnedText(uid.clone()));
                }
                Band::TextGroup { member_uids, .. } => {
                    let mut members = member_uids.clone();
                    members.sort_by(|a, b| {
                        let ya = self.overlay_page_y(a);
                        let yb = self.overlay_page_y(b);
                        ya.partial_cmp(&yb).unwrap_or(std::cmp::Ordering::Equal)
                    });
                    for uid in members {
                        order.push(persist::BandRef::PinnedText(uid));
                    }
                }
            }
        }
        order
    }

    /// Page-Y (vertical center) of an overlay by uid, for the page-Y sub-order of a text group.
    pub(super) fn overlay_page_y(&self, uid: &str) -> f32 {
        self.overlays
            .iter()
            .find(|o| o.uid == uid)
            .map_or(0.0, |o| o.center_page_px[1])
    }

    /// Moves an INDIVIDUAL text/image overlay one step in the page's UNIFIED band-Z order (text +
    /// raster interleaved), shared with the PS editor. `up` raises it toward the top, `down` lowers it.
    ///
    /// Routed exactly like the PS editor's band move: the page's bands are flattened so the target owns
    /// its own pinned band (a text inside a group is pinned OUT of the group's page-Y auto-order for
    /// this page only), the target is swapped one step, the new order is persisted via
    /// `persist::save_page_band_order` (the disk authority for pin + Z — which a later `flush_page_text`
    /// then PRESERVES via `merge_preserved_text_fields`, so the reorder is never clobbered), and the
    /// SAME order is mirrored into the shared doc via `set_z_order` so both tabs re-project in step.
    pub(super) fn move_overlay_in_unified_z(&mut self, page_idx: usize, overlay_idx: usize, up: bool) {
        let Some(uid) = self.overlays.get(overlay_idx).map(|o| o.uid.clone()) else {
            return;
        };
        self.move_node_in_unified_z(page_idx, &uid, up);
    }

    /// Moves a RASTER one step in the page's unified band-Z order (text + raster interleaved). Resolves
    /// the raster's uid from `raster_layers_by_page[page][raster_idx]` and reuses the shared band-Z core.
    pub(super) fn move_raster_in_unified_z(&mut self, page_idx: usize, raster_idx: usize, up: bool) {
        let resolved = self
            .raster_layers_by_page
            .get(&page_idx)
            .and_then(|v| v.get(raster_idx))
            .map(|l| l.uid.clone());
        crate::trace_log!(
            cat::TYPING,
            "move_raster_in_unified_z page={} idx={} up={} uid={:?}",
            page_idx,
            raster_idx,
            up,
            resolved
        );
        let Some(uid) = resolved else {
            return;
        };
        self.move_node_in_unified_z(page_idx, &uid, up);
    }

    /// Uid-based core: moves the node `uid` (a raster or a text/image overlay) one step in the page's
    /// unified band-Z order. Flattens the page's bands to per-node refs, swaps the target one step with
    /// its neighbour, persists the new band order via `save_page_band_order` (the disk authority both
    /// tabs read back), and mirrors the SAME order into the shared doc via `set_z_order`. Shared by the
    /// overlay and raster reorder entry points.
    pub(super) fn move_node_in_unified_z(&mut self, page_idx: usize, uid: &str, up: bool) {
        use crate::models::layer_model::persist;
        let Some(primary) = self.layers_primary_dir.clone() else {
            return;
        };

        // Ensure the page's rasters have on-disk manifest nodes BEFORE `save_page_band_order`:
        // `apply_band_order` silently SKIPS a `BandRef::Raster` whose node is not yet in the manifest,
        // and the typing tab otherwise only flushes TEXT — so a raster's new Z would never reach disk
        // (the doc move below would show it moved, then it would revert on the next reload). Mirrors
        // the PS editor's pre-reorder flush; `persist_current_page_rasters` uses the SYNCHRONOUS
        // `doc.flush_page`, so the raster is on disk before the band-order write reassigns its Z.
        self.persist_current_page_rasters(page_idx);

        // Flatten to per-node bands, then swap the target one step with its neighbour.
        let mut order = self.flatten_page_bands_to_refs(page_idx);
        let n_raster_bands = order
            .iter()
            .filter(|b| matches!(b, persist::BandRef::Raster(_)))
            .count();
        let target_pos = order.iter().position(|b| matches!(
            b,
            persist::BandRef::PinnedText(u) | persist::BandRef::Raster(u) if u == uid
        ));
        crate::trace_log!(
            cat::TYPING,
            "move_node_in_unified_z uid={} up={} order_len={} raster_bands={} target_pos={:?}",
            uid,
            up,
            order.len(),
            n_raster_bands,
            target_pos
        );
        let Some(i) = target_pos else {
            return;
        };
        let j = if up { i + 1 } else { i.wrapping_sub(1) };
        if (up && j >= order.len()) || (!up && i == 0) {
            crate::trace_log!(
                cat::TYPING,
                "move_node_in_unified_z uid={} at-end i={} len={} -> no-op",
                uid,
                i,
                order.len()
            );
            return; // already at the requested end
        }
        order.swap(i, j);

        // Persist the new band order (pin + Z) to disk — the authority both tabs read back.
        match persist::save_page_band_order(&primary, page_idx, &order) {
            Ok(()) => {
                // Drop the cached bands so the next projection reloads the new pinned-band order.
                self.bands_by_page.remove(&page_idx);
                // Mirror the SAME order into the shared doc so it (and, via its version bump, the PS
                // tab) re-projects without a disk round-trip.
                let node_order: Vec<String> = order
                    .iter()
                    .filter_map(|b| match b {
                        persist::BandRef::Raster(u) | persist::BandRef::PinnedText(u) => {
                            Some(u.clone())
                        }
                        persist::BandRef::TextGroup(_) => None,
                    })
                    .collect();
                let routed = self.route_to_doc(page_idx, |doc| {
                    doc.set_z_order(page_idx, &node_order);
                });
                crate::trace_log!(
                    cat::TYPING,
                    "move_node_in_unified_z persisted+routed uid={} node_order_len={} routed={}",
                    uid,
                    node_order.len(),
                    routed
                );
                if !routed {
                    // No doc wired / page not resident: drop the raster cache too so it reloads.
                    self.raster_layers_by_page.remove(&page_idx);
                }
            }
            Err(e) => crate::runtime_log::log_warn(&format!(
                "не удалось изменить порядок слоя в общем Z: {e}"
            )),
        }
    }

    /// Once-per-frame check: if the shared `LayerDoc` changed since we last projected (its `version`
    /// advanced), and we are idle (not loading/saving), re-project the current page from the doc.
    ///
    /// The doc is the in-memory source of truth shared with the PS tab, so any edit there (or our own
    /// that routed through the doc) bumps `version`; we just `sync_from_doc(current_page)` to rebuild
    /// this tab's projections. This is the in-memory cross-tab sync (no disk reload, no revision Arc).
    pub(super) fn maybe_reproject_from_doc_version(&mut self, current_page: usize) {
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        // Don't fight in-flight work; we'll pick the change up on a later frame.
        if self.loading_rx.is_some()
            || self.save_rx.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.edit_render_rx.is_some()
        {
            return;
        }
        let Ok(guard) = doc.lock() else {
            return;
        };
        if guard.version() == self.last_doc_version {
            return;
        }
        if guard.page(current_page).is_some() {
            crate::trace_log!(
                cat::SYNC,
                "reproject_from_doc page={} old_version={} new_version={} resident=true",
                current_page,
                self.last_doc_version,
                guard.version()
            );
            self.sync_from_doc(current_page, &guard);
        } else {
            // The current page is not resident (e.g. just evicted by a self-write that will reload it
            // shortly). Adopt the version so we don't spin; the page-load path re-projects on arrival.
            crate::trace_log!(
                cat::SYNC,
                "reproject_from_doc page={} old_version={} new_version={} resident=false adopt_only",
                current_page,
                self.last_doc_version,
                guard.version()
            );
            self.last_doc_version = guard.version();
        }
    }

    /// Page pixel size `[w, h]` for `page_idx`, resolved lazily from the cached page image path
    /// (header-only `image_dimensions`) and memoized. Used for legacy-overlay uv→px decoding when the
    /// page is handed to the shared doc. Falls back to `[1, 1]` when unknown.
    pub(super) fn page_size_px(&mut self, page_idx: usize) -> [usize; 2] {
        if let Some(size) = self.page_sizes_px.get(&page_idx) {
            return *size;
        }
        let size = self
            .page_image_paths
            .get(&page_idx)
            .and_then(|path| image::image_dimensions(path).ok())
            .map(|(w, h)| [w as usize, h as usize])
            .unwrap_or([1, 1]);
        self.page_sizes_px.insert(page_idx, size);
        size
    }

    /// Pixel sizes for EVERY page of the chapter (memoized via [`Self::page_size_px`]). The shared doc
    /// needs the full map — not just the loaded page — because the legacy absolute-ribbon migration
    /// recovers a chapter-wide ribbon scale from every page's aspect ratio.
    pub(super) fn page_sizes_map(&mut self) -> HashMap<usize, [usize; 2]> {
        let pages: Vec<usize> = self.page_image_paths.keys().copied().collect();
        let mut out = HashMap::with_capacity(pages.len());
        for idx in pages {
            out.insert(idx, self.page_size_px(idx));
        }
        out
    }

    /// (Re)loads the read-only PS raster layers for `page_idx` if not already cached for it.
    pub(super) fn ensure_raster_layers_for_page(&mut self, page_idx: usize) {
        if self.raster_layers_by_page.contains_key(&page_idx) {
            return;
        }
        let Some(primary) = self.layers_primary_dir.clone() else {
            self.raster_layers_by_page.insert(page_idx, Vec::new());
            self.bands_by_page.insert(page_idx, Vec::new());
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        // Unified per-page Z bands, used to interleave rasters with overlays in one ordered pass.
        let bands = crate::models::layer_model::persist::load_page_bands(
            &primary,
            fallback.as_deref(),
            page_idx,
        );
        self.bands_by_page.insert(page_idx, bands);
        let loaded = crate::models::layer_model::persist::load_page_rasters(
            &primary,
            fallback.as_deref(),
            page_idx,
        );
        let layers = match loaded {
            Ok(page) => page
                .layers
                .into_iter()
                .map(|l| TypingRasterLayer {
                    uid: l.uid,
                    name: l.name,
                    visible: l.visible,
                    opacity: l.opacity,
                    transform: l.transform,
                    image: l.image,
                    base_file: l.base_file,
                    effects: l.effects,
                    deform: l.deform,
                    mask_clip_enabled: l.mask_clip.unwrap_or(false),
                    clipped_image: None,
                    texture: None,
                })
                .collect(),
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[typing] load PS raster layers for page {page_idx} failed: {err}"
                ));
                Vec::new()
            }
        };
        // A just-created raster asked to be selected once its page reloaded — resolve by uid now.
        if let Some((pending_page, uid)) = self.pending_select_raster_uid.clone()
            && pending_page == page_idx
            && let Some(idx) = layers.iter().position(|l| l.uid == uid)
        {
            self.selected_raster_idx = Some(idx);
            self.selected_overlay_idx = None;
            self.pending_select_raster_uid = None;
        }
        self.raster_layers_by_page.insert(page_idx, layers);
        // The shared unified layer document is the source of truth for layer MODEL state. Ensure the
        // page is resident, then rebuild the per-page projections (rasters / overlays / bands) from
        // it, overriding the disk-loaded caches above so both tabs read one model.
        // Full chapter page sizes: the legacy ribbon migration in the doc needs every page's aspect.
        let page_sizes = self.page_sizes_map();
        if let Some(doc) = self.layer_doc.clone()
            && let Ok(mut doc_guard) = doc.lock()
        {
            let _ = doc_guard.ensure_page_loaded(page_idx, &primary, fallback.as_deref(), &page_sizes);
            self.sync_from_doc(page_idx, &doc_guard);
        }
    }

    /// Rebuilds the per-page projections (`raster_layers_by_page`, `overlays`, `bands_by_page`) for
    /// `page_idx` from the resident `LayerDoc` page, which is the source of truth for MODEL state
    /// (transform, deform, effects, display pixels, render_data, z, visibility, opacity, group).
    ///
    /// Runtime/GPU/UI state is kept LOCAL and matched to nodes by `uid`:
    /// - Rasters: a fresh `TypingRasterLayer` per doc Raster node; the GPU texture is preserved
    ///   across rebuilds via `raster_texture_generations` and only dropped (forcing re-upload) when
    ///   the node's `generation` changed.
    /// - Overlays: each doc Text node is reconciled onto the existing `TypingOverlayRuntime` with the
    ///   same uid — its MODEL fields are updated from the node while runtime fields (texture, upload
    ///   state, payload tracking) are preserved; the GPU texture is re-uploaded only on a generation
    ///   change. Runtime REMOVAL stays owned by `remove_overlay` / the disk loader, so the projected
    ///   overlay indices are stable across a sync.
    /// - Bands: one `Raster`/`PinnedText` band per node, with `z` taken directly from the node.
    pub(super) fn sync_from_doc(
        &mut self,
        page_idx: usize,
        doc: &crate::models::layer_model::layer_doc::LayerDoc,
    ) {
        use crate::models::layer_model::layer_doc::NodeBody;
        use crate::models::layer_model::ordering::Band;
        let _sync_span = crate::trace_scope!(
            cat::SYNC,
            "sync_from_doc page={} doc_version={}",
            page_idx,
            doc.version()
        );
        let Some(page) = doc.page(page_idx) else {
            return;
        };

        // --- Rasters: one projected layer per doc Raster node, texture preserved by generation. ---
        // Capture the OLD positional → uid mapping before the rebuild. `selected_raster_idx`,
        // `transform_mode_raster_idx`, and `raster_drag_state.raster_idx` are positions into THIS page's
        // raster list, which `sync_from_doc` rebuilds in z-order every reproject. After a raster reorder
        // (⬆/⬇, or a PS reorder that reprojects), a positional index would point at a DIFFERENT raster, so
        // a transform/delete would hit the wrong layer. We resolve each tracked index to its uid here and
        // remap to the uid's NEW position after the rebuild (clearing it if the raster is gone).
        let prev_raster_uids: Vec<String> = self
            .raster_layers_by_page
            .get(&page_idx)
            .map(|layers| layers.iter().map(|l| l.uid.clone()).collect())
            .unwrap_or_default();
        let selected_raster_uid = self
            .selected_raster_idx
            .and_then(|i| prev_raster_uids.get(i).cloned());
        let transform_raster_uid = self
            .transform_mode_raster_idx
            .and_then(|i| prev_raster_uids.get(i).cloned());
        let drag_raster_uid = self
            .raster_drag_state
            .as_ref()
            .and_then(|d| prev_raster_uids.get(d.raster_idx).cloned());

        let mut prev_rasters: HashMap<String, egui::TextureHandle> = self
            .raster_layers_by_page
            .remove(&page_idx)
            .map(|layers| {
                layers
                    .into_iter()
                    .filter_map(|l| l.texture.map(|t| (l.uid, t)))
                    .collect()
            })
            .unwrap_or_default();

        let mut rasters: Vec<TypingRasterLayer> = Vec::new();
        for node in &page.nodes {
            let NodeBody::Raster {
                display_image,
                effects,
                base_file,
                mask_clip,
                ..
            } = &node.body
            else {
                continue;
            };
            // Preserve the GPU texture when the generation the texture was built from is unchanged. The
            // mask-clip toggle bumps the node generation, so this invalidates the texture (and the
            // cached clipped image below) → re-clip + re-upload.
            let cache_key = (page_idx, node.uid.clone());
            let gen_unchanged = self.raster_texture_generations.get(&cache_key).copied()
                == Some(node.generation);
            let texture = if gen_unchanged {
                prev_rasters.remove(&node.uid)
            } else {
                self.raster_texture_generations
                    .insert(cache_key, node.generation);
                None
            };
            rasters.push(TypingRasterLayer {
                uid: node.uid.clone(),
                name: node.name.clone(),
                visible: node.visible,
                opacity: node.opacity,
                transform: node.transform,
                image: display_image.clone(),
                base_file: base_file.clone(),
                effects: effects.clone(),
                deform: node.deform.clone(),
                mask_clip_enabled: mask_clip.unwrap_or(false),
                // A generation change (e.g. a mask-clip toggle) invalidates the cached clipped image.
                clipped_image: None,
                texture,
            });
        }
        // Any textures left in `prev_rasters` belonged to nodes whose generation changed (or which
        // are gone); they are dropped here, freeing their GPU handles.
        drop(prev_rasters);

        // Remap the tracked raster indices to their uid's NEW position in the rebuilt z-ordered list, so
        // a reorder doesn't silently retarget selection / transform / drag to a different raster. Only
        // touch a field when we resolved a uid for THIS page above (so a selection on another page, or a
        // freshly-set index, is left alone). A uid that's gone (deleted) clears the field.
        if let Some(uid) = &selected_raster_uid {
            self.selected_raster_idx = rasters.iter().position(|l| &l.uid == uid);
        }
        if let Some(uid) = &transform_raster_uid {
            self.transform_mode_raster_idx = rasters.iter().position(|l| &l.uid == uid);
        }
        if let Some(uid) = &drag_raster_uid {
            match rasters.iter().position(|l| &l.uid == uid) {
                Some(new_idx) => {
                    if let Some(drag) = self.raster_drag_state.as_mut() {
                        drag.raster_idx = new_idx;
                    }
                }
                None => self.raster_drag_state = None,
            }
        }

        self.raster_layers_by_page.insert(page_idx, rasters);

        // --- Overlays: reconcile-OR-CREATE doc Text nodes onto the local runtimes by uid (this page). ---
        // The doc is the source of truth for text. For a runtime that already exists (in-session-created,
        // already-projected, or loaded from legacy `text_info.json`) we reconcile its MODEL fields. For a
        // doc Text node with NO local runtime we MATERIALIZE one from the node (mirrors PS's
        // `sync_view_from_doc`). Without this, a MIGRATED chapter — whose `text_info.json` is retired to
        // `.bak`, so the legacy disk loader populates no `self.overlays` — would show no text in the
        // typing tab even though PS and the doc carry it. The runtime's deterministic rendered-PNG name
        // (`text_image_file_name`) is the same the doc's text flush writes, so a later placement-save
        // round-trips.
        let mut to_requeue: Vec<usize> = Vec::new();
        for node in &page.nodes {
            let NodeBody::Text { render_data, image, mask_clip, .. } = &node.body else {
                continue;
            };
            let center = [node.transform.cx, node.transform.cy];
            let angle_deg = node.transform.rotation.to_degrees();
            let user_scale = node.transform.scale;
            let size_px = image.size;
            let deform_mesh = node.deform.as_ref().and_then(|d| {
                TypingOverlayDeformMesh::new(d.cols, d.rows, d.points_px.clone(), size_px)
            });
            let render_data_json = if render_data.is_null() {
                None
            } else {
                Some(render_data.clone())
            };

            let cache_key = (page_idx, node.uid.clone());
            let existing_idx = self
                .overlays
                .iter()
                .position(|o| o.uid == node.uid && o.page_idx == page_idx);

            match existing_idx {
                Some(idx) => {
                    // Reconcile MODEL fields; preserve runtime/payload-tracking fields.
                    let pixels_changed = self.raster_texture_generations.get(&cache_key).copied()
                        != Some(node.generation);
                    let rt = &mut self.overlays[idx];
                    rt.center_page_px = center;
                    rt.angle_deg = angle_deg;
                    rt.user_scale = user_scale;
                    rt.deform_mesh = deform_mesh;
                    rt.render_data_json = render_data_json;
                    if pixels_changed {
                        rt.size_px = size_px;
                        rt.source_rgba = color_image_to_rgba(image);
                        rt.display_texture_stale = true;
                        self.raster_texture_generations
                            .insert(cache_key, node.generation);
                        to_requeue.push(idx);
                    }
                }
                None => {
                    // CREATE: materialize a runtime from the doc node (migrated-chapter case).
                    let runtime = text_runtime_from_doc_node(
                        &node.uid,
                        page_idx,
                        center,
                        user_scale,
                        angle_deg,
                        deform_mesh,
                        mask_clip.unwrap_or(false),
                        node.text_layer_idx.unwrap_or(0) as usize,
                        render_data_json,
                        size_px,
                        color_image_to_rgba(image),
                    );
                    self.overlays.push(runtime);
                    let idx = self.overlays.len() - 1;
                    // Mark the texture generation as projected so a subsequent sync doesn't needlessly
                    // re-upload, and queue this frame's upload so it renders immediately.
                    self.raster_texture_generations
                        .insert(cache_key, node.generation);
                    to_requeue.push(idx);
                }
            }
        }
        for idx in to_requeue {
            self.queue_overlay_texture_upload(idx);
        }
        // Note: runtime REMOVAL is owned by `remove_overlay` (which also fixes the positional upload
        // queue + selection indices) and by the disk loader on a full reload; `sync_from_doc` does
        // not drop runtimes, so the projected overlay indices stay stable across a sync.

        // --- Bands: derive unified Z directly from the doc node z. ---
        let mut bands: Vec<Band> = Vec::with_capacity(page.nodes.len());
        for node in &page.nodes {
            match node.kind {
                crate::models::layer_model::layer_doc::NodeKind::Raster => {
                    bands.push(Band::Raster {
                        uid: node.uid.clone(),
                        z: node.z,
                    });
                }
                crate::models::layer_model::layer_doc::NodeKind::Text => {
                    bands.push(Band::PinnedText {
                        uid: node.uid.clone(),
                        z: node.z,
                    });
                }
            }
        }
        self.bands_by_page.insert(page_idx, bands);

        // A just-created raster asked to be selected once its page synced — resolve by uid now.
        if let Some((pending_page, uid)) = self.pending_select_raster_uid.clone()
            && pending_page == page_idx
            && let Some(idx) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|ls| ls.iter().position(|l| l.uid == uid))
        {
            self.selected_raster_idx = Some(idx);
            self.selected_overlay_idx = None;
            self.pending_select_raster_uid = None;
        }

        // Record the doc version we just projected so the per-frame `maybe_reproject_from_doc_version`
        // check does not redundantly re-project until the doc changes again.
        self.last_doc_version = doc.version();
    }

    /// Routes an edit to the shared `LayerDoc`: locks it, runs `edit` against the resident page (it
    /// must already be loaded via `ensure_raster_layers_for_page`), then rebuilds the per-page
    /// projections from the doc with `sync_from_doc`. No-op (returns false) if no doc is wired; the
    /// caller then keeps its legacy local-cache + disk path. Returns true when the doc handled it.
    pub(super) fn route_to_doc<F>(&mut self, page_idx: usize, edit: F) -> bool
    where
        F: FnOnce(&mut crate::models::layer_model::layer_doc::LayerDoc),
    {
        let Some(doc) = self.layer_doc.clone() else {
            return false;
        };
        let Ok(mut guard) = doc.lock() else {
            return false;
        };
        if guard.page(page_idx).is_none() {
            // The page is not resident in the doc; let the caller fall back to its legacy path.
            return false;
        }
        edit(&mut guard);
        // Guarantee a cross-tab notification even if `edit` mutated node fields directly via
        // `node_mut` (which does not bump the version). Idempotent if `edit` already bumped.
        guard.mark_changed();
        self.sync_from_doc(page_idx, &guard);
        true
    }

    /// Draws a single cached read-only PS raster layer (by page + index) into `painter`, lazily
    /// uploading its texture via `ctx`. Uses the same page-px -> scene mapping (`scene_from_page_px`)
    /// as the text overlays. Visibility/opacity handling matches `draw_page_raster_layers`.
    pub(super) fn draw_one_raster_layer(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        page_idx: usize,
        raster_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|layers| layers.get_mut(raster_idx))
        else {
            return;
        };
        if !layer.visible || layer.opacity <= 0.0 {
            return;
        }
        let [w, h] = layer.image.size;
        if w == 0 || h == 0 {
            return;
        }
        // Use the mask-clipped image when mask-clip is on (precomputed in `prepare_raster_mask_clips`),
        // else the plain display image.
        let upload_image = layer
            .clipped_image
            .as_ref()
            .filter(|_| layer.mask_clip_enabled)
            .unwrap_or(&layer.image)
            .clone();
        let texture = layer.texture.get_or_insert_with(|| {
            ctx.load_texture("typing_ps_raster_layer", upload_image, TextureOptions::LINEAR)
        });
        let texture_id = texture.id();
        // Deformed raster: positioned by its cols×rows mesh (absolute page px), exactly like a
        // deformed text overlay. The affine transform does not apply while deformed.
        if let Some(grid) = &layer.deform {
            if grid.cols >= 2 && grid.rows >= 2 && grid.points_px.len() == grid.cols * grid.rows {
                let mesh_scene: Vec<Pos2> = grid
                    .points_px
                    .iter()
                    .map(|p| scene_from_page_px(image_rect, zoom, *p))
                    .collect();
                draw_textured_deform_mesh(painter, texture_id, &mesh_scene, grid.cols, grid.rows);
                return;
            }
        }
        // Transform: center in page px, uniform scale, rotation (radians). Corners are the
        // image quad centered on (cx, cy), scaled and rotated, then mapped page-px -> scene.
        let cx = layer.transform.cx;
        let cy = layer.transform.cy;
        let scale = layer.transform.scale;
        let (sin_a, cos_a) = layer.transform.rotation.sin_cos();
        let hw = w as f32 * 0.5 * scale;
        let hh = h as f32 * 0.5 * scale;
        // Local corner offsets (top-left, top-right, bottom-right, bottom-left).
        let corners = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)];
        let mut quad = [Pos2::ZERO; 4];
        for (i, (dx, dy)) in corners.iter().enumerate() {
            let rx = dx * cos_a - dy * sin_a;
            let ry = dx * sin_a + dy * cos_a;
            quad[i] = scene_from_page_px(image_rect, zoom, [cx + rx, cy + ry]);
        }
        let tint = Color32::from_white_alpha((layer.opacity.clamp(0.0, 1.0) * 255.0) as u8);
        let mut mesh = Mesh::with_texture(texture.id());
        let uvs = [
            Pos2::new(0.0, 0.0),
            Pos2::new(1.0, 0.0),
            Pos2::new(1.0, 1.0),
            Pos2::new(0.0, 1.0),
        ];
        for i in 0..4 {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: quad[i],
                uv: uvs[i],
                color: tint,
            });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(egui::Shape::mesh(mesh));
    }

    /// Unified band Z for a raster (by uid) on `page_idx`: the Z of the matching `Raster` band, or a
    /// top-of-stack key (`bands.len()`) for an unsaved raster not yet in the manifest.
    pub(super) fn raster_band_z(&self, page_idx: usize, uid: &str) -> u32 {
        let Some(bands) = self.bands_by_page.get(&page_idx) else {
            return 0;
        };
        for band in bands {
            if let crate::models::layer_model::ordering::Band::Raster { uid: u, z } = band
                && u == uid
            {
                return *z;
            }
        }
        bands.len() as u32
    }

    /// Unified band Z for an overlay on `page_idx`: if a `PinnedText` band with `uid` exists, its Z;
    /// else the Z of the `TextGroup` band whose `layer_idx == layer_idx`; else a top-of-stack key
    /// (`bands.len()`) for an item not yet in the manifest.
    pub(super) fn overlay_band_z(&self, page_idx: usize, uid: &str, layer_idx: usize) -> u32 {
        use crate::models::layer_model::ordering::Band;
        let Some(bands) = self.bands_by_page.get(&page_idx) else {
            return 0;
        };
        for band in bands {
            if let Band::PinnedText { uid: u, z } = band
                && u == uid
            {
                return *z;
            }
        }
        let layer_idx_u32 = u32::try_from(layer_idx).unwrap_or(u32::MAX);
        for band in bands {
            if let Band::TextGroup {
                layer_idx: li, z, ..
            } = band
                && *li == layer_idx_u32
            {
                return *z;
            }
        }
        bands.len() as u32
    }

    /// The TOPMOST text/image overlay whose scene quad contains `pointer` on `page_idx`, as
    /// `(overlay_idx, unified band-Z)`, or `None` if no overlay is under the pointer. Used by the unified
    /// click hit-test so a raster cannot steal a click that lands on a higher-Z overlay (and vice-versa
    /// once text can sit below a raster). Mirrors `merged_fills`' overlay band-Z lookup.
    pub(super) fn topmost_overlay_at(
        &self,
        page_idx: usize,
        pointer: Option<Pos2>,
        image_rect: Rect,
        zoom: f32,
    ) -> Option<(usize, u32)> {
        let p = pointer?;
        let mut best: Option<(usize, u32)> = None;
        for (idx, overlay) in self.overlays.iter().enumerate() {
            if overlay.page_idx != page_idx || overlay.texture.is_none() {
                continue;
            }
            let quad = overlay_quad_scene(overlay, image_rect, zoom);
            if !point_in_quad(p, &quad) {
                continue;
            }
            let z = self.overlay_band_z(page_idx, &overlay.uid, overlay.layer_idx);
            if best.is_none_or(|(_, bz)| z >= bz) {
                best = Some((idx, z));
            }
        }
        best
    }

    pub(super) fn begin_canvas_frame(&mut self) {
        self.primary_pointer_targets_overlay_this_frame = false;
    }

    pub(super) fn layout_editor_active(&self) -> bool {
        self.layout_editor.is_some()
    }

    pub(super) fn layout_editor_editing_active(&self) -> bool {
        self.layout_editor
            .as_ref()
            .is_some_and(|editor| editor.mode == TypingLayoutEditorMode::Editing)
    }

    pub(super) fn layout_editor_preview_active(&self) -> bool {
        self.layout_editor
            .as_ref()
            .is_some_and(|editor| editor.mode == TypingLayoutEditorMode::Preview)
    }

    pub(super) fn next_shape_variant_preview_id(&mut self) -> u64 {
        self.shape_variant_preview_next_id = self.shape_variant_preview_next_id.wrapping_add(1);
        self.shape_variant_preview_next_id
    }

    pub(super) fn primary_pointer_targets_overlay_this_frame(&self) -> bool {
        self.primary_pointer_targets_overlay_this_frame
    }

    pub(super) fn gpu_memory_snapshot(&self, pinned_pages: &BTreeSet<usize>) -> Vec<CacheResourceInfo> {
        self.overlays
            .iter()
            .enumerate()
            .filter(|(_, overlay)| overlay.texture.is_some())
            .map(|(idx, overlay)| CacheResourceInfo {
                id: format!("typing-text-overlay-gpu:{idx}:{}", overlay.file_name),
                kind: CacheResourceKind::TextOverlayGpu,
                page_idx: Some(overlay.page_idx),
                estimated_bytes: u64::try_from(
                    overlay.size_px[0]
                        .saturating_mul(overlay.size_px[1])
                        .saturating_mul(4),
                )
                .unwrap_or(u64::MAX),
                last_used_frame: overlay.last_texture_used_frame,
                reload_cost: CacheReloadCost::RebuildFromModel,
                dirty: false,
                visible: pinned_pages.contains(&overlay.page_idx),
                reconstructable: !overlay.source_rgba.is_empty(),
            })
            .collect()
    }

    pub(super) fn evict_gpu_cache(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let snapshot = self.gpu_memory_snapshot(&request.pinned_pages);
        let candidates = select_eviction_candidates(&snapshot, request);
        let mut evicted = Vec::new();
        let mut freed = 0_u64;
        for resource in candidates.resources {
            let Some(idx) = resource
                .id
                .strip_prefix("typing-text-overlay-gpu:")
                .and_then(|tail| tail.split(':').next())
                .and_then(|raw| raw.parse::<usize>().ok())
            else {
                continue;
            };
            let Some(overlay) = self.overlays.get_mut(idx) else {
                continue;
            };
            if overlay.texture.take().is_some() {
                overlay.display_texture_stale = true;
                overlay.last_texture_used_frame = 0;
                freed = freed.saturating_add(resource.estimated_bytes);
                evicted.push(resource);
            }
        }
        CacheEvictionReport {
            resources: evicted,
            estimated_freed_bytes: freed,
        }
    }
}
