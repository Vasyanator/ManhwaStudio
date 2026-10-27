/*
File: cleaning/tools/ai_editor/engines/flux3_klein/ui/advanced.rs

Purpose:
«Для экспертов» of the FLUX.3 klein panel: the switches set once per machine or per page —
«Референсы и в текстовый энкодер» (`vl_refs`; closed while a GGUF encoder is selected, D6),
«Автоэнкодер в bfloat16», the fp8 matmul path (fp8r only) and «Ограничить референсы». The composite's feather and colour match are not here:
they are the shared «Постобработка» section (`../../shared/postprocess_section.rs`) drawn last
in the body by `mod.rs`.

Key functions:
- `draw_advanced_section()`

Notes:
Folded by default. Every control writes the persisted settings and raises `changed`; nothing
here decides anything (the wire values are `Flux3KleinSettings::to_params`). Project widgets
only (`WheelComboBox`).
*/

use super::*;

/// Draws «Для экспертов». `changed` is raised when a persisted value moved.
pub(super) fn draw_advanced_section(ui: &mut egui::Ui, settings: &mut Flux3KleinSettings, changed: &mut bool) {
    RegionEditToolBase::draw_region_editor_collapsible_section(ui, "cleaning_flux3_klein_advanced", t!("cleaning.tools.flux3_klein.advanced_heading"), false, |ui| {
        // D6: a GGUF encoder is text-only (no vision tower), and the backend refuses the pair by
        // name. While one is selected the checkbox is closed and shows the EFFECTIVE value —
        // off, which is what `normalized()` sends — while the stored choice is left untouched
        // and comes back with the bf16 encoder (FLUX.2 klein's D8 pattern). The reason is on
        // hover AND in a line under it.
        if settings.encoder_format().is_gguf() {
            let mut effective = false;
            ui.add_enabled(false, egui::Checkbox::new(&mut effective, t!("cleaning.tools.flux3_klein.vl_refs_label")))
                .on_disabled_hover_text(t!("cleaning.tools.flux3_klein.vl_refs_gguf_disabled_hint"));
            ui.small(t!("cleaning.tools.flux3_klein.vl_refs_gguf_disabled_hint"));
        } else {
            *changed |= ui
                .checkbox(&mut settings.vl_refs, t!("cleaning.tools.flux3_klein.vl_refs_label"))
                .on_hover_text(t!("cleaning.tools.flux3_klein.vl_refs_hint"))
                .changed();
        }
        *changed |= ui
            .checkbox(&mut settings.ae_bf16, t!("cleaning.tools.flux3_klein.ae_bf16_label"))
            .on_hover_text(t!("cleaning.tools.flux3_klein.ae_bf16_hint"))
            .changed();
        // The fp8 path exists only for the fp8r weights; for the others it is not drawn at all
        // (and `to_params` sends `auto`), while the stored choice waits for fp8r to come back.
        if settings.dit_format() == Flux3DitFormat::Fp8r {
            let mut path = Flux3Fp8Compute::from_wire(&settings.fp8_compute);
            ui.horizontal_wrapped(|ui| {
                ui.label(t!("cleaning.tools.flux3_klein.fp8_compute_label")).on_hover_text(t!("cleaning.tools.flux3_klein.fp8_compute_hint"));
                WheelComboBox::from_id_salt("cleaning_flux3_klein_fp8_compute")
                    .selected_text(path.label())
                    .show_ui(ui, |ui| {
                        for candidate in Flux3Fp8Compute::all() {
                            ui.selectable_value(&mut path, candidate, candidate.label());
                        }
                    })
                    .response
                    .on_hover_text(t!("cleaning.tools.flux3_klein.fp8_compute_hint"));
            });
            if path.wire() != settings.fp8_compute {
                settings.fp8_compute = path.wire().to_string();
                *changed = true;
            }
        }
        let mut cap = if FLUX3_REFERENCE_CAP_SIDES.contains(&settings.reference_max_side) { settings.reference_max_side } else { 0 };
        let cap_label = |side: u32| {
            if side == 0 {
                t!("cleaning.tools.flux3_klein.reference_cap_off").to_string()
            } else {
                tf!("cleaning.tools.flux3_klein.reference_cap_side", side = side)
            }
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(t!("cleaning.tools.flux3_klein.reference_cap_label")).on_hover_text(t!("cleaning.tools.flux3_klein.reference_cap_hint"));
            WheelComboBox::from_id_salt("cleaning_flux3_klein_reference_cap")
                .selected_text(cap_label(cap))
                .show_ui(ui, |ui| {
                    for side in FLUX3_REFERENCE_CAP_SIDES {
                        ui.selectable_value(&mut cap, side, cap_label(side));
                    }
                })
                .response
                .on_hover_text(t!("cleaning.tools.flux3_klein.reference_cap_hint"));
        });
        if cap != settings.reference_max_side {
            settings.reference_max_side = cap;
            *changed = true;
        }
    });
}
