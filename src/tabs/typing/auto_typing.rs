/*
FILE HEADER (tabs/typing/auto_typing.rs)
- Назначение: алгоритм авто-тайпа для вкладки `Текст`: поиск оптического центра
  текстового оверлея и авто-поиск центра пузыря на composited-картинке
  (`страница + clean overlay` из shared cache).
- Ключевые сущности:
  - `TypingAutoTypingSettings`: runtime-параметры авто-тайпа из UI.
  - `TypingAutoTypingDetectionResult`: результат поиска пузыря в UV-координатах страницы
    (центр, bbox, контур, статус).
  - `compute_overlay_visual_center`: оптический центр оверлея
    (x по alpha-weight, y по row-profile + bias).
  - `detect_bubble_from_overlay_cache`: поиск пузыря от точки в UV на composited-изображении
    страницы из `CleanOverlaysModel::cached_page_rgba` с учётом clean overlay.
  - `smooth_contour_lone_spikes`: сглаживание одиночных выступов контура пузыря
    (типа "хвоста"), чтобы они меньше влияли на центрирование.
- Поведение:
  - Поиск пузыря выполняется region-growing с ограничениями по цветовым дельтам.
  - Область валидируется по геометрии (`fill/solidity/shape/radial`) как в тестовом
    `src/bin/test_center_find.rs`.
  - Для UI отладки результат возвращает UV-контур/границы/центр (по сглаженному контуру).
*/

use crate::models::clean_overlays_model::CleanOverlaysModel;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const OPTICAL_CENTER_Y_BIAS_RATIO: f32 = -0.02;

// bubble detection tuning
const MAX_COLOR_STEP_DELTA: f32 = 0.30;
const MAX_COLOR_MEAN_DELTA: f32 = 0.42;
const MAX_COLOR_SEED_DELTA: f32 = 0.22;
const SMOOTH_GRADIENT_MEAN_BONUS: f32 = 0.22;
const SMOOTH_GRADIENT_SEED_BONUS: f32 = 0.38;
const MIN_REGION_PIXELS: usize = 800;
const MAX_REGION_RATIO: f32 = 0.85;
const MIN_FILL_RATIO: f32 = 0.50;
const MIN_SOLIDITY: f32 = 0.90;
const MAX_SHAPE_FACTOR: f32 = 3.20;
const MAX_RADIAL_CV: f32 = 0.45;
const MIN_RADIAL_MIN_MEAN_RATIO: f32 = 0.45;
const CONTOUR_SPIKE_REL_OVERSHOOT: f32 = 0.07;
const CONTOUR_SPIKE_MIN_OVERSHOOT_PX: f32 = 4.0;
const CONTOUR_SPIKE_KEEP_RATIO: f32 = 0.35;
const CONTOUR_SMOOTH_PASSES: usize = 2;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TypingAutoTypingSettings {
    pub debug_visuals: bool,
    pub extra_downward_shift_percent: f32,
}

impl Default for TypingAutoTypingSettings {
    fn default() -> Self {
        Self {
            debug_visuals: false,
            extra_downward_shift_percent: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TypingAutoTypingDetectionResult {
    pub page_size: [usize; 2],
    pub accepted: bool,
    pub status: String,
    pub bubble_center_uv: Option<[f32; 2]>,
    pub bubble_bounds_uv: Option<[f32; 4]>,
    pub bubble_contour_uv: Vec<[f32; 2]>,
}

#[derive(Clone)]
struct AnalysisImage {
    width: usize,
    height: usize,
    rgba: Arc<Vec<u8>>,
}

#[derive(Clone, Copy)]
struct IPoint {
    x: i32,
    y: i32,
}

struct DetectionResult {
    status: String,
    accepted: bool,
    center: Option<(f32, f32)>,
    bounds: Option<(usize, usize, usize, usize)>,
    contour: Vec<(f32, f32)>,
}

pub(crate) fn compute_overlay_visual_center(
    overlay_size: [usize; 2],
    overlay_rgba: &[u8],
    extra_downward_shift_percent: f32,
) -> Option<[f32; 2]> {
    let width = overlay_size[0];
    let height = overlay_size[1];
    if width == 0 || height == 0 || overlay_rgba.len() != width * height * 4 {
        return None;
    }

    let mut sum_alpha = 0.0f64;
    let mut sum_x = 0.0f64;
    let mut row_alpha = vec![0.0f64; height];
    let mut ink_min_y = height;
    let mut ink_max_y = 0usize;

    for (y, row_a) in row_alpha.iter_mut().enumerate() {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let alpha = overlay_rgba[idx + 3] as f64;
            if alpha <= 0.0 {
                continue;
            }
            sum_alpha += alpha;
            sum_x += (x as f64 + 0.5) * alpha;
            *row_a += alpha;
            ink_min_y = ink_min_y.min(y);
            ink_max_y = ink_max_y.max(y);
        }
    }

    if sum_alpha <= f64::EPSILON {
        return None;
    }

    let mut sum_row_w = 0.0f64;
    let mut sum_y_optical = 0.0f64;
    for (y, row_sum) in row_alpha.iter().enumerate() {
        if *row_sum <= 0.0 {
            continue;
        }
        // Подавляем доминирование самых длинных строк.
        let w = row_sum.sqrt();
        sum_row_w += w;
        sum_y_optical += (y as f64 + 0.5) * w;
    }
    if sum_row_w <= f64::EPSILON {
        return None;
    }

    let center_x = (sum_x / sum_alpha) as f32;
    let mut center_y = (sum_y_optical / sum_row_w) as f32;
    let ink_height = (ink_max_y + 1 - ink_min_y) as f32;
    let dynamic_bias = OPTICAL_CENTER_Y_BIAS_RATIO + extra_downward_shift_percent / 100.0;
    center_y += ink_height * dynamic_bias;
    center_y = center_y.clamp(0.0, height as f32 - 1.0);

    Some([center_x, center_y])
}

pub(crate) fn detect_bubble_from_overlay_cache(
    model: &Arc<Mutex<CleanOverlaysModel>>,
    page_idx: usize,
    click_uv: [f32; 2],
) -> Result<TypingAutoTypingDetectionResult, String> {
    let image = build_source_from_cache_model(model, page_idx)?;
    if image.width == 0 || image.height == 0 {
        return Err(t!("typing.auto_typing.cache_page_zero_size_error").to_string());
    }

    let click_x = uv_to_px_index(click_uv[0], image.width);
    let click_y = uv_to_px_index(click_uv[1], image.height);
    let detection = detect_bubble_from_click(&image, click_x, click_y);

    let size = [image.width, image.height];
    Ok(TypingAutoTypingDetectionResult {
        page_size: size,
        accepted: detection.accepted,
        status: detection.status,
        bubble_center_uv: detection
            .center
            .map(|(cx, cy)| [px_to_uv_center(cx, size[0]), px_to_uv_center(cy, size[1])]),
        bubble_bounds_uv: detection.bounds.map(|(min_x, min_y, max_x, max_y)| {
            [
                px_to_uv_edge(min_x as f32, size[0]),
                px_to_uv_edge(min_y as f32, size[1]),
                px_to_uv_edge(max_x as f32, size[0]),
                px_to_uv_edge(max_y as f32, size[1]),
            ]
        }),
        bubble_contour_uv: detection
            .contour
            .into_iter()
            .map(|(x, y)| [px_to_uv_center(x, size[0]), px_to_uv_center(y, size[1])])
            .collect(),
    })
}

fn build_source_from_cache_model(
    model: &Arc<Mutex<CleanOverlaysModel>>,
    page_idx: usize,
) -> Result<AnalysisImage, String> {
    let mut locked = model
        .lock()
        .map_err(|_| t!("typing.errors.clean_overlay_cache_unavailable").to_string())?;

    let Some(page_rgba) = locked.cached_page_rgba(page_idx) else {
        return Err(
            t!("typing.auto_typing.page_not_ready_error").to_string(),
        );
    };
    let page_size = [page_rgba.width() as usize, page_rgba.height() as usize];
    if page_size[0] == 0 || page_size[1] == 0 {
        return Err(t!("typing.auto_typing.source_page_zero_size_error").to_string());
    }

    let mut out = page_rgba.as_raw().clone();

    if let Some(overlay) = locked.get(page_idx) {
        let overlay_size = overlay.size;
        let overlay_rgba = color_image_to_rgba_unmultiplied(overlay);
        let overlay_resized = if overlay_size == page_size {
            overlay_rgba
        } else {
            resize_rgba_nearest(overlay_rgba.as_slice(), overlay_size, page_size)
        };
        blend_rgba_source_over(out.as_mut_slice(), overlay_resized.as_slice());
    }

    Ok(AnalysisImage {
        width: page_size[0],
        height: page_size[1],
        rgba: Arc::new(out),
    })
}

fn uv_to_px_index(uv: f32, size: usize) -> usize {
    if size <= 1 {
        return 0;
    }
    (uv.clamp(0.0, 1.0) * (size as f32 - 1.0)).round() as usize
}

fn px_to_uv_center(px: f32, size: usize) -> f32 {
    if size == 0 {
        return 0.0;
    }
    ((px + 0.5) / size as f32).clamp(0.0, 1.0)
}

fn px_to_uv_edge(px: f32, size: usize) -> f32 {
    if size == 0 {
        return 0.0;
    }
    (px / size as f32).clamp(0.0, 1.0)
}

fn detect_bubble_from_click(
    image: &AnalysisImage,
    click_x: usize,
    click_y: usize,
) -> DetectionResult {
    if click_x >= image.width || click_y >= image.height {
        return DetectionResult {
            status: t!("typing.auto_typing.search_point_outside_error").to_string(),
            accepted: false,
            center: None,
            bounds: None,
            contour: Vec::new(),
        };
    }

    let width = image.width;
    let height = image.height;
    let area_cap = ((width * height) as f32 * MAX_REGION_RATIO) as usize;

    let mut in_region = vec![false; width * height];
    let mut queue = VecDeque::new();
    let seed_idx = click_y * width + click_x;
    in_region[seed_idx] = true;
    queue.push_back(seed_idx);

    let seed_color = rgb_at(image, seed_idx);
    let mut sum_r = seed_color[0] as f64;
    let mut sum_g = seed_color[1] as f64;
    let mut sum_b = seed_color[2] as f64;
    let mut count: usize = 1;

    while let Some(idx) = queue.pop_front() {
        let x = idx % width;
        let y = idx / width;
        let current = rgb_at(image, idx);
        let neigh = [
            (x as i32 - 1, y as i32),
            (x as i32 + 1, y as i32),
            (x as i32, y as i32 - 1),
            (x as i32, y as i32 + 1),
        ];

        let mean = [
            (sum_r / count as f64) as f32,
            (sum_g / count as f64) as f32,
            (sum_b / count as f64) as f32,
        ];

        for (nx, ny) in neigh {
            if nx < 0 || ny < 0 {
                continue;
            }
            let nx = nx as usize;
            let ny = ny as usize;
            if nx >= width || ny >= height {
                continue;
            }

            let n_idx = ny * width + nx;
            if in_region[n_idx] {
                continue;
            }

            let n_color = rgb_at(image, n_idx);
            let step_delta = color_delta_ratio(current, n_color);
            if step_delta > MAX_COLOR_STEP_DELTA {
                continue;
            }

            // Для плавного градиента внутри пузыря ослабляем глобальные лимиты.
            let smooth_factor = smooth_transition_factor(step_delta);
            let mean_limit = MAX_COLOR_MEAN_DELTA + SMOOTH_GRADIENT_MEAN_BONUS * smooth_factor;
            let seed_limit = MAX_COLOR_SEED_DELTA + SMOOTH_GRADIENT_SEED_BONUS * smooth_factor;

            let mean_delta = color_delta_ratio_f32(
                mean,
                [n_color[0] as f32, n_color[1] as f32, n_color[2] as f32],
            );
            if mean_delta > mean_limit {
                continue;
            }
            let seed_delta = color_delta_ratio(seed_color, n_color);
            if seed_delta > seed_limit {
                continue;
            }

            in_region[n_idx] = true;
            queue.push_back(n_idx);
            sum_r += n_color[0] as f64;
            sum_g += n_color[1] as f64;
            sum_b += n_color[2] as f64;
            count += 1;

            if count > area_cap {
                return DetectionResult {
                    status: t!("typing.auto_typing.region_too_large_error").to_string(),
                    accepted: false,
                    center: None,
                    bounds: None,
                    contour: Vec::new(),
                };
            }
        }
    }

    if count < MIN_REGION_PIXELS {
        return DetectionResult {
            status: tf!("typing.auto_typing.region_too_small_error", count = count),
            accepted: false,
            center: None,
            bounds: None,
            contour: Vec::new(),
        };
    }

    let mut region_points: Vec<IPoint> = Vec::with_capacity(count);
    let mut boundary: Vec<IPoint> = Vec::new();
    let mut min_x = width - 1;
    let mut max_x = 0usize;
    let mut min_y = height - 1;
    let mut max_y = 0usize;

    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            if !in_region[idx] {
                continue;
            }
            region_points.push(IPoint {
                x: x as i32,
                y: y as i32,
            });
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);

            let neighbors = [
                (x as i32 - 1, y as i32),
                (x as i32 + 1, y as i32),
                (x as i32, y as i32 - 1),
                (x as i32, y as i32 + 1),
            ];
            let mut is_boundary = false;
            for (nx, ny) in neighbors {
                if nx < 0 || ny < 0 || nx as usize >= width || ny as usize >= height {
                    is_boundary = true;
                    break;
                }
                let n_idx = ny as usize * width + nx as usize;
                if !in_region[n_idx] {
                    is_boundary = true;
                    break;
                }
            }
            if is_boundary {
                boundary.push(IPoint {
                    x: x as i32,
                    y: y as i32,
                });
            }
        }
    }

    let shape = evaluate_bubble_shape(&region_points, &boundary, min_x, min_y, max_x, max_y);
    let raw_bounds = (min_x, min_y, max_x, max_y);
    let raw_contour = build_contour_polyline(&boundary, region_points.len());
    let contour = smooth_contour_lone_spikes(&raw_contour);
    let bounds_px = contour_bounds_px(&contour, width, height).unwrap_or(raw_bounds);
    let bounds = Some(bounds_px);
    if !shape.accepted {
        return DetectionResult {
            status: tf!("typing.auto_typing.not_a_bubble_error", shape = shape.reason),
            accepted: false,
            center: None,
            bounds,
            contour,
        };
    }

    let center = contour_polygon_centroid(&contour).or_else(|| {
        let center_x = (bounds_px.0 as f32 + bounds_px.2 as f32) * 0.5;
        let center_y = (bounds_px.1 as f32 + bounds_px.3 as f32) * 0.5;
        Some((center_x, center_y))
    });
    let center = center.map(|(cx, cy)| {
        (
            cx.clamp(0.0, width as f32 - 1.0),
            cy.clamp(0.0, height as f32 - 1.0),
        )
    });
    let (center_x, center_y) = center.unwrap_or((0.0, 0.0));
    DetectionResult {
        // Manual `tf!`: the tool cannot express the `{:.1}` numeric precision, so the
        // formatted coordinates are pre-rendered and passed as plain string arguments.
        status: tf!(
            "typing.auto_typing.center_found_status",
            x = format!("{center_x:.1}"),
            y = format!("{center_y:.1}"),
            shape = shape.class_label
        ),
        accepted: true,
        center: Some((center_x, center_y)),
        bounds,
        contour,
    }
}

fn build_contour_polyline(boundary: &[IPoint], region_size: usize) -> Vec<(f32, f32)> {
    if boundary.is_empty() {
        return Vec::new();
    }

    let mut cx = 0.0f32;
    let mut cy = 0.0f32;
    for p in boundary {
        cx += p.x as f32;
        cy += p.y as f32;
    }
    cx /= boundary.len() as f32;
    cy /= boundary.len() as f32;

    let mut polar: Vec<(f32, f32, f32, f32)> = boundary
        .iter()
        .map(|p| {
            let x = p.x as f32;
            let y = p.y as f32;
            let angle = (y - cy).atan2(x - cx);
            let dx = x - cx;
            let dy = y - cy;
            let r2 = dx * dx + dy * dy;
            (angle, r2, x, y)
        })
        .collect();
    polar.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut contour: Vec<(f32, f32)> = Vec::with_capacity(polar.len());
    let mut idx = 0usize;
    while idx < polar.len() {
        let angle = polar[idx].0;
        let mut best = polar[idx];
        idx += 1;
        while idx < polar.len() && (polar[idx].0 - angle).abs() < 0.02 {
            if polar[idx].1 > best.1 {
                best = polar[idx];
            }
            idx += 1;
        }
        contour.push((best.2, best.3));
    }

    let target = ((region_size as f32).sqrt() * 2.0).clamp(80.0, 480.0) as usize;
    if contour.len() > target {
        let step = contour.len() as f32 / target as f32;
        let mut reduced = Vec::with_capacity(target);
        let mut t = 0.0f32;
        while (t as usize) < contour.len() && reduced.len() < target {
            reduced.push(contour[t as usize]);
            t += step;
        }
        contour = reduced;
    }

    contour
}

fn smooth_contour_lone_spikes(contour: &[(f32, f32)]) -> Vec<(f32, f32)> {
    if contour.len() < 16 {
        return contour.to_vec();
    }

    let n = contour.len();
    let mut cx = 0.0f32;
    let mut cy = 0.0f32;
    for &(x, y) in contour {
        cx += x;
        cy += y;
    }
    cx /= n as f32;
    cy /= n as f32;

    let mut angles = Vec::with_capacity(n);
    let mut radii = Vec::with_capacity(n);
    for &(x, y) in contour {
        let dx = x - cx;
        let dy = y - cy;
        angles.push(dy.atan2(dx));
        radii.push((dx * dx + dy * dy).sqrt());
    }
    let mean_r = radii.iter().copied().sum::<f32>() / n as f32;
    let min_allowed = (mean_r * CONTOUR_SPIKE_REL_OVERSHOOT).max(CONTOUR_SPIKE_MIN_OVERSHOOT_PX);

    let mut limited_r = radii.clone();
    for i in 0..n {
        let l1 = radii[(i + n - 1) % n];
        let l2 = radii[(i + n - 2) % n];
        let r1 = radii[(i + 1) % n];
        let r2 = radii[(i + 2) % n];
        let baseline = (l1 + l2 + r1 + r2) * 0.25;
        let overshoot = radii[i] - baseline;
        let threshold = (baseline * CONTOUR_SPIKE_REL_OVERSHOOT).max(min_allowed);
        let is_local_peak = radii[i] > l1 && radii[i] > r1;
        if is_local_peak && overshoot > threshold {
            limited_r[i] = baseline + threshold * CONTOUR_SPIKE_KEEP_RATIO;
        }
    }

    let mut smoothed: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let r = limited_r[i];
            let a = angles[i];
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect();

    for _ in 0..CONTOUR_SMOOTH_PASSES {
        let prev = smoothed.clone();
        for i in 0..n {
            let p0 = prev[(i + n - 1) % n];
            let p1 = prev[i];
            let p2 = prev[(i + 1) % n];
            smoothed[i] = (
                p1.0 * 0.50 + (p0.0 + p2.0) * 0.25,
                p1.1 * 0.50 + (p0.1 + p2.1) * 0.25,
            );
        }
    }

    smoothed
}

fn contour_bounds_px(
    contour: &[(f32, f32)],
    width: usize,
    height: usize,
) -> Option<(usize, usize, usize, usize)> {
    if contour.is_empty() || width == 0 || height == 0 {
        return None;
    }
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for &(x, y) in contour {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    let max_x_edge = (width.saturating_sub(1)) as f32;
    let max_y_edge = (height.saturating_sub(1)) as f32;
    let mut out_min_x = min_x.floor().clamp(0.0, max_x_edge) as usize;
    let mut out_max_x = max_x.ceil().clamp(0.0, max_x_edge) as usize;
    let mut out_min_y = min_y.floor().clamp(0.0, max_y_edge) as usize;
    let mut out_max_y = max_y.ceil().clamp(0.0, max_y_edge) as usize;
    if out_min_x > out_max_x {
        std::mem::swap(&mut out_min_x, &mut out_max_x);
    }
    if out_min_y > out_max_y {
        std::mem::swap(&mut out_min_y, &mut out_max_y);
    }
    Some((out_min_x, out_min_y, out_max_x, out_max_y))
}

fn contour_polygon_centroid(contour: &[(f32, f32)]) -> Option<(f32, f32)> {
    if contour.is_empty() {
        return None;
    }
    if contour.len() < 3 {
        let mut sx = 0.0f32;
        let mut sy = 0.0f32;
        for &(x, y) in contour {
            sx += x;
            sy += y;
        }
        return Some((sx / contour.len() as f32, sy / contour.len() as f32));
    }

    let mut cross_sum = 0.0f64;
    let mut cx_sum = 0.0f64;
    let mut cy_sum = 0.0f64;
    for i in 0..contour.len() {
        let (x0, y0) = contour[i];
        let (x1, y1) = contour[(i + 1) % contour.len()];
        let cross = x0 as f64 * y1 as f64 - x1 as f64 * y0 as f64;
        cross_sum += cross;
        cx_sum += (x0 as f64 + x1 as f64) * cross;
        cy_sum += (y0 as f64 + y1 as f64) * cross;
    }
    if cross_sum.abs() <= f64::EPSILON {
        let mut sx = 0.0f32;
        let mut sy = 0.0f32;
        for &(x, y) in contour {
            sx += x;
            sy += y;
        }
        return Some((sx / contour.len() as f32, sy / contour.len() as f32));
    }

    let inv = 1.0 / (3.0 * cross_sum);
    Some(((cx_sum * inv) as f32, (cy_sum * inv) as f32))
}

struct ShapeEvaluation {
    accepted: bool,
    class_label: &'static str,
    reason: String,
}

fn evaluate_bubble_shape(
    region_points: &[IPoint],
    boundary: &[IPoint],
    min_x: usize,
    min_y: usize,
    max_x: usize,
    max_y: usize,
) -> ShapeEvaluation {
    if boundary.len() < 20 {
        return ShapeEvaluation {
            accepted: false,
            class_label: "",
            reason: t!("typing.auto_typing.shape_reason_border_too_short").to_string(),
        };
    }

    let area = region_points.len() as f32;
    let bbox_w = (max_x - min_x + 1) as f32;
    let bbox_h = (max_y - min_y + 1) as f32;
    let bbox_area = bbox_w * bbox_h;
    if bbox_area <= 1.0 {
        return ShapeEvaluation {
            accepted: false,
            class_label: "",
            reason: t!("typing.auto_typing.shape_reason_empty_bbox").to_string(),
        };
    }

    let fill_ratio = area / bbox_area;

    let mut cx = 0.0f32;
    let mut cy = 0.0f32;
    for p in region_points {
        cx += p.x as f32;
        cy += p.y as f32;
    }
    cx /= area;
    cy /= area;

    let mut sum_r = 0.0f32;
    let mut sum_r2 = 0.0f32;
    let mut min_r = f32::MAX;
    for p in boundary {
        let dx = p.x as f32 - cx;
        let dy = p.y as f32 - cy;
        let r = (dx * dx + dy * dy).sqrt();
        sum_r += r;
        sum_r2 += r * r;
        min_r = min_r.min(r);
    }
    let mean_r = sum_r / boundary.len() as f32;
    let var_r = (sum_r2 / boundary.len() as f32 - mean_r * mean_r).max(0.0);
    let std_r = var_r.sqrt();
    let radial_cv = if mean_r > 1.0 { std_r / mean_r } else { 1.0 };
    let min_mean_ratio = if mean_r > 1.0 { min_r / mean_r } else { 0.0 };

    let perimeter = boundary.len() as f32;
    let shape_factor = (perimeter * perimeter) / (4.0 * std::f32::consts::PI * area.max(1.0));

    let hull = convex_hull(boundary);
    let hull_area = polygon_area(&hull).max(1.0);
    let solidity = area / hull_area;

    // These reasons are joined into the user-facing "not a bubble" message, so they are
    // localized. Manual `tf!`: the tool cannot express the `{:.2}` numeric precision, so
    // each metric is pre-rendered and passed as a plain string argument.
    let mut reasons = Vec::new();
    if fill_ratio < MIN_FILL_RATIO {
        reasons.push(tf!(
            "typing.auto_typing.reason_low_fill",
            value = format!("{fill_ratio:.2}")
        ));
    }
    if solidity < MIN_SOLIDITY {
        reasons.push(tf!(
            "typing.auto_typing.reason_low_solidity",
            value = format!("{solidity:.2}")
        ));
    }
    if shape_factor > MAX_SHAPE_FACTOR {
        reasons.push(tf!(
            "typing.auto_typing.reason_ragged_border",
            value = format!("{shape_factor:.2}")
        ));
    }
    if radial_cv > MAX_RADIAL_CV {
        reasons.push(tf!(
            "typing.auto_typing.reason_radius_spread",
            value = format!("{radial_cv:.2}")
        ));
    }
    if min_mean_ratio < MIN_RADIAL_MIN_MEAN_RATIO {
        reasons.push(tf!(
            "typing.auto_typing.reason_deep_dents",
            value = format!("{min_mean_ratio:.2}")
        ));
    }

    if !reasons.is_empty() {
        return ShapeEvaluation {
            accepted: false,
            class_label: "",
            reason: reasons.join(", "),
        };
    }

    let class_label = if shape_factor < 1.45 {
        t!("typing.auto_typing.shape_reason_round")
    } else if fill_ratio > 0.78 && radial_cv > 0.20 && shape_factor < 2.6 {
        t!("typing.auto_typing.shape_reason_rectangular")
    } else {
        t!("typing.auto_typing.shape_reason_angular")
    };

    ShapeEvaluation {
        accepted: true,
        class_label,
        reason: String::new(),
    }
}

fn convex_hull(points: &[IPoint]) -> Vec<IPoint> {
    if points.len() <= 3 {
        return points.to_vec();
    }

    let mut pts = points.to_vec();
    pts.sort_by_key(|p| (p.x, p.y));
    pts.dedup_by_key(|p| (p.x, p.y));
    if pts.len() <= 3 {
        return pts;
    }

    let mut lower: Vec<IPoint> = Vec::with_capacity(pts.len());
    for p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], *p) <= 0 {
            lower.pop();
        }
        lower.push(*p);
    }

    let mut upper: Vec<IPoint> = Vec::with_capacity(pts.len());
    for p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], *p) <= 0 {
            upper.pop();
        }
        upper.push(*p);
    }

    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

fn cross(a: IPoint, b: IPoint, c: IPoint) -> i64 {
    let abx = (b.x - a.x) as i64;
    let aby = (b.y - a.y) as i64;
    let acx = (c.x - a.x) as i64;
    let acy = (c.y - a.y) as i64;
    abx * acy - aby * acx
}

fn polygon_area(poly: &[IPoint]) -> f32 {
    if poly.len() < 3 {
        return 0.0;
    }
    let mut acc = 0.0f64;
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        acc += a.x as f64 * b.y as f64 - b.x as f64 * a.y as f64;
    }
    (acc.abs() * 0.5) as f32
}

fn rgb_at(image: &AnalysisImage, idx: usize) -> [u8; 3] {
    let base = idx * 4;
    [image.rgba[base], image.rgba[base + 1], image.rgba[base + 2]]
}

fn color_delta_ratio(a: [u8; 3], b: [u8; 3]) -> f32 {
    let dr = (a[0] as f32 - b[0] as f32) / 255.0;
    let dg = (a[1] as f32 - b[1] as f32) / 255.0;
    let db = (a[2] as f32 - b[2] as f32) / 255.0;
    ((dr * dr + dg * dg + db * db) / 3.0).sqrt()
}

fn color_delta_ratio_f32(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dr = (a[0] - b[0]) / 255.0;
    let dg = (a[1] - b[1]) / 255.0;
    let db = (a[2] - b[2]) / 255.0;
    ((dr * dr + dg * dg + db * db) / 3.0).sqrt()
}

fn smooth_transition_factor(step_delta: f32) -> f32 {
    if MAX_COLOR_STEP_DELTA <= 0.0 {
        return 0.0;
    }
    let t = (1.0 - step_delta / MAX_COLOR_STEP_DELTA).clamp(0.0, 1.0);
    t * t
}

fn color_image_to_rgba_unmultiplied(image: &egui::ColorImage) -> Vec<u8> {
    let mut raw = Vec::with_capacity(image.pixels.len().saturating_mul(4));
    for px in &image.pixels {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        raw.extend_from_slice(&[r, g, b, a]);
    }
    raw
}

fn resize_rgba_nearest(src_rgba: &[u8], src_size: [usize; 2], dst_size: [usize; 2]) -> Vec<u8> {
    let [src_w, src_h] = src_size;
    let [dst_w, dst_h] = dst_size;
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return vec![0u8; dst_w.saturating_mul(dst_h).saturating_mul(4)];
    }
    if src_rgba.len() != src_w.saturating_mul(src_h).saturating_mul(4) {
        return vec![0u8; dst_w.saturating_mul(dst_h).saturating_mul(4)];
    }
    let mut out = vec![0u8; dst_w.saturating_mul(dst_h).saturating_mul(4)];
    for y in 0..dst_h {
        let sy = y.saturating_mul(src_h) / dst_h;
        for x in 0..dst_w {
            let sx = x.saturating_mul(src_w) / dst_w;
            let src_idx = (sy.saturating_mul(src_w).saturating_add(sx)).saturating_mul(4);
            let dst_idx = (y.saturating_mul(dst_w).saturating_add(x)).saturating_mul(4);
            out[dst_idx..dst_idx + 4].copy_from_slice(&src_rgba[src_idx..src_idx + 4]);
        }
    }
    out
}

fn blend_rgba_source_over(dst_rgba: &mut [u8], src_rgba: &[u8]) {
    if dst_rgba.len() != src_rgba.len() || !dst_rgba.len().is_multiple_of(4) {
        return;
    }
    for i in (0..dst_rgba.len()).step_by(4) {
        let sr = src_rgba[i] as f32 / 255.0;
        let sg = src_rgba[i + 1] as f32 / 255.0;
        let sb = src_rgba[i + 2] as f32 / 255.0;
        let sa = src_rgba[i + 3] as f32 / 255.0;
        if sa <= f32::EPSILON {
            continue;
        }

        let dr = dst_rgba[i] as f32 / 255.0;
        let dg = dst_rgba[i + 1] as f32 / 255.0;
        let db = dst_rgba[i + 2] as f32 / 255.0;
        let da = dst_rgba[i + 3] as f32 / 255.0;

        let out_a = sa + da * (1.0 - sa);
        let (out_r, out_g, out_b) = if out_a <= f32::EPSILON {
            (0.0, 0.0, 0.0)
        } else {
            (
                (sr * sa + dr * da * (1.0 - sa)) / out_a,
                (sg * sa + dg * da * (1.0 - sa)) / out_a,
                (sb * sa + db * da * (1.0 - sa)) / out_a,
            )
        };

        dst_rgba[i] = (out_r * 255.0).round().clamp(0.0, 255.0) as u8;
        dst_rgba[i + 1] = (out_g * 255.0).round().clamp(0.0, 255.0) as u8;
        dst_rgba[i + 2] = (out_b * 255.0).round().clamp(0.0, 255.0) as u8;
        dst_rgba[i + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}
