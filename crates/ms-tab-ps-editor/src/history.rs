/*
File: crates/ms-tab-ps-editor/src/history.rs

Purpose:
Per-page undo/redo of the PS editor and the doc writers the reversible ops use.

Key functions:
- `undo()` / `redo()` (pub), `finish_history_step()`, `take_history()`: the
  `ActionHistory<PsEditOp>` take-and-restore idiom.
- `apply_ps_*()`: the `PsEditOp` appliers (raster tiles, clean overlay region, layer lifecycle,
  field patch, group meta, text visibility).
- `write_group_meta()`, `write_text_visibility()`, `record_pixel_region()`: doc-level writers that
  also record undo.
- `edit_doc_node()`, `flush_text_page()`, `refresh_view_if_doc_version_changed()`: doc mutation and
  re-projection helpers.

Notes:
Raster undo is a bounded per-tile delta reversal (`RasterDiff`), safe on the GUI thread.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

impl PsEditorTabState {
    /// Undoes the most recent PS-editor edit on the current page, if any. Returns whether anything
    /// changed. Routes the reverted pixels to the shared doc + enqueues a disk save so cross-tab
    /// state and persistence stay in sync. Safe on the GUI thread: the raster apply is a bounded,
    /// per-tile delta reversal (no full-image work beyond the changed tiles).
    pub fn undo(&mut self, project: &ProjectData) -> bool {
        // Take-and-restore idiom (see `take_history`): the op `Ctx` is `Self` but `history` is a field
        // of `Self`, so `self.history.undo(self)` would double-borrow. Restore the history
        // UNCONDITIONALLY (no `?` between take and restore) so the stack is never lost on an error path.
        let mut history = self.take_history();
        let mut result = history.undo(self);
        // An orphaned metadata entry (its group / text was deleted) has already left the stack and can
        // never apply; step past it so the keypress undoes the next applicable edit. Terminates: each
        // iteration pops one entry, and an empty stack yields `Ok(false)`.
        while let Err(edit_op::PsEditOpError::TargetGone { page_idx }) = &result {
            ms_log::trace_log!(cat::PS_EDITOR, "undo: skipped an entry whose target left page {}", page_idx);
            result = history.undo(self);
        }
        self.history = history;
        self.finish_history_step(result, project, "undo")
    }

    /// Redoes the most recently undone PS-editor edit on the current page, if any. See [`Self::undo`]
    /// (orphaned metadata entries are skipped the same way).
    pub fn redo(&mut self, project: &ProjectData) -> bool {
        let mut history = self.take_history();
        let mut result = history.redo(self);
        while let Err(edit_op::PsEditOpError::TargetGone { page_idx }) = &result {
            ms_log::trace_log!(cat::PS_EDITOR, "redo: skipped an entry whose target left page {}", page_idx);
            result = history.redo(self);
        }
        self.history = history;
        self.finish_history_step(result, project, "redo")
    }

    /// Shared tail of `undo`/`redo`: on a real change, persist the active page so the reverted state
    /// survives a reload / save-to-project (plus the page's text payload when the step changed a text
    /// node, see `history_text_flush_page`); logs and swallows an apply error (nothing changed).
    ///
    /// Uses `persist_current_page` (not a bare doc enqueue) because it reads the reconciled
    /// `self.stack` and carries the EXPLICIT `removed_uids` from `deleted_raster_uids` — required so a
    /// `LayerLifecycle` delete/undo drops or keeps the on-disk raster PNG correctly (the doc's own
    /// `enqueue_page_save` passes an empty removed set, which would resurrect a just-deleted raster).
    /// The PNG encode still runs off the GUI thread (the saver owns it).
    pub(crate) fn finish_history_step(
        &mut self,
        result: Result<bool, edit_op::PsEditOpError>,
        project: &ProjectData,
        op: &str,
    ) -> bool {
        // Taken on every outcome so a failed step cannot leak a flush request into a later one.
        let text_flush_page = self.history_text_flush_page.take();
        match result {
            Ok(true) => {
                self.persist_current_page(project);
                if let Some(page_idx) = text_flush_page {
                    self.flush_text_page(page_idx, project);
                }
                true
            }
            Ok(false) => false,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[ps_editor] {op}: {err}"));
                false
            }
        }
    }

    /// Move the undo history out of `self`, leaving an empty history that preserves the count limit
    /// and byte budget. Used only by the take-and-restore undo/redo idiom; the caller MUST put a
    /// history back before returning.
    pub(crate) fn take_history(&mut self) -> ActionHistory<PsEditOp> {
        let limit = self.history.limit();
        let replacement = match self.history.weight_budget() {
            Some(budget) => ActionHistory::with_weight_budget(limit, budget),
            None => ActionHistory::new(limit),
        };
        std::mem::replace(&mut self.history, replacement)
    }

    /// Applies a reversible raster delta to the resident layer identified by `layer_uid` on
    /// `page_idx`, in `dir`. Mutates the layer's `image` + `base_image` (via
    /// `edit_op::apply_raster_diff_to_layer`), marks the affected `render_cache` tiles dirty, and
    /// pushes the resulting pixels to the shared doc (in-memory) so a later reprojection and cross-tab
    /// consumers see the same result. This is the only mutation path used by PS-editor undo/redo.
    ///
    /// # Errors
    /// - [`edit_op::PsEditOpError::NotResident`] if the stack is absent, on a different page, or the
    ///   uid is no longer a resident raster.
    /// - [`edit_op::PsEditOpError::Raster`] if the delta cannot be applied (size mismatch / corrupt).
    pub(crate) fn apply_ps_raster_edit(
        &mut self,
        page_idx: usize,
        layer_uid: &str,
        diff: &RasterDiff,
        dir: ApplyDirection,
    ) -> Result<(), edit_op::PsEditOpError> {
        // Resolve the target raster (resident + matching page) and mutate its pixels.
        let (id, uid, reverted, dirty) = {
            let stack = self
                .stack
                .as_mut()
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            if stack.page_idx() != page_idx {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            }
            let id = stack
                .layers()
                .iter()
                .find(|l| l.kind == LayerKind::Raster && l.uid.to_string() == layer_uid)
                .map(|l| l.id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            let layer = stack
                .layer_mut(id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            let dirty = edit_op::apply_raster_diff_to_layer(layer, diff, dir)?;
            layer.pixels_dirty = true;
            (id, layer.uid.to_string(), layer.image.clone(), dirty)
        };

        // Invalidate only the touched tiles so the next upload re-sends the reverted pixels.
        if let Some(cache) = self.render_cache.get_mut(&id) {
            for rect in &dirty {
                cache.mark_dirty_rect(tools::DirtyRect {
                    min_x: rect.origin_px[0] as usize,
                    min_y: rect.origin_px[1] as usize,
                    max_x: rect.origin_px[0].saturating_add(rect.size_px[0].saturating_sub(1))
                        as usize,
                    max_y: rect.origin_px[1].saturating_add(rect.size_px[1].saturating_sub(1))
                        as usize,
                });
            }
        }

        // Route the reverted pixels to the shared doc (in-memory) so cross-tab consumers and the next
        // `sync_view_from_doc` reprojection agree. A paintable raster has no effects, so
        // base == display == reverted pixels. Disk persistence is enqueued by the undo/redo caller.
        if let Some(doc) = self.layer_doc.clone()
            && let Ok(mut guard) = doc.lock()
            && guard.page(page_idx).is_some()
        {
            guard.set_raster_pixels(page_idx, &uid, reverted.clone(), reverted, Vec::new(), true);
            guard.mark_page_changed(page_idx);
        }
        Ok(())
    }

    /// Writes the `Клин` base layer's CURRENT pixels over the page-space rect `(x, y, w, h)` back
    /// into the shared `CleanOverlaysModel`, and re-establishes `base_image == image` over the same
    /// rect.
    ///
    /// This is the ONE write-back path for every Clean-layer pixel edit (brush commit, cut,
    /// merge-into, undo/redo), so the rules below hold exactly once:
    /// - only the given rect is pushed — a ribbon page can be ~800x19000 px, so the whole buffer is
    ///   never sent;
    /// - `chunk` is a raw crop of the premultiplied `Layer::image`; the model converts it to
    ///   straight RGBA itself;
    /// - the model write happens FIRST and the local commit (`base_image` mirror, `pixels_dirty`)
    ///   only after the model accepted it — otherwise a rejected write would leave the edit alive
    ///   solely in this tab's stack, to die unnoticed at the next reload;
    /// - the write is REFUSED when the model's materialized overlay for this page has a different
    ///   size than the stack's page: `replace_region` would clamp to the MODEL's size and scale the
    ///   chunk into it, landing a brush stroke scaled and misaligned. That legacy mismatch is the
    ///   case `page_loader` answers with a transparent page-sized `Клин`, so it is a real state, and
    ///   a silent rescale is exactly the incorrect fallback the project forbids. NOT covered: an
    ///   overlay the model remembers a size for but has not materialized (only reachable through
    ///   `detach_page_overlay`) — the model exposes no size accessor for that state;
    /// - `last_overlay_revision` is adopted IN THE SAME LOCK SCOPE, but ONLY when it still equals
    ///   the revision read just before our own write — i.e. when nothing foreign changed the model
    ///   while this tab was away. Adopting it unconditionally would swallow a background writer's
    ///   bump (the page manager's clean-attach worker is one) and permanently suppress the reload
    ///   that reconciles it, leaving the tab painting on stale pixels for the rest of the session.
    ///   Declining to adopt costs one redundant full-page reload at the next
    ///   `sync_view_from_canvas`; it never costs the user's work, which the model already holds and
    ///   the reload restores from `overlay_rgba`.
    /// - `pixels_dirty` is cleared: the shared model (and its autosave worker) owns Clean
    ///   persistence, and `persist_current_page` deliberately never writes base layers.
    ///
    /// Returns `true` when the model accepted the region. `false` means the stack is absent or on
    /// another page, the rect is empty after clamping, there is no Clean layer, no model is bound,
    /// the model's lock is poisoned, or the overlay size mismatched — the first three are ordinary
    /// no-ops, the rest are logged with context.
    pub(crate) fn write_clean_region_to_model(
        &mut self,
        page_idx: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> bool {
        // Phase 1 — crop the chunk out of the stack, mutating nothing. The local commit is phase 3,
        // after the model has actually taken the pixels.
        let Some(stack) = self.stack.as_ref() else {
            return false;
        };
        if stack.page_idx() != page_idx {
            return false;
        }
        let page_size = stack.size();
        let Some(clean) = stack.layers().iter().find(|l| l.kind == LayerKind::Clean) else {
            return false;
        };
        let clean_id = clean.id;
        // Clamp the requested rect into the layer image; an empty result is a no-op.
        let [iw, ih] = clean.image.size;
        let x0 = x.min(iw);
        let y0 = y.min(ih);
        let x1 = x.saturating_add(w).min(iw);
        let y1 = y.saturating_add(h).min(ih);
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        let (cw, ch) = (x1 - x0, y1 - y0);
        let mut pixels: Vec<Color32> = Vec::with_capacity(cw.saturating_mul(ch));
        for row in y0..y1 {
            let start = row * iw + x0;
            let Some(span) = clean.image.pixels.get(start..start + cw) else {
                // Clamped above, so this cannot happen; bail out instead of panicking.
                return false;
            };
            pixels.extend_from_slice(span);
        }
        let chunk = ColorImage::new([cw, ch], pixels);

        // Phase 2 — push to the shared model.
        let Some(model) = self.overlays_model.as_ref() else {
            ms_log::runtime_log::log_error(format!(
                "[ps_editor] no clean overlay model is bound; the Клин edit on page {page_idx} \
                 rect ({x0},{y0},{cw},{ch}) was not shared"
            ));
            return false;
        };
        let Ok(mut locked) = model.lock() else {
            ms_log::runtime_log::log_error(format!(
                "[ps_editor] clean overlay model lock poisoned; the Клин edit on page {page_idx} \
                 rect ({x0},{y0},{cw},{ch}) was not shared"
            ));
            return false;
        };
        // Refuse rather than let `replace_region` scale our chunk into a differently-sized overlay.
        // `overlay_size` is the REMEMBERED page size, which is what the model normalizes a write to;
        // a materialized-buffer check (`get`) would miss a detached page, where no buffer exists yet
        // the remembered size still drives the rescale.
        if let Some(model_size) = locked.overlay_size(page_idx)
            && model_size != page_size
        {
            drop(locked);
            ms_log::runtime_log::log_error(format!(
                "[ps_editor] clean overlay size mismatch on page {page_idx}: model has \
                 {}x{} px, the editor page is {}x{} px; the Клин edit was refused instead of \
                 being rescaled",
                model_size[0], model_size[1], page_size[0], page_size[1]
            ));
            return false;
        }
        // Read the revision BEFORE our write so the adoption below can tell our own bump apart from
        // a foreign one that landed while this tab was not syncing.
        let revision_before = locked.revision();
        let region = ms_models::clean_overlays_model::RegionPx { x: x0, y: y0, w: cw, h: ch };
        let accepted = locked.replace_region(page_idx, page_size, region, &chunk);
        // Same lock scope as the write: adopt the revision our own write just produced, so the next
        // `sync_view_from_canvas` does not mistake it for another tab's edit and reload the page.
        // Conditional on purpose — see the doc comment: if a foreign writer bumped the revision
        // while we were away, adopting now would hide that edit from the reload forever.
        if accepted && self.last_overlay_revision == revision_before {
            self.last_overlay_revision = locked.revision();
        }
        drop(locked);
        ms_log::trace_log!(
            cat::SYNC,
            "clean_write page={} rect=({},{},{},{}) accepted={} rev_before={} rev_known={}",
            page_idx,
            x0,
            y0,
            cw,
            ch,
            accepted,
            revision_before,
            self.last_overlay_revision
        );
        if !accepted {
            ms_log::runtime_log::log_error(format!(
                "[ps_editor] the clean overlay model rejected the Клин edit on page {page_idx} \
                 rect ({x0},{y0},{cw},{ch}); the local pixels were left uncommitted"
            ));
            return false;
        }

        // Phase 3 — the model holds the write, so commit the local bookkeeping.
        let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(clean_id)) else {
            return false;
        };
        // The undo "before" is read from `base_image`, so it must track `image` once an edit is
        // committed — the same invariant `edit_op::apply_raster_diff_to_layer` keeps for rasters.
        if layer.base_image.size == layer.image.size {
            for row in y0..y1 {
                let start = row * iw + x0;
                let (Some(src), Some(dst)) = (
                    layer.image.pixels.get(start..start + cw),
                    layer.base_image.pixels.get_mut(start..start + cw),
                ) else {
                    continue;
                };
                dst.copy_from_slice(src);
            }
        }
        layer.pixels_dirty = false;
        true
    }

    /// Applies a reversible pixel delta to the `Клин` base layer on `page_idx`, in `dir`, and pushes
    /// the result back to the shared `CleanOverlaysModel` (see [`PsEditOp::CleanPixels`]).
    ///
    /// The target is resolved by KIND, never by uid: base-layer uids are regenerated on every page
    /// load, so a uid recorded at stroke time would not match after a reload. Mutates the layer's
    /// `image` + `base_image`, marks the affected `render_cache` tiles dirty, and routes each
    /// changed rect through `write_clean_region_to_model` so an undo is as durable and as
    /// cross-tab-visible as the forward edit.
    ///
    /// # Errors
    /// - [`edit_op::PsEditOpError::NotResident`] if the stack is absent, on a different page, or has
    ///   no Clean layer.
    /// - [`edit_op::PsEditOpError::Raster`] if the delta cannot be applied (size mismatch / corrupt).
    pub(crate) fn apply_ps_clean_edit(
        &mut self,
        page_idx: usize,
        diff: &RasterDiff,
        dir: ApplyDirection,
    ) -> Result<(), edit_op::PsEditOpError> {
        let (id, dirty) = {
            let stack = self
                .stack
                .as_mut()
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            if stack.page_idx() != page_idx {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            }
            let id = stack
                .layers()
                .iter()
                .find(|l| l.kind == LayerKind::Clean)
                .map(|l| l.id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            let layer = stack
                .layer_mut(id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            let dirty = edit_op::apply_raster_diff_to_layer(layer, diff, dir)?;
            (id, dirty)
        };

        // Invalidate only the touched tiles so the next upload re-sends the reverted pixels.
        if let Some(cache) = self.render_cache.get_mut(&id) {
            for rect in &dirty {
                cache.mark_dirty_rect(tools::DirtyRect {
                    min_x: rect.origin_px[0] as usize,
                    min_y: rect.origin_px[1] as usize,
                    max_x: rect.origin_px[0].saturating_add(rect.size_px[0].saturating_sub(1))
                        as usize,
                    max_y: rect.origin_px[1].saturating_add(rect.size_px[1].saturating_sub(1))
                        as usize,
                });
            }
        }

        // Share the reverted pixels. NOTE: `replace_region` records its OWN reversible diff in the
        // clean model's cross-tab history, so this undo also lands there as a further FORWARD edit.
        // State never diverges; the only oddity is that a later Ctrl+Z on the cleaning tab can
        // re-apply what PS just reverted. Documented in `MODULE_README.md`.
        for rect in &dirty {
            self.write_clean_region_to_model(
                page_idx,
                rect.origin_px[0] as usize,
                rect.origin_px[1] as usize,
                rect.size_px[0] as usize,
                rect.size_px[1] as usize,
            );
        }
        Ok(())
    }

    /// Realizes a whole-raster-layer add/delete for undo/redo (see [`PsEditOp::LayerLifecycle`]).
    /// `dir == Added` re-inserts `layer` (with its retained pixels) into the shared doc at Z `z` and
    /// re-projects, so the stack + `render_cache` are rebuilt by `sync_view_from_doc`; `dir == Removed`
    /// removes the node by uid and re-projects (which drops its cache). Deletion bookkeeping
    /// (`deleted_raster_uids`) is updated so the next `persist_current_page` drops/keeps the on-disk
    /// PNG correctly. Never panics.
    ///
    /// # Errors
    /// [`edit_op::PsEditOpError::NotResident`] if no doc is wired, the target page is not resident, or
    /// the add/remove could not be applied.
    pub(crate) fn apply_ps_layer_lifecycle(
        &mut self,
        page_idx: usize,
        layer: &Layer,
        z: u32,
        dir: LifecycleDir,
    ) -> Result<(), edit_op::PsEditOpError> {
        // The op only makes sense while its page's stack is resident (the history is per-page).
        if self.stack.as_ref().map(LayerStack::page_idx) != Some(page_idx) {
            return Err(edit_op::PsEditOpError::NotResident { page_idx });
        }
        let Some(doc) = self.layer_doc.clone() else {
            return Err(edit_op::PsEditOpError::NotResident { page_idx });
        };
        let uid = layer.uid.to_string();
        let ok = {
            let Ok(mut guard) = doc.lock() else {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            };
            if guard.page(page_idx).is_none() {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            }
            match dir {
                LifecycleDir::Added => {
                    // Rebuild the doc node from the retained layer. `pixels_dirty = true` so the next
                    // persist rewrites its base PNG (the delete pruned it); preserve the deform mesh.
                    let mut node = layer_to_raster_node(layer);
                    node.pixels_dirty = true;
                    if let ms_models::layer_model::layer_doc::NodeBody::Raster {
                        base_image,
                        display_image,
                        ..
                    } = &mut node.body
                    {
                        *base_image = layer.base_image.clone();
                        *display_image = layer.image.clone();
                    }
                    node.deform = layer.deform.clone();
                    let added = guard.add_node_at_z(page_idx, node, z);
                    if added {
                        guard.mark_page_changed(page_idx);
                    }
                    added
                }
                LifecycleDir::Removed => {
                    let removed = guard.remove_node(page_idx, &uid);
                    if removed {
                        guard.mark_page_changed(page_idx);
                    }
                    removed
                }
            }
        };
        if !ok {
            return Err(edit_op::PsEditOpError::NotResident { page_idx });
        }
        // Keep the deletion bookkeeping consistent so `persist_current_page` drops (Removed) or keeps
        // (Added) the on-disk raster: `save_page_rasters` preserves manifest rasters not in the stack,
        // so a removal must be explicit.
        match dir {
            LifecycleDir::Added => {
                if let Some(set) = self.deleted_raster_uids.get_mut(&page_idx) {
                    set.remove(&uid);
                }
            }
            LifecycleDir::Removed => {
                self.deleted_raster_uids
                    .entry(page_idx)
                    .or_default()
                    .insert(uid);
            }
        }
        // Rebuild the stack raster layers + text + bands (and prune/create the render cache) from the
        // mutated doc — the same projection the forward add/delete paths use.
        self.sync_view_from_doc(page_idx);
        Ok(())
    }

    /// Applies a single metadata/geometry field change (visibility / opacity / transform / deform) to
    /// the raster identified by `layer_uid` on `page_idx`, driving it to the patch's `after` value (see
    /// [`PsEditOp::FieldPatch`]). Routes through the shared doc setter (so cross-tab consumers agree)
    /// and re-projects; falls back to a direct stack mutation when no doc page is resident. These
    /// fields do not change pixels, so no `render_cache` invalidation is needed (compositing re-reads
    /// them each frame). Never panics.
    ///
    /// # Errors
    /// [`edit_op::PsEditOpError::NotResident`] if the stack is absent, on a different page, or the uid
    /// is no longer a resident raster.
    pub(crate) fn apply_ps_field_patch(
        &mut self,
        page_idx: usize,
        layer_uid: &str,
        field: &LayerFieldPatch,
    ) -> Result<(), edit_op::PsEditOpError> {
        // Resolve the target raster (resident + matching page).
        let id = {
            let stack = self
                .stack
                .as_ref()
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            if stack.page_idx() != page_idx {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            }
            stack
                .layers()
                .iter()
                .find(|l| l.kind == LayerKind::Raster && l.uid.to_string() == layer_uid)
                .map(|l| l.id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?
        };
        // Drive the doc to the `after` value (mirrors the forward edit's setter), then re-project.
        let applied = self.edit_doc_node(page_idx, |doc| match field {
            LayerFieldPatch::Visibility { after, .. } => {
                doc.set_visibility(page_idx, layer_uid, *after);
            }
            LayerFieldPatch::Opacity { after, .. } => doc.set_opacity(page_idx, layer_uid, *after),
            LayerFieldPatch::Transform { after, .. } => {
                doc.set_transform(page_idx, layer_uid, transform_to_rec(*after));
            }
            LayerFieldPatch::Deform { after, .. } => {
                doc.set_deform(page_idx, layer_uid, after.clone());
            }
        });
        if !applied {
            // No doc page resident: mutate the stack layer directly (matches the forward edits' local
            // fallback). `edit_doc_node` returning false means the doc was never touched.
            let layer = self
                .stack
                .as_mut()
                .and_then(|s| s.layer_mut(id))
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            edit_op::apply_field_patch_to_layer(layer, field);
        }
        Ok(())
    }

    /// Drives the PS unified group `group_uid` on `page_idx` to `after` for undo/redo (see
    /// [`PsEditOp::GroupMeta`]) through the shared doc (`LayerDoc::set_group_meta`, so the typing tab
    /// and the export follow) and re-projects; falls back to the stack group when no doc page is
    /// resident. `finish_history_step` persists the page afterwards. Never panics.
    ///
    /// # Errors
    /// [`edit_op::PsEditOpError::NotResident`] if the stack is absent or on a different page;
    /// [`edit_op::PsEditOpError::TargetGone`] if the group was deleted (undo/redo skip that entry).
    pub(crate) fn apply_ps_group_meta(
        &mut self,
        page_idx: usize,
        group_uid: &str,
        after: GroupMetaValue,
    ) -> Result<(), edit_op::PsEditOpError> {
        let stack = self
            .stack
            .as_ref()
            .filter(|s| s.page_idx() == page_idx)
            .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
        if stack.group_by_uid(group_uid).is_none() {
            return Err(edit_op::PsEditOpError::TargetGone { page_idx });
        }
        self.write_group_meta(page_idx, group_uid, after, None);
        Ok(())
    }

    /// Drives the text node `text_uid` on `page_idx` to visibility `after` for undo/redo (see
    /// [`PsEditOp::TextVisibility`]) through the shared doc and re-projects, and asks
    /// `finish_history_step` to enqueue the page's text save. Falls back to the local runtime when no
    /// doc page is resident. Never panics.
    ///
    /// # Errors
    /// [`edit_op::PsEditOpError::NotResident`] if the stack is absent or on a different page;
    /// [`edit_op::PsEditOpError::TargetGone`] if no local text runtime carries `text_uid` any more
    /// (the text was deleted; undo/redo skip that entry).
    pub(crate) fn apply_ps_text_visibility(
        &mut self,
        page_idx: usize,
        text_uid: &str,
        after: bool,
    ) -> Result<(), edit_op::PsEditOpError> {
        if !self.stack.as_ref().is_some_and(|s| s.page_idx() == page_idx) {
            return Err(edit_op::PsEditOpError::NotResident { page_idx });
        }
        if !self.text_layers.iter().any(|t| t.uid == text_uid) {
            return Err(edit_op::PsEditOpError::TargetGone { page_idx });
        }
        if self.write_text_visibility(page_idx, text_uid, after) {
            self.history_text_flush_page = Some(page_idx);
        }
        Ok(())
    }

    /// The composite meta of the stack group `group_uid`, or `None` when the stack has no such group.
    pub(crate) fn group_meta_value(&self, group_uid: &str) -> Option<GroupMetaValue> {
        self.stack
            .as_ref()
            .and_then(|s| s.group_by_uid(group_uid))
            .map(|g| GroupMetaValue {
                visible: g.visible,
                opacity: g.opacity,
            })
    }

    /// Writes a PS unified group's composite meta into the shared doc (`LayerDoc::set_group_meta`)
    /// and re-projects the stack from it. With `persist_now = Some(project)` the page save is enqueued
    /// at once (`route_to_doc`: a discrete eye toggle); with `None` persistence is deferred to the
    /// tab-switch / page-leave flush (`edit_doc_node`: live opacity drag steps, undo/redo, whose
    /// caller persists). Without a resident doc page the stack group is written directly and the page
    /// marked dirty (the legacy local path; `persist_current_page` writes stack groups).
    pub(crate) fn write_group_meta(
        &mut self,
        page_idx: usize,
        group_uid: &str,
        value: GroupMetaValue,
        persist_now: Option<&ProjectData>,
    ) {
        let edit = |doc: &mut ms_models::layer_model::layer_doc::LayerDoc| {
            if !doc.set_group_meta(page_idx, group_uid, value.visible, value.opacity) {
                // The stack group has no doc twin (or a NaN opacity reached here): the projection that
                // follows drops / keeps the stack group exactly as the doc says.
                ms_log::trace_log!(
                    cat::SYNC,
                    "write_group_meta: doc refused page={} uid={} opacity={}",
                    page_idx,
                    group_uid,
                    value.opacity
                );
            }
        };
        let routed = match persist_now {
            Some(project) => self.route_to_doc(page_idx, project, edit),
            None => self.edit_doc_node(page_idx, edit),
        };
        if !routed {
            if let Some(g) = self.stack.as_mut().and_then(|s| group_mut_by_uid(s, group_uid)) {
                g.visible = value.visible;
                g.opacity = value.opacity;
            }
            self.layers_dirty = true;
        }
    }

    /// Writes a text node's visibility into the shared doc (`LayerDoc::set_visibility`) and
    /// re-projects the text runtimes from it. Returns `true` when the doc took the edit (the caller
    /// then owes the page a text save); `false` means no doc page was resident and only the local
    /// runtime changed (nothing to persist: such a page has no doc text payload to write).
    pub(crate) fn write_text_visibility(&mut self, page_idx: usize, text_uid: &str, visible: bool) -> bool {
        let routed = self.edit_doc_node(page_idx, |doc| {
            doc.set_visibility(page_idx, text_uid, visible);
        });
        if !routed && let Some(layer) = self.text_layers.iter_mut().find(|t| t.uid == text_uid) {
            layer.visible = visible;
        }
        routed
    }

    /// Closes an open group-opacity drag (`group_opacity_gesture`): records ONE
    /// [`PsEditOp::GroupMeta`] for the whole gesture when the value actually moved. Called on the
    /// first panel frame without a group-opacity change, and before a drag on another group starts.
    pub(crate) fn finish_group_opacity_gesture(&mut self) {
        let Some((page_idx, group_uid, before)) = self.group_opacity_gesture.take() else {
            return;
        };
        let Some(after) = self.group_meta_value(&group_uid) else {
            return;
        };
        if (after.opacity - before.opacity).abs() > f32::EPSILON || after.visible != before.visible {
            self.history.record(PsEditOp::GroupMeta {
                page_idx,
                group_uid,
                before,
                after,
            });
        }
    }

    /// Records a just-performed pixel edit of the ACTIVE layer as one reversible undo entry
    /// (observer style — the forward edit was already applied live).
    ///
    /// Builds a region-bounded `RasterDiff` from the pre-edit `base_image` ("before") and the
    /// edited `image` ("after") over `region` — the ACTIVE LAYER's own pixel grid, like every
    /// [`tools::DirtyRect`] — so no full-image scan is needed. A no-op edit (empty diff) is not
    /// recorded. `label` is the already-localized history caption.
    ///
    /// Handles BOTH paintable kinds through the pure [`pixel_region_edit_op`]: a user raster
    /// records a `PsEditOp::RasterPixels` keyed on its stable doc uid; the `Клин` base layer
    /// records a `PsEditOp::CleanPixels`, which carries NO uid — base-layer uids are regenerated on
    /// every page load, so only the layer KIND identifies it.
    ///
    /// Must be called BEFORE the pixels are pushed onward, while `base_image` still holds the
    /// pre-edit state. Both callers are in `commit_pixel_region`, whose contract states that.
    pub(crate) fn record_pixel_region(&mut self, page_idx: usize, union: tools::DirtyRect, label: &str) {
        // Capture the region-local before/after buffers + uid while borrowing the stack immutably.
        let captured = {
            let Some(stack) = self.stack.as_ref() else {
                return;
            };
            if stack.page_idx() != page_idx {
                return;
            }
            let Some(layer) = stack.layer(stack.active_id()) else {
                return;
            };
            // Only a directly-editable layer (raster without effects, or Клин) has
            // base_image == pre-stroke pixels.
            if !layer.can_edit_pixels() {
                return;
            }
            let kind = layer.kind;
            let size = layer.image.size;
            let max_x = size[0].saturating_sub(1);
            let max_y = size[1].saturating_sub(1);
            let x0 = union.min_x.min(max_x);
            let y0 = union.min_y.min(max_y);
            let x1 = union.max_x.min(max_x);
            let y1 = union.max_y.min(max_y);
            if x1 < x0 || y1 < y0 {
                return;
            }
            let w = x1 - x0 + 1;
            let h = y1 - y0 + 1;
            let before = edit_op::copy_region_premul(&layer.base_image, x0, y0, w, h);
            let after = edit_op::copy_region_premul(&layer.image, x0, y0, w, h);
            (
                kind,
                layer.uid.to_string(),
                size,
                [x0, y0],
                [w, h],
                before,
                after,
            )
        };
        let (kind, uid, size, origin, region, before, after) = captured;
        let (Some(origin), Some(region), Some(image_size)) = (
            usize_pair_to_u32(origin),
            usize_pair_to_u32(region),
            usize_pair_to_u32(size),
        ) else {
            return;
        };
        match RasterDiff::from_region_pixels(
            &before,
            &after,
            origin,
            region,
            image_size,
            PS_UNDO_TILE_SIDE,
        ) {
            Ok(diff) if diff.is_empty() => {}
            Ok(diff) => {
                if let Some(op) = pixel_region_edit_op(
                    kind,
                    page_idx,
                    uid,
                    Arc::new(diff),
                    label.to_string(),
                ) {
                    self.history.record(op);
                }
            }
            Err(err) => {
                ms_log::runtime_log::log_warn(format!(
                    "[ps_editor] failed to build the {label} undo diff (page {page_idx}): {err}"
                ));
            }
        }
    }

    /// The unified Z of the raster band with `uid` from the current `bands` projection, or 0 when
    /// absent (a just-added raster whose band has not been projected yet, restored on top on redo).
    ///
    /// Not a composite lookup: it captures the doc-node Z an undo/redo `LayerLifecycle` re-inserts
    /// at, so its "absent" answer is a re-insert position, not the owner's past-the-top draw key
    /// (`ordering::band_z`). Draw order never reads it.
    pub(crate) fn raster_band_z(&self, uid: &str) -> u32 {
        self.bands
            .iter()
            .find_map(|band| match band {
                Band::Raster { uid: u, z } if u == uid => Some(*z),
                Band::Raster { .. } | Band::TextGroup { .. } | Band::PinnedText { .. } => None,
            })
            .unwrap_or(0)
    }

    /// Resolves a raster `LayerId` to its stable doc uid (the cross-tab identity).
    pub(crate) fn raster_uid(&self, id: LayerId) -> Option<String> {
        self.stack
            .as_ref()
            .and_then(|s| s.layer(id))
            .filter(|l| l.kind == LayerKind::Raster)
            .map(|l| l.uid.to_string())
    }

    /// Mutates the shared `LayerDoc` in memory for a high-frequency / live edit (e.g. an opacity
    /// slider or a visibility toggle) and re-projects, WITHOUT flushing to disk or bumping the
    /// revision — matching the legacy behavior where such edits persisted only on page-leave. Returns
    /// false (caller keeps its legacy local-only path) when no doc is wired or the page isn't resident.
    pub(crate) fn edit_doc_node<F>(&mut self, page_idx: usize, edit: F) -> bool
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
            // one edit (e.g. one opacity drag tick) is one revision step.
            if guard.page_revision(page_idx) == revision_before {
                guard.mark_page_changed(page_idx);
            }
        }
        // This edit only changed in-memory MODEL state (it deferred disk persistence to page-leave /
        // tab-switch), so the page now needs a flush on the next tab-switch.
        self.layers_dirty = true;
        self.sync_view_from_doc(page_idx);
        true
    }

    /// Flushes the shared doc's TEXT payload for `page_idx` into the staging `layers.json` (inline v3),
    /// after a PS-side text edit routed through `edit_doc_node`. Text-only — leaves rasters on disk
    /// untouched. The doc is the sole text writer; PS no longer writes `text_info.json`.
    pub(crate) fn flush_text_page(&mut self, page_idx: usize, project: &ProjectData) {
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(mut guard) = doc.lock() else {
            return;
        };
        // ASYNC text-only persist: enqueue to the background saver (falls back to sync flush when no
        // saver is enabled). The save-to-project/app-close barriers guarantee the enqueued text lands.
        if let Err(err) = guard.enqueue_page_text_save(
            page_idx,
            &project.paths.unsaved_layers_dir,
            Some(&project.paths.layers_dir),
        ) {
            ms_log::runtime_log::log_warn(format!("[ps_editor] doc text flush: {err}"));
        }
    }

    /// Per-frame cross-tab sync: re-project the active page when the shared `LayerDoc` changed THAT
    /// page (its `page_revision` advanced, or the active page differs from the last projected one)
    /// since this tab last projected. Any edit of the page in the typing tab (or our own that routed
    /// through the doc) advances its page revision; an edit of another page does not. This is the
    /// in-memory cross-tab path (replacing the old disk-revision bridge). Cheap when nothing changed:
    /// one lock and one map lookup.
    pub(crate) fn refresh_view_if_doc_version_changed(&mut self) {
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        // No active page: nothing to project (re-checked next frame, still only a field read).
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        let current = match doc.lock() {
            Ok(guard) => (page_idx, guard.page_revision(page_idx)),
            Err(_) => return,
        };
        if Some(current) == self.last_projected_revision {
            return;
        }
        if self.dragging_text_layer.is_some() {
            return; // don't yank a layer out from under an in-progress drag
        }
        ms_log::trace_log!(
            cat::SYNC,
            "page_revision_changed old={:?} new={:?} -> reproject",
            self.last_projected_revision,
            current
        );
        // Materialize the text-layer RUNTIMES from the doc (so a text layer the typing tab just created
        // has a local `PsTextLayer` for the projection to reconcile onto — no disk read), then project
        // the shared doc over the stack rasters / text model / bands. `sync_view_from_doc` updates
        // `last_projected_revision` and preserves uncommitted in-memory edits (pixels_dirty layers).
        self.materialize_text_runtime_from_doc(page_idx);
        self.sync_view_from_doc(page_idx);
    }
}
