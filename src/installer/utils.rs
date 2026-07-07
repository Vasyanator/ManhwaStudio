/*
File: utils.rs

Purpose:
Runs installer backend work outside the egui UI layer.

Main responsibilities:
- download and unpack uv and app release assets;
- create the managed Python environment and install Python dependencies;
- install static base dependencies for every install and torch-dependent extras only for full installs;
- install CPU or GPU PyTorch wheels selected by the UI;
- sanitize ZIP entry path components that are invalid on Windows before writing files;
- handle platform integration helpers such as elevation, shortcuts, registry entries, and uninstall cleanup.
- on Windows Program Files installs, create the root install directory and grant inheritable Users
  modify rights before installer-managed files are created.

Notes:
The initial install flow intentionally does not download application AI model weights. Runtime
code downloads app-managed models lazily through `ai_models.rs` when a feature needs them.
*/

use std::collections::HashMap;
use std::env;
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use ms_thread as thread;
use web_time::SystemTime;
use web_time::{Duration, Instant};

use crate::config;
use crate::gpu_utils::{
    RuntimeVersion, detect_amd_gpu_linux, detect_cuda_runtime_version,
    detect_nvidia_compute_capability, detect_nvidia_gpu, detect_rocm_runtime_version,
};
use crate::python_manager;
use eframe::egui;
use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::Archive;
use zip::ZipArchive;

use super::install::{
    EMBEDDED_APP_ICON_ICO, EMBEDDED_APP_ICON_PNG, INSTALL_SUBDIR_NAME, InstallDependencyProfile,
    InstallEvent, TorchBackend, TorchChoicePrompt, TorchInstallSelection, TorchPreflightResult,
    TorchWheelOption,
};
#[cfg(target_os = "windows")]
use super::install::{UninstallEvent, run_windows_uninstall_window, send_uninstall_progress};

const UV_RELEASE_API: &str = "https://api.github.com/repos/astral-sh/uv/releases/latest";
const APP_RELEASES_API: &str = "https://api.github.com/repos/Vasyanator/ManhwaStudio/releases";
const APP_ZIP_ASSET_NAME: &str = "ManhwaStudio.zip";
const PYTHON_VERSION_REQUEST: &str = "3.11";
const TORCH_VERSION: &str = "2.9.1";
const TORCHVISION_VERSION: &str = "0.24.1";
const PADDLE_CU126_INDEX_URL: &str = "https://www.paddlepaddle.org.cn/packages/stable/cu126/";
const ENABLE_PADDLE_CUDA_EXTRA_PACKAGES_INSTALL: bool = false;
const BASE_DEPENDENCIES: &[&str] = &[
    "cloakbrowser",
    "deep-translator",
    "jaconv",
    "numpy",
    "onnxruntime; platform_system != \"Windows\"",
    "onnxruntime-directml; platform_system == \"Windows\"",
    "opencv-python",
    "Pillow",
    "playwright",
    "pyclipper",
    "requests",
    "selenium",
    "transformers",
    // Backs the loopback-WebSocket fallback transport used when AF_UNIX is
    // unavailable (Windows). Kept unconditional: wsproto is a tiny pure-Python
    // package, so installing it everywhere avoids a Windows-only branch that would
    // otherwise have to stay in sync with the runtime transport selection.
    "wsproto",
];
const TORCH_DEPENDENCIES: &[&str] = &[
    "certifi",
    "diffusers",
    "easydict",
    "easyocr",
    "einops",
    "kornia",
    "manga-ocr",
    "omegaconf",
    "packaging",
    "pandas",
    "pytorch-lightning",
    "PyYAML",
    "reline",
    "shapely",
    "surya-ocr",
    "tqdm",
];

fn send_progress(
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

fn send_console_line(tx: &mpsc::Sender<InstallEvent>, line: impl Into<String>) {
    let _ = tx.send(InstallEvent::ConsoleLine(line.into()));
}

#[derive(Deserialize)]
struct GithubRelease {
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubReleaseListItem {
    tag_name: Option<String>,
    name: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize, Clone)]
pub(crate) struct GithubAsset {
    pub(crate) name: String,
    pub(crate) browser_download_url: String,
}

#[derive(Debug)]
pub(crate) enum UpdateWorkerEvent {
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
pub(crate) struct ExternalUpdateTarget {
    pub(crate) root_dir: PathBuf,
    pub(crate) executable_path: PathBuf,
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

pub(super) fn run_install_worker(
    root_dir: PathBuf,
    launcher_exe_path: Option<PathBuf>,
    dependency_profile: InstallDependencyProfile,
    torch_selection: TorchInstallSelection,
    tx: &mpsc::Sender<InstallEvent>,
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

    let _ = tx.send(InstallEvent::Step("Подготовка директорий...".to_string()));
    prepare_install_root_dir(&root_dir)?;
    let installer_dir = root_dir.join("installer_files");
    fs::create_dir_all(&installer_dir)
        .map_err(|e| format!("не удалось создать installer_files: {e}"))?;
    let downloads_dir = installer_dir.join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| format!("не удалось создать installer_files/downloads: {e}"))?;
    send_progress(tx, 1.0, "Подготовка", prep_range.1, "Подготовка завершена");

    let resolved_arch = detect_arch()?;
    let platform = detect_platform()?;

    let _ = tx.send(InstallEvent::Step(format!(
        "Поиск последней сборки uv для {platform}/{resolved_arch}..."
    )));
    let asset = fetch_latest_uv_asset(platform, &resolved_arch)?;
    let archive_path = downloads_dir.join(&asset.name);

    let _ = tx.send(InstallEvent::Step(format!("Скачивание {}...", asset.name)));
    download_asset(
        &asset.browser_download_url,
        &archive_path,
        tx,
        python_range.0,
        lerp(python_range.0, python_range.1, 0.55),
        "uv",
    )?;

    let uv_dir = installer_dir.join("uv");
    if uv_dir.exists() {
        let _ = tx.send(InstallEvent::Step(
            "Удаление предыдущего installer_files/uv...".to_string(),
        ));
        fs::remove_dir_all(&uv_dir).map_err(|e| {
            format!(
                "не удалось очистить старую папку '{}': {e}",
                uv_dir.display()
            )
        })?;
    }
    fs::create_dir_all(&uv_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", uv_dir.display()))?;

    let _ = tx.send(InstallEvent::Step(
        "Распаковка архива в installer_files/uv...".to_string(),
    ));
    extract_archive(
        &archive_path,
        &uv_dir,
        tx,
        "Распаковка uv",
        lerp(python_range.0, python_range.1, 0.55),
        python_range.1,
    )?;
    flatten_single_root_dir(&uv_dir)?;
    let uv_exe = resolve_uv_executable(&uv_dir)?;
    let pip_runner = PipInstallRunner::Uv(uv_exe.clone());

    let uv_python_dir = uv_dir.join("python");
    let uv_cache_dir = uv_dir.join("cache");
    fs::create_dir_all(&uv_python_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", uv_python_dir.display()))?;
    fs::create_dir_all(&uv_cache_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", uv_cache_dir.display()))?;
    let uv_python_dir_str = uv_python_dir.to_string_lossy().into_owned();
    let uv_cache_dir_str = uv_cache_dir.to_string_lossy().into_owned();
    let uv_env = [
        ("UV_PYTHON_INSTALL_DIR", uv_python_dir_str.as_str()),
        ("UV_CACHE_DIR", uv_cache_dir_str.as_str()),
    ];

    let _ = tx.send(InstallEvent::Step(format!(
        "Установка Python {PYTHON_VERSION_REQUEST} через uv..."
    )));
    run_command_with_retry(
        &uv_exe,
        &root_dir,
        &["python", "install", PYTHON_VERSION_REQUEST],
        &format!("установка Python {PYTHON_VERSION_REQUEST} через uv"),
        2,
        Some(tx),
        &uv_env,
    )?;

    let managed_venv_dir = installer_dir.join("venv");
    if managed_venv_dir.exists() {
        let _ = tx.send(InstallEvent::Step(
            "Удаление предыдущего installer_files/venv...".to_string(),
        ));
        fs::remove_dir_all(&managed_venv_dir).map_err(|e| {
            format!(
                "не удалось очистить старую папку '{}': {e}",
                managed_venv_dir.display()
            )
        })?;
    }

    let managed_venv_dir_str = managed_venv_dir.to_string_lossy().into_owned();
    let _ = tx.send(InstallEvent::Step(
        "Создание installer_files/venv через uv...".to_string(),
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
        "создание installer_files/venv через uv",
        2,
        Some(tx),
        &uv_env,
    )?;
    let python_exe = python_manager::resolve_python_executable_in_dir(&managed_venv_dir)?;

    let _ = tx.send(InstallEvent::Step(
        "Поиск ManhwaStudio.zip в последнем релизе...".to_string(),
    ));
    let app_asset = fetch_latest_app_zip_asset()?;
    let app_zip_path = downloads_dir.join(APP_ZIP_ASSET_NAME);

    let _ = tx.send(InstallEvent::Step(
        "Скачивание ManhwaStudio.zip...".to_string(),
    ));
    download_asset(
        &app_asset.browser_download_url,
        &app_zip_path,
        tx,
        app_range.0,
        lerp(app_range.0, app_range.1, 0.50),
        "ManhwaStudio.zip",
    )?;

    let app_extract_dir = downloads_dir.join("manhwastudio_extract");
    if app_extract_dir.exists() {
        fs::remove_dir_all(&app_extract_dir).map_err(|e| {
            format!(
                "не удалось очистить временную папку '{}': {e}",
                app_extract_dir.display()
            )
        })?;
    }
    fs::create_dir_all(&app_extract_dir).map_err(|e| {
        format!(
            "не удалось создать временную папку '{}': {e}",
            app_extract_dir.display()
        )
    })?;

    let _ = tx.send(InstallEvent::Step(
        "Распаковка ManhwaStudio.zip...".to_string(),
    ));
    extract_archive(
        &app_zip_path,
        &app_extract_dir,
        tx,
        "Распаковка ManhwaStudio.zip",
        lerp(app_range.0, app_range.1, 0.50),
        lerp(app_range.0, app_range.1, 0.85),
    )?;
    flatten_single_root_dir(&app_extract_dir)?;

    let _ = tx.send(InstallEvent::Step(
        "Копирование файлов ManhwaStudio в рабочую папку...".to_string(),
    ));
    merge_dir_contents(&app_extract_dir, &root_dir)?;
    write_embedded_app_icon(&root_dir)?;
    copy_launcher_to_install_dir(launcher_exe_path.as_deref(), &root_dir, tx)?;
    send_progress(
        tx,
        1.0,
        "Копирование файлов ManhwaStudio",
        app_range.1,
        "Файлы ManhwaStudio развернуты",
    );
    fs::remove_dir_all(&app_extract_dir).map_err(|e| {
        format!(
            "не удалось удалить временную папку '{}': {e}",
            app_extract_dir.display()
        )
    })?;

    let _ = tx.send(InstallEvent::Step(
        "Установка базовых Python-зависимостей...".to_string(),
    ));
    install_static_python_dependencies(DependencyInstallRequest {
        root_dir: &root_dir,
        pip_runner: &pip_runner,
        python_exe: &python_exe,
        tx,
        label: "базовых зависимостей",
        dependencies: BASE_DEPENDENCIES,
        overall_start: base_deps_range.0,
        overall_end: base_deps_range.1,
    })?;

    match dependency_profile {
        InstallDependencyProfile::Fast => {
            send_progress(
                tx,
                1.0,
                "PyTorch: быстрый режим",
                torch_range.1,
                "PyTorch и torch-зависимости пропущены",
            );
            send_progress(
                tx,
                1.0,
                "Torch-зависимости: быстрый режим",
                torch_deps_range.1,
                "Torch-зависимости пропущены",
            );
        }
        InstallDependencyProfile::Full => {
            install_torch_stage(
                &root_dir,
                &pip_runner,
                &python_exe,
                &torch_selection,
                tx,
                torch_range.0,
                torch_range.1,
            )?;
            let install_cuda_extra_packages = ENABLE_PADDLE_CUDA_EXTRA_PACKAGES_INSTALL
                && matches!(&torch_selection, TorchInstallSelection::InstallGpu(option) if option.backend == TorchBackend::Cuda);
            install_torch_python_dependencies(
                &root_dir,
                &pip_runner,
                &python_exe,
                tx,
                install_cuda_extra_packages,
                torch_deps_range.0,
                torch_deps_range.1,
            )?;
        }
    }

    finalize_windows_post_install(&root_dir, tx, windows_post_range.0, windows_post_range.1)?;
    send_progress(tx, 1.0, "Установка завершена", 1.0, "Установка завершена");

    Ok(())
}

pub(crate) fn run_torch_upgrade_worker(
    root_dir: PathBuf,
    torch_selection: TorchInstallSelection,
    install_full_dependencies: bool,
    tx: &mpsc::Sender<InstallEvent>,
) -> Result<(), String> {
    let _ = tx.send(InstallEvent::Step(
        "Подготовка Python-окружения для PyTorch...".to_string(),
    ));
    let python_exe = python_manager::resolve_python_executable(&root_dir)
        .map_err(|err| format!("не удалось найти Python окружение для установки PyTorch: {err}"))?;
    let pip_runner = resolve_runtime_pip_runner(&root_dir, &python_exe);
    send_console_line(
        tx,
        format!(
            "[PyTorch] Python: {}; installer: {}",
            python_exe.display(),
            pip_runner.label()
        ),
    );

    let torch_range = if install_full_dependencies {
        (0.0, 0.42)
    } else {
        (0.0, 1.0)
    };
    install_torch_stage(
        &root_dir,
        &pip_runner,
        &python_exe,
        &torch_selection,
        tx,
        torch_range.0,
        torch_range.1,
    )?;

    if install_full_dependencies {
        let install_cuda_extra_packages = ENABLE_PADDLE_CUDA_EXTRA_PACKAGES_INSTALL
            && matches!(&torch_selection, TorchInstallSelection::InstallGpu(option) if option.backend == TorchBackend::Cuda);
        install_torch_python_dependencies(
            &root_dir,
            &pip_runner,
            &python_exe,
            tx,
            install_cuda_extra_packages,
            0.42,
            1.0,
        )?;
    }

    send_progress(tx, 1.0, "PyTorch установка завершена", 1.0, "Готово");
    Ok(())
}

pub(crate) fn run_update_binary_stage(root_dir: PathBuf, tx: &mpsc::Sender<UpdateWorkerEvent>) {
    let result = run_update_binary_stage_inner(&root_dir, None, tx);
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
) {
    let result = run_update_binary_stage_inner(&target.root_dir, Some(&target.executable_path), tx);
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

pub(crate) fn run_update_continuation_stage(
    root_dir: PathBuf,
    torch_selection: Option<TorchInstallSelection>,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
) {
    match run_update_continuation_stage_inner(&root_dir, torch_selection, tx) {
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
) -> Result<UpdateBinaryStageOutcome, String> {
    let current_exe = match target_executable {
        Some(executable) => executable.to_path_buf(),
        None => env::current_exe()
            .map_err(|e| format!("не удалось определить путь текущего executable: {e}"))?,
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
        "Проверка последнего релиза ManhwaStudio...".to_string(),
    ));
    send_update_progress(tx, 0.0, "Подготовка", 0.0, "Этап: обновление бинарника");
    let remote_version =
        fetch_latest_app_release_tag_with_required_asset(platform_binary_asset_name())?;
    if compare_version_strings(&remote_version, &local_version).is_le() {
        return Ok(UpdateBinaryStageOutcome::NoUpdate {
            local_version,
            remote_version,
        });
    }

    let _ = tx.send(UpdateWorkerEvent::Step(format!(
        "Доступна версия {remote_version}. Скачивание бинарника..."
    )));
    let binary_asset = fetch_latest_app_asset_by_name(platform_binary_asset_name())?;
    let downloads_dir = root_dir.join("installer_files").join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", downloads_dir.display()))?;
    let downloaded_binary = downloads_dir.join(format!("{}.update", binary_asset.name));

    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    download_asset(
        &binary_asset.browser_download_url,
        &downloaded_binary,
        &install_tx,
        0.0,
        0.82,
        &binary_asset.name,
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
        send_update_progress(tx, 1.0, "Бинарник скачан", 0.92, "Подготовка перезапуска");
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
        "Перезапуск запущен",
        1.0,
        "Перезапуск в новую версию",
    );
    Ok(UpdateBinaryStageOutcome::RelaunchStarted)
}

fn run_update_continuation_stage_inner(
    root_dir: &Path,
    torch_selection: Option<TorchInstallSelection>,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
) -> Result<UpdateContinuationOutcome, String> {
    let mut progress_cursor = 0.0_f32;
    let env_range = alloc_progress_range(&mut progress_cursor, 0.22);
    let torch_range = alloc_progress_range(&mut progress_cursor, 0.20);
    let deps_range = alloc_progress_range(&mut progress_cursor, 0.28);
    let app_range = (progress_cursor, 1.0_f32);

    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    let (uv_exe, python_exe) = ensure_uv_managed_python_environment(root_dir, tx, env_range)?;
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
            &install_tx,
            torch_range,
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
            "PyTorch не требуется",
            torch_range.1,
            "Этап PyTorch пропущен",
        );
    }

    install_missing_dependencies_for_update(
        root_dir,
        &pip_runner,
        &python_exe,
        install_type,
        tx,
        deps_range,
    )?;
    download_and_extract_app_archive_for_update(root_dir, tx, app_range)?;
    send_update_progress(tx, 1.0, "Обновление завершено", 1.0, "Готово");
    Ok(UpdateContinuationOutcome::Completed)
}

/// Returns the GitHub *release asset* download file name of the main executable
/// for the current platform. This is the name of the file uploaded to the
/// release, NOT the executable's on-disk name — see `platform_executable_file_name`.
///
/// Must stay in sync with `platform_binary_asset_name` in `update.rs`:
/// Windows → `manhwastudio_rs.exe`, macOS → `manhwastudio_rs_macos` (suffixed so
/// it does not collide with the bare Linux asset), Linux and other Unix →
/// `manhwastudio_rs`.
///
/// On macOS the asset name intentionally differs from the on-disk executable
/// name: the release ships the binary renamed to `manhwastudio_rs_macos`, but on
/// disk (and inside `ManhwaStudio.zip`) the executable is the bare
/// `manhwastudio_rs`. Use this function ONLY for the release download; use
/// `platform_executable_file_name` for anything that names the file on disk.
fn platform_binary_asset_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "manhwastudio_rs.exe"
    } else if cfg!(target_os = "macos") {
        "manhwastudio_rs_macos"
    } else {
        "manhwastudio_rs"
    }
}

/// Returns the *on-disk* file name of the main executable for the current
/// platform: Windows → `manhwastudio_rs.exe`, everything else (Linux and macOS) →
/// `manhwastudio_rs` (no `_macos` suffix).
///
/// This is the name the executable carries on disk and inside the extracted
/// `ManhwaStudio.zip` payload, which is deliberately different from the release
/// download name on macOS (see `platform_binary_asset_name`). On Windows/Linux
/// the two coincide; on macOS they differ, so any code that strips or locates the
/// executable within the install tree or an extracted archive must use this
/// function, not the release-asset name.
fn platform_executable_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "manhwastudio_rs.exe"
    } else {
        "manhwastudio_rs"
    }
}

fn query_executable_version(executable: &Path, root_dir: &Path) -> Result<String, String> {
    let (status, output) = run_command_streaming(executable, root_dir, &["--version"], None, &[])?;
    if !status.success() {
        return Err(format!(
            "не удалось получить версию '{}': команда --version завершилась с ошибкой\n{}",
            executable.display(),
            output.trim()
        ));
    }
    parse_executable_version_output(&output).ok_or_else(|| {
        format!(
            "не удалось распознать версию из вывода '{} --version': {}",
            executable.display(),
            output.trim()
        )
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
            format!(
                "не удалось выставить executable permissions для '{}': {e}",
                downloaded_binary.display()
            )
        })?;
    }
    fs::rename(downloaded_binary, current_exe).map_err(|e| {
        format!(
            "не удалось заменить executable '{}' файлом '{}': {e}",
            current_exe.display(),
            downloaded_binary.display()
        )
    })
}

#[cfg(not(target_os = "windows"))]
fn spawn_continue_update_process(root_dir: &Path, executable: &Path) -> Result<(), String> {
    let mut cmd = Command::new(executable);
    cmd.current_dir(root_dir).arg("--continue-update");
    cmd.spawn().map_err(|e| {
        format!(
            "не удалось запустить '{}' --continue-update: {e}",
            executable.display()
        )
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
        .map_err(|e| format!("не удалось записать '{}': {e}", script_path.display()))?;
    let mut cmd = Command::new("cmd");
    apply_windows_no_window(&mut cmd);
    cmd.current_dir(root_dir).args(["/C", "start", "", "/MIN"]);
    cmd.arg(&script_path);
    cmd.spawn().map_err(|e| {
        format!(
            "не удалось запустить replacement script '{}': {e}",
            script_path.display()
        )
    })?;
    Ok(())
}

fn ensure_uv_managed_python_environment(
    root_dir: &Path,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    overall_range: (f32, f32),
) -> Result<(PathBuf, PathBuf), String> {
    let uv_exe = ensure_uv_runtime(
        root_dir,
        tx,
        overall_range.0,
        lerp(overall_range.0, overall_range.1, 0.45),
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
            "Создание нового installer_files/venv через uv...".to_string(),
        ));
        if managed_venv_dir.exists() {
            fs::remove_dir_all(&managed_venv_dir).map_err(|e| {
                format!(
                    "не удалось удалить старое окружение '{}': {e}",
                    managed_venv_dir.display()
                )
            })?;
        }
        let uv_env = build_uv_env(root_dir)?;
        run_command_with_retry(
            &uv_exe,
            root_dir,
            &["python", "install", PYTHON_VERSION_REQUEST],
            &format!("установка Python {PYTHON_VERSION_REQUEST} через uv"),
            2,
            None,
            &uv_env.as_env_slice(),
        )?;
        let venv_dir = managed_venv_dir.to_string_lossy().into_owned();
        run_command_with_retry(
            &uv_exe,
            root_dir,
            &["venv", "--python", PYTHON_VERSION_REQUEST, &venv_dir],
            "создание installer_files/venv через uv",
            2,
            None,
            &uv_env.as_env_slice(),
        )?;
    }

    let python_exe = python_manager::resolve_python_executable_in_dir(&managed_venv_dir)?;
    send_update_progress(
        tx,
        1.0,
        "Python окружение готово",
        overall_range.1,
        "Python окружение готово",
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
        .map_err(|e| format!("не удалось создать '{}': {e}", uv_python_dir.display()))?;
    fs::create_dir_all(&uv_cache_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", uv_cache_dir.display()))?;
    Ok(UvEnv {
        python_dir: uv_python_dir.to_string_lossy().into_owned(),
        cache_dir: uv_cache_dir.to_string_lossy().into_owned(),
    })
}

fn ensure_uv_runtime(
    root_dir: &Path,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    overall_start: f32,
    overall_end: f32,
) -> Result<PathBuf, String> {
    let uv_dir = root_dir.join("installer_files").join("uv");
    if uv_dir.is_dir()
        && let Ok(uv_exe) = resolve_uv_executable(&uv_dir)
    {
        send_update_progress(tx, 1.0, "uv уже установлен", overall_end, "uv готов");
        return Ok(uv_exe);
    }

    let _ = tx.send(UpdateWorkerEvent::Step(
        "Скачивание uv для обновления окружения...".to_string(),
    ));
    fs::create_dir_all(&uv_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", uv_dir.display()))?;
    let downloads_dir = root_dir.join("installer_files").join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", downloads_dir.display()))?;
    let asset = fetch_latest_uv_asset(detect_platform()?, &detect_arch()?)?;
    let archive_path = downloads_dir.join(&asset.name);
    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    download_asset(
        &asset.browser_download_url,
        &archive_path,
        &install_tx,
        overall_start,
        lerp(overall_start, overall_end, 0.55),
        "uv",
    )?;
    if uv_dir.exists() {
        fs::remove_dir_all(&uv_dir)
            .map_err(|e| format!("не удалось очистить '{}': {e}", uv_dir.display()))?;
    }
    fs::create_dir_all(&uv_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", uv_dir.display()))?;
    extract_archive(
        &archive_path,
        &uv_dir,
        &install_tx,
        "Распаковка uv",
        lerp(overall_start, overall_end, 0.55),
        overall_end,
    )?;
    flatten_single_root_dir(&uv_dir)?;
    resolve_uv_executable(&uv_dir)
}

fn read_current_ai_install_type(root_dir: &Path) -> config::AiInstallType {
    let cfg = config::JsonConfig::new(
        root_dir.join(config::USER_CONFIG_FILE),
        config::user_config_defaults(),
    );
    match cfg {
        Ok(cfg) => config::AiInstallType::from_user_settings(&cfg.data),
        Err(_) => config::AiInstallType::None,
    }
}

fn maybe_update_torch(
    root_dir: &Path,
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    torch_selection: Option<TorchInstallSelection>,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    install_tx: &mpsc::Sender<InstallEvent>,
    overall_range: (f32, f32),
) -> Result<UpdateContinuationOutcome, String> {
    let installed = freeze_installed_packages(pip_runner, python_exe, root_dir, tx)?;
    let installed_torch = installed.get("torch").map(String::as_str);
    if installed_torch
        .is_some_and(|version| compare_version_strings(version, TORCH_VERSION).is_ge())
    {
        send_update_progress(
            tx,
            1.0,
            "PyTorch уже актуален",
            overall_range.1,
            "Этап PyTorch завершён",
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
        install_tx,
        overall_range.0,
        overall_range.1,
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
) -> Result<(), String> {
    let installed = freeze_installed_packages(pip_runner, python_exe, root_dir, tx)?;
    let mut required = BASE_DEPENDENCIES
        .iter()
        .copied()
        .filter(|dep| dependency_marker_matches_current_platform(dep))
        .collect::<Vec<_>>();
    if install_type == config::AiInstallType::Full {
        required.extend(TORCH_DEPENDENCIES.iter().copied());
    }

    let missing = required
        .into_iter()
        .filter(|dep| {
            dependency_package_name(dep)
                .map(|name| !installed.contains_key(&normalize_package_name(name)))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();

    if missing.is_empty() {
        send_update_progress(
            tx,
            1.0,
            "Python зависимости актуальны",
            overall_range.1,
            "Зависимости актуальны",
        );
        return Ok(());
    }

    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    let total = missing.len().max(1) as f32;
    for (idx, dep) in missing.iter().enumerate() {
        let start_ratio = idx as f32 / total;
        let end_ratio = (idx + 1) as f32 / total;
        let _ = tx.send(UpdateWorkerEvent::Step(format!(
            "Установка отсутствующей зависимости: {dep}"
        )));
        run_pip_install_with_retry(
            pip_runner,
            python_exe,
            root_dir,
            &[*dep],
            &format!("установка отсутствующей зависимости '{dep}'"),
            3,
            Some(&install_tx),
        )?;
        send_update_progress(
            tx,
            end_ratio,
            format!("Установлено: {dep}"),
            lerp(overall_range.0, overall_range.1, end_ratio),
            format!("Зависимости: {}/{}", idx + 1, missing.len()),
        );
        if start_ratio == 0.0 {
            send_update_console_line(tx, "[Update] Missing dependencies are being installed");
        }
    }
    Ok(())
}

fn download_and_extract_app_archive_for_update(
    root_dir: &Path,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
    overall_range: (f32, f32),
) -> Result<(), String> {
    let _ = tx.send(UpdateWorkerEvent::Step(
        "Скачивание ManhwaStudio.zip...".to_string(),
    ));
    let downloads_dir = root_dir.join("installer_files").join("downloads");
    fs::create_dir_all(&downloads_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", downloads_dir.display()))?;
    let app_asset = fetch_latest_app_zip_asset()?;
    let archive_path = downloads_dir.join(&app_asset.name);
    let install_tx = UpdateToInstallEventBridge { tx }.sender();
    download_asset(
        &app_asset.browser_download_url,
        &archive_path,
        &install_tx,
        overall_range.0,
        lerp(overall_range.0, overall_range.1, 0.45),
        "ManhwaStudio.zip",
    )?;

    let staging_dir = root_dir.join("installer_files").join("update_extract");
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)
            .map_err(|e| format!("не удалось очистить '{}': {e}", staging_dir.display()))?;
    }
    fs::create_dir_all(&staging_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", staging_dir.display()))?;
    extract_archive(
        &archive_path,
        &staging_dir,
        &install_tx,
        "Распаковка ManhwaStudio.zip",
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
        "Архив приложения распакован",
        overall_range.1,
        "Файлы приложения обновлены",
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
            format!(
                "не удалось удалить staged executable '{}': {e}",
                binary_path.display()
            )
        })?;
    }
    Ok(())
}

fn freeze_installed_packages(
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    root_dir: &Path,
    tx: &mpsc::Sender<UpdateWorkerEvent>,
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
    send_update_console_line(tx, format!("$ {} {}", executable.display(), args.join(" ")));
    let (status, output) = run_command_streaming(executable, root_dir, &args_ref, None, &[])?;
    if !status.success() {
        return Err(format!(
            "uv pip freeze завершился ошибкой:\n{}",
            output.trim()
        ));
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

fn dependency_package_name(dep: &str) -> Option<&str> {
    let without_marker = dep.split_once(';').map_or(dep, |(left, _)| left).trim();
    let end = without_marker
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.'))
        .unwrap_or(without_marker.len());
    let name = without_marker[..end].trim();
    (!name.is_empty()).then_some(name)
}

fn normalize_package_name(name: &str) -> String {
    name.trim().replace(['_', '.'], "-").to_ascii_lowercase()
}

fn compare_version_strings(left: &str, right: &str) -> std::cmp::Ordering {
    let left_parts = parse_version_parts(left);
    let right_parts = parse_version_parts(right);
    for (left, right) in left_parts.iter().zip(right_parts.iter()) {
        let ordering = match (left, right) {
            (VersionPart::Number(left), VersionPart::Number(right)) => left.cmp(right),
            (VersionPart::Text(left), VersionPart::Text(right)) => left.cmp(right),
            (VersionPart::Number(_), VersionPart::Text(_)) => std::cmp::Ordering::Greater,
            (VersionPart::Text(_), VersionPart::Number(_)) => std::cmp::Ordering::Less,
        };
        if !ordering.is_eq() {
            return ordering;
        }
    }
    left_parts.len().cmp(&right_parts.len())
}

fn parse_version_parts(version: &str) -> Vec<VersionPart> {
    let normalized = version
        .trim()
        .strip_prefix('v')
        .or_else(|| version.trim().strip_prefix('V'))
        .unwrap_or_else(|| version.trim());
    normalized
        .split(['.', '-', '+', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| match part.parse::<u64>() {
            Ok(value) => VersionPart::Number(value),
            Err(_) => VersionPart::Text(part.to_ascii_lowercase()),
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum VersionPart {
    Number(u64),
    Text(String),
}

fn prepare_install_root_dir(root_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(root_dir).map_err(|e| {
        format!(
            "не удалось создать папку установки '{}': {e}",
            root_dir.display()
        )
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
            .map_err(|e| format!("не удалось определить путь текущего exe: {e}"))?,
    };
    let file_name = source
        .file_name()
        .ok_or_else(|| format!("не удалось получить имя файла для '{}'", source.display()))?;
    let target = target_root.join(file_name);

    if paths_point_to_same_file(&source, &target) {
        send_console_line(
            tx,
            format!(
                "текущий exe уже находится в целевой папке: '{}'",
                target.display()
            ),
        );
        return Ok(());
    }

    fs::copy(&source, &target).map_err(|e| {
        format!(
            "не удалось скопировать текущий exe '{}' -> '{}': {e}",
            source.display(),
            target.display()
        )
    })?;

    if let Ok(meta) = fs::metadata(&source) {
        let _ = fs::set_permissions(&target, meta.permissions());
    }

    send_console_line(
        tx,
        format!(
            "Скопирован текущий exe: '{}' -> '{}'",
            source.display(),
            target.display()
        ),
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
        format!(
            "не удалось записать встроенную иконку '{}': {e}",
            icon_path.display()
        )
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
            .ok_or_else(|| "не удалось определить AppData\\Roaming".to_string())?;
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

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or(home_dir_path()?.join(".local").join("share"));
        return Ok(base.join(INSTALL_SUBDIR_NAME));
    }

    #[allow(unreachable_code)]
    Err("неподдерживаемая ОС".to_string())
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

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return Ok(PathBuf::from("/opt").join(INSTALL_SUBDIR_NAME));
    }

    #[allow(unreachable_code)]
    Err("неподдерживаемая ОС".to_string())
}

#[cfg(not(target_os = "windows"))]
fn home_dir_path() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "не удалось определить домашнюю папку пользователя".to_string())
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

#[cfg(target_os = "windows")]
pub(super) fn is_running_elevated() -> bool {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut out_size: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut out_size,
        ) != 0;
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

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
    relaunch_self_elevated_with_args(root_dir, &args)
}

#[cfg(target_os = "macos")]
pub(super) fn relaunch_self_elevated(root_dir: &Path, target_dir: &Path) -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| format!("не удалось определить путь exe: {e}"))?;
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
        .map_err(|e| format!("не удалось запросить повышение прав через osascript: {e}"))?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "windows")))]
pub(super) fn relaunch_self_elevated(root_dir: &Path, target_dir: &Path) -> Result<(), String> {
    let exe = env::current_exe().map_err(|e| format!("не удалось определить путь exe: {e}"))?;

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
        .map_err(|e| format!("не удалось запросить повышение прав через sudo/pkexec: {e}"))?;
    Ok(())
}

#[cfg(not(any(unix, target_os = "windows")))]
pub(super) fn relaunch_self_elevated(_root_dir: &Path, _target_dir: &Path) -> Result<(), String> {
    Err("запрос повышения прав не поддерживается на этой ОС".to_string())
}

#[cfg(target_os = "windows")]
fn relaunch_self_elevated_with_args(root_dir: &Path, args: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = env::current_exe().map_err(|e| format!("не удалось определить путь exe: {e}"))?;
    let verb = to_wide("runas");
    let exe_w = to_wide(exe.to_string_lossy().as_ref());
    let args_w = to_wide(args);
    let root_dir_w = to_wide(root_dir.to_string_lossy().as_ref());
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe_w.as_ptr(),
            args_w.as_ptr(),
            root_dir_w.as_ptr(),
            SW_SHOWNORMAL,
        )
    };
    if (result as isize) <= 32 {
        return Err("система отклонила запуск с повышением прав (UAC)".to_string());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn run_windows_create_start_menu_shortcut_for_install(
    install_dir: &Path,
    continue_create_start_menu_shortcut: bool,
) -> Result<(), String> {
    if create_start_menu_shortcut_requires_elevation(install_dir) {
        if continue_create_start_menu_shortcut {
            return Err(
                "Создание ярлыка меню Пуск было перезапущено с флагом продолжения, но права администратора не получены."
                    .to_string(),
            );
        }
        let args = build_windows_create_start_menu_shortcut_args(install_dir, true);
        relaunch_self_elevated_with_args(install_dir, &args)?;
        return Ok(());
    }

    create_windows_start_menu_shortcut(install_dir).map(|_| ())
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn run_windows_create_start_menu_shortcut_for_install(
    _install_dir: &Path,
    _continue_create_start_menu_shortcut: bool,
) -> Result<(), String> {
    Err("Создание ярлыка меню Пуск не поддерживается на этой ОС".to_string())
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

#[cfg(target_os = "windows")]
pub(super) fn create_windows_desktop_shortcut(install_dir: &Path) -> Result<PathBuf, String> {
    let desktop_dir = windows_desktop_dir()
        .ok_or_else(|| "не удалось определить Desktop для создания ярлыка".to_string())?;
    let shortcut_path = desktop_dir.join("ManhwaStudio.lnk");
    create_windows_shortcut_at(install_dir, &shortcut_path)?;
    Ok(shortcut_path)
}

#[cfg(target_os = "windows")]
fn windows_desktop_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|p| p.join("Desktop"))
        .filter(|p| p.is_dir())
}

#[cfg(target_os = "windows")]
fn create_windows_shortcut_at(install_dir: &Path, shortcut_path: &Path) -> Result<(), String> {
    let launcher_path = resolve_windows_launcher_target(install_dir)?;
    if let Some(parent) = shortcut_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "не удалось создать папку ярлыка '{}': {e}",
                parent.display()
            )
        })?;
    }
    // Берём иконку из самого exe (PE-ресурс), чтобы Windows гарантированно подхватывал её.
    let icon_location = format!("{},0", launcher_path.to_string_lossy());
    let script = format!(
        "$ws=New-Object -ComObject WScript.Shell; \
         $sc=$ws.CreateShortcut('{shortcut}'); \
         $sc.TargetPath='{target}'; \
         $sc.WorkingDirectory='{workdir}'; \
         $sc.IconLocation='{icon}'; \
         $sc.Save();",
        shortcut = escape_ps_single_quote(&shortcut_path.to_string_lossy()),
        target = escape_ps_single_quote(&launcher_path.to_string_lossy()),
        workdir = escape_ps_single_quote(&install_dir.to_string_lossy()),
        icon = escape_ps_single_quote(&icon_location),
    );

    let mut cmd = Command::new("powershell");
    apply_windows_no_window(&mut cmd);
    let status = cmd
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg(script)
        .status()
        .map_err(|e| format!("не удалось запустить PowerShell для создания ярлыка: {e}"))?;

    if !status.success() {
        return Err(format!(
            "PowerShell завершился с кодом {} при создании ярлыка",
            status.code().unwrap_or(-1)
        ));
    }

    Ok(())
}

#[cfg(target_os = "windows")]
pub(super) fn resolve_windows_launcher_target(install_dir: &Path) -> Result<PathBuf, String> {
    let preferred_main = install_dir.join("manhwastudio_rs.exe");
    if preferred_main.is_file() {
        return Ok(preferred_main);
    }

    let fallback_name = env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_os_string()))
        .unwrap_or_else(|| "manhwastudio_rs.exe".into());
    let fallback = install_dir.join(fallback_name);
    if fallback.is_file() {
        return Ok(fallback);
    }

    Err(format!(
        "не найден исполняемый файл лаунчера в '{}'",
        install_dir.display()
    ))
}

#[cfg(target_os = "windows")]
fn finalize_windows_post_install(
    root_dir: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    if !is_windows_all_users_install_dir(root_dir) {
        send_progress(
            tx,
            1.0,
            "Windows интеграция: не требуется",
            overall_end,
            "All-users интеграция не требуется",
        );
        return Ok(());
    }

    let launcher_path = resolve_windows_launcher_target(root_dir)?;
    let _ = tx.send(InstallEvent::Step(
        "Windows: настройка реестра и ярлыка в меню Пуск...".to_string(),
    ));
    send_progress(
        tx,
        0.10,
        "Windows интеграция: подготовка",
        lerp_progress(overall_start, overall_end, 0.10),
        "Подготовка post-install интеграции",
    );

    register_windows_install_in_registry(root_dir, &launcher_path)?;
    send_console_line(
        tx,
        "Добавлены записи реестра: App Paths + Uninstall (HKLM)".to_string(),
    );
    send_progress(
        tx,
        0.75,
        "Windows интеграция: реестр",
        lerp_progress(overall_start, overall_end, 0.75),
        "Реестр обновлён",
    );

    let start_menu_shortcut = create_windows_start_menu_shortcut(root_dir)?;
    send_console_line(
        tx,
        format!(
            "Создан ярлык меню Пуск: '{}'",
            start_menu_shortcut.display()
        ),
    );
    send_progress(
        tx,
        1.0,
        "Windows интеграция: завершено",
        overall_end,
        "Реестр и меню Пуск настроены",
    );

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn finalize_windows_post_install(
    _root_dir: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    _overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    send_progress(
        tx,
        1.0,
        "Windows интеграция: пропуск",
        overall_end,
        "Windows интеграция не применяется",
    );
    Ok(())
}

#[cfg(target_os = "windows")]
pub(super) fn is_windows_all_users_install_dir(path: &Path) -> bool {
    is_windows_program_files_dir(path)
}

#[cfg(target_os = "windows")]
fn is_windows_program_files_dir(path: &Path) -> bool {
    const PROGRAM_FILES_ENV_VARS: &[&str] = &["ProgramFiles", "ProgramFiles(x86)"];
    let normalized_path = normalize_windows_path(path);
    PROGRAM_FILES_ENV_VARS.iter().any(|env_name| {
        env::var_os(env_name)
            .map(PathBuf::from)
            .map(|root| windows_path_is_same_or_child_of(&normalized_path, &root))
            .unwrap_or(false)
    })
}

#[cfg(target_os = "windows")]
fn windows_path_is_same_or_child_of(normalized_path: &str, candidate_root: &Path) -> bool {
    let normalized_root = normalize_windows_path(candidate_root);
    normalized_path == normalized_root
        || normalized_path
            .strip_prefix(&normalized_root)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

#[cfg(target_os = "windows")]
pub(super) fn normalize_windows_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

#[cfg(target_os = "windows")]
pub(super) fn create_windows_start_menu_shortcut(install_dir: &Path) -> Result<PathBuf, String> {
    let programs_dir = windows_start_menu_programs_dir(is_windows_all_users_install_dir(
        install_dir,
    ))
    .ok_or_else(|| "не удалось определить папку меню Пуск для создания ярлыка".to_string())?;
    let shortcut_path = programs_dir.join("ManhwaStudio.lnk");
    create_windows_shortcut_at(install_dir, &shortcut_path)?;
    Ok(shortcut_path)
}

#[cfg(target_os = "windows")]
fn windows_start_menu_programs_dir(all_users: bool) -> Option<PathBuf> {
    if all_users {
        let base = env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        return Some(
            base.join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        );
    }

    env::var_os("APPDATA").map(PathBuf::from).map(|base| {
        base.join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
    })
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
        .map_err(|e| format!("не удалось запустить icacls: {e}"))?;
    if !status.success() {
        return Err(format!(
            "icacls завершился с кодом {} при настройке прав установки",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn register_windows_install_in_registry(
    install_dir: &Path,
    launcher_path: &Path,
) -> Result<(), String> {
    let registry_root = windows_uninstall_registry_root(install_dir);
    let app_path_key = format!(
        r"{registry_root}\Software\Microsoft\Windows\CurrentVersion\App Paths\manhwastudio_rs.exe"
    );
    reg_add_string_value(&app_path_key, None, &launcher_path.to_string_lossy())?;
    reg_add_string_value(&app_path_key, Some("Path"), &install_dir.to_string_lossy())?;

    let uninstall_key = format!(
        r"{registry_root}\Software\Microsoft\Windows\CurrentVersion\Uninstall\ManhwaStudio"
    );
    reg_add_string_value(&uninstall_key, Some("DisplayName"), "ManhwaStudio")?;
    reg_add_string_value(&uninstall_key, Some("Publisher"), "Vasyanator")?;
    reg_add_string_value(
        &uninstall_key,
        Some("InstallLocation"),
        &install_dir.to_string_lossy(),
    )?;
    reg_add_string_value(
        &uninstall_key,
        Some("DisplayIcon"),
        &launcher_path.to_string_lossy(),
    )?;
    let uninstall_command = format!(
        "{} --uninstall",
        quote_windows_arg(launcher_path.to_string_lossy().as_ref())
    );
    reg_add_string_value(&uninstall_key, Some("UninstallString"), &uninstall_command)?;
    reg_add_string_value(
        &uninstall_key,
        Some("QuietUninstallString"),
        &uninstall_command,
    )?;
    reg_add_u32_value(&uninstall_key, "NoModify", 1)?;
    reg_add_u32_value(&uninstall_key, "NoRepair", 1)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_uninstall_registry_root(install_dir: &Path) -> &'static str {
    if is_windows_all_users_install_dir(install_dir) {
        "HKLM"
    } else {
        "HKCU"
    }
}

#[cfg(target_os = "windows")]
fn reg_add_string_value(
    key: &str,
    value_name: Option<&str>,
    value_data: &str,
) -> Result<(), String> {
    let mut cmd = Command::new("reg");
    apply_windows_no_window(&mut cmd);
    cmd.arg("add").arg(key);
    if let Some(name) = value_name {
        cmd.arg("/v").arg(name);
    } else {
        cmd.arg("/ve");
    }
    let status = cmd
        .arg("/t")
        .arg("REG_SZ")
        .arg("/d")
        .arg(value_data)
        .arg("/f")
        .status()
        .map_err(|e| format!("не удалось запустить reg add для '{key}': {e}"))?;
    if !status.success() {
        return Err(format!(
            "reg add завершился с кодом {} для ключа '{}'",
            status.code().unwrap_or(-1),
            key
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn reg_add_u32_value(key: &str, value_name: &str, value_data: u32) -> Result<(), String> {
    let mut cmd = Command::new("reg");
    apply_windows_no_window(&mut cmd);
    let status = cmd
        .arg("add")
        .arg(key)
        .arg("/v")
        .arg(value_name)
        .arg("/t")
        .arg("REG_DWORD")
        .arg("/d")
        .arg(value_data.to_string())
        .arg("/f")
        .status()
        .map_err(|e| format!("не удалось запустить reg add для '{key}/{value_name}': {e}"))?;
    if !status.success() {
        return Err(format!(
            "reg add завершился с кодом {} для '{}\\{}'",
            status.code().unwrap_or(-1),
            key,
            value_name
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub(super) fn reg_query_string_value(
    key: &str,
    value_name: Option<&str>,
) -> Result<Option<String>, String> {
    let mut cmd = Command::new("reg");
    apply_windows_no_window(&mut cmd);
    cmd.arg("query").arg(key);
    if let Some(name) = value_name {
        cmd.arg("/v").arg(name);
    } else {
        cmd.arg("/ve");
    }

    let output = cmd
        .output()
        .map_err(|e| format!("не удалось запустить reg query для '{key}': {e}"))?;
    if !output.status.success() {
        // Registry probing is best-effort. On localized Windows builds `reg query`
        // uses different "not found" strings, so treat an empty/non-matching result
        // as absence instead of aborting the installer-entry flow.
        if !reg_query_output_contains_value(&output.stdout) {
            return Ok(None);
        }
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(value) = extract_reg_query_value(line) {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

#[cfg(target_os = "windows")]
fn reg_query_output_contains_value(stdout: &[u8]) -> bool {
    let stdout = String::from_utf8_lossy(stdout);
    stdout
        .lines()
        .any(|line| extract_reg_query_value(line).is_some())
}

#[cfg(target_os = "windows")]
fn extract_reg_query_value(line: &str) -> Option<String> {
    const REG_MARKERS: &[&str] = &["REG_SZ", "REG_EXPAND_SZ"];
    for marker in REG_MARKERS {
        let Some(index) = line.find(marker) else {
            continue;
        };
        let value = line[(index + marker.len())..].trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(target_os = "windows")]
#[allow(dead_code)]
pub fn run_windows_uninstall_from_current_exe(
    continue_uninstall: bool,
    uninstall_signal_file: Option<&Path>,
) -> Result<(), String> {
    let current_exe =
        env::current_exe().map_err(|e| format!("не удалось определить путь exe: {e}"))?;
    let install_dir = current_exe
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "не удалось определить папку установленной программы".to_string())?;

    if uninstall_requires_elevation(&install_dir) {
        if continue_uninstall {
            return Err(
                "Удаление было перезапущено с флагом продолжения, но права администратора не получены."
                    .to_string(),
            );
        }
        let args = build_windows_uninstall_args(uninstall_signal_file, true);
        relaunch_self_elevated_with_args(&install_dir, &args)?;
        return Ok(());
    }

    let result = run_windows_uninstall_window(current_exe, install_dir);
    if let Some(signal_file) = uninstall_signal_file {
        let _ = write_uninstall_signal_file(signal_file, &result);
    }
    result
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn run_windows_uninstall_from_current_exe(
    _continue_uninstall: bool,
    _uninstall_signal_file: Option<&Path>,
) -> Result<(), String> {
    Err("Windows uninstall не поддерживается на этой ОС".to_string())
}

#[cfg(target_os = "windows")]
fn uninstall_requires_elevation(install_dir: &Path) -> bool {
    !is_running_elevated() && !has_write_access_for_install(install_dir)
}

#[cfg(target_os = "windows")]
fn remove_windows_shortcuts_for_install(install_dir: &Path) -> Result<(), String> {
    let mut targets = Vec::new();
    if let Some(dir) = windows_desktop_dir() {
        targets.push(dir.join("ManhwaStudio.lnk"));
    }
    if let Some(dir) =
        windows_start_menu_programs_dir(is_windows_all_users_install_dir(install_dir))
    {
        targets.push(dir.join("ManhwaStudio.lnk"));
    }

    for shortcut in targets {
        remove_path_if_exists(&shortcut)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn remove_windows_registry_entries_for_install(install_dir: &Path) -> Result<(), String> {
    let registry_root = windows_uninstall_registry_root(install_dir);
    let uninstall_key = format!(
        r"{registry_root}\Software\Microsoft\Windows\CurrentVersion\Uninstall\ManhwaStudio"
    );
    let app_path_key = format!(
        r"{registry_root}\Software\Microsoft\Windows\CurrentVersion\App Paths\manhwastudio_rs.exe"
    );

    reg_delete_tree_if_exists(&uninstall_key)?;
    reg_delete_tree_if_exists(&app_path_key)?;
    Ok(())
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
        "Подготовка удаления",
        format!("Папка установки: {}", install_dir.display()),
    );

    match remove_windows_shortcuts_for_install(&install_dir) {
        Ok(()) => {
            send_uninstall_progress(tx, 0.18, "Удаляем ярлыки", "Desktop и Start Menu очищены.");
        }
        Err(err) if !is_running_elevated() => {
            crate::runtime_log::log_warn(format!(
                "[windows-uninstall] shortcut cleanup skipped without elevation: {err}"
            ));
            send_uninstall_progress(
                tx,
                0.18,
                "Удаляем ярлыки",
                "Недоступные системные ярлыки пропущены без прав администратора.",
            );
        }
        Err(err) => return Err(err),
    }

    match remove_windows_registry_entries_for_install(&install_dir) {
        Ok(()) => {
            send_uninstall_progress(
                tx,
                0.30,
                "Удаляем записи системы",
                "Registry cleanup завершён.",
            );
        }
        Err(err) if !is_running_elevated() => {
            crate::runtime_log::log_warn(format!(
                "[windows-uninstall] registry cleanup skipped without elevation: {err}"
            ));
            send_uninstall_progress(
                tx,
                0.30,
                "Удаляем записи системы",
                "Недоступные системные записи реестра пропущены без прав администратора.",
            );
        }
        Err(err) => return Err(err),
    }

    remove_install_dir_contents_except_path_with_progress(&install_dir, &current_exe, tx)?;

    send_uninstall_progress(
        tx,
        0.94,
        "Запускаем финальную очистку",
        "Планируем самоудаление exe и папки после завершения процесса.",
    );
    schedule_windows_self_delete(&current_exe, &install_dir)?;
    send_uninstall_progress(
        tx,
        1.0,
        "Удаление завершено",
        "Helper-процесс принял финальную очистку.",
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn reg_delete_tree_if_exists(key: &str) -> Result<(), String> {
    let mut cmd = Command::new("reg");
    apply_windows_no_window(&mut cmd);
    let output = cmd
        .arg("delete")
        .arg(key)
        .arg("/f")
        .output()
        .map_err(|e| format!("не удалось запустить reg delete для '{key}': {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    let stdout = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if stderr.contains("unable to find")
        || stderr.contains("не удается найти")
        || stdout.contains("unable to find")
        || stdout.contains("не удается найти")
    {
        return Ok(());
    }

    Err(format!(
        "reg delete завершился с кодом {} для '{}'",
        output.status.code().unwrap_or(-1),
        key
    ))
}

#[cfg(target_os = "windows")]
fn write_uninstall_signal_file(
    signal_file: &Path,
    result: &Result<(), String>,
) -> Result<(), String> {
    if let Some(parent) = signal_file.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "не удалось создать папку для файла сигнала '{}': {e}",
                parent.display()
            )
        })?;
    }
    let payload = match result {
        Ok(()) => "ok".to_string(),
        Err(err) => format!("error: {err}"),
    };
    fs::write(signal_file, payload).map_err(|e| {
        format!(
            "не удалось записать файл сигнала удаления '{}': {e}",
            signal_file.display()
        )
    })
}

#[cfg(target_os = "windows")]
fn remove_install_dir_contents_except_path_with_progress(
    install_dir: &Path,
    keep_path: &Path,
    tx: &mpsc::Sender<UninstallEvent>,
) -> Result<(), String> {
    let entries = fs::read_dir(install_dir).map_err(|e| {
        format!(
            "не удалось прочитать папку '{}': {e}",
            install_dir.display()
        )
    })?;

    let mut targets = Vec::new();
    let mut total_nodes = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!(
                "не удалось прочитать содержимое папки '{}': {e}",
                install_dir.display()
            )
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
            "Удаляем файлы программы",
            "Дополнительных файлов для удаления не осталось.",
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
        "Удаляем файлы программы",
        format!("Удалено объектов: {removed_nodes}/{total_nodes}."),
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn count_removable_nodes(path: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("не удалось прочитать '{}': {e}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        let mut total = 1_u64;
        for entry in fs::read_dir(path)
            .map_err(|e| format!("не удалось прочитать папку '{}': {e}", path.display()))?
        {
            let child = entry
                .map_err(|e| format!("не удалось прочитать содержимое '{}': {e}", path.display()))?
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
        .map_err(|e| format!("не удалось прочитать '{}': {e}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        for entry in fs::read_dir(path)
            .map_err(|e| format!("не удалось прочитать папку '{}': {e}", path.display()))?
        {
            let child = entry
                .map_err(|e| format!("не удалось прочитать содержимое '{}': {e}", path.display()))?
                .path();
            remove_path_recursive_with_progress(&child, removed_nodes, total_nodes, tx)?;
        }
        fs::remove_dir(path)
            .map_err(|e| format!("не удалось удалить папку '{}': {e}", path.display()))?;
    } else {
        fs::remove_file(path)
            .map_err(|e| format!("не удалось удалить файл '{}': {e}", path.display()))?;
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
        "Удаляем файлы программы",
        format!("Удаляем: {file_name} ({removed_nodes}/{total_nodes})"),
    );
    Ok(())
}

#[cfg(target_os = "windows")]
pub(super) fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|e| format!("не удалось удалить папку '{}': {e}", path.display()))?;
    } else {
        fs::remove_file(path)
            .map_err(|e| format!("не удалось удалить файл '{}': {e}", path.display()))?;
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
        .map_err(|e| format!("не удалось запустить финальную очистку удаления: {e}"))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn lerp_progress(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t.clamp(0.0, 1.0)
}

#[cfg(target_os = "windows")]
fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
fn quote_windows_arg(text: &str) -> String {
    let escaped = text.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(target_os = "windows")]
fn escape_ps_single_quote(text: &str) -> String {
    text.replace('\'', "''")
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
        other => Err(format!("неподдерживаемая ОС '{other}'")),
    }
}

pub(super) fn detect_arch() -> Result<String, String> {
    match env::consts::ARCH {
        "x86_64" => Ok("x86_64".to_string()),
        "aarch64" => Ok("aarch64".to_string()),
        other => Err(format!("неподдерживаемая архитектура '{other}'")),
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
        .map_err(|e| format!("не удалось прочитать '{}': {e}", uv_dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("ошибка чтения '{}': {e}", uv_dir.display()))?;

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

    Err(format!(
        "не удалось найти uv executable в '{}'",
        uv_dir.display()
    ))
}

enum PipInstallRunner {
    Uv(PathBuf),
    PythonPip,
}

impl PipInstallRunner {
    fn label(&self) -> String {
        match self {
            Self::Uv(path) => format!("uv pip ({})", path.display()),
            Self::PythonPip => "python -m pip".to_string(),
        }
    }
}

fn resolve_runtime_pip_runner(root_dir: &Path, python_exe: &Path) -> PipInstallRunner {
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
    label: &'a str,
    dependencies: &'a [&'a str],
    overall_start: f32,
    overall_end: f32,
}

fn install_static_python_dependencies(request: DependencyInstallRequest<'_>) -> Result<(), String> {
    let DependencyInstallRequest {
        root_dir,
        pip_runner,
        python_exe,
        tx,
        label,
        dependencies,
        overall_start,
        overall_end,
    } = request;
    let total_units = (dependencies.len() + 1).max(1) as f32;

    let _ = tx.send(InstallEvent::Step(
        "Обновление pip / wheel / setuptools...".to_string(),
    ));
    send_progress(
        tx,
        0.0,
        "Обновление pip",
        overall_start,
        format!("Этап: установка {label}"),
    );
    run_pip_install_with_retry(
        pip_runner,
        python_exe,
        root_dir,
        &["--upgrade", "pip", "wheel", "setuptools"],
        "обновление pip/wheel/setuptools",
        1,
        Some(tx),
    )?;

    let mut completed_units = 1.0;
    send_progress(
        tx,
        completed_units / total_units,
        "Обновление pip завершено",
        lerp(overall_start, overall_end, completed_units / total_units),
        format!("Этап: установка {label}"),
    );

    for dep in dependencies {
        let stage_start = completed_units / total_units;
        let _ = tx.send(InstallEvent::Step(format!("Установка зависимости: {dep}")));
        send_progress(
            tx,
            stage_start,
            format!("Установка: {dep}"),
            lerp(overall_start, overall_end, stage_start),
            format!("Этап: установка {label}"),
        );

        run_pip_install_with_retry(
            pip_runner,
            python_exe,
            root_dir,
            &[*dep],
            &format!("установка зависимости '{dep}'"),
            3,
            Some(tx),
        )?;

        completed_units += 1.0;
        let ratio = completed_units / total_units;
        send_progress(
            tx,
            ratio,
            format!("Установлено: {dep}"),
            lerp(overall_start, overall_end, ratio),
            format!("Этап: установка {label}"),
        );
    }

    Ok(())
}

fn install_torch_python_dependencies(
    root_dir: &Path,
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    install_cuda_extra_packages: bool,
    overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    install_static_python_dependencies(DependencyInstallRequest {
        root_dir,
        pip_runner,
        python_exe,
        tx,
        label: "torch-зависимостей",
        dependencies: TORCH_DEPENDENCIES,
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
) -> Result<(), String> {
    let cuda_packages = ["paddlepaddle-gpu==3.3.0", "nvidia-cuda-cccl-cu12"];
    let _ = tx.send(InstallEvent::Step(
        "Установка CUDA-пакетов PaddlePaddle (без зависимостей)...".to_string(),
    ));
    send_progress(
        tx,
        0.0,
        format!("Установка: {} + {}", cuda_packages[0], cuda_packages[1]),
        overall_start,
        "Этап: CUDA-пакеты PaddlePaddle",
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
        "установка CUDA-пакетов PaddlePaddle",
        3,
        Some(tx),
    )?;
    send_progress(
        tx,
        1.0,
        "Установлены CUDA-пакеты PaddlePaddle",
        overall_end,
        "Этап CUDA-пакетов PaddlePaddle завершён",
    );
    Ok(())
}

// Kept as a disabled legacy path while the installer uses static dependency groups.
#[allow(dead_code)]
fn install_python_dependencies_from_requirements_file(
    root_dir: &Path,
    uv_exe: &Path,
    python_exe: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    install_cuda_extra_packages: bool,
    overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    let requirements = load_requirements_lines(&root_dir.join("requirements.txt"))?;
    let total_units =
        (requirements.len() + 2 + usize::from(install_cuda_extra_packages)).max(2) as f32;

    let _ = tx.send(InstallEvent::Step(
        "Обновление pip / wheel / setuptools...".to_string(),
    ));
    send_progress(
        tx,
        0.0,
        "Обновление pip",
        overall_start,
        "Этап: установка зависимостей",
    );
    run_uv_pip_install_with_retry(
        uv_exe,
        python_exe,
        root_dir,
        &["--upgrade", "pip", "wheel", "setuptools"],
        "обновление pip/wheel/setuptools",
        1,
        Some(tx),
    )?;
    let mut completed_units = 1.0;
    send_progress(
        tx,
        completed_units / total_units,
        "Обновление pip завершено",
        lerp(overall_start, overall_end, completed_units / total_units),
        "Этап: установка зависимостей",
    );

    for dep in requirements {
        let stage_start = completed_units / total_units;
        let _ = tx.send(InstallEvent::Step(format!("Установка зависимости: {dep}")));
        send_progress(
            tx,
            stage_start,
            format!("Установка: {dep}"),
            lerp(overall_start, overall_end, stage_start),
            "Этап: установка зависимостей",
        );

        run_uv_pip_install_with_retry(
            uv_exe,
            python_exe,
            root_dir,
            &[dep.as_str()],
            &format!("установка зависимости '{dep}'"),
            3,
            Some(tx),
        )?;

        completed_units += 1.0;
        let ratio = completed_units / total_units;
        send_progress(
            tx,
            ratio,
            format!("Установлено: {dep}"),
            lerp(overall_start, overall_end, ratio),
            "Этап: установка зависимостей",
        );
    }

    if install_cuda_extra_packages {
        let cuda_packages = ["paddlepaddle-gpu==3.3.0", "nvidia-cuda-cccl-cu12"];
        let stage_start = completed_units / total_units;
        let _ = tx.send(InstallEvent::Step(
            "Установка CUDA-пакетов PaddlePaddle (без зависимостей)...".to_string(),
        ));
        send_progress(
            tx,
            stage_start,
            format!("Установка: {} + {}", cuda_packages[0], cuda_packages[1]),
            lerp(overall_start, overall_end, stage_start),
            "Этап: установка зависимостей",
        );
        run_uv_pip_install_with_retry(
            uv_exe,
            python_exe,
            root_dir,
            &[
                "--no-deps",
                "--index-url",
                PADDLE_CU126_INDEX_URL,
                cuda_packages[0],
                cuda_packages[1],
            ],
            "установка CUDA-пакетов PaddlePaddle",
            3,
            Some(tx),
        )?;
        completed_units += 1.0;
        let ratio = completed_units / total_units;
        send_progress(
            tx,
            ratio,
            "Установлены CUDA-пакеты PaddlePaddle",
            lerp(overall_start, overall_end, ratio),
            "Этап: установка зависимостей",
        );
    }

    let pinned_dep = "protobuf==3.20.3";
    let stage_start = completed_units / total_units;
    let _ = tx.send(InstallEvent::Step(format!(
        "Установка фиксированной зависимости: {pinned_dep}"
    )));
    send_progress(
        tx,
        stage_start,
        format!("Установка: {pinned_dep}"),
        lerp(overall_start, overall_end, stage_start),
        "Этап: установка зависимостей",
    );
    run_uv_pip_install_with_retry(
        uv_exe,
        python_exe,
        root_dir,
        &[pinned_dep],
        &format!("установка зависимости '{pinned_dep}'"),
        3,
        Some(tx),
    )?;
    completed_units += 1.0;
    let ratio = completed_units / total_units;
    send_progress(
        tx,
        ratio,
        format!("Установлено: {pinned_dep}"),
        lerp(overall_start, overall_end, ratio),
        "Этап: установка зависимостей",
    );

    Ok(())
}

fn install_torch_stage(
    root_dir: &Path,
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    selection: &TorchInstallSelection,
    tx: &mpsc::Sender<InstallEvent>,
    overall_start: f32,
    overall_end: f32,
) -> Result<(), String> {
    match selection {
        TorchInstallSelection::SkipCpu => {
            let _ = tx.send(InstallEvent::Step("Установка CPU PyTorch...".to_string()));
            send_progress(
                tx,
                0.0,
                "PyTorch: CPU",
                overall_start,
                "Этап: установка PyTorch",
            );

            let torch_spec = format!("torch=={TORCH_VERSION}");
            let torchvision_spec = format!("torchvision=={TORCHVISION_VERSION}");
            run_pip_install_with_retry(
                pip_runner,
                python_exe,
                root_dir,
                &[
                    "--force-reinstall",
                    "--no-cache-dir",
                    &torch_spec,
                    &torchvision_spec,
                ],
                "установка CPU PyTorch",
                3,
                Some(tx),
            )?;

            send_progress(
                tx,
                1.0,
                "PyTorch: установлено (CPU)",
                overall_end,
                "Этап PyTorch завершён",
            );
            Ok(())
        }
        TorchInstallSelection::InstallGpu(option) => {
            let _ = tx.send(InstallEvent::Step(format!(
                "Установка PyTorch для {}...",
                option.label
            )));
            send_progress(
                tx,
                0.0,
                format!("PyTorch: {}", option.label),
                overall_start,
                "Этап: установка PyTorch",
            );

            let torch_spec = format!("torch=={TORCH_VERSION}");
            let torchvision_spec = format!("torchvision=={TORCHVISION_VERSION}");
            let index_url = format!("https://download.pytorch.org/whl/{}", option.wheel_tag);
            let args = [
                "--force-reinstall".to_string(),
                "--no-cache-dir".to_string(),
                torch_spec,
                torchvision_spec,
                "--index-url".to_string(),
                index_url,
            ];
            let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();

            run_pip_install_with_retry(
                pip_runner,
                python_exe,
                root_dir,
                &args_ref,
                &format!("установка PyTorch ({})", option.label),
                3,
                Some(tx),
            )?;

            send_progress(
                tx,
                1.0,
                format!("PyTorch: установлено ({})", option.label),
                overall_end,
                "Этап PyTorch завершён",
            );
            Ok(())
        }
    }
}

pub(crate) fn detect_torch_preflight() -> TorchPreflightResult {
    if cfg!(target_os = "macos") {
        return TorchPreflightResult::Skip {
            reason: "macOS обнаружен, этап установки GPU-колёс PyTorch пропущен".to_string(),
        };
    }

    let has_nvidia = detect_nvidia_gpu();
    let has_amd_linux = cfg!(target_os = "linux") && detect_amd_gpu_linux();
    if !has_nvidia && !has_amd_linux {
        return TorchPreflightResult::Skip {
            reason: "GPU NVIDIA/AMD не обнаружен, оставляем CPU-версию PyTorch".to_string(),
        };
    }

    let mut options = Vec::new();
    let mut detected = Vec::new();
    let mut failures = Vec::new();

    if has_nvidia {
        let cuda_capability = detect_nvidia_compute_capability();
        if let Some(capability) = cuda_capability {
            detected.push(format!("NVIDIA SM {capability}"));
        } else {
            failures.push(
                "NVIDIA найден, но Compute Capability (SM) не определилась; ограничения SM не применены"
                    .to_string(),
            );
        }
        if let Some(cuda_version) = detect_cuda_runtime_version() {
            detected.push(format!("CUDA {cuda_version}"));
            let cuda_options = build_cuda_torch_options(cuda_version, cuda_capability);
            if cuda_options.is_empty()
                && let Some(capability) = cuda_capability
            {
                if capability < RuntimeVersion::new(6, 1) {
                    failures.push(format!(
                        "NVIDIA SM {capability} < 6.1, GPU-установка PyTorch отключена (только CPU)"
                    ));
                } else if capability < RuntimeVersion::new(7, 5) {
                    failures.push(format!(
                            "NVIDIA SM {capability} < 7.5: доступна только CUDA 12.6, но runtime CUDA ниже 12.6"
                        ));
                }
            }
            options.extend(cuda_options);
        } else {
            failures.push("NVIDIA найден, но версия CUDA не определилась".to_string());
        }
    }

    if has_amd_linux {
        if let Some(rocm_version) = detect_rocm_runtime_version() {
            detected.push(format!("ROCm {rocm_version}"));
            options.extend(build_rocm_torch_options(rocm_version));
        } else {
            failures.push("AMD найден, но версия ROCm не определилась".to_string());
        }
    }

    if options.is_empty() {
        let reason = if !failures.is_empty() {
            format!(
                "{}. Пропускаем этап PyTorch и оставляем CPU.",
                failures.join("; ")
            )
        } else {
            "Версия CUDA/ROCm ниже минимально поддерживаемых wheel, оставляем CPU.".to_string()
        };
        return TorchPreflightResult::Skip { reason };
    }

    let recommended_index = choose_recommended_torch_option(&options);
    let summary = if detected.is_empty() {
        "Доступны GPU-варианты PyTorch.".to_string()
    } else {
        format!("Обнаружено: {}.", detected.join(", "))
    };

    TorchPreflightResult::Choose(TorchChoicePrompt {
        options,
        recommended_index,
        summary,
    })
}

fn build_cuda_torch_options(
    cuda_version: RuntimeVersion,
    cuda_capability: Option<RuntimeVersion>,
) -> Vec<TorchWheelOption> {
    let variants = [
        (RuntimeVersion::new(12, 6), "CUDA 12.6", "cu126"),
        (RuntimeVersion::new(12, 8), "CUDA 12.8", "cu128"),
        (RuntimeVersion::new(13, 0), "CUDA 13.0", "cu130"),
    ];

    let mut options = variants
        .into_iter()
        .filter(|(required, _, _)| *required <= cuda_version)
        .map(|(required, label, wheel_tag)| TorchWheelOption {
            backend: TorchBackend::Cuda,
            wheel_tag: wheel_tag.to_string(),
            label: format!("{label} ({wheel_tag})"),
            version: required,
        })
        .collect::<Vec<_>>();

    if let Some(capability) = cuda_capability {
        if capability < RuntimeVersion::new(6, 1) {
            return Vec::new();
        }
        if capability < RuntimeVersion::new(7, 5) {
            options.retain(|option| option.version == RuntimeVersion::new(12, 6));
        }
    }

    options
}

fn build_rocm_torch_options(rocm_version: RuntimeVersion) -> Vec<TorchWheelOption> {
    let required = RuntimeVersion::new(6, 4);
    if rocm_version < required {
        return Vec::new();
    }

    vec![TorchWheelOption {
        backend: TorchBackend::Rocm,
        wheel_tag: "rocm6.4".to_string(),
        label: "ROCm 6.4 (rocm6.4)".to_string(),
        version: required,
    }]
}

fn choose_recommended_torch_option(options: &[TorchWheelOption]) -> usize {
    options
        .iter()
        .enumerate()
        .max_by_key(|(_, option)| {
            (
                option.version,
                match option.backend {
                    TorchBackend::Cuda => 2_u8,
                    TorchBackend::Rocm => 1_u8,
                },
            )
        })
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn load_requirements_lines(path: &Path) -> Result<Vec<String>, String> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("не удалось прочитать '{}': {e}", path.display()))?;

    let lines = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    Ok(lines)
}

fn run_uv_pip_install_with_retry(
    uv_exe: &Path,
    python_exe: &Path,
    cwd: &Path,
    pip_args: &[&str],
    action_name: &str,
    attempts: usize,
    tx: Option<&mpsc::Sender<InstallEvent>>,
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
    run_command_with_retry(uv_exe, cwd, &args_ref, action_name, attempts, tx, &[])
}

fn run_pip_install_with_retry(
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    cwd: &Path,
    pip_args: &[&str],
    action_name: &str,
    attempts: usize,
    tx: Option<&mpsc::Sender<InstallEvent>>,
) -> Result<(), String> {
    match pip_runner {
        PipInstallRunner::Uv(uv_exe) => run_uv_pip_install_with_retry(
            uv_exe,
            python_exe,
            cwd,
            pip_args,
            action_name,
            attempts,
            tx,
        ),
        PipInstallRunner::PythonPip => {
            let mut args = vec!["-m".to_string(), "pip".to_string(), "install".to_string()];
            args.extend(pip_args.iter().map(|arg| (*arg).to_string()));
            let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
            run_command_with_retry(python_exe, cwd, &args_ref, action_name, attempts, tx, &[])
        }
    }
}

fn run_command_with_retry(
    executable: &Path,
    cwd: &Path,
    args: &[&str],
    action_name: &str,
    attempts: usize,
    tx: Option<&mpsc::Sender<InstallEvent>>,
    extra_env: &[(&str, &str)],
) -> Result<(), String> {
    let mut last_output = String::new();
    for attempt in 1..=attempts.max(1) {
        if let Some(tx) = tx {
            send_console_line(tx, format!("$ {} {}", executable.display(), args.join(" ")));
            if attempts > 1 {
                send_console_line(tx, format!("Попытка {attempt}/{attempts}: {action_name}"));
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
                    format!("Повтор установки из-за ошибки ({action_name})..."),
                );
            }
            std::thread::sleep(Duration::from_millis(600));
            continue;
        }
    }

    Err(format!(
        "Не удалось выполнить {action_name} после {attempts} попыток.\nСообщение pip:\n{last_output}"
    ))
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
        .map_err(|e| format!("не удалось запустить '{}': {e}", executable.display()))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "не удалось получить stdout процесса".to_string())?;
    let tx_out = tx.cloned();
    let out_handle = ms_thread::spawn(move || stream_reader_lines(stdout, tx_out));
    let err_handle = child.stderr.take().map(|stderr| {
        let tx_err = tx.cloned();
        ms_thread::spawn(move || stream_reader_lines(stderr, tx_err))
    });

    let status = child
        .wait()
        .map_err(|e| format!("ошибка ожидания завершения Python-процесса: {e}"))?;
    let out_text = out_handle
        .join()
        .map_err(|_| "ошибка join stdout reader".to_string())?;
    let err_text = match err_handle {
        Some(handle) => handle
            .join()
            .map_err(|_| "ошибка join stderr reader".to_string())?,
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

pub(super) fn open_url_in_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("cmd");
        apply_windows_no_window(&mut cmd);
        cmd.arg("/C").arg("start").arg("").arg(url);
        cmd.spawn()
            .map_err(|e| format!("не удалось открыть URL '{url}': {e}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("не удалось открыть URL '{url}': {e}"))?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("не удалось открыть URL '{url}': {e}"))?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(format!("неподдерживаемая ОС для открытия URL '{}'", url))
}

fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t.clamp(0.0, 1.0)
}

fn fetch_latest_uv_asset(platform: Platform, arch: &str) -> Result<GithubAsset, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(45))
        .build();

    let mut req = agent
        .get(UV_RELEASE_API)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "ManhwaStudioMiniLauncher/installer");
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
    }

    let response = req
        .call()
        .map_err(|e| format!("не удалось получить релиз uv: {e}"))?;
    let body = response
        .into_string()
        .map_err(|e| format!("не удалось прочитать релиз uv: {e}"))?;
    let release: GithubRelease = serde_json::from_str(&body)
        .map_err(|e| format!("не удалось распарсить JSON релиза uv: {e}"))?;

    select_uv_asset(&release.assets, platform, arch)
}

fn fetch_latest_app_zip_asset() -> Result<GithubAsset, String> {
    fetch_latest_app_asset_by_name(APP_ZIP_ASSET_NAME)
}

fn fetch_latest_app_asset_by_name(asset_name: &str) -> Result<GithubAsset, String> {
    fetch_latest_app_release_with_asset(asset_name).map(|(_, asset)| asset)
}

fn fetch_latest_app_release_tag_with_required_asset(asset_name: &str) -> Result<String, String> {
    fetch_latest_app_release_with_asset(asset_name).map(|(tag, _)| tag)
}

fn fetch_latest_app_release_with_asset(asset_name: &str) -> Result<(String, GithubAsset), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(45))
        .build();

    let mut req = agent
        .get(APP_RELEASES_API)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "ManhwaStudioMiniLauncher/installer");
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
    }

    let response = req
        .call()
        .map_err(|e| format!("не удалось получить список релизов ManhwaStudio: {e}"))?;
    let body = response
        .into_string()
        .map_err(|e| format!("не удалось прочитать список релизов ManhwaStudio: {e}"))?;
    let releases: Vec<GithubReleaseListItem> = serde_json::from_str(&body)
        .map_err(|e| format!("не удалось распарсить JSON релизов ManhwaStudio: {e}"))?;

    for release in releases {
        let tag = release
            .tag_name
            .or(release.name)
            .unwrap_or_default()
            .trim()
            .to_string();
        if tag.is_empty() {
            continue;
        }
        if let Some(asset) = release
            .assets
            .into_iter()
            .find(|asset| asset.name == asset_name)
        {
            return Ok((tag, asset));
        }
    }

    Err(format!(
        "не найден asset '{}' в релизах ManhwaStudio",
        asset_name
    ))
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
            format!(
                "не найдена подходящая сборка uv для {platform}/{arch}; ожидается asset '{expected_name}'"
            )
        })
}

fn download_asset(
    url: &str,
    dst_path: &Path,
    tx: &mpsc::Sender<InstallEvent>,
    progress_start: f32,
    progress_end: f32,
    label_prefix: &str,
) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(120))
        .build();
    let mut req = agent
        .get(url)
        .set("User-Agent", "ManhwaStudioMiniLauncher/installer");
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
    }

    let response = req
        .call()
        .map_err(|e| format!("не удалось скачать {label_prefix}: {e}"))?;
    let total = response
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let mut reader = response.into_reader();
    let mut file = File::create(dst_path)
        .map_err(|e| format!("не удалось создать '{}': {e}", dst_path.display()))?;

    let mut downloaded: u64 = 0;
    let mut buf = vec![0_u8; 256 * 1024];
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|e| format!("ошибка чтения HTTP-потока: {e}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buf[..read])
            .map_err(|e| format!("ошибка записи '{}': {e}", dst_path.display()))?;
        downloaded += read as u64;

        if last_emit.elapsed() >= Duration::from_millis(120) {
            let stage_progress = if total > 0 {
                downloaded as f32 / total as f32
            } else {
                0.0
            };
            send_progress(
                tx,
                stage_progress.clamp(0.0, 1.0),
                if total > 0 {
                    format!(
                        "{label_prefix}: {} / {}",
                        format_bytes(downloaded),
                        format_bytes(total)
                    )
                } else {
                    format!("{label_prefix}: {}", format_bytes(downloaded))
                },
                progress_start + stage_progress.clamp(0.0, 1.0) * (progress_end - progress_start),
                format!("Скачивание {label_prefix}"),
            );
            last_emit = Instant::now();
        }
    }
    file.flush()
        .map_err(|e| format!("ошибка финализации файла '{}': {e}", dst_path.display()))?;

    send_progress(
        tx,
        1.0,
        format!("Скачивание {label_prefix} завершено"),
        progress_end,
        format!("Скачивание {label_prefix} завершено"),
    );
    Ok(())
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
            .map_err(|e| format!("не удалось открыть '{}': {e}", archive_path.display()))?;
        let decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|e| format!("не удалось открыть zstd-декодер: {e}"))?;
        send_progress(
            tx,
            0.0,
            format!("{label_prefix}: подготовка"),
            overall_start,
            label_prefix,
        );
        extract_tar(decoder, target_dir)?;
        send_progress(
            tx,
            1.0,
            format!("{label_prefix}: завершено"),
            overall_end,
            format!("{label_prefix}: завершено"),
        );
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let file = File::open(archive_path)
            .map_err(|e| format!("не удалось открыть '{}': {e}", archive_path.display()))?;
        let decoder = GzDecoder::new(file);
        send_progress(
            tx,
            0.0,
            format!("{label_prefix}: подготовка"),
            overall_start,
            label_prefix,
        );
        extract_tar(decoder, target_dir)?;
        send_progress(
            tx,
            1.0,
            format!("{label_prefix}: завершено"),
            overall_end,
            format!("{label_prefix}: завершено"),
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
        return Err(format!(
            "неподдерживаемый формат архива: {}",
            archive_path.display()
        ));
    }

    Ok(())
}

fn extract_tar<R: Read>(reader: R, target_dir: &Path) -> Result<(), String> {
    let mut archive = Archive::new(reader);
    let entries = archive
        .entries()
        .map_err(|e| format!("ошибка чтения tar-entries: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("ошибка tar-entry: {e}"))?;
        entry
            .unpack_in(target_dir)
            .map_err(|e| format!("ошибка распаковки tar-entry: {e}"))?;
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
        .map_err(|e| format!("не удалось открыть '{}': {e}", archive_path.display()))?;
    let mut zip =
        ZipArchive::new(file).map_err(|e| format!("не удалось открыть ZIP-архив: {e}"))?;

    let total = zip.len().max(1);
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("ошибка чтения ZIP entry {i}: {e}"))?;
        let rel = match entry.enclosed_name() {
            Some(path) => path.to_path_buf(),
            None => continue,
        };
        let out_path = target_dir.join(archive_entry_relative_output_path(&rel));

        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|e| format!("не удалось создать каталог '{}': {e}", out_path.display()))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("не удалось создать каталог '{}': {e}", parent.display())
                })?;
            }
            let mut out = File::create(&out_path)
                .map_err(|e| format!("не удалось создать '{}': {e}", out_path.display()))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| format!("ошибка распаковки '{}': {e}", out_path.display()))?;
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
        format!("{label_prefix}: завершено"),
        overall_end,
        format!("{label_prefix}: завершено"),
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
        .map_err(|e| format!("не удалось прочитать '{}': {e}", target_dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("ошибка чтения содержимого '{}': {e}", target_dir.display()))?;

    if entries.len() != 1 {
        return Ok(());
    }

    let only = &entries[0];
    let file_type = only
        .file_type()
        .map_err(|e| format!("ошибка stat '{}': {e}", only.path().display()))?;
    if !file_type.is_dir() {
        return Ok(());
    }

    let nested_root = only.path();
    let nested_entries: Vec<_> = fs::read_dir(&nested_root)
        .map_err(|e| format!("не удалось прочитать '{}': {e}", nested_root.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("ошибка чтения содержимого '{}': {e}", nested_root.display()))?;

    for entry in nested_entries {
        let from = entry.path();
        let to = target_dir.join(entry.file_name());
        if to.exists() {
            if to.is_dir() {
                fs::remove_dir_all(&to)
                    .map_err(|e| format!("не удалось удалить '{}': {e}", to.display()))?;
            } else {
                fs::remove_file(&to)
                    .map_err(|e| format!("не удалось удалить '{}': {e}", to.display()))?;
            }
        }
        fs::rename(&from, &to).map_err(|e| {
            format!(
                "не удалось переместить '{}' -> '{}': {e}",
                from.display(),
                to.display()
            )
        })?;
    }

    fs::remove_dir_all(&nested_root)
        .map_err(|e| format!("не удалось удалить '{}': {e}", nested_root.display()))?;
    Ok(())
}

fn merge_dir_contents(src_dir: &Path, dst_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dst_dir)
        .map_err(|e| format!("не удалось создать '{}': {e}", dst_dir.display()))?;

    let entries: Vec<_> = fs::read_dir(src_dir)
        .map_err(|e| format!("не удалось прочитать '{}': {e}", src_dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("ошибка чтения '{}': {e}", src_dir.display()))?;

    for entry in entries {
        let src_path = entry.path();
        let dst_path = dst_dir.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| format!("ошибка stat '{}': {e}", src_path.display()))?;

        if file_type.is_dir() {
            merge_dir_contents(&src_path, &dst_path)?;
            fs::remove_dir_all(&src_path)
                .map_err(|e| format!("не удалось удалить '{}': {e}", src_path.display()))?;
            continue;
        }

        if dst_path.exists() {
            if dst_path.is_dir() {
                fs::remove_dir_all(&dst_path)
                    .map_err(|e| format!("не удалось удалить '{}': {e}", dst_path.display()))?;
            } else {
                fs::remove_file(&dst_path)
                    .map_err(|e| format!("не удалось удалить '{}': {e}", dst_path.display()))?;
            }
        } else if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("не удалось создать '{}': {e}", parent.display()))?;
        }

        fs::rename(&src_path, &dst_path).map_err(|e| {
            format!(
                "не удалось переместить '{}' -> '{}': {e}",
                src_path.display(),
                dst_path.display()
            )
        })?;
    }

    Ok(())
}

fn format_bytes(bytes: u64) -> String {
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
        compare_version_strings, dependency_package_name, normalize_package_name,
        parse_executable_version_output, parse_pip_freeze_packages,
        platform_binary_asset_name, platform_executable_file_name,
        sanitize_windows_archive_path_component,
    };
    use std::cmp::Ordering;

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
        assert_eq!(
            dependency_package_name("onnxruntime; platform_system != \"Windows\""),
            Some("onnxruntime")
        );
        assert_eq!(dependency_package_name("torch==2.9.1"), Some("torch"));
        assert_eq!(normalize_package_name("deep_translator"), "deep-translator");
    }

    #[test]
    fn compare_version_strings_handles_prefixed_numeric_versions() {
        assert_eq!(compare_version_strings("2.9.1", "2.9.0"), Ordering::Greater);
        assert_eq!(compare_version_strings("v2.9.1", "2.9.1"), Ordering::Equal);
        assert_eq!(compare_version_strings("2.8.0", "2.9.1"), Ordering::Less);
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
    fn on_disk_executable_name_never_carries_macos_asset_suffix() {
        // The on-disk / in-archive executable name must not carry the `_macos`
        // suffix that only the release download name uses, otherwise the update
        // strip step would miss the staged binary on macOS. On non-Windows the
        // on-disk name is the bare `manhwastudio_rs`.
        let on_disk = platform_executable_file_name();
        assert!(
            !on_disk.contains("_macos"),
            "on-disk executable name must not use the release-asset `_macos` suffix, got {on_disk}"
        );

        let asset = platform_binary_asset_name();
        if cfg!(target_os = "windows") {
            assert_eq!(on_disk, "manhwastudio_rs.exe");
            assert_eq!(asset, "manhwastudio_rs.exe");
        } else {
            assert_eq!(on_disk, "manhwastudio_rs");
        }
        if cfg!(target_os = "macos") {
            // The two names intentionally differ on macOS: bare on disk, suffixed
            // as the release download.
            assert_eq!(asset, "manhwastudio_rs_macos");
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
}
