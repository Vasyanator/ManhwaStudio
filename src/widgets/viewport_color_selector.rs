/*
FILE HEADER (widgets/viewport_color_selector.rs)
- Назначение: переиспользуемый stateful-виджет выбора цвета:
  прямоугольник цвета + кнопка `Пипетка`.
- Ключевые сущности:
  - `ViewportColorSelector`: хранит состояние eyedropper-режима и последний screenshot viewport.
  - `ViewportColorSelectorResponse`: результат кадра виджета (`changed/committed/eyedropper_active`).
- Ключевые методы:
  - `ViewportColorSelector::draw`: рендер виджета, запуск/остановка пипетки, обновление preview-цвета.
  - `ViewportColorSelector::poll_screenshot_events`: чтение `Event::Screenshot` по токену этого виджета.
  - `sample_color_at_pointer`: выбор цвета по пикселю viewport под курсором.
*/
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
}

pub struct ViewportColorSelector {
    eyedropper_active: bool,
    primary_click_consumed_this_frame: bool,
    skip_primary_click_until_release: bool,
    screenshot_token: u64,
    latest_screenshot: Option<Arc<egui::ColorImage>>,
    start_color_before_eyedropper: Option<Color32>,
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
        }
    }
}

impl ViewportColorSelector {
    pub fn eyedropper_active(&self) -> bool {
        self.eyedropper_active
    }

    pub fn primary_click_consumed_this_frame(&self) -> bool {
        self.primary_click_consumed_this_frame
    }

    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        color: &mut Color32,
    ) -> ViewportColorSelectorResponse {
        self.poll_screenshot_events(ui.ctx());
        self.primary_click_consumed_this_frame = false;

        let mut out = ViewportColorSelectorResponse::default();

        ui.horizontal(|ui| {
            if self.eyedropper_active {
                draw_locked_color_swatch(ui, *color);
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
