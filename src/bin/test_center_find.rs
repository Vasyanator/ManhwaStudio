#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/*
FILE OVERVIEW: src/bin/test_center_find.rs
Тестовый egui-бинарник для проверки позиционирования картинки-оверлея и поиска центра пузыря.

Ключевые константы:
- `BACKGROUND_IMAGE_PATH` — путь к фоновой картинке.
- `OVERLAY_IMAGE_PATH` — путь к картинке оверлея.
- Пороговые константы в блоке `bubble detection tuning` управляют ростом области и валидацией формы.

Ключевые сущности:
- `load_png_for_texture_and_analysis` — читает PNG и готовит данные для egui + анализа пикселей.
- `compute_overlay_visual_center` — считает оптический центр оверлея (x по alpha, y по row-profile + bias вниз).
- `spawn_detection_worker` — отдельный поток анализа, чтобы не блокировать GUI.
- `detect_bubble_from_click` — region growing от точки клика с остановкой на резком скачке цвета.
- `evaluate_bubble_shape` — проверка, похожа ли найденная область на круглый/квадратный/угловатый пузырь.
- `OverlayTestApp` — состояние текстур, drag-оверлея, автопоиска по `C`, debug toggle и результата поиска.

Поведение:
- Фон рисуется как базовая картинка в центральной панели.
- Оверлей рисуется поверх фона и перетаскивается мышью.
- По ПКМ внутри фона запускается фоновый поиск границ пузыря и его центра от точки клика.
- По `C` берётся визуальный центр оверлея, из этой точки запускается поиск пузыря, и при успехе оверлей
  сдвигается так, чтобы его визуальный центр совпал с центром пузыря.
- Чекбокс `Показывать отладку` управляет видимостью контуров, bbox и маркеров центров.
- При успехе рисуются bbox и маркер центра; при неуспехе выводится причина.
*/

use eframe::egui;
use egui::{ColorImage, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

const APP_TITLE: &str = "test_center_find";
const BACKGROUND_IMAGE_PATH: &str = "old_or_test/test_center_find/1.png";
const OVERLAY_IMAGE_PATH: &str = "old_or_test/test_center_find/text_overlay.png";
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

fn main() {
    let native_options = eframe::NativeOptions::default();
    let run_result = eframe::run_native(
        APP_TITLE,
        native_options,
        Box::new(|cc| Ok(Box::new(OverlayTestApp::new(cc)))),
    );

    if let Err(err) = run_result {
        eprintln!("[{APP_TITLE}] failed to start: {err}");
    }
}

#[derive(Clone)]
struct AnalysisImage {
    width: usize,
    height: usize,
    rgba: Arc<Vec<u8>>,
}

fn load_png_for_texture_and_analysis(path: &str) -> Result<(ColorImage, AnalysisImage), String> {
    let dyn_image = image::open(path).map_err(|e| format!("{path}: {e}"))?;
    let rgba = dyn_image.to_rgba8();
    let width = rgba.width() as usize;
    let height = rgba.height() as usize;
    let pixels = rgba.into_raw();
    let color_image = ColorImage::from_rgba_unmultiplied([width, height], &pixels);
    let analysis_image = AnalysisImage {
        width,
        height,
        rgba: Arc::new(pixels),
    };
    Ok((color_image, analysis_image))
}

fn compute_overlay_visual_center(image: &AnalysisImage) -> Option<Vec2> {
    let mut sum_alpha = 0.0f64;
    let mut sum_x = 0.0f64;
    let mut row_alpha = vec![0.0f64; image.height];
    let mut ink_min_y = image.height;
    let mut ink_max_y = 0usize;

    for (y, row_a) in row_alpha.iter_mut().enumerate().take(image.height) {
        for x in 0..image.width {
            let idx = (y * image.width + x) * 4;
            let alpha = image.rgba[idx + 3] as f64;
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
        // Вариант 2: подавляем доминирование самых длинных строк через sqrt(row_alpha).
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
    center_y += ink_height * OPTICAL_CENTER_Y_BIAS_RATIO;
    center_y = center_y.clamp(0.0, image.height as f32 - 1.0);

    Some(Vec2::new(center_x, center_y))
}

#[derive(Clone, Copy)]
struct IPoint {
    x: i32,
    y: i32,
}

struct DetectionRequest {
    token: u64,
    click_x: usize,
    click_y: usize,
}

struct DetectionResult {
    token: u64,
    status: String,
    accepted: bool,
    center: Option<(f32, f32)>,
    bounds: Option<(usize, usize, usize, usize)>,
    contour: Vec<(f32, f32)>,
}

fn spawn_detection_worker(
    image: AnalysisImage,
) -> (Sender<DetectionRequest>, Receiver<DetectionResult>) {
    let (job_tx, job_rx) = mpsc::channel::<DetectionRequest>();
    let (result_tx, result_rx) = mpsc::channel::<DetectionResult>();

    let _ = std::thread::Builder::new()
        .name("test-center-find-worker".to_string())
        .spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let result = detect_bubble_from_click(&image, job.token, job.click_x, job.click_y);
                let _ = result_tx.send(result);
            }
        });

    (job_tx, result_rx)
}

fn detect_bubble_from_click(
    image: &AnalysisImage,
    token: u64,
    click_x: usize,
    click_y: usize,
) -> DetectionResult {
    if click_x >= image.width || click_y >= image.height {
        return DetectionResult {
            token,
            status: "Клик вне изображения".to_string(),
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
            // Если переход резкий, бонус почти нулевой и пиксель отсекается как граница.
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
                    token,
                    status: "Область слишком большая: похоже, это не пузырь".to_string(),
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
            token,
            status: format!("Слишком маленькая область ({count} px): не пузырь"),
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
    let bounds = Some((min_x, min_y, max_x, max_y));
    let contour = build_contour_polyline(&boundary, region_points.len());
    if !shape.accepted {
        return DetectionResult {
            token,
            status: format!("Это не похоже на пузырь: {}", shape.reason),
            accepted: false,
            center: None,
            bounds,
            contour,
        };
    }

    let center_x = (min_x as f32 + max_x as f32) * 0.5;
    let center_y = (min_y as f32 + max_y as f32) * 0.5;
    let status = format!(
        "Центр найден ({:.1}, {:.1}), форма: {}",
        center_x, center_y, shape.class_label
    );

    DetectionResult {
        token,
        status,
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
            reason: "граница слишком короткая".to_string(),
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
            reason: "пустой bbox".to_string(),
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

    let mut reasons = Vec::new();
    if fill_ratio < MIN_FILL_RATIO {
        reasons.push(format!("низкая заполненность bbox ({fill_ratio:.2})"));
    }
    if solidity < MIN_SOLIDITY {
        reasons.push(format!(
            "сильные впадины/невыпуклость (solidity {solidity:.2})"
        ));
    }
    if shape_factor > MAX_SHAPE_FACTOR {
        reasons.push(format!("слишком рваная граница (shape {shape_factor:.2})"));
    }
    if radial_cv > MAX_RADIAL_CV {
        reasons.push(format!("большой разброс радиусов (cv {radial_cv:.2})"));
    }
    if min_mean_ratio < MIN_RADIAL_MIN_MEAN_RATIO {
        reasons.push(format!(
            "глубокие впадины контура (min/mean {min_mean_ratio:.2})"
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
        "круглый/овальный"
    } else if fill_ratio > 0.78 && radial_cv > 0.20 && shape_factor < 2.6 {
        "квадратный/прямоугольный"
    } else {
        "угловатый"
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

struct OverlayTestApp {
    background_texture: Option<TextureHandle>,
    overlay_texture: Option<TextureHandle>,
    background_size: Vec2,
    overlay_size: Vec2,
    overlay_visual_center: Option<Vec2>,
    overlay_position: Vec2,
    init_error: Option<String>,
    detect_job_tx: Option<Sender<DetectionRequest>>,
    detect_result_rx: Option<Receiver<DetectionResult>>,
    detect_next_token: u64,
    detect_pending_token: Option<u64>,
    detect_pending_auto_align_token: Option<u64>,
    detect_status: String,
    detect_accepted: bool,
    detect_center: Option<(f32, f32)>,
    detect_bounds: Option<(usize, usize, usize, usize)>,
    detect_contour: Vec<(f32, f32)>,
    show_debug_visuals: bool,
}

impl OverlayTestApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self {
            background_texture: None,
            overlay_texture: None,
            background_size: Vec2::ZERO,
            overlay_size: Vec2::ZERO,
            overlay_visual_center: None,
            overlay_position: Vec2::ZERO,
            init_error: None,
            detect_job_tx: None,
            detect_result_rx: None,
            detect_next_token: 1,
            detect_pending_token: None,
            detect_pending_auto_align_token: None,
            detect_status: "ПКМ: поиск центра пузыря, C: автосовмещение оверлея с пузырём"
                .to_string(),
            detect_accepted: false,
            detect_center: None,
            detect_bounds: None,
            detect_contour: Vec::new(),
            show_debug_visuals: true,
        };

        let background = load_png_for_texture_and_analysis(BACKGROUND_IMAGE_PATH);
        let overlay = load_png_for_texture_and_analysis(OVERLAY_IMAGE_PATH);

        match (background, overlay) {
            (Ok((bg_tex_img, bg_analysis)), Ok((ov_tex_img, ov_analysis))) => {
                app.background_size =
                    Vec2::new(bg_tex_img.size[0] as f32, bg_tex_img.size[1] as f32);
                app.overlay_size = Vec2::new(ov_tex_img.size[0] as f32, ov_tex_img.size[1] as f32);
                app.overlay_visual_center = compute_overlay_visual_center(&ov_analysis);
                app.overlay_position = (app.background_size - app.overlay_size) * 0.5;
                if app.overlay_visual_center.is_none() {
                    app.detect_status =
                        "Оверлей полностью прозрачный: центр по весу пикселей не найден"
                            .to_string();
                }

                app.background_texture = Some(cc.egui_ctx.load_texture(
                    "test_center_find_background",
                    bg_tex_img,
                    TextureOptions::LINEAR,
                ));
                app.overlay_texture = Some(cc.egui_ctx.load_texture(
                    "test_center_find_overlay",
                    ov_tex_img,
                    TextureOptions::LINEAR,
                ));

                let (job_tx, result_rx) = spawn_detection_worker(bg_analysis);
                app.detect_job_tx = Some(job_tx);
                app.detect_result_rx = Some(result_rx);
            }
            (bg_res, ov_res) => {
                let mut errors: Vec<String> = Vec::new();
                if let Err(err) = bg_res {
                    errors.push(format!("Background load error: {err}"));
                }
                if let Err(err) = ov_res {
                    errors.push(format!("Overlay load error: {err}"));
                }
                app.init_error = Some(errors.join("\n"));
            }
        }

        app
    }

    fn queue_detection_request(&mut self, click_x: usize, click_y: usize, auto_align: bool) {
        let Some(job_tx) = self.detect_job_tx.clone() else {
            return;
        };

        let token = self.detect_next_token;
        self.detect_next_token += 1;
        self.detect_pending_token = Some(token);
        self.detect_pending_auto_align_token = if auto_align { Some(token) } else { None };
        self.detect_status = if auto_align {
            format!("Идёт автоанализ от центра оверлея ({click_x}, {click_y})...")
        } else {
            format!("Идёт анализ точки ({click_x}, {click_y})...")
        };
        self.detect_accepted = false;
        self.detect_center = None;
        self.detect_bounds = None;
        self.detect_contour.clear();

        let _ = job_tx.send(DetectionRequest {
            token,
            click_x,
            click_y,
        });
    }

    fn trigger_overlay_center_auto_align(&mut self) {
        let Some(local_center) = self.overlay_visual_center else {
            self.detect_status = "Оверлей прозрачный: нет пикселей для центра по весу".to_string();
            self.detect_accepted = false;
            return;
        };

        if self.background_size.x < 1.0 || self.background_size.y < 1.0 {
            return;
        }

        let click_point = self.overlay_position + local_center;
        let click_x = click_point
            .x
            .clamp(0.0, self.background_size.x - 1.0)
            .round() as usize;
        let click_y = click_point
            .y
            .clamp(0.0, self.background_size.y - 1.0)
            .round() as usize;

        self.queue_detection_request(click_x, click_y, true);
    }

    fn poll_detection_results(&mut self) {
        let Some(rx) = &self.detect_result_rx else {
            return;
        };

        while let Ok(result) = rx.try_recv() {
            if Some(result.token) != self.detect_pending_token {
                continue;
            }
            let should_auto_align = self.detect_pending_auto_align_token == Some(result.token);
            self.detect_pending_token = None;
            self.detect_pending_auto_align_token = None;
            self.detect_accepted = result.accepted;
            self.detect_center = result.center;
            self.detect_bounds = result.bounds;
            self.detect_contour = result.contour;

            if should_auto_align && result.accepted {
                if let (Some((cx, cy)), Some(local_center)) =
                    (result.center, self.overlay_visual_center)
                {
                    self.overlay_position = Vec2::new(cx, cy) - local_center;
                    self.detect_status = format!(
                        "{} | Оверлей совмещён: центр оверлея -> центр пузыря",
                        result.status
                    );
                } else {
                    self.detect_status = format!(
                        "{} | Совмещение пропущено: нет центра оверлея",
                        result.status
                    );
                }
            } else if should_auto_align {
                self.detect_status = format!("{} | Совмещение не выполнено", result.status);
            } else {
                self.detect_status = result.status;
            }
        }
    }
}

impl eframe::App for OverlayTestApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.key_pressed(egui::Key::C)) {
            self.trigger_overlay_center_auto_align();
        }

        self.poll_detection_results();

        if self.detect_pending_token.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(APP_TITLE);
            ui.label(format!("Background: {BACKGROUND_IMAGE_PATH}"));
            ui.label(format!("Overlay: {OVERLAY_IMAGE_PATH}"));
            ui.separator();
            ui.checkbox(&mut self.show_debug_visuals, "Показывать отладку");

            let status_color = if self.detect_pending_token.is_some() {
                egui::Color32::YELLOW
            } else if self.detect_accepted {
                egui::Color32::LIGHT_GREEN
            } else {
                egui::Color32::LIGHT_RED
            };
            ui.colored_label(status_color, &self.detect_status);

            if let Some(err) = &self.init_error {
                ui.separator();
                ui.colored_label(egui::Color32::RED, err);
                return;
            }

            let (background_texture_id, overlay_texture_id) = {
                let (Some(background_texture), Some(overlay_texture)) =
                    (&self.background_texture, &self.overlay_texture)
                else {
                    ui.separator();
                    ui.label("Textures are not ready.");
                    return;
                };
                (background_texture.id(), overlay_texture.id())
            };

            ui.separator();
            egui::ScrollArea::both().show(ui, |ui| {
                let (bg_rect, _) = ui.allocate_exact_size(self.background_size, Sense::hover());
                ui.painter().image(
                    background_texture_id,
                    bg_rect,
                    Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

                let bg_id = ui.id().with("test_center_find_background_click");
                let bg_response = ui.interact(bg_rect, bg_id, Sense::click());
                if bg_response.clicked_by(egui::PointerButton::Secondary)
                    && let Some(pointer_pos) = bg_response.interact_pointer_pos()
                {
                    let local = pointer_pos - bg_rect.min;
                    let click_x = local.x.clamp(0.0, self.background_size.x - 1.0) as usize;
                    let click_y = local.y.clamp(0.0, self.background_size.y - 1.0) as usize;
                    self.queue_detection_request(click_x, click_y, false);
                }

                if self.show_debug_visuals && self.detect_contour.len() >= 2 {
                    let stroke_color = if self.detect_accepted {
                        egui::Color32::from_rgb(102, 255, 153)
                    } else {
                        egui::Color32::from_rgb(255, 160, 160)
                    };
                    for i in 0..self.detect_contour.len() {
                        let a = self.detect_contour[i];
                        let b = self.detect_contour[(i + 1) % self.detect_contour.len()];
                        let a = bg_rect.min + Vec2::new(a.0, a.1);
                        let b = bg_rect.min + Vec2::new(b.0, b.1);
                        ui.painter()
                            .line_segment([a, b], egui::Stroke::new(1.5, stroke_color));
                    }
                }

                if self.show_debug_visuals
                    && let Some((min_x, min_y, max_x, max_y)) = self.detect_bounds
                {
                    let rect = Rect::from_min_max(
                        bg_rect.min + Vec2::new(min_x as f32, min_y as f32),
                        bg_rect.min + Vec2::new(max_x as f32, max_y as f32),
                    );
                    let stroke_color = if self.detect_accepted {
                        egui::Color32::from_rgba_unmultiplied(140, 255, 140, 120)
                    } else {
                        egui::Color32::from_rgba_unmultiplied(255, 140, 140, 120)
                    };
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(1.0, stroke_color),
                        egui::StrokeKind::Outside,
                    );
                }

                if self.show_debug_visuals
                    && let Some((cx, cy)) = self.detect_center
                {
                    let center = bg_rect.min + Vec2::new(cx, cy);
                    let color = egui::Color32::RED;
                    ui.painter().line_segment(
                        [center + Vec2::new(-8.0, 0.0), center + Vec2::new(8.0, 0.0)],
                        egui::Stroke::new(2.0, color),
                    );
                    ui.painter().line_segment(
                        [center + Vec2::new(0.0, -8.0), center + Vec2::new(0.0, 8.0)],
                        egui::Stroke::new(2.0, color),
                    );
                    ui.painter()
                        .circle_stroke(center, 12.0, egui::Stroke::new(1.5, color));
                }

                let overlay_min = bg_rect.min + self.overlay_position;
                let overlay_rect = Rect::from_min_size(overlay_min, self.overlay_size);
                let drag_id = ui.id().with("test_center_find_drag_overlay");
                let drag_response = ui.interact(overlay_rect, drag_id, Sense::drag());
                if drag_response.dragged() {
                    self.overlay_position += drag_response.drag_delta();
                    ctx.request_repaint();
                }

                ui.painter().image(
                    overlay_texture_id,
                    overlay_rect,
                    Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

                if self.show_debug_visuals
                    && let Some(local_center) = self.overlay_visual_center
                {
                    let visual_center = overlay_min + local_center;
                    let color = egui::Color32::from_rgb(80, 210, 255);
                    ui.painter().line_segment(
                        [
                            visual_center + Vec2::new(-6.0, 0.0),
                            visual_center + Vec2::new(6.0, 0.0),
                        ],
                        egui::Stroke::new(1.5, color),
                    );
                    ui.painter().line_segment(
                        [
                            visual_center + Vec2::new(0.0, -6.0),
                            visual_center + Vec2::new(0.0, 6.0),
                        ],
                        egui::Stroke::new(1.5, color),
                    );
                }
            });
        });
    }
}
