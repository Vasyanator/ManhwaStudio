/*
File: crates/ms-sdcpp/src/lib.rs

Purpose:
Crate root of `ms-sdcpp`, the app's Stable-Diffusion.cpp (sd.cpp) runtime: which pinned
release build runs on this machine, how it is installed and removed, whether the host meets
its requirements, the process-global "is sd.cpp ready" answer, and the model-family-agnostic
request types every consumer (FLUX.2 klein, FLUX.3 klein) builds on.

Modules:
- catalog        : the pinned release (`PINNED`), `SdCppBuild`, one `PackageRow` per OS x build
- install        : the package directory, install / remove / is-installed over `ms-native-package`
- platform_check : glibc / VC++ runtime / Vulkan loader probes (`check_requirements`)
- status         : the process-global `SdCppStatus` snapshot and its refresh worker
- error          : `SdCppError` (localized `Display`), `PackageFailure`, `LoadProblem`
- request        : images, model files, context options, generate request / job, run events,
                   device and probe reports
- families       : family -> context paths, required roles, text conditioning, and the
                   pre-flight `validate_model_files` (DiT <-> encoder pairing)
- model_probe    : bounded GGUF / safetensors / index / config.json header readers
- protocol       : the app <-> worker wire (`WORKER_PROTOCOL`, requests, events, frames)
- ffi (private)  : `#[repr(C)]` mirrors of the vendored header and the library loader
- worker         : the `--sdcpp-worker` process (`run_worker_main`) that runs the library
- client         : the app side: `service()` (generate / probe / unload), worker launch

Notes:
GUI-free; every probe, install and refresh is blocking I/O for a worker thread. Native-only:
compiled out on wasm (like `ms-download`). It does not depend on `ms-local-ai`: the `--no-ai`
gate belongs to the consumers (`AiRouteKind::SdCpp.availability`). The library is never loaded
into the app process: the worker process (`--sdcpp-worker`, `worker`) does that.
*/

#![cfg(not(target_arch = "wasm32"))]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]
// The crate's public names (`SdCppError`, `SdCppBuild`, …) intentionally repeat the crate's
// subject: they are re-exported and read at call sites without the module path.
#![allow(clippy::module_name_repetitions)]

// `t!` / `tf!` for the localized error, requirement and build texts.
#[macro_use]
extern crate ms_i18n;

pub mod catalog;
pub mod client;
pub mod error;
pub mod families;
mod ffi;
pub mod install;
pub mod model_probe;
pub mod platform_check;
pub mod protocol;
pub mod request;
pub mod status;
pub mod worker;

pub use catalog::{PINNED, PackageRow, PinnedRelease, SdCppBuild, build_from_choice, builds_here, default_build_here, offers_a_build_here, row_here};
pub use error::{LoadProblem, PackageFailure, SdCppError};
pub use families::{CtxPaths, ModelFilesError, TextConditioning, ctx_paths, text_conditioning, validate_model_files};
pub use install::{install, is_installed, package_dir, remove, set_before_remove_hook};
pub use platform_check::{RequirementProblem, VC_REDIST_URL, check_requirements};
pub use request::{
    ComponentRole, ContextOptions, DeviceInfo, DeviceSelection, FlashAttention, GenerateJob, GenerateRequest, GrayImage, ImageError, ModelFamily, ModelFiles, ProbeReport, RgbImage, RunEvent,
    RunStage,
};
pub use client::{SdCppService, service};
pub use status::{SdCppStatus, publish_probe, publish_worker_running, ready, request_refresh, snapshot};
