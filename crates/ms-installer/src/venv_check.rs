/*
File: crates/ms-installer/src/venv_check.rs

Purpose:
Non-interactive readiness check of the managed Python environment behind the
`--check-venv` startup flag.

Main responsibilities:
- decide whether the environment of a runtime root is complete, without opening a window;
- describe WHY it is not complete in a user-facing message and in the runtime log;
- keep the decision free of GUI code: the caller (`lib.rs`) owns the installer window and
  the process exit code.

Key structures:
- EnvironmentReadiness: ready / not ready with a typed reason.
- NotReadyReason: missing install type, missing interpreter, failed probe, missing packages,
  a PyTorch that does not satisfy the recorded choice (absent, outdated, wrong flavour, missing
  ROCm device packages).

Key functions:
- check_environment(): runs the whole check for a runtime root.

Notes:
A failed probe (for example `pip freeze` not starting) is reported as NOT ready. This check
must never claim readiness it could not verify.
The package requirements come from this crate's `utils`, and the two decisions that could drift
from the repair worker are shared with it as single predicates: `torch::torch_readiness` /
`installed_torch_is_current` (PyTorch against the recorded choice, `torch/readiness.rs`) and `utils::missing_specs_for_readiness` (which softens the strict install
rule only for interchangeable distributions of the same module).
*/

use std::path::Path;

use crate::torch::{TorchReadiness, required_torch_version, torch_readiness};
use crate::utils;
use ms_config as config;
use ms_config::torch_choice::TorchChoiceRecord;
use ms_log::runtime_log;
use ms_sysprobe::python_manager;

/// Outcome of a managed-environment readiness check.
#[derive(Debug)]
pub enum EnvironmentReadiness {
    /// Every package required by the recorded install type is present.
    Ready,
    /// The environment cannot be used as-is; carries the reason for the user and the log.
    NotReady(NotReadyReason),
}

/// Why an environment is not ready.
///
/// Every variant maps to a user-facing sentence via [`NotReadyReason::user_message`];
/// the enum itself is what the caller logs and (later) may branch on.
#[derive(Debug)]
pub enum NotReadyReason {
    /// `General.ai_install_type` is absent or `None`: the user still has to choose
    /// between the fast and the full dependency set, which needs the installer UI.
    InstallTypeUnknown,
    /// No Python interpreter could be discovered under the runtime root.
    PythonEnvMissing(String),
    /// The installed-package probe itself failed; readiness is unknown, so not ready.
    ProbeFailed(String),
    /// The interpreter works but required packages are absent or installed at a version
    /// outside their pin (the dependency specs to install).
    MissingPackages(Vec<String>),
    /// A `Full` environment whose PyTorch does not satisfy the recorded torch choice
    /// (`General.torch_flavour` / `torch_rocm_gfx`; legacy version-only rule when
    /// unrecorded). `readiness` is never `Current`; `required` is the record's version.
    TorchNotReady { readiness: TorchReadiness, required: &'static str },
}

impl NotReadyReason {
    /// Returns a localized, user-facing explanation of this reason.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::InstallTypeUnknown => {
                t!("venv_check.reason_install_type_unknown").to_string()
            }
            Self::PythonEnvMissing(err) => {
                tf!("venv_check.reason_python_env_missing", err = err)
            }
            Self::ProbeFailed(err) => tf!("venv_check.reason_probe_failed", err = err),
            Self::MissingPackages(packages) => tf!(
                "venv_check.reason_missing_packages",
                packages = packages.join(", ")
            ),
            // `Current` never reaches this variant; it would read as "missing".
            Self::TorchNotReady { readiness, required } => readiness
                .user_message(required)
                .unwrap_or_else(|| tf!("venv_check.reason_torch_missing", required = *required)),
        }
    }
}

/// Reads `root_dir/user_config.json` through the document store without creating or
/// rewriting it.
///
/// An absent file yields an empty object (the same shape a fresh install has), so
/// the caller sees "no recorded install type" instead of an error.
///
/// # Errors
/// Returns a diagnostic message when the file exists but cannot be read or parsed.
fn read_user_settings(root_dir: &Path) -> Result<serde_json::Value, String> {
    let doc = ms_docstore::DocRef::new(root_dir.join(config::USER_CONFIG_FILE), ms_docstore::DocKind::UserConfig);
    match ms_docstore::read_value(&doc) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Ok(serde_json::Value::Object(serde_json::Map::new())),
        // `DocStoreError`'s Display already names the path and the parse/OS cause.
        Err(err) => Err(format!("failed to read user settings: {err}")),
    }
}

/// Checks whether the managed Python environment of `root_dir` is complete.
///
/// The check reads `General.ai_install_type` from the root's `user_config.json`
/// (never writing it), resolves the interpreter through `python_manager`, and
/// compares the dependency set required for that install type against `pip freeze`.
/// It performs no installation and opens no window.
///
/// Returns [`EnvironmentReadiness::Ready`] only when every required package was
/// observed; any failure to *verify* readiness (unreadable config, unresolvable
/// interpreter, failed probe) is reported as not ready with the reason attached.
#[must_use]
pub fn check_environment(root_dir: &Path) -> EnvironmentReadiness {
    let (install_type, torch_record) = match read_user_settings(root_dir) {
        Ok(settings) => (
            config::AiInstallType::from_user_settings(&settings),
            TorchChoiceRecord::from_user_settings(&settings),
        ),
        Err(err) => {
            // An unreadable user config cannot prove readiness; treat it like an
            // unknown install type so the user gets the installer choice screen.
            runtime_log::log_warn(format!(
                "[check-venv] failed to read user settings of '{}': {err}",
                root_dir.display()
            ));
            (config::AiInstallType::None, TorchChoiceRecord::Unrecorded)
        }
    };
    match install_type {
        config::AiInstallType::None => {
            return EnvironmentReadiness::NotReady(NotReadyReason::InstallTypeUnknown);
        }
        config::AiInstallType::Base | config::AiInstallType::Full => {}
    }

    let python_exe = match python_manager::resolve_python_executable(root_dir) {
        Ok(python_exe) => python_exe,
        Err(err) => {
            return EnvironmentReadiness::NotReady(NotReadyReason::PythonEnvMissing(err));
        }
    };
    let pip_runner = utils::resolve_runtime_pip_runner(root_dir, &python_exe);
    runtime_log::log_info(format!(
        "[check-venv] install type {}, python '{}'",
        install_type.as_str(),
        python_exe.display()
    ));

    // No worker channel here: this flow is console-only, so the probe stays silent.
    let installed = match utils::freeze_installed_packages(&pip_runner, &python_exe, root_dir, None)
    {
        Ok(installed) => installed,
        Err(err) => {
            return EnvironmentReadiness::NotReady(NotReadyReason::ProbeFailed(err));
        }
    };

    let required = utils::required_dependency_specs(install_type);
    // Readiness view of "missing": a spec is also satisfied by an interchangeable
    // distribution of the same module (e.g. any onnxruntime build). Installation keeps
    // the strict rule; only the question "must we bother the user?" is softened.
    let missing = utils::missing_specs_for_readiness(&required, &installed);
    if !missing.is_empty() {
        return EnvironmentReadiness::NotReady(NotReadyReason::MissingPackages(
            missing.into_iter().map(str::to_string).collect(),
        ));
    }

    // PyTorch carries a VERSION and FLAVOUR requirement (the recorded choice), not just a
    // presence requirement, and the decision is shared with the repair worker's "skip the
    // Torch stage?" (`installed_torch_is_current` is `torch_readiness == Current`).
    if install_type == config::AiInstallType::Full {
        let readiness = torch_readiness(&installed, &torch_record);
        if readiness != TorchReadiness::Current {
            return EnvironmentReadiness::NotReady(NotReadyReason::TorchNotReady {
                readiness,
                required: required_torch_version(&torch_record),
            });
        }
    }

    EnvironmentReadiness::Ready
}
