/*
File: cleaning/tools/ai_editor/engines/flux3_klein/ui/advanced.rs

Purpose:
«Для экспертов» of the FLUX.3 klein panel: the switches set once per machine or per page —
«Референсы и в текстовый энкодер» (`vl_refs`), «Автоэнкодер в bfloat16», the fp8 matmul path
(fp8r only), «Ограничить референсы», and the composite's feather and colour match (global, plus
the local match's strength and scale, closed while the global one is off).

Key functions:
- `draw_advanced_section()`

Notes:
Folded by default. Every control writes the persisted settings and raises `changed`; nothing
here decides anything (the wire values are `Flux3KleinSettings::to_params`). Project widgets
only (`WheelSlider`, `WheelComboBox`).
*/

use super::*;

/// Draws «Для экспертов». `changed` is raised when a persisted value moved.
pub(super) fn draw_advanced_section(ui: &mut egui::Ui, settings: &mut Flux3KleinSettings, changed: &mut bool) {
    RegionEditToolBase::draw_region_editor_collapsible_section(ui, "cleaning_flux3_klein_advanced", t!("cleaning.tools.flux3_klein.advanced_heading"), false, |ui| {
        *changed |= ui
            .checkbox(&mut settings.vl_refs, t!("cleaning.tools.flux3_klein.vl_refs_label"))
            .on_hover_text(t!("cleaning.tools.flux3_klein.vl_refs_hint"))
            .changed();
        *changed |= ui
            .checkbox(&mut settings.ae_bf16, t!("cleaning.tools.flux3_klein.ae_bf16_label"))
            .on_hover_text(t!("cleaning.tools.flux3_klein.ae_bf16_hint"))
            .changed();
        // The fp8 path exists only for the fp8r weights; for the others it is not drawn at all
        // (and `to_params` sends `auto`), while the stored choice waits for fp8r to come back.
        if settings.format() == Flux3Format::Fp8r {
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
        *changed |= ui
            .add(WheelSlider::new(&mut settings.mask_feather_px, 0..=FLUX3_FEATHER_MAX).text(t!("cleaning.tools.flux3_klein.mask_feather_label")))
            .on_hover_text(t!("cleaning.tools.flux3_klein.mask_feather_hint"))
            .changed();
        *changed |= ui
            .checkbox(&mut settings.color_match, t!("cleaning.tools.flux3_klein.color_match_label"))
            .on_hover_text(t!("cleaning.tools.flux3_klein.color_match_hint"))
            .changed();
        // The local match refines the global one and the backend applies it only under it, so
        // both dials close while the checkbox is off; the stored values are kept. A disabled
        // `WheelSlider` ignores the wheel itself (`egui-docs/04-widgets.md`).
        let color_match = settings.color_match;
        ui.indent("cleaning_flux3_klein_color_match_local", |ui| {
            *changed |= ui
                .add_enabled(
                    color_match,
                    WheelSlider::new(&mut settings.color_match_local_pct, 0..=FLUX3_COLOR_MATCH_LOCAL_PCT_MAX)
                        // A unit symbol, not prose: the project's percent dials spell it literally.
                        .suffix("%")
                        .text(t!("cleaning.tools.flux3_klein.color_match_local_label")),
                )
                .on_hover_text(t!("cleaning.tools.flux3_klein.color_match_local_hint"))
                .on_disabled_hover_text(t!("cleaning.tools.flux3_klein.color_match_local_hint"))
                .changed();
            *changed |= ui
                .add_enabled(
                    color_match,
                    WheelSlider::new(&mut settings.color_match_scale_px, FLUX3_COLOR_MATCH_SCALE_MIN..=FLUX3_COLOR_MATCH_SCALE_MAX)
                        .suffix(t!("cleaning.tab.pixels_suffix"))
                        .text(t!("cleaning.tools.flux3_klein.color_match_scale_label")),
                )
                .on_hover_text(t!("cleaning.tools.flux3_klein.color_match_scale_hint"))
                .on_disabled_hover_text(t!("cleaning.tools.flux3_klein.color_match_scale_hint"))
                .changed();
        });
    });
}
