/*
File: src/ai_install_probe.rs

Purpose:
Detects installed AI Python package capabilities through the same activated shell path used by
launcher settings.

Main responsibilities:
- activate the app-local Python environment when available;
- probe PyTorch and ONNX Runtime package/import status with an isolated Python snippet;
- convert probe results into the persisted `AiInstallType` level.

Notes:
The probe may execute external Python and must run during startup before GUI creation or on a
background worker when used from UI code.
*/

use crate::config;
// The activated-shell Python probe (spawning `sh`/`powershell` + Python) is
// native-only; the pure report parsing/classification stays target-neutral.
#[cfg(not(target_arch = "wasm32"))]
use crate::python_manager::{self, PythonShellKind};
use crate::runtime_log;
use serde::Deserialize;
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use ms_thread as thread;

#[cfg(not(target_arch = "wasm32"))]
const AI_PROBE_PYTHON_CODE: &str = r#"
import json
from importlib import metadata


result = {}


def distribution_info(candidates):
    for name in candidates:
        try:
            version = metadata.version(name)
        except metadata.PackageNotFoundError:
            continue
        return {"name": name, "version": version}
    return None


try:
    import torch
except ModuleNotFoundError:
    result["torch"] = {"installed": False}
except Exception as exc:
    result["torch"] = {
        "installed": True,
        "version": None,
        "support": [],
        "import_error": f"{type(exc).__name__}: {exc}",
    }
else:
    support = ["CPU"]
    if getattr(torch.version, "cuda", None):
        support.append("CUDA")
    if getattr(torch.version, "hip", None):
        support.append("ROCm")
    result["torch"] = {
        "installed": True,
        "version": getattr(torch, "__version__", ""),
        "support": support,
        "import_error": None,
    }


try:
    import onnxruntime as ort
except ModuleNotFoundError:
    dist = distribution_info([
        "onnxruntime",
        "onnxruntime-gpu",
        "onnxruntime-directml",
        "onnxruntime-openvino",
        "onnxruntime-migraphx",
    ])
    if dist is None:
        result["onnxruntime"] = {"installed": False}
    else:
        package_providers = []
        if dist["name"] == "onnxruntime-migraphx":
            package_providers.append("MIGraphXExecutionProvider")
        elif dist["name"] == "onnxruntime-gpu":
            package_providers.extend(["CUDAExecutionProvider", "TensorrtExecutionProvider"])
        elif dist["name"] == "onnxruntime-directml":
            package_providers.append("DmlExecutionProvider")
        elif dist["name"] == "onnxruntime-openvino":
            package_providers.append("OpenVINOExecutionProvider")
        elif dist["name"] == "onnxruntime":
            package_providers.append("CPUExecutionProvider")
        result["onnxruntime"] = {
            "installed": True,
            "version": dist["version"],
            "providers": package_providers,
            "import_error": f"Distribution {dist['name']} is installed, but module 'onnxruntime' was not importable.",
        }
except Exception as exc:
    dist = distribution_info([
        "onnxruntime",
        "onnxruntime-gpu",
        "onnxruntime-directml",
        "onnxruntime-openvino",
        "onnxruntime-migraphx",
    ])
    result["onnxruntime"] = {
        "installed": True,
        "version": None if dist is None else dist["version"],
        "providers": [],
        "import_error": f"{type(exc).__name__}: {exc}",
    }
else:
    result["onnxruntime"] = {
        "installed": True,
        "version": getattr(ort, "__version__", ""),
        "providers": ort.get_available_providers(),
        "import_error": None,
    }

print(json.dumps(result, ensure_ascii=False))
"#;

#[derive(Debug, Clone)]
pub struct AiComputationsReport {
    pub torch: AiPackageProbe,
    pub onnxruntime: AiPackageProbe,
}

#[derive(Debug, Clone, Default)]
pub struct AiPackageProbe {
    pub installed: bool,
    pub version: Option<String>,
    pub support: Vec<String>,
    pub providers: Vec<String>,
    pub import_error: Option<String>,
}

#[must_use]
pub fn detect_ai_install_type_from_report(report: &AiComputationsReport) -> config::AiInstallType {
    if report.torch.installed && report.torch.import_error.is_none() {
        config::AiInstallType::Full
    } else if report.onnxruntime.installed && report.onnxruntime.import_error.is_none() {
        config::AiInstallType::Base
    } else {
        config::AiInstallType::None
    }
}

pub fn spawn_ai_computations_probe(
    app_dir: PathBuf,
) -> Receiver<Result<AiComputationsReport, String>> {
    let (tx, rx) = mpsc::channel();
    let spawn_result = thread::Builder::new()
        .name("ai-computations-probe".to_string())
        .spawn(move || {
            let result = collect_ai_computations_report(&app_dir);
            if tx.send(result).is_err() {
                runtime_log::log_warn("[ai-install-probe] result receiver was dropped");
            }
        });

    if let Err(err) = spawn_result {
        let (fallback_tx, fallback_rx) = mpsc::channel();
        let message = format!("Не удалось запустить фоновую проверку ИИ окружения: {err}");
        if fallback_tx.send(Err(message)).is_err() {
            runtime_log::log_warn("[ai-install-probe] failed to send probe spawn error");
        }
        return fallback_rx;
    }

    rx
}

/// Web stub: probing an installed Python/AI environment requires spawning an
/// activated shell + Python, which does not exist in the browser. Returns a clear
/// typed error instead of a fabricated report.
#[cfg(target_arch = "wasm32")]
pub fn collect_ai_computations_report(_app_dir: &Path) -> Result<AiComputationsReport, String> {
    Err("Проверка ИИ окружения недоступна в веб-версии.".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn collect_ai_computations_report(app_dir: &Path) -> Result<AiComputationsReport, String> {
    let environment = match python_manager::detect_python_environment(app_dir) {
        Ok(environment) => Some(environment),
        Err(err) => {
            runtime_log::log_warn(format!(
                "[ai-install-probe] AI package probe will use inherited shell PATH because app-local Python env was not found: {err}"
            ));
            None
        }
    };
    runtime_log::log_info(format!(
        "[ai-install-probe] probing AI Python packages through activated shell in '{}'",
        app_dir.display()
    ));

    let shell_script = ai_probe_shell_script(app_dir, environment.as_ref());
    let mut command = build_ai_probe_shell_command(&shell_script);
    apply_hidden_process_flags(&mut command);
    command
        .current_dir(app_dir)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = command
        .output()
        .map_err(|err| format!("Не удалось запустить Python для проверки ИИ пакетов: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        runtime_log::log_error(format!(
            "[ai-install-probe] AI package probe failed; status={}; stderr={}",
            output.status,
            stderr.trim()
        ));
        return Err(format!(
            "Проверка ИИ пакетов завершилась с ошибкой: {}",
            stderr.trim()
        ));
    }

    parse_ai_probe_output(&stdout)
}

pub fn parse_ai_probe_output(stdout: &str) -> Result<AiComputationsReport, String> {
    let Some(json_line) = stdout.lines().rev().find(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('{') && trimmed.contains("\"torch\"")
    }) else {
        return Err(format!(
            "Python не вернул JSON проверки ИИ пакетов. Ответ: {}",
            stdout.trim()
        ));
    };
    let raw: RawAiProbeReport = serde_json::from_str(json_line.trim()).map_err(|err| {
        format!(
            "Python вернул некорректный JSON проверки ИИ пакетов: {err}. Ответ: {}",
            stdout.trim()
        )
    })?;

    Ok(AiComputationsReport {
        torch: raw.torch.into_torch_probe(),
        onnxruntime: raw.onnxruntime.into_onnxruntime_probe(),
    })
}

#[derive(Debug, Deserialize)]
struct RawAiProbeReport {
    torch: RawPackageProbe,
    onnxruntime: RawPackageProbe,
}

#[derive(Debug, Deserialize)]
struct RawPackageProbe {
    installed: bool,
    version: Option<String>,
    #[serde(default)]
    support: Vec<String>,
    #[serde(default)]
    providers: Vec<String>,
    import_error: Option<String>,
}

impl RawPackageProbe {
    fn into_torch_probe(self) -> AiPackageProbe {
        AiPackageProbe {
            installed: self.installed,
            version: self.version.filter(|version| !version.trim().is_empty()),
            support: self.support,
            providers: Vec::new(),
            import_error: self
                .import_error
                .filter(|message| !message.trim().is_empty()),
        }
    }

    fn into_onnxruntime_probe(self) -> AiPackageProbe {
        let providers = self
            .providers
            .into_iter()
            .map(|provider| short_onnx_execution_provider_name(&provider))
            .collect();
        AiPackageProbe {
            installed: self.installed,
            version: self.version.filter(|version| !version.trim().is_empty()),
            support: Vec::new(),
            providers,
            import_error: self
                .import_error
                .filter(|message| !message.trim().is_empty()),
        }
    }
}

fn short_onnx_execution_provider_name(provider: &str) -> String {
    match provider {
        "CPUExecutionProvider" => "CPU".to_string(),
        "CUDAExecutionProvider" => "CUDA".to_string(),
        "DmlExecutionProvider" => "DirectML".to_string(),
        "TensorrtExecutionProvider" => "TensorRT".to_string(),
        "MIGraphXExecutionProvider" => "MiGraphX".to_string(),
        "ROCMExecutionProvider" => "ROCm".to_string(),
        "OpenVINOExecutionProvider" => "OpenVINO".to_string(),
        "CoreMLExecutionProvider" => "CoreML".to_string(),
        "AzureExecutionProvider" => "Azure".to_string(),
        other => other
            .strip_suffix("ExecutionProvider")
            .unwrap_or(other)
            .to_string(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn ai_probe_shell_script(
    app_dir: &Path,
    environment: Option<&python_manager::PythonEnvironment>,
) -> String {
    let mut commands = Vec::new();
    commands.push(configure_shell_encoding_command());
    commands.push(change_directory_command(app_dir));
    if let Some(environment) = environment {
        commands.extend(python_manager::activation_commands(
            environment,
            python_shell_kind(),
        ));
    } else {
        commands.push(shell_echo_command(
            "Локальное Python-окружение не найдено; используем python из текущего PATH.",
        ));
    }
    commands.push(ai_probe_python_command());
    shell_join_commands(commands)
}

#[cfg(target_os = "windows")]
fn build_ai_probe_shell_command(script: &str) -> Command {
    let mut command = Command::new("powershell");
    command
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg(script);
    command
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn build_ai_probe_shell_command(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    command
}

#[cfg(target_os = "windows")]
fn ai_probe_python_command() -> String {
    format!(
        "$manhwaStudioAiProbeCode = {};\n\
         $manhwaStudioAiProbePath = [System.IO.Path]::Combine([System.IO.Path]::GetTempPath(), ('manhwastudio_ai_probe_' + [System.Guid]::NewGuid().ToString('N') + '.py'));\n\
         [System.IO.File]::WriteAllText($manhwaStudioAiProbePath, $manhwaStudioAiProbeCode, (New-Object System.Text.UTF8Encoding $false));\n\
         python -X utf8 $manhwaStudioAiProbePath;\n\
         $manhwaStudioAiProbeStatus = $LASTEXITCODE;\n\
         Remove-Item -LiteralPath $manhwaStudioAiProbePath -ErrorAction SilentlyContinue;\n\
         exit $manhwaStudioAiProbeStatus",
        powershell_single_quoted_here_string(AI_PROBE_PYTHON_CODE)
    )
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn ai_probe_python_command() -> String {
    format!(
        "python -X utf8 - <<'MANHWASTUDIO_AI_PROBE_PY'\n{}\nMANHWASTUDIO_AI_PROBE_PY",
        AI_PROBE_PYTHON_CODE
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn shell_join_commands(commands: Vec<String>) -> String {
    commands.join(";\n")
}

#[cfg(target_os = "windows")]
fn configure_shell_encoding_command() -> String {
    "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8".to_string()
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn configure_shell_encoding_command() -> String {
    "export LANG=C.UTF-8; export LC_ALL=C.UTF-8".to_string()
}

#[cfg(target_os = "windows")]
fn change_directory_command(path: &Path) -> String {
    format!("Set-Location -LiteralPath '{}'", powershell_escape(path))
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn change_directory_command(path: &Path) -> String {
    format!("cd '{}'", sh_escape(path))
}

#[cfg(target_os = "windows")]
fn python_shell_kind() -> PythonShellKind {
    PythonShellKind::PowerShell
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn python_shell_kind() -> PythonShellKind {
    PythonShellKind::PosixSh
}

#[cfg(target_os = "windows")]
fn shell_echo_command(message: &str) -> String {
    format!("Write-Output '{}'", powershell_escape_str(message))
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn shell_echo_command(message: &str) -> String {
    format!("printf '%s\n' '{}'", sh_escape_str(message))
}

#[cfg(target_os = "windows")]
fn apply_hidden_process_flags(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn apply_hidden_process_flags(_command: &mut Command) {}

#[cfg(target_os = "windows")]
fn powershell_escape(path: &Path) -> String {
    powershell_escape_str(&path.to_string_lossy())
}

#[cfg(target_os = "windows")]
fn powershell_escape_str(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(target_os = "windows")]
fn powershell_single_quoted_here_string(value: &str) -> String {
    format!("@'\n{}\n'@", value.replace("\r\n", "\n"))
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn sh_escape(path: &Path) -> String {
    sh_escape_str(&path.to_string_lossy())
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_os = "windows"))]
fn sh_escape_str(value: &str) -> String {
    value.replace('\'', r"'\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ai_probe_output_accepts_missing_torch() {
        let report = parse_ai_probe_output(
            r#"
noise
{"torch":{"installed":false},"onnxruntime":{"installed":true,"version":"1.22.0","providers":["CPUExecutionProvider"],"import_error":null}}
"#,
        )
        .expect("probe output should parse");

        assert!(!report.torch.installed);
        assert!(report.torch.version.is_none());
        assert!(report.torch.support.is_empty());
        assert!(report.torch.import_error.is_none());
        assert!(report.onnxruntime.installed);
        assert_eq!(report.onnxruntime.version.as_deref(), Some("1.22.0"));
        assert_eq!(report.onnxruntime.providers, ["CPU"]);
    }

    #[test]
    fn parse_ai_probe_output_preserves_torch_import_error() {
        let report = parse_ai_probe_output(
            r#"{"torch":{"installed":true,"version":null,"support":[],"import_error":"ImportError: broken DLL"},"onnxruntime":{"installed":false}}"#,
        )
        .expect("probe output should parse");

        assert!(report.torch.installed);
        assert_eq!(
            report.torch.import_error.as_deref(),
            Some("ImportError: broken DLL")
        );
        assert!(!report.onnxruntime.installed);
    }

    #[test]
    fn detect_ai_install_type_prefers_torch_over_onnxruntime() {
        let report = AiComputationsReport {
            torch: AiPackageProbe {
                installed: true,
                import_error: None,
                ..AiPackageProbe::default()
            },
            onnxruntime: AiPackageProbe {
                installed: true,
                import_error: None,
                ..AiPackageProbe::default()
            },
        };

        assert_eq!(
            detect_ai_install_type_from_report(&report),
            config::AiInstallType::Full
        );
    }

    #[test]
    fn detect_ai_install_type_uses_onnxruntime_for_base() {
        let report = AiComputationsReport {
            torch: AiPackageProbe {
                installed: false,
                import_error: None,
                ..AiPackageProbe::default()
            },
            onnxruntime: AiPackageProbe {
                installed: true,
                import_error: None,
                ..AiPackageProbe::default()
            },
        };

        assert_eq!(
            detect_ai_install_type_from_report(&report),
            config::AiInstallType::Base
        );
    }

    #[test]
    fn detect_ai_install_type_treats_import_errors_as_missing() {
        let report = AiComputationsReport {
            torch: AiPackageProbe {
                installed: true,
                import_error: Some("ImportError: broken".to_string()),
                ..AiPackageProbe::default()
            },
            onnxruntime: AiPackageProbe {
                installed: true,
                import_error: Some("ImportError: broken".to_string()),
                ..AiPackageProbe::default()
            },
        };

        assert_eq!(
            detect_ai_install_type_from_report(&report),
            config::AiInstallType::None
        );
    }
}
