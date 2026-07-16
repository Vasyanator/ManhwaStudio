/*
File: tab/draw_page.rs

Purpose:
Per-page overlay drawing and interaction for the typing tab. Hosts the large
`draw_page_overlays` method that renders and drives egui interaction for the
text/image overlays and interleaved read-only raster layers on a single page,
plus the small helpers it relies on for repaint gating, pixel snapping, on-screen
visibility clamping, and vertical drag page transitions.

Notes:
Extracted verbatim from `tab.rs`. Methods are `pub(super)` so `tab.rs` and sibling
submodules of `tab` can use them. `use super::*;` pulls in the parent module's
types and imports. Struct/enum definitions and the other `impl` blocks on
`TypingTextOverlayLayer` remain in `tab.rs`; these methods reach the private items
that stay there as descendants of module `tab`.
*/

use super::*;

impl TypingTextOverlayLayer {


    // All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_page_overlays(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        mask_panel_open: bool,
        panel_text_input_focused: bool,
        eyedropper_blocks_focus_clear: bool,
        auto_typing_settings: TypingAutoTypingSettings,
        strict_pixel_movement: bool,
    ) -> Vec<[Pos2; 4]> {
        if self
            .selected_overlay_idx
            .is_some_and(|idx| idx >= self.overlays.len())
        {
            self.selected_overlay_idx = None;
        }
        if self
            .transform_mode_overlay_idx
            .is_some_and(|idx| idx >= self.overlays.len())
        {
            self.transform_mode_overlay_idx = None;
        }
        // Whenever transform mode is not active, drop the transient VECTOR working state (mesh + drag)
        // and reset the mode kind. Covers click-away/exit/loader-reset uniformly.
        if self.transform_mode_overlay_idx.is_none()
            && (self.transform_mode_kind != TypingTransformModeKind::Raster
                || self.vector_transform_mesh.is_some()
                || self.vector_transform_drag.is_some()
                || self.vector_transform_base.is_some()
                || self.vector_transform_base_rx.is_some())
        {
            self.exit_vector_transform_mode();
        }
        if self
            .drag_state
            .as_ref()
            .is_some_and(|state| state.overlay_idx >= self.overlays.len())
        {
            self.drag_state = None;
            self.drag_has_changes = false;
        }
        if self
            .width_resize_drag
            .is_some_and(|state| state.overlay_idx >= self.overlays.len())
        {
            self.width_resize_drag = None;
        }
        // One selection at a time across the two layer kinds: an overlay selection wins (overlay
        // interaction runs before the raster pass below; `select_raster` clears overlays directly).
        if self.selected_overlay_idx.is_some() {
            self.selected_raster_idx = None;
            self.selected_raster_page = None;
            self.transform_mode_raster_idx = None;
        }
        if mask_panel_open {
            if let Some(selected_idx) = self.selected_overlay_idx {
                let should_validate = self
                    .overlays
                    .get(selected_idx)
                    .is_some_and(|overlay| overlay.page_idx == page_idx);
                if should_validate
                    && self.enforce_overlay_visibility_limit(
                        selected_idx,
                        image_rect,
                        zoom,
                        strict_pixel_movement,
                    )
                {
                    self.mark_overlay_geometry_changed(selected_idx, false);
                    // EDIT (visibility-limit clamp): deferred.
                    self.mark_placement_save_dirty();
                }
            }
            self.clear_selection();
        }

        if !ui.input(|i| i.pointer.primary_down()) {
            if self.drag_state.is_some() && self.drag_has_changes {
                if let Some(state) = self.drag_state.as_ref() {
                    self.flush_overlay_texture_if_stale(state.overlay_idx);
                }
                // EDIT (move/rotate drag released off-widget): deferred.
                self.mark_placement_save_dirty();
            }
            self.drag_state = None;
            self.drag_has_changes = false;
            // A width-resize drag persists per-frame (each `resize_selected_overlay_width` dispatches a
            // re-render + placement save), so on button-up there is nothing to commit — just clear it so
            // it can't get stuck if the widget's `drag_stopped` was never observed.
            self.width_resize_drag = None;
            // Settle a vector-transform drag whose pointer was released off-widget (the in-method
            // `drag_stopped` path handles the normal on-widget release; whichever fires first wins via
            // the `take`).
            if let Some(drag) = self.vector_transform_drag.take()
                && drag.has_changes
            {
                self.settle_vector_transform(drag.overlay_idx, ctx);
            }
        }

        let clip_rect = ui.clip_rect().intersect(image_rect);
        if self.poll_auto_typing_job(ctx) {
            ctx.request_repaint();
        }
        if !clip_rect.is_positive() {
            return Vec::new();
        }
        // Ensure the read-only PS raster layers and unified Z bands for this page are loaded; the
        // actual raster quads are now drawn interleaved with the text overlays (one ordered pass
        // below) so a raster moved above a text group in the PS editor renders on top.
        self.ensure_raster_layers_for_page(page_idx);
        let layout_editor_active = self.layout_editor.is_some();
        if !mask_panel_open && !layout_editor_active {
            self.try_trigger_selected_overlay_auto_typing_by_hotkey(
                ctx,
                page_idx,
                image_rect,
                zoom,
                panel_text_input_focused,
                auto_typing_settings,
            );
            self.try_rotate_selected_overlay_by_ctrl_wheel(ui, page_idx, image_rect, zoom);
            self.try_rotate_selected_raster_by_ctrl_wheel(ui, page_idx);
            self.try_scale_selected_overlay_by_shortcuts(ui, page_idx);
            self.try_scale_selected_raster_by_shortcuts(ui, page_idx);
            self.try_move_selected_overlay_by_arrow_shortcuts(
                ui,
                page_idx,
                image_rect,
                zoom,
                panel_text_input_focused,
                strict_pixel_movement,
            );
            self.try_move_selected_raster_by_arrow_shortcuts(
                ui,
                page_idx,
                image_rect,
                zoom,
                panel_text_input_focused,
                strict_pixel_movement,
            );
        }
        let mut adjusted_by_visibility_limit = false;
        for idx in 0..self.overlays.len() {
            let Some(overlay) = self.overlays.get(idx) else {
                continue;
            };
            if overlay.page_idx != page_idx {
                continue;
            }
            if self
                .drag_state
                .as_ref()
                .is_some_and(|state| state.overlay_idx == idx && state.page_idx == page_idx)
            {
                continue;
            }
            if self.enforce_overlay_visibility_limit(idx, image_rect, zoom, strict_pixel_movement) {
                self.mark_overlay_geometry_changed(idx, false);
                adjusted_by_visibility_limit = true;
            }
        }
        if adjusted_by_visibility_limit {
            // EDIT (per-frame visibility-limit clamp of every overlay on the page): deferred. This ran
            // on any frame a clamp moved a layer, e.g. throughout a zoom gesture.
            self.mark_placement_save_dirty();
        }
        let painter = ui.painter().with_clip_rect(clip_rect);
        let mut needs_texture_upload = Vec::new();
        for (idx, overlay) in self.overlays.iter().enumerate() {
            if overlay.page_idx == page_idx
                && (overlay.texture.is_none() || overlay.display_texture_stale)
            {
                needs_texture_upload.push(idx);
            }
        }
        for idx in needs_texture_upload {
            self.queue_overlay_texture_upload(idx);
        }
        if !self.pending_upload_indices.is_empty() {
            ctx.request_repaint();
        }

        struct OverlayDrawEntry {
            idx: usize,
            bounds_rect: Rect,
            selection_bounds_rect: Rect,
            quad_scene: [Pos2; 4],
            mesh_scene: Vec<Pos2>,
            selection_mesh_scene: Vec<Pos2>,
            mesh_cols: usize,
            mesh_rows: usize,
            occluder_quads: Vec<[Pos2; 4]>,
            texture: egui::TextureHandle,
            render_width_px: Option<u32>,
        }

        let mut draw_entries: Vec<OverlayDrawEntry> = Vec::new();
        let current_frame = ui.ctx().cumulative_frame_nr();
        for idx in 0..self.overlays.len() {
            let Some(overlay) = self.overlays.get(idx) else {
                continue;
            };
            if overlay.page_idx != page_idx || overlay.texture.is_none() {
                continue;
            }
            if self.layout_editor.as_ref().is_some_and(|editor| {
                editor.mode == TypingLayoutEditorMode::Editing
                    && editor.overlay_idx == idx
                    && editor.page_idx == page_idx
            }) {
                continue;
            }
            let geometry = overlay_scene_geometry(overlay, image_rect, zoom);
            if geometry.bounds_rect.width() <= 0.5 || geometry.bounds_rect.height() <= 0.5 {
                continue;
            }
            if !geometry.bounds_rect.intersects(clip_rect) {
                continue;
            }
            if let Some(overlay) = self.overlays.get_mut(idx) {
                overlay.last_texture_used_frame = current_frame;
            }
            let Some(overlay) = self.overlays.get(idx) else {
                continue;
            };
            let is_selected_text =
                self.selected_overlay_idx == Some(idx) && overlay.kind == TypingOverlayKind::Text;
            let render_width_px = if overlay.kind == TypingOverlayKind::Text {
                overlay.render_data_json.as_ref().map(|render_data| {
                    overlay_render_data_width_hint(
                        Some(render_data),
                        u32::try_from(overlay.size_px[0]).unwrap_or(u32::MAX),
                    )
                })
            } else {
                None
            };
            let selection_mesh_scene = if is_selected_text {
                expand_selection_mesh_to_min_screen_side(
                    &geometry.mesh_scene,
                    geometry.mesh_cols,
                    geometry.mesh_rows,
                )
            } else {
                geometry.mesh_scene.clone()
            };
            let selection_bounds_rect = if is_selected_text {
                deform_mesh_bounds(&selection_mesh_scene)
            } else {
                geometry.bounds_rect
            };
            draw_entries.push(OverlayDrawEntry {
                idx,
                bounds_rect: geometry.bounds_rect,
                selection_bounds_rect,
                quad_scene: geometry.quad_scene,
                occluder_quads: build_mesh_occluder_quads(
                    &geometry.mesh_scene,
                    geometry.mesh_cols,
                    geometry.mesh_rows,
                ),
                mesh_scene: geometry.mesh_scene,
                selection_mesh_scene,
                mesh_cols: geometry.mesh_cols,
                mesh_rows: geometry.mesh_rows,
                texture: overlay.texture.as_ref().expect("checked above").clone(),
                render_width_px,
            });
        }

        // Bottom-to-top by the UNIFIED manual band-Z (retire the old layer_idx + page-Y auto-order):
        // the top overlay draws last (on top) AND registers its egui interaction last, so on an overlap
        // the topmost-by-Z overlay wins the click — the same Z the raster/text unified hit-test and the
        // `merged_fills` draw order use, so draw order == manual order == click order.
        draw_entries.sort_by(|a, b| {
            let z = |idx: usize| {
                self.overlays
                    .get(idx)
                    .map(|o| self.overlay_band_z(page_idx, &o.uid, o.layer_idx))
                    .unwrap_or(0)
            };
            z(a.idx).cmp(&z(b.idx))
        });

        if !draw_entries.is_empty() && !mask_panel_open && !layout_editor_active {
            let mut clicked_overlay_idx: Option<usize> = None;
            let mut pending_delete_overlay_idx: Option<usize> = None;
            let mut pending_enter_layout_editor_idx: Option<usize> = None;
            let popup_open_before = ui.ctx().any_popup_open();
            // Sticky-фокус: если клик пришёлся внутрь рамки уже выделенного оверлея,
            // фокус остаётся на нём, даже если сверху лежит перекрывающий оверлей или
            // растровый слой. Считаем это один раз по позиции клика и по grab-мешу
            // выделенного оверлея (та же область, что и `pointer_inside_grab_area`).
            let click_in_selected_frame = ui
                .input(|i| i.pointer.primary_clicked())
                .then(|| ui.input(|i| i.pointer.interact_pos()))
                .flatten()
                .zip(self.selected_overlay_idx)
                .is_some_and(|(pos, selected_idx)| {
                    draw_entries.iter().any(|entry| {
                        entry.idx == selected_idx
                            && deform_mesh_contains_point(
                                &entry.selection_mesh_scene,
                                entry.mesh_cols,
                                entry.mesh_rows,
                                pos,
                            )
                    })
                });
            // Sticky-фокус на ПЕРЕТАСКИВАНИИ (по позиции курсора, без клика): курсор находится
            // внутри grab-рамки уже выделенного оверлея. Тогда перекрывающий НЕвыделенный оверлей
            // регистрируется как click-only (см. ниже), и egui отдаёт drag выделенному оверлею.
            let pointer_in_selected_overlay_frame = ui
                .input(|i| i.pointer.latest_pos())
                .zip(self.selected_overlay_idx)
                .is_some_and(|(pos, selected_idx)| {
                    draw_entries.iter().any(|entry| {
                        entry.idx == selected_idx
                            && deform_mesh_contains_point(
                                &entry.selection_mesh_scene,
                                entry.mesh_cols,
                                entry.mesh_rows,
                                pos,
                            )
                    })
                });
            for entry in &draw_entries {
                // VECTOR transform mode owns this overlay's interaction in a dedicated pass
                // (`draw_vector_transform_overlay`), so skip the normal select/move/raster-deform
                // interaction here — the two transform kinds never share drag state.
                if self.transform_mode_overlay_idx == Some(entry.idx)
                    && self.transform_mode_kind == TypingTransformModeKind::Vector
                {
                    continue;
                }
                let is_transform_mode = self.transform_mode_overlay_idx == Some(entry.idx)
                    && self.transform_mode_kind == TypingTransformModeKind::Raster;
                let show_rotate_handle =
                    self.selected_overlay_idx == Some(entry.idx) && !is_transform_mode;
                let rotate_handle_pos = if show_rotate_handle {
                    Some(rotation_handle_scene(&entry.quad_scene, image_rect))
                } else {
                    None
                };
                // Width-guide end ticks are draggable to resize the layer's configured width. Shown only
                // when `show_rotate_handle` (selected overlay, outside raster-transform mode), and
                // `entry.render_width_px` is `Some` only for a TEXT overlay, so together this is
                // text-only. The tick positions mirror `draw_text_overlay_width_guide` exactly.
                let width_guide_ticks = if show_rotate_handle {
                    entry.render_width_px.and_then(|render_width_px| {
                        let overlay = self.overlays.get(entry.idx)?;
                        // Rotation-invariant source->screen scale (see `draw_text_overlay_width_guide`).
                        let display_scale = zoom * overlay.user_scale.max(0.01);
                        let half_width =
                            (render_width_px.max(1) as f32 * display_scale).max(1.0) * 0.5;
                        let center_x = entry.selection_bounds_rect.center().x;
                        let line_y =
                            entry.selection_bounds_rect.top() - TEXT_OVERLAY_WIDTH_GUIDE_GAP_PX;
                        Some((
                            Pos2::new(center_x - half_width, line_y),
                            Pos2::new(center_x + half_width, line_y),
                        ))
                    })
                } else {
                    None
                };
                let mut interact_rect = if is_transform_mode {
                    entry
                        .bounds_rect
                        .expand(TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX * 2.0 + 2.0)
                } else if self.selected_overlay_idx == Some(entry.idx) {
                    entry.selection_bounds_rect
                } else {
                    entry.bounds_rect
                };
                if let Some(handle_pos) = rotate_handle_pos {
                    let handle_rect = Rect::from_center_size(
                        handle_pos,
                        Vec2::splat(TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 4.0),
                    );
                    interact_rect = interact_rect.union(handle_rect);
                }
                if let Some((left_tick, right_tick)) = width_guide_ticks {
                    let tick_size = Vec2::splat(TEXT_OVERLAY_WIDTH_GUIDE_HANDLE_RADIUS_PX * 2.0);
                    interact_rect = interact_rect
                        .union(Rect::from_center_size(left_tick, tick_size))
                        .union(Rect::from_center_size(right_tick, tick_size));
                }
                // Если курсор внутри рамки уже выделенного оверлея, перекрывающий НЕвыделенный
                // оверлей не должен перехватывать DRAG: регистрируем его click-only, чтобы egui
                // отдал drag выделенному оверлею (его виджет sense'ит click_and_drag). Клик
                // (нажал-отпустил) по-прежнему попадает сюда и переселектит — см. блок
                // sticky-фокуса по `click_in_selected_frame`.
                // Тот же приём для ВЕКТОРНОЙ трансформации: её ручки живут в отдельном пассе
                // (`draw_vector_transform_overlay`, регистрируется ПОСЛЕ этого цикла), а ручки по
                // углам сетки лежат вне полигона оверлея, поэтому `pointer_in_selected_overlay_frame`
                // там ложно false. egui НЕ отдаёт drag позже зарегистрированному перекрывающему
                // виджету, если более ранний тоже sense'ит drag, — поэтому пока активна векторная
                // трансформация, все ОСТАЛЬНЫЕ оверлеи (сам трансформируемый уже пропущен `continue`
                // выше) регистрируем click-only, чтобы drag достался виджету ручек.
                let vector_transform_active = self.transform_mode_overlay_idx.is_some()
                    && self.transform_mode_kind == TypingTransformModeKind::Vector;
                let sense = if vector_transform_active
                    || (pointer_in_selected_overlay_frame
                        && self.selected_overlay_idx != Some(entry.idx))
                {
                    Sense::click()
                } else {
                    Sense::click_and_drag()
                };
                let response = ui.interact(
                    interact_rect,
                    Id::new(("typing_text_overlay", entry.idx)),
                    sense,
                );
                let pointer_pos = response.interact_pointer_pos();
                let pointer_inside_visual = pointer_pos.is_some_and(|pos| {
                    deform_mesh_contains_point(
                        &entry.mesh_scene,
                        entry.mesh_cols,
                        entry.mesh_rows,
                        pos,
                    )
                });
                let pointer_inside_grab_area = pointer_pos.is_some_and(|pos| {
                    let hit_mesh = if self.selected_overlay_idx == Some(entry.idx) {
                        &entry.selection_mesh_scene
                    } else {
                        &entry.mesh_scene
                    };
                    deform_mesh_contains_point(hit_mesh, entry.mesh_cols, entry.mesh_rows, pos)
                });
                let pointer_on_handle = pointer_pos.and_then(|pos| {
                    if !is_transform_mode || !self.deform_mode.is_handle_mode() {
                        return None;
                    }
                    match self.deform_mode {
                        TypingDeformMode::Perspective => {
                            hit_test_transform_handle(pos, &entry.quad_scene)
                        }
                        TypingDeformMode::Bend => hit_test_bend_handle(
                            pos,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                        ),
                        TypingDeformMode::Frame => hit_test_frame_handle(
                            pos,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                            self.frame_handle_side_points,
                        ),
                        TypingDeformMode::Grid => hit_test_grid_handle(
                            pos,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                            self.frame_handle_side_points,
                        ),
                        _ => None,
                    }
                });
                let pointer_on_rotate_handle =
                    pointer_pos
                        .zip(rotate_handle_pos)
                        .is_some_and(|(pointer, handle)| {
                            pointer.distance(handle) <= TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 2.0
                        });
                // `Some(true)` -> right tick hit, `Some(false)` -> left tick, `None` -> neither.
                let pointer_on_width_handle = pointer_pos.zip(width_guide_ticks).and_then(
                    |(pointer, (left_tick, right_tick))| {
                        let radius = TEXT_OVERLAY_WIDTH_GUIDE_HANDLE_RADIUS_PX;
                        if pointer.distance(right_tick) <= radius {
                            Some(true)
                        } else if pointer.distance(left_tick) <= radius {
                            Some(false)
                        } else {
                            None
                        }
                    },
                );
                let pointer_targets_overlay = pointer_inside_grab_area
                    || pointer_on_handle.is_some()
                    || pointer_on_rotate_handle
                    || pointer_on_width_handle.is_some();
                // Show the horizontal-resize cursor while HOVERING a width tick, or while actively
                // dragging one. Hover uses `response.hover_pos()` (set on plain mouse-over), NOT
                // `interact_pointer_pos()` above (which is `Some` only during an active click/drag, so it
                // would flip the cursor only after pressing).
                let width_resize_active = self
                    .width_resize_drag
                    .is_some_and(|state| state.overlay_idx == entry.idx);
                let hover_on_width_handle = response.hover_pos().zip(width_guide_ticks).is_some_and(
                    |(pointer, (left_tick, right_tick))| {
                        let radius = TEXT_OVERLAY_WIDTH_GUIDE_HANDLE_RADIUS_PX;
                        pointer.distance(left_tick) <= radius
                            || pointer.distance(right_tick) <= radius
                    },
                );
                if hover_on_width_handle || width_resize_active {
                    ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }

                if response.clicked() && pointer_targets_overlay {
                    // Не перехватываем фокус перекрывающим оверлеем, если клик попал
                    // в рамку уже выделенного (нижнего) оверлея — фокус удержит
                    // блок sticky-фокуса после цикла.
                    if !(click_in_selected_frame && self.selected_overlay_idx != Some(entry.idx)) {
                        clicked_overlay_idx = Some(entry.idx);
                        self.selected_overlay_idx = Some(entry.idx);
                        self.primary_pointer_targets_overlay_this_frame = true;
                    }
                }
                if response.secondary_clicked() && pointer_inside_visual {
                    self.selected_overlay_idx = Some(entry.idx);
                    if let Some(origin) = pointer_pos {
                        self.start_shape_variant_preview_if_available(ui.ctx(), entry.idx, origin);
                    }
                }

                // Vector-transform menu availability for this overlay: `Some(true)` = text overlay
                // whose layout mode allows the vector warp; `Some(false)` = text overlay whose layout
                // mode disallows it (disabled + tooltip); `None` = image overlay (item hidden).
                let vector_menu_state = self.overlays.get(entry.idx).and_then(|overlay| {
                    (overlay.kind == TypingOverlayKind::Text).then(|| {
                        vector_transform_allowed_for_layout_mode(overlay_text_layout_mode(
                            overlay.render_data_json.as_ref(),
                        ))
                    })
                });
                response.context_menu(|menu_ui| {
                    if self.selected_overlay_idx != Some(entry.idx) {
                        menu_ui.label(t!("typing.canvas.select_overlay_hint"));
                        return;
                    }
                    if self
                        .shape_variant_preview
                        .as_ref()
                        .is_none_or(|state| state.overlay_idx != entry.idx)
                    {
                        let origin = menu_ui
                            .ctx()
                            .pointer_latest_pos()
                            .unwrap_or_else(|| menu_ui.min_rect().left_top());
                        self.start_shape_variant_preview_if_available(
                            menu_ui.ctx(),
                            entry.idx,
                            origin,
                        );
                    }
                    if menu_ui
                        .button(t!("typing.context_menu.enter_layout_edit_mode"))
                        .clicked()
                    {
                        pending_enter_layout_editor_idx = Some(entry.idx);
                        menu_ui.close();
                    }
                    menu_ui.separator();
                    if !is_transform_mode {
                        if menu_ui
                            .button(t!("typing.context_menu.enter_transform_mode_raster"))
                            .clicked()
                        {
                            if self.ensure_overlay_deform_mesh(entry.idx, image_rect, zoom) {
                                crate::trace_log!(
                                    cat::TYPING,
                                    "overlay_transform_mode enter idx={} kind=raster",
                                    entry.idx
                                );
                                self.transform_mode_overlay_idx = Some(entry.idx);
                                // Raster transform edits the runtime deform mesh; ensure no vector
                                // working state lingers.
                                self.exit_vector_transform_mode();
                                self.deform_mode = TypingDeformMode::Perspective;
                                self.drag_state = None;
                            }
                            menu_ui.close();
                        }
                        // Vector transform is TEXT-only and only for layouts that compose with the
                        // post-layout warp (Normal / Shape / CustomVectorLines). Image overlays hide
                        // it; Formula / CustomRasterLines show it disabled with a tooltip.
                        match vector_menu_state {
                            Some(true) => {
                                if menu_ui
                                    .button(t!("typing.context_menu.enter_transform_mode_vector"))
                                    .clicked()
                                {
                                    crate::trace_log!(
                                        cat::TYPING,
                                        "overlay_transform_mode enter idx={} kind=vector",
                                        entry.idx
                                    );
                                    self.selected_overlay_idx = Some(entry.idx);
                                    self.transform_mode_overlay_idx = Some(entry.idx);
                                    self.transform_mode_kind = TypingTransformModeKind::Vector;
                                    self.deform_mode = TypingDeformMode::Perspective;
                                    self.drag_state = None;
                                    self.seed_vector_transform_mesh(entry.idx, image_rect, zoom);
                                    menu_ui.close();
                                }
                            }
                            Some(false) => {
                                menu_ui
                                    .add_enabled(
                                        false,
                                        egui::Button::new(
                                            t!("typing.context_menu.enter_transform_mode_vector"),
                                        ),
                                    )
                                    .on_disabled_hover_text(t!("typing.context_menu.layout_not_supported"));
                            }
                            None => {}
                        }
                    } else {
                        if menu_ui
                            .button(t!("typing.context_menu.exit_transform_mode_raster"))
                            .clicked()
                        {
                            crate::trace_log!(
                                cat::TYPING,
                                "overlay_transform_mode exit idx={}",
                                entry.idx
                            );
                            if self.transform_mode_overlay_idx == Some(entry.idx) {
                                self.transform_mode_overlay_idx = None;
                            }
                            self.drag_state = None;
                            self.drag_has_changes = false;
                            menu_ui.close();
                        }
                        if menu_ui
                            .button(t!("typing.context_menu.reset_transform_raster"))
                            .clicked()
                        {
                            crate::trace_log!(
                                cat::TYPING,
                                "overlay_transform_reset idx={}",
                                entry.idx
                            );
                            if let Some(overlay) = self.overlays.get_mut(entry.idx) {
                                overlay.deform_mesh = None;
                            }
                            self.mark_overlay_geometry_changed(entry.idx, false);
                            // EDIT (reset raster transform from the context menu): deferred.
                            self.mark_placement_save_dirty();
                            self.drag_state = None;
                            self.drag_has_changes = false;
                            menu_ui.close();
                        }
                    }
                    menu_ui.separator();
                    if let Some(overlay) = self.overlays.get(entry.idx) {
                        let toggle_label = if overlay.mask_clip_enabled {
                            t!("typing.context_menu.disable_mask_clip")
                        } else {
                            t!("typing.context_menu.enable_mask_clip")
                        };
                        if menu_ui.button(toggle_label).clicked() {
                            let mut new_state = false;
                            if let Some(overlay) = self.overlays.get_mut(entry.idx) {
                                overlay.mask_clip_enabled = !overlay.mask_clip_enabled;
                                new_state = overlay.mask_clip_enabled;
                            }
                            crate::trace_log!(
                                cat::TYPING,
                                "overlay_mask_clip_toggle idx={} enabled={}",
                                entry.idx,
                                new_state
                            );
                            self.mark_overlay_pixels_dirty(entry.idx);
                            // EDIT (mask-clip toggle from the context menu): deferred.
                            self.mark_placement_save_dirty();
                            menu_ui.close();
                        }
                    }
                    menu_ui.separator();
                    {
                        // ▲ / ▼ move the overlay one step in the unified Z order (text + raster
                        // interleaved, shared with the PS editor). No more per-overlay text-group
                        // number — order is the shared layer stack.
                        let mut move_z_up: Option<bool> = None;
                        menu_ui.horizontal(|row| {
                            row.label(t!("typing.context_menu.order"));
                            if row.button("▲").clicked() {
                                move_z_up = Some(true);
                            }
                            if row.button("▼").clicked() {
                                move_z_up = Some(false);
                            }
                        });
                        if let Some(up) = move_z_up {
                            self.move_overlay_in_unified_z(page_idx, entry.idx, up);
                        }
                    }
                    menu_ui.separator();
                    if menu_ui.button(t!("typing.context_menu.delete_overlay")).clicked() {
                        pending_delete_overlay_idx = Some(entry.idx);
                        menu_ui.close();
                    }
                    self.update_shape_variant_preview_menu_rect(entry.idx, menu_ui.min_rect());
                });

                if response.drag_started() && pointer_targets_overlay {
                    self.primary_pointer_targets_overlay_this_frame = true;
                    if let Some(pointer_pos) = pointer_pos {
                        // A width-guide tick starts a width-resize drag (param edit + re-render), NOT a
                        // placement drag, so set that state and skip the placement drag setup.
                        if let Some(right) = pointer_on_width_handle {
                            self.selected_overlay_idx = Some(entry.idx);
                            self.width_resize_drag = Some(WidthResizeDragState {
                                overlay_idx: entry.idx,
                                page_idx,
                                pointer_start_x: pointer_pos.x,
                                start_width_px: entry.render_width_px.unwrap_or(0),
                                right,
                            });
                            continue;
                        }
                        let Some((
                            mut start_center_page_px,
                            start_angle_deg,
                            has_mesh,
                            mut start_mesh,
                        )) = self.overlays.get(entry.idx).map(|overlay| {
                            (
                                overlay.center_page_px,
                                overlay.angle_deg,
                                overlay.deform_mesh.is_some(),
                                overlay.deform_mesh.clone().unwrap_or_else(|| {
                                    default_overlay_quad_mesh(overlay, image_rect, zoom)
                                }),
                            )
                        })
                        else {
                            continue;
                        };

                        crate::trace_log!(
                            cat::INPUT,
                            "overlay_drag_begin owner={} idx={} selected_was={:?} reason=drag_started",
                            if self.selected_overlay_idx == Some(entry.idx) {
                                "selected"
                            } else {
                                "reselect"
                            },
                            entry.idx,
                            self.selected_overlay_idx
                        );
                        self.selected_overlay_idx = Some(entry.idx);
                        let mut mode = if pointer_on_rotate_handle {
                            TypingOverlayDragMode::Rotate
                        } else if has_mesh {
                            TypingOverlayDragMode::MoveMesh
                        } else {
                            TypingOverlayDragMode::MoveCenter
                        };
                        let start_mesh_scene = scene_mesh_points(&start_mesh, image_rect, zoom);
                        let start_center_scene = deform_mesh_center_scene(&start_mesh_scene);
                        let start_pointer_angle_rad =
                            pointer_angle_rad(start_center_scene, pointer_pos);

                        if self.transform_mode_overlay_idx == Some(entry.idx) {
                            let _ = self.ensure_overlay_deform_mesh(entry.idx, image_rect, zoom);
                            if let Some(current_mesh) = self
                                .overlays
                                .get(entry.idx)
                                .and_then(|overlay| overlay.deform_mesh.clone())
                            {
                                mode = TypingOverlayDragMode::MoveMesh;
                                if let Some(handle_idx) = pointer_on_handle {
                                    mode = match self.deform_mode {
                                        TypingDeformMode::Perspective => {
                                            TypingOverlayDragMode::PerspectiveHandle(handle_idx)
                                        }
                                        TypingDeformMode::Bend => {
                                            TypingOverlayDragMode::BendHandle(handle_idx)
                                        }
                                        TypingDeformMode::Frame => {
                                            TypingOverlayDragMode::FrameHandle(handle_idx)
                                        }
                                        TypingDeformMode::Grid => {
                                            TypingOverlayDragMode::GridHandle(handle_idx)
                                        }
                                        _ => TypingOverlayDragMode::MoveMesh,
                                    };
                                } else if self.deform_mode.is_brush_mode() && pointer_inside_visual
                                {
                                    mode = TypingOverlayDragMode::BrushStroke(self.deform_mode);
                                }
                                let snapped_on_drag_start =
                                    if matches!(mode, TypingOverlayDragMode::MoveMesh) {
                                        let page_size = page_size_from_image_rect(image_rect, zoom);
                                        self.snap_overlay_to_pixel_position(
                                            entry.idx, page_size, true,
                                        )
                                    } else {
                                        false
                                    };
                                let current_mesh = if snapped_on_drag_start {
                                    self.overlays
                                        .get(entry.idx)
                                        .and_then(|overlay| overlay.deform_mesh.clone())
                                        .unwrap_or(current_mesh)
                                } else {
                                    current_mesh
                                };
                                if snapped_on_drag_start
                                    && let Some(overlay) = self.overlays.get(entry.idx)
                                {
                                    start_center_page_px = overlay.center_page_px;
                                }
                                crate::trace_log!(
                                    cat::INPUT,
                                    "overlay_drag_begin transform=true idx={} page={} mode={:?} deform_mode={:?}",
                                    entry.idx,
                                    page_idx,
                                    mode,
                                    self.deform_mode
                                );
                                self.drag_state = Some(TypingOverlayDragState {
                                    overlay_idx: entry.idx,
                                    page_idx,
                                    pointer_start_scene: pointer_pos,
                                    mode,
                                    start_has_mesh: has_mesh,
                                    start_center_page_px,
                                    start_angle_deg,
                                    start_pointer_angle_rad,
                                    start_mesh: current_mesh,
                                });
                                self.drag_has_changes = snapped_on_drag_start;
                                continue;
                            }
                        }

                        let snapped_on_drag_start = if matches!(
                            mode,
                            TypingOverlayDragMode::MoveCenter | TypingOverlayDragMode::MoveMesh
                        ) {
                            let page_size = page_size_from_image_rect(image_rect, zoom);
                            self.snap_overlay_to_pixel_position(entry.idx, page_size, true)
                        } else {
                            false
                        };
                        if snapped_on_drag_start && let Some(overlay) = self.overlays.get(entry.idx)
                        {
                            start_center_page_px = overlay.center_page_px;
                            start_mesh = overlay.deform_mesh.clone().unwrap_or_else(|| {
                                default_overlay_quad_mesh(overlay, image_rect, zoom)
                            });
                        }
                        crate::trace_log!(
                            cat::INPUT,
                            "overlay_drag_begin transform=false idx={} page={} mode={:?}",
                            entry.idx,
                            page_idx,
                            mode
                        );
                        self.drag_state = Some(TypingOverlayDragState {
                            overlay_idx: entry.idx,
                            page_idx,
                            pointer_start_scene: pointer_pos,
                            mode,
                            start_has_mesh: has_mesh,
                            start_center_page_px,
                            start_angle_deg,
                            start_pointer_angle_rad,
                            start_mesh,
                        });
                        self.drag_has_changes = snapped_on_drag_start;
                    }
                }

                // Width-resize drag: change the layer's configured width_px and re-render (latest-wins).
                // Runs BEFORE the placement `dragged()` handler below, whose `drag_state.take()` would
                // otherwise `continue` past this block (width resize uses a separate state field).
                if let Some(state) = self.width_resize_drag
                    && state.overlay_idx == entry.idx
                    && state.page_idx == page_idx
                {
                    self.primary_pointer_targets_overlay_this_frame = true;
                    if response.dragged()
                        && let Some(pointer_pos) = pointer_pos
                    {
                        let display_scale = self
                            .overlays
                            .get(entry.idx)
                            .map_or(zoom, |overlay| zoom * overlay.user_scale.max(0.01));
                        let new_width = width_from_guide_drag(
                            state.start_width_px,
                            state.right,
                            pointer_pos.x - state.pointer_start_x,
                            display_scale,
                        );
                        self.resize_selected_overlay_width(entry.idx, new_width, ctx);
                    }
                    if response.drag_stopped() {
                        self.width_resize_drag = None;
                    }
                    continue;
                }

                if response.dragged() {
                    let Some(mut state) = self.drag_state.take() else {
                        continue;
                    };
                    if state.overlay_idx != entry.idx || state.page_idx != page_idx {
                        self.drag_state = Some(state);
                        continue;
                    }
                    let Some(pointer_pos) = pointer_pos else {
                        self.drag_state = Some(state);
                        continue;
                    };

                    let page_size = page_size_from_image_rect(image_rect, zoom);
                    let raw_delta_page_px = [
                        (pointer_pos.x - state.pointer_start_scene.x) / zoom.max(f32::EPSILON),
                        (pointer_pos.y - state.pointer_start_scene.y) / zoom.max(f32::EPSILON),
                    ];
                    let delta_page_px = match state.mode {
                        TypingOverlayDragMode::MoveCenter | TypingOverlayDragMode::MoveMesh => {
                            quantize_drag_page_delta(raw_delta_page_px, strict_pixel_movement)
                        }
                        TypingOverlayDragMode::PerspectiveHandle(_)
                        | TypingOverlayDragMode::BendHandle(_)
                        | TypingOverlayDragMode::FrameHandle(_)
                        | TypingOverlayDragMode::GridHandle(_)
                        | TypingOverlayDragMode::BrushStroke(_)
                        | TypingOverlayDragMode::Rotate => raw_delta_page_px,
                    };
                    let move_center_transition = match state.mode {
                        TypingOverlayDragMode::MoveCenter => {
                            Some(self.remap_drag_vertical_page_transition(
                                state.page_idx,
                                state.start_center_page_px[1] + delta_page_px[1],
                                page_size,
                            ))
                        }
                        _ => None,
                    };
                    let move_mesh_transition = match state.mode {
                        TypingOverlayDragMode::MoveMesh => {
                            let mut raw_mesh = state.start_mesh.clone();
                            raw_mesh.translate(delta_page_px[0], delta_page_px[1], page_size);
                            let center_y =
                                raw_mesh.points_px.iter().map(|point| point[1]).sum::<f32>()
                                    / raw_mesh.points_px.len().max(1) as f32;
                            let (next_page_idx, next_center_v) = self
                                .remap_drag_vertical_page_transition(
                                    state.page_idx,
                                    center_y,
                                    page_size,
                                );
                            Some((raw_mesh, center_y, next_page_idx, next_center_v))
                        }
                        _ => None,
                    };
                    let mut overlay_changed = false;
                    let mut page_changed = false;
                    if let Some(overlay) = self.overlays.get_mut(entry.idx) {
                        let prev_center_page_px = overlay.center_page_px;
                        let prev_angle = overlay.angle_deg;
                        let prev_mesh = overlay.deform_mesh.clone();
                        let prev_page_idx = overlay.page_idx;
                        match state.mode {
                            TypingOverlayDragMode::MoveCenter => {
                                let (next_page_idx, next_y_px) =
                                    move_center_transition.unwrap_or((
                                        state.page_idx,
                                        clamp_overlay_page_coord(
                                            state.start_center_page_px[1] + delta_page_px[1],
                                            page_size[1],
                                        ),
                                    ));
                                overlay.center_page_px = clamp_page_point(
                                    [state.start_center_page_px[0] + delta_page_px[0], next_y_px],
                                    page_size,
                                );
                                overlay.page_idx = next_page_idx;
                                page_changed = overlay.page_idx != prev_page_idx;
                            }
                            TypingOverlayDragMode::MoveMesh => {
                                let (mut deform_mesh, center_y, next_page_idx, next_center_y) =
                                    move_mesh_transition.unwrap_or((
                                        state.start_mesh.clone(),
                                        state
                                            .start_mesh
                                            .points_px
                                            .iter()
                                            .map(|point| point[1])
                                            .sum::<f32>()
                                            / state.start_mesh.points_px.len().max(1) as f32,
                                        state.page_idx,
                                        state
                                            .start_mesh
                                            .points_px
                                            .iter()
                                            .map(|point| point[1])
                                            .sum::<f32>()
                                            / state.start_mesh.points_px.len().max(1) as f32,
                                    ));
                                if next_page_idx != state.page_idx {
                                    let shift_y = next_center_y - center_y;
                                    deform_mesh.translate(0.0, shift_y, page_size);
                                }
                                overlay.deform_mesh = Some(deform_mesh);
                                overlay.page_idx = next_page_idx;
                                page_changed = overlay.page_idx != prev_page_idx;
                                sync_overlay_center_from_deform_mesh(overlay, page_size);
                            }
                            TypingOverlayDragMode::PerspectiveHandle(handle_idx) => {
                                if handle_idx < 4 {
                                    overlay.deform_mesh = Some(apply_perspective_corner_drag(
                                        &state.start_mesh,
                                        handle_idx,
                                        delta_page_px,
                                        page_size,
                                    ));
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                }
                            }
                            TypingOverlayDragMode::BendHandle(handle_idx) => {
                                if handle_idx < bend_handle_count() {
                                    overlay.deform_mesh = Some(apply_bend_handle_drag(
                                        &state.start_mesh,
                                        handle_idx,
                                        delta_page_px,
                                        page_size,
                                    ));
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                }
                            }
                            TypingOverlayDragMode::FrameHandle(handle_idx) => {
                                if handle_idx < frame_handle_count(self.frame_handle_side_points) {
                                    overlay.deform_mesh = Some(apply_sampled_handle_drag(
                                        &state.start_mesh,
                                        SampledHandleMode::Frame,
                                        self.frame_handle_side_points,
                                        handle_idx,
                                        self.pull_neighbor_handles,
                                        delta_page_px,
                                        page_size,
                                    ));
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                }
                            }
                            TypingOverlayDragMode::GridHandle(handle_idx) => {
                                if handle_idx < grid_handle_count(self.frame_handle_side_points) {
                                    overlay.deform_mesh = Some(apply_sampled_handle_drag(
                                        &state.start_mesh,
                                        SampledHandleMode::Grid,
                                        self.frame_handle_side_points,
                                        handle_idx,
                                        self.pull_neighbor_handles,
                                        delta_page_px,
                                        page_size,
                                    ));
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                }
                            }
                            TypingOverlayDragMode::BrushStroke(mode) => {
                                let default_mesh =
                                    default_overlay_deform_mesh(overlay, image_rect, zoom);
                                overlay.deform_mesh = Some(apply_brush_deform_drag(
                                    mode,
                                    &state.start_mesh,
                                    &default_mesh,
                                    state.pointer_start_scene,
                                    pointer_pos,
                                    image_rect,
                                    zoom,
                                    &self.deform_tool_settings,
                                ));
                                sync_overlay_center_from_deform_mesh(overlay, page_size);
                            }
                            TypingOverlayDragMode::Rotate => {
                                let start_mesh_scene =
                                    scene_mesh_points(&state.start_mesh, image_rect, zoom);
                                let center_scene = deform_mesh_center_scene(&start_mesh_scene);
                                let current_angle = pointer_angle_rad(center_scene, pointer_pos);
                                let delta_angle = normalize_angle_rad(
                                    current_angle - state.start_pointer_angle_rad,
                                );
                                if state.start_has_mesh {
                                    let rotated_scene = rotate_mesh_scene(
                                        &start_mesh_scene,
                                        center_scene,
                                        delta_angle,
                                    );
                                    let rotated_uv = rotated_scene
                                        .into_iter()
                                        .map(|scene| page_px_from_scene(image_rect, zoom, scene))
                                        .collect::<Vec<_>>();
                                    overlay.deform_mesh = TypingOverlayDeformMesh::new(
                                        state.start_mesh.cols,
                                        state.start_mesh.rows,
                                        rotated_uv,
                                        page_size,
                                    );
                                    sync_overlay_center_from_deform_mesh(overlay, page_size);
                                } else {
                                    overlay.angle_deg = normalize_angle_deg(
                                        state.start_angle_deg + delta_angle.to_degrees(),
                                    );
                                }
                            }
                        }
                        if overlay.center_page_px != prev_center_page_px
                            || (overlay.angle_deg - prev_angle).abs() > 1e-4
                            || overlay.deform_mesh != prev_mesh
                            || overlay.page_idx != prev_page_idx
                        {
                            self.drag_has_changes = true;
                            overlay_changed = true;
                        }
                    }
                    if !page_changed
                        && self.enforce_overlay_visibility_limit(
                            entry.idx,
                            image_rect,
                            zoom,
                            strict_pixel_movement,
                        )
                    {
                        self.drag_has_changes = true;
                        overlay_changed = true;
                    }
                    if overlay_changed {
                        self.mark_overlay_geometry_changed(entry.idx, true);
                    }
                    let brush_continue =
                        matches!(state.mode, TypingOverlayDragMode::BrushStroke(_));
                    if (page_changed || brush_continue)
                        && let Some(overlay) = self.overlays.get(entry.idx)
                    {
                        state.page_idx = overlay.page_idx;
                        state.pointer_start_scene = pointer_pos;
                        state.start_center_page_px = overlay.center_page_px;
                        state.start_angle_deg = overlay.angle_deg;
                        if let Some(mesh) = overlay.deform_mesh.clone() {
                            state.start_mesh = mesh;
                        }
                    }
                    self.drag_state = Some(state);
                }

                if response.drag_stopped()
                    && self
                        .drag_state
                        .as_ref()
                        .is_some_and(|state| state.overlay_idx == entry.idx)
                {
                    if crate::trace::trace_enabled() {
                        let (center, angle) = self
                            .overlays
                            .get(entry.idx)
                            .map(|o| (o.center_page_px, o.angle_deg))
                            .unwrap_or(([0.0, 0.0], 0.0));
                        crate::trace_log!(
                            cat::INPUT,
                            "overlay_drag_end idx={} committed={} center=({:.1},{:.1}) angle={:.1}",
                            entry.idx,
                            self.drag_has_changes,
                            center[0],
                            center[1],
                            angle
                        );
                    }
                    if self.drag_has_changes {
                        self.flush_overlay_texture_if_stale(entry.idx);
                        // EDIT (move/rotate drag end): deferred.
                        self.mark_placement_save_dirty();
                    }
                    self.drag_state = None;
                    self.drag_has_changes = false;
                }
            }

            // Клик внутри рамки выделенного оверлея считаем нацеленным на него:
            // помечаем кадр как «попал в оверлей» (чтобы растровый слой выше не
            // перехватил фокус, см. `interact_page_rasters`) и подставляем
            // выделенный индекс в `clicked_overlay_idx`, чтобы не сработал сброс
            // выделения при клике по «пустому» месту.
            if click_in_selected_frame {
                self.primary_pointer_targets_overlay_this_frame = true;
                if clicked_overlay_idx.is_none() {
                    clicked_overlay_idx = self.selected_overlay_idx;
                }
            }

            self.poll_shape_variant_preview(ui.ctx());
            if let Some(variant) = self.draw_shape_variant_preview(ui.ctx()) {
                self.apply_shape_variant_to_overlay(ctx, variant);
            }

            if let Some(delete_idx) = pending_delete_overlay_idx {
                self.remove_overlay(delete_idx);
                return Vec::new();
            }
            if let Some(editor_idx) = pending_enter_layout_editor_idx {
                self.begin_layout_editor_for_overlay(editor_idx, image_rect, zoom);
                ctx.request_repaint();
            }
            let popup_open_after = ui.ctx().any_popup_open();
            let popup_open = popup_open_before || popup_open_after;
            let delete_pressed = ui.input(|i| i.key_pressed(egui::Key::Delete));
            if delete_pressed
                && !ui.ctx().egui_wants_keyboard_input()
                && let Some(selected_idx) = self.selected_overlay_idx
                && self
                    .overlays
                    .get(selected_idx)
                    .is_some_and(|overlay| overlay.page_idx == page_idx)
            {
                self.remove_overlay(selected_idx);
                return Vec::new();
            }

            let clicked_on_image_without_overlay = ui.input(|i| {
                i.pointer.primary_clicked()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|pos| image_rect.contains(pos))
                    && clicked_overlay_idx.is_none()
            }) && !popup_open
                && !crate::input_util::pointer_over_floating_area(ui.ctx())
                && !eyedropper_blocks_focus_clear;
            if clicked_on_image_without_overlay {
                if self
                    .selected_overlay_idx
                    .and_then(|idx| self.overlays.get(idx))
                    .is_some_and(|overlay| overlay.page_idx == page_idx)
                {
                    if let Some(selected_idx) = self.selected_overlay_idx
                        && self.enforce_overlay_visibility_limit(
                            selected_idx,
                            image_rect,
                            zoom,
                            strict_pixel_movement,
                        )
                    {
                        snap_overlay_center_to_pixels_if_enabled(
                            self.overlays
                                .get_mut(selected_idx)
                                .expect("selected overlay exists after visibility enforcement"),
                            strict_pixel_movement,
                            page_size_from_image_rect(image_rect, zoom),
                        );
                        self.mark_overlay_geometry_changed(selected_idx, false);
                        // EDIT (clamp on click-away). The deselect immediately below is itself a focus
                        // loss, so `flush_edit_save_on_selection_change` writes this on the same frame.
                        self.mark_placement_save_dirty();
                    }
                    if self.transform_mode_overlay_idx == self.selected_overlay_idx {
                        self.transform_mode_overlay_idx = None;
                    }
                    self.selected_overlay_idx = None;
                }
                if self
                    .drag_state
                    .as_ref()
                    .is_some_and(|state| state.page_idx == page_idx)
                {
                    self.drag_state = None;
                    self.drag_has_changes = false;
                }
            }
            if self
                .transform_mode_overlay_idx
                .is_some_and(|idx| self.selected_overlay_idx != Some(idx))
                && !popup_open
            {
                self.transform_mode_overlay_idx = None;
            }
        }

        // Unified-Z fill pass: interleave the read-only PS raster quads with the text/image overlay
        // textured meshes in one pass ordered bottom-to-top by band Z. (Selection decorations and
        // editing handles are drawn afterwards so they always sit on top.)
        enum MergedFillItem {
            /// Index into the page's cached `raster_layers_by_page` vector.
            Raster(usize),
            /// Index into `draw_entries`.
            Overlay(usize),
        }
        let mut merged_fills: Vec<(u32, u32, MergedFillItem)> = Vec::new();
        // Rasters: band Z from the matching `Raster` band (else top). Tiebreak `0` keeps the cached
        // bottom-to-top raster order via the raster index in the third tuple slot's stable sort.
        if let Some(rasters) = self.raster_layers_by_page.get(&page_idx) {
            for (raster_idx, raster) in rasters.iter().enumerate() {
                let band_z = self.raster_band_z(page_idx, &raster.uid);
                merged_fills.push((band_z, 0, MergedFillItem::Raster(raster_idx)));
            }
        }
        // Overlays: band Z from the overlay's text group / pinned-text band (else top). Tiebreak `1`
        // so that, within the same band Z, overlays draw above rasters; `draw_entries` is already in
        // the desired within-group order, preserved by the stable sort.
        for (entry_pos, entry) in draw_entries.iter().enumerate() {
            let band_z = self
                .overlays
                .get(entry.idx)
                .map(|overlay| self.overlay_band_z(page_idx, &overlay.uid, overlay.layer_idx))
                .unwrap_or_else(|| {
                    self.bands_by_page
                        .get(&page_idx)
                        .map(|b| b.len() as u32)
                        .unwrap_or(0)
                });
            merged_fills.push((band_z, 1, MergedFillItem::Overlay(entry_pos)));
        }
        // Stable sort: primary band Z, then raster-below-overlay tiebreak; existing raster order and
        // within-group overlay order are preserved as the stable tiebreak.
        merged_fills.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, _, item) in &merged_fills {
            match item {
                MergedFillItem::Raster(raster_idx) => {
                    self.draw_one_raster_layer(
                        ui.ctx(),
                        &painter,
                        page_idx,
                        *raster_idx,
                        image_rect,
                        zoom,
                    );
                }
                MergedFillItem::Overlay(entry_pos) => {
                    let entry = &draw_entries[*entry_pos];
                    // Hide the plain baked PNG while the live VECTOR-transform warped preview is drawn
                    // for this overlay (`draw_vector_transform_overlay` below textures the un-warped base
                    // onto the working mesh). Skipping here avoids double-drawing the static + warped
                    // copies. Falls back to drawing the baked PNG when the preview is not active.
                    if self.vector_transform_preview_active(entry.idx) {
                        continue;
                    }
                    draw_textured_deform_mesh(
                        &painter,
                        entry.texture.id(),
                        &entry.mesh_scene,
                        entry.mesh_cols,
                        entry.mesh_rows,
                        Color32::WHITE,
                    );
                }
            }
        }

        for entry in &draw_entries {
            if !mask_panel_open && self.selected_overlay_idx == Some(entry.idx) {
                let selection_path = mesh_boundary_path(
                    &entry.selection_mesh_scene,
                    entry.mesh_cols,
                    entry.mesh_rows,
                );
                draw_dashed_selection_path(&painter, &selection_path);
                if let Some(render_width_px) = entry.render_width_px {
                    // Rotation-invariant source->screen scale (matches `default_overlay_quad_scene`):
                    // the specified width must not stretch with the overlay's raster rotation.
                    let display_scale = self
                        .overlays
                        .get(entry.idx)
                        .map_or(zoom, |overlay| zoom * overlay.user_scale.max(0.01));
                    draw_text_overlay_width_guide(
                        &painter,
                        entry.selection_bounds_rect,
                        render_width_px,
                        display_scale,
                    );
                }
                // RASTER transform mode draws its handles on the overlay's runtime deform mesh here.
                // VECTOR transform mode draws its OWN handles/wireframe on the working mesh from
                // `draw_vector_transform_overlay`, so only the dashed selection path (above) is drawn
                // here for it. A plain (non-transform) selection shows the rotation handle.
                let raster_transform_here = self.transform_mode_overlay_idx == Some(entry.idx)
                    && self.transform_mode_kind == TypingTransformModeKind::Raster;
                let vector_transform_here = self.transform_mode_overlay_idx == Some(entry.idx)
                    && self.transform_mode_kind == TypingTransformModeKind::Vector;
                if raster_transform_here {
                    match self.deform_mode {
                        TypingDeformMode::Perspective => {
                            draw_perspective_handles(&painter, &entry.quad_scene)
                        }
                        TypingDeformMode::Bend => draw_bend_handles(
                            &painter,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                        ),
                        TypingDeformMode::Frame => draw_frame_handles(
                            &painter,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                            self.frame_handle_side_points,
                        ),
                        TypingDeformMode::Grid => draw_grid_handles(
                            &painter,
                            &entry.mesh_scene,
                            entry.mesh_cols,
                            entry.mesh_rows,
                            self.frame_handle_side_points,
                        ),
                        _ => {}
                    }
                } else if !vector_transform_here {
                    draw_rotation_handle(&painter, &entry.quad_scene, image_rect);
                }
            }
        }
        // VECTOR transform mode owns its overlay's interaction + handle/wireframe drawing in a
        // dedicated pass (drawn after the baked-PNG fill so it sits on top). Gated off while the mask
        // panel or the layout editor is active, matching the normal overlay interaction gate.
        if !mask_panel_open && !layout_editor_active {
            self.draw_vector_transform_overlay(ui, ctx, page_idx, image_rect, zoom, &painter);
        }
        if self.layout_editor.is_some() && !mask_panel_open {
            self.draw_layout_editor_on_page(ui, ctx, page_idx, image_rect, zoom, clip_rect);
        }
        if let Some(selected_idx) = self.transform_mode_overlay_idx
            && self.transform_mode_kind == TypingTransformModeKind::Raster
            && self.selected_overlay_idx == Some(selected_idx)
            && self.deform_mode.is_brush_mode()
            && let Some(selected_entry) =
                draw_entries.iter().find(|entry| entry.idx == selected_idx)
            && let Some(pointer_pos) = ui.ctx().input(|i| i.pointer.latest_pos())
            && deform_mesh_contains_point(
                &selected_entry.mesh_scene,
                selected_entry.mesh_cols,
                selected_entry.mesh_rows,
                pointer_pos,
            )
        {
            draw_brush_preview(
                &painter,
                pointer_pos,
                self.deform_tool_settings.brush_radius_px,
            );
        }
        self.draw_auto_typing_debug_visuals(&painter, page_idx, image_rect, auto_typing_settings);
        if !mask_panel_open && !layout_editor_active {
            self.interact_page_rasters(ui, page_idx, image_rect, zoom, &painter);
        }
        draw_entries
            .into_iter()
            .flat_map(|entry| entry.occluder_quads.into_iter())
            .collect()
    }

    pub(super) fn wants_repaint(&self) -> bool {
        self.loading_rx.is_some()
            || self.create_selection.is_some()
            || self.create_editor.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.edit_render_rx.is_some()
            || self.auto_typing_job.is_some()
            || self.export_rx.is_some()
            || self.create_status_error.is_some()
            || self.create_status_warning.is_some()
            || self.save_rx.is_some()
            || !self.pending_upload_indices.is_empty()
            || self.drag_state.is_some()
            || self.vector_transform_drag.is_some()
            || self.vector_transform_base_rx.is_some()
            || self.layout_editor.is_some()
    }

    pub(super) fn snap_overlay_to_pixel_position(
        &mut self,
        overlay_idx: usize,
        page_size: [usize; 2],
        defer_mask_refresh: bool,
    ) -> bool {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return false;
        };
        let previous_center = overlay.center_page_px;
        let previous_mesh = overlay.deform_mesh.clone();
        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            return false;
        };
        snap_overlay_center_to_pixels_if_enabled(overlay, true, page_size);
        let changed =
            overlay.center_page_px != previous_center || overlay.deform_mesh != previous_mesh;
        if changed {
            self.mark_overlay_geometry_changed(overlay_idx, defer_mask_refresh);
        }
        changed
    }

    pub(super) fn enforce_overlay_visibility_limit(
        &mut self,
        overlay_idx: usize,
        image_rect: Rect,
        zoom: f32,
        strict_pixel_movement: bool,
    ) -> bool {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return false;
        };
        if !image_rect.is_positive() || overlay.size_px[0] == 0 || overlay.size_px[1] == 0 {
            return false;
        }

        let bounds = if overlay.deform_mesh.is_some() {
            let deform_mesh = overlay_deform_mesh(overlay, image_rect, zoom);
            let page_size = page_size_from_image_rect(image_rect, zoom);
            let bounds_uv = deform_mesh_bounds_uv(&deform_mesh, page_size);
            if !bounds_uv.is_positive() {
                return false;
            }
            Rect::from_min_max(
                scene_from_uv(image_rect, bounds_uv.min.x, bounds_uv.min.y),
                scene_from_uv(image_rect, bounds_uv.max.x, bounds_uv.max.y),
            )
        } else {
            quad_bounds(&default_overlay_quad_scene(overlay, image_rect, zoom))
        };

        let min_visible_w = bounds.width() * TEXT_OVERLAY_MIN_VISIBLE_FRACTION;
        let min_visible_h = bounds.height() * TEXT_OVERLAY_MIN_VISIBLE_FRACTION;

        let target_left = bounds.left().clamp(
            image_rect.left() + min_visible_w - bounds.width(),
            image_rect.right() - min_visible_w,
        );
        let target_top = bounds.top().clamp(
            image_rect.top() + min_visible_h - bounds.height(),
            image_rect.bottom() - min_visible_h,
        );
        let dx = target_left - bounds.left();
        let dy = target_top - bounds.top();
        if dx.abs() <= 1e-6 && dy.abs() <= 1e-6 {
            return false;
        }

        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            return false;
        };
        let page_size = page_size_from_image_rect(image_rect, zoom);
        if let Some(deform_mesh) = overlay.deform_mesh.as_mut() {
            let dx_px = dx / zoom.max(f32::EPSILON);
            let dy_px = dy / zoom.max(f32::EPSILON);
            deform_mesh.translate(dx_px, dy_px, page_size);
            sync_overlay_center_from_deform_mesh(overlay, page_size);
        } else {
            let dx_px = dx / zoom.max(f32::EPSILON);
            let dy_px = dy / zoom.max(f32::EPSILON);
            overlay.center_page_px = clamp_page_point(
                [
                    overlay.center_page_px[0] + dx_px,
                    overlay.center_page_px[1] + dy_px,
                ],
                page_size,
            );
        }
        snap_overlay_center_to_pixels_if_enabled(overlay, strict_pixel_movement, page_size);
        true
    }

    pub(super) fn remap_drag_vertical_page_transition(
        &self,
        mut page_idx: usize,
        mut y_px: f32,
        page_size: [usize; 2],
    ) -> (usize, f32) {
        let min_v = overlay_uv_min() * page_size[1].max(1) as f32;
        let max_v = overlay_uv_max() * page_size[1].max(1) as f32;
        loop {
            if y_px > max_v && page_idx + 1 < self.page_count {
                y_px = min_v + (y_px - max_v);
                page_idx += 1;
                continue;
            }
            if y_px < min_v && page_idx > 0 {
                y_px = max_v - (min_v - y_px);
                page_idx -= 1;
                continue;
            }
            break;
        }
        (page_idx, clamp_overlay_page_coord(y_px, page_size[1]))
    }
}
