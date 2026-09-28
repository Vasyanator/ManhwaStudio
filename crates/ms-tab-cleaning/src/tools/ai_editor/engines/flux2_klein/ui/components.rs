/*
File: cleaning/tools/ai_editor/engines/flux2_klein/ui/components.rs

Purpose:
The «Память» half of the panel and the merged component list of «Установка модели»: the
memory preset, the RAM/VRAM forecast, and the one row per model component that carries
presence, size, residency and the backend's own action buttons together.

Main responsibilities:
- draw the memory-preset picker, which offers «Пользовательский» but never as a choice
  (`draw_preset_row`);
- draw the backend's forecast, warning visibly when it does not fit (`draw_estimate_ui`);
- draw the merged component list — region size, `.status` errors, one row per component,
  and the device the backend runs on (`draw_component_list_ui`);
- draw one row's residency, degrading an unknown literal to "not known" with the literal
  on hover (`draw_component_residency_label`).

Key functions:
- `draw_preset_row()`, `draw_estimate_ui()`
- `draw_component_list_ui()`, `draw_component_residency_label()`

Notes:
ONE list, and that is the point: `.status` answers about presence and about residency for
the same five components, and drawing them as two blocks named every component twice.
What a row may OFFER is the backend's `actions` list and is never re-derived here; the
local gate only says whether the pipeline is free.
*/

use super::*;

/// Draws the memory-preset picker. `Пользовательский` is shown when the current values
/// match no preset but is never offered as a choice.
pub(super) fn draw_preset_row(ui: &mut egui::Ui, settings: &mut Flux2KleinSettings, changed: &mut bool) {
    let active = MemoryPreset::detect(settings);
    let mut picked: Option<MemoryPreset> = None;
    ui.horizontal(|ui| {
        ui.label(t!("cleaning.tools.flux2_klein.preset_label"))
            .on_hover_text(t!("cleaning.tools.flux2_klein.preset_hint"));
        WheelComboBox::from_id_salt("cleaning_flux2_klein_preset")
            .selected_text(active.label())
            .show_ui(ui, |ui| {
                for preset in MemoryPreset::selectable() {
                    if ui
                        .selectable_label(preset == active, preset.label())
                        .clicked()
                    {
                        picked = Some(preset);
                    }
                }
            })
            .response
            .on_hover_text(t!("cleaning.tools.flux2_klein.preset_hint"));
    });
    if let Some(preset) = picked {
        *changed |= preset.apply(settings);
    }
}

/// Draws the backend's memory forecast, warning visibly when it does not fit.
pub(super) fn draw_estimate_ui(
    ui: &mut egui::Ui,
    estimate: Option<&Flux2Estimate>,
    error: Option<&str>,
    busy: bool,
    status: Option<&Flux2Status>,
) {
    if busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.small(t!("cleaning.tools.flux2_klein.estimate_running_status"));
        });
        ui.ctx().request_repaint();
    }
    if let Some(error) = error {
        ui.small(tf!("cleaning.tools.flux2_klein.estimate_error", err = error));
    }
    let Some(estimate) = estimate else {
        if !busy && error.is_none() {
            ui.small(t!("cleaning.tools.flux2_klein.estimate_unknown_status"));
        }
        return;
    };
    let line = estimate_status_line(estimate, status);
    // The line stays one short sentence; the per-phase peaks and the full per-component
    // breakdown live in its tooltip, because the forecast is max(prompt encoding,
    // denoise, decode) and which of the phases dominates is the actionable part.
    let tooltip = estimate_tooltip(estimate);
    if estimate.fits {
        ui.small(line).on_hover_text(tooltip);
    } else {
        ui.colored_label(ms_theme::status::WARNING, line)
            .on_hover_text(tooltip);
        ui.colored_label(
            ms_theme::status::WARNING,
            t!("cleaning.tools.flux2_klein.estimate_does_not_fit_warning"),
        );
    }
}

/// Draws the merged component list of «Установка модели»: the current region size, the
/// `.status` errors, one row per model component, and the device the backend runs on.
///
/// ONE list, and that is the point. `.status` answers two things about the same five
/// components — whether their files are on disk, and where their weights currently are —
/// and the panel used to print them as two adjacent blocks that named every component
/// twice. A row now carries presence, size, residency and the backend's action buttons
/// together, so a component is read once.
///
/// What each row may offer is the BACKEND's `actions` list and is never re-derived here
/// (see [`Flux2ComponentAction`]); `actions_enabled` is only the local "the pipeline is
/// free" gate, and a disabled button still explains itself on hover. `action` receives the
/// one button pressed this frame, in the at-most-one shape the prompt-cache block uses.
pub(super) fn draw_component_list_ui(
    ui: &mut egui::Ui,
    status: Option<&Flux2Status>,
    error: Option<&str>,
    region: Option<[usize; 2]>,
    actions_enabled: bool,
    action: &mut Option<(Flux2ComponentId, Flux2ComponentAction)>,
) {
    // No frame on the canvas yet: there is no region to report, and printing `0x0` would
    // read as a broken selection rather than as an absent one.
    if let Some([w, h]) = region {
        ui.small(tf!(
            "cleaning.tools.flux2_klein.region_size_status",
            w = w,
            h = h
        ));
    }
    if let Some(error) = error {
        ui.colored_label(
            ms_theme::status::ERROR,
            tf!("cleaning.tools.flux2_klein.status_error", err = error),
        );
    }
    let Some(status) = status else {
        ui.small(t!("cleaning.tools.flux2_klein.status_unknown_status"));
        return;
    };
    if !status.available && !status.reason.is_empty() {
        ui.colored_label(ms_theme::status::WARNING, status.reason.as_str());
    }
    // The residency half is answered for the whole block at once, so its three
    // non-row outcomes are reported once above the rows instead of five times inside them.
    let residency = match flux2_component_block(Some(status)) {
        // Unreachable: `status` is `Some` here. Spelled out so a new variant cannot slip
        // through a catch-all.
        Flux2ComponentBlock::Hidden => None,
        Flux2ComponentBlock::Busy => {
            ui.small(t!("cleaning.tools.flux2_klein.component_residency_busy_status"));
            None
        }
        Flux2ComponentBlock::Unknown => {
            ui.small(t!(
                "cleaning.tools.flux2_klein.component_residency_unknown_status"
            ));
            None
        }
        Flux2ComponentBlock::Rows(rows) => Some(rows),
    };
    for row in flux2_component_rows(status, residency) {
        // The panel is a dock tab and is often ~300 px wide: a row of a label, a state and
        // up to three buttons has to be allowed to wrap rather than clip.
        ui.horizontal_wrapped(|ui| {
            let mark = if row.present { "✓" } else { "✗" };
            let line = if row.size_bytes > 0 {
                tf!(
                    "cleaning.tools.flux2_klein.component_sized_status",
                    mark = mark,
                    name = row.component.label(),
                    size = format_gib(row.size_bytes)
                )
            } else {
                tf!(
                    "cleaning.tools.flux2_klein.component_status",
                    mark = mark,
                    name = row.component.label()
                )
            };
            ui.small(line)
                .on_hover_text(flux2_component_row_tooltip(row.component, row.path));
            let Some(residency) = row.residency else {
                return;
            };
            draw_component_residency_label(ui, residency);
            for offered in &residency.actions {
                if flux2_gated_button(
                    ui,
                    actions_enabled,
                    offered.label(),
                    component_action_tooltip(residency.id, *offered),
                    t!("cleaning.tools.flux2_klein.component_action_disabled_tooltip"),
                ) {
                    *action = Some((residency.id, *offered));
                }
            }
        });
    }
    if !status.device.is_empty() {
        ui.small(tf!(
            "cleaning.tools.flux2_klein.device_status",
            device = status.device
        ));
    }
    if status.loaded {
        ui.small(t!("cleaning.tools.flux2_klein.pipeline_loaded_status"));
    }
}

/// Draws the residency of one row: the coloured state, with the hover that explains it.
///
/// A state this build does not know is drawn as "not known" — never as one of the five —
/// and the literal the backend sent goes into the hover, so a newer backend's answer can
/// still be read off the screen instead of disappearing.
pub(super) fn draw_component_residency_label(ui: &mut egui::Ui, row: &Flux2ComponentResidency) {
    let Some(residency) = row.residency else {
        let response = ui.small(t!("cleaning.tools.flux2_klein.component_residency_unknown"));
        if !row.residency_wire.is_empty() {
            response.on_hover_text(tf!(
                "cleaning.tools.flux2_klein.component_residency_unrecognized_hint",
                value = row.residency_wire
            ));
        }
        return;
    };
    let response = match residency.color() {
        Some(color) => ui.colored_label(color, residency.label()),
        None => ui.small(residency.label()),
    };
    if let Some(hint) = residency.hint() {
        response.on_hover_text(hint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_carries_the_residency_block_strings() {
        // A translation that drops `{value}`, `{name}` or `{action}` compiles and passes
        // the key-existence test while leaving the user with a sentence that names nothing.
        let interpolated = [
            (
                "cleaning.tools.flux2_klein.component_residency_unrecognized_hint",
                vec!["{value}"],
            ),
            (
                "cleaning.tools.flux2_klein.component_action_running_status",
                vec!["{name}", "{action}"],
            ),
            (
                "cleaning.tools.flux2_klein.component_action_error",
                vec!["{err}"],
            ),
        ];
        let plain = [
            "cleaning.tools.flux2_klein.component_residency_busy_status",
            "cleaning.tools.flux2_klein.component_residency_unknown_status",
            "cleaning.tools.flux2_klein.component_residency_unknown",
            "cleaning.tools.flux2_klein.component_text_encoder_hint",
            "cleaning.tools.flux2_klein.component_transformer_hint",
            "cleaning.tools.flux2_klein.component_vae_hint",
            // The merged list shows all five components, so the two that carry no weights
            // need an explanation of their own — the residency block never had to give one.
            "cleaning.tools.flux2_klein.component_tokenizer_hint",
            "cleaning.tools.flux2_klein.component_scheduler_hint",
            "cleaning.tools.flux2_klein.component_action_disabled_tooltip",
            "cleaning.tools.flux2_klein.component_action_done_status",
            "cleaning.tools.flux2_klein.component_action_load_pair_tooltip",
            "cleaning.tools.flux2_klein.component_action_unload_pair_tooltip",
            "cleaning.tools.flux2_klein.vae_tiling_label",
            "cleaning.tools.flux2_klein.vae_tiling_hint",
            "cleaning.tools.flux2_klein.vae_slicing_label",
            "cleaning.tools.flux2_klein.vae_slicing_hint",
        ];
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
            for key in plain {
                assert!(
                    !entry(key).trim().is_empty(),
                    "locale `{tag}`: `{key}` is empty"
                );
            }
            for (key, placeholders) in &interpolated {
                let text = entry(key);
                for placeholder in placeholders {
                    assert!(
                        text.contains(placeholder),
                        "locale `{tag}`: `{key}` must carry `{placeholder}`, `{text}` does not"
                    );
                }
            }
            // Every residency and every action needs a name of its own: two states drawn
            // with the same word would make the block unreadable in that language.
            let mut residencies = Vec::new();
            for suffix in ["not_loaded", "ram", "gpu", "offloaded", "mixed"] {
                let text = entry(&format!(
                    "cleaning.tools.flux2_klein.component_residency_{suffix}"
                ));
                assert!(
                    !residencies.contains(&text),
                    "locale `{tag}`: two residencies share the wording `{text}`"
                );
                residencies.push(text);
            }
            for suffix in ["load", "unload", "to_ram", "to_gpu", "warmup"] {
                for kind in ["button", "tooltip"] {
                    let key =
                        format!("cleaning.tools.flux2_klein.component_action_{suffix}_{kind}");
                    assert!(
                        !entry(&key).trim().is_empty(),
                        "locale `{tag}`: `{key}` is empty"
                    );
                }
            }
        }
    }
}
