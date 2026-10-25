/*
FILE HEADER (tools/mask_brush.rs)
- Назначение: общий примитив кисти для рисования маски в `egui::ColorImage`.
- Ключевые сущности:
  - `MaskBrush`: хранит радиус кисти и его границы.
  - `BinaryMaskMut`: mutable view of a row-major binary (`0/255`) mask with its size.
  - `LineSegmentPx`: a stroke segment in image pixel coordinates.
- Input (the shared brush owner of the unified input system, `ms-input`):
  - `declarations()`: the brush actions every brush host shares — axis `brush.size` (Shift+wheel),
    commands `brush.smaller` (-) / `brush.larger` (= / Shift+= / numpad +); scope `Sub(None)`,
    active only while a host publishes `BRUSH_ACTIVE`.
  - `MaskBrush::apply_input`: applies this frame's brush actions from an `InputFrame`.
  - `MaskBrush::step_by_notches` / `shrink_one_step` / `grow_one_step`: the radius laws.
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
/// edit and advanced recognition, Typing mask edit, the Cleaning brushes, region frame and
/// region-editor window) publishes
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
}

impl Default for MaskBrush {
    fn default() -> Self {
        Self {
            radius_px: 24,
            min_radius_px: 1,
            max_radius_px: 200,
            wheel_step_px: 2,
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

    /// Applies this frame's brush actions from `input`: `brush.size` notches (summed over its
    /// sources: the vertical and horizontal wheel may both move in one frame), then
    /// `brush.smaller` / `brush.larger`. Call it once per frame while the host publishes
    /// [`BRUSH_ACTIVE`] (the router fires them only then). Returns whether the radius changed.
    pub fn apply_input(&mut self, input: &InputFrame) -> bool {
        let notches = input.axis_samples(BRUSH_SIZE).fold(0_i32, |sum, sample| sum.saturating_add(sample.steps));
        let mut changed = self.step_by_notches(notches);
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

    pub fn paint_mask_segment(
        &self,
        dst: &mut egui::ColorImage,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        erase: bool,
    ) {
        let segment = LineSegmentPx { x0: from_x, y0: from_y, x1: to_x, y1: to_y };
        paint_line_with_brush(dst, segment, self.radius_px.max(1) as i32, Color32::WHITE, erase);
    }

    /// Paints (`255`) or erases (`0`) `segment` into the binary `mask` with this brush's radius
    /// (at least one pixel). Pixels outside the mask are clipped; a mask whose `data` is shorter
    /// than `width * height`, or with a zero dimension, makes the call a no-op.
    pub fn paint_binary_mask_segment(&self, mask: BinaryMaskMut<'_>, segment: LineSegmentPx, erase: bool) {
        paint_line_on_binary_mask(mask, segment, self.radius_px.max(1) as i32, erase);
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

/// Mutable view of a row-major binary mask (`0` = off, `255` = on) of `width`×`height` pixels.
///
/// `data` must hold at least `width * height` bytes; the painters treat a shorter buffer or a
/// zero dimension as an invalid mask and do nothing.
#[derive(Debug)]
pub struct BinaryMaskMut<'a> {
    /// Mask bytes, row stride `width`.
    pub data: &'a mut [u8],
    /// Mask width in pixels.
    pub width: usize,
    /// Mask height in pixels.
    pub height: usize,
}

/// A straight brush-stroke segment from `(x0, y0)` to `(x1, y1)` in image pixel coordinates.
/// A zero-length segment stamps a single circle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSegmentPx {
    /// Start x.
    pub x0: i32,
    /// Start y.
    pub y0: i32,
    /// End x.
    pub x1: i32,
    /// End y.
    pub y1: i32,
}

/// Stamps filled circles of `radius` (at least one pixel) along `segment` into `dst`, writing
/// `color`, or transparent pixels when `erase` is set. Pixels outside `dst` are clipped.
pub fn paint_line_with_brush(dst: &mut egui::ColorImage, segment: LineSegmentPx, radius: i32, color: Color32, erase: bool) {
    let LineSegmentPx { x0, y0, x1, y1 } = segment;
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

/// Binary-mask twin of `paint_line_with_brush`: writes `255` (or `0` when `erase`) along
/// `segment`; an invalid mask (see `BinaryMaskMut`) is a no-op.
fn paint_line_on_binary_mask(mask: BinaryMaskMut<'_>, segment: LineSegmentPx, radius: i32, erase: bool) {
    let BinaryMaskMut { data: mask_data, width: mask_width, height: mask_height } = mask;
    let LineSegmentPx { x0, y0, x1, y1 } = segment;
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

    /// A click (zero-length segment) stamps one disc; a horizontal segment covers both ends and
    /// the span between them; erasing writes `0` over the same pixels.
    #[test]
    fn binary_mask_segment_paints_discs_along_the_segment() {
        let b = brush(1);
        let mut data = vec![0u8; 7 * 5];
        b.paint_binary_mask_segment(BinaryMaskMut { data: &mut data, width: 7, height: 5 }, LineSegmentPx { x0: 3, y0: 2, x1: 3, y1: 2 }, false);
        let set: Vec<usize> = data.iter().enumerate().filter(|(_, v)| **v == 255).map(|(i, _)| i).collect();
        assert_eq!(set, vec![3 + 7, 2 + 2 * 7, 3 + 2 * 7, 4 + 2 * 7, 3 + 3 * 7], "radius-1 disc = centre + 4 neighbours");

        let mut line = vec![0u8; 7 * 5];
        b.paint_binary_mask_segment(BinaryMaskMut { data: &mut line, width: 7, height: 5 }, LineSegmentPx { x0: 1, y0: 2, x1: 5, y1: 2 }, false);
        assert!((0..7).all(|x| line[x + 2 * 7] == 255), "the whole middle row is covered");
        b.paint_binary_mask_segment(BinaryMaskMut { data: &mut line, width: 7, height: 5 }, LineSegmentPx { x0: 1, y0: 2, x1: 5, y1: 2 }, true);
        assert!(line.iter().all(|v| *v == 0), "erasing the same segment clears it");
    }

    /// A buffer shorter than `width * height`, or a zero dimension, is an invalid mask: no-op.
    #[test]
    fn binary_mask_segment_ignores_an_invalid_mask() {
        let b = brush(2);
        let mut short = vec![0u8; 10];
        b.paint_binary_mask_segment(BinaryMaskMut { data: &mut short, width: 4, height: 4 }, LineSegmentPx { x0: 1, y0: 1, x1: 2, y1: 2 }, false);
        assert!(short.iter().all(|v| *v == 0));
        let mut empty = vec![0u8; 4];
        b.paint_binary_mask_segment(BinaryMaskMut { data: &mut empty, width: 0, height: 4 }, LineSegmentPx { x0: 0, y0: 0, x1: 0, y1: 0 }, false);
        assert!(empty.iter().all(|v| *v == 0));
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
