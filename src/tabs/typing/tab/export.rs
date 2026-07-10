/*
File: tab/export.rs

Purpose:
Export pipeline for the typing tab: rasterizing typed overlays onto page images
and writing them to a folder (PNG), plus the async export job polling/request UI
methods on `TypingTextOverlayLayer`. Covers overlay compositing math local to
export (bilinear quad UV sampling, textured-triangle rasterization, source-over
blending, clean-overlay snapshot loading, mask sampling).

Notes:
Extracted verbatim from `tab.rs`. Free fns and methods are `pub(super)` so `tab.rs`
and sibling submodules of `tab` can use them. `use super::*;` pulls in the parent
module's types and imports. Some export helpers (compositing/deform variants) remain
in `tab.rs` and are reused from here as descendants of module `tab`.
*/

use super::*;
// `write_image` is a `PngEncoder` method from this trait; needed for the in-memory
// PNG encode that replaces `image::save_buffer`/`fs::write` on the storage seam.
use image::ImageEncoder;

pub(super) fn export_typing_pages_to_folder(
    mut jobs: Vec<TypingExportPageJob>,
    output_dir: PathBuf,
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    progress_tx: mpsc::Sender<TypingExportEvent>,
) -> Result<TypingExportResult, String> {
    // Route directory creation through the storage seam (web build writes to its virtual store).
    crate::storage::storage()
        .create_dir_all(output_dir.to_string_lossy().as_ref())
        .map_err(|err| tf!("typing.errors.create_output_dir_error", output_dir = output_dir.display(), err = err))?;
    let total = jobs.len();
    if jobs.is_empty() {
        return Ok(TypingExportResult {
            exported: 0,
            total,
            output_dir,
        });
    }
    prepare_export_clean_overlay_snapshots(&mut jobs, clean_overlays_model)?;

    let worker_count = thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .saturating_sub(1)
        .max(1)
        .min(jobs.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let mut worker_handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let tx = tx.clone();
        let queue = Arc::clone(&queue);
        worker_handles.push(thread::spawn(move || {
            loop {
                let job = {
                    let mut locked = queue.lock().unwrap_or_else(|p| p.into_inner());
                    locked.pop_front()
                };
                let Some(job) = job else {
                    break;
                };
                if tx.send(export_typing_single_page(job)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);

    let mut exported = 0usize;
    let mut processed = 0usize;
    let mut first_error: Option<String> = None;
    for result in rx {
        processed = processed.saturating_add(1);
        match result {
            Ok(()) => exported = exported.saturating_add(1),
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        let _ = progress_tx.send(TypingExportEvent::Progress {
            done: processed,
            total,
        });
    }
    for handle in worker_handles {
        let _ = handle.join();
    }
    if let Some(err) = first_error {
        return Err(err);
    }
    Ok(TypingExportResult {
        exported,
        total,
        output_dir,
    })
}

pub(super) fn export_typing_single_page(job: TypingExportPageJob) -> Result<(), String> {
    match job.export_format {
        TypingExportFormat::Png => {
            let (base_rgba, base_w, base_h) = flatten_typing_export_page_rgba(&job)?;
            // Encode RGBA8 to a PNG buffer in memory (default `PngEncoder` params, identical to
            // `save_buffer` for a `.png` path) then hand the bytes to the storage seam.
            let mut buf = Vec::new();
            image::codecs::png::PngEncoder::new(&mut buf)
                .write_image(
                    &base_rgba,
                    base_w as u32,
                    base_h as u32,
                    image::ColorType::Rgba8.into(),
                )
                .map_err(|err| {
                    tf!("typing.errors.save_page_error", job = job.output_path.display(), err = err)
                })?;
            crate::storage::storage()
                .write(job.output_path.to_string_lossy().as_ref(), &buf)
                .map_err(|err| {
                    tf!("typing.errors.save_page_error", job = job.output_path.display(), err = err)
                })
        }
        TypingExportFormat::Psd => {
            let bytes = super::super::psd_export::export_typing_single_page_psd(&job)?;
            crate::storage::storage()
                .write(job.output_path.to_string_lossy().as_ref(), &bytes)
                .map_err(|err| {
                    tf!("typing.errors.save_page_error", job = job.output_path.display(), err = err)
                })
        }
    }
}

pub(super) fn prepare_export_clean_overlay_snapshots(
    jobs: &mut [TypingExportPageJob],
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
) -> Result<(), String> {
    for job in jobs {
        job.clean_overlay_rgba = load_clean_overlay_snapshot_for_export(
            clean_overlays_model.as_ref(),
            job.page_idx,
            job.clean_overlay_path.as_deref(),
        )?;
    }
    Ok(())
}

pub(super) fn load_clean_overlay_snapshot_for_export(
    clean_overlays_model: Option<&Arc<Mutex<CleanOverlaysModel>>>,
    page_idx: usize,
    clean_overlay_path: Option<&Path>,
) -> Result<Option<Arc<image::RgbaImage>>, String> {
    let Some(model) = clean_overlays_model else {
        return load_clean_overlay_rgba_from_disk(clean_overlay_path)
            .map(|image| image.map(Arc::new));
    };
    if let Ok(locked) = model.lock()
        && let Some(image) = locked.overlay_rgba(page_idx)
    {
        return Ok(Some(image));
    }
    let Some(decoded) = load_clean_overlay_rgba_from_disk(clean_overlay_path)? else {
        return Ok(None);
    };
    if let Ok(mut locked) = model.lock() {
        if let Some(image) = locked.overlay_rgba(page_idx) {
            return Ok(Some(image));
        }
        locked.replace_from_rgba(page_idx, decoded.clone());
        if let Some(image) = locked.overlay_rgba(page_idx) {
            return Ok(Some(image));
        }
    }
    Ok(Some(Arc::new(decoded)))
}

pub(super) fn load_clean_overlay_rgba_from_disk(
    clean_overlay_path: Option<&Path>,
) -> Result<Option<image::RgbaImage>, String> {
    let Some(clean_overlay_path) = clean_overlay_path else {
        return Ok(None);
    };
    let path_str = clean_overlay_path.to_string_lossy();
    let bytes = crate::storage::storage()
        .read(path_str.as_ref())
        .map_err(|err| {
            tf!("typing.errors.open_clean_overlay_error", clean_overlay_path = clean_overlay_path.display(), err = err)
        })?;
    let clean = image::load_from_memory(&bytes)
        .map_err(|err| {
            tf!("typing.errors.open_clean_overlay_error", clean_overlay_path = clean_overlay_path.display(), err = err)
        })?
        .to_rgba8();
    Ok(Some(clean))
}

pub(super) fn rasterize_textured_triangle(
    base_rgba: &mut [u8],
    base_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_size: [usize; 2],
    v0: ([f32; 2], [f32; 2]),
    v1: ([f32; 2], [f32; 2]),
    v2: ([f32; 2], [f32; 2]),
) {
    fn edge(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
        (p[0] - a[0]) * (b[1] - a[1]) - (p[1] - a[1]) * (b[0] - a[0])
    }

    let area = edge(v0.0, v1.0, v2.0);
    if area.abs() <= f32::EPSILON {
        return;
    }
    let min_x = v0.0[0].min(v1.0[0]).min(v2.0[0]).floor().max(0.0) as i32;
    let max_x = v0.0[0]
        .max(v1.0[0])
        .max(v2.0[0])
        .ceil()
        .min(base_size[0].saturating_sub(1) as f32) as i32;
    let min_y = v0.0[1].min(v1.0[1]).min(v2.0[1]).floor().max(0.0) as i32;
    let max_y = v0.0[1]
        .max(v1.0[1])
        .max(v2.0[1])
        .ceil()
        .min(base_size[1].saturating_sub(1) as f32) as i32;
    if min_x > max_x || min_y > max_y {
        return;
    }

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let p = [x as f32 + 0.5, y as f32 + 0.5];
            let w0 = edge(v1.0, v2.0, p) / area;
            let w1 = edge(v2.0, v0.0, p) / area;
            let w2 = edge(v0.0, v1.0, p) / area;
            if w0 < -0.0001 || w1 < -0.0001 || w2 < -0.0001 {
                continue;
            }

            let s = (w0 * v0.1[0] + w1 * v1.1[0] + w2 * v2.1[0]).clamp(0.0, 1.0);
            let t = (w0 * v0.1[1] + w1 * v1.1[1] + w2 * v2.1[1]).clamp(0.0, 1.0);
            let src = sample_overlay_bilinear_rgba(overlay_rgba, overlay_size, s, t);
            if src[3] == 0 {
                continue;
            }

            let dst_idx = (y as usize * base_size[0] + x as usize) * 4;
            blend_source_over(&mut base_rgba[dst_idx..dst_idx + 4], &src);
        }
    }
}

pub(super) fn sample_overlay_bilinear_rgba(rgba: &[u8], size: [usize; 2], s: f32, t: f32) -> [u8; 4] {
    let w = size[0].max(1);
    let h = size[1].max(1);
    if rgba.len() != w * h * 4 {
        return [0, 0, 0, 0];
    }
    if w == 1 || h == 1 {
        let x = if w == 1 {
            0
        } else {
            (s.clamp(0.0, 1.0) * (w.saturating_sub(1)) as f32).round() as usize
        };
        let y = if h == 1 {
            0
        } else {
            (t.clamp(0.0, 1.0) * (h.saturating_sub(1)) as f32).round() as usize
        };
        let idx = (y * w + x) * 4;
        return [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]];
    }

    let fx = (s.clamp(0.0, 1.0) * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (t.clamp(0.0, 1.0) * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
    let x0 = fx.floor().clamp(0.0, (w - 1) as f32) as usize;
    let y0 = fy.floor().clamp(0.0, (h - 1) as f32) as usize;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;

    let i00 = (y0 * w + x0) * 4;
    let i10 = (y0 * w + x1) * 4;
    let i01 = (y1 * w + x0) * 4;
    let i11 = (y1 * w + x1) * 4;

    let bilerp = |v00: f32, v10: f32, v01: f32, v11: f32| {
        let top = v00 + (v10 - v00) * tx;
        let bot = v01 + (v11 - v01) * tx;
        top + (bot - top) * ty
    };

    // Interpolate in premultiplied alpha to avoid matte-color fringing
    // on semi-transparent glyph edges during export.
    let a00 = rgba[i00 + 3] as f32 / 255.0;
    let a10 = rgba[i10 + 3] as f32 / 255.0;
    let a01 = rgba[i01 + 3] as f32 / 255.0;
    let a11 = rgba[i11 + 3] as f32 / 255.0;
    let out_a = bilerp(a00, a10, a01, a11).clamp(0.0, 1.0);
    if out_a <= f32::EPSILON {
        return [0, 0, 0, 0];
    }

    let mut out = [0u8; 4];
    for c in 0..3 {
        let p00 = (rgba[i00 + c] as f32 / 255.0) * a00;
        let p10 = (rgba[i10 + c] as f32 / 255.0) * a10;
        let p01 = (rgba[i01 + c] as f32 / 255.0) * a01;
        let p11 = (rgba[i11 + c] as f32 / 255.0) * a11;
        let out_p = bilerp(p00, p10, p01, p11).clamp(0.0, 1.0);
        let out_c = (out_p / out_a).clamp(0.0, 1.0);
        out[c] = (out_c * 255.0).round() as u8;
    }
    out[3] = (out_a * 255.0).round() as u8;
    out
}

pub(super) fn blend_source_over(dst: &mut [u8], src: &[u8]) {
    if dst.len() < 4 || src.len() < 4 {
        return;
    }
    let sa = src[3] as f32 / 255.0;
    if sa <= 0.0 {
        return;
    }
    let da = dst[3] as f32 / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        dst[0] = 0;
        dst[1] = 0;
        dst[2] = 0;
        dst[3] = 0;
        return;
    }

    for c in 0..3 {
        let s = src[c] as f32 / 255.0;
        let d = dst[c] as f32 / 255.0;
        let out = (s * sa + d * da * (1.0 - sa)) / out_a;
        dst[c] = (out * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

pub(super) fn default_quad_uv_for_page(
    center_page_px: [f32; 2],
    overlay_size_px: [usize; 2],
    user_scale: f32,
    angle_deg: f32,
    page_size: [usize; 2],
) -> [[f32; 2]; 4] {
    let page_w = page_size[0].max(1) as f32;
    let page_h = page_size[1].max(1) as f32;
    let center_scene = clamp_page_point(center_page_px, page_size);
    let half_w = overlay_size_px[0] as f32 * user_scale.max(0.01) * 0.5;
    let half_h = overlay_size_px[1] as f32 * user_scale.max(0.01) * 0.5;
    let mut quad_scene = [
        [center_scene[0] - half_w, center_scene[1] - half_h],
        [center_scene[0] + half_w, center_scene[1] - half_h],
        [center_scene[0] + half_w, center_scene[1] + half_h],
        [center_scene[0] - half_w, center_scene[1] + half_h],
    ];
    if angle_deg.abs() > f32::EPSILON {
        let angle = angle_deg.to_radians();
        let (sin_a, cos_a) = angle.sin_cos();
        for point in &mut quad_scene {
            let dx = point[0] - center_scene[0];
            let dy = point[1] - center_scene[1];
            point[0] = center_scene[0] + dx * cos_a - dy * sin_a;
            point[1] = center_scene[1] + dx * sin_a + dy * cos_a;
        }
    }

    let quad_uv = quad_scene.map(|point| [point[0] / page_w, point[1] / page_h]);
    clamp_quad_uv(quad_uv)
}

pub(super) fn export_bilinear_quad_uv(quad_uv: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let t = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let top_u = quad_uv[0][0] + (quad_uv[1][0] - quad_uv[0][0]) * t;
    let top_v = quad_uv[0][1] + (quad_uv[1][1] - quad_uv[0][1]) * t;
    let bot_u = quad_uv[3][0] + (quad_uv[2][0] - quad_uv[3][0]) * t;
    let bot_v = quad_uv[3][1] + (quad_uv[2][1] - quad_uv[3][1]) * t;
    [top_u + (bot_u - top_u) * v, top_v + (bot_v - top_v) * v]
}

pub(super) fn bilinear_quad_page_px(quad_px: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let t = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let top_x = quad_px[0][0] + (quad_px[1][0] - quad_px[0][0]) * t;
    let top_y = quad_px[0][1] + (quad_px[1][1] - quad_px[0][1]) * t;
    let bot_x = quad_px[3][0] + (quad_px[2][0] - quad_px[3][0]) * t;
    let bot_y = quad_px[3][1] + (quad_px[2][1] - quad_px[3][1]) * t;
    [top_x + (bot_x - top_x) * v, top_y + (bot_y - top_y) * v]
}

pub(super) fn export_clip_overlay_rgba_if_needed(
    mask: &TypingMaskExportPage,
    overlay_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_deform_mesh: &TypingOverlayDeformMesh,
) -> Option<Vec<u8>> {
    if overlay_size[0] == 0 || overlay_size[1] == 0 {
        return None;
    }
    if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
        return None;
    }
    if mask.width == 0 || mask.height == 0 || mask.data.len() != mask.width * mask.height {
        return None;
    }

    let mut out = overlay_rgba.to_vec();
    let mut touched_active = false;
    for y in 0..overlay_size[1] {
        let tv = (y as f32 + 0.5) / overlay_size[1] as f32;
        for x in 0..overlay_size[0] {
            let tu = (x as f32 + 0.5) / overlay_size[0] as f32;
            let px_idx = (y * overlay_size[0] + x) * 4;
            if out[px_idx + 3] == 0 {
                continue;
            }
            let uv = sample_deform_mesh_uv(overlay_deform_mesh, tu, tv, [mask.width, mask.height]);
            let active = export_sample_mask_active(mask, uv[0], uv[1]);
            if active {
                touched_active = true;
            } else {
                out[px_idx + 3] = 0;
            }
        }
    }
    if touched_active { Some(out) } else { None }
}

pub(super) fn export_sample_mask_active(mask: &TypingMaskExportPage, u: f32, v: f32) -> bool {
    if mask.width == 0 || mask.height == 0 {
        return false;
    }
    let x = (u.clamp(0.0, 1.0) * (mask.width.saturating_sub(1)) as f32).round() as usize;
    let y = (v.clamp(0.0, 1.0) * (mask.height.saturating_sub(1)) as f32).round() as usize;
    mask.data
        .get(y.saturating_mul(mask.width).saturating_add(x))
        .is_some_and(|v| *v > 0)
}

impl TypingTextOverlayLayer {
    pub(super) fn poll_export_jobs(&mut self, ctx: &egui::Context) -> bool {
        let Some(state) = self.export_rx.as_ref() else {
            return false;
        };
        let mut changed = false;
        loop {
            match state.rx.try_recv() {
                Ok(TypingExportEvent::Progress { done, total }) => {
                    self.export_status = TypingExportUiStatus::Running { done, total };
                    changed = true;
                }
                Ok(TypingExportEvent::Finished(result)) => {
                    self.export_rx = None;
                    match result {
                        Ok(result) => {
                            crate::trace_log!(
                                cat::PERSIST,
                                "export result=ok exported={} total={}",
                                result.exported,
                                result.total
                            );
                            self.create_status_error = None;
                            self.export_status = TypingExportUiStatus::Success {
                                done: result.exported,
                                total: result.total,
                            };
                            let _ = result.output_dir;
                        }
                        Err(err) => {
                            crate::trace_log!(cat::PERSIST, "export result=err err={}", err);
                            self.export_status = TypingExportUiStatus::Error {
                                message: err.clone(),
                            };
                            self.set_create_error(ctx, err);
                        }
                    }
                    changed = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.export_rx = None;
                    let err = t!("typing.export.channel_error").to_string();
                    self.export_status = TypingExportUiStatus::Error {
                        message: err.clone(),
                    };
                    self.set_create_error(ctx, err);
                    changed = true;
                    break;
                }
            }
        }
        changed
    }

    pub(super) fn request_export_to_folder(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        masks_snapshot: HashMap<usize, TypingMaskExportPage>,
        output_dir: PathBuf,
        export_format: TypingExportFormat,
    ) {
        if self.export_rx.is_some() {
            self.set_create_error(ctx, t!("typing.export.already_running_error"));
            return;
        }
        if project.pages.is_empty() {
            self.set_create_error(ctx, t!("typing.export.no_pages_error"));
            return;
        }
        crate::trace_log!(
            cat::PERSIST,
            "export dispatch pages={} format={:?} output_dir={}",
            project.pages.len(),
            export_format,
            output_dir.display()
        );
        let clean_overlays_model = self.clean_overlays_model.clone();

        let mut overlays_by_page = HashMap::<usize, Vec<TypingExportOverlaySnapshot>>::new();
        for overlay in &self.overlays {
            if overlay.size_px[0] == 0 || overlay.size_px[1] == 0 {
                continue;
            }
            if overlay.source_rgba.len() != overlay.size_px[0] * overlay.size_px[1] * 4 {
                continue;
            }
            let band_z = self.overlay_band_z(overlay.page_idx, &overlay.uid, overlay.layer_idx);
            overlays_by_page.entry(overlay.page_idx).or_default().push(
                TypingExportOverlaySnapshot {
                    page_idx: overlay.page_idx,
                    center_page_px: overlay.center_page_px,
                    mask_clip_enabled: overlay.mask_clip_enabled,
                    layer_idx: overlay.layer_idx,
                    user_scale: overlay.user_scale,
                    angle_deg: overlay.angle_deg,
                    deform_mesh: overlay.deform_mesh.clone(),
                    size_px: overlay.size_px,
                    source_rgba: overlay.source_rgba.clone(),
                    render_data_json: overlay.render_data_json.clone(),
                    uid: overlay.uid.clone(),
                    band_z,
                },
            );
        }

        // Bottom-to-top by the UNIFIED manual band-Z (same as the on-screen draw order), so the export
        // stacks text exactly as shown. (Was the old layer_idx + page-Y auto-order.)
        for (page, overlays) in overlays_by_page.iter_mut() {
            overlays.sort_by_key(|o| self.overlay_band_z(*page, &o.uid, o.layer_idx));
        }

        // Snapshot the on-screen PS raster layers PER PAGE from the doc projection, so the export
        // composites EXACTLY what the canvas shows (post-effects display image, in-session transform /
        // deform, band-Z) rather than re-reading `layers.json` from disk — which silently dropped rasters
        // for the user (missing `_fx.png`, unflushed staging, etc.). `ensure_raster_layers_for_page` is
        // lazy (only visited pages are projected), so project every export page first.
        // Projecting every page (`ensure_raster_layers_for_page`) resolves `pending_select_raster_uid`
        // and would mutate the user's current selection. Triggering an export must NOT change selection,
        // so snapshot and restore it around the projection loop.
        let saved_selected_raster = self.selected_raster_idx;
        let saved_selected_raster_page = self.selected_raster_page;
        let saved_selected_overlay = self.selected_overlay_idx;
        let saved_pending_select = self.pending_select_raster_uid.clone();

        let mut rasters_by_page = HashMap::<usize, Vec<TypingExportRasterSnapshot>>::new();
        for page in &project.pages {
            self.ensure_raster_layers_for_page(page.idx);
            let Some(layers) = self.raster_layers_by_page.get(&page.idx) else {
                continue;
            };
            if layers.is_empty() {
                continue;
            }
            let snaps: Vec<TypingExportRasterSnapshot> = layers
                .iter()
                .map(|l| TypingExportRasterSnapshot {
                    visible: l.visible,
                    opacity: l.opacity,
                    transform: l.transform,
                    deform: l.deform.clone(),
                    rgba: color_image_to_rgba(&l.image),
                    size_px: l.image.size,
                    band_z: self.raster_band_z(page.idx, &l.uid),
                    mask_clip_enabled: l.mask_clip_enabled,
                })
                .collect();
            rasters_by_page.insert(page.idx, snaps);
        }

        // Restore the selection the projection loop may have changed (export is side-effect-free).
        self.selected_raster_idx = saved_selected_raster;
        self.selected_raster_page = saved_selected_raster_page;
        self.selected_overlay_idx = saved_selected_overlay;
        self.pending_select_raster_uid = saved_pending_select;

        let jobs = project
            .pages
            .iter()
            .map(|page| {
                let stem = page
                    .path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("page");
                let clean_overlay_path = project.paths.clean_layers_dir.join(format!("{stem}.png"));
                let out_ext = match export_format {
                    TypingExportFormat::Png => "png",
                    TypingExportFormat::Psd => "psd",
                };
                let out_name = format!("{stem}.{out_ext}");
                TypingExportPageJob {
                    page_idx: page.idx,
                    page_path: page.path.clone(),
                    output_path: output_dir.join(out_name),
                    clean_overlay_path: {
                        // `is_file()` via the storage seam: exists AND is not a directory.
                        let store = crate::storage::storage();
                        let is_file = {
                            let s = clean_overlay_path.to_string_lossy();
                            store.exists(s.as_ref()) && !store.is_dir(s.as_ref())
                        };
                        is_file.then_some(clean_overlay_path)
                    },
                    clean_overlay_rgba: None,
                    overlays: overlays_by_page.remove(&page.idx).unwrap_or_default(),
                    rasters: rasters_by_page.remove(&page.idx).unwrap_or_default(),
                    mask: masks_snapshot.get(&page.idx).cloned(),
                    export_format,
                    layers_primary_dir: self.layers_primary_dir.clone(),
                    layers_fallback_dir: self.layers_fallback_dir.clone(),
                }
            })
            .collect::<Vec<_>>();
        let total_pages = jobs.len();
        self.export_status = TypingExportUiStatus::Running {
            done: 0,
            total: total_pages,
        };
        let (tx, rx) = mpsc::channel::<TypingExportEvent>();
        thread::spawn(move || {
            let result =
                export_typing_pages_to_folder(jobs, output_dir, clean_overlays_model, tx.clone());
            let _ = tx.send(TypingExportEvent::Finished(result));
        });
        self.export_rx = Some(TypingExportRenderState { rx });
    }

    pub(super) fn export_status_for_ui(&self) -> TypingExportUiStatus {
        self.export_status.clone()
    }
}

/// Загружает страницу-источник, накладывает клин и все оверлеи (так же, как делает
/// PNG-экспорт) и возвращает финальный плоский RGBA8 буфер + размеры страницы.
/// Используется и PNG-веткой, и PSD-веткой (для composite image_data).
/// Сравнение оверлеев по порядку наложения (от низа стопки к верху).
/// Приоритет: меньший `layer_idx` ниже; внутри одного слоя — чем ниже на
/// картинке (больший `center_y`), тем выше в стопке. Используется и для отрисовки
/// в редакторе, и для композиции при экспорте, чтобы UI и PNG/PSD совпадали.
// `overlay_stack_cmp` (the old layer_idx + page-Y auto-order) was retired: text is now ordered by the
// unified manual band-Z everywhere (draw, interaction, export), like rasters.
pub(in crate::tabs::typing) fn flatten_typing_export_page_rgba(
    job: &TypingExportPageJob,
) -> Result<(Vec<u8>, usize, usize), String> {
    let page_path_str = job.page_path.to_string_lossy();
    let page_bytes = crate::storage::storage()
        .read(page_path_str.as_ref())
        .map_err(|err| {
            tf!("typing.errors.open_page_error", job = job.page_path.display(), err = err)
        })?;
    let mut base = image::load_from_memory(&page_bytes)
        .map_err(|err| {
            tf!("typing.errors.open_page_error", job = job.page_path.display(), err = err)
        })?
        .to_rgba8();
    let base_w = base.width() as usize;
    let base_h = base.height() as usize;
    let base_rgba = base.as_mut();

    if let Some(clean) = job.clean_overlay_rgba.as_ref() {
        composite_overlay_full_image_over(
            base_rgba,
            [base_w, base_h],
            clean.as_raw(),
            [clean.width() as usize, clean.height() as usize],
        );
    }

    // PS raster layers to composite, normalized to a common shape (straight RGBA + band-Z). PREFER the
    // on-screen snapshot taken from the doc projection (`job.rasters`, matching the canvas exactly);
    // FALL BACK to a disk read of `layers.json` only when no snapshot was provided (back-compat). Then
    // interleave rasters with text/image overlays in the SAME band-Z order the live canvas uses.
    use crate::models::layer_model::ordering::Band;
    use crate::models::layer_model::persist;
    struct RasterDraw {
        visible: bool,
        opacity: f32,
        transform: crate::models::layer_model::manifest::TransformRec,
        deform: Option<crate::models::layer_model::manifest::DeformRec>,
        rgba: Vec<u8>,
        size_px: [usize; 2],
        band_z: u32,
        mask_clip_enabled: bool,
    }

    // On-disk page bands: needed for OVERLAY (text) band-Z in both paths, and for raster band-Z in the
    // disk-fallback path (the snapshot carries raster band-Z directly).
    let disk_bands = match job.layers_primary_dir.as_deref() {
        Some(primary) => {
            persist::load_page_bands(primary, job.layers_fallback_dir.as_deref(), job.page_idx)
        }
        None => Vec::new(),
    };

    let raster_draws: Vec<RasterDraw> = if !job.rasters.is_empty() {
        job.rasters
            .iter()
            .map(|r| RasterDraw {
                visible: r.visible,
                opacity: r.opacity,
                transform: r.transform,
                deform: r.deform.clone(),
                rgba: r.rgba.clone(),
                size_px: r.size_px,
                band_z: r.band_z,
                mask_clip_enabled: r.mask_clip_enabled,
            })
            .collect()
    } else if let Some(primary) = job.layers_primary_dir.as_deref() {
        let fb = job.layers_fallback_dir.as_deref();
        let loaded = persist::load_page_rasters(primary, fb, job.page_idx)
            .unwrap_or_else(|err| {
                eprintln!(
                    "WARN typing::flatten_export_failed_to_load_rasters page={} err={err}",
                    job.page_idx
                );
                persist::PageRasters {
                    groups: Vec::new(),
                    layers: Vec::new(),
                }
            })
            .layers;
        let raster_band_z = |uid: &str| -> u32 {
            for band in &disk_bands {
                if let Band::Raster { uid: u, z } = band
                    && u == uid
                {
                    return *z;
                }
            }
            disk_bands.len() as u32
        };
        loaded
            .into_iter()
            .map(|l| {
                let rgba: Vec<u8> = l
                    .image
                    .pixels
                    .iter()
                    .flat_map(|p| p.to_srgba_unmultiplied())
                    .collect();
                let band_z = raster_band_z(&l.uid);
                RasterDraw {
                    visible: l.visible,
                    opacity: l.opacity,
                    transform: l.transform,
                    deform: l.deform,
                    size_px: l.image.size,
                    rgba,
                    band_z,
                    mask_clip_enabled: l.mask_clip.unwrap_or(false),
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let overlay_z = |uid: &str, layer_idx: usize| -> u32 {
        for band in &disk_bands {
            if let Band::PinnedText { uid: u, z } = band
                && u == uid
            {
                return *z;
            }
        }
        let layer_idx_u32 = u32::try_from(layer_idx).unwrap_or(u32::MAX);
        for band in &disk_bands {
            if let Band::TextGroup {
                layer_idx: li, z, ..
            } = band
                && *li == layer_idx_u32
            {
                return *z;
            }
        }
        disk_bands.len() as u32
    };

    enum Item {
        Raster(usize),
        Overlay(usize),
    }
    // Source BOTH raster and overlay band-Z from the SAME place to avoid divergence: when the in-memory
    // raster snapshot is present, the overlay snapshot's `band_z` (captured from the same `bands_by_page`)
    // is authoritative; otherwise fall back to the disk band lookup. Tie-break keeps raster=0 below
    // overlay=1 at the same Z (text on top of a same-Z raster).
    let use_snapshot_z = !job.rasters.is_empty();
    let mut items: Vec<(u32, u32, Item)> = Vec::new();
    for (i, r) in raster_draws.iter().enumerate() {
        items.push((r.band_z, 0, Item::Raster(i)));
    }
    for (i, ov) in job.overlays.iter().enumerate() {
        if ov.page_idx != job.page_idx {
            continue;
        }
        let z = if use_snapshot_z { ov.band_z } else { overlay_z(&ov.uid, ov.layer_idx) };
        items.push((z, 1, Item::Overlay(i)));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    for (_, _, item) in &items {
        match item {
            Item::Overlay(i) => {
                let overlay = &job.overlays[*i];
                let deform_mesh = export_overlay_deform_mesh_for_page(overlay, [base_w, base_h]);
                let clipped_rgba = export_overlay_clipped_rgba(job, overlay, &deform_mesh);
                if let Some(top_left_px) = direct_overlay_blit_top_left_px(overlay) {
                    composite_overlay_at_page_position_over(
                        base_rgba,
                        [base_w, base_h],
                        clipped_rgba.as_slice(),
                        overlay.size_px,
                        top_left_px,
                    );
                } else {
                    composite_overlay_mesh_over_page(
                        base_rgba,
                        [base_w, base_h],
                        clipped_rgba.as_slice(),
                        overlay.size_px,
                        &deform_mesh,
                    );
                }
            }
            Item::Raster(i) => {
                let r = &raster_draws[*i];
                if !r.visible {
                    continue;
                }
                let [w, h] = r.size_px;
                if w == 0 || h == 0 || r.rgba.len() != w * h * 4 {
                    continue;
                }
                // Honor the deform mesh when present (matching the canvas), else build the affine quad.
                let mesh = if let Some(d) = &r.deform {
                    TypingOverlayDeformMesh {
                        cols: d.cols,
                        rows: d.rows,
                        points_px: d.points_px.clone(),
                    }
                } else {
                    let (s, c) = r.transform.rotation.sin_cos();
                    let hw = w as f32 * 0.5 * r.transform.scale;
                    let hh = h as f32 * 0.5 * r.transform.scale;
                    let rot = |dx: f32, dy: f32| {
                        [
                            r.transform.cx + dx * c - dy * s,
                            r.transform.cy + dx * s + dy * c,
                        ]
                    };
                    // Row-major TL, TR, BL, BR. Construct the mesh directly (not via
                    // `TypingOverlayDeformMesh::new`) to skip its page clamping — a raster may extend
                    // off-page.
                    let points_px = vec![rot(-hw, -hh), rot(hw, -hh), rot(-hw, hh), rot(hw, hh)];
                    TypingOverlayDeformMesh {
                        cols: 2,
                        rows: 2,
                        points_px,
                    }
                };
                // Mask-clip ON → clip the raster to the page mask through its mesh (same as on-screen
                // `clipped_image` and the text-overlay export clip), so it exports WITHOUT pixels outside
                // the mask. Falls back to unclipped only if there is no mask snapshot.
                let mut rgba = if r.mask_clip_enabled {
                    job.mask
                        .as_ref()
                        .and_then(|mask| {
                            export_clip_overlay_rgba_if_needed(mask, [w, h], r.rgba.as_slice(), &mesh)
                        })
                        .unwrap_or_else(|| r.rgba.clone())
                } else {
                    r.rgba.clone()
                };
                if r.opacity < 1.0 {
                    for px in rgba.chunks_exact_mut(4) {
                        px[3] = (px[3] as f32 * r.opacity).round().clamp(0.0, 255.0) as u8;
                    }
                }
                composite_overlay_mesh_over_page(base_rgba, [base_w, base_h], &rgba, [w, h], &mesh);
            }
        }
    }

    Ok((base.into_raw(), base_w, base_h))
}

/// Применяет маску обрезки к оверлею, если она включена и доступна; иначе
/// возвращает исходный RGBA. Общая логика для PNG- и PSD-экспорта.
pub(in crate::tabs::typing) fn export_overlay_clipped_rgba(
    job: &TypingExportPageJob,
    overlay: &TypingExportOverlaySnapshot,
    deform_mesh: &TypingOverlayDeformMesh,
) -> Vec<u8> {
    if overlay.mask_clip_enabled {
        job.mask
            .as_ref()
            .and_then(|mask| {
                export_clip_overlay_rgba_if_needed(
                    mask,
                    overlay.size_px,
                    overlay.source_rgba.as_slice(),
                    deform_mesh,
                )
            })
            .unwrap_or_else(|| overlay.source_rgba.clone())
    } else {
        overlay.source_rgba.clone()
    }
}

pub(in crate::tabs::typing) fn composite_overlay_full_image_over(
    base_rgba: &mut [u8],
    base_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_size: [usize; 2],
) {
    if base_size[0] == 0 || base_size[1] == 0 || overlay_size[0] == 0 || overlay_size[1] == 0 {
        return;
    }
    if base_rgba.len() != base_size[0] * base_size[1] * 4 {
        return;
    }
    if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
        return;
    }
    let w = base_size[0].min(overlay_size[0]);
    let h = base_size[1].min(overlay_size[1]);
    for y in 0..h {
        for x in 0..w {
            let dst_idx = (y * base_size[0] + x) * 4;
            let src_idx = (y * overlay_size[0] + x) * 4;
            blend_source_over(
                &mut base_rgba[dst_idx..dst_idx + 4],
                &overlay_rgba[src_idx..src_idx + 4],
            );
        }
    }
}

pub(in crate::tabs::typing) fn composite_overlay_at_page_position_over(
    base_rgba: &mut [u8],
    base_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_size: [usize; 2],
    top_left_px: [i32; 2],
) {
    if base_size[0] == 0 || base_size[1] == 0 || overlay_size[0] == 0 || overlay_size[1] == 0 {
        return;
    }
    if base_rgba.len() != base_size[0] * base_size[1] * 4 {
        return;
    }
    if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
        return;
    }

    let base_w_i32 = i32::try_from(base_size[0]).unwrap_or(i32::MAX);
    let base_h_i32 = i32::try_from(base_size[1]).unwrap_or(i32::MAX);
    let overlay_w_i32 = i32::try_from(overlay_size[0]).unwrap_or(i32::MAX);
    let overlay_h_i32 = i32::try_from(overlay_size[1]).unwrap_or(i32::MAX);
    let start_x = top_left_px[0].max(0);
    let start_y = top_left_px[1].max(0);
    let end_x = top_left_px[0].saturating_add(overlay_w_i32).min(base_w_i32);
    let end_y = top_left_px[1].saturating_add(overlay_h_i32).min(base_h_i32);
    if start_x >= end_x || start_y >= end_y {
        return;
    }

    for dst_y in start_y..end_y {
        let src_y = dst_y - top_left_px[1];
        for dst_x in start_x..end_x {
            let src_x = dst_x - top_left_px[0];
            let dst_idx = (dst_y as usize * base_size[0] + dst_x as usize) * 4;
            let src_idx = (src_y as usize * overlay_size[0] + src_x as usize) * 4;
            blend_source_over(
                &mut base_rgba[dst_idx..dst_idx + 4],
                &overlay_rgba[src_idx..src_idx + 4],
            );
        }
    }
}

pub(in crate::tabs::typing) fn composite_overlay_mesh_over_page(
    base_rgba: &mut [u8],
    base_size: [usize; 2],
    overlay_rgba: &[u8],
    overlay_size: [usize; 2],
    deform_mesh: &TypingOverlayDeformMesh,
) {
    if base_size[0] == 0 || base_size[1] == 0 || overlay_size[0] == 0 || overlay_size[1] == 0 {
        return;
    }
    if base_rgba.len() != base_size[0] * base_size[1] * 4 {
        return;
    }
    if overlay_rgba.len() != overlay_size[0] * overlay_size[1] * 4 {
        return;
    }
    if deform_mesh.cols < 2 || deform_mesh.rows < 2 {
        return;
    }

    for row in 0..(deform_mesh.rows - 1) {
        let t0 = row as f32 / (deform_mesh.rows - 1) as f32;
        let t1 = (row + 1) as f32 / (deform_mesh.rows - 1) as f32;
        for col in 0..(deform_mesh.cols - 1) {
            let s0 = col as f32 / (deform_mesh.cols - 1) as f32;
            let s1 = (col + 1) as f32 / (deform_mesh.cols - 1) as f32;
            // Raw page-pixel corners (NO clamping to the page rect): the triangle rasterizer below
            // already clips pixel iteration to the page bounds, so clamping the vertices would only
            // distort geometry that extends off-page — e.g. a scaled-up raster, making its scale
            // appear ignored. Off-page parts are correctly clipped by the rasterizer's bbox.
            let p00 = deform_mesh.point(col, row);
            let p10 = deform_mesh.point(col + 1, row);
            let p01 = deform_mesh.point(col, row + 1);
            let p11 = deform_mesh.point(col + 1, row + 1);

            rasterize_textured_triangle(
                base_rgba,
                base_size,
                overlay_rgba,
                overlay_size,
                (p00, [s0, t0]),
                (p10, [s1, t0]),
                (p01, [s0, t1]),
            );
            rasterize_textured_triangle(
                base_rgba,
                base_size,
                overlay_rgba,
                overlay_size,
                (p01, [s0, t1]),
                (p10, [s1, t0]),
                (p11, [s1, t1]),
            );
        }
    }
}

pub(in crate::tabs::typing) fn direct_overlay_blit_top_left_px(overlay: &TypingExportOverlaySnapshot) -> Option<[i32; 2]> {
    if overlay.deform_mesh.is_some()
        || overlay.angle_deg.abs() > 1e-4
        || (overlay.user_scale - 1.0).abs() > 1e-4
    {
        return None;
    }
    Some([
        (overlay.center_page_px[0] - overlay.size_px[0] as f32 * 0.5).round() as i32,
        (overlay.center_page_px[1] - overlay.size_px[1] as f32 * 0.5).round() as i32,
    ])
}

pub(in crate::tabs::typing) fn export_overlay_deform_mesh_for_page(
    overlay: &TypingExportOverlaySnapshot,
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    overlay.deform_mesh.clone().unwrap_or_else(|| {
        default_deform_mesh_for_page(
            overlay.center_page_px,
            overlay.size_px,
            overlay.user_scale,
            overlay.angle_deg,
            page_size,
        )
    })
}
