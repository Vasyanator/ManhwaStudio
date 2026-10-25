/*
FILE HEADER (cleaning/tools/gradient.rs)
- Назначение: инструмент "Градиент" на базе `RegionMaskInpaintToolBase`.
- Ключевые сущности:
  - `GradientFillTool`: wiring инструмента (выделение региона, UI, routing ввода/курсора).
  - `run(...)`: основной pipeline масочного заполнения из Python `ui_new/tools/gradient_fill.py`.
  - `LabPlanes` / `LabBuffers` (borrowed / owned planar Lab of one grid), `ScanGeometry` (one
    scanline family), `ScreenedPoissonParams` (one channel consolidation): the parameter
    bundles of the private scanline and Poisson helpers.
- Алгоритм:
  - Перевод RGB -> Lab.
  - По маске выбирается ROI и оценивается лучший угол тонких scanline-линий.
  - Заполняется область под маской вдоль найденного угла с smoothstep-смешиванием.
  - Пустоты дозаполняются nearest-проходами, L-канал дополнительно согласуется screened-poisson.
  - Fallback: константная заливка медианой по кольцу вокруг маски.
- Важно:
  - Обработка запускается по кнопке "Обработать" в `RegionMaskInpaintToolBase`.
  - По `Применить` результат вставляется обратно в clean-overlay выбранного региона.
- Shared kernels (no second copy may live here):
  - The screened-Poisson L-channel consolidation calls the SHARED red-black SOR kernel
    `ms_tools::red_black_sor_sweeps`; this file only builds its `u0`/`lam`/`denom` buffers.
    A second SOR implementation anywhere in the project is a defect.
  - `dilate` is only a `bool` adapter over the one square dilation `ms_raster::dilate_square`
    (iterated 3×3 growth = one square window of radius `iters`); no morphology lives here.
  - sRGB <-> CIELAB goes through the project's one conversion `ms_region_postprocess::LabTables`
    (+ `encode_srgb`); only the dithered, half-up quantization of `lab_to_rgb` is this tool's.
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, RegionOverlayFrame, StrokePoint};
use std::sync::Arc;
use crate::input::CleaningLabels;
use ms_input::{ContextId, InputFrame};
use ms_canvas::CanvasView;
use ms_project::ProjectData;
use ms_tools::{red_black_sor_sweeps, SorProblem, SorSchedule};
use ms_region_postprocess::{LabTables, encode_srgb};
use eframe::egui;
use egui::Color32;

const ANGLE_STEP_DEG: usize = 3;
const DELTA_E_THRESHOLD: f32 = 2.5;

#[derive(Clone, Copy)]
struct AngleBoundaryPair {
    l_in: f32,
    a_in: f32,
    b_in: f32,
    l_out: f32,
    a_out: f32,
    b_out: f32,
}

/// Planar CIELAB of one row-major `width x height` grid: the three channels, each of length
/// `width * height`, borrowed together so a scanline helper cannot mix channels of two grids.
#[derive(Clone, Copy)]
struct LabPlanes<'a> {
    l: &'a [f32],
    a: &'a [f32],
    b: &'a [f32],
    width: usize,
    height: usize,
}

/// Owned planar CIELAB channels of one grid (the output of [`scan_fill_lines`]).
struct LabBuffers {
    l: Vec<f32>,
    a: Vec<f32>,
    b: Vec<f32>,
}

/// One family of parallel scanlines: lines at `theta_deg` degrees, `v_step` pixels apart
/// across the lines (a step below 1 counts as 1) and sampled every `t_step` pixels along them.
#[derive(Clone, Copy)]
struct ScanGeometry {
    theta_deg: f32,
    v_step: usize,
    t_step: f32,
}

/// Settings of one screened-Poisson channel consolidation ([`screened_poisson_refine`]): the
/// solve region is the mask's bounding box grown by `pad` pixels, `lam_in` / `lam_out` are the
/// data-fidelity weights inside / outside the mask, and `schedule` drives the shared SOR kernel.
#[derive(Clone, Copy)]
struct ScreenedPoissonParams {
    pad: usize,
    lam_in: f32,
    lam_out: f32,
    schedule: SorSchedule,
}

pub struct GradientFillTool {
    inpaint_base: RegionMaskInpaintToolBase,
}

impl Default for GradientFillTool {
    fn default() -> Self {
        Self {
            inpaint_base: RegionMaskInpaintToolBase::new("gradient_fill", Some(8)),
        }
    }
}

impl GradientFillTool {
    fn run(image: &egui::ColorImage, mask: &egui::ColorImage) -> Result<egui::ColorImage, String> {
        if image.size != mask.size {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        let w = image.size[0];
        let h = image.size[1];
        if w == 0 || h == 0 {
            return Ok(image.clone());
        }

        let mut base_rgb = Vec::with_capacity(w.saturating_mul(h));
        let mut mask_bits = vec![false; w.saturating_mul(h)];
        for (idx, (px, m)) in image.pixels.iter().zip(mask.pixels.iter()).enumerate() {
            let [r, g, b, _] = px.to_srgba_unmultiplied();
            base_rgb.push([r, g, b]);
            mask_bits[idx] = m.a() > 0;
        }
        if !mask_bits.iter().any(|v| *v) {
            return Ok(image.clone());
        }

        let filled_rgb = scanlines_parallel_lab(&base_rgb, &mask_bits, w, h);
        let mut out = image.clone();
        for idx in 0..mask_bits.len() {
            if !mask_bits[idx] {
                continue;
            }
            let [r, g, b] = filled_rgb[idx];
            let a = out.pixels[idx].a();
            out.pixels[idx] = Color32::from_rgba_unmultiplied(r, g, b, a);
        }
        Ok(out)
    }
}

impl CleaningTool for GradientFillTool {
    fn title(&self) -> &'static str {
        t!("cleaning.tools.gradient.title")
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small(t!("cleaning.tools.gradient.description_hint"));
        ui.small(tf!("cleaning.tools.gradient.select_region_gesture_hint", gesture = self.inpaint_base.labels().select_rect));
    }


    fn input_contexts(&self) -> Vec<ContextId> {
        self.inpaint_base.input_contexts()
    }

    fn apply_input(&mut self, input: &InputFrame) -> bool {
        self.inpaint_base.apply_input(input)
    }

    fn set_input_labels(&mut self, labels: &Arc<CleaningLabels>) {
        self.inpaint_base.set_input_labels(labels);
    }

    fn set_ai_backend_available(&mut self, available: bool) {
        self.inpaint_base.set_ai_backend_available(available);
    }

    fn set_ai_backend_torch_available(&mut self, available: bool) {
        self.inpaint_base.set_ai_backend_torch_available(available);
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        self.inpaint_base.begin_selection(canvas, point);
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, _from: StrokePoint, to: StrokePoint) {
        self.inpaint_base.update_selection(canvas, to);
    }

    fn stroke_end(&mut self, canvas: &mut CanvasView) {
        self.inpaint_base.end_selection(canvas);
    }

    /// A cancelled selection claim (a second finger, focus loss) drops only the drag in flight.
    fn stroke_cancel(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.abort_selection_drag();
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        input: &InputFrame,
    ) {
        let window_title = t!("cleaning.tools.gradient.overlay_title");
        self.inpaint_base.draw_overlay_ui(RegionOverlayFrame { ctx, canvas, project, input, window_title }, Self::run);
    }

    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        self.inpaint_base.draw_cursor(ui, canvas, pointer_scene_pos);
    }
}

fn scanlines_parallel_lab(base_rgb: &[[u8; 3]], mask: &[bool], w: usize, h: usize) -> Vec<[u8; 3]> {
    if !mask.iter().any(|v| *v) {
        return base_rgb.to_vec();
    }

    let (l, a, b) = rgb_to_lab(base_rgb);
    let Some((x0, x1, y0, y1)) = mask_bbox(mask, w, h) else {
        return base_rgb.to_vec();
    };

    let pad = 16usize;
    let rx0 = x0.saturating_sub(pad);
    let ry0 = y0.saturating_sub(pad);
    let rx1 = x1.saturating_add(pad + 1).min(w);
    let ry1 = y1.saturating_add(pad + 1).min(h);
    if rx1.saturating_sub(rx0) < 2 || ry1.saturating_sub(ry0) < 2 {
        return base_rgb.to_vec();
    }

    let rw = rx1 - rx0;
    let rh = ry1 - ry0;

    let mut roi_mask = vec![false; rw.saturating_mul(rh)];
    let mut l_roi = vec![0.0f32; rw.saturating_mul(rh)];
    let mut a_roi = vec![0.0f32; rw.saturating_mul(rh)];
    let mut b_roi = vec![0.0f32; rw.saturating_mul(rh)];
    for y in 0..rh {
        for x in 0..rw {
            let gi = idx2d(rx0 + x, ry0 + y, w);
            let ri = idx2d(x, y, rw);
            roi_mask[ri] = mask[gi];
            l_roi[ri] = l[gi];
            a_roi[ri] = a[gi];
            b_roi[ri] = b[gi];
        }
    }

    let mut best_score = f32::NEG_INFINITY;
    let mut best_theta = 0.0f32;

    let (g_ly, g_lx) = gradient_2d(&l, w, h);
    let ring = ring_mask(mask, w, h, 1, 3);
    let mut tested = [false; 180];
    let primary_angles = primary_angle_candidates(&g_lx, &g_ly, &ring);

    let mut try_angle = |ang: usize| {
        let a_deg = ang % 180;
        if tested[a_deg] {
            return;
        }
        tested[a_deg] = true;
        let roi_lab = LabPlanes { l: &l_roi, a: &a_roi, b: &b_roi, width: rw, height: rh };
        let scan = ScanGeometry { theta_deg: a_deg as f32, v_step: 2, t_step: 1.0 };
        let score = angle_consistency_score(roi_lab, &roi_mask, scan, 10.0, 300);
        if score > best_score {
            best_score = score;
            best_theta = a_deg as f32;
        }
    };

    for ang in primary_angles {
        try_angle(ang);
    }
    for ang in (0..180).step_by(ANGLE_STEP_DEG) {
        try_angle(ang);
    }

    let LabBuffers { l: mut fill_l, a: mut fill_a, b: mut fill_b } = scan_fill_lines(
        LabPlanes { l: &l_roi, a: &a_roi, b: &b_roi, width: rw, height: rh },
        &roi_mask,
        ScanGeometry { theta_deg: best_theta, v_step: 1, t_step: 1.0 },
        DELTA_E_THRESHOLD,
    );

    nearest_fill_in_mask(&mut fill_l, &roi_mask, rw, rh, 2);
    nearest_fill_in_mask(&mut fill_a, &roi_mask, rw, rh, 2);
    nearest_fill_in_mask(&mut fill_b, &roi_mask, rw, rh, 2);

    let has_fill = roi_mask
        .iter()
        .zip(fill_l.iter())
        .any(|(m, v)| *m && !v.is_nan());
    if !has_fill {
        let fallback_ring = if ring.iter().any(|v| *v) {
            ring
        } else {
            ring_mask(mask, w, h, 1, 4)
        };
        let lab = LabPlanes { l: &l, a: &a, b: &b, width: w, height: h };
        return fill_constant_from_ring(base_rgb, mask, lab, &fallback_ring);
    }

    let mut l_ref = l.clone();
    let mut a_ref = a.clone();
    let mut b_ref = b.clone();
    for y in 0..rh {
        for x in 0..rw {
            let ri = idx2d(x, y, rw);
            if !roi_mask[ri] {
                continue;
            }
            let gi = idx2d(rx0 + x, ry0 + y, w);
            l_ref[gi] = fill_l[ri];
            a_ref[gi] = fill_a[ri];
            b_ref[gi] = fill_b[ri];
        }
    }

    let poisson = ScreenedPoissonParams { pad: pad + 4, lam_in: 1.0, lam_out: 120.0, schedule: SorSchedule { iters: 220, omega: 1.95 } };
    let l_refined = screened_poisson_refine(&l_ref, &l, mask, [w, h], poisson);
    let rgb_hat = lab_to_rgb(&l_refined, &a_ref, &b_ref, w, h, 0.6, Some(mask));

    let mut out = base_rgb.to_vec();
    for idx in 0..mask.len() {
        if mask[idx] {
            out[idx] = rgb_hat[idx];
        }
    }
    out
}

fn primary_angle_candidates(gx: &[f32], gy: &[f32], ring: &[bool]) -> Vec<usize> {
    let mut sum_x = 0.0f32;
    let mut sum_y = 0.0f32;
    let mut count = 0usize;
    for idx in 0..ring.len() {
        if !ring[idx] {
            continue;
        }
        sum_x += gx[idx];
        sum_y += gy[idx];
        count = count.saturating_add(1);
    }
    if count == 0 {
        return Vec::new();
    }
    let vx = sum_x / count as f32;
    let vy = sum_y / count as f32;
    let norm = (vx * vx + vy * vy).sqrt();
    if norm <= 1e-6 {
        return Vec::new();
    }

    let tx = -vy;
    let ty = vx;
    let mut ang = ty.atan2(tx).to_degrees();
    while ang < 0.0 {
        ang += 180.0;
    }
    while ang >= 180.0 {
        ang -= 180.0;
    }

    let mut out = Vec::with_capacity(7);
    for delta in [-9.0f32, -6.0, -3.0, 0.0, 3.0, 6.0, 9.0] {
        let mut a = ang + delta;
        while a < 0.0 {
            a += 180.0;
        }
        while a >= 180.0 {
            a -= 180.0;
        }
        out.push(a.round() as usize % 180);
    }
    out
}

/// Fallback fill: every masked pixel of `base_rgb` gets the per-channel median Lab colour of the
/// `ring` pixels (of all unmasked pixels when the ring is empty), the L channel is then
/// consolidated by screened Poisson. `lab` is `base_rgb` in Lab; returns `base_rgb` unchanged
/// when there is nothing to sample.
fn fill_constant_from_ring(base_rgb: &[[u8; 3]], mask: &[bool], lab: LabPlanes<'_>, ring: &[bool]) -> Vec<[u8; 3]> {
    let LabPlanes { l, a, b, width: w, height: h } = lab;
    let mut l_samples = Vec::new();
    let mut a_samples = Vec::new();
    let mut b_samples = Vec::new();
    for idx in 0..ring.len() {
        if ring[idx] {
            l_samples.push(l[idx]);
            a_samples.push(a[idx]);
            b_samples.push(b[idx]);
        }
    }
    if l_samples.is_empty() {
        for idx in 0..mask.len() {
            if !mask[idx] {
                l_samples.push(l[idx]);
                a_samples.push(a[idx]);
                b_samples.push(b[idx]);
            }
        }
    }
    if l_samples.is_empty() {
        return base_rgb.to_vec();
    }

    let l_med = median_f32(&mut l_samples);
    let a_med = median_f32(&mut a_samples);
    let b_med = median_f32(&mut b_samples);

    let mut l_hat = l.to_vec();
    let mut a_hat = a.to_vec();
    let mut b_hat = b.to_vec();
    for idx in 0..mask.len() {
        if mask[idx] {
            l_hat[idx] = l_med;
            a_hat[idx] = a_med;
            b_hat[idx] = b_med;
        }
    }

    let poisson = ScreenedPoissonParams { pad: 12, lam_in: 1.0, lam_out: 120.0, schedule: SorSchedule { iters: 200, omega: 1.95 } };
    let l_refined = screened_poisson_refine(&l_hat, l, mask, [w, h], poisson);
    let rgb_hat = lab_to_rgb(&l_refined, &a_hat, &b_hat, w, h, 0.6, Some(mask));
    let mut out = base_rgb.to_vec();
    for idx in 0..mask.len() {
        if mask[idx] {
            out[idx] = rgb_hat[idx];
        }
    }
    out
}

/// Scores how well the scanlines of `scan` bridge the mask: minus the sum of the Lab colour
/// distances (each capped at `delta_e_cap`) across at most `max_pairs` boundary pairs, plus a
/// small bonus per pair. `-1e9` when no line crosses the mask cleanly.
fn angle_consistency_score(lab: LabPlanes<'_>, mask: &[bool], scan: ScanGeometry, delta_e_cap: f32, max_pairs: usize) -> f32 {
    let pairs = collect_boundary_pairs(lab, mask, scan, max_pairs);
    if pairs.is_empty() {
        return -1e9;
    }
    let mut score = 0.0f32;
    for pair in pairs.iter() {
        let dl = pair.l_in - pair.l_out;
        let da = pair.a_in - pair.a_out;
        let db = pair.b_in - pair.b_out;
        let de = (dl * dl + da * da + db * db).sqrt();
        score -= de.min(delta_e_cap);
    }
    score + 0.25 * pairs.len() as f32
}

/// Walks the scanlines of `scan` over the grid and, for each line that enters and leaves the
/// mask, records the Lab colours one pixel before the entry and one pixel after the exit (both
/// unmasked). Stops after `max_pairs` pairs.
fn collect_boundary_pairs(lab: LabPlanes<'_>, mask: &[bool], scan: ScanGeometry, max_pairs: usize) -> Vec<AngleBoundaryPair> {
    let LabPlanes { l, a, b, width: w, height: h } = lab;
    let ScanGeometry { theta_deg, v_step, t_step } = scan;
    let theta = theta_deg.to_radians();
    let c = theta.cos();
    let s = theta.sin();

    let corners = [
        (0.0f32, 0.0f32),
        (0.0f32, (h.saturating_sub(1)) as f32),
        ((w.saturating_sub(1)) as f32, 0.0f32),
        ((w.saturating_sub(1)) as f32, (h.saturating_sub(1)) as f32),
    ];
    let mut umin = f32::INFINITY;
    let mut umax = f32::NEG_INFINITY;
    let mut vmin = f32::INFINITY;
    let mut vmax = f32::NEG_INFINITY;
    for (x, y) in corners {
        let u = x * c + y * s;
        let v = -x * s + y * c;
        umin = umin.min(u);
        umax = umax.max(u);
        vmin = vmin.min(v);
        vmax = vmax.max(v);
    }

    let mut out = Vec::new();
    let mut v = vmin;
    while v <= vmax && out.len() < max_pairs {
        let mut transitions = Vec::<(f32, bool)>::new();
        let mut t = umin;
        let mut last_state: Option<bool> = None;
        while t <= umax {
            let x = c * t - s * v;
            let y = s * t + c * v;
            let xi = x.round() as i32;
            let yi = y.round() as i32;
            if xi >= 0 && xi < w as i32 && yi >= 0 && yi < h as i32 {
                let state = mask[idx2d(xi as usize, yi as usize, w)];
                match last_state {
                    None => last_state = Some(state),
                    Some(prev) if prev != state => {
                        transitions.push((t, state));
                        last_state = Some(state);
                    }
                    _ => {}
                }
            }
            t += t_step;
        }

        if transitions.len() >= 2 {
            let xi0 = (c * umin - s * v).round() as i32;
            let yi0 = (s * umin + c * v).round() as i32;
            let xw = xi0.rem_euclid(w as i32) as usize;
            let yw = yi0.rem_euclid(h as i32) as usize;
            let mut cur_state = mask[idx2d(xw, yw, w)];

            let mut t_in: Option<f32> = None;
            let mut t_out: Option<f32> = None;
            for (tk, st) in transitions {
                if !cur_state && st {
                    t_in = Some(tk);
                }
                if cur_state && !st {
                    t_out = Some(tk);
                }
                cur_state = st;
            }

            if let (Some(t_in), Some(t_out)) = (t_in, t_out)
                && (t_out - t_in) >= (2.0 * t_step)
            {
                let tin = t_in - 1.0;
                let tout = t_out + 1.0;

                let x_in = (c * tin - s * v).round() as i32;
                let y_in = (s * tin + c * v).round() as i32;
                let x_out = (c * tout - s * v).round() as i32;
                let y_out = (s * tout + c * v).round() as i32;

                if x_in >= 0
                    && x_in < w as i32
                    && y_in >= 0
                    && y_in < h as i32
                    && x_out >= 0
                    && x_out < w as i32
                    && y_out >= 0
                    && y_out < h as i32
                {
                    let i_in = idx2d(x_in as usize, y_in as usize, w);
                    let i_out = idx2d(x_out as usize, y_out as usize, w);
                    if !mask[i_in] && !mask[i_out] {
                        out.push(AngleBoundaryPair {
                            l_in: l[i_in],
                            a_in: a[i_in],
                            b_in: b[i_in],
                            l_out: l[i_out],
                            a_out: a[i_out],
                            b_out: b[i_out],
                        });
                    }
                }
            }
        }

        v += v_step.max(1) as f32;
    }

    out
}

/// Fills the masked span of every scanline of `scan` from its two unmasked ends: the ends'
/// mean when their Lab distance is at most `delta_e_thr`, else a smoothstep blend between them.
/// Returns the filled channels; a pixel no line filled is NaN.
fn scan_fill_lines(lab: LabPlanes<'_>, mask: &[bool], scan: ScanGeometry, delta_e_thr: f32) -> LabBuffers {
    let LabPlanes { l, a, b, width: w, height: h } = lab;
    let ScanGeometry { theta_deg, v_step, t_step } = scan;
    let mut out_l = vec![f32::NAN; w.saturating_mul(h)];
    let mut out_a = vec![f32::NAN; w.saturating_mul(h)];
    let mut out_b = vec![f32::NAN; w.saturating_mul(h)];
    let theta = theta_deg.to_radians();
    let c = theta.cos();
    let s = theta.sin();

    let corners = [
        (0.0f32, 0.0f32),
        (0.0f32, (h.saturating_sub(1)) as f32),
        ((w.saturating_sub(1)) as f32, 0.0f32),
        ((w.saturating_sub(1)) as f32, (h.saturating_sub(1)) as f32),
    ];
    let mut umin = f32::INFINITY;
    let mut umax = f32::NEG_INFINITY;
    let mut vmin = f32::INFINITY;
    let mut vmax = f32::NEG_INFINITY;
    for (x, y) in corners {
        let u = x * c + y * s;
        let v = -x * s + y * c;
        umin = umin.min(u);
        umax = umax.max(u);
        vmin = vmin.min(v);
        vmax = vmax.max(v);
    }

    let mut v = vmin;
    while v <= vmax {
        let mut line_x = Vec::<usize>::new();
        let mut line_y = Vec::<usize>::new();

        let mut t = umin;
        while t <= umax + 0.5 * t_step {
            let x = (c * t - s * v).round() as i32;
            let y = (s * t + c * v).round() as i32;
            if x >= 0 && x < w as i32 && y >= 0 && y < h as i32 {
                line_x.push(x as usize);
                line_y.push(y as usize);
            }
            t += t_step;
        }

        if line_x.is_empty() {
            v += v_step.max(1) as f32;
            continue;
        }

        let mut inside_idx = Vec::<usize>::new();
        for i in 0..line_x.len() {
            if mask[idx2d(line_x[i], line_y[i], w)] {
                inside_idx.push(i);
            }
        }
        if inside_idx.is_empty() {
            v += v_step.max(1) as f32;
            continue;
        }

        let t0 = *inside_idx.first().unwrap_or(&0);
        let t1 = *inside_idx.last().unwrap_or(&0);

        let pre_idx = t0.saturating_sub(1);
        let post_idx = (t1 + 1).min(line_x.len().saturating_sub(1));
        let pre_i = idx2d(line_x[pre_idx], line_y[pre_idx], w);
        let post_i = idx2d(line_x[post_idx], line_y[post_idx], w);
        if mask[pre_i] || mask[post_i] {
            v += v_step.max(1) as f32;
            continue;
        }

        let lin = l[pre_i];
        let ain = a[pre_i];
        let bin = b[pre_i];
        let lout = l[post_i];
        let aout = a[post_i];
        let bout = b[post_i];

        let dl = lin - lout;
        let da = ain - aout;
        let db = bin - bout;
        let delta_e = (dl * dl + da * da + db * db).sqrt();

        let seg_len = (t1 as i32 - t0 as i32).max(1) as usize;
        for k in 0..=(t1 - t0) {
            let gx = line_x[t0 + k];
            let gy = line_y[t0 + k];
            let go = idx2d(gx, gy, w);

            if delta_e <= delta_e_thr {
                out_l[go] = 0.5 * (lin + lout);
                out_a[go] = 0.5 * (ain + aout);
                out_b[go] = 0.5 * (bin + bout);
            } else {
                let alpha = k as f32 / seg_len as f32;
                let t = smoothstep(alpha);
                out_l[go] = (1.0 - t) * lin + t * lout;
                out_a[go] = (1.0 - t) * ain + t * aout;
                out_b[go] = (1.0 - t) * bin + t * bout;
            }
        }

        v += v_step.max(1) as f32;
    }
    LabBuffers { l: out_l, a: out_a, b: out_b }
}

fn nearest_fill_in_mask(arr: &mut [f32], mask: &[bool], w: usize, h: usize, passes: usize) {
    for _ in 0..passes {
        let mut has_nan = false;
        for i in 0..arr.len() {
            if mask[i] && arr[i].is_nan() {
                has_nan = true;
                break;
            }
        }
        if !has_nan {
            return;
        }

        let mut v = vec![0.0f32; arr.len()];
        let mut ws = vec![0.0f32; arr.len()];
        for i in 0..arr.len() {
            if !arr[i].is_nan() {
                v[i] = arr[i];
                ws[i] = 1.0;
            }
        }

        let mut upd = vec![f32::NAN; arr.len()];
        for y in 0..h {
            let y_up = (y + 1).min(h.saturating_sub(1));
            let y_down = y.saturating_sub(1);
            for x in 0..w {
                let x_right = (x + 1).min(w.saturating_sub(1));
                let x_left = x.saturating_sub(1);

                let i_up = idx2d(x, y_up, w);
                let i_down = idx2d(x, y_down, w);
                let i_right = idx2d(x_right, y, w);
                let i_left = idx2d(x_left, y, w);

                let vsum = v[i_up] + v[i_down] + v[i_right] + v[i_left];
                let wsum = ws[i_up] + ws[i_down] + ws[i_right] + ws[i_left];

                let i = idx2d(x, y, w);
                if wsum > 0.0 {
                    upd[i] = vsum / wsum;
                }
            }
        }

        for i in 0..arr.len() {
            if mask[i] && arr[i].is_nan() {
                arr[i] = upd[i];
            }
        }
    }
}

/// Screened-Poisson consolidation of one channel of a row-major `[width, height]` grid:
/// solves for a smooth field pinned to `channel_pred` inside the mask and to `channel_orig`
/// outside it, within the region `params` describes, through the shared SOR kernel. Returns
/// `channel_pred` with the solved region written back (unchanged when the mask is empty).
fn screened_poisson_refine(
    channel_pred: &[f32],
    channel_orig: &[f32],
    mask: &[bool],
    [w, h]: [usize; 2],
    params: ScreenedPoissonParams,
) -> Vec<f32> {
    let ScreenedPoissonParams { pad, lam_in, lam_out, schedule } = params;
    let Some((x0, x1, y0, y1)) = mask_bbox(mask, w, h) else {
        return channel_pred.to_vec();
    };

    let rx0 = x0.saturating_sub(pad);
    let ry0 = y0.saturating_sub(pad);
    let rx1 = x1.saturating_add(pad + 1).min(w);
    let ry1 = y1.saturating_add(pad + 1).min(h);

    let rw = rx1.saturating_sub(rx0);
    let rh = ry1.saturating_sub(ry0);
    if rw == 0 || rh == 0 {
        return channel_pred.to_vec();
    }

    let mut roi_mask = vec![false; rw.saturating_mul(rh)];
    let mut u0 = vec![0.0f32; rw.saturating_mul(rh)];
    let mut lam = vec![0.0f32; rw.saturating_mul(rh)];
    for y in 0..rh {
        for x in 0..rw {
            let gi = idx2d(rx0 + x, ry0 + y, w);
            let ri = idx2d(x, y, rw);
            let m = mask[gi];
            roi_mask[ri] = m;
            u0[ri] = if m {
                channel_pred[gi]
            } else {
                channel_orig[gi]
            };
            lam[ri] = if m { lam_in } else { lam_out };
        }
    }

    let mut u = u0.clone();
    let mut denom = vec![0.0f32; lam.len()];
    for i in 0..lam.len() {
        denom[i] = 4.0 + lam[i];
    }

    if rw >= 3 && rh >= 3 {
        let problem = SorProblem { u0: &u0, lam: &lam, denom: &denom, width: rw, height: rh };
        red_black_sor_sweeps(&mut u, &problem, schedule);
    }

    let mut out = channel_pred.to_vec();
    for y in 0..rh {
        for x in 0..rw {
            let gi = idx2d(rx0 + x, ry0 + y, w);
            let ri = idx2d(x, y, rw);
            if roi_mask[ri] || !mask[gi] {
                out[gi] = u[ri];
            }
        }
    }
    out
}

fn ring_mask(mask: &[bool], w: usize, h: usize, inner: usize, outer: usize) -> Vec<bool> {
    let inner = inner.max(1);
    let outer = outer.max(inner);
    let m_in = if inner > 1 {
        dilate(mask, w, h, inner - 1)
    } else {
        mask.to_vec()
    };
    let m_out = dilate(mask, w, h, outer);
    let mut ring = vec![false; mask.len()];
    for i in 0..ring.len() {
        ring[i] = m_out[i] && !m_in[i];
    }
    ring
}

/// Binary dilation by a 3×3 full structuring element (8-connectivity), applied `iters` times —
/// equal to one square dilation of radius `iters`, computed by the one square dilation
/// `ms_raster::dilate_square` (O(w*h) whatever `iters`). This is only the `bool` adapter.
///
/// `iters == 0` or a zero side returns the input unchanged. `mask` must hold `w * h` cells;
/// otherwise the call is a caller bug: it is logged and the input is returned undilated (same
/// length, so the ring arithmetic downstream stays in bounds). `run` allocates `w * h` cells,
/// so that branch is unreachable from the tool.
fn dilate(mask: &[bool], w: usize, h: usize, iters: usize) -> Vec<bool> {
    if iters == 0 || w == 0 || h == 0 {
        return mask.to_vec();
    }
    let bytes = mask.iter().map(|&on| u8::from(on)).collect::<Vec<u8>>();
    match ms_raster::dilate_square(&bytes, w, h, iters, iters, 1) {
        Ok(grown) => grown.into_iter().map(|px| px != 0).collect(),
        Err(err) => {
            ms_log::runtime_log::log_error(format!("[gradient] dilation skipped: {err}"));
            mask.to_vec()
        }
    }
}

/// Planar CIELAB (`l`, `a`, `b`) of row-major sRGB pixels, through the project's one
/// conversion (`ms_region_postprocess::LabTables`).
fn rgb_to_lab(rgb: &[[u8; 3]]) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let tables = LabTables::new();
    let mut l = Vec::with_capacity(rgb.len());
    let mut a = Vec::with_capacity(rgb.len());
    let mut b = Vec::with_capacity(rgb.len());
    for px in rgb {
        let [pl, pa, pb] = tables.to_lab(*px);
        l.push(pl);
        a.push(pa);
        b.push(pb);
    }
    (l, a, b)
}

/// Planar CIELAB -> row-major sRGB pixels (`w` = row width, for the dither position).
///
/// The Lab -> linear step is the shared `LabTables::to_linear_rgb`; the quantization stays this
/// tool's own: the linear value is clipped only BELOW 0 before `encode_srgb`, the optional
/// dither (`dither_eps` levels of `pseudo_noise`, only where `dither_mask` is set) is added to
/// the still-unclipped encoded value, and the result is clamped to `0..=1` and rounded half up.
/// That differs from `LabTables::to_srgb` (clip to `0..=1` first, round half to even) on
/// purpose: it keeps the gradient fill's pinned output.
fn lab_to_rgb(
    l: &[f32],
    a: &[f32],
    b: &[f32],
    w: usize,
    _h: usize,
    dither_eps: f32,
    dither_mask: Option<&[bool]>,
) -> Vec<[u8; 3]> {
    let tables = LabTables::new();
    let mut out = vec![[0u8; 3]; l.len()];
    let dither_amp = if dither_eps > 0.0 {
        dither_eps / 255.0
    } else {
        0.0
    };

    for idx in 0..l.len() {
        let [mut r, mut g, mut bl] = tables
            .to_linear_rgb([l[idx], a[idx], b[idx]])
            .map(|linear| encode_srgb(linear.max(0.0)));

        if dither_amp > 0.0 && dither_mask.is_none_or(|m| m[idx]) {
            let (x, y) = (idx % w, idx / w);
            r = (r + pseudo_noise(x, y, 0) * dither_amp).clamp(0.0, 1.0);
            g = (g + pseudo_noise(x, y, 1) * dither_amp).clamp(0.0, 1.0);
            bl = (bl + pseudo_noise(x, y, 2) * dither_amp).clamp(0.0, 1.0);
        }

        out[idx] = [
            (r * 255.0 + 0.5).clamp(0.0, 255.0) as u8,
            (g * 255.0 + 0.5).clamp(0.0, 255.0) as u8,
            (bl * 255.0 + 0.5).clamp(0.0, 255.0) as u8,
        ];
    }

    out
}

fn gradient_2d(arr: &[f32], w: usize, h: usize) -> (Vec<f32>, Vec<f32>) {
    let mut gy = vec![0.0f32; arr.len()];
    let mut gx = vec![0.0f32; arr.len()];

    for y in 0..h {
        for x in 0..w {
            let i = idx2d(x, y, w);

            gy[i] = if h <= 1 {
                0.0
            } else if y == 0 {
                arr[idx2d(x, 1, w)] - arr[i]
            } else if y == h - 1 {
                arr[i] - arr[idx2d(x, h - 2, w)]
            } else {
                0.5 * (arr[idx2d(x, y + 1, w)] - arr[idx2d(x, y - 1, w)])
            };

            gx[i] = if w <= 1 {
                0.0
            } else if x == 0 {
                arr[idx2d(1, y, w)] - arr[i]
            } else if x == w - 1 {
                arr[i] - arr[idx2d(w - 2, y, w)]
            } else {
                0.5 * (arr[idx2d(x + 1, y, w)] - arr[idx2d(x - 1, y, w)])
            };
        }
    }

    (gy, gx)
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn pseudo_noise(x: usize, y: usize, c: usize) -> f32 {
    let mut v = (x as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((y as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .wrapping_add((c as u64).wrapping_mul(0x94D0_49BB_1331_11EB));
    v ^= v >> 30;
    v = v.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    v ^= v >> 27;
    v = v.wrapping_mul(0x94D0_49BB_1331_11EB);
    v ^= v >> 31;
    let unit = ((v >> 40) as f32) / ((1u64 << 24) as f32);
    unit * 2.0 - 1.0
}

fn mask_bbox(mask: &[bool], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let mut x0 = w;
    let mut y0 = h;
    let mut x1 = 0usize;
    let mut y1 = 0usize;
    let mut any = false;

    for y in 0..h {
        for x in 0..w {
            if !mask[idx2d(x, y, w)] {
                continue;
            }
            any = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }

    any.then_some((x0, x1, y0, y1))
}

fn median_f32(values: &mut [f32]) -> f32 {
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        0.5 * (values[mid - 1] + values[mid])
    } else {
        values[mid]
    }
}

#[inline]
fn idx2d(x: usize, y: usize, w: usize) -> usize {
    y.saturating_mul(w).saturating_add(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference sequential dilation, copied verbatim from the pre-parallel implementation
    /// (clone-per-iteration, 3×3 full structuring element, replicate border).
    fn sequential_dilate_reference(mask: &[bool], w: usize, h: usize, iters: usize) -> Vec<bool> {
        let mut cur = mask.to_vec();
        for _ in 0..iters {
            let prev = cur.clone();
            for y in 0..h {
                let y0 = y.saturating_sub(1);
                let y1 = (y + 1).min(h.saturating_sub(1));
                for x in 0..w {
                    let x0 = x.saturating_sub(1);
                    let x1 = (x + 1).min(w.saturating_sub(1));
                    let mut on = false;
                    'outer: for ny in y0..=y1 {
                        for nx in x0..=x1 {
                            if prev[idx2d(nx, ny, w)] {
                                on = true;
                                break 'outer;
                            }
                        }
                    }
                    cur[idx2d(x, y, w)] = on;
                }
            }
        }
        cur
    }


    /// Dilation test: ping-pong/parallel dilation must equal the old clone-per-iter result exactly.
    #[test]
    fn parallel_dilate_matches_sequential() {
        let w = 13usize;
        let h = 11usize;
        let mut mask = vec![false; w * h];
        // A couple of seed points and an edge point to exercise replicate-border behavior.
        mask[idx2d(6, 5, w)] = true;
        mask[idx2d(1, 1, w)] = true;
        mask[idx2d(0, 0, w)] = true;
        mask[idx2d(w - 1, h - 1, w)] = true;

        for iters in 0..=4usize {
            let expected = sequential_dilate_reference(&mask, w, h, iters);
            let got = dilate(&mask, w, h, iters);
            assert_eq!(
                expected, got,
                "dilation mismatch for iters={iters} (structuring element / border drift)",
            );
        }
    }

    /// Single-row and single-column masks exercise the `h - 1` / `w - 1` saturating-clamp border
    /// path of `dilate`; the parallel result must equal the verbatim sequential reference.
    #[test]
    fn parallel_dilate_matches_sequential_thin() {
        // Single row: 5×1.
        {
            let (w, h) = (5usize, 1usize);
            let mut mask = vec![false; w * h];
            mask[idx2d(2, 0, w)] = true;
            mask[idx2d(0, 0, w)] = true;
            for iters in 0..=3usize {
                let expected = sequential_dilate_reference(&mask, w, h, iters);
                let got = dilate(&mask, w, h, iters);
                assert_eq!(
                    expected, got,
                    "single-row dilation mismatch for iters={iters}",
                );
            }
        }
        // Single column: 1×5.
        {
            let (w, h) = (1usize, 5usize);
            let mut mask = vec![false; w * h];
            mask[idx2d(0, 2, w)] = true;
            mask[idx2d(0, 0, w)] = true;
            for iters in 0..=3usize {
                let expected = sequential_dilate_reference(&mask, w, h, iters);
                let got = dilate(&mask, w, h, iters);
                assert_eq!(
                    expected, got,
                    "single-column dilation mismatch for iters={iters}",
                );
            }
        }
    }

    /// Empty/degenerate inputs must not panic and must round-trip the input.
    #[test]
    fn dilate_handles_degenerate_inputs() {
        assert_eq!(dilate(&[], 0, 0, 3), Vec::<bool>::new());
        let mask = vec![true, false, true, false];
        assert_eq!(dilate(&mask, 2, 2, 0), mask);
    }

    // Characterization tests (detector Phase 1 refactor): the expected values were
    // OBSERVED from the pre-refactor code, never derived by hand. The random mask is
    // the same one (generator, seed, 37x23, ~8 %) the translation `dilate_binary`
    // characterization test uses, so equal set counts across the copies are visible.

    /// FNV-1a 64 over `bytes`: a compact, dependency-free fingerprint for pinned buffers.
    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// Deterministic 64-bit LCG (Knuth MMIX constants); returns the high 31 bits.
    fn lcg_next(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // `>> 33` leaves 31 significant bits, so the conversion cannot fail.
        u32::try_from(*state >> 33).unwrap_or(0)
    }

    /// Random 37x23 mask with ~8 % set pixels using the mixed values 1/7/255.
    fn characterization_mask() -> Vec<u8> {
        let mut state = 0xd11a_7e01_u64;
        (0..37 * 23)
            .map(|_| {
                let roll = lcg_next(&mut state);
                if roll % 100 < 8 {
                    [1u8, 7, 255][usize::try_from(roll % 3).unwrap_or(0)]
                } else {
                    0
                }
            })
            .collect()
    }

    /// characterization: the gradient `dilate` (iterated 3x3 over `bool`) at
    /// iters = 0, 1, 3.
    #[test]
    fn characterization_dilate_random() {
        let src = characterization_mask().into_iter().map(|px| px != 0).collect::<Vec<_>>();
        let observed = [0usize, 1, 3]
            .into_iter()
            .map(|iters| {
                let out = dilate(&src, 37, 23, iters);
                let bytes = out.iter().map(|&on| u8::from(on)).collect::<Vec<_>>();
                (iters, out.iter().filter(|&&on| on).count(), fnv1a64(&bytes))
            })
            .collect::<Vec<_>>();
        // (iters, set count, FNV of the 0/1 bytes): byte-identical to the translation
        // `dilate_binary` (r, r) output on the same mask.
        assert_eq!(
            observed,
            vec![
                (0, 64, 9_260_574_274_878_741_243),
                (1, 407, 12_066_787_267_297_046_334),
                (3, 834, 13_298_246_393_450_976_053),
            ],
            "observed={observed:?}"
        );
    }

    /// Deterministic `w x h` RGBA fixture: a diagonal two-colour ramp with LCG noise, plus a
    /// mask that covers an off-centre ellipse (alpha 255) and one stray pixel (alpha 9).
    fn characterization_fill_fixture(w: usize, h: usize, seed: u64) -> (egui::ColorImage, egui::ColorImage) {
        let mut state = seed;
        let mut image = egui::ColorImage::filled([w, h], Color32::BLACK);
        let mut mask = egui::ColorImage::filled([w, h], Color32::TRANSPARENT);
        let (cx, cy) = (w * 3 / 5, h / 2);
        for y in 0..h {
            for x in 0..w {
                let t = (x + 2 * y) * 255 / (w + 2 * h);
                let noise = usize::try_from(lcg_next(&mut state) % 9).unwrap_or(0);
                let r = u8::try_from((40 + t * 3 / 4 + noise).min(255)).unwrap_or(255);
                let g = u8::try_from((200usize.saturating_sub(t / 2) + noise).min(255)).unwrap_or(255);
                let b = u8::try_from((90 + t / 3).min(255)).unwrap_or(255);
                image.pixels[y * w + x] = Color32::from_rgb(r, g, b);
                let dx = x.abs_diff(cx);
                let dy = y.abs_diff(cy);
                if dx * dx * 4 + dy * dy * 9 <= (w * w) / 4 {
                    mask.pixels[y * w + x] = Color32::from_rgba_unmultiplied(255, 255, 255, 255);
                }
            }
        }
        mask.pixels[w + 1] = Color32::from_rgba_unmultiplied(0, 0, 0, 9);
        (image, mask)
    }

    /// characterization (S-066 Lab move): the whole gradient fill on two noisy ramps, pinned
    /// by an FNV-1a fingerprint of the output RGBA bytes. The values were OBSERVED from the
    /// tool's private Lab conversion before it switched to the shared
    /// `ms_region_postprocess::LabTables`; they must stay byte-identical.
    #[test]
    fn characterization_gradient_fill_output() {
        let observed = [(29usize, 21usize, 0x5eed_0001_u64), (40, 17, 0x5eed_0002)]
            .into_iter()
            .map(|(w, h, seed)| {
                let (image, mask) = characterization_fill_fixture(w, h, seed);
                let out = GradientFillTool::run(&image, &mask).unwrap_or_else(|err| panic!("fixture {w}x{h}: {err}"));
                let bytes = out.pixels.iter().flat_map(|px| px.to_array()).collect::<Vec<_>>();
                let changed = out.pixels.iter().zip(&image.pixels).filter(|(a, b)| a != b).count();
                (w, h, changed, fnv1a64(&bytes))
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![(29, 21, 111, 4_103_040_897_038_428_570), (40, 17, 209, 9_970_743_423_388_696_185)], "observed={observed:?}");
    }

    /// characterization (S-066 Lab move): `rgb_to_lab` -> `lab_to_rgb` with and without the
    /// dither over a 9-level RGB cube (729 colours), pinned by FNV-1a fingerprints observed
    /// before the switch to the shared Lab conversion.
    #[test]
    fn characterization_lab_round_trip() {
        let levels = [0u8, 1, 17, 64, 127, 128, 200, 254, 255];
        let mut cube = Vec::with_capacity(levels.len().pow(3));
        for r in levels {
            for g in levels {
                for b in levels {
                    cube.push([r, g, b]);
                }
            }
        }
        let (l, a, b) = rgb_to_lab(&cube);
        let w = levels.len();
        let plain = lab_to_rgb(&l, &a, &b, w, cube.len() / w, 0.0, None);
        let dithered = lab_to_rgb(&l, &a, &b, w, cube.len() / w, 0.6, None);
        let identity = plain.iter().zip(&cube).filter(|(x, y)| x == y).count();
        let observed = (identity, fnv1a64(&plain.concat()), fnv1a64(&dithered.concat()));
        assert_eq!(observed, (729, 7_310_844_075_801_908_823, 11_622_179_329_196_392_038), "observed={observed:?}");
    }

    /// characterization (parameter-struct refactor): the constant-from-ring fallback, which the
    /// whole-fill fixtures never reach, on the first fixture's ramp with a ring around its mask.
    /// The fingerprint was OBSERVED before the Lab planes became one struct; it must stay
    /// byte-identical.
    #[test]
    fn characterization_fill_constant_from_ring() {
        let (w, h) = (29usize, 21usize);
        let (image, mask_image) = characterization_fill_fixture(w, h, 0x5eed_0001);
        let base_rgb = image.pixels.iter().map(|px| [px.r(), px.g(), px.b()]).collect::<Vec<_>>();
        let mask = mask_image.pixels.iter().map(|px| px.a() > 127).collect::<Vec<_>>();
        let ring = ring_mask(&mask, w, h, 1, 3);
        let (l, a, b) = rgb_to_lab(&base_rgb);
        let out = fill_constant_from_ring(&base_rgb, &mask, LabPlanes { l: &l, a: &a, b: &b, width: w, height: h }, &ring);
        let changed = out.iter().zip(&base_rgb).filter(|(x, y)| x != y).count();
        let observed = (changed, fnv1a64(&out.concat()));
        assert_eq!(observed, (111, 4_430_390_945_410_059_422), "observed={observed:?}");
    }
}
