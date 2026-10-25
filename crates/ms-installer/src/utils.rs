/*
File: utils.rs

Purpose:
Runs installer backend work outside the egui UI layer.

Main responsibilities:
- download and unpack uv and app release assets;
- create the managed Python environment and install Python dependencies;
- own THE Python package list (`BASE_DEPENDENCIES`, `TORCH_DEPENDENCIES`; the gitignored
  `requirements/base.txt` / `torch.txt` mirror it, guarded by a test) and install the base group for every
  install and the torch-dependent extras only for full installs;
- decide which specs an existing environment still lacks (`missing_dependency_specs`: absent,
  or installed outside the spec's version pin, read through `pip_spec`), shared by repair,
  update continuation and the `--check-venv` readiness view;
- sequence the Torch stage (owned by `torch/`: pins, options, command, readiness) inside the
  install, repair and update workers, and provide it the pip / uv runner, retry loop and
  progress helpers (`pub(crate)`);
- provision ONLY the Python environment for the installer's environment-repair mode
  (`run_environment_repair_worker`): no app archive, executable, icon, shortcut, registry or
  elevation work, and no destruction of an existing working environment;
- sanitize ZIP entry path components that are invalid on Windows before writing files;
- sequence the Windows integration inside install and uninstall (`finalize_windows_post_install`,
  `run_windows_uninstall_worker`): the records themselves (registry entries, shortcuts,
  elevation probe and UAC relaunch) are owned by `ms_os_integration::windows`; this file only
  calls them, reports progress / console lines, and maps their typed errors onto the installer's
  localized messages (`IntegrationError::user_message`).
- register the Windows "Open with" entry best-effort for single-image input types (all-users AND
  per-user installs; per-user installs get nothing else) and remove it on uninstall.
- keep the install-continuation elevation wrappers (`relaunch_self_elevated`, Linux/macOS
  branches included) and the hidden service-flag argument builders.
- on Windows Program Files installs, create the root install directory and grant inheritable Users
  modify rights before installer-managed files are created.

Notes:
The initial install flow intentionally does not download application AI model weights. Runtime
code downloads app-managed models lazily through `ai_models.rs` when a feature needs them.
Release metadata and asset downloads live in `download.rs` (`github_api_get`,
`download_asset`) and the app release selection plus its `SHA256SUMS` pins in `release.rs`;
this file only picks the uv asset (`fetch_latest_uv_asset`, `select_uv_asset`) and sequences
the downloads, passing each app asset's pin into `download_asset`. Every worker takes a cancel flag (`&AtomicBool`, set when its window
closes) and hands it to the downloads and to `run_command_with_retry`, whose failed commands are
retried after a capped 2 / 4 / 8 s backoff that the flag interrupts.
*/

use std::collections::HashMap;
use std::env;
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use ms_thread as thread;
use web_time::SystemTime;
use web_time::Duration;

use ms_config as config;
use crate::pip_spec::{Pep440Version, Requirement};
use ms_sysprobe::python_manager;
use ms_config::version_format::{compare_release_versions, version_core};
// The Windows records (registry entries, shortcuts, elevation) are owned by
// `ms_os_integration`; this module only sequences them inside install / uninstall and turns
// their typed errors into the installer's localized messages (`user_message()`).
#[cfg(target_os = "windows")]
use ms_os_integration::windows::elevation::relaunch_self_elevated_with_args;
#[cfg(target_os = "windows")]
use ms_os_integration::windows::registry::{
    register_windows_install_in_registry, register_windows_open_with, remove_windows_registry_entries_for_install,
};
#[cfg(target_os = "windows")]
use ms_os_integration::windows::shortcut::{
    create_windows_start_menu_shortcut, remove_windows_shortcuts_for_install, resolve_windows_launcher_target,
};
#[cfg(target_os = "windows")]
use ms_os_integration::windows::values::quote_windows_arg;
#[cfg(target_os = "windows")]
use ms_os_integration::windows::{is_windows_all_users_install_dir, normalize_windows_path, windows_uninstall_registry_root};
use eframe::egui;
use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::Archive;
use zip::ZipArchive;

// Install-dir defaults exist only on desktop OSes; Android has no installed copy.
#[cfg(not(target_os = "android"))]
use super::install::INSTALL_SUBDIR_NAME;
use super::install::{
    EMBEDDED_APP_ICON_ICO, EMBEDDED_APP_ICON_PNG, InstallDependencyProfile,
    InstallEvent,
};
use super::torch::{
    TorchChoicePrompt, TorchInstallSelection, TorchPreflightResult, detect_torch_preflight, install_torch_stage,
    installed_torch_is_current, installed_torch_version, read_torch_choice_record, write_torch_constraints,
};
#[cfg(target_os = "windows")]
use super::install::{UninstallEvent, run_windows_uninstall_window, send_uninstall_progress};
use super::release::{APP_ZIP_ASSET_NAME, ArchiveRelease, fetch_app_release_by_tag, fetch_latest_app_release};
use super::download::{AssetDownload, GithubAsset, INSTALLER_USER_AGENT, download_asset, github_api_get};

const UV_RELEASE_API: &str = "https://api.github.com/repos/astral-sh/uv/releases/latest";
// Per-platform × per-arch GitHub *release asset* names of the main executable,
// as produced by `build-all.py`. These name the file uploaded to the release,
// NOT the on-disk executable (see `platform_executable_file_name`). Releases no
// longer carry the Windows / Linux aarch64 binaries: a build for those targets
// keeps compiling, but `release::select_app_release` finds no release for it.
const WINDOWS_BINARY_ASSET_NAME_X86_64: &str = "manhwastudio_rs.exe";
const WINDOWS_BINARY_ASSET_NAME_AARCH64: &str = "manhwastudio_rs_arm64.exe";
const MACOS_BINARY_ASSET_NAME_X86_64: &str = "manhwastudio_rs_macos";
const MACOS_BINARY_ASSET_NAME_AARCH64: &str = "manhwastudio_rs_macos_arm64";
const LINUX_BINARY_ASSET_NAME_X86_64: &str = "manhwastudio_rs";
const LINUX_BINARY_ASSET_NAME_AARCH64: &str = "manhwastudio_rs_linux_arm64";
const PYTHON_VERSION_REQUEST: &str = "3.11";
const PADDLE_CU126_INDEX_URL: &str = "https://www.paddlepaddle.org.cn/packages/stable/cu126/";
pub(crate) const ENABLE_PADDLE_CUDA_EXTRA_PACKAGES_INSTALL: bool = false;
// THE Python package list of the app (installer, repair, update continuation and the
// `--check-venv` readiness check all read it): the base group for every install and the
// torch-dependent extras for full installs, PyTorch itself excluded (`torch/`). The dev files
// `requirements/base.txt` / `requirements/torch.txt` (gitignored) mirror these two lists spec for
// spec; `tests::requirements_files_mirror_installer_lists` fails on drift.
//
// A version specifier is a REQUIREMENT on existing environments too: a package installed at a
// version outside it counts as missing (`missing_dependency_specs`) and is reinstalled at the
// spec. Every spec must parse with `pip_spec::Requirement` (guarded by a test) and use only the
// markers `dependency_marker_matches_current_platform` understands.
const BASE_DEPENDENCIES: &[&str] = &[
    "cloakbrowser",
    "deep-translator",
    "jaconv",
    "numpy",
    "onnxruntime; platform_system != \"Windows\"",
    "onnxruntime-directml; platform_system == \"Windows\"",
    // The headless build, not `opencv-python`: surya-ocr pins `opencv-python-headless`,
    // easyocr and reline require it, and the backend never opens an OpenCV window. Two
    // distributions of the `cv2` module in one environment overwrite each other's files.
    "opencv-python-headless",
    "Pillow",
    "playwright",
    "pyclipper",
    "requests",
    "selenium",
    // Capped to the line the backend is tested on: remote-code and FLUX.3 klein text-encoder
    // code depend on its internal API (a fresh resolve of 5.x breaks `flux3_klein/text_encoder.py`).
    // 4.57 also caps huggingface-hub below 1.0 itself, so the hub needs no pin of its own.
    "transformers>=4.57.1,<4.58",
    // Backs the loopback-WebSocket fallback transport used when AF_UNIX is
    // unavailable (Windows). Kept unconditional: wsproto is a tiny pure-Python
    // package, so installing it everywhere avoids a Windows-only branch that would
    // otherwise have to stay in sync with the runtime transport selection.
    "wsproto",
];
const TORCH_DEPENDENCIES: &[&str] = &[
    // Empty-weights (meta-device) model construction of FLUX.2 / FLUX.3 klein
    // (`flux2_klein/large_model.py`, `flux3_klein/text_encoder.py`).
    "accelerate",
    // >=0.39 for Flux2KleinInpaintPipeline (added in 0.38); 0.40 is avoided on purpose because
    // it requires huggingface-hub>=1.23, which forces transformers 5.x.
    "diffusers>=0.39,<0.40",
    "easydict",
    // easyocr / surya-ocr are pinned to the versions whose weight layout the app-managed model
    // catalog mirrors (easyocr `config.py` file names + md5, Surya manifest checkpoints); a
    // version change needs the catalog (`ms_sysprobe::ai_models`) changed together.
    "easyocr==1.7.2",
    "einops",
    // GGUF checkpoint loading of FLUX Fill (`GGUFQuantizationConfig`; diffusers imports it lazily).
    "gguf",
    "kornia",
    "manga-ocr",
    "omegaconf",
    "packaging",
    "pandas",
    "pytorch-lightning",
    "PyYAML",
    "reline",
    "shapely",
    "surya-ocr==0.17.1",
    "tqdm",
];

/// Sends a stage + overall progress update (values clamped to 0..=1); a closed channel is
/// ignored.
pub(super) fn send_progress(
    tx: &mpsc::Sender<InstallEvent>,
    stage_value: f32,
    stage_label: impl Into<String>,
    overall_value: f32,
    overall_label: impl Into<String>,
) {
    let _ = tx.send(InstallEvent::Progress {
        stage_value: stage_value.clamp(0.0, 1.0),
        stage_label: stage_label.into(),
        overall_value: overall_value.clamp(0.0, 1.0),
        overall_label: overall_label.into(),
    });
}

/// Sends one installer-console line; a closed channel (window gone) is ignored.
pub(crate) fn send_console_line(tx: &mpsc::Sender<InstallEvent>, line: impl Into<String>) {
    let _ = tx.send(InstallEvent::ConsoleLine(line.into()));
}

#[derive(Deserialize)]
struct GithubRelease {
    assets: Vec<GithubAsset>,
}

#[derive(Debug)]
pub enum UpdateWorkerEvent {
    Step(String),
    ConsoleLine(String),
    Progress {
        stage_value: f32,
        stage_label: String,
        overall_value: f32,
        overall_label: String,
    },
    TorchChoiceRequired(TorchChoicePrompt),
    NoUpdate {
        local_version: String,
        remote_version: String,
    },
    RelaunchStarted,
    Finished(Result<(), String>),
}

#[derive(Clone, Debug)]
pub struct ExternalUpdateTarget {
    /// Install root of the copy to update — the directory that holds the executable and
    /// the managed Python payload. Built by the caller (startup's custom-folder prompt).
    pub root_dir: PathBuf,
    /// The executable inside `root_dir` that will be replaced.
    pub executable_path: PathBuf,
}

fn send_update_progress(
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    stage_value: f32,
    stage_label: impl Into<String>,
    overall_value: f32,
    overall_label: impl Into<String>,
) {
    let _ = tx.send(UpdateWorkerEvent::Progress {
        stage_value: stage_value.clamp(0.0, 1.0),
        stage_label: stage_label.into(),
        overall_value: overall_value.clamp(0.0, 1.0),
        overall_label: overall_label.into(),
    });
}

fn send_update_console_line(tx: &mpsc::Sender<UpdateWorkerEvent>, line: impl Into<String>) {
    let _ = tx.send(UpdateWorkerEvent::ConsoleLine(line.into()));
}

struct UpdateToInstallEventBridge<'a> {
    tx: &'a mpsc::Sender<UpdateWorkerEvent>,
}

impl UpdateToInstallEventBridge<'_> {
    fn sender(&self) -> mpsc::Sender<InstallEvent> {
        let (install_tx, install_rx) = mpsc::channel();
        let update_tx = self.tx.clone();
        let _ = thread::Builder::new()
            .name("update-install-event-bridge".to_string())
            .spawn(move || {
                while let Ok(event) = install_rx.recv() {
                    match event {
                        InstallEvent::Step(text) => {
                            let _ = update_tx.send(UpdateWorkerEvent::Step(text));
                        }
                        InstallEvent::ConsoleLine(line) => {
                            let _ = update_tx.send(UpdateWorkerEvent::ConsoleLine(line));
                        }
                        InstallEvent::Progress {
                            stage_value,
                            stage_label,
                            overall_value,
                            overall_label,
                        } => {
                            let _ = update_tx.send(UpdateWorkerEvent::Progress {
                                stage_value,
                                stage_label,
                                overall_value,
                                overall_label,
                            });
                        }
                        InstallEvent::TorchPreflightReady(_) | InstallEvent::Finished(_) => {}
                    }
                }
            });
        install_tx
    }
}

/// The full-install pipeline (worker thread): Python runtime, app files, dependencies, then
/// the Windows integration. `display_version` is the installed program's version core, written
/// as the Uninstall entry's `DisplayVersion` on an all-users Windows install.
pub(super) fn run_install_worker(
    root_dir: PathBuf,
    launcher_exe_path: Option<PathBuf>,
    display_version: &str,
    dependency_profile: InstallDependencyProfile,
    torch_selection: TorchInstallSelection,
    tx: &mpsc::Sender<InstallEvent>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    // Более равномерные веса этапов общего прогресса.
    let mut progress_cursor = 0.0_f32;
    let prep_range = alloc_progress_range(&mut progress_cursor, 0.03);
    let python_range = alloc_progress_range(&mut progress_cursor, 0.17);
    let app_range = alloc_progress_range(&mut progress_cursor, 0.17);
    let base_deps_range = alloc_progress_range(&mut progress_cursor, 0.22);
    let torch_range = alloc_progress_range(&mut progress_cursor, 0.16);
    let torch_deps_range = alloc_progress_range(&mut progress_cursor, 0.24);
    let windows_post_range = (progress_cursor, 1.0_f32);

    let _ = tx.send(InstallEvent::Step(t!("installer.utils.preparing_dirs_status").to_string()));
    prepare_install_root_dir(&root_dir)?;
    let installer_dir = root_dir.join("installer_files");
    fs::create_dir_all(&installer_dir)
        .map_err(|e| tf!("installer.utils.create_installer_files_error", e = e))?;
    let downloads_dir = installer_dir.join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| tf!("installer.utils.create_installer_downloads_error", e = e))?;
    send_progress(tx, 1.0, t!("installer.common.preparation"), prep_range.1, t!("installer.utils.preparation_done"));

    let resolved_arch = detect_arch()?;
    let platform = detect_platform()?;

    let _ = tx.send(InstallEvent::Step(tf!("installer.utils.searching_uv_status", platform = platform, resolved_arch = resolved_arch)));
    let asset = fetch_latest_uv_asset(platform, &resolved_arch, cancel)?;
    let archive_path = downloads_dir.join(&asset.name);

    let _ = tx.send(InstallEvent::Step(tf!("installer.utils.downloading_asset_status", asset = asset.name)));
    download_asset(
        &AssetDownload {
            asset: &asset,
            dst_path: &archive_path,
            label_prefix: "uv",
            overall_range: (python_range.0, lerp(python_range.0, python_range.1, 0.55)),
            required_sha256: None,
        },
        tx,
        cancel,
    )?;

    let uv_dir = installer_dir.join("uv");
    if uv_dir.exists() {
        let _ = tx.send(InstallEvent::Step(
            t!("installer.utils.removing_old_uv_status").to_string(),
        ));
        fs::remove_dir_all(&uv_dir).map_err(|e| {
            tf!("installer.utils.clear_old_uv_folder_error", uv_dir = uv_dir.display(), e = e)
        })?;
    }
    fs::create_dir_all(&uv_dir)
        .map_err(|e| tf!("installer.utils.create_uv_dir_error", uv_dir = uv_dir.display(), e = e))?;

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.extracting_uv_status").to_string(),
    ));
    extract_archive(
        &archive_path,
        &uv_dir,
        tx,
        t!("installer.utils.extract_uv_stage"),
        lerp(python_range.0, python_range.1, 0.55),
        python_range.1,
    )?;
    flatten_single_root_dir(&uv_dir)?;
    let uv_exe = resolve_uv_executable(&uv_dir)?;
    let pip_runner = PipInstallRunner::Uv(uv_exe.clone());

    let uv_python_dir = uv_dir.join("python");
    let uv_cache_dir = uv_dir.join("cache");
    fs::create_dir_all(&uv_python_dir)
        .map_err(|e| tf!("installer.utils.create_uv_python_dir_error", uv_python_dir = uv_python_dir.display(), e = e))?;
    fs::create_dir_all(&uv_cache_dir)
        .map_err(|e| tf!("installer.utils.create_uv_cache_dir_error", uv_cache_dir = uv_cache_dir.display(), e = e))?;
    let uv_python_dir_str = uv_python_dir.to_string_lossy().into_owned();
    let uv_cache_dir_str = uv_cache_dir.to_string_lossy().into_owned();
    let uv_env = [
        ("UV_PYTHON_INSTALL_DIR", uv_python_dir_str.as_str()),
        ("UV_CACHE_DIR", uv_cache_dir_str.as_str()),
    ];

    let _ = tx.send(InstallEvent::Step(tf!("installer.utils.installing_python_status", python_version_request = PYTHON_VERSION_REQUEST)));
    run_command_with_retry(
        &uv_exe,
        &root_dir,
        &["python", "install", PYTHON_VERSION_REQUEST],
        RetryRun { action_name: &tf!("installer.utils.install_python_action", python_version_request = PYTHON_VERSION_REQUEST), attempts: 2, tx: Some(tx), cancel },
        &uv_env,
    )?;

    let managed_venv_dir = installer_dir.join("venv");
    if managed_venv_dir.exists() {
        let _ = tx.send(InstallEvent::Step(
            t!("installer.utils.removing_old_venv_status").to_string(),
        ));
        fs::remove_dir_all(&managed_venv_dir).map_err(|e| {
            tf!("installer.utils.clear_old_venv_folder_error", managed_venv_dir = managed_venv_dir.display(), e = e)
        })?;
    }

    let managed_venv_dir_str = managed_venv_dir.to_string_lossy().into_owned();
    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.creating_venv_status").to_string(),
    ));
    run_command_with_retry(
        &uv_exe,
        &root_dir,
        &[
            "venv",
            "--python",
            PYTHON_VERSION_REQUEST,
            &managed_venv_dir_str,
        ],
        RetryRun { action_name: t!("installer.utils.create_venv_action"), attempts: 2, tx: Some(tx), cancel },
        &uv_env,
    )?;
    let python_exe = python_manager::resolve_python_executable_in_dir(&managed_venv_dir)?;

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.searching_app_zip_status").to_string(),
    ));
    // The archive is verified against the release's `SHA256SUMS` before it is published into
    // `downloads/`, i.e. before it can be extracted (`release.rs`).
    let app_release = fetch_latest_app_release("install-worker", cancel)?;
    let app_checksums = app_release.checksums.fetch(cancel)?;
    let app_asset = &app_release.archive;
    let app_zip_sha256 = app_checksums.pin_for(app_asset)?;
    let app_zip_path = downloads_dir.join(APP_ZIP_ASSET_NAME);

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.downloading_app_zip_status").to_string(),
    ));
    download_asset(
        &AssetDownload {
            asset: app_asset,
            dst_path: &app_zip_path,
            label_prefix: APP_ZIP_ASSET_NAME,
            overall_range: (app_range.0, lerp(app_range.0, app_range.1, 0.50)),
            required_sha256: Some(app_zip_sha256),
        },
        tx,
        cancel,
    )?;

    let app_extract_dir = downloads_dir.join("manhwastudio_extract");
    if app_extract_dir.exists() {
        fs::remove_dir_all(&app_extract_dir).map_err(|e| {
            tf!("installer.utils.clear_temp_app_extract_error", app_extract_dir = app_extract_dir.display(), e = e)
        })?;
    }
    fs::create_dir_all(&app_extract_dir).map_err(|e| {
        tf!("installer.utils.create_temp_app_extract_error", app_extract_dir = app_extract_dir.display(), e = e)
    })?;

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.extracting_app_zip_status").to_string(),
    ));
    extract_archive(
        &app_zip_path,
        &app_extract_dir,
        tx,
        t!("installer.utils.extract_app_zip_stage"),
        lerp(app_range.0, app_range.1, 0.50),
        lerp(app_range.0, app_range.1, 0.85),
    )?;
    flatten_single_root_dir(&app_extract_dir)?;

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.copying_app_files_status").to_string(),
    ));
    merge_dir_contents(&app_extract_dir, &root_dir)?;
    write_embedded_app_icon(&root_dir)?;
    copy_launcher_to_install_dir(launcher_exe_path.as_deref(), &root_dir, tx)?;
    send_progress(
        tx,
        1.0,
        t!("installer.utils.copy_app_files_stage"),
        app_range.1,
        t!("installer.utils.app_files_deployed_status"),
    );
    fs::remove_dir_all(&app_extract_dir).map_err(|e| {
        tf!("installer.utils.delete_temp_app_extract_error", app_extract_dir = app_extract_dir.display(), e = e)
    })?;

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.installing_base_deps_status").to_string(),
    ));
    install_static_python_dependencies(DependencyInstallRequest {
        root_dir: &root_dir,
        pip_runner: &pip_runner,
        python_exe: &python_exe,
        tx,
        cancel,
        label: t!("installer.utils.base_deps_label"),
        dependencies: BASE_DEPENDENCIES,
        // A fresh environment has no torch yet, so there is nothing to constrain.
        constraints: None,
        overall_start: base_deps_range.0,
        overall_end: base_deps_range.1,
    })?;

    match dependency_profile {
        InstallDependencyProfile::Fast => {
            send_progress(
                tx,
                1.0,
                t!("installer.utils.pytorch_fast_mode"),
                torch_range.1,
                t!("installer.utils.pytorch_skipped"),
            );
            send_progress(
                tx,
                1.0,
                t!("installer.utils.torch_deps_fast_mode"),
                torch_deps_range.1,
                t!("installer.utils.torch_deps_skipped"),
            );
        }
        InstallDependencyProfile::Full => {
            let installed = install_torch_stage(
                &root_dir,
                &pip_runner,
                &python_exe,
                &torch_selection,
                tx,
                (torch_range.0, torch_range.1),
                cancel,
            )?;
            // Every later PyPI install keeps the torch just installed (`-c`), whatever its flavour.
            let constraints = write_torch_constraints(&root_dir, &installed)?;
            let install_cuda_extra_packages = ENABLE_PADDLE_CUDA_EXTRA_PACKAGES_INSTALL && torch_selection.is_cuda();
            install_torch_python_dependencies(
                &PostTorchInstallTarget {
                    root_dir: &root_dir,
                    pip_runner: &pip_runner,
                    python_exe: &python_exe,
                    tx,
                    cancel,
                    constraints: constraints.as_deref(),
                },
                install_cuda_extra_packages,
                (torch_deps_range.0, torch_deps_range.1),
            )?;
        }
    }

    finalize_windows_post_install(&root_dir, display_version, tx, windows_post_range.0, windows_post_range.1)?;
    send_progress(tx, 1.0, t!("installer.common.install_complete"), 1.0, t!("installer.common.install_complete"));

    Ok(())
}

/// Provisions ONLY the managed Python environment of an existing installation root.
///
/// This backs the installer's environment-repair mode (`--check-venv` and the
/// repair window). Compared to [`run_install_worker`] it deliberately does NOT
/// touch the application itself: no `ManhwaStudio.zip` download or extraction, no
/// executable copy, no icon, no shortcuts, no Windows registry entries, no
/// elevation and no post-install finalization. It also never destroys an existing
/// working environment — in particular the user's own `venv/` in the root is
/// reused as-is and never deleted.
///
/// Steps: create `installer_files/` (+ `downloads/`), ensure the `uv` runtime,
/// reuse or create the managed interpreter, install the still-missing base
/// dependencies and — for [`InstallDependencyProfile::Full`] — the Torch stage plus
/// the still-missing torch extras.
///
/// `torch_selection` is ignored for `Fast`. Progress is reported through `tx` with
/// stage weights covering this reduced set of stages.
///
/// # Errors
/// Returns a user-facing message when a directory cannot be created, `uv` cannot be
/// resolved, no interpreter can be provisioned, or a package installation fails.
pub(super) fn run_environment_repair_worker(
    root_dir: PathBuf,
    dependency_profile: InstallDependencyProfile,
    torch_selection: TorchInstallSelection,
    tx: &mpsc::Sender<InstallEvent>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut progress_cursor = 0.0_f32;
    let prep_range = alloc_progress_range(&mut progress_cursor, 0.05);
    let uv_range = alloc_progress_range(&mut progress_cursor, 0.25);
    let python_range = alloc_progress_range(&mut progress_cursor, 0.10);
    let base_deps_range = alloc_progress_range(&mut progress_cursor, 0.25);
    let torch_range = alloc_progress_range(&mut progress_cursor, 0.15);
    let torch_deps_range = (progress_cursor, 1.0_f32);

    // Only the installer-owned subtree is created here; `prepare_install_root_dir`
    // (root creation + Windows ACL rewrite) belongs to a real install, not a repair.
    let _ = tx.send(InstallEvent::Step(
        t!("installer.repair.preparing_dirs_status").to_string(),
    ));
    let installer_dir = root_dir.join("installer_files");
    fs::create_dir_all(&installer_dir)
        .map_err(|e| tf!("installer.utils.create_installer_files_error", e = e))?;
    let downloads_dir = installer_dir.join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| tf!("installer.utils.create_installer_downloads_error", e = e))?;
    send_progress(
        tx,
        1.0,
        t!("installer.common.preparation"),
        prep_range.1,
        t!("installer.utils.preparation_done"),
    );

    let uv_exe = ensure_uv_runtime(&root_dir, tx, uv_range.0, uv_range.1, cancel)?;
    let python_exe = ensure_repair_python_environment(&root_dir, &uv_exe, tx, python_range, cancel)?;
    let pip_runner = resolve_runtime_pip_runner(&root_dir, &python_exe);
    send_console_line(
        tx,
        format!(
            "[Env] Python: {}; installer: {}",
            python_exe.display(),
            pip_runner.label()
        ),
    );

    let installed = freeze_installed_packages(&pip_runner, &python_exe, &root_dir, None)?;
    // A repaired environment may already hold a torch: the base installs must keep it too.
    let constraints = write_torch_constraints(&root_dir, &installed)?;
    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.installing_base_deps_status").to_string(),
    ));
    install_missing_static_dependencies(
        DependencyInstallRequest {
            root_dir: &root_dir,
            pip_runner: &pip_runner,
            python_exe: &python_exe,
            tx,
            cancel,
            label: t!("installer.utils.base_deps_label"),
            dependencies: BASE_DEPENDENCIES,
            constraints: constraints.as_deref(),
            overall_start: base_deps_range.0,
            overall_end: base_deps_range.1,
        },
        &installed,
    )?;

    match dependency_profile {
        InstallDependencyProfile::Fast => {
            send_progress(
                tx,
                1.0,
                t!("installer.utils.pytorch_fast_mode"),
                torch_range.1,
                t!("installer.utils.pytorch_skipped"),
            );
            send_progress(
                tx,
                1.0,
                t!("installer.utils.torch_deps_fast_mode"),
                torch_deps_range.1,
                t!("installer.utils.torch_deps_skipped"),
            );
        }
        InstallDependencyProfile::Full => {
            // A current PyTorch is left alone: re-running the Torch stage would
            // force-reinstall a multi-gigabyte wheel that the environment already has.
            // The predicate (against the recorded choice) is shared with the readiness check,
            // so "ready" and "nothing left for the worker to do" can never drift apart.
            let record = read_torch_choice_record(&root_dir);
            let current_torch =
                installed_torch_is_current(&installed, &record).then(|| installed_torch_version(&installed));
            match current_torch.flatten() {
                Some(version) => {
                    send_console_line(
                        tx,
                        tf!("installer.repair.pytorch_already_installed_log", version = version),
                    );
                    send_progress(
                        tx,
                        1.0,
                        t!("installer.utils.pytorch_already_current_status"),
                        torch_range.1,
                        t!("installer.utils.pytorch_stage_done"),
                    );
                }
                None => {
                    install_torch_stage(
                        &root_dir,
                        &pip_runner,
                        &python_exe,
                        &torch_selection,
                        tx,
                        (torch_range.0, torch_range.1),
                        cancel,
                    )?;
                }
            }

            // Re-probe: the Torch stage pulls in transitive packages, so the pre-Torch
            // snapshot would over-report the missing torch extras.
            let installed = freeze_installed_packages(&pip_runner, &python_exe, &root_dir, None)?;
            let constraints = write_torch_constraints(&root_dir, &installed)?;
            install_missing_static_dependencies(
                DependencyInstallRequest {
                    root_dir: &root_dir,
                    pip_runner: &pip_runner,
                    python_exe: &python_exe,
                    tx,
                    cancel,
                    label: t!("installer.utils.torch_deps_label"),
                    dependencies: TORCH_DEPENDENCIES,
                    constraints: constraints.as_deref(),
                    overall_start: torch_deps_range.0,
                    overall_end: torch_deps_range.1,
                },
                &installed,
            )?;
        }
    }

    send_progress(
        tx,
        1.0,
        t!("installer.repair.env_ready_status"),
        1.0,
        t!("installer.repair.env_ready_status"),
    );
    Ok(())
}

/// Resolves the interpreter the repair worker installs packages into.
///
/// A working environment found by `python_manager` (managed `installer_files/venv`,
/// a user `venv/`, a conda env, or a standalone interpreter) is reused verbatim —
/// repair must never delete an environment the user still relies on. Only when no
/// interpreter resolves at all is a fresh uv-managed `installer_files/venv` created,
/// after removing a leftover broken one (that directory is installer-owned).
///
/// `overall_range` is the slice of the overall progress bar this step reports into.
///
/// # Errors
/// Returns a user-facing message when the managed venv cannot be removed, `uv`
/// fails to install Python 3.11 or to create the venv, or no interpreter can be
/// resolved inside the freshly created venv.
fn ensure_repair_python_environment(
    root_dir: &Path,
    uv_exe: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    overall_range: (f32, f32),
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    if let Ok(python_exe) = python_manager::resolve_python_executable(root_dir) {
        send_console_line(
            tx,
            tf!("installer.repair.reusing_env_log", python = python_exe.display()),
        );
        send_progress(
            tx,
            1.0,
            t!("installer.utils.python_env_ready"),
            overall_range.1,
            t!("installer.utils.python_env_ready"),
        );
        return Ok(python_exe);
    }

    let managed_venv_dir = root_dir.join("installer_files").join("venv");
    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.creating_new_venv_status").to_string(),
    ));
    if managed_venv_dir.exists() {
        // Reached only when NO interpreter resolved anywhere, so this directory is a
        // broken installer-managed leftover. The user's own `venv/` is never here.
        fs::remove_dir_all(&managed_venv_dir).map_err(|e| {
            tf!("installer.utils.remove_old_env_error", managed_venv_dir = managed_venv_dir.display(), e = e)
        })?;
    }

    let uv_env = build_uv_env(root_dir)?;
    run_command_with_retry(
        uv_exe,
        root_dir,
        &["python", "install", PYTHON_VERSION_REQUEST],
        RetryRun { action_name: &tf!("installer.utils.install_python_action", python_version_request = PYTHON_VERSION_REQUEST), attempts: 2, tx: Some(tx), cancel },
        &uv_env.as_env_slice(),
    )?;
    send_progress(
        tx,
        0.5,
        t!("installer.utils.creating_venv_status"),
        lerp(overall_range.0, overall_range.1, 0.5),
        t!("installer.utils.creating_venv_status"),
    );
    let managed_venv_dir_str = managed_venv_dir.to_string_lossy().into_owned();
    run_command_with_retry(
        uv_exe,
        root_dir,
        &[
            "venv",
            "--python",
            PYTHON_VERSION_REQUEST,
            &managed_venv_dir_str,
        ],
        RetryRun { action_name: t!("installer.utils.create_venv_action"), attempts: 2, tx: Some(tx), cancel },
        &uv_env.as_env_slice(),
    )?;
    let python_exe = python_manager::resolve_python_executable_in_dir(&managed_venv_dir)?;
    send_progress(
        tx,
        1.0,
        t!("installer.utils.python_env_ready"),
        overall_range.1,
        t!("installer.utils.python_env_ready"),
    );
    Ok(python_exe)
}

/// Installs only the dependencies of `request` that `installed` does not provide, or provides
/// at a version outside the spec's pin (see [`missing_dependency_specs`]).
///
/// `installed` is a `pip freeze` snapshot (see [`freeze_installed_packages`]). When
/// nothing is missing the request is skipped entirely — including the pip/wheel
/// upgrade [`install_static_python_dependencies`] performs — and the stage is
/// reported as complete.
fn install_missing_static_dependencies(
    request: DependencyInstallRequest<'_>,
    installed: &HashMap<String, String>,
) -> Result<(), String> {
    let missing = missing_dependency_specs(request.dependencies, installed);
    if missing.is_empty() {
        send_progress(
            request.tx,
            1.0,
            t!("installer.utils.python_deps_current_status"),
            request.overall_end,
            t!("installer.utils.deps_current_status"),
        );
        return Ok(());
    }
    install_static_python_dependencies(DependencyInstallRequest {
        dependencies: &missing,
        ..request
    })
}

pub(crate) fn run_update_binary_stage(root_dir: PathBuf, tx: &mpsc::Sender<UpdateWorkerEvent>, cancel: &AtomicBool) {
    let result = run_update_binary_stage_inner(&root_dir, None, tx, cancel);
    match result {
        Ok(UpdateBinaryStageOutcome::RelaunchStarted) => {
            let _ = tx.send(UpdateWorkerEvent::RelaunchStarted);
        }
        Ok(UpdateBinaryStageOutcome::NoUpdate {
            local_version,
            remote_version,
        }) => {
            let _ = tx.send(UpdateWorkerEvent::NoUpdate {
                local_version,
                remote_version,
            });
        }
        Err(err) => {
            let _ = tx.send(UpdateWorkerEvent::Finished(Err(err)));
        }
    }
}

pub(crate) fn run_external_update_binary_stage(
    target: ExternalUpdateTarget,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    cancel: &AtomicBool,
) {
    let result = run_update_binary_stage_inner(&target.root_dir, Some(&target.executable_path), tx, cancel);
    match result {
        Ok(UpdateBinaryStageOutcome::RelaunchStarted) => {
            let _ = tx.send(UpdateWorkerEvent::RelaunchStarted);
        }
        Ok(UpdateBinaryStageOutcome::NoUpdate {
            local_version,
            remote_version,
        }) => {
            let _ = tx.send(UpdateWorkerEvent::NoUpdate {
                local_version,
                remote_version,
            });
        }
        Err(err) => {
            let _ = tx.send(UpdateWorkerEvent::Finished(Err(err)));
        }
    }
}

/// Runs the `--continue-update` stage in the freshly installed binary. `own_version` is that
/// binary's `MS_APP_VERSION` (`HostVersion::version`): the archive is taken from the release
/// with exactly that tag, never from a newer one (`release::select_app_release_by_tag`).
pub(crate) fn run_update_continuation_stage(
    root_dir: PathBuf,
    own_version: &str,
    torch_selection: Option<TorchInstallSelection>,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    cancel: &AtomicBool,
) {
    match run_update_continuation_stage_inner(&root_dir, own_version, torch_selection, tx, cancel) {
        Ok(UpdateContinuationOutcome::Completed) => {
            let _ = tx.send(UpdateWorkerEvent::Finished(Ok(())));
        }
        Ok(UpdateContinuationOutcome::WaitingForTorchChoice) => {}
        Err(err) => {
            let _ = tx.send(UpdateWorkerEvent::Finished(Err(err)));
        }
    }
}

enum UpdateBinaryStageOutcome {
    RelaunchStarted,
    NoUpdate {
        local_version: String,
        remote_version: String,
    },
}

enum UpdateContinuationOutcome {
    Completed,
    WaitingForTorchChoice,
}

fn run_update_binary_stage_inner(
    root_dir: &Path,
    target_executable: Option<&Path>,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    cancel: &AtomicBool,
) -> Result<UpdateBinaryStageOutcome, String> {
    let current_exe = match target_executable {
        Some(executable) => executable.to_path_buf(),
        None => env::current_exe()
            .map_err(|e| tf!("installer.utils.determine_current_executable_error", e = e))?,
    };
    let local_version = query_executable_version(&current_exe, root_dir)?;
    send_update_console_line(
        tx,
        format!(
            "[Update] Target executable: {}; version: {}",
            current_exe.display(),
            local_version
        ),
    );

    let _ = tx.send(UpdateWorkerEvent::Step(
        t!("installer.utils.checking_latest_release_status").to_string(),
    ));
    send_update_progress(tx, 0.0, t!("installer.common.preparation"), 0.0, t!("installer.utils.stage_update_binary"));
    let release = fetch_latest_app_release("update-binary-stage", cancel)?;
    let remote_version = release.tag.clone();
    // The probed binary prints its `MS_APP_VERSION`: a commit count for current builds, but
    // `3.6.0+1cd9638-83-dirty` for an installed copy built before commit-count versioning. Both
    // sides are therefore reduced to their semver core before comparing: build metadata carries
    // no ordering meaning, and leaving it in would make such a build compare as strictly newer
    // than its own release tag forever.
    if compare_release_versions(version_core(&remote_version), version_core(&local_version)).is_le() {
        return Ok(UpdateBinaryStageOutcome::NoUpdate {
            local_version,
            remote_version,
        });
    }

    let _ = tx.send(UpdateWorkerEvent::Step(tf!("installer.utils.version_available_status", remote_version = remote_version)));
    // The binary is verified against the release's `SHA256SUMS` before it is published into
    // `downloads/`, i.e. before it can replace the program (`release.rs`).
    let checksums = release.checksums.fetch(cancel)?;
    let binary_asset = &release.binary;
    let binary_sha256 = checksums.pin_for(binary_asset)?;
    let downloads_dir = root_dir.join("installer_files").join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| tf!("installer.utils.create_downloads_dir_error", downloads_dir = downloads_dir.display(), e = e))?;
    let downloaded_binary = downloads_dir.join(format!("{}.update", binary_asset.name));

    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    download_asset(
        &AssetDownload {
            asset: binary_asset,
            dst_path: &downloaded_binary,
            label_prefix: &binary_asset.name,
            overall_range: (0.0, 0.82),
            required_sha256: Some(binary_sha256),
        },
        &install_tx,
        cancel,
    )?;

    send_update_console_line(
        tx,
        format!(
            "[Update] Downloaded executable '{}' -> '{}'",
            binary_asset.name,
            downloaded_binary.display()
        ),
    );

    #[cfg(target_os = "windows")]
    {
        send_update_progress(tx, 1.0, t!("installer.utils.binary_downloaded_status"), 0.92, t!("installer.utils.preparing_restart_status"));
        spawn_windows_update_replacement_script(root_dir, &downloaded_binary, &current_exe)?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        replace_unix_executable(&downloaded_binary, &current_exe)?;
        spawn_continue_update_process(root_dir, &current_exe)?;
    }

    send_update_progress(
        tx,
        1.0,
        t!("installer.utils.restart_started_status"),
        1.0,
        t!("installer.utils.restart_to_new_version_status"),
    );
    Ok(UpdateBinaryStageOutcome::RelaunchStarted)
}

fn run_update_continuation_stage_inner(
    root_dir: &Path,
    own_version: &str,
    torch_selection: Option<TorchInstallSelection>,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    cancel: &AtomicBool,
) -> Result<UpdateContinuationOutcome, String> {
    let mut progress_cursor = 0.0_f32;
    let env_range = alloc_progress_range(&mut progress_cursor, 0.22);
    let torch_range = alloc_progress_range(&mut progress_cursor, 0.20);
    let deps_range = alloc_progress_range(&mut progress_cursor, 0.28);
    let app_range = (progress_cursor, 1.0_f32);

    // Resolve this binary's own release BEFORE the environment is touched: if it is not
    // listed (or lacks the archive / `SHA256SUMS`), the stage stops with nothing changed.
    let _ = tx.send(UpdateWorkerEvent::Step(t!("installer.utils.finding_own_release_status").to_string()));
    let app_release = fetch_app_release_by_tag(own_version, "update-continuation", cancel)?;
    send_update_console_line(tx, format!("[Update] Program files from release {}", app_release.tag));

    let (uv_exe, python_exe) = ensure_uv_managed_python_environment(root_dir, tx, env_range, cancel)?;
    let pip_runner = PipInstallRunner::Uv(uv_exe);
    let install_type = read_current_ai_install_type(root_dir);
    send_update_console_line(
        tx,
        format!("[Update] AI install type: {}", install_type.as_str()),
    );

    if install_type == config::AiInstallType::Full {
        match maybe_update_torch(
            root_dir,
            &pip_runner,
            &python_exe,
            torch_selection,
            tx,
            torch_range,
            cancel,
        )? {
            UpdateContinuationOutcome::Completed => {}
            UpdateContinuationOutcome::WaitingForTorchChoice => {
                return Ok(UpdateContinuationOutcome::WaitingForTorchChoice);
            }
        }
    } else {
        send_update_progress(
            tx,
            1.0,
            t!("installer.utils.pytorch_not_needed_status"),
            torch_range.1,
            t!("installer.utils.pytorch_stage_skipped"),
        );
    }

    install_missing_dependencies_for_update(
        root_dir,
        &pip_runner,
        &python_exe,
        install_type,
        tx,
        deps_range,
        cancel,
    )?;
    download_and_extract_app_archive_for_update(root_dir, &app_release, tx, app_range, cancel)?;
    send_update_progress(tx, 1.0, t!("installer.utils.update_complete_status"), 1.0, t!("installer.common.ready"));
    Ok(UpdateContinuationOutcome::Completed)
}

/// Returns the GitHub *release asset* download file name of the main executable
/// for the current build's OS and architecture. This is the name of the file
/// uploaded to the release, NOT the executable's on-disk name — see
/// `platform_executable_file_name`.
///
/// The single source of truth for the six release-asset names: the release
/// selection (`release::select_app_release`, used by every update and install
/// path) calls it, so the check and the download always agree on the asset.
///
/// Selection is by OS × architecture at compile time:
///
/// | OS      | x86_64                  | aarch64                        |
/// |---------|-------------------------|--------------------------------|
/// | Windows | `manhwastudio_rs.exe`   | `manhwastudio_rs_arm64.exe`    |
/// | macOS   | `manhwastudio_rs_macos` | `manhwastudio_rs_macos_arm64`  |
/// | Linux   | `manhwastudio_rs`       | `manhwastudio_rs_linux_arm64`  |
///
/// Only `x86_64` and `aarch64` are shipped. Because selection is a compile-time
/// `cfg!` chain, `aarch64` is matched explicitly and `x86_64` is the residual
/// arm — any other target arch would silently resolve to the x86_64 asset, but no
/// such build is produced or published.
///
/// On macOS the asset name intentionally differs from the on-disk executable
/// name: the release ships the binary renamed with a `_macos` suffix, but on disk
/// (and inside `ManhwaStudio.zip`) the executable is the bare `manhwastudio_rs`.
/// Use this function ONLY for the release download; use
/// `platform_executable_file_name` for anything that names the file on disk.
pub(crate) fn platform_binary_asset_name() -> &'static str {
    let is_aarch64 = cfg!(target_arch = "aarch64");
    if cfg!(target_os = "windows") {
        if is_aarch64 {
            WINDOWS_BINARY_ASSET_NAME_AARCH64
        } else {
            WINDOWS_BINARY_ASSET_NAME_X86_64
        }
    } else if cfg!(target_os = "macos") {
        if is_aarch64 {
            MACOS_BINARY_ASSET_NAME_AARCH64
        } else {
            MACOS_BINARY_ASSET_NAME_X86_64
        }
    } else if is_aarch64 {
        LINUX_BINARY_ASSET_NAME_AARCH64
    } else {
        LINUX_BINARY_ASSET_NAME_X86_64
    }
}

/// Returns the *on-disk* file name of the main executable for the current
/// platform: Windows → `manhwastudio_rs.exe`, everything else (Linux and macOS) →
/// `manhwastudio_rs` (no `_macos` suffix).
///
/// This is the name the executable carries on disk and inside the extracted
/// `ManhwaStudio.zip` payload, which is deliberately different from the release
/// download name (see `platform_binary_asset_name`). The two coincide only on
/// x86_64 Windows/Linux: macOS assets carry a `_macos` suffix on both arches,
/// and every aarch64 asset carries an arch suffix that never appears on disk.
/// Any code that strips or locates the executable within the install tree or an
/// extracted archive must use this function, not the release-asset name.
fn platform_executable_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        ms_os_integration::identity::WINDOWS_EXE_NAME
    } else {
        "manhwastudio_rs"
    }
}

/// Runs `executable --version` in `root_dir` and returns the version string it printed.
///
/// The returned string is the copy's `MS_APP_VERSION`: a commit count (`335`) for current
/// builds, but `3.6.0+1cd9638-83-dirty` for one built before commit-count versioning. It may be
/// DISPLAYED as-is, but any comparison must first reduce both sides with [`version_core`] (see
/// `crates/ms-config/src/version_format.rs`).
///
/// Blocking: it spawns a process and waits for it, so callers must be on a worker thread.
///
/// # Errors
/// Returns a localized message when the process cannot be started, exits non-zero, or
/// prints nothing that parses as a version.
pub(super) fn query_executable_version(executable: &Path, root_dir: &Path) -> Result<String, String> {
    let (status, output) = run_command_streaming(executable, root_dir, &["--version"], None, &[])?;
    if !status.success() {
        return Err(tf!("installer.utils.get_version_error", executable = executable.display(), output = output.trim()));
    }
    parse_executable_version_output(&output).ok_or_else(|| {
        tf!("installer.utils.parse_version_error", executable = executable.display(), output = output.trim())
    })
}

fn parse_executable_version_output(output: &str) -> Option<String> {
    output.split_whitespace().rev().find_map(|token| {
        let trimmed = token.trim_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' || ch == '+')
        });
        if trimmed.chars().any(|ch| ch.is_ascii_digit()) {
            Some(trimmed.to_string())
        } else {
            None
        }
    })
}

#[cfg(not(target_os = "windows"))]
fn replace_unix_executable(downloaded_binary: &Path, current_exe: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(downloaded_binary, fs::Permissions::from_mode(0o755)).map_err(|e| {
            tf!("installer.utils.set_exec_permissions_error", downloaded_binary = downloaded_binary.display(), e = e)
        })?;
    }
    fs::rename(downloaded_binary, current_exe).map_err(|e| {
        tf!("installer.utils.replace_executable_error", current_exe = current_exe.display(), downloaded_binary = downloaded_binary.display(), e = e)
    })
}

#[cfg(not(target_os = "windows"))]
fn spawn_continue_update_process(root_dir: &Path, executable: &Path) -> Result<(), String> {
    let mut cmd = Command::new(executable);
    cmd.current_dir(root_dir).arg("--continue-update");
    cmd.spawn().map_err(|e| {
        tf!("installer.utils.launch_continue_update_error", executable = executable.display(), e = e)
    })?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn spawn_windows_update_replacement_script(
    root_dir: &Path,
    downloaded_binary: &Path,
    current_exe: &Path,
) -> Result<(), String> {
    let script_path = root_dir
        .join("installer_files")
        .join("downloads")
        .join("continue_update.cmd");
    let script = format!(
        "@echo off\r\n\
         setlocal\r\n\
         set \"SRC={src}\"\r\n\
         set \"DST={dst}\"\r\n\
         set \"ROOT={root}\"\r\n\
         for /l %%i in (1,1,60) do (\r\n\
         \tmove /Y \"%SRC%\" \"%DST%\" >nul 2>nul\r\n\
         \tif not errorlevel 1 (\r\n\
         \t\tstart \"\" /D \"%ROOT%\" \"%DST%\" --continue-update\r\n\
         \t\texit /b 0\r\n\
         \t)\r\n\
         \ttimeout /t 1 /nobreak >nul\r\n\
         )\r\n\
         exit /b 1\r\n",
        src = downloaded_binary.display(),
        dst = current_exe.display(),
        root = root_dir.display(),
    );
    fs::write(&script_path, script)
        .map_err(|e| tf!("installer.utils.write_script_error", script_path = script_path.display(), e = e))?;
    let mut cmd = Command::new("cmd");
    apply_windows_no_window(&mut cmd);
    cmd.current_dir(root_dir).args(["/C", "start", "", "/MIN"]);
    cmd.arg(&script_path);
    cmd.spawn().map_err(|e| {
        tf!("installer.utils.run_replacement_script_error", script_path = script_path.display(), e = e)
    })?;
    Ok(())
}

fn ensure_uv_managed_python_environment(
    root_dir: &Path,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    overall_range: (f32, f32),
    cancel: &AtomicBool,
) -> Result<(PathBuf, PathBuf), String> {
    // `ensure_uv_runtime` reports on the installer event channel (it is shared with the
    // install/repair workers), so the update channel is bridged into it here.
    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    let uv_exe = ensure_uv_runtime(
        root_dir,
        &install_tx,
        overall_range.0,
        lerp(overall_range.0, overall_range.1, 0.45),
        cancel,
    )?;
    let managed_venv_dir = root_dir.join("installer_files").join("venv");
    let has_uv_managed_venv = python_manager::detect_python_environment(root_dir)
        .ok()
        .is_some_and(|environment| match environment {
            python_manager::PythonEnvironment::VirtualEnv { root } => root == managed_venv_dir,
            python_manager::PythonEnvironment::CondaEnv { .. }
            | python_manager::PythonEnvironment::StandalonePython { .. } => false,
        })
        && python_manager::resolve_python_executable_in_dir(&managed_venv_dir).is_ok();

    if !has_uv_managed_venv {
        let _ = tx.send(UpdateWorkerEvent::Step(
            t!("installer.utils.creating_new_venv_status").to_string(),
        ));
        if managed_venv_dir.exists() {
            fs::remove_dir_all(&managed_venv_dir).map_err(|e| {
                tf!("installer.utils.remove_old_env_error", managed_venv_dir = managed_venv_dir.display(), e = e)
            })?;
        }
        let uv_env = build_uv_env(root_dir)?;
        run_command_with_retry(
            &uv_exe,
            root_dir,
            &["python", "install", PYTHON_VERSION_REQUEST],
            RetryRun { action_name: &tf!("installer.utils.install_python_action", python_version_request = PYTHON_VERSION_REQUEST), attempts: 2, tx: None, cancel },
            &uv_env.as_env_slice(),
        )?;
        let venv_dir = managed_venv_dir.to_string_lossy().into_owned();
        run_command_with_retry(
            &uv_exe,
            root_dir,
            &["venv", "--python", PYTHON_VERSION_REQUEST, &venv_dir],
            RetryRun { action_name: t!("installer.utils.create_venv_action"), attempts: 2, tx: None, cancel },
            &uv_env.as_env_slice(),
        )?;
    }

    let python_exe = python_manager::resolve_python_executable_in_dir(&managed_venv_dir)?;
    send_update_progress(
        tx,
        1.0,
        t!("installer.utils.python_env_ready"),
        overall_range.1,
        t!("installer.utils.python_env_ready"),
    );
    Ok((uv_exe, python_exe))
}

struct UvEnv {
    python_dir: String,
    cache_dir: String,
}

impl UvEnv {
    fn as_env_slice(&self) -> [(&str, &str); 2] {
        [
            ("UV_PYTHON_INSTALL_DIR", self.python_dir.as_str()),
            ("UV_CACHE_DIR", self.cache_dir.as_str()),
        ]
    }
}

fn build_uv_env(root_dir: &Path) -> Result<UvEnv, String> {
    let uv_root = root_dir.join("installer_files").join("uv");
    let uv_python_dir = uv_root.join("python");
    let uv_cache_dir = uv_root.join("cache");
    fs::create_dir_all(&uv_python_dir)
        .map_err(|e| tf!("installer.utils.create_uv_python_dir_error", uv_python_dir = uv_python_dir.display(), e = e))?;
    fs::create_dir_all(&uv_cache_dir)
        .map_err(|e| tf!("installer.utils.create_uv_cache_dir_error", uv_cache_dir = uv_cache_dir.display(), e = e))?;
    Ok(UvEnv {
        python_dir: uv_python_dir.to_string_lossy().into_owned(),
        cache_dir: uv_cache_dir.to_string_lossy().into_owned(),
    })
}

/// Resolves the installer-managed `uv` executable, downloading and unpacking it
/// into `installer_files/uv` when it is not there yet.
///
/// Reports through the installer event channel; update-flow callers bridge their
/// own channel into it (`UpdateToInstallEventBridge`).
///
/// # Errors
/// Returns a user-facing message when the release asset cannot be fetched,
/// downloaded, extracted, or when no `uv` executable is found afterwards.
fn ensure_uv_runtime(
    root_dir: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    overall_start: f32,
    overall_end: f32,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let uv_dir = root_dir.join("installer_files").join("uv");
    if uv_dir.is_dir()
        && let Ok(uv_exe) = resolve_uv_executable(&uv_dir)
    {
        send_progress(tx, 1.0, t!("installer.utils.uv_already_installed"), overall_end, t!("installer.utils.uv_ready"));
        return Ok(uv_exe);
    }

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.downloading_uv_for_update_status").to_string(),
    ));
    fs::create_dir_all(&uv_dir)
        .map_err(|e| tf!("installer.utils.create_uv_dir_error", uv_dir = uv_dir.display(), e = e))?;
    let downloads_dir = root_dir.join("installer_files").join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| tf!("installer.utils.create_downloads_dir_error", downloads_dir = downloads_dir.display(), e = e))?;
    let asset = fetch_latest_uv_asset(detect_platform()?, &detect_arch()?, cancel)?;
    let archive_path = downloads_dir.join(&asset.name);
    download_asset(
        &AssetDownload {
            asset: &asset,
            dst_path: &archive_path,
            label_prefix: "uv",
            overall_range: (overall_start, lerp(overall_start, overall_end, 0.55)),
            required_sha256: None,
        },
        tx,
        cancel,
    )?;
    if uv_dir.exists() {
        fs::remove_dir_all(&uv_dir)
            .map_err(|e| tf!("installer.utils.clear_uv_dir_error", uv_dir = uv_dir.display(), e = e))?;
    }
    fs::create_dir_all(&uv_dir)
        .map_err(|e| tf!("installer.utils.create_uv_dir_error", uv_dir = uv_dir.display(), e = e))?;
    extract_archive(
        &archive_path,
        &uv_dir,
        tx,
        t!("installer.utils.extract_uv_stage"),
        lerp(overall_start, overall_end, 0.55),
        overall_end,
    )?;
    flatten_single_root_dir(&uv_dir)?;
    resolve_uv_executable(&uv_dir)
}

/// Reads `General.ai_install_type` from `root_dir/user_config.json` as a read-only probe.
///
/// Never creates or rewrites the document (no default backfill): the only value read here
/// has the default `None`, which is also what an absent key yields. An absent document
/// means `None`; an unreadable or malformed one is logged and also treated as `None`, so
/// the update continues without the Full-only PyTorch step.
fn read_current_ai_install_type(root_dir: &Path) -> config::AiInstallType {
    let doc = ms_docstore::DocRef::new(root_dir.join(config::USER_CONFIG_FILE), ms_docstore::DocKind::UserConfig);
    match ms_docstore::read_value(&doc) {
        Ok(Some(settings)) => config::AiInstallType::from_user_settings(&settings),
        Ok(None) => config::AiInstallType::None,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[update] failed to read the AI install type of '{}': {err}; assuming None",
                root_dir.display()
            ));
            config::AiInstallType::None
        }
    }
}

fn maybe_update_torch(
    root_dir: &Path,
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    torch_selection: Option<TorchInstallSelection>,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    overall_range: (f32, f32),
    cancel: &AtomicBool,
) -> Result<UpdateContinuationOutcome, String> {
    let installed = freeze_installed_packages(pip_runner, python_exe, root_dir, Some(tx))?;
    // Same predicate as the repair worker and the readiness check: one definition of
    // "the installed PyTorch satisfies the recorded choice".
    if installed_torch_is_current(&installed, &read_torch_choice_record(root_dir)) {
        send_update_progress(
            tx,
            1.0,
            t!("installer.utils.pytorch_already_current_status"),
            overall_range.1,
            t!("installer.utils.pytorch_stage_done"),
        );
        return Ok(UpdateContinuationOutcome::Completed);
    }

    let selection = if let Some(selection) = torch_selection {
        selection
    } else {
        match detect_torch_preflight() {
            TorchPreflightResult::Skip { reason } => {
                send_update_console_line(tx, format!("[PyTorch] {reason}"));
                TorchInstallSelection::SkipCpu
            }
            TorchPreflightResult::Choose(prompt) => {
                let _ = tx.send(UpdateWorkerEvent::TorchChoiceRequired(prompt));
                return Ok(UpdateContinuationOutcome::WaitingForTorchChoice);
            }
        }
    };

    install_torch_stage(
        root_dir,
        pip_runner,
        python_exe,
        &selection,
        &UpdateToInstallEventBridge { tx }.sender(),
        (overall_range.0, overall_range.1),
        cancel,
    )?;
    Ok(UpdateContinuationOutcome::Completed)
}

fn install_missing_dependencies_for_update(
    root_dir: &Path,
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    install_type: config::AiInstallType,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    overall_range: (f32, f32),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let installed = freeze_installed_packages(pip_runner, python_exe, root_dir, Some(tx))?;
    // PyTorch itself is deliberately absent from this list: the update flow installs
    // or refreshes it through the dedicated Torch stage (`maybe_update_torch`), which
    // needs the right wheel index.
    let required = required_dependency_specs(install_type);
    let missing = missing_dependency_specs(&required, &installed);
    // Keep the installed torch (any flavour) through these PyPI installs.
    let constraints = write_torch_constraints(root_dir, &installed)?;
    let constraint_args = constraint_args(constraints.as_deref());

    if missing.is_empty() {
        send_update_progress(
            tx,
            1.0,
            t!("installer.utils.python_deps_current_status"),
            overall_range.1,
            t!("installer.utils.deps_current_status"),
        );
        return Ok(());
    }

    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    let total = missing.len().max(1) as f32;
    for (idx, dep) in missing.iter().enumerate() {
        let start_ratio = idx as f32 / total;
        let end_ratio = (idx + 1) as f32 / total;
        let _ = tx.send(UpdateWorkerEvent::Step(tf!("installer.utils.installing_missing_dep_status", dep = dep)));
        let mut args = vec![*dep];
        args.extend(constraint_args.iter().map(String::as_str));
        run_pip_install_with_retry(
            pip_runner,
            python_exe,
            root_dir,
            &args,
            RetryRun { action_name: &tf!("installer.utils.install_missing_dep_action", dep = dep), attempts: 3, tx: Some(&install_tx), cancel },
        )?;
        send_update_progress(
            tx,
            end_ratio,
            tf!("installer.utils.installed_dep_status", dep = dep),
            lerp(overall_range.0, overall_range.1, end_ratio),
            tf!("installer.utils.deps_progress", idx = idx + 1, missing = missing.len()),
        );
        if start_ratio == 0.0 {
            send_update_console_line(tx, "[Update] Missing dependencies are being installed");
        }
    }
    Ok(())
}

/// Downloads `app_release`'s archive (verified against its `SHA256SUMS`) and merges it over
/// `root_dir`, keeping the already-replaced executable.
fn download_and_extract_app_archive_for_update(
    root_dir: &Path,
    app_release: &ArchiveRelease,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    overall_range: (f32, f32),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let _ = tx.send(UpdateWorkerEvent::Step(
        t!("installer.utils.downloading_app_zip_status").to_string(),
    ));
    let downloads_dir = root_dir.join("installer_files").join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| tf!("installer.utils.create_downloads_dir_error", downloads_dir = downloads_dir.display(), e = e))?;
    // Verified against the release's `SHA256SUMS` before it is published, i.e. before the
    // extraction below can see it (`release.rs`).
    let app_checksums = app_release.checksums.fetch(cancel)?;
    let app_asset = &app_release.archive;
    let app_zip_sha256 = app_checksums.pin_for(app_asset)?;
    let archive_path = downloads_dir.join(&app_asset.name);
    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    download_asset(
        &AssetDownload {
            asset: app_asset,
            dst_path: &archive_path,
            label_prefix: APP_ZIP_ASSET_NAME,
            overall_range: (overall_range.0, lerp(overall_range.0, overall_range.1, 0.45)),
            required_sha256: Some(app_zip_sha256),
        },
        &install_tx,
        cancel,
    )?;

    let staging_dir = root_dir.join("installer_files").join("update_extract");
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)
            .map_err(|e| tf!("installer.utils.clear_staging_dir_error", staging_dir = staging_dir.display(), e = e))?;
    }
    fs::create_dir_all(&staging_dir)
        .map_err(|e| tf!("installer.utils.create_staging_dir_error", staging_dir = staging_dir.display(), e = e))?;
    extract_archive(
        &archive_path,
        &staging_dir,
        &install_tx,
        t!("installer.utils.extract_app_zip_stage"),
        lerp(overall_range.0, overall_range.1, 0.45),
        lerp(overall_range.0, overall_range.1, 0.82),
    )?;
    flatten_single_root_dir(&staging_dir)?;
    remove_staged_platform_binary(&staging_dir)?;
    merge_dir_contents(&staging_dir, root_dir)?;
    let _ = fs::remove_dir_all(&staging_dir);
    send_update_progress(
        tx,
        1.0,
        t!("installer.utils.app_archive_extracted_status"),
        overall_range.1,
        t!("installer.utils.app_files_updated_status"),
    );
    Ok(())
}

/// Removes the platform executable that was shipped inside the extracted
/// `ManhwaStudio.zip` staging tree so the merge step cannot overwrite the
/// freshly-updated executable with a stale copy from the archive.
///
/// The archive carries the executable under its ON-DISK name
/// (`platform_executable_file_name`), which on macOS differs from the
/// `_macos`-suffixed release download name; using the on-disk name here is what
/// makes the strip fire on every platform.
fn remove_staged_platform_binary(staging_dir: &Path) -> Result<(), String> {
    let binary_path = staging_dir.join(platform_executable_file_name());
    if binary_path.is_file() {
        fs::remove_file(&binary_path).map_err(|e| {
            tf!("installer.utils.delete_staged_executable_error", binary_path = binary_path.display(), e = e)
        })?;
    }
    Ok(())
}

/// Returns the packages installed in `python_exe`'s environment as a map of
/// normalized package name -> version, taken from `pip freeze`.
///
/// `tx` is optional: update/repair workers pass their console channel so the
/// executed command is echoed into the installer console, while non-interactive
/// callers (the `--check-venv` console flow) pass `None` and stay silent.
///
/// # Errors
/// Returns the command output when `pip freeze` cannot be started or exits with a
/// failure status. A failed probe is never reported as "nothing installed".
pub fn freeze_installed_packages(
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    root_dir: &Path,
    tx: Option<&mpsc::Sender<UpdateWorkerEvent>>,
) -> Result<HashMap<String, String>, String> {
    let (executable, args) = match pip_runner {
        PipInstallRunner::Uv(uv_exe) => {
            let python_arg = python_exe.to_string_lossy().into_owned();
            (
                uv_exe.as_path(),
                vec![
                    "pip".to_string(),
                    "freeze".to_string(),
                    "--python".to_string(),
                    python_arg,
                ],
            )
        }
        PipInstallRunner::PythonPip => (
            python_exe,
            vec!["-m".to_string(), "pip".to_string(), "freeze".to_string()],
        ),
    };
    let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
    if let Some(tx) = tx {
        send_update_console_line(tx, format!("$ {} {}", executable.display(), args.join(" ")));
    }
    let (status, output) = run_command_streaming(executable, root_dir, &args_ref, None, &[])?;
    if !status.success() {
        return Err(tf!("installer.utils.uv_pip_freeze_error", output = output.trim()));
    }
    Ok(parse_pip_freeze_packages(&output))
}

fn parse_pip_freeze_packages(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(parse_pip_freeze_line)
        .collect::<HashMap<_, _>>()
}

fn parse_pip_freeze_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("-e ") {
        return None;
    }
    let (name, version) = trimmed.split_once("==")?;
    Some((normalize_package_name(name), version.trim().to_string()))
}

/// Returns the full dependency spec list a working environment must provide for
/// `install_type`.
///
/// `Base` requires the base group; `Full` additionally requires the torch-dependent
/// extras. `None` (no recorded install type) is treated like `Base`: callers that
/// need a user decision must detect the missing install type themselves — this
/// function never guesses a *smaller* requirement than reality.
///
/// PyTorch itself is NOT in this list: its requirement is a version and flavour rule against
/// the recorded choice, which a version specifier alone cannot express. Callers checking a `Full`
/// environment must also consult [`installed_torch_is_current`].
///
/// The returned specs are raw dependency strings (they may carry version pins and
/// environment markers); filter them with [`missing_dependency_specs`].
#[must_use]
pub fn required_dependency_specs(
    install_type: config::AiInstallType,
) -> Vec<&'static str> {
    let mut required = BASE_DEPENDENCIES.to_vec();
    match install_type {
        config::AiInstallType::None | config::AiInstallType::Base => {}
        config::AiInstallType::Full => required.extend(TORCH_DEPENDENCIES.iter().copied()),
    }
    required
}

/// Returns the subset of `required` that still has to be installed.
///
/// A spec is skipped when its environment marker does not apply to the current
/// platform (e.g. `onnxruntime-directml; platform_system == "Windows"` off
/// Windows). Otherwise it is missing when its package — name normalized the same way
/// as `pip freeze` output — is not a key of `installed`, OR when the installed version
/// does not satisfy the spec's version specifier (`transformers>=4.57.1,<4.58` with
/// 5.20.1 installed). Installing the returned spec then brings that package to the pin,
/// so the readiness check and the repair / update workers share one notion of
/// "the environment has what this build needs".
///
/// Fail-open toward installing: a spec this module cannot parse, or an installed version
/// that is not PEP 440, counts as missing (logged), never as satisfied.
#[must_use]
pub(crate) fn missing_dependency_specs<'a>(
    required: &[&'a str],
    installed: &HashMap<String, String>,
) -> Vec<&'a str> {
    required
        .iter()
        .copied()
        .filter(|dep| dependency_marker_matches_current_platform(dep))
        .filter(|dep| !dependency_spec_satisfied(dep, installed))
        .collect()
}

/// Whether `installed` holds `dep`'s package at a version its specifier accepts.
///
/// See [`missing_dependency_specs`] for the fail-open rule; this function does not look at
/// the environment marker.
fn dependency_spec_satisfied(dep: &str, installed: &HashMap<String, String>) -> bool {
    let requirement = match Requirement::parse(dep) {
        Ok(requirement) => requirement,
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
                "[Deps] cannot read dependency spec '{dep}': {err}. Treating it as missing so the installer asks pip for it."
            ));
            return false;
        }
    };
    let Some(installed_version) = installed.get(&normalize_package_name(requirement.name)) else {
        return false;
    };
    if requirement.specifier.is_any() {
        return true;
    }
    match Pep440Version::parse(installed_version) {
        Ok(version) => requirement.specifier.is_satisfied_by(&version),
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[Deps] installed version of '{}' is unreadable ({err}); reinstalling it at '{dep}'.",
                requirement.name
            ));
            false
        }
    }
}

/// PyPI distributions that all ship the same importable `onnxruntime` module.
///
/// Mirrors the distribution list probed by `ms_sysprobe::ai_install_probe`, plus the WebGPU
/// build. Exactly one of them can be installed at a time (they collide on the module),
/// so from the runtime's point of view any single one satisfies "onnxruntime is
/// available".
const ONNXRUNTIME_DISTRIBUTIONS: &[&str] = &[
    "onnxruntime",
    "onnxruntime-gpu",
    "onnxruntime-directml",
    "onnxruntime-openvino",
    "onnxruntime-migraphx",
    "onnxruntime-webgpu",
];

/// PyPI distributions that all ship the same importable `cv2` module.
///
/// The app installs `opencv-python-headless`, but an environment built by an older installer
/// (which named `opencv-python`) or by hand may provide `cv2` through another of them; they
/// collide on the module just like the onnxruntime builds.
const OPENCV_DISTRIBUTIONS: &[&str] = &[
    "opencv-python-headless",
    "opencv-python",
    "opencv-contrib-python-headless",
    "opencv-contrib-python",
];

/// The families of distributions that provide one Python module each.
const EQUIVALENT_DISTRIBUTION_FAMILIES: &[&[&str]] = &[ONNXRUNTIME_DISTRIBUTIONS, OPENCV_DISTRIBUTIONS];

/// Returns the interchangeable distributions of `normalized_name`, if it belongs to a
/// family where several distributions provide the same Python module.
///
/// This table is READINESS-ONLY and intentionally softer than installation. Installing
/// must pick ONE concrete distribution (`BASE_DEPENDENCIES` names `onnxruntime` off
/// Windows and `onnxruntime-directml` on Windows); checking only has to answer "will
/// the runtime find this module?", and a user or a GPU-specific setup may legitimately
/// have chosen a different build of the same thing. Treating those as missing would
/// offer a repair on every launch of a perfectly working environment.
fn equivalent_distributions(normalized_name: &str) -> Option<&'static [&'static str]> {
    EQUIVALENT_DISTRIBUTION_FAMILIES.iter().copied().find(|family| {
        family
            .iter()
            .any(|candidate| normalize_package_name(candidate) == normalized_name)
    })
}

/// Whether `dep` is satisfied by an interchangeable distribution in `installed`.
///
/// Returns `false` for every dependency outside [`equivalent_distributions`], and for a
/// spec with a version specifier: a pin names one distribution's version line, which another
/// distribution's version says nothing about.
fn dependency_satisfied_by_equivalent(dep: &str, installed: &HashMap<String, String>) -> bool {
    let Ok(requirement) = Requirement::parse(dep) else {
        return false;
    };
    if !requirement.specifier.is_any() {
        return false;
    }
    equivalent_distributions(&normalize_package_name(requirement.name)).is_some_and(|variants| {
        variants
            .iter()
            .any(|variant| installed.contains_key(&normalize_package_name(variant)))
    })
}

/// Returns the specs from `required` that a READINESS check must report as missing.
///
/// Same as [`missing_dependency_specs`], except that a spec is also considered
/// satisfied when an interchangeable distribution of it is installed (see
/// [`equivalent_distributions`]). Installation deliberately keeps the strict rule:
/// this softer view only decides whether to bother the user with a repair.
#[must_use]
pub fn missing_specs_for_readiness<'a>(
    required: &[&'a str],
    installed: &HashMap<String, String>,
) -> Vec<&'a str> {
    missing_dependency_specs(required, installed)
        .into_iter()
        .filter(|dep| !dependency_satisfied_by_equivalent(dep, installed))
        .collect()
}

/// Whether `dep`'s PEP 508 environment marker applies to the platform this binary
/// runs on.
///
/// Supported markers — the only ones used by [`BASE_DEPENDENCIES`] and
/// [`TORCH_DEPENDENCIES`] — are exactly `platform_system == "Windows"` and
/// `platform_system != "Windows"`. A dependency with NO marker, or with any marker
/// this function does not recognize, is reported as applying to the current platform.
///
/// That fail-open direction is deliberate: an unrecognized marker can only lead to a
/// package being considered required where it is not (an unnecessary install or an
/// unnecessary repair offer), never to a package being skipped and an environment
/// wrongly called ready. A full PEP 508 evaluator is out of scope here — the two
/// markers above are the whole contract of the embedded dependency lists.
///
/// Adding a dependency with a different marker REQUIRES extending this function.
fn dependency_marker_matches_current_platform(dep: &str) -> bool {
    let Some((_, marker)) = dep.split_once(';') else {
        return true;
    };
    let marker = marker.trim();
    match marker {
        "platform_system != \"Windows\"" => !cfg!(target_os = "windows"),
        "platform_system == \"Windows\"" => cfg!(target_os = "windows"),
        _ => true,
    }
}

fn normalize_package_name(name: &str) -> String {
    name.trim().replace(['_', '.'], "-").to_ascii_lowercase()
}

fn prepare_install_root_dir(root_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(root_dir).map_err(|e| {
        tf!("installer.utils.create_install_dir_error", root_dir = root_dir.display(), e = e)
    })?;
    prepare_windows_install_root_acl(root_dir)
}

#[cfg(target_os = "windows")]
fn prepare_windows_install_root_acl(root_dir: &Path) -> Result<(), String> {
    if is_windows_all_users_install_dir(root_dir) {
        grant_windows_users_modify_acl_with_inheritance(root_dir)?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn prepare_windows_install_root_acl(_root_dir: &Path) -> Result<(), String> {
    Ok(())
}

fn alloc_progress_range(cursor: &mut f32, span: f32) -> (f32, f32) {
    let start = *cursor;
    let end = (*cursor + span).clamp(0.0, 1.0);
    *cursor = end;
    (start, end)
}

fn copy_launcher_to_install_dir(
    launcher_exe_path: Option<&Path>,
    target_root: &Path,
    tx: &mpsc::Sender<InstallEvent>,
) -> Result<(), String> {
    let source = match launcher_exe_path {
        Some(path) => path.to_path_buf(),
        None => env::current_exe()
            .map_err(|e| tf!("installer.utils.determine_current_exe_error", e = e))?,
    };
    let file_name = source
        .file_name()
        .ok_or_else(|| tf!("installer.utils.get_filename_error", source = source.display()))?;
    let target = target_root.join(file_name);

    if paths_point_to_same_file(&source, &target) {
        send_console_line(
            tx,
            tf!("installer.utils.exe_already_in_target_error", target = target.display()),
        );
        return Ok(());
    }

    fs::copy(&source, &target).map_err(|e| {
        tf!("installer.utils.copy_exe_error", source = source.display(), target = target.display(), e = e)
    })?;

    if let Ok(meta) = fs::metadata(&source) {
        let _ = fs::set_permissions(&target, meta.permissions());
    }

    send_console_line(
        tx,
        tf!("installer.utils.copied_exe_log", source = source.display(), target = target.display()),
    );
    Ok(())
}

fn paths_point_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    if !right.exists() {
        return false;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left_canon), Ok(right_canon)) => left_canon == right_canon,
        _ => false,
    }
}

pub(super) fn load_embedded_icon_data() -> Option<egui::IconData> {
    let image = image::load_from_memory(EMBEDDED_APP_ICON_ICO)
        .or_else(|_| image::load_from_memory(EMBEDDED_APP_ICON_PNG))
        .ok()?;
    let rgba = image.into_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

fn write_embedded_app_icon(target_root: &Path) -> Result<(), String> {
    let icon_path = target_root.join("app_icon_512.png");
    fs::write(&icon_path, EMBEDDED_APP_ICON_PNG).map_err(|e| {
        tf!("installer.utils.write_icon_error", icon_path = icon_path.display(), e = e)
    })
}

pub(super) fn default_local_install_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        let base = env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("USERPROFILE")
                    .map(PathBuf::from)
                    .map(|p| p.join("AppData").join("Roaming"))
            })
            .ok_or_else(|| t!("installer.utils.appdata_not_found_error").to_string())?;
        return Ok(base.join(INSTALL_SUBDIR_NAME));
    }

    #[cfg(target_os = "macos")]
    {
        let home = home_dir_path()?;
        return Ok(home
            .join("Library")
            .join("Application Support")
            .join(INSTALL_SUBDIR_NAME));
    }

    // Android has no XDG data dir or system-wide /opt: it falls through to the typed unsupported-OS error.
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    {
        let base = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or(home_dir_path()?.join(".local").join("share"));
        return Ok(base.join(INSTALL_SUBDIR_NAME));
    }

    #[allow(unreachable_code)]
    Err(t!("installer.utils.unsupported_os_error").to_string())
}

pub(super) fn default_all_users_install_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        let base = env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        return Ok(base.join(INSTALL_SUBDIR_NAME));
    }

    #[cfg(target_os = "macos")]
    {
        return Ok(PathBuf::from("/Applications").join(INSTALL_SUBDIR_NAME));
    }

    // Same: no system-wide install location on Android.
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
    {
        return Ok(PathBuf::from("/opt").join(INSTALL_SUBDIR_NAME));
    }

    #[allow(unreachable_code)]
    Err(t!("installer.utils.unsupported_os_error").to_string())
}

// Used only by the macOS/Linux install-dir defaults (none on Android or Windows).
#[cfg(not(any(target_os = "windows", target_os = "android")))]
fn home_dir_path() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| t!("installer.utils.home_dir_not_found_error").to_string())
}

pub(super) fn has_write_access_for_install(target_dir: &Path) -> bool {
    let probe_parent = if target_dir.is_dir() {
        target_dir.to_path_buf()
    } else {
        target_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| target_dir.to_path_buf())
    };

    if fs::create_dir_all(&probe_parent).is_err() {
        return false;
    }

    let suffix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe_file = probe_parent.join(format!(
        ".manhwastudio_write_probe_{}_{}",
        std::process::id(),
        suffix
    ));

    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe_file)
    {
        Ok(_) => {
            let _ = fs::remove_file(probe_file);
            true
        }
        Err(_) => false,
    }
}

/// Whether an existing copy's install directory can be written to, and if not, whether
/// administrator rights are still a plausible remedy.
///
/// Program Files installs are normally writable WITHOUT elevation: the installer grants the
/// built-in Users group inheritable Modify rights once, at install time
/// (`grant_windows_users_modify_acl_with_inheritance`, run from `prepare_install_root_dir`),
/// which is why ordinary updates need no UAC prompt. A negative probe is therefore the
/// exception — an install made before that ACL behavior existed, an ACL changed afterwards, a
/// per-user install owned by another Windows account, or a folder held by policy or antivirus.
///
/// The `test` arm of the `cfg` exists so this platform-independent decision is compiled and
/// exercised by `cargo test` on non-Windows development hosts; the feature itself is reachable
/// only from the Windows existing-install window.
#[cfg(any(target_os = "windows", test))]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum ReplaceTargetAccess {
    /// A write probe inside the install directory succeeded; the replacement may proceed.
    Writable,
    /// The probe failed while this process runs unelevated, so relaunching as administrator
    /// is one of the remedies worth offering the user.
    Blocked,
    /// The probe failed even though this process is ALREADY elevated: administrator rights
    /// are not the missing piece, so the message must not suggest them.
    BlockedWhileElevated,
}

/// Classifies write access to an install directory from the two facts the caller probed.
///
/// Pure decision function, deliberately separated from the probes (`has_write_access_for_install`
/// and `is_running_elevated`) so it can be tested on any host.
#[cfg(any(target_os = "windows", test))]
#[must_use]
pub(super) fn classify_replace_target_access(directory_is_writable: bool, running_elevated: bool) -> ReplaceTargetAccess {
    if directory_is_writable {
        ReplaceTargetAccess::Writable
    } else if running_elevated {
        ReplaceTargetAccess::BlockedWhileElevated
    } else {
        ReplaceTargetAccess::Blocked
    }
}

/// Builds the name of the staging file used to replace `target_file_name` in place.
///
/// The name introduces no path separator, so the staging file lands in the target's OWN
/// directory and the final rename cannot cross a volume boundary. It is unique per process
/// and per attempt so two replacements never collide, and carries the `.part` suffix this
/// module already uses for staged downloads.
#[cfg(any(target_os = "windows", test))]
#[must_use]
pub(super) fn staged_replacement_file_name(target_file_name: &str, process_id: u32, unique_suffix: u128) -> String {
    format!("{target_file_name}.replace-{process_id}-{unique_suffix}.part")
}

/// Replaces the executable at `target` with a copy of `source`, atomically.
///
/// `source` is copied to a staging file NEXT TO `target` and then renamed over it, so a failed
/// or interrupted copy can never leave `target` missing or truncated; the staging file is
/// removed on every failure path. `source` is normally `env::current_exe()`.
///
/// This replaces the EXECUTABLE ONLY: the target's Python payload (`ai_backend.py`, `modules/`)
/// and its virtual environment are deliberately left untouched — see the existing-install
/// window contract in `MODULE_README.md`.
///
/// Blocking filesystem work: call it from a worker thread, never from the GUI thread.
///
/// # Errors
/// Returns a localized message when `target` has no parent directory or file name, when it
/// resolves to the same file as `source`, when the copy fails, or when the rename fails. A
/// rename refused because the destination is in use (Windows keeps a running image locked)
/// gets its own message naming that cause instead of a bare OS error.
#[cfg(any(target_os = "windows", test))]
pub(super) fn replace_executable_with_local_file(source: &Path, target: &Path) -> Result<(), String> {
    if paths_point_to_same_file(source, target) {
        return Err(tf!("installer.utils.replace_same_file_error", target = target.display()));
    }
    let target_dir = target
        .parent()
        .ok_or_else(|| tf!("installer.utils.replace_target_path_error", target = target.display()))?;
    let target_file_name = target
        .file_name()
        .ok_or_else(|| tf!("installer.utils.replace_target_path_error", target = target.display()))?
        .to_string_lossy()
        .into_owned();
    let unique_suffix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let staged = target_dir.join(staged_replacement_file_name(
        &target_file_name,
        std::process::id(),
        unique_suffix,
    ));

    if let Err(e) = fs::copy(source, &staged) {
        // Cleanup of our own staging file: a partial copy must never be left behind, and
        // there is nothing useful to report if the removal of an already-failed write fails.
        let _ = fs::remove_file(&staged);
        return Err(tf!("installer.utils.replace_stage_copy_error", source = source.display(), staged = staged.display(), e = e));
    }
    // Carry the source's mode over so a Unix copy stays executable. On Windows permissions
    // come from the directory ACL, so a failure here changes nothing and is not worth an abort.
    if let Ok(metadata) = fs::metadata(source) {
        let _ = fs::set_permissions(&staged, metadata.permissions());
    }

    if let Err(e) = fs::rename(&staged, target) {
        // Same reasoning as above: drop the staging file, keep the rename error.
        let _ = fs::remove_file(&staged);
        return Err(if error_means_destination_in_use(&e) {
            tf!("installer.utils.replace_target_in_use_error", target = target.display(), e = e)
        } else {
            tf!("installer.utils.replace_rename_error", target = target.display(), staged = staged.display(), e = e)
        });
    }
    Ok(())
}

/// Whether a failed rename onto an existing executable means the destination is currently in
/// use rather than genuinely inaccessible.
///
/// Windows keeps a running image locked and reports `ERROR_ACCESS_DENIED` (5) or
/// `ERROR_SHARING_VIOLATION` (32) for a `MoveFileEx` over it. Because the install directory's
/// writability is probed BEFORE the copy, those two codes at the rename step point at the file
/// being open, not at missing rights.
#[cfg(target_os = "windows")]
fn error_means_destination_in_use(error: &std::io::Error) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    matches!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION))
}

/// Non-Windows counterpart used only by the host test build: other platforms replace a running
/// binary without complaint, so no error code maps to "destination in use".
#[cfg(all(test, not(target_os = "windows")))]
fn error_means_destination_in_use(_error: &std::io::Error) -> bool {
    false
}

// The Windows probe lives in `ms_os_integration`; re-exported so `install.rs` (which glob-imports
// this module) sees one `is_running_elevated` on every OS next to the unix/other variants below.
#[cfg(target_os = "windows")]
pub(super) use ms_os_integration::windows::elevation::is_running_elevated;

#[cfg(all(unix, not(target_os = "windows")))]
pub(super) fn is_running_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(any(unix, target_os = "windows")))]
pub(super) fn is_running_elevated() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub(super) fn relaunch_self_elevated(root_dir: &Path, target_dir: &Path) -> Result<(), String> {
    let args = format!(
        "--continue-install --continue-install-target {}",
        quote_windows_arg(target_dir.to_string_lossy().as_ref())
    );
    relaunch_self_elevated_with_args(root_dir, &args).map_err(|e| e.user_message())
}

#[cfg(target_os = "macos")]
pub(super) fn relaunch_self_elevated(root_dir: &Path, target_dir: &Path) -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| tf!("installer.utils.determine_exe_error", e = e))?;
    let cmd = format!(
        "cd {} && {} --continue-install --continue-install-target {}",
        shell_quote(root_dir),
        shell_quote(&exe),
        shell_quote(target_dir),
    );
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        escape_applescript(&cmd)
    );
    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn()
        .map_err(|e| tf!("installer.utils.osascript_elevation_error", e = e))?;
    Ok(())
}

// pkexec/sudo do not exist on Android; it takes the unsupported-OS arm below.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android")), not(target_os = "windows")))]
pub(super) fn relaunch_self_elevated(root_dir: &Path, target_dir: &Path) -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| tf!("installer.utils.determine_exe_error", e = e))?;

    let pkexec_result = Command::new("pkexec")
        .current_dir(root_dir)
        .arg(&exe)
        .arg("--continue-install")
        .arg("--continue-install-target")
        .arg(target_dir)
        .spawn();
    if pkexec_result.is_ok() {
        return Ok(());
    }

    Command::new("sudo")
        .current_dir(root_dir)
        .arg(&exe)
        .arg("--continue-install")
        .arg("--continue-install-target")
        .arg(target_dir)
        .spawn()
        .map_err(|e| tf!("installer.utils.sudo_elevation_error", e = e))?;
    Ok(())
}

#[cfg(any(target_os = "android", not(any(unix, target_os = "windows"))))]
pub(super) fn relaunch_self_elevated(_root_dir: &Path, _target_dir: &Path) -> Result<(), String> {
    Err(t!("installer.utils.elevation_unsupported_os_error").to_string())
}

/// Creates the all-users Start Menu shortcut of the copy in `install_dir`, relaunching this
/// executable elevated (with `continue_create_start_menu_shortcut` set) when needed. Windows only:
/// startup rejects the flags elsewhere. Errors are localized user texts.
#[cfg(target_os = "windows")]
pub fn run_windows_create_start_menu_shortcut_for_install(
    install_dir: &Path,
    continue_create_start_menu_shortcut: bool,
) -> Result<(), String> {
    if create_start_menu_shortcut_requires_elevation(install_dir) {
        if continue_create_start_menu_shortcut {
            return Err(
                t!("installer.utils.start_menu_no_admin_error")
                    .to_string(),
            );
        }
        let args = build_windows_create_start_menu_shortcut_args(install_dir, true);
        relaunch_self_elevated_with_args(install_dir, &args).map_err(|e| e.user_message())?;
        return Ok(());
    }

    create_windows_start_menu_shortcut(install_dir).map(|_| ()).map_err(|e| e.user_message())
}

#[cfg(target_os = "windows")]
fn create_start_menu_shortcut_requires_elevation(install_dir: &Path) -> bool {
    is_windows_all_users_install_dir(install_dir) && !is_running_elevated()
}

#[cfg(target_os = "windows")]
fn build_windows_create_start_menu_shortcut_args(
    install_dir: &Path,
    continue_create_start_menu_shortcut: bool,
) -> String {
    let mut args = String::from("--create-start-menu-shortcut-install-dir ");
    args.push_str(&quote_windows_arg(install_dir.to_string_lossy().as_ref()));
    if continue_create_start_menu_shortcut {
        args.push(' ');
        args.push_str("--continue-create-start-menu-shortcut");
    }
    args
}

#[cfg(target_os = "windows")]
fn build_windows_uninstall_args(
    uninstall_signal_file: Option<&Path>,
    continue_uninstall: bool,
) -> String {
    let mut args = String::from("--uninstall");
    if continue_uninstall {
        args.push(' ');
        args.push_str("--continue-uninstall");
    }
    if let Some(signal_file) = uninstall_signal_file {
        args.push(' ');
        args.push_str("--uninstall-signal-file ");
        args.push_str(&quote_windows_arg(signal_file.to_string_lossy().as_ref()));
    }
    args
}

/// Windows integration of a finished install: per-user installs get only the HKCU "Open
/// with" entry; all-users installs get the App Paths + Uninstall entries (`display_version` =
/// the installed program's version core as `DisplayVersion`), "Open with" and the
/// all-users Start-menu shortcut.
#[cfg(target_os = "windows")]
fn finalize_windows_post_install(
    root_dir: &Path,
    display_version: &str,
    tx: &mpsc::Sender<InstallEvent>,
    overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    let launcher_path = resolve_windows_launcher_target(root_dir).map_err(|e| e.user_message())?;
    if !is_windows_all_users_install_dir(root_dir) {
        // A per-user install gets exactly one piece of Windows integration: the HKCU "Open with"
        // entry (user decision Q3 of the single-image plan). No App Paths, no Uninstall entry, no
        // Start Menu shortcut — those stay all-users only.
        register_windows_open_with_best_effort(windows_uninstall_registry_root(root_dir), &launcher_path, tx);
        send_progress(
            tx,
            1.0,
            t!("installer.utils.windows_integration_registry_stage"),
            overall_end,
            t!("installer.utils.registry_updated_status"),
        );
        return Ok(());
    }

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.windows_integration_setup_status").to_string(),
    ));
    send_progress(
        tx,
        0.10,
        t!("installer.utils.windows_integration_prepare_stage"),
        lerp_progress(overall_start, overall_end, 0.10),
        t!("installer.utils.prepare_postinstall_status"),
    );

    register_windows_install_in_registry(root_dir, &launcher_path, display_version).map_err(|e| e.user_message())?;
    send_console_line(
        tx,
        t!("installer.utils.registry_entries_added_log").to_string(),
    );
    register_windows_open_with_best_effort(windows_uninstall_registry_root(root_dir), &launcher_path, tx);
    send_progress(
        tx,
        0.75,
        t!("installer.utils.windows_integration_registry_stage"),
        lerp_progress(overall_start, overall_end, 0.75),
        t!("installer.utils.registry_updated_status"),
    );

    let start_menu_shortcut = create_windows_start_menu_shortcut(root_dir).map_err(|e| e.user_message())?;
    send_console_line(
        tx,
        tf!("installer.utils.start_menu_shortcut_created_log", start_menu_shortcut = start_menu_shortcut.display()),
    );
    send_progress(
        tx,
        1.0,
        t!("installer.utils.windows_integration_done_stage"),
        overall_end,
        t!("installer.utils.registry_start_menu_configured_status"),
    );

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn finalize_windows_post_install(
    _root_dir: &Path,
    _display_version: &str,
    tx: &mpsc::Sender<InstallEvent>,
    _overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    send_progress(
        tx,
        1.0,
        t!("installer.utils.windows_integration_skip_stage"),
        overall_end,
        t!("installer.utils.windows_integration_not_applied_status"),
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn grant_windows_users_modify_acl_with_inheritance(install_dir: &Path) -> Result<(), String> {
    let mut cmd = Command::new("icacls");
    apply_windows_no_window(&mut cmd);
    let status = cmd
        .arg(install_dir)
        .arg("/inheritance:e")
        .arg("/grant")
        .arg("*S-1-5-32-545:(OI)(CI)M")
        .arg("/C")
        .arg("/Q")
        .status()
        .map_err(|e| tf!("installer.utils.run_icacls_error", e = e))?;
    if !status.success() {
        return Err(tf!("installer.utils.icacls_exit_error", status = status.code().unwrap_or(-1)));
    }
    Ok(())
}

/// Registers "Open with" without ever failing the install: the entry is optional and every
/// mandatory step has already succeeded when this runs. Success and failure both reach the
/// session log and the installer console; a failure leaves the install complete.
#[cfg(target_os = "windows")]
fn register_windows_open_with_best_effort(
    registry_root: &str,
    launcher_path: &Path,
    tx: &mpsc::Sender<InstallEvent>,
) {
    match register_windows_open_with(registry_root, launcher_path).map_err(|e| e.user_message()) {
        Ok(()) => {
            ms_log::runtime_log::log_info(format!(
                "[windows-install] registered \"Open with\" entry under {registry_root} for {}",
                launcher_path.display()
            ));
            send_console_line(tx, tf!("installer.utils.open_with_registered_log", root = registry_root));
        }
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[windows-install] \"Open with\" registration under {registry_root} for {} failed, install continues: {err}",
                launcher_path.display()
            ));
            send_console_line(tx, tf!("installer.utils.open_with_registration_failed_log", error = err));
        }
    }
}

/// Uninstalls the copy the running executable belongs to, relaunching elevated (with
/// `continue_uninstall` set) when needed; the outcome is also written to `uninstall_signal_file`
/// when given. Windows only: startup rejects the flags elsewhere. Errors are localized user texts.
#[cfg(target_os = "windows")]
pub fn run_windows_uninstall_from_current_exe(
    continue_uninstall: bool,
    uninstall_signal_file: Option<&Path>,
) -> Result<(), String> {
    let current_exe =
        env::current_exe().map_err(|e| tf!("installer.utils.determine_exe_error", e = e))?;
    let install_dir = current_exe
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| t!("installer.utils.installed_program_folder_not_found_error").to_string())?;

    if uninstall_requires_elevation(&install_dir) {
        if continue_uninstall {
            return Err(
                t!("installer.utils.uninstall_no_admin_error")
                    .to_string(),
            );
        }
        let args = build_windows_uninstall_args(uninstall_signal_file, true);
        relaunch_self_elevated_with_args(&install_dir, &args).map_err(|e| e.user_message())?;
        return Ok(());
    }

    let result = run_windows_uninstall_window(current_exe, install_dir);
    if let Some(signal_file) = uninstall_signal_file {
        // The waiting reinstall flow learns the outcome only from this file; a failed write must
        // at least be logged, the uninstall result itself is still returned below.
        if let Err(err) = write_uninstall_signal_file(signal_file, &result) {
            ms_log::runtime_log::log_error(format!(
                "[installer] uninstall: could not write the signal file {}: {err}",
                signal_file.display()
            ));
        }
    }
    result
}

#[cfg(target_os = "windows")]
fn uninstall_requires_elevation(install_dir: &Path) -> bool {
    !is_running_elevated() && !has_write_access_for_install(install_dir)
}

#[cfg(target_os = "windows")]
pub(super) fn run_windows_uninstall_worker(
    current_exe: PathBuf,
    install_dir: PathBuf,
    tx: &mpsc::Sender<UninstallEvent>,
) -> Result<(), String> {
    send_uninstall_progress(
        tx,
        0.05,
        t!("installer.utils.prepare_uninstall_stage"),
        tf!("installer.utils.install_folder_label", install_dir = install_dir.display()),
    );

    match remove_windows_shortcuts_for_install(&install_dir).map_err(|e| e.user_message()) {
        Ok(()) => {
            send_uninstall_progress(tx, 0.18, t!("installer.utils.removing_shortcuts_stage"), t!("installer.utils.shortcuts_cleaned_status"));
        }
        Err(err) if !is_running_elevated() => {
            ms_log::runtime_log::log_warn(format!(
                "[windows-uninstall] shortcut cleanup skipped without elevation: {err}"
            ));
            send_uninstall_progress(
                tx,
                0.18,
                t!("installer.utils.removing_shortcuts_stage"),
                t!("installer.utils.system_shortcuts_skipped_status"),
            );
        }
        Err(err) => return Err(err),
    }

    match remove_windows_registry_entries_for_install(&install_dir).map_err(|e| e.user_message()) {
        Ok(summary) => {
            // Entries of another install, or ones whose ownership could not be verified, stay
            // by design; "complete" would hide that a dead entry may remain in the Apps list.
            // Each kept entry and its reason are already in the session log.
            let status = if summary.kept.is_empty() {
                t!("installer.utils.registry_cleanup_done_status").to_string()
            } else {
                tf!("installer.utils.registry_cleanup_entries_kept_status", count = summary.kept.len())
            };
            send_uninstall_progress(tx, 0.30, t!("installer.utils.removing_registry_stage"), status);
        }
        Err(err) if !is_running_elevated() => {
            ms_log::runtime_log::log_warn(format!(
                "[windows-uninstall] registry cleanup skipped without elevation: {err}"
            ));
            send_uninstall_progress(
                tx,
                0.30,
                t!("installer.utils.removing_registry_stage"),
                t!("installer.utils.system_registry_skipped_status"),
            );
        }
        Err(err) => return Err(err),
    }

    remove_install_dir_contents_except_path_with_progress(&install_dir, &current_exe, tx)?;

    send_uninstall_progress(
        tx,
        0.94,
        t!("installer.utils.final_cleanup_stage"),
        t!("installer.utils.schedule_self_delete_status"),
    );
    schedule_windows_self_delete(&current_exe, &install_dir)?;
    send_uninstall_progress(
        tx,
        1.0,
        t!("installer.utils.uninstall_complete_stage"),
        t!("installer.utils.helper_accepted_cleanup_status"),
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn write_uninstall_signal_file(
    signal_file: &Path,
    result: &Result<(), String>,
) -> Result<(), String> {
    if let Some(parent) = signal_file.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            tf!("installer.utils.create_signal_file_folder_error", parent = parent.display(), e = e)
        })?;
    }
    let payload = match result {
        Ok(()) => "ok".to_string(),
        Err(err) => format!("error: {err}"),
    };
    fs::write(signal_file, payload).map_err(|e| {
        tf!("installer.utils.write_uninstall_signal_error", signal_file = signal_file.display(), e = e)
    })
}

#[cfg(target_os = "windows")]
fn remove_install_dir_contents_except_path_with_progress(
    install_dir: &Path,
    keep_path: &Path,
    tx: &mpsc::Sender<UninstallEvent>,
) -> Result<(), String> {
    let entries = fs::read_dir(install_dir).map_err(|e| {
        tf!("installer.utils.read_install_dir_error", install_dir = install_dir.display(), e = e)
    })?;

    let mut targets = Vec::new();
    let mut total_nodes = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|e| {
            tf!("installer.utils.read_install_dir_contents_error", install_dir = install_dir.display(), e = e)
        })?;
        let path = entry.path();
        if normalize_windows_path(&path) == normalize_windows_path(keep_path) {
            continue;
        }
        total_nodes += count_removable_nodes(&path)?;
        targets.push(path);
    }

    if total_nodes == 0 {
        send_uninstall_progress(
            tx,
            0.90,
            t!("installer.utils.removing_files_stage"),
            t!("installer.utils.no_more_files_status"),
        );
        return Ok(());
    }

    let mut removed_nodes = 0_u64;
    for path in targets {
        remove_path_recursive_with_progress(&path, &mut removed_nodes, total_nodes, tx)?;
    }

    send_uninstall_progress(
        tx,
        0.90,
        t!("installer.utils.removing_files_stage"),
        tf!("installer.utils.removed_objects_status", removed_nodes = removed_nodes, total_nodes = total_nodes),
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn count_removable_nodes(path: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| tf!("installer.utils.read_path_error", path = path.display(), e = e))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        let mut total = 1_u64;
        for entry in fs::read_dir(path)
            .map_err(|e| tf!("installer.utils.read_folder_path_error", path = path.display(), e = e))?
        {
            let child = entry
                .map_err(|e| tf!("installer.utils.read_contents_path_error", path = path.display(), e = e))?
                .path();
            total += count_removable_nodes(&child)?;
        }
        Ok(total)
    } else {
        Ok(1)
    }
}

#[cfg(target_os = "windows")]
fn remove_path_recursive_with_progress(
    path: &Path,
    removed_nodes: &mut u64,
    total_nodes: u64,
    tx: &mpsc::Sender<UninstallEvent>,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| tf!("installer.utils.read_path_error", path = path.display(), e = e))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        for entry in fs::read_dir(path)
            .map_err(|e| tf!("installer.utils.read_folder_path_error", path = path.display(), e = e))?
        {
            let child = entry
                .map_err(|e| tf!("installer.utils.read_contents_path_error", path = path.display(), e = e))?
                .path();
            remove_path_recursive_with_progress(&child, removed_nodes, total_nodes, tx)?;
        }
        fs::remove_dir(path)
            .map_err(|e| tf!("installer.utils.remove_folder_error", path = path.display(), e = e))?;
    } else {
        fs::remove_file(path)
            .map_err(|e| tf!("installer.utils.remove_file_error", path = path.display(), e = e))?;
    }

    *removed_nodes += 1;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("...");
    let delete_progress = if total_nodes > 0 {
        *removed_nodes as f32 / total_nodes as f32
    } else {
        1.0
    };
    send_uninstall_progress(
        tx,
        lerp_progress(0.40, 0.90, delete_progress),
        t!("installer.utils.removing_files_stage"),
        tf!("installer.utils.removing_file_progress", file_name = file_name, removed_nodes = removed_nodes, total_nodes = total_nodes),
    );
    Ok(())
}

/// Removes `path` (a directory recursively, otherwise as a file); a missing path is `Ok`.
/// The installer's own general deleter (uninstall signal file, install-tree cleanup), kept
/// separate from the shortcut removal of `ms_os_integration` so a change to shortcut handling
/// never changes installer cleanup.
///
/// # Errors
/// The localized `installer.utils.remove_folder_error` / `remove_file_error` text.
#[cfg(target_os = "windows")]
pub(super) fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|e| tf!("installer.utils.remove_folder_error", path = path.display(), e = e))?;
    } else {
        fs::remove_file(path)
            .map_err(|e| tf!("installer.utils.remove_file_error", path = path.display(), e = e))?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn schedule_windows_self_delete(current_exe: &Path, install_dir: &Path) -> Result<(), String> {
    let current_pid = std::process::id();
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         $pidToWait={pid}; \
         $exe='{exe}'; \
         $dir='{dir}'; \
         while (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 500 }}; \
         for ($i = 0; $i -lt 240; $i++) {{ \
             Remove-Item -LiteralPath $exe -Force -ErrorAction SilentlyContinue; \
             if (-not (Test-Path -LiteralPath $exe)) {{ break }}; \
             Start-Sleep -Milliseconds 500; \
         }}; \
         for ($i = 0; $i -lt 240; $i++) {{ \
             Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue; \
             if (-not (Test-Path -LiteralPath $dir)) {{ exit 0 }}; \
             Start-Sleep -Milliseconds 500; \
         }}; \
         exit 1",
        pid = current_pid,
        exe = escape_ps_single_quote(current_exe.to_string_lossy().as_ref()),
        dir = escape_ps_single_quote(install_dir.to_string_lossy().as_ref()),
    );
    let mut cmd = Command::new("powershell");
    apply_windows_no_window(&mut cmd);
    cmd.current_dir(env::temp_dir());
    cmd.arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-WindowStyle")
        .arg("Hidden")
        .arg("-Command")
        .arg(script)
        .spawn()
        .map_err(|e| tf!("installer.utils.run_final_cleanup_error", e = e))?;
    Ok(())
}

/// Escapes `text` for a PowerShell single-quoted string (`'` doubled); used by the self-delete
/// script above. Pure (also built into host test builds, where its test runs).
#[cfg(any(target_os = "windows", test))]
fn escape_ps_single_quote(text: &str) -> String {
    text.replace('\'', "''")
}

#[cfg(target_os = "windows")]
fn lerp_progress(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t.clamp(0.0, 1.0)
}

#[cfg(target_os = "macos")]
fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn escape_applescript(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn detect_platform() -> Result<Platform, String> {
    match env::consts::OS {
        "windows" => Ok(Platform::Windows),
        "macos" => Ok(Platform::Macos),
        "linux" => Ok(Platform::Linux),
        other => Err(tf!("installer.utils.unsupported_os_named_error", other = other)),
    }
}

pub(super) fn detect_arch() -> Result<String, String> {
    match env::consts::ARCH {
        "x86_64" => Ok("x86_64".to_string()),
        "aarch64" => Ok("aarch64".to_string()),
        other => Err(tf!("installer.utils.unsupported_arch_error", other = other)),
    }
}

pub(super) fn detect_arch_label(arch: &str) -> &str {
    match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => "unknown",
    }
}

fn resolve_uv_executable(uv_dir: &Path) -> Result<PathBuf, String> {
    let candidates: &[&str] = if cfg!(windows) {
        &["uv.exe", "bin/uv.exe"]
    } else {
        &["uv", "bin/uv"]
    };

    for rel in candidates {
        let candidate = uv_dir.join(rel);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    let nested: Vec<_> = fs::read_dir(uv_dir)
        .map_err(|e| tf!("installer.utils.read_uv_dir_error", uv_dir = uv_dir.display(), e = e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tf!("installer.utils.read_uv_dir_io_error", uv_dir = uv_dir.display(), e = e))?;

    for entry in nested {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        for rel in candidates {
            let candidate = path.join(rel);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(tf!("installer.utils.uv_executable_not_found_error", uv_dir = uv_dir.display()))
}

/// Which program installs Python packages into the managed environment.
#[derive(Debug)]
pub enum PipInstallRunner {
    /// A resolved `uv` executable driving `uv pip ...` against an explicit interpreter.
    Uv(PathBuf),
    /// Fallback: the environment's own `python -m pip`.
    PythonPip,
}

impl PipInstallRunner {
    /// Short display name of the runner for console lines (`uv pip (<path>)` / `python -m pip`).
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Uv(path) => format!("uv pip ({})", path.display()),
            Self::PythonPip => "python -m pip".to_string(),
        }
    }
}

/// Picks the package installer for an already-provisioned environment: the `uv`
/// next to the interpreter, then the installer-managed `installer_files/uv`, then
/// any `uv` on `PATH`, and finally `python -m pip`.
pub fn resolve_runtime_pip_runner(root_dir: &Path, python_exe: &Path) -> PipInstallRunner {
    let uv_name = if cfg!(target_os = "windows") {
        "uv.exe"
    } else {
        "uv"
    };
    if let Some(env_uv) = python_exe
        .parent()
        .map(|python_dir| python_dir.join(uv_name))
        .filter(|candidate| candidate.is_file())
    {
        return PipInstallRunner::Uv(env_uv);
    }

    let bundled_uv_dir = root_dir.join("installer_files").join("uv");
    if bundled_uv_dir.is_dir()
        && let Ok(uv_exe) = resolve_uv_executable(&bundled_uv_dir)
    {
        return PipInstallRunner::Uv(uv_exe);
    }
    find_executable_on_path(uv_name)
        .map(PipInstallRunner::Uv)
        .unwrap_or(PipInstallRunner::PythonPip)
}

fn find_executable_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

struct DependencyInstallRequest<'a> {
    root_dir: &'a Path,
    pip_runner: &'a PipInstallRunner,
    python_exe: &'a Path,
    tx: &'a mpsc::Sender<InstallEvent>,
    /// Checked before every pip command and during its retry backoff.
    cancel: &'a AtomicBool,
    label: &'a str,
    dependencies: &'a [&'a str],
    /// `-c` constraints file passed to every install of the request (the installed torch
    /// pins, `torch::write_torch_constraints`); `None` when no torch is installed.
    constraints: Option<&'a Path>,
    overall_start: f32,
    overall_end: f32,
}

/// The environment a post-torch dependency install targets
/// ([`install_torch_python_dependencies`]).
pub(crate) struct PostTorchInstallTarget<'a> {
    pub(crate) root_dir: &'a Path,
    pub(crate) pip_runner: &'a PipInstallRunner,
    pub(crate) python_exe: &'a Path,
    pub(crate) tx: &'a mpsc::Sender<InstallEvent>,
    /// Checked before every pip command and during its retry backoff.
    pub(crate) cancel: &'a AtomicBool,
    /// `-c` constraints file pinning the installed torch (`torch::write_torch_constraints`).
    pub(crate) constraints: Option<&'a Path>,
}

/// The `-c <file>` arguments for `constraints` (empty for `None`).
fn constraint_args(constraints: Option<&Path>) -> Vec<String> {
    constraints
        .map(|path| vec!["-c".to_string(), path.to_string_lossy().into_owned()])
        .unwrap_or_default()
}

/// Upgrades pip / wheel / setuptools, then installs `request.dependencies` one spec per
/// command, every command under `request.constraints`; progress maps onto the request range.
///
/// # Errors
/// The localized cancel text or the last output of the failed install command.
fn install_static_python_dependencies(request: DependencyInstallRequest<'_>) -> Result<(), String> {
    let DependencyInstallRequest {
        root_dir,
        pip_runner,
        python_exe,
        tx,
        cancel,
        label,
        dependencies,
        constraints,
        overall_start,
        overall_end,
    } = request;
    let constraint_args = constraint_args(constraints);
    let with_constraints = |args: &[&str]| -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).chain(constraint_args.iter().cloned()).collect()
    };
    let total_units = (dependencies.len() + 1).max(1) as f32;

    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.updating_pip_status").to_string(),
    ));
    send_progress(
        tx,
        0.0,
        t!("installer.utils.updating_pip_stage"),
        overall_start,
        tf!("installer.utils.stage_installing_label", label = label),
    );
    let upgrade_args = with_constraints(&["--upgrade", "pip", "wheel", "setuptools"]);
    run_pip_install_with_retry(
        pip_runner,
        python_exe,
        root_dir,
        &upgrade_args.iter().map(String::as_str).collect::<Vec<_>>(),
        RetryRun { action_name: t!("installer.utils.update_pip_action"), attempts: 1, tx: Some(tx), cancel },
    )?;

    let mut completed_units = 1.0;
    send_progress(
        tx,
        completed_units / total_units,
        t!("installer.utils.update_pip_done_status"),
        lerp(overall_start, overall_end, completed_units / total_units),
        tf!("installer.utils.stage_installing_label", label = label),
    );

    for dep in dependencies {
        let stage_start = completed_units / total_units;
        let _ = tx.send(InstallEvent::Step(tf!("installer.utils.installing_dep_status", dep = dep)));
        send_progress(
            tx,
            stage_start,
            tf!("installer.utils.install_short_dep_status", dep = dep),
            lerp(overall_start, overall_end, stage_start),
            tf!("installer.utils.stage_installing_label", label = label),
        );

        let dep_args = with_constraints(&[*dep]);
        run_pip_install_with_retry(
            pip_runner,
            python_exe,
            root_dir,
            &dep_args.iter().map(String::as_str).collect::<Vec<_>>(),
            RetryRun { action_name: &tf!("installer.utils.install_dep_action", dep = dep), attempts: 3, tx: Some(tx), cancel },
        )?;

        completed_units += 1.0;
        let ratio = completed_units / total_units;
        send_progress(
            tx,
            ratio,
            tf!("installer.utils.installed_dep_status", dep = dep),
            lerp(overall_start, overall_end, ratio),
            tf!("installer.utils.stage_installing_label", label = label),
        );
    }

    Ok(())
}

/// Installs the torch-dependent extras (`TORCH_DEPENDENCIES`) one spec per call after the
/// Torch stage, each under `target.constraints`, plus the Paddle CUDA packages when
/// `install_cuda_extra_packages`; progress maps onto `overall_range`.
///
/// # Errors
/// The localized cancel text or the last output of the failed install command.
pub(crate) fn install_torch_python_dependencies(
    target: &PostTorchInstallTarget<'_>,
    install_cuda_extra_packages: bool,
    overall_range: (f32, f32),
) -> Result<(), String> {
    let PostTorchInstallTarget { root_dir, pip_runner, python_exe, tx, cancel, constraints } = *target;
    let (overall_start, overall_end) = overall_range;
    install_static_python_dependencies(DependencyInstallRequest {
        root_dir,
        pip_runner,
        python_exe,
        tx,
        cancel,
        label: t!("installer.utils.torch_deps_label"),
        dependencies: TORCH_DEPENDENCIES,
        constraints,
        overall_start,
        overall_end: if install_cuda_extra_packages {
            lerp(overall_start, overall_end, 0.88)
        } else {
            overall_end
        },
    })?;

    if install_cuda_extra_packages {
        install_cuda_paddle_packages(
            root_dir,
            pip_runner,
            python_exe,
            tx,
            lerp(overall_start, overall_end, 0.88),
            overall_end,
            cancel,
        )?;
    }

    Ok(())
}

fn install_cuda_paddle_packages(
    root_dir: &Path,
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    overall_start: f32,
    overall_end: f32,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let cuda_packages = ["paddlepaddle-gpu==3.3.0", "nvidia-cuda-cccl-cu12"];
    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.installing_paddle_cuda_status").to_string(),
    ));
    send_progress(
        tx,
        0.0,
        tf!("installer.utils.installing_two_packages_status", cuda_packages = cuda_packages[0], cuda_packages_2 = cuda_packages[1]),
        overall_start,
        t!("installer.utils.stage_paddle_cuda"),
    );
    run_pip_install_with_retry(
        pip_runner,
        python_exe,
        root_dir,
        &[
            "--no-deps",
            "--index-url",
            PADDLE_CU126_INDEX_URL,
            cuda_packages[0],
            cuda_packages[1],
        ],
        RetryRun { action_name: t!("installer.utils.install_paddle_cuda_action"), attempts: 3, tx: Some(tx), cancel },
    )?;
    send_progress(
        tx,
        1.0,
        t!("installer.utils.paddle_cuda_installed_status"),
        overall_end,
        t!("installer.utils.paddle_cuda_stage_done_status"),
    );
    Ok(())
}

/// How a retried command is run: its user-facing name, the attempt budget, the console sink
/// (`None` = silent) and the cancel flag checked before every attempt and during the backoff.
#[derive(Clone, Copy)]
pub(crate) struct RetryRun<'a> {
    pub(crate) action_name: &'a str,
    pub(crate) attempts: usize,
    pub(crate) tx: Option<&'a mpsc::Sender<InstallEvent>>,
    pub(crate) cancel: &'a AtomicBool,
}

fn run_uv_pip_install_with_retry(
    uv_exe: &Path,
    python_exe: &Path,
    cwd: &Path,
    pip_args: &[&str],
    run: RetryRun<'_>,
) -> Result<(), String> {
    let python_arg = python_exe.to_string_lossy().into_owned();
    let mut args = vec![
        "pip".to_string(),
        "install".to_string(),
        "--python".to_string(),
        python_arg,
    ];
    args.extend(pip_args.iter().map(|arg| (*arg).to_string()));
    let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_command_with_retry(uv_exe, cwd, &args_ref, run, &[])
}

/// Runs `pip install <pip_args>` into `python_exe`'s environment through `pip_runner`
/// (`uv pip install --python <py>` or `python -m pip install`) under the retry rules of `run`.
///
/// # Errors
/// See `run_command_with_retry`.
pub(crate) fn run_pip_install_with_retry(
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    cwd: &Path,
    pip_args: &[&str],
    run: RetryRun<'_>,
) -> Result<(), String> {
    match pip_runner {
        PipInstallRunner::Uv(uv_exe) => run_uv_pip_install_with_retry(
            uv_exe,
            python_exe,
            cwd,
            pip_args,
            run,
        ),
        PipInstallRunner::PythonPip => {
            let mut args = vec!["-m".to_string(), "pip".to_string(), "install".to_string()];
            args.extend(pip_args.iter().map(|arg| (*arg).to_string()));
            let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
            run_command_with_retry(python_exe, cwd, &args_ref, run, &[])
        }
    }
}

/// Uninstalls `names` from `python_exe`'s environment through `pip_runner`
/// (`uv pip uninstall --python <py> <names…>` or `python -m pip uninstall -y <names…>`) under
/// the retry rules of `run`. Does nothing when `names` is empty.
///
/// # Errors
/// See `run_command_with_retry`.
pub(crate) fn run_pip_uninstall_with_retry(
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    cwd: &Path,
    names: &[String],
    run: RetryRun<'_>,
) -> Result<(), String> {
    if names.is_empty() {
        return Ok(());
    }
    let (executable, mut args) = match pip_runner {
        PipInstallRunner::Uv(uv_exe) => (
            uv_exe.as_path(),
            vec![
                "pip".to_string(),
                "uninstall".to_string(),
                "--python".to_string(),
                python_exe.to_string_lossy().into_owned(),
            ],
        ),
        PipInstallRunner::PythonPip => (
            python_exe,
            vec!["-m".to_string(), "pip".to_string(), "uninstall".to_string(), "-y".to_string()],
        ),
    };
    args.extend(names.iter().cloned());
    let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_command_with_retry(executable, cwd, &args_ref, run, &[])
}

/// Wait before the retry that follows `failed_attempts` failed runs of a command: 2, 4, 8 s,
/// capped at 8 s.
fn command_retry_delay(failed_attempts: usize) -> Duration {
    let exponent = u32::try_from(failed_attempts.saturating_sub(1)).unwrap_or(u32::MAX).min(2);
    Duration::from_secs(2_u64 << exponent)
}

/// Sleeps `delay` in 100 ms slices through `sleep`, polling `cancel` before every slice.
/// Returns `true` as soon as the flag is seen (the wait is abandoned).
fn wait_unless_cancelled(delay: Duration, cancel: &AtomicBool, sleep: &dyn Fn(Duration)) -> bool {
    const SLICE: Duration = Duration::from_millis(100);
    let mut remaining = delay;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        if remaining.is_zero() {
            return false;
        }
        let slice = remaining.min(SLICE);
        sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

/// Runs `executable` up to `run.attempts` times until it exits successfully, streaming its
/// output to the console sink.
///
/// Every failed run is retried after a capped exponential backoff (`command_retry_delay`).
/// pip / uv failures cannot be told apart (a network blip and an unsatisfiable requirement
/// both end in a non-zero exit), so every failure is retried and the attempt budget stays
/// small and per call site. `run.cancel` is checked before every attempt and during the
/// backoff; a running child process is not interrupted.
///
/// # Errors
/// The localized cancel text, a spawn failure, or the last output after the final attempt.
fn run_command_with_retry(
    executable: &Path,
    cwd: &Path,
    args: &[&str],
    run: RetryRun<'_>,
    extra_env: &[(&str, &str)],
) -> Result<(), String> {
    let RetryRun { action_name, attempts, tx, cancel } = run;
    let mut last_output = String::new();
    for attempt in 1..=attempts.max(1) {
        if cancel.load(Ordering::Relaxed) {
            return Err(tf!("installer.utils.command_cancelled_error", action_name = action_name));
        }
        if let Some(tx) = tx {
            send_console_line(tx, format!("$ {} {}", executable.display(), args.join(" ")));
            if attempts > 1 {
                send_console_line(tx, tf!("installer.utils.retry_attempt_status", attempt = attempt, attempts = attempts, action_name = action_name));
            }
        }

        let (status, output_text) = run_command_streaming(executable, cwd, args, tx, extra_env)?;
        last_output = output_text.trim().to_string();

        if status.success() {
            return Ok(());
        }

        if attempt < attempts {
            if let Some(tx) = tx {
                send_console_line(
                    tx,
                    tf!("installer.utils.retry_after_error_status", action_name = action_name),
                );
            }
            if wait_unless_cancelled(command_retry_delay(attempt), cancel, &std::thread::sleep) {
                return Err(tf!("installer.utils.command_cancelled_error", action_name = action_name));
            }
        }
    }

    Err(tf!("installer.utils.action_failed_after_retries_error", action_name = action_name, attempts = attempts, last_output = last_output))
}

fn run_command_streaming(
    executable: &Path,
    cwd: &Path,
    args: &[&str],
    tx: Option<&mpsc::Sender<InstallEvent>>,
    extra_env: &[(&str, &str)],
) -> Result<(std::process::ExitStatus, String), String> {
    let mut cmd = std::process::Command::new(executable);
    let inherit_stderr = should_inherit_command_stderr(executable);
    apply_windows_no_window(&mut cmd);
    cmd.current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(if inherit_stderr {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| tf!("installer.utils.run_executable_error", executable = executable.display(), e = e))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| t!("installer.utils.no_process_stdout_error").to_string())?;
    let tx_out = tx.cloned();
    let out_handle = ms_thread::spawn(move || stream_reader_lines(stdout, tx_out));
    let err_handle = child.stderr.take().map(|stderr| {
        let tx_err = tx.cloned();
        ms_thread::spawn(move || stream_reader_lines(stderr, tx_err))
    });

    let status = child
        .wait()
        .map_err(|e| tf!("installer.utils.wait_python_process_error", e = e))?;
    let out_text = out_handle
        .join()
        .map_err(|_| t!("installer.utils.join_stdout_reader_error").to_string())?;
    let err_text = match err_handle {
        Some(handle) => handle
            .join()
            .map_err(|_| t!("installer.utils.join_stderr_reader_error").to_string())?,
        None => String::new(),
    };

    Ok((status, format!("{out_text}\n{err_text}")))
}

fn should_inherit_command_stderr(executable: &Path) -> bool {
    std::io::stderr().is_terminal()
        && executable
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case("uv"))
}

fn stream_reader_lines<R: Read>(reader: R, tx: Option<mpsc::Sender<InstallEvent>>) -> String {
    let mut collected = String::new();
    let mut br = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match br.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let printable = line.trim_end_matches(&['\r', '\n'][..]).to_string();
                if !printable.is_empty() {
                    if let Some(tx) = &tx {
                        send_console_line(tx, printable.clone());
                    }
                    collected.push_str(&printable);
                    collected.push('\n');
                }
            }
            Err(_) => break,
        }
    }
    collected
}

pub(super) fn apply_windows_no_window(cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t.clamp(0.0, 1.0)
}

fn fetch_latest_uv_asset(platform: Platform, arch: &str, cancel: &AtomicBool) -> Result<GithubAsset, String> {
    let body = github_api_get(UV_RELEASE_API, INSTALLER_USER_AGENT, cancel)
        .map_err(|e| tf!("installer.utils.get_uv_release_error", e = e))?;
    let release: GithubRelease = serde_json::from_str(&body)
        .map_err(|e| tf!("installer.utils.parse_uv_release_error", e = e))?;

    select_uv_asset(&release.assets, platform, arch)
}

fn select_uv_asset(
    assets: &[GithubAsset],
    platform: Platform,
    arch: &str,
) -> Result<GithubAsset, String> {
    let expected_name = match platform {
        Platform::Windows => format!("uv-{arch}-pc-windows-msvc.zip"),
        Platform::Macos => format!("uv-{arch}-apple-darwin.tar.gz"),
        Platform::Linux => format!("uv-{arch}-unknown-linux-gnu.tar.gz"),
    };

    assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(&expected_name))
        .cloned()
        .ok_or_else(|| {
            tf!("installer.utils.uv_build_not_found_error", platform = platform, arch = arch, expected_name = expected_name)
        })
}

fn extract_archive(
    archive_path: &Path,
    target_dir: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    label_prefix: &str,
    overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    let lower = archive_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if lower.ends_with(".tar.zst") {
        let file = File::open(archive_path)
            .map_err(|e| tf!("installer.utils.open_archive_error", archive_path = archive_path.display(), e = e))?;
        let decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|e| tf!("installer.utils.open_zstd_decoder_error", e = e))?;
        send_progress(
            tx,
            0.0,
            tf!("installer.utils.archive_preparing_progress", label_prefix = label_prefix),
            overall_start,
            label_prefix,
        );
        extract_tar(decoder, target_dir)?;
        send_progress(
            tx,
            1.0,
            tf!("installer.utils.archive_done_progress", label_prefix = label_prefix),
            overall_end,
            tf!("installer.utils.archive_done_progress", label_prefix = label_prefix),
        );
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let file = File::open(archive_path)
            .map_err(|e| tf!("installer.utils.open_archive_error", archive_path = archive_path.display(), e = e))?;
        let decoder = GzDecoder::new(file);
        send_progress(
            tx,
            0.0,
            tf!("installer.utils.archive_preparing_progress", label_prefix = label_prefix),
            overall_start,
            label_prefix,
        );
        extract_tar(decoder, target_dir)?;
        send_progress(
            tx,
            1.0,
            tf!("installer.utils.archive_done_progress", label_prefix = label_prefix),
            overall_end,
            tf!("installer.utils.archive_done_progress", label_prefix = label_prefix),
        );
    } else if lower.ends_with(".zip") {
        extract_zip(
            archive_path,
            target_dir,
            tx,
            label_prefix,
            overall_start,
            overall_end,
        )?;
    } else {
        return Err(tf!("installer.utils.unsupported_archive_format_error", archive_path = archive_path.display()));
    }

    Ok(())
}

fn extract_tar<R: Read>(reader: R, target_dir: &Path) -> Result<(), String> {
    let mut archive = Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| tf!("installer.utils.read_tar_entries_error", e = e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| tf!("installer.utils.tar_entry_error", e = e))?;
        entry
            .unpack_in(target_dir)
            .map_err(|e| tf!("installer.utils.extract_tar_entry_error", e = e))?;
    }
    Ok(())
}

fn extract_zip(
    archive_path: &Path,
    target_dir: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    label_prefix: &str,
    overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    let file = File::open(archive_path)
        .map_err(|e| tf!("installer.utils.open_archive_error", archive_path = archive_path.display(), e = e))?;
    let mut zip =
        ZipArchive::new(file).map_err(|e| tf!("installer.utils.open_zip_archive_error", e = e))?;

    let total = zip.len().max(1);
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| tf!("installer.utils.read_zip_entry_error", i = i, e = e))?;
        let rel = match entry.enclosed_name() {
            Some(path) => path.to_path_buf(),
            None => continue,
        };
        let out_path = target_dir.join(archive_entry_relative_output_path(&rel));

        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|e| tf!("installer.utils.create_out_dir_error", out_path = out_path.display(), e = e))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    tf!("installer.utils.create_parent_dir_error", parent = parent.display(), e = e)
                })?;
            }
            let mut out = File::create(&out_path)
                .map_err(|e| tf!("installer.utils.create_out_path_error", out_path = out_path.display(), e = e))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| tf!("installer.utils.extract_out_path_error", out_path = out_path.display(), e = e))?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let perms = fs::Permissions::from_mode(mode);
                let _ = fs::set_permissions(&out_path, perms);
            }
        }

        let stage = i as f32 / total as f32;
        send_progress(
            tx,
            stage,
            format!("{label_prefix}: {}/{}", i + 1, total),
            overall_start + stage * (overall_end - overall_start),
            label_prefix,
        );
    }

    send_progress(
        tx,
        1.0,
        tf!("installer.utils.archive_done_progress", label_prefix = label_prefix),
        overall_end,
        tf!("installer.utils.archive_done_progress", label_prefix = label_prefix),
    );

    Ok(())
}

fn archive_entry_relative_output_path(path: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let mut sanitized_path = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(name) => {
                    sanitized_path.push(sanitize_windows_archive_path_component(
                        &name.to_string_lossy(),
                    ));
                }
                std::path::Component::CurDir
                | std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_) => {}
            }
        }
        sanitized_path
    }

    #[cfg(not(target_os = "windows"))]
    {
        path.to_path_buf()
    }
}

#[cfg(any(target_os = "windows", test))]
fn sanitize_windows_archive_path_component(component: &str) -> String {
    let mut sanitized: String = component
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();

    while sanitized.ends_with([' ', '.']) {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    let stem = sanitized
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let is_reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|n| (1..=9).contains(&n))
        || stem
            .strip_prefix("LPT")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|n| (1..=9).contains(&n));
    if is_reserved {
        sanitized.push('_');
    }
    sanitized
}

fn flatten_single_root_dir(target_dir: &Path) -> Result<(), String> {
    let entries: Vec<_> = fs::read_dir(target_dir)
        .map_err(|e| tf!("installer.utils.read_target_dir_error", target_dir = target_dir.display(), e = e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tf!("installer.utils.read_target_dir_contents_error", target_dir = target_dir.display(), e = e))?;

    if entries.len() != 1 {
        return Ok(());
    }

    let only = &entries[0];
    let file_type = only
        .file_type()
        .map_err(|e| tf!("installer.utils.stat_only_error", only = only.path().display(), e = e))?;
    if !file_type.is_dir() {
        return Ok(());
    }

    let nested_root = only.path();
    let nested_entries: Vec<_> = fs::read_dir(&nested_root)
        .map_err(|e| tf!("installer.utils.read_nested_root_error", nested_root = nested_root.display(), e = e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tf!("installer.utils.read_nested_root_contents_error", nested_root = nested_root.display(), e = e))?;

    for entry in nested_entries {
        let from = entry.path();
        let to = target_dir.join(entry.file_name());
        if to.exists() {
            if to.is_dir() {
                fs::remove_dir_all(&to)
                    .map_err(|e| tf!("installer.utils.remove_to_error", to = to.display(), e = e))?;
            } else {
                fs::remove_file(&to)
                    .map_err(|e| tf!("installer.utils.remove_to_error", to = to.display(), e = e))?;
            }
        }
        fs::rename(&from, &to).map_err(|e| {
            tf!("installer.utils.move_from_to_error", from = from.display(), to = to.display(), e = e)
        })?;
    }

    fs::remove_dir_all(&nested_root)
        .map_err(|e| tf!("installer.utils.remove_nested_root_error", nested_root = nested_root.display(), e = e))?;
    Ok(())
}

fn merge_dir_contents(src_dir: &Path, dst_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dst_dir)
        .map_err(|e| tf!("installer.utils.create_dst_dir_error", dst_dir = dst_dir.display(), e = e))?;

    let entries: Vec<_> = fs::read_dir(src_dir)
        .map_err(|e| tf!("installer.utils.read_src_dir_error", src_dir = src_dir.display(), e = e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tf!("installer.utils.read_src_dir_io_error", src_dir = src_dir.display(), e = e))?;

    for entry in entries {
        let src_path = entry.path();
        let dst_path = dst_dir.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| tf!("installer.utils.stat_src_path_error", src_path = src_path.display(), e = e))?;

        if file_type.is_dir() {
            merge_dir_contents(&src_path, &dst_path)?;
            fs::remove_dir_all(&src_path)
                .map_err(|e| tf!("installer.utils.remove_src_path_error", src_path = src_path.display(), e = e))?;
            continue;
        }

        if dst_path.exists() {
            if dst_path.is_dir() {
                fs::remove_dir_all(&dst_path)
                    .map_err(|e| tf!("installer.utils.remove_dst_path_error", dst_path = dst_path.display(), e = e))?;
            } else {
                fs::remove_file(&dst_path)
                    .map_err(|e| tf!("installer.utils.remove_dst_path_error", dst_path = dst_path.display(), e = e))?;
            }
        } else if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| tf!("installer.utils.create_parent_path_error", parent = parent.display(), e = e))?;
        }

        fs::rename(&src_path, &dst_path).map_err(|e| {
            tf!("installer.utils.move_src_dst_error", src_path = src_path.display(), dst_path = dst_path.display(), e = e)
        })?;
    }

    Ok(())
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum Platform {
    Windows,
    Macos,
    Linux,
}

impl Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Windows => write!(f, "Windows"),
            Platform::Macos => write!(f, "macOS"),
            Platform::Linux => write!(f, "Linux"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LINUX_BINARY_ASSET_NAME_AARCH64, LINUX_BINARY_ASSET_NAME_X86_64,
        MACOS_BINARY_ASSET_NAME_AARCH64, MACOS_BINARY_ASSET_NAME_X86_64,
        WINDOWS_BINARY_ASSET_NAME_AARCH64, WINDOWS_BINARY_ASSET_NAME_X86_64,
        BASE_DEPENDENCIES, ONNXRUNTIME_DISTRIBUTIONS, TORCH_DEPENDENCIES,
        OPENCV_DISTRIBUTIONS, Requirement, missing_dependency_specs,
        missing_specs_for_readiness, normalize_package_name,
        parse_executable_version_output, parse_pip_freeze_packages, platform_binary_asset_name,
        platform_executable_file_name, required_dependency_specs,
        sanitize_windows_archive_path_component,
    };
    use super::{
        ReplaceTargetAccess, classify_replace_target_access, replace_executable_with_local_file,
        staged_replacement_file_name,
    };
    use super::{command_retry_delay, wait_unless_cancelled};
    use std::time::Duration;
    use ms_config::AiInstallType;
    use std::collections::HashMap;

    /// Builds a `pip freeze` style snapshot from `name==version` pairs, normalizing
    /// the keys exactly like the real parser does.
    fn installed_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(name, version)| (normalize_package_name(name), (*version).to_string()))
            .collect()
    }

    #[test]
    fn required_specs_grow_with_the_install_type() {
        let base = required_dependency_specs(AiInstallType::Base);
        let full = required_dependency_specs(AiInstallType::Full);
        let none = required_dependency_specs(AiInstallType::None);

        // `None` carries no smaller requirement than `Base`: an unknown install type
        // is a UI decision, not a licence to require less.
        assert_eq!(none, base, "None must require the same set as Base");
        assert_eq!(
            base.len(),
            BASE_DEPENDENCIES.len(),
            "Base must require exactly the base group"
        );
        assert_eq!(
            full.len(),
            BASE_DEPENDENCIES.len() + TORCH_DEPENDENCIES.len(),
            "Full must add exactly the torch extras"
        );
        // torch has a version AND flavour requirement, which a plain spec cannot
        // express, so it lives in `installed_torch_is_current` instead of this list.
        assert!(
            !full.contains(&"torch"),
            "torch must not be a plain dependency spec"
        );
        for dep in &base {
            assert!(full.contains(dep), "Full must be a superset of Base ({dep})");
        }
    }

    /// The readiness view accepts any interchangeable onnxruntime distribution, while
    /// the strict (installation) view keeps requiring the exact one.
    #[test]
    fn readiness_accepts_every_onnxruntime_distribution() {
        let required = [
            "onnxruntime; platform_system != \"Windows\"",
            "onnxruntime-directml; platform_system == \"Windows\"",
        ];
        for variant in ONNXRUNTIME_DISTRIBUTIONS {
            let installed = installed_map(&[(variant, "1.27.0")]);
            assert!(
                missing_specs_for_readiness(&required, &installed).is_empty(),
                "{variant} must satisfy the onnxruntime requirement for readiness"
            );
        }
        // Underscore/case spellings normalize like any other package name.
        let installed = installed_map(&[("ONNXRuntime_WebGPU", "1.27.0")]);
        assert!(
            missing_specs_for_readiness(&required, &installed).is_empty(),
            "the equivalence table must compare normalized package names"
        );
    }

    #[test]
    fn readiness_still_reports_onnxruntime_when_no_variant_is_installed() {
        let installed = installed_map(&[("numpy", "2.4.4")]);
        let required = [
            "onnxruntime; platform_system != \"Windows\"",
            "onnxruntime-directml; platform_system == \"Windows\"",
        ];
        assert_eq!(
            missing_specs_for_readiness(&required, &installed).len(),
            1,
            "with no onnxruntime distribution installed the platform's spec stays missing"
        );
        // Installation is unaffected by the readiness leniency.
        let installed_variant = installed_map(&[("onnxruntime-webgpu", "1.27.0")]);
        assert_eq!(
            missing_dependency_specs(&required, &installed_variant).len(),
            1,
            "the strict install view must keep requiring its exact distribution"
        );
    }

    #[test]
    fn readiness_does_not_soften_unrelated_packages() {
        let installed = installed_map(&[("onnxruntime-webgpu", "1.27.0")]);
        assert_eq!(
            missing_specs_for_readiness(&["numpy", "Pillow"], &installed),
            vec!["numpy", "Pillow"],
            "the equivalence table must only cover the onnxruntime and opencv families"
        );
    }

    #[test]
    fn missing_specs_ignore_installed_packages_and_normalize_names() {
        // `Pillow` -> `pillow` and `manga-ocr` vs `manga_ocr` must both be recognized
        // as installed despite the differing spelling in the freeze output.
        let installed = installed_map(&[("pillow", "11.0.0"), ("manga_ocr", "0.1.14")]);
        let missing = missing_dependency_specs(&["Pillow", "manga-ocr", "numpy"], &installed);
        assert_eq!(missing, vec!["numpy"]);
    }

    #[test]
    fn missing_specs_compare_installed_versions_with_pins() {
        let installed = installed_map(&[
            ("torch", "2.9.1+cu126"),
            ("transformers", "5.20.1"),
            ("diffusers", "0.39.0"),
            ("easyocr", "1.7.1"),
        ]);
        assert!(
            missing_dependency_specs(&["torch==2.9.1", "diffusers>=0.39,<0.40"], &installed).is_empty(),
            "an installed version inside the pin satisfies it (local build tags ignored)"
        );
        assert_eq!(
            missing_dependency_specs(&["transformers>=4.57.1,<4.58", "easyocr==1.7.2"], &installed),
            vec!["transformers>=4.57.1,<4.58", "easyocr==1.7.2"],
            "a version outside the pin is missing and is returned as the spec to install"
        );
        assert_eq!(
            missing_dependency_specs(&["torchvision>=0.24"], &installed),
            vec!["torchvision>=0.24"],
            "an absent package stays missing and is returned as the original spec"
        );
    }

    #[test]
    fn missing_specs_fail_open_toward_installing() {
        let installed = installed_map(&[("easyocr", "not-a-version"), ("weird", "1.0")]);
        assert_eq!(
            missing_dependency_specs(&["easyocr==1.7.2", "weird[extra]>=1"], &installed),
            vec!["easyocr==1.7.2", "weird[extra]>=1"],
            "an unreadable installed version or spec must never count as satisfied"
        );
        // An unpinned spec never looks at the version, readable or not.
        assert!(missing_dependency_specs(&["easyocr"], &installed).is_empty());
    }

    /// The readiness view must report an outdated pinned package too: a "ready" verdict
    /// implies the repair worker has nothing left to do.
    #[test]
    fn readiness_reports_a_package_outside_its_pin() {
        let installed = installed_map(&[("transformers", "5.20.1")]);
        assert_eq!(
            missing_specs_for_readiness(&["transformers>=4.57.1,<4.58"], &installed),
            vec!["transformers>=4.57.1,<4.58"]
        );
    }

    #[test]
    fn readiness_accepts_every_opencv_distribution_for_the_unpinned_spec() {
        for variant in OPENCV_DISTRIBUTIONS {
            let installed = installed_map(&[(variant, "4.12.0.88")]);
            assert!(
                missing_specs_for_readiness(&["opencv-python-headless"], &installed).is_empty(),
                "{variant} provides cv2 and must satisfy readiness"
            );
        }
    }

    #[test]
    fn every_embedded_spec_parses() {
        for dep in BASE_DEPENDENCIES.iter().chain(TORCH_DEPENDENCIES) {
            assert!(Requirement::parse(dep).is_ok(), "embedded spec '{dep}' must parse");
        }
    }

    /// The gitignored dev files `requirements/base.txt` / `requirements/torch.txt` mirror the
    /// embedded lists spec for spec (comments, blank lines and `-r` includes aside). Skipped on a
    /// checkout without them (they are untracked, so a fresh clone has none).
    #[test]
    fn requirements_files_mirror_installer_lists() {
        let requirements_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../requirements");
        for (file_name, embedded) in [("base.txt", BASE_DEPENDENCIES), ("torch.txt", TORCH_DEPENDENCIES)] {
            let path = requirements_dir.join(file_name);
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("skipping requirements mirror check: {} is absent", path.display());
                    continue;
                }
                Err(err) => panic!("cannot read {}: {err}", path.display()),
            };
            let listed: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("-r "))
                .collect();
            assert_eq!(listed, embedded.to_vec(), "{} must list exactly the installer's specs, in order", path.display());
        }
    }

    #[test]
    fn missing_specs_apply_platform_markers() {
        let installed = installed_map(&[]);
        let required = [
            "onnxruntime; platform_system != \"Windows\"",
            "onnxruntime-directml; platform_system == \"Windows\"",
        ];
        let missing = missing_dependency_specs(&required, &installed);
        assert_eq!(
            missing.len(),
            1,
            "exactly one of the two mutually exclusive specs applies here"
        );
        if cfg!(target_os = "windows") {
            assert!(missing[0].starts_with("onnxruntime-directml"));
        } else {
            assert!(missing[0].starts_with("onnxruntime;"));
        }
    }

    #[test]
    fn command_retry_backoff_doubles_from_two_seconds_and_caps_at_eight() {
        let delays: Vec<u64> = (1..=5).map(|attempt| command_retry_delay(attempt).as_secs()).collect();
        assert_eq!(delays, vec![2, 4, 8, 8, 8]);
    }

    #[test]
    fn command_retry_wait_sleeps_in_slices_and_stops_on_cancel() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};

        let slices = Mutex::new(Vec::new());
        let cancel = AtomicBool::new(false);
        let record = |slice: Duration| {
            if let Ok(mut slices) = slices.lock() {
                slices.push(slice);
            }
        };
        assert!(!wait_unless_cancelled(Duration::from_millis(250), &cancel, &record));
        assert_eq!(slices.lock().map(|slices| slices.len()).unwrap_or(0), 3, "250 ms = 100 + 100 + 50 ms slices");

        // The flag set during the wait ends it at the next slice boundary.
        let cancel_on_sleep = |_slice: Duration| cancel.store(true, Ordering::Relaxed);
        assert!(wait_unless_cancelled(Duration::from_secs(8), &cancel, &cancel_on_sleep));
        assert!(wait_unless_cancelled(Duration::from_secs(8), &cancel, &|_| panic!("a set flag must not sleep")));
    }

    #[test]
    fn parse_pip_freeze_packages_normalizes_distribution_names() {
        let packages = parse_pip_freeze_packages(
            "deep_translator==1.11.4\nopencv-python==4.12.0\n# comment\n-e ./local\n",
        );

        assert_eq!(
            packages.get("deep-translator").map(String::as_str),
            Some("1.11.4")
        );
        assert_eq!(
            packages.get("opencv-python").map(String::as_str),
            Some("4.12.0")
        );
        assert!(!packages.contains_key("local"));
    }

    #[test]
    fn dependency_package_name_ignores_versions_and_markers() {
        let name = |dep: &'static str| Requirement::parse(dep).ok().map(|requirement| requirement.name);
        assert_eq!(name("onnxruntime; platform_system != \"Windows\""), Some("onnxruntime"));
        assert_eq!(name("torch==2.9.1"), Some("torch"));
        assert_eq!(normalize_package_name("deep_translator"), "deep-translator");
    }

    #[test]
    fn parse_executable_version_output_uses_last_version_like_token() {
        assert_eq!(
            parse_executable_version_output("manhwastudio_rs 3.4.0\n").as_deref(),
            Some("3.4.0")
        );
        assert_eq!(
            parse_executable_version_output("ManhwaStudio v3.5.1-beta").as_deref(),
            Some("v3.5.1-beta")
        );
    }

    #[test]
    fn platform_binary_asset_names_match_release_contract() {
        // The release pipeline (`build-all.py`) publishes assets under exactly
        // these names; a typo or swapped pairing here means shipped installs can
        // never update again. Pin every OS × arch constant to its exact literal
        // so any drift fails loudly (exactness also keeps the six names pairwise
        // distinct within one release).
        assert_eq!(WINDOWS_BINARY_ASSET_NAME_X86_64, "manhwastudio_rs.exe");
        assert_eq!(WINDOWS_BINARY_ASSET_NAME_AARCH64, "manhwastudio_rs_arm64.exe");
        assert_eq!(MACOS_BINARY_ASSET_NAME_X86_64, "manhwastudio_rs_macos");
        assert_eq!(MACOS_BINARY_ASSET_NAME_AARCH64, "manhwastudio_rs_macos_arm64");
        assert_eq!(LINUX_BINARY_ASSET_NAME_X86_64, "manhwastudio_rs");
        assert_eq!(LINUX_BINARY_ASSET_NAME_AARCH64, "manhwastudio_rs_linux_arm64");

        // The selector must hand back the exact literal for the compiling
        // OS × arch pair, guarding against miswired constants in its branches.
        let expected = if cfg!(target_os = "windows") {
            if cfg!(target_arch = "aarch64") {
                "manhwastudio_rs_arm64.exe"
            } else {
                "manhwastudio_rs.exe"
            }
        } else if cfg!(target_os = "macos") {
            if cfg!(target_arch = "aarch64") {
                "manhwastudio_rs_macos_arm64"
            } else {
                "manhwastudio_rs_macos"
            }
        } else if cfg!(target_arch = "aarch64") {
            "manhwastudio_rs_linux_arm64"
        } else {
            "manhwastudio_rs"
        };
        assert_eq!(platform_binary_asset_name(), expected);
    }

    #[test]
    fn on_disk_executable_name_never_carries_macos_asset_suffix() {
        // The on-disk / in-archive executable name must not carry the `_macos`
        // suffix that only the release download name uses, otherwise the update
        // strip step would miss the staged binary on macOS. On non-Windows the
        // on-disk name is the bare `manhwastudio_rs`, regardless of arch.
        let on_disk = platform_executable_file_name();
        assert!(
            !on_disk.contains("_macos"),
            "on-disk executable name must not use the release-asset `_macos` suffix, got {on_disk}"
        );

        let asset = platform_binary_asset_name();
        let is_aarch64 = cfg!(target_arch = "aarch64");
        if cfg!(target_os = "windows") {
            assert_eq!(on_disk, "manhwastudio_rs.exe");
        } else {
            assert_eq!(on_disk, "manhwastudio_rs");
        }

        // Backward-compat guard: on x86_64 the asset names must equal the historical
        // values so existing x86_64 installs and older releases keep updating.
        if !is_aarch64 {
            if cfg!(target_os = "windows") {
                assert_eq!(asset, "manhwastudio_rs.exe");
            } else if cfg!(target_os = "macos") {
                assert_eq!(asset, "manhwastudio_rs_macos");
            } else {
                assert_eq!(asset, "manhwastudio_rs");
            }
        }

        if cfg!(target_os = "macos") {
            // The two names intentionally differ on macOS: bare on disk, suffixed
            // as the release download (with an extra `_arm64` on aarch64).
            assert!(asset.starts_with("manhwastudio_rs_macos"));
            assert_ne!(on_disk, asset);
        }
    }

    #[test]
    fn sanitize_windows_archive_path_component_replaces_invalid_names() {
        assert_eq!(
            sanitize_windows_archive_path_component("1: Лента картинок и её параметры"),
            "1_ Лента картинок и её параметры"
        );
        assert_eq!(
            sanitize_windows_archive_path_component("bad<name>|?."),
            "bad_name___"
        );
        assert_eq!(sanitize_windows_archive_path_component("CON"), "CON_");
    }

    #[test]
    fn replace_target_access_treats_elevation_as_a_remedy_only_when_it_can_help() {
        // The expected case: the installer already granted the Users group inheritable
        // Modify rights, so the probe succeeds and no elevation talk belongs in the UI.
        assert_eq!(classify_replace_target_access(true, false), ReplaceTargetAccess::Writable);
        assert_eq!(
            classify_replace_target_access(true, true),
            ReplaceTargetAccess::Writable,
            "already being elevated must not change a successful probe"
        );
        assert_eq!(
            classify_replace_target_access(false, false),
            ReplaceTargetAccess::Blocked,
            "unelevated and blocked: administrator rights are worth offering as one remedy"
        );
        assert_eq!(
            classify_replace_target_access(false, true),
            ReplaceTargetAccess::BlockedWhileElevated,
            "already elevated and still blocked: rights are not the missing piece"
        );
    }

    #[test]
    fn staged_replacement_file_name_stays_in_the_target_directory_and_is_unique() {
        let name = staged_replacement_file_name("manhwastudio_rs.exe", 4242, 99);
        assert!(name.starts_with("manhwastudio_rs.exe"), "{name} must be recognizable as the target");
        assert!(name.ends_with(".part"), "{name} must carry the staging suffix");
        assert!(
            !name.contains('/') && !name.contains('\\'),
            "{name} must not introduce a path separator: the rename must not cross a volume"
        );
        assert_ne!(
            name,
            staged_replacement_file_name("manhwastudio_rs.exe", 4242, 100),
            "two attempts of one process must not collide"
        );
        assert_ne!(
            name,
            staged_replacement_file_name("manhwastudio_rs.exe", 4243, 99),
            "two processes must not collide"
        );
    }

    #[test]
    fn replacing_an_executable_overwrites_the_target_and_leaves_no_staging_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source_binary");
        let target = dir.path().join("installed_binary");
        std::fs::write(&source, b"new build").expect("source must be writable");
        std::fs::write(&target, b"old build").expect("target must be writable");

        replace_executable_with_local_file(&source, &target).expect("replacement must succeed");

        assert_eq!(std::fs::read(&target).expect("target must be readable"), b"new build");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("temp dir must be listable")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name.to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "staging files must not survive: {leftovers:?}");
    }

    #[test]
    fn replacing_a_missing_target_still_produces_the_file() {
        // The user may be repairing an install whose executable is already gone; the
        // staged copy plus rename must create it rather than fail on the missing file.
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source_binary");
        let target = dir.path().join("installed_binary");
        std::fs::write(&source, b"new build").expect("source must be writable");

        replace_executable_with_local_file(&source, &target).expect("replacement must succeed");

        assert_eq!(std::fs::read(&target).expect("target must be readable"), b"new build");
    }

    #[test]
    fn replacing_a_file_with_itself_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("installed_binary");
        std::fs::write(&target, b"old build").expect("target must be writable");

        let error = replace_executable_with_local_file(&target, &target)
            .expect_err("replacing a file with itself must be refused");
        assert!(!error.is_empty(), "the refusal must carry a message");
        assert_eq!(
            std::fs::read(&target).expect("target must be readable"),
            b"old build",
            "a refused replacement must not touch the target"
        );
    }

    #[test]
    fn a_failed_replacement_leaves_the_target_intact() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("installed_binary");
        std::fs::write(&target, b"old build").expect("target must be writable");
        let missing_source = dir.path().join("does_not_exist");

        let error = replace_executable_with_local_file(&missing_source, &target)
            .expect_err("copying a missing source must fail");
        assert!(!error.is_empty(), "the failure must carry a message");
        assert_eq!(
            std::fs::read(&target).expect("target must still exist"),
            b"old build",
            "the installed copy must survive a failed replacement"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("temp dir must be listable")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name.to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "staging files must not survive a failure: {leftovers:?}");
    }

    /// The update's install-type probe must never create or rewrite the install
    /// target's `user_config.json` (no default backfill on disk).
    #[test]
    fn read_current_ai_install_type_is_read_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(super::config::USER_CONFIG_FILE);

        assert_eq!(super::read_current_ai_install_type(dir.path()), super::config::AiInstallType::None);
        assert!(!path.exists(), "an absent user config must stay absent");

        let raw = r#"{"General":{"ai_install_type":"Full"}}"#;
        std::fs::write(&path, raw).expect("fixture must be writable");
        assert_eq!(super::read_current_ai_install_type(dir.path()), super::config::AiInstallType::Full);
        assert_eq!(std::fs::read_to_string(&path).expect("fixture readable"), raw, "no defaults may be backfilled");

        std::fs::write(&path, "{ not json").expect("fixture must be writable");
        assert_eq!(super::read_current_ai_install_type(dir.path()), super::config::AiInstallType::None);
        assert_eq!(std::fs::read_to_string(&path).expect("fixture readable"), "{ not json");
    }

    /// PowerShell single-quoted strings escape `'` by doubling it; nothing else changes.
    #[test]
    fn powershell_single_quotes_are_doubled() {
        assert_eq!(super::escape_ps_single_quote("a'b''c"), "a''b''''c");
        assert_eq!(super::escape_ps_single_quote("plain"), "plain");
    }
}
