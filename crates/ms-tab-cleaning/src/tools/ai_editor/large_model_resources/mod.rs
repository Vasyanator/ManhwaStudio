/*
File: ai_editor/large_model_resources/mod.rs

Purpose:
The two-axis memory widget of a LARGE-model cleaning engine («Видеопамять» ×
«Оперативная память»), drawn inside the engine's «Память и скорость» section from one
`large_models.profiles` answer: the two step pickers with their per-step forecasts, the
selected forecast, the text-encoder line, the reclaim note, the refusal with the backend's
suggestions, the learned floor with «Забыть», and the folded «Пределы» cap editor.

Key structures:
- `ResourcesView`: what the widget reads for one frame (the answer, its freshness, the caps
  buffer and the side operations' state).
- `ResourcesAction`: what the user did, folded back by the engine after the frame.
- `ResourcesController` (`controller.rs`): the engine-side state behind the widget — the
  forecast query and its arming, the caps writer, the floor reset — and the fold.

Key functions:
- `draw_large_model_resources()`: the widget.
- `draw_encoder_formats()`: the text encoder's weight-format rows, drawn separately by an
  engine whose encoder has several formats (FLUX.2 klein 4B).
- `step_row_label()`, `auto_item_label()`, `run_forecast()`, `forecast_line()`,
  `format_row_label()`, `cap_changes_for()`: the PURE decisions (unit tested; `forecast_line`
  — the whole run, text encoder included — is also what an engine's readiness line prints).

Notes:
- Model-agnostic: a model with one weight format (FLUX.2 klein 9B, and 4B outside download
  mode) passes no format; FLUX.3 klein and FLUX.2 klein 4B in download mode (its transformer
  format) pass `Some(&mut format)` and get the format rows. The encoder's rows (`encoder_formats`) are a
  separate call, so the widget's signature and actions serve every engine unchanged.
- Step SEMANTICS are the backend's (`runtime/large_models/tiers.py`); labels, hints and wire
  ids come from `ms_local_ai::large_models`. The widget talks to nothing: the engine's
  `ResourcesController` owns the `ProfilesQuery`, the `LimitsWriter` and the reset worker.
- No literals: every text is a `large_models.*` key (en + ru).
*/

// The engine-side state behind the widget: forecast query, caps writer, floor reset, fold.
mod controller;
pub(super) use controller::ResourcesController;

use eframe::egui;
use ms_local_ai::large_models::{
    Axis, AxisRows, CapChange, Forecast, FormatRow, LargeModelProfiles, LargeModelStep, LargeModelsError, Limits, SelectedPlan, StepChoice, StepRow,
    SuggestionKind, format_duration, format_gib,
};
use ms_widgets::{ByteCapEdit, WheelComboBox};

/// What the widget reads for one frame.
pub(super) struct ResourcesView<'a> {
    /// The last answer, possibly about an older question (`stale`).
    pub(super) profiles: Option<&'a LargeModelProfiles>,
    /// The answer describes another question than the current settings (shown dimmed).
    pub(super) stale: bool,
    /// A question is on the wire.
    pub(super) busy: bool,
    /// The current question failed.
    pub(super) error: Option<&'a LargeModelsError>,
    /// The AI backend is reachable; the caps can be edited only then.
    pub(super) connected: bool,
    /// The caps as edited; the widget writes into it and raises `LimitsEdited` naming the
    /// axis it changed.
    pub(super) limits_edit: &'a mut Limits,
    /// The last cap write's failure (already localized).
    pub(super) limits_error: Option<&'a str>,
    /// A learned-floor reset is in flight.
    pub(super) learned_reset_busy: bool,
    /// The last reset's failure (already localized).
    pub(super) learned_reset_error: Option<&'a str>,
}

impl ResourcesView<'_> {
    /// The same view for a shorter borrow, so a panel that holds one for the frame can hand
    /// it to [`draw_large_model_resources`] from inside a closure.
    pub(super) fn reborrow(&mut self) -> ResourcesView<'_> {
        ResourcesView {
            profiles: self.profiles,
            stale: self.stale,
            busy: self.busy,
            error: self.error,
            connected: self.connected,
            limits_edit: &mut *self.limits_edit,
            limits_error: self.limits_error,
            learned_reset_busy: self.learned_reset_busy,
            learned_reset_error: self.learned_reset_error,
        }
    }
}

/// What the user did this frame. The engine folds these back after the panel is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResourcesAction {
    /// A picker changed `choice` (already written into it).
    StepsChanged,
    /// A backend suggestion's step pair was taken.
    ApplySuggestion(StepChoice),
    /// A weight format was picked (only when the engine passes a format).
    ApplyFormat(String),
    /// One cap changed (already written into `limits_edit`); debounce and send THAT axis
    /// only — the other travels as `CapChange::Keep`, so another editor's change of it is
    /// never overwritten with this panel's stale value.
    LimitsEdited(Axis),
    /// «Забыть»: forget the learned floor of this model.
    ResetLearned,
    /// «Энкодер на CPU» was toggled (already written into the flag the widget was handed).
    EncoderOnCpuChanged,
}

/// The label of one picker row: the step, and what the answer says about it.
///
/// Above the learned floor -> "above the learned limit"; refused -> "does not fit"; the same
/// plan as a higher step -> "= «higher»"; otherwise the step with its peak and time when the
/// answer has them (the peak is device memory on the VRAM axis, the backend's whole RAM on
/// the RAM axis).
#[must_use]
pub(super) fn step_row_label<S: LargeModelStep>(row: &StepRow<S>) -> String {
    let step = row.id.label();
    if row.below_floor {
        return tf!("large_models.row.below_floor_label", step = step);
    }
    if !row.fits {
        return tf!("large_models.row.does_not_fit_label", step = step);
    }
    if let Some(other) = row.same_as {
        return tf!("large_models.row.same_as_label", step = step, other = other.label());
    }
    match (row.peak, row.total_s) {
        (Some(peak), Some(total)) => match S::AXIS {
            Axis::Vram => {
                tf!("large_models.row.vram_fits_label", step = step, vram = format_gib(peak), time = format_duration(total))
            }
            Axis::Ram => {
                tf!("large_models.row.ram_fits_label", step = step, ram = format_gib(peak), time = format_duration(total))
            }
        },
        (Some(_) | None, _) => step.to_string(),
    }
}

/// The `limits.set` pair for an edit of ONE `axis`: that axis as edited in `limits`, the
/// other `Keep`.
#[must_use]
pub(super) fn cap_changes_for(axis: Axis, limits: Limits) -> (CapChange, CapChange) {
    match axis {
        Axis::Vram => (CapChange::from_cap(limits.vram_cap), CapChange::Keep),
        Axis::Ram => (CapChange::Keep, CapChange::from_cap(limits.ram_cap)),
    }
}

/// The «Авто» entry of a picker: plain, or naming what it resolves to right now.
#[must_use]
pub(super) fn auto_item_label<S: LargeModelStep>(recommended: Option<S>) -> String {
    match recommended {
        Some(step) => tf!("large_models.step.auto_resolved_label", step = step.label()),
        None => S::AUTO.label().to_string(),
    }
}

/// The forecast of the WHOLE run an answer describes: the selected plan's, plus the text
/// encoder's when the answer carries one (`profiles.encoder`; the backend sends it exactly
/// when the run must count the encoder). `None` without a selected forecast.
///
/// Combined the way the backend's ADMISSION combines them (`service.py`
/// `large_model_profiles` / `_reservation_of`): the pipeline is planned with the encoder's
/// reservation CLAIMED beside it, so the two reservations add up. Device memory: the two
/// device peaks add. Host memory: both host peaks include the backend's baseline RSS, which
/// the process holds once, so the sum takes it out once (`Forecast::host_baseline`; a
/// backend that does not report it counts it twice, an over-estimate, never an under one).
/// Time: encode and denoise run one after the other, so loads and totals add, and a step's
/// time stays the pipeline's. Calibrated only when both halves are.
#[must_use]
pub(super) fn run_forecast(profiles: &LargeModelProfiles) -> Option<Forecast> {
    let pipeline = profiles.selected.forecast.as_ref()?;
    let Some(encoder) = profiles.encoder.as_ref().and_then(|encoder| encoder.forecast.as_ref()) else {
        return Some(pipeline.clone());
    };
    let baseline = encoder.host_baseline.or(pipeline.host_baseline).unwrap_or(0).min(encoder.host_peak);
    Some(Forecast {
        device_peak: pipeline.device_peak.saturating_add(encoder.device_peak),
        host_peak: pipeline.host_peak.saturating_add(encoder.host_peak - baseline),
        peak_phase: pipeline.peak_phase.clone(),
        load_cold_s: pipeline.load_cold_s + encoder.load_cold_s,
        load_warm_s: pipeline.load_warm_s + encoder.load_warm_s,
        per_step_s: pipeline.per_step_s,
        total_s: pipeline.total_s + encoder.total_s,
        calibrated: pipeline.calibrated && encoder.calibrated,
        device_calibrated: pipeline.device_calibrated && encoder.device_calibrated,
        host_baseline: pipeline.host_baseline,
    })
}

/// The one-line forecast of the whole run ([`run_forecast`]): peaks and the time of a
/// repeated run, with the first run's time beside it. `None` without a forecast. Shared by
/// the widget and the engine's always-visible readiness line, so the two cannot read
/// differently.
#[must_use]
pub(super) fn forecast_line(profiles: &LargeModelProfiles) -> Option<String> {
    run_forecast(profiles).as_ref().map(forecast_text)
}

fn forecast_text(forecast: &Forecast) -> String {
    tf!(
        "large_models.forecast.line_status",
        vram = format_gib(forecast.device_peak),
        ram = format_gib(forecast.host_peak),
        total = format_duration(forecast.warm_total_s()),
        cold = format_duration(forecast.total_s)
    )
}

/// The text encoder's forecast line. With «Энкодер на CPU» on the encoder never takes video
/// memory, so the line names its RAM alone — a device figure there (the backend's, whatever
/// it reports) would read as if the box did nothing.
#[must_use]
pub(super) fn encoder_line(encoder: &Forecast, cpu_only: bool) -> String {
    if cpu_only {
        tf!("large_models.forecast.encoder_cpu_status", ram = format_gib(encoder.host_peak))
    } else {
        tf!("large_models.forecast.encoder_status", vram = format_gib(encoder.device_peak), ram = format_gib(encoder.host_peak))
    }
}

/// The text of a refused plan: the backend's reason, or the generic one without it.
fn refusal_text(selected: &SelectedPlan) -> String {
    selected
        .reason
        .as_ref()
        .map_or_else(|| t!("large_models.refusal.device_short_error").to_string(), |reason| reason.user_message())
}

/// Draws the widget and returns what the user did.
///
/// `id_salt` namespaces every stored widget state (the two pickers, the «Пределы» fold), so
/// two engines never share one; `choice` is the user's step pair and is written in place
/// (`StepsChanged`); `format` is the weight-format choice of a model that has several, `None`
/// for one that has one; `encoder_on_cpu` is the «Энкодер на CPU» flag of a model with a
/// separate text encoder (`EncoderOnCpuChanged`), `None` for one without — with the flag on, the
/// encoder obeys only the RAM step, so its forecast line names RAM alone.
pub(super) fn draw_large_model_resources(
    ui: &mut egui::Ui,
    id_salt: &str,
    choice: &mut StepChoice,
    format: Option<&mut String>,
    encoder_on_cpu: Option<&mut bool>,
    view: ResourcesView<'_>,
) -> Vec<ResourcesAction> {
    let mut actions = Vec::new();
    let ResourcesView { profiles, stale, busy, error, connected, limits_edit, limits_error, learned_reset_busy, learned_reset_error } = view;
    ui.push_id(id_salt, |ui| {
        let vram_applies = profiles.is_none_or(|profiles| profiles.vram.applicable);
        if vram_applies {
            let rows = profiles.map(|profiles| &profiles.vram);
            if draw_axis_picker(ui, "large_models.vram.axis_label", t!("large_models.vram.axis_label"), t!("large_models.vram.axis_hint"), &mut choice.vram, rows) {
                actions.push(ResourcesAction::StepsChanged);
            }
        }
        let rows = profiles.map(|profiles| &profiles.ram);
        if draw_axis_picker(ui, "large_models.ram.axis_label", t!("large_models.ram.axis_label"), t!("large_models.ram.axis_hint"), &mut choice.ram, rows) {
            actions.push(ResourcesAction::StepsChanged);
        }
        // Read before the checkbox borrows the flag: the forecast lines below describe it.
        let encoder_cpu_only = encoder_on_cpu.as_deref().copied().unwrap_or(false);
        if let Some(encoder_on_cpu) = encoder_on_cpu
            && ui
                .checkbox(encoder_on_cpu, t!("large_models.encoder_on_cpu_label"))
                .on_hover_text(t!("large_models.encoder_on_cpu_hint"))
                .changed()
        {
            actions.push(ResourcesAction::EncoderOnCpuChanged);
        }

        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small(t!("large_models.forecast.busy_status"));
            });
        }
        if let Some(error) = error {
            ui.colored_label(ms_theme::status::ERROR, tf!("large_models.forecast.error_status", err = error.user_message()));
        }

        let mut open_limits = false;
        if let Some(profiles) = profiles {
            draw_selected(ui, profiles, stale, encoder_cpu_only);
            if !profiles.selected.fits {
                open_limits = draw_refusal(ui, profiles, format.is_some(), &mut actions);
            }
            if let Some(format) = format {
                draw_formats(ui, profiles, format, &mut actions);
            }
            if let Some(floor) = profiles.learned_floor.as_ref() {
                let any = || t!("large_models.step.no_floor_label").to_string();
                ui.horizontal_wrapped(|ui| {
                    ui.small(tf!(
                        "large_models.floor.status",
                        failure = floor.reason.label(),
                        vram = floor.vram.map_or_else(any, |step| step.label().to_string()),
                        ram = floor.ram.map_or_else(any, |step| step.label().to_string())
                    ));
                    if ui
                        .add_enabled(connected && !learned_reset_busy, egui::Button::new(t!("large_models.floor.reset_button")).small())
                        .on_hover_text(t!("large_models.floor.reset_tooltip"))
                        .on_disabled_hover_text(t!("large_models.limits.offline_hint"))
                        .clicked()
                    {
                        actions.push(ResourcesAction::ResetLearned);
                    }
                });
            }
            if let Some(error) = learned_reset_error {
                ui.colored_label(ms_theme::status::ERROR, error);
            }
        } else if !busy && error.is_none() {
            ui.small(t!("large_models.forecast.unknown_status"));
        }

        let probed = profiles.map(|profiles| profiles.limits);
        egui::CollapsingHeader::new(t!("large_models.limits.heading_label"))
            .id_salt("large_models.limits.heading_label")
            .open(open_limits.then_some(true))
            .show(ui, |ui| {
                if connected {
                    let vram_edited = ui
                        .add(
                            ByteCapEdit::new(
                                &mut limits_edit.vram_cap,
                                "large_models.limits.vram_cap_label",
                                t!("large_models.limits.vram_cap_label"),
                                t!("large_models.limits.unlimited_label"),
                                t!("large_models.limits.gib_suffix"),
                            )
                            .max_bytes(probed.and_then(|limits| limits.vram_total)),
                        )
                        .changed();
                    let ram_edited = ui
                        .add(
                            ByteCapEdit::new(
                                &mut limits_edit.ram_cap,
                                "large_models.limits.ram_cap_label",
                                t!("large_models.limits.ram_cap_label"),
                                t!("large_models.limits.unlimited_label"),
                                t!("large_models.limits.gib_suffix"),
                            )
                            .max_bytes(probed.and_then(|limits| limits.ram_total)),
                        )
                        .changed();
                    if vram_edited {
                        actions.push(ResourcesAction::LimitsEdited(Axis::Vram));
                    }
                    if ram_edited {
                        actions.push(ResourcesAction::LimitsEdited(Axis::Ram));
                    }
                } else {
                    // Not drawn as disabled editors: the backend is the caps' only writer, and a
                    // value typed while it is down could never be sent.
                    ui.small(t!("large_models.limits.offline_hint"));
                }
                ui.small(t!("large_models.limits.scope_hint"));
                if let Some(error) = limits_error {
                    ui.colored_label(ms_theme::status::ERROR, error);
                }
            });
    });
    actions
}

/// One step picker: «Авто» plus the concrete steps with what the answer says about each.
/// Returns `true` when the user picked another step.
fn draw_axis_picker<S: LargeModelStep>(
    ui: &mut egui::Ui,
    id_salt: &'static str,
    label: &str,
    hint: &str,
    value: &mut S,
    rows: Option<&AxisRows<S>>,
) -> bool {
    let row_of = |step: S| rows.and_then(|rows| rows.steps.iter().find(|row| row.id == step));
    let item_label = |step: S| {
        if step == S::AUTO {
            auto_item_label(rows.and_then(|rows| rows.recommended))
        } else {
            row_of(step).map_or_else(|| step.label().to_string(), step_row_label)
        }
    };
    let selected_text = if *value == S::AUTO { item_label(S::AUTO) } else { value.label().to_string() };
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(label).on_hover_text(hint);
        WheelComboBox::from_id_salt(id_salt)
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                for step in std::iter::once(S::AUTO).chain(S::CONCRETE.iter().copied()) {
                    if ui.selectable_label(*value == step, item_label(step)).on_hover_text(step.hint()).clicked() {
                        picked = Some(step);
                    }
                }
            })
            .response
            .on_hover_text(value.hint());
    });
    match picked {
        Some(step) if step != *value => {
            *value = step;
            true
        }
        Some(_) | None => false,
    }
}

/// The selected plan: the WHOLE run's forecast ([`run_forecast`], with the uncalibrated
/// note), the encoder line as its breakdown, and the reclaim line. A stale answer is drawn
/// weak, so the panel does not flicker while the next lands. `encoder_cpu_only`: «Энкодер на
/// CPU» is on, so the encoder line names RAM alone ([`encoder_line`]).
fn draw_selected(ui: &mut egui::Ui, profiles: &LargeModelProfiles, stale: bool, encoder_cpu_only: bool) {
    let selected = &profiles.selected;
    if let Some(forecast) = run_forecast(profiles) {
        if !forecast.calibrated {
            ui.small(t!("large_models.forecast.uncalibrated_hint"));
        }
        let line = forecast_text(&forecast);
        let tooltip = tf!("large_models.forecast.peak_phase_tooltip", phase = forecast.peak_phase);
        if stale {
            ui.weak(line).on_hover_text(tooltip);
        } else if selected.fits {
            ui.small(line).on_hover_text(tooltip);
        } else {
            ui.colored_label(ms_theme::status::WARNING, line).on_hover_text(tooltip);
        }
    }
    if let Some(encoder) = profiles.encoder.as_ref().and_then(|encoder| encoder.forecast.as_ref()) {
        ui.small(encoder_line(encoder, encoder_cpu_only));
    }
    if let Some(reclaim) = selected.needs_reclaim.filter(|reclaim| reclaim.device_bytes > 0 || reclaim.host_bytes > 0) {
        ui.small(tf!(
            "large_models.forecast.reclaim_status",
            vram = format_gib(reclaim.device_bytes),
            ram = format_gib(reclaim.host_bytes)
        ));
    }
}

/// The refusal line and the backend's suggestions. Returns `true` when «Пределы» must open
/// (a raise-limits suggestion is a pointer at it, not a button).
fn draw_refusal(ui: &mut egui::Ui, profiles: &LargeModelProfiles, offers_formats: bool, actions: &mut Vec<ResourcesAction>) -> bool {
    ui.colored_label(ms_theme::status::ERROR, refusal_text(&profiles.selected));
    let mut open_limits = false;
    for suggestion in &profiles.suggestions {
        match &suggestion.kind {
            SuggestionKind::Steps => {
                if let Some(pair) = suggestion.choice
                    && ui
                        .small_button(tf!("large_models.suggestion.steps_button", vram = pair.vram.label(), ram = pair.ram.label()))
                        .clicked()
                {
                    actions.push(ResourcesAction::ApplySuggestion(pair));
                }
            }
            SuggestionKind::RaiseLimits => {
                ui.small(t!("large_models.suggestion.raise_limits_hint"));
                open_limits = true;
            }
            SuggestionKind::Format => {
                if offers_formats
                    && let Some(format) = suggestion.format.as_ref()
                    && ui.small_button(tf!("large_models.suggestion.format_button", format = format)).clicked()
                {
                    actions.push(ResourcesAction::ApplyFormat(format.clone()));
                }
            }
            // A kind this build does not know offers nothing it could not explain.
            SuggestionKind::Other(_) => {}
        }
    }
    open_limits
}

/// The weight-format rows of a model with several formats.
fn draw_formats(ui: &mut egui::Ui, profiles: &LargeModelProfiles, format: &mut String, actions: &mut Vec<ResourcesAction>) {
    if profiles.formats.len() < 2 {
        return;
    }
    ui.label(t!("large_models.format.heading_label"));
    if let Some(picked) = draw_format_rows(ui, &profiles.formats, format) {
        actions.push(ResourcesAction::ApplyFormat(picked));
    }
}

/// Draws the TEXT ENCODER's weight-format rows (`profiles.encoder_formats`) under their own
/// heading, with the same row labels and visuals as the model's format rows. Draws nothing
/// with fewer than two rows (one format offers no choice; older backends and FLUX.3 report
/// none).
///
/// `format` is the engine's encoder-format choice; a picked row is written into it and its id
/// returned (`None` when nothing new was picked). Separate from
/// [`draw_large_model_resources`] so that widget's signature and [`ResourcesAction`] stay the
/// same for every engine.
pub(super) fn draw_encoder_formats(ui: &mut egui::Ui, profiles: &LargeModelProfiles, format: &mut String) -> Option<String> {
    if profiles.encoder_formats.len() < 2 {
        return None;
    }
    ui.label(t!("large_models.format.encoder_heading_label"));
    draw_format_rows(ui, &profiles.encoder_formats, format)
}

/// The label of one format row: the format and the step pair `Auto` resolves to in it, or
/// that it does not fit.
#[must_use]
fn format_row_label(row: &FormatRow) -> String {
    match row.fits_best {
        Some(pair) => tf!("large_models.format.row_fits_label", format = row.id, vram = pair.vram.label(), ram = pair.ram.label()),
        None => tf!("large_models.format.row_does_not_fit_label", format = row.id),
    }
}

/// The selectable rows of one format list (an unavailable row is disabled and says why on
/// hover). A click on another row writes it into `format` and returns its id.
fn draw_format_rows(ui: &mut egui::Ui, rows: &[FormatRow], format: &mut String) -> Option<String> {
    let mut picked = None;
    for row in rows {
        let reason = row.reason.clone().unwrap_or_default();
        let response = ui
            .add_enabled(row.available, egui::Button::selectable(*format == row.id, format_row_label(row)))
            .on_disabled_hover_text(tf!("large_models.format.unavailable_tooltip", reason = reason));
        if response.clicked() && *format != row.id {
            format.clone_from(&row.id);
            picked = Some(row.id.clone());
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_local_ai::large_models::{RamStep, VramStep};
    use serde_json::Value;

    fn row<S: LargeModelStep>(id: S) -> StepRow<S> {
        StepRow { id, fits: true, same_as: None, below_floor: false, peak: Some(1 << 30), total_s: Some(12.0) }
    }

    /// Installs the English reference catalog under the crate's shared locale lock, as the
    /// other UI-string tests here do (`t!` reads the PROCESS-GLOBAL catalog).
    fn english() -> std::sync::MutexGuard<'static, ()> {
        let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");
        guard
    }

    #[test]
    fn a_row_label_says_what_the_answer_says_about_the_step() {
        let _locale = english();
        assert_eq!(step_row_label(&row(VramStep::Phase)), "Current stage only — ≈ 1.0 GiB · ~12 s");
        assert_eq!(step_row_label(&row(RamStep::Keep)), "Keep models in memory — ≈ 1.0 GiB RAM · ~12 s");
        assert_eq!(step_row_label(&StepRow { fits: false, ..row(VramStep::Max) }), "Maximum speed — does not fit");
        assert_eq!(step_row_label(&StepRow { same_as: Some(VramStep::Half), ..row(VramStep::Min) }), "Minimum — = «At most half»");
        assert_eq!(
            step_row_label(&StepRow { below_floor: true, fits: false, ..row(VramStep::Max) }),
            "Maximum speed — above the learned limit",
            "a step above the learned limit says so even when it would not fit either"
        );
        assert_eq!(step_row_label(&StepRow { peak: None, ..row(RamStep::Run) }), "During the task only", "no figures: the bare step");
    }

    #[test]
    fn a_cap_edit_sends_only_its_own_axis() {
        let limits = Limits { vram_cap: Some(8), ram_cap: None };
        assert_eq!(cap_changes_for(Axis::Vram, limits), (CapChange::Set(8), CapChange::Keep));
        assert_eq!(cap_changes_for(Axis::Ram, limits), (CapChange::Keep, CapChange::Clear));
    }

    #[test]
    fn the_auto_item_names_what_it_resolves_to() {
        let _locale = english();
        assert_eq!(auto_item_label(Some(VramStep::Phase)), "Auto (now «Current stage only»)");
        assert_eq!(auto_item_label::<RamStep>(None), "Auto");
    }

    /// With «Энкодер на CPU» on, the encoder line names RAM only; off, both memories.
    #[test]
    fn the_encoder_line_is_ram_only_when_the_encoder_is_on_the_cpu() {
        let _locale = english();
        let encoder = Forecast {
            device_peak: 1 << 31,
            host_peak: 1 << 33,
            peak_phase: "encode".to_string(),
            load_cold_s: 1.0,
            load_warm_s: 1.0,
            per_step_s: 0.0,
            total_s: 1.0,
            calibrated: true,
            device_calibrated: true,
            host_baseline: None,
        };
        assert_eq!(encoder_line(&encoder, true), "Text encoder (on the CPU): ≈ 8.0 GiB RAM");
        assert_eq!(encoder_line(&encoder, false), "Text encoder: ≈ 2.0 GiB video memory, 8.0 GiB RAM");
    }

    /// The pinned v13 profiles answer, for tests that need a whole `LargeModelProfiles`.
    fn fixture_profiles() -> LargeModelProfiles {
        ms_local_ai::large_models::parse_profiles(
            &serde_json::from_str::<Value>(include_str!("../../../../../ms-local-ai/fixtures/large_models_profiles_v13.json")).expect("fixture JSON"),
        )
        .expect("fixture parses")
    }

    /// The pipeline's forecast of the line tests: 1.5 GiB device, 4 GiB host of which 1 GiB is
    /// the backend's baseline.
    fn pipeline_forecast() -> Forecast {
        Forecast {
            device_peak: 3 << 29,
            host_peak: 1 << 32,
            peak_phase: "denoise".to_string(),
            load_cold_s: 30.0,
            load_warm_s: 3.0,
            per_step_s: 1.0,
            total_s: 40.0,
            calibrated: true,
            device_calibrated: true,
            host_baseline: Some(1 << 30),
        }
    }

    #[test]
    fn the_forecast_line_exists_only_with_a_forecast() {
        let _locale = english();
        let mut profiles = fixture_profiles();
        profiles.encoder = None;
        profiles.selected.forecast = None;
        assert_eq!(forecast_line(&profiles), None);
        profiles.selected.forecast = Some(pipeline_forecast());
        // The repeated run swaps the cold load (30 s) for the warm one (3 s): 40 - 30 + 3.
        assert_eq!(
            forecast_line(&profiles).as_deref(),
            Some("≈ 1.5 GiB video memory, 4.0 GiB RAM, ~13 s (40 s on the first run)")
        );
        let mut selected = profiles.selected.clone();
        selected.reason = None;
        assert_eq!(refusal_text(&selected), "Does not fit into video memory.", "no reason: the generic one");
        selected.reason = Some(ms_local_ai::large_models::RefusalReason::HostShort);
        assert_eq!(refusal_text(&selected), "Not enough system memory.");
    }

    /// With an encoder in the answer, the run's forecast is the sum the backend's admission
    /// assumes: device peaks add, host peaks add with the shared baseline taken out once,
    /// times add; without a reported baseline the host sum errs high, never low.
    #[test]
    fn the_run_forecast_adds_the_text_encoder_as_admission_does() {
        let _locale = english();
        let mut profiles = fixture_profiles();
        profiles.selected.forecast = Some(pipeline_forecast());
        let encoder_forecast = Forecast {
            device_peak: 1 << 30,
            host_peak: (1 << 30) + (1 << 31),
            peak_phase: "encode".to_string(),
            load_cold_s: 10.0,
            load_warm_s: 1.0,
            per_step_s: 0.0,
            total_s: 12.0,
            calibrated: false,
            device_calibrated: true,
            host_baseline: Some(1 << 30),
        };
        let encoder = profiles.encoder.as_mut().expect("the fixture carries the encoder plan");
        encoder.forecast = Some(encoder_forecast.clone());
        let total = run_forecast(&profiles).expect("a forecast");
        assert_eq!(total.device_peak, (3 << 29) + (1 << 30), "device peaks add");
        assert_eq!(total.host_peak, (1 << 32) + (1 << 31), "the encoder adds its growth, not a second baseline");
        assert!((total.total_s - 52.0).abs() < 1e-9 && (total.load_cold_s - 40.0).abs() < 1e-9 && (total.load_warm_s - 4.0).abs() < 1e-9);
        assert!((total.per_step_s - 1.0).abs() < 1e-9, "a denoise step is the pipeline's");
        assert!(!total.calibrated, "calibrated only when both halves are");
        // 2.5 GiB device, 6 GiB host, repeated 52 - 40 + 4 = 16 s.
        assert_eq!(forecast_line(&profiles).as_deref(), Some("≈ 2.5 GiB video memory, 6.0 GiB RAM, ~16 s (52 s on the first run)"));

        let encoder = profiles.encoder.as_mut().expect("the fixture carries the encoder plan");
        encoder.forecast = Some(Forecast { host_baseline: None, ..encoder_forecast });
        profiles.selected.forecast = Some(Forecast { host_baseline: None, ..pipeline_forecast() });
        assert_eq!(run_forecast(&profiles).map(|total| total.host_peak), Some((1 << 32) + (1 << 30) + (1 << 31)), "no baseline: counted twice, over not under");

        profiles.encoder = None;
        assert_eq!(run_forecast(&profiles), profiles.selected.forecast, "no encoder: the plan's own forecast");
    }

    fn format_row(id: &str, fits_best: Option<StepChoice>) -> FormatRow {
        FormatRow {
            id: id.to_string(),
            available: true,
            reason: None,
            dit_bytes: None,
            compute_path: None,
            quality_rank: None,
            fits_best,
        }
    }

    #[test]
    fn a_format_row_label_names_where_auto_lands_or_that_it_does_not_fit() {
        let _locale = english();
        let pair = StepChoice { vram: VramStep::Phase, ram: RamStep::Keep };
        assert_eq!(format_row_label(&format_row("q4_k_m", Some(pair))), "q4_k_m — fits as «Current stage only» / «Keep models in memory»");
        assert_eq!(format_row_label(&format_row("bf16", None)), "bf16 — does not fit");
    }

    /// The encoder rows draw only when there is a choice (two rows or more), and drawing them
    /// picks nothing by itself.
    #[test]
    fn the_encoder_rows_draw_only_with_a_choice() {
        let mut profiles = fixture_profiles();
        let ctx = egui::Context::default();
        let mut format = "bf16".to_string();
        let draw = |profiles: &LargeModelProfiles, format: &mut String| {
            let mut picked = Some(String::new());
            let output = ctx.run_ui(egui::RawInput::default(), |ui| picked = draw_encoder_formats(ui, profiles, format));
            let shapes = output.shapes.len();
            // Headless: egui 0.36 panics when a `TexturesDelta` is dropped unapplied.
            output.drop_without_applying_deltas();
            (shapes, picked)
        };
        let (empty, picked) = draw(&profiles, &mut format);
        assert_eq!(picked, None);
        profiles.encoder_formats = vec![format_row("bf16", None)];
        assert_eq!(draw(&profiles, &mut format), (empty, None), "one row offers no choice: nothing drawn");
        profiles.encoder_formats.push(format_row("q8_0", None));
        let (drawn, picked) = draw(&profiles, &mut format);
        assert!(drawn > empty, "two rows are drawn");
        assert_eq!(picked, None);
        assert_eq!(format, "bf16", "drawing does not change the choice");
    }

    /// Every `large_models.*` / `ai_backend.large_models.*` key is in BOTH hand-maintained
    /// catalogs with the same `{placeholders}` — a unit test cannot see that through `t!`.
    #[test]
    fn the_reference_catalogs_agree_on_every_large_model_key() {
        let catalogs: Vec<(&str, Value)> = ms_i18n::embedded_locales()
            .iter()
            .filter(|(tag, _)| matches!(*tag, "en" | "ru"))
            .map(|(tag, source)| (*tag, serde_json::from_str(source).unwrap_or_else(|err| panic!("locale `{tag}`: {err}"))))
            .collect();
        assert_eq!(catalogs.len(), 2);
        let placeholders = |text: &str| -> Vec<String> {
            let mut out: Vec<String> = text.split('{').skip(1).filter_map(|rest| rest.split_once('}').map(|(name, _)| name.to_string())).collect();
            out.sort();
            out
        };
        let (_, en) = &catalogs[0];
        let (_, ru) = &catalogs[1];
        let en = en.as_object().expect("en is an object");
        let mut seen = 0;
        for (key, value) in en.iter().filter(|(key, _)| key.starts_with("large_models.") || key.starts_with("ai_backend.large_models.")) {
            seen += 1;
            let en_text = value.as_str().unwrap_or_default();
            let ru_text = ru.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("ru lacks `{key}`"));
            assert!(!ru_text.trim().is_empty() && !en_text.trim().is_empty(), "`{key}` is empty");
            assert_eq!(placeholders(en_text), placeholders(ru_text), "`{key}`: the placeholders differ");
        }
        assert!(seen > 40, "the large-model keys are missing from en");
        // The axis names are fixed by the user's decision, verbatim.
        assert_eq!(ru.get("large_models.vram.axis_label").and_then(Value::as_str), Some("Видеопамять"));
        assert_eq!(ru.get("large_models.ram.axis_label").and_then(Value::as_str), Some("Оперативная память"));
    }
}
