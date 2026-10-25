/*
File: crates/ms-tab-ps-editor/src/composite.rs

Purpose:
Composite order and pixel compositing of the PS canvas: the draw plan above the base layers and the
page-rectangle composite used by selection copy and the patch tool.

Key functions:
- `composite_steps()`: thin adapter over `ordering::composite_plan` (tests:
  `composite_plan_tests.rs`).
- `stack_group_uids()`, `stack_group_folds()`: group fold source.
- `visible_layers_bottom_to_top()`, `composite_rect()`, `composite_to_page()`: the one
  page-rectangle composite.
- `over()`, `scale_premultiplied()`: premultiplied blend helpers.

Notes:
`PsStep`, `PsRasterView`, `PsTextView`, `CompositeLayer`, `CompositeBound` are defined in `lib.rs`.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

/// The stack's unified group uids as strings, in `stack.groups()` order. The stack keys groups by
/// `Uuid`; the composite owner matches `&str` uids, so callers stringify once and borrow the result
/// through [`stack_group_folds`].
pub(crate) fn stack_group_uids(stack: &LayerStack) -> Vec<String> {
    stack.groups().iter().map(|g| g.uid.to_string()).collect()
}

/// The stack's unified groups as owner fold views; `uids` must be [`stack_group_uids`] of the same
/// stack (zipped by position).
pub(crate) fn stack_group_folds<'a>(stack: &LayerStack, uids: &'a [String]) -> Vec<GroupFold<'a>> {
    stack
        .groups()
        .iter()
        .zip(uids)
        .map(|(g, uid)| GroupFold {
            uid,
            visible: g.visible,
            opacity: g.opacity,
        })
        .collect()
}

/// The PS canvas draw plan above the base layers: `(band_z, step)` bottom-to-top (draw in
/// returned order).
///
/// Pure adaptation onto the ONE owner `ordering::composite_plan` (visibility, group fold, band-Z
/// lookup, order): rasters enter in stack order, then texts in `text_layers` order (creation order,
/// newest last = on top), so equal-rank ties resolve by that input order and a raster sits below a
/// text at equal band Z. Items the owner omits (invisible, hidden group, opacity `<= 0` or NaN)
/// produce no step. Characterized by `composite_plan_tests`.
pub(crate) fn composite_steps(
    rasters: &[PsRasterView],
    texts: &[PsTextView<'_>],
    bands: &[Band],
    groups: &[GroupFold<'_>],
) -> Vec<(u32, PsStep)> {
    let raster_items = rasters.iter().map(|raster| CompositeItem {
        key: CompositeKey::Raster { uid: &raster.uid },
        group_uid: raster.group_uid.as_deref(),
        visible: raster.visible,
        opacity: raster.opacity,
    });
    let text_items = texts.iter().map(|text| CompositeItem {
        key: CompositeKey::Text {
            uid: text.uid,
            layer_idx: text.layer_idx,
        },
        group_uid: text.group_uid,
        visible: text.visible,
        opacity: 1.0,
    });
    let items: Vec<CompositeItem<'_>> = raster_items.chain(text_items).collect();

    // `step.item` indexes `items`: `[0, rasters.len())` are rasters, the rest are texts in order.
    ordering::composite_plan(bands, groups, &items)
        .into_iter()
        .filter_map(|step| {
            let ps_step = match rasters.get(step.item) {
                Some(raster) => PsStep::Raster {
                    id: raster.id,
                    opacity: step.opacity,
                },
                None => PsStep::Text {
                    index: texts.get(step.item.checked_sub(rasters.len())?)?.index,
                    opacity: step.opacity,
                },
            };
            Some((step.z, ps_step))
        })
        .collect()
}

/// Source-over composite of premultiplied-alpha colors (`src` painted over `dst`).
///
/// egui's `Color32` stores premultiplied sRGB, so straight additive over-compositing is correct:
/// `out = src + dst * (1 - src_a)`.
pub(crate) fn over(src: Color32, dst: Color32) -> Color32 {
    let sa = src.a() as u32;
    if sa == 255 {
        return src;
    }
    if sa == 0 {
        return dst;
    }
    let inv = 255 - sa;
    let blend = |s: u8, d: u8| -> u8 { (s as u32 + (d as u32 * inv) / 255).min(255) as u8 };
    Color32::from_rgba_premultiplied(
        blend(src.r(), dst.r()),
        blend(src.g(), dst.g()),
        blend(src.b(), dst.b()),
        blend(src.a(), dst.a()),
    )
}

/// Scales a premultiplied colour by a layer opacity in `0.0..=1.0`.
///
/// All four channels scale together, which is exactly what premultiplied alpha means and what
/// `TiledTexture::draw` does at composite time; doing it before [`over`] reproduces the visible
/// composite for the eyedropper.
pub(crate) fn scale_premultiplied(color: Color32, opacity: f32) -> Color32 {
    let k = opacity.clamp(0.0, 1.0);
    if k >= 1.0 {
        return color;
    }
    // `k` is clamped to `0.0..=1.0` and the product is clamped to `0.0..=255.0` before the
    // conversion, so neither cast can truncate or saturate (§17's proven-safe exception).
    let s = |c: u8| (f32::from(c) * k).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgba_premultiplied(s(color.r()), s(color.g()), s(color.b()), s(color.a()))
}

/// The layers of `stack` in COMPOSITE order (bottom to top), with the opacity each contributes at.
///
/// `bound` selects the part of the stack the composite covers; see [`CompositeBound`] for why the
/// inclusive and the exclusive form are named types.
///
/// Hidden layers and layers at zero opacity are skipped, exactly as
/// [`PsEditorTabState::sample_visible_composite`] skips them. TEXT overlays are absent by
/// construction: they are not `LayerStack` layers at all.
///
/// **Why the plain stack order is the composite order.** `sample_visible_composite` sorts the user
/// rasters by their unified band Z because it holds the tab's `bands`. It does not have to: the tab
/// projects the shared `LayerDoc`'s node list — which the doc keeps sorted by `z` — onto the stack
/// through `LayerStack::reorder_rasters`, which preserves that order and keeps the two base layers
/// first. A bottom-to-top walk of `stack.layers()` therefore yields the same sequence the band-Z
/// sort produces, and it is available to code that holds only a `LayerStack` (a tool's
/// `PsToolContext`), where `bands` is not.
pub(crate) fn visible_layers_bottom_to_top(
    stack: &LayerStack,
    bound: CompositeBound,
) -> Vec<CompositeLayer<'_>> {
    stack
        .layers()
        .iter()
        .enumerate()
        .take_while(|(idx, _)| bound.includes(*idx))
        .filter(|(_, layer)| stack.layer_visible(layer))
        .map(|(_, layer)| (layer, stack.layer_opacity(layer)))
        .filter(|(_, opacity)| *opacity > 0.0)
        .collect()
}

/// Composites `layers` (bottom-to-top, src-over) over `rect` of the page into a `rect`-sized image.
///
/// The ROI counterpart of [`composite_to_page`], and the one place the project composites a page
/// RECTANGLE: a whole-page composite is unusable on a ribbon page (~800x19000 px), and sampling a
/// per-pixel helper instead re-resolves the layer order for every pixel. The order and the
/// opacities are resolved ONCE by the caller (`visible_layers_bottom_to_top`), so this function is
/// a pure buffer walk.
///
/// Each layer is sampled through its transform ([`sample_layer_world`]), so rotated, scaled and
/// "incomplete" layers contribute correctly, and its `opacity` is applied before the src-over —
/// which is what `TiledTexture::draw` does at composite time.
///
/// `mask` restricts the composite to the page pixels the selection contains; masked-out pixels stay
/// transparent. `None` composites the whole rectangle.
///
/// Pixels are PREMULTIPLIED, the `Layer::image` convention. A rectangle reaching past the page or
/// past a layer simply samples transparent there, so no clamping is performed or needed.
pub(crate) fn composite_rect(
    layers: &[CompositeLayer<'_>],
    rect: OverlayRectPx,
    mask: Option<&Selection>,
) -> ColorImage {
    let mut out = ColorImage::filled([rect.w, rect.h], Color32::TRANSPARENT);
    for row in 0..rect.h {
        let y = rect.y + row;
        for col in 0..rect.w {
            let x = rect.x + col;
            if mask.is_some_and(|mask| !mask.contains(x, y)) {
                continue;
            }
            let mut px = Color32::TRANSPARENT;
            for (layer, opacity) in layers {
                px = over(scale_premultiplied(sample_layer_world(layer, x, y), *opacity), px);
            }
            out.pixels[row * rect.w + col] = px;
        }
    }
    out
}

/// Composites `layers` (bottom-to-top, src-over) into a fresh page-sized image, sampling each
/// through its transform so rotated/scaled/incomplete layers contribute correctly.
pub(crate) fn composite_to_page(layers: &[&Layer], size: [usize; 2]) -> ColorImage {
    let [w, h] = size;
    let mut out = ColorImage::filled(size, Color32::TRANSPARENT);
    for y in 0..h {
        for x in 0..w {
            let mut px = Color32::TRANSPARENT;
            for layer in layers {
                px = over(sample_layer_world(layer, x, y), px);
            }
            out.pixels[y * w + x] = px;
        }
    }
    out
}
