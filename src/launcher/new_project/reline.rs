/*
File: src/launcher/new_project/reline.rs

Purpose:
Background Reline pipeline bridge for the New Project launcher window.

Main responsibilities:
- save in-memory ribbon pages to temporary PNG files;
- call the already running Python AI backend `/reline/process` endpoint;
- fetch the Reline model catalog from `/reline/models`;
- load processed PNG output back into ribbon pages;
- keep Reline inference and model downloads away from the egui thread.

Key structures:
- RelineController
- RelineModelCatalogController
- RelineOptions
- RelineInputImage
- RelineEvent

Notes:
This module does not start the Python AI backend. The user or settings runtime must run it
separately; failures are surfaced as backend connectivity errors.
*/

use crate::backend_ipc::{self, CallError};
use crate::launcher::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use crate::tabs::translation::backend_health::{ai_backend_offline_error, check_ai_backend_health};
use image::{ImageFormat, RgbaImage};
use ms_thread as thread;
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use web_time::{Duration, SystemTime, UNIX_EPOCH};

// Reline runs over the v2 framed transport. Endpoint labels are kept only for
// human-readable log/error messages and the display-only `backend_endpoint`.
const RELINE_ENDPOINT: &str = "/reline/process";
const RELINE_MODELS_ENDPOINT: &str = "/reline/models";
// Per-call timeouts for the v2 framed `call`. Listing models is cheap; one Reline
// pass can run for up to an hour on CPU.
const RELINE_MODELS_CALL_TIMEOUT: Duration = Duration::from_secs(45);
const RELINE_PROCESS_CALL_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct RelineInputImage {
    pub name: String,
    pub image: RgbaImage,
}

#[derive(Clone, Serialize)]
pub struct RelineOptions {
    pub reader_mode: String,
    pub upscale: RelineUpscaleOptions,
    pub sharp: RelineSharpOptions,
    pub halftone: RelineHalftoneOptions,
    pub resize: RelineResizeOptions,
    pub level: RelineLevelOptions,
    pub cvt_color: RelineCvtColorOptions,
}

#[derive(Clone, Serialize)]
pub struct RelineUpscaleOptions {
    pub enabled: bool,
    pub model_name: String,
    pub model_path: String,
    pub model_url: String,
    pub tiler: String,
    pub target_scale: Option<u32>,
    pub dtype: String,
    pub exact_tiler_size: u32,
    pub allow_cpu_upscale: bool,
}

#[derive(Clone, Serialize)]
pub struct RelineSharpOptions {
    pub enabled: bool,
    pub low_input: i32,
    pub high_input: i32,
    pub gamma: f32,
    pub diapason_white: i32,
    pub diapason_black: i32,
    pub canny: bool,
    pub canny_type: String,
}

#[derive(Clone, Serialize)]
pub struct RelineHalftoneOptions {
    pub enabled: bool,
    pub dot_size: i32,
    pub angle: i32,
    pub dot_type: String,
    pub halftone_mode: String,
    pub ssaa_scale: Option<f32>,
    pub ssaa_filter: String,
    pub disable_auto_dot: bool,
}

#[derive(Clone, Serialize)]
pub struct RelineResizeOptions {
    pub enabled: bool,
    pub height: Option<u32>,
    pub width: Option<u32>,
    pub percent: Option<f32>,
    pub filter: String,
    pub gamma_correction: bool,
    pub spread: bool,
    pub spread_size: u32,
}

#[derive(Clone, Serialize)]
pub struct RelineLevelOptions {
    pub enabled: bool,
    pub low_input: i32,
    pub high_input: i32,
    pub low_output: i32,
    pub high_output: i32,
    pub gamma: f32,
}

#[derive(Clone, Serialize)]
pub struct RelineCvtColorOptions {
    pub enabled: bool,
    pub cvt_type: String,
}

#[derive(Debug)]
struct PendingReline {
    rx: Receiver<RelineWorkerEvent>,
}

pub struct RelineController {
    pending: Option<PendingReline>,
}

pub struct RelineSuccess {
    pub pages: Vec<RibbonPage>,
    pub processed_images: usize,
    pub backend_endpoint: String,
}

#[derive(Clone, Debug)]
pub struct RelineModelCatalogEntry {
    pub name: String,
    pub filename: String,
    pub downloaded: bool,
}

pub enum RelineEvent {
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    Completed(RelineSuccess),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

enum RelineWorkerEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Finished(Result<RelineSuccess, RelineError>),
}

#[derive(Debug)]
struct RelineError {
    user_message: String,
    log_message: String,
}

#[derive(Debug)]
struct PendingRelineModelCatalog {
    rx: Receiver<RelineModelCatalogWorkerEvent>,
}

pub struct RelineModelCatalogController {
    pending: Option<PendingRelineModelCatalog>,
}

pub enum RelineModelCatalogEvent {
    Loaded(Vec<RelineModelCatalogEntry>),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

enum RelineModelCatalogWorkerEvent {
    Finished(Result<Vec<RelineModelCatalogEntry>, RelineError>),
}

impl RelineController {
    pub fn new() -> Self {
        Self { pending: None }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn begin(&mut self, images: Vec<RelineInputImage>, options: RelineOptions) {
        self.pending = Some(PendingReline {
            rx: spawn_reline_worker(images, options),
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<RelineEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(RelineWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(RelineEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(RelineWorkerEvent::Finished(result)) => match result {
                    Ok(success) => {
                        ctx.request_repaint();
                        return Some(RelineEvent::Completed(success));
                    }
                    Err(err) => {
                        return Some(RelineEvent::Failed {
                            user_message: err.user_message,
                            log_message: err.log_message,
                        });
                    }
                },
                Err(mpsc::TryRecvError::Empty) => {
                    self.pending = Some(pending);
                    return last_progress;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(RelineEvent::WorkerDisconnected);
                }
            }
        }
    }
}

impl RelineModelCatalogController {
    pub fn new() -> Self {
        Self { pending: None }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn begin(&mut self) {
        if self.pending.is_some() {
            return;
        }
        self.pending = Some(PendingRelineModelCatalog {
            rx: spawn_reline_model_catalog_worker(),
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<RelineModelCatalogEvent> {
        let pending = self.pending.take()?;
        match pending.rx.try_recv() {
            Ok(RelineModelCatalogWorkerEvent::Finished(result)) => {
                ctx.request_repaint();
                match result {
                    Ok(models) => Some(RelineModelCatalogEvent::Loaded(models)),
                    Err(err) => Some(RelineModelCatalogEvent::Failed {
                        user_message: err.user_message,
                        log_message: err.log_message,
                    }),
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.pending = Some(pending);
                None
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(RelineModelCatalogEvent::WorkerDisconnected)
            }
        }
    }
}

fn spawn_reline_model_catalog_worker() -> Receiver<RelineModelCatalogWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    match thread::Builder::new()
        .name("new-project-reline-models".to_string())
        .spawn(move || {
            let result = fetch_reline_model_catalog();
            if tx_worker
                .send(RelineModelCatalogWorkerEvent::Finished(result))
                .is_err()
            {
                crate::runtime_log::log_warn(
                    "[new-project] failed to send Reline model catalog result to UI",
                );
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "[new-project] failed to spawn Reline model catalog worker: {err}"
            ));
            if tx
                .send(RelineModelCatalogWorkerEvent::Finished(Err(RelineError {
                    user_message: t!("launcher.new_project.reline.load_models_error").to_string(),
                    log_message: format!("failed to spawn Reline model catalog worker: {err}"),
                })))
                .is_err()
            {
                crate::runtime_log::log_warn(
                    "[new-project] failed to deliver Reline model catalog spawn error",
                );
            }
        }
    }
    rx
}

fn fetch_reline_model_catalog() -> Result<Vec<RelineModelCatalogEntry>, RelineError> {
    let client = backend_ipc::shared_client().map_err(|err| RelineError {
        user_message: ai_backend_offline_error().to_string(),
        log_message: format!("Reline model catalog backend offline: {err}"),
    })?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_RELINE_MODELS,
            json!({}),
            &[],
            RELINE_MODELS_CALL_TIMEOUT,
        )
        .map_err(|err| {
            map_reline_call_error(err, RELINE_MODELS_ENDPOINT, t!("launcher.new_project.reline.models_list_label"))
        })?;

    let models = parse_reline_models(&header);
    Ok(models)
}

/// Parses the `models` array from a `reline.models` response header into the UI
/// catalog entries, dropping unnamed entries and sorting case-insensitively by
/// name (identical to the previous HTTP-path behavior).
fn parse_reline_models(header: &Value) -> Vec<RelineModelCatalogEntry> {
    let mut models: Vec<RelineModelCatalogEntry> = header
        .get("models")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|entry| {
                    let name = entry
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if name.is_empty() {
                        return None;
                    }
                    Some(RelineModelCatalogEntry {
                        name,
                        filename: entry
                            .get("filename")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        downloaded: entry
                            .get("downloaded")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    models.sort_by_key(|left| left.name.to_lowercase());
    models
}

/// Maps a v2 `CallError` from a Reline call to a `RelineError`, preserving the
/// previous UX: `Error` surfaces the backend message; `Interrupted` is a
/// transient abort; `Transport` is the backend-offline path.
fn map_reline_call_error(err: CallError, endpoint: &str, what: &str) -> RelineError {
    match err {
        CallError::Error(msg) => RelineError {
            user_message: tf!("launcher.new_project.reline.backend_error", msg = msg),
            log_message: format!("Reline endpoint '{endpoint}' returned error: {msg}"),
        },
        CallError::Interrupted(msg) => RelineError {
            user_message: tf!("launcher.new_project.reline.request_aborted", what = what),
            log_message: format!("Reline endpoint '{endpoint}' interrupted: {msg}"),
        },
        CallError::Transport(msg) => RelineError {
            user_message: ai_backend_offline_error().to_string(),
            log_message: format!("Reline endpoint '{endpoint}' transport error: {msg}"),
        },
    }
}

fn spawn_reline_worker(
    images: Vec<RelineInputImage>,
    options: RelineOptions,
) -> Receiver<RelineWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    match thread::Builder::new()
        .name("new-project-reline".to_string())
        .spawn(move || {
            let result = run_reline(images, options, &tx_worker);
            if tx_worker.send(RelineWorkerEvent::Finished(result)).is_err() {
                crate::runtime_log::log_warn("[new-project] failed to send Reline result to UI");
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "[new-project] failed to spawn Reline worker: {err}"
            ));
            if tx
                .send(RelineWorkerEvent::Finished(Err(RelineError {
                    user_message: t!("launcher.new_project.reline.start_error").to_string(),
                    log_message: format!("failed to spawn Reline worker: {err}"),
                })))
                .is_err()
            {
                crate::runtime_log::log_warn("[new-project] failed to deliver Reline spawn error");
            }
        }
    }
    rx
}

fn run_reline(
    images: Vec<RelineInputImage>,
    options: RelineOptions,
    progress_tx: &Sender<RelineWorkerEvent>,
) -> Result<RelineSuccess, RelineError> {
    if images.is_empty() {
        return Err(RelineError {
            user_message: t!("launcher.new_project.open_or_download_first_error").to_string(),
            log_message: "Reline started without input images".to_string(),
        });
    }

    check_ai_backend_health().map_err(|err| RelineError {
        user_message: t!("launcher.new_project.reline.backend_unavailable_error")
            .to_string(),
        log_message: format!("Reline backend health check failed: {err}"),
    })?;

    let total = images.len();
    send_progress(progress_tx, "prepare", 0, total);
    let work_dir = create_work_dir()?;
    // Display-only label: AF_UNIX socket path plus the Reline endpoint.
    let endpoint = format!(
        "{}{RELINE_ENDPOINT}",
        backend_ipc::backend_socket_path().display()
    );
    let mut processed = Vec::with_capacity(total);

    let result = (|| {
        for (index, input) in images.iter().enumerate() {
            send_progress(progress_tx, "reline", index + 1, total);
            let stem = format!("{:04}_{}", index + 1, sanitize_file_stem(&input.name));
            let input_path = work_dir.join(format!("{stem}_input.png"));
            let output_path = work_dir.join(format!("{stem}_output.png"));
            save_rgba_png(&input_path, &input.image)?;
            process_one_image(&input_path, &output_path, &options)?;
            let output_image = image::open(&output_path)
                .map_err(|err| RelineError {
                    user_message: t!("launcher.new_project.reline.corrupt_image_error").to_string(),
                    log_message: format!(
                        "failed to decode Reline output '{}': {err}",
                        output_path.display()
                    ),
                })?
                .to_rgba8();
            processed.push(ImportedImage {
                name: input.name.clone(),
                image: image::DynamicImage::ImageRgba8(output_image),
            });
        }
        Ok(())
    })();

    if let Err(err) = fs::remove_dir_all(&work_dir) {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to remove Reline temp dir '{}': {err}",
            work_dir.display()
        ));
    }

    result?;
    send_progress(progress_tx, "preview", total, total);
    Ok(RelineSuccess {
        pages: build_ribbon_pages(processed),
        processed_images: total,
        backend_endpoint: endpoint,
    })
}

fn process_one_image(
    input_path: &Path,
    output_path: &Path,
    options: &RelineOptions,
) -> Result<(), RelineError> {
    let header_fields = reline_process_header(input_path, output_path, options);
    let client = backend_ipc::shared_client().map_err(|err| RelineError {
        user_message: ai_backend_offline_error().to_string(),
        log_message: format!("Reline backend offline: {err}"),
    })?;
    // `reline.process` returns the backend service result dict verbatim in the
    // response header (`status:"ok"` already guarantees `ok`); the processed PNG
    // is written to `output_path` on disk — no image bytes cross the socket.
    let (_header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_RELINE_PROCESS,
            header_fields,
            &[],
            RELINE_PROCESS_CALL_TIMEOUT,
        )
        .map_err(|err| map_reline_call_error(err, RELINE_ENDPOINT, t!("launcher.new_project.reline.processing_image_label")))?;
    if !output_path.is_file() {
        return Err(RelineError {
            user_message: t!("launcher.new_project.reline.no_output_image_error").to_string(),
            log_message: format!(
                "Reline response ok but output path is missing: '{}'",
                output_path.display()
            ),
        });
    }
    Ok(())
}

/// Builds the inline `reline.process` request header: the on-disk `image_path`
/// (required), an optional `output_path`, and the serialized `RelineOptions` as
/// `params`. No image bytes are carried — Reline reads/writes the on-disk paths.
fn reline_process_header(input_path: &Path, output_path: &Path, options: &RelineOptions) -> Value {
    json!({
        "image_path": input_path,
        "output_path": output_path,
        "params": options,
    })
}

fn save_rgba_png(path: &Path, image: &RgbaImage) -> Result<(), RelineError> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|err| RelineError {
            user_message: t!("launcher.new_project.reline.prepare_png_error").to_string(),
            log_message: format!("failed to encode Reline input PNG: {err}"),
        })?;
    fs::write(path, bytes).map_err(|err| RelineError {
        user_message: t!("launcher.new_project.reline.write_temp_error").to_string(),
        log_message: format!("failed to write Reline input '{}': {err}", path.display()),
    })
}

fn create_work_dir() -> Result<PathBuf, RelineError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "manhwastudio_reline_{}_{}",
        std::process::id(),
        millis
    ));
    fs::create_dir_all(&path).map_err(|err| RelineError {
        user_message: t!("launcher.new_project.reline.prepare_temp_dir_error").to_string(),
        log_message: format!(
            "failed to create Reline temp dir '{}': {err}",
            path.display()
        ),
    })?;
    Ok(path)
}

fn send_progress(
    progress_tx: &Sender<RelineWorkerEvent>,
    stage: &'static str,
    current: usize,
    total: usize,
) {
    if progress_tx
        .send(RelineWorkerEvent::Progress {
            stage,
            current,
            total,
        })
        .is_err()
    {
        crate::runtime_log::log_warn("[new-project] failed to send Reline progress to UI");
    }
}

fn sanitize_file_stem(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("image");
    let sanitized: String = stem
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "image".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_options() -> RelineOptions {
        RelineOptions {
            reader_mode: "default".to_string(),
            upscale: RelineUpscaleOptions {
                enabled: false,
                model_name: "model".to_string(),
                model_path: String::new(),
                model_url: String::new(),
                tiler: "tiler".to_string(),
                target_scale: None,
                dtype: "fp16".to_string(),
                exact_tiler_size: 0,
                allow_cpu_upscale: false,
            },
            sharp: RelineSharpOptions {
                enabled: false,
                low_input: 0,
                high_input: 255,
                gamma: 1.0,
                diapason_white: 0,
                diapason_black: 0,
                canny: false,
                canny_type: "type".to_string(),
            },
            halftone: RelineHalftoneOptions {
                enabled: false,
                dot_size: 1,
                angle: 0,
                dot_type: "type".to_string(),
                halftone_mode: "mode".to_string(),
                ssaa_scale: None,
                ssaa_filter: "filter".to_string(),
                disable_auto_dot: false,
            },
            resize: RelineResizeOptions {
                enabled: false,
                height: None,
                width: None,
                percent: None,
                filter: "filter".to_string(),
                gamma_correction: false,
                spread: false,
                spread_size: 0,
            },
            level: RelineLevelOptions {
                enabled: false,
                low_input: 0,
                high_input: 255,
                low_output: 0,
                high_output: 255,
                gamma: 1.0,
            },
            cvt_color: RelineCvtColorOptions {
                enabled: false,
                cvt_type: "type".to_string(),
            },
        }
    }

    #[test]
    fn reline_process_header_carries_paths_and_params() {
        let header = reline_process_header(
            Path::new("/tmp/in.png"),
            Path::new("/tmp/out.png"),
            &sample_options(),
        );
        assert_eq!(
            header.get("image_path").and_then(Value::as_str),
            Some("/tmp/in.png")
        );
        assert_eq!(
            header.get("output_path").and_then(Value::as_str),
            Some("/tmp/out.png")
        );
        // `params` mirrors the serialized RelineOptions (RelineOptions: Serialize).
        let params = header.get("params").expect("params present");
        assert_eq!(
            params.get("reader_mode").and_then(Value::as_str),
            Some("default")
        );
        assert_eq!(
            params
                .get("upscale")
                .and_then(|u| u.get("enabled"))
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn parse_reline_models_filters_sorts_and_reads_fields() {
        let header = json!({
            "models": [
                { "name": "Zebra", "filename": "zebra.pth", "downloaded": true },
                { "name": "  ", "filename": "blank.pth", "downloaded": true },
                { "name": " Alpha ", "filename": "alpha.pth" },
                { "filename": "noname.pth", "downloaded": true }
            ]
        });
        let models = parse_reline_models(&header);
        // Unnamed/blank entries dropped; remaining sorted case-insensitively.
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "Alpha");
        assert_eq!(models[0].filename, "alpha.pth");
        assert!(!models[0].downloaded); // missing `downloaded` => false
        assert_eq!(models[1].name, "Zebra");
        assert!(models[1].downloaded);
    }

    #[test]
    fn parse_reline_models_handles_missing_models_key() {
        assert!(parse_reline_models(&json!({})).is_empty());
    }

    #[test]
    fn map_reline_call_error_maps_each_variant() {
        let err =
            map_reline_call_error(CallError::Error("boom".to_string()), "/reline/process", "x");
        assert_eq!(
            err.user_message,
            tf!("launcher.new_project.reline.backend_error", msg = "boom"),
        );

        let err = map_reline_call_error(
            CallError::Interrupted("c".to_string()),
            "/reline/process",
            "x",
        );
        assert_eq!(
            err.user_message,
            tf!("launcher.new_project.reline.request_aborted", what = "x"),
        );

        let err = map_reline_call_error(
            CallError::Transport("dead".to_string()),
            "/reline/process",
            "x",
        );
        assert_eq!(err.user_message, ai_backend_offline_error());
    }
}
