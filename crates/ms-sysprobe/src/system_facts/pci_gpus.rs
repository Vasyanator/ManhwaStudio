/*
File: crates/ms-sysprobe/src/system_facts/pci_gpus.rs

Purpose:
The GPU half of `system_facts` on the PCI platforms (Linux, Windows): one display-device walk
(`amd_adapters::enumerate_display_devices`), mapped to adapters, enriched by ONE
`nvidia-smi --query-gpu` call, made whatever the walk saw (plus plain `nvidia-smi` for the
driver's CUDA version when it reported GPUs).

Key functions:
- probe(): walk + NVIDIA enrichment -> (adapters, CUDA driver version); the platform file fills
  missing names and builds `GpuFacts`.

Notes:
Every process runs through `gpu_utils::run_command_bounded` with `NVIDIA_SMI_TIMEOUT`; a
missing `nvidia-smi` is the normal "no NVIDIA driver" answer and is not logged. A driver too
old for the `compute_cap` field is retried once without it.
*/

use std::time::Duration;

use ms_log::runtime_log;

use crate::amd_adapters::enumerate_display_devices;
use crate::gpu_utils::{CommandRunError, RuntimeVersion, parse_version_after_keywords, run_command_bounded};

use super::parse::{NVIDIA_SMI_FORMAT, NVIDIA_SMI_QUERY_WITH_COMPUTE_CAP, NVIDIA_SMI_QUERY_WITHOUT_COMPUTE_CAP, NvidiaSmiGpu, parse_nvidia_smi_query};
use super::{GpuAdapterFacts, adapter_from_pci, merge_nvidia_smi};

/// Upper bound of one `nvidia-smi` run (it answers in ~0.2 s; a wedged driver can hang it).
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// Walks the PCI display devices, maps them to adapters and merges the `nvidia-smi` rows.
/// Returns the (unordered) adapters and the CUDA version the NVIDIA driver supports. Blocking.
pub(super) fn probe() -> (Vec<GpuAdapterFacts>, Option<RuntimeVersion>) {
    let devices = enumerate_display_devices();
    let adapters: Vec<GpuAdapterFacts> = devices.iter().map(adapter_from_pci).collect();
    // Asked whatever the walk saw: an NVIDIA GPU can be invisible to it (no DRM card with
    // modesetting off or on a headless compute card, WSL without `/sys/class/drm`). Without an
    // NVIDIA driver this costs one failed spawn (`NotFound`).
    let rows = nvidia_smi_rows();
    let cuda = if rows.is_empty() { None } else { nvidia_cuda_driver_version() };
    (merge_nvidia_smi(adapters, rows), cuda)
}

/// Runs the `nvidia-smi` GPU query (retrying once without `compute_cap` when the driver
/// rejects the query with a non-zero exit) and parses its rows. Empty when the tool is missing
/// or fails (logged unless missing).
fn nvidia_smi_rows() -> Vec<NvidiaSmiGpu> {
    match nvidia_smi(&[NVIDIA_SMI_QUERY_WITH_COMPUTE_CAP, NVIDIA_SMI_FORMAT]) {
        NvidiaSmiAnswer::Output(text) => return parse_nvidia_smi_query(&text, true),
        NvidiaSmiAnswer::Missing | NvidiaSmiAnswer::Failed => return Vec::new(),
        // Old drivers reject the unknown `compute_cap` field with a non-zero exit.
        NvidiaSmiAnswer::Rejected => {}
    }
    match nvidia_smi(&[NVIDIA_SMI_QUERY_WITHOUT_COMPUTE_CAP, NVIDIA_SMI_FORMAT]) {
        NvidiaSmiAnswer::Output(text) => parse_nvidia_smi_query(&text, false),
        NvidiaSmiAnswer::Missing | NvidiaSmiAnswer::Rejected | NvidiaSmiAnswer::Failed => Vec::new(),
    }
}

/// The `CUDA Version:` of plain `nvidia-smi` output; `None` (logged) when absent.
fn nvidia_cuda_driver_version() -> Option<RuntimeVersion> {
    let NvidiaSmiAnswer::Output(text) = nvidia_smi(&[]) else {
        return None;
    };
    let version = parse_version_after_keywords(&text, &["cuda version"]);
    if version.is_none() {
        runtime_log::log_info("[system-facts] nvidia-smi printed no `CUDA Version:`");
    }
    version
}

/// Outcome of one `nvidia-smi` run.
enum NvidiaSmiAnswer {
    /// Exit 0; stdout.
    Output(String),
    /// Not installed (no NVIDIA driver): the normal negative answer.
    Missing,
    /// Ran and exited non-zero (logged): e.g. a query field this driver does not know.
    Rejected,
    /// Could not run to completion: timed out, unreadable output, spawn error (logged).
    Failed,
}

/// Runs `nvidia-smi args` under [`NVIDIA_SMI_TIMEOUT`].
fn nvidia_smi(args: &[&str]) -> NvidiaSmiAnswer {
    match run_command_bounded("nvidia-smi", args, NVIDIA_SMI_TIMEOUT) {
        Ok(output) if output.status.success() => NvidiaSmiAnswer::Output(output.stdout),
        Ok(output) => {
            runtime_log::log_info(format!(
                "[system-facts] `nvidia-smi {}` exited with {}: {}",
                args.join(" "),
                output.status,
                output.stdout.lines().chain(output.stderr.lines()).next().unwrap_or_default().trim()
            ));
            NvidiaSmiAnswer::Rejected
        }
        Err(CommandRunError::Spawn(err)) if err.kind() == std::io::ErrorKind::NotFound => NvidiaSmiAnswer::Missing,
        Err(err) => {
            runtime_log::log_warn(format!("[system-facts] `nvidia-smi {}` failed: {err}", args.join(" ")));
            NvidiaSmiAnswer::Failed
        }
    }
}
