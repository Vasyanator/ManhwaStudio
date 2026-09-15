/*
File: src/bin/render_gallery/main.rs

Purpose:
Golden-image regression harness for the production text renderer
(`render_next::render_text_to_image`). Renders a fixed, deterministic set of
representative cases to PNGs so the vector-engine refactor can be compared
tolerance/visual before and after each phase.

How it reaches the engine:
The production renderer now lives in the `ms-text-render` crate, so this harness
simply depends on it (`ms_text_render::render_text_to_image` / `::types::*`) — no
`#[path]` mounts needed anymore.

Key items:
- `Case`: one named render case (params + output name).
- `all_cases()`: the fixed feature-matrix of cases.
- `onpath_case()` / `wave_points()` / `sharp_turn_points()`: builders for the
  custom-vector-line cases. The on-path cases come in pairs that differ ONLY by
  `distance_mode`, so the curvature-compensating `MinimumPreviousDistance` can be
  compared against the plain arc-length walk on identical geometry.
- `raster_line_pixels()` / `write_raster_line_source()`: the generated layout
  image the `CustomRasterLines` case traces its line out of. Written to
  `<outdir>/inputs/`, never next to the goldens.
- `rgba_diff` / `DiffStats`: pure buffer comparison helper for regression tests.
- `main`: parse argv[1]=outdir, render every case, write `<outdir>/<name>.png`.

Notes:
Fully deterministic: no randomness, no time, fixed inputs. Uses the repo font
`test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf` (Latin + Cyrillic).
*/

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use std::sync::Arc;

use ms_text_render::render_text_to_image;
use ms_text_render::{FontContent, FontContentSet, font_content_id};
use ms_text_render::types::{
    AntiAliasingMode, HorizontalAlign, KerningMode, RenderedTextImage, TextDrawnLinesLayoutParams,
    TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams, TextShape,
    TextVectorLine, TextVectorLineDistanceMode, TextVectorLineTextDirection, TextVectorLinesLayoutParams,
    TextVectorPoint, TextWrapMode, VerticalLineDirection,
};

/// One named render case: parameters plus the base file name for its PNG.
struct Case {
    name: &'static str,
    params: TextRenderParams,
}

/// Working name every case references its font by. Resolved through the
/// `FontContentSet` built in `main`.
const GALLERY_FONT_NAME: &str = "gallery-font";

/// Absolute path to the harness font (Latin + Cyrillic coverage), resolved from
/// the crate manifest dir so the bin works regardless of the current directory.
fn font_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
}

/// Builds the in-memory font source for the harness by reading the fixture font
/// once and exposing it under `GALLERY_FONT_NAME`.
fn build_font_set() -> Result<FontContentSet, String> {
    let path = font_path();
    let bytes = std::fs::read(&path)
        .map_err(|err| format!("failed to read harness font {}: {err}", path.display()))?;
    let content_id = font_content_id(&bytes);
    Ok(FontContentSet::new(vec![FontContent {
        name: GALLERY_FONT_NAME.to_string(),
        original_name: GALLERY_FONT_NAME.to_string(),
        data: Arc::new(bytes),
        face_index: 0,
        content_id,
        custom_kerning: None,
    }]))
}

/// Neutral baseline parameters shared by every case. Individual cases clone this
/// and override only the fields relevant to the feature they exercise.
fn base_params() -> TextRenderParams {
    TextRenderParams {
        text: String::new(),
        text_color: [10, 10, 10, 255],
        font_name: GALLERY_FONT_NAME.to_string(),
        font_size_px: 48.0,
        line_spacing_px: 0.0,
        line_spacing_percent: 20.0,
        kerning_mode: KerningMode::Auto,
        kerning_px: 0.0,
        kerning_percent: 0.0,
        glyph_height_percent: 100.0,
        glyph_width_percent: 100.0,
        width_px: 500,
        align: HorizontalAlign::LEFT,
        selected_face_index: 0,
        force_bold: false,
        force_italic: false,
        faux_bold: None,
        faux_italic_slant_deg: None,
        uppercase_text: false,
        trim_extra_spaces: true,
        replace_ellipsis_with_dots: true,
        force_remove_ellipsis_glyph: false,
        hanging_punctuation: 0.0,
        new_line_after_sentence: false,
        enable_inline_style_tags: false,
        text_wrap_mode: TextWrapMode::WholeWords,
        text_shape: TextShape::Free,
        shape_min_width_percent: 55.0,
        shape_variant: 5,
        compare_shape_with: None,
        allow_moderate_trees: false,
        text_line_mode: TextLineMode::Horizontal,
        vertical_line_direction: VerticalLineDirection::RightToLeft,
        text_layout_mode: TextLayoutMode::Normal,
        formula_layout: TextFormulaLayoutParams::default(),
        drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
        vector_lines_layout: TextVectorLinesLayoutParams::default(),
        effects_json: String::new(),
        // Mirror the panel default so the golden output matches the app.
        anti_aliasing: AntiAliasingMode::Strong,
        global_rotation_deg: 0.0,
        line_placement_percent: 0.0,
        line_placement_reference: ms_text_render::types::LinePlacementReference::GlyphHeight,
        raster_transform: None,
        extra_info: ms_text_render::types::RenderExtraInfoRequest::default(),
    }
}

/// Parse an AA mode name (`none`/`sharp`/`crisp`/`strong`/`smooth`) for the
/// optional second CLI argument. Returns `None` for an unknown name.
fn parse_aa_mode(raw: &str) -> Option<AntiAliasingMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => Some(AntiAliasingMode::None),
        "sharp" => Some(AntiAliasingMode::Sharp),
        "crisp" => Some(AntiAliasingMode::Crisp),
        "strong" => Some(AntiAliasingMode::Strong),
        "smooth" => Some(AntiAliasingMode::Smooth),
        _ => None,
    }
}

/// Sample a sine-wave path used by the on-path (custom vector line) case.
/// Deterministic: fixed sample count and closed-form geometry, no randomness.
fn wave_points() -> Vec<TextVectorPoint> {
    let samples: u16 = 48;
    let mut points = Vec::with_capacity(usize::from(samples) + 1);
    for i in 0..=samples {
        let f = f32::from(i) / f32::from(samples);
        let x = 40.0 + f * 820.0;
        let y = 200.0 + 110.0 * (f * std::f32::consts::TAU).sin();
        points.push(TextVectorPoint { x, y });
    }
    points
}

/// A polyline with three SHARP corners (about 60, 75 and 72 degrees), the
/// geometry where on-path spacing is hardest: the chord between two adjacent
/// glyphs falls far behind their arc-length step, and a glyph can reach the one
/// two places before it.
fn sharp_turn_points() -> Vec<TextVectorPoint> {
    [
        (40.0, 90.0),
        (280.0, 90.0),
        (420.0, 330.0),
        (700.0, 330.0),
        (770.0, 110.0),
    ]
    .into_iter()
    .map(|(x, y)| TextVectorPoint { x, y })
    .collect()
}

/// Pixels of the single line drawn into the custom-RASTER-line source image:
/// a shallow one-sided circular arc, EXACTLY ONE pixel per image column.
///
/// One pixel per column matters, it is not a style choice: the tracer
/// (`drawn_lines::trace_ordered_pixels`) walks 8-connected neighbours preferring
/// the straightest one, so a chain two pixels thick anywhere leaves a pixel
/// behind, and the walk doubles back to collect the leftovers once it reaches
/// the far end — which folds the path and piles the tail glyphs on top of each
/// other. The arc is x-monotone with `|dy/dx| <= 0.62`, so stepping x by one and
/// rounding y gives a clean 8-connected chain.
fn raster_line_pixels() -> Vec<(u32, u32)> {
    // Circle through (450, 120) with its center straight below it; only the
    // shallow top section is used, so the bend is consistently one-sided.
    let (center_x, center_y, radius) = (450.0f32, 640.0f32, 520.0f32);
    let half_span = 270.0f32;
    let first = (center_x - half_span) as u32;
    let last = (center_x + half_span) as u32;
    (first..=last)
        .filter_map(|x| {
            let dx = x as f32 - center_x;
            let y = center_y - (radius * radius - dx * dx).sqrt();
            // `as u32` is safe: y stays inside the 420 px canvas by construction.
            (y >= 0.0).then(|| (x, y.round() as u32))
        })
        .collect()
}

/// Write the layout source image the `CustomRasterLines` case traces its line
/// from: one 1-pixel-wide red (`DRAWN_LINE_PALETTE[0]`) chain whose FIRST pixel
/// is opaque (the start marker, alpha >= `start_alpha`) and whose remaining
/// pixels sit at a lower alpha (>= `continuation_alpha`).
///
/// Generated rather than committed so the harness stays a single self-contained
/// binary with no binary fixtures, and deterministic because the geometry is
/// closed-form. It is written under `<outdir>/inputs/`, NOT next to the golden
/// PNGs, so a directory-wide golden comparison never sees it.
///
/// # Errors
/// Returns a message if the directory or the PNG cannot be written.
fn write_raster_line_source(path: &Path) -> Result<(), String> {
    const START_ALPHA: u8 = 255;
    const CONTINUATION_ALPHA: u8 = 128;
    let mut image = image::RgbaImage::new(900, 420);
    for (index, (x, y)) in raster_line_pixels().into_iter().enumerate() {
        if x >= image.width() || y >= image.height() {
            continue;
        }
        let alpha = if index == 0 {
            START_ALPHA
        } else {
            CONTINUATION_ALPHA
        };
        image.put_pixel(x, y, image::Rgba([255, 0, 0, alpha]));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    image
        .save(path)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Build a single-line on-path custom-vector-line case.
///
/// Shares everything except the text, the polyline, the corner smoothing and the
/// spacing mode, so two cases that differ ONLY by `distance_mode` can be
/// compared pixel for pixel on identical geometry.
fn onpath_case(
    text: &str,
    points: Vec<TextVectorPoint>,
    corner_smoothing_px: f32,
    distance_mode: TextVectorLineDistanceMode,
) -> TextRenderParams {
    let mut params = base_params();
    params.text = text.to_string();
    params.font_size_px = 44.0;
    params.text_layout_mode = TextLayoutMode::CustomVectorLines;
    params.text_line_mode = TextLineMode::Horizontal;
    params.vector_lines_layout = TextVectorLinesLayoutParams {
        width_px: 900,
        height_px: 420,
        use_tangent_rotation: true,
        static_rotation_rad: 0.0,
        normal_offset_px: 0.0,
        letter_spacing_mul: 1.0,
        letter_spacing_px: 0.0,
        lines: vec![TextVectorLine {
            points,
            corner_smoothing_px,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode,
            flip_text: false,
        }],
    };
    params
}

/// The fixed feature-matrix of golden cases. Order is stable across runs.
///
/// `raster_lines_image` is the layout source written by
/// [`write_raster_line_source`]; the `CustomRasterLines` case traces its line
/// out of it.
fn all_cases(raster_lines_image: &Path) -> Vec<Case> {
    let paragraph =
        "Привет, мир! Hello world.\nЭто тест переноса строк and mixed Latin текста для базовой проверки раскладки.";

    let mut cases = Vec::new();

    // 1. Horizontal, left align, multi-line Cyrillic+Latin paragraph.
    let mut h_left = base_params();
    h_left.text = paragraph.to_string();
    h_left.width_px = 520;
    h_left.align = HorizontalAlign::LEFT;
    cases.push(Case { name: "h_left", params: h_left });

    // 2. Horizontal, justified, same text, narrower width so wrap + justify fire.
    let mut h_justified = base_params();
    h_justified.text = paragraph.to_string();
    h_justified.width_px = 380;
    h_justified.align = HorizontalAlign::JUSTIFY;
    cases.push(Case { name: "h_justified", params: h_justified });

    // 3. Horizontal with inline tags: bold, per-run color, per-run size change.
    let mut h_inline = base_params();
    h_inline.enable_inline_style_tags = true;
    h_inline.text = "Inline: <b>жирный</b>, <color=#CC2020>красный</color> и <size=84>КРУПНО</size> текст.".to_string();
    h_inline.width_px = 640;
    cases.push(Case { name: "h_inline", params: h_inline });

    // 4. Horizontal, narrow width forcing soft-hyphen wrap of a long Russian word.
    let mut h_softhyphen = base_params();
    h_softhyphen.text =
        "Слово: превысокомногорассмотрительствующий конец переноса.".to_string();
    h_softhyphen.width_px = 200;
    cases.push(Case { name: "h_softhyphen", params: h_softhyphen });

    // 5/6. Shape-aware layout (Oval, Rectangle) via TextShape in Normal layout.
    let shape_text =
        "Форма текста меняет ширину каждой строки so that lines follow a silhouette profile для проверки раскладки по форме.";
    let mut shape_oval = base_params();
    shape_oval.text = shape_text.to_string();
    shape_oval.width_px = 520;
    shape_oval.text_shape = TextShape::Oval;
    cases.push(Case { name: "shape_oval", params: shape_oval });

    let mut shape_rect = base_params();
    shape_rect.text = shape_text.to_string();
    shape_rect.width_px = 520;
    shape_rect.text_shape = TextShape::Rectangle;
    cases.push(Case { name: "shape_rect", params: shape_rect });

    // 7. Vertical text mode, short Cyrillic string.
    let mut vertical = base_params();
    vertical.text = "ГРОМ\nБАМ".to_string();
    vertical.font_size_px = 64.0;
    vertical.width_px = 400;
    vertical.text_line_mode = TextLineMode::Vertical;
    vertical.vertical_line_direction = VerticalLineDirection::RightToLeft;
    cases.push(Case {
        name: "vertical",
        params: vertical.clone(),
    });

    // 7b. Optical-kerning variant of the vertical case. Shares the exact params of
    // `vertical` except `kerning_mode`, so a diff against `vertical` shows the
    // optical vertical path in isolation (the Metric `vertical` case stays
    // byte-identical to the pre-change golden baseline).
    let mut vertical_optical = vertical;
    vertical_optical.kerning_mode = KerningMode::Optical;
    cases.push(Case {
        name: "vertical_optical",
        params: vertical_optical,
    });

    // 8. On-path custom vector line following a sampled wave, min-distance mode.
    let mut onpath = base_params();
    onpath.text = "Волна wave волна curve".to_string();
    onpath.font_size_px = 44.0;
    onpath.text_layout_mode = TextLayoutMode::CustomVectorLines;
    onpath.text_line_mode = TextLineMode::Horizontal;
    onpath.vector_lines_layout = TextVectorLinesLayoutParams {
        width_px: 900,
        height_px: 420,
        use_tangent_rotation: true,
        static_rotation_rad: 0.0,
        normal_offset_px: 0.0,
        letter_spacing_mul: 1.0,
        letter_spacing_px: 0.0,
        lines: vec![TextVectorLine {
            points: wave_points(),
            corner_smoothing_px: 8.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::MinimumPreviousDistance,
            flip_text: false,
        }],
    };
    cases.push(Case { name: "onpath_curve", params: onpath });

    // 8b. The SAME wave in `ByLineLength`, so the two spacing modes can be
    // compared on identical geometry: `ByLineLength` is the straight-line
    // typography the ink mode must reproduce once the bend is taken out.
    cases.push(Case {
        name: "onpath_curve_by_length",
        params: onpath_case(
            "Волна wave волна curve",
            wave_points(),
            8.0,
            TextVectorLineDistanceMode::ByLineLength,
        ),
    });

    // 8c/8d. A sharp-cornered polyline in both modes. Here the chord between
    // adjacent glyphs is far shorter than their arc step and a glyph can reach
    // past its immediate predecessor, so this is where the ink mode has to do
    // real work rather than agree with the arc-length walk.
    cases.push(Case {
        name: "onpath_sharp_turn",
        params: onpath_case(
            "Резкий поворот sharp turn",
            sharp_turn_points(),
            2.0,
            TextVectorLineDistanceMode::MinimumPreviousDistance,
        ),
    });
    cases.push(Case {
        name: "onpath_sharp_turn_by_length",
        params: onpath_case(
            "Резкий поворот sharp turn",
            sharp_turn_points(),
            2.0,
            TextVectorLineDistanceMode::ByLineLength,
        ),
    });

    // 8e/8f. Pairs with strongly asymmetric side bearings (`ГЛ`, `АВ`, `Ту`,
    // `АV`, `j`, quotes): the shapes whose real facing edge is nowhere near
    // their bounding box, i.e. the ones a bbox-half-width spacing target got
    // most wrong.
    cases.push(Case {
        name: "onpath_sidebearings",
        params: onpath_case(
            "ГЛАВА Ту АV j «Ж»",
            wave_points(),
            8.0,
            TextVectorLineDistanceMode::MinimumPreviousDistance,
        ),
    });
    cases.push(Case {
        name: "onpath_sidebearings_by_length",
        params: onpath_case(
            "ГЛАВА Ту АV j «Ж»",
            wave_points(),
            8.0,
            TextVectorLineDistanceMode::ByLineLength,
        ),
    });

    // 8g. `TextLayoutMode::Formula`. The other on-path cases are all
    // `CustomVectorLines`; this one is the only golden that exercises the
    // FORMULA arc-length accumulator and `OnPathStepSpacing::for_formula` (whose
    // seed floor is half the em, not 1 px), so a change to the step arithmetic
    // can no longer slip through unnoticed on that path.
    let mut formula_wave = base_params();
    formula_wave.text = "Формула formula волна".to_string();
    formula_wave.font_size_px = 44.0;
    formula_wave.width_px = 820;
    formula_wave.text_layout_mode = TextLayoutMode::Formula;
    formula_wave.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * w".to_string(),
        y_expr: "130 * sin(t * tau)".to_string(),
        rotation_expr: "0".to_string(),
        use_tangent_rotation: true,
        ..TextFormulaLayoutParams::default()
    };
    cases.push(Case { name: "formula_wave", params: formula_wave });

    // 8h. `TextLayoutMode::CustomRasterLines`, the third consumer of the same
    // on-path step. It traces its line out of the generated layout image, which
    // also keeps the raster tracer itself (`drawn_lines::load_raster_line_paths`)
    // under a golden.
    let mut raster_line = base_params();
    raster_line.text = "Растровая линия raster".to_string();
    raster_line.font_size_px = 40.0;
    raster_line.text_layout_mode = TextLayoutMode::CustomRasterLines;
    raster_line.drawn_lines_layout = TextDrawnLinesLayoutParams {
        image_path: Some(raster_lines_image.to_path_buf()),
        ..TextDrawnLinesLayoutParams::default()
    };
    cases.push(Case { name: "raster_line_arc", params: raster_line });

    // 9. Stroke + shadow effects, locked via JSON, on white text.
    let mut effects = base_params();
    effects.text = "ЭФФЕКТ FX".to_string();
    effects.text_color = [255, 255, 255, 255];
    effects.font_size_px = 80.0;
    effects.width_px = 520;
    effects.effects_json = r#"[
        {"effect":"stroke","width":5,"color":[20,20,20,255],"opacity_mode":"from_contour","opacity":100},
        {"effect":"shadow","offset_x":10,"offset_y":10,"blur":8,"color":[0,0,0,180],"opacity":70}
    ]"#
    .to_string();
    cases.push(Case { name: "effect_stroke_shadow", params: effects });

    // 10/11. Optical-kerning variants of the horizontal cases. These share the
    // exact params of `h_left`/`h_justified` except `kerning_mode`, so a diff
    // against them shows the optical horizontal path in isolation (the Metric
    // cases above stay byte-identical to the pre-change golden baseline).
    let mut h_left_optical = base_params();
    h_left_optical.text = paragraph.to_string();
    h_left_optical.width_px = 520;
    h_left_optical.align = HorizontalAlign::LEFT;
    h_left_optical.kerning_mode = KerningMode::Optical;
    cases.push(Case { name: "h_left_optical", params: h_left_optical });

    let mut h_justified_optical = base_params();
    h_justified_optical.text = paragraph.to_string();
    h_justified_optical.width_px = 380;
    h_justified_optical.align = HorizontalAlign::JUSTIFY;
    h_justified_optical.kerning_mode = KerningMode::Optical;
    cases.push(Case { name: "h_justified_optical", params: h_justified_optical });

    // 12/13/14. Cyrillic Fixed vs Auto vs Optical triplet. All three share
    // identical params except `kerning_mode`, so diffs isolate each kerning path.
    // `auto_cyrillic` (font-pair kerning) is byte-identical to the pre-change
    // `metric_cyrillic` golden (Auto == old Metric); `fixed_cyrillic` drops the
    // font pair kerning (own-advance spacing) so it visibly differs from Auto when
    // the font has kern pairs. An inline color span on "текст" exercises the
    // per-glyph draw scale path in the measurement.
    let cyrillic_text =
        "Тестовый <color=#CC2020>текст</color>".to_string();
    let mut auto_cyrillic = base_params();
    auto_cyrillic.enable_inline_style_tags = true;
    auto_cyrillic.text = cyrillic_text.clone();
    auto_cyrillic.width_px = 640;
    auto_cyrillic.kerning_mode = KerningMode::Auto;
    cases.push(Case { name: "auto_cyrillic", params: auto_cyrillic });

    let mut fixed_cyrillic = base_params();
    fixed_cyrillic.enable_inline_style_tags = true;
    fixed_cyrillic.text = cyrillic_text.clone();
    fixed_cyrillic.width_px = 640;
    fixed_cyrillic.kerning_mode = KerningMode::Fixed;
    cases.push(Case { name: "fixed_cyrillic", params: fixed_cyrillic });

    let mut optical_cyrillic = base_params();
    optical_cyrillic.enable_inline_style_tags = true;
    optical_cyrillic.text = cyrillic_text;
    optical_cyrillic.width_px = 640;
    optical_cyrillic.kerning_mode = KerningMode::Optical;
    cases.push(Case { name: "optical_cyrillic", params: optical_cyrillic });

    cases
}

/// Write a rendered RGBA image to `path` as PNG.
///
/// # Errors
/// Returns a message if the buffer length does not match `width*height*4` or if
/// the PNG cannot be encoded/written.
fn write_png(path: &Path, image: &RenderedTextImage) -> Result<(), String> {
    let buffer = image::RgbaImage::from_raw(image.width, image.height, image.rgba.clone())
        .ok_or_else(|| {
            format!(
                "buffer length {} does not match {}x{}x4",
                image.rgba.len(),
                image.width,
                image.height
            )
        })?;
    buffer
        .save(path)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Per-pixel RGBA difference statistics between two equally sized buffers.
/// Test-only regression helper (used by the `tests` module below).
#[cfg(test)]
#[derive(Debug, Clone)]
struct DiffStats {
    /// Largest per-channel absolute delta across all pixels/channels.
    max_channel_delta: u8,
    /// Per-pixel maximum channel delta (one entry per pixel).
    pixel_max_deltas: Vec<u8>,
}

#[cfg(test)]
impl DiffStats {
    /// Percentage (0.0..=100.0) of pixels whose maximum channel delta strictly
    /// exceeds `threshold`.
    fn pct_pixels_over(&self, threshold: u8) -> f64 {
        if self.pixel_max_deltas.is_empty() {
            return 0.0;
        }
        let over = self
            .pixel_max_deltas
            .iter()
            .filter(|&&delta| delta > threshold)
            .count();
        (over as f64 / self.pixel_max_deltas.len() as f64) * 100.0
    }
}

/// Compute per-pixel RGBA delta statistics between two buffers of the same
/// dimensions. Pure and deterministic.
///
/// # Errors
/// Returns a message if the buffers do not both equal `width*height*4` bytes.
#[cfg(test)]
fn rgba_diff(a: &[u8], b: &[u8], width: u32, height: u32) -> Result<DiffStats, String> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().map(|h| w * h))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "rgba_diff: dimensions overflow usize".to_string())?;
    if a.len() != expected || b.len() != expected {
        return Err(format!(
            "rgba_diff: buffer lengths {}/{} do not match {width}x{height}x4 = {expected}",
            a.len(),
            b.len()
        ));
    }

    let pixel_count = expected / 4;
    let mut pixel_max_deltas = Vec::with_capacity(pixel_count);
    let mut max_channel_delta = 0u8;
    for pixel in 0..pixel_count {
        let base = pixel * 4;
        let mut pixel_delta = 0u8;
        for channel in 0..4 {
            let delta = a[base + channel].abs_diff(b[base + channel]);
            pixel_delta = pixel_delta.max(delta);
        }
        max_channel_delta = max_channel_delta.max(pixel_delta);
        pixel_max_deltas.push(pixel_delta);
    }

    Ok(DiffStats {
        max_channel_delta,
        pixel_max_deltas,
    })
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(outdir) = args.next() else {
        eprintln!("usage: render_gallery <output_dir> [aa_mode]");
        return ExitCode::FAILURE;
    };
    // Optional second arg selects the AA mode; default mirrors the panel default.
    let aa_mode = match args.next() {
        Some(name) => match parse_aa_mode(&name) {
            Some(mode) => mode,
            None => {
                eprintln!(
                    "unknown aa_mode {name:?}; expected none|sharp|crisp|strong|smooth"
                );
                return ExitCode::FAILURE;
            }
        },
        None => AntiAliasingMode::Strong,
    };
    let outdir = PathBuf::from(outdir);
    if let Err(err) = std::fs::create_dir_all(&outdir) {
        eprintln!("failed to create output dir {}: {err}", outdir.display());
        return ExitCode::FAILURE;
    }

    let font_set = match build_font_set() {
        Ok(set) => set,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    // The custom-raster-line case needs a layout image to trace; it is generated
    // into a SUBDIRECTORY so it never lands among the golden PNGs themselves.
    let raster_lines_image = outdir.join("inputs").join("raster_lines.png");
    if let Err(err) = write_raster_line_source(&raster_lines_image) {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }

    let mut cases = all_cases(&raster_lines_image);
    for case in &mut cases {
        case.params.anti_aliasing = aa_mode;
    }
    let mut ok = 0usize;
    let mut failed = 0usize;
    for case in &cases {
        let path = outdir.join(format!("{}.png", case.name));
        match render_text_to_image(&case.params, &font_set, None) {
            Ok(image) => match write_png(&path, &image) {
                Ok(()) => {
                    ok += 1;
                    println!("{}: {}x{}", case.name, image.width, image.height);
                }
                Err(err) => {
                    failed += 1;
                    eprintln!("{}: WRITE FAILED: {err}", case.name);
                }
            },
            Err(err) => {
                failed += 1;
                eprintln!("{}: RENDER FAILED: {err}", case.name);
            }
        }
    }

    println!("done: {ok} ok, {failed} failed, out={}", outdir.display());
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::{rgba_diff, DiffStats};

    #[test]
    fn rgba_diff_identical_is_zero() {
        let buf = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        let stats = rgba_diff(&buf, &buf, 2, 1).expect("equal-size diff");
        assert_eq!(stats.max_channel_delta, 0);
        assert_eq!(stats.pct_pixels_over(0), 0.0);
    }

    #[test]
    fn rgba_diff_shifted_buffer() {
        let a = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        // Shift every channel by +10 (clamped is irrelevant; values stay < 255).
        let b: Vec<u8> = a.iter().map(|&v| v.saturating_add(10)).collect();
        let stats = rgba_diff(&a, &b, 2, 1).expect("equal-size diff");
        assert_eq!(stats.max_channel_delta, 10);
        // Every pixel differs by 10, so all exceed threshold 5 and none exceed 20.
        assert_eq!(stats.pct_pixels_over(5), 100.0);
        assert_eq!(stats.pct_pixels_over(20), 0.0);
    }

    #[test]
    fn rgba_diff_rejects_mismatched_lengths() {
        let a = vec![0u8; 8];
        let b = vec![0u8; 4];
        assert!(rgba_diff(&a, &b, 2, 1).is_err());
    }

    #[test]
    fn diff_stats_empty_is_zero_pct() {
        let stats = DiffStats {
            max_channel_delta: 0,
            pixel_max_deltas: Vec::new(),
        };
        assert_eq!(stats.pct_pixels_over(0), 0.0);
    }
}
