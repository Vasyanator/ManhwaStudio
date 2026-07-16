/*
File: tab/selection_rasters.rs

Purpose:
Selection state and raster-layer canvas interaction for the typing tab: clearing
and switching the single active selection (text overlay vs raster), the edit-panel
selection payload, overlay/raster removal, wheel/keyboard transform shortcuts
(rotate, scale, arrow-nudge), raster deform-mesh seeding, geometry persistence
routing, and the full raster select/move/rotate/perspective canvas interaction
with its context menu.

Notes:
Extracted verbatim from `tab.rs`. Methods are `pub(super)` so `tab.rs` and sibling
submodules of `tab` can use them. `use super::*;` pulls in the parent module's
types and imports. Struct/enum definitions and the rest of the big
`impl TypingTextOverlayLayer` block remain in `tab.rs`; these methods reach the
private items that stay there as descendants of module `tab`.
*/

use super::*;

impl TypingTextOverlayLayer {
    pub(super) fn clear_selection(&mut self) {
        if crate::trace::trace_enabled()
            && (self.selected_overlay_idx.is_some() || self.selected_raster_idx.is_some())
        {
            crate::trace_log!(
                cat::TYPING,
                "clear_selection overlay_idx={:?} raster_idx={:?}",
                self.selected_overlay_idx,
                self.selected_raster_idx
            );
        }
        self.selected_overlay_idx = None;
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
        self.drag_has_changes = false;
        self.width_resize_drag = None;
        self.shape_variant_preview = None;
        self.selected_raster_idx = None;
        self.selected_raster_page = None;
        self.transform_mode_raster_idx = None;
        self.raster_drag_state = None;
        self.raster_drag_has_changes = false;
    }

    /// Selects the raster at `raster_idx` on `page_idx`, clearing any overlay selection (one selection
    /// at a time across the two layer kinds). Selecting a DIFFERENT raster exits raster transform mode.
    /// `selected_raster_page` is set alongside `selected_raster_idx` so per-page shortcut handlers can
    /// tell which page the selection lives on.
    pub(super) fn select_raster(&mut self, page_idx: usize, raster_idx: usize) {
        if self.selected_raster_idx != Some(raster_idx) || self.selected_raster_page != Some(page_idx)
        {
            crate::trace_log!(
                cat::TYPING,
                "select_raster page_idx={} raster_idx={}",
                page_idx,
                raster_idx
            );
        }
        if self.transform_mode_raster_idx != Some(raster_idx) {
            self.transform_mode_raster_idx = None;
        }
        self.selected_raster_idx = Some(raster_idx);
        self.selected_raster_page = Some(page_idx);
        self.selected_overlay_idx = None;
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
        self.drag_has_changes = false;
        self.width_resize_drag = None;
        self.shape_variant_preview = None;
    }

    pub(super) fn has_selected_overlay(&self) -> bool {
        self.selected_overlay_idx
            .and_then(|idx| self.overlays.get(idx))
            .is_some()
    }

    pub(super) fn selected_overlay_for_edit(&self) -> Option<TypingSelectedOverlayForEdit> {
        let overlay_idx = self.selected_overlay_idx?;
        let overlay = self.overlays.get(overlay_idx)?;
        let width_px_hint = overlay_render_data_width_hint(
            overlay.render_data_json.as_ref(),
            (overlay.size_px[0] as f32 * overlay.user_scale.max(0.01))
                .round()
                .max(1.0) as u32,
        );
        Some(TypingSelectedOverlayForEdit {
            overlay_idx,
            overlay_kind: overlay.kind,
            render_data_json: overlay.render_data_json.clone(),
            width_px_hint,
            user_scale: overlay.user_scale,
            rotation_deg: overlay.angle_deg,
            target: TypingEditTarget::Overlay(overlay_idx),
        })
    }

    /// The edit-panel payload for the current selection: a text/image overlay, or — when a raster is
    /// selected — the raster, shown with the same image UI (scale + rotation + effects, no text).
    pub(super) fn selected_item_for_edit(&self, page_idx: usize) -> Option<TypingSelectedOverlayForEdit> {
        if self.selected_overlay_idx.is_some() {
            return self.selected_overlay_for_edit();
        }
        let raster_idx = self.selected_raster_idx?;
        let raster = self.raster_layers_by_page.get(&page_idx)?.get(raster_idx)?;
        Some(TypingSelectedOverlayForEdit {
            overlay_idx: 0, // unused for a raster target
            overlay_kind: TypingOverlayKind::Image,
            render_data_json: Some(serde_json::json!({ "effects": raster.effects.clone() })),
            width_px_hint: raster.image.size[0] as u32,
            user_scale: raster.transform.scale,
            rotation_deg: raster.transform.rotation.to_degrees(),
            target: TypingEditTarget::Raster {
                page_idx,
                uid: raster.uid.clone(),
            },
        })
    }

    /// Flushes a deferred text-layer save when the SELECTION changes — the primary focus-loss point.
    ///
    /// Observes BOTH selection axes: the text/image overlay (`selected_overlay_idx`) and the raster
    /// (`selected_raster_page` + `selected_raster_idx`). Watching the overlay axis alone missed the
    /// overlay→raster switch, which is a focus loss for the overlay just as much as deselecting it.
    /// The raster index is meaningless without its page (the same index exists on every page), so the
    /// two are tracked as one pair, matching how the selection itself is stored.
    ///
    /// A change is only flushed when SOMETHING was previously selected: on the first selection of a
    /// session there is no prior focus to lose, and any edit pending from elsewhere is still covered by
    /// the idle debounce.
    pub(super) fn flush_edit_save_on_selection_change(&mut self) {
        let raster = self.selected_raster_page.zip(self.selected_raster_idx);
        if self.last_selected_overlay_idx == self.selected_overlay_idx
            && self.last_selected_raster == raster
        {
            return;
        }
        let had_selection =
            self.last_selected_overlay_idx.is_some() || self.last_selected_raster.is_some();
        self.last_selected_overlay_idx = self.selected_overlay_idx;
        self.last_selected_raster = raster;
        if had_selection {
            self.flush_placement_save_if_dirty(TypingSaveFlushReason::SelectionChange);
        }
    }

    pub(super) fn remove_overlay(&mut self, overlay_idx: usize) {
        if overlay_idx >= self.overlays.len() {
            return;
        }
        // Capture the doc-node identity (TEXT overlays only) before removing the runtime, so the
        // matching node can be dropped from the shared doc afterward.
        let doc_node = self
            .overlays
            .get(overlay_idx)
            .filter(|o| o.kind == TypingOverlayKind::Text)
            .map(|o| (o.page_idx, o.uid.clone()));
        if crate::trace::trace_enabled()
            && let Some(o) = self.overlays.get(overlay_idx)
        {
            crate::trace_log!(
                cat::TYPING,
                "remove_overlay idx={} uid={} kind={:?} page={}",
                overlay_idx,
                o.uid,
                o.kind,
                o.page_idx
            );
        }
        self.overlays.remove(overlay_idx);
        self.shape_variant_preview = None;

        self.pending_upload_indices = self
            .pending_upload_indices
            .iter()
            .filter_map(|&idx| {
                if idx == overlay_idx {
                    None
                } else if idx > overlay_idx {
                    Some(idx - 1)
                } else {
                    Some(idx)
                }
            })
            .collect();
        self.pending_upload_set = self.pending_upload_indices.iter().copied().collect();

        // Detect (before shifting) whether the overlay under the active transform is the one being
        // removed, so its transient vector-transform state can be torn down fully rather than reindexed.
        let removed_was_transform_overlay = self.transform_mode_overlay_idx == Some(overlay_idx);
        shift_index_after_remove(&mut self.selected_overlay_idx, overlay_idx);
        shift_index_after_remove(&mut self.transform_mode_overlay_idx, overlay_idx);
        shift_index_after_remove(&mut self.last_selected_overlay_idx, overlay_idx);
        if let Some(mut drag_state) = self.drag_state.take() {
            if drag_state.overlay_idx == overlay_idx {
                self.drag_state = None;
            } else {
                if drag_state.overlay_idx > overlay_idx {
                    drag_state.overlay_idx -= 1;
                }
                self.drag_state = Some(drag_state);
            }
        }
        if let Some(mut auto_job) = self.auto_typing_job.take() {
            if auto_job.overlay_idx == overlay_idx {
                self.auto_typing_job = None;
            } else {
                if auto_job.overlay_idx > overlay_idx {
                    auto_job.overlay_idx -= 1;
                }
                self.auto_typing_job = Some(auto_job);
            }
        }
        // Keep the transient vector-transform state consistent with the shifted overlay indices. When the
        // removed overlay was the active transform target, tear the whole preview down (resets the mode
        // kind to Raster and bumps the base-render token); otherwise just reindex so a deleted lower-index
        // overlay can't leave the drag/base pointing at the wrong overlay.
        if removed_was_transform_overlay {
            self.exit_vector_transform_mode();
        } else {
            if let Some(mut drag) = self.vector_transform_drag.take() {
                if drag.overlay_idx == overlay_idx {
                    self.vector_transform_drag = None;
                } else {
                    if drag.overlay_idx > overlay_idx {
                        drag.overlay_idx -= 1;
                    }
                    self.vector_transform_drag = Some(drag);
                }
            }
            if let Some(mut base) = self.vector_transform_base.take() {
                if base.overlay_idx == overlay_idx {
                    self.vector_transform_base = None;
                } else {
                    if base.overlay_idx > overlay_idx {
                        base.overlay_idx -= 1;
                    }
                    self.vector_transform_base = Some(base);
                }
            }
        }
        self.drag_has_changes = false;
        // Drop the matching node from the shared doc (the source of truth), then re-project bands.
        if let Some((page_idx, uid)) = doc_node {
            self.route_to_doc(page_idx, move |doc| {
                doc.remove_node(page_idx, &uid);
            });
        }
        // STRUCTURAL — stays EAGER, deliberately NOT deferred. A deletion's durability must not depend
        // on a later flush point being reached: `write_page_text_payload` keeps an emptied page
        // present-but-empty precisely so a deletion survives reload, and a lost deferral would let the
        // overlay resurrect from the stale on-disk set. The save persists the whole live state, so it
        // settles any deferred edit on this overlay too — but only once actually dispatched.
        self.dispatch_structural_placement_save("overlay delete");
    }

    /// Removes a raster layer from the current page: drops the doc node (the source of truth), removes
    /// the cached projection, fixes `selected_raster_idx` / `transform_mode_raster_idx` / drag state,
    /// frees its texture, and persists. Mirrors `remove_overlay`.
    pub(super) fn remove_raster(&mut self, page_idx: usize, raster_idx: usize) {
        let Some(uid) = self
            .raster_layers_by_page
            .get(&page_idx)
            .and_then(|v| v.get(raster_idx))
            .map(|l| l.uid.clone())
        else {
            return;
        };
        crate::trace_log!(
            cat::TYPING,
            "remove_raster page={} raster_idx={} uid={}",
            page_idx,
            raster_idx,
            uid
        );
        // Drop the node from the shared doc (its texture goes with the cached layer below).
        self.route_to_doc(page_idx, |doc| {
            doc.remove_node(page_idx, &uid);
        });
        // Remove the cached projection (its `texture` handle is freed on drop).
        if let Some(layers) = self.raster_layers_by_page.get_mut(&page_idx)
            && raster_idx < layers.len()
        {
            layers.remove(raster_idx);
        }
        self.raster_texture_generations
            .retain(|(p, u), _| !(*p == page_idx && *u == uid));
        // Fix the selection / transform-mode / drag indices (shift down past the removed one). The
        // selection/transform indices are positions into THIS page's list, so only shift them when the
        // selection belongs to `page_idx`; shifting against another page's removal would corrupt them.
        // Keep `selected_raster_page` in lock-step: a shift that empties the index clears the page too.
        if self.selected_raster_page == Some(page_idx) {
            shift_index_after_remove(&mut self.selected_raster_idx, raster_idx);
            shift_index_after_remove(&mut self.transform_mode_raster_idx, raster_idx);
            if self.selected_raster_idx.is_none() {
                self.selected_raster_page = None;
            }
        }
        if let Some(mut state) = self.raster_drag_state.take() {
            if state.page_idx == page_idx && state.raster_idx == raster_idx {
                self.raster_drag_state = None;
                self.raster_drag_has_changes = false;
            } else {
                if state.page_idx == page_idx && state.raster_idx > raster_idx {
                    state.raster_idx -= 1;
                }
                self.raster_drag_state = Some(state);
            }
        }
        // Persist: flush the page, explicitly DROPPING the removed raster from the manifest (otherwise
        // `save_page_rasters` would preserve it as another tab's, and it would resurrect on disk).
        if let Some(primary) = self.layers_primary_dir.clone() {
            let fallback = self.layers_fallback_dir.clone();
            if let Some(doc) = self.layer_doc.clone()
                && let Ok(mut guard) = doc.lock()
                && let Err(err) =
                    guard.flush_page_dropping_raster(page_idx, &primary, fallback.as_deref(), &uid)
            {
                crate::runtime_log::log_warn(format!("[typing] persist raster delete: {err}"));
            }
        }
        // STRUCTURAL — stays EAGER (see `remove_overlay`): anti-resurrection durability must not wait
        // on a flush point. This also persists the live text state, settling any deferred edit.
        self.dispatch_structural_placement_save("raster delete");
    }

    /// Ctrl/Cmd+wheel rotation of the selected text overlay by ±2° per notch. Which rotation is driven
    /// is chosen by the app-wide `rotation_ctrl_wheel::rotation_ctrl_wheel_mode()` setting:
    /// `Raster` rotates the rasterized PNG placement (`angle_deg` / deform-mesh), while `Vector` adds
    /// to the render param `global_rotation_deg` and re-renders the glyph outlines sharply. Vector is
    /// only valid for an eligible text overlay (allowed layout mode, no raster deform mesh); when it is
    /// not, this quietly falls back to the raster rotation so the wheel still visibly rotates. No-op
    /// unless a text overlay on `page_idx` is selected, not in transform mode, and Ctrl/Cmd is held.
    pub(super) fn try_rotate_selected_overlay_by_ctrl_wheel(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        if self.transform_mode_overlay_idx == Some(selected_idx) {
            return;
        }

        let Some(selected_overlay) = self.overlays.get(selected_idx) else {
            return;
        };
        if selected_overlay.page_idx != page_idx {
            return;
        }

        let (ctrl_or_command, scroll_delta_y) = ui.ctx().input(|input| {
            (
                input.modifiers.ctrl || input.modifiers.command,
                // Raw wheel: this rotation is Ctrl-gated, and egui zeroes
                // `smooth_scroll_delta` while Ctrl/Cmd is held.
                crate::input_util::raw_wheel_delta(input).y,
            )
        });
        if !ctrl_or_command || scroll_delta_y.abs() <= f32::EPSILON {
            return;
        }

        let steps: f32 = if scroll_delta_y > 0.0 { 1.0 } else { -1.0 };
        let delta_deg: f32 = steps * 2.0;
        let delta_rad = delta_deg.to_radians();

        // Snapshot the overlay's rotation inputs AND its vector eligibility in one borrow.
        let (start_angle_deg, start_mesh_scene, start_mesh_dims, had_mesh, is_text, layout_allows_vector) = {
            let overlay = &self.overlays[selected_idx];
            let geometry = overlay_scene_geometry(overlay, image_rect, zoom);
            let is_text = overlay.kind == TypingOverlayKind::Text;
            let layout_allows_vector = vector_transform_allowed_for_layout_mode(
                overlay_text_layout_mode(overlay.render_data_json.as_ref()),
            );
            (
                overlay.angle_deg,
                geometry.mesh_scene,
                (geometry.mesh_cols, geometry.mesh_rows),
                overlay.deform_mesh.is_some(),
                is_text,
                layout_allows_vector,
            )
        };

        // A vector re-render is TEXT-render based: valid only for a text overlay whose layout mode
        // composes with the post-layout warp and that has NO raster deform mesh (the mesh path is a
        // raster-only post-process on top of the PNG).
        let vector_eligible = is_text && layout_allows_vector && !had_mesh;
        use crate::tabs::typing::rotation_ctrl_wheel::{self, RotationCtrlWheelMode};
        let mode = rotation_ctrl_wheel::rotation_ctrl_wheel_mode();
        // Exhaustive on the owned enum (no `_` arm): Raster always drives placement; Vector drives the
        // render param only when the overlay is eligible, otherwise it falls back to raster below.
        let use_vector = match mode {
            RotationCtrlWheelMode::Vector => vector_eligible,
            RotationCtrlWheelMode::Raster => false,
        };
        if matches!(mode, RotationCtrlWheelMode::Vector) && !vector_eligible {
            // Quiet fallback: the setting asks for a vector rotation but this overlay can't be
            // re-rendered vectorially, so Ctrl+wheel still rotates the raster placement below.
            crate::trace_log!(
                cat::TYPING,
                "ctrl_wheel rotate: vector mode but overlay idx={} not vector-eligible (text={} layout_ok={} mesh={}); raster fallback",
                selected_idx,
                is_text,
                layout_allows_vector,
                had_mesh
            );
        }

        // Zero the wheel delta regardless of branch (Ctrl-gated raw wheel; egui already zeroes the
        // smooth delta while Ctrl/Cmd is held, but the raw notch must not scroll the page too).
        ui.ctx().input_mut(|input| {
            input.smooth_scroll_delta = Vec2::ZERO;
        });

        // VECTOR: add the delta to the render param `global_rotation_deg` and re-render sharply. When
        // the overlay has no usable render_data/text_params (should not happen for an eligible text
        // overlay), fall through to the raster path so Ctrl+wheel still visibly rotates.
        if use_vector {
            let ctx = ui.ctx().clone();
            if self.rotate_selected_overlay_global_rotation(selected_idx, delta_deg, &ctx) {
                return;
            }
        }

        // RASTER: rotate the already-rasterized placement (EXACT prior behavior; also the fallback).
        if let Some(overlay) = self.overlays.get_mut(selected_idx) {
            if had_mesh {
                let center_scene = deform_mesh_center_scene(&start_mesh_scene);
                let rotated_scene = rotate_mesh_scene(&start_mesh_scene, center_scene, delta_rad);
                let page_size = page_size_from_image_rect(image_rect, zoom);
                let rotated_page_px = rotated_scene
                    .into_iter()
                    .map(|scene| page_px_from_scene(image_rect, zoom, scene))
                    .collect::<Vec<_>>();
                overlay.deform_mesh = TypingOverlayDeformMesh::new(
                    start_mesh_dims.0,
                    start_mesh_dims.1,
                    rotated_page_px,
                    page_size,
                );
                sync_overlay_center_from_deform_mesh(overlay, page_size);
            } else {
                overlay.angle_deg = normalize_angle_deg(start_angle_deg + delta_deg);
            }
        }

        self.mark_overlay_geometry_changed(selected_idx, false);
        // EDIT (Ctrl+wheel raster rotation): deferred — a wheel spin marks once per notch and writes
        // once the gesture settles, instead of once per notch.
        self.mark_placement_save_dirty();
    }

    /// Applies a Ctrl+wheel VECTOR rotation to a text overlay: adds `delta_deg` to its render param
    /// `global_rotation_deg`, normalized CONTINUOUSLY into the panel's `(-180, 180]` range (so
    /// repeated wheel steps keep rotating past ±180 instead of sticking at a clamp), injects it into
    /// `render_data.text_params`, and dispatches the sharp latest-wins background edit-render via the
    /// shared `dispatch_vector_rerender` (which writes the render_data back onto the overlay so the
    /// edit panel re-syncs, and requests a placement save).
    ///
    /// Returns `true` when the re-render was dispatched, `false` when the overlay has no usable
    /// `render_data` / `text_params` object so the caller can fall back to a raster rotation.
    fn rotate_selected_overlay_global_rotation(
        &mut self,
        overlay_idx: usize,
        delta_deg: f32,
        ctx: &egui::Context,
    ) -> bool {
        let Some(mut render_data) = self
            .overlays
            .get(overlay_idx)
            .and_then(|o| o.render_data_json.clone())
        else {
            return false;
        };
        let Some(text_params) = render_data
            .get_mut("text_params")
            .and_then(Value::as_object_mut)
        else {
            return false;
        };
        // Read the current global rotation the same way `codec.rs` does (absent -> 0.0), add the
        // wheel delta, and wrap continuously into (-180, 180] via `normalize_angle_deg`.
        let current = text_params
            .get("global_rotation_deg")
            .and_then(value_as_f32)
            .unwrap_or(0.0);
        let new_rotation = normalize_angle_deg(current + delta_deg);
        text_params.insert("global_rotation_deg".to_string(), json!(new_rotation));
        // Once the param is committed we own the vector path: dispatch surfaces any render error
        // itself, so we return `true` even if it reports a failure (no double raster rotation).
        self.dispatch_vector_rerender(overlay_idx, render_data, ctx);
        true
    }

    /// Sets the selected TEXT overlay's configured `width_px` (source px) to `new_width_px`, clamped to
    /// [`TEXT_OVERLAY_WIDTH_MIN_PX`, `TEXT_OVERLAY_WIDTH_MAX_PX`] (mirrors the edit-panel width slider),
    /// and dispatches the latest-wins background re-render via `dispatch_vector_rerender` (which re-wraps
    /// the text, writes the render_data back onto the overlay so the edit panel re-syncs, and requests a
    /// placement save). Used by the on-canvas width-guide drag handle.
    ///
    /// Returns `true` when the width changed and a re-render was dispatched, `false` when the overlay has
    /// no usable `render_data`/`text_params` or the width was already at the requested value.
    pub(super) fn resize_selected_overlay_width(
        &mut self,
        overlay_idx: usize,
        new_width_px: u32,
        ctx: &egui::Context,
    ) -> bool {
        let clamped = new_width_px.clamp(TEXT_OVERLAY_WIDTH_MIN_PX, TEXT_OVERLAY_WIDTH_MAX_PX);
        let Some(mut render_data) = self
            .overlays
            .get(overlay_idx)
            .and_then(|overlay| overlay.render_data_json.clone())
        else {
            return false;
        };
        let Some(text_params) = render_data
            .get_mut("text_params")
            .and_then(Value::as_object_mut)
        else {
            return false;
        };
        // Read the current width the same way `codec.rs` does (absent -> None) to skip a redundant
        // re-render once the drag pins the value at a clamp bound.
        let current = text_params
            .get("width_px")
            .and_then(value_as_f32)
            .map(|width| width.round().max(1.0) as u32);
        if current == Some(clamped) {
            return false;
        }
        text_params.insert("width_px".to_string(), json!(clamped));
        self.dispatch_vector_rerender(overlay_idx, render_data, ctx)
    }

    pub(super) fn try_scale_selected_overlay_by_shortcuts(&mut self, ui: &mut egui::Ui, page_idx: usize) {
        // Do not hijack typing in any focused text field.
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }

        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        let Some(selected_overlay) = self.overlays.get(selected_idx) else {
            return;
        };
        if selected_overlay.page_idx != page_idx || selected_overlay.deform_mesh.is_some() {
            return;
        }

        let (increase, decrease, reset) = ui.ctx().input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, egui::Key::Equals)
                    || input.consume_key(egui::Modifiers::NONE, egui::Key::Plus)
                    || input.consume_key(egui::Modifiers::SHIFT, egui::Key::Equals),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Minus),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Num0),
            )
        });

        if !increase && !decrease && !reset {
            return;
        }

        let mut changed = false;
        if let Some(overlay) = self.overlays.get_mut(selected_idx) {
            let prev_scale = overlay.user_scale;
            if reset {
                overlay.user_scale = 1.0;
            } else {
                let factor = if increase {
                    1.1
                } else if decrease {
                    1.0 / 1.1
                } else {
                    1.0
                };
                overlay.user_scale = (overlay.user_scale * factor).clamp(0.05, 20.0);
            }
            changed = (overlay.user_scale - prev_scale).abs() > 1e-6;
        }

        if changed {
            self.mark_overlay_geometry_changed(selected_idx, false);
            // EDIT (`-`/`=`/`0` scale): deferred; key repeat marks per frame and writes once on settle.
            self.mark_placement_save_dirty();
            ui.ctx().request_repaint();
        }
    }

    /// Scale the selected raster with the `-` / `=` / `0` keys (parity with the overlay shortcut).
    /// Guards on `selected_raster_page == Some(page_idx)` so, on a multi-page canvas, the keys only
    /// scale the raster on the page that owns the selection (this runs once per visible page).
    pub(super) fn try_scale_selected_raster_by_shortcuts(&mut self, ui: &mut egui::Ui, page_idx: usize) {
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let Some(idx) = self.selected_raster_idx else {
            return;
        };
        if self.selected_raster_page != Some(page_idx) {
            return;
        }
        let (increase, decrease, reset) = ui.ctx().input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, egui::Key::Equals)
                    || input.consume_key(egui::Modifiers::NONE, egui::Key::Plus)
                    || input.consume_key(egui::Modifiers::SHIFT, egui::Key::Equals),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Minus),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Num0),
            )
        });
        if !increase && !decrease && !reset {
            return;
        }
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|v| v.get_mut(idx))
        else {
            return;
        };
        let prev = layer.transform.scale;
        if reset {
            layer.transform.scale = 1.0;
        } else if increase {
            layer.transform.scale = (layer.transform.scale * 1.1).clamp(0.05, 20.0);
        } else if decrease {
            layer.transform.scale = (layer.transform.scale / 1.1).clamp(0.05, 20.0);
        }
        if (layer.transform.scale - prev).abs() <= 1e-6 {
            return;
        }
        let (uid, transform) = (layer.uid.clone(), layer.transform);
        self.persist_raster_transform_deferred(page_idx, &uid, transform);
        ui.ctx().request_repaint();
    }

    /// Ctrl/Cmd+wheel rotation of the selected RASTER layer by ±2° per notch (parity with the text-
    /// overlay Ctrl+wheel rotate `try_rotate_selected_overlay_by_ctrl_wheel`).
    ///
    /// Raster layers (imported rasters, rasterized text, cutouts) have NO vector rotation, so this
    /// ALWAYS applies the ordinary affine `transform.rotation` regardless of the app-wide
    /// `RotationCtrlWheelMode` — it never re-renders glyph outlines (mirrors the overlay handler's
    /// `Raster` branch). The delta is added in RADIANS with the SAME sign and WITHOUT normalization as
    /// the raster mouse drag-rotate (`apply_raster_drag` `Rotate`), so wheel and drag stay consistent.
    ///
    /// No-op unless a raster on `page_idx` is selected AND that selection BELONGS to `page_idx`
    /// (`selected_raster_page`), it is not in perspective transform mode, and Ctrl/Cmd is held with a
    /// non-zero wheel notch. The page guard is essential: `draw_page_overlays` runs once per visible
    /// page and this reads the RAW (summed) wheel delta, so without it one notch would rotate the same
    /// index on every simultaneously-visible page. The `selected_*` selections are mutually exclusive,
    /// so this and the overlay handler never both fire for one event. The change routes through the
    /// shared doc and DEFERS the disk write off the GUI thread via `persist_raster_transform_deferred`.
    pub(super) fn try_rotate_selected_raster_by_ctrl_wheel(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
    ) {
        let Some(idx) = self.selected_raster_idx else {
            return;
        };
        // Only rotate the raster whose selection lives on THIS page (per-page invocation guard).
        if self.selected_raster_page != Some(page_idx) {
            return;
        }
        if self.transform_mode_raster_idx == Some(idx) {
            return;
        }

        let (ctrl_or_command, scroll_delta_y) = ui.ctx().input(|input| {
            (
                input.modifiers.ctrl || input.modifiers.command,
                // Raw wheel: this rotation is Ctrl-gated, and egui zeroes `smooth_scroll_delta`
                // while Ctrl/Cmd is held.
                crate::input_util::raw_wheel_delta(input).y,
            )
        });
        if !ctrl_or_command {
            return;
        }
        let Some(delta_rad) = ctrl_wheel_raster_rotation_step_rad(scroll_delta_y) else {
            return;
        };

        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|v| v.get_mut(idx))
        else {
            return;
        };

        // Zero the wheel delta so the Ctrl-gated raw notch does not also scroll the page (egui
        // already zeroes the smooth delta while Ctrl/Cmd is held, but the raw notch remains).
        ui.ctx().input_mut(|input| {
            input.smooth_scroll_delta = Vec2::ZERO;
        });

        layer.transform.rotation += delta_rad;
        let (uid, transform) = (layer.uid.clone(), layer.transform);
        crate::trace_log!(
            cat::TYPING,
            "ctrl_wheel rotate raster idx={} rotation_rad={}",
            idx,
            transform.rotation
        );
        self.persist_raster_transform_deferred(page_idx, &uid, transform);
        ui.ctx().request_repaint();
    }

    /// Routes one raster's transform to the shared doc (the cross-tab source of truth) and persists
    /// it to the unsaved layers dir so it survives reloads / save-to-project.
    /// Ensures the raster at `raster_idx` has a deform mesh (seeding an identity grid from its current
    /// affine transform when it has none), so entering perspective transform mode has handles to drag.
    /// Returns the resulting mesh (resolution-normalized), or `None` if the raster is absent. Mirrors
    /// `ensure_overlay_deform_mesh`. Pure in-memory on the cached layer; persisted on drag-end.
    pub(super) fn ensure_raster_deform_mesh(
        &mut self,
        page_idx: usize,
        raster_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) -> Option<TypingOverlayDeformMesh> {
        let page_size = page_size_from_image_rect(image_rect, zoom);
        let layer = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|v| v.get_mut(raster_idx))?;
        let mesh = match &layer.deform {
            Some(rec) => {
                let m = TypingOverlayDeformMesh::from_deform_rec(rec, page_size)?;
                normalize_deform_mesh_resolution(&m, page_size)
            }
            None => {
                // Seed an identity grid covering the raster's current affine quad.
                let m = default_deform_mesh_for_page(
                    [layer.transform.cx, layer.transform.cy],
                    layer.image.size,
                    layer.transform.scale,
                    layer.transform.rotation.to_degrees(),
                    page_size,
                );
                layer.deform = Some(crate::models::layer_model::manifest::DeformRec {
                    cols: m.cols,
                    rows: m.rows,
                    points_px: m.points_px.clone(),
                });
                m
            }
        };
        Some(mesh)
    }

    pub(super) fn persist_raster_transform(
        &mut self,
        page_idx: usize,
        uid: &str,
        transform: crate::models::layer_model::manifest::TransformRec,
    ) {
        let Some(dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        // Route the MODEL change to the shared doc: it bumps the doc version (so the PS tab
        // re-projects) and re-projects this tab's page.
        let uid_owned = uid.to_string();
        self.route_to_doc(page_idx, |doc| doc.set_transform(page_idx, &uid_owned, transform));
        // Persist to disk so the transform survives a reload / save-to-project.
        if let Err(err) = crate::models::layer_model::persist::update_raster_transform(
            &dir,
            page_idx,
            uid,
            transform,
            fallback.as_deref(),
        ) {
            crate::runtime_log::log_warn(format!("[typing] persist raster transform: {err}"));
        }
    }

    /// Like [`persist_raster_transform`] but DEFERS the disk write to the shared doc's coalescing
    /// background saver (off the GUI thread) instead of a synchronous manifest rewrite. Used by the
    /// Ctrl+wheel rotation, whose rapid per-notch events must not block the GUI thread with file I/O
    /// per event (CLAUDE.md §5). The in-memory doc is updated LIVE (so the transform is visible
    /// immediately and the PS tab re-projects), and only the disk persist is deferred and coalesced.
    ///
    /// This is the async counterpart of `persist_current_page_rasters`' synchronous `flush_page`: a
    /// transform-only change marks no pixels dirty, so the enqueued job re-encodes no PNGs.
    /// `enqueue_page_save` itself falls back to a synchronous `flush_page` when no saver is enabled.
    pub(super) fn persist_raster_transform_deferred(
        &mut self,
        page_idx: usize,
        uid: &str,
        transform: crate::models::layer_model::manifest::TransformRec,
    ) {
        let Some(dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        // Live in-memory update: bumps the doc version (PS tab re-projects) and re-projects this page.
        let uid_owned = uid.to_string();
        self.route_to_doc(page_idx, |doc| doc.set_transform(page_idx, &uid_owned, transform));
        // Defer the disk write to the doc's coalescing background saver.
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(mut guard) = doc.lock() else {
            return;
        };
        if let Err(err) = guard.enqueue_page_save(page_idx, &dir, fallback.as_deref()) {
            crate::runtime_log::log_warn(format!("[typing] defer raster transform save: {err}"));
        }
    }

    /// Flushes the doc page's RASTER nodes to disk (whole-page `save_page_rasters`), used after a
    /// raster mask-clip toggle (routed through the doc) so the flag survives a reload / save-to-project.
    /// `save_page_rasters` carries each raster's `mask_clip`. No-op if the doc/page is not resident.
    pub(super) fn persist_current_page_rasters(&mut self, page_idx: usize) {
        let Some(primary) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(mut guard) = doc.lock() else {
            return;
        };
        if let Err(err) = guard.flush_page(page_idx, &primary, fallback.as_deref()) {
            crate::runtime_log::log_warn(format!("[typing] persist raster mask-clip: {err}"));
        }
    }

    /// Routes a raster's deform mesh (+ its affine transform) to the shared doc and persists both to
    /// disk. Used by the raster perspective transform mode and by "Сбросить трансформацию" (deform =
    /// None). The doc is the source of truth, so the PS tab re-projects via its version watch.
    pub(super) fn persist_raster_deform(
        &mut self,
        page_idx: usize,
        uid: &str,
        transform: crate::models::layer_model::manifest::TransformRec,
        deform: Option<crate::models::layer_model::manifest::DeformRec>,
    ) {
        let Some(dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        let uid_owned = uid.to_string();
        let deform_for_doc = deform.clone();
        self.route_to_doc(page_idx, |doc| {
            doc.set_transform(page_idx, &uid_owned, transform);
            doc.set_deform(page_idx, &uid_owned, deform_for_doc);
        });
        if let Err(err) = crate::models::layer_model::persist::update_raster_geometry(
            &dir,
            page_idx,
            uid,
            transform,
            deform,
            fallback.as_deref(),
        ) {
            crate::runtime_log::log_warn(format!("[typing] persist raster deform: {err}"));
        }
    }

    /// Like [`persist_raster_deform`] but enqueues the whole-page persistence through the shared
    /// document saver. The saver captures both the affine transform and the full deform mesh, so
    /// rapid keyboard nudges update the live doc immediately without rewriting `layers.json` on the
    /// GUI thread. As with [`persist_raster_transform_deferred`], no saver keeps the existing
    /// synchronous `flush_page` fallback.
    pub(super) fn persist_raster_deform_deferred(
        &mut self,
        page_idx: usize,
        uid: &str,
        transform: crate::models::layer_model::manifest::TransformRec,
        deform: Option<crate::models::layer_model::manifest::DeformRec>,
    ) {
        let Some(dir) = self.layers_primary_dir.clone() else {
            return;
        };
        let fallback = self.layers_fallback_dir.clone();
        let uid_owned = uid.to_string();
        let deform_for_doc = deform.clone();
        self.route_to_doc(page_idx, |doc| {
            doc.set_transform(page_idx, &uid_owned, transform);
            doc.set_deform(page_idx, &uid_owned, deform_for_doc);
        });
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(mut guard) = doc.lock() else {
            return;
        };
        if let Err(err) = guard.enqueue_page_save(page_idx, &dir, fallback.as_deref()) {
            crate::runtime_log::log_warn(format!("[typing] defer raster deform save: {err}"));
        }
    }

    /// Canvas select + move/rotate drag for raster layers (parity with overlays). Runs after the
    /// overlay interaction so overlays win pointer ties; draws the selection decoration. The raster
    /// pixels themselves are drawn in the unified merged-fill pass.
    pub(super) fn interact_page_rasters(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        painter: &egui::Painter,
    ) {
        let count = self
            .raster_layers_by_page
            .get(&page_idx)
            .map_or(0, |v| v.len());
        // Only the page that OWNS the current raster selection may bounds-clear it: this runs once per
        // visible page and the same index exists on every page, so an unguarded clear could drop a
        // selection that is valid on its own page just because another visible page has fewer rasters.
        if self.selected_raster_page == Some(page_idx) {
            if self.selected_raster_idx.is_some_and(|i| i >= count) {
                self.selected_raster_idx = None;
                self.selected_raster_page = None;
            }
            if self.transform_mode_raster_idx.is_some_and(|i| i >= count) {
                self.transform_mode_raster_idx = None;
            }
        }
        if self
            .raster_drag_state
            .as_ref()
            .is_some_and(|s| s.page_idx != page_idx || s.raster_idx >= count)
        {
            self.raster_drag_state = None;
            self.raster_drag_has_changes = false;
        }

        // Drag-end: persist the final geometry (transform, and the mesh for a perspective edit).
        let primary_down = ui.input(|i| i.pointer.primary_down());
        if !primary_down
            && let Some(state) = self.raster_drag_state.take()
        {
            if self.raster_drag_has_changes
                && let Some(layer) = self
                    .raster_layers_by_page
                    .get(&state.page_idx)
                    .and_then(|v| v.get(state.raster_idx))
            {
                let (uid, transform, deform) =
                    (layer.uid.clone(), layer.transform, layer.deform.clone());
                if matches!(state.mode, TypingRasterDragMode::PerspectiveHandle(_)) {
                    self.persist_raster_deform(state.page_idx, &uid, transform, deform);
                } else {
                    self.persist_raster_transform(state.page_idx, &uid, transform);
                }
            }
            self.raster_drag_has_changes = false;
        }
        if count == 0 {
            return;
        }

        // Deferred menu actions (set inside the menu closure, applied after this method).
        let mut menu_enter_transform: Option<usize> = None;
        let mut menu_exit_transform = false;
        let mut menu_reset_transform: Option<usize> = None;
        let mut menu_toggle_mask_clip: Option<usize> = None;
        let mut menu_move_z: Option<(usize, bool)> = None;
        let mut menu_delete: Option<usize> = None;

        // === Perspective transform mode: edit the selected raster's deform mesh corners. ===
        if let Some(sel) = self.transform_mode_raster_idx {
            let mesh = self.ensure_raster_deform_mesh(page_idx, sel, image_rect, zoom);
            let deform = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|v| v.get(sel))
                .and_then(|l| l.deform.clone());
            if let (Some(_), Some(deform)) = (mesh, deform)
                && let Some(corners) = deform_mesh_corners_scene(&deform, image_rect, zoom)
            {
                let pointer = ui.ctx().pointer_latest_pos();
                let interact_rect = egui::Rect::from_points(&corners).expand(
                    TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX * 2.0 + 2.0,
                );
                let resp = ui.interact(
                    interact_rect,
                    egui::Id::new(("typing_raster_xform", page_idx, sel)),
                    egui::Sense::click_and_drag(),
                );
                // Start a corner-handle drag.
                if self.raster_drag_state.is_none()
                    && resp.drag_started()
                    && let Some(p) = pointer
                    && let Some(handle_idx) = hit_test_transform_handle(p, &corners)
                {
                    let page_size = page_size_from_image_rect(image_rect, zoom);
                    let start_mesh =
                        TypingOverlayDeformMesh::from_deform_rec(&deform, page_size);
                    let start_transform = self
                        .raster_layers_by_page
                        .get(&page_idx)
                        .and_then(|v| v.get(sel))
                        .map(|l| l.transform)
                        .unwrap_or(crate::models::layer_model::manifest::TransformRec {
                            cx: 0.0,
                            cy: 0.0,
                            rotation: 0.0,
                            scale: 1.0,
                        });
                    self.raster_drag_state = Some(TypingRasterDragState {
                        page_idx,
                        raster_idx: sel,
                        mode: TypingRasterDragMode::PerspectiveHandle(handle_idx),
                        pointer_start_scene: p,
                        start_transform,
                        start_pointer_angle_rad: 0.0,
                        start_mesh,
                    });
                    self.raster_drag_has_changes = false;
                    self.primary_pointer_targets_overlay_this_frame = true;
                }
                // Continue the corner drag.
                if let Some(state) = self.raster_drag_state.clone()
                    && state.raster_idx == sel
                    && matches!(state.mode, TypingRasterDragMode::PerspectiveHandle(_))
                    && (resp.dragged() || primary_down)
                    && let Some(p) = pointer
                {
                    self.apply_raster_drag(&state, p, image_rect, zoom);
                    self.primary_pointer_targets_overlay_this_frame = true;
                }
                self.raster_context_menu(
                    &resp,
                    page_idx,
                    sel,
                    true,
                    &mut menu_enter_transform,
                    &mut menu_exit_transform,
                    &mut menu_reset_transform,
                    &mut menu_toggle_mask_clip,
                    &mut menu_move_z,
                    &mut menu_delete,
                );

                // Decoration: deformed mesh wireframe outline + corner handles.
                let scene_pts = deform_mesh_scene_points(&deform, image_rect, zoom);
                draw_textured_deform_mesh_wire(painter, &scene_pts, deform.cols, deform.rows);
                draw_perspective_handles(painter, &corners);
            }
            self.apply_raster_menu_actions(
                page_idx,
                image_rect,
                zoom,
                menu_enter_transform,
                menu_exit_transform,
                menu_reset_transform,
                menu_toggle_mask_clip,
                menu_move_z,
                menu_delete,
            );
            return;
        }

        // === Normal mode: move / rotate drag + selection + context menu. ===
        // Scene quads + centers for this page's rasters.
        let entries: Vec<(usize, [Pos2; 4], Pos2)> = (0..count)
            .filter_map(|i| {
                let l = self.raster_layers_by_page.get(&page_idx)?.get(i)?;
                let quad = raster_quad_scene(&l.transform, l.image.size, image_rect, zoom);
                let center = scene_from_page_px(image_rect, zoom, [l.transform.cx, l.transform.cy]);
                Some((i, quad, center))
            })
            .collect();
        let pointer = ui.ctx().pointer_latest_pos();

        // === Unified topmost-at-pointer gate (text vs raster) ===
        // The raster interaction runs AFTER the overlay pass, and egui gives the LATER-registered widget
        // the click — so without this a raster would steal a click that lands on a higher-Z text overlay.
        // Decide the winner by UNIFIED band-Z (same axis as the draw order): if a TEXT overlay is on top
        // at the pointer, claim the click for overlays (`primary_pointer_targets_overlay_this_frame`) so
        // the raster pass below gates out. If a RASTER is on top (text now allowed BELOW a raster), do
        // NOT set the overlay gate, so the raster pass can take it. Skipped during an active drag (the
        // drag owns the pointer) and when an overlay already claimed the click this frame.
        if self.raster_drag_state.is_none() && !self.primary_pointer_targets_overlay_this_frame {
            let topmost_raster_z = topmost_raster_target(&entries, pointer, image_rect, None)
                .and_then(|(idx, _, _, _)| {
                    self.raster_layers_by_page
                        .get(&page_idx)
                        .and_then(|v| v.get(idx))
                        .map(|l| self.raster_band_z(page_idx, &l.uid))
                });
            let topmost_overlay = self.topmost_overlay_at(page_idx, pointer, image_rect, zoom);
            if unified_topmost_pointer_target(topmost_overlay.map(|(_, z)| z), topmost_raster_z)
                == TypingPointerTarget::Overlay
            {
                // A higher-or-equal-Z overlay is on top. Gate the raster pass so it can't steal the
                // click. egui awarded the click to the later-registered raster widget (so the overlay
                // pass's `.clicked()` did NOT fire) — so on a primary click here, SELECT the winning
                // overlay directly, matching the visual top. (Click already routed to the raster by egui,
                // so this is the only place the overlay can claim it.)
                self.primary_pointer_targets_overlay_this_frame = true;
                if let Some((overlay_idx, _)) = topmost_overlay {
                    let primary_clicked = ui.input(|i| i.pointer.primary_clicked());
                    if primary_clicked && self.selected_overlay_idx != Some(overlay_idx) {
                        self.selected_overlay_idx = Some(overlay_idx);
                        self.selected_raster_idx = None;
                        self.selected_raster_page = None;
                        self.transform_mode_raster_idx = None;
                    }
                }
            }
        }

        if let Some(state) = self.raster_drag_state.clone() {
            // Continue an active drag (same Id keeps egui's drag association). This owns the selected
            // raster's `("typing_raster", page_idx, raster_idx)` Id for the frame, so the branches below
            // must NOT also create a resp for it.
            if let Some((_, quad, _)) = entries.iter().find(|(i, _, _)| *i == state.raster_idx) {
                let resp = ui.interact(
                    egui::Rect::from_points(quad),
                    egui::Id::new(("typing_raster", page_idx, state.raster_idx)),
                    egui::Sense::click_and_drag(),
                );
                if (resp.dragged() || primary_down)
                    && let Some(p) = pointer
                {
                    self.apply_raster_drag(&state, p, image_rect, zoom);
                    self.primary_pointer_targets_overlay_this_frame = true;
                }
                // Keep the menu attached to the selected raster's resp even mid-drag, so it persists.
                self.raster_context_menu(
                    &resp,
                    page_idx,
                    state.raster_idx,
                    false,
                    &mut menu_enter_transform,
                    &mut menu_exit_transform,
                    &mut menu_reset_transform,
                    &mut menu_toggle_mask_clip,
                    &mut menu_move_z,
                    &mut menu_delete,
                );
            }
        } else {
            // No active drag. Two independent responses are created (distinct Ids):
            //   (1) the SELECTED raster's resp UNCONDITIONALLY every frame — so its context menu stays
            //       open regardless of pointer position (mirrors transform-mode and text overlays); and
            //   (2) the topmost NON-selected raster under the pointer (a hit-test), so a first
            //       right/left click selects it and opens its menu immediately.
            // Tie gating with overlays is preserved: when an overlay claimed the pointer this frame
            // (`primary_pointer_targets_overlay_this_frame`), we still CREATE the selected raster's resp
            // and attach the menu (so it persists), but we DON'T run its click/drag handling.
            let gated = self.primary_pointer_targets_overlay_this_frame;

            // (1) Selected raster: unconditional resp + menu.
            if let Some(sel) = self.selected_raster_idx
                && let Some((_, sel_quad, sel_center)) =
                    entries.iter().find(|(i, _, _)| *i == sel).copied()
            {
                let resp = ui.interact(
                    egui::Rect::from_points(&sel_quad),
                    egui::Id::new(("typing_raster", page_idx, sel)),
                    egui::Sense::click_and_drag(),
                );
                if !gated {
                    let on_rotate = pointer.is_some_and(|p| {
                        let (_, handle) = rotation_handle_scene_with_corner(&sel_quad, image_rect);
                        p.distance(handle) <= TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 2.0
                    });
                    let over = pointer
                        .is_some_and(|p| point_in_quad(p, &sel_quad) || on_rotate);
                    if over && (resp.clicked() || resp.secondary_clicked()) {
                        // Already selected; just claim the click so the deselect-on-empty doesn't fire.
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                    if over
                        && resp.drag_started()
                        && let Some(p) = pointer
                        && let Some(start_transform) = self
                            .raster_layers_by_page
                            .get(&page_idx)
                            .and_then(|v| v.get(sel))
                            .map(|l| l.transform)
                    {
                        crate::trace_log!(
                            cat::INPUT,
                            "raster_drag_begin owner=selected idx={} selected_was={:?} reason=selected_under_pointer",
                            sel,
                            self.selected_raster_idx
                        );
                        self.raster_drag_state = Some(TypingRasterDragState {
                            page_idx,
                            raster_idx: sel,
                            mode: if on_rotate {
                                TypingRasterDragMode::Rotate
                            } else {
                                TypingRasterDragMode::Move
                            },
                            pointer_start_scene: p,
                            start_transform,
                            start_pointer_angle_rad: pointer_angle_rad(sel_center, p),
                            start_mesh: None,
                        });
                        self.raster_drag_has_changes = false;
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                }
                self.raster_context_menu(
                    &resp,
                    page_idx,
                    sel,
                    false,
                    &mut menu_enter_transform,
                    &mut menu_exit_transform,
                    &mut menu_reset_transform,
                    &mut menu_toggle_mask_clip,
                    &mut menu_move_z,
                    &mut menu_delete,
                );
            }

            // (2) Non-selected rasters: topmost hit-test (skips the selected idx → no duplicate Id).
            if !self.primary_pointer_targets_overlay_this_frame {
                let target = topmost_raster_target(
                    &entries,
                    pointer,
                    image_rect,
                    self.selected_raster_idx,
                );
                if let Some((idx, quad, center, on_rotate)) = target {
                    // Sticky-focus on DRAG: if the pointer is ALSO over the currently-selected raster's
                    // quad, this non-selected widget must NOT capture the drag — egui awards both
                    // `hits.click` and `hits.drag` to the last-registered widget at the pixel (this one),
                    // which would steal the drag from the selected raster (branch 1). So when the selected
                    // raster is under the pointer, register THIS widget as click-only: `hits.drag` then
                    // falls back to branch (1)'s click_and_drag widget (the selected raster), so a drag
                    // moves the SELECTED layer. A click (press-release) still lands here → reselect.
                    let pointer_over_selected = pointer.is_some_and(|p| {
                        self.selected_raster_idx
                            .and_then(|sel| entries.iter().find(|(i, _, _)| *i == sel))
                            .is_some_and(|(_, sel_quad, _)| point_in_quad(p, sel_quad))
                    });
                    let sense = if pointer_over_selected {
                        egui::Sense::click()
                    } else {
                        egui::Sense::click_and_drag()
                    };
                    let resp = ui.interact(
                        egui::Rect::from_points(&quad),
                        egui::Id::new(("typing_raster", page_idx, idx)),
                        sense,
                    );
                    if resp.clicked() {
                        self.select_raster(page_idx, idx);
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                    // Right-click selects the raster (mirror the overlay menu), then opens the menu.
                    if resp.secondary_clicked() {
                        self.select_raster(page_idx, idx);
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                    if resp.drag_started()
                        && let Some(p) = pointer
                        && let Some(start_transform) = self
                            .raster_layers_by_page
                            .get(&page_idx)
                            .and_then(|v| v.get(idx))
                            .map(|l| l.transform)
                    {
                        crate::trace_log!(
                            cat::INPUT,
                            "raster_drag_begin owner=reselect idx={} selected_was={:?} reason=no_selected_under_pointer",
                            idx,
                            self.selected_raster_idx
                        );
                        self.select_raster(page_idx, idx);
                        self.raster_drag_state = Some(TypingRasterDragState {
                            page_idx,
                            raster_idx: idx,
                            mode: if on_rotate {
                                TypingRasterDragMode::Rotate
                            } else {
                                TypingRasterDragMode::Move
                            },
                            pointer_start_scene: p,
                            start_transform,
                            start_pointer_angle_rad: pointer_angle_rad(center, p),
                            start_mesh: None,
                        });
                        self.raster_drag_has_changes = false;
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                    self.raster_context_menu(
                        &resp,
                        page_idx,
                        idx,
                        false,
                        &mut menu_enter_transform,
                        &mut menu_exit_transform,
                        &mut menu_reset_transform,
                        &mut menu_toggle_mask_clip,
                        &mut menu_move_z,
                        &mut menu_delete,
                    );
                }
            }
        }

        // Deselect when clicking empty image area (no raster and no overlay targeted this frame).
        if self.selected_raster_idx.is_some()
            && self.raster_drag_state.is_none()
            && !self.primary_pointer_targets_overlay_this_frame
        {
            let clicked_empty = ui.input(|i| {
                i.pointer.primary_clicked()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|p| image_rect.contains(p))
            }) && !crate::input_util::pointer_over_floating_area(ui.ctx());
            if clicked_empty {
                self.selected_raster_idx = None;
                self.selected_raster_page = None;
                self.transform_mode_raster_idx = None;
            }
        }

        // Selection decoration (dashed boundary + rotate handle).
        if let Some(sel) = self.selected_raster_idx
            && let Some((_, quad, _)) = entries.iter().find(|(i, _, _)| *i == sel)
        {
            let path = [quad[0], quad[1], quad[2], quad[3], quad[0]];
            draw_dashed_selection_path(painter, &path);
            draw_rotation_handle(painter, quad, image_rect);
        }

        self.apply_raster_menu_actions(
            page_idx,
            image_rect,
            zoom,
            menu_enter_transform,
            menu_exit_transform,
            menu_reset_transform,
            menu_toggle_mask_clip,
            menu_move_z,
            menu_delete,
        );
    }

    /// Attaches the raster context menu to `resp`, recording chosen actions into the deferred `out_*`
    /// slots (applied after the closure by `apply_raster_menu_actions`, avoiding mid-closure mutation).
    /// `is_transform_mode` toggles the enter/exit/reset items. Mirrors the text-overlay canvas menu.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn raster_context_menu(
        &self,
        resp: &egui::Response,
        _page_idx: usize,
        idx: usize,
        is_transform_mode: bool,
        out_enter_transform: &mut Option<usize>,
        out_exit_transform: &mut bool,
        out_reset_transform: &mut Option<usize>,
        out_toggle_mask_clip: &mut Option<usize>,
        out_move_z: &mut Option<(usize, bool)>,
        out_delete: &mut Option<usize>,
    ) {
        let mask_clip_on = self
            .raster_layers_by_page
            .get(&_page_idx)
            .and_then(|v| v.get(idx))
            .map(|l| l.mask_clip_enabled)
            .unwrap_or(false);
        resp.context_menu(|menu_ui| {
            if self.selected_raster_idx != Some(idx) {
                menu_ui.label(t!("typing.context_menu.select_layer_hint"));
                return;
            }
            if !is_transform_mode {
                if menu_ui.button(t!("typing.context_menu.enter_transform_mode")).clicked() {
                    *out_enter_transform = Some(idx);
                    menu_ui.close();
                }
            } else {
                if menu_ui.button(t!("typing.context_menu.exit_transform_mode")).clicked() {
                    *out_exit_transform = true;
                    menu_ui.close();
                }
                if menu_ui.button(t!("typing.context_menu.reset_transform")).clicked() {
                    *out_reset_transform = Some(idx);
                    menu_ui.close();
                }
            }
            menu_ui.separator();
            let toggle_label = if mask_clip_on {
                t!("typing.context_menu.disable_mask_clip")
            } else {
                t!("typing.context_menu.enable_mask_clip")
            };
            if menu_ui.button(toggle_label).clicked() {
                *out_toggle_mask_clip = Some(idx);
                menu_ui.close();
            }
            menu_ui.separator();
            menu_ui.horizontal(|row| {
                row.label(t!("typing.context_menu.order"));
                if row.button("▲").clicked() {
                    *out_move_z = Some((idx, true));
                }
                if row.button("▼").clicked() {
                    *out_move_z = Some((idx, false));
                }
            });
            menu_ui.separator();
            if menu_ui.button(t!("typing.context_menu.delete_layer")).clicked() {
                *out_delete = Some(idx);
                menu_ui.close();
            }
        });
    }

    /// Applies the deferred raster context-menu actions captured by `raster_context_menu`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_raster_menu_actions(
        &mut self,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        enter_transform: Option<usize>,
        exit_transform: bool,
        reset_transform: Option<usize>,
        toggle_mask_clip: Option<usize>,
        move_z: Option<(usize, bool)>,
        delete: Option<usize>,
    ) {
        if let Some(idx) = enter_transform {
            // Seed the mesh (if absent) and enter perspective transform mode.
            if self.ensure_raster_deform_mesh(page_idx, idx, image_rect, zoom).is_some() {
                self.transform_mode_raster_idx = Some(idx);
                self.deform_mode = TypingDeformMode::Perspective;
                self.raster_drag_state = None;
                self.raster_drag_has_changes = false;
                // Persist the seeded mesh so it survives without a drag.
                if let Some(layer) = self
                    .raster_layers_by_page
                    .get(&page_idx)
                    .and_then(|v| v.get(idx))
                {
                    let (uid, transform, deform) =
                        (layer.uid.clone(), layer.transform, layer.deform.clone());
                    self.persist_raster_deform(page_idx, &uid, transform, deform);
                }
            }
        }
        if exit_transform {
            self.transform_mode_raster_idx = None;
            self.raster_drag_state = None;
            self.raster_drag_has_changes = false;
        }
        if let Some(idx) = reset_transform {
            // Clear the deform (back to plain affine), persist, exit transform mode.
            if let Some(layer) = self
                .raster_layers_by_page
                .get_mut(&page_idx)
                .and_then(|v| v.get_mut(idx))
            {
                layer.deform = None;
            }
            if let Some(layer) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|v| v.get(idx))
            {
                let (uid, transform) = (layer.uid.clone(), layer.transform);
                self.persist_raster_deform(page_idx, &uid, transform, None);
            }
            self.transform_mode_raster_idx = None;
            self.raster_drag_state = None;
            self.raster_drag_has_changes = false;
        }
        if let Some(idx) = toggle_mask_clip
            && let Some(layer) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|v| v.get(idx))
        {
            let uid = layer.uid.clone();
            let new_val = !layer.mask_clip_enabled;
            // Route through the doc (source of truth): bumps generation → re-clip + re-upload, and
            // bumps the doc version → the PS tab re-projects.
            self.route_to_doc(page_idx, |doc| {
                doc.set_raster_mask_clip(page_idx, &uid, Some(new_val));
            });
            // Persist so it survives a reload / save-to-project (whole-page raster save preserves it).
            self.persist_current_page_rasters(page_idx);
        }
        if let Some((idx, up)) = move_z {
            self.move_raster_in_unified_z(page_idx, idx, up);
        }
        if let Some(idx) = delete {
            self.remove_raster(page_idx, idx);
        }
    }

    /// Applies an in-progress raster drag (move or rotate) to the cached transform.
    pub(super) fn apply_raster_drag(
        &mut self,
        state: &TypingRasterDragState,
        pointer: Pos2,
        image_rect: Rect,
        zoom: f32,
    ) {
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&state.page_idx)
            .and_then(|v| v.get_mut(state.raster_idx))
        else {
            return;
        };
        match state.mode {
            TypingRasterDragMode::Move => {
                let z = zoom.max(f32::EPSILON);
                layer.transform.cx =
                    state.start_transform.cx + (pointer.x - state.pointer_start_scene.x) / z;
                layer.transform.cy =
                    state.start_transform.cy + (pointer.y - state.pointer_start_scene.y) / z;
            }
            TypingRasterDragMode::Rotate => {
                let center = scene_from_page_px(
                    image_rect,
                    zoom,
                    [state.start_transform.cx, state.start_transform.cy],
                );
                let cur = pointer_angle_rad(center, pointer);
                layer.transform.rotation =
                    state.start_transform.rotation + (cur - state.start_pointer_angle_rad);
            }
            TypingRasterDragMode::PerspectiveHandle(handle_idx) => {
                let Some(start_mesh) = &state.start_mesh else {
                    return;
                };
                let page_size = page_size_from_image_rect(image_rect, zoom);
                let z = zoom.max(f32::EPSILON);
                // Pointer delta in page px (scene → page).
                let delta_page_px = [
                    (pointer.x - state.pointer_start_scene.x) / z,
                    (pointer.y - state.pointer_start_scene.y) / z,
                ];
                let mesh = apply_perspective_corner_drag(
                    start_mesh,
                    handle_idx,
                    delta_page_px,
                    page_size,
                );
                layer.deform = Some(crate::models::layer_model::manifest::DeformRec {
                    cols: mesh.cols,
                    rows: mesh.rows,
                    points_px: mesh.points_px.clone(),
                });
            }
        }
        self.raster_drag_has_changes = true;
    }

    pub(super) fn try_move_selected_overlay_by_arrow_shortcuts(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        panel_text_input_focused: bool,
        strict_pixel_movement: bool,
    ) {
        if panel_text_input_focused {
            return;
        }

        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        let Some(selected_overlay) = self.overlays.get(selected_idx) else {
            return;
        };
        if selected_overlay.page_idx != page_idx {
            return;
        }

        let (left_1, right_1, up_1, down_1, left_5, right_5, up_5, down_5) =
            ui.ctx().input_mut(|input| {
                (
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowLeft),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowRight),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowUp),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowDown),
                )
            });

        let delta_x_px = (right_1 as i32 - left_1 as i32) + (right_5 as i32 - left_5 as i32) * 5;
        let delta_y_px = (down_1 as i32 - up_1 as i32) + (down_5 as i32 - up_5 as i32) * 5;
        if delta_x_px == 0 && delta_y_px == 0 {
            return;
        }

        let page_delta = [delta_x_px as f32, delta_y_px as f32];
        let page_size = page_size_from_image_rect(image_rect, zoom);
        if let Some(overlay) = self.overlays.get_mut(selected_idx) {
            if let Some(mesh) = overlay.deform_mesh.as_mut() {
                mesh.translate(page_delta[0], page_delta[1], page_size);
                sync_overlay_center_from_deform_mesh(overlay, page_size);
            } else {
                overlay.center_page_px = clamp_page_point(
                    [
                        overlay.center_page_px[0] + page_delta[0],
                        overlay.center_page_px[1] + page_delta[1],
                    ],
                    page_size,
                );
            }
            snap_overlay_center_to_pixels_if_enabled(overlay, strict_pixel_movement, page_size);
        }

        let _ = self.enforce_overlay_visibility_limit(
            selected_idx,
            image_rect,
            zoom,
            strict_pixel_movement,
        );
        // EDIT (arrow-key nudge): deferred; held arrows mark every frame and write once on settle.
        self.mark_placement_save_dirty();
        ui.ctx().request_repaint();
    }

    /// Nudges the selected RASTER layer by whole page pixels with the arrow keys (parity with the
    /// overlay nudge `try_move_selected_overlay_by_arrow_shortcuts`). SHIFT moves by 5 px. Mirrors the
    /// raster mouse-drag Move path: a perspective-deformed raster translates its mesh, otherwise the
    /// affine `transform.cx/cy` move (clamped to the page, snapped to whole pixels when
    /// `strict_pixel_movement`). The change is routed to the shared doc and enqueued for persistence,
    /// so OS key-repeat never performs manifest I/O on the GUI thread.
    ///
    /// Gated on `selected_raster_idx` (mutually exclusive with `selected_overlay_idx`, so it only
    /// consumes the arrow keys when a raster is selected; the overlay nudge, called first, returns
    /// before consuming keys when no overlay is selected) AND on `selected_raster_page == Some(page_idx)`
    /// so, since this runs once per visible page, only the raster on the owning page is nudged.
    pub(super) fn try_move_selected_raster_by_arrow_shortcuts(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        panel_text_input_focused: bool,
        strict_pixel_movement: bool,
    ) {
        if panel_text_input_focused {
            return;
        }

        let Some(selected_idx) = self.selected_raster_idx else {
            return;
        };
        // Only nudge the raster on the page that OWNS the selection (this runs once per visible page).
        if self.selected_raster_page != Some(page_idx) {
            return;
        }
        let has_layer = self
            .raster_layers_by_page
            .get(&page_idx)
            .is_some_and(|v| selected_idx < v.len());
        if !has_layer {
            return;
        }

        let (left_1, right_1, up_1, down_1, left_5, right_5, up_5, down_5) =
            ui.ctx().input_mut(|input| {
                (
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowLeft),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowRight),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowUp),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowDown),
                )
            });

        let delta_x_px = (right_1 as i32 - left_1 as i32) + (right_5 as i32 - left_5 as i32) * 5;
        let delta_y_px = (down_1 as i32 - up_1 as i32) + (down_5 as i32 - up_5 as i32) * 5;
        if delta_x_px == 0 && delta_y_px == 0 {
            return;
        }

        let page_delta = [delta_x_px as f32, delta_y_px as f32];
        let page_size = page_size_from_image_rect(image_rect, zoom);
        let Some(layer) = self
            .raster_layers_by_page
            .get_mut(&page_idx)
            .and_then(|v| v.get_mut(selected_idx))
        else {
            return;
        };

        // A perspective-deformed raster (mesh present) renders from its mesh points, so translate the
        // mesh; the plain affine raster moves its center. Matches the mouse-drag Move path.
        if let Some(rec) = layer.deform.as_ref() {
            let Some(mut mesh) = TypingOverlayDeformMesh::from_deform_rec(rec, page_size) else {
                return;
            };
            mesh.translate(page_delta[0], page_delta[1], page_size);
            layer.deform = Some(crate::models::layer_model::manifest::DeformRec {
                cols: mesh.cols,
                rows: mesh.rows,
                points_px: mesh.points_px.clone(),
            });
            let (uid, transform, deform) =
                (layer.uid.clone(), layer.transform, layer.deform.clone());
            self.persist_raster_deform_deferred(page_idx, &uid, transform, deform);
        } else {
            let mut center = clamp_page_point(
                [
                    layer.transform.cx + page_delta[0],
                    layer.transform.cy + page_delta[1],
                ],
                page_size,
            );
            if strict_pixel_movement {
                center = clamp_page_point([center[0].round(), center[1].round()], page_size);
            }
            layer.transform.cx = center[0];
            layer.transform.cy = center[1];
            let (uid, transform) = (layer.uid.clone(), layer.transform);
            self.persist_raster_transform_deferred(page_idx, &uid, transform);
        }
        ui.ctx().request_repaint();
    }
}
