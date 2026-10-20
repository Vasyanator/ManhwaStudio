/*
File: torch/stage.rs

Purpose:
Runs the Torch install stage inside the installer, repair, update and launcher-upgrade workers.

Key functions:
- `install_torch_stage()`: freeze -> uninstall the AMD packages that do not match the
  selection -> install the selected wheels (with retries) -> freeze -> record the choice.
  Returns the post-install snapshot so the caller can write the constraints file.
- `run_torch_upgrade_worker()`: the launcher settings page's worker (replace the installed
  PyTorch, optionally followed by the torch-dependent extras under the constraints file).

Notes:
Worker-thread only (spawns uv / pip). The pip arguments and the uninstall set come from
`command.rs`; the runner, the retry loop, progress helpers and the extras list stay in
`utils.rs`. The record is written only after a successful install; a failed write is a
warning (readiness then offers a repair), never a failed stage.
*/

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use ms_sysprobe::python_manager;

use super::TorchInstallSelection;
use super::choice_store::persist_torch_choice_record;
use super::command::{amd_packages_to_remove, torch_install_pip_args};
use super::constraints::write_torch_constraints;
use super::readiness::{required_torch_version, torch_readiness};
use crate::install::InstallEvent;
use crate::utils::{
    ENABLE_PADDLE_CUDA_EXTRA_PACKAGES_INSTALL, PipInstallRunner, PostTorchInstallTarget, RetryRun, freeze_installed_packages,
    install_torch_python_dependencies, resolve_runtime_pip_runner, run_pip_install_with_retry,
    run_pip_uninstall_with_retry, send_console_line, send_progress,
};

/// Replaces the PyTorch of the environment in `root_dir` with `torch_selection`, then, when
/// `install_full_dependencies`, installs the torch-dependent extras constrained to the
/// installed torch.
///
/// Used by the launcher settings page; it offers no cancel, so retries run to their budget.
///
/// # Errors
/// The localized message of a Python lookup failure or of the failed step.
pub fn run_torch_upgrade_worker(
    root_dir: PathBuf,
    torch_selection: TorchInstallSelection,
    install_full_dependencies: bool,
    tx: &mpsc::Sender<InstallEvent>,
) -> Result<(), String> {
    // The launcher's settings page offers no cancel for this worker (it runs to completion
    // while the page shows its progress), so its command retries see a flag that never sets.
    let never_cancelled = AtomicBool::new(false);
    let cancel = &never_cancelled;
    let _ = tx.send(InstallEvent::Step(
        t!("installer.utils.preparing_pytorch_env_status").to_string(),
    ));
    let python_exe = python_manager::resolve_python_executable(&root_dir)
        .map_err(|err| tf!("installer.utils.find_python_for_pytorch_error", err = err))?;
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
    let installed = install_torch_stage(
        &root_dir,
        &pip_runner,
        &python_exe,
        &torch_selection,
        tx,
        (torch_range.0, torch_range.1),
        cancel,
    )?;

    if install_full_dependencies {
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
            (0.42, 1.0),
        )?;
    }

    send_progress(tx, 1.0, t!("installer.utils.pytorch_install_complete"), 1.0, t!("installer.common.ready"));
    Ok(())
}

/// Installs the PyTorch wheels of `selection` into `python_exe`'s environment through
/// `pip_runner`, mapping the stage onto `overall_range` of the overall bar, and records the
/// choice in `root_dir`'s `user_config`.
///
/// Steps: freeze; uninstall `command::amd_packages_to_remove` for the selection's record
/// (skipped when empty); install (three attempts); freeze again; persist the record. Returns
/// the post-install `pip freeze` snapshot.
///
/// # Errors
/// The localized message of an invalid selection, a failed freeze, uninstall or install, or
/// the cancel text. The record is NOT written on any error. A failed record write after a
/// successful install is only a warning (logged, console line), not an error.
pub(crate) fn install_torch_stage(
    root_dir: &Path,
    pip_runner: &PipInstallRunner,
    python_exe: &Path,
    selection: &TorchInstallSelection,
    tx: &mpsc::Sender<InstallEvent>,
    overall_range: (f32, f32),
    cancel: &AtomicBool,
) -> Result<HashMap<String, String>, String> {
    let (overall_start, overall_end) = overall_range;
    // Validate before touching the environment: an invalid selection uninstalls nothing.
    let args = torch_install_pip_args(selection).map_err(|err| {
        ms_log::runtime_log::log_error(format!("[torch] invalid PyTorch selection {selection:?}: {err:?}"));
        err.user_message()
    })?;
    let record = selection.record();
    let (step_text, stage_label, action_name, done_text) = match selection {
        TorchInstallSelection::SkipCpu => (
            t!("installer.utils.installing_cpu_pytorch_status").to_string(),
            "PyTorch: CPU".to_string(),
            t!("installer.utils.install_cpu_pytorch_action").to_string(),
            t!("installer.utils.pytorch_installed_cpu_status").to_string(),
        ),
        TorchInstallSelection::InstallGpu(option) | TorchInstallSelection::InstallRocm { option, .. } => (
            tf!("installer.utils.installing_pytorch_status", option = option.label),
            format!("PyTorch: {}", option.label),
            tf!("installer.utils.install_pytorch_action", option = option.label),
            tf!("installer.utils.pytorch_installed_status", option = option.label),
        ),
    };

    let _ = tx.send(InstallEvent::Step(step_text));
    send_progress(tx, 0.0, stage_label, overall_start, t!("installer.utils.stage_install_pytorch"));

    let before = freeze_installed_packages(pip_runner, python_exe, root_dir, None)?;
    let to_remove = amd_packages_to_remove(&before, &record);
    if !to_remove.is_empty() {
        send_console_line(tx, format!("[PyTorch] removing AMD packages that do not match {record:?}: {}", to_remove.join(", ")));
        let _ = tx.send(InstallEvent::Step(t!("installer.torch.removing_amd_packages_status").to_string()));
    }
    run_pip_uninstall_with_retry(
        pip_runner,
        python_exe,
        root_dir,
        &to_remove,
        RetryRun { action_name: t!("installer.torch.remove_amd_packages_action"), attempts: 3, tx: Some(tx), cancel },
    )?;

    let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_pip_install_with_retry(
        pip_runner,
        python_exe,
        root_dir,
        &args_ref,
        RetryRun { action_name: &action_name, attempts: 3, tx: Some(tx), cancel },
    )?;

    let installed = freeze_installed_packages(pip_runner, python_exe, root_dir, None)?;
    // The install command succeeded; a result that still does not satisfy the record (e.g. a
    // ROCm torch without its device packages) is surfaced, and readiness will offer a repair.
    if let Some(reason) = torch_readiness(&installed, &record).user_message(required_torch_version(&record)) {
        ms_log::runtime_log::log_warn(format!("[torch] installed PyTorch does not satisfy {record:?}: {reason}"));
        send_console_line(tx, tf!("installer.torch.post_install_mismatch_warning", reason = reason));
    }
    match persist_torch_choice_record(root_dir, &record) {
        Ok(()) => send_console_line(tx, format!("[PyTorch] recorded choice {record:?}")),
        Err(err) => {
            // Already logged by the store. The install itself succeeded, so the stage does
            // not fail: the old (or no) record makes readiness offer a repair, the safe side.
            send_console_line(tx, tf!("installer.torch.record_save_warning", err = err));
        }
    }

    send_progress(tx, 1.0, done_text, overall_end, t!("installer.utils.pytorch_stage_done"));
    Ok(installed)
}
