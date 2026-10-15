/*
FILE HEADER (tools/mask_brush.rs)
- Назначение: общий примитив кисти для рисования маски в `egui::ColorImage`.
- Ключевые сущности:
  - `MaskBrush`: хранит радиус кисти и его границы.
- Input (the shared brush owner of the unified input system, `ms-input`):
  - `declarations()`: the brush actions every brush host shares — axis `brush.size` (Shift+wheel),
    commands `brush.smaller` (-) / `brush.larger` (= / Shift+= / numpad +); scope `Sub(None)`,
    active only while a host publishes `BRUSH_ACTIVE`.
  - `MaskBrush::apply_input`: applies this frame's brush actions from an `InputFrame`.
  - `MaskBrush::step_by_notches` / `shrink_one_step` / `grow_one_step`: the radius laws.
  - `MaskBrush::handle_wheel` / `handle_size_shortcuts`: TRANSITIONAL raw-input path kept only
    for the Cleaning brushes until input-unification WP7 moves them to `apply_input`.
- Ключевые методы:
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
use ms_input::{
    ActionId, ActionMeta, ActionSpec, AxisSource, AxisSpec, Binding, Button, Chord, CommandSpec, Condition, ContextId, Declarations, InputFrame, ModifierKey, Scope,
};

/// Context a brush host publishes (`StudioTab::input_contexts`) while its brush mode is on; the
/// brush actions are active only then.
pub const BRUSH_ACTIVE: ContextId = ContextId("brush.active");
/// Axis: brush size by wheel notches (Shift+wheel by default).
pub const BRUSH_SIZE: ActionId = ActionId("brush.size");
/// Command: shrink the brush one step.
pub const BRUSH_SMALLER: ActionId = ActionId("brush.smaller");
/// Command: grow the brush one step.
pub const BRUSH_LARGER: ActionId = ActionId("brush.larger");

const SECTION: &str = "tools.brush.section_title";

fn brush_meta(id: ActionId, title_key: &'static str, defaults: Vec<Binding>) -> ActionMeta {
    ActionMeta { id, title_key, section_key: SECTION, scope: Scope::Sub(None), when: vec![Condition::Context(BRUSH_ACTIVE)], defaults }
}

fn brush_command(id: ActionId, title_key: &'static str, chords: Vec<Chord>) -> ActionSpec {
    ActionSpec::Command(CommandSpec { meta: brush_meta(id, title_key, chords.into_iter().map(Binding::command).collect()), accepts: Vec::new(), repeat: true })
}

/// The shared brush actions, registered once by the studio. Every brush host (Translation mask
/// edit and advanced recognition, Typing mask edit, later the Cleaning brushes) publishes
/// [`BRUSH_ACTIVE`] while its brush is on and calls [`MaskBrush::apply_input`].
///
/// Shift+wheel also has a horizontal-wheel alternative: some backends deliver Shift+wheel as
/// horizontal scroll. The `+` key spellings (Shift+= and the numpad plus) keep the old `+` reading
/// under physical-key matching.
#[must_use]
pub fn declarations() -> Declarations {
    let shift = || Chord::new([Button::Modifier(ModifierKey::Shift)]);
    let actions = vec![
        ActionSpec::Axis(AxisSpec {
            meta: brush_meta(BRUSH_SIZE, "tools.brush.size_title", vec![Binding::axis(AxisSource::WheelY, shift()), Binding::axis(AxisSource::WheelX, shift())]),
        }),
        brush_command(BRUSH_SMALLER, "tools.brush.smaller_title", vec![Chord::new([Button::Key(egui::Key::Minus)])]),
        brush_command(
            BRUSH_LARGER,
            "tools.brush.larger_title",
            vec![
                Chord::new([Button::Key(egui::Key::Equals)]),
                Chord::new([Button::Modifier(ModifierKey::Shift), Button::Key(egui::Key::Equals)]),
                Chord::new([Button::Key(egui::Key::Plus)]),
            ],
        ),
    ];
    Declarations { actions, ..Declarations::default() }
}

/// A round mask brush: its radius in image pixels and the radius bounds.
#[derive(Debug, Clone)]
pub struct MaskBrush {
    radius_px: usize,
    min_radius_px: usize,
    max_radius_px: usize,
    wheel_step_px: usize,
    /// Sub-notch wheel remainder of the transitional raw path (`handle_wheel`) only; the frame
    /// path (`apply_input`) gets whole notches from the router.
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

    /// Sets the radius, clamped to the brush bounds. Returns whether it changed.
    pub fn set_radius_px(&mut self, radius_px: usize) -> bool {
        let next = radius_px.clamp(self.min_radius_px, self.max_radius_px);
        if next == self.radius_px {
            return false;
        }
        self.radius_px = next;
        true
    }

    /// Applies this frame's brush actions from `input`: `brush.size` notches, then `brush.smaller`
    /// / `brush.larger`. Call it once per frame while the host publishes [`BRUSH_ACTIVE`] (the
    /// router fires them only then). Returns whether the radius changed.
    pub fn apply_input(&mut self, input: &InputFrame) -> bool {
        let mut changed = false;
        if let Some(sample) = input.axis(BRUSH_SIZE) {
            changed |= self.step_by_notches(sample.steps);
        }
        if input.fired(BRUSH_SMALLER).is_some() {
            changed |= self.shrink_one_step();
        }
        if input.fired(BRUSH_LARGER).is_some() {
            changed |= self.grow_one_step();
        }
        changed
    }

    /// Grows (positive) or shrinks (negative) the radius by `notches` wheel notches of
    /// `wheel_step_px` each, clamped. Returns whether the radius changed.
    pub fn step_by_notches(&mut self, notches: i32) -> bool {
        if notches == 0 {
            return false;
        }
        let step = self.wheel_step_px.saturating_mul(usize::try_from(notches.unsigned_abs()).unwrap_or(usize::MAX));
        let next = if notches > 0 { self.radius_px.saturating_add(step) } else { self.radius_px.saturating_sub(step) };
        self.set_radius_px(next.max(1))
    }

    /// Shrinks the radius to `floor(radius * 0.9)`, at least one pixel, never below the minimum.
    /// Returns whether the radius changed.
    pub fn shrink_one_step(&mut self) -> bool {
        let next = (self.radius_px.saturating_mul(9) / 10).min(self.radius_px.saturating_sub(1));
        self.set_radius_px(next.max(self.min_radius_px))
    }

    /// Grows the radius to `ceil(radius * 1.1)`, at least one pixel, never above the maximum.
    /// Returns whether the radius changed.
    pub fn grow_one_step(&mut self) -> bool {
        let next = (self.radius_px.saturating_mul(11).saturating_add(9) / 10).max(self.radius_px.saturating_add(1));
        self.set_radius_px(next.min(self.max_radius_px))
    }

    /// Transitional raw path: Shift + `delta_y` wheel points resize the brush (40 points per
    /// notch). Returns `true` while Shift is held (the wheel belongs to the brush), `false` and a
    /// reset remainder otherwise.
    ///
    /// Only for the Cleaning brushes, which still read the wheel themselves; removed when they
    /// take [`Self::apply_input`] (input-unification WP7). New hosts never call it.
    pub fn handle_wheel(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        const WHEEL_NOTCH: f32 = 40.0;
        if !modifiers.shift {
            self.wheel_accum = 0.0;
            return false;
        }
        if delta_y.abs() <= f32::EPSILON {
            return true;
        }
        self.wheel_accum += delta_y;
        let notches = (self.wheel_accum / WHEEL_NOTCH).trunc();
        if notches == 0.0 {
            return true;
        }
        self.wheel_accum -= notches * WHEEL_NOTCH;
        // `notches` is a whole number bounded by one frame's wheel travel, and a float-to-int
        // `as` saturates, so the conversion cannot wrap.
        self.step_by_notches(notches as i32);
        true
    }

    /// Transitional raw path: `-` shrinks, `=` / `+` grow the brush (any modifiers, as before).
    /// Returns whether the radius changed.
    ///
    /// Only for the Cleaning brushes, which still read their keys themselves; removed when they
    /// take [`Self::apply_input`] (input-unification WP7). New hosts never call it.
    pub fn handle_size_shortcuts(&mut self, ctx: &egui::Context) -> bool {
        let (minus, equals, plus) = ctx.input(|i| (i.key_pressed(egui::Key::Minus), i.key_pressed(egui::Key::Equals), i.key_pressed(egui::Key::Plus)));
        let mut changed = false;
        if minus {
            changed |= self.shrink_one_step();
        }
        if equals || plus {
            changed |= self.grow_one_step();
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

#[cfg(test)]
mod tests {
    use ms_input::{Declarations, InputRegistry, view};

    use super::*;

    fn brush(radius_px: usize) -> MaskBrush {
        let mut brush = MaskBrush::default();
        brush.set_radius_px(radius_px);
        brush
    }

    #[test]
    fn wheel_notches_step_by_two_pixels_and_clamp() {
        let mut b = brush(24);
        assert!(b.step_by_notches(3));
        assert_eq!(b.radius_px(), 30);
        assert!(b.step_by_notches(-2));
        assert_eq!(b.radius_px(), 26);
        assert!(!b.step_by_notches(0), "no notch, no change");
        assert!(b.step_by_notches(-1000));
        assert_eq!(b.radius_px(), 1, "clamped at the minimum");
        assert!(b.step_by_notches(i32::MAX));
        assert_eq!(b.radius_px(), 200, "clamped at the maximum, no overflow");
        assert!(!b.step_by_notches(1), "already at the maximum");
    }

    #[test]
    fn size_commands_scale_by_ten_percent_with_a_one_pixel_minimum_step() {
        let mut b = brush(20);
        assert!(b.shrink_one_step());
        assert_eq!(b.radius_px(), 18);
        assert!(b.grow_one_step());
        assert_eq!(b.radius_px(), 20, "ceil(18 * 1.1) = 20");
        let mut small = brush(3);
        assert!(small.shrink_one_step());
        assert_eq!(small.radius_px(), 2, "floor(2.7) = 2");
        assert!(small.shrink_one_step());
        assert_eq!(small.radius_px(), 1);
        assert!(!small.shrink_one_step(), "the minimum stays");
        assert!(small.grow_one_step());
        assert_eq!(small.radius_px(), 2, "a small brush still grows by one pixel");
        let mut big = brush(195);
        assert!(big.grow_one_step());
        assert_eq!(big.radius_px(), 200, "clamped at the maximum");
        assert!(!big.grow_one_step());
    }

    /// The brush set registers next to the view set without conflicts (its keys sit on another
    /// scope level than the view zoom commands, so an overlap there is a shadow, never a conflict).
    #[test]
    fn declarations_register_beside_the_view_set() {
        let mut declarations = Declarations::default();
        declarations.extend(view::declarations());
        declarations.extend(super::declarations());
        let registry = match InputRegistry::new(declarations) {
            Ok(registry) => registry,
            Err(err) => panic!("the brush declarations must be valid: {err}"),
        };
        let brush_conflicts: Vec<_> = registry.conflicts().conflicts.iter().filter(|conflict| conflict.a.0.starts_with("brush.") || conflict.b.is_some_and(|b| b.0.starts_with("brush."))).collect();
        assert!(brush_conflicts.is_empty(), "{brush_conflicts:?}");
        assert_eq!(registry.bindings(BRUSH_LARGER).len(), 3);
    }
}
