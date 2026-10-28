/*
File: crates/ms-launcher/src/pages/system_info/report.rs

Purpose:
GUI-free assembly of the launcher's "System info" tab: turns the worker's probe result, the
backend's pushed PyTorch report and the process's local-AI availability into localized display
cards, and renders the same cards as the plain-text report the "Copy report" button copies.

Key structures:
- ProbeResult / NativeProbe / NativeSelection / TorchOffer / RocmOffer: what the worker learned
  (hardware facts, native ONNX verdicts, the PyTorch offer).
- BackendTorch: the running backend's PyTorch build, read from the health snapshot per frame.
- ReportCard / ReportItem / Tone: the display model the view draws and `plain_text` prints.

Key functions:
- TorchOffer::from_preflight(), BackendTorch::from_snapshot(): owner data -> report data.
- build_report(): the three cards (System, Graphics, AI acceleration).
- plain_text(): the copyable text of those cards.

Notes:
No decision is made here: hardware facts come from `ms_sysprobe::system_facts`, build
availability from `ms_settings_ui::native_builds`, the PyTorch offer from the installer's
preflight, the backend text from `ms_settings_ui::torch_build_text`. This file only words and
arranges them. Pure and cheap (a few dozen short strings), so the view rebuilds it every frame.
*/

use ms_config::torch_choice::GfxArch;
use ms_installer::torch::{DriverNote, TorchPreflightResult, rocm_gfx_is_experimental};
use ms_local_ai::backend_health::{AiBackendHealthSnapshot, TorchBuildError, TorchBuildInfo};
use ms_local_ai::large_models::format_gib;
use ms_local_ai::local_ai::{LocalAiAvailability, LocalAiOff, LocalAiOffReason};
use ms_log::runtime_log;
use ms_native_runtime::NativeFallbackReason;
use ms_onnx_runtime::builds;
use ms_settings_ui::native_builds::{NativeBuildAvailability, native_fallback_reason_text};
use ms_settings_ui::torch_build_text::{torch_build_kernel_warning, torch_build_summary, torch_build_unreadable_text};
use ms_sysprobe::system_facts::{
    CpuFacts, DeviceFacts, GpuAdapterFacts, GpuEnumeration, GpuFacts, GpuKind, GpuVendor, OsFacts, SystemFacts,
};

/// Separator of the parts of one display line (OS line, adapter detail line), like the other
/// launcher and settings summaries.
const PART_SEPARATOR: &str = " · ";

/// Everything the background probe of the tab learned. Built only by the worker.
#[derive(Debug, Clone)]
pub(super) struct ProbeResult {
    /// OS, device, CPU, RAM and graphics adapters.
    pub(super) facts: SystemFacts,
    /// The native ONNX runtime's verdicts; `None` when the runtime is off for this process
    /// (it was not probed).
    pub(super) native: Option<NativeProbe>,
    /// The PyTorch build the installer would offer; `None` when the Python backend is off for
    /// this process (no preflight ran).
    pub(super) torch: Option<TorchOffer>,
}

/// The native ONNX runtime's view of this machine.
#[derive(Debug, Clone)]
pub(super) struct NativeProbe {
    /// Every catalog build judged by the runtime's rule over the probed hardware facts.
    pub(super) availability: NativeBuildAvailability,
    /// The selection the next native load would resolve from `user_config`; `None` when the
    /// config could not be read (logged by the worker).
    pub(super) selection: Option<NativeSelection>,
}

/// The effective native ONNX selection (`ms_native_runtime::evaluate_native_selection`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NativeSelection {
    /// The configured (or per-OS default) build slug.
    pub(super) requested: &'static str,
    /// The build the runtime actually loads (`cpu` after a fallback).
    pub(super) effective: &'static str,
    /// Why the requested build cannot run here; `None` when it can.
    pub(super) fallback: Option<NativeFallbackReason>,
}

/// The PyTorch build the installer's preflight would preselect on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TorchOffer {
    /// A GPU wheel set (CUDA or ROCm), the prompt's recommended option.
    Gpu {
        /// The option's label as the installer's choice shows it (already localized; names the
        /// CUDA wheel tag or the ROCm architecture).
        label: String,
        /// ROCm facts; `None` for CUDA.
        rocm: Option<RocmOffer>,
    },
    /// CPU PyTorch: the preflight skipped the GPU choice (`reason`, localized by the installer)
    /// or preselected the CPU row (`reason == None`).
    Cpu {
        /// Why no GPU wheel is offered.
        reason: Option<String>,
    },
}

/// The ROCm-specific part of a recommended ROCm offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RocmOffer {
    /// The architecture the option installs by default.
    pub(super) gfx: GfxArch,
    /// The Windows driver warning of the option.
    pub(super) driver_note: DriverNote,
}

impl TorchOffer {
    /// Reduces the preflight answer to the recommended offer: `Skip` -> CPU with its reason;
    /// `Choose` -> the option at `recommended_index`, or CPU when the index points past the
    /// options (the choice widget's CPU row).
    pub(super) fn from_preflight(result: TorchPreflightResult) -> Self {
        match result {
            TorchPreflightResult::Skip { reason } => Self::Cpu { reason: Some(reason) },
            TorchPreflightResult::Choose(prompt) => {
                let recommended = prompt.recommended_index;
                match prompt.options.into_iter().nth(recommended) {
                    Some(option) => Self::Gpu {
                        rocm: option.rocm.map(|rocm| RocmOffer { gfx: rocm.default_gfx, driver_note: rocm.driver_note }),
                        label: option.label,
                    },
                    None => Self::Cpu { reason: None },
                }
            }
        }
    }
}

/// The running backend's PyTorch build, as the pushed health snapshot reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BackendTorch {
    /// The backend is not connected (stopped, starting, or off for the process).
    NotRunning,
    /// Connected, and the backend reports that torch is not importable.
    NotInstalled,
    /// Connected, but no build report arrived yet (warming up) or the backend sent none.
    Unknown,
    /// The reported build.
    Build(TorchBuildInfo),
    /// A report the snapshot could not parse.
    Unreadable(TorchBuildError),
}

impl BackendTorch {
    /// Reads the backend's torch state out of a health snapshot (a clone; no lock held).
    pub(super) fn from_snapshot(snapshot: &AiBackendHealthSnapshot) -> Self {
        if !snapshot.connected {
            return Self::NotRunning;
        }
        match &snapshot.torch_build {
            Some(Ok(build)) => Self::Build(build.clone()),
            Some(Err(err)) => Self::Unreadable(err.clone()),
            None if snapshot.is_torch_available == Some(false) => Self::NotInstalled,
            None => Self::Unknown,
        }
    }
}

/// Colour role of a display text. The view maps it to the launcher palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    /// Regular value text.
    Main,
    /// Secondary text (adapter details, explanations).
    Muted,
    /// Something the user should act on (driver below minimum, missing kernels).
    Warning,
}

/// One line of a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReportItem {
    /// A "label: value" row.
    Row {
        /// Row caption.
        label: String,
        /// Row value.
        value: String,
        /// Value colour.
        tone: Tone,
    },
    /// A bold block title (one graphics adapter).
    Heading(String),
    /// An indented line under the previous row or heading.
    Detail {
        /// The text.
        text: String,
        /// Its colour.
        tone: Tone,
    },
    /// A full-width line on its own ("no adapters found", local AI off).
    Note {
        /// The text.
        text: String,
        /// Its colour.
        tone: Tone,
    },
}

/// One titled card of the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReportCard {
    /// Card title.
    pub(super) title: String,
    /// Card lines, top to bottom.
    pub(super) items: Vec<ReportItem>,
}

/// Builds the tab's cards: System, Graphics (omitted where the platform cannot enumerate
/// adapters), AI acceleration (omitted when neither local runtime exists on this platform).
pub(super) fn build_report(probe: &ProbeResult, backend: &BackendTorch, local_ai: &LocalAiAvailability) -> Vec<ReportCard> {
    let mut cards = vec![system_card(&probe.facts)];
    if let Some(card) = gpu_card(&probe.facts.gpus) {
        cards.push(card);
    }
    if let Some(card) = ai_card(probe, backend, local_ai) {
        cards.push(card);
    }
    cards
}

/// The plain-text form of `cards`, exactly the strings the tab shows: the card title, then
/// its rows as "label: value", headings, details indented one level deeper, cards separated
/// by a blank line. Ends with a newline.
pub(super) fn plain_text(cards: &[ReportCard]) -> String {
    let mut out = String::new();
    for (index, card) in cards.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&card.title);
        out.push('\n');
        for item in &card.items {
            match item {
                ReportItem::Row { label, value, .. } => {
                    out.push_str("  ");
                    out.push_str(label);
                    out.push_str(": ");
                    out.push_str(value);
                }
                ReportItem::Heading(text) | ReportItem::Note { text, .. } => {
                    out.push_str("  ");
                    out.push_str(text);
                }
                ReportItem::Detail { text, .. } => {
                    out.push_str("    ");
                    out.push_str(text);
                }
            }
            out.push('\n');
        }
    }
    out
}

/// A "label: value" row in the regular tone.
fn row(label: &str, value: String) -> ReportItem {
    ReportItem::Row { label: label.to_string(), value, tone: Tone::Main }
}

/// The "System" card: OS, device, chip, processor, cores, memory.
fn system_card(facts: &SystemFacts) -> ReportCard {
    let mut items = vec![row(t!("launcher.settings.system_info.os_label"), os_line(&facts.os))];
    if let Some(device) = &facts.device {
        if let Some(name) = join_present(&[device.manufacturer.as_deref(), device.model.as_deref()]) {
            items.push(row(t!("launcher.settings.system_info.device_label"), name));
        }
        if let Some(chip) = soc_name(device) {
            items.push(row(t!("launcher.settings.system_info.soc_label"), chip));
        }
    }
    items.push(row(t!("launcher.settings.system_info.cpu_label"), cpu_name(&facts.cpu, facts.device.as_ref())));
    items.push(row(t!("launcher.settings.system_info.cores_label"), cores_text(&facts.cpu)));
    let memory = match facts.memory {
        Some(memory) => tf!(
            "launcher.settings.system_info.memory_value",
            total = format_gib(memory.total_bytes),
            available = format_gib(memory.available_bytes)
        ),
        None => t!("launcher.settings.not_determined_neuter").to_string(),
    };
    items.push(row(t!("launcher.settings.system_info.memory_label"), memory));
    ReportCard { title: t!("launcher.settings.system_info.system_card_title").to_string(), items }
}

/// The OS line: name and marketing version, then the API level (Android) or the build (only
/// where the OS reports no kernel release, i.e. Windows), the kernel (only where the OS has no
/// marketing version of its own, i.e. Linux distributions), and the architecture.
fn os_line(os: &OsFacts) -> String {
    let mut head = match &os.version {
        Some(version) => format!("{} {version}", os.name),
        None => os.name.clone(),
    };
    let suffix = match (os.sdk_level, &os.build) {
        (Some(level), _) => Some(tf!("launcher.settings.system_info.os_api_suffix", level = level)),
        (None, Some(build)) if os.kernel.is_none() => Some(tf!("launcher.settings.system_info.os_build_suffix", build = build)),
        (None, _) => None,
    };
    if let Some(suffix) = suffix {
        head.push(' ');
        head.push_str(&suffix);
    }
    let mut parts = vec![head];
    if os.version.is_none()
        && let Some(kernel) = &os.kernel
    {
        parts.push(tf!("launcher.settings.system_info.os_kernel_part", kernel = kernel));
    }
    parts.push(os.arch.to_string());
    parts.join(PART_SEPARATOR)
}

/// The SoC maker and model joined by a space; `None` when neither is known.
fn soc_name(device: &DeviceFacts) -> Option<String> {
    join_present(&[device.soc_manufacturer.as_deref(), device.soc_model.as_deref()])
}

/// The processor brand string, else the SoC (Android), else "unknown".
fn cpu_name(cpu: &CpuFacts, device: Option<&DeviceFacts>) -> String {
    cpu.name
        .clone()
        .or_else(|| device.and_then(soc_name))
        .unwrap_or_else(|| t!("launcher.settings.system_info.cpu_unknown_value").to_string())
}

/// "8 physical cores, 16 threads"; either half alone when only it is known.
fn cores_text(cpu: &CpuFacts) -> String {
    let physical = cpu.physical_cores.map(|count| tp!("launcher.settings.system_info.physical_cores_value", count));
    let threads = cpu.logical_cores.map(|count| tp!("launcher.settings.system_info.threads_value", count));
    match (physical, threads) {
        (Some(physical), Some(threads)) => tf!("launcher.settings.system_info.cores_value", physical = physical, threads = threads),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => t!("launcher.settings.not_determined_neuter").to_string(),
    }
}

/// Joins the non-empty parts with a space; `None` when nothing is left.
fn join_present(parts: &[Option<&str>]) -> Option<String> {
    let present: Vec<&str> = parts.iter().flatten().map(|part| part.trim()).filter(|part| !part.is_empty()).collect();
    (!present.is_empty()).then(|| present.join(" "))
}

/// The "Graphics" card; `None` where the platform hides its adapters (the list would mean
/// "unknown", not "none").
fn gpu_card(gpus: &GpuFacts) -> Option<ReportCard> {
    if gpus.enumeration == GpuEnumeration::Unsupported {
        return None;
    }
    let mut items = Vec::new();
    if gpus.adapters.is_empty() {
        items.push(ReportItem::Note { text: t!("launcher.settings.system_info.gpu_none_status").to_string(), tone: Tone::Muted });
    }
    for adapter in &gpus.adapters {
        items.push(ReportItem::Heading(adapter_name(adapter)));
        let details = adapter_details(adapter);
        if !details.is_empty() {
            items.push(ReportItem::Detail { text: details.join(PART_SEPARATOR), tone: Tone::Muted });
        }
    }
    if let Some(cuda) = gpus.cuda_driver_version {
        items.push(row(t!("launcher.settings.system_info.cuda_driver_label"), cuda.to_string()));
    }
    Some(ReportCard { title: t!("launcher.settings.system_info.gpu_card_title").to_string(), items })
}

/// Vendor brand name; `None` for a vendor without one here. Brand names are not translated.
fn vendor_brand(vendor: GpuVendor) -> Option<&'static str> {
    match vendor {
        GpuVendor::Amd => Some("AMD"),
        GpuVendor::Nvidia => Some("NVIDIA"),
        GpuVendor::Intel => Some("Intel"),
        GpuVendor::Apple => Some("Apple"),
        GpuVendor::Other { .. } => None,
    }
}

/// The adapter's product name, else "<Vendor> GPU (vvvv:dddd)" (generic name for an unknown
/// vendor; the PCI id only when known).
fn adapter_name(adapter: &GpuAdapterFacts) -> String {
    if let Some(name) = adapter.name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
        return name.to_string();
    }
    let base = match vendor_brand(adapter.vendor) {
        Some(vendor) => tf!("launcher.settings.system_info.gpu_vendor_unnamed_label", vendor = vendor),
        None => t!("launcher.settings.system_info.gpu_unnamed_label").to_string(),
    };
    match adapter.pci_id {
        // PCI ids are printed the way `lspci -nn` and Device Manager show them.
        Some((vendor_id, device_id)) => format!("{base} ({vendor_id:04x}:{device_id:04x})"),
        None => base,
    }
}

/// The detail parts of an adapter, in display order: kind, VRAM, AMD gfx, NVIDIA SM, Apple
/// GPU cores, driver (kernel driver name and/or version).
fn adapter_details(adapter: &GpuAdapterFacts) -> Vec<String> {
    let mut parts = Vec::new();
    match adapter.kind {
        GpuKind::Discrete => parts.push(t!("launcher.settings.system_info.gpu_discrete_value").to_string()),
        GpuKind::Integrated => parts.push(t!("launcher.settings.system_info.gpu_integrated_value").to_string()),
        GpuKind::Unknown => {}
    }
    if let Some(bytes) = adapter.vram_bytes {
        parts.push(tf!("launcher.settings.system_info.gpu_vram_part", size = format_gib(bytes)));
    }
    if let Some(gfx) = &adapter.amd_gfx {
        // LLVM processor names are identifiers, never translated.
        parts.push(gfx.as_str().to_string());
    }
    if let Some(capability) = adapter.nvidia_compute_capability {
        parts.push(tf!("launcher.settings.system_info.gpu_compute_capability_part", version = capability));
    }
    if let Some(cores) = adapter.gpu_cores {
        parts.push(tp!("launcher.settings.system_info.gpu_cores_part", cores));
    }
    if let Some(driver) = join_present(&[adapter.kernel_driver.as_deref(), adapter.driver_version.as_deref()]) {
        parts.push(tf!("launcher.settings.system_info.gpu_driver_part", driver = driver));
    }
    parts
}

/// Whether a runtime is switched off by the `--no-ai` flag (not by the platform).
fn off_by_flag(state: Result<(), LocalAiOff>) -> bool {
    matches!(state, Err(LocalAiOff { reason: LocalAiOffReason::DisabledByFlag, .. }))
}

/// The "AI acceleration" card. A runtime the platform lacks is left out; one switched off by
/// `--no-ai` is one line saying so (the runtime's own refusal text); when both runtimes are off
/// by the flag, the card is a single line. `None` when the platform has neither runtime.
fn ai_card(probe: &ProbeResult, backend: &BackendTorch, local_ai: &LocalAiAvailability) -> Option<ReportCard> {
    let native_listed = local_ai.native_onnx.is_ok() || off_by_flag(local_ai.native_onnx);
    let backend_listed = local_ai.backend.is_ok() || off_by_flag(local_ai.backend);
    if !native_listed && !backend_listed {
        return None;
    }
    let title = t!("launcher.settings.system_info.ai_card_title").to_string();
    if off_by_flag(local_ai.native_onnx) && off_by_flag(local_ai.backend) {
        let text = t!("launcher.settings.system_info.local_ai_off_status").to_string();
        return Some(ReportCard { title, items: vec![ReportItem::Note { text, tone: Tone::Muted }] });
    }
    let mut items = Vec::new();
    match local_ai.native_onnx {
        Ok(()) => native_items(probe.native.as_ref(), &mut items),
        Err(off) if off.reason == LocalAiOffReason::DisabledByFlag => {
            items.push(ReportItem::Note { text: off.user_message(), tone: Tone::Muted });
        }
        Err(_) => {}
    }
    match local_ai.backend {
        Ok(()) => {
            torch_offer_items(probe.torch.as_ref(), &mut items);
            backend_items(backend, &mut items);
        }
        Err(off) if off.reason == LocalAiOffReason::DisabledByFlag => {
            items.push(ReportItem::Note { text: off.user_message(), tone: Tone::Muted });
        }
        Err(_) => {}
    }
    Some(ReportCard { title, items })
}

/// The catalog display label of a build slug (the slug itself for an unknown one).
fn build_label(slug: &str) -> &str {
    builds::build_by_slug(slug).map_or(slug, |build| build.display_label())
}

/// The native ONNX rows: the effective build (with the fallback, if any) and the builds that
/// run here.
fn native_items(native: Option<&NativeProbe>, items: &mut Vec<ReportItem>) {
    let label = t!("launcher.settings.system_info.native_label");
    let Some(native) = native else {
        items.push(row(label, t!("launcher.settings.not_determined_neuter").to_string()));
        return;
    };
    let (value, tone) = match native.selection {
        None => (t!("launcher.settings.not_determined_neuter").to_string(), Tone::Main),
        Some(NativeSelection { effective, fallback: None, .. }) => (build_label(effective).to_string(), Tone::Main),
        Some(NativeSelection { requested, effective, fallback: Some(reason) }) if requested != effective => (
            tf!(
                "launcher.settings.system_info.native_fallback_value",
                build = build_label(effective),
                requested = build_label(requested),
                reason = native_fallback_reason_text(reason)
            ),
            Tone::Main,
        ),
        // The CPU build itself cannot run (no archive for this OS/arch): nothing to fall back to.
        Some(NativeSelection { effective, fallback: Some(reason), .. }) => (
            tf!(
                "launcher.settings.system_info.native_unavailable_value",
                build = build_label(effective),
                reason = native_fallback_reason_text(reason)
            ),
            Tone::Warning,
        ),
    };
    items.push(ReportItem::Row { label: label.to_string(), value, tone });
    let available: Vec<&str> = native.availability.statuses().iter().filter(|status| status.available()).map(|status| build_label(status.slug)).collect();
    let available = if available.is_empty() {
        t!("launcher.settings.system_info.native_none_available_value").to_string()
    } else {
        available.join(", ")
    };
    items.push(row(t!("launcher.settings.system_info.native_available_label"), available));
}

/// The PyTorch offer row, with the ROCm driver warning or the CPU reason under it.
fn torch_offer_items(offer: Option<&TorchOffer>, items: &mut Vec<ReportItem>) {
    let label = t!("launcher.settings.system_info.torch_offer_label");
    match offer {
        None => items.push(row(label, t!("launcher.settings.not_determined_neuter").to_string())),
        Some(TorchOffer::Gpu { label: option, rocm }) => {
            let value = match rocm {
                Some(rocm) if rocm_gfx_is_experimental(&rocm.gfx) => {
                    tf!("launcher.settings.system_info.torch_offer_experimental_value", option = option)
                }
                Some(_) | None => option.clone(),
            };
            items.push(row(label, value));
            if let Some(note) = rocm.as_ref().and_then(|rocm| rocm.driver_note.message()) {
                items.push(ReportItem::Detail { text: note, tone: Tone::Warning });
            }
        }
        Some(TorchOffer::Cpu { reason }) => {
            items.push(row(label, t!("ai_backend.torch_build_flavour_cpu_label").to_string()));
            if let Some(reason) = reason {
                items.push(ReportItem::Detail { text: reason.clone(), tone: Tone::Muted });
            }
        }
    }
}

/// The row of the PyTorch build the running backend reports, with the missing-kernel warning.
fn backend_items(backend: &BackendTorch, items: &mut Vec<ReportItem>) {
    let label = t!("launcher.settings.system_info.backend_torch_label").to_string();
    let (value, tone) = match backend {
        BackendTorch::NotRunning => (t!("launcher.settings.system_info.backend_not_running_value").to_string(), Tone::Muted),
        BackendTorch::NotInstalled => (t!("launcher.settings.not_installed").to_string(), Tone::Main),
        BackendTorch::Unknown => (t!("launcher.settings.not_determined_neuter").to_string(), Tone::Main),
        BackendTorch::Build(build) => (torch_build_summary(build), Tone::Main),
        BackendTorch::Unreadable(err) => (torch_build_unreadable_text(err), Tone::Warning),
    };
    items.push(ReportItem::Row { label, value, tone });
    if let BackendTorch::Build(build) = backend
        && let Some(warning) = torch_build_kernel_warning(build)
    {
        items.push(ReportItem::Detail { text: warning, tone: Tone::Warning });
    }
}

/// Logs the finished probe once (the facts module logs its own sources).
pub(super) fn log_probe_summary(probe: &ProbeResult) {
    runtime_log::log_info(format!(
        "[launcher-system-info] probe finished: {} GPU adapter(s), native probed: {}, torch offer: {:?}",
        probe.facts.gpus.adapters.len(),
        probe.native.is_some(),
        probe.torch
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_local_ai::local_ai::LocalRuntime;
    use ms_native_runtime::NativeHardwareFacts;
    use ms_platform::{PlatformFeature, Unavailable, UnavailableReason};
    use ms_sysprobe::gpu_utils::RuntimeVersion;
    use std::sync::MutexGuard;

    /// Holds the process-global locale lock: assertions compare `t!` text.
    fn locale_guard() -> MutexGuard<'static, ()> {
        ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn linux_os() -> OsFacts {
        OsFacts {
            name: "Ubuntu 26.04 LTS".to_string(),
            version: None,
            build: None,
            sdk_level: None,
            kernel: Some("7.0.0".to_string()),
            arch: "x86_64",
        }
    }

    fn adapter(vendor: GpuVendor, kind: GpuKind) -> GpuAdapterFacts {
        GpuAdapterFacts {
            vendor,
            name: None,
            pci_id: None,
            kind,
            vram_bytes: None,
            amd_gfx: None,
            nvidia_compute_capability: None,
            driver_version: None,
            kernel_driver: None,
            gpu_cores: None,
        }
    }

    fn facts(gpus: GpuFacts) -> SystemFacts {
        SystemFacts {
            os: linux_os(),
            device: None,
            cpu: CpuFacts { name: Some("AMD Ryzen 9 7950X".to_string()), physical_cores: Some(16), logical_cores: Some(32) },
            memory: None,
            gpus,
        }
    }

    fn probe(gpus: GpuFacts) -> ProbeResult {
        ProbeResult { facts: facts(gpus), native: None, torch: None }
    }

    fn supported(adapters: Vec<GpuAdapterFacts>) -> GpuFacts {
        GpuFacts { enumeration: GpuEnumeration::Supported, adapters, cuda_driver_version: None }
    }

    fn local_ai(no_ai: bool, backend_on_platform: bool) -> LocalAiAvailability {
        let backend = if backend_on_platform {
            Ok(())
        } else {
            Err(Unavailable::new(PlatformFeature::PythonBackend, UnavailableReason::NotOnThisPlatform))
        };
        LocalAiAvailability::resolve(no_ai, backend, Ok(()), Ok(()))
    }

    fn titles(cards: &[ReportCard]) -> Vec<&str> {
        cards.iter().map(|card| card.title.as_str()).collect()
    }

    /// The OS line per platform shape: Linux shows the kernel, Windows the build, Android the
    /// API level, macOS neither build nor kernel.
    #[test]
    fn os_line_follows_the_platform_shape() {
        let _guard = locale_guard();
        let kernel = tf!("launcher.settings.system_info.os_kernel_part", kernel = "7.0.0");
        assert_eq!(os_line(&linux_os()), format!("Ubuntu 26.04 LTS · {kernel} · x86_64"));

        let windows = OsFacts {
            name: "Windows 11 Pro".to_string(),
            version: Some("24H2".to_string()),
            build: Some("26100.4061".to_string()),
            sdk_level: None,
            kernel: None,
            arch: "x86_64",
        };
        let build = tf!("launcher.settings.system_info.os_build_suffix", build = "26100.4061");
        assert_eq!(os_line(&windows), format!("Windows 11 Pro 24H2 {build} · x86_64"));

        let macos = OsFacts {
            name: "macOS".to_string(),
            version: Some("15.3".to_string()),
            build: Some("24D60".to_string()),
            sdk_level: None,
            kernel: Some("24.3.0".to_string()),
            arch: "aarch64",
        };
        assert_eq!(os_line(&macos), "macOS 15.3 · aarch64");

        let android = OsFacts {
            name: "Android".to_string(),
            version: Some("15".to_string()),
            build: None,
            sdk_level: Some(35),
            kernel: Some("6.1.75".to_string()),
            arch: "aarch64",
        };
        let api = tf!("launcher.settings.system_info.os_api_suffix", level = 35);
        assert_eq!(os_line(&android), format!("Android 15 {api} · aarch64"));
    }

    /// The processor falls back to the SoC, then to "unknown"; cores show whichever half is
    /// known.
    #[test]
    fn cpu_and_cores_fall_back_in_order() {
        let _guard = locale_guard();
        let device = DeviceFacts {
            manufacturer: Some("samsung".to_string()),
            model: Some("SM-S921B".to_string()),
            soc_manufacturer: Some("Qualcomm".to_string()),
            soc_model: Some("SM8650".to_string()),
        };
        let nameless = CpuFacts { name: None, physical_cores: None, logical_cores: Some(8) };
        assert_eq!(cpu_name(&nameless, Some(&device)), "Qualcomm SM8650");
        assert_eq!(cpu_name(&nameless, None), t!("launcher.settings.system_info.cpu_unknown_value"));
        assert_eq!(cores_text(&nameless), tp!("launcher.settings.system_info.threads_value", 8));
        let unknown = CpuFacts { name: None, physical_cores: None, logical_cores: None };
        assert_eq!(cores_text(&unknown), t!("launcher.settings.not_determined_neuter"));
    }

    /// An unnamed adapter shows its vendor and PCI id; details carry kind, VRAM, gfx, SM and
    /// the driver in that order; an unknown kind adds no part.
    #[test]
    fn adapter_name_and_details() {
        let _guard = locale_guard();
        let mut amd = adapter(GpuVendor::Amd, GpuKind::Discrete);
        amd.pci_id = Some((0x1002, 0x7550));
        amd.vram_bytes = Some(16 << 30);
        amd.amd_gfx = GfxArch::parse("gfx1201");
        amd.kernel_driver = Some("amdgpu".to_string());
        let vendor = tf!("launcher.settings.system_info.gpu_vendor_unnamed_label", vendor = "AMD");
        assert_eq!(adapter_name(&amd), format!("{vendor} (1002:7550)"));
        assert_eq!(
            adapter_details(&amd),
            vec![
                t!("launcher.settings.system_info.gpu_discrete_value").to_string(),
                tf!("launcher.settings.system_info.gpu_vram_part", size = "16.0"),
                "gfx1201".to_string(),
                tf!("launcher.settings.system_info.gpu_driver_part", driver = "amdgpu"),
            ]
        );

        let mut nvidia = adapter(GpuVendor::Nvidia, GpuKind::Discrete);
        nvidia.name = Some("NVIDIA GeForce RTX 4090".to_string());
        nvidia.nvidia_compute_capability = Some(RuntimeVersion::new(8, 9));
        assert_eq!(adapter_name(&nvidia), "NVIDIA GeForce RTX 4090");
        assert!(adapter_details(&nvidia).contains(&tf!("launcher.settings.system_info.gpu_compute_capability_part", version = "8.9")));

        let other = adapter(GpuVendor::Other { vendor_id: None }, GpuKind::Unknown);
        assert_eq!(adapter_name(&other), t!("launcher.settings.system_info.gpu_unnamed_label"));
        assert!(adapter_details(&other).is_empty());
    }

    /// Graphics card: hidden when enumeration is unsupported, a "none found" note for an empty
    /// supported list, and the CUDA driver row after the adapters.
    #[test]
    fn gpu_card_visibility_and_cuda_row() {
        let _guard = locale_guard();
        let unsupported = GpuFacts { enumeration: GpuEnumeration::Unsupported, adapters: Vec::new(), cuda_driver_version: None };
        assert!(gpu_card(&unsupported).is_none());

        let empty = gpu_card(&supported(Vec::new())).expect("supported list is shown");
        assert_eq!(
            empty.items,
            vec![ReportItem::Note { text: t!("launcher.settings.system_info.gpu_none_status").to_string(), tone: Tone::Muted }]
        );

        let mut with_cuda = supported(vec![adapter(GpuVendor::Nvidia, GpuKind::Discrete)]);
        with_cuda.cuda_driver_version = Some(RuntimeVersion::new(12, 6));
        let card = gpu_card(&with_cuda).expect("shown");
        assert_eq!(card.items.last(), Some(&row(t!("launcher.settings.system_info.cuda_driver_label"), "12.6".to_string())));
    }

    /// AI card gating: platform-absent runtimes are left out; `--no-ai` collapses the card to
    /// one line; Android-like (no backend) shows only the native rows.
    #[test]
    fn ai_card_follows_local_ai_availability() {
        let _guard = locale_guard();
        let mut probe = probe(supported(Vec::new()));
        probe.native = Some(NativeProbe { availability: NativeBuildAvailability::evaluate(&NativeHardwareFacts::default()), selection: None });
        let backend = BackendTorch::NotRunning;

        let cards = build_report(&probe, &backend, &local_ai(true, true));
        let ai = cards.last().expect("ai card");
        assert_eq!(
            ai.items,
            vec![ReportItem::Note { text: t!("launcher.settings.system_info.local_ai_off_status").to_string(), tone: Tone::Muted }]
        );

        let cards = build_report(&probe, &backend, &local_ai(false, false));
        let ai = cards.last().expect("ai card");
        let labels: Vec<&str> = ai
            .items
            .iter()
            .filter_map(|item| match item {
                ReportItem::Row { label, .. } => Some(label.as_str()),
                ReportItem::Heading(_) | ReportItem::Detail { .. } | ReportItem::Note { .. } => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![t!("launcher.settings.system_info.native_label"), t!("launcher.settings.system_info.native_available_label")]
        );

        // --no-ai where the backend does not exist: the native runtime's own flag message only.
        let cards = build_report(&probe, &backend, &local_ai(true, false));
        let native_off = LocalAiOff { runtime: LocalRuntime::NativeOnnx, reason: LocalAiOffReason::DisabledByFlag };
        let ai = cards.last().expect("ai card");
        assert_eq!(ai.items, vec![ReportItem::Note { text: native_off.user_message(), tone: Tone::Muted }]);

        let full = build_report(&probe, &backend, &local_ai(false, true));
        assert_eq!(
            titles(&full),
            vec![
                t!("launcher.settings.system_info.system_card_title"),
                t!("launcher.settings.system_info.gpu_card_title"),
                t!("launcher.settings.system_info.ai_card_title")
            ]
        );
    }

    /// The native row names the fallback; the PyTorch offer marks experimental gfx10 and
    /// carries the driver note; a CPU skip shows its reason.
    #[test]
    fn native_and_torch_rows() {
        let _guard = locale_guard();
        let mut items = Vec::new();
        let native = NativeProbe {
            availability: NativeBuildAvailability::evaluate(&NativeHardwareFacts::default()),
            selection: Some(NativeSelection { requested: "cuda13", effective: "cpu", fallback: Some(NativeFallbackReason::CudaRuntimeMissing) }),
        };
        native_items(Some(&native), &mut items);
        let expected = tf!(
            "launcher.settings.system_info.native_fallback_value",
            build = build_label("cpu"),
            requested = build_label("cuda13"),
            reason = native_fallback_reason_text(NativeFallbackReason::CudaRuntimeMissing)
        );
        assert_eq!(items.first(), Some(&row(t!("launcher.settings.system_info.native_label"), expected)));

        let mut items = Vec::new();
        let offer = TorchOffer::Gpu {
            label: "AMD ROCm gfx1030".to_string(),
            rocm: Some(RocmOffer { gfx: GfxArch::parse("gfx1030").expect("valid gfx"), driver_note: DriverNote::Requirement }),
        };
        torch_offer_items(Some(&offer), &mut items);
        assert_eq!(
            items,
            vec![
                row(
                    t!("launcher.settings.system_info.torch_offer_label"),
                    tf!("launcher.settings.system_info.torch_offer_experimental_value", option = "AMD ROCm gfx1030")
                ),
                ReportItem::Detail { text: DriverNote::Requirement.message().expect("requirement note"), tone: Tone::Warning },
            ]
        );

        let mut items = Vec::new();
        torch_offer_items(Some(&TorchOffer::Cpu { reason: Some("no GPU".to_string()) }), &mut items);
        assert_eq!(items.get(1), Some(&ReportItem::Detail { text: "no GPU".to_string(), tone: Tone::Muted }));
    }

    /// The plain text lists every card line in order with the documented indentation.
    #[test]
    fn plain_text_layout() {
        let cards = vec![
            ReportCard {
                title: "A".to_string(),
                items: vec![
                    ReportItem::Row { label: "L".to_string(), value: "V".to_string(), tone: Tone::Main },
                    ReportItem::Heading("H".to_string()),
                    ReportItem::Detail { text: "D".to_string(), tone: Tone::Muted },
                ],
            },
            ReportCard { title: "B".to_string(), items: vec![ReportItem::Note { text: "N".to_string(), tone: Tone::Warning }] },
        ];
        assert_eq!(plain_text(&cards), "A\n  L: V\n  H\n    D\n\nB\n  N\n");
    }

    /// The whole report renders without panicking for this machine's live facts.
    #[test]
    fn live_facts_render() {
        let probe = ProbeResult { facts: ms_sysprobe::system_facts::probe_system_facts(), native: None, torch: None };
        let text = plain_text(&build_report(&probe, &BackendTorch::NotRunning, &local_ai(false, true)));
        assert!(!text.is_empty());
    }
}
