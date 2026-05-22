/*
File: gpu_utils.rs

Purpose:
Shared GPU and accelerator detection helpers for installer and runtime code.

Main responsibilities:
- detect NVIDIA/AMD adapters through platform tools;
- detect CUDA, ROCm, and NVIDIA Compute Capability versions;
- inspect Linux driver/ROCm installation state;
- resolve AMD GPU architecture / LLVM target and validate ROCm 7.2 support;
- detect likely DirectML-capable accelerators on Windows.

Key structures:
- RuntimeVersion
- GpuArchitecture
- LinuxDriverStatus
- RocmInstallationStatus
- RocmSupportValidation
- DirectMlAccelerator

Key functions:
- detect_nvidia_gpu()
- detect_amd_gpu()
- detect_cuda_runtime_version()
- detect_nvidia_compute_capability()
- detect_rocm_runtime_version()
- validate_rocm_7_2_support_linux()
- detect_directml_accelerators_windows()

Notes:
Detection intentionally uses short-lived system commands and filesystem probes.
Callers must run these helpers outside the GUI thread.
*/

use std::fmt::Display;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct RuntimeVersion {
    pub major: u32,
    pub minor: u32,
}

impl RuntimeVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

impl Display for RuntimeVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GpuVendor {
    Amd,
    Nvidia,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuArchitecture {
    pub vendor: GpuVendor,
    pub name: Option<String>,
    pub architecture: Option<String>,
    pub llvm_target: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxDriverStatus {
    pub amdgpu_loaded: bool,
    pub kfd_available: bool,
    pub dri_available: bool,
    pub nvidia_driver_available: bool,
    pub nvidia_device_nodes_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RocmInstallationStatus {
    pub present: bool,
    pub version: Option<RuntimeVersion>,
    pub details: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RocmSupportValidation {
    pub supported: bool,
    pub reason: String,
    pub driver_status: LinuxDriverStatus,
    pub rocm_status: RocmInstallationStatus,
    pub architectures: Vec<GpuArchitecture>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectMlAccelerator {
    pub name: String,
    pub vendor: Option<GpuVendor>,
}

pub const ROCM_7_2_SUPPORTED_LLVM_TARGETS: &[&str] = &[
    "gfx950", "gfx942", "gfx90a", "gfx908", "gfx1201", "gfx1200", "gfx1101", "gfx1100", "gfx1030",
];

#[must_use]
pub fn rocm_7_2_supported_llvm_targets() -> &'static [&'static str] {
    ROCM_7_2_SUPPORTED_LLVM_TARGETS
}

#[must_use]
pub fn detect_nvidia_gpu() -> bool {
    if let Some(output) = command_output("nvidia-smi", &["-L"]) {
        let text = output.to_ascii_lowercase();
        if text.contains("gpu") || text.contains("nvidia") {
            return true;
        }
    }

    if cfg!(target_os = "windows")
        && windows_video_controller_output()
            .as_deref()
            .is_some_and(|output| output.to_ascii_lowercase().contains("nvidia"))
    {
        return true;
    }

    if cfg!(target_os = "linux")
        && let Some(output) = command_output("lspci", &[])
        && output.to_ascii_lowercase().contains("nvidia")
    {
        return true;
    }

    false
}

#[must_use]
pub fn detect_amd_gpu() -> bool {
    if cfg!(target_os = "linux") {
        return detect_amd_gpu_linux();
    }

    if cfg!(target_os = "windows") {
        return windows_video_controller_output()
            .as_deref()
            .is_some_and(text_has_amd_gpu);
    }

    false
}

#[must_use]
pub fn detect_amd_gpu_linux() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }

    if let Some(output) = command_output("lspci", &[]) {
        return text_has_amd_gpu(&output);
    }

    false
}

#[must_use]
pub fn detect_cuda_runtime_version() -> Option<RuntimeVersion> {
    if let Some(output) = command_output("nvidia-smi", &[])
        && let Some(v) = parse_version_after_keywords(&output, &["cuda version"])
    {
        return Some(v);
    }

    if let Some(output) = command_output("nvcc", &["--version"])
        && let Some(v) = parse_version_after_keywords(&output, &["release", "cuda"])
    {
        return Some(v);
    }

    None
}

#[must_use]
pub fn detect_rocm_runtime_version() -> Option<RuntimeVersion> {
    if let Some(output) =
        command_output_from_candidates(&["rocminfo", "/opt/rocm/bin/rocminfo"], &[])
        && let Some(v) = parse_version_after_keywords(&output, &["rocm version", "runtime version"])
    {
        return Some(v);
    }

    if let Some(output) =
        command_output_from_candidates(&["hipcc", "/opt/rocm/bin/hipcc"], &["--version"])
        && let Some(v) = parse_version_after_keywords(&output, &["rocm version", "hip version"])
    {
        return Some(v);
    }

    if let Some(output) = command_output_from_candidates(
        &["rocm-smi", "/opt/rocm/bin/rocm-smi"],
        &["--showdriverversion"],
    ) && let Some(v) = parse_first_major_minor(&output)
    {
        return Some(v);
    }

    None
}

#[must_use]
pub fn detect_nvidia_compute_capability() -> Option<RuntimeVersion> {
    if let Some(output) = command_output(
        "nvidia-smi",
        &["--query-gpu=compute_cap", "--format=csv,noheader"],
    ) && let Some(v) = parse_min_runtime_version_from_lines(&output)
    {
        return Some(v);
    }

    if let Some(output) = command_output("nvidia-smi", &["-q"])
        && let Some(v) = parse_version_after_keywords(&output, &["compute capability"])
    {
        return Some(v);
    }

    None
}

#[must_use]
pub fn detect_nvidia_gpu_architecture() -> Option<GpuArchitecture> {
    detect_nvidia_compute_capability().map(|capability| GpuArchitecture {
        vendor: GpuVendor::Nvidia,
        name: Some(format!("NVIDIA SM {capability}")),
        architecture: Some(format!("sm_{}{}", capability.major, capability.minor)),
        llvm_target: None,
    })
}

#[must_use]
pub fn linux_driver_status() -> LinuxDriverStatus {
    LinuxDriverStatus {
        amdgpu_loaded: Path::new("/sys/module/amdgpu").is_dir(),
        kfd_available: Path::new("/dev/kfd").exists(),
        dri_available: Path::new("/dev/dri").is_dir(),
        nvidia_driver_available: Path::new("/proc/driver/nvidia/version").is_file()
            || command_output("nvidia-smi", &["-L"]).is_some(),
        nvidia_device_nodes_available: linux_nvidia_device_nodes_available(),
    }
}

#[must_use]
pub fn detect_rocm_installation_linux() -> RocmInstallationStatus {
    if !cfg!(target_os = "linux") {
        return RocmInstallationStatus {
            present: false,
            version: None,
            details: "ROCm Linux probe skipped on this target.".to_string(),
        };
    }

    let mut details = Vec::new();
    let version = detect_rocm_runtime_version();

    for command in ["rocminfo", "hipcc", "rocm-smi"] {
        if let Some(path) = command_path(command) {
            details.push(format!("{command}: {}", path.display()));
        }
    }
    if Path::new("/opt/rocm").is_dir() {
        details.push("/opt/rocm exists".to_string());
    }

    let present = version.is_some() || !details.is_empty();
    if details.is_empty() {
        details.push("ROCm not detected through rocminfo/hipcc/rocm-smi or /opt/rocm.".to_string());
    }

    RocmInstallationStatus {
        present,
        version,
        details: details.join("\n"),
    }
}

#[must_use]
pub fn detect_amd_gpu_architectures_linux() -> Vec<GpuArchitecture> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }

    let mut architectures = Vec::new();
    for target in detect_amd_gfx_targets_from_rocm_tools() {
        architectures.push(GpuArchitecture {
            vendor: GpuVendor::Amd,
            name: None,
            architecture: rocm_architecture_family(&target).map(str::to_string),
            llvm_target: Some(target),
        });
    }

    if architectures.is_empty() {
        architectures.extend(detect_amd_gfx_targets_from_lspci().into_iter().map(
            |(name, target)| GpuArchitecture {
                vendor: GpuVendor::Amd,
                name: Some(name),
                architecture: rocm_architecture_family(&target).map(str::to_string),
                llvm_target: Some(target),
            },
        ));
    }

    dedupe_architectures(architectures)
}

#[must_use]
pub fn validate_rocm_7_2_support_linux() -> RocmSupportValidation {
    let driver_status = linux_driver_status();
    let rocm_status = detect_rocm_installation_linux();
    let architectures = detect_amd_gpu_architectures_linux();

    if !detect_amd_gpu_linux() {
        return RocmSupportValidation {
            supported: false,
            reason: "AMD GPU was not detected on Linux.".to_string(),
            driver_status,
            rocm_status,
            architectures,
        };
    }

    if !driver_status.amdgpu_loaded || !driver_status.kfd_available {
        return RocmSupportValidation {
            supported: false,
            reason: "AMD GPU detected, but amdgpu driver or /dev/kfd is unavailable.".to_string(),
            driver_status,
            rocm_status,
            architectures,
        };
    }

    if !rocm_status.present {
        return RocmSupportValidation {
            supported: false,
            reason: "AMD GPU detected, but ROCm installation was not found.".to_string(),
            driver_status,
            rocm_status,
            architectures,
        };
    }

    let has_supported_architecture = architectures.iter().any(|arch| {
        arch.llvm_target
            .as_deref()
            .is_some_and(is_rocm_7_2_supported_llvm_target)
    });

    if has_supported_architecture {
        RocmSupportValidation {
            supported: true,
            reason: "Detected AMD GPU architecture is supported by ROCm 7.2.".to_string(),
            driver_status,
            rocm_status,
            architectures,
        }
    } else {
        RocmSupportValidation {
            supported: false,
            reason:
                "AMD GPU architecture was not detected or is not officially supported by ROCm 7.2."
                    .to_string(),
            driver_status,
            rocm_status,
            architectures,
        }
    }
}

#[must_use]
pub fn detect_directml_accelerators_windows() -> Vec<DirectMlAccelerator> {
    if !cfg!(target_os = "windows") {
        return Vec::new();
    }

    windows_video_controller_output()
        .map(|output| {
            output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .filter(|line| !line.to_ascii_lowercase().contains("microsoft basic"))
                .filter_map(directml_accelerator_from_name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

#[must_use]
pub fn has_directml_accelerator_windows() -> bool {
    !detect_directml_accelerators_windows().is_empty()
}

#[must_use]
pub fn is_rocm_7_2_supported_llvm_target(target: &str) -> bool {
    let normalized = target.trim().to_ascii_lowercase();
    ROCM_7_2_SUPPORTED_LLVM_TARGETS
        .iter()
        .any(|supported| *supported == normalized)
}

fn directml_accelerator_from_name(name: &str) -> Option<DirectMlAccelerator> {
    let lower = name.to_ascii_lowercase();
    let vendor = if lower.contains("nvidia") {
        Some(GpuVendor::Nvidia)
    } else if text_has_amd_gpu(name) {
        Some(GpuVendor::Amd)
    } else if lower.contains("intel") || lower.contains("arc") || lower.contains("iris") {
        None
    } else {
        return None;
    };

    Some(DirectMlAccelerator {
        name: name.to_string(),
        vendor,
    })
}

fn detect_amd_gfx_targets_from_rocm_tools() -> Vec<String> {
    let mut targets = Vec::new();
    for (command, args) in [
        ("rocm_agent_enumerator", &[] as &[&str]),
        ("/opt/rocm/bin/rocm_agent_enumerator", &[] as &[&str]),
        ("rocminfo", &[] as &[&str]),
        ("/opt/rocm/bin/rocminfo", &[] as &[&str]),
    ] {
        if let Some(output) = command_output(command, args) {
            targets.extend(parse_gfx_targets(&output));
        }
    }
    dedupe_strings(targets)
}

fn detect_amd_gfx_targets_from_lspci() -> Vec<(String, String)> {
    command_output("lspci", &[])
        .map(|output| {
            output
                .lines()
                .filter(|line| text_has_amd_gpu(line))
                .filter_map(|line| {
                    amd_lspci_line_to_gfx_target(line).map(|target| (line.to_string(), target))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn amd_lspci_line_to_gfx_target(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let target = if lower.contains("mi355") || lower.contains("mi350") {
        "gfx950"
    } else if lower.contains("mi325") || lower.contains("mi300") {
        "gfx942"
    } else if lower.contains("mi250") || lower.contains("mi210") {
        "gfx90a"
    } else if lower.contains("mi100") {
        "gfx908"
    } else if lower.contains("r9700") || lower.contains("r9600d") || lower.contains("9070") {
        "gfx1201"
    } else if lower.contains("9060") {
        "gfx1200"
    } else if lower.contains("7900") || lower.contains("w7900") || lower.contains("w7800") {
        "gfx1100"
    } else if lower.contains("7800") || lower.contains("7700") || lower.contains("w7700") {
        "gfx1101"
    } else if lower.contains("w6800") || lower.contains("v620") {
        "gfx1030"
    } else {
        return None;
    };
    Some(target.to_string())
}

fn rocm_architecture_family(target: &str) -> Option<&'static str> {
    match target {
        "gfx950" => Some("CDNA4"),
        "gfx942" => Some("CDNA3"),
        "gfx90a" => Some("CDNA2"),
        "gfx908" => Some("CDNA"),
        "gfx1201" | "gfx1200" => Some("RDNA4"),
        "gfx1101" | "gfx1100" => Some("RDNA3"),
        "gfx1030" => Some("RDNA2"),
        _ => None,
    }
}

fn parse_version_after_keywords(text: &str, keywords: &[&str]) -> Option<RuntimeVersion> {
    let lower = text.to_ascii_lowercase();
    for keyword in keywords {
        let needle = keyword.to_ascii_lowercase();
        if let Some(idx) = lower.find(&needle) {
            let start = idx + needle.len();
            if let Some(v) = parse_first_major_minor(&text[start..]) {
                return Some(v);
            }
        }
    }
    None
}

fn parse_first_major_minor(text: &str) -> Option<RuntimeVersion> {
    for token in text.split(|ch: char| !(ch.is_ascii_digit() || ch == '.')) {
        if !token.contains('.') {
            continue;
        }
        let mut parts = token.split('.');
        let major_s = parts.next().unwrap_or_default();
        let minor_s = parts.next().unwrap_or_default();
        if major_s.is_empty() || minor_s.is_empty() {
            continue;
        }
        let major = match major_s.parse::<u32>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let minor = match minor_s.parse::<u32>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        return Some(RuntimeVersion::new(major, minor));
    }
    None
}

fn parse_min_runtime_version_from_lines(text: &str) -> Option<RuntimeVersion> {
    let mut min_value: Option<RuntimeVersion> = None;
    for line in text.lines() {
        let parsed = match parse_first_major_minor(line) {
            Some(v) => v,
            None => continue,
        };
        min_value = Some(match min_value {
            Some(current_min) => current_min.min(parsed),
            None => parsed,
        });
    }
    min_value
}

fn parse_gfx_targets(text: &str) -> Vec<String> {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter_map(normalize_gfx_token)
        .collect()
}

fn normalize_gfx_token(token: &str) -> Option<String> {
    let lower = token.trim().to_ascii_lowercase();
    let start = lower.find("gfx")?;
    let tail = &lower[start..];
    let normalized = tail
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();
    if normalized.len() > 3 {
        Some(normalized)
    } else {
        None
    }
}

fn text_has_amd_gpu(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("amd ")
        || lower.contains(" amd ")
        || lower.contains("amd/ati")
        || lower.contains("advanced micro devices")
        || lower.contains("radeon")
}

fn windows_video_controller_output() -> Option<String> {
    if let Some(output) = command_output("wmic", &["path", "win32_VideoController", "get", "name"])
    {
        return Some(output);
    }

    command_output(
        "powershell",
        &[
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name) -join \"`n\"",
        ],
    )
}

fn command_output_from_candidates(commands: &[&str], args: &[&str]) -> Option<String> {
    commands
        .iter()
        .find_map(|command| command_output(command, args))
}

fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(command);
    apply_windows_no_window(&mut cmd);
    let output = cmd
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.to_string()
    } else if stderr.trim().is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };

    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn command_path(command: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(command))
            .find(|candidate| candidate.is_file())
    })
}

fn linux_nvidia_device_nodes_available() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    if Path::new("/dev/nvidiactl").exists() {
        return true;
    }

    #[cfg(target_os = "linux")]
    {
        fs::read_dir("/dev")
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(Result::ok))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .any(|name| {
                name.strip_prefix("nvidia")
                    .is_some_and(|suffix| suffix.chars().all(|ch| ch.is_ascii_digit()))
            })
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn dedupe_architectures(items: Vec<GpuArchitecture>) -> Vec<GpuArchitecture> {
    let mut deduped = Vec::with_capacity(items.len());
    for item in items {
        if deduped.iter().any(|existing: &GpuArchitecture| {
            existing.vendor == item.vendor
                && existing.llvm_target == item.llvm_target
                && existing.name == item.name
        }) {
            continue;
        }
        deduped.push(item);
    }
    deduped
}

fn dedupe_strings(items: Vec<String>) -> Vec<String> {
    let mut deduped = Vec::with_capacity(items.len());
    for item in items {
        if deduped.iter().any(|existing| existing == &item) {
            continue;
        }
        deduped.push(item);
    }
    deduped
}

#[cfg(target_os = "windows")]
fn apply_windows_no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(target_os = "windows"))]
fn apply_windows_no_window(_command: &mut Command) {}
