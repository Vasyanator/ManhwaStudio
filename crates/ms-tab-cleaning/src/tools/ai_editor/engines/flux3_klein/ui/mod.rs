/*
File: cleaning/tools/ai_editor/engines/flux3_klein/ui/mod.rs

Purpose:
The parameter panel of the FLUX.3 klein engine: the panel BODY and its order, and the root of
the `ui` module whose siblings draw the folded sections.

Main responsibilities:
- own `Flux3PanelCtx` (everything the panel reads or mutates for exactly one frame) and
  `Flux3PanelIntents` (what the controls ask the engine to do, folded after the frame);
- draw the body in its ORDER: the settings-file notice, the runtime switch (PyTorch /
  Stable-Diffusion.cpp, the shared `draw_runtime_switch`; closed with the reason while a job of
  either runtime runs), the prompt block (the field, the
  preset name with «Кэшировать» / «Переименовать», the one cache line — the `vl_refs` line in
  its place in that mode — the «Библиотека промптов» toggle and fold, all the shared
  `../../shared/prompt_cache_ui.rs` block fed with FLUX.3's context; on sd.cpp the field
  alone), «Референсы», steps and seed, the readiness line (on sd.cpp: the shared runtime line,
  then the files line), then the sibling folds «Установка модели» (`install.rs`),
  «Память и скорость» (the shared `large_model_resources` widget with the DiT format rows and
  «Энкодер на CPU», then the shared text-encoder format rows; on sd.cpp replaced by the shared
  «Stable-Diffusion.cpp» options) and «Для экспертов»
  (`advanced.rs`), the mask note, and last the shared
  «Постобработка» section (`shared::draw_postprocess_section`, bottom-docked).
- hide, never fake, what sd.cpp does not have: the prompt library, the encoder's VL mode, the AE
  dtype, the fp8 path, the memory widget and the backend's `.unload`.

Key structures:
- `Flux3PanelCtx`, `Flux3PanelIntents`

Key functions:
- `Flux3PanelCtx::draw()` and the per-block methods
- `flux3_apply_encoder_pick()`: stores a picked encoder row (known wire ids only)

Notes:
FLUX.2 klein's rule: what is touched per edit (prompt, references, steps, seed) is never folded;
what is set once per machine always is. The panel decides nothing a test would assert — the
lines come from `../decisions.rs` and the shared `prompt_cache_gates` / `prompt_cache_line`, the
state belongs to `../engine/`. No literals: every text is a `cleaning.tools.flux3_klein.*` or a
shared `cleaning.tools.prompt_library.*` key; localized collapsibles carry stable salts.
*/

use super::*;
use crate::tools::ai_editor::large_model_resources::draw_encoder_formats;

// «Установка модели»: the two weight-format pickers, source mode, package folder or the catalog
// download block, residency, unload (an inherent `impl Flux3PanelCtx` block, so nothing to
// import).
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
    /// Only the «Нет» stash (`uncached_prompt`) moved: a save is owed, nothing else.
    pub(super) uncached_changed: bool,
    /// «Обновить»: re-ask `.status`.
    pub(super) want_status: bool,
    /// «Обновить»: the forecast's machine may have changed outside the app.
    pub(super) want_forecast: bool,
    /// What the user did in the memory widget.
    pub(super) resources_actions: Vec<ResourcesAction>,
    /// «Выгрузить из памяти» (the backend's `.unload`, PyTorch).
    pub(super) unload: bool,
    /// «Выгрузить модель» of the sd.cpp worker process (`spawn_sdcpp_unload`).
    pub(super) sdcpp_unload: bool,
    /// A file dialog to open.
    pub(super) picker: Option<Flux3PickerPurpose>,
    /// A reference to drop (its index).
    pub(super) remove_reference: Option<usize>,
    /// «Убрать все».
    pub(super) clear_references: bool,
    pub(super) hf_token_action: Option<HfTokenAction>,
    /// The catalog download block's click (download / verify / cancel).
    pub(super) catalog_action: ModelDownloadAction,
    /// The one prompt-library control pressed this frame («Кэшировать», «Переименовать»,
    /// «Экспорт», «Импорт»).
    pub(super) prompt_cache_action: Option<PromptCacheAction>,
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
    /// The same answer only while it describes the current root (presence marks, format rows).
    pub(super) current_status: Option<&'a Flux3Status>,
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
    /// The gated repositories of the current bundle not known to be installed
    /// (`catalog_gated_repos_missing`); the token row is drawn while it is non-empty.
    pub(super) gated_repos: &'a [&'static str],
    /// The catalog block's state for the bundle the selection needed when the frame began
    /// (download mode, `Flux3KleinEngine::catalog_snapshot`); `None` otherwise or in a test build.
    pub(super) catalog_snapshot: Option<&'a OcrModelPanelSnapshot>,
    /// The catalog bundle is known to be incomplete: the readiness line says so.
    pub(super) bundle_missing: bool,
    pub(super) ai_backend_available: bool,
    /// A run or a prompt-cache operation holds the backend and the bar
    /// (`Flux3KleinEngine::pipeline_busy`): every prompt-library control waits for it.
    pub(super) pipeline_busy: bool,
    /// The model's prompt library (the shared controller): the listing, the name field, the
    /// outcome lines and the session-only «Библиотека промптов» fold flag.
    pub(super) prompt_cache: &'a mut PromptCacheController,
    /// Three-state `.status` answer for the prompt in the field (`Some(true)` served from the
    /// cache, `Some(false)` not, `None` not known), guarded to the prompt it was asked about.
    pub(super) prompt_cache_state: Option<bool>,
    /// The entry `.status` named for that prompt (same guard): the encoder the line names first.
    pub(super) prompt_cache_status_entry: Option<&'a PromptLibraryEntry>,
    /// The process-global Stable-Diffusion.cpp status snapshot of this frame.
    pub(super) sdcpp_status: &'a ms_sdcpp::SdCppStatus,
    /// The `--no-ai` / platform gate of the sd.cpp route.
    pub(super) sdcpp_local: Result<(), ms_local_ai::local_ai::LocalAiOff>,
    /// The device saved for sd.cpp, as the files worker last read it (`None` = not read yet).
    pub(super) sdcpp_device: Option<&'a ms_config::SdCppDeviceChoice>,
    /// What is known about the sd.cpp files of the current selection.
    pub(super) sdcpp_files: Flux3SdcppFilesState,
    /// Why the runtime switch is closed (`Flux3KleinEngine::runtime_switch_block_reason`).
    pub(super) runtime_switch_block: Option<&'a str>,
    /// Why the PyTorch side alone is closed (PyTorch unavailable, `pytorch_unavailable_reason`).
    pub(super) pytorch_unavailable: Option<&'static str>,
    /// Whether the runtime switch is drawn (the sd.cpp route is available, or it is selected).
    pub(super) show_runtime_switch: bool,
    pub(super) install_section_seeded: &'a mut bool,
    pub(super) install_section_open_prev: &'a mut Option<bool>,
    pub(super) intents: &'a mut Flux3PanelIntents,
}

impl Flux3PanelCtx<'_> {
    /// Draws the scrolled body in its designed order (see the file header).
    pub(super) fn draw(&mut self, ui: &mut egui::Ui) {
        self.draw_settings_problem(ui);
        self.draw_runtime_row(ui);
        self.draw_prompt(ui);
        self.draw_references(ui);
        self.draw_steps_and_seed(ui);
        let install_requested = match self.settings.runtime() {
            ImageEditRuntime::PyTorch => self.draw_readiness(ui),
            ImageEditRuntime::SdCpp => self.draw_sdcpp_readiness_block(ui),
        };
        self.draw_install_section(ui, install_requested);
        match self.settings.runtime() {
            ImageEditRuntime::PyTorch => self.draw_memory_section(ui),
            ImageEditRuntime::SdCpp => {
                // The shared options widget draws no heading: the section titles it, closed by
                // default like the «Память и скорость» section it replaces.
                let options = &mut self.settings.sdcpp;
                let changed = &mut self.intents.settings_changed;
                RegionEditToolBase::draw_region_editor_collapsible_section(ui, "cleaning_flux3_klein_sdcpp", t!("cleaning.tools.sdcpp.options_heading"), false, |ui| {
                    *changed |= draw_sdcpp_options(ui, options, "cleaning_flux3_klein_sdcpp_options");
                });
            }
        }
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

    /// The PyTorch / Stable-Diffusion.cpp switch at the top of the body (the shared widget under
    /// a fixed salt), closed with the reason while a job of either runtime runs. A change is a
    /// settings change: saved, and the engine re-probes the bundle the new runtime needs.
    fn draw_runtime_row(&mut self, ui: &mut egui::Ui) {
        if !self.show_runtime_switch {
            return;
        }
        let mut runtime = self.settings.runtime();
        if draw_runtime_switch(ui, &mut runtime, self.runtime_switch_block, self.pytorch_unavailable, "cleaning_flux3_klein_runtime") {
            self.settings.runtime = runtime.wire().to_string();
            self.intents.settings_changed = true;
        }
    }

    /// Whether «Установка модели» has a definite verdict to seed its fold with: `None` = not
    /// known yet, `Some(true)` = ready, `Some(false)` = something to install or fix. PyTorch reads
    /// the `.status` verdict, sd.cpp the files state.
    pub(super) fn install_verdict(&self) -> Option<bool> {
        match self.settings.runtime() {
            ImageEditRuntime::PyTorch => match self.readiness {
                Flux3ModelReadiness::Unknown => None,
                Flux3ModelReadiness::Ready => Some(true),
                Flux3ModelReadiness::Missing(_) | Flux3ModelReadiness::NoRoot | Flux3ModelReadiness::RootNotAbsolute => Some(false),
            },
            ImageEditRuntime::SdCpp => match self.sdcpp_files {
                Flux3SdcppFilesState::Checking => None,
                Flux3SdcppFilesState::Ready => Some(true),
                Flux3SdcppFilesState::Refused(_) => Some(false),
            },
        }
    }

    /// The prompt block, FLUX.2 klein's UX: the field (one, in any language — the encoder is
    /// multilingual), the preset name with «Кэшировать» / «Переименовать», the one line about
    /// the cache, the «Библиотека промптов» toggle and its fold.
    ///
    /// The field is MATCHED against the model's library every frame: a typed text equal to a
    /// preset's (after trimming) is that preset — the name field shows its name, the line names
    /// the encoder of the entry a run will use — and a text no preset holds is kept as the «Нет»
    /// stash (`track_uncached`). An edit (typed or picked) is a settings change; the engine
    /// re-asks `.status` from it (`note_settings_changed`: `prompt_cached` is about one prompt).
    /// In `vl_refs` the cache serves nothing: «Кэшировать» is closed with that reason
    /// (`flux3_cache_context`) and the VL line takes the cache line's place; the library still
    /// picks, exports and imports texts.
    ///
    /// On Stable-Diffusion.cpp only the field is drawn: the library is the backend's.
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
        if self.settings.runtime() == ImageEditRuntime::SdCpp {
            return;
        }
        self.intents.uncached_changed |= track_uncached(&mut self.settings.uncached_prompt, self.prompt_cache.library.as_ref(), &self.settings.prompt);
        {
            let ctl = &mut *self.prompt_cache;
            let hit = ctl.library.as_ref().and_then(|listing| listing.text_for(&self.settings.prompt));
            sync_preset_name(&mut ctl.name_input, &mut ctl.name_synced_for, hit);
        }
        let text_encoder_available = self.text_encoder_available();
        let hit = self.prompt_cache.hit(&self.settings.prompt);
        // NOT the cache job alone: a run holds the same pipeline and the same progress bar.
        let gates = prompt_cache_gates(
            &flux3_cache_context(self.settings),
            hit,
            &self.prompt_cache.name_input,
            self.prompt_cache_state,
            text_encoder_available,
            self.ai_backend_available,
            self.pipeline_busy,
        );
        let line = prompt_cache_line(hit.map(|text| &text.selected), self.prompt_cache_status_entry, self.prompt_cache_state, text_encoder_available, &flux3_encoder_caption);
        if let Some(action) = draw_cache_row(ui, &mut self.prompt_cache_view(&gates)) {
            self.intents.prompt_cache_action = Some(action);
        }
        if self.settings.text_mode_in_effect() {
            draw_prompt_cache_line(ui, &line);
        } else {
            ui.small(t!("cleaning.tools.flux3_klein.prompt_cache_vl_refs_status"));
        }
        draw_prompt_library_toggle(ui, &mut self.prompt_cache.library_open);
        if self.prompt_cache.library_open {
            let output = draw_prompt_cache_library(ui, &mut self.prompt_cache_view(&gates));
            if output.prompt_changed {
                self.intents.settings_changed = true;
            }
            if let Some(action) = output.action {
                self.intents.prompt_cache_action = Some(action);
            }
        }
    }

    /// The shared prompt-cache block's borrow for one draw call: the controller, the prompt
    /// field and the «Нет» stash, with FLUX.3's encoder caption and its two stable id salts
    /// (literals, never localized: the name field's text cursor and the picker's popup live
    /// under them). FLUX.3 has no notes of its own (no legacy migration).
    fn prompt_cache_view<'b>(&'b mut self, gates: &'b PromptCacheGates) -> PromptCacheBlockView<'b> {
        PromptCacheBlockView {
            ctl: &mut *self.prompt_cache,
            prompt: &mut self.settings.prompt,
            uncached: &self.settings.uncached_prompt,
            gates,
            caption: &flux3_encoder_caption,
            name_id_salt: "cleaning_flux3_klein_prompt_cache_name",
            library_id_salt: "cleaning_flux3_klein_prompt_library",
            notes: &[],
        }
    }

    /// Whether the selected text encoder is on this machine, as the backend reports it: the
    /// guarded `.status` row of the selected format first (re-asked on every selection change),
    /// the library listing's own answer as the fallback while `.status` is outstanding. `None`
    /// claims nothing.
    fn text_encoder_available(&self) -> Option<bool> {
        self.current_status
            .map(|status| status.encoder_available(self.settings.encoder_format()))
            .or_else(|| self.prompt_cache.library.as_ref().and_then(|library| library.text_encoder_available))
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
            Flux3ModelReadiness::Missing(_) | Flux3ModelReadiness::NoRoot | Flux3ModelReadiness::RootNotAbsolute | Flux3ModelReadiness::Unknown => None,
        };
        let line = flux3_readiness_line(&self.readiness, self.bundle_missing, memory);
        let install_requested = self.draw_readiness_line(ui, &line);
        if let Some(error) = self.status_error {
            ui.colored_label(ms_theme::status::ERROR, tf!("cleaning.tools.flux3_klein.status_error", err = error));
        }
        install_requested
    }

    /// The readiness of the Stable-Diffusion.cpp runtime: the shared runtime line (installed,
    /// host requirement, device, version), then the ONE line about the selection's files with
    /// «Установить» while something is missing and «Обновить» always (it re-checks the files and
    /// the runtime). Returns whether «Установить» was pressed.
    fn draw_sdcpp_readiness_block(&mut self, ui: &mut egui::Ui) -> bool {
        ui.separator();
        draw_sdcpp_readiness(ui, self.sdcpp_status, self.sdcpp_local, self.sdcpp_device);
        let line = flux3_sdcpp_readiness_line(&self.sdcpp_files, self.bundle_missing);
        self.draw_readiness_line(ui, &line)
    }

    /// One readiness line in its tone, with «Установить» when it offers it and «Обновить».
    /// Returns whether «Установить» was pressed.
    fn draw_readiness_line(&mut self, ui: &mut egui::Ui, line: &Flux3ReadinessLine) -> bool {
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
        install_requested
    }

    /// «Память и скорость»: the shared two-axis widget with the DiT format rows and
    /// «Энкодер на CPU», and the text encoder's format rows right under it
    /// (`draw_encoder_formats`, FLUX.2 klein's layout; drawn only when the answer carries two
    /// rows or more). A picked encoder row is stored through [`flux3_apply_encoder_pick`] and
    /// raises `settings_changed`, which saves it and re-arms the forecast, `.status` and the
    /// catalog bundle probe (`Flux3KleinEngine::fold_panel_frame`). Folded by default; the
    /// forecast is on the readiness line already.
    fn draw_memory_section(&mut self, ui: &mut egui::Ui) {
        let settings = &mut *self.settings;
        let intents = &mut *self.intents;
        let resources = &mut self.resources;
        RegionEditToolBase::draw_region_editor_collapsible_section(ui, "cleaning_flux3_klein_memory", t!("cleaning.tools.flux3_klein.memory_heading"), false, |ui| {
            // Copied out before `reborrow` hands the view to the widget.
            let profiles = resources.profiles;
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
            if let Some(profiles) = profiles {
                // The widget marks the row of the NORMALIZED choice: a stored id the vocabulary
                // does not know reads as the default encoder everywhere else too.
                let mut encoder = settings.encoder_format().wire().to_string();
                if let Some(picked) = draw_encoder_formats(ui, profiles, &mut encoder) {
                    intents.settings_changed |= flux3_apply_encoder_pick(settings, &picked);
                }
            }
        });
    }
}

/// Stores the text-encoder format picked in the forecast's encoder rows. Returns whether the
/// setting changed (the caller raises `settings_changed`).
///
/// The row ids are the backend's echo of the candidates the params sent
/// (`Flux3EncoderFormat::wire()`); an id outside that vocabulary is a contract drift: it is
/// logged and never stored, so the settings file only ever holds a known format.
pub(super) fn flux3_apply_encoder_pick(settings: &mut Flux3KleinSettings, picked: &str) -> bool {
    let Some(format) = Flux3EncoderFormat::from_wire(picked) else {
        ms_log::runtime_log::log_warn(format!("[cleaning] {FLUX3_LOG_LABEL} forecast offered an unknown text-encoder format `{picked}`; the selection is kept"));
        return false;
    };
    if settings.text_encoder_format == format.wire() {
        return false;
    }
    settings.text_encoder_format = format.wire().to_string();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picked encoder row is stored as its wire id and reports a change once; re-picking the
    /// stored format changes nothing, and an id outside the vocabulary is never stored.
    #[test]
    fn a_picked_encoder_row_is_stored_only_when_known_and_new() {
        let mut settings = Flux3KleinSettings::default();
        assert_eq!(settings.encoder_format(), Flux3EncoderFormat::Bf16);
        assert!(flux3_apply_encoder_pick(&mut settings, Flux3EncoderFormat::Q80.wire()));
        assert_eq!(settings.encoder_format(), Flux3EncoderFormat::Q80);
        assert_eq!(settings.text_encoder_format, "q8_0");
        assert!(!flux3_apply_encoder_pick(&mut settings, "q8_0"), "the stored format again is no change");
        assert!(!flux3_apply_encoder_pick(&mut settings, "q2_k"), "an unknown id is refused");
        assert_eq!(settings.text_encoder_format, "q8_0", "and never stored");
        // A legacy, unknown stored value is replaced by any known pick (bf16 included).
        settings.text_encoder_format = "legacy".to_string();
        assert!(flux3_apply_encoder_pick(&mut settings, "bf16"));
        assert_eq!(settings.text_encoder_format, "bf16");
    }

    /// Every key this panel and its decisions spell (and the shared «Постобработка» section's)
    /// exists in BOTH reference catalogs (en and ru; es / fr / pt fall back to en) with the
    /// placeholders the code fills, and the two texts of a placeholder key agree on them. A `t!` call cannot check this: a unit test
    /// runs with no catalog loaded. The retired download keys are absent everywhere.
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
        // The retired backend download's keys are gone from EVERY shipped catalog.
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for key in FLUX3_RETIRED_KEYS {
                let key = format!("cleaning.tools.flux3_klein.{key}");
                assert!(catalog.get(&key).is_none(), "locale `{tag}` still carries the retired `{key}`");
            }
        }
    }
}
