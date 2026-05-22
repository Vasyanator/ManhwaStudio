/*
File: src/launcher/new_project/waifu2x.rs

Purpose:
Background waifu2x pipeline for the New Project launcher window.

Main responsibilities:
- resolve the bundled waifu2x shared library and model directory for the current platform;
- download and extract the platform waifu2x package when the bundled runtime is missing;
- lazily load the shared library on first use and keep the model/context alive while the
  New Project window stays open;
- process ribbon pages directly from memory in a worker thread and stream progress back to egui;
- cancel active processing and unload the runtime when the New Project window closes.

Key structures:
- Waifu2xController
- Waifu2xOptions
- Waifu2xInputImage
- Waifu2xEvent
- Waifu2xBackendProbe

Notes:
The backend is dynamically loaded at runtime via `libloading`; the application must start
without the library present. If the bundled shared library or default model directory is
missing, the worker downloads the platform archive from the configured GitHub release and
extracts it into `data_dir()/waifu2x` before processing.
*/

use crate::config;
use crate::launcher::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use image::RgbaImage;
use libloading::Library;
use std::ffi::{CString, c_char, c_int, c_void};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use zip::ZipArchive;

#[derive(Clone)]
pub struct Waifu2xInputImage {
    pub name: String,
    pub image: RgbaImage,
}

#[derive(Clone, Copy)]
pub struct Waifu2xOptions {
    pub noise: i32,
    pub scale: u32,
    pub tile_size: u32,
}

#[derive(Debug)]
struct PendingWaifu2x {
    rx: Receiver<Waifu2xWorkerEvent>,
}

pub struct Waifu2xController {
    pending: Option<PendingWaifu2x>,
    backend: Waifu2xBackendProbe,
    runtime: Option<Arc<Waifu2xSharedRuntime>>,
}

pub struct Waifu2xSuccess {
    pub pages: Vec<RibbonPage>,
    pub processed_images: usize,
    pub backend_path: PathBuf,
    backend: Waifu2xBackendProbe,
}

pub enum Waifu2xEvent {
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    Completed(Waifu2xSuccess),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

enum Waifu2xWorkerEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Finished(Result<Waifu2xSuccess, Waifu2xError>),
}

#[derive(Debug)]
struct Waifu2xError {
    user_message: String,
    log_message: String,
}

#[derive(Clone)]
pub struct Waifu2xBackendProbe {
    library_path: PathBuf,
    model_dir: PathBuf,
    availability: Waifu2xAvailability,
}

#[derive(Clone)]
enum Waifu2xAvailability {
    Available,
    Unavailable { user_message: String },
}

struct Waifu2xSharedRuntime {
    backend: Mutex<Waifu2xBackendProbe>,
    state: Mutex<Option<Waifu2xRuntime>>,
    cancel_handle: Mutex<Option<Waifu2xCancelHandle>>,
    shutdown_requested: AtomicBool,
}

#[derive(Clone, Copy)]
struct Waifu2xCancelHandle {
    context: *mut c_void,
    cancel: unsafe extern "C" fn(*mut c_void),
}

struct Waifu2xRuntime {
    _library: Library,
    api: Waifu2xApi,
    context: *mut c_void,
    loaded_config: Option<LoadedWaifu2xConfig>,
}

// SAFETY: The waifu2x context is never accessed concurrently without external synchronisation.
// `Waifu2xSharedRuntime` serialises load/process through `state: Mutex<_>`, and cross-thread
// cancellation uses the dedicated C API function designed for that purpose.
unsafe impl Send for Waifu2xRuntime {}

// SAFETY: The cancel handle is copied between threads only to call the library's thread-safe
// cancellation entry point on the same context while the owning runtime stays alive.
unsafe impl Send for Waifu2xCancelHandle {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LoadedWaifu2xConfig {
    noise: i32,
    scale: u32,
    tile_size: u32,
    gpu_id: i32,
    num_threads: i32,
}

#[derive(Clone, Copy)]
struct Waifu2xApi {
    abi_version: unsafe extern "C" fn() -> c_int,
    global_init: unsafe extern "C" fn() -> c_int,
    global_cleanup: unsafe extern "C" fn() -> c_int,
    default_gpu_index: unsafe extern "C" fn() -> c_int,
    create: unsafe extern "C" fn() -> *mut c_void,
    destroy: unsafe extern "C" fn(*mut c_void),
    load: unsafe extern "C" fn(*mut c_void, *const Waifu2xLoadParams) -> c_int,
    process: unsafe extern "C" fn(
        *mut c_void,
        *const u8,
        c_int,
        c_int,
        c_int,
        *mut Waifu2xImage,
    ) -> c_int,
    image_free: unsafe extern "C" fn(*mut Waifu2xImage),
    cancel: unsafe extern "C" fn(*mut c_void),
    last_error: unsafe extern "C" fn(*const c_void, *mut c_char, usize) -> usize,
}

#[repr(C)]
struct Waifu2xImage {
    data: *mut u8,
    width: c_int,
    height: c_int,
    channels: c_int,
}

#[repr(C)]
struct Waifu2xLoadParams {
    model_dir_utf8: *const c_char,
    noise: c_int,
    scale: c_int,
    tile_size: c_int,
    gpu_id: c_int,
    tta_mode: c_int,
    num_threads: c_int,
}

const WAIFU2X_ABI_VERSION: i32 = 1;
const DOWNLOAD_BUFFER_SIZE: usize = 64 * 1024;

impl Waifu2xController {
    pub fn new() -> Self {
        let backend = probe_waifu2x_backend();
        let runtime = Some(Arc::new(Waifu2xSharedRuntime::new(backend.clone())));
        Self {
            pending: None,
            backend,
            runtime,
        }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        self.backend.unavailable_reason()
    }

    pub fn backend_path(&self) -> &Path {
        &self.backend.library_path
    }

    pub fn begin(&mut self, images: Vec<Waifu2xInputImage>, options: Waifu2xOptions) {
        let runtime = self
            .runtime
            .get_or_insert_with(|| Arc::new(Waifu2xSharedRuntime::new(self.backend.clone())))
            .clone();
        self.pending = Some(PendingWaifu2x {
            rx: spawn_waifu2x_worker(runtime, images, options),
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<Waifu2xEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(Waifu2xWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(Waifu2xEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(Waifu2xWorkerEvent::Finished(result)) => match result {
                    Ok(success) => {
                        self.backend = success.backend.clone();
                        ctx.request_repaint();
                        return Some(Waifu2xEvent::Completed(success));
                    }
                    Err(err) => {
                        return Some(Waifu2xEvent::Failed {
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
                    return Some(Waifu2xEvent::WorkerDisconnected);
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.request_shutdown();
        }
        self.pending = None;
    }
}

impl Waifu2xBackendProbe {
    fn is_available(&self) -> bool {
        matches!(self.availability, Waifu2xAvailability::Available)
    }

    fn unavailable_reason(&self) -> Option<&str> {
        match &self.availability {
            Waifu2xAvailability::Available => None,
            Waifu2xAvailability::Unavailable { user_message } => Some(user_message.as_str()),
        }
    }

    fn missing_error(&self) -> Waifu2xError {
        Waifu2xError {
            user_message: self
                .unavailable_reason()
                .unwrap_or("waifu2x недоступен.")
                .to_string(),
            log_message: format!(
                "waifu2x backend unavailable: library='{}', model_dir='{}'",
                self.library_path.display(),
                self.model_dir.display()
            ),
        }
    }
}

impl Waifu2xSharedRuntime {
    fn new(backend: Waifu2xBackendProbe) -> Self {
        Self {
            backend: Mutex::new(backend),
            state: Mutex::new(None),
            cancel_handle: Mutex::new(None),
            shutdown_requested: AtomicBool::new(false),
        }
    }

    fn process_images(
        &self,
        images: &[Waifu2xInputImage],
        options: Waifu2xOptions,
        progress_tx: &Sender<Waifu2xWorkerEvent>,
    ) -> Result<Waifu2xSuccess, Waifu2xError> {
        let total = images.len();
        let backend = self.ensure_backend_ready(progress_tx)?;
        let mut state = self.state.lock().map_err(|_| Waifu2xError {
            user_message: "waifu2x завершился с ошибкой.".to_string(),
            log_message: "waifu2x runtime mutex poisoned".to_string(),
        })?;

        if state.is_none() {
            send_progress(progress_tx, "prepare", 0, total);
            let runtime = create_runtime(&backend)?;
            let cancel = Waifu2xCancelHandle {
                context: runtime.context,
                cancel: runtime.api.cancel,
            };
            set_cancel_handle(&self.cancel_handle, Some(cancel));
            *state = Some(runtime);
        }

        let runtime = match state.as_mut() {
            Some(runtime) => runtime,
            None => {
                return Err(Waifu2xError {
                    user_message: "Не удалось запустить waifu2x.".to_string(),
                    log_message: "waifu2x runtime disappeared after creation".to_string(),
                });
            }
        };

        runtime.ensure_loaded(&backend, options)?;

        let mut processed = Vec::with_capacity(total);
        for (index, input) in images.iter().enumerate() {
            if self.shutdown_requested.load(Ordering::SeqCst) {
                return Err(Waifu2xError {
                    user_message: "waifu2x был остановлен при закрытии окна.".to_string(),
                    log_message: "waifu2x cancelled because new project window closed".to_string(),
                });
            }
            send_progress(progress_tx, "waifu2x", index + 1, total);
            let output_image = runtime
                .process_image(&input.image)
                .map_err(|err| Waifu2xError {
                    user_message: "waifu2x завершился с ошибкой. Подробности смотрите в логах."
                        .to_string(),
                    log_message: format!("failed to process image '{}': {err}", input.name),
                })?;
            processed.push(ImportedImage {
                name: input.name.clone(),
                image: image::DynamicImage::ImageRgba8(output_image),
            });
        }

        send_progress(progress_tx, "preview", total, total);
        let pages = build_ribbon_pages(processed);
        if self.shutdown_requested.load(Ordering::SeqCst) {
            set_cancel_handle(&self.cancel_handle, None);
            *state = None;
        }

        Ok(Waifu2xSuccess {
            processed_images: total,
            pages,
            backend_path: backend.library_path.clone(),
            backend,
        })
    }

    fn ensure_backend_ready(
        &self,
        progress_tx: &Sender<Waifu2xWorkerEvent>,
    ) -> Result<Waifu2xBackendProbe, Waifu2xError> {
        let mut backend = self.backend.lock().map_err(|_| Waifu2xError {
            user_message: "waifu2x завершился с ошибкой.".to_string(),
            log_message: "waifu2x backend mutex poisoned".to_string(),
        })?;
        if backend.is_available() {
            return Ok(backend.clone());
        }

        install_waifu2x_backend(progress_tx)?;
        *backend = probe_waifu2x_backend();
        if backend.is_available() {
            Ok(backend.clone())
        } else {
            Err(backend.missing_error())
        }
    }

    fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        self.cancel();
        if let Ok(mut state) = self.state.try_lock() {
            set_cancel_handle(&self.cancel_handle, None);
            *state = None;
        }
    }

    fn cancel(&self) {
        let Ok(handle) = self.cancel_handle.lock() else {
            return;
        };
        let Some(handle) = *handle else {
            return;
        };
        // SAFETY: The handle is installed only while the runtime/context is alive.
        unsafe {
            (handle.cancel)(handle.context);
        }
    }
}

impl Waifu2xRuntime {
    fn ensure_loaded(
        &mut self,
        backend: &Waifu2xBackendProbe,
        options: Waifu2xOptions,
    ) -> Result<(), Waifu2xError> {
        let gpu_id = self.detect_gpu_id();
        let num_threads = available_parallelism_i32();
        let desired = LoadedWaifu2xConfig {
            noise: options.noise,
            scale: options.scale,
            tile_size: options.tile_size,
            gpu_id,
            num_threads,
        };
        if self.loaded_config == Some(desired) {
            return Ok(());
        }

        let model_dir_utf8 = backend.model_dir.to_str().ok_or_else(|| Waifu2xError {
            user_message: "Не удалось подготовить путь к модели waifu2x.".to_string(),
            log_message: format!(
                "waifu2x model dir is not valid UTF-8: '{}'",
                backend.model_dir.display()
            ),
        })?;
        let model_dir = CString::new(model_dir_utf8).map_err(|err| Waifu2xError {
            user_message: "Не удалось подготовить путь к модели waifu2x.".to_string(),
            log_message: format!(
                "waifu2x model dir contains interior NUL '{}': {err}",
                backend.model_dir.display()
            ),
        })?;
        let scale = i32::try_from(options.scale).map_err(|err| Waifu2xError {
            user_message: "Некорректный масштаб waifu2x.".to_string(),
            log_message: format!(
                "failed to convert waifu2x scale {} to i32: {err}",
                options.scale
            ),
        })?;
        let tile_size = i32::try_from(options.tile_size).map_err(|err| Waifu2xError {
            user_message: "Некорректный tile size waifu2x.".to_string(),
            log_message: format!(
                "failed to convert waifu2x tile size {} to i32: {err}",
                options.tile_size
            ),
        })?;
        let params = Waifu2xLoadParams {
            model_dir_utf8: model_dir.as_ptr(),
            noise: options.noise,
            scale,
            tile_size,
            gpu_id,
            tta_mode: 0,
            num_threads,
        };

        // SAFETY: `context` and `params` are valid for the duration of the call.
        let status = unsafe { (self.api.load)(self.context, &params) };
        if status != 0 {
            return Err(Waifu2xError {
                user_message: "Не удалось загрузить модель waifu2x.".to_string(),
                log_message: format!(
                    "waifu2x_load failed with status {status}: {}",
                    self.last_error_message()
                ),
            });
        }

        self.loaded_config = Some(desired);
        Ok(())
    }

    fn process_image(&mut self, image: &RgbaImage) -> Result<RgbaImage, String> {
        let width = i32::try_from(image.width()).map_err(|err| {
            format!(
                "failed to convert input width {} to i32: {err}",
                image.width()
            )
        })?;
        let height = i32::try_from(image.height()).map_err(|err| {
            format!(
                "failed to convert input height {} to i32: {err}",
                image.height()
            )
        })?;
        let mut output = Waifu2xImage {
            data: std::ptr::null_mut(),
            width: 0,
            height: 0,
            channels: 0,
        };

        // SAFETY: `context` is valid, input buffer is immutable and tightly packed RGBA,
        // and `output` is zero-initialized as required by the C API.
        let status = unsafe {
            (self.api.process)(
                self.context,
                image.as_raw().as_ptr(),
                width,
                height,
                4,
                &mut output,
            )
        };
        if status != 0 {
            return Err(format!(
                "waifu2x_process failed with status {status}: {}",
                self.last_error_message()
            ));
        }

        let result = waifu2x_image_to_rgba(&self.api, &mut output);
        if let Err(err) = &result {
            crate::runtime_log::log_warn(format!(
                "[new-project] failed to decode waifu2x output image: {err}"
            ));
        }
        result
    }

    fn detect_gpu_id(&self) -> i32 {
        // SAFETY: the library is initialised and the function has no preconditions beyond that.
        let gpu_id = unsafe { (self.api.default_gpu_index)() };
        gpu_id.max(-1)
    }

    fn last_error_message(&self) -> String {
        last_error_message(self.api, self.context)
    }
}

impl Drop for Waifu2xRuntime {
    fn drop(&mut self) {
        // SAFETY: teardown order mirrors the C API contract. Errors are logged and ignored.
        unsafe {
            (self.api.cancel)(self.context);
            (self.api.destroy)(self.context);
            let cleanup_status = (self.api.global_cleanup)();
            if cleanup_status != 0 {
                crate::runtime_log::log_warn(format!(
                    "[new-project] waifu2x_global_cleanup returned {cleanup_status}"
                ));
            }
        }
    }
}

pub fn default_waifu2x_backend_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    let relative = Path::new("waifu2x")
        .join("Win")
        .join("waifu2x-ncnn-vulkan.dll");

    #[cfg(target_os = "macos")]
    let relative = Path::new("waifu2x")
        .join("Mac")
        .join("libwaifu2x-ncnn-vulkan.dylib");

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let relative = Path::new("waifu2x")
        .join("Lin")
        .join("libwaifu2x-ncnn-vulkan.so");

    config::data_dir().join(relative)
}

pub fn default_waifu2x_model_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let relative = Path::new("waifu2x").join("Win").join("models-cunet");

    #[cfg(target_os = "macos")]
    let relative = Path::new("waifu2x").join("Mac").join("models-cunet");

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let relative = Path::new("waifu2x").join("Lin").join("models-cunet");

    config::data_dir().join(relative)
}

fn probe_waifu2x_backend() -> Waifu2xBackendProbe {
    let library_path = default_waifu2x_backend_path();
    let model_dir = default_waifu2x_model_dir();
    let availability = if !library_path.is_file() {
        Waifu2xAvailability::Unavailable {
            user_message: format!(
                "waifu2x отключён: не найдена библиотека '{}'.",
                library_path.display()
            ),
        }
    } else if !model_dir.is_dir() {
        Waifu2xAvailability::Unavailable {
            user_message: format!(
                "waifu2x отключён: не найдена папка моделей '{}'.",
                model_dir.display()
            ),
        }
    } else {
        Waifu2xAvailability::Available
    };
    Waifu2xBackendProbe {
        library_path,
        model_dir,
        availability,
    }
}

fn install_waifu2x_backend(progress_tx: &Sender<Waifu2xWorkerEvent>) -> Result<(), Waifu2xError> {
    send_progress(progress_tx, "download_waifu2x", 0, 1);
    let package_path = download_waifu2x_package(progress_tx)?;
    extract_waifu2x_package(&package_path, progress_tx)?;
    if let Err(err) = fs::remove_file(&package_path) {
        crate::runtime_log::log_warn(format!(
            "[new-project] failed to remove temporary waifu2x archive '{}': {err}",
            package_path.display()
        ));
    }
    Ok(())
}

fn download_waifu2x_package(
    progress_tx: &Sender<Waifu2xWorkerEvent>,
) -> Result<PathBuf, Waifu2xError> {
    let download_dir = config::data_dir().join("waifu2x").join(".download");
    fs::create_dir_all(&download_dir).map_err(|err| Waifu2xError {
        user_message: "Не удалось подготовить папку для загрузки waifu2x.".to_string(),
        log_message: format!(
            "failed to create waifu2x download dir '{}': {err}",
            download_dir.display()
        ),
    })?;

    let archive_path = download_dir.join(waifu2x_archive_name());
    let partial_path = archive_path.with_extension("zip.part");
    if partial_path.exists() {
        fs::remove_file(&partial_path).map_err(|err| Waifu2xError {
            user_message: "Не удалось обновить временный файл waifu2x.".to_string(),
            log_message: format!(
                "failed to remove old partial waifu2x archive '{}': {err}",
                partial_path.display()
            ),
        })?;
    }

    let url = waifu2x_package_url();
    crate::runtime_log::log_info(format!(
        "[new-project] downloading waifu2x backend from {url}"
    ));
    let response = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(20))
        .timeout_read(std::time::Duration::from_secs(120))
        .build()
        .get(url)
        .call()
        .map_err(|err| waifu2x_download_error(url, err))?;

    let total_bytes = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .map(saturating_usize)
        .unwrap_or(0);

    let mut reader = response.into_reader();
    let mut output = File::create(&partial_path).map_err(|err| Waifu2xError {
        user_message: "Не удалось создать файл архива waifu2x.".to_string(),
        log_message: format!(
            "failed to create waifu2x archive '{}': {err}",
            partial_path.display()
        ),
    })?;
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_SIZE];
    let mut downloaded = 0_u64;
    loop {
        let read = reader.read(&mut buffer).map_err(|err| Waifu2xError {
            user_message: "Не удалось скачать архив waifu2x.".to_string(),
            log_message: format!("failed to read waifu2x response from '{url}': {err}"),
        })?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|err| Waifu2xError {
                user_message: "Не удалось записать архив waifu2x.".to_string(),
                log_message: format!(
                    "failed to write waifu2x archive '{}': {err}",
                    partial_path.display()
                ),
            })?;
        let read_u64 = u64::try_from(read).map_err(|err| Waifu2xError {
            user_message: "Не удалось скачать архив waifu2x.".to_string(),
            log_message: format!("failed to convert downloaded chunk size {read}: {err}"),
        })?;
        downloaded = downloaded.saturating_add(read_u64);
        send_progress(
            progress_tx,
            "download_waifu2x",
            saturating_usize(downloaded),
            total_bytes,
        );
    }
    output.flush().map_err(|err| Waifu2xError {
        user_message: "Не удалось сохранить архив waifu2x.".to_string(),
        log_message: format!(
            "failed to flush waifu2x archive '{}': {err}",
            partial_path.display()
        ),
    })?;
    drop(output);

    fs::rename(&partial_path, &archive_path).map_err(|err| Waifu2xError {
        user_message: "Не удалось завершить загрузку waifu2x.".to_string(),
        log_message: format!(
            "failed to move waifu2x archive '{}' to '{}': {err}",
            partial_path.display(),
            archive_path.display()
        ),
    })?;
    Ok(archive_path)
}

fn extract_waifu2x_package(
    archive_path: &Path,
    progress_tx: &Sender<Waifu2xWorkerEvent>,
) -> Result<(), Waifu2xError> {
    send_progress(progress_tx, "extract_waifu2x", 0, 1);
    let archive_file = File::open(archive_path).map_err(|err| Waifu2xError {
        user_message: "Не удалось открыть архив waifu2x.".to_string(),
        log_message: format!(
            "failed to open waifu2x archive '{}': {err}",
            archive_path.display()
        ),
    })?;
    let mut archive = ZipArchive::new(archive_file).map_err(|err| Waifu2xError {
        user_message: "Не удалось прочитать архив waifu2x.".to_string(),
        log_message: format!(
            "failed to parse waifu2x zip archive '{}': {err}",
            archive_path.display()
        ),
    })?;
    let total = archive.len();
    let extract_root = config::data_dir().join("waifu2x");
    fs::create_dir_all(&extract_root).map_err(|err| Waifu2xError {
        user_message: "Не удалось подготовить папку waifu2x.".to_string(),
        log_message: format!(
            "failed to create waifu2x root '{}': {err}",
            extract_root.display()
        ),
    })?;

    for index in 0..total {
        let mut entry = archive.by_index(index).map_err(|err| Waifu2xError {
            user_message: "Не удалось распаковать архив waifu2x.".to_string(),
            log_message: format!(
                "failed to access entry {index} in '{}': {err}",
                archive_path.display()
            ),
        })?;
        let Some(relative_path) = entry.enclosed_name() else {
            return Err(Waifu2xError {
                user_message: "Архив waifu2x содержит небезопасный путь.".to_string(),
                log_message: format!(
                    "unsafe waifu2x zip entry '{}' in '{}'",
                    entry.name(),
                    archive_path.display()
                ),
            });
        };
        let output_path = extract_root.join(relative_path);
        if entry.is_dir() {
            fs::create_dir_all(&output_path).map_err(|err| Waifu2xError {
                user_message: "Не удалось создать папку из архива waifu2x.".to_string(),
                log_message: format!(
                    "failed to create waifu2x extracted dir '{}': {err}",
                    output_path.display()
                ),
            })?;
        } else {
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent).map_err(|err| Waifu2xError {
                    user_message: "Не удалось создать папку из архива waifu2x.".to_string(),
                    log_message: format!(
                        "failed to create waifu2x extracted parent '{}': {err}",
                        parent.display()
                    ),
                })?;
            }
            let mut output = File::create(&output_path).map_err(|err| Waifu2xError {
                user_message: "Не удалось записать файл из архива waifu2x.".to_string(),
                log_message: format!(
                    "failed to create waifu2x extracted file '{}': {err}",
                    output_path.display()
                ),
            })?;
            io::copy(&mut entry, &mut output).map_err(|err| Waifu2xError {
                user_message: "Не удалось распаковать файл waifu2x.".to_string(),
                log_message: format!(
                    "failed to extract waifu2x file '{}' to '{}': {err}",
                    entry.name(),
                    output_path.display()
                ),
            })?;
        }
        send_progress(
            progress_tx,
            "extract_waifu2x",
            index.saturating_add(1),
            total,
        );
    }
    Ok(())
}

fn waifu2x_download_error(url: &str, err: ureq::Error) -> Waifu2xError {
    match err {
        ureq::Error::Status(status, response) => Waifu2xError {
            user_message: "Не удалось скачать waifu2x.".to_string(),
            log_message: format!(
                "waifu2x download failed for '{url}' with HTTP {status}: {}",
                response.status_text()
            ),
        },
        ureq::Error::Transport(transport) => Waifu2xError {
            user_message: "Не удалось скачать waifu2x. Проверьте подключение к интернету."
                .to_string(),
            log_message: format!("waifu2x download transport error for '{url}': {transport}"),
        },
    }
}

#[cfg(target_os = "windows")]
fn waifu2x_archive_name() -> &'static str {
    "Win.zip"
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn waifu2x_archive_name() -> &'static str {
    "Lin.zip"
}

#[cfg(target_os = "macos")]
fn waifu2x_archive_name() -> &'static str {
    "Mac.zip"
}

fn waifu2x_package_url() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        concat!(
            "https://github.com/Vasyanator/libwaifu2x-ncnn-vulkan/releases/download/1.0",
            "/Win.zip"
        )
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        concat!(
            "https://github.com/Vasyanator/libwaifu2x-ncnn-vulkan/releases/download/1.0",
            "/Lin.zip"
        )
    }
    #[cfg(target_os = "macos")]
    {
        concat!(
            "https://github.com/Vasyanator/libwaifu2x-ncnn-vulkan/releases/download/1.0",
            "/Mac.zip"
        )
    }
}

fn saturating_usize(value: u64) -> usize {
    match usize::try_from(value) {
        Ok(converted) => converted,
        Err(_) => usize::MAX,
    }
}

fn spawn_waifu2x_worker(
    runtime: Arc<Waifu2xSharedRuntime>,
    images: Vec<Waifu2xInputImage>,
    options: Waifu2xOptions,
) -> Receiver<Waifu2xWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    match thread::Builder::new()
        .name("new-project-waifu2x".to_string())
        .spawn(move || {
            let result = run_waifu2x(images, options, runtime.as_ref(), &tx_worker);
            if tx_worker
                .send(Waifu2xWorkerEvent::Finished(result))
                .is_err()
            {
                crate::runtime_log::log_warn("[new-project] failed to send waifu2x result to UI");
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "[new-project] failed to spawn waifu2x worker: {err}"
            ));
            if tx
                .send(Waifu2xWorkerEvent::Finished(Err(Waifu2xError {
                    user_message: "Не удалось запустить waifu2x.".to_string(),
                    log_message: format!("failed to spawn waifu2x worker: {err}"),
                })))
                .is_err()
            {
                crate::runtime_log::log_warn("[new-project] failed to deliver waifu2x spawn error");
            }
        }
    }
    rx
}

fn run_waifu2x(
    images: Vec<Waifu2xInputImage>,
    options: Waifu2xOptions,
    runtime: &Waifu2xSharedRuntime,
    progress_tx: &Sender<Waifu2xWorkerEvent>,
) -> Result<Waifu2xSuccess, Waifu2xError> {
    if images.is_empty() {
        return Err(Waifu2xError {
            user_message: "Сначала откройте или скачайте изображения.".to_string(),
            log_message: "waifu2x started without input images".to_string(),
        });
    }
    runtime.process_images(&images, options, progress_tx)
}

fn create_runtime(backend: &Waifu2xBackendProbe) -> Result<Waifu2xRuntime, Waifu2xError> {
    // SAFETY: Loading a shared library is inherently unsafe; the path points to the bundled
    // waifu2x backend resolved by this module.
    let library = unsafe { Library::new(&backend.library_path) }.map_err(|err| Waifu2xError {
        user_message: "Не удалось загрузить библиотеку waifu2x.".to_string(),
        log_message: format!(
            "failed to load waifu2x library '{}': {err}",
            backend.library_path.display()
        ),
    })?;
    let api = load_waifu2x_api(&library, &backend.library_path)?;

    // SAFETY: the function pointer comes from the freshly loaded library.
    let abi_version = unsafe { (api.abi_version)() };
    if abi_version != WAIFU2X_ABI_VERSION {
        return Err(Waifu2xError {
            user_message: "Неподдерживаемая версия библиотеки waifu2x.".to_string(),
            log_message: format!(
                "waifu2x ABI mismatch for '{}': expected {}, got {}",
                backend.library_path.display(),
                WAIFU2X_ABI_VERSION,
                abi_version
            ),
        });
    }

    // SAFETY: this is the required global runtime initialisation before creating a context.
    let init_status = unsafe { (api.global_init)() };
    if init_status != 0 {
        return Err(Waifu2xError {
            user_message: "Не удалось инициализировать runtime waifu2x.".to_string(),
            log_message: format!(
                "waifu2x_global_init failed for '{}': status {init_status}",
                backend.library_path.display()
            ),
        });
    }

    // SAFETY: global_init succeeded, so creating a context is valid.
    let context = unsafe { (api.create)() };
    if context.is_null() {
        // SAFETY: matched cleanup after successful global_init.
        let cleanup_status = unsafe { (api.global_cleanup)() };
        if cleanup_status != 0 {
            crate::runtime_log::log_warn(format!(
                "[new-project] waifu2x_global_cleanup after create failure returned {cleanup_status}"
            ));
        }
        return Err(Waifu2xError {
            user_message: "Не удалось создать контекст waifu2x.".to_string(),
            log_message: format!(
                "waifu2x_create returned null for '{}'",
                backend.library_path.display()
            ),
        });
    }

    Ok(Waifu2xRuntime {
        _library: library,
        api,
        context,
        loaded_config: None,
    })
}

fn load_waifu2x_api(library: &Library, library_path: &Path) -> Result<Waifu2xApi, Waifu2xError> {
    Ok(Waifu2xApi {
        abi_version: load_symbol(library, library_path, b"waifu2x_abi_version\0")?,
        global_init: load_symbol(library, library_path, b"waifu2x_global_init\0")?,
        global_cleanup: load_symbol(library, library_path, b"waifu2x_global_cleanup\0")?,
        default_gpu_index: load_symbol(library, library_path, b"waifu2x_default_gpu_index\0")?,
        create: load_symbol(library, library_path, b"waifu2x_create\0")?,
        destroy: load_symbol(library, library_path, b"waifu2x_destroy\0")?,
        load: load_symbol(library, library_path, b"waifu2x_load\0")?,
        process: load_symbol(library, library_path, b"waifu2x_process\0")?,
        image_free: load_symbol(library, library_path, b"waifu2x_image_free\0")?,
        cancel: load_symbol(library, library_path, b"waifu2x_cancel\0")?,
        last_error: load_symbol(library, library_path, b"waifu2x_last_error\0")?,
    })
}

fn load_symbol<T>(library: &Library, library_path: &Path, symbol: &[u8]) -> Result<T, Waifu2xError>
where
    T: Copy,
{
    // SAFETY: the symbol names are from the companion waifu2x C header and the returned
    // function pointers remain valid while `library` is kept alive inside `Waifu2xRuntime`.
    let symbol_ref = unsafe { library.get::<T>(symbol) }.map_err(|err| Waifu2xError {
        user_message: "Библиотека waifu2x повреждена или несовместима.".to_string(),
        log_message: format!(
            "failed to resolve symbol '{}' from '{}': {err}",
            String::from_utf8_lossy(symbol).trim_end_matches('\0'),
            library_path.display()
        ),
    })?;
    Ok(*symbol_ref)
}

fn last_error_message(api: Waifu2xApi, context: *mut c_void) -> String {
    // SAFETY: querying the required buffer length with a null buffer is part of the API contract.
    let len = unsafe { (api.last_error)(context.cast_const(), std::ptr::null_mut(), 0) };
    if len == 0 {
        return "no details provided by waifu2x".to_string();
    }

    let mut buffer = vec![0_u8; len.saturating_add(1)];
    // SAFETY: the buffer is writable and large enough for the NUL terminator.
    let written = unsafe {
        (api.last_error)(
            context.cast_const(),
            buffer.as_mut_ptr().cast::<c_char>(),
            buffer.len(),
        )
    };
    let used = written.min(len).min(buffer.len().saturating_sub(1));
    String::from_utf8_lossy(&buffer[..used]).into_owned()
}

fn waifu2x_image_to_rgba(api: &Waifu2xApi, image: &mut Waifu2xImage) -> Result<RgbaImage, String> {
    if image.data.is_null() {
        return Err("waifu2x returned a null output buffer".to_string());
    }

    let width = usize::try_from(image.width)
        .map_err(|err| format!("failed to convert output width {}: {err}", image.width))?;
    let height = usize::try_from(image.height)
        .map_err(|err| format!("failed to convert output height {}: {err}", image.height))?;
    let channels = usize::try_from(image.channels).map_err(|err| {
        format!(
            "failed to convert output channels {}: {err}",
            image.channels
        )
    })?;
    if channels != 3 && channels != 4 {
        // SAFETY: buffer ownership belongs to the library and must be released with image_free.
        unsafe {
            (api.image_free)(image);
        }
        return Err(format!(
            "unexpected waifu2x output channel count {channels}"
        ));
    }

    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| "waifu2x output dimensions overflowed".to_string())?;
    let byte_len = pixel_count
        .checked_mul(channels)
        .ok_or_else(|| "waifu2x output byte length overflowed".to_string())?;

    // SAFETY: `data` points to a library-owned buffer of `byte_len` bytes according to
    // the C API contract; we copy it before freeing.
    let bytes = unsafe { std::slice::from_raw_parts(image.data.cast_const(), byte_len) }.to_vec();
    // SAFETY: free the library-owned buffer after copying it.
    unsafe {
        (api.image_free)(image);
    }

    let rgba = match channels {
        4 => bytes,
        3 => {
            let mut expanded = Vec::with_capacity(pixel_count.saturating_mul(4));
            for chunk in bytes.chunks_exact(3) {
                expanded.extend_from_slice(chunk);
                expanded.push(255);
            }
            expanded
        }
        _ => unreachable!(),
    };

    let width_u32 = u32::try_from(width)
        .map_err(|err| format!("failed to convert width {width} to u32: {err}"))?;
    let height_u32 = u32::try_from(height)
        .map_err(|err| format!("failed to convert height {height} to u32: {err}"))?;
    RgbaImage::from_raw(width_u32, height_u32, rgba)
        .ok_or_else(|| "failed to build RGBA image from waifu2x output buffer".to_string())
}

fn available_parallelism_i32() -> i32 {
    match thread::available_parallelism() {
        Ok(value) => i32::try_from(value.get()).unwrap_or(i32::MAX),
        Err(_) => 1,
    }
}

fn set_cancel_handle(
    cancel_handle: &Mutex<Option<Waifu2xCancelHandle>>,
    handle: Option<Waifu2xCancelHandle>,
) {
    let Ok(mut slot) = cancel_handle.lock() else {
        crate::runtime_log::log_warn("[new-project] failed to lock waifu2x cancel handle");
        return;
    };
    *slot = handle;
}

fn send_progress(
    progress_tx: &Sender<Waifu2xWorkerEvent>,
    stage: &'static str,
    current: usize,
    total: usize,
) {
    if progress_tx
        .send(Waifu2xWorkerEvent::Progress {
            stage,
            current,
            total,
        })
        .is_err()
    {
        crate::runtime_log::log_warn("[new-project] failed to send waifu2x progress to UI");
    }
}

/// Synchronous waifu2x entry point for use by the batch executor (already off GUI thread).
/// Returns the resulting `RgbaImage`s or an error string.
pub fn run_waifu2x_sync(
    images: Vec<Waifu2xInputImage>,
    options: Waifu2xOptions,
) -> Result<Vec<RgbaImage>, String> {
    let backend = probe_waifu2x_backend();
    if !backend.is_available() {
        return Err(backend.missing_error().log_message);
    }
    let runtime = Waifu2xSharedRuntime::new(backend);
    let (dummy_tx, _rx) = mpsc::channel::<Waifu2xWorkerEvent>();
    runtime
        .process_images(&images, options, &dummy_tx)
        .map(|success| {
            success
                .pages
                .into_iter()
                .map(|page| (*page.full_image()).clone())
                .collect()
        })
        .map_err(|err| err.log_message)
}
