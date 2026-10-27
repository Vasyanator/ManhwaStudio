/*
File: cleaning/tools/ai_editor/engines/flux2_klein/ui/mod.rs

Purpose:
The parameter panel of the FLUX.2 klein engine. This file is the panel BODY — the one
place that decides what the panel contains and in which order — and the root of the `ui`
module, whose siblings draw the individual blocks it calls into.

Main responsibilities:
- own `Flux2PanelCtx`: everything the panel may read or mutate, borrowed for exactly one
  frame, with the intents the controls raise travelling back as plain flags;
- draw the body in its designed ORDER: the prompt block (field, preset name with
  «Кэшировать» / «Переименовать», the cache line, the two unfolding toggles; the library fold
  holds the shared preset picker, «Экспорт» and «Импорт» — all of it the shared prompt-cache
  block of `../../shared/prompt_cache_ui.rs`, fed with FLUX.2's context and notes), «Сила изменения», the readiness line, the three sibling
  collapsible sections, the mask hint and, docked at the bottom, the shared
  «Постобработка» section (`draw_postprocess_section`);
- draw the two PINNED parts the host places outside the panel's scroll: the progress with
  the run status (`draw_flux2_progress_section`, top) and the per-run option
  «Внимание только к маске» (`draw_flux2_run_options`, bottom);
- delegate each block to the submodule that owns it;
- in «Память и скорость», hand the shared widget the transformer format and draw the
  encoder's format rows under it (`draw_encoder_formats`) while the weight formats are in
  effect (download mode, either variant); manual mode passes none.

Key structures:
- `Flux2PanelCtx`

Key functions:
- `Flux2PanelCtx::draw()` and the per-block methods it calls
- `draw_flux2_progress_section()`, `draw_flux2_run_options()`: the pinned parts

Submodules:
- `install.rs`: the source-mode switch, the model paths and the catalog download block
  (encoder source toggle, the two weight-format pickers, the token row, the bundle).
- `components.rs`: the merged component list (the memory steps are the shared
  `ai_editor::large_model_resources` widget, drawn by `draw_memory_section`).
- `advanced.rs`: «Для экспертов».
- `mask_hint.rs`: the mask hint (the progress bar is the shared `draw_stream_progress`).

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
// The merged component list of «Установка модели».
mod components;
use components::*;
// «Для экспертов»: the parameters set once per machine or per page.
mod advanced;
use advanced::*;
// The mask hint the body closes with.
mod mask_hint;
use mask_hint::*;

// ---------------------------------------------------------------------------------------
// Pinned parts
// ---------------------------------------------------------------------------------------

/// The section pinned at the TOP of the panel: the progress every long operation of this
/// engine shares, and the engine's line about the last run under it.
///
/// Draws nothing at all while no operation owns the bar and no run has reported, so an
/// idle panel gives the whole height to its parameters.
pub(super) fn draw_flux2_progress_section(
    ui: &mut egui::Ui,
    progress: &Mutex<EngineProgress>,
    run_status: Option<&str>,
) {
    draw_stream_progress(ui, progress);
    if let Some(status) = run_status {
        ui.small(status);
    }
}

/// The per-run option pinned at the BOTTOM of the panel, under «Обработать»:
/// «Внимание только к маске». Returns `true` when the user toggled it.
///
/// Open only while a mask is painted ([`flux2_text_attention_block_reason`]); closed, it
/// shows the user's STORED choice greyed out, with the reason on hover, and cannot change
/// it — the choice comes back into effect as soon as a mask is painted.
pub(super) fn draw_flux2_run_options(ui: &mut egui::Ui, text_attention_in_mask: &mut bool, ctx: RunOptionsCtx) -> bool {
    let block_reason = flux2_text_attention_block_reason(ctx.mask_painted);
    let response = ui
        .add_enabled(
            block_reason.is_none(),
            egui::Checkbox::new(text_attention_in_mask, t!("cleaning.tools.flux2_klein.text_attention_in_mask_label")),
        )
        .on_hover_text(t!("cleaning.tools.flux2_klein.text_attention_in_mask_hint"));
    let response = match block_reason {
        Some(reason) => response.on_disabled_hover_text(reason),
        None => response,
    };
    response.changed()
}

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
    /// Which checkpoint this panel belongs to. The bodies of the two engines differ only by
    /// what the variant offers (the uncensored-encoder toggle, the manual encoder FILE
    /// button) and by its catalog bundle (whether a gated repository needs the token row).
    pub(super) variant: Flux2Variant,
    pub(super) settings: &'a mut Flux2KleinSettings,
    pub(super) status: Option<&'a Flux2Status>,
    pub(super) status_error: Option<&'a str>,
    /// The large-model forecast and the caps buffer («Память и скорость», the readiness
    /// line).
    pub(super) resources: ResourcesView<'a>,
    /// What is wrong with the settings file, drawn above everything else.
    pub(super) settings_problem: Option<&'a SettingsProblem>,
    /// The last failed settings write (localized), drawn beside the file notice.
    pub(super) save_error: Option<&'a str>,
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
    /// Three-state answer for the prompt currently in the field: `Some(true)` cached,
    /// `Some(false)` not cached, `None` not known yet.
    pub(super) prompt_cache_state: Option<bool>,
    /// The entry `.status` named for the prompt in the field (same staleness guard as
    /// `prompt_cache_state`): the encoder the cache line names first.
    pub(super) prompt_cache_status_entry: Option<&'a PromptLibraryEntry>,
    /// The model's prompt library (the shared controller): the listing, the name field, the
    /// outcome lines and the session-only «Библиотека промптов» fold flag, which the toggle
    /// writes directly.
    pub(super) prompt_cache: &'a mut PromptCacheController,
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
    /// The gated repositories of the current bundle not known to be installed
    /// ([`catalog_gated_repos_missing`]); the token row is drawn while it is non-empty.
    pub(super) gated_repos: &'a [&'static str],
    /// The line a source switch left under the uncensored toggle — see
    /// [`Flux2KleinEngine::encoder_format_notice`]. Written by the toggle directly.
    pub(super) encoder_format_notice: &'a mut Option<String>,
    pub(super) ai_backend_available: bool,
    /// Set when a control changed a persisted value (a save is owed, the forecast re-armed).
    pub(super) settings_changed: &'a mut bool,
    /// Set when only a «Постобработка» value changed: a save is owed, but the forecast and
    /// the catalog are not re-asked (`Flux2KleinEngine::note_postprocess_changed`).
    pub(super) postprocess_changed: &'a mut bool,
    /// Set when only the «Нет» stash (`uncached_prompt`) moved: a save is owed, nothing else.
    pub(super) uncached_changed: &'a mut bool,
    /// Set when the component catalog should be re-queried.
    pub(super) want_status: &'a mut bool,
    /// Set when the memory forecast should be asked again (refresh button).
    pub(super) want_forecast: &'a mut bool,
    /// What the user did in the memory widget this frame.
    pub(super) resources_actions: &'a mut Vec<ResourcesAction>,
    /// Set when the user asked to unload the backend pipeline.
    pub(super) unload_requested: &'a mut bool,
    /// Set when the user pressed the "translate into English" arrow.
    pub(super) translate_requested: &'a mut bool,
    /// The one prompt-cache control the user pressed this frame, if any.
    pub(super) prompt_cache_action: &'a mut Option<PromptCacheAction>,
    /// The one per-component action button the user pressed this frame, if any.
    pub(super) component_action: &'a mut Option<(Flux2ComponentId, Flux2ComponentAction)>,
    /// At most one file dialog request per frame.
    pub(super) picker_requested: &'a mut Option<Flux2PickerPurpose>,
    /// The one secret-store control the user pressed this frame, if any.
    pub(super) hf_token_action: &'a mut Option<HfTokenAction>,
    /// The catalog download block's state for the bundle the selection needed when the frame
    /// began (download mode, [`Flux2KleinEngine::catalog_snapshot`]); `None` otherwise.
    pub(super) catalog_snapshot: Option<&'a OcrModelPanelSnapshot>,
    /// The catalog bundle is known to be incomplete: the readiness line says so
    /// ([`flux2_readiness_line`]). Always `false` in manual mode.
    pub(super) bundle_missing: bool,
    /// The catalog download block's click this frame (download / verify / cancel).
    pub(super) catalog_action: &'a mut ModelDownloadAction,
}

impl Flux2PanelCtx<'_> {
    /// Draws the scrolled «Редактор области» body for this engine: the prompt block, the
    /// parameters, the note on what the mask means and, last, «Постобработка».
    ///
    /// No scroll area, no progress and no run button: the panel that hosts this body owns
    /// its scrolling, the progress is pinned above it (`draw_flux2_progress_section`), and
    /// «Обработать» / «Применить» / «Отменить» belong to the frame and its host
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
        self.draw_settings_problem(ui);
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
        // LAST, by the shared section's contract: it docks at the bottom of the body's free
        // room. The values are the host's live post-process (`AiEngine::postprocess_settings`),
        // so an edit here re-derives a pending result without a new run.
        *self.postprocess_changed |= draw_postprocess_section(ui, "cleaning_flux2_klein", &mut self.settings.postprocess);
    }

    /// Draws what is wrong with the settings file, when something is: a write that failed
    /// (retried automatically), a damaged file that
    /// was moved aside (a warning; defaults are in use and saving works), or one that could
    /// not be (an error; saving is blocked so the file is never overwritten).
    pub(super) fn draw_settings_problem(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = self.save_error {
            ui.colored_label(ms_theme::status::ERROR, error);
        }
        match self.settings_problem {
            None => {}
            Some(SettingsProblem::MovedAside(path)) => {
                ui.colored_label(
                    ms_theme::status::WARNING,
                    tf!("cleaning.tools.flux2_klein.settings_malformed_status", path = path.display()),
                );
            }
            Some(SettingsProblem::SaveBlocked(reason)) => {
                ui.colored_label(
                    ms_theme::status::ERROR,
                    tf!("cleaning.tools.flux2_klein.settings_save_blocked_error", err = reason),
                );
            }
        }
    }

    /// Draws the prompt block: the English field that is actually sent, the preset name with
    /// «Кэшировать» / «Переименовать», the one compact line about its cache state, and the two
    /// toggles that unfold the translator and the prompt library.
    ///
    /// The field is MATCHED against the model's library every frame: a typed text equal to a
    /// preset's (after trimming) is that preset — the name field shows its name, the line names
    /// the encoder of the entry a run will use — and a text no preset holds is kept as the «Нет»
    /// stash ([`track_uncached`]). Both toggles are stock `Ui::toggle_value`s in a
    /// `horizontal_wrapped`: the panel is a dock tab and is often ~300 px wide, so a row of two
    /// captions must be allowed to wrap rather than clip. «Перевод» is the persisted
    /// `translate_prompt` setting; «Библиотека промптов» is session-only (see
    /// [`PromptCacheController::library_open`]). The cache row, the line, the library toggle and
    /// fold are the shared prompt-cache block (`../../shared/prompt_cache_ui.rs`); this method
    /// supplies FLUX.2's gate context, encoder caption, id salts and migration notes.
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
        if prompt_edited {
            self.note_prompt_changed();
        }
        *self.uncached_changed |= track_uncached(&mut self.settings.uncached_prompt, self.prompt_cache.library.as_ref(), &self.settings.prompt);
        {
            let ctl = &mut *self.prompt_cache;
            let hit = ctl.library.as_ref().and_then(|listing| listing.text_for(&self.settings.prompt));
            sync_preset_name(&mut ctl.name_input, &mut ctl.name_synced_for, hit);
        }
        let text_encoder_available = self.text_encoder_available();
        let hit = self.prompt_cache.hit(&self.settings.prompt);
        let gates = prompt_cache_gates(
            &flux2_cache_context(self.settings),
            hit,
            &self.prompt_cache.name_input,
            self.prompt_cache_state,
            text_encoder_available,
            self.ai_backend_available,
            // NOT the cache job alone: a generation and a component action hold the same
            // pipeline and the same progress bar, so starting a cache operation under one of
            // them would both queue behind it and steal its bar.
            self.pipeline_busy,
        );
        // The ONE compact line under the cache row says what the next run will cost. It stays
        // OUTSIDE the collapsible library on purpose: a cached prompt skips the encoder read
        // and also waives the encoder in the run gate, so this is everyday information.
        let line = prompt_cache_line(hit.map(|text| &text.selected), self.prompt_cache_status_entry, self.prompt_cache_state, text_encoder_available, &flux2_encoder_caption);
        let notes = self.prompt_library_notes();
        if let Some(action) = draw_cache_row(ui, &mut self.prompt_cache_view(&gates, &notes)) {
            *self.prompt_cache_action = Some(action);
        }
        draw_prompt_cache_line(ui, &line);

        ui.horizontal_wrapped(|ui| {
            *self.settings_changed |= ui
                .toggle_value(
                    &mut self.settings.translate_prompt,
                    t!("cleaning.tools.flux2_klein.translate_prompt_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.translate_prompt_hint"))
                .changed();
            draw_prompt_library_toggle(ui, &mut self.prompt_cache.library_open);
        });

        if self.settings.translate_prompt {
            self.draw_translator(ui);
        }
        if self.prompt_cache.library_open {
            let output = draw_prompt_cache_library(ui, &mut self.prompt_cache_view(&gates, &notes));
            if output.prompt_changed {
                self.note_prompt_changed();
            }
            if let Some(action) = output.action {
                *self.prompt_cache_action = Some(action);
            }
        }
    }

    /// Raises what an edited prompt owes: a save, and a fresh `.status` — `prompt_cached` is an
    /// answer about ONE prompt. Re-arming the one-shot flag (rather than firing a query here)
    /// is what keeps a keystroke from becoming a request: at most one query is ever in flight,
    /// and the flag simply stays armed until it returns.
    fn note_prompt_changed(&mut self) {
        *self.settings_changed = true;
        *self.want_status = true;
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

    /// The shared prompt-cache block's borrow for one draw call: the controller, the prompt
    /// field and the «Нет» stash, with FLUX.2's encoder caption and its two stable id salts
    /// (literals, never localized: the name field's text cursor and the picker's popup live
    /// under them).
    fn prompt_cache_view<'b>(&'b mut self, gates: &'b PromptCacheGates, notes: &'b [PromptCacheNote]) -> PromptCacheBlockView<'b> {
        PromptCacheBlockView {
            ctl: &mut *self.prompt_cache,
            prompt: &mut self.settings.prompt,
            uncached: &self.settings.uncached_prompt,
            gates,
            caption: &flux2_encoder_caption,
            name_id_salt: "cleaning_flux2_klein_prompt_cache_name",
            library_id_salt: "cleaning_flux2_klein_prompt_library",
            notes,
        }
    }

    /// FLUX.2's own lines of the library fold: the report of the one-time migration of the old
    /// per-encoder caches (how many were carried over or deleted, and — as a warning — how many
    /// could not be carried over and were left in place). Drawn by the shared fold after its
    /// outcome lines.
    fn prompt_library_notes(&self) -> Vec<PromptCacheNote> {
        let mut notes = Vec::new();
        let legacy = self.prompt_cache.library.as_ref().and_then(|listing| listing.legacy.as_ref()).filter(|legacy| !legacy.is_empty());
        if let Some(legacy) = legacy {
            notes.push(PromptCacheNote::Small(tf!(
                "cleaning.tools.flux2_klein.prompt_cache_legacy_status",
                migrated = legacy.migrated,
                deleted = legacy.deleted
            )));
            if legacy.left_in_place > 0 {
                notes.push(PromptCacheNote::Warning(tf!("cleaning.tools.flux2_klein.prompt_cache_legacy_left_status", count = legacy.left_in_place)));
            }
        }
        notes
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
                self.prompt_cache
                    .library
                    .as_ref()
                    .and_then(|library| library.text_encoder_available)
            })
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
            Flux2ModelReadiness::Ready => self.resources.profiles.and_then(|profiles| {
                forecast_line(profiles).map(|line| Flux2MemorySummary { line, fits: profiles.selected.fits })
            }),
            Flux2ModelReadiness::Missing(_) | Flux2ModelReadiness::Unknown => None,
        };
        let line = flux2_readiness_line(readiness, self.bundle_missing, memory);
        let text = line.text();
        let mut install_requested = false;
        ui.horizontal_wrapped(|ui| {
            match line.tone() {
                Flux2LineTone::Ok => {
                    ui.colored_label(ms_theme::status::SUCCESS, text);
                }
                Flux2LineTone::Warn => {
                    ui.colored_label(ms_theme::status::WARNING, text);
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
                *self.want_forecast = true;
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
        let hf_token_action = &mut *self.hf_token_action;
        let hf_token_state = self.hf_token_state;
        let hf_token_input = &mut *self.hf_token_input;
        let hf_token_status = self.hf_token_status;
        let hf_token_busy = self.hf_token_busy;
        let gated_repos = self.gated_repos;
        let encoder_format_notice = &mut *self.encoder_format_notice;
        let catalog_snapshot = self.catalog_snapshot;
        let catalog_action = &mut *self.catalog_action;

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
                        // A GGUF text encoder is a single FILE, so a variant that offers GGUF
                        // encoder formats (both do) gets a file button beside the folder one.
                        let mut encoder_buttons = Vec::with_capacity(2);
                        let gguf_encoders = Flux2EncoderSource::offered_for(variant)
                            .iter()
                            .any(|&source| Flux2WeightFormat::offered_for(variant, Flux2WeightAxis::TextEncoder, source).iter().any(|format| format.is_gguf()));
                        if gguf_encoders {
                            encoder_buttons.push((
                                "📄",
                                t!("cleaning.tools.flux2_klein.browse_file_tooltip"),
                                Flux2PickerPurpose::TextEncoderFile,
                            ));
                        }
                        encoder_buttons.push((
                            "📁",
                            t!("cleaning.tools.flux2_klein.browse_folder_tooltip"),
                            Flux2PickerPurpose::TextEncoderDir,
                        ));
                        draw_path_row(
                            ui,
                            &Flux2PathRow {
                                label: t!("cleaning.tools.flux2_klein.text_encoder_path_label"),
                                hint: t!("cleaning.tools.flux2_klein.text_encoder_path_hint"),
                                id_salt: "cleaning_flux2_klein_text_encoder_path",
                                buttons: &encoder_buttons,
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
                    // Both variants download through the app's catalog: the encoder source,
                    // the two weight formats, the token row while a gated spec is missing and
                    // one pinned bundle. The three manual rows are NOT drawn here: in this mode
                    // the paths are derived, and an editable field whose value nothing reads is
                    // a lie; the derived destinations are shown read-only inside the block.
                    Flux2SourceMode::Download => draw_catalog_download_block(
                        ui,
                        settings,
                        Flux2CatalogDownloadView {
                            snapshot: catalog_snapshot,
                            status,
                            gated_repos,
                            token_state: hf_token_state,
                            token_input: hf_token_input,
                            token_status: hf_token_status,
                            token_busy: hf_token_busy,
                        },
                        Flux2CatalogDownloadOut { changed, action: catalog_action, hf_token_action, encoder_format_notice },
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

    /// Draws «Память и скорость»: the two memory steps with the backend's forecast for them,
    /// the learned floor and the folded caps — the shared large-model widget.
    ///
    /// Closed by default: the steps are chosen once per machine and the forecast they produce
    /// is already on the readiness line above. The widget writes the step pair into a copy
    /// that is stored back here, and every other action travels to the engine.
    ///
    /// While the weight formats are in effect (download mode) the widget also gets the
    /// transformer format (its format rows; a picked row is written into the setting) and the
    /// encoder's format rows are drawn right under it (`draw_encoder_formats`); a pick on
    /// either raises `settings_changed`, which saves it and re-arms the status, the forecast
    /// and the bundle probe.
    pub(super) fn draw_memory_section(&mut self, ui: &mut egui::Ui) {
        let settings = &mut *self.settings;
        let changed = &mut *self.settings_changed;
        let actions_out = &mut *self.resources_actions;
        let resources = &mut self.resources;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            "cleaning_flux2_klein_memory",
            t!("cleaning.tools.flux2_klein.memory_heading"),
            false,
            |ui| {
                let mut choice = settings.step_choice();
                // Weight formats exist only while they are in effect (download mode): then
                // the transformer's format rows come from the widget and the encoder's right
                // under it; manual mode draws no rows.
                let formats = settings.weight_formats_in_effect();
                // Copied out before `reborrow` hands the view to the widget.
                let profiles = resources.profiles;
                let actions = draw_large_model_resources(
                    ui,
                    "cleaning_flux2_klein_resources",
                    &mut choice,
                    formats.then_some(&mut settings.transformer_format),
                    Some(&mut settings.encoder_on_cpu),
                    resources.reborrow(),
                );
                if actions.contains(&ResourcesAction::StepsChanged) {
                    *changed |= settings.set_step_choice(choice);
                }
                actions_out.extend(actions);
                if formats
                    && let Some(profiles) = profiles
                    && draw_encoder_formats(ui, profiles, &mut settings.text_encoder_format).is_some()
                {
                    // The row already wrote its id into the setting; the engine saves it and
                    // re-arms the status, the forecast and the bundle probe.
                    *changed = true;
                }
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
        const PLAIN: [&str; 20] = [
            "cleaning.tools.flux2_klein.guidance_label",
            // The two halves of the ONE disabled control in this panel. A language that
            // lacks either leaves the user with a grey dial and no reason for it, which is
            // the exact bug report the explanation exists to prevent.
            "cleaning.tools.flux2_klein.guidance_unsupported_disabled_tooltip",
            "cleaning.tools.flux2_klein.guidance_unsupported_note",
            "cleaning.tools.flux2_klein.strength_hint",
            "cleaning.tools.flux2_klein.fixed_seed_hint",
            "cleaning.tools.flux2_klein.dtype_hint",
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
            "cleaning.tools.prompt_library.prompt_library_label",
            "cleaning.tools.flux2_klein.model_ready_status",
            "cleaning.tools.flux2_klein.model_readiness_unknown_status",
            "cleaning.tools.flux2_klein.install_hint",
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

    /// The shared prompt library (protocol 23): the reference catalogs carry every new line
    /// with the placeholders the code fills, and none of the retired save / load / family keys.
    /// The ids whose text was reworded under a new id (an install's on-disk catalog never
    /// overwrites an existing key) are gone from EVERY shipped catalog; es / fr / pt fall back
    /// to en for the new ones.
    #[test]
    fn the_reference_catalogs_carry_the_prompt_library_keys_and_not_the_retired_ones() {
        const RETIRED: [&str; 16] = [
            "cleaning.tools.flux2_klein.prompt_cache_load_button",
            "cleaning.tools.flux2_klein.prompt_cache_load_tooltip",
            "cleaning.tools.flux2_klein.prompt_cache_load_disabled_tooltip",
            "cleaning.tools.flux2_klein.prompt_cache_load_empty_error",
            "cleaning.tools.flux2_klein.prompt_cache_loaded_status",
            "cleaning.tools.flux2_klein.prompt_cache_loading_status",
            "cleaning.tools.flux2_klein.prompt_cache_save_button",
            "cleaning.tools.flux2_klein.prompt_cache_save_tooltip",
            "cleaning.tools.flux2_klein.prompt_cache_save_disabled_tooltip",
            "cleaning.tools.flux2_klein.prompt_cache_saved_status",
            "cleaning.tools.flux2_klein.prompt_cache_saving_status",
            "cleaning.tools.flux2_klein.prompt_cache_unverified_encoder_warning",
            "cleaning.tools.flux2_klein.prompt_cache_entry_family",
            "cleaning.tools.flux2_klein.prompt_cache_entry_with_family",
            "cleaning.tools.flux2_klein.prompt_cache_library_none",
            "cleaning.tools.flux2_klein.prompt_cache_import_foreign_warning",
        ];
        // Old id -> the id its reworded text lives under now.
        const RENAMED: [(&str, &str); 9] = [
            ("cleaning.tools.flux2_klein.prompt_cache_build_disabled_tooltip", "cleaning.tools.prompt_library.prompt_library_save_disabled_tooltip"),
            ("cleaning.tools.flux2_klein.prompt_cache_build_tooltip", "cleaning.tools.prompt_library.prompt_library_save_tooltip"),
            ("cleaning.tools.flux2_klein.prompt_cache_built_status", "cleaning.tools.prompt_library.prompt_library_saved_status"),
            ("cleaning.tools.flux2_klein.prompt_cache_encoder_missing_disabled_tooltip", "cleaning.tools.prompt_library.prompt_library_no_encoder_disabled_tooltip"),
            ("cleaning.tools.flux2_klein.prompt_cache_export_disabled_tooltip", "cleaning.tools.prompt_library.prompt_library_export_disabled_tooltip"),
            ("cleaning.tools.flux2_klein.prompt_cache_export_tooltip", "cleaning.tools.prompt_library.prompt_library_export_tooltip"),
            ("cleaning.tools.flux2_klein.prompt_cache_import_tooltip", "cleaning.tools.prompt_library.prompt_library_import_tooltip"),
            ("cleaning.tools.flux2_klein.prompt_cache_name_hint", "cleaning.tools.prompt_library.prompt_library_preset_name_hint"),
            ("cleaning.tools.flux2_klein.prompt_library_hint", "cleaning.tools.prompt_library.prompt_library_toggle_hint"),
        ];
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for (old, new) in RENAMED {
                assert!(catalog.get(old).is_none(), "locale `{tag}` still carries the renamed `{old}`");
                if matches!(*tag, "en" | "ru") {
                    let value = catalog.get(new).and_then(Value::as_str).unwrap_or_else(|| panic!("locale `{tag}` lacks `{new}`"));
                    assert!(!value.trim().is_empty(), "locale `{tag}`: `{new}` is empty");
                }
            }
        }
        const WITH_PLACEHOLDERS: [(&str, &[&str]); 14] = [
            ("cleaning.tools.prompt_library.prompt_cached_with_encoder_status", &["{encoder}"]),
            ("cleaning.tools.prompt_library.prompt_cache_memory_suffix", &[]),
            ("cleaning.tools.flux2_klein.encoder_caption", &["{source}", "{format}"]),
            ("cleaning.tools.flux2_klein.encoder_source_stock", &[]),
            ("cleaning.tools.flux2_klein.encoder_source_uncensored", &[]),
            ("cleaning.tools.prompt_library.prompt_cache_build_more_precise_status", &["{format}"]),
            ("cleaning.tools.prompt_library.prompt_cache_build_current_exists_status", &[]),
            ("cleaning.tools.prompt_library.prompt_cache_rename_button", &[]),
            ("cleaning.tools.prompt_library.prompt_cache_rename_tooltip", &[]),
            ("cleaning.tools.prompt_library.prompt_cache_name_tooltip", &[]),
            ("cleaning.tools.flux2_klein.prompt_cache_legacy_status", &["{migrated}", "{deleted}"]),
            ("cleaning.tools.flux2_klein.prompt_cache_legacy_left_status", &["{count}"]),
            ("cleaning.tools.prompt_library.prompt_cache_import_already_present_status", &["{name}"]),
            ("cleaning.tools.prompt_library.prompt_cache_skipped_status", &["{count}"]),
        ];
        for (tag, source) in ms_i18n::embedded_locales().iter().filter(|(tag, _)| matches!(*tag, "en" | "ru")) {
            let catalog: Value = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for key in RETIRED {
                assert!(catalog.get(key).is_none(), "locale `{tag}` still carries the retired `{key}`");
            }
            for (key, needed) in WITH_PLACEHOLDERS {
                let value = catalog.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("locale `{tag}` lacks `{key}`"));
                assert!(!value.trim().is_empty(), "locale `{tag}`: `{key}` is empty");
                for placeholder in needed {
                    assert!(value.contains(placeholder), "locale `{tag}`: `{key}` drops `{placeholder}`");
                }
            }
        }
    }

    /// The pinned run option and the forecast line it adds are a checkbox with no other
    /// explanation, so the two catalogs this project maintains by hand (`en`, the reference,
    /// and `ru`) must carry every one of their keys; `es` / `fr` / `pt` fall back to `en`.
    #[test]
    fn the_reference_catalogs_carry_the_text_attention_keys() {
        const KEYS: [&str; 3] = [
            "cleaning.tools.flux2_klein.text_attention_in_mask_label",
            "cleaning.tools.flux2_klein.text_attention_in_mask_hint",
            "cleaning.tools.flux2_klein.text_attention_in_mask_disabled_hint",
        ];
        for (tag, source) in ms_i18n::embedded_locales().iter().filter(|(tag, _)| matches!(*tag, "en" | "ru")) {
            let catalog: Value = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for key in KEYS {
                let value = catalog
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("locale `{tag}` lacks the key `{key}`"));
                assert!(!value.trim().is_empty(), "locale `{tag}`: `{key}` is empty");
            }
        }
    }

    /// The memory section's own lines: the retired preset/placement/estimate vocabulary — and
    /// the engine's own feather / colour-match keys, now the shared «Постобработка» section's
    /// (`ms_region_postprocess`) — is gone from both reference catalogs, and the new lines carry
    /// their placeholders.
    #[test]
    fn the_reference_catalogs_carry_the_step_era_keys_and_not_the_retired_ones() {
        const RETIRED_PREFIXES: [&str; 11] = [
            "cleaning.tools.flux2_klein.preset_",
            "cleaning.tools.flux2_klein.placement_",
            "cleaning.tools.flux2_klein.estimate_",
            "cleaning.tools.flux2_klein.low_cpu_mem_usage_",
            "cleaning.tools.flux2_klein.vae_slicing_",
            "cleaning.tools.flux2_klein.unload_before_vae_",
            "cleaning.tools.flux2_klein.unload_text_encoder_",
            "cleaning.tools.flux2_klein.text_encoder_fp8_hint_pointless",
            "cleaning.tools.flux2_klein.preset_fields_",
            "cleaning.tools.flux2_klein.mask_feather_",
            "cleaning.tools.flux2_klein.color_match_",
        ];
        const WITH_PLACEHOLDERS: [(&str, &[&str]); 4] = [
            ("cleaning.tools.flux2_klein.settings_save_error", &["{err}"]),
            ("cleaning.tools.flux2_klein.degraded_status", &["{vram}", "{ram}"]),
            ("cleaning.tools.flux2_klein.settings_malformed_status", &["{path}"]),
            ("cleaning.tools.flux2_klein.settings_save_blocked_error", &["{err}"]),
        ];
        for (tag, source) in ms_i18n::embedded_locales().iter().filter(|(tag, _)| matches!(*tag, "en" | "ru")) {
            let catalog: Value = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            let object = catalog.as_object().expect("a catalog is an object");
            for key in object.keys() {
                assert!(
                    !RETIRED_PREFIXES.iter().any(|prefix| key.starts_with(prefix)),
                    "locale `{tag}` still carries the retired `{key}`"
                );
            }
            for (key, needed) in WITH_PLACEHOLDERS {
                let value = object.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("locale `{tag}` lacks `{key}`"));
                for placeholder in needed {
                    assert!(value.contains(placeholder), "locale `{tag}`: `{key}` drops `{placeholder}`");
                }
            }
        }
    }
}
