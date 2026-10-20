# Module: crates/ms-installer/src/torch

## Purpose
The installer's PyTorch seam: the pin sets (PyPI / pytorch.org for CPU and CUDA, AMD whl-next
for ROCm), the wheel options offered on this machine, the install command and stage, the
recorded choice (`General.torch_flavour` / `torch_rocm_gfx`), the constraints file that keeps
the installed torch, and the "installed PyTorch matches the record" predicate. Consumed by the
installer window (`install.rs`), the update window and continuation (`update.rs`, `utils.rs`),
the repair worker and readiness check (`utils.rs`, `venv_check.rs`) and the launcher settings
page (`ms-launcher` `pages/settings_page.rs`).

## Architecture
```text
preflight::detect_torch_preflight   (nvidia probes + ms_sysprobe::amd_adapters)
    -> build_torch_options(GpuFacts) (pure) -> catalog builders
    -> TorchPreflightResult::{Skip, Choose(TorchChoicePrompt)}
    -> screen keeps TorchChoiceState::new(prompt); choice_ui::draw_torch_choice each frame
       -> TorchInstallSelection::{SkipCpu, InstallGpu(cuda), InstallRocm{option, gfx}}
    -> stage::install_torch_stage:
         freeze -> command::amd_packages_to_remove -> utils uninstall
         -> command::torch_install_pip_args -> utils install (retry)
         -> freeze -> choice_store::persist_torch_choice_record -> returns snapshot
    -> caller: constraints::write_torch_constraints(snapshot) -> `-c` on every later PyPI install
readiness::torch_readiness(freeze, choice_store::read_torch_choice_record) -> TorchReadiness
```
The runner (`uv pip` / `python -m pip`, install and uninstall), the retry loop, the progress
helpers and the torch-dependent extras list stay in `utils.rs`; this directory calls them
(`pub(crate)`). The flavour / gfx vocabulary and the record codec are `ms_config::torch_choice`;
AMD adapter and gfx detection is `ms_sysprobe::amd_adapters`.

## Files and submodules
- `mod.rs`: the choice data model (`TorchOs`, `TorchWheelOption`, `RocmOptionInfo`,
  `DriverNote`, `TorchChoicePrompt`, `TorchPreflightResult`, `TorchInstallSelection` with
  `for_option` / `record`) and the public re-exports.
- `choice_ui.rs`: the one PyTorch choice widget (`TorchChoiceState`, `draw_torch_choice`) drawn
  by the installer window, the update window and the launcher settings page: summary, one radio
  row per option (recommended marker, size, ROCm architecture picker with detected
  architectures first, the gfx10 "experimental" note, the driver note), the CPU row and one
  confirm button.
- `catalog.rs`: every pin and index URL, the per-OS ROCm device-extra list, the ROCm download
  estimate table, the minimum Windows driver, the option builders and the recommendation rule.
- `preflight.rs`: `build_torch_options` (pure) and `detect_torch_preflight` (blocking probes;
  worker thread only).
- `command.rs`: selection -> exact `pip install` arguments (validated, `TorchCommandError`) and
  the AMD uninstall set.
- `stage.rs`: `install_torch_stage` (crate-internal) and `run_torch_upgrade_worker` (launcher).
- `readiness.rs`: `TorchReadiness`, `torch_readiness`, `installed_torch_is_current`,
  `required_torch_version`, `installed_torch_version`.
- `choice_store.rs`: `read_torch_choice_record` (read-only, failures read as `Unrecorded`) and
  `persist_torch_choice_record` (through `install::update_install_target_general`).
- `constraints.rs`: `installer_files/torch-constraints.txt` from a freeze snapshot.
- `tests.rs`: golden option / label / argument vectors and the builder, uninstall, readiness,
  record-store, constraints and driver-note tables.

## Contracts and invariants
- Pins and index URLs are written ONLY in `catalog.rs`. Each option carries its exact
  `index_url` and specs; the command uses those fields (ROCm: `catalog::rocm_torch_specs` of the
  validated architecture) and never derives a URL or a spec from a label.
- ROCm installs from the AMD index ALONE (never mixed with PyPI in one resolve: PyPI holds
  squatted ROCm placeholders), with the mandatory `[device-gfxN]` extra (a bare AMD torch has no
  GPU kernels), without `--force-reinstall --no-cache-dir` (exact local-version pins replace any
  other torch; the cache lets retries reuse wheels). CPU / CUDA keep both flags. No torchaudio.
- A ROCm option exists only for a known architecture in the OS's device-extra list; on Linux it
  also needs amdgpu (the only driver the adapter enumeration reports) and `/dev/kfd`, never a
  system ROCm version. Every refusal pushes a localized reason; nothing is guessed from a name.
  The Windows driver note is a warning only. One architecture per install.
- The record is written only after a successful install; a failed write is a warning, never a
  failed stage (readiness then sees the old record or `Unrecorded` and offers a repair).
- `Unrecorded` keeps the legacy version-only rule (any flavour at or above the CPU/CUDA pin), so
  installs made before the record existed are never re-prompted. A recorded ROCm choice needs
  the exact pins plus `amd-torch-device-{gfx}`, `amd-torchvision-device-{gfx}` and `rocm-sdk-core`.
- `installed_torch_is_current` is the single readiness predicate: the `--check-venv` check, the
  repair worker's "skip the Torch stage" and the update continuation's "PyTorch already current"
  all use it with the recorded choice, so "ready" implies "nothing to do".
- Before a ROCm{gfx} install every device package (`amd-torch-device-*`,
  `amd-torchvision-device-*`, `rocm-sdk-device-*`) whose suffix is not exactly `{gfx}` is
  uninstalled, family wheels (`-gfx12-0`, `-gfx110x`) included; the resolve re-pulls the right
  family wheel from uv's cache. No stale or sibling-arch device package survives a reinstall.
- Every PyPI install while a torch is installed passes `-c` with the constraints file, for every
  flavour; a dependency that needs another torch then fails loudly instead of replacing it.
- Preflight and the stage spawn processes and do file I/O: worker threads only. The choice
  widget reads in-memory data only; its `TorchChoiceState` owns the prompt (no index can point
  into another prompt) and lives in the screen's state across frames. A ROCm row's label and
  size follow the PICKED architecture (`catalog::rocm_option_label`,
  `catalog::rocm_download_estimate` for `TorchOs::current()`); the size text rule has one owner,
  `download_size_text` in `mod.rs`.
- `tests.rs` holds literal golden vectors: a pin, flag, label or URL change is a deliberate
  test edit.
- uv offers no HTTP Range resume for wheels, so an interrupted torch download restarts that
  wheel (the retry reuses the cache for completed ones); the project's resumable-download rule is
  not met here (manager-tracked known gap).

## Editing map
- To change a pin, a wheel index, the ROCm architecture list, the size table or the driver
  minimum, edit `catalog.rs` (and the golden vectors in `tests.rs`).
- To change which GPUs lead to which offer, edit `preflight.rs` (`build_torch_options`).
- To change the install flags or the uninstall set, edit `command.rs`; the stage's steps,
  progress and retry budget, `stage.rs`.
- To change the readiness rule, edit `readiness.rs`; the record's keys and codec live in
  `ms_config::torch_choice`.
- To change how the choice is drawn, edit `choice_ui.rs` (all three screens follow); the
  screens (`install.rs`, `update.rs`, the launcher settings page) only own the state's lifetime,
  the heading around the widget and what happens with the returned selection.
