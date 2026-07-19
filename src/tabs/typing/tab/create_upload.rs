/*
File: tab/create_upload.rs

Purpose:
Text/image overlay creation UI and GPU texture upload plumbing for the typing
tab. Covers shift-drag selection capture, the inline text editor, dispatching the
create-overlay/create-raster render workers, transient status hints, runtime
overlay insertion into the doc, raster mask-clip preparation, and the
per-overlay texture upload/dirty-tracking helpers on `TypingTextOverlayLayer`.

Notes:
Extracted verbatim from `tab.rs`. Methods are `pub(super)` so `tab.rs` and sibling
submodules of `tab` can use them. `use super::*;` pulls in the parent module's
types and imports. Struct/enum definitions and the rest of the big
`impl TypingTextOverlayLayer` block remain in `tab.rs`; these methods reach the
private items that stay there as descendants of module `tab`.
*/

use super::*;

/// Layer id of the full-canvas Shift+drag selection-capture overlay.
///
/// Kept on [`egui::Order::Middle`] so it sits BELOW the Foreground typing panels (their
/// Wheel widgets keep winning the z-order hit-test over their own rects and keep
/// receiving hover/scroll) but ABOVE the Background canvas content (bare-canvas
/// drag-selection is unaffected). Shared by the spawn site here and the tab's canvas
/// Shift+wheel font handler so both agree on one layer identity.
pub(super) fn shift_drag_capture_layer_id() -> egui::LayerId {
    egui::LayerId::new(
        egui::Order::Middle,
        egui::Id::new("typing_text_create_shift_capture"),
    )
}

impl TypingTextOverlayLayer {
    pub(super) fn wants_canvas_shift_drag_selection(&self, ctx: &egui::Context) -> bool {
        self.create_selection.is_some()
            || self.create_editor.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || ctx.input(|i| i.modifiers.shift)
    }

    pub(super) fn draw_create_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        let now_s = ctx.input(|i| i.time);
        if self
            .create_status_error
            .as_ref()
            .is_some_and(|(_, hide_at)| now_s >= *hide_at)
        {
            self.create_status_error = None;
        }
        if self
            .create_status_warning
            .as_ref()
            .is_some_and(|(_, hide_at)| now_s >= *hide_at)
        {
            self.create_status_warning = None;
        }

        self.capture_shift_drag_selection(ctx, canvas_rect, canvas, project, top_panel);
        self.draw_active_shift_selection(ctx);
        self.draw_text_editor(ctx, project, top_panel);
        self.draw_render_inflight_hint(ctx);
        self.draw_status_error(ctx, canvas_rect);
        self.draw_status_warning(ctx, canvas_rect);
    }

    pub(super) fn capture_shift_drag_selection(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        if self.loading_rx.is_some()
            || self.create_editor.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
        {
            return;
        }
        let shift_down = ctx.input(|i| i.modifiers.shift);
        let selection_active = self.create_selection.is_some();
        if !shift_down && !selection_active {
            return;
        }

        // Middle order (not Foreground): the capture overlay must sit below the Foreground
        // panels so the panels win the z-order hit-test over their own rects and their Wheel
        // widgets keep receiving hover/scroll; Middle still sits above the Background canvas,
        // so drag-selection over bare canvas is unaffected.
        let capture_layer = shift_drag_capture_layer_id();
        egui::Area::new(capture_layer.id)
            .order(capture_layer.order)
            .fixed_pos(canvas_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(canvas_rect.size());
                let local_rect = Rect::from_min_size(Pos2::ZERO, canvas_rect.size());
                let sense = if shift_down {
                    egui::Sense::click_and_drag()
                } else {
                    egui::Sense::hover()
                };
                let response =
                    ui.interact(local_rect, ui.id().with("typing_text_shift_drag"), sense);

                if shift_down
                    && response.drag_started()
                    && let Some(pos) = response.interact_pointer_pos()
                    && contains_any_page(canvas, project, pos)
                {
                    self.create_selection = Some(TypingCreateSelection {
                        start: pos,
                        current: pos,
                    });
                }

                if let Some(selection) = self.create_selection.as_mut()
                    && let Some(pos) = ctx.input(|i| i.pointer.latest_pos())
                {
                    selection.current = pos;
                }

                let should_finish =
                    self.create_selection.is_some() && (response.drag_stopped() || !shift_down);
                if should_finish && let Some(selection) = self.create_selection.take() {
                    let rect = selection.rect();
                    if rect.width() >= TEXT_CREATE_SELECTION_MIN_SIDE_PX
                        && rect.height() >= TEXT_CREATE_SELECTION_MIN_SIDE_PX
                    {
                        self.open_text_editor_for_selection(ctx, canvas, project, top_panel, rect);
                    }
                }
            });
    }

    pub(super) fn draw_active_shift_selection(&self, ctx: &egui::Context) {
        let Some(selection) = self.create_selection else {
            return;
        };
        let rect = selection.rect();
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("typing_text_shift_selection_painter"),
        ));
        painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(245, 210, 60, 52));
        painter.rect_stroke(
            rect,
            0.0,
            Stroke::new(2.0, Color32::from_rgb(245, 210, 60)),
            egui::StrokeKind::Outside,
        );
    }

    pub(super) fn open_text_editor_for_selection(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
        scene_selection_rect: Rect,
    ) {
        let Some((page_idx, page_rect, scene_rect)) =
            resolve_selection_to_page(canvas, project, scene_selection_rect)
        else {
            self.set_create_error(ctx, t!("typing.create.selection_must_cross_page_error"));
            return;
        };

        let width_px = selection_width_in_source_px(canvas, page_idx, page_rect, scene_rect);
        if width_px == 0 {
            self.set_create_error(ctx, t!("typing.create.selection_width_error"));
            return;
        }

        let center_page_px = selection_center_page_px(page_rect, scene_rect, canvas.zoom());
        let seed_text =
            pick_bubble_text_for_selection(&project.bubbles, page_idx, scene_rect, page_rect)
                .unwrap_or_default();

        let mut font_family = None;
        let mut font_size_px = 24.0;
        if let Some(spec) = top_panel.create_editor_font_spec() {
            font_family = self.ensure_editor_font(ctx, &spec);
            font_size_px = spec.ui_font_size_px.clamp(8.0, 128.0);
        }

        self.create_editor = Some(TypingCreateTextEditor {
            page_idx,
            scene_rect,
            center_page_px,
            width_px,
            text: seed_text,
            font_family,
            font_size_px,
            needs_focus: true,
            window_focused_last_frame: ctx.input(|input| input.viewport().focused.unwrap_or(true)),
        });
        self.create_status_error = None;
    }

    pub(super) fn ensure_editor_font(
        &mut self,
        ctx: &egui::Context,
        spec: &TypingEditorFontSpec,
    ) -> Option<egui::FontFamily> {
        let cache_key = (spec.font_path.clone(), spec.face_index);
        if let Some(name) = self.editor_font_cache.get(&cache_key) {
            return Some(egui::FontFamily::Name(name.clone().into()));
        }

        let font_bytes = fs::read(&spec.font_path).ok()?;
        self.editor_font_next_id = self.editor_font_next_id.saturating_add(1);
        let font_name = format!("typing-editor-font-{}", self.editor_font_next_id);
        let mut font_data = egui::FontData::from_owned(font_bytes);
        font_data.index = spec.face_index as u32;
        ctx.add_font(egui::epaint::text::FontInsert::new(
            font_name.as_str(),
            font_data,
            vec![egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Name(font_name.clone().into()),
                priority: egui::epaint::text::FontPriority::Highest,
            }],
        ));
        self.editor_font_cache.insert(cache_key, font_name.clone());
        Some(egui::FontFamily::Name(font_name.into()))
    }

    pub(super) fn draw_text_editor(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        if self.create_editor.is_none() {
            return;
        }

        let editor_rect = {
            let editor = self.create_editor.as_mut().expect("checked above");
            let desired_rect = Rect::from_min_size(
                editor.scene_rect.min,
                egui::vec2(
                    editor.scene_rect.width().max(TEXT_EDITOR_MIN_WIDTH_PX),
                    editor.scene_rect.height().max(TEXT_EDITOR_MIN_HEIGHT_PX),
                ),
            );
            let text_edit_id = Id::new((
                "typing_text_editor_input",
                editor.page_idx,
                editor.scene_rect.min.x.to_bits(),
                editor.scene_rect.min.y.to_bits(),
            ));
            let area_response = egui::Area::new(Id::new((
                "typing_text_editor_area",
                editor.page_idx,
                editor.scene_rect.min.x.to_bits(),
                editor.scene_rect.min.y.to_bits(),
            )))
            .order(egui::Order::Foreground)
            .fixed_pos(desired_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(desired_rect.size());
                ui.set_max_size(desired_rect.size());
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(235, 200, 85)))
                    .show(ui, |ui| {
                        ui.set_min_size(desired_rect.size());
                        let family = editor
                            .font_family
                            .clone()
                            .filter(|family| is_font_family_bound(ctx, family))
                            .unwrap_or(egui::FontFamily::Proportional);
                        let edit = egui::TextEdit::multiline(&mut editor.text)
                            .id(text_edit_id)
                            .font(egui::FontId::new(editor.font_size_px, family))
                            .desired_width(f32::INFINITY)
                            .desired_rows(1)
                            .lock_focus(true)
                            .frame(egui::Frame::NONE);
                        let output = edit.show(ui);
                        let viewport_focused =
                            ctx.input(|input| input.viewport().focused.unwrap_or(true));
                        let clicked_inside_editor = ctx.input(|input| {
                            input.pointer.primary_clicked()
                                && input
                                    .pointer
                                    .interact_pos()
                                    .is_some_and(|pos| desired_rect.contains(pos))
                        });
                        if editor.needs_focus
                            || (viewport_focused && !editor.window_focused_last_frame)
                            || (clicked_inside_editor && !output.response.has_focus())
                        {
                            output.response.request_focus();
                            editor.needs_focus = false;
                        }
                        editor.window_focused_last_frame = viewport_focused;
                    });
            });
            area_response.response.rect
        };

        let clicked_outside = ctx.input(|i| {
            i.pointer.primary_clicked()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|pos| !editor_rect.contains(pos))
        });
        if clicked_outside && let Some(finished_editor) = self.create_editor.take() {
            self.start_create_overlay_render(ctx, project, top_panel, finished_editor);
        }
    }

    pub(super) fn start_create_overlay_render(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
        editor: TypingCreateTextEditor,
    ) {
        if editor.text.trim().is_empty() {
            self.create_status_error = None;
            return;
        }

        let (render_params, render_data_json) =
            match top_panel.build_create_text_render_bundle(editor.text.clone(), editor.width_px) {
                Ok(bundle) => bundle,
                Err(err) => {
                    self.set_create_error(ctx, err);
                    return;
                }
            };

        let request = TypingCreateOverlayRequest {
            text_images_dir: project.paths.unsaved_layers_dir.clone(),
            page_idx: editor.page_idx,
            center_page_px: editor.center_page_px,
            render_params,
            render_data_json,
            font_provider: Arc::clone(&self.font_provider),
        };
        crate::trace_log!(
            cat::SYNC,
            "create_overlay_render dispatch page={} center=({:.1},{:.1}) width_px={}",
            editor.page_idx,
            editor.center_page_px[0],
            editor.center_page_px[1],
            editor.width_px
        );
        let (tx, rx) = mpsc::channel::<Result<TypingOverlayDecoded, String>>();
        thread::spawn(move || {
            let result = render_and_store_created_overlay(request);
            let _ = tx.send(result);
        });
        self.create_render_state = Some(TypingCreateRenderState {
            rx,
            scene_rect: Some(editor.scene_rect),
        });
        self.create_status_error = None;
    }

    pub(super) fn request_create_image_overlay(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        page_idx: usize,
        center_page_px: [f32; 2],
        request: TypingCreateImageRequest,
    ) {
        if self.loading_rx.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.create_raster_state.is_some()
        {
            self.set_create_error(ctx, t!("typing.create.wait_current_operation_error"));
            return;
        }
        if project.pages.is_empty() {
            self.set_create_error(ctx, t!("typing.create.no_pages_error"));
            return;
        }
        let target_page_idx = page_idx.min(project.pages.len().saturating_sub(1));
        let source = match request {
            TypingCreateImageRequest::FromClipboard => TypingCreateImageSource::Clipboard,
            TypingCreateImageRequest::FromFile(path) => TypingCreateImageSource::File(path),
        };
        // DATA-SAFETY (anti-resurrection): the worker's `add_page_raster` seeds an unstaged page from the
        // COMMITTED manifest (so a typeset page keeps its text — the drop fix). But committed is STALE
        // w.r.t. an in-session deletion: when the user deleted the page's LAST text, the placement-save
        // skipped the now-empty page (`pages_with_text` no longer lists it), so the deletion lived only
        // in the doc. Seeding committed would RESURRECT it. Fix: flush the target page's CURRENT doc text
        // to staging NOW (main thread, has the doc) — for a deleted-last-text page this writes it
        // PRESENT-but-EMPTY, so `ensure_page_staged` sees the page present and does NOT seed stale text;
        // for a typeset page it writes the current text, which the new raster is then added on top of.
        self.flush_target_page_text_to_staging(target_page_idx);

        // External images now become RASTER layers (in layers.json), not text/image overlays, so
        // they are first-class in both the typing and PS editor tabs.
        let create_request = TypingCreateRasterRequest {
            layers_dir: project.paths.unsaved_layers_dir.clone(),
            fallback_dir: Some(project.paths.layers_dir.clone()),
            page_idx: target_page_idx,
            center_page_px,
            source,
        };
        let (tx, rx) = mpsc::channel::<Result<TypingCreatedRaster, String>>();
        thread::spawn(move || {
            let _ = tx.send(render_and_store_created_raster(create_request));
        });
        self.create_raster_state = Some(TypingCreateRasterState { rx });
        self.create_status_error = None;
    }

    pub(super) fn draw_render_inflight_hint(&self, ctx: &egui::Context) {
        let Some(state) = self.create_render_state.as_ref() else {
            return;
        };
        let Some(scene_rect) = state.scene_rect else {
            return;
        };
        let hint_pos = scene_rect.center() - egui::vec2(76.0, 18.0);
        egui::Area::new("typing_text_editor_render_hint".into())
            .order(egui::Order::Foreground)
            .fixed_pos(hint_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("typing.create.rendering_text_hint"));
                    });
                });
            });
    }

    pub(super) fn draw_status_error(&self, ctx: &egui::Context, canvas_rect: Rect) {
        let Some((message, _)) = self.create_status_error.as_ref() else {
            return;
        };
        egui::Area::new("typing_text_editor_error".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-220.0, 16.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(240, 110, 110)))
                    .show(ui, |ui| {
                        ui.colored_label(Color32::from_rgb(240, 110, 110), message);
                    });
            });
    }

    pub(super) fn draw_status_warning(&self, ctx: &egui::Context, canvas_rect: Rect) {
        let Some((message, _)) = self.create_status_warning.as_ref() else {
            return;
        };
        egui::Area::new("typing_text_editor_warning".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-220.0, 52.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(232, 188, 66)))
                    .show(ui, |ui| {
                        ui.colored_label(Color32::from_rgb(232, 188, 66), message);
                    });
            });
    }

    pub(super) fn set_create_error(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        let now_s = ctx.input(|i| i.time);
        self.create_status_error = Some((message.into(), now_s + TEXT_EDITOR_STATUS_ERROR_SECONDS));
    }

    pub(super) fn set_create_warning(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        let now_s = ctx.input(|i| i.time);
        self.create_status_warning =
            Some((message.into(), now_s + TEXT_EDITOR_STATUS_ERROR_SECONDS));
    }

    pub(super) fn insert_runtime_overlay(&mut self, decoded: TypingOverlayDecoded) {
        let idx = self.overlays.len();
        // Build the doc Text node for a TEXT overlay (the doc is the source of truth, so it joins the
        // unified Z stack and re-projects like the rest). Image overlays remain local-only → no node.
        //
        // CRITICAL ordering: build the node here, but ADD it to the doc only AFTER the runtime is pushed
        // into `self.overlays` (below). `route_to_doc` reprojects via `sync_from_doc`, whose CREATE/None
        // branch MATERIALIZES a runtime for any doc Text node that has no matching local runtime yet. If
        // we added the node before pushing the runtime, that branch would create a SECOND runtime for the
        // same uid — a duplicate text layer (one doc-backed, one orphaned). The duplicate is invisible at
        // create time (both render the same image, perfectly overlapping) but becomes visible on the
        // first advanced-form apply: `sync_from_doc` reconciles only the FIRST uid match, leaving the
        // other stuck on the pre-form render.
        let pending_text_node = if decoded.kind == TypingOverlayKind::Text
            && decoded.size_px[0] > 0
            && decoded.size_px[1] > 0
            && decoded.rgba.len() == decoded.size_px[0] * decoded.size_px[1] * 4
        {
            use crate::models::layer_model::layer_doc::{LayerNode, NodeBody, NodeKind};
            let page_idx = decoded.page_idx;
            let uid = decoded.uid.clone();
            let name = decoded
                .render_data_json
                .as_ref()
                .and_then(|v| v.get("text"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(40).collect::<String>())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| t!("typing.layers.text_row_label").to_string());
            let transform = crate::models::layer_model::manifest::TransformRec {
                cx: decoded.center_page_px[0],
                cy: decoded.center_page_px[1],
                rotation: decoded.angle_deg.to_radians(),
                scale: decoded.user_scale,
            };
            let deform = decoded.deform_mesh.as_ref().map(|m| {
                crate::models::layer_model::manifest::DeformRec {
                    cols: m.cols,
                    rows: m.rows,
                    points_px: m.points_px.clone(),
                }
            });
            let image =
                ColorImage::from_rgba_unmultiplied(decoded.size_px, decoded.rgba.as_slice());
            let render_data = decoded.render_data_json.clone().unwrap_or(Value::Null);
            let node = LayerNode {
                uid: uid.clone(),
                name,
                kind: NodeKind::Text,
                z: 0, // set on top by add_node
                visible: true,
                opacity: 1.0,
                group_uid: None,
                // The typing tab's «Группа текста N» axis — carried so the doc flush persists it.
                text_layer_idx: u32::try_from(decoded.layer_idx).ok(),
                transform,
                deform,
                generation: 0,
                // A freshly rendered overlay: mark dirty so the doc flush writes its rendered PNG.
                pixels_dirty: true,
                body: NodeBody::Text {
                    render_data,
                    image,
                    is_image: matches!(decoded.kind, TypingOverlayKind::Image),
                    payload_uid: uid,
                    // Carry the overlay's mask-clip flag so the v3 inline payload persists it.
                    mask_clip: Some(decoded.mask_clip_enabled),
                },
            };
            Some((page_idx, node))
        } else {
            None
        };
        self.overlays.push(TypingOverlayRuntime {
            uid: decoded.uid,
            kind: decoded.kind,
            page_idx: decoded.page_idx,
            center_page_px: decoded.center_page_px,
            mask_clip_enabled: decoded.mask_clip_enabled,
            layer_idx: decoded.layer_idx,
            user_scale: decoded.user_scale,
            angle_deg: decoded.angle_deg,
            deform_mesh: decoded.deform_mesh,
            file_name: decoded.file_name,
            original_file_name: decoded.original_file_name,
            render_data_json: decoded.render_data_json,
            size_px: decoded.size_px,
            source_rgba: decoded.rgba,
            // Carry the freshly-created overlay's mean/median centers for the centering-assist
            // marker/binding (all-`None` unless assist requested them).
            extra: decoded.extra,
            centering_frame: None,
            texture: None,
            display_texture_stale: true,
            last_texture_used_frame: 0,
        });
        // Now that the runtime is in `self.overlays`, add the doc node. `route_to_doc`'s reproject finds
        // the runtime by uid and RECONCILES it (no duplicate materialized). See the ordering note above.
        if let Some((page_idx, node)) = pending_text_node {
            self.route_to_doc(page_idx, move |doc| {
                doc.add_node(page_idx, node);
            });
        }
        self.queue_overlay_texture_upload(idx);
        self.selected_overlay_idx = Some(idx);
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
    }

    /// Computes the mask-clipped DISPLAY image for every mask-clip-enabled raster whose clipped image
    /// is not yet cached, and drops its GPU texture so `draw_one_raster_layer` re-uploads the clipped
    /// version. Runs before the overlay upload (which already has the mask layer). Mirrors the overlay
    /// clip path (`clip_overlay_rgba_if_needed` with the layer's deform mesh as page-relative UV; an
    /// affine raster uses an identity quad mesh derived from its transform).
    pub(super) fn prepare_raster_mask_clips(&mut self, mask_layer: &TypingMaskLayer) {
        let pages: Vec<usize> = self.raster_layers_by_page.keys().copied().collect();
        for page_idx in pages {
            let Some(page_size) = mask_layer.page_mask_size(page_idx) else {
                continue;
            };
            let Some(layers) = self.raster_layers_by_page.get_mut(&page_idx) else {
                continue;
            };
            for layer in layers.iter_mut() {
                if !layer.mask_clip_enabled {
                    layer.clipped_image = None;
                    continue;
                }
                if layer.clipped_image.is_some() {
                    continue; // already computed for this generation
                }
                let [w, h] = layer.image.size;
                if w == 0 || h == 0 {
                    continue;
                }
                // Deform mesh in page-relative UV (the raster's mesh, or an identity quad for affine).
                let mesh = match &layer.deform {
                    Some(rec) => TypingOverlayDeformMesh::from_deform_rec(rec, page_size),
                    None => Some(default_deform_mesh_for_page(
                        [layer.transform.cx, layer.transform.cy],
                        layer.image.size,
                        layer.transform.scale,
                        layer.transform.rotation.to_degrees(),
                        page_size,
                    )),
                };
                let Some(mesh) = mesh else { continue };
                let points_uv: Vec<[f32; 2]> = mesh
                    .points_px
                    .iter()
                    .map(|&p| page_px_to_uv(p, page_size))
                    .collect();
                let src_rgba = color_image_to_rgba(&layer.image);
                if let Some(clipped) = mask_layer.clip_overlay_rgba_if_needed(
                    page_idx,
                    [w, h],
                    &src_rgba,
                    mesh.cols,
                    mesh.rows,
                    &points_uv,
                ) {
                    layer.clipped_image =
                        Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &clipped));
                    // Force re-upload with the clipped pixels.
                    layer.texture = None;
                }
            }
        }
    }

    pub(super) fn upload_pending_textures(
        &mut self,
        ctx: &egui::Context,
        mask_layer: &TypingMaskLayer,
    ) -> bool {
        self.prepare_raster_mask_clips(mask_layer);
        let mut uploaded_any = false;
        let mut uploaded_textures = 0usize;
        let mut uploaded_bytes = 0usize;

        while uploaded_textures < TEXT_OVERLAY_UPLOAD_TEXTURE_BUDGET_PER_FRAME
            && uploaded_bytes < TEXT_OVERLAY_UPLOAD_BYTES_BUDGET_PER_FRAME
        {
            let Some(idx) = self.pending_upload_indices.pop_front() else {
                break;
            };
            self.pending_upload_set.remove(&idx);
            let Some(overlay) = self.overlays.get_mut(idx) else {
                continue;
            };
            if overlay.texture.is_some() && !overlay.display_texture_stale {
                continue;
            }
            if overlay.source_rgba.is_empty() {
                continue;
            };
            if overlay.size_px[0] == 0 || overlay.size_px[1] == 0 {
                continue;
            }
            if overlay.source_rgba.len() != overlay.size_px[0] * overlay.size_px[1] * 4 {
                continue;
            }

            let display_rgba = if overlay.mask_clip_enabled {
                if let Some(page_size) = mask_layer.page_mask_size(overlay.page_idx) {
                    let deform_mesh = overlay_deform_mesh_for_page(overlay, page_size);
                    let deform_mesh_points_uv = deform_mesh
                        .points_px
                        .iter()
                        .map(|&point| page_px_to_uv(point, page_size))
                        .collect::<Vec<_>>();
                    mask_layer
                        .clip_overlay_rgba_if_needed(
                            overlay.page_idx,
                            overlay.size_px,
                            &overlay.source_rgba,
                            deform_mesh.cols,
                            deform_mesh.rows,
                            deform_mesh_points_uv.as_slice(),
                        )
                        .unwrap_or_else(|| overlay.source_rgba.clone())
                } else {
                    overlay.source_rgba.clone()
                }
            } else {
                overlay.source_rgba.clone()
            };

            let image = egui::ColorImage::from_rgba_unmultiplied(
                [overlay.size_px[0], overlay.size_px[1]],
                &display_rgba,
            );
            if let Some(texture) = overlay.texture.as_mut() {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                let texture = ctx.load_texture(
                    format!(
                        "typing-text-overlay-{}-{}-{}",
                        overlay.page_idx, idx, overlay.file_name
                    ),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                overlay.texture = Some(texture);
            }
            overlay.display_texture_stale = false;

            uploaded_any = true;
            uploaded_textures += 1;
            uploaded_bytes += display_rgba.len();
        }

        if uploaded_any {
            crate::trace_log!(
                cat::RENDER,
                "upload_overlay_textures count={} bytes={} pending_remaining={}",
                uploaded_textures,
                uploaded_bytes,
                self.pending_upload_indices.len()
            );
        }
        uploaded_any
    }

    pub(super) fn ensure_overlay_deform_mesh(&mut self, overlay_idx: usize, view: PageView) -> bool {
        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            return false;
        };
        let page_size = view.page_size_px();
        if overlay.deform_mesh.is_none() {
            overlay.deform_mesh = Some(default_overlay_deform_mesh(overlay, view));
        } else if let Some(mesh) = overlay.deform_mesh.as_ref() {
            let normalized = normalize_deform_mesh_resolution(mesh, page_size);
            if &normalized != mesh {
                overlay.deform_mesh = Some(normalized);
            }
        }
        sync_overlay_center_from_deform_mesh(overlay, page_size);
        true
    }

    pub(super) fn queue_overlay_texture_upload(&mut self, idx: usize) {
        if idx >= self.overlays.len() {
            return;
        }
        if self.pending_upload_set.insert(idx) {
            self.pending_upload_indices.push_back(idx);
        }
    }

    pub(super) fn mark_overlay_pixels_dirty(&mut self, idx: usize) {
        if let Some(overlay) = self.overlays.get_mut(idx) {
            overlay.display_texture_stale = true;
        } else {
            return;
        }
        self.queue_overlay_texture_upload(idx);
    }

    pub(super) fn mark_overlay_geometry_changed(&mut self, idx: usize, defer_mask_refresh: bool) {
        let should_refresh = if let Some(overlay) = self.overlays.get_mut(idx) {
            if !overlay.mask_clip_enabled {
                false
            } else {
                overlay.display_texture_stale = true;
                true
            }
        } else {
            return;
        };
        if should_refresh && !defer_mask_refresh {
            self.queue_overlay_texture_upload(idx);
        }
    }

    pub(super) fn flush_overlay_texture_if_stale(&mut self, idx: usize) {
        if self
            .overlays
            .get(idx)
            .is_some_and(|overlay| overlay.display_texture_stale)
        {
            self.queue_overlay_texture_upload(idx);
        }
    }

    pub(super) fn mark_page_texture_dirty(&mut self, page_idx: usize) {
        for idx in 0..self.overlays.len() {
            if self.overlays[idx].page_idx == page_idx && self.overlays[idx].mask_clip_enabled {
                self.mark_overlay_pixels_dirty(idx);
            }
        }
    }
}
