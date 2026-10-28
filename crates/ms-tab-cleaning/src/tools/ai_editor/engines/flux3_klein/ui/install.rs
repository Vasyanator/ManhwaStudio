/*
File: cleaning/tools/ai_editor/engines/flux3_klein/ui/install.rs

Purpose:
«Установка модели» of the FLUX.3 klein panel: the two weight-format pickers («Формат весов» of
the DiT, «Веса текстового энкодера»; each row names its catalog download size and carries ✓
when `.status` found its file), the source-mode switch («Скачать для меня» / «Своя папка»), the
package folder row of manual mode, the catalog download block of download mode (repository,
licence page, destination, the parts present, the Hugging Face token row while a gated spec of
the bundle is not installed, and ONE pinned bundle download over the shared
`draw_ocr_model_download`), the residency line and «Выгрузить из памяти».

On Stable-Diffusion.cpp: the DiT picker lists only the formats sd.cpp runs
(`Flux3DitFormat::offered_on`) and shows the one a run uses, with one line when the stored
fp8r / nvfp4 falls back; the catalog block fetches the sd.cpp bundle
(`flux3_klein_package_specs_for(SdCpp, …)`, via `flux3_catalog_bundle`); the backend's parts
list, residency and «Выгрузить из памяти» give way to the shared «Выгрузить модель» of the sd.cpp
worker process.

Key functions:
- `Flux3PanelCtx::draw_install_section()`, `draw_format_pickers()`, `draw_source_mode_switch()`,
  `Flux3PanelCtx::draw_download_block()`

Notes:
The section starts folded and opens ITSELF once, on the first definite non-ready verdict, unless
the user already moved the fold (FLUX.2 klein's seeding rule); «Установить» on the readiness
line opens it on the frame it is pressed. Presence comes from the last `.status` answer, never a
filesystem probe (this runs on the GUI thread). The pickers are drawn in BOTH modes: in manual
mode a GGUF file is looked up at its catalog path under the chosen folder (plan D14). The token
is never formatted or logged here.
*/

use super::*;

impl Flux3PanelCtx<'_> {
    /// Draws «Установка модели». `open_requested` is «Установить», pressed this frame above.
    pub(super) fn draw_install_section(&mut self, ui: &mut egui::Ui, open_requested: bool) {
        ui.separator();
        let section_id = ui.make_persistent_id("cleaning_flux3_klein_install_section");
        let mut section = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), section_id, false);
        // A fold that differs from what the previous frame left is the user's own click; it
        // retires the one-shot seeding for good.
        let user_moved = self.install_section_open_prev.is_some_and(|prev| prev != section.is_open());
        if !*self.install_section_seeded {
            match self.install_verdict() {
                None => {}
                Some(true) => *self.install_section_seeded = true,
                Some(false) => {
                    if !user_moved {
                        section.set_open(true);
                    }
                    *self.install_section_seeded = true;
                }
            }
            if user_moved {
                *self.install_section_seeded = true;
            }
        }
        if open_requested {
            section.set_open(true);
        }
        *self.install_section_open_prev = Some(section.is_open());
        section
            .show_header(ui, |ui| {
                ui.label(t!("cleaning.tools.flux3_klein.install_heading"));
            })
            .body(|ui| {
                draw_format_pickers(ui, self.settings, self.current_status, &mut self.intents.settings_changed);
                draw_source_mode_switch(ui, self.settings, &mut self.intents.settings_changed);
                match self.settings.source_mode() {
                    Flux3SourceMode::Manual => self.draw_model_root_row(ui),
                    Flux3SourceMode::Download => self.draw_download_block(ui),
                }
                ui.separator();
                match self.settings.runtime() {
                    ImageEditRuntime::PyTorch => self.draw_residency(ui),
                    ImageEditRuntime::SdCpp => {
                        if draw_sdcpp_unload_button(ui, self.sdcpp_status.worker_running) {
                            self.intents.sdcpp_unload = true;
                        }
                    }
                }
            });
    }

    /// The package folder of manual mode: an editable path with a folder picker, and what the
    /// last answer says is inside it.
    fn draw_model_root_row(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("cleaning.tools.flux3_klein.model_root_label")).on_hover_text(t!("cleaning.tools.flux3_klein.package_folder_hint"));
        ui.horizontal(|ui| {
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.settings.model_root)
                    .id_salt("cleaning_flux3_klein_model_root")
                    .hint_text(t!("cleaning.tools.flux3_klein.model_root_placeholder"))
                    .desired_width(ui.available_width() - 32.0),
            );
            self.intents.settings_changed |= edit.on_hover_text(t!("cleaning.tools.flux3_klein.package_folder_hint")).changed();
            // A glyph is not prose; the meaning is on the hover.
            if ui.small_button("📁").on_hover_text(t!("cleaning.tools.flux3_klein.browse_folder_tooltip")).clicked() {
                self.intents.picker = Some(Flux3PickerPurpose::ModelRoot);
            }
        });
        self.draw_package_parts(ui);
    }

    /// The three parts the selected formats need, each with its presence from the last answer
    /// about the current root; nothing is claimed without one. PyTorch only: on sd.cpp the files
    /// line of the readiness block is the answer (the backend's `.status` is not asked there).
    fn draw_package_parts(&mut self, ui: &mut egui::Ui) {
        if self.settings.runtime() == ImageEditRuntime::SdCpp {
            return;
        }
        let format = self.settings.dit_format();
        let encoder = self.settings.encoder_format();
        let Some(status) = self.current_status.filter(|_| !matches!(self.readiness, Flux3ModelReadiness::NoRoot | Flux3ModelReadiness::RootNotAbsolute)) else {
            return;
        };
        for (part, present, reason) in [
            (Flux3Part::Dit(format), status.dit_available(format), status.dit(format).map_or("", |row| row.reason.as_str())),
            (Flux3Part::Autoencoder, status.ae_present, ""),
            (Flux3Part::TextEncoder(encoder), status.encoder_available(encoder), status.encoder_reason(encoder)),
        ] {
            ui.horizontal_wrapped(|ui| {
                let (mark, color) = if present { ("✓", ms_theme::status::SUCCESS) } else { ("✗", ms_theme::status::WARNING) };
                ui.colored_label(color, mark);
                let response = ui.small(part.label());
                if !reason.is_empty() {
                    // The backend's own words: the technical half of the verdict.
                    response.on_hover_text(reason.to_string());
                }
            });
        }
    }

    /// The download mode's body: the gated repository and its licence page, where the files go,
    /// the parts present, the token row while a gated spec of the bundle is not installed, and
    /// the catalog download block of the selection's bundle.
    ///
    /// The app fetches the pinned files itself (resumable, access preflight, adoption of an
    /// existing tree by hash, verify); no backend method is involved. The block is skipped for
    /// the one frame in which the pickers have just moved the bundle away from the snapshot, so
    /// its click can only land on the bundle it shows.
    fn draw_download_block(&mut self, ui: &mut egui::Ui) {
        ui.small(t!("cleaning.tools.flux3_klein.catalog_download_hint"));
        ui.horizontal_wrapped(|ui| {
            ui.small(tf!("cleaning.tools.flux3_klein.download_repo_status", repo = FLUX3_REPO));
            ui.hyperlink_to(t!("cleaning.tools.flux3_klein.licence_link"), ms_widgets::hf_access_hint::hf_repo_url(FLUX3_REPO));
        });
        ui.label(t!("cleaning.tools.flux3_klein.download_destination_label"));
        // A path is data: selectable so it can be copied, never translated.
        ui.add(egui::Label::new(egui::RichText::new(self.settings.effective_root()).small()).selectable(true));
        self.draw_package_parts(ui);
        // The token is needed only to fetch from the gated BFL repository; once its specs of the
        // bundle are installed the row would ask for nothing. The per-repository access lines and
        // their links are the bundle block's own, below.
        if !self.gated_repos.is_empty() {
            let hint = HfAccessHint::new("cleaning_flux3_klein_hf_access_hint", token_save_button_label()).repo(t!("cleaning.tools.flux3_klein.title"), FLUX3_REPO);
            draw_hf_token_row(
                ui,
                HfTokenView { state: self.hf_token_state, input: &mut *self.hf_token_input, status: self.hf_token_status, busy: self.hf_token_busy },
                "cleaning_flux3_klein_hf_token",
                Some(hint),
                &mut self.intents.hf_token_action,
            );
        }
        let current = bundle_key(&flux3_catalog_bundle(self.settings));
        if let Some(snapshot) = self.catalog_snapshot.filter(|snapshot| snapshot.bundle_key == current) {
            let clicked = draw_ocr_model_download(ui, snapshot);
            if clicked != ModelDownloadAction::None {
                self.intents.catalog_action = clicked;
            }
        }
    }

    /// Where the model sits in memory right now, and the button that frees it.
    fn draw_residency(&mut self, ui: &mut egui::Ui) {
        if let Some(status) = self.status {
            let place = |value: Option<&String>| value.map_or_else(|| t!("cleaning.tools.flux3_klein.residency_none").to_string(), Clone::clone);
            if status.loaded || status.dit_device.is_some() || status.encoder_resident.is_some() {
                ui.small(tf!(
                    "cleaning.tools.flux3_klein.residency_status",
                    dit = place(status.dit_device.as_ref()),
                    encoder = place(status.encoder_resident.as_ref())
                ));
            } else {
                ui.small(t!("cleaning.tools.flux3_klein.residency_unloaded_status"));
            }
        }
        if gated_button(
            ui,
            self.ai_backend_available && !self.pipeline_busy,
            t!("cleaning.tools.flux3_klein.unload_button"),
            t!("cleaning.tools.flux3_klein.unload_hint"),
            t!("cleaning.tools.flux3_klein.unload_disabled_tooltip"),
        ) {
            self.intents.unload = true;
        }
        if let Some(text) = self.unload_status {
            ui.small(text);
        }
    }
}

/// The two weight-format pickers: «Формат весов» (the DiT; the memory widget's format rows
/// change the same setting from the forecast's side) and «Веса текстового энкодера». What the
/// run loads AND what the catalog bundle fetches. Each row is captioned by
/// `flux3_dit_format_row` / `flux3_encoder_format_row` (name, catalog size, ✓ from `status`, the
/// answer about the CURRENT root) and hovered by its format hint.
///
/// The DiT rows are the selected runtime's (`Flux3DitFormat::offered_on`) and the closed combo
/// shows the format a run USES (`run_dit_format`); on sd.cpp a stored format it cannot run stays
/// stored (it returns with PyTorch) and one line under the picker says which format runs
/// instead (`flux3_sdcpp_format_fallback_line`). Picking a row stores it.
///
/// `WheelComboBox` with a fixed `id_salt` each, never `egui::ComboBox` and never an id from the
/// localized caption (`egui-docs/04-widgets.md` §0.2, `05-ids-and-i18n.md` §2).
fn draw_format_pickers(ui: &mut egui::Ui, settings: &mut Flux3KleinSettings, status: Option<&Flux3Status>, changed: &mut bool) {
    let used = settings.run_dit_format();
    let mut format = used;
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("cleaning.tools.flux3_klein.format_label")).on_hover_text(t!("cleaning.tools.flux3_klein.dit_format_picker_hint"));
        WheelComboBox::from_id_salt("cleaning_flux3_klein_format")
            .selected_text(flux3_dit_format_row(format, status))
            .show_ui(ui, |ui| {
                for &candidate in Flux3DitFormat::offered_on(settings.runtime()) {
                    ui.selectable_value(&mut format, candidate, flux3_dit_format_row(candidate, status)).on_hover_text(flux3_dit_format_hint(candidate));
                }
            })
            .response
            .on_hover_text(flux3_dit_format_hint(format));
    });
    // Only a real pick is stored: showing the fallback must not overwrite the stored format.
    if format != used {
        settings.format = format.wire().to_string();
        *changed = true;
    }
    if let Some(line) = flux3_sdcpp_format_fallback_line(settings) {
        ui.small(line);
    }
    let mut encoder = settings.encoder_format();
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("cleaning.tools.flux3_klein.text_encoder_format_label")).on_hover_text(t!("cleaning.tools.flux3_klein.text_encoder_format_hint"));
        WheelComboBox::from_id_salt("cleaning_flux3_klein_text_encoder_format")
            .selected_text(flux3_encoder_format_row(encoder, status))
            .show_ui(ui, |ui| {
                for &candidate in Flux3EncoderFormat::all() {
                    ui.selectable_value(&mut encoder, candidate, flux3_encoder_format_row(candidate, status)).on_hover_text(flux3_encoder_format_hint(candidate));
                }
            })
            .response
            .on_hover_text(flux3_encoder_format_hint(encoder));
    });
    if encoder.wire() != settings.text_encoder_format {
        settings.text_encoder_format = encoder.wire().to_string();
        *changed = true;
    }
}

/// The source-mode switch: two toggle buttons (FLUX.2 klein's idiom), each under a stable salt
/// — the mode's token — so a language switch cannot change which button egui sees.
fn draw_source_mode_switch(ui: &mut egui::Ui, settings: &mut Flux3KleinSettings, changed: &mut bool) {
    let active = settings.source_mode();
    let mut picked = None;
    ui.label(t!("cleaning.tools.flux3_klein.source_mode_label"));
    ui.horizontal_wrapped(|ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        for mode in Flux3SourceMode::all() {
            let response = ui.push_id(mode.wire(), |ui| ui.add(egui::Button::new(mode.label()).selected(mode == active))).inner;
            if response.on_hover_text(mode.hint()).clicked() {
                picked = Some(mode);
            }
        }
    });
    if let Some(mode) = picked
        && mode != active
    {
        settings.source_mode = mode.wire().to_string();
        *changed = true;
    }
}
