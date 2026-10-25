/*
File: crates/ms-tab-ps-editor/src/selection_ops.rs

Purpose:
Selection-driven operations: text-layer drag, the selection marquee and context menu, clip-to-layer
ops and merge-down.

Key functions:
- `handle_text_layer_drag()`: translate-only drag of the topmost text layer.
- `draw_selection_marquee()`: static dashed marquee (see `marquee.rs`).
- `draw_selection_menu()`, `clip_op_submenu()`, `clip_layer_picker()`, `perform_clip()`,
  `clear_selection()`.
- `merge_down()`: merges a raster into the raster below it by band Z.

Notes:
All methods are inherent methods of `PsEditorTabState`.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

impl PsEditorTabState {
    /// Drags the topmost text layer under the pointer (translate only). Returns true while a drag is
    /// active so the raster tool is bypassed. On release, routes the new transform to the shared doc
    /// (bumping its version so the typing tab re-projects) and flushes the doc's inline text payload to
    /// `layers.json`.
    pub(crate) fn handle_text_layer_drag(
        &mut self,
        input: &CanvasInput,
        view: &viewport::ViewTransform,
        pointer_in_viewport: bool,
        project: &ProjectData,
    ) -> bool {
        let pointer_world = input.hover_pos.map(|p| view.screen_to_world(p).to_vec2());

        if self.dragging_text_layer.is_none() {
            if input.primary_pressed
                && pointer_in_viewport
                && let Some(world) = pointer_world
                && let Some(idx) = self
                    .text_layers
                    .iter()
                    .rposition(|l| l.visible && !l.has_deform() && l.contains_world(world))
            {
                self.dragging_text_layer = Some(idx);
                self.text_drag_mode = if input.modifiers.shift {
                    TextDragMode::Rotate
                } else if input.modifiers.command {
                    TextDragMode::Scale
                } else {
                    TextDragMode::Translate
                };
                self.text_drag_last = world;
                if let Some(layer) = self.text_layers.get(idx) {
                    let c = layer.center();
                    self.text_drag_ref = match self.text_drag_mode {
                        TextDragMode::Rotate => (world.y - c.y).atan2(world.x - c.x),
                        TextDragMode::Scale => (world - c).length(),
                        TextDragMode::Translate => 0.0,
                    };
                }
            }
            return self.dragging_text_layer.is_some();
        }

        let Some(idx) = self.dragging_text_layer else {
            return false;
        };
        if input.primary_down {
            if let (Some(world), Some(layer)) = (pointer_world, self.text_layers.get_mut(idx)) {
                match self.text_drag_mode {
                    TextDragMode::Translate => layer.translate(world - self.text_drag_last),
                    TextDragMode::Rotate => {
                        let c = layer.center();
                        let angle = (world.y - c.y).atan2(world.x - c.x);
                        layer.rotate_by(angle - self.text_drag_ref);
                        self.text_drag_ref = angle;
                    }
                    TextDragMode::Scale => {
                        let c = layer.center();
                        let dist = (world - c).length();
                        if self.text_drag_ref > 1e-3 {
                            layer.scale_by(dist / self.text_drag_ref);
                        }
                        self.text_drag_ref = dist.max(1e-3);
                    }
                }
                self.text_drag_last = world;
            }
            return true;
        }

        // Released: persist the new placement. Read the layer data out first so the `text_layers`
        // borrow ends before `edit_doc_node` takes `&mut self`.
        let target = self
            .text_layers
            .get(idx)
            .map(|l| (l.uid.clone(), l.center(), l.rotation(), l.scale()));
        if let Some((uid, center, rotation, scale)) = target {
            // Update the shared doc's Text node transform in memory (bumping its version, so the typing
            // tab re-projects), then flush the doc's INLINE text payload to `layers.json`. The doc is
            // the sole text writer — PS no longer writes `text_info.json`.
            if let Some(page_idx) = self.active_page_idx {
                let rec = TransformRec {
                    cx: center.x,
                    cy: center.y,
                    rotation,
                    scale,
                };
                self.edit_doc_node(page_idx, |doc| {
                    doc.set_transform(page_idx, &uid, rec);
                });
                self.flush_text_page(page_idx, project);
            }
        }
        self.dragging_text_layer = None;
        true
    }

    /// Draws the selection as a thin alternating black/white dashed marquee along its boundary
    /// loops, instead of a translucent fill. The pattern is static (no animation).
    ///
    /// The loops come from `Selection::outline_loops` and follow the mask's PIXEL EDGES, so a
    /// freehand lasso arrives as a staircase of one-pixel steps. Two consequences shape this code
    /// and both run on the GUI thread every frame:
    /// - the dash phase must accumulate along the WHOLE loop (see `walk_dash_runs`), otherwise the
    ///   pattern restarts on every one-pixel step and the marquee degenerates into a solid line;
    /// - the point count is unbounded, so the screen-space path is decimated (below) and the total
    ///   number of emitted runs is capped by `MARQUEE_MAX_RUNS`.
    ///
    /// All loops share one shape buffer and a single `Painter::extend`.
    pub(crate) fn draw_selection_marquee(&self, ui: &egui::Ui, view: &viewport::ViewTransform) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        if !selection.any() {
            return;
        }
        let painter = ui.painter_at(view.viewport_rect);
        let mut shapes: Vec<egui::Shape> = Vec::new();
        let mut screen: Vec<Pos2> = Vec::new();
        let mut budget = MARQUEE_MAX_RUNS;
        for loop_pts in selection.outline_loops() {
            if budget == 0 {
                break;
            }
            if loop_pts.len() < 2 {
                continue;
            }
            screen.clear();
            decimate_to_screen(loop_pts, view, &mut screen);
            let emitted = walk_dash_runs(&screen, MARQUEE_DASH_PX, budget, |run| {
                let color = if run.black {
                    Color32::BLACK
                } else {
                    Color32::WHITE
                };
                shapes.push(egui::Shape::line_segment(
                    [run.from, run.to],
                    Stroke::new(1.0, color),
                ));
            });
            budget -= emitted;
        }
        painter.extend(shapes);
    }

    /// Right-click / finger long-press menu on the canvas offering copy/cut of the selection from
    /// chosen layers.
    ///
    /// Opens on a secondary click or when `long_press_at` (this frame's
    /// `TouchOutput::long_press_at`, screen points) is `Some`, anchored there; both take the one
    /// open path `ms_input::touch::context_menu_popup`. The touched-layer list is recomputed
    /// once per menu open so the per-frame menu closure stays cheap even for large selections.
    ///
    /// `hud_gesture_possible` suppresses the menu for the frame unless a long-press opens it: Alt +
    /// right-drag is the brush's size/hardness HUD, and a short one still produces a secondary
    /// CLICK on this `Sense::click_and_drag` canvas, which would open the menu, so the only way to
    /// keep the menu out of the gesture is not to call it. The flag is mouse-only — a finger
    /// long-press is no HUD gesture. The caller must scope the flag to the tool that owns the
    /// gesture — the brush — so Alt+right-click keeps opening the menu under every other tool.
    pub(crate) fn draw_selection_menu(
        &mut self,
        response: &egui::Response,
        project: &ProjectData,
        hud_gesture_possible: bool,
        long_press_at: Option<Pos2>,
    ) {
        if hud_gesture_possible && long_press_at.is_none() {
            return;
        }
        if response.secondary_clicked() || long_press_at.is_some() {
            self.refresh_clip_touched_layers();
            self.clip_selected_layers.clear();
        }
        let has_selection = self.selection.as_ref().is_some_and(Selection::any);
        ms_input::touch::context_menu_popup(response, long_press_at)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                if !has_selection {
                    ui.label(t!("ps_editor.selection_menu.no_selection"));
                    return;
                }
                ui.menu_button(t!("ps_editor.selection_menu.copy"), |ui| {
                    self.clip_op_submenu(ui, ClipMode::Copy, project)
                });
                ui.menu_button(t!("ps_editor.selection_menu.cut"), |ui| {
                    self.clip_op_submenu(ui, ClipMode::Cut, project)
                });
            });
    }

    /// One copy/cut submenu: top layer, a multi-select layer picker, or all layers.
    pub(crate) fn clip_op_submenu(&mut self, ui: &mut egui::Ui, mode: ClipMode, project: &ProjectData) {
        let touched = self.clip_touched_layers.clone();
        if ui
            .add_enabled(
                !touched.is_empty(),
                egui::Button::new(t!("ps_editor.selection_menu.clip_from_top_layer")),
            )
            .clicked()
        {
            if let Some(&top) = touched.last() {
                self.perform_clip(mode, &[top], project);
            }
            egui::Popup::close_all(ui.ctx());
        }
        ui.menu_button(t!("ps_editor.selection_menu.clip_from_layers"), |ui| {
            self.clip_layer_picker(ui, mode, &touched, project);
        });
        if ui
            .button(t!("ps_editor.selection_menu.clip_from_all_layers"))
            .clicked()
        {
            let all: Vec<LayerId> = self
                .stack
                .as_ref()
                .map(|stack| stack.layers().iter().map(|layer| layer.id).collect())
                .unwrap_or_default();
            self.perform_clip(mode, &all, project);
            egui::Popup::close_all(ui.ctx());
        }
    }

    /// Multi-select picker listing the layers the selection touches (top-to-bottom in the UI).
    pub(crate) fn clip_layer_picker(
        &mut self,
        ui: &mut egui::Ui,
        mode: ClipMode,
        touched: &[LayerId],
        project: &ProjectData,
    ) {
        if touched.is_empty() {
            ui.label(t!("ps_editor.selection_menu.no_layers_touched"));
            return;
        }
        // Display top-to-bottom (reverse of the bottom-to-top stack order).
        for &id in touched.iter().rev() {
            let name = self
                .stack
                .as_ref()
                .and_then(|stack| stack.layer(id))
                .map_or_else(
                    || tf!("ps_editor.selection_menu.layer_fallback", id = id),
                    |layer| layer.name.clone(),
                );
            let mut checked = self.clip_selected_layers.contains(&id);
            if ui.checkbox(&mut checked, name).changed() {
                if checked {
                    self.clip_selected_layers.insert(id);
                } else {
                    self.clip_selected_layers.remove(&id);
                }
            }
        }
        ui.separator();
        let count = self.clip_selected_layers.len();
        let label = format!("{} ({count})", mode.verb());
        if ui
            .add_enabled(count > 0, egui::Button::new(label))
            .clicked()
        {
            // Preserve bottom-to-top stack order for compositing.
            let ids: Vec<LayerId> = touched
                .iter()
                .copied()
                .filter(|id| self.clip_selected_layers.contains(id))
                .collect();
            self.perform_clip(mode, &ids, project);
            egui::Popup::close_all(ui.ctx());
        }
    }

    /// Recomputes which layers the current selection overlaps (bottom-to-top).
    pub(crate) fn refresh_clip_touched_layers(&mut self) {
        let mut touched = Vec::new();
        if let (Some(stack), Some(selection)) = (self.stack.as_ref(), self.selection.as_ref())
            && let Some(bounds) = selection.bounds()
        {
            for layer in stack.layers() {
                if layer_touches_selection(layer, selection, bounds) {
                    touched.push(layer.id);
                }
            }
        }
        self.clip_touched_layers = touched;
    }

    /// Copies (or, for [`ClipMode::Cut`], moves) the selection from `layer_ids` (bottom-to-top)
    /// into a new raster layer composited from those layers in order.
    ///
    /// For a cut, the selected pixels are cleared from every chosen layer **except** the locked
    /// source layer, which is immutable and can never be cut from; the clean overlay and raster
    /// layers are cleared normally. Cleared `Клин` pixels are written back to the shared
    /// `CleanOverlaysModel` (it is not a doc node), so a cut from it survives a page switch — the
    /// same write-back path the brush commit and merge-into use.
    pub(crate) fn perform_clip(&mut self, mode: ClipMode, layer_ids: &[LayerId], project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // The selection bounds bound the Clean write-back below (a ribbon page is far too large to
        // push whole).
        let bounds = {
            let (stack, selection) = match (self.stack.as_mut(), self.selection.as_ref()) {
                (Some(stack), Some(selection)) if selection.any() => (stack, selection),
                _ => return,
            };
            match clip_into_new_layer(stack, selection, mode, layer_ids) {
                Some(bounds) => bounds,
                None => return,
            }
        };

        // The new clip layer is now the active raster on the stack; mirror it as a doc node. For a
        // cut, also push the cleared base pixels of every source RASTER back to the doc (dirty). The
        // clean base layer (cut from but not a doc node) is unaffected here. `route_to_doc` flushes +
        // bumps + re-projects.
        let new_node = self
            .stack
            .as_ref()
            .and_then(|s| s.layer(s.active_id()))
            .map(layer_to_raster_node);
        // Snapshot cleared source rasters (cut only): uid -> cleared pixels.
        let cleared: Vec<(String, ColorImage)> = if mode == ClipMode::Cut {
            self.stack
                .as_ref()
                .map(|s| {
                    layer_ids
                        .iter()
                        .filter_map(|id| s.layer(*id))
                        .filter(|l| l.kind == LayerKind::Raster)
                        .map(|l| (l.uid.to_string(), l.image.clone()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if let Some(node) = new_node {
            self.route_to_doc(page_idx, project, |doc| {
                for (uid, img) in &cleared {
                    doc.set_raster_pixels(
                        page_idx,
                        uid,
                        img.clone(),
                        img.clone(),
                        Vec::new(),
                        true,
                    );
                }
                doc.add_node(page_idx, node);
            });
        }

        // A cut clears pixels from the chosen layers; re-upload them fully (the cleared region in
        // a transformed layer is not axis-aligned). The fresh layer's cache entry is created by
        // `sync_render_cache` next frame, already fully dirty.
        if mode == ClipMode::Cut {
            for &id in layer_ids {
                if let Some(cache) = self.render_cache.get_mut(&id) {
                    cache.mark_all_dirty();
                }
            }
            // `Клин` was cut from but has no doc node, so the routing above skipped it. Without this
            // the cleared pixels lived only in the tab's stack and died on the next page switch or
            // overlay-revision reload.
            let cut_clean = self.stack.as_ref().is_some_and(|s| {
                layer_ids
                    .iter()
                    .filter_map(|id| s.layer(*id))
                    .any(|l| l.kind == LayerKind::Clean)
            });
            if cut_clean {
                self.write_clean_region_to_model(
                    page_idx,
                    bounds.min_x,
                    bounds.min_y,
                    bounds.max_x.saturating_sub(bounds.min_x) + 1,
                    bounds.max_y.saturating_sub(bounds.min_y) + 1,
                );
            }
        }
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Merges the raster layer `id` down onto the layer directly beneath it on the unified band-Z
    /// axis. The lower layer becomes a page-sized identity layer holding both composited (src-over);
    /// the upper layer is removed. No-op when `id` is a base layer or has nothing beneath it.
    ///
    /// The lower participant may be the `Клин` base layer (the bottom-most raster merges into it),
    /// in which case the merged pixels go to the shared `CleanOverlaysModel` instead of the doc —
    /// `Клин` is not a doc node — bounded to the upper layer's page footprint
    /// ([`page_footprint_rect`]), which is the only region a merge can change. `Исходник` is never a
    /// target. The UPPER participant is always a user raster.
    ///
    /// NOT undoable (by design, like the cut): it removes a layer AND rewrites another's pixels,
    /// which needs a batch op the history does not have yet.
    pub(crate) fn merge_down(&mut self, id: LayerId, project: &ProjectData) -> bool {
        let Some(page_idx) = self.active_page_idx else {
            return false;
        };
        // Pick the layer directly BELOW by unified band-Z (the visually-below raster), not the stack
        // neighbor — so a manual reorder merges the pair the user actually sees. Resolve this before
        // borrowing the stack to keep the borrow checker happy.
        let Some(below_uid) = self.raster_below_uid(id) else {
            return false;
        };
        let Some(stack) = self.stack.as_ref() else {
            return false;
        };
        let size = stack.size();
        // Build the merged pixels and resolve both participants' ids/uids (the borrow ends here).
        let (below_id, below_kind, upper_uid, upper_footprint, merged) = {
            let layers = stack.layers();
            let Some(upper) = layers.iter().find(|l| l.id == id) else {
                return false;
            };
            let Some(below) = layers.iter().find(|l| l.uid.to_string() == below_uid) else {
                return false;
            };
            // The UPPER participant must be a user raster (base layers are never merged away). The
            // lower one may be `Клин`, but never `Исходник` — which `merge_candidates_by_band_z`
            // already excludes; the check is repeated here so the invariant is enforced at the
            // mutation site and not only by the candidate query.
            if upper.kind.is_base() || below.kind == LayerKind::Source {
                return false;
            }
            // A merge rewrites the lower layer's base pixels, so refuse while either participant
            // still shows a non-destructive effects chain — bake it first.
            if !upper.effects.is_empty() || !below.effects.is_empty() {
                ms_log::runtime_log::log_warn("[ps_editor] Сначала запеките слой");
                return false;
            }
            // Bottom-to-top: below then upper, so the upper composites OVER the below.
            let merged = composite_to_page(&[below, upper], size);
            (
                below.id,
                below.kind,
                upper.uid.to_string(),
                page_footprint_rect(upper, size),
                merged,
            )
        };
        // Record the upper raster's deletion (so the manifest save drops it — `save_page_rasters`
        // preserves unowned rasters otherwise), then absorb the composited pixels into the lower
        // participant. Which store that is depends on its kind: a raster becomes a page-sized
        // identity doc node (pixels_dirty), while `Клин` — not a doc node — takes the pixels on the
        // stack and pushes them to the shared clean model. Either way the upper raster's doc node is
        // removed and `persist_current_page` (below) writes the manifest with the removed uid.
        self.record_raster_deletion(id);
        match below_kind {
            LayerKind::Raster => {
                let merged_for_doc = merged;
                let identity = transform_to_rec(LayerTransform::identity_for(size));
                let below_uid_for_doc = below_uid.clone();
                self.edit_doc_node(page_idx, |doc| {
                    doc.set_raster_pixels(
                        page_idx,
                        &below_uid_for_doc,
                        merged_for_doc.clone(),
                        merged_for_doc,
                        Vec::new(),
                        true,
                    );
                    doc.set_transform(page_idx, &below_uid_for_doc, identity);
                    doc.remove_node(page_idx, &upper_uid);
                });
            }
            LayerKind::Clean => {
                // `Клин` is not a doc node, so only the upper raster is removed there; the merged
                // pixels are absorbed by the base layer and pushed to the shared overlay model.
                self.edit_doc_node(page_idx, |doc| {
                    doc.remove_node(page_idx, &upper_uid);
                });
                if let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(below_id)) {
                    layer.transform = LayerTransform::identity_for(size);
                    layer.base_image = merged.clone();
                    layer.image = merged;
                }
                // Push ONLY the upper layer's page footprint, not the whole page. Everywhere else
                // `composite_to_page` reproduces the `Клин` pixels unchanged (the upper sample is
                // transparent there and `Клин` is a page-sized identity layer), so a bounded write
                // is complete — and a whole-page write on a ~800x19000 ribbon means two full RGBA
                // copies plus a tiled zstd diff of both, on the GUI thread and under the model lock
                // (the autosave worker shares it). An upper layer that really does cover the page
                // still costs the whole page; that is inherent to the merge it asked for.
                if let Some((fx, fy, fw, fh)) = upper_footprint {
                    self.write_clean_region_to_model(page_idx, fx, fy, fw, fh);
                }
            }
            // `raster_below_uid` never yields the immutable source layer.
            LayerKind::Source => return false,
        }
        self.persist_current_page(project);
        // Refresh caches and selection locally (the projection rebuilt the stack).
        self.render_cache.remove(&id);
        if let Some(cache) = self.render_cache.get_mut(&below_id) {
            cache.mark_all_dirty();
        }
        if let Some(stack) = self.stack.as_mut() {
            stack.set_active(below_id);
        }
        // The upper participant's row is gone and the active layer just moved to the merge target,
        // so the panel's primary row follows it. Set explicitly (not via `normalize_panel_primary`):
        // the projection inside `edit_doc_node` already re-pointed the primary at the then-active
        // layer, so it is "valid" by now and normalization alone would leave it on the wrong row.
        self.panel_primary = self.active_row_sel();
        self.panel_selection.clear();
        self.panel_anchor = None;
        true
    }
}
