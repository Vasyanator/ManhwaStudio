# Module: crates/ms-sysprobe/src

## Purpose
Everything the application needs from the HOST it runs on, and nothing about the
application's own layers: GPU/accelerator capabilities, the app-local Python runtime,
desktop region capture, which AI Python packages are installed, the app-managed AI model
tree on Hugging Face, the Hugging Face credential in the OS secret store, and the
process-global Torch-availability slot. Independent modules with nothing in common except
that each asks the operating system or a remote service a blocking question and returns a
plain answer, with no knowledge of the caller's state.

This is a standalone workspace crate. `src/main.rs` mounts it under the historical module
names with
`pub use ms_sysprobe::{ai_install_probe, gpu_utils, paste_image, python_manager, screen_capture};` and
`pub use ms_sysprobe::{ai_backend_capabilities, ai_models, hf_token};`, so every
application call site keeps writing `crate::gpu_utils::…` and friends.

## Architecture
No shared state and one cross-module call (`ai_install_probe` runs its snippet through
`python_manager`): `lib.rs` is a plain list of `pub mod`. Each
module talks to the OS through one of three mechanisms — a subprocess whose stdout it
parses (`nvidia-smi`, `rocminfo`, `vulkaninfo`, `grim`, `screencapture`), a Win32 API call
(`windows-sys`), or a filesystem probe — and every one of them is a `Result`/`Option`
returning function with no side effect on the caller's state.

Callers own the threading: nothing here spawns a worker for you.

## Files and submodules
- `lib.rs`: crate root. Mounts the modules and the `ms-i18n` macros. Nothing else.
- `ai_backend_capabilities.rs`: one process-global atomic tri-state mirroring whether the
  running AI backend has Torch, so a UI gate is an atomic load rather than an IPC round
  trip. The health-snapshot handler is the only writer; `Unknown` (never probed) is a
  distinct state and must not be rendered as "absent". No I/O at all — it lives here
  because it is the cached answer to a host question the rest of the modules ask directly.
- `ai_models.rs`: the app-managed model catalog (`Vasyanator2/ManhwaStudio_AI_Models`).
  Resolves the files a given model needs, then STREAMS each one straight into a
  caller-supplied model root — no Hugging Face cache blobs, no symlinks — through a temp
  file that is renamed only after a complete, flushed write, so an interrupted download
  never leaves a half file that looks present. Download is serialized by a process mutex.
  Native-only for the network half; on wasm the signatures stay and return a typed
  "unavailable on web" error. The model ROOT is a parameter: this module reads no config.
- `hf_token.rs`: the process-wide Hugging Face access token. Cached value plus free
  get/set/clear, backed by the OS secret store (`keyring`) under its OWN service name
  `"ManhwaStudio Hugging Face"` — never the OCR key entry, which is a different credential
  with a different lifetime. Seeded once at startup by the binary on a worker thread, so
  reads are lock acquisitions rather than OS round trips. Tri-state: `Unknown` (not read
  yet) is distinct from `Missing`. The token value is never logged, never interpolated into
  a message, and never written to any config or settings document.
- `ai_install_probe.rs`: runs one isolated Python snippet through the ACTIVATED app-local
  environment (the same shell path launcher settings use), reports PyTorch / ONNX Runtime
  package and import status, and classifies the machine into `ms_config::AiInstallType`
  (`None` / `Base` / `Full`). Report parsing and classification are target-neutral; only the
  spawn half is `#[cfg(not(wasm32))]`. Edit here for a new probed package or a changed level
  rule; PERSISTING the level is the caller's job (`main.rs`).
- `gpu_utils.rs`: vendor/architecture detection (NVIDIA, AMD, Apple), CUDA/ROCm runtime
  versions, Linux driver + ROCm installation state, ROCm 7.2 LLVM-target support, Windows
  DirectML accelerators, and the WebGPU adapter enumeration whose returned index IS Dawn's
  `device_id` (DXGI on Windows, `vulkaninfo --summary` on Linux, empty on macOS). Also the
  build-aware runtime gates `native_cuda_build_available` / `native_openvino_runtime_available`.
  Edit here for a new accelerator/runtime probe.
- `python_manager.rs`: the only owner of Python interpreter discovery (`installer_files/venv`,
  `venv`, `installer_files/env`, `installer_files/python`), of the configured `Command`
  values that run it, and of the Windows Job Object that kills a spawned Python child with
  the Rust parent. Native-only: the file carries `#![cfg(not(target_arch = "wasm32"))]`.
  Edit here for anything about locating or launching Python.
- `paste_image.rs`: `read_image_from_clipboard()` — one normalized RGBA image out of the
  system clipboard, `arboard` first and then the Wayland/X11 command-line helpers
  (`wl-paste`, `xclip`, `xsel`) where `arboard` cannot serve the offered representation.
  Native-only; the readers are compiled out on wasm. A host probe like `screen_capture`,
  and shared across crates (the typing and translation tabs, the launcher), which is why it
  is not in any one of them. Returns pixels only: spawning the worker and converting into
  egui/`image` types stays with the caller.
- `screen_capture.rs`: `query_virtual_desktop_bounds()` and `capture_screen_rect()` behind
  one RGBA contract, hiding the per-OS differences (Win32 GDI; `grim`/`maim`/`import` plus
  `xrandr`/`hyprctl`/`wlr-randr` on Linux; `screencapture` on macOS). Edit here for a new
  capture backend or a new compositor.

## Contracts and invariants
- Every public function here BLOCKS: it spawns a process, calls a Win32 API, or touches the
  filesystem. None of them may be called from the GUI thread; callers run them on workers
  and cache the result. `screen_capture` is the sharpest case — a full-screen grab.
- A probe that cannot answer returns `false` / `None` / an empty `Vec` / `Err(String)`. It
  must NEVER fabricate a plausible-looking adapter, version, or interpreter path: a missing
  or unparseable tool is "unknown", not "absent" and not "present". `detect_webgpu_adapters`
  returning empty is the reference behaviour.
- Parsing is separated from spawning wherever the output format is non-trivial (e.g.
  `parse_vulkaninfo_devices`), so the format can be unit-tested without the tool installed.
- Python is only DISCOVERED here, never installed. Downloads and dependency installation
  belong to `src/installer/`, which calls into this crate for the discovery contract.
- Crate boundary: dependencies are `ms-log` (diagnostics), `ms-i18n` (user-facing strings),
  `ms-config` (the `AiInstallType` level only), `ms-thread` (the AI probe's worker),
  `image` (the capture contract), `serde`/`serde_json` (probe output), `hf-hub` + `ureq`
  (`ai_models`, native-only), `keyring` (`hf_token`, native-only), and `windows-sys` on
  Windows. No probe may READ OR WRITE a config document, and the crate must NOT gain a
  dependency on the application's upper layers (`project`, `app`, the tabs) or on egui —
  being a GUI-free leaf is what lets it type-check in parallel with the binary.
- The Windows feature list of `windows-sys` is this crate's own business. Building only
  inside the workspace hides a missing feature (cargo unifies features with the binary's
  larger list), so verify with `cargo clippy -p ms-sysprobe --target x86_64-pc-windows-gnu`.

## Editing map
- To add a GPU / accelerator / runtime probe, see `gpu_utils.rs`; keep the "unknown is not
  absent" rule and add a pure parser plus its test for any new text format.
- To change where Python is looked for, or how a Python child is spawned or killed, see
  `python_manager.rs` — it is the single source, and no caller may build its own `Command`.
- To support a new compositor or capture tool, see `screen_capture.rs`.
- To probe another AI package, or to change what makes an install `Base` rather than `Full`,
  see `ai_install_probe.rs` (the snippet and `detect_ai_install_type_from_report`).
- To add a model or change which files one needs, see `ai_models.rs`; a new caller must go
  through it rather than downloading into the model tree itself.
- To change how the Hugging Face token is stored or seeded, see `hf_token.rs` — and keep
  the rule that no message may interpolate the value.
- To let a new consumer reach these modules, no change is needed here: it already sees them
  as `crate::ai_install_probe` / `crate::gpu_utils` / `crate::python_manager` /
  `crate::screen_capture` / `crate::ai_models` / `crate::hf_token` /
  `crate::ai_backend_capabilities` through the `src/main.rs` re-export.
