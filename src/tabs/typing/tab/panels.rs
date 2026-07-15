/*
File: tab/panels.rs

Purpose:
Panel-drawing methods for the typing tab's canvas overlays: the deformation-mode
popup, the unified per-page layers list, and the vector layout-editor panels
(the single mode panel that also hosts the vector-lines params + preview-opacity
slider while Editing, editor lifecycle enter/exit, and the on-page editor overlay
that paints the edited layer dimmed under the frame).

Notes:
Extracted verbatim from `tab.rs`. Methods are `pub(super)` so `tab.rs` and sibling
submodules of `tab` can use them. `use super::*;` pulls in the parent module's
types and imports. Struct/enum definitions and the rest of the big
`impl TypingTextOverlayLayer` block remain in `tab.rs`; these methods reach the
private items that stay there as descendants of module `tab`.
*/

use super::*;

impl TypingTextOverlayLayer {
    pub(super) fn draw_deformation_mode_panel(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        if self.transform_mode_overlay_idx.is_none() {
            return;
        }
        let area_pos = canvas_rect.left_top() + egui::vec2(16.0, 16.0);
        egui::Area::new("typing_deformation_mode_panel".into())
            .order(egui::Order::Foreground)
            .fixed_pos(area_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_rgba_unmultiplied(95, 22, 22, 235))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(255, 110, 110)))
                    .show(ui, |ui| {
                        ui.visuals_mut().override_text_color =
                            Some(Color32::from_rgb(255, 235, 235));
                        ui.label(egui::RichText::new(t!("typing.deform.panel_heading")).strong());
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            for mode in [
                                TypingDeformMode::Perspective,
                                TypingDeformMode::Bend,
                                TypingDeformMode::Frame,
                                TypingDeformMode::Grid,
                                TypingDeformMode::Bulge,
                                TypingDeformMode::Pinch,
                                TypingDeformMode::Push,
                                TypingDeformMode::Twirl,
                                TypingDeformMode::Restore,
                                TypingDeformMode::Smooth,
                                TypingDeformMode::Stretch,
                                TypingDeformMode::Fold,
                            ] {
                                ui.selectable_value(&mut self.deform_mode, mode, mode.label());
                            }
                        });
                        if matches!(
                            self.deform_mode,
                            TypingDeformMode::Frame | TypingDeformMode::Grid
                        ) {
                            ui.add_space(6.0);
                            ui.label(t!("typing.deform.point_density_label"));
                            ui.horizontal_wrapped(|ui| {
                                let max_side_points = TEXT_OVERLAY_DEFORM_SURFACE_COLS
                                    .min(TEXT_OVERLAY_DEFORM_SURFACE_ROWS);
                                for side_points in 3..=max_side_points {
                                    ui.selectable_value(
                                        &mut self.frame_handle_side_points,
                                        side_points,
                                        format!("{side_points}*{side_points}"),
                                    );
                                }
                            });
                            ui.checkbox(&mut self.pull_neighbor_handles, t!("typing.deform.drag_neighbors"));
                        }
                        if self.deform_mode.is_brush_mode() {
                            ui.add_space(6.0);
                            ui.add(
                                WheelSlider::new(
                                    &mut self.deform_tool_settings.brush_radius_px,
                                    16.0..=280.0,
                                )
                                .text(t!("typing.deform.radius_label")),
                            );
                            ui.add(
                                WheelSlider::new(
                                    &mut self.deform_tool_settings.brush_strength,
                                    0.05..=1.5,
                                )
                                .text(t!("typing.deform.strength_label")),
                            );
                        }
                    });
            });
    }

    /// Task C: compact, collapsible layers list for the current page. Shows the read-only PS raster
    /// rows (name + visibility) followed by this tab's text/image overlays, which can be reordered
    /// (up/down) within the page. Reordering rewrites overlay array order, hence persisted z.
    /// Renders the «Слои» tab BODY (the unified interleaved layer list with per-row ⬆/⬇ move,
    /// text-preview names, the width-resize, and the 8-row scroll) into the supplied `ui`. The outer
    /// Area/Frame and the tab header/collapse are provided by the combined Actions/Layers panel (drawn
    /// from `TypingTopPanelState`). The WIDTH is still user-resizable here and persisted in
    /// `layers_panel_width`, driving the per-width `max_chars` preview budget. No-op while the layout
    /// editor is active.
    /// The current persisted «Слои» list width — lets the combined panel size its Frame so the list's
    /// inner width-resize can actually widen the panel.
    pub(in crate::tabs::typing) fn layers_panel_width(&self) -> f32 {
        self.layers_panel_width
    }

    pub(in crate::tabs::typing) fn draw_layers_tab_body(&mut self, ui: &mut egui::Ui, page_idx: usize) {
        if self.layout_editor.is_some() {
            return;
        }
        self.ensure_raster_layers_for_page(page_idx);

        // Indices into `self.overlays` for this page, in array order (== persisted z order).
        let page_overlay_indices: Vec<usize> = self
            .overlays
            .iter()
            .enumerate()
            .filter(|(_, o)| o.page_idx == page_idx)
            .map(|(i, _)| i)
            .collect();

        let raster_count = self
            .raster_layers_by_page
            .get(&page_idx)
            .map_or(0, Vec::len);

        // Build ONE unified, interleaved row list (text + image overlays + rasters) ordered by unified
        // band-Z DESCENDING (top of the stack first). Overlay above raster at equal Z (the canvas/hit-test
        // tie-break). This uses the SAME Z the canvas/hit-test use, so the panel matches what's drawn.
        let mut row_inputs: Vec<(TypingLayerRow, u32, bool)> = Vec::new();
        for &ov_idx in &page_overlay_indices {
            if let Some(o) = self.overlays.get(ov_idx) {
                let z = self.overlay_band_z(page_idx, &o.uid, o.layer_idx);
                row_inputs.push((TypingLayerRow::Overlay(ov_idx), z, false));
            }
        }
        for raster_idx in 0..raster_count {
            if let Some(uid) = self
                .raster_layers_by_page
                .get(&page_idx)
                .and_then(|v| v.get(raster_idx))
                .map(|l| l.uid.clone())
            {
                let z = self.raster_band_z(page_idx, &uid);
                row_inputs.push((TypingLayerRow::Raster(raster_idx), z, true));
            }
        }
        let ordered_rows = order_unified_layer_rows(row_inputs);

        // A single move per frame across BOTH kinds; the row identity carries the kind.
        let mut move_row: Option<(TypingLayerRow, bool)> = None;
        let mut select_overlay: Option<usize> = None;
        let mut select_raster: Option<usize> = None;

        // Representative glyph width + row height from the current font/spacing (not magic numbers).
        // egui's `Fonts*::glyph_width`/`row_height` need a &mut view (only `Painter`/`Ui` text
        // measuring gives it), so measure a 10-glyph run via a galley and divide.
        //
        // NOT localizable: this is a measurement probe, not a label. Its length must stay exactly
        // `PROBE_GLYPHS` or `char_px` below is wrong, and a locale file is user-editable — routing
        // it through `t!` would let a translation silently break this panel's layout.
        const MEASURE_PROBE: &str = "oooooooooo";
        const PROBE_GLYPHS: f32 = 10.0;
        let font_id = egui::TextStyle::Body.resolve(&ui.ctx().global_style());
        let probe = ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(MEASURE_PROBE.to_string(), font_id.clone(), Color32::WHITE)
        });
        let char_px = (probe.rect.width() / PROBE_GLYPHS).max(1.0);
        let line_height = probe.rect.height().max(1.0);
        // A row is a line plus the vertical item spacing between rows.
        let row_height = (line_height + ui.ctx().global_style().spacing.item_spacing.y).max(1.0);
        let list_height = row_height * LAYERS_PANEL_DEFAULT_ROWS as f32;

        // MIN width = overhead + exactly `LAYERS_PANEL_MIN_PREVIEW_CHARS` chars of preview, so at the
        // narrowest the preview shows 5 chars and the panel can't shrink further. Clamp the persisted
        // width up to it.
        let min_width =
            LAYERS_PANEL_ROW_OVERHEAD_PX + LAYERS_PANEL_MIN_PREVIEW_CHARS as f32 * char_px;
        if self.layers_panel_width < min_width {
            self.layers_panel_width = min_width;
        }
        let panel_width = self.layers_panel_width;
        // Preview char budget from the CURRENT width: how many chars fit after the fixed overhead.
        let max_chars = preview_char_budget(panel_width - LAYERS_PANEL_ROW_OVERHEAD_PX, char_px);

        let mut new_width = panel_width;
        // Width-only resize for the list; HEIGHT follows content, capped at ~8 rows by the ScrollArea
        // (`auto_shrink` lets a short list hug). The combined panel's Frame + the «Слои» tab supply the
        // surrounding chrome.
        egui::Resize::default()
            .id_salt("typing_layers_panel_resize")
            .resizable([true, false])
            .default_size(egui::vec2(panel_width, 0.0))
            .min_size(egui::vec2(min_width, 0.0))
            .show(ui, |ui| {
                new_width = ui.available_width().max(min_width);
                egui::ScrollArea::vertical()
                    .max_height(list_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if ordered_rows.is_empty() {
                            ui.weak(t!("typing.layers.no_layers_hint"));
                        }
                        for row in &ordered_rows {
                            match *row {
                                TypingLayerRow::Overlay(ov_idx) => {
                                    let Some(overlay) = self.overlays.get(ov_idx) else {
                                        continue;
                                    };
                                    let label = match overlay.kind {
                                        TypingOverlayKind::Text => {
                                            let text = overlay
                                                .render_data_json
                                                .as_ref()
                                                .and_then(|rd| rd.get("text_params"))
                                                .and_then(|tp| tp.get("text"))
                                                .and_then(Value::as_str)
                                                .unwrap_or("");
                                            let preview = text_preview_label(text, max_chars);
                                            if preview.is_empty() {
                                                t!("typing.layers.text_row_label").to_string()
                                            } else {
                                                tf!("typing.layers.text_row_with_preview_label", preview = preview)
                                            }
                                        }
                                        TypingOverlayKind::Image => t!("typing.layers.image_row_label").to_string(),
                                    };
                                    let selected = self.selected_overlay_idx == Some(ov_idx);
                                    ui.horizontal(|ui| {
                                        if ui.button("⬆").clicked() {
                                            move_row = Some((*row, true));
                                        }
                                        if ui.button("⬇").clicked() {
                                            move_row = Some((*row, false));
                                        }
                                        if ui.selectable_label(selected, label).clicked() {
                                            select_overlay = Some(ov_idx);
                                        }
                                    });
                                }
                                TypingLayerRow::Raster(raster_idx) => {
                                    let Some(layer) = self
                                        .raster_layers_by_page
                                        .get(&page_idx)
                                        .and_then(|v| v.get(raster_idx))
                                    else {
                                        continue;
                                    };
                                    let selected = self.selected_raster_idx == Some(raster_idx);
                                    let label = format!("🖼 {}", layer.name);
                                    ui.horizontal(|ui| {
                                        if ui.button("⬆").clicked() {
                                            move_row = Some((*row, true));
                                        }
                                        if ui.button("⬇").clicked() {
                                            move_row = Some((*row, false));
                                        }
                                        if ui.selectable_label(selected, label).clicked() {
                                            select_raster = Some(raster_idx);
                                        }
                                    });
                                }
                            }
                        }
                    });
            });
        // Persist the (clamped) user-chosen width for next frame.
        self.layers_panel_width = new_width.max(min_width);

        if let Some(idx) = select_overlay {
            self.selected_overlay_idx = Some(idx);
            self.selected_raster_idx = None;
            self.selected_raster_page = None;
            self.transform_mode_raster_idx = None;
        }
        if let Some(idx) = select_raster {
            self.select_raster(page_idx, idx);
        }
        // Apply at most one Z change per frame, routing by row kind. Both move helpers route through the
        // shared doc band reorder, so text and rasters interleave correctly. ⬆ raises one step, ⬇ lowers.
        if let Some((row, up)) = move_row {
            match row {
                TypingLayerRow::Overlay(idx) => {
                    self.move_overlay_in_unified_z(page_idx, idx, up)
                }
                TypingLayerRow::Raster(idx) => {
                    self.move_raster_in_unified_z(page_idx, idx, up)
                }
            }
        }
    }

    /// Draws the floating layout-editor UI while the editor is active. No-op when the editor
    /// is closed. Delegates to the single mode panel, which merges the mode switch, the
    /// vector-lines params, and the preview-opacity slider (params + slider shown only in Editing).
    pub(super) fn draw_layout_editor_panels(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        if self.layout_editor.is_none() {
            return;
        }
        self.draw_layout_editor_mode_panel(ctx, canvas_rect);
    }

    /// Draws the top-left "Редактирование раскладки" panel: title + red "Выйти" + the
    /// Editing/Preview toggle, and — only in Editing — the "Векторные" vector-lines params
    /// (in a bounded scroll area) plus a "Прозрачность превью" slider bound to
    /// `TypingLayoutEditorState::preview_opacity`. The panel is widened in Editing to fit the params.
    /// A settled line-param edit (smoothing / direction / distance mode / flip / add-remove line)
    /// re-renders the layer via `rerender_layout_editor_overlay`, like an on-canvas frame resize.
    pub(super) fn draw_layout_editor_mode_panel(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        let controls_rect =
            ctx.memory(|mem| mem.area_rect(Id::new(CANVAS_LEFT_TOP_CONTROLS_AREA_ID)));
        let default_pos = controls_rect
            .map(|rect| egui::pos2(rect.left(), rect.bottom() + 8.0))
            .unwrap_or(canvas_rect.left_top() + Vec2::new(16.0, 16.0));
        // Editing hosts the merged vector-lines params + opacity slider, so it needs the
        // wide panel; Preview only shows the compact mode switch, so it stays narrow.
        let editing = self.layout_editor_editing_active();
        let panel_width = if editing {
            TEXT_LAYOUT_EDITOR_PANEL_WIDTH_PX
        } else {
            TEXT_LAYOUT_EDITOR_MODE_PANEL_WIDTH_PX
        };
        // Set by the vector-lines params sub-panel when a line param settles this frame; the layer
        // is re-rendered after the panel closes (its `&mut self` borrow must end first).
        let mut params_changed = false;
        egui::Area::new("typing_layout_editor_mode_panel".into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .default_pos(default_pos)
            .show(ctx, |ui| {
                ui.set_width(panel_width);
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_rgba_unmultiplied(36, 36, 44, 240))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(120, 140, 180)))
                    .show(ui, |ui| {
                        ui.set_width(panel_width);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(t!("typing.layout_editor.mode_heading"))
                                    .strong()
                                    .color(Color32::from_rgb(245, 245, 255)),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let exit = egui::Button::new(
                                        egui::RichText::new(t!("typing.layout_editor.exit_button")).strong().color(Color32::WHITE),
                                    )
                                    .fill(Color32::from_rgb(180, 38, 38));
                                    if ui.add(exit).clicked() {
                                        self.exit_layout_editor();
                                    }
                                },
                            );
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let mode = self
                                .layout_editor
                                .as_ref()
                                .map(|editor| editor.mode)
                                .unwrap_or(TypingLayoutEditorMode::Editing);
                            if ui
                                .selectable_label(
                                    mode == TypingLayoutEditorMode::Editing,
                                    t!("typing.layout_editor.edit_mode_tab"),
                                )
                                .clicked()
                            {
                                self.enter_layout_editor_editing();
                            }
                            if ui
                                .selectable_label(
                                    mode == TypingLayoutEditorMode::Preview,
                                    t!("typing.layout_editor.preview_mode_tab"),
                                )
                                .clicked()
                            {
                                self.enter_layout_editor_preview(ctx);
                            }
                        });
                        // The vector-lines params and the on-canvas preview-opacity
                        // control live inside this panel, but only in Editing mode.
                        if editing {
                            ui.separator();
                            ui.label(egui::RichText::new(t!("typing.layout_editor.vector_mode_tab")).strong());
                            egui::ScrollArea::vertical()
                                .max_height(360.0)
                                .show(ui, |ui| {
                                    if let Some(editor) = self.layout_editor.as_mut() {
                                        params_changed |=
                                            draw_layout_editor_vector_lines_tab(ui, editor);
                                    }
                                });
                            ui.separator();
                            ui.label(t!("typing.layout_editor.preview_opacity_label"));
                            if let Some(editor) = self.layout_editor.as_mut() {
                                ui.add(
                                    egui::Slider::new(&mut editor.preview_opacity, 0.0..=1.0)
                                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                                        .show_value(true),
                                );
                            }
                        }
                    });
            });
        // A settled line-param edit on the panel re-renders the layer, matching the on-canvas
        // frame-resize / point-edit path. The `&mut self` borrow held by the panel above has
        // ended here, so re-rendering (which also needs `&mut self`) is safe.
        if params_changed {
            self.rerender_layout_editor_overlay(ctx);
        }
    }

    pub(super) fn begin_layout_editor_for_overlay(&mut self, overlay_idx: usize, image_rect: Rect, zoom: f32) {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return;
        };
        let geometry = overlay_scene_geometry(overlay, image_rect, zoom);
        let page_size = page_size_from_image_rect(image_rect, zoom);
        let saved_vector_layout = overlay.render_data_json.as_ref().and_then(|render_data| {
            text_render_params_from_render_data(render_data)
                .map(|params| params.vector_lines_layout)
        });
        let frame_page_rect = saved_vector_layout
            .as_ref()
            .filter(|layout| {
                layout.width_px > 1 || layout.height_px > 1 || !layout.lines.is_empty()
            })
            .map(|layout| {
                let center = geometry.bounds_rect.center();
                let center_page = page_px_from_scene(image_rect, zoom, center);
                frame_rect_from_center_and_size(
                    Pos2::new(center_page[0], center_page[1]),
                    Vec2::new(
                        layout.width_px.max(1) as f32,
                        layout.height_px.max(1) as f32,
                    ),
                    page_size,
                )
            })
            .unwrap_or_else(|| {
                let min_page = page_px_from_scene(image_rect, zoom, geometry.bounds_rect.min);
                let max_page = page_px_from_scene(image_rect, zoom, geometry.bounds_rect.max);
                Rect::from_min_max(
                    Pos2::new(
                        min_page[0].clamp(0.0, page_size[0].max(1) as f32),
                        min_page[1].clamp(0.0, page_size[1].max(1) as f32),
                    ),
                    Pos2::new(
                        max_page[0].clamp(0.0, page_size[0].max(1) as f32),
                        max_page[1].clamp(0.0, page_size[1].max(1) as f32),
                    ),
                )
            });
        let loaded_lines = saved_vector_layout
            .map(layout_editor_lines_from_vector_layout)
            .filter(|lines| !lines.is_empty())
            .unwrap_or_else(|| {
                vec![TypingLayoutEditorLine {
                    label: t!("typing.layout_editor.line_first_label").to_string(),
                    points: Vec::new(),
                    corner_smoothing_px: 0.0,
                    text_direction: TextVectorLineTextDirection::LeftToRight,
                    distance_mode: TextVectorLineDistanceMode::ByLineLength,
                    flip_text: false,
                }]
            });
        self.layout_editor = Some(TypingLayoutEditorState {
            overlay_idx,
            page_idx: overlay.page_idx,
            frame_page_rect,
            mode: TypingLayoutEditorMode::Editing,
            active_line_idx: 0,
            lines: loaded_lines,
            frame_drag: None,
            line_drag: None,
            preview_opacity: 0.5,
        });
        self.selected_overlay_idx = Some(overlay_idx);
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
        self.drag_has_changes = false;
    }

    pub(super) fn exit_layout_editor(&mut self) {
        if self.edit_render_data_dirty {
            self.request_overlay_placement_save();
            self.edit_render_data_dirty = false;
        }
        self.layout_editor = None;
    }

    pub(super) fn enter_layout_editor_editing(&mut self) {
        if let Some(editor) = self.layout_editor.as_mut() {
            editor.mode = TypingLayoutEditorMode::Editing;
        }
    }

    pub(super) fn enter_layout_editor_preview(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.layout_editor.as_mut() else {
            return;
        };
        editor.mode = TypingLayoutEditorMode::Preview;
        self.rerender_layout_editor_overlay(ctx);
    }

    /// Re-renders the edited overlay from the layout editor's current frame + vector lines
    /// and starts a background edit render job, so the on-canvas layer (Preview text, or the
    /// dimmed Editing-mode preview) reflects the new layout. No-op if the editor is closed or
    /// the overlay is not text. Called both when entering Preview and after a completed
    /// Editing-mode line/frame edit.
    ///
    /// While the editor is open the layer's `center_page_px` is synced to the frame center (here
    /// and in `apply_edit_overlay_render_result`) so the resized layer follows the frame instead
    /// of staying centered on its previous position.
    pub(super) fn rerender_layout_editor_overlay(&mut self, ctx: &egui::Context) {
        let (overlay_idx, vector_layout, frame_center) = match self.layout_editor.as_ref() {
            Some(editor) => (
                editor.overlay_idx,
                vector_lines_layout_from_editor(editor),
                editor.frame_page_rect.center(),
            ),
            None => return,
        };
        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            self.layout_editor = None;
            return;
        };
        if overlay.kind != TypingOverlayKind::Text {
            return;
        }
        let Some(render_data_json) = overlay
            .render_data_json
            .as_ref()
            .and_then(|render_data| render_data_with_vector_layout(render_data, &vector_layout))
        else {
            self.set_create_error(ctx, t!("typing.layout_editor.update_vector_params_error"));
            return;
        };
        let Some(render_params) = text_render_params_from_render_data(&render_data_json) else {
            self.set_create_error(ctx, t!("typing.layout_editor.build_preview_params_error"));
            return;
        };
        let Some(text_images_dir) = self.text_images_save_dir.clone() else {
            self.set_create_error(
                ctx,
                t!("typing.layout_editor.text_images_dir_missing_error"),
            );
            return;
        };

        overlay.render_data_json = Some(render_data_json.clone());
        overlay.user_scale = 1.0;
        overlay.size_px = [
            usize::try_from(vector_layout.width_px).unwrap_or(usize::MAX),
            usize::try_from(vector_layout.height_px).unwrap_or(usize::MAX),
        ];
        // While the layout editor is open, the layer must FOLLOW the editor frame instead of
        // staying centered on its previous position. The on-canvas quad is rebuilt as
        // `Rect::from_center_size(center_page_px, size_px)`, so a size change (frame resize, line
        // edit, or Editing->Preview) that leaves `center_page_px` stale drifts the layer off the
        // frame. Sync the center to the frame center here for the optimistic redraw; the authored
        // center is pushed to the doc when the render result lands (`apply_edit_overlay_render_result`).
        overlay.center_page_px = [frame_center.x, frame_center.y];
        self.edit_render_data_dirty = true;
        let edit_request = TypingEditOverlayRequest {
            token: 0,
            latest_token: Arc::clone(&self.edit_render_latest_token),
            overlay_idx,
            file_name: overlay.file_name.clone(),
            text_images_dir,
            user_scale: 1.0,
            rotation_deg: overlay.angle_deg,
            render_params,
            render_data_json,
            font_provider: Arc::clone(&self.font_provider),
        };
        self.start_edit_overlay_render_job(edit_request);
    }

    /// Draws the on-canvas layout-editor overlay for `page_idx` (Editing sub-mode only): the frame
    /// handles, vector lines/dots, and — painted UNDER the frame at `preview_opacity` — the edited
    /// overlay's current rendered text layer, so the artist can trace vector lines over the real text.
    /// No-op when the editor is closed, in Preview, or bound to a different page.
    pub(super) fn draw_layout_editor_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        clip_rect: Rect,
    ) {
        let Some(editor) = self.layout_editor.as_mut() else {
            return;
        };
        if editor.page_idx != page_idx {
            return;
        }
        if editor.mode != TypingLayoutEditorMode::Editing {
            return;
        }
        if editor.overlay_idx >= self.overlays.len() {
            self.layout_editor = None;
            return;
        }
        ensure_layout_editor_has_line(editor);
        let page_size = page_size_from_image_rect(image_rect, zoom);
        let frame_scene = layout_editor_frame_scene_rect(editor.frame_page_rect, image_rect, zoom);
        let line_rect_response = ui.interact(
            frame_scene,
            Id::new(("typing_layout_editor_lines", editor.overlay_idx)),
            Sense::click_and_drag(),
        );
        let active_line_idx = editor
            .active_line_idx
            .min(editor.lines.len().saturating_sub(1));
        editor.active_line_idx = active_line_idx;
        // Re-render the overlay only once a discrete layout edit settles this frame
        // (point add/move/delete, or frame resize below), never mid-drag.
        let mut needs_rerender = handle_layout_editor_vector_canvas_input(
            editor,
            active_line_idx,
            frame_scene,
            image_rect,
            zoom,
            &line_rect_response,
            ctx,
        );

        let frame_scene = layout_editor_frame_scene_rect(editor.frame_page_rect, image_rect, zoom);
        for (handle, handle_pos) in layout_frame_handle_points(frame_scene) {
            let handle_rect = Rect::from_center_size(
                handle_pos,
                Vec2::splat(TEXT_LAYOUT_EDITOR_FRAME_HANDLE_RADIUS_PX * 4.0),
            );
            let response = ui.interact(
                handle_rect,
                Id::new((
                    "typing_layout_editor_frame_handle",
                    editor.overlay_idx,
                    handle,
                )),
                Sense::drag(),
            );
            let pointer_page = response.interact_pointer_pos().map(|pos| {
                let page = page_px_from_scene(image_rect, zoom, pos);
                Pos2::new(page[0], page[1])
            });
            if response.drag_started()
                && let Some(pointer_page) = pointer_page
            {
                editor.frame_drag = Some(TypingLayoutFrameDragState {
                    handle,
                    pointer_start_page_px: pointer_page,
                    start_rect: editor.frame_page_rect,
                });
            }
            if response.dragged()
                && let (Some(drag), Some(pointer_page)) = (editor.frame_drag, pointer_page)
                && drag.handle == handle
            {
                let delta = pointer_page - drag.pointer_start_page_px;
                editor.frame_page_rect =
                    apply_layout_frame_drag(drag.start_rect, drag.handle, delta, page_size);
                clamp_layout_editor_points_to_frame(editor);
                ctx.request_repaint();
            }
            if response.drag_stopped()
                && editor.frame_drag.is_some_and(|drag| drag.handle == handle)
            {
                editor.frame_drag = None;
                // Frame resize changes the layout box -> re-render the preview layer.
                needs_rerender = true;
            }
        }

        // Copy the disjoint editor fields before touching `self.overlays`: `editor`
        // borrows `self.layout_editor`, so reading `self.overlays` alongside it is
        // allowed under NLL only because these are separate fields of `self`.
        let overlay_idx = editor.overlay_idx;
        let preview_opacity = editor.preview_opacity;

        let painter = ui.painter().with_clip_rect(clip_rect);
        // Render the edited overlay's current rendered text layer dimmed UNDER the
        // frame+dots so the artist can trace the vector lines over the real text.
        // Clamp is proven to keep the product in [0,255]; the cast cannot lose data.
        let alpha = (preview_opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        if alpha > 0
            && let Some(overlay) = self.overlays.get(overlay_idx)
            && let Some(texture) = overlay.texture.as_ref()
        {
            let geometry = overlay_scene_geometry(overlay, image_rect, zoom);
            draw_textured_deform_mesh(
                &painter,
                texture.id(),
                &geometry.mesh_scene,
                geometry.mesh_cols,
                geometry.mesh_rows,
                Color32::from_white_alpha(alpha),
            );
        }
        draw_layout_editor_frame(&painter, frame_scene);
        draw_layout_editor_vector_lines(&painter, frame_scene, zoom, editor);

        // `editor` (and `painter`) are no longer borrowed past this point, so re-rendering
        // (which needs `&mut self`) is safe. Runs after a completed line/frame edit so the
        // dimmed preview texture updates once the drag finishes.
        if needs_rerender {
            self.rerender_layout_editor_overlay(ctx);
        }
    }
}
