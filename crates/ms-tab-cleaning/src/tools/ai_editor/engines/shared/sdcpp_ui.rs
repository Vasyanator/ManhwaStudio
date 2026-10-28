/*
File: cleaning/tools/ai_editor/engines/shared/sdcpp_ui.rs

Purpose:
The drawing half of a large-model engine's Stable-Diffusion.cpp runtime (FLUX.2 klein,
FLUX.3 klein): the PyTorch / Stable-Diffusion.cpp switch at the top of the AI region editor
panel, the readiness line of the sd.cpp runtime, the per-engine sd.cpp options and the
«Выгрузить модель» button. Model-agnostic.

Key items:
- `draw_runtime_switch()`: two toggle buttons, disabled with a reason while `blocked`; the
  PyTorch one also while PyTorch is unavailable (`pytorch_unavailable_reason`, strict `AiCaps`).
- `sdcpp_auto_switch()` + `log_sdcpp_auto_switch()`: the ONE rule both FLUX engines apply in
  `AiEngine::on_selected` — leave PyTorch for sd.cpp when PyTorch is out of reach (Torch known
  absent, or the backend down and not starting), on settings loaded from the engine's file.
- `SdCppReadiness` (`of`, `text`, `run_block_reason`) + `draw_sdcpp_readiness()`: no build
  for this machine, not installed, host requirement, device missing, probe failure, ready
  (native only).
- `draw_sdcpp_options()`: encoder on CPU, weights in RAM, VRAM budget («Авто» or a
  `WheelSlider`), VAE tiling, flash attention (`WheelComboBox`).
- `draw_sdcpp_unload_button()`.

Notes:
Draw only; no I/O on the GUI thread. The readiness reads the process-global
`ms_sdcpp::SdCppStatus` the caller snapshotted (`ms_sdcpp::snapshot()`, one short lock) and the
device choice the engine read on its status worker (`read_sdcpp_choice`). Every `WheelComboBox`
and button group has a stable, non-localized id salt (`egui-docs/05-ids-and-i18n.md` §2); no
stock `Slider` / `ComboBox` / `DragValue` (`egui-docs/04-widgets.md` §0.2). Texts are
`cleaning.tools.sdcpp.*`.
*/

use eframe::egui;
use ms_config::ImageEditRuntime;
use ms_log::runtime_log;
use ms_widgets::{AiCaps, AiRequirement, WheelComboBox, WheelSlider};

use super::sdcpp_options::{FlashAttentionChoice, SdCppRunOptions, VRAM_BUDGET_MAX_GIB, VRAM_BUDGET_MIN_GIB};

/// The budget a manual VRAM limit starts from when the user unticks «Авто».
const VRAM_BUDGET_INITIAL_GIB: f32 = 8.0;
/// The VRAM budget dial's step, in GiB.
const VRAM_BUDGET_STEP_GIB: f64 = 0.5;

/// The localized name of a runtime.
fn runtime_label(runtime: ImageEditRuntime) -> &'static str {
    match runtime {
        ImageEditRuntime::PyTorch => t!("cleaning.tools.sdcpp.runtime_pytorch"),
        ImageEditRuntime::SdCpp => t!("cleaning.tools.sdcpp.runtime_sdcpp"),
    }
}

/// Why the PyTorch side of the runtime switch is closed: PyTorch is not available by the
/// strict capability signal every `AiButton` uses (`AiRequirement::Torch` over `caps`), with
/// that requirement's own hover reason; `None` while it is available. Pure.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn pytorch_unavailable_reason(caps: &AiCaps) -> Option<&'static str> {
    (!AiRequirement::Torch.satisfied(caps)).then(|| AiRequirement::Torch.disabled_reason(caps))
}

/// Whether PyTorch is known to be out of reach for the automatic switch: Torch is known absent
/// (`torch == Some(false)`), or the backend is unavailable (`backend == Some(false)`) and no
/// start of it is underway (`backend_starting`). A backend still booting, or simply not answered
/// yet, is NOT out of reach: Torch users must not be flipped at every start. Pure.
#[must_use]
fn pytorch_out_of_reach(caps: &AiCaps) -> bool {
    caps.torch == Some(false) || (caps.backend == Some(false) && !caps.backend_starting)
}

/// Whether an engine that has just become the user's (`AiEngine::on_selected`, on settings that
/// were loaded from its file) must move its runtime from PyTorch to Stable-Diffusion.cpp: it is
/// on PyTorch, PyTorch is out of reach ([`pytorch_out_of_reach`] over `caps`), sd.cpp is
/// offered here (`sdcpp_route_available`) and the switch is not blocked by a job in flight.
/// sd.cpp need not be installed: its readiness line offers the install. Never on a per-frame
/// basis. Pure.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn sdcpp_auto_switch(runtime: ImageEditRuntime, caps: &AiCaps, sdcpp_offered: bool, switch_blocked: bool) -> bool {
    match runtime {
        ImageEditRuntime::PyTorch => pytorch_out_of_reach(caps) && sdcpp_offered && !switch_blocked,
        ImageEditRuntime::SdCpp => false,
    }
}

/// Logs an automatic PyTorch -> sd.cpp switch of `engine_id` (an `info` line of the session log).
pub(in crate::tools::ai_editor::engines) fn log_sdcpp_auto_switch(engine_id: &str) {
    runtime_log::log_info(format!("[cleaning] {engine_id}: PyTorch is unavailable; the runtime switched to Stable-Diffusion.cpp"));
}

/// Draws the runtime switch: a caption and one toggle button per runtime (the house idiom of
/// the engines' mode switches), each under its frozen wire id as salt inside `id_salt`, so a
/// language switch cannot change which button egui sees. While `blocked` is `Some(reason)` (a
/// run or a download in flight) both buttons are disabled and hovering says why; otherwise the
/// PyTorch button alone is disabled while `pytorch_unavailable` is `Some(reason)`
/// ([`pytorch_unavailable_reason`]) and hovering it gives that reason. The sd.cpp button stays
/// open: its readiness line offers the install. Returns whether `runtime` changed.
pub(in crate::tools::ai_editor::engines) fn draw_runtime_switch(ui: &mut egui::Ui, runtime: &mut ImageEditRuntime, blocked: Option<&str>, pytorch_unavailable: Option<&str>, id_salt: &str) -> bool {
    let active = *runtime;
    let mut picked = None;
    ui.push_id(id_salt, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            ui.label(t!("cleaning.tools.sdcpp.runtime_label"));
            for candidate in ImageEditRuntime::ALL {
                let unavailable = match candidate {
                    ImageEditRuntime::PyTorch => pytorch_unavailable,
                    ImageEditRuntime::SdCpp => None,
                };
                let enabled = blocked.is_none() && unavailable.is_none();
                let response = ui.push_id(candidate.wire(), |ui| ui.add_enabled(enabled, egui::Button::new(runtime_label(candidate)).selected(candidate == active))).inner;
                let response = match (blocked, unavailable) {
                    (Some(reason), _) => response.on_disabled_hover_text(tf!("cleaning.tools.sdcpp.runtime_switch_blocked_tooltip", reason = reason)),
                    (None, Some(reason)) => response.on_disabled_hover_text(reason),
                    (None, None) => response,
                };
                if response.clicked() {
                    picked = Some(candidate);
                }
            }
        });
    });
    match picked {
        Some(candidate) if candidate != active => {
            *runtime = candidate;
            true
        }
        Some(_) | None => false,
    }
}

/// Draws the engine's sd.cpp options, without a heading: the caller puts them in its own
/// section, titled `cleaning.tools.sdcpp.options_heading`. `id_salt` scopes every widget (two
/// engines may draw the block). Returns whether any option changed (the engine then schedules
/// its settings save).
pub(in crate::tools::ai_editor::engines) fn draw_sdcpp_options(ui: &mut egui::Ui, options: &mut SdCppRunOptions, id_salt: &str) -> bool {
    let mut changed = false;
    ui.push_id(id_salt, |ui| {
        changed |= ui.checkbox(&mut options.text_encoder_on_cpu, t!("cleaning.tools.sdcpp.te_on_cpu_checkbox")).on_hover_text(t!("cleaning.tools.sdcpp.te_on_cpu_hint")).changed();
        changed |= ui.checkbox(&mut options.weights_in_ram, t!("cleaning.tools.sdcpp.weights_in_ram_checkbox")).on_hover_text(t!("cleaning.tools.sdcpp.weights_in_ram_hint")).changed();
        changed |= draw_vram_budget(ui, &mut options.vram_budget_gib);
        changed |= ui.checkbox(&mut options.vae_tiling, t!("cleaning.tools.sdcpp.vae_tiling_checkbox")).on_hover_text(t!("cleaning.tools.sdcpp.vae_tiling_hint")).changed();
        changed |= draw_flash_attention(ui, &mut options.flash_attention);
    });
    changed
}

/// The VRAM budget row: an «Авто» checkbox (`None`) and, when it is off, a `WheelSlider` over
/// the valid range on a 0.5 GiB grid.
fn draw_vram_budget(ui: &mut egui::Ui, budget: &mut Option<f32>) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("cleaning.tools.sdcpp.vram_budget_label")).on_hover_text(t!("cleaning.tools.sdcpp.vram_budget_hint"));
        let mut auto = budget.is_none();
        if ui.checkbox(&mut auto, t!("cleaning.tools.sdcpp.vram_budget_auto_checkbox")).changed() {
            *budget = if auto { None } else { Some(VRAM_BUDGET_INITIAL_GIB) };
            changed = true;
        }
        if let Some(gib) = budget.as_mut() {
            // A hand-edited out-of-range value is shown (and saved) clamped.
            *gib = gib.clamp(VRAM_BUDGET_MIN_GIB, VRAM_BUDGET_MAX_GIB);
            changed |= ui
                .add(WheelSlider::new(gib, VRAM_BUDGET_MIN_GIB..=VRAM_BUDGET_MAX_GIB).step_by(VRAM_BUDGET_STEP_GIB).wheel_step(VRAM_BUDGET_STEP_GIB).suffix(t!("cleaning.tools.sdcpp.vram_budget_suffix")))
                .changed();
        }
    });
    changed
}

/// The flash-attention picker: a `WheelComboBox` under a fixed salt.
fn draw_flash_attention(ui: &mut egui::Ui, choice: &mut FlashAttentionChoice) -> bool {
    let mut index = FlashAttentionChoice::ALL.iter().position(|candidate| candidate == choice).unwrap_or(0);
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("cleaning.tools.sdcpp.flash_attention_label")).on_hover_text(t!("cleaning.tools.sdcpp.flash_attention_hint"));
        changed = WheelComboBox::from_id_salt("cleaning.tools.sdcpp.flash_attention_label")
            .show_index(ui, &mut index, FlashAttentionChoice::ALL.len(), |row| FlashAttentionChoice::ALL.get(row).copied().unwrap_or_default().label())
            .changed();
    });
    if let Some(&picked) = FlashAttentionChoice::ALL.get(index)
        && picked != *choice
    {
        *choice = picked;
        changed = true;
    }
    changed
}

/// The «Выгрузить модель» button: enabled while `enabled` — an sd.cpp worker runs (the status
/// snapshot's `worker_running`) and the caller has no run of its own in flight. Returns whether it was clicked; the engine then calls
/// `spawn_sdcpp_unload` (never the blocking unload on the GUI thread).
pub(in crate::tools::ai_editor::engines) fn draw_sdcpp_unload_button(ui: &mut egui::Ui, enabled: bool) -> bool {
    ui.add_enabled(enabled, egui::Button::new(t!("cleaning.tools.sdcpp.unload_button"))).on_hover_text(t!("cleaning.tools.sdcpp.unload_hint")).clicked()
}

#[cfg(not(target_arch = "wasm32"))]
pub(in crate::tools::ai_editor::engines) use readiness::*;

/// The readiness of the sd.cpp runtime (native only: it reads `ms_sdcpp` types).
#[cfg(not(target_arch = "wasm32"))]
mod readiness {
    use eframe::egui;
    use ms_config::SdCppDeviceChoice;
    use ms_local_ai::local_ai::LocalAiOff;
    use ms_sdcpp::SdCppStatus;

    /// What the region editor tells the user about the sd.cpp runtime, most fundamental
    /// problem first.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(in crate::tools::ai_editor::engines) enum SdCppReadiness {
        /// `--no-ai` or the platform switches the runtime off; the localized reason.
        Disabled(String),
        /// The status has not been refreshed yet (or a refresh is running).
        Checking,
        /// A refresh finished and the pinned release has no build for this machine (the platform
        /// feature is on, e.g. an aarch64 desktop, but the catalog has no archive for it).
        NoBuild,
        /// The selected build is not installed.
        NotInstalled,
        /// The host misses a requirement of the build; the localized problem.
        Requirement(String),
        /// The last probe of the build failed (library load, crash); the localized error. Not
        /// a run block: a run reports the same failure with its own detail.
        ProbeFailed(String),
        /// The saved device is not among the devices the last probe found; its display name.
        DeviceMissing(String),
        /// Installed and usable; the library version when a probe reported it.
        Ready(Option<String>),
    }

    impl SdCppReadiness {
        /// Derives the readiness from the status snapshot, the process gate of the sd.cpp route
        /// (`AiRouteKind::SdCpp.availability`) and the saved device (`None` = not read yet; a
        /// missing device is then not reported).
        ///
        /// "Is sd.cpp ready" itself is `SdCppStatus::ready` (its one owner, also behind the
        /// `AiButton` gate); this only names WHY it is not, and adds the engine-level probe and
        /// device facts on top of a ready runtime.
        #[must_use]
        pub(in crate::tools::ai_editor::engines) fn of(status: &SdCppStatus, local: Result<(), LocalAiOff>, device: Option<&SdCppDeviceChoice>) -> Self {
            if let Err(off) = local {
                return Self::Disabled(off.user_message());
            }
            if let Err(unavailable) = &status.platform {
                return Self::Disabled(unavailable.user_message());
            }
            match status.ready() {
                None => {
                    // A finished refresh that selected nothing found no build here; anything else
                    // is a refresh still to come or running.
                    let refreshed = status.revision > 0 && !status.refreshing;
                    return if refreshed && status.selected.is_none() { Self::NoBuild } else { Self::Checking };
                }
                Some(false) => {
                    if status.catalog_problem().is_some() {
                        return Self::NoBuild;
                    }
                    // Past the platform and the catalog, `ready` answers `Some(false)` only for a
                    // missing install or an unmet requirement; a missing install is named first.
                    return match &status.requirements {
                        Some(Err(problem)) if status.installed != Some(false) => Self::Requirement(problem.to_string()),
                        Some(_) | None => Self::NotInstalled,
                    };
                }
                Some(true) => {}
            }
            match &status.probe {
                Some(Err(err)) => Self::ProbeFailed(err.to_string()),
                Some(Ok(report)) => match device {
                    Some(SdCppDeviceChoice::Named { name, description }) if !report.devices.iter().any(|found| found.name == *name) => {
                        Self::DeviceMissing(if description.trim().is_empty() { name.clone() } else { format!("{name} — {description}") })
                    }
                    Some(SdCppDeviceChoice::Auto | SdCppDeviceChoice::Named { .. }) | None => Self::Ready(Some(report.sd_version.clone())),
                },
                None => Self::Ready(None),
            }
        }

        /// The localized line describing this state.
        #[must_use]
        pub(in crate::tools::ai_editor::engines) fn text(&self) -> String {
            match self {
                Self::Disabled(reason) => tf!("cleaning.tools.sdcpp.readiness_disabled", reason = reason),
                Self::Checking => t!("cleaning.tools.sdcpp.readiness_checking").to_string(),
                Self::NoBuild => t!("cleaning.tools.sdcpp.readiness_no_build").to_string(),
                Self::NotInstalled => t!("cleaning.tools.sdcpp.readiness_not_installed").to_string(),
                Self::Requirement(problem) => tf!("cleaning.tools.sdcpp.readiness_requirement", problem = problem),
                Self::ProbeFailed(error) => tf!("cleaning.tools.sdcpp.readiness_probe_failed", error = error),
                Self::DeviceMissing(device) => tf!("cleaning.tools.sdcpp.readiness_device_missing", device = device),
                Self::Ready(Some(version)) => tf!("cleaning.tools.sdcpp.readiness_ready", version = version),
                Self::Ready(None) => t!("cleaning.tools.sdcpp.readiness_installed").to_string(),
            }
        }

        /// Why a run on sd.cpp cannot start now (the line itself), or `None` when it may: ready,
        /// or a failed probe (the run then reports the failure in detail).
        #[must_use]
        pub(in crate::tools::ai_editor::engines) fn run_block_reason(&self) -> Option<String> {
            match self {
                Self::Disabled(_) | Self::Checking | Self::NoBuild | Self::NotInstalled | Self::Requirement(_) | Self::DeviceMissing(_) => Some(self.text()),
                Self::ProbeFailed(_) | Self::Ready(_) => None,
            }
        }

        /// Whether the line is a problem (drawn in the warning colour).
        fn is_problem(&self) -> bool {
            match self {
                Self::Disabled(_) | Self::NoBuild | Self::NotInstalled | Self::Requirement(_) | Self::ProbeFailed(_) | Self::DeviceMissing(_) => true,
                Self::Checking | Self::Ready(_) => false,
            }
        }
    }

    /// Draws the readiness line of the sd.cpp runtime and returns the readiness (the engine
    /// gates its run on [`SdCppReadiness::run_block_reason`]). A status that was never refreshed
    /// (revision 0, nothing running) asks for one refresh — a non-blocking worker spawn.
    pub(in crate::tools::ai_editor::engines) fn draw_sdcpp_readiness(ui: &mut egui::Ui, status: &SdCppStatus, local: Result<(), LocalAiOff>, device: Option<&SdCppDeviceChoice>) -> SdCppReadiness {
        if status.revision == 0 && !status.refreshing && status.platform.is_ok() && local.is_ok() {
            ms_sdcpp::request_refresh();
        }
        let readiness = SdCppReadiness::of(status, local, device);
        let text = readiness.text();
        if readiness.is_problem() {
            ui.colored_label(ms_theme::status::WARNING, text);
        } else {
            ui.small(text);
        }
        readiness
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use ms_config::SdCppDeviceChoice;
    use ms_local_ai::local_ai::LocalAiAvailability;
    use ms_sdcpp::{DeviceInfo, ProbeReport, RequirementProblem, SdCppBuild, SdCppError, SdCppStatus};

    use super::readiness::SdCppReadiness;
    use super::{pytorch_unavailable_reason, sdcpp_auto_switch};
    use ms_config::ImageEditRuntime;
    use ms_widgets::AiCaps;

    /// A capability snapshot for the auto-switch tests.
    fn caps(backend: Option<bool>, torch: Option<bool>, backend_starting: bool) -> AiCaps {
        AiCaps { backend, torch, ort: None, sdcpp: None, backend_starting }
    }

    /// The automatic PyTorch -> sd.cpp switch happens only on PyTorch, with PyTorch out of reach
    /// (Torch known absent, or the backend down and not starting), sd.cpp offered and the switch
    /// open; every other case keeps the runtime.
    #[test]
    fn auto_switch_leaves_an_unavailable_pytorch_only() {
        let py = ImageEditRuntime::PyTorch;
        // Torch known absent (the backend answered without Torch).
        assert!(sdcpp_auto_switch(py, &caps(Some(true), Some(false), false), true, false));
        // No backend at all (offline, nothing starting): the no-Python user.
        assert!(sdcpp_auto_switch(py, &caps(Some(false), None, false), true, false));
        // Backend down but a start is underway: still booting, keep PyTorch.
        assert!(!sdcpp_auto_switch(py, &caps(Some(false), None, true), true, false), "the backend is starting");
        // Not answered yet: unknown is not absent.
        assert!(!sdcpp_auto_switch(py, &caps(None, None, false), true, false), "unknown backend");
        assert!(!sdcpp_auto_switch(py, &caps(Some(true), None, false), true, false), "Torch not reported yet");
        assert!(!sdcpp_auto_switch(py, &caps(Some(true), Some(true), false), true, false), "PyTorch is available");
        assert!(!sdcpp_auto_switch(py, &caps(Some(false), None, false), false, false), "sd.cpp is not offered here");
        assert!(!sdcpp_auto_switch(py, &caps(Some(false), None, false), true, true), "a job blocks the switch");
        assert!(!sdcpp_auto_switch(ImageEditRuntime::SdCpp, &caps(Some(false), None, false), true, false), "already on sd.cpp");
    }

    /// The PyTorch side closes on the strict signal: unknown counts as unavailable.
    #[test]
    fn pytorch_side_closes_unless_torch_is_known_available() {
        let caps = |torch| AiCaps { backend: Some(true), torch, ort: None, sdcpp: None, backend_starting: false };
        assert!(pytorch_unavailable_reason(&caps(Some(true))).is_none());
        assert!(pytorch_unavailable_reason(&caps(Some(false))).is_some());
        assert!(pytorch_unavailable_reason(&caps(None)).is_some(), "unknown is not available");
    }

    /// A status of an installed, requirement-clean build with a probe listing `Vulkan0`.
    fn ready_status() -> SdCppStatus {
        SdCppStatus {
            selected: Some(SdCppBuild::Vulkan),
            platform: Ok(()),
            installed: Some(true),
            requirements: Some(Ok(())),
            probe: Some(Ok(ProbeReport { sd_version: "master-1".to_string(), sd_commit: "abc".to_string(), devices: vec![DeviceInfo { name: "Vulkan0".to_string(), description: "GPU".to_string() }], physical_cores: 8 })),
            worker_running: false,
            refreshing: false,
            revision: 3,
        }
    }

    /// The states come out in order of how fundamental the problem is, and only the blocking
    /// ones close the run.
    #[test]
    fn readiness_states_and_run_blocks() {
        let allowed = LocalAiAvailability::resolve(false, Ok(()), Ok(()), Ok(())).sdcpp;
        let no_ai = LocalAiAvailability::resolve(true, Ok(()), Ok(()), Ok(())).sdcpp;
        let ready = ready_status();
        assert_eq!(SdCppReadiness::of(&ready, allowed, None), SdCppReadiness::Ready(Some("master-1".to_string())));
        assert!(SdCppReadiness::of(&ready, allowed, None).run_block_reason().is_none());
        assert!(matches!(SdCppReadiness::of(&ready, no_ai, None), SdCppReadiness::Disabled(_)));

        let unchecked = SdCppStatus { installed: None, ..ready.clone() };
        assert_eq!(SdCppReadiness::of(&unchecked, allowed, None), SdCppReadiness::Checking);
        let never_refreshed = SdCppStatus { selected: None, installed: None, requirements: None, probe: None, revision: 0, ..ready.clone() };
        assert_eq!(SdCppReadiness::of(&never_refreshed, allowed, None), SdCppReadiness::Checking);
        let no_build = SdCppStatus { revision: 4, ..never_refreshed.clone() };
        assert_eq!(SdCppReadiness::of(&no_build, allowed, None), SdCppReadiness::NoBuild);
        assert!(SdCppReadiness::NoBuild.run_block_reason().is_some());
        let refreshing = SdCppStatus { refreshing: true, ..no_build };
        assert_eq!(SdCppReadiness::of(&refreshing, allowed, None), SdCppReadiness::Checking);
        assert!(SdCppReadiness::Checking.run_block_reason().is_some());

        let missing = SdCppStatus { installed: Some(false), ..ready.clone() };
        assert_eq!(SdCppReadiness::of(&missing, allowed, None), SdCppReadiness::NotInstalled);

        let old_glibc = SdCppStatus { requirements: Some(Err(RequirementProblem::NotGlibc)), ..ready.clone() };
        let readiness = SdCppReadiness::of(&old_glibc, allowed, None);
        assert!(matches!(readiness, SdCppReadiness::Requirement(_)));
        assert!(readiness.run_block_reason().is_some());

        let probe_failed = SdCppStatus { probe: Some(Err(SdCppError::Busy)), ..ready.clone() };
        let readiness = SdCppReadiness::of(&probe_failed, allowed, None);
        assert!(matches!(readiness, SdCppReadiness::ProbeFailed(_)));
        assert!(readiness.run_block_reason().is_none(), "a failed probe does not block: the run reports in detail");

        let unprobed = SdCppStatus { probe: None, ..ready.clone() };
        assert_eq!(SdCppReadiness::of(&unprobed, allowed, None), SdCppReadiness::Ready(None));
    }

    /// A saved device the probe did not find is reported (with its description) and blocks;
    /// a found one or «Авто» is ready.
    #[test]
    fn a_saved_device_missing_from_the_probe_blocks() {
        let allowed = LocalAiAvailability::resolve(false, Ok(()), Ok(()), Ok(())).sdcpp;
        let status = ready_status();
        let gone = SdCppDeviceChoice::Named { name: "Vulkan1".to_string(), description: "Radeon".to_string() };
        let readiness = SdCppReadiness::of(&status, allowed, Some(&gone));
        assert_eq!(readiness, SdCppReadiness::DeviceMissing("Vulkan1 — Radeon".to_string()));
        assert!(readiness.run_block_reason().is_some());
        let bare = SdCppDeviceChoice::Named { name: "Vulkan1".to_string(), description: String::new() };
        assert_eq!(SdCppReadiness::of(&status, allowed, Some(&bare)), SdCppReadiness::DeviceMissing("Vulkan1".to_string()));
        let found = SdCppDeviceChoice::Named { name: "Vulkan0".to_string(), description: "GPU".to_string() };
        assert!(matches!(SdCppReadiness::of(&status, allowed, Some(&found)), SdCppReadiness::Ready(_)));
        assert!(matches!(SdCppReadiness::of(&status, allowed, Some(&SdCppDeviceChoice::Auto)), SdCppReadiness::Ready(_)));
    }

    /// Every `cleaning.tools.sdcpp.*` key this module family spells is in `en` and `ru` with its
    /// placeholders (a `t!` cannot check that: unit tests run with no catalog loaded).
    #[test]
    fn every_reference_catalog_carries_the_sdcpp_strings() {
        let keys: &[(&str, &[&str])] = &[
            ("runtime_label", &[]),
            ("runtime_pytorch", &[]),
            ("runtime_sdcpp", &[]),
            ("runtime_switch_blocked_tooltip", &["{reason}"]),
            ("options_heading", &[]),
            ("te_on_cpu_checkbox", &[]),
            ("te_on_cpu_hint", &[]),
            ("weights_in_ram_checkbox", &[]),
            ("weights_in_ram_hint", &[]),
            ("vram_budget_label", &[]),
            ("vram_budget_hint", &[]),
            ("vram_budget_auto_checkbox", &[]),
            ("vram_budget_suffix", &[]),
            ("vae_tiling_checkbox", &[]),
            ("vae_tiling_hint", &[]),
            ("flash_attention_label", &[]),
            ("flash_attention_hint", &[]),
            ("flash_attention_auto", &[]),
            ("flash_attention_on", &[]),
            ("flash_attention_off", &[]),
            ("readiness_not_installed", &[]),
            ("readiness_requirement", &["{problem}"]),
            ("readiness_device_missing", &["{device}"]),
            ("readiness_ready", &["{version}"]),
            ("readiness_installed", &[]),
            ("readiness_disabled", &["{reason}"]),
            ("readiness_checking", &[]),
            ("readiness_no_build", &[]),
            ("readiness_probe_failed", &["{error}"]),
            ("stage_starting", &[]),
            ("stage_loading", &[]),
            ("stage_generating", &[]),
            ("unload_button", &[]),
            ("unload_hint", &[]),
            ("torch_missing_hint", &[]),
        ];
        for (tag, source) in ms_i18n::embedded_locales() {
            if !matches!(*tag, "en" | "ru") {
                continue;
            }
            let catalog: serde_json::Value = serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for (key, placeholders) in keys {
                let key = format!("cleaning.tools.sdcpp.{key}");
                let text = catalog.get(&key).and_then(serde_json::Value::as_str).unwrap_or_else(|| panic!("locale `{tag}` lacks `{key}`"));
                assert!(!text.trim().is_empty(), "locale `{tag}`: `{key}` is empty");
                for placeholder in *placeholders {
                    assert!(text.contains(placeholder), "locale `{tag}`: `{key}` must carry `{placeholder}`");
                }
            }
        }
    }
}
