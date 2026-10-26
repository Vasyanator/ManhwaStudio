/*
File: cleaning/tools/ai_editor/engines/flux3_klein/ui/mod.rs

Purpose:
The parameter panel of the FLUX.3 klein engine: the panel BODY and its order, and the root of
the `ui` module whose siblings draw the folded sections.

Main responsibilities:
- own `Flux3PanelCtx` (everything the panel reads or mutates for exactly one frame) and
  `Flux3PanelIntents` (what the controls ask the engine to do, folded after the frame);
- draw the body in its ORDER: the settings-file notice, the prompt, «Референсы», steps and
  seed, the readiness line, then the sibling folds «Установка модели» (`install.rs`),
  «Память и скорость» (the shared `large_model_resources` widget with the format rows and
  «Энкодер на CPU») and «Для экспертов» (`advanced.rs`), the mask note, and last the shared
  «Постобработка» section (`shared::draw_postprocess_section`, bottom-docked).

Key structures:
- `Flux3PanelCtx`, `Flux3PanelIntents`

Key functions:
- `Flux3PanelCtx::draw()` and the per-block methods

Notes:
FLUX.2 klein's rule: what is touched per edit (prompt, references, steps, seed) is never folded;
what is set once per machine always is. The panel decides nothing a test would assert — the
lines come from `../decisions.rs`, the state belongs to `../engine/`. No literals: every text is
a `cleaning.tools.flux3_klein.*` key; localized collapsibles carry stable salts.
*/

use super::*;

// «Установка модели»: source mode, package folder or download block, residency, unload (an
// inherent `impl Flux3PanelCtx` block, so nothing to import).
mod install;
// «Для экспертов».
mod advanced;
use advanced::*;

/// What the panel's controls asked for this frame, folded by `Flux3KleinEngine::apply_intents`.
#[derive(Debug, Default)]
pub(super) struct Flux3PanelIntents {
    /// A persisted value changed (a save is owed, the forecast re-armed).
    pub(super) settings_changed: bool,
    /// Only a «Постобработка» value changed: a save is owed, the forecast and `.status` are
    /// not re-asked (`Flux3KleinEngine::note_postprocess_changed`).
    pub(super) postprocess_changed: bool,
    /// «Обновить»: re-ask `.status`.
    pub(super) want_status: bool,
    /// «Обновить»: the forecast's machine may have changed outside the app.
    pub(super) want_forecast: bool,
    /// What the user did in the memory widget.
    pub(super) resources_actions: Vec<ResourcesAction>,
    /// «Выгрузить из памяти».
    pub(super) unload: bool,
    /// A file dialog to open.
    pub(super) picker: Option<Flux3PickerPurpose>,
    /// A reference to drop (its index).
    pub(super) remove_reference: Option<usize>,
    /// «Убрать все».
    pub(super) clear_references: bool,
    pub(super) hf_token_action: Option<HfTokenAction>,
    pub(super) download_action: Option<HfDownloadAction>,
}

/// Everything the panel may read or mutate, borrowed for one frame.
pub(super) struct Flux3PanelCtx<'a> {
    pub(super) settings: &'a mut Flux3KleinSettings,
    /// The seed field's text (blank = random).
    pub(super) seed_input: &'a mut String,
    /// The seed the last finished run used.
    pub(super) last_seed: Option<u64>,
    /// The last `.status` answer (raw: the residency line is about the machine, not the root).
    pub(super) status: Option<&'a Flux3Status>,
    pub(super) status_error: Option<&'a str>,
    /// THE model verdict, already guarded against a stale answer.
    pub(super) readiness: Flux3ModelReadiness,
    pub(super) resources: ResourcesView<'a>,
    pub(super) references: &'a Flux3References,
    /// Picked files are being read.
    pub(super) references_busy: bool,
    pub(super) references_errors: &'a [String],
    pub(super) settings_problem: Option<&'a SettingsProblem>,
    pub(super) save_error: Option<&'a str>,
    pub(super) unload_status: Option<&'a str>,
    pub(super) hf_token_state: ms_sysprobe::hf_token::HfTokenState,
    pub(super) hf_token_input: &'a mut String,
    pub(super) hf_token_status: Option<&'a str>,
    pub(super) hf_token_busy: bool,
    /// The last access check, already filtered to the selected format.
    pub(super) download_check: Option<&'a Flux3DownloadCheck>,
    pub(super) download_check_error: Option<&'a str>,
    pub(super) download_check_busy: bool,
    pub(super) download_busy: bool,
    pub(super) download_status: Option<&'a str>,
    pub(super) ai_backend_available: bool,
    /// A run or a download holds the backend and the bar.
    pub(super) pipeline_busy: bool,
    pub(super) install_section_seeded: &'a mut bool,
    pub(super) install_section_open_prev: &'a mut Option<bool>,
    pub(super) intents: &'a mut Flux3PanelIntents,
}

impl Flux3PanelCtx<'_> {
    /// Draws the scrolled body in its designed order (see the file header).
    pub(super) fn draw(&mut self, ui: &mut egui::Ui) {
        self.draw_settings_problem(ui);
        self.draw_prompt(ui);
        self.draw_references(ui);
        self.draw_steps_and_seed(ui);
        let install_requested = self.draw_readiness(ui);
        self.draw_install_section(ui, install_requested);
        self.draw_memory_section(ui);
        draw_advanced_section(ui, self.settings, &mut self.intents.settings_changed);
        ui.separator();
        ui.small(t!("cleaning.tools.flux3_klein.mask_hint"));
        // LAST: the shared section docks at the bottom of the body's free room. A change is a
        // save like any other edit (but no forecast question); the host re-derives a pending
        // result from it every frame.
        self.intents.postprocess_changed |= draw_postprocess_section(ui, "cleaning_flux3_klein", &mut self.settings.postprocess);
    }

    /// A failed write (retried), a damaged file moved aside, or a file that blocks saving.
    fn draw_settings_problem(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = self.save_error {
            ui.colored_label(ms_theme::status::ERROR, error);
        }
        match self.settings_problem {
            None => {}
            Some(SettingsProblem::MovedAside(path)) => {
                ui.colored_label(ms_theme::status::WARNING, tf!("cleaning.tools.flux3_klein.settings_malformed_status", path = path.display()));
            }
            Some(SettingsProblem::SaveBlocked(reason)) => {
                ui.colored_label(ms_theme::status::ERROR, tf!("cleaning.tools.flux3_klein.settings_save_blocked_error", err = reason));
            }
        }
    }

    /// The prompt field: one field in any language (the encoder is multilingual).
    fn draw_prompt(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label(t!("cleaning.tools.flux3_klein.prompt_label")).on_hover_text(t!("cleaning.tools.flux3_klein.prompt_tooltip"));
        self.intents.settings_changed |= ui
            .add(
                egui::TextEdit::multiline(&mut self.settings.prompt)
                    .id_salt("cleaning_flux3_klein_prompt")
                    .hint_text(t!("cleaning.tools.flux3_klein.prompt_hint"))
                    .desired_rows(3),
            )
            .changed();
    }

    /// «Референсы»: the extra images (name, size, remove), «Добавить» / «Убрать все», the count
    /// out of the maximum, and why the last picked files were not all added.
    fn draw_references(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(tf!("cleaning.tools.flux3_klein.references_heading", count = self.references.items.len(), max = FLUX3_MAX_EXTRA_REFERENCES))
                .on_hover_text(t!("cleaning.tools.flux3_klein.references_tooltip"));
            if gated_button(
                ui,
                self.references.free_slots() > 0 && !self.references_busy,
                t!("cleaning.tools.flux3_klein.references_add_button"),
                t!("cleaning.tools.flux3_klein.references_add_tooltip"),
                t!("cleaning.tools.flux3_klein.references_add_disabled_tooltip"),
            ) {
                self.intents.picker = Some(Flux3PickerPurpose::References);
            }
            if !self.references.items.is_empty()
                && ui.small_button(t!("cleaning.tools.flux3_klein.references_clear_button")).on_hover_text(t!("cleaning.tools.flux3_klein.references_clear_tooltip")).clicked()
            {
                self.intents.clear_references = true;
            }
            if self.references_busy {
                ui.spinner();
            }
        });
        for (index, reference) in self.references.items.iter().enumerate() {
            ui.horizontal(|ui| {
                // The glyph is not prose; its meaning is on the hover.
                if ui.small_button("✖").on_hover_text(t!("cleaning.tools.flux3_klein.references_remove_tooltip")).clicked() {
                    self.intents.remove_reference = Some(index);
                }
                // A file name and a size are data, not prose.
                ui.small(format!("{}. {} — {}×{}", index + 1, reference.name, reference.size[0], reference.size[1]));
            });
        }
        for error in self.references_errors {
            ui.colored_label(ms_theme::status::ERROR, error);
        }
    }

    /// The steps and the seed — touched between two runs of the same prompt, so never folded.
    fn draw_steps_and_seed(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        self.intents.settings_changed |= ui
            .add(WheelSlider::new(&mut self.settings.steps, FLUX3_STEPS_MIN..=FLUX3_STEPS_MAX).text(t!("cleaning.common.steps_label")))
            .on_hover_text(t!("cleaning.tools.flux3_klein.steps_hint"))
            .changed();
        ui.horizontal(|ui| {
            ui.label(t!("cleaning.tools.flux3_klein.seed_label")).on_hover_text(t!("cleaning.tools.flux3_klein.seed_hint"));
            let edit = ui.add(
                egui::TextEdit::singleline(self.seed_input)
                    .id_salt("cleaning_flux3_klein_seed")
                    .hint_text(t!("cleaning.tools.flux3_klein.seed_random_hint"))
                    .desired_width(180.0),
            );
            if edit.on_hover_text(t!("cleaning.tools.flux3_klein.seed_hint")).changed()
                && let Some(seed) = parse_seed_input(self.seed_input).seed()
                && seed != self.settings.seed
            {
                self.settings.seed = seed;
                self.intents.settings_changed = true;
            }
        });
        if parse_seed_input(self.seed_input) == Flux3SeedInput::Invalid {
            ui.colored_label(ms_theme::status::ERROR, t!("cleaning.tools.flux3_klein.seed_invalid_error"));
        }
        if let Some(last) = self.last_seed {
            ui.horizontal_wrapped(|ui| {
                ui.small(tf!("cleaning.tools.flux3_klein.last_seed_status", seed = last));
                if self.settings.seed != Some(last)
                    && ui.small_button(t!("cleaning.tools.flux3_klein.reuse_seed_button")).on_hover_text(t!("cleaning.tools.flux3_klein.reuse_seed_tooltip")).clicked()
                {
                    *self.seed_input = last.to_string();
                    self.settings.seed = Some(last);
                    self.intents.settings_changed = true;
                }
            });
        }
    }

    /// The ONE line about whether the model is installed, with «Установить» while something
    /// is missing and «Обновить» always. Returns whether «Установить» was pressed.
    fn draw_readiness(&mut self, ui: &mut egui::Ui) -> bool {
        ui.separator();
        let memory = match self.readiness {
            Flux3ModelReadiness::Ready => self
                .resources
                .profiles
                .and_then(|profiles| forecast_line(profiles).map(|line| Flux3MemorySummary { line, fits: profiles.selected.fits })),
            Flux3ModelReadiness::Missing(_) | Flux3ModelReadiness::NoRoot | Flux3ModelReadiness::Unknown => None,
        };
        let line = flux3_readiness_line(&self.readiness, memory);
        let mut install_requested = false;
        ui.horizontal_wrapped(|ui| {
            match line.tone() {
                Flux3LineTone::Ok => {
                    ui.colored_label(ms_theme::status::SUCCESS, line.text());
                }
                Flux3LineTone::Warn => {
                    ui.colored_label(ms_theme::status::WARNING, line.text());
                }
                Flux3LineTone::Neutral => {
                    ui.small(line.text());
                }
            }
            if line.offers_install()
                && ui.small_button(t!("cleaning.tools.flux3_klein.install_button")).on_hover_text(t!("cleaning.tools.flux3_klein.install_hint")).clicked()
            {
                install_requested = true;
            }
            if ui.small_button(t!("cleaning.tools.flux3_klein.refresh_state_button")).on_hover_text(t!("cleaning.tools.flux3_klein.refresh_state_hint")).clicked() {
                self.intents.want_status = true;
                self.intents.want_forecast = true;
            }
        });
        if let Some(error) = self.status_error {
            ui.colored_label(ms_theme::status::ERROR, tf!("cleaning.tools.flux3_klein.status_error", err = error));
        }
        install_requested
    }

    /// «Память и скорость»: the shared two-axis widget with the weight-format rows and
    /// «Энкодер на CPU». Folded by default; the forecast is on the readiness line already.
    fn draw_memory_section(&mut self, ui: &mut egui::Ui) {
        let settings = &mut *self.settings;
        let intents = &mut *self.intents;
        let resources = &mut self.resources;
        RegionEditToolBase::draw_region_editor_collapsible_section(ui, "cleaning_flux3_klein_memory", t!("cleaning.tools.flux3_klein.memory_heading"), false, |ui| {
            let mut choice = settings.step_choice();
            let actions = draw_large_model_resources(
                ui,
                "cleaning_flux3_klein_resources",
                &mut choice,
                Some(&mut settings.format),
                Some(&mut settings.encoder_on_cpu),
                resources.reborrow(),
            );
            if actions.contains(&ResourcesAction::StepsChanged) {
                intents.settings_changed |= settings.set_step_choice(choice);
            }
            intents.resources_actions.extend(actions);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key this panel and its decisions spell (and the shared «Постобработка» section's)
    /// exists in BOTH reference catalogs (en and ru; es / fr / pt fall back to en) with the
    /// placeholders the code fills, and the two texts of a placeholder key agree on them. A `t!` call cannot check this: a unit test
    /// runs with no catalog loaded.
    #[test]
    fn the_reference_catalogs_carry_every_flux3_key() {
        let mut keys: Vec<(String, &[&str])> = FLUX3_PLAIN_KEYS.iter().map(|key| (format!("cleaning.tools.flux3_klein.{key}"), &[][..])).collect();
        keys.extend(FLUX3_PLACEHOLDER_KEYS.iter().map(|(key, placeholders)| (format!("cleaning.tools.flux3_klein.{key}"), *placeholders)));
        keys.extend(FLUX3_STAGES.iter().map(|stage| (format!("cleaning.tools.flux3_klein.stage_{stage}"), &[][..])));
        keys.extend(POSTPROCESS_KEYS.iter().map(|key| ((*key).to_string(), &[][..])));
        for (tag, source) in ms_i18n::embedded_locales().iter().filter(|(tag, _)| matches!(*tag, "en" | "ru")) {
            let catalog: Value = serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for (key, placeholders) in &keys {
                let text = catalog.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("locale `{tag}` lacks `{key}`"));
                assert!(!text.trim().is_empty(), "locale `{tag}`: `{key}` is empty");
                for placeholder in *placeholders {
                    assert!(text.contains(placeholder), "locale `{tag}`: `{key}` must carry `{placeholder}`");
                }
            }
        }
    }
}
