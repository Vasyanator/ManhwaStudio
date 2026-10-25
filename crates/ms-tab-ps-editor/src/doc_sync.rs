/*
File: crates/ms-tab-ps-editor/src/doc_sync.rs

Purpose:
Wiring of the shared models into the tab and the doc -> view projection: the shared `LayerDoc` is
the single source of a page's layers once it was decoded.

Key functions:
- `set_overlays_model()`, `set_layer_doc()`: model injection from the studio root.
- `materialize_text_runtime_from_doc()`: builds the local text-layer runtimes from the doc's inline
  payloads.
- `sync_view_from_doc()`: projects the doc over the stack rasters, text model and bands, preserving
  uncommitted pixel edits.
- `route_to_doc()`: applies one edit to the doc (and enqueues the saver).

Notes:
Every method here is an inherent method of `PsEditorTabState` (defined in `lib.rs`).
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

impl PsEditorTabState {
    /// Wires the shared clean-overlay model used as the source/clean layer provider.
    pub fn set_overlays_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        self.overlays_model = Some(model);
    }

    /// Wires the app-owned shared unified layer document (see `layer_doc`).
    pub fn set_layer_doc(
        &mut self,
        doc: std::sync::Arc<std::sync::Mutex<ms_models::layer_model::layer_doc::LayerDoc>>,
    ) {
        self.layer_doc = Some(doc);
    }

    /// Pixel sizes for EVERY page of the chapter, keyed by page index (memoized via header-only
    /// `image_dimensions`). The shared doc needs the full map — not just the loaded page — because the
    /// legacy absolute-ribbon migration recovers a chapter-wide ribbon scale from every page's aspect.
    pub(crate) fn page_sizes_map(&mut self, project: &ProjectData) -> HashMap<usize, [usize; 2]> {
        let mut out = HashMap::with_capacity(project.pages.len());
        for page in &project.pages {
            let size = match self.page_sizes_px.get(&page.idx) {
                Some(size) => *size,
                None => {
                    let size = image::image_dimensions(&page.path)
                        .map(|(w, h)| [w as usize, h as usize])
                        .unwrap_or([1, 1]);
                    self.page_sizes_px.insert(page.idx, size);
                    size
                }
            };
            out.insert(page.idx, size);
        }
        out
    }

    /// Materializes the PS text-layer RUNTIMES (`PsTextLayer`) for `page_idx` from the shared
    /// `LayerDoc`, reading NO disk: PS reads disk exactly once per page (the page loader's decoded
    /// payload); every later state — including the PS-owned pin / text-group metadata the doc node
    /// carries (`text_pinned`, `text_pinned_by_group`, `text_layer_idx`, `group_uid`) — comes from
    /// the doc.
    ///
    /// Every doc Text node gets a runtime: an existing one (matched by uid) keeps its GPU texture and
    /// has its metadata refreshed from the node; a missing one is created skeletal via
    /// `PsTextLayer::meta_from_node`. Runtimes whose uid left the doc are dropped. Image, geometry
    /// and bands are then filled by the subsequent `sync_view_from_doc` projection. When no doc is
    /// wired or the page is not resident the runtimes are cleared (nothing to project), so a page
    /// switch never leaks the previous page's text layers.
    pub(crate) fn materialize_text_runtime_from_doc(&mut self, page_idx: usize) {
        use ms_models::layer_model::layer_doc::NodeKind;
        let doc = self.layer_doc.clone();
        let guard = doc.as_ref().and_then(|doc| doc.lock().ok());
        let Some(page) = guard.as_ref().and_then(|guard| guard.page(page_idx)) else {
            self.text_layers.clear();
            self.bands.clear();
            return;
        };
        let mut prev: HashMap<String, PsTextLayer> = self
            .text_layers
            .drain(..)
            .map(|t| (t.uid.clone(), t))
            .collect();
        let mut runtimes: Vec<PsTextLayer> = Vec::new();
        for node in page.nodes.iter().filter(|n| n.kind == NodeKind::Text) {
            let layer_idx = node.text_layer_idx.unwrap_or(0);
            let runtime = match prev.remove(&node.uid) {
                Some(mut existing) => {
                    // The doc owns this metadata now (PS mirrors every pin / grouping edit into it
                    // via `set_text_pin_meta`), so refresh it exactly like the former disk re-read did.
                    existing.layer_idx = layer_idx;
                    existing.group_uid = node.group_uid.clone();
                    existing.pinned = node.text_pinned;
                    existing.pinned_by_group = node.text_pinned_by_group;
                    existing
                }
                None => PsTextLayer::meta_from_node(
                    node.uid.clone(),
                    node.name.clone(),
                    layer_idx,
                    node.group_uid.clone(),
                    node.text_pinned,
                    node.text_pinned_by_group,
                ),
            };
            runtimes.push(runtime);
        }
        self.text_layers = runtimes;
    }

    /// Rebuilds this tab's per-page projections (`stack` raster layers + groups, `text_layers`,
    /// `bands`) from the shared `LayerDoc`'s resident page, which is the source of truth for layer
    /// MODEL state (transform, effects, display pixels, z, visibility, opacity, group). Local
    /// runtime/GPU/UI state is preserved and matched by uid:
    ///
    /// - Rasters: each doc Raster node is reconciled onto the `LayerStack` raster with the same uid
    ///   (its `LayerId` — and thus its `render_cache` `TiledTexture` — is preserved); a node without a
    ///   matching stack raster gets a fresh layer, and a stack raster whose uid left the doc is
    ///   removed. The render-cache tile is dropped (forcing re-upload) only when the node's
    ///   `generation` or image size changed. The stack's raster order is set to the doc z order.
    /// - Groups: rebuilt from the doc page's `GroupMeta`, mapping each uid to a stable session
    ///   `GroupId` (reusing the existing id when the group survived), and each raster's group is set.
    ///   Name, visibility, opacity and the panel-only `collapsed` flag all come from the doc, their
    ///   single in-session owner.
    /// - Text layers: each doc Text node is reconciled onto the existing `PsTextLayer` with the same
    ///   uid — MODEL fields (transform/deform/visible/image/group) are updated while pin / text-group
    ///   (`layer_idx`) metadata and the GPU texture are preserved (the texture re-uploads only on a
    ///   generation change). A text node without a local runtime (normally created beforehand by
    ///   `materialize_text_runtime_from_doc`) is built here with the node's own doc-owned pin meta.
    /// - Bands: one `Raster`/`PinnedText` band per node, z taken directly from the node.
    ///
    /// Replaces the disk-reload path: callers load the doc page (page-load / bridge) then project here.
    pub(crate) fn sync_view_from_doc(&mut self, page_idx: usize) {
        use ms_models::layer_model::layer_doc::{NodeBody, NodeKind};
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(guard) = doc.lock() else {
            return;
        };
        let Some(page) = guard.page(page_idx) else {
            return;
        };
        let Some(stack) = self.stack.as_mut() else {
            return;
        };

        let _s = ms_log::trace_scope!(
            cat::SYNC,
            "sync_view_from_doc page={} page_revision={}",
            page_idx,
            guard.page_revision(page_idx)
        );

        // --- Groups: rebuild from the doc, reusing session ids for surviving uids. ---
        let mut uid_to_gid: HashMap<String, GroupId> = stack
            .groups()
            .iter()
            .map(|g| (g.uid.to_string(), g.id))
            .collect();
        // Drop groups no longer in the doc.
        let doc_group_uids: HashSet<String> = page.groups.iter().map(|g| g.uid.clone()).collect();
        let stale_gids: Vec<GroupId> = stack
            .groups()
            .iter()
            .filter(|g| !doc_group_uids.contains(&g.uid.to_string()))
            .map(|g| g.id)
            .collect();
        for gid in stale_gids {
            stack.remove_group(gid);
        }
        for gmeta in &page.groups {
            let gid = if let Some(&gid) = uid_to_gid.get(&gmeta.uid) {
                gid
            } else {
                let parsed =
                    uuid::Uuid::parse_str(&gmeta.uid).unwrap_or_else(|_| uuid::Uuid::new_v4());
                let gid = stack.add_group_with_uid(gmeta.name.clone(), parsed);
                uid_to_gid.insert(gmeta.uid.clone(), gid);
                gid
            };
            if let Some(g) = stack.group_mut(gid) {
                g.name = gmeta.name.clone();
                // Every group field is doc-owned (`LayerDoc::set_group_meta` / `set_group_collapsed`),
                // so the stack copy is always a projection of the doc.
                g.visible = gmeta.visible;
                g.opacity = gmeta.opacity;
                g.collapsed = gmeta.collapsed;
            }
        }

        // --- Rasters: reconcile doc Raster nodes onto stack rasters by uid. ---
        // uid -> existing session LayerId for current stack rasters.
        let existing_ids: HashMap<String, LayerId> = stack
            .layers()
            .iter()
            .filter(|l| l.kind == LayerKind::Raster)
            .map(|l| (l.uid.to_string(), l.id))
            .collect();
        let doc_raster_uids: HashSet<String> = page
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Raster)
            .map(|n| n.uid.clone())
            .collect();
        // Remove stack rasters whose uid left the doc; drop their caches.
        let removed_ids: Vec<LayerId> = stack
            .layers()
            .iter()
            .filter(|l| {
                l.kind == LayerKind::Raster && !doc_raster_uids.contains(&l.uid.to_string())
            })
            .map(|l| l.id)
            .collect();
        for id in &removed_ids {
            stack.remove_layer(*id);
        }
        // The active raster id, preserved across the rebuild when it survives.
        let prev_active = stack.active_id();

        // Caches to drop after the borrow ends (generation/size changed or layer removed).
        let mut drop_caches: Vec<LayerId> = removed_ids;
        // Ordered (bottom-to-top by doc z) raster ids, to set the stack order afterwards.
        let mut ordered_raster_ids: Vec<LayerId> = Vec::new();

        for node in &page.nodes {
            let NodeBody::Raster {
                base_image,
                display_image,
                effects,
                ..
            } = &node.body
            else {
                continue;
            };
            let cache_key = (page_idx, node.uid.clone());
            let gen_changed =
                self.node_generations.get(&cache_key).copied() != Some(node.generation);
            let group = node
                .group_uid
                .as_ref()
                .and_then(|u| uid_to_gid.get(u).copied());
            if let Some(&id) = existing_ids.get(&node.uid) {
                if let Some(layer) = stack.layer_mut(id) {
                    // A layer with uncommitted base-pixel edits that are ALSO still dirty in the doc
                    // node (an in-progress paint not yet committed/flushed) keeps its live pixels; only
                    // its non-pixel model fields are reconciled, so a revision-driven projection never
                    // clobbers in-flight work. Once routed+flushed (doc node clean), the projection
                    // adopts the doc's pixels and clears the local dirty flag.
                    let keep_pixels = layer.pixels_dirty && node.pixels_dirty;
                    let size_changed = !keep_pixels && layer.image.size != display_image.size;
                    layer.name = node.name.clone();
                    layer.visible = node.visible;
                    layer.opacity = node.opacity;
                    layer.group = group;
                    // Deform is geometry (absolute page px), not pixels — sync it regardless of the
                    // pixels-dirty guard so a mesh edit in either tab reflects immediately.
                    layer.deform = node.deform.clone();
                    if !keep_pixels {
                        layer.transform = rec_to_transform(node.transform);
                        layer.image = display_image.clone();
                        layer.base_image = base_image.clone();
                        layer.effects = effects.clone();
                        layer.pixels_dirty = node.pixels_dirty;
                        if gen_changed || size_changed {
                            drop_caches.push(id);
                        }
                    }
                }
                ordered_raster_ids.push(id);
            } else {
                let id = stack.add_raster_layer_image(
                    node.name.clone(),
                    display_image.clone(),
                    rec_to_transform(node.transform),
                );
                if let Some(layer) = stack.layer_mut(id) {
                    layer.uid =
                        uuid::Uuid::parse_str(&node.uid).unwrap_or_else(|_| uuid::Uuid::new_v4());
                    layer.visible = node.visible;
                    layer.opacity = node.opacity;
                    layer.base_image = base_image.clone();
                    layer.effects = effects.clone();
                    layer.group = group;
                    layer.deform = node.deform.clone();
                }
                ordered_raster_ids.push(id);
            }
            self.node_generations.insert(cache_key, node.generation);
        }
        // Set the stack's raster order to the doc z order (bottom-to-top), keeping base layers first.
        stack.reorder_rasters(&ordered_raster_ids);
        stack.set_active(prev_active);

        // --- Text layers: reconcile doc Text nodes onto local runtimes by uid. ---
        // Build the new text-layer list in doc order, preserving pin / layer_idx / texture by uid.
        let mut prev_text: HashMap<String, PsTextLayer> = self
            .text_layers
            .drain(..)
            .map(|t| (t.uid().to_string(), t))
            .collect();
        let mut new_text: Vec<PsTextLayer> = Vec::new();
        for node in &page.nodes {
            let NodeBody::Text {
                image, render_data, ..
            } = &node.body
            else {
                continue;
            };
            // Raw overlay text (for the panel row preview); empty when render_data lacks it.
            let text_content = render_data
                .get("text_params")
                .and_then(|tp| tp.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let cache_key = (page_idx, node.uid.clone());
            let gen_changed =
                self.node_generations.get(&cache_key).copied() != Some(node.generation);
            // Reconcile onto the prior runtime when one exists (preserving pin / text-group / texture);
            // otherwise BUILD a fresh PsTextLayer from the doc node. The doc is the source of truth for
            // text AND for its PS pin meta (`text_pinned` / `text_pinned_by_group`, mirrored by every PS
            // band / grouping / pin edit), so a runtime-less node takes its meta from the node. The
            // prior runtime already carries the same values (`materialize_text_runtime_from_doc`
            // refreshes them from the node, and PS pin edits update both sides together).
            let prev = prev_text.remove(&node.uid);
            let (layer_idx, pinned, pinned_by_group) = match &prev {
                Some(p) => (p.layer_idx, p.pinned, p.pinned_by_group),
                None => (node.text_layer_idx.unwrap_or(0), node.text_pinned, node.text_pinned_by_group),
            };
            // Preserve the GPU texture unless the node's pixels changed (only possible with a prior).
            let texture = match prev {
                Some(mut p) if !gen_changed => p.take_texture(),
                _ => None,
            };
            self.node_generations.insert(cache_key, node.generation);
            new_text.push(PsTextLayer {
                uid: node.uid.clone(),
                name: node.name.clone(),
                visible: node.visible,
                layer_idx,
                group_uid: node.group_uid.clone(),
                pinned,
                pinned_by_group,
                text_content,
                image: image.clone(),
                transform: LayerTransform {
                    center: Vec2::new(node.transform.cx, node.transform.cy),
                    rotation: node.transform.rotation,
                    scale: node.transform.scale,
                },
                deform: node.deform.clone(),
                texture,
            });
        }
        self.text_layers = new_text;

        // --- Bands: derive unified Z directly from the doc node z (the shared ordering owner). ---
        self.bands = ordering::doc_page_bands(page);

        // Record the page revision we just projected so the per-frame check does not redundantly
        // re-project until the doc changes this page again.
        self.last_projected_revision = Some((page_idx, guard.page_revision(page_idx)));

        drop(guard);
        for id in drop_caches {
            self.render_cache.remove(&id);
        }
        // This projection may have removed the very row `panel_primary` names (a raster deleted or
        // merged away here or in the other tab, a rasterized text, a deleted group). Re-establish
        // the "always a valid primary row" invariant before the panel next draws — otherwise the
        // controls strip renders nothing at all, not even its hint.
        self.normalize_panel_primary();
    }

    /// Routes a raster/group MODEL edit to the shared `LayerDoc`: locks it, runs `edit` against the
    /// resident page (loaded by `poll_loader`), flushes the page to disk (so it survives reloads /
    /// save-to-project), and re-projects the view from the doc. The doc mutation advances the page's
    /// revision, so the typing tab re-projects via its per-frame revision check. Returns false (the
    /// caller keeps its legacy path) when no doc is wired or the page isn't resident.
    pub(crate) fn route_to_doc<F>(&mut self, page_idx: usize, project: &ProjectData, edit: F) -> bool
    where
        F: FnOnce(&mut ms_models::layer_model::layer_doc::LayerDoc),
    {
        let Some(doc) = self.layer_doc.clone() else {
            return false;
        };
        {
            let Ok(mut guard) = doc.lock() else {
                return false;
            };
            if guard.page(page_idx).is_none() {
                return false;
            }
            let revision_before = guard.page_revision(page_idx);
            edit(&mut guard);
            // Guarantee a cross-tab notification even if `edit` mutated node fields directly via
            // `node_mut` (which does not bump); skipped when a mutator already bumped THIS page, so
            // one edit is one revision step.
            if guard.page_revision(page_idx) == revision_before {
                guard.mark_page_changed(page_idx);
            }
            // Persist so the change survives a reload / save-to-project. ASYNC: enqueue the page job
            // to the background saver (PNG encode + manifest RMW off the GUI thread); falls back to a
            // synchronous flush when no saver is enabled. The save-to-project merge worker and the
            // app-close drain barrier the queue, so an enqueued write is never lost.
            if let Err(err) = guard.enqueue_page_save(
                page_idx,
                &project.paths.unsaved_layers_dir,
                Some(&project.paths.layers_dir),
            ) {
                ms_log::runtime_log::log_warn(format!("[ps_editor] doc flush: {err}"));
            }
            // This page now has its persist enqueued; the tab-switch flush is redundant for it.
            self.layers_dirty = false;
        }
        self.sync_view_from_doc(page_idx);
        true
    }
}
