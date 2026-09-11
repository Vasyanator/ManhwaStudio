/*
File: crates/ms-sysprobe/src/lib.rs

Purpose:
Crate root of `ms-sysprobe` — the host-environment probes the application asks about
the machine it runs on: GPU/accelerator detection, the app-local Python runtime,
desktop region capture, and which AI Python packages are installed. GUI-free and
application-free: nothing here reads a config document, project data, or egui.

Submodules:
- `ai_backend_capabilities`: process-global slot mirroring whether the AI backend has Torch.
- `ai_install_probe`: which AI Python packages are installed, classified into the persisted
  `ms_config::AiInstallType` level.
- `ai_models`: lazy, resumable download of the app-managed model tree from the
  `Vasyanator2/ManhwaStudio_AI_Models` Hugging Face repository into a caller-supplied root.
- `hf_token`: the process-wide Hugging Face access token, cached in memory and backed by the
  OS secret store (`keyring`).
- `gpu_utils`: NVIDIA/AMD/DirectML/WebGPU adapter detection, CUDA/ROCm runtime versions,
  Linux driver state.
- `python_manager`: discovery of the app-local Python interpreter and construction of the
  configured `Command` values that run it. Native-only (`#![cfg(not(wasm32))]` in the file).
- `screen_capture`: virtual-desktop bounds and blocking region capture into an `RgbaImage`.

Contract:
- Every probe here performs blocking OS calls (subprocesses, GDI, filesystem) and must be
  called from a worker thread, never from the GUI thread.
- No dependency on the application crate. Diagnostics go to `ms_log::runtime_log`; the
  user-facing strings go through the `ms-i18n` macros mounted below.
- No probe reads or writes a config DOCUMENT. `ai_install_probe` names
  `ms_config::AiInstallType` only because classifying a machine into that persisted level is
  its whole output; persisting it stays with the caller (`main.rs`).
*/

#![warn(clippy::all)]

// User-facing failure strings (`t!` / `tf!`) from the shared catalog. The macros are
// `#[macro_export]`ed and expand through `$crate::`, so one crate-root `#[macro_use]`
// covers the whole crate — the same line `src/main.rs` uses for the binary.
#[macro_use]
extern crate ms_i18n;

pub mod ai_backend_capabilities;
pub mod ai_install_probe;
pub mod ai_models;
pub mod hf_token;
pub mod gpu_utils;
pub mod python_manager;
pub mod paste_image;
pub mod screen_capture;
