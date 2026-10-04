/*
File: region_edit_v2/host_panels.rs

Purpose:
The two panel bodies of `RegionEditHost` (`host.rs`): the compact part in «Выбранный
инструмент» (engine picker, brush, mask layer, mask generation, mask actions, per-layer pixel
counts) and the host's own half of the main «Редактор области» panel (the run / apply / cancel
row, the «no mask needed» hint, the frame's status line, the size violation and the last
message). Split from `host.rs` only to keep that file readable; the two are one type.

Key functions:
- `draw_engine_picker()`, `draw_brush_controls()`, `draw_layer_picker()`,
  `draw_mask_generation()`, `draw_mask_actions()`, `draw_mask_summary()`: the compact panel
- `draw_host_actions()`: the host's part of the main panel
- `constraint_lines()`: the active engine's size rules as panel sentences (pure, tested)
- `engine_picker_shown()`: the one-engine rule of the picker (pure, tested)

Notes:
A panel body runs inside `CanvasView::draw` and may mutate only the tool, so every action here
QUEUES (`RegionFrame::request_*`, `generate_mask_requested`) and `draw_overlay_ui` performs it.
Localized controls take their `id_salt` from the tool's `HostSpec`, so two hosted tools never
share stored widget state.
*/

use super::engine::{AiEngine, EngineSection, violation_text};
use super::geometry::{FrameConstraints, check_size};
use super::host::RegionEditHost;
use crate::tools::mask_generation;
use ms_widgets::WheelSlider;
use eframe::egui;

/// Smallest and largest brush radius, in region pixels, the compact panel offers.
///
/// `MaskBrush` clamps to its own range anyway, so these bound the SLIDER, not the brush; they
/// are kept equal to that range so the slider cannot present a value the brush would refuse.
const BRUSH_RADIUS_MIN_PX: usize = 1;
const BRUSH_RADIUS_MAX_PX: usize = 200;

/// The picker sections, in the order they are drawn. A section with no engine is skipped.
const ENGINE_SECTIONS: [EngineSection; 2] = [EngineSection::WithoutPrompt, EngineSection::WithPrompt];

/// How much taller than an ordinary button «Обработать» is drawn.
///
/// It is the panel's primary action and sits in a row with two secondary ones, so it is given
/// half again their height. Only the SPACING is scaled — the widget, its colours and its shape
/// stay the theme's, so the button reads as emphasised rather than as a different control.
const PROCESS_BUTTON_EMPHASIS: f32 = 1.5;

impl RegionEditHost {
    /// Draws the engine picker: toggle buttons in one row per non-empty section (§13.1).
    ///
    /// Disabled as a whole for TWO independent reasons, each with its own tooltip, because
    /// "the button is grey" is not a reason a user can act on:
    /// - the frame is locked (D15): engines declare different mask layers, so a switch would
    ///   have to discard painted work;
    /// - the selected engine says a switch is unsafe right now
    ///   ([`AiEngine::switch_block_reason`]) — a model download in flight, which no frame
    ///   state describes.
    ///
    /// The frame lock is tested FIRST and keeps its own wording: it is the older and more
    /// specific rule, and an engine that is merely busy must not restate it.
    ///
    /// Not drawn at all for a one-engine catalog ([`engine_picker_shown`]): a row with a single
    /// button that cannot switch to anything is noise. Returns whether anything was drawn, so
    /// the caller separates it from what follows only when it is there.
    pub(super) fn draw_engine_picker(&mut self, ui: &mut egui::Ui) -> bool {
        if self.engines.is_empty() {
            ui.colored_label(ui.visuals().error_fg_color, t!("cleaning.tools.area_editor.error_no_engine"));
            return true;
        }
        if !engine_picker_shown(self.engines.len()) {
            return false;
        }
        let locked = !self.frame.lock().is_free();
        // Read once per draw, before the rows borrow `self.engines`: it is `Option<String>`,
        // not `Copy`, and the rows need it for every button's disabled tooltip.
        let switch_blocked = if locked { None } else { self.switch_block_reason() };
        // Borrowed, not cloned: `t!` hands out a `&'static str` and the engine's reason lives
        // in the local above, so a per-frame allocation buys nothing here.
        let disabled_hint: &str = match switch_blocked.as_deref() {
            Some(reason) => reason,
            None => t!("cleaning.tools.area_editor.engine_locked_hint"),
        };
        let enabled = !locked && switch_blocked.is_none();
        let mut requested: Option<usize> = None;
        for section in ENGINE_SECTIONS {
            // Captions are resolved here, before the row closure, so the closure borrows only
            // this local list and not `self.engines`.
            let entries: Vec<(usize, String)> = self
                .engines
                .iter()
                .enumerate()
                .filter(|(_, engine)| engine.section() == section)
                .map(|(idx, engine)| (idx, engine.title()))
                .collect();
            if entries.is_empty() {
                continue;
            }
            ui.label(match section {
                EngineSection::WithoutPrompt => t!("cleaning.tools.area_editor.section_without_prompt"),
                EngineSection::WithPrompt => t!("cleaning.tools.area_editor.section_with_prompt"),
            });
            ui.horizontal_wrapped(|ui| {
                // Inside a wrapping layout egui defaults widget text to `Wrap`, which would
                // break a long engine name over two LINES instead of moving its button to the
                // next ROW — the same fix `tab.rs::draw_tool_button_rows` makes.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                for (idx, title) in entries {
                    let response = ui.add_enabled(enabled, egui::Button::new(title).selected(idx == self.selected));
                    if response.on_disabled_hover_text(disabled_hint).clicked() {
                        requested = Some(idx);
                    }
                }
            });
        }
        if let Some(idx) = requested {
            self.select_engine(idx);
        }
        true
    }

    /// Draws the brush row of the compact panel: radius and paint/erase mode.
    pub(super) fn draw_brush_controls(&mut self, ui: &mut egui::Ui) {
        let mut radius = self.frame.brush_mut().radius_px();
        if ui
            .add(
                WheelSlider::new(&mut radius, BRUSH_RADIUS_MIN_PX..=BRUSH_RADIUS_MAX_PX)
                    .text(t!("cleaning.common.size_label")),
            )
            .changed()
        {
            // The setter answers whether it changed anything after clamping; the slider is
            // rebuilt from the brush next frame either way, so the answer has no reader here.
            self.frame.brush_mut().set_radius_px(radius);
        }

        let mut erase = self.frame.erase();
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut erase, false, t!("cleaning.tools.area_editor.brush_paint_button"));
            ui.selectable_value(&mut erase, true, t!("cleaning.tools.area_editor.brush_erase_button"))
                .on_hover_text(t!("cleaning.tools.area_editor.brush_erase_hint"));
        });
        self.frame.set_erase(erase);
    }

    /// Draws the mask-layer picker of the compact panel.
    ///
    /// The layer names come from the FRAME, which holds what the active engine declared, so a
    /// switch renames the buttons without this panel knowing anything about engines. A single
    /// layer needs no picker — the switch would be a button that cannot do anything.
    pub(super) fn draw_layer_picker(&mut self, ui: &mut egui::Ui) {
        if self.frame.masks().layer_count() < 2 {
            return;
        }
        ui.label(t!("cleaning.tools.area_editor.mask_layer_label"));
        let mut active = self.frame.masks().active();
        let labels: Vec<(usize, String)> = (0..self.frame.masks().layer_count())
            .map(|idx| (idx, self.frame.layer_label(idx)))
            .collect();
        ui.horizontal_wrapped(|ui| {
            for (idx, label) in labels {
                ui.selectable_value(&mut active, idx, label);
            }
        });
        self.frame.masks_mut().set_active(active);
    }

    /// Draws the two mask actions of the compact panel: undo one stroke, erase everything.
    pub(super) fn draw_mask_actions(&mut self, ui: &mut egui::Ui) {
        let editable = self.mask_editable();
        let has_mask = !self.frame.masks().is_empty();
        let mut nothing_to_undo = false;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    editable && has_mask,
                    egui::Button::new(t!("cleaning.tools.area_editor.undo_stroke_button")),
                )
                .clicked()
                && !self.frame.masks_mut().undo()
            {
                nothing_to_undo = true;
            }
            if ui
                .add_enabled(
                    editable && has_mask,
                    egui::Button::new(t!("cleaning.region_frame.button.clear_mask")),
                )
                .clicked()
            {
                self.frame.masks_mut().clear_all();
            }
        });
        if nothing_to_undo {
            self.report_info(t!("cleaning.tools.area_editor.nothing_to_undo").to_string());
        }
    }

    /// Draws the mask-generation block of the compact panel: the source picker, the selected
    /// source's parameters in a collapsed section, the live progress of a streaming source, and
    /// «Сгенерировать маску».
    ///
    /// It sits among the mask actions because that is what it is — filling the selected layer
    /// from a backend detector instead of by hand — and it is the HOST's, not an engine's: every
    /// engine gets it, including the ones whose mask means "you may change here" rather than
    /// "remove this". The block therefore says nothing about what the mask means; the engines
    /// state that in their own panel bodies.
    ///
    /// The button only QUEUES the job ([`Self::generate_mask_requested`]): a dock panel body
    /// may mutate the tool alone, and the load needs the canvas and the project.
    pub(super) fn draw_mask_generation(&mut self, ui: &mut egui::Ui) {
        let busy = self.mask_generation_rx.is_some() || self.pending_load.is_some();
        // The ✓/«скачать» marks of the watermark catalog come from the backend and are fetched
        // lazily: at most one query in flight, and never while a detection is running.
        self.mask_generation.refresh_watermark_catalog(self.backend_available, busy);

        let torch_available = self.torch_available;
        let section_id = ui.make_persistent_id(self.spec.mask_generation_section_salt);
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), section_id, false)
            .show_header(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(t!("cleaning.mask_editor.mask_gen_params_heading"));
                    ui.add_space(8.0);
                    self.mask_generation.draw_source_picker(ui, self.spec.mask_source_picker_salt, torch_available);
                });
            })
            .body(|ui| self.mask_generation.draw_source_params(ui));
        // Outside the collapsible, so a running detection stays visible while the parameters
        // are folded away.
        self.mask_generation.draw_progress(ui);

        // The button's enablement and its tooltip come from the SAME rule the click is checked
        // against, so a greyed-out button and a refused click can never give different reasons.
        let block_reason = self.mask_generation_block_reason();
        let hint = match block_reason {
            Some((ref text, _)) => text.clone(),
            None => mask_generation::generate_button_hover_text(
                self.mask_generation.params.source,
                self.backend_available,
                self.torch_available,
            ),
        };
        if ui
            .add_enabled(block_reason.is_none(), egui::Button::new(t!("cleaning.mask_editor.generate_mask_button")))
            .on_hover_text(hint.clone())
            // `on_hover_text` is enabled-only (`Tooltip::for_enabled`), so the reason the button
            // is greyed out needs the disabled variant too.
            .on_disabled_hover_text(hint)
            .clicked()
        {
            self.generate_mask_requested = true;
        }
    }

    /// One line per mask layer with its painted-pixel count, while anything is painted.
    ///
    /// It answers the one question neither the frame's chrome nor its status line can: the
    /// frame stays LOCKED — green, unmovable, unresizable — while a SINGLE mask pixel is set
    /// anywhere inside it, and a stray dot is invisible at a low zoom. The counts name the
    /// layer that still holds something, so the user can undo that stroke instead of erasing
    /// the whole mask to get the frame back.
    pub(super) fn draw_mask_summary(&self, ui: &mut egui::Ui) {
        let masks = self.frame.masks();
        if masks.is_empty() {
            return;
        }
        for idx in 0..masks.layer_count() {
            ui.small(tf!(
                "cleaning.tools.area_editor.layer_row",
                name = self.frame.layer_label(idx),
                count = masks.layer_set_px(idx)
            ));
        }
    }

    /// Draws the host's own part of the main panel: the run button, the two actions that
    /// resolve a pending result, the green «no mask needed» hint, the frame's status line
    /// and the last message.
    ///
    /// «Обработать» lives HERE and nowhere else: the frame's own chrome row carries only
    /// Применить / Отменить / Стереть маску. Applying and cancelling are repeated here because
    /// a frame holding a result is LOCKED and a locked frame may scroll out of view entirely —
    /// its chrome row is then unreachable and this panel is the only way to resolve the result.
    pub(super) fn draw_host_actions(&mut self, ui: &mut egui::Ui) {
        let enabled = self.frame.buttons();
        let block_reason = self.engine().and_then(AiEngine::run_block_reason);
        ui.horizontal_wrapped(|ui| {
            // `interact_size.y` is what actually sets a button's height — a `Button` takes the
            // larger of it and its padded text — so the emphasis is applied there, and the
            // padding is scaled with it or the label would rattle inside a taller frame.
            // The base is measured rather than assumed, so the button keeps its proportion if
            // the theme's font or spacing changes.
            let base_height = ui.spacing().interact_size.y.max(
                ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y,
            );
            let process = ui
                .scope(|ui| {
                    let spacing = ui.spacing_mut();
                    spacing.button_padding *= PROCESS_BUTTON_EMPHASIS;
                    spacing.interact_size.y = base_height * PROCESS_BUTTON_EMPHASIS;
                    ui.add_enabled(
                        enabled.process && block_reason.is_none(),
                        egui::Button::new(t!("cleaning.tools.area_editor.process_button")),
                    )
                })
                .inner;
            // The engine's own reason is more specific than the generic hint, so it wins when
            // there is one: "no model selected" beats "paint a mask first".
            let process = match block_reason.as_ref() {
                Some(reason) => process.on_disabled_hover_text(reason),
                None => process.on_hover_text(t!("cleaning.tools.area_editor.process_hint")),
            };
            if process.clicked() {
                self.frame.request_process();
            }
            if ui
                .add_enabled(enabled.apply, egui::Button::new(t!("cleaning.region_frame.button.apply")))
                .clicked()
            {
                self.frame.request_apply();
            }
            if ui
                .add_enabled(enabled.cancel, egui::Button::new(t!("cleaning.region_frame.button.cancel")))
                .clicked()
            {
                self.frame.request_cancel();
            }
        });
        self.draw_empty_mask_hint(ui);
        ui.label(self.frame.status_text());
        if let Some(violation) = self.frame.size_violation() {
            ui.colored_label(ui.visuals().error_fg_color, violation_text(violation));
            self.draw_size_requirements(ui);
        }
        if let Some(message) = self.message.as_ref() {
            if message.error {
                ui.colored_label(ui.visuals().error_fg_color, &message.text);
            } else {
                ui.small(&message.text);
            }
        }
    }

    /// Says, in green under «Обработать», that a run may start with nothing painted.
    ///
    /// Drawn exactly while BOTH halves hold: no mask layer holds a single pixel, and the
    /// selected engine accepts an empty mask. It answers the question the button itself
    /// cannot — an enabled «Обработать» over an empty mask looks the same as one over a
    /// painted mask, so without this line the only way to learn that painting is optional
    /// is to click and see. The rule is generic on purpose: an engine that cannot run
    /// without a mask simply never shows it, and the frame's own gate
    /// (`FrameButtons::process`) refuses the run for that engine anyway — this line only
    /// reports the permission, it never grants one.
    ///
    /// Deliberately NOT a hint about the OTHER half: "paint the area the model may change"
    /// belongs to the engine's own panel body, which is where the mask's meaning is
    /// explained and where it differs from engine to engine.
    fn draw_empty_mask_hint(&self, ui: &mut egui::Ui) {
        if !self.frame.masks().is_empty() {
            return;
        }
        if !self.engine().is_some_and(AiEngine::allows_empty_mask) {
            return;
        }
        ui.small(
            egui::RichText::new(t!("cleaning.tools.area_editor.no_mask_hint"))
                // An affirmative hint ("works without a mask"): the shared success tone.
                .color(ms_theme::status::SUCCESS),
        );
    }

    /// Spells out the size the ACTIVE engine wants, under the sentence that says the current
    /// one is wrong.
    ///
    /// Drawn only while the frame is invalid: the numbers are what the user needs to resize
    /// towards, and naming them at every other moment would be noise beside a frame that is
    /// already the right shape. This is also the only place a switch to an engine with
    /// stricter requirements becomes actionable rather than merely red (D16).
    fn draw_size_requirements(&self, ui: &mut egui::Ui) {
        let constraints = self.frame.constraints();
        for line in constraint_lines(constraints) {
            ui.small(line);
        }
        // The size a drag would settle on, named only when it is a real way out: a legal size
        // that differs from the current one. A contradictory rule set has no such size, and
        // naming the red snap result there would point the user at another refusal.
        if let (Some((w, h)), Some(rect)) = (self.frame.snapped_size(), self.frame.rect_px())
            && (rect.w, rect.h) != (w, h)
            && check_size(w, h, constraints).is_none()
        {
            ui.small(tf!("cleaning.tools.area_editor.constraint_nearest", width = w, height = h));
        }
    }
}

/// Whether the engine picker is drawn for a catalog of `engine_count` engines: only when there
/// is something to switch between. Generic on purpose — a hosted tool with one engine (the
/// cloud API editor) shows no picker, one with several (the AI area editor) keeps it.
#[must_use]
pub(super) fn engine_picker_shown(engine_count: usize) -> bool {
    engine_count >= 2
}

/// The active engine's size rules as panel sentences, one per rule that is set, in the
/// order grid and minimum side, maximum side, minimum area, maximum area, aspect, size
/// table, upscale allowance.
///
/// A symmetric aspect keeps the one-number sentence; only a genuinely two-sided limit spells
/// out the range. The values are printed as DECLARED (the geometry sanitizes them where it
/// reads them), so the sentence names what the engine asked for. Resolved on every call, so
/// it follows a language switch.
#[must_use]
pub(super) fn constraint_lines(c: &FrameConstraints) -> Vec<String> {
    let mut lines = vec![tf!(
        "cleaning.tools.area_editor.constraint_multiple",
        multiple = c.multiple,
        min_side = c.min_side
    )];
    if let Some(max_side) = c.max_side {
        lines.push(tf!("cleaning.tools.area_editor.constraint_max_side", max_side = max_side));
    }
    if let Some(min_area) = c.min_area {
        lines.push(tf!("cleaning.tools.area_editor.constraint_min_area", area = min_area));
    }
    if let Some(max_area) = c.max_area {
        lines.push(tf!("cleaning.tools.area_editor.constraint_max_area", area = max_area));
    }
    if let Some(aspect) = c.aspect {
        // Compared by bits: "declared symmetric" is the question, not "numerically close".
        if aspect.max_w_over_h.to_bits() == aspect.max_h_over_w.to_bits() {
            lines.push(tf!("cleaning.tools.area_editor.constraint_max_aspect", aspect = aspect.max_w_over_h));
        } else {
            lines.push(tf!(
                "cleaning.tools.area_editor.constraint_aspect_range",
                tall = aspect.max_h_over_w,
                wide = aspect.max_w_over_h
            ));
        }
    }
    if !c.sizes.is_empty() {
        let sizes = c.sizes.iter().map(|(w, h)| format!("{w}×{h}")).collect::<Vec<_>>().join(", ");
        lines.push(tf!("cleaning.tools.area_editor.constraint_sizes", sizes = sizes));
    }
    if c.max_upscale > 1 {
        lines.push(tf!("cleaning.tools.area_editor.constraint_upscale", factor = c.max_upscale));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::region_edit_v2::geometry::AspectLimit;

    /// One engine has nothing to switch to; the AI area editor's four keep their picker.
    #[test]
    fn the_engine_picker_is_shown_only_for_a_choice() {
        assert!(!engine_picker_shown(1));
        assert!(engine_picker_shown(2));
        assert!(engine_picker_shown(4));
    }

    /// The original rules read exactly as before the extension, one sentence each, and every
    /// extended rule adds its own sentence in the documented order.
    #[test]
    fn constraint_lines_name_every_rule_that_is_set() {
        // `tf!` answers against the PROCESS-GLOBAL catalog; install the reference catalog
        // under the shared lock, as the other UI-string tests of this crate do.
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");

        let klein = FrameConstraints { multiple: 16, min_side: 128, max_area: Some(1_048_576), aspect: Some(AspectLimit::symmetric(8.0)), ..FrameConstraints::UNCONSTRAINED };
        assert_eq!(
            constraint_lines(&klein),
            vec![
                "Multiple of 16 px, shortest side from 128 px".to_string(),
                "Area up to 1048576 px²".to_string(),
                "Aspect ratio up to 8:1".to_string(),
            ]
        );
        assert_eq!(constraint_lines(&FrameConstraints::UNCONSTRAINED).len(), 1, "the grid line is always there");

        static SIZES: [(u32, u32); 2] = [(1024, 1024), (832, 1248)];
        let full = FrameConstraints {
            multiple: 8,
            min_side: 64,
            max_side: Some(2048),
            min_area: Some(65_536),
            max_area: Some(4_194_304),
            aspect: Some(AspectLimit { max_w_over_h: 2.0, max_h_over_w: 3.0 }),
            sizes: &SIZES,
            max_upscale: 4,
        };
        assert_eq!(
            constraint_lines(&full),
            vec![
                "Multiple of 8 px, shortest side from 64 px".to_string(),
                "Each side up to 2048 px".to_string(),
                "Area from 65536 px²".to_string(),
                "Area up to 4194304 px²".to_string(),
                "Aspect ratio from 1:3 to 2:1".to_string(),
                "Allowed sizes only: 1024×1024, 832×1248".to_string(),
                "The region may be upscaled up to ×4 before processing".to_string(),
            ]
        );
    }
}
