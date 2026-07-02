/*
File: src/tabs/typing/render_next/drawn_lines.rs

Purpose:
Подготовка кастомных line-layout траекторий для typing render_next.

Main responsibilities:
- извлекать растровые линии из layout PNG по фиксированной палитре;
- строить векторные линии из сохранённых точек с простым сглаживанием углов;
- возвращать общий arc-length path, который glyph-renderer может сэмплировать одинаково
  для raster и vector custom layout modes.

Key structures:
- DrawnLinePoint
- DrawnLinePath

Key functions:
- load_raster_line_paths()
- build_vector_line_paths()

Notes:
Модуль не рисует glyph-ы. Он только нормализует разные источники кастомной раскладки
в ordered path representation.
*/

use crate::types::{
    TextDrawnLinesLayoutParams, TextVectorLine, TextVectorLineTextDirection,
    TextVectorLinesLayoutParams, TextVectorPoint,
};
use image::RgbaImage;
use std::collections::HashSet;
use std::path::Path;

type DrawnLinePixel = (i32, i32);
type DrawnLineEdge = (DrawnLinePixel, DrawnLinePixel);

#[derive(Debug, Clone, Copy)]
pub(crate) struct DrawnLinePoint {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) arc_len_px: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct DrawnLinePath {
    pub(crate) points: Vec<DrawnLinePoint>,
    pub(crate) total_len_px: f32,
    pub(crate) direction: TextVectorLineTextDirection,
    pub(crate) honor_text_direction: bool,
}

pub(crate) fn load_raster_line_paths(
    path: &Path,
    params: &TextDrawnLinesLayoutParams,
) -> Result<Vec<Option<DrawnLinePath>>, String> {
    let image = image::open(path)
        .map_err(|err| {
            format!(
                "Не удалось открыть layout-изображение {}: {err}",
                path.display()
            )
        })?
        .to_rgba8();
    let mut out = Vec::with_capacity(DRAWN_LINE_PALETTE.len());
    for color in DRAWN_LINE_PALETTE {
        out.push(trace_raster_line_for_color(
            &image,
            color,
            params.color_tolerance,
            params.continuation_alpha,
            params.start_alpha,
        )?);
    }
    Ok(out)
}

pub(crate) fn build_vector_line_paths(
    params: &TextVectorLinesLayoutParams,
) -> Vec<Option<DrawnLinePath>> {
    params.lines.iter().map(build_vector_line_path).collect()
}

fn build_vector_line_path(line: &TextVectorLine) -> Option<DrawnLinePath> {
    let points = line.points.as_slice();
    if points.len() < 2 {
        return None;
    }
    let smoothed = smooth_vector_points(points, line.corner_smoothing_px);
    let mut path = vector_points_to_path(smoothed.as_slice());
    path.direction = line.text_direction;
    path.honor_text_direction = true;
    if path.total_len_px <= 0.0 {
        None
    } else {
        Some(path)
    }
}

pub fn smooth_vector_points(
    points: &[TextVectorPoint],
    corner_smoothing_px: f32,
) -> Vec<TextVectorPoint> {
    let smoothing = corner_smoothing_px.clamp(0.0, 256.0);
    if smoothing <= 0.0 || points.len() < 3 {
        return points.to_vec();
    }
    let iterations = ((smoothing / 16.0).ceil() as usize).clamp(1, 5);
    let mut out = points.to_vec();
    for _ in 0..iterations {
        out = chaikin_smooth_once(out.as_slice());
    }
    out
}

fn chaikin_smooth_once(points: &[TextVectorPoint]) -> Vec<TextVectorPoint> {
    let Some(first) = points.first().copied() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(points.len().saturating_mul(2));
    out.push(first);
    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        out.push(TextVectorPoint {
            x: a.x.mul_add(0.75, b.x * 0.25),
            y: a.y.mul_add(0.75, b.y * 0.25),
        });
        out.push(TextVectorPoint {
            x: a.x.mul_add(0.25, b.x * 0.75),
            y: a.y.mul_add(0.25, b.y * 0.75),
        });
    }
    if let Some(last) = points.last().copied() {
        out.push(last);
    }
    out
}

fn trace_raster_line_for_color(
    image: &RgbaImage,
    color: [u8; 3],
    tolerance: u8,
    continuation_alpha: u8,
    start_alpha: u8,
) -> Result<Option<DrawnLinePath>, String> {
    let mut pixels = HashSet::<DrawnLinePixel>::new();
    let mut start_pixels = Vec::<DrawnLinePixel>::new();
    for (x, y, pixel) in image.enumerate_pixels() {
        let [r, g, b, a] = pixel.0;
        if a < continuation_alpha || !rgb_matches_with_tolerance([r, g, b], color, tolerance) {
            continue;
        }
        let x = i32::try_from(x).map_err(|_| "layout-изображение слишком широкое".to_string())?;
        let y = i32::try_from(y).map_err(|_| "layout-изображение слишком высокое".to_string())?;
        pixels.insert((x, y));
        if a >= start_alpha {
            start_pixels.push((x, y));
        }
    }
    if pixels.is_empty() {
        return Ok(None);
    }
    if start_pixels.is_empty() {
        return Err(format!(
            "Для цвета #{:02X}{:02X}{:02X} не найдена стартовая точка с alpha >= {}.",
            color[0], color[1], color[2], start_alpha
        ));
    }
    let start = start_pixel_nearest_centroid(start_pixels.as_slice());
    let ordered = trace_ordered_pixels(&pixels, start);
    if ordered.len() < 2 {
        return Err(format!(
            "Линия цвета #{:02X}{:02X}{:02X} слишком короткая.",
            color[0], color[1], color[2]
        ));
    }
    Ok(Some(raster_points_to_path(ordered.as_slice())))
}

fn rgb_matches_with_tolerance(actual: [u8; 3], expected: [u8; 3], tolerance: u8) -> bool {
    actual
        .into_iter()
        .zip(expected)
        .all(|(a, b)| a.abs_diff(b) <= tolerance)
}

fn start_pixel_nearest_centroid(pixels: &[DrawnLinePixel]) -> DrawnLinePixel {
    let (sum_x, sum_y) = pixels.iter().fold((0i64, 0i64), |(sx, sy), (x, y)| {
        (sx + i64::from(*x), sy + i64::from(*y))
    });
    let len = pixels.len().max(1) as f32;
    let center_x = sum_x as f32 / len;
    let center_y = sum_y as f32 / len;
    pixels
        .iter()
        .copied()
        .min_by(|a, b| {
            let da = distance2_to_point(*a, center_x, center_y);
            let db = distance2_to_point(*b, center_x, center_y);
            da.total_cmp(&db)
        })
        .unwrap_or((0, 0))
}

fn distance2_to_point(pixel: DrawnLinePixel, x: f32, y: f32) -> f32 {
    let dx = pixel.0 as f32 - x;
    let dy = pixel.1 as f32 - y;
    dx * dx + dy * dy
}

fn trace_ordered_pixels(
    pixels: &HashSet<DrawnLinePixel>,
    start: DrawnLinePixel,
) -> Vec<DrawnLinePixel> {
    let mut ordered = vec![start];
    let mut visited_edges = HashSet::<DrawnLineEdge>::new();
    let mut current = start;
    let mut prev_dir = (1i32, 0i32);
    let max_steps = pixels.len().saturating_mul(4).max(1);
    for _ in 0..max_steps {
        let Some(next) = choose_next_drawn_line_pixel(pixels, &visited_edges, current, prev_dir)
        else {
            break;
        };
        visited_edges.insert(normalize_edge(current, next));
        prev_dir = (next.0 - current.0, next.1 - current.1);
        current = next;
        ordered.push(current);
    }
    ordered
}

fn choose_next_drawn_line_pixel(
    pixels: &HashSet<DrawnLinePixel>,
    visited_edges: &HashSet<DrawnLineEdge>,
    current: DrawnLinePixel,
    prev_dir: DrawnLinePixel,
) -> Option<DrawnLinePixel> {
    let mut candidates = Vec::<DrawnLinePixel>::new();
    for dy in -1..=1 {
        for dx in -1..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            let candidate = (current.0 + dx, current.1 + dy);
            if pixels.contains(&candidate)
                && !visited_edges.contains(&normalize_edge(current, candidate))
            {
                candidates.push(candidate);
            }
        }
    }
    candidates.into_iter().max_by(|a, b| {
        let score_a = direction_score(current, *a, prev_dir);
        let score_b = direction_score(current, *b, prev_dir);
        score_a.total_cmp(&score_b)
    })
}

fn direction_score(
    current: DrawnLinePixel,
    candidate: DrawnLinePixel,
    prev_dir: DrawnLinePixel,
) -> f32 {
    let dx = (candidate.0 - current.0) as f32;
    let dy = (candidate.1 - current.1) as f32;
    let px = prev_dir.0 as f32;
    let py = prev_dir.1 as f32;
    let len = (dx * dx + dy * dy).sqrt().max(1e-6);
    let prev_len = (px * px + py * py).sqrt().max(1e-6);
    (dx * px + dy * py) / (len * prev_len)
}

fn normalize_edge(a: DrawnLinePixel, b: DrawnLinePixel) -> DrawnLineEdge {
    if a <= b { (a, b) } else { (b, a) }
}

fn raster_points_to_path(points: &[DrawnLinePixel]) -> DrawnLinePath {
    let vector_points: Vec<_> = points
        .iter()
        .map(|(x, y)| TextVectorPoint {
            x: *x as f32,
            y: *y as f32,
        })
        .collect();
    vector_points_to_path(vector_points.as_slice())
}

fn vector_points_to_path(points: &[TextVectorPoint]) -> DrawnLinePath {
    let mut out = Vec::<DrawnLinePoint>::with_capacity(points.len());
    let mut total_len_px = 0.0f32;
    let mut previous: Option<TextVectorPoint> = None;
    for point in points {
        if let Some(prev) = previous {
            let dx = point.x - prev.x;
            let dy = point.y - prev.y;
            total_len_px += (dx * dx + dy * dy).sqrt();
        }
        out.push(DrawnLinePoint {
            x: point.x,
            y: point.y,
            arc_len_px: total_len_px,
        });
        previous = Some(*point);
    }
    DrawnLinePath {
        points: out,
        total_len_px,
        direction: TextVectorLineTextDirection::LeftToRight,
        honor_text_direction: false,
    }
}

const DRAWN_LINE_PALETTE: [[u8; 3]; 12] = [
    [255, 0, 0],
    [255, 128, 0],
    [191, 143, 0],
    [0, 180, 0],
    [0, 180, 160],
    [0, 180, 255],
    [0, 60, 255],
    [128, 0, 255],
    [220, 0, 120],
    [128, 72, 24],
    [128, 128, 128],
    [128, 0, 32],
];
