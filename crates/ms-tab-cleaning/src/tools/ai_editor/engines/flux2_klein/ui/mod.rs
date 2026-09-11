/*
File: cleaning/tools/ai_editor/engines/flux2_klein/ui/mod.rs

Purpose:
The parameter panel of the FLUX.2 klein engine. This file is the panel BODY — the one
place that decides what the panel contains and in which order — and the root of the `ui`
module, whose siblings draw the individual blocks it calls into.

Main responsibilities:
- own `Flux2PanelCtx`: everything the panel may read or mutate, borrowed for exactly one
  frame, with the intents the controls raise travelling back as plain flags;
- draw the body in its designed ORDER: progress bar, run status, the prompt block with
  its cache line and its two unfolding toggles, «Сила изменения», the readiness line, and
  the three sibling collapsible sections;
- delegate each block to the submodule that owns it.

Key structures:
- `Flux2PanelCtx`

Key functions:
- `Flux2PanelCtx::draw()` and the per-block methods it calls

Submodules:
- `install.rs`: the source-mode switch, the model paths and the whole download block.
- `components.rs`: the memory preset, the forecast and the merged component list.
- `advanced.rs`: «Для экспертов».
- `progress.rs`: the progress bars and the mask hint.

Notes:
No file under `ui/` decides anything a test would want to assert on its own: the verdicts
and the lines it renders are computed in `decisions.rs`, and the state it mutates belongs
to the engine. The tests kept here are catalog tests — they check that every locale
carries the keys and the interpolation placeholders this panel spells.
*/

use super::*;

// The source-mode switch, the model paths and the whole download block.
mod install;
use install::*;
// The memory preset, the forecast and the merged component list.
mod components;
use components::*;
// «Для экспертов»: the parameters set once per machine or per page.
mod advanced;
use advanced::*;
// The progress bars and the mask hint the body opens with.
mod progress;
use progress::*;

// ---------------------------------------------------------------------------------------
// Parameter panel body
// ---------------------------------------------------------------------------------------

/// Everything the parameter panel may read or mutate, borrowed for exactly one frame.
///
/// It exists so the panel can be split into small methods instead of one function with a
/// dozen locals, and so the intents the controls raise (re-query the catalog, open a
/// picker, start a translation) travel back to the engine as plain flags rather than as
/// work done from inside a widget closure.
pub(super) struct Flux2PanelCtx<'a> {
    /// Which checkpoint this panel belongs to. It decides the two honest differences
    /// between the two engines' bodies — whether the uncensored-encoder toggle exists at
    /// all, and whether a Hugging Face token is needed to download the model — and names
    /// the repository the download block reports on.
    pub(super) variant: Flux2Variant,
    pub(super) settings: &'a mut Flux2KleinSettings,
    pub(super) status: Option<&'a Flux2Status>,
    pub(super) status_error: Option<&'a str>,
    pub(super) estimate: Option<&'a Flux2Estimate>,
    pub(super) estimate_error: Option<&'a str>,
    pub(super) unload_status: &'a mut Option<String>,
    /// The engine's line about the last per-component action, drawn under the block.
    pub(super) component_action_status: Option<&'a str>,
    /// One of the three long operations holds the backend's pipeline and the shared
    /// progress bar ([`flux2_pipeline_busy`]). Everything that means "wait for the current
    /// operation to finish" is gated on THIS and not on one receiver: a generation closes
    /// the prompt-cache controls for exactly the same reason a cache build does.
    pub(super) pipeline_busy: bool,
    pub(super) translate_status: Option<&'a str>,
    pub(super) translate_busy: bool,
    pub(super) estimate_busy: bool,
    /// Three-state answer for the prompt currently in the field: `Some(true)` cached,
    /// `Some(false)` not cached, `None` not known yet.
    pub(super) prompt_cache_state: Option<bool>,
    pub(super) prompt_cache_status: Option<&'a str>,
    pub(super) prompt_cache_warning: Option<&'a str>,
    pub(super) prompt_cache_library: Option<&'a Flux2PromptCacheList>,
    pub(super) prompt_cache_list_error: Option<&'a str>,
    pub(super) prompt_cache_list_busy: bool,
    pub(super) prompt_cache_busy: bool,
    pub(super) prompt_cache_selected: &'a mut Option<String>,
    pub(super) prompt_cache_name_input: &'a mut String,
    /// Session flag of the «Библиотека промптов» toggle — see
    /// [`Flux2KleinEngine::prompt_library_open`]. The toggle writes it directly, which is
    /// why it is a `&mut bool` and not one more intent field.
    pub(super) prompt_library_open: &'a mut bool,
    /// Session flag of «Установка модели» — see [`Flux2KleinEngine::install_section_seeded`].
    pub(super) install_section_seeded: &'a mut bool,
    /// The fold state that section was left in last frame — see
    /// [`Flux2KleinEngine::install_section_open_prev`]. Written back at the end of the
    /// frame, which is what tells the user's own clicks from this file's `set_open`.
    pub(super) install_section_open_prev: &'a mut Option<bool>,
    /// THE model verdict for this frame, already guarded against a stale catalog
    /// ([`Flux2KleinEngine::model_readiness`]). Handed in rather than derived here so the
    /// readiness line and the run gate cannot answer the question differently.
    pub(super) readiness: Flux2ModelReadiness,
    /// Tri-state of the OS secret store: not read yet / no token / a token is stored.
    pub(super) hf_token_state: ms_sysprobe::hf_token::HfTokenState,
    /// The token field's buffer. Drawn masked and never persisted.
    pub(super) hf_token_input: &'a mut String,
    pub(super) hf_token_status: Option<&'a str>,
    /// A secret-store save or delete is in flight; both buttons are closed meanwhile.
    pub(super) hf_token_busy: bool,
    pub(super) download_check: Option<&'a Flux2DownloadCheck>,
    pub(super) download_check_error: Option<&'a str>,
    pub(super) download_check_busy: bool,
    /// A `.download.start` is in flight. Separate from `pipeline_busy` because this is
    /// what decides whether the block offers «Скачать» or «Отмена».
    pub(super) download_busy: bool,
    pub(super) download_status: Option<&'a str>,
    pub(super) progress: &'a Arc<Mutex<Flux2Progress>>,
    /// The engine's own line about the last run, drawn under the progress bar.
    pub(super) run_status: Option<&'a str>,
    pub(super) ai_backend_available: bool,
    /// Set when a control changed a persisted value.
    pub(super) settings_changed: &'a mut bool,
    /// Set when the component catalog should be re-queried.
    pub(super) want_status: &'a mut bool,
    /// Set when the memory forecast should be re-queried.
    pub(super) want_estimate: &'a mut bool,
    /// Set when the user asked to unload the backend pipeline.
    pub(super) unload_requested: &'a mut bool,
    /// Set when the user pressed the "translate into English" arrow.
    pub(super) translate_requested: &'a mut bool,
    /// The one prompt-cache control the user pressed this frame, if any.
    pub(super) prompt_cache_action: &'a mut Option<Flux2PromptCacheAction>,
    /// The one per-component action button the user pressed this frame, if any.
    pub(super) component_action: &'a mut Option<(Flux2ComponentId, Flux2ComponentAction)>,
    /// At most one file dialog request per frame.
    pub(super) picker_requested: &'a mut Option<Flux2PickerPurpose>,
    /// The one secret-store control the user pressed this frame, if any.
    pub(super) hf_token_action: &'a mut Option<Flux2HfTokenAction>,
    /// The one download control the user pressed this frame, if any.
    pub(super) download_action: &'a mut Option<Flux2DownloadAction>,
}

impl Flux2PanelCtx<'_> {
    /// Draws the whole «Редактор области» body for this engine: its progress bar and run
    /// status, the prompt block, the parameters, and the note on what the mask means.
    ///
    /// No scroll area and no run button: the panel that hosts this body owns its scrolling,
    /// and «Обработать» / «Применить» / «Отменить» belong to the frame and its host
    /// (`dev-docs/region_edit_v2_plan.md` §13.1).
    ///
    /// `region` is the frame rectangle's size, `None` while the tool has no frame; the
    /// controls that describe a region are then simply not drawn.
    ///
    /// The ORDER is the panel's whole design and is load-bearing: what a user touches on
    /// every edit comes first and is never behind a fold (the prompt, its cache state, the
    /// strength dial), the ONE line that says whether the model is installed at all is next,
    /// and the three one-time-setup surfaces are collapsible siblings under it. The model
    /// paths used to sit two clicks deep while the expert prompt library was expanded on
    /// every frame; that is the inversion this order exists to undo.
    pub(super) fn draw(&mut self, ui: &mut egui::Ui, region: Option<[usize; 2]>) {
        draw_flux2_progress_ui(ui, self.progress);
        if let Some(status) = self.run_status {
            ui.small(status);
        }
        self.draw_prompt(ui);
        self.draw_strength(ui);
        // Handed in, not derived: the readiness line reports it and the setup section
        // decides its INITIAL open state from it, and the run gate answers the same
        // question — two derivations could disagree, and only the engine holds the paths
        // the catalog was asked about.
        let readiness = self.readiness;
        // Pressed THIS frame, and the section below is drawn after — so «Установить» opens
        // it without a session flag and without forcing it open on any later frame.
        let install_requested = self.draw_readiness(ui, readiness);
        self.draw_install_section(ui, region, readiness, install_requested);
        self.draw_memory_section(ui);
        // Read before the `&mut` borrow of the settings, and derived by the same pure
        // decision the tests assert: only a positive `guidance_supported: false` closes the
        // dial, so a backend that never reports the field leaves it exactly as it was.
        let guidance_supported = flux2_guidance_supported(self.status);
        draw_advanced_section(ui, self.settings, guidance_supported, self.settings_changed);
        draw_flux2_mask_hint(ui);
    }

    /// Draws the prompt block: the English field that is actually sent, the one compact
    /// line about its cache state, and the two toggles that unfold the translator and the
    /// prompt-cache library.
    ///
    /// Both toggles are stock `Ui::toggle_value`s in a `horizontal_wrapped`: the panel is a
    /// dock tab and is often ~300 px wide, so a row of two captions must be allowed to wrap
    /// rather than clip. «Перевод» is the persisted `translate_prompt` setting; «Библиотека
    /// промптов» is session-only (see [`Flux2KleinEngine::prompt_library_open`]).
    pub(super) fn draw_prompt(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label(t!("cleaning.tools.flux2_klein.prompt_label"));
        let prompt_edited = ui
            .add(
                egui::TextEdit::multiline(&mut self.settings.prompt)
                    .id_salt("cleaning_flux2_klein_prompt")
                    .hint_text(t!("cleaning.tools.flux2_klein.prompt_hint"))
                    .desired_rows(3),
            )
            .changed();
        *self.settings_changed |= prompt_edited;
        if prompt_edited {
            // `prompt_cached` is an answer about ONE prompt, so an edited field makes the
            // catalog stale in exactly the way a changed model path does. Re-arming the
            // one-shot flag (rather than firing a query here) is what keeps a keystroke
            // from becoming a request: at most one query is ever in flight, and the flag
            // simply stays armed until it returns — the same discipline the memory
            // forecast uses.
            *self.want_status = true;
        }
        self.draw_prompt_cache_line(ui);

        ui.horizontal_wrapped(|ui| {
            *self.settings_changed |= ui
                .toggle_value(
                    &mut self.settings.translate_prompt,
                    t!("cleaning.tools.flux2_klein.translate_prompt_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.translate_prompt_hint"))
                .changed();
            ui.toggle_value(
                self.prompt_library_open,
                t!("cleaning.tools.flux2_klein.prompt_library_label"),
            )
            .on_hover_text(t!("cleaning.tools.flux2_klein.prompt_library_hint"));
        });

        if self.settings.translate_prompt {
            self.draw_translator(ui);
        }
        if *self.prompt_library_open {
            self.draw_prompt_cache_library(ui);
        }
    }

    /// Draws the machine-translation sub-block: the user-language field with the translate
    /// button at the right edge of its own row, the service and source-language pickers
    /// under it, and the status line of the last translation.
    ///
    /// The button carries a caption rather than a glyph and points UP, at the English field
    /// it fills — the direction a bare arrow left the user to guess.
    ///
    /// Unfolded by the «Перевод» toggle and unchanged in behaviour — it still writes into
    /// the ENGLISH field, which stays the only text that reaches the backend.
    pub(super) fn draw_translator(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("cleaning.tools.flux2_klein.source_prompt_label"));
        // The button belongs beside the field it reads, not below the pickers that only
        // configure it: right-to-left places it at the row's right edge first, and the
        // field then takes whatever width is left, so the pair reads as one action.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            let can_translate =
                !self.translate_busy && !self.settings.source_prompt.trim().is_empty();
            let translate = ui
                .add_enabled(
                    can_translate,
                    egui::Button::new(t!("cleaning.tools.flux2_klein.translate_button_label")),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.translate_button_tooltip"))
                .on_disabled_hover_text(t!(
                    "cleaning.tools.flux2_klein.translate_button_disabled_tooltip"
                ));
            if translate.clicked() {
                *self.translate_requested = true;
            }
            if self.translate_busy {
                ui.spinner();
                ui.ctx().request_repaint();
            }
            // Asked for explicitly: a right-to-left layout gives a multiline `TextEdit`
            // its natural width and packs it against the button, leaving a gap on the
            // left. `available_width` here is already the row minus the button.
            let field_width = ui.available_width();
            *self.settings_changed |= ui
                .add(
                    egui::TextEdit::multiline(&mut self.settings.source_prompt)
                        .id_salt("cleaning_flux2_klein_source_prompt")
                        .hint_text(t!("cleaning.tools.flux2_klein.source_prompt_hint"))
                        .desired_width(field_width)
                        .desired_rows(2),
                )
                .changed();
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(t!("cleaning.tools.flux2_klein.mt_service_label"));
            let mut service = MtService::from_key(&self.settings.mt_service)
                .unwrap_or(MtService::Google);
            WheelComboBox::from_id_salt("cleaning_flux2_klein_mt_service")
                .selected_text(service.title())
                .show_ui(ui, |ui| {
                    for candidate in MtService::all() {
                        ui.selectable_value(&mut service, *candidate, candidate.title());
                    }
                });
            if service.key() != self.settings.mt_service {
                self.settings.mt_service = service.key().to_string();
                *self.settings_changed = true;
            }

            ui.label(t!("cleaning.tools.flux2_klein.source_lang_label"));
            let mut lang = normalize_source_lang(&self.settings.source_lang);
            WheelComboBox::from_id_salt("cleaning_flux2_klein_source_lang")
                .selected_text(source_lang_title(&lang))
                .show_ui(ui, |ui| {
                    for candidate in MT_SOURCE_LANGUAGES {
                        ui.selectable_value(
                            &mut lang,
                            candidate.code.to_string(),
                            candidate.title(),
                        );
                    }
                });
            if lang != self.settings.source_lang {
                self.settings.source_lang = lang;
                *self.settings_changed = true;
            }

        });
        if let Some(status) = self.translate_status {
            ui.small(status);
        }
    }

    /// Draws the ONE compact line under the prompt field that says what the next run will
    /// cost: whether these embeddings are already held, or whether this machine has no
    /// encoder to make them with.
    ///
    /// It stays OUTSIDE the collapsible library on purpose. Encoding a new prompt costs a
    /// read of the Qwen3 encoder, where a cached one costs almost nothing, and a cached
    /// prompt is also what waives the encoder in the run gate — so this is everyday
    /// information, while the library that produces it is expert tooling.
    /// [`flux2_prompt_cache_line`] decides which line that is, and an unknown state draws
    /// nothing at all.
    pub(super) fn draw_prompt_cache_line(&mut self, ui: &mut egui::Ui) {
        match flux2_prompt_cache_line(self.prompt_cache_state, self.text_encoder_available()) {
            Flux2PromptCacheLine::Silent => {}
            Flux2PromptCacheLine::Cached => {
                ui.colored_label(
                    FLUX2_STATUS_OK_COLOR,
                    t!("cleaning.tools.flux2_klein.prompt_cached_status"),
                );
            }
            Flux2PromptCacheLine::NotCached => {
                ui.colored_label(
                    FLUX2_STATUS_WARN_COLOR,
                    t!("cleaning.tools.flux2_klein.prompt_not_cached_status"),
                );
            }
            // A WARNING, not an error: without an encoder the tool still generates from
            // ready caches, and only encoding a new prompt is closed.
            Flux2PromptCacheLine::NoEncoder => {
                ui.colored_label(
                    FLUX2_STATUS_WARN_COLOR,
                    t!("cleaning.tools.flux2_klein.text_encoder_missing_warning"),
                );
            }
        }
    }

    /// Draws the prompt-cache LIBRARY, unfolded by the «Библиотека промптов» toggle:
    /// «Кэшировать», the save-under-a-name row, and the library row (the saved entries of
    /// the current encoder family plus load / export / import), with the outcome lines of
    /// the last operation under them.
    ///
    /// Every gate is unchanged — the state the line above reports is what decides most of
    /// them, and folding the buttons away neither grants nor withdraws anything.
    pub(super) fn draw_prompt_cache_library(&mut self, ui: &mut egui::Ui) {
        let text_encoder_available = self.text_encoder_available();
        // Only a positive `false` counts: "not known" must neither warn nor close a button.
        let encoder_missing = text_encoder_available == Some(false);

        let gates = flux2_prompt_cache_gates(
            self.settings,
            self.prompt_cache_state,
            text_encoder_available,
            self.prompt_cache_name_input,
            self.prompt_cache_selected.is_some(),
            self.ai_backend_available,
            // NOT `prompt_cache_busy`: a generation and a component action hold the same
            // pipeline and the same progress bar, so starting a cache operation under one
            // of them would both queue behind it and steal its bar.
            self.pipeline_busy,
        );
        // The two encode-only controls need their own explanation when it is the missing
        // encoder that closed them: the generic tooltip tells the user to fill in a path,
        // which is precisely what they cannot do on this machine.
        let build_disabled_tooltip = if encoder_missing {
            t!("cleaning.tools.flux2_klein.prompt_cache_encoder_missing_disabled_tooltip")
        } else {
            t!("cleaning.tools.flux2_klein.prompt_cache_build_disabled_tooltip")
        };
        let save_disabled_tooltip = if encoder_missing {
            t!("cleaning.tools.flux2_klein.prompt_cache_encoder_missing_disabled_tooltip")
        } else {
            t!("cleaning.tools.flux2_klein.prompt_cache_save_disabled_tooltip")
        };
        // Collected from the rows below and written once at the end: every row needs
        // `self`, and an `Option` assignment keeps "at most one action per frame" a
        // property of the code rather than a convention.
        let mut action: Option<Flux2PromptCacheAction> = None;

        ui.horizontal_wrapped(|ui| {
            if flux2_gated_button(
                ui,
                gates.build,
                t!("cleaning.tools.flux2_klein.prompt_cache_build_button"),
                t!("cleaning.tools.flux2_klein.prompt_cache_build_tooltip"),
                build_disabled_tooltip,
            ) {
                action = Some(Flux2PromptCacheAction::Build);
            }
            if self.prompt_cache_busy {
                ui.spinner();
                ui.ctx().request_repaint();
            }
        });

        // Saving asks for a NAME, in the same shape the watermark library and the typing
        // presets ask for one: an inline field beside the button that commits it, not a
        // modal of this tool's own.
        ui.horizontal(|ui| {
            let field_width = (ui.available_width() - FLUX2_CACHE_NAME_BUTTON_RESERVE).max(80.0);
            ui.add(
                egui::TextEdit::singleline(self.prompt_cache_name_input)
                    .id_salt("cleaning_flux2_klein_prompt_cache_name")
                    .hint_text(t!("cleaning.tools.flux2_klein.prompt_cache_name_hint"))
                    .desired_width(field_width),
            );
            if flux2_gated_button(
                ui,
                gates.save,
                t!("cleaning.tools.flux2_klein.prompt_cache_save_button"),
                t!("cleaning.tools.flux2_klein.prompt_cache_save_tooltip"),
                save_disabled_tooltip,
            ) {
                action = Some(Flux2PromptCacheAction::Save);
            }
        });

        ui.horizontal_wrapped(|ui| {
            self.draw_prompt_cache_library_combo(ui);
            for (enabled, caption, tooltip, disabled_tooltip, requested) in [
                (
                    gates.load,
                    t!("cleaning.tools.flux2_klein.prompt_cache_load_button"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_load_tooltip"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_load_disabled_tooltip"),
                    Flux2PromptCacheAction::Load,
                ),
                (
                    gates.export,
                    t!("cleaning.tools.flux2_klein.prompt_cache_export_button"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_export_tooltip"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_export_disabled_tooltip"),
                    Flux2PromptCacheAction::Export,
                ),
                (
                    gates.import,
                    t!("cleaning.tools.flux2_klein.prompt_cache_import_button"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_import_tooltip"),
                    t!("cleaning.tools.flux2_klein.prompt_cache_import_disabled_tooltip"),
                    Flux2PromptCacheAction::Import,
                ),
            ] {
                if flux2_gated_button(ui, enabled, caption, tooltip, disabled_tooltip) {
                    action = Some(requested);
                }
            }
            if self.prompt_cache_list_busy {
                ui.spinner();
                ui.ctx().request_repaint();
            }
        });

        if let Some(action) = action {
            *self.prompt_cache_action = Some(action);
        }
        if let Some(error) = self.prompt_cache_list_error {
            ui.colored_label(
                FLUX2_STATUS_ERROR_COLOR,
                tf!(
                    "cleaning.tools.flux2_klein.prompt_cache_list_error",
                    err = error
                ),
            );
        }
        if let Some(status) = self.prompt_cache_status {
            ui.small(status);
        }
        if let Some(warning) = self.prompt_cache_warning {
            ui.colored_label(FLUX2_STATUS_WARN_COLOR, warning);
        }
    }

    /// Whether a text encoder is installed on this machine, as the backend reports it.
    ///
    /// `.status` is the primary source because it is re-queried on every settings change;
    /// the library listing answers the same question and is the fallback, so the warning
    /// does not disappear while a status query is outstanding. `None` from both means the
    /// answer is unknown and nothing is claimed.
    pub(super) fn text_encoder_available(&self) -> Option<bool> {
        self.status
            .and_then(|status| status.text_encoder_available)
            .or_else(|| {
                self.prompt_cache_library
                    .and_then(|library| library.text_encoder_available)
            })
    }

    /// Draws the picker of saved library entries.
    ///
    /// The selection is a NAME, so a refreshed listing can neither move it to a different
    /// entry nor keep one that has disappeared. An empty library (or one not listed yet)
    /// shows a placeholder and offers no rows — the load and export buttons are gated on
    /// the selection, so nothing can act on it.
    ///
    /// **With no active family the listing spans the whole library**, so each row is
    /// labelled with the family it belongs to: the user is then looking at other encoders'
    /// caches as well as their own, and a bare name would hide that. The wire still
    /// identifies an entry by NAME alone, so the family is display-only — and a name
    /// present in two families is refused by the backend rather than resolved at random.
    pub(super) fn draw_prompt_cache_library_combo(&mut self, ui: &mut egui::Ui) {
        let entries: &[Flux2PromptCacheEntry] = self
            .prompt_cache_library
            .map_or(&[], |library| library.entries.as_slice());
        // An empty top-level family is the backend saying that none is active — which is
        // the case in which entries of several families share one listing.
        let show_family = self
            .prompt_cache_library
            .is_some_and(|library| library.family.is_empty());
        let selected_text = match self.prompt_cache_selected.as_deref() {
            Some(name) => entries
                .iter()
                .find(|entry| entry.name == name)
                .map_or_else(|| name.to_string(), |entry| entry.label(show_family)),
            None if entries.is_empty() => {
                t!("cleaning.tools.flux2_klein.prompt_cache_library_empty").to_string()
            }
            None => t!("cleaning.tools.flux2_klein.prompt_cache_library_none").to_string(),
        };
        let mut picked: Option<String> = None;
        WheelComboBox::from_id_salt("cleaning_flux2_klein_prompt_cache_library")
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                for entry in entries {
                    let selected = self.prompt_cache_selected.as_deref() == Some(entry.name.as_str());
                    // The prompt an entry encodes is what tells two similar names apart,
                    // and it is far too long for a combo row — so it lives on hover,
                    // together with the creation date.
                    if ui
                        .selectable_label(selected, entry.label(show_family))
                        .on_hover_text(prompt_cache_entry_tooltip(entry, show_family))
                        .clicked()
                    {
                        picked = Some(entry.name.clone());
                    }
                }
            });
        if let Some(name) = picked {
            *self.prompt_cache_selected = Some(name);
        }
    }

    /// Draws the panel's one creative dial: how far the model may move away from what is
    /// already drawn in the region.
    ///
    /// It sits outside every fold on purpose — it is the control a user reaches for
    /// between two runs of the same prompt, and the only generation parameter that is.
    /// The rest live in «Для экспертов».
    pub(super) fn draw_strength(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        *self.settings_changed |= ui
            .add(
                WheelSlider::new(
                    &mut self.settings.strength,
                    FLUX2_STRENGTH_MIN..=FLUX2_STRENGTH_MAX,
                )
                .text(t!("cleaning.tools.flux2_klein.strength_label")),
            )
            .on_hover_text(t!("cleaning.tools.flux2_klein.strength_hint"))
            .changed();
    }

    /// Draws the always-visible readiness line with its buttons, and reports whether
    /// «Установить» was pressed this frame.
    ///
    /// ONE line, because "is the model installed?" has ONE answer
    /// ([`flux2_model_readiness`]); the panel used to leave the user to assemble it from a
    /// five-row presence catalog, a three-row residency block and a refusal under
    /// «Обработать». What the line says is decided by [`flux2_readiness_line`], never here.
    ///
    /// The refresh button re-arms BOTH the component catalog and the memory forecast. Both
    /// are re-armed automatically by everything that happens inside the app, so the only
    /// staleness left is the kind caused from outside it — another process taking VRAM, a
    /// file appearing on disk — and that is one condition, not the two buttons this
    /// replaces.
    pub(super) fn draw_readiness(&mut self, ui: &mut egui::Ui, readiness: Flux2ModelReadiness) -> bool {
        ui.separator();
        // Only the READY line carries the forecast: while a component is missing the
        // figures describe a run that cannot start, and naming what is missing is the
        // actionable half.
        let memory = match readiness {
            Flux2ModelReadiness::Ready => self.estimate.map(|estimate| Flux2MemorySummary {
                line: estimate_status_line(estimate, self.status),
                fits: estimate.fits,
            }),
            Flux2ModelReadiness::Missing(_) | Flux2ModelReadiness::Unknown => None,
        };
        let line = flux2_readiness_line(readiness, memory);
        let text = line.text();
        let mut install_requested = false;
        ui.horizontal_wrapped(|ui| {
            match line.tone() {
                Flux2LineTone::Ok => {
                    ui.colored_label(FLUX2_STATUS_OK_COLOR, text);
                }
                Flux2LineTone::Warn => {
                    ui.colored_label(FLUX2_STATUS_WARN_COLOR, text);
                }
                Flux2LineTone::Neutral => {
                    ui.small(text);
                }
            }
            if line.offers_install()
                && ui
                    .small_button(t!("cleaning.tools.flux2_klein.install_button"))
                    .on_hover_text(t!("cleaning.tools.flux2_klein.install_hint"))
                    .clicked()
            {
                install_requested = true;
            }
            if ui
                .small_button(t!("cleaning.tools.flux2_klein.refresh_state_button"))
                .on_hover_text(t!("cleaning.tools.flux2_klein.refresh_state_hint"))
                .clicked()
            {
                *self.want_status = true;
                *self.want_estimate = true;
            }
        });
        install_requested
    }

    /// Draws «Установка модели»: where the model comes from, what of it is on disk, where
    /// its weights currently are, and the one button that frees them again.
    ///
    /// This is the section without which nothing works, so it starts folded and OPENS
    /// ITSELF once — on the first `Missing` verdict that lands, and only while the user has
    /// not already moved the fold himself ([`flux2_install_seed`],
    /// [`Flux2KleinEngine::install_section_seeded`]). It is deliberately not driven by
    /// `default_open`: on the frame the panel first draws, `.status` has not answered and
    /// the verdict is `Unknown`, so a default read from it would open the section on every
    /// launch. After the one seeding the state is never forced again, because a section
    /// reopened per frame cannot be closed. `open_requested` is «Установить» of the
    /// readiness line, pressed in THIS frame above: the button opens the section it points
    /// at without a session flag of its own.
    ///
    /// A raw [`egui::CollapsingHeader`] cannot be opened from elsewhere, which is why this
    /// one section is built from [`egui::collapsing_header::CollapsingState`] while its two
    /// siblings use `RegionEditToolBase::draw_region_editor_collapsible_section`.
    pub(super) fn draw_install_section(
        &mut self,
        ui: &mut egui::Ui,
        region: Option<[usize; 2]>,
        readiness: Flux2ModelReadiness,
        open_requested: bool,
    ) {
        let variant = self.variant;
        let settings = &mut *self.settings;
        let changed = &mut *self.settings_changed;
        let unload_requested = &mut *self.unload_requested;
        let picker_requested = &mut *self.picker_requested;
        let unload_status = &mut *self.unload_status;
        let component_action = &mut *self.component_action;
        let component_action_status = self.component_action_status;
        // The buttons of the list are the backend's own; this only says whether the
        // pipeline is free to run one right now.
        let component_actions_enabled = self.ai_backend_available && !self.pipeline_busy;
        let status = self.status;
        let status_error = self.status_error;
        let pipeline_busy = self.pipeline_busy;
        let ai_backend_available = self.ai_backend_available;
        let hf_token_action = &mut *self.hf_token_action;
        let download_action = &mut *self.download_action;
        let hf_token_state = self.hf_token_state;
        let hf_token_input = &mut *self.hf_token_input;
        let hf_token_status = self.hf_token_status;
        let hf_token_busy = self.hf_token_busy;
        let download_check = self.download_check;
        let download_check_error = self.download_check_error;
        let download_check_busy = self.download_check_busy;
        let download_busy = self.download_busy;
        let download_status = self.download_status;

        ui.separator();
        let section_id = ui.make_persistent_id("cleaning_flux2_klein_install_section");
        let mut section = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            section_id,
            false,
        );
        // The first DEFINITE verdict decides the initial state, and only it: on the frame
        // the panel first draws, `.status` has not answered and the verdict is `Unknown`,
        // so seeding from `default_open` would open the section on every launch even on a
        // machine where everything is installed. A fold that differs from what the last
        // frame LEFT it in was moved by the header or by Escape — i.e. by the user — and
        // that retires the seeding for good.
        let user_moved = self.install_section_open_prev.is_some_and(|prev| prev != section.is_open());
        match flux2_install_seed(*self.install_section_seeded, user_moved, readiness) {
            Flux2InstallSeed::Keep => {}
            Flux2InstallSeed::Open => {
                section.set_open(true);
                *self.install_section_seeded = true;
            }
            Flux2InstallSeed::Seal => *self.install_section_seeded = true,
        }
        if open_requested {
            section.set_open(true);
        }
        // Recorded BEFORE `show_header` consumes the state, so it holds what THIS file left
        // behind; a click inside the header changes the stored state afterwards and is what
        // the next frame reads as `user_moved`.
        *self.install_section_open_prev = Some(section.is_open());
        section
            .show_header(ui, |ui| {
                ui.label(t!("cleaning.tools.flux2_klein.install_heading"));
            })
            .body(|ui| {
                // The one line about the OTHER engine, and it belongs here rather than on
                // the everyday path: this section is where the model is installed and
                // where the residency list below reports what is loaded, which is exactly
                // the state the sentence explains. The backend holds ONE pipeline keyed by
                // the component paths, so running the other variant unloads this one; this
                // side orchestrates nothing and only says so.
                ui.small(t!("cleaning.tools.flux2_klein.variant_residency_hint"));
                // The switch sits ABOVE everything it governs: exactly one of the two
                // bodies below is drawn, and the user has to see which one he is in before
                // he reads it.
                draw_source_mode_switch(ui, settings, changed);
                match settings.source_mode() {
                    Flux2SourceMode::Manual => {
                        draw_path_row(
                            ui,
                            &Flux2PathRow {
                                label: t!("cleaning.tools.flux2_klein.text_encoder_path_label"),
                                hint: t!("cleaning.tools.flux2_klein.text_encoder_path_hint"),
                                id_salt: "cleaning_flux2_klein_text_encoder_path",
                                buttons: &[(
                                    "📁",
                                    t!("cleaning.tools.flux2_klein.browse_folder_tooltip"),
                                    Flux2PickerPurpose::TextEncoderDir,
                                )],
                            },
                            &mut settings.text_encoder_path,
                            changed,
                            picker_requested,
                        );
                        draw_path_row(
                            ui,
                            &Flux2PathRow {
                                label: t!("cleaning.tools.flux2_klein.transformer_path_label"),
                                hint: t!("cleaning.tools.flux2_klein.transformer_path_hint"),
                                id_salt: "cleaning_flux2_klein_transformer_path",
                                buttons: &[
                                    (
                                        "📄",
                                        t!("cleaning.tools.flux2_klein.browse_file_tooltip"),
                                        Flux2PickerPurpose::TransformerFile,
                                    ),
                                    (
                                        "📁",
                                        t!("cleaning.tools.flux2_klein.browse_folder_tooltip"),
                                        Flux2PickerPurpose::TransformerDir,
                                    ),
                                ],
                            },
                            &mut settings.transformer_path,
                            changed,
                            picker_requested,
                        );
                        draw_path_row(
                            ui,
                            &Flux2PathRow {
                                label: t!("cleaning.tools.flux2_klein.vae_path_label"),
                                hint: t!("cleaning.tools.flux2_klein.vae_path_hint"),
                                id_salt: "cleaning_flux2_klein_vae_path",
                                buttons: &[
                                    (
                                        "📄",
                                        t!("cleaning.tools.flux2_klein.browse_file_tooltip"),
                                        Flux2PickerPurpose::VaeFile,
                                    ),
                                    (
                                        "📁",
                                        t!("cleaning.tools.flux2_klein.browse_folder_tooltip"),
                                        Flux2PickerPurpose::VaeDir,
                                    ),
                                ],
                            },
                            &mut settings.vae_path,
                            changed,
                            picker_requested,
                        );
                    }
                    // The three manual rows are NOT drawn here: in this mode the paths are
                    // derived, and an editable field whose value nothing reads is a lie.
                    // The derived destinations are shown read-only inside the block.
                    Flux2SourceMode::Download => draw_download_block(
                        ui,
                        settings,
                        Flux2DownloadView {
                            variant,
                            token_state: hf_token_state,
                            token_input: hf_token_input,
                            token_status: hf_token_status,
                            token_busy: hf_token_busy,
                            check: download_check,
                            check_error: download_check_error,
                            check_busy: download_check_busy,
                            download_busy,
                            download_status,
                            ai_backend_available,
                            pipeline_busy,
                            status,
                        },
                        changed,
                        hf_token_action,
                        download_action,
                    ),
                }

                ui.separator();
                draw_component_list_ui(
                    ui,
                    status,
                    status_error,
                    region,
                    component_actions_enabled,
                    component_action,
                );
                if let Some(text) = component_action_status {
                    ui.small(text);
                }
                if ui
                    .small_button(t!("cleaning.tools.flux2_klein.unload_button"))
                    .on_hover_text(t!("cleaning.tools.flux2_klein.unload_hint"))
                    .clicked()
                {
                    *unload_requested = true;
                }
                if let Some(text) = unload_status.as_ref() {
                    ui.small(text);
                }
            });
    }

    /// Draws «Память и скорость»: the memory preset and the backend's forecast, and
    /// nothing else.
    ///
    /// Closed by default, because a preset is chosen once and the forecast it produces is
    /// already on the readiness line above — the section exists to CHANGE the preset, not
    /// to report it. The seven fields a preset owns live in «Для экспертов», where
    /// overriding one is presented as what it is.
    pub(super) fn draw_memory_section(&mut self, ui: &mut egui::Ui) {
        let settings = &mut *self.settings;
        let changed = &mut *self.settings_changed;
        let estimate = self.estimate;
        let estimate_error = self.estimate_error;
        let estimate_busy = self.estimate_busy;
        let status = self.status;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            "cleaning_flux2_klein_memory",
            t!("cleaning.tools.flux2_klein.memory_heading"),
            false,
            |ui| {
                draw_preset_row(ui, settings, changed);
                draw_estimate_ui(ui, estimate, estimate_error, estimate_busy, status);
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal that replaces the backend's untranslated «Путь ... не найден» is only
    /// actionable if the translation actually names the missing parts, and a hover that is
    /// missing in one language is a control with no explanation there. Both are properties
    /// of the CATALOGS, which a unit test cannot see through `t!` — it runs with none.
    #[test]
    fn every_catalog_carries_the_new_panel_keys() {
        const WITH_COMPONENTS: [&str; 3] = [
            "cleaning.tools.flux2_klein.model_not_downloaded_error",
            "cleaning.tools.flux2_klein.model_components_missing_error",
            // The readiness line is the panel's ONE report of the same verdict, and it is
            // actionable only while it names the parts that are missing.
            "cleaning.tools.flux2_klein.model_missing_status",
        ];
        const PLAIN: [&str; 26] = [
            "cleaning.tools.flux2_klein.guidance_label",
            // The two halves of the ONE disabled control in this panel. A language that
            // lacks either leaves the user with a grey dial and no reason for it, which is
            // the exact bug report the explanation exists to prevent.
            "cleaning.tools.flux2_klein.guidance_unsupported_disabled_tooltip",
            "cleaning.tools.flux2_klein.guidance_unsupported_note",
            "cleaning.tools.flux2_klein.strength_hint",
            "cleaning.tools.flux2_klein.fixed_seed_hint",
            "cleaning.tools.flux2_klein.preset_hint",
            "cleaning.tools.flux2_klein.placement_hint",
            "cleaning.tools.flux2_klein.dtype_hint",
            "cleaning.tools.flux2_klein.low_cpu_mem_usage_hint",
            "cleaning.tools.flux2_klein.color_match_hint",
            "cleaning.tools.flux2_klein.refresh_state_hint",
            "cleaning.tools.flux2_klein.unload_hint",
            "cleaning.tools.flux2_klein.text_encoder_path_hint",
            "cleaning.tools.flux2_klein.transformer_path_hint",
            "cleaning.tools.flux2_klein.vae_path_hint",
            // The restructured panel: the three section headings, the two prompt toggles
            // and the readiness line are the whole navigation of this tool, so a language
            // that lacks one of them loses a section rather than a sentence.
            "cleaning.tools.flux2_klein.install_heading",
            "cleaning.tools.flux2_klein.memory_heading",
            "cleaning.tools.flux2_klein.advanced_heading",
            "cleaning.tools.flux2_klein.translate_prompt_label",
            "cleaning.tools.flux2_klein.translate_prompt_hint",
            "cleaning.tools.flux2_klein.prompt_library_label",
            "cleaning.tools.flux2_klein.prompt_library_hint",
            "cleaning.tools.flux2_klein.model_ready_status",
            "cleaning.tools.flux2_klein.model_readiness_unknown_status",
            "cleaning.tools.flux2_klein.install_hint",
            "cleaning.tools.flux2_klein.preset_fields_hint",
        ];
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for key in WITH_COMPONENTS.into_iter().chain(PLAIN) {
                let value = catalog
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("locale `{tag}` lacks the key `{key}`"));
                assert!(!value.trim().is_empty(), "locale `{tag}`: `{key}` is empty");
            }
            for key in WITH_COMPONENTS {
                let value = catalog.get(key).and_then(Value::as_str).unwrap_or_default();
                assert!(
                    value.contains("{components}"),
                    "locale `{tag}`: `{key}` drops `{{components}}`, so the refusal would not \
                     say WHICH parts are missing — the one thing that makes it actionable"
                );
            }
            // The other half of the readiness line: a translation that drops `{memory}`
            // would silently swallow the whole forecast, because that line is the only
            // place it is shown while «Память и скорость» stays folded.
            let ready = catalog
                .get("cleaning.tools.flux2_klein.model_ready_memory_status")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("locale `{tag}` lacks the ready-with-forecast line"));
            assert!(
                ready.contains("{memory}"),
                "locale `{tag}`: the ready line drops `{{memory}}` and would hide the forecast"
            );
        }
    }

    #[test]
    fn every_catalog_keeps_the_placeholders_a_library_row_is_built_from() {
        // A translation that drops `{family}` or `{name}` compiles and passes the
        // key-existence test, while leaving the user unable to tell whose cache a row is —
        // which is the entire reason the family is on the row at all.
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            let entry = |key: &str| -> String {
                catalog
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("locale `{tag}` lacks the key `{key}`"))
                    .to_owned()
            };
            let row = entry("cleaning.tools.flux2_klein.prompt_cache_entry_with_family");
            assert!(
                row.contains("{family}") && row.contains("{name}"),
                "locale `{tag}`: a library row names both the family and the entry, `{row}` does not"
            );
            let hover = entry("cleaning.tools.flux2_klein.prompt_cache_entry_family");
            assert!(
                hover.contains("{family}"),
                "locale `{tag}`: the hover line carries the family, `{hover}` does not"
            );
        }
    }
}
