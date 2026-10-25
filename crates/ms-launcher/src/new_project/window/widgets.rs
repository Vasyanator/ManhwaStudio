/*
File: crates/ms-launcher/src/new_project/window/widgets.rs

Purpose:
Small UI helpers shared by the New Project window files: progress display, section and field
layout helpers, index combos (on `WheelComboBox`), sized buttons and the batch child window's close path.

Key items:
- `current_progress`, `show_operation_progress`, `progress_fraction` / `progress_value` / `progress_label`.
- `section_group`, `sub_group`, `field_label*`, `combo_index*`, `button_sized`.
- `close_batch_window`: the one close path of the batch-processing child window.

Notes:
No state of its own beyond what the callers pass in.
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn current_progress(&self, any_loading: bool) -> ProgressDisplay {
        if let Some(progress) = self.active_progress.as_ref() {
            let stage_name = progress_stage_title(&progress.stage);
            let label = if progress.total == 0 {
                format!(
                    "{}: {stage_name}",
                    progress_operation_title(progress.operation)
                )
            } else {
                format!(
                    "{}: {stage_name} ({}/{})",
                    progress_operation_title(progress.operation),
                    progress.current.min(progress.total),
                    progress.total,
                )
            };
            return ProgressDisplay {
                fraction: progress_fraction(progress.current, progress.total),
                label,
            };
        }

        ProgressDisplay {
            fraction: progress_value(any_loading),
            label: progress_label(any_loading).to_string(),
        }
    }

    pub(super) fn show_operation_progress(&self, ui: &mut Ui, operation: &'static str) {
        let Some(progress) = self.active_progress.as_ref() else {
            return;
        };
        if progress.operation != operation {
            return;
        }
        ui.add_space(8.0);
        ui.label(
            RichText::new(tf!("launcher.new_project.stage_status", progress_stage_title = progress_stage_title(&progress.stage)))
                .small()
                .weak(),
        );
        ui.add(
            ProgressBar::new(progress_fraction(progress.current, progress.total))
                .animate(true)
                .desired_width(fill_width(ui))
                .text(progress_status_label(
                    &progress.stage,
                    progress.current,
                    progress.total,
                )),
        );
    }
}

/// Closes the batch-processing window (if open) and drops what its opening started
/// (`on_window_closed`). Every close path goes through here: its X / toolbar Close, and the
/// close of its parent New Project window.
pub(super) fn close_batch_window(window: &mut EmbeddedWindowState, batch: &mut BatchProcessingWindowState) {
    if window.is_open() {
        window.close();
        batch.on_window_closed();
    }
}

pub(super) fn progress_status_label(stage: &str, current: usize, total: usize) -> String {
    if stage == "collect_canvas" && total == 0 && current > 0 {
        return tf!("launcher.new_project.canvas_collect_status", current = current);
    }
    let prefix = progress_stage_title(stage);
    if total == 0 {
        format!("{prefix}...")
    } else {
        format!("{prefix}: {}/{}", current.min(total), total)
    }
}

pub(super) fn section_group(ui: &mut Ui, title: &str, add_body: impl FnOnce(&mut Ui)) {
    Frame::group(ui.style())
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.label(RichText::new(title).size(18.0).strong());
            ui.add_space(10.0);
            add_body(ui);
        });
}

pub(super) fn sub_group(ui: &mut Ui, title: &str, add_body: impl FnOnce(&mut Ui)) {
    Frame::group(ui.style())
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.label(RichText::new(title).size(15.0).strong());
            ui.add_space(8.0);
            add_body(ui);
        });
}

pub(super) fn field_label(ui: &mut Ui, label: &str) {
    ui.label(RichText::new(label).small());
}

pub(super) fn field_label_hover(ui: &mut Ui, label: &str, hover_text: &str) {
    ui.label(RichText::new(label).small())
        .on_hover_text(hover_text);
}

pub(super) fn numeric_text_field(ui: &mut Ui, label: &str, value: &mut String) {
    field_label(ui, label);
    ui.add(TextEdit::singleline(value).desired_width(160.0));
}

pub(super) fn selected_label<'a>(values: &'a [&str], selected: usize, field_name: &str) -> Option<&'a str> {
    match values.get(selected).copied() {
        Some(value) => Some(value),
        None => {
            ms_log::runtime_log::log_error(format!(
                "[new-project] invalid combo index for {field_name}: {selected}"
            ));
            None
        }
    }
}

/// Index combo over borrowed labels: shows `values[*selected]` ("—" for an out-of-range index),
/// a click selects a row, and the wheel over the closed combo cycles the index.
pub(super) fn combo_index(ui: &mut Ui, id: &str, values: &[&str], selected: &mut usize) {
    combo_index_by(ui, id, values.len(), selected, |index| values.get(index).copied());
}

/// [`combo_index`] over owned labels.
pub(super) fn combo_index_owned(ui: &mut Ui, id: &str, values: &[String], selected: &mut usize) {
    combo_index_by(ui, id, values.len(), selected, |index| values.get(index).map(String::as_str));
}

/// [`combo_index`] over `(label, value)` pairs; only the label is shown.
pub(super) fn combo_index_pairs(ui: &mut Ui, id: &str, values: &[(String, String)], selected: &mut usize) {
    combo_index_by(ui, id, values.len(), selected, |index| {
        values.get(index).map(|(name, _)| name.as_str())
    });
}

/// Shared body of the `combo_index*` helpers on the project `WheelComboBox` (stock
/// `egui::ComboBox` is forbidden in product UI, `PROJECT_RULES.md`): `label(i)` is `Some` for
/// every `i < len`. Wheel steps over the closed combo cycle `*selected` through `0..len`.
fn combo_index_by<'a>(
    ui: &mut Ui,
    id: &str,
    len: usize,
    selected: &mut usize,
    label: impl Fn(usize) -> Option<&'a str>,
) {
    let combo = WheelComboBox::from_id_salt(id)
        .width(fill_width(ui))
        .selected_text(label(*selected).unwrap_or("—"))
        .show_ui_with_wheel(ui, |ui| {
            for index in 0..len {
                if let Some(value) = label(index) {
                    ui.selectable_value(selected, index, value);
                }
            }
        });
    if let Some(steps) = combo.wheel_steps {
        *selected = cycle_wrapped_index(*selected, len, steps);
    }
}

pub(super) fn fill_width(ui: &Ui) -> f32 {
    safe_dimension(ui.available_width(), 120.0)
}

pub(super) fn safe_dimension(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 1.0 {
        value
    } else {
        fallback
    }
}

pub(super) fn button_sized(ui: &mut Ui, label: &str, size: egui::Vec2, enabled: bool) -> egui::Response {
    ui.add_enabled(enabled, Button::new(label).min_size(size))
}

pub(super) fn progress_value(is_loading: bool) -> f32 {
    if is_loading { 0.0 } else { 1.0 }
}

pub(super) fn progress_label(is_loading: bool) -> &'static str {
    if is_loading {
        t!("launcher.common.loading")
    } else {
        t!("launcher.common.done")
    }
}

pub(super) fn progress_fraction(current: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        (current.min(total) as f32 / total as f32).clamp(0.0, 1.0)
    }
}
