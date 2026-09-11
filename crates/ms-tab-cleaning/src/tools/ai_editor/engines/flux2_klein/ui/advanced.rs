/*
File: cleaning/tools/ai_editor/engines/flux2_klein/ui/advanced.rs

Purpose:
«Для экспертов», the last of the panel's three collapsible sections: the generation
parameters nobody touches between two runs, the mask shaping, and — behind their own
label — the placement fields the memory preset owns.

Main responsibilities:
- draw the section and report, through its `changed` flag, that a persisted value moved;
- close the `guidance_scale` control, with the reason stated in the open, on a checkpoint
  the backend reports as distilled.

Key functions:
- `draw_advanced_section()`

Notes:
The one control here that is not always live is `guidance_scale`: a distilled checkpoint
ignores it (`.status` answers `guidance_supported: false`), and a value above 1.0 then only
doubles the per-step compute. The stored value survives untouched — the backend owns the
run's semantics and the setting has to come back on a non-distilled checkpoint.

Closed by default, and it gathers everything that is set once for a machine or for a page
rather than per edit. «Сила изменения» deliberately does NOT live here — it is the panel's
one creative dial and stays always visible in the body. The preset-owned group is
introduced by a line saying what overriding one costs, because changing any of the seven
silently moves the memory preset to «Пользовательский» and the picker that reports it is
in another section.
*/

use super::*;

/// Draws «Для экспертов», the last of the three collapsible sections: the generation
/// parameters nobody touches between two runs, the mask shaping, and — behind their own
/// label — the placement fields the memory preset owns.
///
/// Closed by default. It gathers everything that used to be spread over the old
/// «Параметры» wrapper and a SECOND header nested inside it: a value here is set once for a
/// machine or for a page, never per edit. «Сила изменения» deliberately does NOT live here
/// — it is the panel's one creative dial and stays always visible.
///
/// The preset-owned group is introduced by a line saying what overriding one costs, because
/// changing any of the seven silently moves the memory preset to «Пользовательский»
/// ([`MemoryPreset::detect`]) and the picker that reports it is in another section.
///
/// `guidance_supported` is [`flux2_guidance_supported`] of the last `.status` answer. When
/// it is `false` the guidance control is drawn closed with the reason both on hover and in
/// a line under it; the stored value is left exactly as it is.
pub(super) fn draw_advanced_section(
    ui: &mut egui::Ui,
    settings: &mut Flux2KleinSettings,
    guidance_supported: bool,
    changed: &mut bool,
) {
    RegionEditToolBase::draw_region_editor_collapsible_section(
        ui,
        "cleaning_flux2_klein_advanced",
        t!("cleaning.tools.flux2_klein.advanced_heading"),
        false,
        |ui| {
            *changed |= ui
                .add(
                    WheelSlider::new(&mut settings.steps, FLUX2_STEPS_MIN..=FLUX2_STEPS_MAX)
                        .text(t!("cleaning.common.steps_label")),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.steps_hint"))
                .changed();
            // A DISTILLED checkpoint switches classifier-free guidance off outright, so on
            // one of those the dial can only cost compute. Three decisions are deliberate:
            //
            // * The STORED value is never touched — not clamped, not reset, not hidden.
            //   The backend owns the run's semantics, and a user who later points the
            //   engine at a non-distilled checkpoint must find his setting where he left it.
            // * Unlike the fp8 checkbox further down, this one IS closed rather than merely
            //   re-explained: a number the backend ignores traps nobody, while a `true`
            //   behind a disabled checkbox would.
            // * A disabled `WheelSlider` still sees the wheel. `add_enabled` clears
            //   `Response::hovered()` but not `contains_pointer()` — a disabled widget stays
            //   in egui's hit test, which is how `on_disabled_hover_text` works at all
            //   (egui-0.35.0/src/hit_test.rs, "treat it as if it isn't sensing anything"
            //   drops only CLICK and DRAG) — and the widget's wheel path keys on
            //   `hovered() || contains_pointer()` (`src/widgets/wheel_slider.rs`,
            //   `pointer_over_response_rect`). The closed draw is therefore handed a COPY,
            //   so a wheel notch over it moves a value that dies with the frame.
            let mut guidance_scratch = settings.guidance_scale;
            let guidance_value = if guidance_supported {
                &mut settings.guidance_scale
            } else {
                &mut guidance_scratch
            };
            let guidance = ui
                .add_enabled(
                    guidance_supported,
                    WheelSlider::new(guidance_value, FLUX2_GUIDANCE_MIN..=FLUX2_GUIDANCE_MAX)
                        .text(t!("cleaning.tools.flux2_klein.guidance_label")),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.guidance_hint"))
                .on_disabled_hover_text(t!(
                    "cleaning.tools.flux2_klein.guidance_unsupported_disabled_tooltip"
                ));
            *changed |= guidance_supported && guidance.changed();
            // The tooltip alone is not enough: a control that is grey for a reason nobody
            // can read off it reaches the developer as a bug report, so the reason is also
            // stated in the open, under the control it disables.
            if !guidance_supported {
                ui.small(t!("cleaning.tools.flux2_klein.guidance_unsupported_note"));
            }
            ui.horizontal(|ui| {
                *changed |= ui
                    .checkbox(
                        &mut settings.use_seed,
                        t!("cleaning.tools.flux2_klein.fixed_seed_label"),
                    )
                    .on_hover_text(t!("cleaning.tools.flux2_klein.fixed_seed_hint"))
                    .changed();
                if settings.use_seed {
                    *changed |= SeedSpinBox::new(&mut settings.seed).draw(ui).changed();
                }
            });
            // Growing the mask contour has no meaning while nothing is painted: the mask
            // is then the whole region and the backend IGNORES `mask_dilate_px`. The
            // slider is NOT faded for that, because the state it depends on is unknowable
            // here — the working mode is derived from the painted mask at RUN time and the
            // host pushes an engine the rectangle and the availability flags, never the
            // mask stack. The condition goes into the hover text instead, where it is true
            // at every moment rather than only at the drawn one. Feathering is unaffected
            // either way: it still softens how the regenerated region joins the page.
            *changed |= ui
                .add(
                    WheelSlider::new(&mut settings.mask_dilate_px, 0..=FLUX2_DILATE_MAX)
                        .text(t!("cleaning.common.mask_expand_label")),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.mask_expand_hint"))
                .changed();
            *changed |= ui
                .add(
                    WheelSlider::new(&mut settings.mask_feather_px, 0..=FLUX2_FEATHER_MAX)
                        .text(t!("cleaning.tools.flux2_klein.mask_feather_label")),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.mask_feather_hint"))
                .changed();
            *changed |= ui
                .checkbox(
                    &mut settings.color_match,
                    t!("cleaning.tools.flux2_klein.color_match_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.color_match_hint"))
                .changed();

            ui.separator();
            ui.label(t!("cleaning.tools.flux2_klein.preset_fields_label"));
            ui.small(t!("cleaning.tools.flux2_klein.preset_fields_hint"));
            ui.horizontal(|ui| {
                ui.label(t!("cleaning.tools.flux2_klein.placement_label"))
                    .on_hover_text(t!("cleaning.tools.flux2_klein.placement_hint"));
                let mut placement = Flux2Placement::from_wire(&settings.placement);
                WheelComboBox::from_id_salt("cleaning_flux2_klein_placement")
                    .selected_text(placement.label())
                    .show_ui(ui, |ui| {
                        for candidate in Flux2Placement::all() {
                            ui.selectable_value(&mut placement, candidate, candidate.label());
                        }
                    })
                    .response
                    .on_hover_text(t!("cleaning.tools.flux2_klein.placement_hint"));
                if placement.wire() != settings.placement {
                    settings.placement = placement.wire().to_string();
                    *changed = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label(t!("cleaning.tools.flux2_klein.dtype_label"))
                    .on_hover_text(t!("cleaning.tools.flux2_klein.dtype_hint"));
                let mut dtype = Flux2Dtype::from_wire(&settings.dtype);
                WheelComboBox::from_id_salt("cleaning_flux2_klein_dtype")
                    .selected_text(dtype.wire())
                    .show_ui(ui, |ui| {
                        for candidate in Flux2Dtype::all() {
                            // The dtype names are technical identifiers, not prose.
                            ui.selectable_value(&mut dtype, candidate, candidate.wire());
                        }
                    })
                    .response
                    .on_hover_text(t!("cleaning.tools.flux2_klein.dtype_hint"));
                if dtype.wire() != settings.dtype {
                    settings.dtype = dtype.wire().to_string();
                    *changed = true;
                }
            });
            *changed |= ui
                .checkbox(
                    &mut settings.low_cpu_mem_usage,
                    t!("cleaning.tools.flux2_klein.low_cpu_mem_usage_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.low_cpu_mem_usage_hint"))
                .changed();
            *changed |= ui
                .checkbox(
                    &mut settings.vae_tiling,
                    t!("cleaning.tools.flux2_klein.vae_tiling_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.vae_tiling_hint"))
                .changed();
            *changed |= ui
                .checkbox(
                    &mut settings.vae_slicing,
                    t!("cleaning.tools.flux2_klein.vae_slicing_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.vae_slicing_hint"))
                .changed();
            *changed |= ui
                .checkbox(
                    &mut settings.unload_transformer_before_vae,
                    t!("cleaning.tools.flux2_klein.unload_before_vae_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.unload_before_vae_hint"))
                .changed();
            *changed |= ui
                .checkbox(
                    &mut settings.unload_text_encoder_after_encode,
                    t!("cleaning.tools.flux2_klein.unload_text_encoder_label"),
                )
                .on_hover_text(t!("cleaning.tools.flux2_klein.unload_text_encoder_hint"))
                .changed();
            // fp8 only moves the peak while the encoder is still resident: once it is
            // unloaded right after the prompt is encoded, the peak belongs to the
            // transformer and quantizing the encoder buys nothing. The checkbox stays LIVE
            // and plain anyway: the value is persisted and still goes on the wire, so a
            // disabled control would trap a `true` the user could only clear by first
            // turning the unloading off again. The interaction is explained where an
            // explanation belongs — in the ordinary hover, which says what the combination
            // means instead of the plain description of the switch.
            let pointless = settings.unload_text_encoder_after_encode;
            *changed |= ui
                .checkbox(
                    &mut settings.text_encoder_fp8,
                    t!("cleaning.tools.flux2_klein.text_encoder_fp8_label"),
                )
                .on_hover_text(if pointless {
                    t!("cleaning.tools.flux2_klein.text_encoder_fp8_hint_pointless")
                } else {
                    t!("cleaning.tools.flux2_klein.text_encoder_fp8_hint")
                })
                .changed();
        },
    );
}
