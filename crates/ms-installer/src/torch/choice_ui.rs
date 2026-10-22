/*
File: torch/choice_ui.rs

Purpose:
The one PyTorch choice widget, drawn by the installer window, the update window and the
launcher settings page: the detection summary, one radio row per GPU wheel option (label,
recommended marker, download size, wheel-tag tooltip; for ROCm the architecture picker, the
"experimental" note of RDNA1/2 and the driver note), the CPU row and one confirm button.

Key items:
- `TorchChoiceState`: the prompt plus what is selected in it (option or CPU, the picked ROCm
  architecture per ROCm option). Owned by the screen across frames.
- `draw_torch_choice()`: draws the state and returns the selection when the confirm button
  is clicked.

Notes:
The state owns its prompt, so a selection index can never point into another prompt. The
widget only reads in-memory data (no I/O); the screens start the install themselves.
*/

use eframe::egui;
use ms_config::torch_choice::{GfxArch, TorchFlavour};
use ms_widgets::WheelComboBox;

use super::catalog::{rocm_download_estimate, rocm_gfx_is_experimental, rocm_option_label};
use super::{TorchChoicePrompt, TorchInstallSelection, TorchOs, TorchWheelOption, download_size_text};

/// Which row of the choice is selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChoiceSlot {
    /// The CPU row (`TorchInstallSelection::SkipCpu`).
    Cpu,
    /// The GPU option at this index of the prompt's `options`.
    Option(usize),
}

/// One entry of a ROCm option's architecture picker.
#[derive(Clone, Debug)]
struct GfxChoice {
    gfx: GfxArch,
    /// Name of the first detected adapter with this architecture, `None` when none has it.
    detected_name: Option<String>,
}

/// The architecture picker state of one ROCm option.
#[derive(Clone, Debug)]
struct RocmPick {
    /// Detected architectures first (adapter order), then the rest of the OS's device extras.
    choices: Vec<GfxChoice>,
    /// Index into `choices`; always in range (`choices` is never empty).
    selected: usize,
}

impl RocmPick {
    /// The picker of a ROCm option: its supported list with detected architectures first,
    /// preselecting `default_gfx`.
    ///
    /// `default_gfx` is in the supported list by the catalog's invariant; should it not be,
    /// it is still offered (first) so the state stays representable, and the command builder
    /// rejects it with its own error when installed.
    fn new(rocm: &super::RocmOptionInfo) -> Self {
        let detected_name = |gfx: &GfxArch| {
            rocm.detected
                .iter()
                .find(|adapter| adapter.gfx.as_ref() == Some(gfx))
                .map(|adapter| adapter.name.clone())
        };
        let supported: Vec<GfxArch> = rocm.supported.iter().filter_map(|name| GfxArch::parse(name)).collect();
        let mut choices: Vec<GfxChoice> = Vec::with_capacity(supported.len() + 1);
        for adapter in &rocm.detected {
            if let Some(gfx) = adapter.gfx.as_ref()
                && supported.contains(gfx)
                && !choices.iter().any(|choice| &choice.gfx == gfx)
            {
                choices.push(GfxChoice { gfx: gfx.clone(), detected_name: Some(adapter.name.clone()) });
            }
        }
        for gfx in supported {
            if !choices.iter().any(|choice| choice.gfx == gfx) {
                choices.push(GfxChoice { gfx, detected_name: None });
            }
        }
        let selected = match choices.iter().position(|choice| choice.gfx == rocm.default_gfx) {
            Some(index) => index,
            None => {
                choices.insert(0, GfxChoice { gfx: rocm.default_gfx.clone(), detected_name: detected_name(&rocm.default_gfx) });
                0
            }
        };
        Self { choices, selected }
    }

    /// The architecture currently picked.
    fn gfx(&self) -> &GfxArch {
        // Cannot panic: `new` sets `selected` to an index of `choices` (which always holds at
        // least the default architecture) and `TorchChoiceState::set_rocm_choice` only stores
        // range-checked indices.
        &self.choices[self.selected].gfx
    }
}

/// The PyTorch choice of one screen: the prompt and the user's current selection in it.
///
/// Create it once when the prompt arrives and keep it in the screen's state across frames
/// (a state rebuilt every frame would forget the user's pick). The recommended option is
/// preselected; with no GPU option the CPU row is.
#[derive(Clone, Debug)]
pub struct TorchChoiceState {
    prompt: TorchChoicePrompt,
    selected: ChoiceSlot,
    /// Aligned with `prompt.options`: `Some` exactly for a ROCm option.
    rocm_picks: Vec<Option<RocmPick>>,
}

impl TorchChoiceState {
    /// The state for `prompt`, preselecting its recommended option (the CPU row when the
    /// prompt has no options or its recommended index is out of range).
    #[must_use]
    pub fn new(prompt: TorchChoicePrompt) -> Self {
        let selected =
            if prompt.recommended_index < prompt.options.len() { ChoiceSlot::Option(prompt.recommended_index) } else { ChoiceSlot::Cpu };
        let rocm_picks = prompt.options.iter().map(|option| option.rocm.as_ref().map(RocmPick::new)).collect();
        Self { prompt, selected, rocm_picks }
    }

    /// The prompt this state was built for.
    #[must_use]
    pub fn prompt(&self) -> &TorchChoicePrompt {
        &self.prompt
    }

    /// What confirming now would install: `SkipCpu` for the CPU row, `InstallRocm` with the
    /// picked architecture for a ROCm option, `InstallGpu` for a CUDA option.
    #[must_use]
    pub fn selection(&self) -> TorchInstallSelection {
        let ChoiceSlot::Option(index) = self.selected else {
            return TorchInstallSelection::SkipCpu;
        };
        // `selected` only ever holds an index of `prompt.options` (set by `new` and
        // `select_option`, both range-checked).
        let Some(option) = self.prompt.options.get(index) else {
            return TorchInstallSelection::SkipCpu;
        };
        match self.rocm_picks.get(index).and_then(Option::as_ref) {
            Some(pick) => TorchInstallSelection::InstallRocm { option: option.clone(), gfx: pick.gfx().clone() },
            None => TorchInstallSelection::for_option(option.clone()),
        }
    }

    /// Selects the CPU row.
    fn select_cpu(&mut self) {
        self.selected = ChoiceSlot::Cpu;
    }

    /// Selects the GPU option at `index`; an out-of-range index is ignored.
    fn select_option(&mut self, index: usize) {
        if index < self.prompt.options.len() {
            self.selected = ChoiceSlot::Option(index);
        }
    }

    /// Picks entry `choice` of ROCm option `index`'s architecture list and selects that
    /// option; out-of-range indices and non-ROCm options are ignored.
    fn set_rocm_choice(&mut self, index: usize, choice: usize) {
        if let Some(Some(pick)) = self.rocm_picks.get_mut(index)
            && choice < pick.choices.len()
        {
            pick.selected = choice;
            self.selected = ChoiceSlot::Option(index);
        }
    }

    /// The row label of option `index`: a ROCm option is labelled with the picked
    /// architecture, any other option with its own label.
    fn option_label(&self, index: usize, option: &TorchWheelOption) -> String {
        match self.rocm_picks.get(index).and_then(Option::as_ref) {
            Some(pick) => rocm_option_label(pick.gfx()),
            None => option.label.clone(),
        }
    }

    /// The download-size text of option `index`: for ROCm the estimate of the picked
    /// architecture on this OS, otherwise the option's own.
    fn option_size_text(&self, index: usize, option: &TorchWheelOption) -> Option<String> {
        match self.rocm_picks.get(index).and_then(Option::as_ref) {
            Some(pick) => download_size_text(rocm_download_estimate(TorchOs::current(), pick.gfx()), TorchFlavour::Rocm),
            None => option.download_size_text(),
        }
    }
}

/// Draws the PyTorch choice of `state` and returns the selection on the frame the confirm
/// button (captioned `confirm_label`) is clicked, `None` otherwise.
///
/// Draws the prompt's summary, one radio row per GPU option (the recommended one marked, its
/// download size, for ROCm the architecture picker, the RDNA1/2 "experimental" note and the
/// driver note), a hint when there is no GPU option, the CPU row and the button. Cheap and
/// non-blocking: in-memory data only.
pub fn draw_torch_choice(ui: &mut egui::Ui, state: &mut TorchChoiceState, confirm_label: &str) -> Option<TorchInstallSelection> {
    ui.label(state.prompt.summary.as_str());
    ui.add_space(8.0);

    if state.prompt.options.is_empty() {
        ui.small(t!("installer.torch.no_gpu_options_hint"));
        ui.add_space(6.0);
    }

    for index in 0..state.prompt.options.len() {
        ui.push_id(("installer.torch.option_row", index), |ui| draw_option_row(ui, state, index));
        ui.add_space(6.0);
    }

    if ui.radio(state.selected == ChoiceSlot::Cpu, t!("installer.torch.cpu_option_label")).clicked() {
        state.select_cpu();
    }

    ui.add_space(12.0);
    if ui.button(confirm_label).clicked() { Some(state.selection()) } else { None }
}

/// Draws the radio row of GPU option `index` and, for a ROCm option, its picker and notes.
fn draw_option_row(ui: &mut egui::Ui, state: &mut TorchChoiceState, index: usize) {
    // Cloned so the row can read the option while `state` is mutated by the controls.
    let Some(option) = state.prompt.options.get(index).cloned() else {
        return;
    };
    let label = state.option_label(index, &option);
    let title = if index == state.prompt.recommended_index {
        tf!("installer.torch.option_recommended_marker", option = label)
    } else {
        label
    };
    ui.horizontal_wrapped(|ui| {
        let radio = ui
            .radio(state.selected == ChoiceSlot::Option(index), title)
            .on_hover_text(tf!("installer.torch.wheel_tag_tooltip", tag = option.wheel_tag));
        if radio.clicked() {
            state.select_option(index);
        }
        if let Some(size) = state.option_size_text(index, &option) {
            ui.label(egui::RichText::new(size).small().weak());
        }
    });

    let Some(rocm) = option.rocm.as_ref() else {
        return;
    };
    ui.indent("installer.torch.rocm_details", |ui| {
        let Some(Some(pick)) = state.rocm_picks.get(index) else {
            return;
        };
        let item_texts: Vec<String> = pick
            .choices
            .iter()
            .map(|choice| match &choice.detected_name {
                Some(name) => tf!("installer.torch.gfx_detected_item_label", gfx = choice.gfx, name = name),
                None => choice.gfx.to_string(),
            })
            .collect();
        let mut picked = pick.selected;
        let experimental = rocm_gfx_is_experimental(pick.gfx());
        ui.horizontal(|ui| {
            ui.label(t!("installer.torch.gfx_picker_label"));
            let response = WheelComboBox::from_id_salt("installer.torch.gfx_picker_label").show_index(
                ui,
                &mut picked,
                item_texts.len(),
                |item| item_texts.get(item).cloned().unwrap_or_default(),
            );
            if response.changed() {
                state.set_rocm_choice(index, picked);
            }
        });
        if experimental {
            ui.colored_label(ms_theme::status::WARNING, t!("installer.torch.gfx_experimental_hint"));
        }
        if let Some(note) = rocm.driver_note.message() {
            ui.colored_label(ms_theme::status::WARNING, note);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torch::catalog::{build_cuda_torch_options, build_rocm_torch_option, rocm_device_extras};
    use ms_sysprobe::amd_adapters::{AMD_PCI_VENDOR_ID, AmdAdapter, GfxSource};
    use ms_sysprobe::gpu_utils::RuntimeVersion;

    fn gfx(name: &str) -> GfxArch {
        GfxArch::parse(name).unwrap_or_else(|| panic!("{name} must parse"))
    }

    fn adapter(name: &str, gfx_name: &str) -> AmdAdapter {
        AmdAdapter {
            vendor_id: AMD_PCI_VENDOR_ID,
            device_id: 0x7550,
            revision: None,
            name: name.to_string(),
            gfx: Some(gfx(gfx_name)),
            gfx_source: Some(GfxSource::DeviceIdTable),
            integrated: Some(false),
            driver_version: None,
        }
    }

    fn rocm_option(default_gfx: &str, detected: Vec<AmdAdapter>) -> TorchWheelOption {
        build_rocm_torch_option(TorchOs::current(), gfx(default_gfx), detected, None)
    }

    fn prompt(options: Vec<TorchWheelOption>, recommended_index: usize) -> TorchChoicePrompt {
        TorchChoicePrompt { options, recommended_index, summary: String::new() }
    }

    fn rocm_gfx(selection: &TorchInstallSelection) -> Option<&str> {
        match selection {
            TorchInstallSelection::InstallRocm { gfx, .. } => Some(gfx.as_str()),
            TorchInstallSelection::SkipCpu | TorchInstallSelection::InstallGpu(_) => None,
        }
    }

    #[test]
    fn empty_prompt_preselects_cpu() {
        let state = TorchChoiceState::new(prompt(Vec::new(), 0));
        assert!(matches!(state.selection(), TorchInstallSelection::SkipCpu));
    }

    #[test]
    fn out_of_range_recommendation_preselects_cpu() {
        let state = TorchChoiceState::new(prompt(vec![rocm_option("gfx1201", Vec::new())], 3));
        assert!(matches!(state.selection(), TorchInstallSelection::SkipCpu));
    }

    #[test]
    fn recommended_cuda_option_is_preselected() {
        let mut options = build_cuda_torch_options(RuntimeVersion::new(13, 0), Some(RuntimeVersion::new(8, 9)));
        options.push(rocm_option("gfx1201", Vec::new()));
        let state = TorchChoiceState::new(prompt(options, 2));
        match state.selection() {
            TorchInstallSelection::InstallGpu(option) => assert_eq!(option.wheel_tag, "cu130"),
            other => panic!("expected the recommended CUDA option, got {other:?}"),
        }
    }

    #[test]
    fn rocm_option_installs_default_gfx_until_another_is_picked() {
        let mut state = TorchChoiceState::new(prompt(vec![rocm_option("gfx1201", vec![adapter("Radeon RX 9070", "gfx1201")])], 0));
        assert_eq!(rocm_gfx(&state.selection()), Some("gfx1201"));

        let pick = state.rocm_picks[0].as_ref().map(|pick| pick.choices.iter().position(|choice| choice.gfx.as_str() == "gfx1100"));
        let Some(Some(choice)) = pick else { panic!("gfx1100 must be offered") };
        state.select_cpu();
        state.set_rocm_choice(0, choice);
        assert_eq!(rocm_gfx(&state.selection()), Some("gfx1100"), "picking an architecture selects its option");
        assert_eq!(state.option_label(0, &state.prompt.options[0]), rocm_option_label(&gfx("gfx1100")));
        assert_eq!(
            state.option_size_text(0, &state.prompt.options[0]),
            download_size_text(rocm_download_estimate(TorchOs::current(), &gfx("gfx1100")), TorchFlavour::Rocm)
        );
    }

    #[test]
    fn picker_lists_detected_first_and_every_supported_once() {
        let detected = vec![adapter("Radeon RX 7900", "gfx1100"), adapter("Radeon 780M", "gfx1103"), adapter("Second 7900", "gfx1100")];
        let state = TorchChoiceState::new(prompt(vec![rocm_option("gfx1100", detected)], 0));
        let Some(pick) = state.rocm_picks[0].as_ref() else { panic!("ROCm option must have a picker") };
        let names: Vec<&str> = pick.choices.iter().map(|choice| choice.gfx.as_str()).collect();
        assert_eq!(&names[..2], ["gfx1100", "gfx1103"]);
        assert_eq!(pick.choices[0].detected_name.as_deref(), Some("Radeon RX 7900"));
        assert_eq!(pick.choices[2].detected_name, None);
        let supported = rocm_device_extras(TorchOs::current());
        assert_eq!(names.len(), supported.len(), "every supported architecture parses and appears once");
        assert!(supported.iter().all(|name| names.contains(name)));
        assert_eq!(pick.selected, 0);
    }

    #[test]
    fn out_of_range_inputs_are_ignored() {
        let mut state = TorchChoiceState::new(prompt(vec![rocm_option("gfx1201", Vec::new())], 0));
        state.set_rocm_choice(0, usize::MAX);
        state.set_rocm_choice(7, 0);
        state.select_option(7);
        assert_eq!(rocm_gfx(&state.selection()), Some("gfx1201"));
    }
}
