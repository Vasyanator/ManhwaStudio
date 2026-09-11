/*
FILE HEADER (tools/mask_brush.rs)
- Назначение: общий примитив кисти для рисования маски в `egui::ColorImage`.
- Ключевые сущности:
  - `MaskBrush`: хранит радиус кисти и инкрементальный state wheel-жеста.
- Ключевые методы:
  - `handle_wheel`: Shift+wheel меняет радиус без блокировки GUI потока.
  - `handle_size_shortcuts`: hotkeys `-`, `=`, `+` для изменения размера.
  - `paint_mask_segment`: рисует/стирает сегмент маски круглой кистью.
  - `paint_binary_mask_segment`: рисует/стирает бинарную маску (`0/255`) без
    промежуточных аллокаций `ColorImage`.
  - `draw_circle_cursor_on_image`: рисует кольцо курсора в image viewport.
- Ключевые helper-функции:
  - `paint_line_with_brush`: штампует круги вдоль отрезка.
  - `paint_circle`: заполняет круг в целевом `ColorImage`.
*/

use eframe::egui;
use egui::{Color32, Pos2, Rect};

#[derive(Debug, Clone)]
pub struct MaskBrush {
    radius_px: usize,
    min_radius_px: usize,
    max_radius_px: usize,
    wheel_step_px: usize,
    wheel_accum: f32,
}

impl Default for MaskBrush {
    fn default() -> Self {
        Self {
            radius_px: 24,
            min_radius_px: 1,
            max_radius_px: 200,
            wheel_step_px: 2,
            wheel_accum: 0.0,
        }
    }
}

impl MaskBrush {
    pub fn radius_px(&self) -> usize {
        self.radius_px
    }

    pub fn set_radius_px(&mut self, radius_px: usize) -> bool {
        let next = radius_px.clamp(self.min_radius_px, self.max_radius_px);
        if next == self.radius_px {
            return false;
        }
        self.radius_px = next;
        true
    }

    pub fn handle_wheel(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        if !modifiers.shift {
            self.wheel_accum = 0.0;
            return false;
        }
        if delta_y.abs() <= f32::EPSILON {
            return true;
        }
        const WHEEL_NOTCH: f32 = 40.0;
        self.wheel_accum += delta_y;

        let steps = (self.wheel_accum / WHEEL_NOTCH).trunc() as isize;
        if steps == 0 {
            return true;
        }
        self.wheel_accum -= steps as f32 * WHEEL_NOTCH;
        let next = self.radius_px as isize + steps * self.wheel_step_px as isize;
        self.set_radius_px(next.max(1) as usize);
        true
    }

    pub fn handle_size_shortcuts(&mut self, ctx: &egui::Context) -> bool {
        let (minus, equals, plus) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Minus),
                i.key_pressed(egui::Key::Equals),
                i.key_pressed(egui::Key::Plus),
            )
        });
        let mut changed = false;
        if minus {
            let mut next = ((self.radius_px as f32) * 0.9).floor() as usize;
            if next >= self.radius_px && self.radius_px > self.min_radius_px {
                next = self.radius_px.saturating_sub(1);
            }
            next = next.max(self.min_radius_px);
            changed |= self.set_radius_px(next);
        }
        if equals || plus {
            let mut next = ((self.radius_px as f32) * 1.1).ceil() as usize;
            if next <= self.radius_px && self.radius_px < self.max_radius_px {
                next = self.radius_px.saturating_add(1);
            }
            next = next.min(self.max_radius_px);
            changed |= self.set_radius_px(next);
        }
        changed
    }

    pub fn paint_mask_segment(
        &self,
        dst: &mut egui::ColorImage,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        erase: bool,
    ) {
        paint_line_with_brush(
            dst,
            from_x,
            from_y,
            to_x,
            to_y,
            self.radius_px.max(1) as i32,
            Color32::WHITE,
            erase,
        );
    }

    // All parameters are independent brush stroke properties; grouping would obscure painting intent.
    #[allow(clippy::too_many_arguments)]
    pub fn paint_binary_mask_segment(
        &self,
        mask_data: &mut [u8],
        mask_width: usize,
        mask_height: usize,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        erase: bool,
    ) {
        paint_line_on_binary_mask(
            mask_data,
            mask_width,
            mask_height,
            from_x,
            from_y,
            to_x,
            to_y,
            self.radius_px.max(1) as i32,
            erase,
        );
    }

    pub fn draw_circle_cursor_on_image(
        &self,
        ui: &mut egui::Ui,
        image_rect: Rect,
        image_size: [usize; 2],
        pointer_pos: Pos2,
    ) {
        if !image_rect.contains(pointer_pos) {
            return;
        }
        let img_w = image_size[0].max(1) as f32;
        let img_h = image_size[1].max(1) as f32;
        let px_scale_x = image_rect.width() / img_w;
        let px_scale_y = image_rect.height() / img_h;
        let radius_scene = self.radius_px.max(1) as f32 * ((px_scale_x + px_scale_y) * 0.5);

        ui.ctx()
            .output_mut(|out| out.cursor_icon = egui::CursorIcon::None);
        ui.painter().circle_stroke(
            pointer_pos,
            radius_scene.max(0.5),
            egui::Stroke::new(2.0, Color32::WHITE),
        );
        ui.painter().circle_stroke(
            pointer_pos,
            (radius_scene - 1.0).max(0.5),
            egui::Stroke::new(1.0, Color32::BLACK),
        );
    }
}

// All parameters are independent brush stroke properties; grouping would obscure painting intent.
#[allow(clippy::too_many_arguments)]
pub fn paint_line_with_brush(
    dst: &mut egui::ColorImage,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    color: Color32,
    erase: bool,
) {
    let r = radius.max(1);
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= f32::EPSILON {
        paint_circle(dst, x0, y0, r, color, erase);
        return;
    }
    let step = (r as f32 * 0.45).max(1.0);
    let stamps = (distance / step).ceil() as usize;
    let mut last = (i32::MIN, i32::MIN);
    for i in 0..=stamps {
        let t = i as f32 / stamps.max(1) as f32;
        let sx = (x0 as f32 + dx * t).round() as i32;
        let sy = (y0 as f32 + dy * t).round() as i32;
        if (sx, sy) == last {
            continue;
        }
        paint_circle(dst, sx, sy, r, color, erase);
        last = (sx, sy);
    }
}

fn paint_circle(
    dst: &mut egui::ColorImage,
    cx: i32,
    cy: i32,
    radius: i32,
    color: Color32,
    erase: bool,
) {
    let r = radius.max(1);
    let r2 = r * r;
    let w_usize = dst.size[0];
    let w = w_usize as i32;
    let h = dst.size[1] as i32;
    let x0 = (cx - r).max(0);
    let x1 = (cx + r).min(w - 1);
    let y0 = (cy - r).max(0);
    let y1 = (cy + r).min(h - 1);
    let fill = if erase { Color32::TRANSPARENT } else { color };

    for y in y0..=y1 {
        let dy = y - cy;
        let rem = r2 - dy * dy;
        if rem < 0 {
            continue;
        }
        let span = (rem as f32).sqrt() as i32;
        let sx0 = x0.max(cx - span) as usize;
        let sx1 = x1.min(cx + span) as usize;
        if sx0 > sx1 {
            continue;
        }
        let row_off = (y as usize).saturating_mul(w_usize);
        for px in &mut dst.pixels[row_off + sx0..=row_off + sx1] {
            *px = fill;
        }
    }
}

// All parameters are independent brush stroke properties; grouping would obscure painting intent.
#[allow(clippy::too_many_arguments)]
fn paint_line_on_binary_mask(
    mask_data: &mut [u8],
    mask_width: usize,
    mask_height: usize,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    erase: bool,
) {
    if mask_width == 0
        || mask_height == 0
        || mask_data.len() < mask_width.saturating_mul(mask_height)
    {
        return;
    }
    let r = radius.max(1);
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= f32::EPSILON {
        paint_circle_on_binary_mask(mask_data, mask_width, mask_height, x0, y0, r, erase);
        return;
    }
    let step = (r as f32 * 0.45).max(1.0);
    let stamps = (distance / step).ceil() as usize;
    let mut last = (i32::MIN, i32::MIN);
    for i in 0..=stamps {
        let t = i as f32 / stamps.max(1) as f32;
        let sx = (x0 as f32 + dx * t).round() as i32;
        let sy = (y0 as f32 + dy * t).round() as i32;
        if (sx, sy) == last {
            continue;
        }
        paint_circle_on_binary_mask(mask_data, mask_width, mask_height, sx, sy, r, erase);
        last = (sx, sy);
    }
}

fn paint_circle_on_binary_mask(
    mask_data: &mut [u8],
    mask_width: usize,
    mask_height: usize,
    cx: i32,
    cy: i32,
    radius: i32,
    erase: bool,
) {
    if mask_width == 0 || mask_height == 0 {
        return;
    }
    let r = radius.max(1);
    let r2 = r * r;
    let w = mask_width as i32;
    let h = mask_height as i32;
    let x0 = (cx - r).max(0);
    let x1 = (cx + r).min(w - 1);
    let y0 = (cy - r).max(0);
    let y1 = (cy + r).min(h - 1);
    let fill = if erase { 0u8 } else { 255u8 };

    for y in y0..=y1 {
        let dy = y - cy;
        let rem = r2 - dy * dy;
        if rem < 0 {
            continue;
        }
        let span = (rem as f32).sqrt() as i32;
        let sx0 = x0.max(cx - span) as usize;
        let sx1 = x1.min(cx + span) as usize;
        if sx0 > sx1 {
            continue;
        }
        let row_off = (y as usize).saturating_mul(mask_width);
        for px in &mut mask_data[row_off + sx0..=row_off + sx1] {
            *px = fill;
        }
    }
}
