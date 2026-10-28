/*
File: crates/ms-sdcpp/src/worker/session.rs

Purpose:
The real `SdBackend` of the worker process: loads the package through `ffi::api::SdApi`,
installs the library's global log / progress callbacks, keeps ONE context (the loaded model)
warm while the files and context options stay the same, and runs `generate_image`.

Key items:
- RealBackend     : `SdBackend` over the C API
- RealCancel      : the cross-thread cancel (flag + `sd_cancel_generation` on the live ctx)
- CallbackState   : what the C callbacks see (frame sink, cancel, recent error lines)
- backend_spec() / params_backend_spec() / flash_attention_mode() : option -> C string rules

Notes:
- Context options map as the official CLI does (`examples/common/common.cpp`
  `prepare_backend_assignments`): `te=cpu` is prepended to the `backend` spec for a text
  encoder on the CPU, `*=cpu` is the `params_backend` spec for weights kept in RAM;
  `eager_load` and `auto_fit` are on, `max_vram` is a GiB string, `flash_attn_mode` is
  `auto` / `on` / `off`.
- Every C string handed to the library lives as long as the call (images, prompts) or as long
  as the context (paths, specs), and every result is freed by the library's own
  `free_sd_image_generation_result`.
- The callbacks run inside `catch_unwind` (a panic must not unwind into C). They may run on
  library threads; their state is `Sync`.
- The library prints only through the log callback once it is installed; anything else goes
  to fd 1, which `stdio` redirected to stderr.
*/

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem::MaybeUninit;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::catalog::{SdCppBuild, row_here};
use crate::error::SdCppError;
use crate::families::ctx_paths;
use crate::ffi::api::SdApi;
use crate::ffi::types::{SampleMethod, Scheduler, SdCancelMode, SdCtx, SdCtxParams, SdImage, SdImageGenerationResult, SdImgGenParams, SdLogLevel};
use crate::protocol::{Event, LogLevel};
use crate::request::{ContextOptions, DeviceSelection, FlashAttention, GenerateRequest, ModelFiles, ProbeReport, RgbImage, RunStage};

use super::{Canceller, EventSink, SdBackend, stdio};

/// How many recent error / warning lines are kept for failure details.
const RECENT_ERRORS: usize = 20;

/// A context pointer that may cross to the reader thread for `sd_cancel_generation`.
struct CtxHandle(*mut SdCtx);

// SAFETY: the pointer is only dereferenced by the library; `sd_cancel_generation` is the one
// call made from another thread, and it is designed for exactly that (it sets an atomic flag).
// The handle is published only while the owning thread is inside `generate_image` and is
// cleared under the same mutex before the context can be freed.
unsafe impl Send for CtxHandle {}

/// The cross-thread cancel of the real backend.
pub(crate) struct RealCancel {
    flag: AtomicBool,
    active: Mutex<Option<CtxHandle>>,
    cancel_fn: OnceLock<unsafe extern "C" fn(*mut SdCtx, SdCancelMode)>,
}

impl RealCancel {
    fn new() -> Self {
        Self { flag: AtomicBool::new(false), active: Mutex::new(None), cancel_fn: OnceLock::new() }
    }

    /// Calls `sd_cancel_generation(ctx, ALL)` on the context being generated, if any.
    fn signal_library(&self) {
        // A poisoned lock still holds a consistent `Option`.
        let active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if let (Some(handle), Some(cancel)) = (active.as_ref(), self.cancel_fn.get()) {
            // SAFETY: `handle.0` is the live context of the running `generate_image` (it is
            // cleared under this lock before the context is freed); the C function only sets
            // the context's cancel flag.
            unsafe { cancel(handle.0, SdCancelMode::ALL) };
        }
    }

    fn set_active(&self, ctx: Option<*mut SdCtx>) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = ctx.map(CtxHandle);
    }
}

impl Canceller for RealCancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.signal_library();
    }

    fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// What the C callbacks see through their `data` pointer. Leaked once per process: the
/// library keeps the pointer for as long as it lives.
struct CallbackState {
    sink: EventSink,
    cancel: Arc<RealCancel>,
    recent_errors: Mutex<VecDeque<String>>,
    /// Steps are forwarded only while sampling (model loading reports progress too).
    generating: AtomicBool,
}

impl CallbackState {
    fn take_recent_errors(&self) -> String {
        let mut recent = self.recent_errors.lock().unwrap_or_else(PoisonError::into_inner);
        let text = recent.iter().cloned().collect::<Vec<_>>().join("\n");
        recent.clear();
        text
    }
}

/// `sd_log_cb_t`: forwards INFO and above as `Log` frames, keeps recent warnings / errors.
unsafe extern "C" fn log_callback(level: SdLogLevel, text: *const c_char, data: *mut c_void) {
    let result = std::panic::catch_unwind(|| {
        if data.is_null() || text.is_null() {
            return;
        }
        // SAFETY: `data` is the leaked `CallbackState` installed by `RealBackend::init`.
        let state = unsafe { &*data.cast::<CallbackState>() };
        // SAFETY: the library passes a NUL-terminated message valid for the call.
        let line = unsafe { CStr::from_ptr(text) }.to_string_lossy().trim_end().to_owned();
        let level = if level.0 >= SdLogLevel::ERROR.0 {
            LogLevel::Error
        } else if level.0 >= SdLogLevel::WARN.0 {
            LogLevel::Warn
        } else if level.0 >= SdLogLevel::INFO.0 {
            LogLevel::Info
        } else {
            return;
        };
        if level != LogLevel::Info {
            let mut recent = state.recent_errors.lock().unwrap_or_else(PoisonError::into_inner);
            if recent.len() == RECENT_ERRORS {
                recent.pop_front();
            }
            recent.push_back(line.clone());
        }
        if let Err(err) = state.sink.send(&Event::Log { level, text: line }, &[]) {
            eprintln!("[sdcpp-worker] cannot forward a log line: {err}");
        }
    });
    if result.is_err() {
        eprintln!("[sdcpp-worker] the log callback panicked");
    }
}

/// `sd_progress_cb_t`: forwards sampling steps; re-issues a pending cancel (a cancel sent
/// before the library reset its own flag at the start of sampling would otherwise be lost).
unsafe extern "C" fn progress_callback(step: c_int, steps: c_int, _time: f32, data: *mut c_void) {
    let result = std::panic::catch_unwind(|| {
        if data.is_null() {
            return;
        }
        // SAFETY: `data` is the leaked `CallbackState` installed by `RealBackend::init`.
        let state = unsafe { &*data.cast::<CallbackState>() };
        if state.cancel.is_cancelled() {
            state.cancel.signal_library();
        }
        if !state.generating.load(Ordering::SeqCst) {
            return;
        }
        let (Ok(step), Ok(steps)) = (u32::try_from(step), u32::try_from(steps)) else {
            return;
        };
        if let Err(err) = state.sink.send(&Event::Step { step, steps }, &[]) {
            eprintln!("[sdcpp-worker] cannot forward a step: {err}");
        }
    });
    if result.is_err() {
        eprintln!("[sdcpp-worker] the progress callback panicked");
    }
}

/// The `backend` spec of `context` (the CLI's `--backend` after `--clip-on-cpu`).
pub(crate) fn backend_spec(context: &ContextOptions) -> String {
    let device = match &context.device {
        DeviceSelection::Auto => String::new(),
        DeviceSelection::Named { name, .. } => name.clone(),
    };
    match (context.text_encoder_on_cpu, device.is_empty()) {
        (true, true) => "te=cpu".to_owned(),
        (true, false) => format!("te=cpu,{device}"),
        (false, _) => device,
    }
}

/// The `params_backend` spec of `context` (the CLI's `--offload-to-cpu`).
pub(crate) fn params_backend_spec(context: &ContextOptions) -> String {
    if context.weights_in_ram { "*=cpu".to_owned() } else { String::new() }
}

/// The `flash_attn_mode` string of `mode`.
pub(crate) const fn flash_attention_mode(mode: FlashAttention) -> &'static str {
    match mode {
        FlashAttention::Auto => "auto",
        FlashAttention::On => "on",
        FlashAttention::Off => "off",
    }
}

/// A C string of `text`, or a typed error naming `what` (an interior NUL).
fn c_string(text: &str, what: &str) -> Result<CString, SdCppError> {
    CString::new(text).map_err(|_| SdCppError::Protocol { detail: format!("{what} contains a NUL byte") })
}

/// A C path string (bytes on Unix, UTF-8 elsewhere — the library converts with `u8path`).
fn c_path(path: &Path) -> Result<CString, SdCppError> {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = path.to_str().ok_or_else(|| SdCppError::Protocol { detail: format!("path is not UTF-8: {}", path.display()) })?.as_bytes().to_vec();
    CString::new(bytes).map_err(|_| SdCppError::Protocol { detail: format!("path contains a NUL byte: {}", path.display()) })
}

/// The live context and the C strings it may still point at.
struct LoadedCtx {
    key: (ModelFiles, ContextOptions),
    ptr: *mut SdCtx,
    _strings: Vec<CString>,
}

/// The real backend.
pub(crate) struct RealBackend {
    api: Option<SdApi>,
    ctx: Option<LoadedCtx>,
    cancel: Arc<RealCancel>,
    callbacks: Option<&'static CallbackState>,
}

impl RealBackend {
    pub(crate) fn new() -> Self {
        Self { api: None, ctx: None, cancel: Arc::new(RealCancel::new()), callbacks: None }
    }

    fn api(&self) -> Result<&SdApi, SdCppError> {
        self.api.as_ref().ok_or_else(|| SdCppError::Protocol { detail: "the library is not loaded".to_owned() })
    }

    /// Installs the global callbacks once, writing through `sink`.
    fn install_callbacks(&mut self, sink: &EventSink) -> Result<&'static CallbackState, SdCppError> {
        if let Some(state) = self.callbacks {
            return Ok(state);
        }
        let state: &'static CallbackState = Box::leak(Box::new(CallbackState { sink: sink.clone(), cancel: self.cancel.clone(), recent_errors: Mutex::new(VecDeque::new()), generating: AtomicBool::new(false) }));
        let api = self.api()?;
        let data = std::ptr::from_ref(state).cast_mut().cast::<c_void>();
        // SAFETY: both callbacks match `sd_log_cb_t` / `sd_progress_cb_t`; `data` points at a
        // leaked `CallbackState` that lives for the rest of the process.
        unsafe {
            (api.set_log_callback)(Some(log_callback), data);
            (api.set_progress_callback)(Some(progress_callback), data);
        }
        self.callbacks = Some(state);
        Ok(state)
    }

    /// Frees the live context, if any.
    fn free_ctx(&mut self) {
        if let (Some(loaded), Some(api)) = (self.ctx.take(), self.api.as_ref()) {
            // SAFETY: `loaded.ptr` came from `new_sd_ctx` and is freed exactly once here; no
            // generation runs (this thread is the only user) and the cancel handle is cleared.
            unsafe { (api.free_ctx)(loaded.ptr) };
        }
    }

    /// Creates the context for `files` / `context` (freeing a different one first).
    fn ensure_ctx(&mut self, files: &ModelFiles, context: &ContextOptions, state: &CallbackState) -> Result<*mut SdCtx, SdCppError> {
        let key = (files.clone(), context.clone());
        if let Some(loaded) = &self.ctx
            && loaded.key == key
        {
            return Ok(loaded.ptr);
        }
        self.free_ctx();
        state.sink.send(&Event::Stage { stage: RunStage::LoadingModel }, &[]).map_err(|detail| SdCppError::Protocol { detail })?;
        let paths = ctx_paths(files);
        let mut strings = Vec::new();
        let mut keep = |value: CString| {
            let ptr = value.as_ptr();
            strings.push(value);
            ptr
        };
        let diffusion = paths.diffusion_model.as_deref().map(c_path).transpose()?.map(&mut keep);
        let vae = paths.vae.as_deref().map(c_path).transpose()?.map(&mut keep);
        let llm = paths.llm.as_deref().map(c_path).transpose()?.map(&mut keep);
        let backend = keep(c_string(&backend_spec(context), "backend spec")?);
        let params_backend = keep(c_string(&params_backend_spec(context), "params backend spec")?);
        let flash = keep(c_string(flash_attention_mode(context.flash_attention), "flash attention mode")?);
        let max_vram = context.max_vram_gib.map(|gib| c_string(&format!("{gib}"), "max vram")).transpose()?.map(&mut keep);

        let api = self.api()?;
        let mut params = MaybeUninit::<SdCtxParams>::uninit();
        // SAFETY: `sd_ctx_params_init` fully initializes the struct it is given.
        let mut params = unsafe {
            (api.ctx_params_init)(params.as_mut_ptr());
            params.assume_init()
        };
        params.diffusion_model_path = diffusion.unwrap_or(std::ptr::null());
        params.vae_path = vae.unwrap_or(std::ptr::null());
        params.llm_path = llm.unwrap_or(std::ptr::null());
        params.backend = backend;
        params.params_backend = params_backend;
        params.flash_attn_mode = flash;
        params.max_vram = max_vram.unwrap_or(std::ptr::null());
        params.eager_load = true;
        params.auto_fit = true;
        if let Some(threads) = context.threads.and_then(|threads| c_int::try_from(threads).ok()).filter(|threads| *threads > 0) {
            params.n_threads = threads;
        }
        state.take_recent_errors();
        // SAFETY: `params` was initialized by the library and every pointer in it is a live
        // C string held in `strings` (kept with the context below) or null.
        let ptr = unsafe { (api.new_ctx)(&raw const params) };
        if ptr.is_null() {
            return Err(SdCppError::ContextCreate { detail: state.take_recent_errors() });
        }
        self.ctx = Some(LoadedCtx { key, ptr, _strings: strings });
        Ok(ptr)
    }
}

/// A borrowed `sd_image_t` over `pixels` (the library only reads input images).
fn sd_image(width: u32, height: u32, channels: u32, pixels: &[u8]) -> SdImage {
    SdImage { width, height, channel: channels, data: pixels.as_ptr().cast_mut() }
}

/// An empty `sd_image_t` (no image).
const fn no_image() -> SdImage {
    SdImage { width: 0, height: 0, channel: 0, data: std::ptr::null_mut() }
}

impl SdBackend for RealBackend {
    fn init(&mut self, package_dir: &Path, build: SdCppBuild) -> Result<(String, String), SdCppError> {
        if let Some(api) = &self.api {
            return Ok((api.version(), api.commit()));
        }
        let row = row_here(build).ok_or(SdCppError::NoBuildHere { build })?;
        stdio::prepare_library_dir(package_dir).map_err(|err| SdCppError::LibraryLoad { problem: crate::error::LoadProblem::Other, detail: format!("{}: {err}", package_dir.display()) })?;
        // SAFETY: this is the worker process (the only caller of `serve` with a
        // `RealBackend`), the package was installed with verified pins, and the library is
        // loaded at most once (guarded by `self.api` above).
        let api = unsafe { SdApi::load(package_dir, row) }?;
        if self.cancel.cancel_fn.set(api.cancel_generation).is_err() {
            eprintln!("[sdcpp-worker] the cancel entry point was already set");
        }
        let versions = (api.version(), api.commit());
        self.api = Some(api);
        Ok(versions)
    }

    fn probe(&mut self) -> Result<ProbeReport, SdCppError> {
        let api = self.api()?;
        Ok(ProbeReport { sd_version: api.version(), sd_commit: api.commit(), devices: api.list_devices(), physical_cores: api.physical_cores() })
    }

    fn generate(&mut self, files: &ModelFiles, context: &ContextOptions, request: &GenerateRequest, sink: &EventSink) -> Result<RgbImage, SdCppError> {
        let state = self.install_callbacks(sink)?;
        if let DeviceSelection::Named { name, .. } = &context.device {
            let devices = self.api()?.list_devices();
            if !devices.iter().any(|device| device.name.eq_ignore_ascii_case(name)) {
                return Err(SdCppError::DeviceNotFound { name: name.clone() });
            }
        }
        let ctx = self.ensure_ctx(files, context, state)?;
        if self.cancel.is_cancelled() {
            return Err(SdCppError::GenerationCancelled);
        }
        let api = self.api()?;
        let prompt = c_string(&request.prompt, "prompt")?;
        let negative = c_string(&request.negative_prompt, "negative prompt")?;
        let sampler_name = request.sample_method.as_deref().map(|name| c_string(name, "sampler")).transpose()?;
        let scheduler_name = request.scheduler.as_deref().map(|name| c_string(name, "scheduler")).transpose()?;
        let to_int = |value: u32, what: &str| c_int::try_from(value).map_err(|_| SdCppError::Protocol { detail: format!("{what} {value} is out of range") });

        // SAFETY (every library call in this block): `ctx` is the live context created by
        // `ensure_ctx`; the parameter structs are initialized by the library's own init
        // functions; every pointer stored in them (prompts, image buffers, reference array)
        // refers to a value that outlives the `generate_image` call.
        unsafe {
            let sample_method = match &sampler_name {
                Some(name) => {
                    let method = (api.str_to_sample_method)(name.as_ptr());
                    if method == SampleMethod::COUNT {
                        return Err(SdCppError::Generation { detail: format!("unknown sampler {:?}", request.sample_method) });
                    }
                    method
                }
                None => (api.default_sample_method)(ctx),
            };
            let scheduler = match &scheduler_name {
                Some(name) => {
                    let scheduler = (api.str_to_scheduler)(name.as_ptr());
                    if scheduler == Scheduler::COUNT {
                        return Err(SdCppError::Generation { detail: format!("unknown scheduler {:?}", request.scheduler) });
                    }
                    scheduler
                }
                None => (api.default_scheduler)(ctx, sample_method),
            };

            let mut params = MaybeUninit::<SdImgGenParams>::uninit();
            (api.img_gen_params_init)(params.as_mut_ptr());
            let mut params = params.assume_init();
            params.prompt = prompt.as_ptr();
            params.negative_prompt = negative.as_ptr();
            params.width = to_int(request.width, "width")?;
            params.height = to_int(request.height, "height")?;
            params.strength = request.strength;
            params.seed = request.seed;
            params.batch_count = 1;
            params.sample_params.sample_steps = to_int(request.steps, "steps")?;
            params.sample_params.guidance.txt_cfg = request.cfg_scale;
            params.sample_params.sample_method = sample_method;
            params.sample_params.scheduler = scheduler;
            params.vae_tiling_params.enabled = request.vae_tiling;
            params.init_image = request.init_image.as_ref().map_or_else(no_image, |image| sd_image(image.width(), image.height(), 3, image.pixels()));
            params.mask_image = request.mask.as_ref().map_or_else(no_image, |mask| sd_image(mask.width(), mask.height(), 1, mask.pixels()));
            let mut references: Vec<SdImage> = request.references.iter().map(|image| sd_image(image.width(), image.height(), 3, image.pixels())).collect();
            if !references.is_empty() {
                params.ref_images = references.as_mut_ptr();
                params.ref_images_count = c_int::try_from(references.len()).map_err(|_| SdCppError::Protocol { detail: "too many reference images".to_owned() })?;
            }

            state.sink.send(&Event::Stage { stage: RunStage::Generating }, &[]).map_err(|detail| SdCppError::Protocol { detail })?;
            state.take_recent_errors();
            state.generating.store(true, Ordering::SeqCst);
            self.cancel.set_active(Some(ctx));
            if self.cancel.is_cancelled() {
                self.cancel.signal_library();
            }
            let mut result: *mut SdImageGenerationResult = std::ptr::null_mut();
            let ok = (api.generate_image)(ctx, &raw const params, &raw mut result);
            self.cancel.set_active(None);
            state.generating.store(false, Ordering::SeqCst);
            drop(references);

            let outcome = if self.cancel.is_cancelled() {
                Err(SdCppError::GenerationCancelled)
            } else if !ok || result.is_null() || (*result).count < 1 || (*result).items.is_null() {
                Err(SdCppError::Generation { detail: state.take_recent_errors() })
            } else {
                copy_rgb(&(*(*result).items).image)
            };
            if !result.is_null() {
                (api.free_image_generation_result)(result);
            }
            outcome
        }
    }

    fn unload(&mut self) {
        self.free_ctx();
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.cancel.clone()
    }
}

/// Copies a library-owned result image into an `RgbImage` (RGBA drops alpha).
///
/// # Safety
/// `image.data` must be null or point to `width * height * channel` readable bytes.
unsafe fn copy_rgb(image: &SdImage) -> Result<RgbImage, SdCppError> {
    let bad = |detail: String| SdCppError::Generation { detail };
    let pixels = usize::try_from(u64::from(image.width) * u64::from(image.height)).map_err(|_| bad("result too large".to_owned()))?;
    let channels = usize::try_from(image.channel).map_err(|_| bad("bad channel count".to_owned()))?;
    if image.data.is_null() || !(channels == 3 || channels == 4) {
        return Err(bad(format!("unexpected result image ({} channels)", image.channel)));
    }
    let len = pixels.checked_mul(channels).ok_or_else(|| bad("result too large".to_owned()))?;
    // SAFETY: per the caller's contract `data` holds `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(image.data, len) };
    let rgb = if channels == 3 { bytes.to_vec() } else { bytes.as_chunks::<4>().0.iter().flat_map(|px| [px[0], px[1], px[2]]).collect() };
    RgbImage::new(image.width, image.height, rgb).map_err(|err| bad(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(device: DeviceSelection, te_cpu: bool, ram: bool) -> ContextOptions {
        ContextOptions { device, text_encoder_on_cpu: te_cpu, weights_in_ram: ram, max_vram_gib: None, flash_attention: FlashAttention::Auto, threads: None }
    }

    #[test]
    fn option_specs_follow_the_cli_rules() {
        let named = DeviceSelection::Named { name: "Vulkan1".into(), description: String::new() };
        assert_eq!(backend_spec(&context(named.clone(), true, false)), "te=cpu,Vulkan1");
        assert_eq!(backend_spec(&context(named.clone(), false, false)), "Vulkan1");
        assert_eq!(backend_spec(&context(DeviceSelection::Auto, true, false)), "te=cpu");
        assert_eq!(backend_spec(&context(DeviceSelection::Auto, false, false)), "");
        assert_eq!(params_backend_spec(&context(named, false, true)), "*=cpu");
        assert_eq!(params_backend_spec(&context(DeviceSelection::Auto, false, false)), "");
        assert_eq!([FlashAttention::Auto, FlashAttention::On, FlashAttention::Off].map(flash_attention_mode), ["auto", "on", "off"]);
    }

    #[test]
    fn result_copy_drops_alpha_and_refuses_bad_shapes() {
        let rgba = [1_u8, 2, 3, 255, 4, 5, 6, 255];
        let mut data = rgba;
        let image = SdImage { width: 2, height: 1, channel: 4, data: data.as_mut_ptr() };
        // SAFETY: `data` holds 2 * 1 * 4 bytes.
        let rgb = unsafe { copy_rgb(&image) }.expect("rgba");
        assert_eq!(rgb.pixels(), &[1, 2, 3, 4, 5, 6]);
        let gray = SdImage { width: 2, height: 1, channel: 1, data: data.as_mut_ptr() };
        // SAFETY: as above (fewer bytes are needed).
        assert!(unsafe { copy_rgb(&gray) }.is_err());
        // SAFETY: a null pointer is refused before any read.
        assert!(unsafe { copy_rgb(&no_image()) }.is_err());
    }
}
