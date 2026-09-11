/*
FILE HEADER (widgets/viewport_color_selector.rs)
- Назначение: переиспользуемый stateful-виджет выбора цвета:
  прямоугольник цвета + кнопка `Пипетка`.
- Ключевые сущности:
  - `ViewportColorSelector`: хранит состояние eyedropper-режима и последний screenshot viewport.
  - `ViewportColorSelectorResponse`: результат кадра виджета (`changed/committed/eyedropper_active`).
- Ключевые методы:
  - `ViewportColorSelector::draw`: рендер виджета, запуск/остановка пипетки, обновление preview-цвета.
  - `ViewportColorSelector::draw_with_presets`: the same frame, but the swatch opens
    the `ColorPresetPicker` popup (palette + preset cells) when the caller hands in a
    `ColorPresets` set. `draw` is the `None` case of it and keeps the stock egui
    color button. The eyedropper outranks both: while it is active the swatch is
    frozen and no preset UI is drawn.
  - `ViewportColorSelector::poll_screenshot_events`: чтение `Event::Screenshot` по токену этого виджета.
  - `sample_color_at_pointer`: выбор цвета по пикселю viewport под курсором.
- Замечание: пипетка меняет цвет в кадрах, где `ColorPresetPicker::draw` не вызывается,
  поэтому КАЖДАЯ её запись в `*color` (и превью, и откат по ПКМ/Escape) сообщается
  пикеру через `note_color_picked_by_user` — иначе он примет подобранный цвет за
  подмену снаружи и снимет выбор ячейки пресета.
*/
use super::color_preset_picker::{ColorPresetPicker, ColorPresets};
use eframe::egui;
use egui::{Color32, Key, Sense, StrokeKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static VIEWPORT_COLOR_SELECTOR_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default)]
pub struct ViewportColorSelectorResponse {
    pub changed: bool,
    pub committed: bool,
    pub eyedropper_active: bool,
    pub primary_click_consumed: bool,
    /// A preset cell was overwritten this frame: the caller must persist its
    /// `ColorPresets`. Always `false` in the preset-less mode.
    pub presets_changed: bool,
}

pub struct ViewportColorSelector {
    eyedropper_active: bool,
    primary_click_consumed_this_frame: bool,
    skip_primary_click_until_release: bool,
    screenshot_token: u64,
    latest_screenshot: Option<Arc<egui::ColorImage>>,
    start_color_before_eyedropper: Option<Color32>,
    /// UI state of the preset popup. Used only by `draw_with_presets`; the
    /// preset colors themselves are never owned here.
    preset_picker: ColorPresetPicker,
}

impl Default for ViewportColorSelector {
    fn default() -> Self {
        Self {
            eyedropper_active: false,
            primary_click_consumed_this_frame: false,
            skip_primary_click_until_release: false,
            screenshot_token: VIEWPORT_COLOR_SELECTOR_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed),
            latest_screenshot: None,
            start_color_before_eyedropper: None,
            preset_picker: ColorPresetPicker::default(),
        }
    }
}

impl ViewportColorSelector {
    /// Whether the viewport eyedropper is sampling right now.
    #[must_use]
    pub fn eyedropper_active(&self) -> bool {
        self.eyedropper_active
    }

    /// Whether this frame's primary click ended a sampling and must therefore
    /// not be handled again by the canvas below.
    #[must_use]
    pub fn primary_click_consumed_this_frame(&self) -> bool {
        self.primary_click_consumed_this_frame
    }

    /// Draws the selector with the stock egui color button.
    ///
    /// Thin wrapper over [`Self::draw_with_presets`] with no preset set, kept as
    /// the API for every call site that has no presets to offer.
    #[must_use]
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        color: &mut Color32,
    ) -> ViewportColorSelectorResponse {
        self.draw_with_presets(ui, color, None)
    }

    /// Draws the selector, optionally with a preset grid.
    ///
    /// `presets` is borrowed for the frame only: the widget reads the cells,
    /// overwrites one when the user confirms an edit, and reports that through
    /// `presets_changed` so the OWNER can persist it. Passing `None` selects the
    /// stock egui color button and can never report `presets_changed`.
    ///
    /// The eyedropper outranks the presets: while it is active the swatch is
    /// frozen and the preset popup is not drawn, so a sampled color cannot be
    /// written into a cell by accident. Every color the sampling writes is
    /// still reported to the picker, because a color the user picked must not
    /// look like an outside replacement of the color being edited.
    ///
    /// The returned response carries `presets_changed`, which only the owner of
    /// the set can act on — dropping it silently loses the user's edit.
    #[must_use]
    pub fn draw_with_presets(
        &mut self,
        ui: &mut egui::Ui,
        color: &mut Color32,
        presets: Option<&mut ColorPresets>,
    ) -> ViewportColorSelectorResponse {
        self.poll_screenshot_events(ui.ctx());
        self.primary_click_consumed_this_frame = false;

        let mut out = ViewportColorSelectorResponse::default();

        ui.horizontal(|ui| {
            if self.eyedropper_active {
                draw_locked_color_swatch(ui, *color);
            } else if let Some(presets) = presets {
                let picked = self.preset_picker.draw(ui, color, presets);
                if picked.color_changed {
                    out.changed = true;
                    out.committed = true;
                }
                out.presets_changed = picked.presets_changed;
            } else if ui.color_edit_button_srgba(color).changed() {
                out.changed = true;
                out.committed = true;
            }

            let button_label = if self.eyedropper_active {
                t!("widgets.viewport_color_selector.eyedropper_active")
            } else {
                t!("widgets.viewport_color_selector.eyedropper")
            };
            let button_resp = ui.button(button_label);
            if button_resp.clicked() && !self.eyedropper_active {
                self.eyedropper_active = true;
                self.skip_primary_click_until_release = true;
                self.start_color_before_eyedropper = Some(*color);
                self.latest_screenshot = None;
            }
        });

        if self.eyedropper_active {
            if let Some(sampled) =
                sample_color_at_pointer(ui.ctx(), self.latest_screenshot.as_deref())
            {
                *color = sampled;
                // Sampling happens in frames where the picker is not drawn at
                // all, so it has to be told; otherwise the next frame it is
                // drawn in would read the sampled color as an outside
                // replacement and drop the preset selection.
                self.preset_picker.note_color_picked_by_user(*color);
            }

            let (primary_clicked, primary_down, secondary_clicked, escape_pressed) =
                ui.ctx().input(|i| {
                    (
                        i.pointer.primary_clicked(),
                        i.pointer.primary_down(),
                        i.pointer.secondary_clicked(),
                        i.key_pressed(Key::Escape),
                    )
                });

            if self.skip_primary_click_until_release {
                if !primary_down {
                    self.skip_primary_click_until_release = false;
                }
            } else if primary_clicked {
                self.eyedropper_active = false;
                self.primary_click_consumed_this_frame = true;
                self.start_color_before_eyedropper = None;
                out.changed = true;
                out.committed = true;
                out.primary_click_consumed = true;
            }

            if secondary_clicked || escape_pressed {
                if let Some(start_color) = self.start_color_before_eyedropper.take() {
                    *color = start_color;
                    // A cancelled sampling must leave the picker exactly where
                    // it was, so the rollback is accounted for just like the
                    // preview writes it undoes.
                    self.preset_picker.note_color_picked_by_user(*color);
                }
                self.eyedropper_active = false;
            }

            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                    self.screenshot_token,
                )));
            ui.ctx().request_repaint();
        }

        out.eyedropper_active = self.eyedropper_active;
        out
    }

    fn poll_screenshot_events(&mut self, ctx: &egui::Context) {
        ctx.input(|i| {
            for event in &i.events {
                let egui::Event::Screenshot {
                    user_data, image, ..
                } = event
                else {
                    continue;
                };
                let Some(data) = &user_data.data else {
                    continue;
                };
                let Some(token) = data.downcast_ref::<u64>() else {
                    continue;
                };
                if *token == self.screenshot_token {
                    self.latest_screenshot = Some(image.clone());
                }
            }
        });
    }
}

fn draw_locked_color_swatch(ui: &mut egui::Ui, color: Color32) {
    let desired_size = ui.spacing().interact_size;
    let (rect, response) = ui.allocate_exact_size(desired_size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }

    egui::widgets::color_picker::show_color_at(ui.painter(), color, rect.shrink(1.0));
    let visuals = ui.style().interact(&response);
    let corner_radius = visuals.corner_radius.at_most(2);
    ui.painter().rect_stroke(
        rect,
        corner_radius,
        (1.0, visuals.bg_fill),
        StrokeKind::Inside,
    );
}

fn sample_color_at_pointer(
    ctx: &egui::Context,
    screenshot: Option<&egui::ColorImage>,
) -> Option<Color32> {
    let screenshot = screenshot?;
    let pointer_pos = ctx.input(|i| i.pointer.hover_pos())?;
    let pixels_per_point = ctx.pixels_per_point().max(0.0001);

    let px_x = (pointer_pos.x * pixels_per_point).floor().max(0.0) as usize;
    let px_y = (pointer_pos.y * pixels_per_point).floor().max(0.0) as usize;
    let width = screenshot.size[0];
    let height = screenshot.size[1];
    if width == 0 || height == 0 {
        return None;
    }
    let x = px_x.min(width.saturating_sub(1));
    let y = px_y.min(height.saturating_sub(1));
    screenshot.pixels.get(y * width + x).copied()
}
