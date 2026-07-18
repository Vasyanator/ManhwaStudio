/*
File: tab/vector_transform.rs

Purpose:
On-canvas VECTOR transform mode for TEXT overlays (Phase 3a + 3b). Lets the user reshape a text overlay
with the SAME deform handles / brushes / mode panel as the raster transform, but the result is baked
into the overlay's `render_data.text_params.raster_transform` (a `VectorMeshWarp`) and applied by the
text renderer on re-render — not the raster post-process `deform_mesh`.

Main responsibilities:
- seed the transient 13x13 working mesh from the overlay's oriented source-rect footprint (identity, or
  resampled from a stored warp) on ENTER;
- own the vector-transform pointer interaction (handle / brush / whole-mesh-move drags) against the
  working mesh;
- Phase 3b LIVE PREVIEW: render (once, off-thread) the UN-WARPED base of the overlay and, during an
  active drag, texture it onto the working mesh via `draw_textured_deform_mesh` so the text bends in
  real time; the plain baked PNG is hidden for that overlay while the warped preview draws. Falls back
  to the Phase-3a wireframe-only draw while the base is not ready. The un-warped base is REQUIRED so the
  mesh warp maps un-warped → warped exactly once (texturing the already-warped baked PNG would
  double-warp);
- LIVE during a drag AND on SETTLE, convert the working page-px mesh to normalized `points_norm`, inject
  the warp into the overlay's `render_data_json`, and trigger the background edit-render
  (`inject_working_mesh_and_rerender`, shared by both). The live path dispatches the real sharp warped
  re-render every frame the mesh changes (latest-wins via `edit_render_latest_token`; the placement save
  coalesces), so the text re-renders crisply in near-real-time; `drag_stopped` does a final settle +
  placement save for the persisted result;
- RESET (remove the warp) and EXIT (clear transient state incl. the base texture).

Key methods:
- seed_vector_transform_mesh / vector_identity_working_mesh
- request_vector_transform_base / poll_vector_transform_base_render (un-warped base GPU cache)
- draw_vector_transform_overlay (interaction + live per-frame re-render + draw, incl. the live
  textured-mesh preview)
- inject_working_mesh_and_rerender (shared convert→inject→dispatch, used by live + settle)
- settle_vector_transform / reset_vector_transform / exit_vector_transform_mode

Notes:
The two transforms COMPOSE: the vector warp is baked into `source_rgba` by the renderer, and the raster
`deform_mesh` still post-processes on top unchanged. This module never touches the raster deform path.
The base texture is a reconstructable GPU cache (kept resident as RGBA, re-uploaded/re-rendered if
lost) held only for the duration of a vector-transform session.
`use super::*;` pulls in the parent module's types, constants, and the pure `mesh_geometry` helpers.
*/

use super::*;

impl TypingTextOverlayLayer {
    /// Content-px source-rect size used to normalize the vector warp for `overlay`: the stored
    /// `raster_transform` src dims when both are `> 0`, else the un-warped baked PNG `size_px`.
    fn vector_transform_src_dims(overlay: &TypingOverlayRuntime) -> [f32; 2] {
        let stored = overlay
            .render_data_json
            .as_ref()
            .and_then(|rd| rd.get("text_params"))
            .and_then(|tp| tp.get("raster_transform"))
            .and_then(decode_vector_mesh_warp);
        if let Some(warp) = stored.as_ref()
            && warp.src_width_px > 0.0
            && warp.src_height_px > 0.0
        {
            return [warp.src_width_px, warp.src_height_px];
        }
        [
            overlay.size_px[0].max(1) as f32,
            overlay.size_px[1].max(1) as f32,
        ]
    }

    /// Build the identity 13x13 working mesh over `overlay`'s oriented source-rect footprint, using the
    /// given content-px source dims. Every lattice node sits at its identity footprint position.
    fn vector_identity_working_mesh(
        overlay: &TypingOverlayRuntime,
        src_px: [f32; 2],
        page_size: [usize; 2],
    ) -> TypingOverlayDeformMesh {
        Self::vector_working_mesh_from_norm_sampler(overlay, src_px, page_size, |u, v| [u, v])
    }

    /// Build the 13x13 working mesh over the footprint, mapping each identity node `(u, v)` through
    /// `warped_norm` (identity for a fresh seed, or a resampler of a stored warp) before placing it on
    /// the footprint. Falls back to a page-centered default mesh only if construction degenerates.
    fn vector_working_mesh_from_norm_sampler(
        overlay: &TypingOverlayRuntime,
        src_px: [f32; 2],
        page_size: [usize; 2],
        mut warped_norm: impl FnMut(f32, f32) -> [f32; 2],
    ) -> TypingOverlayDeformMesh {
        let cols = TEXT_OVERLAY_DEFORM_SURFACE_COLS;
        let rows = TEXT_OVERLAY_DEFORM_SURFACE_ROWS;
        let mut points_px = Vec::with_capacity(cols * rows);
        for row in 0..rows {
            let v = row as f32 / (rows - 1) as f32;
            for col in 0..cols {
                let u = col as f32 / (cols - 1) as f32;
                let [wu, wv] = warped_norm(u, v);
                points_px.push(vector_footprint_page_point(
                    overlay.center_page_px,
                    src_px[0],
                    src_px[1],
                    overlay.user_scale.max(0.01),
                    overlay.angle_deg,
                    wu,
                    wv,
                ));
            }
        }
        TypingOverlayDeformMesh::new(cols, rows, points_px, page_size).unwrap_or_else(|| {
            default_deform_mesh_for_page(
                overlay.center_page_px,
                overlay.size_px,
                overlay.user_scale,
                overlay.angle_deg,
                page_size,
            )
        })
    }

    /// Seed the transient vector working mesh on ENTER of vector transform mode for `overlay_idx`.
    /// Captures the source-rect dims, then seeds the 13x13 mesh from a stored warp (resampled onto the
    /// lattice) or an identity grid. Returns `false` (leaving state untouched) if the overlay is gone.
    pub(super) fn seed_vector_transform_mesh(
        &mut self,
        overlay_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) -> bool {
        let page_size = page_size_from_image_rect(image_rect, zoom);
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return false;
        };
        let src_px = Self::vector_transform_src_dims(overlay);
        // Resample a stored warp of arbitrary resolution onto the fixed 13x13 lattice; identity when
        // there is no stored warp.
        let stored = overlay
            .render_data_json
            .as_ref()
            .and_then(|rd| rd.get("text_params"))
            .and_then(|tp| tp.get("raster_transform"))
            .and_then(decode_vector_mesh_warp);
        let mesh = match stored.as_ref() {
            Some(warp) => Self::vector_working_mesh_from_norm_sampler(
                overlay,
                src_px,
                page_size,
                |u, v| sample_points_norm_bilinear(&warp.points_norm, warp.cols, warp.rows, u, v),
            ),
            None => Self::vector_identity_working_mesh(overlay, src_px, page_size),
        };
        self.vector_transform_src_px = src_px;
        self.vector_transform_mesh = Some(mesh);
        self.vector_transform_drag = None;
        true
    }

    /// Clear all transient vector-transform state (working mesh + active drag + the Phase-3b un-warped
    /// base texture/render) and reset the mode kind to `Raster`. Bumps the base-render token so an
    /// in-flight base render is superseded. Leaves `transform_mode_overlay_idx`/selection to the caller.
    pub(super) fn exit_vector_transform_mode(&mut self) {
        self.transform_mode_kind = TypingTransformModeKind::Raster;
        self.vector_transform_mesh = None;
        self.vector_transform_drag = None;
        self.vector_transform_base = None;
        self.vector_transform_base_rx = None;
        // Invalidate any in-flight base render so its (now stale) result is dropped on arrival.
        self.vector_base_render_token
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Request the UN-WARPED base texture for the live vector preview of `overlay_idx` (Phase 3b).
    ///
    /// Shortcut: when the overlay has NO stored `raster_transform`, its current `source_rgba`/`texture`
    /// ALREADY is the un-warped base, so it is reused directly with no extra render. Otherwise a one-off
    /// off-thread render is dispatched with the warp cleared (`render_text_to_image`, no disk write);
    /// `poll_vector_transform_base_render` installs the result. Never blocks the GUI thread.
    pub(super) fn request_vector_transform_base(&mut self, overlay_idx: usize) {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return;
        };
        if overlay.kind != TypingOverlayKind::Text {
            return;
        }
        let has_warp = overlay
            .render_data_json
            .as_ref()
            .and_then(|rd| rd.get("text_params"))
            .and_then(|tp| tp.get("raster_transform"))
            .and_then(decode_vector_mesh_warp)
            .is_some();
        // No stored warp ⇒ the resident un-warped pixels/texture are the base: reuse them, no render.
        if !has_warp {
            if overlay.source_rgba.is_empty()
                || overlay.size_px[0] == 0
                || overlay.size_px[1] == 0
                || overlay.source_rgba.len() != overlay.size_px[0] * overlay.size_px[1] * 4
            {
                return;
            }
            self.vector_transform_base = Some(TypingVectorTransformBaseTexture {
                overlay_idx,
                size_px: overlay.size_px,
                rgba: overlay.source_rgba.clone(),
                // Reuse the already-uploaded display texture (avoids a re-upload); the resident `rgba`
                // above lets `draw_vector_transform_overlay` re-upload if the handle is later evicted.
                texture: overlay.texture.clone(),
            });
            self.vector_transform_base_rx = None;
            return;
        }
        // Stored warp ⇒ render the overlay with the warp CLEARED so the mesh warp is applied once.
        let Some(render_data) = overlay.render_data_json.as_ref() else {
            return;
        };
        let Some(mut render_params) = text_render_params_from_render_data(render_data) else {
            return;
        };
        render_params.raster_transform = None;
        let token = self
            .vector_base_render_token
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let request = TypingVectorBaseRenderRequest {
            token,
            latest_token: Arc::clone(&self.vector_base_render_token),
            overlay_idx,
            render_params,
            font_provider: Arc::clone(&self.font_provider),
        };
        let (tx, rx) = mpsc::channel::<Result<Option<TypingVectorBaseRenderResult>, String>>();
        thread::spawn(move || {
            let _ = tx.send(render_vector_transform_base(request));
        });
        // Drop any previously reused/rendered base while the fresh render is in flight; the drag falls
        // back to the wireframe-only preview until it lands.
        self.vector_transform_base = None;
        self.vector_transform_base_rx = Some(rx);
    }

    /// Poll the one-off un-warped base render. Installs the result (texture uploaded lazily on draw)
    /// when it is the latest render for the overlay still in VECTOR transform mode. Returns `true` when
    /// a repaint is warranted.
    pub(super) fn poll_vector_transform_base_render(&mut self, _ctx: &egui::Context) -> bool {
        let recv = {
            let Some(rx) = self.vector_transform_base_rx.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    t!("typing.vector_transform.base_render_channel_error").to_string(),
                )),
            }
        };
        let Some(recv) = recv else {
            return false;
        };
        self.vector_transform_base_rx = None;
        match recv {
            Ok(Ok(Some(result))) => {
                // Reject a superseded render or one for an overlay no longer in vector transform mode.
                if self.vector_base_render_token.load(Ordering::Acquire) != result.token {
                    return false;
                }
                if self.transform_mode_kind != TypingTransformModeKind::Vector
                    || self.transform_mode_overlay_idx != Some(result.overlay_idx)
                {
                    return false;
                }
                self.vector_transform_base = Some(TypingVectorTransformBaseTexture {
                    overlay_idx: result.overlay_idx,
                    size_px: result.size_px,
                    rgba: result.rgba,
                    texture: None,
                });
                true
            }
            Ok(Ok(None)) => false,
            Ok(Err(err)) | Err(err) => {
                crate::runtime_log::log_warn(format!("[typing] vector base render: {err}"));
                false
            }
        }
    }

    /// Ensure the un-warped base texture for `overlay_idx` is uploaded and return its `TextureId`, or
    /// `None` when no base pixels are available. Uploads lazily on the GUI thread from the resident
    /// `rgba`, so an evicted or not-yet-uploaded handle is rebuilt without a re-render.
    fn ensure_vector_base_texture_uploaded(
        &mut self,
        ctx: &egui::Context,
        overlay_idx: usize,
    ) -> Option<egui::TextureId> {
        let base = self.vector_transform_base.as_mut()?;
        if base.overlay_idx != overlay_idx {
            return None;
        }
        if base.texture.is_none() {
            if base.rgba.is_empty()
                || base.size_px[0] == 0
                || base.size_px[1] == 0
                || base.rgba.len() != base.size_px[0] * base.size_px[1] * 4
            {
                return None;
            }
            let image = egui::ColorImage::from_rgba_unmultiplied(base.size_px, base.rgba.as_slice());
            base.texture = Some(ctx.load_texture(
                format!("typing-vector-transform-base-{overlay_idx}"),
                image,
                egui::TextureOptions::LINEAR,
            ));
        }
        base.texture.as_ref().map(egui::TextureHandle::id)
    }

    /// Whether the live warped-texture preview should be drawn for `overlay_idx` this frame: VECTOR
    /// mode, an active drag on that overlay, and an un-warped base with pixels available (the texture
    /// may still need upload). When `true`, `draw_page_overlays` HIDES the plain baked PNG for that
    /// overlay so it is not double-drawn under the warped preview.
    pub(super) fn vector_transform_preview_active(&self, overlay_idx: usize) -> bool {
        self.transform_mode_kind == TypingTransformModeKind::Vector
            && self.transform_mode_overlay_idx == Some(overlay_idx)
            && self
                .vector_transform_drag
                .as_ref()
                .is_some_and(|drag| drag.overlay_idx == overlay_idx)
            && self
                .vector_transform_base
                .as_ref()
                .is_some_and(|base| base.overlay_idx == overlay_idx && !base.rgba.is_empty())
    }

    /// Convert the current vector working mesh to a normalized `points_norm` grid over `overlay`'s
    /// oriented footprint (using the captured `vector_transform_src_px`). Row-major, `13*13`.
    fn vector_working_points_norm(&self, overlay: &TypingOverlayRuntime) -> Option<Vec<[f32; 2]>> {
        let mesh = self.vector_transform_mesh.as_ref()?;
        let src = self.vector_transform_src_px;
        let scale = overlay.user_scale.max(0.01);
        Some(
            (0..mesh.rows)
                .flat_map(|row| (0..mesh.cols).map(move |col| (col, row)))
                .map(|(col, row)| {
                    vector_footprint_local_uv(
                        overlay.center_page_px,
                        src[0],
                        src[1],
                        scale,
                        overlay.angle_deg,
                        mesh.point(col, row),
                    )
                })
                .collect(),
        )
    }

    /// Build the `raster_transform` JSON object for the current working mesh, or `None` if no working
    /// mesh / overlay. Shape: `{cols,rows,src_width_px,src_height_px,points_norm:[[u,v],..]}`.
    fn vector_working_raster_transform_json(&self, overlay: &TypingOverlayRuntime) -> Option<Value> {
        let mesh = self.vector_transform_mesh.as_ref()?;
        let points_norm = self.vector_working_points_norm(overlay)?;
        let points: Vec<Value> = points_norm
            .into_iter()
            .map(|p| json!([p[0], p[1]]))
            .collect();
        Some(json!({
            "cols": mesh.cols,
            "rows": mesh.rows,
            "src_width_px": self.vector_transform_src_px[0],
            "src_height_px": self.vector_transform_src_px[1],
            "points_norm": points,
        }))
    }

    /// Set the overlay's `render_data_json` to `render_data`, parse render params, and dispatch the
    /// background edit-render (which bakes the warp into the PNG and swaps `source_rgba`/`size_px`/
    /// texture on completion), then request a placement save. Shared by settle and reset. Returns
    /// `false` (surfacing an error) when the params cannot be built or the staging dir is missing.
    ///
    /// `pub(super)` so sibling `tab` submodules (e.g. the Ctrl+wheel rotation in
    /// `selection_rasters.rs`) can reuse the convert-agnostic inject → dispatch tail.
    pub(super) fn dispatch_vector_rerender(
        &mut self,
        overlay_idx: usize,
        render_data: Value,
        ctx: &egui::Context,
    ) -> bool {
        let Some(text_images_dir) = self.text_images_save_dir.clone() else {
            self.set_create_error(
                ctx,
                t!("typing.vector_transform.text_images_dir_missing_error"),
            );
            return false;
        };
        let Some(mut render_params) = text_render_params_from_render_data(&render_data) else {
            self.set_create_error(
                ctx,
                t!("typing.vector_transform.build_params_error"),
            );
            return false;
        };
        // TEMPORARY debug-only: this re-render lands in the live overlay runtime (via
        // `apply_edit_overlay_render_result`), so request the renderer's mean/median centers while the
        // "Отладка центра" flag is on to keep the markers live through Ctrl+wheel rotation / width drag /
        // vector-transform settle. Remove with the center-debug feature.
        if self.debug_center_markers {
            render_params.extra_info = RenderExtraInfoRequest {
                mean_center: true,
                median_center: true,
            };
        }
        let (file_name, user_scale, rotation_deg) = {
            let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
                return false;
            };
            if overlay.kind != TypingOverlayKind::Text {
                return false;
            }
            // Inject the updated render_data immediately so the edit panel re-syncs its
            // `pending_raster_transform` (via `render_data_changed`) even before the re-render lands.
            overlay.render_data_json = Some(render_data.clone());
            (
                overlay.file_name.clone(),
                overlay.user_scale,
                overlay.angle_deg,
            )
        };
        let request = TypingEditOverlayRequest {
            token: 0,
            latest_token: Arc::clone(&self.edit_render_latest_token),
            overlay_idx,
            file_name,
            text_images_dir,
            user_scale,
            rotation_deg,
            render_params,
            render_data_json: render_data,
            font_provider: Arc::clone(&self.font_provider),
        };
        self.start_edit_overlay_render_job(request);
        // EDIT (vector mesh warp). DEFERRED, and this is the hottest of the converted sites: this fn is
        // reached per DRAG FRAME (via `dispatch_vector_rerender` ← `resize_selected_overlay_width` ←
        // `draw_page_overlays`), so an eager request here spawned a save worker on every frame of a
        // width or transform drag. Marking instead means the whole gesture writes once, on settle.
        self.mark_placement_save_dirty();
        true
    }

    /// Build the `raster_transform` for the current working mesh, inject it into `overlay_idx`'s
    /// `render_data`, and dispatch the background warped re-render (which bakes the warp into the PNG).
    /// Shared by `settle_vector_transform` and the LIVE per-frame drag re-render so the convert →
    /// inject → dispatch logic is written once. `report_errors` gates the user-facing toasts so the
    /// per-frame live path stays silent on the rare structural failures (which also block settle).
    /// Returns `false` when the overlay/render_data/working-mesh state is missing or malformed.
    fn inject_working_mesh_and_rerender(
        &mut self,
        overlay_idx: usize,
        ctx: &egui::Context,
        report_errors: bool,
    ) -> bool {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return false;
        };
        let Some(mut render_data) = overlay.render_data_json.clone() else {
            if report_errors {
                self.set_create_error(
                    ctx,
                    t!("typing.vector_transform.no_render_data_error"),
                );
            }
            return false;
        };
        let Some(raster_transform) = self.vector_working_raster_transform_json(overlay) else {
            return false;
        };
        if let Some(text_params) = render_data
            .get_mut("text_params")
            .and_then(Value::as_object_mut)
        {
            text_params.insert("raster_transform".to_string(), raster_transform);
        } else {
            if report_errors {
                self.set_create_error(
                    ctx,
                    t!("typing.vector_transform.no_text_params_error"),
                );
            }
            return false;
        }
        self.dispatch_vector_rerender(overlay_idx, render_data, ctx)
    }

    /// SETTLE a finished vector-transform drag for `overlay_idx`: convert the working mesh to a warp,
    /// inject it into the overlay's `render_data`, and re-render. No-op if state is missing.
    pub(super) fn settle_vector_transform(&mut self, overlay_idx: usize, ctx: &egui::Context) {
        crate::trace_log!(
            cat::TYPING,
            "vector_transform settle overlay_idx={} src=({:.1},{:.1})",
            overlay_idx,
            self.vector_transform_src_px[0],
            self.vector_transform_src_px[1]
        );
        self.inject_working_mesh_and_rerender(overlay_idx, ctx, true);
    }

    /// RESET the vector transform for `overlay_idx`: drop `raster_transform` from the overlay's
    /// `render_data`, reseed an identity working mesh, and re-render un-warped.
    pub(super) fn reset_vector_transform(
        &mut self,
        overlay_idx: usize,
        image_rect: Rect,
        zoom: f32,
        ctx: &egui::Context,
    ) {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return;
        };
        let Some(mut render_data) = overlay.render_data_json.clone() else {
            return;
        };
        if let Some(text_params) = render_data
            .get_mut("text_params")
            .and_then(Value::as_object_mut)
        {
            text_params.remove("raster_transform");
        }
        crate::trace_log!(cat::TYPING, "vector_transform reset overlay_idx={overlay_idx}");
        if self.dispatch_vector_rerender(overlay_idx, render_data, ctx) {
            // Reseed an identity working mesh so the handles snap back to the un-warped footprint.
            self.seed_vector_transform_mesh(overlay_idx, image_rect, zoom);
        }
    }

    /// Own the VECTOR transform-mode interaction + drawing for the overlay in
    /// `transform_mode_overlay_idx` when `transform_mode_kind == Vector`. Registers its own pointer
    /// widget over the working mesh, applies handle/brush/whole-mesh-move drags to the working mesh,
    /// settles on release, and draws the deform handles (idle) or the mesh WIREFRAME (during a drag).
    /// Called from `draw_page_overlays` after the baked-PNG fill pass so it draws on top.
    pub(super) fn draw_vector_transform_overlay(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
        painter: &egui::Painter,
    ) {
        let Some(overlay_idx) = self.transform_mode_overlay_idx else {
            return;
        };
        if self.transform_mode_kind != TypingTransformModeKind::Vector {
            return;
        }
        // Only drive the overlay that lives on the page being drawn.
        let on_this_page = self
            .overlays
            .get(overlay_idx)
            .is_some_and(|overlay| overlay.page_idx == page_idx && overlay.kind == TypingOverlayKind::Text);
        if !on_this_page {
            return;
        }
        // Defensive: seed if the working mesh was lost.
        if self.vector_transform_mesh.is_none() {
            self.seed_vector_transform_mesh(overlay_idx, image_rect, zoom);
        }
        // Ensure the un-warped base for the live preview (lazy: this also covers ENTER). Request it
        // when it is missing / belongs to another overlay and no render is already in flight; while a
        // render runs the drag falls back to the wireframe-only preview.
        let base_ready = self
            .vector_transform_base
            .as_ref()
            .is_some_and(|base| base.overlay_idx == overlay_idx);
        if !base_ready && self.vector_transform_base_rx.is_none() {
            self.request_vector_transform_base(overlay_idx);
        }
        let Some(mesh) = self.vector_transform_mesh.clone() else {
            return;
        };
        let mesh_scene = scene_mesh_points(&mesh, image_rect, zoom);
        let bounds = deform_mesh_bounds(&mesh_scene);
        if !bounds.is_positive() {
            return;
        }
        let interact_rect = bounds.expand(TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX * 2.0 + 2.0);
        let response = ui.interact(
            interact_rect,
            Id::new(("typing_vector_transform", overlay_idx)),
            Sense::click_and_drag(),
        );
        let pointer_pos = response.interact_pointer_pos();
        let pointer_inside_visual = pointer_pos
            .is_some_and(|pos| deform_mesh_contains_point(&mesh_scene, mesh.cols, mesh.rows, pos));
        let pointer_on_handle = pointer_pos.and_then(|pos| {
            if !self.deform_mode.is_handle_mode() {
                return None;
            }
            match self.deform_mode {
                TypingDeformMode::Perspective => {
                    // Perspective corners are the mesh grid corners; reuse the quad hit-test.
                    let quad = [
                        mesh_scene[0],
                        mesh_scene[mesh.cols - 1],
                        mesh_scene[mesh.cols * mesh.rows - 1],
                        mesh_scene[mesh.cols * (mesh.rows - 1)],
                    ];
                    hit_test_transform_handle(pos, &quad)
                }
                TypingDeformMode::Bend => {
                    hit_test_bend_handle(pos, &mesh_scene, mesh.cols, mesh.rows)
                }
                TypingDeformMode::Frame => hit_test_frame_handle(
                    pos,
                    &mesh_scene,
                    mesh.cols,
                    mesh.rows,
                    self.frame_handle_side_points,
                ),
                TypingDeformMode::Grid => hit_test_grid_handle(
                    pos,
                    &mesh_scene,
                    mesh.cols,
                    mesh.rows,
                    self.frame_handle_side_points,
                ),
                TypingDeformMode::Bulge
                | TypingDeformMode::Pinch
                | TypingDeformMode::Push
                | TypingDeformMode::Twirl
                | TypingDeformMode::Restore
                | TypingDeformMode::Smooth
                | TypingDeformMode::Stretch
                | TypingDeformMode::Fold => None,
            }
        });
        let pointer_targets_overlay = pointer_inside_visual || pointer_on_handle.is_some();
        if pointer_targets_overlay && (response.clicked() || response.dragged()) {
            self.primary_pointer_targets_overlay_this_frame = true;
        }

        // Context menu: exit / reset the VECTOR transform.
        let mut pending_exit = false;
        let mut pending_reset = false;
        response.context_menu(|menu_ui| {
            if menu_ui
                .button(t!("typing.context_menu.exit_transform_mode_vector"))
                .clicked()
            {
                pending_exit = true;
                menu_ui.close();
            }
            if menu_ui
                .button(t!("typing.context_menu.reset_transform_vector"))
                .clicked()
            {
                pending_reset = true;
                menu_ui.close();
            }
        });

        if response.drag_started()
            && pointer_targets_overlay
            && let Some(start_pointer) = pointer_pos
        {
            let mode = if let Some(handle_idx) = pointer_on_handle {
                match self.deform_mode {
                    TypingDeformMode::Perspective => {
                        TypingOverlayDragMode::PerspectiveHandle(handle_idx)
                    }
                    TypingDeformMode::Bend => TypingOverlayDragMode::BendHandle(handle_idx),
                    TypingDeformMode::Frame => TypingOverlayDragMode::FrameHandle(handle_idx),
                    TypingDeformMode::Grid => TypingOverlayDragMode::GridHandle(handle_idx),
                    TypingDeformMode::Bulge
                    | TypingDeformMode::Pinch
                    | TypingDeformMode::Push
                    | TypingDeformMode::Twirl
                    | TypingDeformMode::Restore
                    | TypingDeformMode::Smooth
                    | TypingDeformMode::Stretch
                    | TypingDeformMode::Fold => TypingOverlayDragMode::MoveMesh,
                }
            } else if self.deform_mode.is_brush_mode() && pointer_inside_visual {
                TypingOverlayDragMode::BrushStroke(self.deform_mode)
            } else {
                TypingOverlayDragMode::MoveMesh
            };
            self.primary_pointer_targets_overlay_this_frame = true;
            self.vector_transform_drag = Some(TypingVectorTransformDragState {
                overlay_idx,
                page_idx,
                pointer_start_scene: start_pointer,
                mode,
                start_mesh: mesh.clone(),
                has_changes: false,
            });
        }

        if response.dragged()
            && let Some(pointer) = pointer_pos
            // Peek before taking: only consume the drag once it is confirmed to belong to this
            // overlay/page, otherwise a mismatched guard would drop an active drag (a silent lost edit).
            && self
                .vector_transform_drag
                .as_ref()
                .is_some_and(|state| state.overlay_idx == overlay_idx && state.page_idx == page_idx)
            && let Some(mut state) = self.vector_transform_drag.take()
        {
            let page_size = page_size_from_image_rect(image_rect, zoom);
            let delta_page_px = [
                (pointer.x - state.pointer_start_scene.x) / zoom.max(f32::EPSILON),
                (pointer.y - state.pointer_start_scene.y) / zoom.max(f32::EPSILON),
            ];
            // Identity footprint mesh a brush stroke restores/smooths towards (only built when needed).
            let default_mesh = self.overlays.get(overlay_idx).map(|overlay| {
                Self::vector_identity_working_mesh(overlay, self.vector_transform_src_px, page_size)
            });
            let next_mesh = match state.mode {
                TypingOverlayDragMode::MoveMesh => {
                    let mut m = state.start_mesh.clone();
                    m.translate(delta_page_px[0], delta_page_px[1], page_size);
                    m
                }
                TypingOverlayDragMode::PerspectiveHandle(handle_idx) => {
                    apply_perspective_corner_drag(
                        &state.start_mesh,
                        handle_idx,
                        delta_page_px,
                        page_size,
                    )
                }
                TypingOverlayDragMode::BendHandle(handle_idx) => {
                    apply_bend_handle_drag(&state.start_mesh, handle_idx, delta_page_px, page_size)
                }
                TypingOverlayDragMode::FrameHandle(handle_idx) => apply_sampled_handle_drag(
                    &state.start_mesh,
                    SampledHandleMode::Frame,
                    self.frame_handle_side_points,
                    handle_idx,
                    self.pull_neighbor_handles,
                    delta_page_px,
                    page_size,
                ),
                TypingOverlayDragMode::GridHandle(handle_idx) => apply_sampled_handle_drag(
                    &state.start_mesh,
                    SampledHandleMode::Grid,
                    self.frame_handle_side_points,
                    handle_idx,
                    self.pull_neighbor_handles,
                    delta_page_px,
                    page_size,
                ),
                TypingOverlayDragMode::BrushStroke(brush_mode) => match default_mesh {
                    Some(default_mesh) => apply_brush_deform_drag(
                        brush_mode,
                        &state.start_mesh,
                        &default_mesh,
                        state.pointer_start_scene,
                        pointer,
                        image_rect,
                        zoom,
                        &self.deform_tool_settings,
                    ),
                    None => state.start_mesh.clone(),
                },
                // MoveCenter / Rotate are never produced for a vector edit; keep the mesh unchanged.
                TypingOverlayDragMode::MoveCenter | TypingOverlayDragMode::Rotate => {
                    state.start_mesh.clone()
                }
            };
            state.has_changes = true;
            // Brush strokes accumulate: re-anchor to the just-committed mesh + current pointer so the
            // next frame's displacement stacks (mirrors the raster brush path).
            if matches!(state.mode, TypingOverlayDragMode::BrushStroke(_)) {
                state.start_mesh = next_mesh.clone();
                state.pointer_start_scene = pointer;
            }
            // Did the mesh actually move this frame? `dragged()` can fire with a zero pointer delta,
            // so compare against the mesh being replaced to skip redundant live re-renders.
            let mesh_changed = self.vector_transform_mesh.as_ref() != Some(&next_mesh);
            self.vector_transform_mesh = Some(next_mesh);
            self.vector_transform_drag = Some(state);
            ctx.request_repaint();
            // LIVE sharp re-render: the warped render is fast, so dispatch it every frame the mesh
            // changes instead of only on release. The edit render is latest-wins
            // (`edit_render_latest_token`), so any superseded in-flight render is dropped, and the
            // placement save coalesces behind the in-flight render. The Phase-3b textured-mesh preview
            // still covers the sub-frame gap until the sharp PNG lands. Errors stay silent here (they
            // also block settle, which surfaces them on release).
            if mesh_changed {
                self.inject_working_mesh_and_rerender(overlay_idx, ctx, false);
            }
        }

        let mut drag_active = self.vector_transform_drag.is_some();
        if response.drag_stopped()
            && let Some(state) = self.vector_transform_drag.take()
        {
            drag_active = false;
            if state.has_changes {
                self.settle_vector_transform(overlay_idx, ctx);
            }
        }

        // Draw: while dragging, texture the UN-WARPED base onto the working mesh so the text bends
        // live (Phase 3b); the plain baked PNG for this overlay is hidden by `draw_page_overlays` while
        // this preview draws (see `vector_transform_preview_active`). The base MUST be un-warped so the
        // mesh warp is applied exactly once. Falls back to the wireframe-only preview until the base is
        // ready. The mesh wireframe + the deform handles for the active mode always draw on top so the
        // user can grab them.
        let mesh_scene = self
            .vector_transform_mesh
            .as_ref()
            .map(|m| scene_mesh_points(m, image_rect, zoom))
            .unwrap_or(mesh_scene);
        if drag_active {
            if let Some(base_tex_id) = self.ensure_vector_base_texture_uploaded(ctx, overlay_idx) {
                draw_textured_deform_mesh(
                    painter,
                    base_tex_id,
                    &mesh_scene,
                    mesh.cols,
                    mesh.rows,
                    Color32::WHITE,
                );
            }
            draw_textured_deform_mesh_wire(painter, &mesh_scene, mesh.cols, mesh.rows);
        }
        match self.deform_mode {
            TypingDeformMode::Perspective => {
                let quad = [
                    mesh_scene[0],
                    mesh_scene[mesh.cols - 1],
                    mesh_scene[mesh.cols * mesh.rows - 1],
                    mesh_scene[mesh.cols * (mesh.rows - 1)],
                ];
                draw_perspective_handles(painter, &quad);
            }
            TypingDeformMode::Bend => draw_bend_handles(painter, &mesh_scene, mesh.cols, mesh.rows),
            TypingDeformMode::Frame => draw_frame_handles(
                painter,
                &mesh_scene,
                mesh.cols,
                mesh.rows,
                self.frame_handle_side_points,
            ),
            TypingDeformMode::Grid => draw_grid_handles(
                painter,
                &mesh_scene,
                mesh.cols,
                mesh.rows,
                self.frame_handle_side_points,
            ),
            TypingDeformMode::Bulge
            | TypingDeformMode::Pinch
            | TypingDeformMode::Push
            | TypingDeformMode::Twirl
            | TypingDeformMode::Restore
            | TypingDeformMode::Smooth
            | TypingDeformMode::Stretch
            | TypingDeformMode::Fold => {
                // Brush modes: show the brush radius preview under the pointer.
                if let Some(pos) = ctx.input(|i| i.pointer.latest_pos())
                    && interact_rect.contains(pos)
                {
                    draw_brush_preview(painter, pos, self.deform_tool_settings.brush_radius_px);
                }
            }
        }

        if pending_exit {
            self.transform_mode_overlay_idx = None;
            self.exit_vector_transform_mode();
        } else if pending_reset {
            self.reset_vector_transform(overlay_idx, image_rect, zoom, ctx);
        }
    }
}
