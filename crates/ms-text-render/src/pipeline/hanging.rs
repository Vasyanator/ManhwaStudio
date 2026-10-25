/*
File: crates/ms-text-render/src/pipeline/hanging.rs

Purpose:
Hanging punctuation of a horizontal run: which characters hang, the raw (unweighted)
leading / trailing edge-run widths and the edge-run bounds (HANGING PUNCTUATION contract
in `MODULE_README.md`).
*/

use cosmic_text::{LayoutGlyph, LayoutRun};

fn is_hanging_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '.' | ','
            | '!'
            | '?'
            | ':'
            | ';'
            | '-'
            | '–'
            | '—'
            | '~'
            | '…'
            | '·'
            | '•'
            | '。'
            | '、'
            | '，'
            | '．'
            | '！'
            | '？'
            | '：'
            | '；'
            | '・'
            | '･'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '"'
            | '\''
            | '«'
            | '»'
            | '\u{201C}'
            | '\u{201D}'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{2039}'
            | '\u{203A}'
            | '\u{201E}'
            | '\u{201F}'
            | '\u{201A}'
    )
}

/// Widths of the run's LEADING and TRAILING hanging-punctuation runs, in px.
///
/// Returns `(leading_hang_px, trailing_hang_px)`, both `>= 0`, measured against
/// the pen positions `glyph_xs` (which must be one per glyph of `run`) and the
/// run's logical `line_width_px`. Only the EDGE runs hang: punctuation between
/// two ordinary glyphs is never counted.
///
/// Returns `(0.0, 0.0)` — "nothing hangs" — for an empty/mismatched run and for a
/// DEGENERATE line whose glyphs are all hanging punctuation: hanging such a line
/// would leave no visual width to align by, so it is aligned as if the feature
/// were off. That guard is independent of the hanging strength, so the layout
/// does not jump as the strength is raised.
///
/// The caller applies the strength (`TextRenderParams::hanging_weight`); this
/// function knows nothing about it and never touches `glyph_xs`.
pub(super) fn hanging_metrics_for_layout(
    run: &LayoutRun<'_>,
    glyph_xs: &[f32],
    line_width_px: f32,
) -> (f32, f32) {
    if run.glyphs.is_empty() || glyph_xs.len() != run.glyphs.len() {
        return (0.0, 0.0);
    }

    let mut left_boundary = glyph_xs.first().copied().unwrap_or(0.0);
    let mut right_boundary = line_width_px;
    let mut saw_non_hanging = false;

    for (glyph, glyph_x) in run.glyphs.iter().zip(glyph_xs.iter().copied()) {
        if glyph_is_hanging_punctuation(run.text, glyph) {
            if !saw_non_hanging {
                left_boundary = glyph_x + glyph.w;
            }
            continue;
        }
        left_boundary = glyph_x;
        saw_non_hanging = true;
        break;
    }

    if saw_non_hanging {
        for (glyph, glyph_x) in run.glyphs.iter().zip(glyph_xs.iter().copied()).rev() {
            if glyph_is_hanging_punctuation(run.text, glyph) {
                continue;
            }
            right_boundary = glyph_x + glyph.w;
            break;
        }
    } else {
        left_boundary = glyph_xs.first().copied().unwrap_or(0.0);
        right_boundary = line_width_px;
    }

    let left_edge = glyph_xs.first().copied().unwrap_or(0.0);
    let leading_hang_px = (left_boundary - left_edge).max(0.0);
    let trailing_hang_px = (line_width_px - right_boundary).max(0.0);
    // The width the line would be aligned by at FULL strength. Computed here (and
    // with the same left-to-right subtraction the caller uses) only to detect the
    // degenerate all-hanging line.
    let visual_width_px = (line_width_px - leading_hang_px - trailing_hang_px).max(0.0);

    if visual_width_px <= f32::EPSILON {
        (0.0, 0.0)
    } else {
        (leading_hang_px, trailing_hang_px)
    }
}

/// `true` when the glyph's source text is made up entirely of hanging
/// punctuation. `text` must be the glyph's OWN run text, since `LayoutGlyph`
/// byte offsets are run-relative.
///
/// Uses this module's [`is_hanging_punctuation`] set, the SAME predicate the
/// layout-level hang metrics use, so the exclusion and the visual hang can never
/// disagree about what hangs.
#[must_use]
pub(crate) fn glyph_is_hanging_punctuation(text: &str, glyph: &LayoutGlyph) -> bool {
    let end = glyph.end.min(text.len());
    let start = glyph.start.min(end);
    let Some(slice) = text.get(start..end) else {
        return false;
    };
    !slice.is_empty() && slice.chars().all(is_hanging_punctuation)
}

/// Index bounds of a layout run's non-hanging glyphs: returns `(leading_end,
/// trailing_start)` such that glyphs at index `< leading_end` or `>=
/// trailing_start` belong to the line's LEADING/TRAILING hanging-punctuation
/// runs and must be excluded from the extra-info sampling (same edge-run
/// semantics as [`hanging_metrics_for_layout`]). A run that is entirely hanging
/// punctuation (or empty) excludes nothing and returns `(0, len)`.
///
/// Shared with the formula/custom-line draw paths (`formula::render`), which hang
/// punctuation through the same horizontal wrap. `TextLineMode::Vertical` is NOT
/// a caller: its wrap never hangs punctuation, so nothing hangs there to exclude.
pub(crate) fn hanging_edge_run_bounds(run: &LayoutRun<'_>) -> (usize, usize) {
    let len = run.glyphs.len();
    let Some(first_non) = run
        .glyphs
        .iter()
        .position(|glyph| !glyph_is_hanging_punctuation(run.text, glyph))
    else {
        return (0, len);
    };
    let last_non = run
        .glyphs
        .iter()
        .rposition(|glyph| !glyph_is_hanging_punctuation(run.text, glyph))
        .unwrap_or(first_non);
    (first_non, last_non + 1)
}

/// `true` when the glyph at `index` is in the leading/trailing hanging run given
/// `(leading_end, trailing_start)` from [`hanging_edge_run_bounds`].
#[must_use]
pub(crate) fn is_edge_run_hanging(bounds: (usize, usize), index: usize) -> bool {
    index < bounds.0 || index >= bounds.1
}
