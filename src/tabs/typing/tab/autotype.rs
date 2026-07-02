/*
File: tab/autotype.rs

Purpose:
Auto-typing integration for the typing tab: hotkey-triggered bubble detection,
background job polling, applying the detected bubble alignment to the selected
text overlay, and drawing the auto-typing debug overlay visuals.

Notes:
Extracted verbatim from `tab.rs`. Methods are `pub(super)` so `tab.rs` and sibling
submodules of `tab` can use them. `use super::*;` pulls in the parent module's
types and imports. Struct/enum definitions and the rest of the big
`impl TypingTextOverlayLayer` block remain in `tab.rs`; these methods reach the
private items that stay there as descendants of module `tab`.
*/

use super::*;

impl TypingTextOverlayLayer {
    pub(super) fn try_trigger_selected_overlay_auto_typing_by_hotkey(
        &mut self,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        panel_text_input_focused: bool,
        settings: TypingAutoTypingSettings,
    ) {
        if panel_text_input_focused || ctx.wants_keyboard_input() {
            return;
        }
        if self.auto_typing_job.is_some() {
            return;
        }
        if !ctx.input(|input| input.key_pressed(egui::Key::C)) {
            return;
        }

        let Some(clean_model) = self.clean_overlays_model.clone() else {
            self.set_create_error(
                ctx,
                "Авто-тайп недоступен: модель clean overlay не подключена.",
            );
            return;
        };
        let Some(selected_idx) = self.selected_overlay_idx else {
            return;
        };
        let Some(overlay) = self.overlays.get(selected_idx) else {
            return;
        };
        if overlay.kind != TypingOverlayKind::Text || overlay.page_idx != page_idx {
            return;
        }

        let Some(local_center_px) = compute_overlay_visual_center(
            overlay.size_px,
            overlay.source_rgba.as_slice(),
            settings.extra_downward_shift_percent,
        ) else {
            self.set_create_error(
                ctx,
                "Авто-тайп: у оверлея не найден оптический центр (прозрачный слой).",
            );
            return;
        };
        let overlay_tuv = [
            (local_center_px[0] / overlay.size_px[0].max(1) as f32).clamp(0.0, 1.0),
            (local_center_px[1] / overlay.size_px[1].max(1) as f32).clamp(0.0, 1.0),
        ];
        let overlay_file_name = overlay.file_name.clone();
        let quad_scene = overlay_quad_scene(overlay, image_rect, zoom);
        let click_scene = bilinear_quad_point(quad_scene, overlay_tuv[0], overlay_tuv[1]);
        let mut click_uv = uv_from_scene(image_rect, click_scene);
        click_uv[0] = click_uv[0].clamp(0.0, 1.0);
        click_uv[1] = click_uv[1].clamp(0.0, 1.0);
        ctx.input_mut(|input| {
            let _ = input.consume_key(egui::Modifiers::NONE, egui::Key::C);
        });

        self.auto_typing_next_token = self.auto_typing_next_token.wrapping_add(1);
        let token = self.auto_typing_next_token;
        crate::trace_log!(
            cat::SYNC,
            "auto_typing dispatch token={} overlay_idx={} page={} click_uv=({:.3},{:.3})",
            token,
            selected_idx,
            page_idx,
            click_uv[0],
            click_uv[1]
        );
        let (tx, rx) = mpsc::channel::<Result<TypingAutoTypingWorkerResult, String>>();
        thread::spawn(move || {
            let result = detect_bubble_from_overlay_cache(&clean_model, page_idx, click_uv).map(
                |detection| TypingAutoTypingWorkerResult {
                    token,
                    page_idx,
                    click_uv,
                    detection,
                },
            );
            let _ = tx.send(result);
        });

        self.auto_typing_job = Some(TypingAutoTypingJobState {
            rx,
            token,
            overlay_idx: selected_idx,
            overlay_file_name,
            page_idx,
            overlay_optical_tuv: overlay_tuv,
        });
    }

    pub(super) fn poll_auto_typing_job(&mut self, ctx: &egui::Context) -> bool {
        let recv_result = {
            let Some(state) = self.auto_typing_job.as_ref() else {
                return false;
            };
            match state.rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Фоновый авто-тайп завершился с ошибкой канала.".to_string(),
                )),
            }
        };
        let Some(recv_result) = recv_result else {
            return false;
        };

        let Some(job_state) = self.auto_typing_job.take() else {
            return false;
        };
        match recv_result {
            Ok(Ok(result)) => {
                crate::trace_log!(
                    cat::SYNC,
                    "auto_typing result=ok token={} page={}",
                    result.token,
                    result.page_idx
                );
                self.apply_auto_typing_result(ctx, job_state, result)
            }
            Ok(Err(err)) | Err(err) => {
                crate::trace_log!(cat::SYNC, "auto_typing result=err err={}", err);
                self.set_create_error(ctx, err);
                true
            }
        }
    }

    pub(super) fn apply_auto_typing_result(
        &mut self,
        ctx: &egui::Context,
        job: TypingAutoTypingJobState,
        result: TypingAutoTypingWorkerResult,
    ) -> bool {
        if result.token != job.token || result.page_idx != job.page_idx {
            return false;
        }

        self.auto_typing_debug_visual = Some(TypingAutoTypingDebugVisual {
            page_idx: result.page_idx,
            accepted: result.detection.accepted,
            overlay_center_uv: result.click_uv,
            bubble_center_uv: result.detection.bubble_center_uv,
            bubble_bounds_uv: result.detection.bubble_bounds_uv,
            bubble_contour_uv: result.detection.bubble_contour_uv.clone(),
        });

        if !result.detection.accepted {
            self.set_create_error(ctx, format!("Авто-тайп: {}", result.detection.status));
            return true;
        }
        let Some(target_center_uv) = result.detection.bubble_center_uv else {
            self.set_create_error(
                ctx,
                "Авто-тайп: пузырь найден без центра, выравнивание пропущено.",
            );
            return true;
        };

        let page_size = result.detection.page_size;
        let delta_page_px = {
            let Some(overlay) = self.overlays.get(job.overlay_idx) else {
                return true;
            };
            if overlay.file_name != job.overlay_file_name
                || overlay.kind != TypingOverlayKind::Text
                || overlay.page_idx != job.page_idx
            {
                return true;
            }

            let deform_mesh = overlay_deform_mesh_for_page(overlay, page_size);
            let current_center_uv = sample_deform_mesh_uv(
                &deform_mesh,
                job.overlay_optical_tuv[0],
                job.overlay_optical_tuv[1],
                page_size,
            );
            [
                target_center_uv[0] - current_center_uv[0],
                target_center_uv[1] - current_center_uv[1],
            ]
        };
        let delta_page_px = [
            delta_page_px[0] * page_size[0].max(1) as f32,
            delta_page_px[1] * page_size[1].max(1) as f32,
        ];
        if delta_page_px[0].abs() <= 1e-6 && delta_page_px[1].abs() <= 1e-6 {
            return true;
        }

        if let Some(overlay) = self.overlays.get_mut(job.overlay_idx) {
            if let Some(mesh) = overlay.deform_mesh.as_mut() {
                mesh.translate(delta_page_px[0], delta_page_px[1], page_size);
                sync_overlay_center_from_deform_mesh(overlay, page_size);
            } else {
                overlay.center_page_px = clamp_page_point(
                    [
                        overlay.center_page_px[0] + delta_page_px[0],
                        overlay.center_page_px[1] + delta_page_px[1],
                    ],
                    page_size,
                );
            }
        }
        self.mark_overlay_geometry_changed(job.overlay_idx, false);
        self.request_overlay_placement_save();
        true
    }

    pub(super) fn draw_auto_typing_debug_visuals(
        &self,
        painter: &egui::Painter,
        page_idx: usize,
        image_rect: Rect,
        settings: TypingAutoTypingSettings,
    ) {
        if !settings.debug_visuals {
            return;
        }
        let Some(debug) = self.auto_typing_debug_visual.as_ref() else {
            return;
        };
        if debug.page_idx != page_idx {
            return;
        }

        if debug.bubble_contour_uv.len() >= 2 {
            let stroke_color = if debug.accepted {
                Color32::from_rgb(102, 255, 153)
            } else {
                Color32::from_rgb(255, 160, 160)
            };
            for idx in 0..debug.bubble_contour_uv.len() {
                let a_uv = debug.bubble_contour_uv[idx];
                let b_uv = debug.bubble_contour_uv[(idx + 1) % debug.bubble_contour_uv.len()];
                let a = scene_from_uv(image_rect, a_uv[0], a_uv[1]);
                let b = scene_from_uv(image_rect, b_uv[0], b_uv[1]);
                painter.line_segment([a, b], Stroke::new(1.5, stroke_color));
            }
        }

        if let Some(bounds_uv) = debug.bubble_bounds_uv {
            let min = scene_from_uv(image_rect, bounds_uv[0], bounds_uv[1]);
            let max = scene_from_uv(image_rect, bounds_uv[2], bounds_uv[3]);
            let rect = Rect::from_min_max(min, max);
            let stroke_color = if debug.accepted {
                Color32::from_rgba_unmultiplied(140, 255, 140, 120)
            } else {
                Color32::from_rgba_unmultiplied(255, 140, 140, 120)
            };
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(1.0, stroke_color),
                egui::StrokeKind::Outside,
            );
        }

        if let Some(center_uv) = debug.bubble_center_uv {
            let center = scene_from_uv(image_rect, center_uv[0], center_uv[1]);
            let color = Color32::RED;
            painter.line_segment(
                [center + Vec2::new(-8.0, 0.0), center + Vec2::new(8.0, 0.0)],
                Stroke::new(2.0, color),
            );
            painter.line_segment(
                [center + Vec2::new(0.0, -8.0), center + Vec2::new(0.0, 8.0)],
                Stroke::new(2.0, color),
            );
            painter.circle_stroke(center, 12.0, Stroke::new(1.5, color));
        }

        let overlay_center = scene_from_uv(
            image_rect,
            debug.overlay_center_uv[0],
            debug.overlay_center_uv[1],
        );
        let overlay_color = Color32::from_rgb(80, 210, 255);
        painter.line_segment(
            [
                overlay_center + Vec2::new(-6.0, 0.0),
                overlay_center + Vec2::new(6.0, 0.0),
            ],
            Stroke::new(1.5, overlay_color),
        );
        painter.line_segment(
            [
                overlay_center + Vec2::new(0.0, -6.0),
                overlay_center + Vec2::new(0.0, 6.0),
            ],
            Stroke::new(1.5, overlay_color),
        );
    }
}
