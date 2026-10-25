/*
File: crates/ms-text-render/src/pipeline/hyphen.rs

Purpose:
The ONE owner of the wrap hyphen: the synthesized hard / wrapped hyphen glyph, the
attrs it is shaped with, the soft-hyphen style offset and where a trailing hyphen sits.
The formula / custom-line path imports these.
*/

use crate::inline_styles::apply_inline_style_to_attrs;
use cosmic_text::{Attrs, AttrsOwned, Buffer, FontSystem, LayoutGlyph, LayoutRun, Metrics, Shaping};
use super::{RunPen, RunShaping, SOFT_HYPHEN};
use super::inline_lookup::inline_style_at_offset;

fn build_hard_hyphen_glyph(
    font_system: &mut FontSystem,
    attrs: &Attrs<'_>,
    font_size_px: f32,
    line_height_px: f32,
) -> Option<LayoutGlyph> {
    let mut buffer = Buffer::new(
        font_system,
        Metrics::new(font_size_px.max(1.0), line_height_px.max(1.0)),
    );
    buffer.set_size(font_system, None, None);
    buffer.set_text(font_system, "-", attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);
    buffer
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first().cloned())
}

/// The hard hyphen drawn at the end of a line that wrapped at a consumed soft
/// hyphen, shaped in its own one-line buffer with the inline style active at that
/// soft hyphen ([`wrapped_hyphen_attrs`]). `next` is the following layout run (the
/// soft hyphen may sit at its start). `None` when the face has no hyphen glyph.
/// The one owner for the horizontal and the formula / custom-line paths.
pub(crate) fn build_wrapped_hyphen_glyph(
    font_system: &mut FontSystem,
    shaping: RunShaping<'_>,
    run: &LayoutRun<'_>,
    next: Option<&LayoutRun<'_>>,
) -> Option<LayoutGlyph> {
    let hyphen_attrs = wrapped_hyphen_attrs(shaping, run, next);
    let hyphen_attrs = hyphen_attrs.as_attrs();
    build_hard_hyphen_glyph(
        font_system,
        &hyphen_attrs,
        shaping.pen.font_size_px,
        shaping.hyphen_line_height_px,
    )
}

/// Attrs of the synthesized wrap hyphen: the base attrs plus the inline style
/// active at the consumed soft hyphen. The selected face's faux fallback
/// weight/style (`RunShaping::faux_face_baseline`) is what a faux span resolves
/// to, so the hyphen matches exactly the same face as the text around it.
fn wrapped_hyphen_attrs(
    shaping: RunShaping<'_>,
    run: &LayoutRun<'_>,
    next: Option<&LayoutRun<'_>>,
) -> AttrsOwned {
    let RunShaping {
        pen,
        attrs: base_attrs,
        faux_face_baseline,
        inline_font_registry,
        ..
    } = shaping;
    let RunPen {
        inline_style_spans,
        layout_line_offsets,
        ..
    } = pen;
    let Some(spans) = inline_style_spans else {
        return AttrsOwned::new(base_attrs);
    };
    let Some(style_offset) = soft_hyphen_style_offset(run, next, layout_line_offsets) else {
        return AttrsOwned::new(base_attrs);
    };
    let Some(style) = inline_style_at_offset(spans, style_offset) else {
        return AttrsOwned::new(base_attrs);
    };
    apply_inline_style_to_attrs(base_attrs, style, inline_font_registry, faux_face_baseline)
}

pub(crate) fn soft_hyphen_style_offset(
    run: &LayoutRun<'_>,
    next: Option<&LayoutRun<'_>>,
    layout_line_offsets: &[usize],
) -> Option<usize> {
    let next_run = next?;
    if next_run.line_i != run.line_i {
        return None;
    }

    let line_offset = layout_line_offsets.get(run.line_i).copied().unwrap_or(0);
    let last_glyph = run.glyphs.last()?;
    let next_first_glyph = next_run.glyphs.first()?;
    let end = last_glyph.end.min(run.text.len());
    let next_start = next_first_glyph.start.min(run.text.len());

    if next_start >= end
        && let Some(slice) = run.text.get(end..next_start)
        && let Some(rel_idx) = slice.find(SOFT_HYPHEN)
    {
        return Some(line_offset + end + rel_idx);
    }

    run.text[..end]
        .rfind(SOFT_HYPHEN)
        .filter(|idx| *idx < end)
        .map(|idx| line_offset + idx)
}

pub(crate) fn run_wraps_at_soft_hyphen(run: &LayoutRun<'_>, next: Option<&LayoutRun<'_>>) -> bool {
    let Some(next_run) = next else {
        return false;
    };
    if next_run.line_i != run.line_i {
        return false;
    }

    let Some(last_glyph) = run.glyphs.last() else {
        return false;
    };
    let Some(next_first_glyph) = next_run.glyphs.first() else {
        return false;
    };

    let end = last_glyph.end.min(run.text.len());
    let next_start = next_first_glyph.start.min(run.text.len());
    if next_start >= end {
        if let Some(slice) = run.text.get(end..next_start)
            && slice.contains(SOFT_HYPHEN)
        {
            return true;
        }
        if run.text[..end].ends_with(SOFT_HYPHEN) {
            return true;
        }
    }
    false
}

pub(crate) fn trailing_hyphen_x(run: &LayoutRun<'_>) -> f32 {
    let mut right = run.line_w;
    for glyph in run.glyphs {
        right = right.max(glyph.x + glyph.w);
    }
    right
}
