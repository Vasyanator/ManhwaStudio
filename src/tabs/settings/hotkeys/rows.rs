/*
File: src/tabs/settings/hotkeys/rows.rs

Purpose:
The grouped action list of the hotkeys pane: one collapsible block per group (session, view,
canvas, shared tool contexts, each tab), section headings from the declared section keys, and
per action its title, kind badge, one line per alternative (label, Rebind, Remove, the axis or
gesture editor, its conflicts and shadows), Add / Reset, and read-only fixed actions.

Key functions:
- `draw_groups`: the whole list for one frame (search filter, jump / highlight).

Notes:
- Drawing only READS the registry; every click becomes an `Intent` the pane applies afterwards.
- Every editor is disabled while a capture runs (the capture owns the input until it ends).
*/

use std::time::Duration;

use ms_config::app_tab::AppTab;
use ms_input::{ActionId, ActionKind, AxisBinding, AxisSource, Binding, Button, Chord, GestureBinding, GestureKind, InputRegistry, TouchSource};
use web_time::Instant;

use super::model::{self, Group, SectionBlock};
use super::{HotkeysPane, Intent};
use crate::widgets::{SearchableComboBox, SearchableComboItem, WheelSpinBox};

/// How long an action stays framed after a jump from the conflict summary.
const HIGHLIGHT_DURATION: Duration = Duration::from_secs(2);
/// The selectable axis sources, in display order.
const AXIS_SOURCES: [AxisSource; 5] = [AxisSource::WheelY, AxisSource::WheelX, AxisSource::Pinch, AxisSource::DragX, AxisSource::DragY];
/// The selectable gesture kinds, in display order.
const GESTURE_KINDS: [GestureKind; 4] = [GestureKind::Click, GestureKind::DoubleClick, GestureKind::Drag, GestureKind::LongPress];
/// The touch sources the touch router recognizes (the fixed catalog of touch alternatives).
const TOUCH_SOURCES: [TouchSource; 5] = [TouchSource::LongPress, TouchSource::LongPressDrag, TouchSource::TwoFingerPinch, TouchSource::TwoFingerPan, TouchSource::OneFingerPan];
/// Accepted axis sensitivity range (the model requires finite and > 0).
const SENSITIVITY_RANGE: std::ops::RangeInclusive<f32> = 0.05..=20.0;

/// Draws every group the search lets through; `capturing` is the action being recorded.
pub(super) fn draw_groups(pane: &mut HotkeysPane, ui: &mut egui::Ui, registry: &InputRegistry, capturing: Option<ActionId>, intents: &mut Vec<Intent>) {
    let query = pane.search.trim().to_lowercase();
    let blocks = model::layout_rows(&model::row_keys(registry));
    if blocks.is_empty() {
        ui.weak(t!("settings.hotkeys.none_configurable"));
        return;
    }
    let mut shown_any = false;
    for block in blocks {
        let sections: Vec<SectionBlock> = block
            .sections
            .into_iter()
            .filter_map(|mut section| {
                let section_title = ms_i18n::resolve_key(section.section_key);
                section.actions.retain(|id| model::matches_query(&query, &[registry.title(*id).as_str(), registry.label(*id).as_str(), section_title]));
                (!section.actions.is_empty()).then_some(section)
            })
            .collect();
        if sections.is_empty() {
            continue;
        }
        shown_any = true;
        let holds_jump = pane.jump.is_some_and(|target| sections.iter().any(|section| section.actions.contains(&target)));
        let mut header = egui::CollapsingHeader::new(group_title(block.group)).id_salt(group_salt(block.group)).default_open(matches!(block.group, Group::Session | Group::View));
        if !query.is_empty() || holds_jump {
            header = header.open(Some(true));
        }
        header.show(ui, |ui| {
            for (index, section) in sections.iter().enumerate() {
                if index > 0 {
                    ui.add_space(6.0);
                }
                ui.strong(ms_i18n::resolve_key(section.section_key));
                for id in &section.actions {
                    draw_action(pane, ui, registry, *id, capturing, intents);
                }
            }
        });
    }
    if !shown_any {
        ui.weak(t!("settings.hotkeys.search_no_results"));
    }
}

/// The localized title of a group.
fn group_title(group: Group) -> String {
    match group {
        Group::Session => t!("settings.hotkeys.global_scope_title").to_owned(),
        Group::View => t!("settings.hotkeys.group_view_title").to_owned(),
        Group::Canvas => t!("settings.hotkeys.group_canvas_title").to_owned(),
        Group::SharedContexts => t!("settings.hotkeys.group_shared_contexts_title").to_owned(),
        Group::Tab(AppTab::Translation) => t!("settings.hotkeys.translation_scope_title").to_owned(),
        Group::Tab(tab) => tf!("settings.hotkeys.tab_scope_title", tab = tab.title()),
    }
}

/// A language-independent id salt for a group's collapsing header.
fn group_salt(group: Group) -> (&'static str, &'static str) {
    match group {
        Group::Session => ("settings.hotkeys.group", "session"),
        Group::View => ("settings.hotkeys.group", "view"),
        Group::Canvas => ("settings.hotkeys.group", "canvas"),
        Group::SharedContexts => ("settings.hotkeys.group", "shared_contexts"),
        Group::Tab(tab) => ("settings.hotkeys.group_tab", tab.key()),
    }
}

/// The kind badge text.
fn kind_badge(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Command => t!("settings.hotkeys.kind_command_badge"),
        ActionKind::Mode => t!("settings.hotkeys.kind_mode_badge"),
        ActionKind::Axis => t!("settings.hotkeys.kind_axis_badge"),
        ActionKind::Gesture => t!("settings.hotkeys.kind_gesture_badge"),
    }
}

/// One action block.
fn draw_action(pane: &mut HotkeysPane, ui: &mut egui::Ui, registry: &InputRegistry, id: ActionId, capturing: Option<ActionId>, intents: &mut Vec<Intent>) {
    let Some(view) = registry.action(id.0) else { return };
    let busy = capturing.is_some();
    ui.push_id(id.0, |ui| {
        let frame = egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong(registry.title(id));
                ui.small(kind_badge(view.kind));
                if view.fixed {
                    ui.small(t!("settings.hotkeys.fixed_badge"));
                }
                if capturing == Some(id) {
                    ui.colored_label(ms_theme::status::INFO, t!("settings.hotkeys.recording_badge"));
                }
                if view.overridden && !view.fixed && ui.add_enabled(!busy, egui::Button::new(t!("settings.hotkeys.reset_button"))).clicked() {
                    intents.push(Intent::Reset { action: id });
                }
            });
            if view.fixed {
                ui.monospace(registry.label(id));
                ui.weak(t!("settings.hotkeys.fixed_note"));
                return;
            }
            let report = registry.conflicts();
            if report.truncated.contains(&id) {
                ui.colored_label(ms_theme::status::WARNING, t!("settings.hotkeys.truncated_warning"));
            }
            let conflicts = model::conflicts_by_alternative(report, id);
            let shadows = model::shadows_by_alternative(report, id);
            if view.bindings.is_empty() {
                ui.weak(t!("settings.hotkeys.unassigned"));
            }
            for (index, binding) in view.bindings.iter().enumerate() {
                ui.push_id(index, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace(registry.alternative_label(binding));
                        if ui.add_enabled(!busy, egui::Button::new(t!("settings.hotkeys.rebind_button"))).clicked() {
                            intents.push(Intent::BeginCapture { action: id, slot: ms_input::CaptureSlot::Replace(index) });
                        }
                        if ui.add_enabled(!busy, egui::Button::new(t!("settings.hotkeys.remove_button"))).clicked() {
                            intents.push(Intent::Remove { action: id, index });
                        }
                    });
                    match binding {
                        Binding::Axis(axis) => draw_axis_editor(ui, registry, id, index, axis, busy, intents),
                        Binding::Gesture(gesture) => draw_gesture_editor(ui, id, index, gesture, busy, intents),
                        Binding::Command(_) | Binding::Mode(_) => {}
                    }
                    for conflict in conflicts.get(&index).into_iter().flatten() {
                        ui.colored_label(ms_theme::status::ERROR, registry.conflict_text(conflict, id));
                    }
                    for shadow in shadows.get(&index).into_iter().flatten() {
                        ui.weak(registry.shadow_text(shadow, id));
                    }
                });
            }
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!busy, egui::Button::new(t!("settings.hotkeys.add_button"))).clicked() {
                    intents.push(Intent::BeginCapture { action: id, slot: ms_input::CaptureSlot::Append });
                }
                match view.kind {
                    ActionKind::Axis => {
                        if let Some(source) = model::template_axis_source(registry, id)
                            && ui.add_enabled(!busy, egui::Button::new(t!("settings.hotkeys.add_plain_axis_button"))).clicked()
                        {
                            intents.push(Intent::Append { action: id, binding: Binding::axis(source, Chord::default()) });
                        }
                    }
                    ActionKind::Gesture => {
                        if let Some(kind) = model::template_gesture_kind(registry, id) {
                            draw_add_touch(ui, id, kind, busy, intents);
                        }
                    }
                    ActionKind::Command | ActionKind::Mode => {}
                }
            });
        });
        let rect = frame.response.rect;
        if pane.jump == Some(id) {
            ui.scroll_to_rect(rect, Some(egui::Align::Center));
            pane.jump = None;
            pane.highlight = Some((id, Instant::now() + HIGHLIGHT_DURATION));
        }
        if let Some((target, until)) = pane.highlight
            && target == id
        {
            let now = Instant::now();
            if now < until {
                ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(2.0, ms_theme::status::INFO), egui::StrokeKind::Outside);
                ui.ctx().request_repaint_after(until - now);
            } else {
                pane.highlight = None;
            }
        }
    });
    ui.add_space(4.0);
}

/// Source, sensitivity and inversion of one axis alternative.
fn draw_axis_editor(ui: &mut egui::Ui, registry: &InputRegistry, id: ActionId, index: usize, axis: &AxisBinding, busy: bool, intents: &mut Vec<Intent>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("settings.hotkeys.axis_source_label"));
        for source in AXIS_SOURCES {
            // The model's own source name (the label of a chord-less alternative of it).
            let text = registry.alternative_label(&Binding::axis(source, Chord::default()));
            if ui.add_enabled(!busy, egui::Button::selectable(axis.source == source, text)).clicked() && axis.source != source {
                intents.push(Intent::Replace { action: id, index, binding: Binding::Axis(AxisBinding { source, ..axis.clone() }) });
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("settings.hotkeys.axis_sensitivity_label"));
        let mut sensitivity = axis.sensitivity;
        let response = ui.add_enabled(!busy, WheelSpinBox::new(&mut sensitivity).range(SENSITIVITY_RANGE).speed(0.01).max_decimals(2));
        if response.changed() && sensitivity.is_finite() && sensitivity > 0.0 && (sensitivity - axis.sensitivity).abs() > f32::EPSILON {
            intents.push(Intent::Replace { action: id, index, binding: Binding::Axis(AxisBinding { sensitivity, ..axis.clone() }) });
        }
        let mut invert = axis.invert;
        if ui.add_enabled(!busy, egui::Checkbox::new(&mut invert, t!("settings.hotkeys.axis_invert_label"))).changed() {
            intents.push(Intent::Replace { action: id, index, binding: Binding::Axis(AxisBinding { invert, ..axis.clone() }) });
        }
    });
}

/// The localized name of a gesture kind.
fn gesture_kind_label(kind: GestureKind) -> &'static str {
    match kind {
        GestureKind::Click => t!("settings.hotkeys.gesture_kind_click"),
        GestureKind::DoubleClick => t!("settings.hotkeys.gesture_kind_double_click"),
        GestureKind::Drag => t!("settings.hotkeys.gesture_kind_drag"),
        GestureKind::LongPress => t!("settings.hotkeys.gesture_kind_long_press"),
    }
}

/// Kind (and, for a touch alternative, touch source) of one gesture alternative.
fn draw_gesture_editor(ui: &mut egui::Ui, id: ActionId, index: usize, gesture: &GestureBinding, busy: bool, intents: &mut Vec<Intent>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("settings.hotkeys.gesture_kind_label"));
        for kind in GESTURE_KINDS {
            if ui.add_enabled(!busy, egui::Button::selectable(gesture.kind == kind, gesture_kind_label(kind))).clicked() && gesture.kind != kind {
                intents.push(Intent::Replace { action: id, index, binding: Binding::Gesture(GestureBinding { kind, ..gesture.clone() }) });
            }
        }
    });
    let Button::Touch(current) = gesture.button else { return };
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("settings.hotkeys.touch_source_label"));
        let names = touch_source_names();
        let items: Vec<SearchableComboItem<'_>> = names.iter().map(|name| SearchableComboItem::new(name)).collect();
        let mut selected = TOUCH_SOURCES.iter().position(|source| *source == current).unwrap_or(0);
        ui.add_enabled_ui(!busy, |ui| {
            let response = SearchableComboBox::new(("settings.hotkeys.touch_source", id.0, index)).show(ui, &mut selected, &items);
            if response.changed
                && let Some(source) = TOUCH_SOURCES.get(selected).copied()
                && source != current
            {
                intents.push(Intent::Replace { action: id, index, binding: Binding::Gesture(GestureBinding { button: Button::Touch(source), ..gesture.clone() }) });
            }
        });
    });
}

/// The catalog combo that appends a touch alternative (`kind` from the action's template).
fn draw_add_touch(ui: &mut egui::Ui, id: ActionId, kind: GestureKind, busy: bool, intents: &mut Vec<Intent>) {
    let names = touch_source_names();
    let items: Vec<SearchableComboItem<'_>> = names.iter().map(|name| SearchableComboItem::new(name)).collect();
    // A scratch selection: only a committed pick (`picked`) appends, never a wheel step.
    let mut selected = 0_usize;
    ui.add_enabled_ui(!busy, |ui| {
        let response = SearchableComboBox::new(("settings.hotkeys.add_touch", id.0)).selected_text(t!("settings.hotkeys.add_touch_label")).show(ui, &mut selected, &items);
        if let Some(source) = response.picked.and_then(|picked| TOUCH_SOURCES.get(picked).copied()) {
            intents.push(Intent::Append { action: id, binding: Binding::gesture(Chord::default(), Button::Touch(source), kind) });
        }
    });
}

/// The localized names of `TOUCH_SOURCES`, in the same order.
fn touch_source_names() -> Vec<String> {
    TOUCH_SOURCES.iter().map(|source| ms_input::button_label(&Button::Touch(*source))).collect()
}
