/*
FILE HEADER (cleaning/tools/gradient.rs)
- Назначение: инструмент "Градиент" на базе `RegionMaskInpaintToolBase`.
- Ключевые сущности:
  - `GradientFillTool`: wiring инструмента (выделение региона, UI, routing ввода/курсора).
  - `run(...)`: основной pipeline масочного заполнения из Python `ui_new/tools/gradient_fill.py`.
- Алгоритм:
  - Перевод RGB -> Lab.
  - По маске выбирается ROI и оценивается лучший угол тонких scanline-линий.
  - Заполняется область под маской вдоль найденного угла с smoothstep-смешиванием.
  - Пустоты дозаполняются nearest-проходами, L-канал дополнительно согласуется screened-poisson.
  - Fallback: константная заливка медианой по кольцу вокруг маски.
- Важно:
  - Обработка запускается по кнопке "Обработать" в `RegionMaskInpaintToolBase`.
  - По `Применить` результат вставляется обратно в clean-overlay выбранного региона.
- Параллелизм (rayon, глобальный пул):
  - `red_black_sor_sweeps`: красно-чёрный SOR. Полусвип одного цвета параллелится по строкам;
    обновляемые ячейки одного цвета не читают друг друга (5-точечный stencil читает только соседей
    противоположного цвета), поэтому row-параллель численно идентична последовательному варианту.
    Два полусвипа (red/black) остаются последовательными между собой.
  - `dilate`: ping-pong буфер вместо клонирования на итерацию; каждая итерация параллелится по
    строкам (каждая выходная строка читает 3×3 окно из предыдущего буфера).
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, StrokePoint};
use crate::canvas::CanvasView;
use crate::project::ProjectData;
use eframe::egui;
use egui::Color32;
use rayon::prelude::*;

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
            return Err("Размер изображения и маски не совпадает.".to_string());
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
    fn tool_id(&self) -> &'static str {
        "gradient_fill"
    }

    fn title(&self) -> &'static str {
        "Градиент"
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small("Алгоритм: gradient scanlines в Lab + Poisson-согласование.");
        ui.small("Shift+ЛКМ по canvas: выделить регион.");
    }

    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        self.inpaint_base.on_key_event(ctx)
    }

    fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.inpaint_base.on_wheel_event(delta_y, modifiers)
    }

    fn set_space_pan_active(&mut self, active: bool) {
        self.inpaint_base.set_space_pan_active(active);
    }

    fn set_ai_backend_available(&mut self, available: bool) {
        self.inpaint_base.set_ai_backend_available(available);
    }

    fn set_ai_backend_torch_available(&mut self, available: bool) {
        self.inpaint_base.set_ai_backend_torch_available(available);
    }

    fn wants_primary_stroke(&self, point: StrokePoint) -> bool {
        self.inpaint_base.wants_primary_stroke(point)
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

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        self.inpaint_base
            .draw_overlay_ui(ctx, canvas, project, "Градиентная заливка", Self::run);
    }

    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        self.inpaint_base.draw_cursor(ui, canvas, pointer_scene_pos);
    }

    fn captures_canvas_pointer(&self, pointer_pos: egui::Pos2) -> bool {
        self.inpaint_base.editor_window_contains(pointer_pos)
    }

    fn block_canvas_zoom(&self) -> bool {
        self.inpaint_base.has_open_editor()
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
        let score = angle_consistency_score(
            &l_roi,
            &a_roi,
            &b_roi,
            &roi_mask,
            rw,
            rh,
            a_deg as f32,
            10.0,
            2,
            1.0,
            300,
        );
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

    let mut fill_l = vec![f32::NAN; rw.saturating_mul(rh)];
    let mut fill_a = vec![f32::NAN; rw.saturating_mul(rh)];
    let mut fill_b = vec![f32::NAN; rw.saturating_mul(rh)];

    scan_fill_lines(
        &l_roi,
        &a_roi,
        &b_roi,
        &roi_mask,
        rw,
        rh,
        best_theta,
        1,
        1.0,
        DELTA_E_THRESHOLD,
        &mut fill_l,
        &mut fill_a,
        &mut fill_b,
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
        return fill_constant_from_ring(base_rgb, mask, &l, &a, &b, w, h, &fallback_ring);
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

    let l_refined = screened_poisson_refine(&l_ref, &l, mask, w, h, pad + 4, 1.0, 120.0, 220, 1.95);
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

// All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
#[allow(clippy::too_many_arguments)]
fn fill_constant_from_ring(
    base_rgb: &[[u8; 3]],
    mask: &[bool],
    l: &[f32],
    a: &[f32],
    b: &[f32],
    w: usize,
    h: usize,
    ring: &[bool],
) -> Vec<[u8; 3]> {
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

    let l_refined = screened_poisson_refine(&l_hat, l, mask, w, h, 12, 1.0, 120.0, 200, 1.95);
    let rgb_hat = lab_to_rgb(&l_refined, &a_hat, &b_hat, w, h, 0.6, Some(mask));
    let mut out = base_rgb.to_vec();
    for idx in 0..mask.len() {
        if mask[idx] {
            out[idx] = rgb_hat[idx];
        }
    }
    out
}

// All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
#[allow(clippy::too_many_arguments)]
fn angle_consistency_score(
    l: &[f32],
    a: &[f32],
    b: &[f32],
    mask: &[bool],
    w: usize,
    h: usize,
    theta_deg: f32,
    delta_e_cap: f32,
    v_step: usize,
    t_step: f32,
    max_pairs: usize,
) -> f32 {
    let pairs = collect_boundary_pairs(l, a, b, mask, w, h, theta_deg, v_step, t_step, max_pairs);
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

// All parameters are distinct pixel-buffer or layout properties; grouping would obscure rendering intent.
#[allow(clippy::too_many_arguments)]
fn collect_boundary_pairs(
    l: &[f32],
    a: &[f32],
    b: &[f32],
    mask: &[bool],
    w: usize,
    h: usize,
    theta_deg: f32,
    v_step: usize,
    t_step: f32,
    max_pairs: usize,
) -> Vec<AngleBoundaryPair> {
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

#[allow(clippy::too_many_arguments)]
fn scan_fill_lines(
    l: &[f32],
    a: &[f32],
    b: &[f32],
    mask: &[bool],
    w: usize,
    h: usize,
    theta_deg: f32,
    v_step: usize,
    t_step: f32,
    delta_e_thr: f32,
    out_l: &mut [f32],
    out_a: &mut [f32],
    out_b: &mut [f32],
) {
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

#[allow(clippy::too_many_arguments)]
fn screened_poisson_refine(
    channel_pred: &[f32],
    channel_orig: &[f32],
    mask: &[bool],
    w: usize,
    h: usize,
    pad: usize,
    lam_in: f32,
    lam_out: f32,
    iters: usize,
    omega: f32,
) -> Vec<f32> {
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
        red_black_sor_sweeps(&mut u, &u0, &lam, &denom, rw, rh, iters, omega);
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

/// Runs `iters` red-black SOR iterations over the interior of an `rw`×`rh` ROI in place.
///
/// Each iteration performs two half-sweeps (red then black). A half-sweep updates only the
/// cells of one color, where a cell's color is `(x + y) & 1`. The 5-point Laplacian stencil
/// reads only the 4 axis neighbors, which all have the opposite color, so within one half-sweep
/// no updated cell is read by another updated cell. The updates within a half-sweep are therefore
/// mutually independent and order-invariant, which makes a row-parallel sweep numerically
/// identical to the sequential one.
///
/// `u` is the working buffer (interior updated in place), `u0` the data-fidelity reference,
/// `lam` the per-cell fidelity weight, `denom` the precomputed `4 + lam[i]`. The two half-sweeps
/// stay sequential w.r.t. each other; parallelism is only within a half-sweep, across rows.
// All parameters are distinct solver buffers or ROI dimensions; grouping would obscure the kernel.
#[allow(clippy::too_many_arguments)]
fn red_black_sor_sweeps(
    u: &mut [f32],
    u0: &[f32],
    lam: &[f32],
    denom: &[f32],
    rw: usize,
    rh: usize,
    iters: usize,
    omega: f32,
) {
    // Scratch holds the newly computed value for each updated cell of the current half-sweep.
    // Because updated cells are never read within the same half-sweep, computing into scratch
    // from the immutable snapshot `u` and copying back yields the exact in-place result while
    // letting rows be processed in parallel without aliasing the writes.
    //
    // The buffer is allocated once and deliberately NOT re-zeroed between half-sweeps or
    // iterations: cells of the parity NOT being updated this half-sweep retain stale values from
    // a previous half-sweep, but the copyback below reads only current-parity cells (it recomputes
    // the exact same parity-selection predicate as the compute loop), so those stale cells are
    // never read. Do not add a stale read of `scratch` outside that predicate.
    let mut scratch = vec![0.0f32; u.len()];
    for _ in 0..iters {
        for parity in 0..=1usize {
            // Compute updates for every interior cell of this color in parallel by row. Each row's
            // updated cells write only into `scratch[row]`; reads of `u` touch opposite-color
            // cells (neighbors) that are not updated this half-sweep, so the shared `&u` borrow is
            // race-free. The stride between consecutive rows in the flat buffer is `rw`.
            scratch
                .par_chunks_mut(rw)
                .enumerate()
                .skip(1)
                .take(rh.saturating_sub(2))
                .for_each(|(y, scratch_row)| {
                    let xstart = 1 + ((parity ^ (y & 1)) & 1);
                    let row_base = y * rw;
                    for x in (xstart..(rw - 1)).step_by(2) {
                        let i = row_base + x;
                        let nbr = u[i - 1] + u[i + 1] + u[i - rw] + u[i + rw];
                        let rhs = nbr + lam[i] * u0[i];
                        let next = rhs / denom[i];
                        scratch_row[x] = u[i] + omega * (next - u[i]);
                    }
                });

            // Apply the half-sweep results back into `u`. Only the cells of `parity` were written
            // in `scratch`; recompute the same membership to copy exactly those cells.
            for y in 1..(rh - 1) {
                let xstart = 1 + ((parity ^ (y & 1)) & 1);
                let row_base = y * rw;
                for x in (xstart..(rw - 1)).step_by(2) {
                    let i = row_base + x;
                    u[i] = scratch[i];
                }
            }
        }
    }
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

/// Binary dilation by a 3×3 full structuring element (8-connectivity), applied `iters` times.
///
/// Border pixels use replicate behavior (neighbor indices clamped into range), matching the
/// original implementation. Uses a ping-pong double buffer instead of cloning per iteration and
/// parallelizes each iteration across output rows: every output cell reads only the previous
/// buffer's 3×3 neighborhood and writes a single row, so rows are independent. The flat-buffer
/// row stride is `w`. The result is bit-for-bit identical to the sequential clone-per-iter form.
fn dilate(mask: &[bool], w: usize, h: usize, iters: usize) -> Vec<bool> {
    if iters == 0 || w == 0 || h == 0 {
        return mask.to_vec();
    }

    let mut src = mask.to_vec();
    let mut dst = vec![false; mask.len()];
    for _ in 0..iters {
        // Compute each output row in parallel from the immutable `src` buffer.
        dst.par_chunks_mut(w).enumerate().for_each(|(y, dst_row)| {
            let y0 = y.saturating_sub(1);
            let y1 = (y + 1).min(h - 1);
            for (x, cell) in dst_row.iter_mut().enumerate() {
                let x0 = x.saturating_sub(1);
                let x1 = (x + 1).min(w - 1);
                let mut on = false;
                'outer: for ny in y0..=y1 {
                    let row_base = ny * w;
                    for nx in x0..=x1 {
                        if src[row_base + nx] {
                            on = true;
                            break 'outer;
                        }
                    }
                }
                *cell = on;
            }
        });
        // Swap buffers: the freshly written `dst` becomes the source for the next iteration.
        std::mem::swap(&mut src, &mut dst);
    }
    // After the final swap, `src` holds the latest result.
    src
}

fn rgb_to_lab(rgb: &[[u8; 3]]) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let mut l = vec![0.0f32; rgb.len()];
    let mut a = vec![0.0f32; rgb.len()];
    let mut b = vec![0.0f32; rgb.len()];

    const EPS: f32 = 216.0 / 24389.0;
    const KAPPA: f32 = 24389.0 / 27.0;
    const XN: f32 = 0.95047;
    const YN: f32 = 1.0;
    const ZN: f32 = 1.08883;

    for (i, px) in rgb.iter().enumerate() {
        let r = srgb_to_linear(px[0]);
        let g = srgb_to_linear(px[1]);
        let bl = srgb_to_linear(px[2]);

        let x = (0.4124564 * r + 0.3575761 * g + 0.1804375 * bl) / XN;
        let y = (0.2126729 * r + 0.7151522 * g + 0.0721750 * bl) / YN;
        let z = (0.0193339 * r + 0.119_192 * g + 0.9503041 * bl) / ZN;

        let fx = if x > EPS {
            x.cbrt()
        } else {
            (KAPPA * x + 16.0) / 116.0
        };
        let fy = if y > EPS {
            y.cbrt()
        } else {
            (KAPPA * y + 16.0) / 116.0
        };
        let fz = if z > EPS {
            z.cbrt()
        } else {
            (KAPPA * z + 16.0) / 116.0
        };

        l[i] = 116.0 * fy - 16.0;
        a[i] = 500.0 * (fx - fy);
        b[i] = 200.0 * (fy - fz);
    }

    (l, a, b)
}

fn lab_to_rgb(
    l: &[f32],
    a: &[f32],
    b: &[f32],
    w: usize,
    _h: usize,
    dither_eps: f32,
    dither_mask: Option<&[bool]>,
) -> Vec<[u8; 3]> {
    const EPS: f32 = 216.0 / 24389.0;
    const KAPPA: f32 = 24389.0 / 27.0;
    const XN: f32 = 0.95047;
    const YN: f32 = 1.0;
    const ZN: f32 = 1.08883;

    let mut out = vec![[0u8; 3]; l.len()];
    let dither_amp = if dither_eps > 0.0 {
        dither_eps / 255.0
    } else {
        0.0
    };

    for idx in 0..l.len() {
        let fy = (l[idx] + 16.0) / 116.0;
        let fx = fy + a[idx] / 500.0;
        let fz = fy - b[idx] / 200.0;

        let xr = if fx * fx * fx > EPS {
            fx * fx * fx
        } else {
            (116.0 * fx - 16.0) / KAPPA
        };
        let yr = if fy * fy * fy > EPS {
            fy * fy * fy
        } else {
            (116.0 * fy - 16.0) / KAPPA
        };
        let zr = if fz * fz * fz > EPS {
            fz * fz * fz
        } else {
            (116.0 * fz - 16.0) / KAPPA
        };

        let x = xr * XN;
        let y = yr * YN;
        let z = zr * ZN;

        let r_lin = 3.2404542 * x - 1.5371385 * y - 0.4985314 * z;
        let g_lin = -0.969_266 * x + 1.8760108 * y + 0.0415560 * z;
        let b_lin = 0.0556434 * x - 0.2040259 * y + 1.0572252 * z;

        let mut r = linear_to_srgb(r_lin);
        let mut g = linear_to_srgb(g_lin);
        let mut bl = linear_to_srgb(b_lin);

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

fn srgb_to_linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        12.92 * c.max(0.0)
    } else {
        1.055 * c.max(0.0).powf(1.0 / 2.4) - 0.055
    }
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

    /// Reference sequential red-black SOR, copied verbatim from the pre-parallel implementation.
    /// Used as the golden baseline that the parallel `red_black_sor_sweeps` must reproduce.
    // Mirrors the production kernel signature so the comparison is one-to-one.
    #[allow(clippy::too_many_arguments)]
    fn sequential_sor_reference(
        u: &mut [f32],
        u0: &[f32],
        lam: &[f32],
        denom: &[f32],
        rw: usize,
        rh: usize,
        iters: usize,
        omega: f32,
    ) {
        if rw < 3 || rh < 3 {
            return;
        }
        for _ in 0..iters {
            for parity in 0..=1usize {
                for y in 1..(rh - 1) {
                    let xstart = 1 + ((parity ^ (y & 1)) & 1);
                    for x in (xstart..(rw - 1)).step_by(2) {
                        let i = idx2d(x, y, rw);
                        let nbr = u[idx2d(x - 1, y, rw)]
                            + u[idx2d(x + 1, y, rw)]
                            + u[idx2d(x, y - 1, rw)]
                            + u[idx2d(x, y + 1, rw)];
                        let rhs = nbr + lam[i] * u0[i];
                        let next = rhs / denom[i];
                        u[i] = u[i] + omega * (next - u[i]);
                    }
                }
            }
        }
    }

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

    /// Builds a deterministic, mildly varied SOR fixture (interior masked, fidelity weights).
    fn build_sor_fixture(rw: usize, rh: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let n = rw * rh;
        let mut u0 = vec![0.0f32; n];
        let mut lam = vec![0.0f32; n];
        for y in 0..rh {
            for x in 0..rw {
                let i = idx2d(x, y, rw);
                // Smooth-ish initial field plus a deterministic ripple.
                u0[i] =
                    (x as f32) * 0.37 - (y as f32) * 0.21 + ((x * 7 + y * 13) % 11) as f32 * 0.05;
                // Mark an interior block as "inside the mask" with weak fidelity, rest strong.
                let inside = x >= rw / 4 && x < 3 * rw / 4 && y >= rh / 4 && y < 3 * rh / 4;
                lam[i] = if inside { 1.0 } else { 120.0 };
            }
        }
        let mut denom = vec![0.0f32; n];
        for i in 0..n {
            denom[i] = 4.0 + lam[i];
        }
        (u0, lam, denom)
    }

    /// Golden test: parallel red-black SOR must equal the sequential baseline bit-for-bit.
    ///
    /// The arithmetic per cell is identical and order-independent within a half-sweep, so the
    /// results should match exactly. The tolerance is kept explicit and tight to catch any
    /// stencil/index regression rather than to paper over reordering error.
    #[test]
    fn parallel_sor_matches_sequential() {
        // Bit-exact equality is expected: the parallel and sequential paths apply the exact same
        // per-cell arithmetic in the same operation order (one thread owns each cell, no float
        // reordering across the parallel split), so identical f32 results are guaranteed. Any
        // nonzero diff would signal a real stencil/index regression, not reordering noise.
        const TOL: f32 = 0.0;
        // (3, 3) exercises the minimum interior: a single interior cell at (1, 1), so the parallel
        // `take(rh - 2)` row range and the sequential reference must agree at the smallest ROI.
        for &(rw, rh) in &[(3usize, 3usize), (5, 4), (17, 23), (40, 9), (33, 33)] {
            let (u0, lam, denom) = build_sor_fixture(rw, rh);
            let iters = 64usize;
            let omega = 1.95f32;

            let mut u_seq = u0.clone();
            sequential_sor_reference(&mut u_seq, &u0, &lam, &denom, rw, rh, iters, omega);

            let mut u_par = u0.clone();
            red_black_sor_sweeps(&mut u_par, &u0, &lam, &denom, rw, rh, iters, omega);

            assert_eq!(u_seq.len(), u_par.len());
            for (i, (a, b)) in u_seq.iter().zip(u_par.iter()).enumerate() {
                assert!(
                    (a - b).abs() <= TOL,
                    "SOR mismatch at {i} ({rw}x{rh}): seq={a} par={b}",
                );
            }
        }
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
}
