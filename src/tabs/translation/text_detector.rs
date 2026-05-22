/*
FILE OVERVIEW: src/tabs/translation/text_detector.rs
Background text-detector controller for Translation tab.

Main types:
- `TextDetectorRect`: detected text box in source-image pixels.
- `TextDetectorPageResult`: per-page detection output (boxes + optional mask).
- `TextDetectorAiCtdOptions`: AI CTD runtime options sent to backend.
- `TextDetectorPaddleOcrOptions`: backend Paddle det-only runtime options.
- `TextDetectorSuryaOptions`: backend Surya det-only runtime options.
- `TextDetectorRunMode`: detector mode (`Classic` local, `PaddleOcr` via backend,
  `AiCtd` via backend, `Surya` via backend).
- `TextDetectorControllerEvent`: UI-facing detection progress/results/errors.
- `TranslationTextDetectorController`: command/event bridge + busy-state lifecycle.

Worker flow:
- `worker_loop` receives `WorkerCommand::Detect` and runs `run_detect_batch`.
- `run_detect_batch` processes pages sequentially in background and emits progress.

Backend helpers:
- `detect_page_classic` builds both rects and a real binary `mask_alpha` from accepted
  connected components, so classic mode uses the same editable mask pipeline as AI mode.
- `detect_page_paddle_ocr` calls backend `POST /textdetector/paddle/detect`.
- `detect_page_ai_ctd` calls backend `POST /textdetector/ctd/detect`.
- `detect_page_surya` calls backend `POST /textdetector/surya/detect`.
- `detect_paddle_mask_for_image` / `detect_ai_ctd_mask_for_image` reuse the same backend
  contracts for inline region-image detection in Cleaning tools.
- `post_json` performs HTTP request and gates backend calls with `/health`.
*/

use crate::tabs::translation::backend_health::{ai_backend_addr_text, ensure_ai_backend_healthy};
use crate::{ai_models, config};
use eframe::egui;
use image::imageops::FilterType;
use image::{ColorType, ImageEncoder};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DETECTOR_EVENT_POLL_BUDGET: usize = 128;
const MAX_DETECTOR_DIM: u32 = 1600;
const DETECTOR_BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const DETECTOR_BACKEND_READ_TIMEOUT: Duration = Duration::from_secs(600);
const DETECTOR_BACKEND_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MASK_PIXELS: usize = 100_000_000;

#[derive(Debug, Clone, Copy)]
pub struct TextDetectorRect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl TextDetectorRect {
    pub(crate) fn from_xyxy(x1: f32, y1: f32, x2: f32, y2: f32) -> Option<Self> {
        if !x1.is_finite() || !y1.is_finite() || !x2.is_finite() || !y2.is_finite() {
            return None;
        }
        if x2 <= x1 || y2 <= y1 {
            return None;
        }
        Some(Self { x1, y1, x2, y2 })
    }
}

#[derive(Debug, Clone)]
pub struct TextDetectorPageResult {
    pub source_size: [u32; 2],
    pub blocks: Vec<TextDetectorRect>,
    pub mask_size: [u32; 2],
    pub mask_alpha: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TextDetectorAiCtdOptions {
    pub detect_size: i32,
    pub det_rearrange_max_batches: i32,
    pub font_size_multiplier: f32,
    pub font_size_max: f32,
    pub font_size_min: f32,
    pub mask_dilate_size: i32,
}

impl Default for TextDetectorAiCtdOptions {
    fn default() -> Self {
        Self {
            detect_size: 1280,
            det_rearrange_max_batches: 4,
            font_size_multiplier: 1.0,
            font_size_max: -1.0,
            font_size_min: -1.0,
            mask_dilate_size: 2,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TextDetectorPaddleOcrOptions {
    pub mask_dilate_size: i32,
}

#[derive(Debug, Clone, Default)]
pub struct TextDetectorSuryaOptions;

#[derive(Debug, Clone)]
pub enum TextDetectorRunMode {
    Classic,
    PaddleOcr(TextDetectorPaddleOcrOptions),
    AiCtd(TextDetectorAiCtdOptions),
    Surya(TextDetectorSuryaOptions),
}

#[derive(Debug, Clone)]
pub enum TextDetectorControllerEvent {
    ModelDownloadStarted,
    DetectStarted {
        total: usize,
        replace: bool,
    },
    PageDetected {
        page_idx: usize,
        result: TextDetectorPageResult,
    },
    PageFailed {
        page_idx: usize,
        error: String,
    },
    DetectProgress {
        done: usize,
        total: usize,
    },
    DetectFinished {
        total_blocks: usize,
        failed_pages: usize,
    },
    DetectFailed {
        error: String,
    },
}

#[derive(Debug)]
pub struct TranslationTextDetectorController {
    busy: bool,
    cmd_tx: Sender<WorkerCommand>,
    evt_rx: Receiver<WorkerEvent>,
    worker_thread: Option<JoinHandle<()>>,
}

impl Default for TranslationTextDetectorController {
    fn default() -> Self {
        Self::new()
    }
}

impl TranslationTextDetectorController {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
        let worker_thread = thread::spawn(move || worker_loop(cmd_rx, evt_tx));
        Self {
            busy: false,
            cmd_tx,
            evt_rx,
            worker_thread: Some(worker_thread),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn start_detection(
        &mut self,
        pages: Vec<(usize, PathBuf)>,
        replace: bool,
        mode: TextDetectorRunMode,
        mask_dilate_size: i32,
    ) -> Result<(), String> {
        if self.busy {
            return Err("Детектор уже выполняется.".to_string());
        }
        if pages.is_empty() {
            return Err("Нет страниц для детекции.".to_string());
        }
        self.busy = true;
        if self
            .cmd_tx
            .send(WorkerCommand::Detect {
                pages,
                replace,
                mode,
                mask_dilate_size: mask_dilate_size.clamp(0, 30),
            })
            .is_err()
        {
            self.busy = false;
            return Err("Worker детектора недоступен.".to_string());
        }
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<TextDetectorControllerEvent> {
        let mut out = Vec::new();
        for _ in 0..DETECTOR_EVENT_POLL_BUDGET {
            match self.evt_rx.try_recv() {
                Ok(WorkerEvent::ModelDownloadStarted) => {
                    out.push(TextDetectorControllerEvent::ModelDownloadStarted);
                }
                Ok(WorkerEvent::DetectStarted { total, replace }) => {
                    out.push(TextDetectorControllerEvent::DetectStarted { total, replace });
                }
                Ok(WorkerEvent::PageDetected { page_idx, result }) => {
                    out.push(TextDetectorControllerEvent::PageDetected { page_idx, result });
                }
                Ok(WorkerEvent::PageFailed { page_idx, error }) => {
                    out.push(TextDetectorControllerEvent::PageFailed { page_idx, error });
                }
                Ok(WorkerEvent::DetectProgress { done, total }) => {
                    out.push(TextDetectorControllerEvent::DetectProgress { done, total });
                }
                Ok(WorkerEvent::DetectFailed { error }) => {
                    self.busy = false;
                    out.push(TextDetectorControllerEvent::DetectFailed { error });
                }
                Ok(WorkerEvent::DetectFinished {
                    total_blocks,
                    failed_pages,
                }) => {
                    self.busy = false;
                    out.push(TextDetectorControllerEvent::DetectFinished {
                        total_blocks,
                        failed_pages,
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.busy = false;
                    out.push(TextDetectorControllerEvent::DetectFailed {
                        error: "Worker детектора отключился.".to_string(),
                    });
                    break;
                }
            }
        }
        out
    }
}

impl Drop for TranslationTextDetectorController {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCommand::Stop);
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug)]
enum WorkerCommand {
    Detect {
        pages: Vec<(usize, PathBuf)>,
        replace: bool,
        mode: TextDetectorRunMode,
        mask_dilate_size: i32,
    },
    Stop,
}

#[derive(Debug)]
enum WorkerEvent {
    ModelDownloadStarted,
    DetectStarted {
        total: usize,
        replace: bool,
    },
    PageDetected {
        page_idx: usize,
        result: TextDetectorPageResult,
    },
    PageFailed {
        page_idx: usize,
        error: String,
    },
    DetectProgress {
        done: usize,
        total: usize,
    },
    DetectFailed {
        error: String,
    },
    DetectFinished {
        total_blocks: usize,
        failed_pages: usize,
    },
}

fn worker_loop(cmd_rx: Receiver<WorkerCommand>, evt_tx: Sender<WorkerEvent>) {
    while let Ok(command) = cmd_rx.recv() {
        match command {
            WorkerCommand::Stop => break,
            WorkerCommand::Detect {
                pages,
                replace,
                mode,
                mask_dilate_size,
            } => {
                run_detect_batch(pages, replace, mode, mask_dilate_size, &evt_tx);
            }
        }
    }
}

fn run_detect_batch(
    pages: Vec<(usize, PathBuf)>,
    replace: bool,
    mode: TextDetectorRunMode,
    mask_dilate_size: i32,
    evt_tx: &Sender<WorkerEvent>,
) {
    let total = pages.len();
    let _ = evt_tx.send(WorkerEvent::DetectStarted { total, replace });
    if matches!(
        &mode,
        TextDetectorRunMode::AiCtd(_)
            | TextDetectorRunMode::PaddleOcr(_)
            | TextDetectorRunMode::Surya(_)
    ) && let Err(error) = ensure_ai_backend_healthy()
    {
        let _ = evt_tx.send(WorkerEvent::DetectFailed { error });
        return;
    }
    if let Err(error) = ensure_detector_models_for_mode(&mode, evt_tx) {
        let _ = evt_tx.send(WorkerEvent::DetectFailed { error });
        return;
    }
    let mut total_blocks = 0usize;
    let mut failed_pages = 0usize;

    for (done, (page_idx, path)) in pages.into_iter().enumerate() {
        let detect_result = match &mode {
            TextDetectorRunMode::Classic => detect_page_classic(&path),
            TextDetectorRunMode::PaddleOcr(options) => detect_page_paddle_ocr(&path, options),
            TextDetectorRunMode::AiCtd(options) => detect_page_ai_ctd(&path, options),
            TextDetectorRunMode::Surya(options) => detect_page_surya(&path, options),
        };
        match detect_result {
            Ok(mut result) => {
                apply_mask_dilation(&mut result, mask_dilate_size);
                total_blocks += result.blocks.len();
                let _ = evt_tx.send(WorkerEvent::PageDetected { page_idx, result });
            }
            Err(error) => {
                failed_pages += 1;
                let _ = evt_tx.send(WorkerEvent::PageFailed { page_idx, error });
            }
        }
        let done = done + 1;
        let _ = evt_tx.send(WorkerEvent::DetectProgress { done, total });
    }

    let _ = evt_tx.send(WorkerEvent::DetectFinished {
        total_blocks,
        failed_pages,
    });
}

fn ensure_detector_models_for_mode(
    mode: &TextDetectorRunMode,
    evt_tx: &Sender<WorkerEvent>,
) -> Result<(), String> {
    let models_root = config::models_dir();
    let mut reported = false;
    let mut report_download = || {
        if !reported {
            let _ = evt_tx.send(WorkerEvent::ModelDownloadStarted);
            reported = true;
        }
    };
    match mode {
        TextDetectorRunMode::Classic | TextDetectorRunMode::Surya(_) => Ok(()),
        TextDetectorRunMode::PaddleOcr(_) => {
            ai_models::ensure_paddle_ocr_detector_with_reporter(
                &models_root,
                Some(&mut report_download),
            )?;
            Ok(())
        }
        TextDetectorRunMode::AiCtd(_) => {
            ai_models::ensure_comic_text_detector_torch_with_reporter(
                &models_root,
                Some(&mut report_download),
            )?;
            Ok(())
        }
    }
}

fn detect_page_classic(path: &PathBuf) -> Result<TextDetectorPageResult, String> {
    let img = image::open(path).map_err(|err| {
        format!(
            "Не удалось открыть изображение для детектора ({}): {err}",
            path.display()
        )
    })?;
    let gray = img.to_luma8();
    let source_w = gray.width();
    let source_h = gray.height();
    if source_w == 0 || source_h == 0 {
        return Err(format!(
            "Пустое изображение для детектора: {}",
            path.display()
        ));
    }

    let (proc, scale_x, scale_y) = if source_w.max(source_h) > MAX_DETECTOR_DIM {
        let scale = MAX_DETECTOR_DIM as f32 / source_w.max(source_h) as f32;
        let dst_w = ((source_w as f32 * scale).round() as u32).max(1);
        let dst_h = ((source_h as f32 * scale).round() as u32).max(1);
        let resized = image::imageops::resize(&gray, dst_w, dst_h, FilterType::Triangle);
        let sx = source_w as f32 / dst_w as f32;
        let sy = source_h as f32 / dst_h as f32;
        (resized, sx, sy)
    } else {
        (gray, 1.0, 1.0)
    };

    let proc_w = proc.width() as usize;
    let proc_h = proc.height() as usize;
    let threshold = otsu_threshold(proc.as_raw()).clamp(45, 210);
    let mut fg = vec![0u8; proc_w * proc_h];
    for (idx, px) in proc.as_raw().iter().enumerate() {
        if *px < threshold {
            fg[idx] = 1;
        }
    }

    // Небольшая дилатация склеивает символы в блоки без тяжёлых зависимостей.
    let fg = dilate_binary(&fg, proc_w, proc_h, 2, 1);
    let (blocks, proc_mask_alpha) =
        extract_components_as_rects_and_mask(&fg, proc_w, proc_h, scale_x, scale_y);
    let (mask_size, mask_alpha) = promote_classic_mask_to_source_size(
        proc_mask_alpha,
        [proc.width(), proc.height()],
        [source_w, source_h],
    );

    Ok(TextDetectorPageResult {
        source_size: [source_w, source_h],
        blocks,
        mask_size,
        mask_alpha,
    })
}

fn detect_page_paddle_ocr(
    path: &Path,
    _options: &TextDetectorPaddleOcrOptions,
) -> Result<TextDetectorPageResult, String> {
    ai_models::ensure_paddle_ocr_detector(&config::models_dir())?;
    let payload = json!({
        "page_path": path.to_string_lossy().to_string(),
    });
    let response = post_json("/textdetector/paddle/detect", &payload.to_string())?;
    parse_backend_text_detector_response(&response, path, "Paddle")
}

fn detect_page_ai_ctd(
    path: &Path,
    options: &TextDetectorAiCtdOptions,
) -> Result<TextDetectorPageResult, String> {
    ai_models::ensure_comic_text_detector_torch(&config::models_dir())?;
    let payload = json!({
        "page_path": path.to_string_lossy().to_string(),
        "params": {
            "detect_size": options.detect_size.clamp(896, 2048),
            "det_rearrange_max_batches": options.det_rearrange_max_batches.clamp(1, 64),
            "font size multiplier": options.font_size_multiplier.clamp(0.1, 8.0),
            "font size max": options.font_size_max.clamp(-1.0, 500.0),
            "font size min": options.font_size_min.clamp(-1.0, 500.0)
        }
    });

    let response = post_json("/textdetector/ctd/detect", &payload.to_string())?;
    parse_backend_text_detector_response(&response, path, "CTD")
}

fn detect_page_surya(
    path: &Path,
    _options: &TextDetectorSuryaOptions,
) -> Result<TextDetectorPageResult, String> {
    let payload = json!({
        "page_path": path.to_string_lossy().to_string(),
    });
    let response = post_json("/textdetector/surya/detect", &payload.to_string())?;
    parse_backend_text_detector_response(&response, path, "Surya")
}

pub(crate) fn detect_ai_ctd_mask_for_image(
    image: &egui::ColorImage,
    options: &TextDetectorAiCtdOptions,
) -> Result<([u32; 2], Vec<u8>), String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Ok(([0, 0], Vec::new()));
    }

    ai_models::ensure_comic_text_detector_torch(&config::models_dir())?;
    let image_png = encode_color_image_png_rgba(image)?;
    let payload = json!({
        "image_base64": base64_encode(&image_png),
        "params": {
            "detect_size": options.detect_size.clamp(896, 2048),
            "det_rearrange_max_batches": options.det_rearrange_max_batches.clamp(1, 64),
            "font size multiplier": options.font_size_multiplier.clamp(0.1, 8.0),
            "font size max": options.font_size_max.clamp(-1.0, 500.0),
            "font size min": options.font_size_min.clamp(-1.0, 500.0),
            "mask dilate size": options.mask_dilate_size.clamp(0, 30)
        }
    });
    let response = match post_json("/textdetector/ctd/detect", &payload.to_string()) {
        Ok(response) => response,
        Err(err) if should_retry_ctd_via_temp_file(&err) => {
            post_ai_ctd_mask_via_temp_file(&image_png, options)?
        }
        Err(err) => return Err(err),
    };
    parse_ai_mask_alpha(&response)
}

pub(crate) fn detect_paddle_mask_for_image(
    image: &egui::ColorImage,
    options: &TextDetectorPaddleOcrOptions,
) -> Result<([u32; 2], Vec<u8>), String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Ok(([0, 0], Vec::new()));
    }

    ai_models::ensure_paddle_ocr_detector(&config::models_dir())?;
    let image_png = encode_color_image_png_rgba(image)?;
    let payload = json!({
        "image_base64": base64_encode(&image_png),
    });
    let response = post_json("/textdetector/paddle/detect", &payload.to_string())?;
    let (mask_size, mut mask_alpha) = parse_ai_mask_alpha(&response)?;
    dilate_mask_alpha(&mut mask_alpha, mask_size, options.mask_dilate_size);
    Ok((mask_size, mask_alpha))
}

pub(crate) fn detect_surya_mask_for_image(
    image: &egui::ColorImage,
    dilate_size: i32,
) -> Result<([u32; 2], Vec<u8>), String> {
    if image.size[0] == 0 || image.size[1] == 0 {
        return Ok(([0, 0], Vec::new()));
    }

    let image_png = encode_color_image_png_rgba(image)?;
    let payload = json!({
        "image_base64": base64_encode(&image_png),
    });
    let response = post_json("/textdetector/surya/detect", &payload.to_string())?;
    let (mask_size, mut mask_alpha) = parse_ai_mask_alpha(&response)?;
    dilate_mask_alpha(&mut mask_alpha, mask_size, dilate_size);
    Ok((mask_size, mask_alpha))
}

fn parse_backend_text_detector_response(
    response: &Value,
    path: &Path,
    engine_name: &str,
) -> Result<TextDetectorPageResult, String> {
    if !response.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let msg = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Неизвестная ошибка backend детектора текста.");
        return Err(msg.to_string());
    }

    let source_size = response
        .get("source_size")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "{engine_name} backend вернул некорректный source_size для {}",
                path.display()
            )
        })?;
    if source_size.len() < 2 {
        return Err(format!(
            "{engine_name} backend вернул неполный source_size для {}",
            path.display()
        ));
    }
    let source_w = source_size[0]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            format!(
                "{engine_name} backend: невалидная ширина source_size ({})",
                path.display()
            )
        })?;
    let source_h = source_size[1]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            format!(
                "{engine_name} backend: невалидная высота source_size ({})",
                path.display()
            )
        })?;
    if source_w == 0 || source_h == 0 {
        return Err(format!(
            "{engine_name} backend вернул пустой размер страницы ({})",
            path.display()
        ));
    }

    let mut blocks = parse_backend_blocks(response);

    blocks.sort_by(|a, b| {
        a.y1.total_cmp(&b.y1)
            .then_with(|| a.x1.total_cmp(&b.x1))
            .then_with(|| a.y2.total_cmp(&b.y2))
            .then_with(|| a.x2.total_cmp(&b.x2))
    });
    if blocks.len() > 2500 {
        blocks.truncate(2500);
    }

    let (mask_size, mask_alpha) =
        parse_ai_mask_alpha(response).map_err(|err| format!("{engine_name} backend: {err}"))?;

    Ok(TextDetectorPageResult {
        source_size: [source_w, source_h],
        blocks,
        mask_size,
        mask_alpha,
    })
}

fn parse_backend_blocks(response: &Value) -> Vec<TextDetectorRect> {
    let blocks = response
        .get("blocks")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(parse_backend_xyxy_rect)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !blocks.is_empty() {
        return blocks;
    }

    response
        .get("lines")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(parse_backend_line_rect)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn parse_backend_xyxy_rect(item: &Value) -> Option<TextDetectorRect> {
    let x1 = item.get("x1").and_then(Value::as_f64)?;
    let y1 = item.get("y1").and_then(Value::as_f64)?;
    let x2 = item.get("x2").and_then(Value::as_f64)?;
    let y2 = item.get("y2").and_then(Value::as_f64)?;
    TextDetectorRect::from_xyxy(x1 as f32, y1 as f32, x2 as f32, y2 as f32)
}

fn parse_backend_line_rect(item: &Value) -> Option<TextDetectorRect> {
    if let Some(bbox) = item.get("bbox").and_then(Value::as_array)
        && bbox.len() >= 4
    {
        let x1 = bbox[0].as_f64()?;
        let y1 = bbox[1].as_f64()?;
        let x2 = bbox[2].as_f64()?;
        let y2 = bbox[3].as_f64()?;
        return TextDetectorRect::from_xyxy(x1 as f32, y1 as f32, x2 as f32, y2 as f32);
    }

    let polygon = item.get("polygon").and_then(Value::as_array)?;
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    for point in polygon {
        let point = point.as_array()?;
        if point.len() < 2 {
            continue;
        }
        let x = point[0].as_f64()? as f32;
        let y = point[1].as_f64()? as f32;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }

    if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
        return None;
    }
    TextDetectorRect::from_xyxy(min_x, min_y, max_x, max_y)
}

fn parse_ai_mask_alpha(response: &Value) -> Result<([u32; 2], Vec<u8>), String> {
    let Some(raw_b64) = response.get("mask_png_base64").and_then(Value::as_str) else {
        return Err("Backend детектора текста: не найдено поле mask_png_base64".to_string());
    };
    let trimmed = raw_b64.trim();
    if trimmed.is_empty() {
        return Ok(([0, 0], Vec::new()));
    }
    let bytes = decode_base64_ascii(trimmed)?;
    let image = image::load_from_memory(&bytes)
        .map_err(|err| {
            format!("Backend детектора текста: не удалось декодировать PNG маски: {err}")
        })?
        .to_luma8();
    let w = image.width();
    let h = image.height();
    if w == 0 || h == 0 {
        return Ok(([0, 0], Vec::new()));
    }
    let pixels = (w as usize).saturating_mul(h as usize);
    if pixels == 0 || pixels > MAX_MASK_PIXELS {
        return Err(format!(
            "Backend детектора текста: размер маски слишком большой ({w}x{h})"
        ));
    }
    // CTD mask is logically binary; normalize to 0/255 to keep rendering consistent
    // for both 1-bit and 8-bit encodings.
    let mut alpha = image.into_raw();
    for px in &mut alpha {
        *px = if *px == 0 { 0 } else { 255 };
    }
    Ok(([w, h], alpha))
}

fn apply_mask_dilation(result: &mut TextDetectorPageResult, dilate_size: i32) {
    dilate_mask_alpha(&mut result.mask_alpha, result.mask_size, dilate_size);
}

fn dilate_mask_alpha(mask_alpha: &mut Vec<u8>, mask_size: [u32; 2], dilate_size: i32) {
    let radius = usize::try_from(dilate_size.clamp(0, 30)).unwrap_or(0);
    if radius == 0 || mask_alpha.is_empty() || mask_size[0] == 0 || mask_size[1] == 0 {
        return;
    }

    let width = match usize::try_from(mask_size[0]) {
        Ok(value) => value,
        Err(_) => return,
    };
    let height = match usize::try_from(mask_size[1]) {
        Ok(value) => value,
        Err(_) => return,
    };
    if width == 0 || height == 0 || width.saturating_mul(height) != mask_alpha.len() {
        return;
    }

    let src = mask_alpha
        .iter()
        .map(|&px| if px == 0 { 0 } else { 1 })
        .collect::<Vec<_>>();
    let dilated = dilate_binary(&src, width, height, radius, radius);
    *mask_alpha = dilated
        .into_iter()
        .map(|px| if px == 0 { 0 } else { 255 })
        .collect();
}

fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let width = image.size[0];
    let height = image.size[1];
    let raw = image
        .pixels
        .iter()
        .flat_map(|px| px.to_array())
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            &raw,
            u32::try_from(width).map_err(|_| "Ширина изображения слишком большая.".to_string())?,
            u32::try_from(height).map_err(|_| "Высота изображения слишком большая.".to_string())?,
            ColorType::Rgba8.into(),
        )
        .map_err(|err| format!("Не удалось закодировать PNG изображения: {err}"))?;
    Ok(out)
}

fn base64_encode(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut idx = 0usize;
    while idx + 3 <= bytes.len() {
        let a = bytes[idx];
        let b = bytes[idx + 1];
        let c = bytes[idx + 2];
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 0b0000_0011) << 4) | (b >> 4)) as usize] as char);
        out.push(TABLE[(((b & 0b0000_1111) << 2) | (c >> 6)) as usize] as char);
        out.push(TABLE[(c & 0b0011_1111) as usize] as char);
        idx += 3;
    }
    match bytes.len().saturating_sub(idx) {
        1 => {
            let a = bytes[idx];
            out.push(TABLE[(a >> 2) as usize] as char);
            out.push(TABLE[((a & 0b0000_0011) << 4) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let a = bytes[idx];
            let b = bytes[idx + 1];
            out.push(TABLE[(a >> 2) as usize] as char);
            out.push(TABLE[(((a & 0b0000_0011) << 4) | (b >> 4)) as usize] as char);
            out.push(TABLE[((b & 0b0000_1111) << 2) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

fn should_retry_ctd_via_temp_file(error: &str) -> bool {
    error.contains("Field 'page_path' must be a non-empty string.")
}

fn post_ai_ctd_mask_via_temp_file(
    image_png: &[u8],
    options: &TextDetectorAiCtdOptions,
) -> Result<Value, String> {
    let temp_path = build_ctd_temp_image_path();
    fs::write(&temp_path, image_png).map_err(|err| {
        format!(
            "Не удалось сохранить временное изображение для CTD fallback ({}): {err}",
            temp_path.display()
        )
    })?;

    let payload = json!({
        "page_path": temp_path.to_string_lossy().to_string(),
        "params": {
            "detect_size": options.detect_size.clamp(896, 2048),
            "det_rearrange_max_batches": options.det_rearrange_max_batches.clamp(1, 64),
            "font size multiplier": options.font_size_multiplier.clamp(0.1, 8.0),
            "font size max": options.font_size_max.clamp(-1.0, 500.0),
            "font size min": options.font_size_min.clamp(-1.0, 500.0)
        }
    });
    let result = post_json("/textdetector/ctd/detect", &payload.to_string());
    if let Err(err) = fs::remove_file(&temp_path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "WARN translation::text_detector failed to remove temp CTD file {}: {}",
            temp_path.display(),
            err
        );
    }
    result
}

fn build_ctd_temp_image_path() -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("mangafucker_ctd_region_{pid}_{millis}.png"))
}

fn decode_base64_ascii(input: &str) -> Result<Vec<u8>, String> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut cleaned = Vec::<u8>::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        cleaned.push(b);
    }
    if !cleaned.len().is_multiple_of(4) {
        return Err("Невалидная base64-строка (длина не кратна 4).".to_string());
    }
    let mut out = Vec::<u8>::with_capacity(cleaned.len() / 4 * 3);
    let mut i = 0usize;
    while i < cleaned.len() {
        let c0 = cleaned[i];
        let c1 = cleaned[i + 1];
        let c2 = cleaned[i + 2];
        let c3 = cleaned[i + 3];
        let v0 = base64_val(c0).ok_or_else(|| "Невалидный символ base64.".to_string())?;
        let v1 = base64_val(c1).ok_or_else(|| "Невалидный символ base64.".to_string())?;
        let pad2 = c2 == b'=';
        let pad3 = c3 == b'=';
        let v2 = if pad2 {
            0
        } else {
            base64_val(c2).ok_or_else(|| "Невалидный символ base64.".to_string())?
        };
        let v3 = if pad3 {
            0
        } else {
            base64_val(c3).ok_or_else(|| "Невалидный символ base64.".to_string())?
        };
        let n = ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6) | (v3 as u32);
        out.push(((n >> 16) & 0xff) as u8);
        if !pad2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if !pad3 {
            out.push((n & 0xff) as u8);
        }
        i += 4;
    }
    Ok(out)
}

fn base64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn post_json(path: &str, payload: &str) -> Result<Value, String> {
    ensure_ai_backend_healthy()?;

    let addr = ai_backend_addr_text();
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|err| format!("Ошибка резолва AI backend {addr}: {err}"))?
        .next()
        .ok_or_else(|| format!("Не найден сокет AI backend: {addr}"))?;

    let mut stream = TcpStream::connect_timeout(&socket_addr, DETECTOR_BACKEND_CONNECT_TIMEOUT)
        .map_err(|err| format!("Не удалось подключиться к AI backend: {err}"))?;
    stream
        .set_read_timeout(Some(DETECTOR_BACKEND_READ_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить read timeout AI backend: {err}"))?;
    stream
        .set_write_timeout(Some(DETECTOR_BACKEND_WRITE_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить write timeout AI backend: {err}"))?;

    let req_head = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        ai_backend_addr_text(),
        payload.len()
    );
    stream
        .write_all(req_head.as_bytes())
        .map_err(|err| format!("Не удалось отправить HTTP-заголовки AI backend: {err}"))?;
    stream
        .write_all(payload.as_bytes())
        .map_err(|err| format!("Не удалось отправить HTTP-body AI backend: {err}"))?;

    let (status_code, body) = read_http_response(&mut stream)?;
    let body_text = String::from_utf8_lossy(&body).to_string();
    let json_value: Value = serde_json::from_str(&body_text)
        .map_err(|err| format!("AI backend вернул не-JSON ({status_code}): {err}"))?;

    if status_code >= 400 {
        let msg = json_value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Ошибка AI backend.");
        return Err(format!("AI backend HTTP {status_code}: {msg}"));
    }

    Ok(json_value)
}

fn read_http_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), String> {
    let mut raw = Vec::with_capacity(8 * 1024);
    let mut scratch = [0u8; 4096];
    let mut header_end: Option<usize> = None;

    while header_end.is_none() {
        let n = stream
            .read(&mut scratch)
            .map_err(|err| format!("Не удалось прочитать ответ AI backend: {err}"))?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&scratch[..n]);
        header_end = raw.windows(4).position(|chunk| chunk == b"\r\n\r\n");
    }

    if raw.is_empty() {
        return Err("AI backend вернул пустой HTTP-ответ.".to_string());
    }

    let Some(header_end) = header_end else {
        return Err("Некорректный HTTP-ответ AI backend (нет заголовков).".to_string());
    };
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let status_line = head.lines().next().unwrap_or("HTTP/1.1 500");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|part| part.parse::<u16>().ok())
        .ok_or_else(|| format!("Не удалось распарсить HTTP-статус AI backend: {status_line}"))?;

    let content_length = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("content-length") {
            return None;
        }
        value.trim().parse::<usize>().ok()
    });
    let Some(content_length) = content_length else {
        return Err("AI backend не прислал Content-Length, ответ не может быть завершён без закрытия сокета.".to_string());
    };

    let body_start = header_end + 4;
    let mut body = raw[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut scratch)
            .map_err(|err| format!("Не удалось дочитать body AI backend: {err}"))?;
        if n == 0 {
            return Err(format!(
                "AI backend закрыл соединение до полного body (получено {} из {} байт).",
                body.len(),
                content_length
            ));
        }
        body.extend_from_slice(&scratch[..n]);
    }
    body.truncate(content_length);
    Ok((status_code, body))
}

fn otsu_threshold(gray: &[u8]) -> u8 {
    if gray.is_empty() {
        return 127;
    }
    let mut hist = [0u32; 256];
    for &v in gray {
        hist[v as usize] += 1;
    }
    let total = gray.len() as f64;
    let sum_total = hist
        .iter()
        .enumerate()
        .map(|(i, c)| (i as f64) * (*c as f64))
        .sum::<f64>();

    let mut sum_b = 0.0f64;
    let mut w_b = 0.0f64;
    let mut max_var = -1.0f64;
    let mut threshold = 127u8;

    for (t, &count) in hist.iter().enumerate() {
        w_b += count as f64;
        if w_b <= f64::EPSILON {
            continue;
        }
        let w_f = total - w_b;
        if w_f <= f64::EPSILON {
            break;
        }
        sum_b += (t as f64) * (count as f64);
        let m_b = sum_b / w_b;
        let m_f = (sum_total - sum_b) / w_f;
        let between = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if between > max_var {
            max_var = between;
            threshold = t as u8;
        }
    }
    threshold
}

fn dilate_binary(src: &[u8], width: usize, height: usize, rx: usize, ry: usize) -> Vec<u8> {
    if src.is_empty() || width == 0 || height == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; src.len()];
    for y in 0..height {
        let y0 = y.saturating_sub(ry);
        let y1 = (y + ry).min(height - 1);
        for x in 0..width {
            let x0 = x.saturating_sub(rx);
            let x1 = (x + rx).min(width - 1);
            let mut any = 0u8;
            'scan: for yy in y0..=y1 {
                let row = yy * width;
                for xx in x0..=x1 {
                    if src[row + xx] != 0 {
                        any = 1;
                        break 'scan;
                    }
                }
            }
            out[y * width + x] = any;
        }
    }
    out
}

fn promote_classic_mask_to_source_size(
    mask_alpha: Vec<u8>,
    mask_size: [u32; 2],
    source_size: [u32; 2],
) -> ([u32; 2], Vec<u8>) {
    if mask_alpha.is_empty() || mask_size[0] == 0 || mask_size[1] == 0 || mask_size == source_size {
        return (mask_size, mask_alpha);
    }
    let source_pixels = (source_size[0] as usize).saturating_mul(source_size[1] as usize);
    if source_pixels == 0 || source_pixels > MAX_MASK_PIXELS {
        return (mask_size, mask_alpha);
    }
    let Some(mask_img) = image::GrayImage::from_vec(mask_size[0], mask_size[1], mask_alpha) else {
        return (mask_size, Vec::new());
    };
    let resized = image::imageops::resize(
        &mask_img,
        source_size[0],
        source_size[1],
        FilterType::Nearest,
    );
    let mut alpha = resized.into_raw();
    for px in &mut alpha {
        *px = if *px == 0 { 0 } else { 255 };
    }
    (source_size, alpha)
}

fn extract_components_as_rects_and_mask(
    fg: &[u8],
    width: usize,
    height: usize,
    scale_x: f32,
    scale_y: f32,
) -> (Vec<TextDetectorRect>, Vec<u8>) {
    if fg.is_empty() || width == 0 || height == 0 {
        return (Vec::new(), Vec::new());
    }
    let mut visited = vec![0u8; fg.len()];
    let mut stack = Vec::<usize>::new();
    let mut component_pixels = Vec::<usize>::new();
    let mut rects = Vec::<TextDetectorRect>::new();
    let mut mask_alpha = vec![0u8; fg.len()];

    let img_area = width * height;
    let min_side = (width.min(height) / 240).max(3) as u32;
    let min_area = (img_area / 25_000).max(24) as u32;
    let max_area = ((img_area / 7).max(min_area as usize + 1)) as u32;
    let max_w = (width as f32 * 0.85) as u32;
    let max_h = (height as f32 * 0.40) as u32;

    for seed in 0..fg.len() {
        if fg[seed] == 0 || visited[seed] != 0 {
            continue;
        }
        visited[seed] = 1;
        stack.clear();
        stack.push(seed);
        component_pixels.clear();

        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut area = 0u32;

        while let Some(idx) = stack.pop() {
            component_pixels.push(idx);
            let x = (idx % width) as u32;
            let y = (idx / width) as u32;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            area = area.saturating_add(1);

            let y0 = y.saturating_sub(1) as usize;
            let y1 = (y as usize + 1).min(height - 1);
            let x0 = x.saturating_sub(1) as usize;
            let x1 = (x as usize + 1).min(width - 1);

            for ny in y0..=y1 {
                let row = ny * width;
                for nx in x0..=x1 {
                    let nidx = row + nx;
                    if fg[nidx] == 0 || visited[nidx] != 0 {
                        continue;
                    }
                    visited[nidx] = 1;
                    stack.push(nidx);
                }
            }
        }

        if area < min_area || area > max_area {
            continue;
        }
        let bw = max_x.saturating_sub(min_x) + 1;
        let bh = max_y.saturating_sub(min_y) + 1;
        if bw < min_side || bh < min_side {
            continue;
        }
        if bw > max_w || bh > max_h {
            continue;
        }

        let bbox_area = bw.saturating_mul(bh).max(1);
        let density = area as f32 / bbox_area as f32;
        if !(0.06..=0.95).contains(&density) {
            continue;
        }
        let aspect = bw as f32 / bh as f32;
        if !(0.08..=20.0).contains(&aspect) {
            continue;
        }

        let x1 = min_x as f32 * scale_x;
        let y1 = min_y as f32 * scale_y;
        let x2 = (max_x + 1) as f32 * scale_x;
        let y2 = (max_y + 1) as f32 * scale_y;
        if let Some(rect) = TextDetectorRect::from_xyxy(x1, y1, x2, y2) {
            rects.push(rect);
            for &idx in &component_pixels {
                mask_alpha[idx] = 255;
            }
        }
    }

    rects.sort_by(|a, b| {
        a.y1.total_cmp(&b.y1)
            .then_with(|| a.x1.total_cmp(&b.x1))
            .then_with(|| a.y2.total_cmp(&b.y2))
            .then_with(|| a.x2.total_cmp(&b.x2))
    });
    if rects.len() > 2500 {
        rects.truncate(2500);
    }
    (rects, mask_alpha)
}
