/*
File: crates/ms-launcher/src/new_project/waifu2x.rs

Purpose:
Background waifu2x pipeline for the New Project launcher window.

Main responsibilities:
- resolve the waifu2x shared library and model directory for the current platform, or the
  typed "not on this platform" answer of `PlatformFeature::Waifu2xUpscaler` (macOS, Android
  and wasm have no package);
- download the pinned platform package (`ms_download::fetch_verified`: size + sha256,
  retry with backoff, `.part` resume, cancel on window close) and unpack it through a
  `.staging/` tree that is renamed into place;
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
without the library present. If the shared library or model directory is missing, the worker
downloads the platform archive (`PLATFORM_PACKAGE`, pinned) into
`data_dir()/waifu2x/.download/`, unpacks it into `data_dir()/waifu2x/.staging/`, renames that
tree to the pin's generation folder `gen-<sha256 prefix>/` and then atomically switches the
`active` marker to it; older generations are removed only after the switch. The probe accepts
only the generation the marker names. A `.part` survives a failure or cancel and is resumed
by the next run; a stale `.staging/` is deleted.
*/

use ms_config as config;
use crate::new_project::ribbon::RibbonPage;
#[cfg(not(target_arch = "wasm32"))]
use crate::new_project::ribbon::{ImportedImage, build_ribbon_pages};
use image::RgbaImage;
#[cfg(not(target_arch = "wasm32"))]
use libloading::Library;
#[cfg(not(target_arch = "wasm32"))]
use std::ffi::{CString, c_char, c_int, c_void};
#[cfg(not(target_arch = "wasm32"))]
use std::fs::{self, File};
#[cfg(not(target_arch = "wasm32"))]
use ms_download::resumable::{RangeFetcher, ResumableFetch, RetryPolicy, StreamEvent, UreqFetcher};
#[cfg(not(target_arch = "wasm32"))]
use ms_download::{DownloadError, Expected, FileEvent, FileRequest, fetch_verified};
#[cfg(not(target_arch = "wasm32"))]
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::Sender;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex};
#[cfg(not(target_arch = "wasm32"))]
use ms_thread as thread;
#[cfg(not(target_arch = "wasm32"))]
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
    /// Shared FFI runtime holding the loaded waifu2x dynamic library. The dynamic
    /// library backend does not exist on the web build, so this field (and the
    /// worker it drives) is native-only.
    #[cfg(not(target_arch = "wasm32"))]
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

/// What the probe found for this platform's waifu2x backend.
#[derive(Clone)]
enum Waifu2xAvailability {
    /// Library and model folder are present.
    Available,
    /// The package is not installed (yet): the worker downloads it on first use.
    Missing { user_message: String },
    /// The capability table has no waifu2x on this platform (`PlatformFeature::Waifu2xUpscaler`);
    /// nothing is ever downloaded.
    PlatformUnsupported { user_message: String },
}

#[cfg(not(target_arch = "wasm32"))]
struct Waifu2xSharedRuntime {
    backend: Mutex<Waifu2xBackendProbe>,
    state: Mutex<Option<Waifu2xRuntime>>,
    cancel_handle: Mutex<Option<Waifu2xCancelHandle>>,
    shutdown_requested: AtomicBool,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy)]
struct Waifu2xCancelHandle {
    context: *mut c_void,
    cancel: unsafe extern "C" fn(*mut c_void),
}

#[cfg(not(target_arch = "wasm32"))]
struct Waifu2xRuntime {
    _library: Library,
    api: Waifu2xApi,
    context: *mut c_void,
    loaded_config: Option<LoadedWaifu2xConfig>,
}

// SAFETY: The waifu2x context is never accessed concurrently without external synchronisation.
// `Waifu2xSharedRuntime` serialises load/process through `state: Mutex<_>`, and cross-thread
// cancellation uses the dedicated C API function designed for that purpose.
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for Waifu2xRuntime {}

// SAFETY: The cancel handle is copied between threads only to call the library's thread-safe
// cancellation entry point on the same context while the owning runtime stays alive.
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for Waifu2xCancelHandle {}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LoadedWaifu2xConfig {
    noise: i32,
    scale: u32,
    tile_size: u32,
    gpu_id: i32,
    num_threads: i32,
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
#[repr(C)]
struct Waifu2xImage {
    data: *mut u8,
    width: c_int,
    height: c_int,
    channels: c_int,
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
const WAIFU2X_ABI_VERSION: i32 = 1;

impl Waifu2xController {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Self {
        let backend = probe_waifu2x_backend();
        let runtime = Some(Arc::new(Waifu2xSharedRuntime::new(backend.clone())));
        Self {
            pending: None,
            backend,
            runtime,
        }
    }

    /// Web build: the waifu2x dynamic library backend does not exist, so the probe
    /// reports it unavailable and no FFI runtime is created.
    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        Self {
            pending: None,
            backend: probe_waifu2x_backend(),
        }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn unavailable_reason(&self) -> Option<&str> {
        self.backend.unavailable_reason()
    }

    /// Whether this platform has waifu2x at all (`PlatformFeature::Waifu2xUpscaler`). When
    /// `false`, [`Self::unavailable_reason`] is the localized "not on this platform" text and
    /// a run fails with it without any download.
    pub fn platform_supported(&self) -> bool {
        !matches!(self.backend.availability, Waifu2xAvailability::PlatformUnsupported { .. })
    }

    pub fn backend_path(&self) -> &Path {
        &self.backend.library_path
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn begin(&mut self, images: Vec<Waifu2xInputImage>, options: Waifu2xOptions) {
        let runtime = self
            .runtime
            .get_or_insert_with(|| Arc::new(Waifu2xSharedRuntime::new(self.backend.clone())))
            .clone();
        self.pending = Some(PendingWaifu2x {
            rx: spawn_waifu2x_worker(runtime, images, options),
        });
    }

    /// Web build: there is no waifu2x backend. Enqueue an immediate failure through
    /// the normal channel so `poll` surfaces a clear "unavailable on web" error
    /// instead of silently doing nothing.
    #[cfg(target_arch = "wasm32")]
    pub fn begin(&mut self, _images: Vec<Waifu2xInputImage>, _options: Waifu2xOptions) {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Waifu2xWorkerEvent::Finished(Err(Waifu2xError {
            user_message: t!("launcher.new_project.waifu2x.web_unsupported").to_string(),
            log_message: "waifu2x backend is not available on the web build".to_string(),
        })));
        self.pending = Some(PendingWaifu2x { rx });
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

    #[cfg(not(target_arch = "wasm32"))]
    pub fn shutdown(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.request_shutdown();
        }
        self.pending = None;
    }

    /// Web build: no FFI runtime to tear down; just drop any pending state.
    #[cfg(target_arch = "wasm32")]
    pub fn shutdown(&mut self) {
        self.pending = None;
    }
}

impl Waifu2xBackendProbe {
    #[cfg(not(target_arch = "wasm32"))]
    fn is_available(&self) -> bool {
        matches!(self.availability, Waifu2xAvailability::Available)
    }

    fn unavailable_reason(&self) -> Option<&str> {
        match &self.availability {
            Waifu2xAvailability::Available => None,
            Waifu2xAvailability::Missing { user_message } | Waifu2xAvailability::PlatformUnsupported { user_message } => Some(user_message.as_str()),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn missing_error(&self) -> Waifu2xError {
        Waifu2xError {
            user_message: self
                .unavailable_reason()
                .unwrap_or(t!("launcher.new_project.waifu2x.unavailable_error"))
                .to_string(),
            log_message: format!(
                "waifu2x backend unavailable: library='{}', model_dir='{}'",
                self.library_path.display(),
                self.model_dir.display()
            ),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
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
            user_message: t!("launcher.new_project.waifu2x.failed_error").to_string(),
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
                    user_message: t!("launcher.new_project.waifu2x.start_error").to_string(),
                    log_message: "waifu2x runtime disappeared after creation".to_string(),
                });
            }
        };

        runtime.ensure_loaded(&backend, options)?;

        let mut processed = Vec::with_capacity(total);
        for (index, input) in images.iter().enumerate() {
            if self.shutdown_requested.load(Ordering::SeqCst) {
                return Err(Waifu2xError {
                    user_message: t!("launcher.new_project.waifu2x.stopped_on_close").to_string(),
                    log_message: "waifu2x cancelled because new project window closed".to_string(),
                });
            }
            send_progress(progress_tx, "waifu2x", index + 1, total);
            let output_image = runtime
                .process_image(&input.image)
                .map_err(|err| Waifu2xError {
                    user_message: t!("launcher.new_project.waifu2x.failed_see_logs_error")
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
            user_message: t!("launcher.new_project.waifu2x.failed_error").to_string(),
            log_message: "waifu2x backend mutex poisoned".to_string(),
        })?;
        if backend.is_available() {
            return Ok(backend.clone());
        }
        let package = match waifu2x_package() {
            Ok(package) => package,
            Err(unavailable) => {
                return Err(Waifu2xError {
                    user_message: unavailable.user_message(),
                    log_message: format!("waifu2x is not available on this platform: {unavailable:?}"),
                });
            }
        };

        // Window close sets `shutdown_requested`; it cancels the transfer and the backoff too.
        install_waifu2x_backend(package, &self.shutdown_requested, progress_tx)?;
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

#[cfg(not(target_arch = "wasm32"))]
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
            user_message: t!("launcher.new_project.waifu2x.prepare_model_path_error").to_string(),
            log_message: format!(
                "waifu2x model dir is not valid UTF-8: '{}'",
                backend.model_dir.display()
            ),
        })?;
        let model_dir = CString::new(model_dir_utf8).map_err(|err| Waifu2xError {
            user_message: t!("launcher.new_project.waifu2x.prepare_model_path_error").to_string(),
            log_message: format!(
                "waifu2x model dir contains interior NUL '{}': {err}",
                backend.model_dir.display()
            ),
        })?;
        let scale = i32::try_from(options.scale).map_err(|err| Waifu2xError {
            user_message: t!("launcher.new_project.waifu2x.invalid_scale_error").to_string(),
            log_message: format!(
                "failed to convert waifu2x scale {} to i32: {err}",
                options.scale
            ),
        })?;
        let tile_size = i32::try_from(options.tile_size).map_err(|err| Waifu2xError {
            user_message: t!("launcher.new_project.waifu2x.invalid_tile_error").to_string(),
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
                user_message: t!("launcher.new_project.waifu2x.load_model_error").to_string(),
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
            ms_log::runtime_log::log_warn(format!(
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

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Waifu2xRuntime {
    fn drop(&mut self) {
        // SAFETY: teardown order mirrors the C API contract. Errors are logged and ignored.
        unsafe {
            (self.api.cancel)(self.context);
            (self.api.destroy)(self.context);
            let cleanup_status = (self.api.global_cleanup)();
            if cleanup_status != 0 {
                ms_log::runtime_log::log_warn(format!(
                    "[new-project] waifu2x_global_cleanup returned {cleanup_status}"
                ));
            }
        }
    }
}

/// The pinned waifu2x release package of one platform: where it unpacks and what the
/// download must match. Pins come from the release assets themselves (GitHub's size and
/// `sha256:` digest agree with the hashed bytes).
#[derive(Debug, Clone, Copy)]
// The download fields are read only by the native installer; the web build has no waifu2x.
#[cfg_attr(target_arch = "wasm32", expect(dead_code, reason = "the archive/url/size/sha256 pins are read only by the native installer; the web build has no waifu2x"))]
struct Waifu2xPackage {
    /// Top-level folder of the archive, unpacked as `data_dir()/waifu2x/<dir>`.
    dir: &'static str,
    /// The shared library inside `dir`.
    library: &'static str,
    /// The model folder inside `dir`.
    model_dir: &'static str,
    /// Archive file name (also the staged `.download/<archive>` name).
    archive: &'static str,
    /// Release asset URL.
    url: &'static str,
    /// Exact archive size in bytes.
    size: u64,
    /// sha256 of the archive (64 lowercase hex digits).
    sha256: &'static str,
}

/// The package of this build. Which OS builds have a package at all is decided by
/// `PlatformFeature::Waifu2xUpscaler` (Linux and Windows); this only picks the data.
#[cfg(target_os = "windows")]
const PLATFORM_PACKAGE: Waifu2xPackage = Waifu2xPackage {
    dir: "Win",
    library: "waifu2x-ncnn-vulkan.dll",
    model_dir: "models-cunet",
    archive: "Win.zip",
    url: "https://github.com/Vasyanator/libwaifu2x-ncnn-vulkan/releases/download/1.0/Win.zip",
    size: 36_873_220,
    sha256: "8a087d5f5f35a4eeaa29e95a10741cad1c7890d19c8ddff9a5f0eb78c8844fbb",
};

/// The package of this build (see the Windows twin above).
#[cfg(not(target_os = "windows"))]
const PLATFORM_PACKAGE: Waifu2xPackage = Waifu2xPackage {
    dir: "Lin",
    library: "libwaifu2x-ncnn-vulkan.so",
    model_dir: "models-cunet",
    archive: "Lin.zip",
    url: "https://github.com/Vasyanator/libwaifu2x-ncnn-vulkan/releases/download/1.0/Lin.zip",
    size: 36_131_839,
    sha256: "6379c582c3966173504f2ddc0da00fbb29f41528c12b108fe2691317ded2cc06",
};

/// The waifu2x package of this platform, or the typed "not on this platform" answer of the
/// capability table (macOS, Android and wasm have no package).
fn waifu2x_package() -> Result<&'static Waifu2xPackage, ms_platform::Unavailable> {
    ms_platform::PlatformFeature::Waifu2xUpscaler.require().map(|()| &PLATFORM_PACKAGE)
}

/// `data_dir()/waifu2x`: every package unpacks below it.
fn waifu2x_root() -> PathBuf {
    config::data_dir().join("waifu2x")
}

/// Name of the marker file in the waifu2x root that names the ACTIVE generation folder. It is
/// replaced atomically (temp + fsync + rename), so it always names one complete generation.
const ACTIVE_MARKER: &str = "active";

/// The generation folder of `package`: `gen-<first 16 hex digits of its sha256>`. A new pin
/// unpacks into a new folder, so the current generation is never touched while the next one
/// is being published.
fn generation_name(package: &Waifu2xPackage) -> String {
    let prefix = package.sha256.get(..16).unwrap_or(package.sha256);
    format!("gen-{prefix}")
}

/// Resolves the backend of this platform: unsupported platform, missing library or model
/// folder, or available. Reads the small active-generation marker and probes two paths; no
/// network.
fn probe_waifu2x_backend() -> Waifu2xBackendProbe {
    let root = waifu2x_root();
    let package = match waifu2x_package() {
        Ok(package) => package,
        Err(unavailable) => {
            return Waifu2xBackendProbe {
                library_path: root.clone(),
                model_dir: root,
                availability: Waifu2xAvailability::PlatformUnsupported { user_message: unavailable.user_message() },
            };
        }
    };
    probe_package(&root, package)
}

/// [`probe_waifu2x_backend`] for a supported platform under an explicit `root`: available
/// only when the marker names THIS pin's generation and that generation holds the library
/// and the model folder. A tree without the marker (a legacy unversioned install, or a
/// generation whose switch never completed) is not available and gets reinstalled.
fn probe_package(root: &Path, package: &Waifu2xPackage) -> Waifu2xBackendProbe {
    let generation = generation_name(package);
    let generation_dir = root.join(&generation);
    let library_path = generation_dir.join(package.dir).join(package.library);
    let model_dir = generation_dir.join(package.dir).join(package.model_dir);
    let active = std::fs::read_to_string(root.join(ACTIVE_MARKER)).is_ok_and(|marker| marker.trim() == generation);
    let availability = if !active || !library_path.is_file() {
        Waifu2xAvailability::Missing {
            user_message: tf!("launcher.new_project.waifu2x.library_missing_error", library_path = library_path.display()),
        }
    } else if !model_dir.is_dir() {
        Waifu2xAvailability::Missing {
            user_message: tf!("launcher.new_project.waifu2x.models_dir_missing_error", model_dir = model_dir.display()),
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

/// Downloads the platform package and unpacks it under `waifu2x_root()`.
///
/// The archive is fetched by `ms_download::fetch_verified` against the pinned size and
/// sha256 (retry with backoff, `Range` resume of `.download/<archive>.part` across runs,
/// cancel through `cancel`), then unpacked by [`extract_waifu2x_package`] and deleted.
///
/// # Errors
/// A [`Waifu2xError`] for the download (see [`waifu2x_download_error`]) or the extraction.
#[cfg(not(target_arch = "wasm32"))]
fn install_waifu2x_backend(package: &Waifu2xPackage, cancel: &AtomicBool, progress_tx: &Sender<Waifu2xWorkerEvent>) -> Result<(), Waifu2xError> {
    let root = waifu2x_root();
    let fetcher = UreqFetcher::new(None);
    let retry = RetryPolicy::standard();
    let archive_path = download_waifu2x_package(&fetcher, &retry, cancel, package, &root, progress_tx)?;
    extract_waifu2x_package(&archive_path, &root, &generation_name(package), cancel, progress_tx)?;
    if let Err(err) = fs::remove_file(&archive_path) {
        ms_log::runtime_log::log_warn(format!(
            "[new-project] failed to remove the unpacked waifu2x archive '{}': {err}",
            archive_path.display()
        ));
    }
    Ok(())
}

/// Makes `<root>/.download/<archive>` present and verified, and returns its path.
///
/// An archive left by an earlier run that already matches the pins is kept without a
/// request; a `.part` is resumed. Progress: `download_waifu2x` (bytes staged / total),
/// `verify_waifu2x` (bytes hashed of an existing prefix) and `download_waifu2x_retry`
/// (retry `k` of `n` after a transient network failure).
///
/// # Errors
/// A [`Waifu2xError`] mapped from the `DownloadError` by [`waifu2x_download_error`].
#[cfg(not(target_arch = "wasm32"))]
fn download_waifu2x_package(
    fetcher: &dyn RangeFetcher,
    retry: &RetryPolicy<'_>,
    cancel: &AtomicBool,
    package: &Waifu2xPackage,
    root: &Path,
    progress_tx: &Sender<Waifu2xWorkerEvent>,
) -> Result<PathBuf, Waifu2xError> {
    let download_dir = root.join(".download");
    let archive_path = download_dir.join(package.archive);
    let part_path = download_dir.join(format!("{}.part", package.archive));
    send_progress(progress_tx, "download_waifu2x", 0, 0);
    ms_log::runtime_log::log_info(format!(
        "[new-project] downloading waifu2x backend: url={} dest='{}' size={}",
        package.url,
        archive_path.display(),
        package.size
    ));
    let context = format!("waifu2x package {}", package.archive);
    let engine = ResumableFetch {
        fetcher,
        retry,
        cancel,
        log_tag: "[new-project]",
        log_context: &context,
    };
    let request = FileRequest {
        url: package.url,
        part: &part_path,
        dest: &archive_path,
        expected: Expected::SizeSha256 { size: package.size, sha256: package.sha256 },
    };
    let mut on_event = |event: FileEvent| match event {
        FileEvent::Waiting => send_progress(progress_tx, "download_waifu2x", 0, 0),
        FileEvent::Verifying { done, total } => {
            send_progress(progress_tx, "verify_waifu2x", saturating_usize(done), saturating_usize(total));
        }
        FileEvent::Stream(StreamEvent::Downloading { staged, total }) => {
            send_progress(progress_tx, "download_waifu2x", saturating_usize(staged), total.map_or(0, saturating_usize));
        }
        FileEvent::Stream(StreamEvent::Retrying { attempt, max_attempts, .. }) => {
            send_progress(progress_tx, "download_waifu2x_retry", saturating_usize(u64::from(attempt)), saturating_usize(u64::from(max_attempts)));
        }
    };
    match fetch_verified(&engine, &request, &mut on_event) {
        Ok(_) => {
            ms_log::runtime_log::log_info(format!("[new-project] waifu2x package ready: '{}'", archive_path.display()));
            Ok(archive_path)
        }
        Err(err) => Err(waifu2x_download_error(package.url, &err)),
    }
}

/// Maps a download failure to the user text and a log line with the typed detail.
#[cfg(not(target_arch = "wasm32"))]
fn waifu2x_download_error(url: &str, err: &DownloadError) -> Waifu2xError {
    let user_message = match err {
        DownloadError::Cancelled => t!("launcher.new_project.waifu2x.stopped_on_close"),
        DownloadError::Http { .. } => t!("launcher.new_project.waifu2x.download_error_check_internet"),
        DownloadError::Io { .. } => t!("launcher.new_project.waifu2x.write_archive_error"),
        DownloadError::Busy { .. }
        | DownloadError::InvalidRequest { .. }
        | DownloadError::SizeMismatch { .. }
        | DownloadError::HashMismatch { .. }
        | DownloadError::Archive { .. } => t!("launcher.new_project.waifu2x.download_error"),
    };
    Waifu2xError {
        user_message: user_message.to_string(),
        log_message: format!("waifu2x download failed for '{url}': {err}"),
    }
}

/// Unpacks `archive_path` into `<root>/.staging/`, fsyncing every file and directory, then
/// publishes it as generation `generation` through [`publish_generation`].
///
/// The active generation is never written: a stale `.staging/` is incomplete by definition
/// and is deleted at the next start, and nothing becomes available before the marker
/// switch. Entries escaping the archive root are rejected.
///
/// # Errors
/// A [`Waifu2xError`] for an unreadable archive, an unsafe entry, a filesystem failure, or
/// `cancel` (checked per entry).
#[cfg(not(target_arch = "wasm32"))]
fn extract_waifu2x_package(
    archive_path: &Path,
    root: &Path,
    generation: &str,
    cancel: &AtomicBool,
    progress_tx: &Sender<Waifu2xWorkerEvent>,
) -> Result<(), Waifu2xError> {
    send_progress(progress_tx, "extract_waifu2x", 0, 0);
    let archive_file = File::open(archive_path).map_err(|err| Waifu2xError {
        user_message: t!("launcher.new_project.waifu2x.open_archive_error").to_string(),
        log_message: format!(
            "failed to open waifu2x archive '{}': {err}",
            archive_path.display()
        ),
    })?;
    let mut archive = ZipArchive::new(archive_file).map_err(|err| Waifu2xError {
        user_message: t!("launcher.new_project.waifu2x.read_archive_error").to_string(),
        log_message: format!(
            "failed to parse waifu2x zip archive '{}': {err}",
            archive_path.display()
        ),
    })?;
    let staging = root.join(".staging");
    let folder_error = |path: &Path, err: io::Error| Waifu2xError {
        user_message: t!("launcher.new_project.waifu2x.prepare_folder_error").to_string(),
        log_message: format!("failed to prepare waifu2x folder '{}': {err}", path.display()),
    };
    if staging.exists() {
        // Left by an interrupted extraction: never published, so never complete.
        fs::remove_dir_all(&staging).map_err(|err| folder_error(&staging, err))?;
    }
    fs::create_dir_all(&staging).map_err(|err| folder_error(&staging, err))?;

    let total = archive.len();
    for index in 0..total {
        if cancel.load(Ordering::SeqCst) {
            return Err(Waifu2xError {
                user_message: t!("launcher.new_project.waifu2x.stopped_on_close").to_string(),
                log_message: "waifu2x extraction cancelled because the new project window closed".to_string(),
            });
        }
        let mut entry = archive.by_index(index).map_err(|err| Waifu2xError {
            user_message: t!("launcher.new_project.waifu2x.extract_archive_error").to_string(),
            log_message: format!(
                "failed to access entry {index} in '{}': {err}",
                archive_path.display()
            ),
        })?;
        let Some(relative_path) = entry.enclosed_name() else {
            return Err(Waifu2xError {
                user_message: t!("launcher.new_project.waifu2x.unsafe_path_error").to_string(),
                log_message: format!(
                    "unsafe waifu2x zip entry '{}' in '{}'",
                    entry.name(),
                    archive_path.display()
                ),
            });
        };
        let output_path = staging.join(relative_path);
        if entry.is_dir() {
            fs::create_dir_all(&output_path).map_err(|err| Waifu2xError {
                user_message: t!("launcher.new_project.waifu2x.create_dir_from_archive_error").to_string(),
                log_message: format!(
                    "failed to create waifu2x extracted dir '{}': {err}",
                    output_path.display()
                ),
            })?;
        } else {
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent).map_err(|err| Waifu2xError {
                    user_message: t!("launcher.new_project.waifu2x.create_dir_from_archive_error").to_string(),
                    log_message: format!(
                        "failed to create waifu2x extracted parent '{}': {err}",
                        parent.display()
                    ),
                })?;
            }
            let write_error = |err: io::Error| Waifu2xError {
                user_message: t!("launcher.new_project.waifu2x.write_file_from_archive_error").to_string(),
                log_message: format!(
                    "failed to write waifu2x extracted file '{}': {err}",
                    output_path.display()
                ),
            };
            let mut output = File::create(&output_path).map_err(write_error)?;
            io::copy(&mut entry, &mut output).map_err(|err| Waifu2xError {
                user_message: t!("launcher.new_project.waifu2x.extract_file_error").to_string(),
                log_message: format!(
                    "failed to extract waifu2x file '{}' to '{}': {err}",
                    entry.name(),
                    output_path.display()
                ),
            })?;
            // Durable before the publishing rename below.
            output.sync_all().map_err(write_error)?;
        }
        send_progress(
            progress_tx,
            "extract_waifu2x",
            index.saturating_add(1),
            total,
        );
    }
    sync_tree(&staging).map_err(|err| folder_error(&staging, err))?;
    publish_generation(&staging, root, generation, &|from, to| fs::rename(from, to))
}

/// The rename step of [`publish_generation`], injectable so tests can make it fail.
#[cfg(not(target_arch = "wasm32"))]
type RenameFn<'a> = &'a dyn Fn(&Path, &Path) -> io::Result<()>;

/// Publishes the complete, fsynced `staging` tree as generation `generation` of `root` and
/// makes it the active one, without ever touching the generation that is active now:
///
/// 1. a leftover `<root>/<generation>` (a switch that never reached step 3, or a broken
///    tree of the same pin) is removed;
/// 2. `staging` is renamed to `<root>/<generation>` (one directory rename) and `root` is
///    fsynced;
/// 3. the [`ACTIVE_MARKER`] is replaced atomically (temp + fsync + rename) to name it, and
///    `root` is fsynced: this is the switch;
/// 4. only then other generations and legacy unversioned package folders are removed
///    (failures are logged; they only cost disk space).
///
/// A failure or crash before step 3 leaves the marker naming the previous generation, which
/// stays usable; [`probe_package`] never reports a generation the marker does not name.
///
/// # Errors
/// A [`Waifu2xError`] naming the step that failed (steps 1-3).
#[cfg(not(target_arch = "wasm32"))]
fn publish_generation(staging: &Path, root: &Path, generation: &str, rename: RenameFn<'_>) -> Result<(), Waifu2xError> {
    let publish_error = |path: &Path, err: io::Error| Waifu2xError {
        user_message: t!("launcher.new_project.waifu2x.prepare_folder_error").to_string(),
        log_message: format!("failed to publish the unpacked waifu2x generation at '{}': {err}", path.display()),
    };
    let target = root.join(generation);
    // `symlink_metadata`: a link in place of the generation folder is removed, not followed.
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(&target).map_err(|err| publish_error(&target, err))?,
        Ok(_) => fs::remove_file(&target).map_err(|err| publish_error(&target, err))?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(publish_error(&target, err)),
    }
    rename(staging, &target).map_err(|err| publish_error(&target, err))?;
    sync_dir(root).map_err(|err| publish_error(root, err))?;

    let marker = root.join(ACTIVE_MARKER);
    let marker_tmp = root.join(format!("{ACTIVE_MARKER}.tmp"));
    File::create(&marker_tmp)
        .and_then(|mut file| {
            file.write_all(format!("{generation}\n").as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| rename(&marker_tmp, &marker))
        .and_then(|()| sync_dir(root))
        .map_err(|err| publish_error(&marker, err))?;
    ms_log::runtime_log::log_info(format!("[new-project] waifu2x generation '{generation}' is active in '{}'", root.display()));

    remove_stale_generations(root, generation);
    Ok(())
}

/// Removes every generation folder other than `active` and the legacy unversioned package
/// folders (`Lin`, `Win`, `Mac`) of `root`. Runs only after the switch; failures are logged.
#[cfg(not(target_arch = "wasm32"))]
fn remove_stale_generations(root: &Path, active: &str) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("[new-project] failed to list '{}' for waifu2x cleanup: {err}", root.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[new-project] failed to read an entry of '{}' for waifu2x cleanup: {err}", root.display()));
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let stale = (name.starts_with("gen-") && name != active) || matches!(name, "Lin" | "Win" | "Mac");
        if !stale || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if let Err(err) = fs::remove_dir_all(entry.path()) {
            ms_log::runtime_log::log_warn(format!(
                "[new-project] failed to remove the stale waifu2x folder '{}': {err}",
                entry.path().display()
            ));
        }
    }
}

/// fsyncs every directory of the tree under `dir` (children first), so the entries the
/// extraction created are durable before the tree is published. No-op off unix.
#[cfg(not(target_arch = "wasm32"))]
fn sync_tree(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        }
    }
    sync_dir(dir)
}

/// fsyncs the directory `dir` (unix: makes renames and creations inside it durable).
#[cfg(not(target_arch = "wasm32"))]
fn sync_dir(dir: &Path) -> io::Result<()> {
    // Windows cannot open a directory as a `File`; NTFS journals the rename itself.
    if cfg!(unix) {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn saturating_usize(value: u64) -> usize {
    match usize::try_from(value) {
        Ok(converted) => converted,
        Err(_) => usize::MAX,
    }
}

#[cfg(not(target_arch = "wasm32"))]
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
                ms_log::runtime_log::log_warn("[new-project] failed to send waifu2x result to UI");
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
                "[new-project] failed to spawn waifu2x worker: {err}"
            ));
            if tx
                .send(Waifu2xWorkerEvent::Finished(Err(Waifu2xError {
                    user_message: t!("launcher.new_project.waifu2x.start_error").to_string(),
                    log_message: format!("failed to spawn waifu2x worker: {err}"),
                })))
                .is_err()
            {
                ms_log::runtime_log::log_warn("[new-project] failed to deliver waifu2x spawn error");
            }
        }
    }
    rx
}

#[cfg(not(target_arch = "wasm32"))]
fn run_waifu2x(
    images: Vec<Waifu2xInputImage>,
    options: Waifu2xOptions,
    runtime: &Waifu2xSharedRuntime,
    progress_tx: &Sender<Waifu2xWorkerEvent>,
) -> Result<Waifu2xSuccess, Waifu2xError> {
    if images.is_empty() {
        return Err(Waifu2xError {
            user_message: t!("launcher.new_project.open_or_download_first_error").to_string(),
            log_message: "waifu2x started without input images".to_string(),
        });
    }
    runtime.process_images(&images, options, progress_tx)
}

#[cfg(not(target_arch = "wasm32"))]
fn create_runtime(backend: &Waifu2xBackendProbe) -> Result<Waifu2xRuntime, Waifu2xError> {
    // SAFETY: Loading a shared library is inherently unsafe; the path points to the bundled
    // waifu2x backend resolved by this module.
    let library = unsafe { Library::new(&backend.library_path) }.map_err(|err| Waifu2xError {
        user_message: t!("launcher.new_project.waifu2x.load_library_error").to_string(),
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
            user_message: t!("launcher.new_project.waifu2x.unsupported_library_version_error").to_string(),
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
            user_message: t!("launcher.new_project.waifu2x.init_runtime_error").to_string(),
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
            ms_log::runtime_log::log_warn(format!(
                "[new-project] waifu2x_global_cleanup after create failure returned {cleanup_status}"
            ));
        }
        return Err(Waifu2xError {
            user_message: t!("launcher.new_project.waifu2x.create_context_error").to_string(),
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

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
fn load_symbol<T>(library: &Library, library_path: &Path, symbol: &[u8]) -> Result<T, Waifu2xError>
where
    T: Copy,
{
    // SAFETY: the symbol names are from the companion waifu2x C header and the returned
    // function pointers remain valid while `library` is kept alive inside `Waifu2xRuntime`.
    let symbol_ref = unsafe { library.get::<T>(symbol) }.map_err(|err| Waifu2xError {
        user_message: t!("launcher.new_project.waifu2x.library_corrupt_error").to_string(),
        log_message: format!(
            "failed to resolve symbol '{}' from '{}': {err}",
            String::from_utf8_lossy(symbol).trim_end_matches('\0'),
            library_path.display()
        ),
    })?;
    Ok(*symbol_ref)
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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
            for chunk in bytes.as_chunks::<3>().0 {
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

#[cfg(not(target_arch = "wasm32"))]
fn available_parallelism_i32() -> i32 {
    match thread::available_parallelism() {
        Ok(value) => i32::try_from(value.get()).unwrap_or(i32::MAX),
        Err(_) => 1,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn set_cancel_handle(
    cancel_handle: &Mutex<Option<Waifu2xCancelHandle>>,
    handle: Option<Waifu2xCancelHandle>,
) {
    let Ok(mut slot) = cancel_handle.lock() else {
        ms_log::runtime_log::log_warn("[new-project] failed to lock waifu2x cancel handle");
        return;
    };
    *slot = handle;
}

#[cfg(not(target_arch = "wasm32"))]
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
        ms_log::runtime_log::log_warn("[new-project] failed to send waifu2x progress to UI");
    }
}

/// Synchronous waifu2x entry point for use by the batch executor (already off GUI thread).
/// Returns the resulting `RgbaImage`s or an error string.
///
/// Web build: the dynamic-library waifu2x backend does not exist in the browser, so
/// this returns a clear error instead of processing images.
#[cfg(target_arch = "wasm32")]
pub fn run_waifu2x_sync(
    _images: Vec<Waifu2xInputImage>,
    _options: Waifu2xOptions,
) -> Result<Vec<RgbaImage>, String> {
    Err("waifu2x backend is not available on the web build".to_string())
}

/// Synchronous waifu2x entry point for use by the batch executor (already off GUI thread).
/// Returns the resulting `RgbaImage`s or an error string.
#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use ms_download::test_support::{ScriptedFetcher, Step};
    use sha2::{Digest, Sha256};
    use std::io::{Cursor, Write};
    use std::time::Duration;
    use zip::write::SimpleFileOptions;

    /// Retry schedule without real waits.
    fn instant_retry() -> RetryPolicy<'static> {
        RetryPolicy {
            max_retries: 5,
            first_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            retry_after_cap: Duration::ZERO,
            wait_slice: Duration::from_millis(1),
            sleep: &|_| {},
        }
    }

    /// A stored (uncompressed) zip holding `entries` (name, content).
    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, content) in entries {
            writer.start_file(*name, options).expect("start zip entry");
            writer.write_all(content).expect("write zip entry");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    /// A package pinned to `body` (or to `sha256` when given).
    fn package_for(body: &[u8], sha256: Option<&'static str>) -> Waifu2xPackage {
        // The pin must be `'static` like the production table; leaking one short string per
        // test is harmless.
        let sha256 = sha256.unwrap_or_else(|| Box::leak(format!("{:x}", Sha256::digest(body)).into_boxed_str()));
        Waifu2xPackage {
            dir: "Lin",
            library: "lib.so",
            model_dir: "models-cunet",
            archive: "Lin.zip",
            url: "https://example.invalid/Lin.zip",
            size: u64::try_from(body.len()).expect("test body fits u64"),
            sha256,
        }
    }

    #[test]
    fn platform_package_follows_the_capability_table() {
        let available = ms_platform::PlatformFeature::Waifu2xUpscaler.is_available();
        assert_eq!(waifu2x_package().is_ok(), available);
        assert_eq!(PLATFORM_PACKAGE.sha256.len(), 64);
        assert!(PLATFORM_PACKAGE.sha256.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert!(PLATFORM_PACKAGE.url.starts_with("https://") && PLATFORM_PACKAGE.url.ends_with(PLATFORM_PACKAGE.archive));
        if !available {
            assert!(!probe_waifu2x_backend().is_available());
        }
    }

    #[test]
    fn download_resumes_after_a_drop_and_reports_the_retry() {
        let root = tempfile::tempdir().expect("temp dir");
        let body: Vec<u8> = (0..50_000_u32).map(|i| u8::try_from(i % 251).expect("< 256")).collect();
        let package = package_for(&body, None);
        let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(10_000)]);
        let (tx, rx) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        let archive = download_waifu2x_package(&fetcher, &instant_retry(), &cancel, &package, root.path(), &tx).expect("download");
        assert_eq!(fs::read(&archive).expect("archive"), body);
        assert!(!root.path().join(".download/Lin.zip.part").exists());
        assert_eq!(fetcher.offsets(), vec![0, 10_000]);
        let stages: Vec<&str> = rx.try_iter().filter_map(|event| match event {
            Waifu2xWorkerEvent::Progress { stage, .. } => Some(stage),
            Waifu2xWorkerEvent::Finished(_) => None,
        }).collect();
        assert!(stages.contains(&"download_waifu2x_retry"), "{stages:?}");

        // A second run finds the verified archive and makes no request.
        let idle = ScriptedFetcher::new(&body, &[]);
        download_waifu2x_package(&idle, &instant_retry(), &cancel, &package, root.path(), &tx).expect("second run");
        assert!(idle.offsets().is_empty());
    }

    #[test]
    fn hash_mismatch_fails_and_drops_the_staged_bytes() {
        let root = tempfile::tempdir().expect("temp dir");
        let body = vec![7_u8; 4096];
        let package = package_for(&body, Some("0000000000000000000000000000000000000000000000000000000000000000"));
        let fetcher = ScriptedFetcher::new(&body, &[]);
        let (tx, _rx) = mpsc::channel();
        let err = download_waifu2x_package(&fetcher, &instant_retry(), &AtomicBool::new(false), &package, root.path(), &tx).expect_err("mismatch");
        assert!(err.log_message.contains("sha256 mismatch"), "{}", err.log_message);
        assert!(!root.path().join(".download/Lin.zip").exists());
        assert!(!root.path().join(".download/Lin.zip.part").exists());
    }

    #[test]
    fn cancel_stops_the_download_and_keeps_the_part() {
        let root = tempfile::tempdir().expect("temp dir");
        let body = vec![3_u8; 8192];
        let package = package_for(&body, None);
        // The first attempt drops after 1000 bytes; the cancel set meanwhile ends the backoff.
        let cancel = AtomicBool::new(false);
        let sleep_cancel = |_: Duration| cancel.store(true, Ordering::SeqCst);
        let retry = RetryPolicy { sleep: &sleep_cancel, first_delay: Duration::from_millis(5), max_delay: Duration::from_millis(5), ..instant_retry() };
        let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(1000)]);
        let (tx, _rx) = mpsc::channel();
        let err = download_waifu2x_package(&fetcher, &retry, &cancel, &package, root.path(), &tx).expect_err("cancelled");
        assert!(err.log_message.contains("cancelled"), "{}", err.log_message);
        assert_eq!(fs::metadata(root.path().join(".download/Lin.zip.part")).expect("part kept").len(), 1000);
    }

    const OLD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NEW_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A package archive with the library and one model file.
    fn package_zip(root: &Path) -> PathBuf {
        let archive = root.join(".download").join("pkg.zip");
        fs::create_dir_all(archive.parent().expect("has parent")).expect("download dir");
        fs::write(&archive, zip_bytes(&[("Lin/lib.so", b"library"), ("Lin/models-cunet/model.bin", b"weights")])).expect("write zip");
        archive
    }

    /// Installs `package` as the active generation by hand (the state after a good install).
    fn install_by_hand(root: &Path, package: &Waifu2xPackage) {
        let generation = root.join(generation_name(package));
        fs::create_dir_all(generation.join("Lin/models-cunet")).expect("generation dirs");
        fs::write(generation.join("Lin/lib.so"), b"old library").expect("library");
        fs::write(root.join(ACTIVE_MARKER), format!("{}\n", generation_name(package))).expect("marker");
    }

    #[test]
    fn extraction_publishes_a_new_generation_and_switches_the_marker() {
        let root = tempfile::tempdir().expect("temp dir");
        let old = package_for(b"x", Some(OLD_SHA));
        let new = package_for(b"x", Some(NEW_SHA));
        install_by_hand(root.path(), &old);
        // Leftovers: an interrupted extraction and a legacy unversioned install.
        fs::create_dir_all(root.path().join(".staging/Lin")).expect("stale staging");
        fs::write(root.path().join(".staging/Lin/junk"), b"junk").expect("stale staging file");
        fs::create_dir_all(root.path().join("Lin")).expect("legacy tree");
        let archive = package_zip(root.path());
        let (tx, _rx) = mpsc::channel();
        extract_waifu2x_package(&archive, root.path(), &generation_name(&new), &AtomicBool::new(false), &tx).expect("extract");

        let generation = root.path().join(generation_name(&new));
        assert_eq!(fs::read(generation.join("Lin/lib.so")).expect("library"), b"library");
        assert_eq!(fs::read(generation.join("Lin/models-cunet/model.bin")).expect("model"), b"weights");
        assert!(!generation.join("Lin/junk").exists());
        assert!(probe_package(root.path(), &new).is_available());
        assert!(!probe_package(root.path(), &old).is_available());
        assert!(!root.path().join(generation_name(&old)).exists());
        assert!(!root.path().join("Lin").exists());
        assert!(!root.path().join(".staging").exists());
        assert!(!root.path().join("active.tmp").exists());
    }

    #[test]
    fn a_failed_rename_keeps_the_old_generation_usable_and_the_next_run_repairs() {
        let root = tempfile::tempdir().expect("temp dir");
        let old = package_for(b"x", Some(OLD_SHA));
        let new = package_for(b"x", Some(NEW_SHA));
        install_by_hand(root.path(), &old);
        let (tx, _rx) = mpsc::channel();
        let unpack = |root: &Path| {
            let archive = package_zip(root);
            let staging = root.join(".staging");
            fs::create_dir_all(&staging).expect("staging");
            let mut zip = ZipArchive::new(File::open(&archive).expect("open zip")).expect("read zip");
            zip.extract(&staging).expect("extract to staging");
            staging
        };

        // Step 2 (tree rename) fails: nothing new is visible.
        let staging = unpack(root.path());
        let fail_all = |_: &Path, _: &Path| Err(io::Error::other("injected rename failure"));
        assert!(publish_generation(&staging, root.path(), &generation_name(&new), &fail_all).is_err());
        assert!(probe_package(root.path(), &old).is_available());
        assert!(!probe_package(root.path(), &new).is_available());

        // Step 3 (marker switch) fails: the new tree exists but is not active.
        let fail_marker = |from: &Path, to: &Path| {
            if to.ends_with(ACTIVE_MARKER) { Err(io::Error::other("injected marker failure")) } else { fs::rename(from, to) }
        };
        assert!(publish_generation(&staging, root.path(), &generation_name(&new), &fail_marker).is_err());
        assert!(root.path().join(generation_name(&new)).join("Lin/lib.so").is_file());
        assert!(probe_package(root.path(), &old).is_available());
        assert!(!probe_package(root.path(), &new).is_available());

        // The next run repairs the interrupted switch.
        let archive = package_zip(root.path());
        extract_waifu2x_package(&archive, root.path(), &generation_name(&new), &AtomicBool::new(false), &tx).expect("repair");
        assert!(probe_package(root.path(), &new).is_available());
        assert!(!root.path().join(generation_name(&old)).exists());
    }

    #[test]
    fn an_unmarked_tree_is_never_available() {
        let root = tempfile::tempdir().expect("temp dir");
        let package = package_for(b"x", Some(NEW_SHA));
        install_by_hand(root.path(), &package);
        fs::remove_file(root.path().join(ACTIVE_MARKER)).expect("drop marker");
        assert!(!probe_package(root.path(), &package).is_available());
        fs::write(root.path().join(ACTIVE_MARKER), "gen-somethingelse\n").expect("foreign marker");
        assert!(!probe_package(root.path(), &package).is_available());
    }

    #[test]
    fn extraction_rejects_unsafe_entries_and_publishes_nothing() {
        let root = tempfile::tempdir().expect("temp dir");
        let archive = root.path().join("pkg.zip");
        fs::write(&archive, zip_bytes(&[("Lin/lib.so", b"library"), ("../evil", b"x")])).expect("write zip");
        let (tx, _rx) = mpsc::channel();
        let err = extract_waifu2x_package(&archive, root.path(), "gen-test", &AtomicBool::new(false), &tx).expect_err("unsafe entry");
        assert!(err.log_message.contains("unsafe waifu2x zip entry"), "{}", err.log_message);
        assert!(!root.path().join("gen-test").exists());
        assert!(!root.path().join(ACTIVE_MARKER).exists());
    }

    #[test]
    fn extraction_honours_cancel() {
        let root = tempfile::tempdir().expect("temp dir");
        let archive = root.path().join("pkg.zip");
        fs::write(&archive, zip_bytes(&[("Lin/lib.so", b"library")])).expect("write zip");
        let (tx, _rx) = mpsc::channel();
        let err = extract_waifu2x_package(&archive, root.path(), "gen-test", &AtomicBool::new(true), &tx).expect_err("cancelled");
        assert!(err.log_message.contains("cancelled"), "{}", err.log_message);
        assert!(!root.path().join("gen-test").exists());
    }
}
