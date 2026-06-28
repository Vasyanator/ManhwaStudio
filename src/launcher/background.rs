/*
File: src/launcher/background.rs

Purpose:
Background image selection and decoding pipeline for the Rust launcher.

Main responsibilities:
- collect a mixed pool of built-in and project menu images in background threads;
- lazily decode images one by one for the animated background;
- prepare a separate blurred texture for each background image so the blur is applied as its own layer;
- clamp decoded image sizes so oversized sources do not reach the GUI/GPU as giant textures.

Key structures:
- `BackgroundImagePlan`
- `LoadedBackgroundImage`

Key functions:
- `spawn_background_plan()`
- `spawn_background_image_load()`

Notes:
- Project image sampling follows the Python launcher rules:
  sample from chapter image folders `src` and `scr`, skip titles with `no_menu_imgs`,
  keep about 30% of the final pool from built-in `ui_new/menu_imgs`.
*/

use crate::config;
use crate::runtime_log;
use image::ImageReader;
use image::RgbaImage;
use image::imageops::FilterType;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Marker file placed inside a title folder to exclude it from the menu background pool.
pub const NO_MENU_IMGS_MARKER: &str = "no_menu_imgs";

const PROJECT_MENU_IMG_SAMPLE_COUNT: usize = 15;
const PROJECT_MENU_IMG_MIN_TOTAL: usize = 21;
const PROJECT_MENU_IMG_DEFAULT_SHARE: f32 = 0.30;
const MENU_BG_MAX_DECODE_PIXELS: u64 = 1_800_000;
const MENU_BG_MAX_TEXTURE_SIDE: u32 = 4096;
const MENU_BG_MAX_SOURCE_PIXELS: u64 = 180_000_000;
const MENU_BG_DARKEN_NUMERATOR: u16 = 170;
const MENU_BG_DARKEN_DENOMINATOR: u16 = 255;
const MENU_BG_BLUR_SIGMA: f32 = 6.0;
const MENU_BG_BLUR_PADDING_PX: u32 = 48;

#[derive(Debug, Clone)]
pub struct BackgroundImageSource {
    pub path: PathBuf,
    pub source_width: u32,
    pub source_height: u32,
}

#[derive(Debug, Clone)]
pub struct BackgroundImagePlan {
    pub entries: Vec<BackgroundImageSource>,
}

#[derive(Debug)]
pub struct LoadedBackgroundImage {
    pub slot_index: usize,
    pub path: PathBuf,
    pub blur_image: egui::ColorImage,
}

pub fn spawn_background_plan(projects_root: PathBuf) -> Receiver<BackgroundImagePlan> {
    let (tx, rx) = mpsc::channel();
    let spawn_result = thread::Builder::new()
        .name("launcher-bg-plan".to_string())
        .spawn(move || {
            let plan = build_background_image_plan(&projects_root);
            if let Err(err) = tx.send(plan) {
                runtime_log::log_warn(format!("[launcher-bg] failed to send image plan: {}", err));
            }
        });

    if let Err(err) = spawn_result {
        runtime_log::log_error(format!(
            "[launcher-bg] failed to spawn image plan worker: {}",
            err
        ));
    }

    rx
}

pub fn spawn_background_image_load(
    image: BackgroundImageLoadRequest,
) -> Receiver<Option<LoadedBackgroundImage>> {
    let (tx, rx) = mpsc::channel();
    let spawn_result = thread::Builder::new()
        .name("launcher-bg-image".to_string())
        .spawn(move || {
            let result = match load_menu_background_image(&image.path, image.target_width) {
                Ok(Some((blur_image, _render_height, _render_blur_padding))) => {
                    Some(LoadedBackgroundImage {
                        slot_index: image.slot_index,
                        path: image.path,
                        blur_image,
                    })
                }
                Ok(None) => None,
                Err(err) => {
                    runtime_log::log_warn(format!(
                        "[launcher-bg] failed to decode '{}': {}",
                        image.path.display(),
                        err
                    ));
                    None
                }
            };

            if let Err(err) = tx.send(result) {
                runtime_log::log_warn(format!(
                    "[launcher-bg] failed to send decoded image: {}",
                    err
                ));
            }
        });

    if let Err(err) = spawn_result {
        runtime_log::log_error(format!(
            "[launcher-bg] failed to spawn image worker: {}",
            err
        ));
    }

    rx
}

#[derive(Debug, Clone)]
pub struct BackgroundImageLoadRequest {
    pub slot_index: usize,
    pub path: PathBuf,
    pub target_width: u32,
}

fn build_background_image_plan(projects_root: &Path) -> BackgroundImagePlan {
    let mut rng = SimpleRng::seeded();
    let mut project_sample = sample_project_menu_images(projects_root, &mut rng);
    let mut built_in_paths = load_all_images_from(&config::program_dir().join("ui_new/menu_imgs"));
    shuffle(&mut built_in_paths, &mut rng);

    if !project_sample.is_empty() {
        if !built_in_paths.is_empty() {
            let default_count = ((project_sample.len() as f32) * PROJECT_MENU_IMG_DEFAULT_SHARE)
                .round()
                .max(1.0) as usize;
            let default_count = default_count.min(built_in_paths.len());
            let mut mixed_paths = Vec::with_capacity(project_sample.len() + default_count);
            mixed_paths.append(&mut project_sample);
            mixed_paths.extend(built_in_paths.into_iter().take(default_count));
            shuffle(&mut mixed_paths, &mut rng);
            return BackgroundImagePlan {
                entries: collect_background_sources(mixed_paths),
            };
        }

        shuffle(&mut project_sample, &mut rng);
        return BackgroundImagePlan {
            entries: collect_background_sources(project_sample),
        };
    }

    if built_in_paths.is_empty() {
        runtime_log::log_info(
            "[launcher-bg] no menu images found; launcher will use a plain background",
        );
    }

    BackgroundImagePlan {
        entries: collect_background_sources(built_in_paths),
    }
}

fn collect_background_sources(paths: Vec<PathBuf>) -> Vec<BackgroundImageSource> {
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        match image::image_dimensions(&path) {
            Ok((source_width, source_height)) if source_width > 0 && source_height > 0 => {
                entries.push(BackgroundImageSource {
                    path,
                    source_width,
                    source_height,
                });
            }
            Ok(_) => {}
            Err(err) => runtime_log::log_warn(format!(
                "[launcher-bg] failed to read dimensions for '{}': {}",
                path.display(),
                err
            )),
        }
    }
    entries
}

fn load_all_images_from(folder: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(folder) else {
        return files;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || !is_supported_image(&path) {
            continue;
        }
        files.push(path);
    }

    files.sort();
    files
}

fn sample_project_menu_images(projects_root: &Path, rng: &mut SimpleRng) -> Vec<PathBuf> {
    let mut sample = Vec::new();
    let mut total = 0usize;

    let Ok(title_dirs) = std::fs::read_dir(projects_root) else {
        return sample;
    };

    let mut title_dirs = title_dirs
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    title_dirs.sort();

    for title_dir in title_dirs {
        if title_dir.join(NO_MENU_IMGS_MARKER).exists() {
            continue;
        }

        let Ok(chapter_dirs) = std::fs::read_dir(&title_dir) else {
            continue;
        };
        let mut chapter_dirs = chapter_dirs
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        chapter_dirs.sort();

        for chapter_dir in chapter_dirs {
            for image_dir_name in ["src", "scr"] {
                let image_dir = chapter_dir.join(image_dir_name);
                if !image_dir.is_dir() {
                    continue;
                }

                let Ok(entries) = std::fs::read_dir(&image_dir) else {
                    continue;
                };
                let mut entries = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file() && is_supported_image(path))
                    .collect::<Vec<_>>();
                entries.sort();

                for path in entries {
                    total += 1;
                    if sample.len() < PROJECT_MENU_IMG_SAMPLE_COUNT {
                        sample.push(path);
                        continue;
                    }

                    let idx = rng.gen_bounded(total);
                    if idx < PROJECT_MENU_IMG_SAMPLE_COUNT {
                        sample[idx] = path;
                    }
                }
            }
        }
    }

    if total >= PROJECT_MENU_IMG_MIN_TOTAL {
        shuffle(&mut sample, rng);
        return sample;
    }

    Vec::new()
}

fn is_supported_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "bmp"
    )
}

fn load_menu_background_image(
    path: &Path,
    target_width: u32,
) -> Result<Option<(egui::ColorImage, f32, f32)>, String> {
    if target_width == 0 {
        return Ok(None);
    }

    let (source_width, source_height) = image::image_dimensions(path)
        .map_err(|err| format!("failed to read dimensions: {}", err))?;
    if source_width == 0 || source_height == 0 {
        return Ok(None);
    }

    let source_pixels = u64::from(source_width) * u64::from(source_height);
    if source_pixels > MENU_BG_MAX_SOURCE_PIXELS {
        runtime_log::log_warn(format!(
            "[launcher-bg] skipped oversized source '{}': {}x{} ({} px)",
            path.display(),
            source_width,
            source_height,
            source_pixels
        ));
        return Ok(None);
    }

    let Some((decode_width, decode_height, render_height, render_blur_padding)) =
        background_render_metrics(target_width, source_width, source_height)
    else {
        return Ok(None);
    };

    let mut reader = ImageReader::open(path)
        .map_err(|err| format!("failed to open image: {}", err))?
        .with_guessed_format()
        .map_err(|err| format!("failed to guess image format: {}", err))?;
    reader.no_limits();

    let decoded = reader
        .decode()
        .map_err(|err| format!("failed to decode image: {}", err))?;
    let rgba = decoded.to_rgba8();
    let resized = image::imageops::resize(&rgba, decode_width, decode_height, FilterType::Triangle);
    let blur_width = decode_width.saturating_add(MENU_BG_BLUR_PADDING_PX.saturating_mul(2));
    let blur_height = decode_height.saturating_add(MENU_BG_BLUR_PADDING_PX.saturating_mul(2));
    let mut blur_canvas = RgbaImage::new(blur_width, blur_height);
    image::imageops::overlay(
        &mut blur_canvas,
        &resized,
        i64::from(MENU_BG_BLUR_PADDING_PX),
        i64::from(MENU_BG_BLUR_PADDING_PX),
    );
    let mut blur = image::imageops::blur(&blur_canvas, MENU_BG_BLUR_SIGMA);

    for pixel in blur.pixels_mut() {
        pixel.0[0] = darken_channel(pixel.0[0]);
        pixel.0[1] = darken_channel(pixel.0[1]);
        pixel.0[2] = darken_channel(pixel.0[2]);
    }

    Ok(Some((
        egui::ColorImage::from_rgba_unmultiplied(
            [blur_width as usize, blur_height as usize],
            blur.as_raw(),
        ),
        render_height,
        render_blur_padding,
    )))
}

pub fn background_render_metrics(
    target_width: u32,
    source_width: u32,
    source_height: u32,
) -> Option<(u32, u32, f32, f32)> {
    if target_width == 0 || source_width == 0 || source_height == 0 {
        return None;
    }

    let render_height =
        (source_height as f64 / source_width as f64 * target_width as f64).max(1.0) as f32;
    let (decode_width, decode_height) =
        fit_size_to_constraints(target_width, source_width, source_height);
    if decode_width == 0 || decode_height == 0 {
        return None;
    }
    let render_blur_padding =
        (target_width as f32 * MENU_BG_BLUR_PADDING_PX as f32 / decode_width as f32).max(1.0);
    Some((
        decode_width,
        decode_height,
        render_height,
        render_blur_padding,
    ))
}

fn fit_size_to_constraints(target_width: u32, source_width: u32, source_height: u32) -> (u32, u32) {
    let mut width = target_width.min(source_width).min(MENU_BG_MAX_TEXTURE_SIDE);
    if width == 0 {
        return (0, 0);
    }

    let height = (u64::from(source_height) * u64::from(width) / u64::from(source_width)).max(1);
    let mut height = height.min(u64::from(MENU_BG_MAX_TEXTURE_SIDE)) as u32;

    let pixels = u64::from(width) * u64::from(height);
    if pixels > MENU_BG_MAX_DECODE_PIXELS {
        let ratio = (MENU_BG_MAX_DECODE_PIXELS as f64 / pixels as f64).sqrt();
        width = ((f64::from(width) * ratio).floor() as u32).max(1);
        height = ((f64::from(height) * ratio).floor() as u32).max(1);
    }

    (width, height)
}

fn darken_channel(value: u8) -> u8 {
    ((u16::from(value) * MENU_BG_DARKEN_NUMERATOR) / MENU_BG_DARKEN_DENOMINATOR) as u8
}

fn shuffle<T>(slice: &mut [T], rng: &mut SimpleRng) {
    if slice.len() < 2 {
        return;
    }

    let mut index = slice.len() - 1;
    while index > 0 {
        let other = rng.gen_bounded(index + 1);
        slice.swap(index, other);
        index -= 1;
    }
}

struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x1234_5678_9abc_def0);
        Self {
            state: nanos ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn gen_bounded(&mut self, upper_bound: usize) -> usize {
        if upper_bound <= 1 {
            return 0;
        }
        (self.next_u64() % upper_bound as u64) as usize
    }
}
