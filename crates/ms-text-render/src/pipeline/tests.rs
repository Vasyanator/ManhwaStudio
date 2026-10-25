//! Unit tests of the render pipeline (`pipeline/mod.rs` and its children), including the
//! pen characterization matrix (`pen_characterization_matrix`, fixture
//! `tests/fixtures/pair_advance_pen_matrix.json`).

use super::faux::{SYNTHESIZED_ITALIC_SLANT_DEG, base_attrs_real_bold_italic, faux_bounds_pads};
use super::text_prep::{prepare_source_text, replace_ellipsis_with_dots};
use super::{FauxGlyphStyle, apply_effects_to_image};
use crate::vector::FauxOutlineParams;
use crate::font_provider::{FontContent, FontContentSet, font_content_id};
use crate::types::{
    AntiAliasingMode, FauxBoldParams, HorizontalAlign, KerningMode, RenderExtraInfoRequest,
    RenderedTextImage, TextDrawnLinesLayoutParams, TextFormulaLayoutParams, TextLayoutMode,
    TextLineMode, TextRenderParams, TextRenderShapeCompareParams, TextShape, TextVectorLine,
    TextVectorLineDistanceMode, TextVectorLineTextDirection, TextVectorLinesLayoutParams,
    TextVectorPoint, TextWrapMode, VectorMeshWarp, VerticalLineDirection,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn test_font_path() -> PathBuf {
    crate::test_fixtures::caller_font_path()
}

/// Builds a single-font provider that resolves `params.font_name` to the test
/// fixture bytes, so the render path never touches the filesystem itself.
fn font_provider(params: &TextRenderParams) -> FontContentSet {
    let bytes = std::fs::read(test_font_path()).unwrap_or_default();
    let content_id = font_content_id(&bytes);
    FontContentSet::new(vec![FontContent {
        name: params.font_name.clone(),
        original_name: params.font_name.clone(),
        data: Arc::new(bytes),
        face_index: params.selected_face_index,
        content_id,
        custom_kerning: None,
    }])
}

/// Local test wrapper matching the pre-refactor `render_text_to_image(&params,
/// cancel)` call shape: builds the fixture font provider from
/// `params.font_name` and delegates to the real entry point.
fn render_text_to_image(
    params: &TextRenderParams,
    cancel: Option<(&Arc<AtomicU64>, u64)>,
) -> Result<RenderedTextImage, String> {
    crate::pipeline::render_text_to_image(params, &font_provider(params), cancel)
}

fn base_params() -> TextRenderParams {
    TextRenderParams {
        text: "Hello world".to_string(),
        text_color: [255, 255, 255, 255],
        font_name: "test-font".to_string(),
        font_size_px: 36.0,
        line_spacing_px: 0.0,
        line_spacing_percent: 100.0,
        kerning_mode: KerningMode::Auto,
        kerning_px: 0.0,
        kerning_percent: 0.0,
        glyph_height_percent: 100.0,
        glyph_width_percent: 100.0,
        width_px: 256,
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
        shape_min_width_percent: 100.0,
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
        // Identity transfer so existing raster/geometry assertions in these
        // tests keep matching the pre-AA coverage exactly.
        anti_aliasing: AntiAliasingMode::Smooth,
        global_rotation_deg: 0.0,
        line_placement_percent: 0.0,
        line_placement_reference: crate::types::LinePlacementReference::GlyphHeight,
        raster_transform: None,
        extra_info: crate::types::RenderExtraInfoRequest::default(),
    }
}

// The optical pure-numeric core (`median_of_gaps`, `optical_delta`,
// `optical_base_advance`) is unit-tested in `super::super::optical` since it
// is shared by the horizontal and vertical paths.

#[test]
fn fixed_kerning_drops_font_pair_kerning_versus_auto() {
    // Text loaded with negative-kern pairs (AV/VA/To/Yo/Wa in the Arimo fixture).
    // `Auto` applies the font's GPOS/`kern` pair kerning (shaped positions);
    // `Fixed` steps by each glyph's OWN advance, so the pairs are NOT pulled
    // together. With kern pairs present, the two renders must differ.
    let kern_text = "AVA To Yo Wa VA";
    let mut auto_params = base_params();
    auto_params.text = kern_text.to_string();
    auto_params.width_px = 640;
    auto_params.kerning_mode = KerningMode::Auto;

    let mut fixed_params = auto_params.clone();
    fixed_params.kerning_mode = KerningMode::Fixed;

    let auto = render_text_to_image(&auto_params, None)
        .expect("Auto kerning render should succeed");
    let fixed = render_text_to_image(&fixed_params, None)
        .expect("Fixed kerning render should succeed");

    // Fixed spacing is looser (own advance, no negative kern), so the inked
    // content is at least as wide as Auto and the buffers differ.
    assert!(
        fixed.width >= auto.width,
        "Fixed own-advance spacing should be no narrower than Auto (fixed {} vs auto {})",
        fixed.width,
        auto.width
    );
    assert_ne!(
        (auto.width, auto.height, &auto.rgba),
        (fixed.width, fixed.height, &fixed.rgba),
        "Fixed must drop font pair kerning and differ from Auto for kern-pair text"
    );
}

#[test]
fn auto_kerning_matches_default_shaped_positions() {
    // `Auto` with zero manual tracking is the fast byte-identical shaped-position
    // path (the historical `Metric` behavior). Rendering the same text twice is
    // deterministic and stable across the enum rename.
    let mut params = base_params();
    params.text = "AVA To Yo".to_string();
    params.kerning_mode = KerningMode::Auto;
    let a = render_text_to_image(&params, None).expect("Auto render should succeed");
    let b = render_text_to_image(&params, None).expect("Auto render should succeed");
    assert_eq!((a.width, a.height, a.rgba), (b.width, b.height, b.rgba));
}

fn alpha_bounds_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<(usize, usize)> {
    let width = width as usize;
    let height = height as usize;
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut found = false;
    for y in 0..height {
        for x in 0..width {
            if rgba[(y * width + x) * 4 + 3] == 0 {
                continue;
            }
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            found = true;
        }
    }
    found.then_some((
        max_x.saturating_sub(min_x).saturating_add(1),
        max_y.saturating_sub(min_y).saturating_add(1),
    ))
}

fn alpha_centroid_y(width: u32, height: u32, rgba: &[u8]) -> Option<f32> {
    let width = width as usize;
    let height = height as usize;
    let mut weighted_y = 0.0f32;
    let mut alpha_sum = 0.0f32;
    for y in 0..height {
        for x in 0..width {
            let alpha = f32::from(rgba[(y * width + x) * 4 + 3]);
            if alpha <= 0.0 {
                continue;
            }
            weighted_y += y as f32 * alpha;
            alpha_sum += alpha;
        }
    }
    (alpha_sum > 0.0).then_some(weighted_y / alpha_sum)
}

/// Alpha that counts as ink for the glyph-geometry probes below. Above the
/// anti-aliasing fringe of an outline edge, so a row/column is reported as
/// ink only where the glyph really covers it.
const INK_ALPHA: u8 = 32;

/// Ink bounding box `(min_x, max_x, min_y, max_y)` of every CONNECTED run of
/// inked columns, split on fully empty columns and returned left to right.
///
/// With space-separated glyphs this is one entry per glyph, so `max_y` is
/// that glyph's ink BOTTOM — the row a baseline-anchored glyph shares with
/// its unscaled neighbours (`H`, `.` and friends have no descender).
fn ink_column_clusters(image: &RenderedTextImage) -> Vec<(usize, usize, usize, usize)> {
    let width = image.width as usize;
    let height = image.height as usize;
    let mut clusters: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut open: Option<(usize, usize, usize, usize)> = None;
    for x in 0..width {
        let mut column_min_y = None;
        let mut column_max_y = 0usize;
        for y in 0..height {
            if image.rgba[(y * width + x) * 4 + 3] < INK_ALPHA {
                continue;
            }
            column_min_y.get_or_insert(y);
            column_max_y = y;
        }
        match (column_min_y, open.as_mut()) {
            (Some(min_y), Some(current)) => {
                current.1 = x;
                current.2 = current.2.min(min_y);
                current.3 = current.3.max(column_max_y);
            }
            (Some(min_y), None) => open = Some((x, x, min_y, column_max_y)),
            (None, _) => {
                if let Some(finished) = open.take() {
                    clusters.push(finished);
                }
            }
        }
    }
    clusters.extend(open);
    clusters
}

/// Ink row bands `(min_y, max_y)`, split on fully empty rows, top to bottom.
/// One entry per text line as long as neighbouring lines do not overlap.
fn ink_row_bands(image: &RenderedTextImage) -> Vec<(usize, usize)> {
    let width = image.width as usize;
    let height = image.height as usize;
    let mut bands: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    for y in 0..height {
        let inked = (0..width).any(|x| image.rgba[(y * width + x) * 4 + 3] >= INK_ALPHA);
        match (inked, open.as_mut()) {
            (true, Some(current)) => current.1 = y,
            (true, None) => open = Some((y, y)),
            (false, _) => {
                if let Some(finished) = open.take() {
                    bands.push(finished);
                }
            }
        }
    }
    bands.extend(open);
    bands
}

/// Faux params for the render tests: strong enough that the geometric
/// growth is unambiguous against AA noise.
fn test_faux_bold(thicken_percent: f32, expand_percent: f32) -> FauxBoldParams {
    FauxBoldParams {
        thicken_percent,
        expand_percent,
        sharp_corners: true,
        outward_only: true,
    }
}

#[test]
fn faux_zero_strength_and_ungated_params_are_byte_identical_to_plain() {
    let base = base_params();
    let plain = render_text_to_image(&base, None).expect("plain render");

    // Faux params WITHOUT the force flag are ignored entirely.
    let mut ungated = base.clone();
    ungated.faux_bold = Some(test_faux_bold(10.0, 5.0));
    ungated.faux_italic_slant_deg = Some(20.0);
    let ungated_render = render_text_to_image(&ungated, None).expect("ungated render");
    assert_eq!(
        (plain.width, plain.height, &plain.rgba),
        (ungated_render.width, ungated_render.height, &ungated_render.rgba),
        "faux params without force flags must be byte-identical to plain"
    );

    // Zero-strength faux bold keeps the Regular face and offsets nothing.
    let mut zero_bold = base.clone();
    zero_bold.force_bold = true;
    zero_bold.faux_bold = Some(test_faux_bold(0.0, 0.0));
    let zero_bold_render = render_text_to_image(&zero_bold, None).expect("zero-bold render");
    assert_eq!(
        (plain.width, plain.height, &plain.rgba),
        (zero_bold_render.width, zero_bold_render.height, &zero_bold_render.rgba),
        "zero-strength faux bold must be byte-identical to plain"
    );

    // Zero slant shears nothing.
    let mut zero_italic = base.clone();
    zero_italic.force_italic = true;
    zero_italic.faux_italic_slant_deg = Some(0.0);
    let zero_italic_render =
        render_text_to_image(&zero_italic, None).expect("zero-italic render");
    assert_eq!(
        (plain.width, plain.height, &plain.rgba),
        (zero_italic_render.width, zero_italic_render.height, &zero_italic_render.rgba),
        "zero faux slant must be byte-identical to plain"
    );
}

#[test]
fn faux_bold_attrs_keep_regular_face() {
    // Faux path must NOT request the real Bold/Italic faces from font
    // matching; the legacy force flags without faux still do.
    let mut params = base_params();
    params.force_bold = true;
    params.force_italic = true;
    assert_eq!(base_attrs_real_bold_italic(&params), (true, true));
    params.faux_bold = Some(test_faux_bold(3.0, 0.0));
    assert_eq!(base_attrs_real_bold_italic(&params), (false, true));
    params.faux_italic_slant_deg = Some(14.0);
    assert_eq!(base_attrs_real_bold_italic(&params), (false, false));
    params.force_bold = false;
    params.force_italic = false;
    assert_eq!(base_attrs_real_bold_italic(&params), (false, false));
}

#[test]
fn faux_bold_grows_ink_extent_by_offset_and_advance() {
    // Two glyphs: the total ink width grows by the pen-step growth
    // (2*d + expand) plus one `d` of outline growth on each outer edge,
    // i.e. 4*d + expand; the ink height grows by 2*d.
    let mut plain = base_params();
    plain.text = "HH".to_string();
    plain.font_size_px = 48.0;
    let plain_render = render_text_to_image(&plain, None).expect("plain render");
    let (plain_w, plain_h) =
        alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
            .expect("plain ink");

    let thicken = 5.0f32;
    let expand = 10.0f32;
    let d = thicken / 100.0 * plain.font_size_px;
    let expand_px = expand / 100.0 * plain.font_size_px;
    let mut faux = plain.clone();
    faux.force_bold = true;
    faux.faux_bold = Some(test_faux_bold(thicken, expand));
    let faux_render = render_text_to_image(&faux, None).expect("faux render");
    let (faux_w, faux_h) =
        alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
            .expect("faux ink");

    let expected_w_growth = 4.0 * d + expand_px;
    let w_growth = faux_w as f32 - plain_w as f32;
    assert!(
        (w_growth - expected_w_growth).abs() <= 2.0,
        "ink width growth {w_growth} should be ~{expected_w_growth} (d={d}, expand={expand_px})"
    );
    let h_growth = faux_h as f32 - plain_h as f32;
    assert!(
        (h_growth - 2.0 * d).abs() <= 2.0,
        "ink height growth {h_growth} should be ~{}",
        2.0 * d
    );
}

#[test]
fn faux_bold_compensates_counter_bearing_glyphs_in_ink_and_advance() {
    // `outward_only` keeps the counter, so the outer contour of a
    // counter-bearing glyph moves by 2*d to give its strokes the same
    // weight the two moving edges of `H` produce. Both the ink and the pen
    // must follow: per glyph the ink grows by 2*(2*d) on each axis and the
    // pen step by 2*(2*d), so two glyphs grow by 8*d in width and 4*d in
    // height. An UNCOMPENSATED advance would leave only 6*d of width growth
    // and overlap the two letters by 2*d.
    let mut plain = base_params();
    plain.text = "OO".to_string();
    plain.font_size_px = 48.0;
    let plain_render = render_text_to_image(&plain, None).expect("plain render");
    let (plain_w, plain_h) =
        alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
            .expect("plain ink");

    let thicken = 5.0f32;
    let d = thicken / 100.0 * plain.font_size_px;
    let mut faux = plain.clone();
    faux.force_bold = true;
    faux.faux_bold = Some(test_faux_bold(thicken, 0.0));
    let faux_render = render_text_to_image(&faux, None).expect("faux render");
    let (faux_w, faux_h) =
        alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
            .expect("faux ink");

    let w_growth = faux_w as f32 - plain_w as f32;
    assert!(
        (w_growth - 8.0 * d).abs() <= 3.0,
        "counter-bearing ink width growth {w_growth} should be ~{} (d={d})",
        8.0 * d
    );
    let h_growth = faux_h as f32 - plain_h as f32;
    assert!(
        (h_growth - 4.0 * d).abs() <= 3.0,
        "counter-bearing ink height growth {h_growth} should be ~{}",
        4.0 * d
    );
}

#[test]
fn faux_thinning_shrinks_ink_extent_and_pulls_the_advance_in() {
    // Mirror of `faux_bold_grows_ink_extent_by_offset_and_advance`: a
    // NEGATIVE thicken pulls each outer edge into the ink by |d| and each
    // pen step in by 2*|d|, so two glyphs lose ~4*|d| of total width. A
    // shrink of only ~2*|d| would mean the advance ignored the sign.
    let mut plain = base_params();
    plain.text = "HH".to_string();
    plain.font_size_px = 64.0;
    let plain_render = render_text_to_image(&plain, None).expect("plain render");
    let (plain_w, plain_h) =
        alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
            .expect("plain ink");

    let d = 3.0 / 100.0 * plain.font_size_px;
    let mut thinned = plain.clone();
    thinned.force_bold = true;
    thinned.faux_bold = Some(test_faux_bold(-3.0, 0.0));
    let thinned_render = render_text_to_image(&thinned, None).expect("thinned render");
    let (thin_w, thin_h) = alpha_bounds_from_rgba(
        thinned_render.width,
        thinned_render.height,
        &thinned_render.rgba,
    )
    .expect("thinned ink");

    let w_shrink = plain_w as f32 - thin_w as f32;
    assert!(
        (w_shrink - 4.0 * d).abs() <= 2.5,
        "ink width shrink {w_shrink} should be ~{} (d=-{d})",
        4.0 * d
    );
    let h_shrink = plain_h as f32 - thin_h as f32;
    assert!(
        (h_shrink - 2.0 * d).abs() <= 2.5,
        "ink height shrink {h_shrink} should be ~{}",
        2.0 * d
    );
}

/// Shape `text` with the fixture font and run the REAL horizontal metric
/// accumulation (`horizontal_run_layout`) over its first layout run.
///
/// Returns the accumulated pen positions and the logical (alignment) width.
/// The `FontSystem` holds ONLY the fixture face — a test binary cannot
/// resolve the shipped `fonts/ui` bundle (see the module Testing Guidance),
/// and this contract is about arithmetic, not about the bundle.
fn fixture_run_layout(text: &str, params: &TextRenderParams) -> (Vec<f32>, f32) {
    fixture_run_layout_with_custom_kerning(text, params, &[])
}

/// [`fixture_run_layout`] with user-authored kerning overrides bound to the
/// fixture face, as `(left, right, offset_per_mille)` triples.
///
/// The face id comes straight out of the database this test built, which is
/// exactly what `CustomKerningMap::record` derives from the load cache in
/// production — so the pen loop sees an override the same way either way.
fn fixture_run_layout_with_custom_kerning(
    text: &str,
    params: &TextRenderParams,
    pairs: &[(char, char, f32)],
) -> (Vec<f32>, f32) {
    use crate::font_provider::CustomKerningTable;
    use crate::font_registry::CustomKerningMap;
    use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, fontdb};

    let bytes = std::fs::read(test_font_path()).expect("fixture font bytes");
    let mut db = fontdb::Database::new();
    db.load_font_data(bytes);
    let face = db.faces().next().expect("fixture face");
    let face_id = face.id;
    let family = face
        .families
        .first()
        .cloned()
        .map(|(name, _language)| name)
        .expect("fixture family name");
    let mut custom_kerning = CustomKerningMap::default();
    if !pairs.is_empty() {
        custom_kerning.insert(
            face_id,
            Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
            "fixture",
        );
    }
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

    let em = params.font_size_px;
    let mut buffer = Buffer::new(&mut font_system, Metrics::new(em, em * 1.2));
    buffer.set_size(&mut font_system, None, None);
    let attrs = Attrs::new()
        .family(Family::Name(family.as_str()))
        .metrics(Metrics::new(em, em));
    buffer.set_text(&mut font_system, text, &attrs, Shaping::Advanced);
    buffer.shape_until_scroll(&mut font_system, false);

    let mut caches = super::GlyphCaches::new();
    let run = buffer.layout_runs().next().expect("one layout run");
    let layout = super::horizontal_run_layout(
        super::RunPen {
            params,
            inline_style_spans: None,
            layout_line_offsets: &[0],
            custom_kerning: &custom_kerning,
            font_size_px: em,
        },
        &run,
        &mut font_system,
        &mut caches,
    );
    (layout.glyph_xs, layout.line_width_px)
}

/// Params for a thinned run of `text` at the minimum (most negative)
/// strength — the case where the faux delta can exceed a glyph's advance.
fn thinned_run_params(text: &str) -> TextRenderParams {
    let mut params = base_params();
    params.text = text.to_string();
    params.font_size_px = 48.0;
    params.force_bold = true;
    params.faux_bold = Some(test_faux_bold(crate::types::FAUX_THICKEN_PERCENT_MIN, 0.0));
    params
}

#[test]
fn a_fully_consumed_glyph_does_not_vote_on_the_extra_info_centers() {
    // At the minimum strength and em 48 the contraction (2*|d| = 4.8 px)
    // exceeds an Arimo stem, so `l` loses every contour and draws
    // NOTHING while `o` survives. The reported ink centers must therefore
    // describe the `o` alone. Before the exclusion the phantom `l` box
    // dragged them to mean [5.25, 8.21] / median [1.25, 8.75] — the median
    // sitting 1.25 px from the left edge of an image whose only ink is the
    // `o` spanning its full width.
    let mut solo = thinned_run_params("o");
    solo.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: true,
    };
    let solo_render = render_text_to_image(&solo, None).expect("solo render");
    let solo_mean = solo_render.extra.mean_center.expect("solo mean center");
    let solo_median = solo_render.extra.median_center.expect("solo median center");

    let mut pair = solo.clone();
    pair.text = "lo".to_string();
    let pair_render = render_text_to_image(&pair, None).expect("pair render");
    let mean = pair_render.extra.mean_center.expect("pair mean center");
    let median = pair_render.extra.median_center.expect("pair median center");
    for (label, got, want) in [("mean", mean, solo_mean), ("median", median, solo_median)] {
        assert!(
            (got[0] - want[0]).abs() <= 1.5 && (got[1] - want[1]).abs() <= 1.5,
            "{label} center {got:?} of \"lo\" must match the surviving `o` alone {want:?}"
        );
        assert!(
            got[0] >= 0.0
                && got[1] >= 0.0
                && got[0] <= pair_render.width as f32
                && got[1] <= pair_render.height as f32,
            "{label} center {got:?} fell outside the {}x{} image",
            pair_render.width,
            pair_render.height
        );
    }

    // A run in which EVERY glyph is consumed reports no center at all
    // rather than the centroid of boxes that drew nothing.
    let mut all_gone = solo.clone();
    all_gone.text = "Il".to_string();
    let gone_render = render_text_to_image(&all_gone, None).expect("consumed render");
    assert!(
        gone_render.rgba.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0),
        "the run should have drawn no ink at all"
    );
    assert_eq!(gone_render.extra.mean_center, None);
    assert_eq!(gone_render.extra.median_center, None);
}

#[test]
fn faux_floored_advance_never_reverses_the_pen() {
    use super::run_layout::faux_floored_advance;

    // No faux delta: the shaped advance is returned bit-for-bit, in both
    // directions, so a run without faux bold cannot move by a single ulp.
    for base in [0.0f32, 13.336, -26.695, f32::MIN_POSITIVE] {
        assert_eq!(faux_floored_advance(base, 0.0).to_bits(), base.to_bits());
    }
    // A contraction smaller than the advance simply narrows the step.
    assert!((faux_floored_advance(13.336, -4.8) - 8.536).abs() < 1e-4);
    // A contraction at least as large as the advance stops the pen instead
    // of walking it backwards — the zero-advance combining-mark case.
    assert_eq!(faux_floored_advance(0.0, -4.8), 0.0);
    assert_eq!(faux_floored_advance(2.0, -4.8), 0.0);
    // RTL: cosmic-text walks the pen leftwards, so the floor mirrors and a
    // THICKENING delta may not flip the step to the right either.
    assert!((faux_floored_advance(-26.695, -4.8) + 31.495).abs() < 1e-4);
    assert_eq!(faux_floored_advance(-2.0, 4.8), 0.0);
}

#[test]
fn faux_thinning_never_steps_the_pen_backwards_over_a_combining_mark() {
    // A base letter followed by a zero-advance combining mark: at the
    // minimum strength the mark's own faux delta (-2 * 0.05 * em = -4.81 px
    // at em 48) exceeds its zero advance. Before the floor the run measured
    // xs = [0.0, 17.070313, 12.257813] — the `b` placed 4.81 px LEFT of the
    // mark that precedes it — and reported line_width_px = 29.328125.
    let text = "a\u{0301}\u{0301}b";
    let params = thinned_run_params(text);
    let (xs, line_width_px) = fixture_run_layout(text, &params);
    assert_eq!(xs.len(), 3, "expected [base, mark, b], got {xs:?}");
    for pair in xs.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "pen stepped backwards: {xs:?} (line width {line_width_px})"
        );
    }
    // The mark contributes EXACTLY nothing: its floored effective advance is
    // a hard zero, so the following glyph sits on the mark's own pen.
    assert_eq!(
        xs[2].to_bits(),
        xs[1].to_bits(),
        "a fully contracted zero-advance mark must leave the pen where it was: {xs:?}"
    );
    // Alignment width follows the SAME floored rule, so it still reaches the
    // last glyph's right edge instead of the pre-floor 29.328125.
    assert!(
        line_width_px >= xs[2],
        "alignment width {line_width_px} is behind the pen {xs:?}"
    );
    let (_solo_xs, solo_width) = fixture_run_layout("b", &thinned_run_params("b"));
    assert!(
        (line_width_px - (xs[2] + solo_width)).abs() < 1e-3,
        "alignment width {line_width_px} must equal the last pen {} plus that \
         glyph's own floored advance {solo_width}",
        xs[2]
    );
}

#[test]
fn faux_thinning_keeps_narrow_punctuation_monotone() {
    // The marginal case: a period/comma advance (0.278 em = 13.34 px at
    // em 48) is wider than the -4.81 px contraction, so the pen must keep
    // moving FORWARD, just by less.
    let text = ".,.,.";
    let params = thinned_run_params(text);
    let (xs, line_width_px) = fixture_run_layout(text, &params);
    assert_eq!(xs.len(), 5, "expected five glyphs, got {xs:?}");
    for pair in xs.windows(2) {
        assert!(
            pair[1] > pair[0],
            "narrow-glyph pen must stay strictly increasing: {xs:?}"
        );
    }
    let mut plain = base_params();
    plain.text = text.to_string();
    plain.font_size_px = 48.0;
    let (plain_xs, _plain_width) = fixture_run_layout(text, &plain);
    assert!(
        xs[4] < plain_xs[4],
        "thinning must still pull the run in: thinned {xs:?} vs plain {plain_xs:?}"
    );
    assert!(
        line_width_px >= xs[4],
        "alignment width {line_width_px} is behind the pen {xs:?}"
    );
}

#[test]
fn faux_bounds_pads_are_never_negative() {
    // A THINNING offset pads by the same MAGNITUDE a thickening one does: an
    // inward offset strays outside the source ink at a reflex vertex (the
    // miter apex is pushed up to 4*|d| along the inward bisector, through
    // any thinner material), and a zero pad would crop that spike. What must
    // never happen is a NEGATIVE pad, which would shrink the canvas below the
    // plain ink.
    let em = 48.0f32;
    for sharp_corners in [true, false] {
        for outward_only in [true, false] {
            for has_counter in [true, false] {
                let thinning = FauxGlyphStyle {
                    bold: FauxOutlineParams::new(-0.05 * em, sharp_corners, outward_only),
                    bold_has_counter: has_counter,
                    shear_x: 0.0,
                };
                assert!(thinning.bold.is_some(), "the thinning params must be valid");
                let pads = faux_bounds_pads(thinning, 30.0, 40.0, 1.0, 1.0);
                let thin_quantized = thinning.bold.map_or(0.0, FauxOutlineParams::d_px).abs();
                let thin_expected = thin_quantized
                    * if outward_only && has_counter { 2.0 } else { 1.0 }
                    * if sharp_corners { 4.0 } else { 1.0 };
                assert!(
                    pads[0] >= 0.0 && pads[1] >= 0.0,
                    "a pad must never be negative (sharp={sharp_corners}, \
                     out={outward_only}, counter={has_counter})"
                );
                assert!(
                    (pads[1] - thin_expected).abs() < 1e-3,
                    "thinning pad {} should be the magnitude {thin_expected} \
                     (sharp={sharp_corners}, out={outward_only}, counter={has_counter})",
                    pads[1]
                );
                assert!(thinning.outer_delta_px() < 0.0, "thinning delta stays negative");

                // The thickening counterpart DOES pad, and the compensated
                // (counter-bearing, outward_only) case pads twice as much.
                let thickening = FauxGlyphStyle {
                    bold: FauxOutlineParams::new(0.05 * em, sharp_corners, outward_only),
                    bold_has_counter: has_counter,
                    shear_x: 0.0,
                };
                let pads = faux_bounds_pads(thickening, 30.0, 40.0, 1.0, 1.0);
                assert!(pads[0] > 0.0 && pads[1] > 0.0, "thickening must pad");
                let compensated = outward_only && has_counter;
                // Compare against the QUANTIZED distance (1/64 px grid),
                // not the requested one, so the check pins the formula
                // rather than the rounding.
                let quantized = thickening.bold.map_or(0.0, FauxOutlineParams::d_px);
                let expected = quantized
                    * if compensated { 2.0 } else { 1.0 }
                    * if sharp_corners { 4.0 } else { 1.0 };
                assert!(
                    (pads[1] - expected).abs() < 1e-3,
                    "pad {} should be {expected}",
                    pads[1]
                );
            }
        }
    }
}

/// Alpha-weighted x centroid over the row band `[y0, y1)`.
fn alpha_centroid_x_in_rows(
    width: u32,
    height: u32,
    rgba: &[u8],
    y0: usize,
    y1: usize,
) -> Option<f32> {
    let width = width as usize;
    let mut weighted_x = 0.0f32;
    let mut alpha_sum = 0.0f32;
    for y in y0..y1.min(height as usize) {
        for x in 0..width {
            let alpha = f32::from(rgba[(y * width + x) * 4 + 3]);
            weighted_x += x as f32 * alpha;
            alpha_sum += alpha;
        }
    }
    (alpha_sum > 0.0).then_some(weighted_x / alpha_sum)
}

#[test]
fn faux_italic_leans_tops_in_slant_direction() {
    // Compare the x centroid of the ink's top half against its bottom
    // half: a positive slant leans tops right, a negative one left.
    let mut base = base_params();
    base.text = "H".to_string();
    base.font_size_px = 64.0;
    base.force_italic = true;

    let lean_of = |slant: f32| -> f32 {
        let mut params = base.clone();
        params.faux_italic_slant_deg = Some(slant);
        let render = render_text_to_image(&params, None).expect("italic render");
        let h = render.height as usize;
        let top = alpha_centroid_x_in_rows(render.width, render.height, &render.rgba, 0, h / 2)
            .expect("top ink");
        let bottom =
            alpha_centroid_x_in_rows(render.width, render.height, &render.rgba, h / 2, h)
                .expect("bottom ink");
        top - bottom
    };

    let right = lean_of(30.0);
    let left = lean_of(-30.0);
    assert!(right > 3.0, "positive slant must lean tops right, got {right}");
    assert!(left < -3.0, "negative slant must lean tops left, got {left}");
}

#[test]
fn optical_kerning_measures_faux_offset_ink() {
    // Under Optical the pen positions come from ink-gap normalization of
    // the OFFSET outlines (no `2*d` pen growth); under Auto the faux pen
    // growth applies. So the optical faux render must be measurably
    // narrower than the Auto faux render of the same text, proving the
    // optical path measures the emboldened contours instead of inheriting
    // the metric growth.
    let mut auto_faux = base_params();
    auto_faux.text = "HHH".to_string();
    auto_faux.font_size_px = 48.0;
    auto_faux.force_bold = true;
    auto_faux.faux_bold = Some(test_faux_bold(6.0, 0.0));
    auto_faux.kerning_mode = KerningMode::Auto;
    let auto_render = render_text_to_image(&auto_faux, None).expect("auto faux render");
    let (auto_w, _) =
        alpha_bounds_from_rgba(auto_render.width, auto_render.height, &auto_render.rgba)
            .expect("auto ink");

    let mut optical_faux = auto_faux.clone();
    optical_faux.kerning_mode = KerningMode::Optical;
    let optical_render =
        render_text_to_image(&optical_faux, None).expect("optical faux render");
    let (optical_w, _) = alpha_bounds_from_rgba(
        optical_render.width,
        optical_render.height,
        &optical_render.rgba,
    )
    .expect("optical ink");

    let d = 6.0 / 100.0 * 48.0;
    assert!(
        (auto_w as f32) - (optical_w as f32) >= d,
        "optical faux ({optical_w}) must be narrower than auto faux ({auto_w}) \
         because optical normalizes the measured (offset) ink gaps"
    );
}

#[test]
fn faux_bold_grows_vertical_stacking() {
    let mut plain = base_params();
    plain.text = "HH".to_string();
    plain.font_size_px = 48.0;
    plain.text_line_mode = TextLineMode::Vertical;
    let plain_render = render_text_to_image(&plain, None).expect("plain vertical render");
    let (_, plain_h) =
        alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
            .expect("plain ink");

    let mut faux = plain.clone();
    faux.force_bold = true;
    faux.faux_bold = Some(test_faux_bold(6.0, 0.0));
    let faux_render = render_text_to_image(&faux, None).expect("faux vertical render");
    let (_, faux_h) =
        alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
            .expect("faux ink");

    // Two glyphs, each thickened by d top+bottom, and the ink-height
    // stacking pads the step accordingly: expect ~4*d total growth.
    let d = 6.0 / 100.0 * 48.0;
    let growth = faux_h as f32 - plain_h as f32;
    assert!(
        (growth - 4.0 * d).abs() <= 3.0,
        "vertical ink height growth {growth} should be ~{}",
        4.0 * d
    );
}

#[test]
fn inline_faux_bold_tag_differs_from_real_bold_tag() {
    // `<b=...>` must produce different pixels than the legacy `<b>` (which
    // resolves the family's Bold face — here the same Regular fixture, so
    // it renders like plain) on the same text.
    let mut real = base_params();
    real.text = "a<b>bb</b>a".to_string();
    real.enable_inline_style_tags = true;
    let real_render = render_text_to_image(&real, None).expect("real-bold render");

    let mut faux = real.clone();
    faux.text = "a<b=8>bb</b>a".to_string();
    let faux_render = render_text_to_image(&faux, None).expect("faux-bold render");
    assert_ne!(
        (real_render.width, real_render.height, &real_render.rgba),
        (faux_render.width, faux_render.height, &faux_render.rgba),
        "a parameterized faux bold span must change the rendered pixels"
    );
}

/// Maximum inked x over the row band `[y0, y1)`, or `None` if the band is
/// fully transparent.
fn max_ink_x_in_rows(
    width: u32,
    height: u32,
    rgba: &[u8],
    y0: usize,
    y1: usize,
) -> Option<usize> {
    let width = width as usize;
    let mut max_x = None;
    for y in y0..y1.min(height as usize) {
        for x in 0..width {
            if rgba[(y * width + x) * 4 + 3] > 0 {
                max_x = Some(max_x.map_or(x, |current: usize| current.max(x)));
            }
        }
    }
    max_x
}

#[test]
fn faux_advance_carries_across_inline_size_boundary() {
    // A faux span ending immediately before an inline size change: the
    // following (differently-sized, non-faux) glyph must be pushed by the
    // full 2*d instead of staying at its shaped position over the
    // thickened ink. Expected total ink width growth vs the same text
    // without faux: d (left edge of the thickened glyph) + 2*d (pen shift
    // of everything after the span) = 3*d. A dropped boundary extra would
    // leave only ~d of growth (and a d-deep overlap).
    let mut plain = base_params();
    plain.text = "H<size=24>H".to_string();
    plain.font_size_px = 48.0;
    plain.enable_inline_style_tags = true;
    let plain_render = render_text_to_image(&plain, None).expect("plain render");
    let (plain_w, _) =
        alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
            .expect("plain ink");

    let mut faux = plain.clone();
    faux.text = "<b=10>H</b><size=24>H".to_string();
    let faux_render = render_text_to_image(&faux, None).expect("faux render");
    let (faux_w, _) =
        alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
            .expect("faux ink");

    let d = 10.0 / 100.0 * 48.0;
    let growth = faux_w as f32 - plain_w as f32;
    assert!(
        (growth - 3.0 * d).abs() <= 2.0,
        "ink width growth {growth} should be ~{} (no overlap at the size boundary)",
        3.0 * d
    );
}

#[test]
fn faux_trailing_extra_included_in_alignment_width() {
    // Right-aligned two-line text: the line ending in a faux-bold glyph
    // must not overshoot the flush margin relative to the plain line
    // below it — the trailing glyph's faux advance growth is part of the
    // logical line width.
    let mut params = base_params();
    params.text = "<b=12>HH</b>\nHH".to_string();
    params.enable_inline_style_tags = true;
    params.font_size_px = 48.0;
    params.align = HorizontalAlign::RIGHT;
    let render = render_text_to_image(&params, None).expect("aligned render");
    let h = render.height as usize;
    let top_right = max_ink_x_in_rows(render.width, render.height, &render.rgba, 0, h / 2)
        .expect("top line ink");
    let bottom_right = max_ink_x_in_rows(render.width, render.height, &render.rgba, h / 2, h)
        .expect("bottom line ink");
    assert!(
        top_right <= bottom_right + 1,
        "faux line right edge ({top_right}) must not overshoot the plain flush margin \
         ({bottom_right})"
    );
}

#[test]
fn vertical_columns_do_not_overlap_under_faux_italic_slant() {
    // Two single-glyph columns at a +/-45 degree faux slant: the column
    // width must include the shear overhang, so a fully transparent pixel
    // column still separates the two glyphs' ink.
    for slant in [45.0f32, -45.0] {
        let mut params = base_params();
        params.text = "H\nH".to_string();
        params.font_size_px = 48.0;
        params.text_line_mode = TextLineMode::Vertical;
        // No extra column spacing: adjacent columns are separated only by
        // the measured visual width, so a missing shear overhang overlaps.
        params.line_spacing_percent = 0.0;
        params.force_italic = true;
        params.faux_italic_slant_deg = Some(slant);
        let render = render_text_to_image(&params, None).expect("vertical italic render");
        let (min_x, _, max_x, _) =
            alpha_box(render.width, render.height, &render.rgba).expect("ink");
        let width = render.width as usize;
        let height = render.height as usize;
        let has_gap = ((min_x + 1)..max_x).any(|x| {
            (0..height).all(|y| render.rgba[(y * width + x) * 4 + 3] == 0)
        });
        assert!(
            has_gap,
            "columns must stay separated at slant {slant} (no shear overlap)"
        );
    }
}

#[test]
fn rotated_faux_bold_italic_stays_within_canvas() {
    // Global rotation + faux bold + faux italic combined must route
    // through the rotated placement path without clipping: the trim pass
    // pads 1px, so ink touching the canvas edge would indicate clipped
    // bounds.
    let mut plain = base_params();
    plain.text = "HH".to_string();
    plain.font_size_px = 48.0;
    plain.global_rotation_deg = 30.0;
    let plain_render = render_text_to_image(&plain, None).expect("plain rotated render");

    let mut faux = plain.clone();
    faux.force_bold = true;
    faux.faux_bold = Some(test_faux_bold(8.0, 0.0));
    faux.force_italic = true;
    faux.faux_italic_slant_deg = Some(25.0);
    let render = render_text_to_image(&faux, None).expect("rotated faux render");
    let (min_x, min_y, max_x, max_y) =
        alpha_box(render.width, render.height, &render.rgba).expect("ink");
    assert!(min_x >= 1 && min_y >= 1, "ink must not touch the left/top edge");
    assert!(
        (max_x as u32) < render.width - 1 && (max_y as u32) < render.height - 1,
        "ink must not touch the right/bottom edge (would indicate clipping)"
    );
    assert_ne!(
        (plain_render.width, plain_render.height, &plain_render.rgba),
        (render.width, render.height, &render.rgba),
        "rotated faux render must differ from the plain rotated render"
    );
}

#[test]
fn formula_mode_applies_faux_bold_across_effective_sizes() {
    // Formula/on-path mode with two effective sizes of the same glyph:
    // each glyph resolves its own quantized d, so the ink cache holds a
    // separate (CacheKey, faux_bits) entry per size and both glyphs
    // thicken (the faux render grows in both axes).
    let mut plain = base_params();
    plain.text = "H<size=24>H".to_string();
    plain.enable_inline_style_tags = true;
    plain.font_size_px = 48.0;
    plain.text_layout_mode = TextLayoutMode::Formula;
    let plain_render = render_text_to_image(&plain, None).expect("plain formula render");
    let (plain_w, plain_h) =
        alpha_bounds_from_rgba(plain_render.width, plain_render.height, &plain_render.rgba)
            .expect("plain ink");

    let mut faux = plain.clone();
    faux.force_bold = true;
    faux.faux_bold = Some(test_faux_bold(8.0, 0.0));
    let faux_render = render_text_to_image(&faux, None).expect("faux formula render");
    let (faux_w, faux_h) =
        alpha_bounds_from_rgba(faux_render.width, faux_render.height, &faux_render.rgba)
            .expect("faux ink");
    assert!(
        faux_w > plain_w && faux_h > plain_h,
        "faux bold must thicken the on-path ink ({plain_w}x{plain_h} -> {faux_w}x{faux_h})"
    );
}

#[test]
fn base_pipeline_renders_non_empty_plain_text() {
    let rendered = render_text_to_image(&base_params(), None).unwrap_or_else(|error| {
        panic!("base render_next pipeline should render text: {error}")
    });
    assert!(rendered.width > 0);
    assert!(rendered.height > 0);
    assert!(rendered.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0));
}

/// Build a `cols x rows` identity mesh (every node at its identity position).
fn identity_mesh(cols: usize, rows: usize) -> VectorMeshWarp {
    let mut points_norm = Vec::with_capacity(cols * rows);
    for i in 0..rows {
        for j in 0..cols {
            points_norm.push([j as f32 / (cols - 1) as f32, i as f32 / (rows - 1) as f32]);
        }
    }
    VectorMeshWarp {
        cols,
        rows,
        // 0 => exercise the LIVE pre-warp-bounds normalization (Phase 1 path); the explicit
        // src-dims (Design B) path is unit-tested in `vector.rs`.
        src_width_px: 0.0,
        src_height_px: 0.0,
        points_norm,
    }
}

/// Full inked-alpha bounding box `(min_x, min_y, max_x, max_y)` in pixels.
fn alpha_box(width: u32, height: u32, rgba: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let (width, height) = (width as usize, height as usize);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (width, height, 0usize, 0usize);
    let mut found = false;
    for y in 0..height {
        for x in 0..width {
            if rgba[(y * width + x) * 4 + 3] == 0 {
                continue;
            }
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            found = true;
        }
    }
    found.then_some((min_x, min_y, max_x, max_y))
}

/// Count of inked (alpha > 0) pixels.
fn inked_pixels(rgba: &[u8]) -> usize {
    rgba.as_chunks::<4>().0.iter().filter(|px| px[3] > 0).count()
}

#[test]
fn mesh_warp_none_and_identity_are_byte_identical() {
    // Contract: `None` and an identity mesh both take the byte-identical fast
    // path, so the output RGBA equals a plain render exactly.
    let base = base_params();
    let plain = render_text_to_image(&base, None).expect("plain render");

    let mut identity = base.clone();
    identity.raster_transform = Some(identity_mesh(13, 13));
    let warped = render_text_to_image(&identity, None).expect("identity-warp render");

    assert_eq!(
        (plain.width, plain.height),
        (warped.width, warped.height),
        "identity warp must not change canvas size"
    );
    assert_eq!(
        plain.rgba, warped.rgba,
        "identity warp must be byte-identical to no warp"
    );
}

#[test]
fn mesh_warp_single_corner_grows_canvas_and_shifts_coverage() {
    // Push the bottom-right lattice node outward (beyond the box). The warped
    // content must extend farther right/down than the un-warped render, and the
    // canvas must grow to hold it (no clipping).
    let base = base_params();
    let plain = render_text_to_image(&base, None).expect("plain render");
    let (_, _, plain_max_x, plain_max_y) =
        alpha_box(plain.width, plain.height, &plain.rgba).expect("plain has ink");

    let mut mesh = identity_mesh(13, 13);
    let last = mesh.points_norm.len() - 1; // bottom-right node (i=12, j=12).
    mesh.points_norm[last] = [1.35, 1.35];
    let mut params = base.clone();
    params.raster_transform = Some(mesh);
    let warped = render_text_to_image(&params, None).expect("corner-warp render");
    let (_, _, warped_max_x, warped_max_y) =
        alpha_box(warped.width, warped.height, &warped.rgba).expect("warped has ink");

    // Canvas grew in at least one dimension to accommodate the outward warp.
    assert!(
        warped.width > plain.width || warped.height > plain.height,
        "canvas must grow: plain {}x{}, warped {}x{}",
        plain.width,
        plain.height,
        warped.width,
        warped.height
    );
    // Coverage shifted outward (down-right): warped ink reaches farther.
    assert!(
        warped_max_x >= plain_max_x && warped_max_y > plain_max_y,
        "warped ink must reach farther down-right: plain ({plain_max_x},{plain_max_y}) \
         warped ({warped_max_x},{warped_max_y})"
    );
    // No clipping: the trimmed content keeps a transparent border on all sides
    // (trim pads by 1px), so the farthest ink is strictly inside the canvas.
    assert!(
        (warped_max_x as u32) < warped.width - 1 && (warped_max_y as u32) < warped.height - 1,
        "warped ink must not touch the canvas edge (would indicate clipping)"
    );
}

#[test]
fn mesh_warp_pure_translation_shifts_without_shape_change() {
    // A translation-equivalent mesh (all nodes shifted by a constant) moves the
    // content without deforming it: the trimmed content size and inked pixel
    // count match the un-warped render, but the pre-trim placement shifts.
    let base = base_params();
    let plain = render_text_to_image(&base, None).expect("plain render");
    let (px0, py0, px1, py1) =
        alpha_box(plain.width, plain.height, &plain.rgba).expect("plain ink");
    let plain_w = px1 - px0;
    let plain_h = py1 - py0;

    // Shift every node right+down by a constant normalized offset.
    let mut mesh = identity_mesh(13, 13);
    for node in &mut mesh.points_norm {
        node[0] += 0.25;
        node[1] += 0.15;
    }
    let mut params = base.clone();
    params.raster_transform = Some(mesh);
    let warped = render_text_to_image(&params, None).expect("translation-warp render");
    let (wx0, wy0, wx1, wy1) =
        alpha_box(warped.width, warped.height, &warped.rgba).expect("warped ink");
    let warped_w = wx1 - wx0;
    let warped_h = wy1 - wy0;

    // Shape (trimmed extent) is preserved within a small AA/rounding tolerance.
    assert!(
        (warped_w as i32 - plain_w as i32).abs() <= 2,
        "translated content width must be unchanged: plain {plain_w}, warped {warped_w}"
    );
    assert!(
        (warped_h as i32 - plain_h as i32).abs() <= 2,
        "translated content height must be unchanged: plain {plain_h}, warped {warped_h}"
    );
    // Ink mass is preserved (a shift that clipped would drop inked pixels).
    let (plain_ink, warped_ink) = (inked_pixels(&plain.rgba), inked_pixels(&warped.rgba));
    let tolerance = plain_ink / 20 + 4; // 5% + slack for AA at the new subpixel phase.
    assert!(
        warped_ink.abs_diff(plain_ink) <= tolerance,
        "translation must preserve ink mass (no clipping): plain {plain_ink}, warped {warped_ink}"
    );
}

/// Push the bottom-right lattice node of a fresh 13x13 identity mesh outward by
/// `amount` (both axes) so the warp bulges the content down-right beyond the box.
fn corner_pushed_mesh(amount: f32) -> VectorMeshWarp {
    let mut mesh = identity_mesh(13, 13);
    let last = mesh.points_norm.len() - 1;
    mesh.points_norm[last] = [1.0 + amount, 1.0 + amount];
    mesh
}

#[test]
fn mesh_warp_applies_on_vertical_path() {
    // Regression: the warp used to be ignored on the vertical path. Identity must
    // stay byte-identical; a non-identity corner push must change the pixels and
    // grow the trimmed canvas outward (down-right) without clipping.
    let mut base = base_params();
    base.text = "ТЕКСТ".to_string();
    base.text_line_mode = TextLineMode::Vertical;
    base.width_px = 120;
    let plain = render_text_to_image(&base, None).expect("vertical plain render");
    let (_, _, plain_max_x, plain_max_y) =
        alpha_box(plain.width, plain.height, &plain.rgba).expect("vertical plain ink");

    let mut identity = base.clone();
    identity.raster_transform = Some(identity_mesh(13, 13));
    let ident = render_text_to_image(&identity, None).expect("vertical identity render");
    assert_eq!(
        (plain.width, plain.height, &plain.rgba),
        (ident.width, ident.height, &ident.rgba),
        "vertical identity warp must be byte-identical to no warp"
    );

    let mut warped_params = base.clone();
    warped_params.raster_transform = Some(corner_pushed_mesh(0.35));
    let warped = render_text_to_image(&warped_params, None).expect("vertical warp render");
    assert_ne!(
        (plain.width, plain.height, &plain.rgba),
        (warped.width, warped.height, &warped.rgba),
        "vertical non-identity warp must change the render"
    );
    let (_, _, warped_max_x, warped_max_y) =
        alpha_box(warped.width, warped.height, &warped.rgba).expect("vertical warp ink");
    assert!(
        warped.width > plain.width || warped.height > plain.height,
        "vertical warp must grow the canvas: plain {}x{} warped {}x{}",
        plain.width,
        plain.height,
        warped.width,
        warped.height
    );
    assert!(
        warped_max_x >= plain_max_x && warped_max_y >= plain_max_y,
        "vertical warp ink must reach at least as far down-right: plain \
         ({plain_max_x},{plain_max_y}) warped ({warped_max_x},{warped_max_y})"
    );
    assert!(
        (warped_max_x as u32) < warped.width - 1 && (warped_max_y as u32) < warped.height - 1,
        "vertical warp ink must not touch the canvas edge (clipping)"
    );
}

#[test]
fn mesh_warp_applies_on_formula_path() {
    // Regression: the warp used to be ignored on the formula/on-path path.
    let mut base = base_params();
    base.text = "FORMULA".to_string();
    base.width_px = 320;
    base.text_layout_mode = TextLayoutMode::Formula;
    base.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * w".to_string(),
        y_expr: "0".to_string(),
        rotation_expr: "0".to_string(),
        use_tangent_rotation: false,
        ..TextFormulaLayoutParams::default()
    };
    let plain = render_text_to_image(&base, None).expect("formula plain render");

    let mut identity = base.clone();
    identity.raster_transform = Some(identity_mesh(13, 13));
    let ident = render_text_to_image(&identity, None).expect("formula identity render");
    assert_eq!(
        (plain.width, plain.height, &plain.rgba),
        (ident.width, ident.height, &ident.rgba),
        "formula identity warp must be byte-identical to no warp"
    );

    let mut warped_params = base.clone();
    warped_params.raster_transform = Some(corner_pushed_mesh(0.5));
    let warped = render_text_to_image(&warped_params, None).expect("formula warp render");
    assert_ne!(
        (plain.width, plain.height, &plain.rgba),
        (warped.width, warped.height, &warped.rgba),
        "formula non-identity warp must change the render"
    );
    assert!(
        warped.width > plain.width || warped.height > plain.height,
        "formula warp must grow the canvas: plain {}x{} warped {}x{}",
        plain.width,
        plain.height,
        warped.width,
        warped.height
    );
    let (_, _, wmax_x, wmax_y) =
        alpha_box(warped.width, warped.height, &warped.rgba).expect("formula warp ink");
    assert!(
        (wmax_x as u32) < warped.width - 1 && (wmax_y as u32) < warped.height - 1,
        "formula warp ink must not touch the canvas edge (clipping)"
    );
}

#[test]
fn mesh_warp_applies_on_shape_path() {
    // `Shape` reuses the formula render path; the warp must apply there too.
    let mut base = base_params();
    base.text = "SHAPE PATH".to_string();
    base.width_px = 240;
    base.text_layout_mode = TextLayoutMode::Shape;
    base.text_shape = TextShape::Free;
    let plain = render_text_to_image(&base, None).expect("shape plain render");

    let mut identity = base.clone();
    identity.raster_transform = Some(identity_mesh(13, 13));
    let ident = render_text_to_image(&identity, None).expect("shape identity render");
    assert_eq!(
        (plain.width, plain.height, &plain.rgba),
        (ident.width, ident.height, &ident.rgba),
        "shape identity warp must be byte-identical to no warp"
    );

    let mut warped_params = base.clone();
    warped_params.raster_transform = Some(corner_pushed_mesh(0.5));
    let warped = render_text_to_image(&warped_params, None).expect("shape warp render");
    assert_ne!(
        (plain.width, plain.height, &plain.rgba),
        (warped.width, warped.height, &warped.rgba),
        "shape non-identity warp must change the render"
    );
}

#[test]
fn mesh_warp_applies_on_vector_lines_path() {
    // Regression: the warp used to be ignored on the custom-vector-lines path,
    // which also uses a FIXED output canvas. Identity keeps that fixed canvas and
    // stays byte-identical; a non-identity warp drops the fixed canvas (like a
    // global rotation) and grows to the warped content bounds without clipping.
    let mut base = base_params();
    base.text = "VECTOR".to_string();
    base.width_px = 260;
    base.text_layout_mode = TextLayoutMode::CustomVectorLines;
    base.vector_lines_layout = TextVectorLinesLayoutParams {
        width_px: 260,
        height_px: 100,
        lines: vec![TextVectorLine {
            points: vec![
                TextVectorPoint { x: 8.0, y: 50.0 },
                TextVectorPoint { x: 130.0, y: 50.0 },
                TextVectorPoint { x: 252.0, y: 50.0 },
            ],
            corner_smoothing_px: 16.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::ByLineLength,
            flip_text: false,
        }],
        ..TextVectorLinesLayoutParams::default()
    };
    let plain = render_text_to_image(&base, None).expect("vector-lines plain render");

    let mut identity = base.clone();
    identity.raster_transform = Some(identity_mesh(13, 13));
    let ident = render_text_to_image(&identity, None).expect("vector-lines identity render");
    assert_eq!(
        (plain.width, plain.height, &plain.rgba),
        (ident.width, ident.height, &ident.rgba),
        "vector-lines identity warp must keep the fixed canvas and stay byte-identical"
    );

    let mut warped_params = base.clone();
    warped_params.raster_transform = Some(corner_pushed_mesh(0.5));
    let warped = render_text_to_image(&warped_params, None).expect("vector-lines warp render");
    assert_ne!(
        (plain.width, plain.height, &plain.rgba),
        (warped.width, warped.height, &warped.rgba),
        "vector-lines non-identity warp must change the render"
    );
    assert!(
        warped.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0),
        "vector-lines warp must still ink pixels"
    );
    let (_, _, wmax_x, wmax_y) =
        alpha_box(warped.width, warped.height, &warped.rgba).expect("vector-lines warp ink");
    assert!(
        (wmax_x as u32) < warped.width - 1 && (wmax_y as u32) < warped.height - 1,
        "vector-lines warp ink must not touch the canvas edge (clipping)"
    );
}

#[test]
fn vertical_step_follows_glyph_ink_height() {
    let mut params = base_params();
    params.text_line_mode = TextLineMode::Vertical;

    // Низкие глифы (точки у базовой линии) укладываются плотно по своей ink-высоте.
    params.text = "...".to_string();
    let dots = render_text_to_image(&params, None).expect("vertical dots render");
    let (_, dots_h) =
        alpha_bounds_from_rgba(dots.width, dots.height, &dots.rgba).expect("dots bounds");

    // Высокие глифы занимают по вертикали заметно больше.
    params.text = "III".to_string();
    let bars = render_text_to_image(&params, None).expect("vertical bars render");
    let (_, bars_h) =
        alpha_bounds_from_rgba(bars.width, bars.height, &bars.rgba).expect("bars bounds");

    // При старом «em на символ» обе высоты были бы почти равны; при шаге по
    // ink-высоте высокие глифы тянутся значительно дальше низких.
    assert!(
        bars_h > dots_h * 2,
        "vertical step should track ink height: III={bars_h} vs ...={dots_h}"
    );
}

#[test]
fn inline_group_rotation_rotates_text_block() {
    let mut params = base_params();
    params.enable_inline_style_tags = true;

    params.text = "Hello world".to_string();
    let plain = render_text_to_image(&params, None).expect("plain render");
    let (plain_w, plain_h) =
        alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba).expect("plain bounds");

    // Поворот всей строки на 90° через машиночитаемый тег делает блок высоким и узким.
    params.text = "<m g=90>Hello world</m>".to_string();
    let rotated = render_text_to_image(&params, None).expect("rotated render");
    assert!(
        rotated.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0),
        "rotated block must render visible pixels"
    );
    let (rotated_w, rotated_h) = alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
        .expect("rotated bounds");
    assert!(
        rotated_h > plain_h,
        "90°-rotated block should be taller: {rotated_h} vs {plain_h}"
    );
    assert!(
        rotated_w < plain_w,
        "90°-rotated block should be narrower: {rotated_w} vs {plain_w}"
    );
}

#[test]
fn global_rotation_rotates_whole_block_while_vector() {
    let mut params = base_params();
    params.text = "Hello world".to_string();

    // Baseline: no global rotation -> wide, short horizontal block.
    params.global_rotation_deg = 0.0;
    let plain = render_text_to_image(&params, None).expect("plain render");
    let (plain_w, plain_h) =
        alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba).expect("plain bounds");
    assert!(
        plain_w > plain_h,
        "horizontal 'Hello world' should be wide and short: {plain_w}x{plain_h}"
    );

    // A 0.0 value must be a true no-op: the routing gate uses abs > EPSILON,
    // so it stays on the normal path and is byte-identical across renders.
    let plain_again = render_text_to_image(&params, None).expect("plain render again");
    assert_eq!(
        plain.rgba, plain_again.rgba,
        "global_rotation_deg = 0.0 must be deterministic and unchanged"
    );

    // 90° rotates the whole laid-out block (vector) -> tall and narrow.
    params.global_rotation_deg = 90.0;
    let rotated = render_text_to_image(&params, None).expect("rotated render");
    assert!(
        rotated.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0),
        "rotated block must render visible pixels"
    );
    let (rotated_w, rotated_h) =
        alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
            .expect("rotated bounds");
    assert!(
        rotated_h > plain_h,
        "90°-rotated block should be taller: {rotated_h} vs {plain_h}"
    );
    assert!(
        rotated_w < plain_w,
        "90°-rotated block should be narrower: {rotated_w} vs {plain_w}"
    );
}

#[test]
fn inline_glyph_rotation_flips_tall_glyph_bounds() {
    let mut params = base_params();
    params.enable_inline_style_tags = true;

    // Заглавная «I» — высокая и узкая.
    params.text = "I".to_string();
    let plain = render_text_to_image(&params, None).expect("plain render");
    let (plain_w, plain_h) =
        alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba).expect("plain bounds");
    assert!(plain_h > plain_w, "capital I should be tall and narrow");

    // Повёрнутая на 90° «I» становится низкой и широкой.
    params.text = "<m r=90>I</m>".to_string();
    let rotated = render_text_to_image(&params, None).expect("glyph-rotated render");
    let (rotated_w, rotated_h) = alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
        .expect("rotated bounds");
    assert!(
        rotated_w > rotated_h,
        "90°-rotated I should be wide and short: {rotated_w}x{rotated_h}"
    );
}

#[test]
fn shape_compare_warns_when_layout_text_is_unchanged() {
    let mut params = base_params();
    params.compare_shape_with = Some(TextRenderShapeCompareParams {
        width_px: params.width_px,
        text_wrap_mode: params.text_wrap_mode,
        shape_min_width_percent: params.shape_min_width_percent,
        shape_variant: params.shape_variant,
        cancel_render_if_layout_text_unchanged: false,
    });

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render unchanged shape compare case: {error}")
    });

    assert!(rendered.width > 0);
    assert!(
        rendered
            .warnings
            .iter()
            .any(|warning| warning == super::UNCHANGED_LAYOUT_TEXT_WARNING),
        "{:?}",
        rendered.warnings
    );
}

#[test]
fn shape_compare_can_skip_render_when_layout_text_is_unchanged() {
    let mut params = base_params();
    params.compare_shape_with = Some(TextRenderShapeCompareParams {
        width_px: params.width_px,
        text_wrap_mode: params.text_wrap_mode,
        shape_min_width_percent: params.shape_min_width_percent,
        shape_variant: params.shape_variant,
        cancel_render_if_layout_text_unchanged: true,
    });

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should skip unchanged shape compare case cleanly: {error}")
    });

    assert_eq!(rendered.width, 0);
    assert_eq!(rendered.height, 0);
    assert!(rendered.rgba.is_empty());
    assert!(
        rendered
            .warnings
            .iter()
            .any(|warning| warning == super::UNCHANGED_LAYOUT_TEXT_WARNING),
        "{:?}",
        rendered.warnings
    );
}

#[test]
fn base_pipeline_cancel_token_stops_render() {
    let token = Arc::new(AtomicU64::new(7));
    token.store(8, Ordering::Release);
    let error = render_text_to_image(&base_params(), Some((&token, 7)))
        .err()
        .unwrap_or_else(|| "missing cancel error".to_string());
    assert!(error.contains("cancelled"));
}

#[test]
fn base_pipeline_renders_inline_font_size_text() {
    let params = base_params();
    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render baseline test case: {error}")
    });
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

#[test]
fn base_pipeline_renders_vertical_text() {
    let mut params = base_params();
    params.text = "вертикальный текст".to_string();
    params.text_line_mode = TextLineMode::Vertical;
    params.vertical_line_direction = VerticalLineDirection::RightToLeft;
    params.text_wrap_mode = TextWrapMode::WholeWords;
    params.width_px = 140;

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render vertical baseline test case: {error}")
    });
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

#[test]
fn base_pipeline_renders_hanging_punctuation() {
    let mut params = base_params();
    params.text = "«Hello!»".to_string();
    params.align = HorizontalAlign::CENTER;
    params.hanging_punctuation = 1.0;

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render hanging punctuation case: {error}")
    });
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

#[test]
fn the_hanging_weight_moves_the_line_origin_between_the_two_ends() {
    // Synthetic run layout: 8px of leading hang, 2px of trailing hang inside a
    // 30px line. The weight scales BOTH the alignment width and the origin shift,
    // and must reproduce the two historical behaviours exactly at the ends.
    let layout = super::HorizontalRunLayout {
        glyph_xs: vec![0.0, 10.0, 20.0],
        line_width_px: 30.0,
        leading_hang_px: 8.0,
        trailing_hang_px: 2.0,
    };

    // Weight 0 == the old "off": the logical width, bit for bit, and no shift.
    assert_eq!(layout.align_width_px(0.0), 30.0);
    assert_eq!(layout.origin_shift_px(0.0), 0.0);
    // Weight 1 == the old "on": the fully hung visual width and the whole lead.
    assert_eq!(layout.align_width_px(1.0), 20.0);
    assert_eq!(layout.origin_shift_px(1.0), 8.0);
    // A mid weight lands strictly between, on both numbers.
    let mid_width = layout.align_width_px(0.5);
    let mid_shift = layout.origin_shift_px(0.5);
    assert!(
        (20.0..30.0).contains(&mid_width) && mid_width > 20.0,
        "mid alignment width {mid_width} must be strictly between 20 and 30"
    );
    assert!(
        (0.0..8.0).contains(&mid_shift) && mid_shift > 0.0,
        "mid origin shift {mid_shift} must be strictly between 0 and 8"
    );

    // ...and so does the resulting centered line origin, which is what the eye sees.
    let origin = |weight: f32| {
        super::horizontal_line_offset(
            100,
            layout.align_width_px(weight),
            HorizontalAlign::CENTER,
        ) as f32
            - layout.origin_shift_px(weight)
    };
    let (off, mid, full) = (origin(0.0), origin(0.5), origin(1.0));
    assert!(
        full < mid && mid < off,
        "the mid-weight origin must sit strictly between the two ends: \
         off={off} mid={mid} full={full}"
    );
}

#[test]
fn a_degenerate_all_hanging_line_never_hangs_at_any_weight() {
    // A line made only of hanging punctuation has no visual width to align by, so
    // the guard reports "nothing hangs" — for EVERY weight, or the layout would
    // jump as the slider moves.
    let layout = super::HorizontalRunLayout {
        glyph_xs: vec![0.0, 10.0],
        line_width_px: 20.0,
        leading_hang_px: 0.0,
        trailing_hang_px: 0.0,
    };
    for weight in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
        assert_eq!(layout.align_width_px(weight), 20.0, "weight {weight}");
        assert_eq!(layout.origin_shift_px(weight), 0.0, "weight {weight}");
    }
}

#[test]
fn the_hanging_weight_reaches_the_pixels_and_broken_values_are_clamped() {
    // The weight comes from a UI slider and a project file, so a broken value must
    // degrade to a legal one instead of poisoning the layout with NaN.
    // TWO lines, only the first with a leading hanging quote: the hang slides that
    // line left RELATIVE to the other one, which a one-line fixture could not show
    // (a rigid translation of the only line is invisible once the canvas is
    // cropped to the ink).
    let mut params = base_params();
    params.text = "«Hi\nAbc".to_string();
    params.align = HorizontalAlign::LEFT;

    let render = |weight: f32| {
        let mut params = params.clone();
        params.hanging_punctuation = weight;
        let image = render_text_to_image(&params, None).expect("render should succeed");
        (image.width, image.height, image.rgba)
    };

    let off = render(0.0);
    let full = render(1.0);
    assert_ne!(
        off, full,
        "the fixture must actually react to hanging punctuation, \
         otherwise the assertions below are vacuous"
    );
    // NaN is "off", never "half" and never a panic.
    assert_eq!(render(f32::NAN), off);
    assert_eq!(render(-2.0), off);
    assert_eq!(render(f32::NEG_INFINITY), off);
    // Above the range the hang saturates instead of overshooting.
    assert_eq!(render(4.0), full);
    assert_eq!(render(f32::INFINITY), full);
    // A legal mid weight is a real third layout, not a rounded end: the slider is
    // continuous all the way down to the pixels.
    let mid = render(0.5);
    assert_ne!(mid, off, "a half hang must not collapse onto the off layout");
    assert_ne!(mid, full, "a half hang must not collapse onto the full layout");
}

#[test]
fn horizontal_center_alignment_keeps_overlong_line_centered() {
    assert_eq!(
        super::horizontal_line_offset(100, 140.0, HorizontalAlign::CENTER),
        -20
    );
}

#[test]
fn base_pipeline_renders_soft_hyphen_wrap() {
    let mut params = base_params();
    params.text = "super\u{00AD}califragilistic".to_string();
    params.width_px = 110;
    params.text_wrap_mode = TextWrapMode::Moderate;

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render soft-hyphen wrap case: {error}")
    });
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

#[test]
fn base_pipeline_keeps_inline_style_across_soft_hyphen_wrap() {
    let mut params = base_params();
    params.text = "<b>super\u{00AD}califragilistic</b>".to_string();
    params.enable_inline_style_tags = true;
    params.width_px = 110;
    params.text_wrap_mode = TextWrapMode::Moderate;

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render inline soft-hyphen wrap case: {error}")
    });

    assert!(
        !rendered
            .warnings
            .iter()
            .any(|warning| warning.contains("inline style spans could not be remapped")),
        "{:?}",
        rendered.warnings
    );
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

#[test]
fn base_pipeline_renders_inline_non_attrs_overrides() {
    let mut params = base_params();
    params.text =
        "<color=#ff0000><stretching=160,120><offset=4,-3>A</offset></stretching></color>\n<line-spacing=18,120><kerning=8,0>BC</kerning></line-spacing>"
            .to_string();
    params.enable_inline_style_tags = true;
    params.width_px = 180;

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render inline non-attrs case: {error}")
    });

    assert!(
        !rendered
            .warnings
            .iter()
            .any(|warning| { warning.contains("currently apply only attrs-level overrides") }),
        "glyph-level inline override warning should disappear after implementation"
    );
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

/// The glyph height scale is anchored at the run BASELINE: an inline
/// `<stretching=100%,H%>` span changes a glyph's ink EXTENT and nothing else,
/// so its bottom edge stays on the same row as the unscaled glyphs beside it.
///
/// Before the baseline anchor the scale pivoted the glyph's own ink-box
/// centre, which left a 50 % glyph floating ~6 px above the line (the reported
/// "текст" defect) and pushed a 200 % one below it. Tolerance is 1 px: the
/// outline is rasterized at the scaled size, so the bottom edge can land one
/// anti-aliased row either way.
#[test]
fn inline_height_scale_keeps_glyphs_on_the_line_baseline() {
    for height_percent in [50u32, 200] {
        let mut params = base_params();
        params.enable_inline_style_tags = true;
        params.width_px = 512;
        params.text = format!("H <stretching=100%,{height_percent}%>H</stretching> H");

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render the inline height case: {error}")
        });
        let clusters = ink_column_clusters(&rendered);
        assert_eq!(
            clusters.len(),
            3,
            "the three space-separated H glyphs must stay separate at {height_percent}%: {clusters:?}"
        );

        let baseline_row = clusters[0].3;
        for (idx, cluster) in clusters.iter().enumerate() {
            let delta = cluster.3 as i64 - baseline_row as i64;
            assert!(
                delta.abs() <= 1,
                "glyph {idx} ink bottom {} must sit on the baseline row {baseline_row} at {height_percent}% (delta {delta}): {clusters:?}",
                cluster.3
            );
        }

        // The scale must still HAPPEN — a baseline anchor is not a no-op.
        let plain_height = clusters[0].3 - clusters[0].2;
        let scaled_height = clusters[1].3 - clusters[1].2;
        if height_percent < 100 {
            assert!(
                scaled_height * 2 < plain_height * 3,
                "the 50% glyph must be visibly shorter ({scaled_height} vs {plain_height})"
            );
        } else {
            assert!(
                scaled_height > plain_height,
                "the 200% glyph must be visibly taller ({scaled_height} vs {plain_height})"
            );
        }
    }
}

/// A partial-line inline `<stretching>` height span must not move the COLUMN
/// GAPS of vertical text.
///
/// The vertical path reads `line_extra_spacing_table` as a horizontal gap
/// between columns, so the old "any overlapping stretch span rewrites the
/// line's height percent" rule leaked a per-character height change straight
/// into the spacing between whole columns (at 50 % it pulled them 18 px
/// closer here). The grow-only ink-rise room is meaningless for a
/// horizontal gap and is deliberately NOT applied there either, so the table
/// the vertical path consumes must be invariant to the tag in BOTH
/// directions.
///
/// The end-to-end half of the check uses the shrinking direction only: a
/// TALLER glyph legitimately widens its own cell, because a vertical cell is
/// sized by its scaled ink extent (`measure_vertical_glyph_visual_width`,
/// `vertical_step_follows_glyph_ink_height`) — that is the cell width, not
/// the gap, and it is not what this contract is about.
#[test]
fn vertical_column_gaps_ignore_an_inline_height_span() {
    let mut params = base_params();
    params.enable_inline_style_tags = true;
    params.text_line_mode = TextLineMode::Vertical;
    let font_size_px = params.font_size_px;

    // The exact table the vertical path is handed for its column gaps.
    let column_gap_table = |text: &str| -> Vec<f32> {
        let parsed = crate::inline_styles::parse_inline_style_tags(text, font_size_px);
        let offsets = super::compute_layout_line_offsets(parsed.plain_text.as_str());
        super::compute_line_extra_spacing_table(
            &params,
            parsed.plain_text.as_str(),
            offsets.as_slice(),
            Some(parsed.spans.as_slice()),
            font_size_px,
            0.0,
        )
    };
    let plain_table = column_gap_table("HH\nHH");
    for percent in ["50%", "200%"] {
        let tagged_table =
            column_gap_table(&format!("H<stretching=100%,{percent}>H</stretching>\nHH"));
        assert_eq!(
            tagged_table, plain_table,
            "an inline height span must not change the column-gap table at {percent}"
        );
    }

    // End-to-end: the shrinking direction cannot touch the cell width, so any
    // movement of the ink columns would come from the gap.
    let column_edges = |text: &str| -> Vec<(usize, usize)> {
        let mut render_params = params.clone();
        render_params.text = text.to_string();
        render_params.width_px = 512;
        let rendered = render_text_to_image(&render_params, None)
            .unwrap_or_else(|error| panic!("vertical stretch render: {error}"));
        ink_column_clusters(&rendered)
            .into_iter()
            .map(|(min_x, max_x, _, _)| (min_x, max_x))
            .collect()
    };
    let plain = column_edges("HH\nHH");
    assert_eq!(plain.len(), 2, "two ink columns expected: {plain:?}");
    let shrunk = column_edges("H<stretching=100%,50%>H</stretching>\nHH");
    assert_eq!(
        shrunk.len(),
        plain.len(),
        "the column count must not change: {shrunk:?} vs {plain:?}"
    );
    assert_eq!(
        shrunk[1].0 as i64 - shrunk[0].1 as i64,
        plain[1].0 as i64 - plain[0].1 as i64,
        "a partial 50% height span must not narrow the column gap ({shrunk:?} vs {plain:?})"
    );
}

/// PURE geometry of the baseline anchor, at float precision.
///
/// The pixel-level baseline tests reduce ink to integer rows with a 1 px
/// tolerance, so a sub-pixel per-glyph drift — exactly the shape of the old
/// box-centre bug, which displaced each glyph by
/// `(glyph_h / 2 − placement_top) · (1 − height_mul)` — could slip through.
/// This pins the invariant directly on glyphs with UNLIKE `placement_top`:
/// the scaled box's BOTTOM edge relative to the baseline must scale by
/// exactly `height_mul`, whatever the glyph's own ink box, and the whole box
/// must be untouched at `height_mul == 1`.
#[test]
fn baseline_anchored_scale_is_exact_for_unlike_ink_boxes() {
    // (glyph_w, glyph_h, placement_top): a capital, a descender-only glyph
    // (box entirely BELOW the baseline, negative top), a period sitting on
    // the baseline, and a tall accented glyph.
    let boxes = [
        (20.0f32, 26.0f32, 26.0f32),
        (14.0, 9.0, -1.0),
        (5.0, 5.0, 5.0),
        (21.0, 37.0, 37.0),
    ];
    let baseline_y = 100.0f32;

    for height_mul in [0.5f32, 1.0, 2.0, 0.37, 2.91] {
        let scale = super::GlyphScaleSettings { width_mul: 1.0, height_mul };
        for (glyph_w, glyph_h, placement_top) in boxes {
            // Content-space box top, as `build_horizontal_placement` derives it.
            let src_top = baseline_y - placement_top;
            let (_, center_y) = scale.scaled_center_about_baseline(
                0.0,
                src_top,
                glyph_w,
                glyph_h,
                baseline_y,
            );
            // The drawn box is `center +- scaled_height / 2` (the outline is
            // scaled by `height_mul` about `center`, see `glyph_outline_transform`).
            let scaled_bottom = center_y + glyph_h * height_mul * 0.5;
            let scaled_top = center_y - glyph_h * height_mul * 0.5;
            // Both edges are the unscaled ones scaled about the baseline.
            let expect_bottom = baseline_y + (src_top + glyph_h - baseline_y) * height_mul;
            let expect_top = baseline_y + (src_top - baseline_y) * height_mul;
            assert!(
                (scaled_bottom - expect_bottom).abs() <= 1e-4,
                "bottom edge must scale about the baseline (h={glyph_h}, top={placement_top}, \
                 mul={height_mul}): {scaled_bottom} vs {expect_bottom}"
            );
            assert!(
                (scaled_top - expect_top).abs() <= 1e-4,
                "top edge must scale about the baseline (h={glyph_h}, top={placement_top}, \
                 mul={height_mul}): {scaled_top} vs {expect_top}"
            );

            // The rect form must describe the SAME box as the centre form.
            let (_, rect_top, _, rect_height) = scale.scaled_rect_about_baseline(
                0.0,
                src_top,
                glyph_w,
                glyph_h,
                baseline_y,
            );
            assert!(
                (rect_top + rect_height * 0.5 - center_y).abs() <= 1e-4,
                "rect and centre helpers must agree: {rect_top}+{rect_height} vs {center_y}"
            );

            if (height_mul - 1.0).abs() <= f32::EPSILON {
                // An unscaled glyph must land exactly where the plain
                // box-centre placement puts it, bit for bit.
                let plain_center_y = src_top + glyph_h * 0.5;
                assert_eq!(
                    center_y, plain_center_y,
                    "height_mul == 1 must be an exact no-op (h={glyph_h}, top={placement_top})"
                );
            }
        }
    }

    // The defect this replaces: at a box-centre anchor two glyphs with
    // unlike ink boxes end up on DIFFERENT baselines. Here they must not.
    let scale = super::GlyphScaleSettings { width_mul: 1.0, height_mul: 0.5 };
    let bottoms: Vec<f32> = boxes
        .iter()
        .map(|(glyph_w, glyph_h, placement_top)| {
            let src_top = baseline_y - placement_top;
            let (_, center_y) = scale.scaled_center_about_baseline(
                0.0,
                src_top,
                *glyph_w,
                *glyph_h,
                baseline_y,
            );
            // Baseline-relative bottom, normalized by the glyph's own
            // unscaled baseline-relative bottom: identical for every glyph.
            let unscaled_bottom = src_top + glyph_h - baseline_y;
            (center_y + glyph_h * 0.5 * 0.5 - baseline_y) - unscaled_bottom * 0.5
        })
        .collect();
    for offset in &bottoms {
        assert!(
            offset.abs() <= 1e-4,
            "every glyph must scale about ONE baseline, not its own centre: {bottoms:?}"
        );
    }
}

/// Path of a secondary fixture face, used to build a MIXED-face line. Its
/// ink extents differ from the Arimo caller-font fixture, which is the whole
/// point: the grow-only room must follow the face that carries the stretch.
fn second_test_font_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fonts/ui/core/00-NotoSans-Regular.ttf")
}

/// The grow-only inline height room follows the glyphs that ACTUALLY carry
/// the stretch, on their OWN face, not the face of the line's first glyph.
///
/// A tall `<stretching>` span can sit on an inline `<font=…>` or on a
/// fallback face whose letters reach higher than the face the line opens
/// with. Sizing the room from one buffer-wide face then mis-allocates the gap
/// above the line.
#[test]
fn inline_height_room_follows_the_stretched_glyphs_own_face() {
    use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, fontdb};

    let mut db = fontdb::Database::new();
    for path in [test_font_path(), second_test_font_path()] {
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("fixture font {}: {error}", path.display()));
        db.load_font_data(bytes);
    }
    let families: Vec<String> = db
        .faces()
        .filter_map(|face| face.families.first().cloned().map(|(name, _)| name))
        .collect();
    assert_eq!(families.len(), 2, "two fixture faces expected: {families:?}");
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

    let em = 36.0f32;
    // Ink rise above the baseline of the glyphs each face draws for `text`,
    // measured the way `InlineHeightRoom::measure` measures it.
    let ink_rise = |font_system: &mut FontSystem, family: &str, text: &str| -> f32 {
        let mut probe = Buffer::new(font_system, Metrics::new(em, em));
        probe.set_size(font_system, None, None);
        let attrs = Attrs::new()
            .family(Family::Name(family))
            .metrics(Metrics::new(em, em));
        probe.set_text(font_system, text, &attrs, Shaping::Advanced);
        probe.shape_until_scroll(font_system, false);
        let glyphs: Vec<_> = probe
            .layout_runs()
            .flat_map(|run| run.glyphs.to_vec())
            .collect();
        let mut cache = crate::vector::OutlineCache::new();
        glyphs
            .iter()
            .filter_map(|glyph| {
                crate::glyph_blit::resolve_outline_for_glyph(
                    font_system,
                    &mut cache,
                    glyph,
                    None,
                )
            })
            .map(|outline| (-outline.local_bbox().0[1]).max(0.0))
            .fold(0.0f32, f32::max)
    };
    let opening_rise = ink_rise(&mut font_system, &families[0], "Ab");
    let stretched_rise = ink_rise(&mut font_system, &families[1], "Cd");
    assert!(
        (stretched_rise - opening_rise).abs() > 0.5,
        "the two fixture faces must draw visibly different ink heights, or this \
         test cannot tell the two implementations apart \
         ({opening_rise} vs {stretched_rise})"
    );

    // `Cd` is stretched to 300% AND shaped in the second face; `Ab` opens the
    // line in the first face, exactly the mixed-face case.
    let parsed = crate::inline_styles::parse_inline_style_tags(
        "Ab<stretching=100%,300%>Cd</stretching>",
        em,
    );
    assert_eq!(parsed.plain_text, "AbCd");

    let opening_attrs = Attrs::new()
        .family(Family::Name(families[0].as_str()))
        .metrics(Metrics::new(em, em));
    let stretched_attrs = Attrs::new()
        .family(Family::Name(families[1].as_str()))
        .metrics(Metrics::new(em, em));
    let mut buffer = Buffer::new(&mut font_system, Metrics::new(em, em));
    buffer.set_size(&mut font_system, None, None);
    buffer.set_rich_text(
        &mut font_system,
        [("Ab", opening_attrs.clone()), ("Cd", stretched_attrs)],
        &opening_attrs,
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(&mut font_system, false);

    let mut params = base_params();
    params.font_size_px = em;
    params.enable_inline_style_tags = true;
    let offsets = super::compute_layout_line_offsets(parsed.plain_text.as_str());
    let room = super::InlineHeightRoom::measure(
        &params,
        &buffer,
        &mut font_system,
        offsets.as_slice(),
        Some(parsed.spans.as_slice()),
    );

    // 300% against a 100% global height leaves an excess multiplier of 2.0.
    let excess = 2.0f32;
    assert!(
        (room.above(0) - stretched_rise * excess).abs() <= 1e-3,
        "the room must come from the STRETCHED glyphs' own face: {} vs {}",
        room.above(0),
        stretched_rise * excess
    );
    // The exact regression: sizing the room from the line's opening face.
    assert!(
        (room.above(0) - opening_rise * excess).abs() > 1e-3,
        "sizing the room from the line's opening face is wrong ({} vs the \
         opening face's {})",
        room.above(0),
        opening_rise * excess
    );
}

/// A PARTIAL inline `<stretching>` height span never re-spaces its line, and
/// the grow-only room lands on the gap ABOVE the tagged line and nowhere else.
///
/// Entry `i` is the extra advance of the gap below line `i` — the gap ABOVE
/// line `i+1` — so it must grow by line `i+1`'s ink rise and by nothing else.
/// Feeding an exact `InlineHeightRoom` pins that pairing without a shaped
/// buffer; the measurement itself is covered by
/// `inline_height_room_*` and the end-to-end tests.
#[test]
fn inline_height_span_grows_the_line_advance_and_never_shrinks_it() {
    let mut params = base_params();
    params.enable_inline_style_tags = true;
    let font_size_px = params.font_size_px;
    let no_room = super::InlineHeightRoom::default();

    let advance_table = |text: &str, room: &super::InlineHeightRoom| -> Vec<f32> {
        let parsed = crate::inline_styles::parse_inline_style_tags(text, font_size_px);
        let offsets = super::compute_layout_line_offsets(parsed.plain_text.as_str());
        super::line_baseline_advance_table(
            &params,
            parsed.plain_text.as_str(),
            offsets.as_slice(),
            Some(parsed.spans.as_slice()),
            font_size_px,
            0.0,
            room,
        )
    };

    let plain = advance_table("AB\nCD\nEF", &no_room);
    assert_eq!(plain.len(), 3, "three layout lines: {plain:?}");

    // Half the height on half of line 1 asks for no room, and the spacing
    // table itself must not react to the stretch tag either.
    let shrunk = advance_table("A<stretching=100%,50%>B</stretching>\nCD\nEF", &no_room);
    assert_eq!(shrunk, plain, "a partial 50% span must not re-space the line");

    // A tall span on line 1 (index 0) asks for room ABOVE line 1 — a gap that
    // does not exist — so NO gap moves. This is the user-reported defect: the
    // distance to the line BELOW a stretched line must not grow.
    let first_line = super::InlineHeightRoom::from_above(vec![26.0, 0.0, 0.0]);
    let tall_first = advance_table("A<stretching=100%,200%>B</stretching>\nCD\nEF", &first_line);
    assert_eq!(
        tall_first, plain,
        "a tall span must not widen the gap BELOW its own line: {tall_first:?} vs {plain:?}"
    );

    // The same span on line 2 (index 1) grows the gap ABOVE it, entry 0, by
    // that line's ink rise — and only that entry.
    let second_line = super::InlineHeightRoom::from_above(vec![0.0, 26.0, 0.0]);
    let tall_second =
        advance_table("AB\nC<stretching=100%,200%>D</stretching>\nEF", &second_line);
    assert!(
        (tall_second[0] - plain[0] - 26.0).abs() <= 1e-3,
        "gap above the tagged line must grow by its ink rise: {tall_second:?}"
    );
    assert!(
        (tall_second[1] - plain[1]).abs() <= 1e-3,
        "the gap below the tagged line must not move: {tall_second:?} vs {plain:?}"
    );
}

/// Baseline-to-baseline advance between the two ink bands of a two-line
/// render. Every text here uses glyphs without descenders, so each band's
/// bottom row IS that line's baseline.
fn two_line_baseline_advance(text: &str) -> usize {
    let mut params = base_params();
    params.enable_inline_style_tags = true;
    params.width_px = 512;
    params.text = text.to_string();
    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render the two-line height case: {error}")
    });
    let bands = ink_row_bands(&rendered);
    assert_eq!(bands.len(), 2, "two separated text lines expected for {text:?}: {bands:?}");
    bands[1].1 - bands[0].1
}

/// End-to-end: an inline `<stretching>` height span grows the gap ABOVE its
/// own line and NOTHING else.
///
/// Two separate defects are pinned here, both user-reported.
/// - A 50 % span used to drive the whole line's spacing through
///   `effective_spacing_percent`, pulling line 2 up by `font_size * 0.5`
///   (72 -> 54 px here) until the ink bands merged.
/// - A 200 % span used to widen the gap BELOW its line as well, by the face
///   descent (72 -> 80 px). The rule is upward-only now: the gap below a
///   stretched line must be byte-identical to the untagged render, and a
///   stretched descender is allowed to reach into it (see `InlineHeightRoom`).
#[test]
fn inline_height_span_grows_only_the_gap_above_its_own_line() {
    let plain = two_line_baseline_advance("H H\nH H");

    // Line 1 tagged: the gap BELOW it must not move, in either direction.
    let shrunk_below = two_line_baseline_advance("H <stretching=100%,50%>H</stretching>\nH H");
    let tall_below = two_line_baseline_advance("H <stretching=100%,200%>H</stretching>\nH H");
    assert_eq!(
        shrunk_below, plain,
        "a partial 50% span must leave the gap below its line alone ({shrunk_below} vs {plain})"
    );
    assert_eq!(
        tall_below, plain,
        "a partial 200% span must NOT widen the gap below its line ({tall_below} vs {plain})"
    );

    // Line 2 tagged: the gap ABOVE it grows, by the glyph's ink rise. `H` is a
    // cap-height glyph (~0.717 em = 25.8 px at 36 px), so the advance lands
    // near 72 + 26; the face ascent (~32.6 px) would overshoot to ~105.
    let tall_above = two_line_baseline_advance("H H\nH <stretching=100%,200%>H</stretching>");
    assert!(
        tall_above > plain,
        "a 200% span must grow the gap above its line ({tall_above} vs {plain})"
    );
    assert!(
        tall_above.abs_diff(plain + 26) <= 2,
        "the growth must be the cap-height ink rise, not the face ascent \
         ({tall_above} vs ~{} expected; the face-ascent rule overshot to ~{})",
        plain + 26,
        plain + 33
    );
}

/// The room grows by the glyphs' REAL INK rise, not by the face `ascent`.
///
/// A face's `ascent` includes headroom far above the ink of most lines, so
/// scaling it made the gap grow visibly faster than the letters did — the
/// user-reported "the distance to the line above grows a bit faster than the
/// character height". A line of x-height-only glyphs is where the two numbers
/// diverge most: `x` tops out around 0.53 em against a ~0.9 em ascent.
#[test]
fn inline_height_room_grows_by_real_ink_not_face_ascent() {
    use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, fontdb};

    let mut db = fontdb::Database::new();
    let bytes = std::fs::read(test_font_path()).expect("fixture font bytes");
    db.load_font_data(bytes);
    let family = db
        .faces()
        .next()
        .and_then(|face| face.families.first().cloned())
        .map(|(name, _language)| name)
        .expect("fixture family name");
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

    let em = 36.0f32;
    let attrs = Attrs::new()
        .family(Family::Name(family.as_str()))
        .metrics(Metrics::new(em, em));

    // `xx` with the SECOND `x` stretched to 200%.
    let parsed = crate::inline_styles::parse_inline_style_tags(
        "x<stretching=100%,200%>x</stretching>",
        em,
    );
    assert_eq!(parsed.plain_text, "xx");
    let mut buffer = Buffer::new(&mut font_system, Metrics::new(em, em));
    buffer.set_size(&mut font_system, None, None);
    buffer.set_text(&mut font_system, parsed.plain_text.as_str(), &attrs, Shaping::Advanced);
    buffer.shape_until_scroll(&mut font_system, false);

    // The two candidate measurements for the same glyph, side by side.
    let glyphs: Vec<_> = buffer
        .layout_runs()
        .flat_map(|run| run.glyphs.to_vec())
        .collect();
    let glyph = glyphs.first().expect("a shaped `x`");
    let mut outline_cache = crate::vector::OutlineCache::new();
    let ink_rise = crate::glyph_blit::resolve_outline_for_glyph(
        &mut font_system,
        &mut outline_cache,
        glyph,
        None,
    )
    .map(|outline| (-outline.local_bbox().0[1]).max(0.0))
    .expect("`x` has an outline");
    let face_ascent = font_system
        .get_font(glyph.font_id)
        .map(|font| font.as_swash().metrics(&[]).scale(em).ascent)
        .expect("fixture face metrics");
    assert!(
        face_ascent - ink_rise > 10.0,
        "the fixture must keep ink rise and face ascent far apart, or this test \
         cannot tell the two rules apart ({ink_rise} vs {face_ascent})"
    );

    let mut params = base_params();
    params.font_size_px = em;
    params.enable_inline_style_tags = true;
    let offsets = super::compute_layout_line_offsets(parsed.plain_text.as_str());
    let room = super::InlineHeightRoom::measure(
        &params,
        &buffer,
        &mut font_system,
        offsets.as_slice(),
        Some(parsed.spans.as_slice()),
    );

    // 200% against a 100% global height leaves an excess multiplier of 1.0.
    assert!(
        (room.above(0) - ink_rise).abs() <= 1e-3,
        "the room must be the glyph's ink rise: {} vs {ink_rise}",
        room.above(0)
    );
    assert!(
        room.above(0) < face_ascent - 10.0,
        "the room must NOT be the face ascent: {} vs {face_ascent}",
        room.above(0)
    );
}

/// Several height tags on ONE line ask for the LARGEST rise among them, never
/// for their sum.
///
/// Two 150 % spans must space exactly like one, and any mix must space
/// exactly like its tallest member alone — wherever that member sits in
/// reading order, so a "last span wins" rule (the shape of the original
/// whole-line spacing defect) is caught as well as a summing one. All texts
/// use `x`, whose ink rise is ~19 px at 36 px: a sum of three spans would
/// overshoot the single-span advance by tens of px, far outside the 1 px
/// rasterization tolerance.
#[test]
fn several_height_spans_on_one_line_take_the_maximum_not_the_sum() {
    let plain = two_line_baseline_advance("x x\nx x");
    let single_200 =
        two_line_baseline_advance("x x\nx <stretching=100%,200%>x</stretching> x x");
    assert!(
        single_200 > plain,
        "a 200% span must grow the gap above its line ({single_200} vs {plain})"
    );

    // Two spans, the larger first and then last.
    for text in [
        "x x\nx <stretching=100%,200%>x</stretching> <stretching=100%,150%>x</stretching> x",
        "x x\nx <stretching=100%,150%>x</stretching> <stretching=100%,200%>x</stretching> x",
    ] {
        assert_eq!(
            two_line_baseline_advance(text),
            single_200,
            "two spans must space like their tallest alone, not like their sum: {text:?}"
        );
    }

    // Three spans, the largest first, in the middle and last: a sum and a max
    // can coincide on two equal values but never on three distinct ones.
    for text in [
        "x x\nx <stretching=100%,200%>x</stretching> <stretching=100%,150%>x</stretching> <stretching=100%,120%>x</stretching>",
        "x x\nx <stretching=100%,120%>x</stretching> <stretching=100%,200%>x</stretching> <stretching=100%,150%>x</stretching>",
        "x x\nx <stretching=100%,120%>x</stretching> <stretching=100%,150%>x</stretching> <stretching=100%,200%>x</stretching>",
    ] {
        assert_eq!(
            two_line_baseline_advance(text),
            single_200,
            "three spans must space like their tallest alone, not like their sum: {text:?}"
        );
    }

    // Two EQUAL spans must also space like one of them.
    let one_150 = two_line_baseline_advance("x x\nx <stretching=100%,150%>x</stretching> x x");
    let two_150 = two_line_baseline_advance(
        "x x\nx <stretching=100%,150%>x</stretching> <stretching=100%,150%>x</stretching> x",
    );
    assert_eq!(
        two_150, one_150,
        "two identical spans must space exactly like one ({two_150} vs {one_150})"
    );
    assert!(
        one_150 < single_200,
        "a 150% span must ask for less room than a 200% one ({one_150} vs {single_200})"
    );
}

/// The GLOBAL `glyph_height_percent` is anchored at the baseline too.
///
/// It is the same defect, only harder to see: with a box-centre pivot every
/// glyph drifted by its OWN centre-to-baseline distance, so `H` and `.` ended
/// up on different baselines (measured 13 vs 18 at 50 %). Glyphs with very
/// different ink boxes must keep ONE shared bottom row at any height.
#[test]
fn global_glyph_height_keeps_one_shared_baseline() {
    for height_percent in [50.0f32, 200.0] {
        let mut params = base_params();
        params.width_px = 512;
        params.glyph_height_percent = height_percent;
        params.text = "H .".to_string();

        let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
            panic!("render_next should render the global height case: {error}")
        });
        let clusters = ink_column_clusters(&rendered);
        assert_eq!(
            clusters.len(),
            2,
            "`H` and `.` must stay separate at {height_percent}%: {clusters:?}"
        );
        let delta = clusters[1].3 as i64 - clusters[0].3 as i64;
        assert!(
            delta.abs() <= 1,
            "`H` and `.` must share one ink bottom row at {height_percent}% (delta {delta}): {clusters:?}"
        );
    }
}

#[test]
fn base_pipeline_renders_sentence_newlines() {
    let mut params = base_params();
    params.text = "First sentence. Second sentence!".to_string();
    params.new_line_after_sentence = true;
    params.width_px = 320;

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render sentence-newline case: {error}")
    });
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

#[test]
fn ellipsis_expansion_replaces_only_u2026() {
    assert_eq!(replace_ellipsis_with_dots("Что…"), "Что...");
    assert_eq!(replace_ellipsis_with_dots("а… б…в"), "а... б...в");
    // Text without `…` must come back byte-identical.
    assert_eq!(replace_ellipsis_with_dots("Что..."), "Что...");
    // Sibling leader characters do not stand for three dots and stay put.
    assert_eq!(replace_ellipsis_with_dots("а‥б⋯в"), "а‥б⋯в");
}

#[test]
fn ellipsis_expansion_is_off_when_disabled() {
    let mut params = base_params();
    params.text = "Что…".to_string();
    params.trim_extra_spaces = false;
    params.replace_ellipsis_with_dots = false;
    assert_eq!(prepare_source_text(&params.text, &params), "Что…");
}

#[test]
fn ellipsis_expansion_feeds_sentence_newlines() {
    // The expansion runs FIRST, so the produced `...` is a sentence end for
    // `new_line_after_sentence` — the same as if the author typed three dots.
    let mut params = base_params();
    params.text = "Что… Дальше".to_string();
    params.trim_extra_spaces = false;
    params.replace_ellipsis_with_dots = true;
    params.new_line_after_sentence = true;
    assert_eq!(prepare_source_text(&params.text, &params), "Что...\nДальше");

    params.replace_ellipsis_with_dots = false;
    assert_eq!(prepare_source_text(&params.text, &params), "Что… Дальше");
}

#[test]
fn base_pipeline_renders_expanded_ellipsis() {
    let mut params = base_params();
    params.text = "Что… дальше…".to_string();
    params.replace_ellipsis_with_dots = true;
    params.width_px = 320;

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render expanded-ellipsis case: {error}")
    });
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

#[test]
fn formula_pipeline_renders_curved_text() {
    let mut params = base_params();
    params.text = "FORMULA PATH".to_string();
    params.width_px = 320;
    params.text_layout_mode = TextLayoutMode::Formula;
    params.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * w".to_string(),
        y_expr: "sin(t * tau) * 28".to_string(),
        rotation_expr: "rad(12) * sin(t * tau)".to_string(),
        use_tangent_rotation: true,
        offset_x_px: 0.0,
        offset_y_px: 42.0,
        ..TextFormulaLayoutParams::default()
    };

    let rendered = render_text_to_image(&params, None)
        .unwrap_or_else(|error| panic!("render_next should render formula test case: {error}"));
    assert!(alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some());
}

#[test]
fn global_rotation_rotates_formula_block() {
    // A flat horizontal on-path line (y = 0) is wide and short; a 90° global
    // rotation must turn the whole block tall and narrow, at the vector level.
    let mut params = base_params();
    params.text = "FORMULA".to_string();
    params.width_px = 320;
    params.text_layout_mode = TextLayoutMode::Formula;
    params.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * w".to_string(),
        y_expr: "0".to_string(),
        rotation_expr: "0".to_string(),
        use_tangent_rotation: false,
        ..TextFormulaLayoutParams::default()
    };

    params.global_rotation_deg = 0.0;
    let plain = render_text_to_image(&params, None).expect("formula plain render");
    let (plain_w, plain_h) =
        alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba).expect("formula bounds");

    params.global_rotation_deg = 90.0;
    let rotated = render_text_to_image(&params, None).expect("formula rotated render");
    assert!(rotated.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0));
    let (rotated_w, rotated_h) =
        alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
            .expect("formula rotated bounds");
    assert!(rotated_h > plain_h, "formula 90°: {rotated_h} !> {plain_h}");
    assert!(rotated_w < plain_w, "formula 90°: {rotated_w} !< {plain_w}");
}

#[test]
fn global_rotation_rotates_vector_lines_block() {
    // Custom vector lines use a fixed canvas; a non-zero global rotation must
    // grow it to the rotated bounds (no clipping) and turn a wide line tall.
    let mut params = base_params();
    params.text = "VECTOR".to_string();
    params.width_px = 260;
    params.text_layout_mode = TextLayoutMode::CustomVectorLines;
    params.vector_lines_layout = TextVectorLinesLayoutParams {
        width_px: 260,
        height_px: 80,
        lines: vec![TextVectorLine {
            points: vec![
                TextVectorPoint { x: 8.0, y: 40.0 },
                TextVectorPoint { x: 120.0, y: 40.0 },
                TextVectorPoint { x: 240.0, y: 40.0 },
            ],
            corner_smoothing_px: 16.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::ByLineLength,
            flip_text: false,
        }],
        ..TextVectorLinesLayoutParams::default()
    };

    params.global_rotation_deg = 0.0;
    let plain = render_text_to_image(&params, None).expect("vector-lines plain render");
    let (plain_w, plain_h) = alpha_bounds_from_rgba(plain.width, plain.height, &plain.rgba)
        .expect("vector-lines bounds");

    params.global_rotation_deg = 90.0;
    let rotated = render_text_to_image(&params, None).expect("vector-lines rotated render");
    assert!(rotated.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0));
    let (rotated_w, rotated_h) =
        alpha_bounds_from_rgba(rotated.width, rotated.height, &rotated.rgba)
            .expect("vector-lines rotated bounds");
    assert!(rotated_h > plain_h, "vector-lines 90°: {rotated_h} !> {plain_h}");
    assert!(rotated_w < plain_w, "vector-lines 90°: {rotated_w} !< {plain_w}");
}

#[test]
fn line_placement_shifts_vector_lines_perpendicular() {
    // A flat horizontal vector line on a fixed (untrimmed) canvas: the
    // content's top alpha row directly reflects the perpendicular shift.
    // +100% (сверху) must raise the content, -100% (снизу) must lower it,
    // and 0% sits between them.
    fn alpha_min_y(width: u32, height: u32, rgba: &[u8]) -> Option<usize> {
        let width = width as usize;
        (0..height as usize)
            .find(|&y| (0..width).any(|x| rgba[(y * width + x) * 4 + 3] != 0))
    }

    let mut params = base_params();
    params.text = "VECTOR".to_string();
    params.width_px = 260;
    params.text_layout_mode = TextLayoutMode::CustomVectorLines;
    params.vector_lines_layout = TextVectorLinesLayoutParams {
        width_px: 260,
        height_px: 140,
        lines: vec![TextVectorLine {
            points: vec![
                TextVectorPoint { x: 8.0, y: 70.0 },
                TextVectorPoint { x: 130.0, y: 70.0 },
                TextVectorPoint { x: 252.0, y: 70.0 },
            ],
            corner_smoothing_px: 16.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::ByLineLength,
            flip_text: false,
        }],
        ..TextVectorLinesLayoutParams::default()
    };

    params.line_placement_percent = 0.0;
    let centered = render_text_to_image(&params, None).expect("vector-lines centered render");
    let centered_min_y = alpha_min_y(centered.width, centered.height, &centered.rgba)
        .expect("centered content");

    params.line_placement_percent = 100.0;
    let top = render_text_to_image(&params, None).expect("vector-lines top render");
    let top_min_y = alpha_min_y(top.width, top.height, &top.rgba).expect("top content");

    params.line_placement_percent = -100.0;
    let bottom = render_text_to_image(&params, None).expect("vector-lines bottom render");
    let bottom_min_y =
        alpha_min_y(bottom.width, bottom.height, &bottom.rgba).expect("bottom content");

    assert!(
        top_min_y < centered_min_y,
        "+100% (сверху) must raise content: {top_min_y} !< {centered_min_y}"
    );
    assert!(
        bottom_min_y > centered_min_y,
        "-100% (снизу) must lower content: {bottom_min_y} !> {centered_min_y}"
    );
}

#[test]
fn line_placement_applied_on_formula_path() {
    // Mixed-height text (ascenders/descenders) so the per-glyph perpendicular
    // shift changes the trimmed raster; the direction itself is proven by the
    // vector-lines test and the `apply_line_placement` unit test.
    let mut params = base_params();
    params.text = "Apjqy bd".to_string();
    params.width_px = 320;
    params.text_layout_mode = TextLayoutMode::Formula;
    params.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * w".to_string(),
        y_expr: "0".to_string(),
        rotation_expr: "0".to_string(),
        use_tangent_rotation: false,
        ..TextFormulaLayoutParams::default()
    };

    params.line_placement_percent = 0.0;
    let centered = render_text_to_image(&params, None).expect("formula centered render");
    assert!(centered.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0));

    params.line_placement_percent = 100.0;
    let top = render_text_to_image(&params, None).expect("formula top render");
    assert!(top.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0));

    params.line_placement_percent = -100.0;
    let bottom = render_text_to_image(&params, None).expect("formula bottom render");
    assert!(bottom.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0));

    let centered_key = (centered.width, centered.height, centered.rgba);
    let top_key = (top.width, top.height, top.rgba);
    let bottom_key = (bottom.width, bottom.height, bottom.rgba);
    assert!(
        top_key != centered_key,
        "formula +100% must change the render vs 0%"
    );
    assert!(
        bottom_key != centered_key,
        "formula -100% must change the render vs 0%"
    );
    assert!(top_key != bottom_key, "formula +100% must differ from -100%");
}

#[test]
fn line_placement_ignored_by_shape() {
    // Shape reuses the formula render path but must HIDE/ignore line
    // placement: a non-zero percent must produce a byte-identical image.
    let mut params = base_params();
    params.text = "shape gating stays byte identical".to_string();
    params.width_px = 280;
    params.text_layout_mode = TextLayoutMode::Shape;
    params.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * 24".to_string(),
        y_expr: "0".to_string(),
        ..TextFormulaLayoutParams::default()
    };

    params.line_placement_percent = 0.0;
    let zero = render_text_to_image(&params, None).expect("shape zero render");
    params.line_placement_percent = 80.0;
    let nonzero = render_text_to_image(&params, None).expect("shape nonzero render");
    assert_eq!(zero.width, nonzero.width);
    assert_eq!(zero.height, nonzero.height);
    assert_eq!(
        zero.rgba, nonzero.rgba,
        "Shape must ignore line_placement_percent"
    );
}

#[test]
fn line_placement_ignored_by_raster_lines() {
    // CustomRasterLines reuses the drawn-lines render path but must
    // HIDE/ignore line placement: a non-zero percent must be byte-identical.
    let mut layout_image = image::RgbaImage::new(260, 80);
    layout_image.put_pixel(8, 40, image::Rgba([255, 0, 0, 255]));
    for x in 9..240 {
        layout_image.put_pixel(x, 40, image::Rgba([255, 0, 0, 128]));
    }
    let layout_path = std::env::temp_dir().join(format!(
        "manhwastudio_line_placement_raster_{}.png",
        std::process::id()
    ));
    layout_image
        .save(&layout_path)
        .unwrap_or_else(|error| panic!("should write raster layout image: {error}"));

    let mut params = base_params();
    params.text = "DRAWN".to_string();
    params.width_px = 260;
    params.text_layout_mode = TextLayoutMode::CustomRasterLines;
    params.drawn_lines_layout = TextDrawnLinesLayoutParams {
        image_path: Some(layout_path.clone()),
        ..TextDrawnLinesLayoutParams::default()
    };

    params.line_placement_percent = 0.0;
    let zero = render_text_to_image(&params, None).expect("raster zero render");
    params.line_placement_percent = 80.0;
    let nonzero = render_text_to_image(&params, None).expect("raster nonzero render");
    let _ = std::fs::remove_file(layout_path);
    assert_eq!(
        zero.rgba, nonzero.rgba,
        "CustomRasterLines must ignore line_placement_percent"
    );
}

#[test]
fn shape_formula_pipeline_keeps_fallback_warning_and_visible_alpha() {
    let mut params = base_params();
    params.text = "shape fallback path should stay readable".to_string();
    params.width_px = 280;
    params.text_layout_mode = TextLayoutMode::Shape;
    params.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * 24".to_string(),
        y_expr: "0".to_string(),
        ..TextFormulaLayoutParams::default()
    };

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render shape fallback test case: {error}")
    });

    assert!(
        rendered
            .warnings
            .iter()
            .any(|warning| warning.contains("Форма слишком узкая"))
    );
    assert!(
        alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some(),
        "render_next should still produce visible alpha after shape fallback"
    );
}

#[test]
fn drawn_lines_pipeline_renders_text_from_layout_image() {
    let mut layout_image = image::RgbaImage::new(260, 80);
    layout_image.put_pixel(8, 40, image::Rgba([255, 0, 0, 255]));
    for x in 9..240 {
        layout_image.put_pixel(x, 40, image::Rgba([255, 0, 0, 128]));
    }
    let layout_path = std::env::temp_dir().join(format!(
        "manhwastudio_drawn_lines_test_{}.png",
        std::process::id()
    ));
    layout_image
        .save(&layout_path)
        .unwrap_or_else(|error| panic!("should write drawn-lines layout image: {error}"));

    let mut params = base_params();
    params.text = "DRAWN".to_string();
    params.width_px = 260;
    params.text_layout_mode = TextLayoutMode::CustomRasterLines;
    params.drawn_lines_layout = TextDrawnLinesLayoutParams {
        image_path: Some(layout_path.clone()),
        ..TextDrawnLinesLayoutParams::default()
    };

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render drawn-lines test case: {error}")
    });
    let _ = std::fs::remove_file(layout_path);
    assert!(
        alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some(),
        "drawn-lines render should produce visible alpha: warnings={:?}",
        rendered.warnings
    );
}

#[test]
fn vector_lines_pipeline_renders_text_from_points() {
    let mut params = base_params();
    params.text = "VECTOR".to_string();
    params.width_px = 260;
    params.text_layout_mode = TextLayoutMode::CustomVectorLines;
    params.vector_lines_layout = TextVectorLinesLayoutParams {
        width_px: 260,
        height_px: 80,
        lines: vec![TextVectorLine {
            points: vec![
                TextVectorPoint { x: 8.0, y: 40.0 },
                TextVectorPoint { x: 120.0, y: 20.0 },
                TextVectorPoint { x: 240.0, y: 40.0 },
            ],
            corner_smoothing_px: 16.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::ByLineLength,
            flip_text: false,
        }],
        ..TextVectorLinesLayoutParams::default()
    };

    let rendered = render_text_to_image(&params, None).unwrap_or_else(|error| {
        panic!("render_next should render vector-lines test case: {error}")
    });
    assert!(
        alpha_bounds_from_rgba(rendered.width, rendered.height, &rendered.rgba).is_some(),
        "vector-lines render should produce visible alpha: warnings={:?}",
        rendered.warnings
    );
}

#[test]
fn vector_lines_pipeline_applies_inline_glyph_offset() {
    let mut base = base_params();
    base.text = "A".to_string();
    base.width_px = 120;
    base.text_layout_mode = TextLayoutMode::CustomVectorLines;
    base.vector_lines_layout = TextVectorLinesLayoutParams {
        width_px: 120,
        height_px: 90,
        use_tangent_rotation: false,
        lines: vec![TextVectorLine {
            points: vec![
                TextVectorPoint { x: 20.0, y: 30.0 },
                TextVectorPoint { x: 100.0, y: 30.0 },
            ],
            corner_smoothing_px: 0.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::ByLineLength,
            flip_text: false,
        }],
        ..TextVectorLinesLayoutParams::default()
    };

    let without_offset = render_text_to_image(&base, None).unwrap_or_else(|error| {
        panic!("render_next should render vector-lines offset baseline: {error}")
    });
    let mut with_offset = base;
    with_offset.text = "<offset=0,24>A</offset>".to_string();
    with_offset.enable_inline_style_tags = true;
    let with_offset = render_text_to_image(&with_offset, None).unwrap_or_else(|error| {
        panic!("render_next should render vector-lines inline offset: {error}")
    });

    let baseline_y = alpha_centroid_y(
        without_offset.width,
        without_offset.height,
        &without_offset.rgba,
    )
    .unwrap_or_else(|| panic!("baseline vector-lines render should have alpha"));
    let offset_y = alpha_centroid_y(with_offset.width, with_offset.height, &with_offset.rgba)
        .unwrap_or_else(|| panic!("offset vector-lines render should have alpha"));
    assert!(
        offset_y > baseline_y + 12.0,
        "inline Y offset should move vector-line glyph down: baseline={baseline_y}, offset={offset_y}"
    );
}

#[test]
fn apply_effects_to_image_without_effects_returns_unchanged() {
    let rgba = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
    let result = apply_effects_to_image(rgba.clone(), 2, 1, "", None)
        .unwrap_or_else(|error| panic!("empty effects should pass image through: {error}"));
    assert_eq!(result.width, 2);
    assert_eq!(result.height, 1);
    assert_eq!(result.rgba, rgba);

    // Пустой JSON-массив эффектов тоже означает «без эффектов».
    let result_empty_array = apply_effects_to_image(rgba.clone(), 2, 1, "[]", None)
        .unwrap_or_else(|error| panic!("empty effects array should pass through: {error}"));
    assert_eq!(result_empty_array.rgba, rgba);
}

#[test]
fn apply_effects_to_image_rejects_mismatched_buffer() {
    let result = apply_effects_to_image(vec![0u8; 7], 2, 1, "", None);
    assert!(
        result.is_err(),
        "buffer length not matching width*height*4 must be an error"
    );
}

#[test]
fn apply_effects_to_image_stroke_grows_canvas() {
    // Сплошной непрозрачный квадрат + обводка должны увеличить холст под запас контура.
    let width = 8u32;
    let height = 8u32;
    let rgba = vec![255u8; (width * height * 4) as usize];
    let effects_json = r#"[{"effect":"stroke","width_px":4,"color":[0,0,0,255]}]"#;
    let result = apply_effects_to_image(rgba, width, height, effects_json, None)
        .unwrap_or_else(|error| panic!("stroke effect should apply to image: {error}"));
    assert!(
        result.width >= width && result.height >= height,
        "stroke should not shrink the canvas: got {}x{}",
        result.width,
        result.height
    );
    assert_eq!(
        result.rgba.len(),
        (result.width * result.height * 4) as usize,
        "RGBA buffer must stay width*height*4 after effects"
    );
}

/// Alpha bounding box `(min_x, min_y, max_x, max_y)` in pixels, or `None` when
/// the image is fully transparent.
fn alpha_bbox(image: &RenderedTextImage) -> Option<(f32, f32, f32, f32)> {
    let width = image.width as usize;
    let height = image.height as usize;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (width, height, 0usize, 0usize);
    let mut found = false;
    for y in 0..height {
        for x in 0..width {
            if image.rgba[(y * width + x) * 4 + 3] == 0 {
                continue;
            }
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            found = true;
        }
    }
    found.then_some((min_x as f32, min_y as f32, max_x as f32, max_y as f32))
}

#[test]
fn extra_info_mean_and_median_land_near_symmetric_line_center() {
    // A short symmetric single line: both centers must be Some, sit inside the
    // image, and land near the inked alpha box center (loose tolerance).
    let mut params = base_params();
    params.text = "HH".to_string();
    params.align = HorizontalAlign::LEFT;
    params.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: true,
    };
    let image = render_text_to_image(&params, None).expect("render should succeed");
    let mean = image.extra.mean_center.expect("mean center requested");
    let median = image.extra.median_center.expect("median center requested");

    for center in [mean, median] {
        assert!(
            center[0] >= 0.0 && center[0] <= image.width as f32,
            "center x {center:?} must lie within the image width {}",
            image.width
        );
        assert!(
            center[1] >= 0.0 && center[1] <= image.height as f32,
            "center y {center:?} must lie within the image height {}",
            image.height
        );
    }

    let (min_x, min_y, max_x, max_y) = alpha_bbox(&image).expect("inked content");
    let box_cx = (min_x + max_x) * 0.5;
    let box_cy = (min_y + max_y) * 0.5;
    let tol = params.font_size_px; // loose: within one em of the ink center
    for center in [mean, median] {
        assert!(
            (center[0] - box_cx).abs() <= tol,
            "center x {center:?} should be near the ink center x {box_cx} (tol {tol})"
        );
        assert!(
            (center[1] - box_cy).abs() <= tol,
            "center y {center:?} should be near the ink center y {box_cy} (tol {tol})"
        );
    }
}

/// Renders `text` twice — hanging punctuation off, then on — and returns both
/// mean centers. Shared by the horizontal and formula exclusion tests.
fn mean_centers_without_and_with_hanging(
    text: &str,
    layout_mode: TextLayoutMode,
) -> ([f32; 2], [f32; 2], usize) {
    let mut params = base_params();
    params.text = text.to_string();
    params.text_layout_mode = layout_mode;
    params.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: false,
    };

    params.hanging_punctuation = 0.0;
    let off = render_text_to_image(&params, None).expect("render should succeed");
    params.hanging_punctuation = 1.0;
    let on = render_text_to_image(&params, None).expect("render should succeed");

    assert_eq!(
        [off.width, off.height],
        [on.width, on.height],
        "the fixture must not change the image size between the two renders, \
         so any center delta is the exclusion and not a different layout"
    );
    (
        off.extra.mean_center.expect("mean requested"),
        on.extra.mean_center.expect("mean requested"),
        off.width as usize,
    )
}

#[test]
fn formula_layout_excludes_hanging_punctuation_from_the_centers() {
    // The formula path hangs punctuation through the same horizontal wrap the
    // Normal path uses, so it must exclude it from the center sampling too —
    // it used to sample every glyph, letting a trailing "?!" drag the center
    // right while the pixels of that punctuation hung outside the block.
    let (off, on, _) = mean_centers_without_and_with_hanging("Aa?!", TextLayoutMode::Formula);
    assert!(
        on[0] < off[0] - 1.0,
        "excluding the trailing punctuation must pull the mean center LEFT: \
         off={off:?} on={on:?}"
    );
}

#[test]
fn formula_and_normal_agree_on_what_hangs() {
    // Same text, same wrap: both layout modes must move their center in the same
    // direction and by a comparable amount when the exclusion kicks in. This is
    // the regression guard against one path being fixed and the other forgotten.
    let (normal_off, normal_on, width) =
        mean_centers_without_and_with_hanging("Aa?!", TextLayoutMode::Normal);
    let (formula_off, formula_on, _) =
        mean_centers_without_and_with_hanging("Aa?!", TextLayoutMode::Formula);
    let normal_shift = normal_off[0] - normal_on[0];
    let formula_shift = formula_off[0] - formula_on[0];
    assert!(normal_shift > 1.0 && formula_shift > 1.0);
    // Loose: the two paths place glyphs differently, so only the magnitude class
    // is comparable, not the exact value.
    let tol = width as f32 * 0.25;
    assert!(
        (normal_shift - formula_shift).abs() <= tol,
        "the two paths must exclude the same glyphs: \
         normal_shift={normal_shift} formula_shift={formula_shift} tol={tol}"
    );
}

#[test]
fn vertical_layout_keeps_punctuation_in_the_centers() {
    // Vertical text never hangs punctuation (its wrap has no such flag), so there
    // is nothing hanging to exclude and the centers must be identical either way.
    // Guards against "fix it everywhere" symmetry that would silently move the
    // vertical center for a setting that mode does not implement.
    let mut params = base_params();
    params.text = "Aa?!".to_string();
    params.text_line_mode = TextLineMode::Vertical;
    params.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: true,
    };
    params.hanging_punctuation = 0.0;
    let off = render_text_to_image(&params, None).expect("render should succeed");
    params.hanging_punctuation = 1.0;
    let on = render_text_to_image(&params, None).expect("render should succeed");
    assert_eq!(off.extra, on.extra, "vertical centers must not react");
}

#[test]
fn extra_info_default_request_yields_default_extra() {
    // No request -> byte-identical fast path and a default (all-None) payload.
    let params = base_params();
    assert!(!params.extra_info.is_active());
    let image = render_text_to_image(&params, None).expect("render should succeed");
    assert_eq!(image.extra, crate::types::RenderedTextExtraInfo::default());
}

#[test]
fn extra_info_tracks_glyphs_through_canvas_growing_effect() {
    // A stroke grows the canvas symmetrically around the glyph, then trim crops
    // back. The effects+trim seams must shift the extra centers so they keep
    // pointing at the glyph: the mean center stays near the (symmetric) alpha
    // box center. Without the shift it would be stale by ~the stroke pad.
    let mut plain = base_params();
    plain.text = "O".to_string();
    plain.align = HorizontalAlign::LEFT;
    plain.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: false,
    };
    let plain_image = render_text_to_image(&plain, None).expect("plain render");
    let plain_mean = plain_image.extra.mean_center.expect("mean requested");
    let (pmin_x, pmin_y, pmax_x, pmax_y) = alpha_bbox(&plain_image).expect("plain ink");
    let plain_off = [
        plain_mean[0] - (pmin_x + pmax_x) * 0.5,
        plain_mean[1] - (pmin_y + pmax_y) * 0.5,
    ];

    let mut stroked = plain.clone();
    stroked.effects_json = r#"[{"effect":"stroke","width_px":8,"color":[0,0,0,255]}]"#.to_string();
    let stroked_image = render_text_to_image(&stroked, None).expect("stroked render");
    let stroked_mean = stroked_image.extra.mean_center.expect("mean requested");
    let (smin_x, smin_y, smax_x, smax_y) = alpha_bbox(&stroked_image).expect("stroked ink");
    let stroked_off = [
        stroked_mean[0] - (smin_x + smax_x) * 0.5,
        stroked_mean[1] - (smin_y + smax_y) * 0.5,
    ];

    // The stroke is symmetric, so the glyph center relative to the alpha box
    // center is stable across both renders (a stale, unshifted center would be
    // off by roughly the 8px stroke pad).
    assert!(
        (plain_off[0] - stroked_off[0]).abs() <= 2.0,
        "mean-center offset drifted in x: plain {plain_off:?} vs stroked {stroked_off:?}"
    );
    assert!(
        (plain_off[1] - stroked_off[1]).abs() <= 2.0,
        "mean-center offset drifted in y: plain {plain_off:?} vs stroked {stroked_off:?}"
    );
}

#[test]
fn extra_info_excludes_trailing_hanging_punctuation_when_enabled() {
    // Left-aligned text with a trailing '.' draws identically whether hanging
    // punctuation is on or off (only leading hang shifts layout), so the only
    // difference is the extra-info sampling: with hanging on, the trailing '.'
    // contributes nothing and both centers move LEFT.
    let mut off = base_params();
    off.text = "Hi.".to_string();
    off.align = HorizontalAlign::LEFT;
    off.hanging_punctuation = 0.0;
    off.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: true,
    };
    let mut on = off.clone();
    on.hanging_punctuation = 1.0;

    let off_image = render_text_to_image(&off, None).expect("hanging-off render");
    let on_image = render_text_to_image(&on, None).expect("hanging-on render");

    let off_mean = off_image.extra.mean_center.expect("mean off");
    let on_mean = on_image.extra.mean_center.expect("mean on");
    assert!(
        on_mean[0] < off_mean[0] - 0.5,
        "excluding the trailing '.' must move the mean center left: on {on_mean:?} vs off {off_mean:?}"
    );

    let off_median = off_image.extra.median_center.expect("median off");
    let on_median = on_image.extra.median_center.expect("median on");
    assert_ne!(
        on_median, off_median,
        "excluding the trailing '.' must change the median center"
    );
}

/// Assert both requested centers are present and land inside the image plane.
fn assert_centers_in_bounds(image: &RenderedTextImage) {
    let mean = image.extra.mean_center.expect("mean center requested");
    let median = image.extra.median_center.expect("median center requested");
    for center in [mean, median] {
        assert!(
            center[0] >= 0.0 && center[0] <= image.width as f32,
            "center x {center:?} must lie within width {}",
            image.width
        );
        assert!(
            center[1] >= 0.0 && center[1] <= image.height as f32,
            "center y {center:?} must lie within height {}",
            image.height
        );
    }
}

#[test]
fn extra_info_populated_on_formula_path() {
    // Formula mode routes through `formula::render` (retry loop). The extras
    // must be populated (both centers Some) and land inside the rendered image.
    let mut params = base_params();
    params.text = "FORMULA".to_string();
    params.width_px = 320;
    params.text_layout_mode = TextLayoutMode::Formula;
    params.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * w".to_string(),
        y_expr: "sin(t * tau) * 20".to_string(),
        rotation_expr: "0".to_string(),
        use_tangent_rotation: false,
        offset_y_px: 40.0,
        ..TextFormulaLayoutParams::default()
    };
    params.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: true,
    };
    let image = render_text_to_image(&params, None).expect("formula render");
    assert_centers_in_bounds(&image);

    // No request -> default (all-None) payload on the same path.
    let mut plain = params.clone();
    plain.extra_info = RenderExtraInfoRequest::default();
    let plain_image = render_text_to_image(&plain, None).expect("formula plain render");
    assert_eq!(
        plain_image.extra,
        crate::types::RenderedTextExtraInfo::default()
    );
}

#[test]
fn extra_info_populated_on_vector_lines_path() {
    // Custom vector lines route through `formula::render`'s drawn-lines path
    // (fixed canvas). Extras must be populated and inside the fixed canvas.
    let mut params = base_params();
    params.text = "VECTOR".to_string();
    params.width_px = 260;
    params.text_layout_mode = TextLayoutMode::CustomVectorLines;
    params.vector_lines_layout = TextVectorLinesLayoutParams {
        width_px: 260,
        height_px: 80,
        lines: vec![TextVectorLine {
            points: vec![
                TextVectorPoint { x: 8.0, y: 40.0 },
                TextVectorPoint { x: 120.0, y: 40.0 },
                TextVectorPoint { x: 240.0, y: 40.0 },
            ],
            corner_smoothing_px: 16.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::ByLineLength,
            flip_text: false,
        }],
        ..TextVectorLinesLayoutParams::default()
    };
    params.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: true,
    };
    let image = render_text_to_image(&params, None).expect("vector-lines render");
    assert_centers_in_bounds(&image);
}

#[test]
fn extra_info_centers_stay_in_bounds_under_formula_rotation() {
    // A 90° global rotation grows the formula canvas to the rotated bounds; the
    // extras, sampled post-rotation, must still lie inside the rotated image.
    let mut params = base_params();
    params.text = "FORMULA".to_string();
    params.width_px = 320;
    params.text_layout_mode = TextLayoutMode::Formula;
    params.formula_layout = TextFormulaLayoutParams {
        x_expr: "t * w".to_string(),
        y_expr: "0".to_string(),
        rotation_expr: "0".to_string(),
        use_tangent_rotation: false,
        ..TextFormulaLayoutParams::default()
    };
    params.global_rotation_deg = 90.0;
    params.extra_info = RenderExtraInfoRequest {
        mean_center: true,
        median_center: true,
    };
    let image = render_text_to_image(&params, None).expect("formula rotated render");
    assert_centers_in_bounds(&image);
}

/// REGRESSION (blocker): a REAL italic request — `force_italic` WITHOUT an
/// explicit faux slant — on a font that ships no italic face must render,
/// not panic.
///
/// Before the guard this reached cosmic-text with `Style::Italic` in the
/// attrs. `Style` is a HARD `Attrs::matches` filter
/// (`cosmic-text-0.14.2/src/attrs.rs:322-327`) and the whole fallback
/// iteration is built from that filtered set, so the upright-only fixture and
/// the upright-only bundled base left it EMPTY and
/// `shape.rs:274 .expect("no default font found")` panicked. (With the
/// shipped `fonts/ui/ext/12-NotoEmoji-Regular.ttf` present the set is not
/// empty — the emoji exemption keeps that one face in — and the failure mode
/// is a full-run `.notdef` tofu render instead; see
/// `font_registry::the_emoji_exemption_makes_a_database_wide_match_check_useless`.)
/// A panic here is doubly bad: `with_leased_font_system` has no Drop guard,
/// so it also leaks the leased `FontSystem` out of the pool.
#[test]
fn real_italic_without_an_italic_face_degrades_to_faux_instead_of_panicking() {
    let mut params = base_params();
    params.text = "Hi".to_string();
    params.font_size_px = 64.0;
    params.force_italic = true;
    assert!(
        params.faux_italic_slant_deg.is_none(),
        "this test only means something for a REAL italic request"
    );

    let degraded = render_text_to_image(&params, None)
        .expect("a real italic request must never panic the renderer");
    assert!(
        degraded.width > 0 && degraded.height > 0,
        "the degraded render must still produce pixels"
    );

    // The degradation is the documented faux italic at the default slant,
    // not "silently upright" and not some other angle.
    let mut explicit_faux = params.clone();
    explicit_faux.faux_italic_slant_deg = Some(SYNTHESIZED_ITALIC_SLANT_DEG);
    let explicit = render_text_to_image(&explicit_faux, None).expect("explicit faux render");
    assert_eq!(
        (degraded.width, degraded.height, &degraded.rgba),
        (explicit.width, explicit.height, &explicit.rgba),
        "the degraded render must equal an explicit faux italic at the default slant"
    );

    let mut upright = params.clone();
    upright.force_italic = false;
    let upright = render_text_to_image(&upright, None).expect("upright render");
    assert_ne!(
        (degraded.width, degraded.height, &degraded.rgba),
        (upright.width, upright.height, &upright.rgba),
        "the degradation must actually slant the text, not silently drop the italic"
    );

    // Degradation must not be silent (CLAUDE.md: no silent fallback for an
    // unsupported request).
    assert!(
        degraded
            .warnings
            .iter()
            .any(|warning| warning.contains("курсив")),
        "the degraded render must warn the user, got {:?}",
        degraded.warnings
    );
}

/// The same guard per inline span: a bare `<i>` is a REAL italic request and
/// must not panic (or silently leave the selected font) when the resolved
/// family ships no italic face.
#[test]
fn real_inline_italic_without_an_italic_face_degrades_to_faux() {
    let mut params = base_params();
    params.text = "a<i>b</i>c".to_string();
    params.font_size_px = 64.0;
    params.enable_inline_style_tags = true;

    let degraded = render_text_to_image(&params, None)
        .expect("a real inline italic must never panic the renderer");
    assert!(degraded.width > 0 && degraded.height > 0);
    assert!(
        degraded
            .warnings
            .iter()
            .any(|warning| warning.contains("<i>")),
        "the degraded inline render must warn the user, got {:?}",
        degraded.warnings
    );

    // An explicit `<i=12>` span is the faux path the degradation lands on.
    let mut explicit_faux = params.clone();
    explicit_faux.text = format!("a<i={SYNTHESIZED_ITALIC_SLANT_DEG}>b</i>c");
    let explicit = render_text_to_image(&explicit_faux, None).expect("explicit faux render");
    assert_eq!(
        (degraded.width, degraded.height, &degraded.rgba),
        (explicit.width, explicit.height, &explicit.rgba),
        "a degraded <i> span must equal an explicit faux <i=slant> span"
    );
}

/// A REAL bold request — `force_bold` WITHOUT explicit faux params — on a family
/// that ships no Bold file must be degraded to faux bold, not sent to the shaper.
///
/// Sending it costs twice, both silently: cosmic-text's primary face pick needs an
/// EXACT weight match and does not rank down inside the family, so the whole run
/// jumps to whatever family DOES have a 700 face (the bundled `Noto Sans Bold`);
/// and the script/common fallback passes admit only `weight_diff == 0` candidates,
/// so every bundled fallback font — all of them 400 — becomes unreachable and rare
/// glyphs in the same run render as tofu.
#[test]
fn real_bold_without_a_bold_face_degrades_to_faux() {
    let mut params = base_params();
    params.text = "Hi".to_string();
    params.font_size_px = 64.0;
    params.force_bold = true;
    assert!(
        params.faux_bold.is_none(),
        "this test only means something for a REAL bold request"
    );

    let degraded =
        render_text_to_image(&params, None).expect("a real bold request must still render");
    assert!(degraded.width > 0 && degraded.height > 0);

    // The degradation is the documented `<b=default>` strength, not "silently
    // regular" and not some other thickness.
    let mut explicit_faux = params.clone();
    explicit_faux.faux_bold = Some(FauxBoldParams::default());
    let explicit = render_text_to_image(&explicit_faux, None).expect("explicit faux render");
    assert_eq!(
        (degraded.width, degraded.height, &degraded.rgba),
        (explicit.width, explicit.height, &explicit.rgba),
        "the degraded render must equal an explicit faux bold at the default strength"
    );

    let mut plain = params.clone();
    plain.force_bold = false;
    let plain = render_text_to_image(&plain, None).expect("plain render");
    assert_ne!(
        (degraded.width, degraded.height, &degraded.rgba),
        (plain.width, plain.height, &plain.rgba),
        "the degradation must actually thicken the text, not silently drop the bold"
    );

    assert!(
        degraded
            .warnings
            .iter()
            .any(|warning| warning.contains("жирного начертания")),
        "the degraded render must warn the user, got {:?}",
        degraded.warnings
    );
}

/// The same guard per inline span: a bare `<b>` is a REAL bold request and must
/// not leave the selected font when its family ships no bold face.
#[test]
fn real_inline_bold_without_a_bold_face_degrades_to_faux() {
    let mut params = base_params();
    params.text = "a<b>b</b>c".to_string();
    params.font_size_px = 64.0;
    params.enable_inline_style_tags = true;

    let degraded =
        render_text_to_image(&params, None).expect("a real inline bold must still render");
    assert!(degraded.width > 0 && degraded.height > 0);
    assert!(
        degraded
            .warnings
            .iter()
            .any(|warning| warning.contains("<b>")),
        "the degraded inline render must warn the user, got {:?}",
        degraded.warnings
    );

    // An explicit `<b=3>` span is the faux path the degradation lands on (3 % is
    // `FauxBoldParams::default().thicken_percent`).
    let mut explicit_faux = params.clone();
    explicit_faux.text = format!(
        "a<b={}>b</b>c",
        FauxBoldParams::default().thicken_percent as u32
    );
    let explicit = render_text_to_image(&explicit_faux, None).expect("explicit faux render");
    assert_eq!(
        (degraded.width, degraded.height, &degraded.rgba),
        (explicit.width, explicit.height, &explicit.rgba),
        "a degraded <b> span must equal an explicit faux <b=thicken> span"
    );
}





/// A custom pair must MOVE the pen, and must do so under `KerningMode::Auto`
/// — the mode whose whole point is "use the font's own pair kerning". If the
/// override did not win there, an authored pair would be invisible in the
/// default mode.
#[test]
fn a_custom_pair_overrides_the_font_under_auto_kerning() {
    let mut params = base_params();
    params.text = "AV".to_string();
    params.font_size_px = 100.0;
    params.kerning_mode = KerningMode::Auto;

    let (plain_xs, plain_width) = fixture_run_layout("AV", &params);
    // -100 per mille of a 100 px em is a flat -10 px on the A->V step.
    let (kerned_xs, kerned_width) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -100.0)]);
    assert_eq!(plain_xs.len(), 2, "the fixture must shape 'AV' as two glyphs");
    assert_eq!(kerned_xs.len(), 2);
    assert!(
        (kerned_xs[0] - plain_xs[0]).abs() < 1e-3,
        "the first glyph of the run never moves: {kerned_xs:?} vs {plain_xs:?}"
    );
    assert!(
        kerned_xs[1] < plain_xs[1],
        "a -100 per mille pair must pull 'V' left; got {kerned_xs:?} vs {plain_xs:?}"
    );
    assert!(
        kerned_width < plain_width,
        "the logical width must shrink with the pen: {kerned_width} vs {plain_width}"
    );

    // The step is the LEFT glyph's own (un-kerned) advance PLUS the authored
    // delta — the pair REPLACES the font's value rather than adding to it. So
    // the authored offsets must map onto the pen linearly, in exact px: at em
    // 100 the three entries -100 / 0 / +100 per mille sit 10 px apart, anchored
    // on the un-kerned advance. `plain_xs[1]` is NOT that anchor — the Arimo
    // fixture kerns `AV` itself, which is precisely the value being replaced.
    let (neutral_xs, _) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', 0.0)]);
    let (widened_xs, _) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', 100.0)]);
    let tightened_step = kerned_xs[1] - kerned_xs[0];
    let neutral_step = neutral_xs[1] - neutral_xs[0];
    let widened_step = widened_xs[1] - widened_xs[0];
    assert!(
        (neutral_step - tightened_step - 10.0).abs() < 1e-2
            && (widened_step - neutral_step - 10.0).abs() < 1e-2,
        "-100 / 0 / +100 per mille must sit exactly 10 px apart at em 100; got \
         {tightened_step}, {neutral_step}, {widened_step}"
    );
    assert!(
        neutral_step > plain_xs[1] - plain_xs[0],
        "the fixture must actually kern 'AV' itself, otherwise this test proves \
         nothing about REPLACING that value: un-kerned {neutral_step} vs shaped {}",
        plain_xs[1] - plain_xs[0]
    );
}

/// The override replaces the font's pair value under EVERY mode, so `Fixed`
/// (which already drops font pair kerning) and `Auto` must land on the SAME
/// pen for an overridden pair. That equality is the whole "indistinguishable
/// from a built-in pair" contract.
#[test]
fn an_overridden_pair_lands_identically_under_auto_and_fixed() {
    let mut params = base_params();
    params.text = "AV".to_string();
    params.font_size_px = 100.0;

    params.kerning_mode = KerningMode::Auto;
    let (auto_xs, _) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -50.0)]);
    params.kerning_mode = KerningMode::Fixed;
    let (fixed_xs, _) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -50.0)]);
    assert_eq!(auto_xs.len(), fixed_xs.len());
    for (auto_x, fixed_x) in auto_xs.iter().zip(fixed_xs.iter()) {
        assert!(
            (auto_x - fixed_x).abs() < 1e-3,
            "an overridden pair must be mode-independent: {auto_xs:?} vs {fixed_xs:?}"
        );
    }
}

/// A pair the table does not list must leave layout BYTE-identical: the
/// presence of a table may not perturb text it says nothing about.
#[test]
fn an_unlisted_pair_leaves_the_pen_untouched() {
    let mut params = base_params();
    params.text = "AV".to_string();
    params.font_size_px = 100.0;
    params.kerning_mode = KerningMode::Auto;

    let (plain_xs, plain_width) = fixture_run_layout("AV", &params);
    // The table is non-empty (so the fast path is off) but mentions another pair.
    let (other_xs, other_width) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('Q', 'z', -200.0)]);
    assert_eq!(plain_xs, other_xs, "an unlisted pair must not move the pen");
    assert!((plain_width - other_width).abs() < 1e-3);
}

/// A `0.0` entry is an instruction, not a no-op: it CANCELS the font's own
/// kerning for that pair, so the pen must land on the un-kerned advance —
/// i.e. exactly where `KerningMode::Fixed` puts it without any override.
#[test]
fn a_zero_entry_cancels_the_fonts_own_pair_kerning() {
    let mut params = base_params();
    params.text = "AV".to_string();
    params.font_size_px = 100.0;

    params.kerning_mode = KerningMode::Fixed;
    let (unkerned_xs, _) = fixture_run_layout("AV", &params);
    params.kerning_mode = KerningMode::Auto;
    let (cancelled_xs, _) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', 0.0)]);
    assert_eq!(unkerned_xs.len(), cancelled_xs.len());
    for (unkerned_x, cancelled_x) in unkerned_xs.iter().zip(cancelled_xs.iter()) {
        assert!(
            (unkerned_x - cancelled_x).abs() < 1e-3,
            "a 0.0 entry must reproduce the un-kerned advance: {cancelled_xs:?} vs \
             {unkerned_xs:?}"
        );
    }
}

/// An override must follow the RUN's direction. cosmic-text lays a right-to-left
/// run out by subtracting each advance, so its pen steps LEFT while every input
/// the override path starts from (`hmtx` advances) is positive. Without the
/// owner's `StepDirection::FollowRun` orientation the overridden pair stepped the pen RIGHT — the glyphs
/// of an RTL word crossed over each other — and a negative authored delta
/// WIDENED the pair instead of tightening it.
#[test]
fn an_overridden_pair_follows_an_rtl_run_leftwards() {
    // Hebrew alef + bet: the fixture (Arimo) covers both, so the pair
    // is same-face and single-cluster on each side, i.e. genuinely overridable.
    const RTL_PAIR: &str = "\u{05D0}\u{05D1}";
    let mut params = base_params();
    params.text = RTL_PAIR.to_string();
    params.font_size_px = 100.0;
    params.kerning_mode = KerningMode::Auto;

    let (plain_xs, _) = fixture_run_layout(RTL_PAIR, &params);
    assert_eq!(plain_xs.len(), 2, "the fixture must shape the pair as two glyphs");
    assert!(
        plain_xs[1] < plain_xs[0],
        "precondition: cosmic-text must lay this run out right-to-left; got {plain_xs:?}"
    );

    let pairs = [('\u{05D0}', '\u{05D1}', -100.0)];
    let (kerned_xs, _) = fixture_run_layout_with_custom_kerning(RTL_PAIR, &params, &pairs);
    assert_eq!(kerned_xs.len(), 2);
    assert!(
        kerned_xs[1] < kerned_xs[0],
        "an overridden RTL pair must keep walking the pen LEFT, not reverse it; \
         got {kerned_xs:?}"
    );

    // Direction aside, the magnitudes must behave exactly as in an LTR run: the
    // three authored offsets -100 / 0 / +100 per mille sit 10 px apart at em 100,
    // and a NEGATIVE offset must TIGHTEN the pair (shrink the distance) in this
    // direction too.
    let (neutral_xs, _) = fixture_run_layout_with_custom_kerning(
        RTL_PAIR,
        &params,
        &[('\u{05D0}', '\u{05D1}', 0.0)],
    );
    let (widened_xs, _) = fixture_run_layout_with_custom_kerning(
        RTL_PAIR,
        &params,
        &[('\u{05D0}', '\u{05D1}', 100.0)],
    );
    let tightened_gap = (kerned_xs[1] - kerned_xs[0]).abs();
    let neutral_gap = (neutral_xs[1] - neutral_xs[0]).abs();
    let widened_gap = (widened_xs[1] - widened_xs[0]).abs();
    assert!(
        tightened_gap < neutral_gap && neutral_gap < widened_gap,
        "a negative authored delta must TIGHTEN an RTL pair: {tightened_gap} / \
         {neutral_gap} / {widened_gap}"
    );
    assert!(
        (neutral_gap - tightened_gap - 10.0).abs() < 1e-2
            && (widened_gap - neutral_gap - 10.0).abs() < 1e-2,
        "-100 / 0 / +100 per mille must sit exactly 10 px apart at em 100; got \
         {tightened_gap}, {neutral_gap}, {widened_gap}"
    );

    // The ANCHOR, not just the differences: an RTL step is spanned by the visually
    // LEFT glyph (`cur`), so a `0.0` entry on a pair the font does not kern must land
    // EXACTLY on the shaped pen, and -100 per mille exactly 10 px tighter than it. The
    // tracked Hebrew font has no alef -> bet kerning and alef / bet differ in width,
    // so anchoring on the wrong glyph is visible here.
    let mut hebrew = crate::characterization::TrackedFont::Hebrew.load();
    let (shaped_xs, _) = tracked_font_run_layout(&mut hebrew, RTL_PAIR, &params, &[]);
    let (cancelled_xs, _) = tracked_font_run_layout(
        &mut hebrew,
        RTL_PAIR,
        &params,
        &[('\u{05D0}', '\u{05D1}', 0.0)],
    );
    for (cancelled_x, shaped_x) in cancelled_xs.iter().zip(shaped_xs.iter()) {
        assert!(
            (cancelled_x - shaped_x).abs() < 1e-3,
            "a 0.0 entry on an un-kerned RTL pair is a no-op: {cancelled_xs:?} vs {shaped_xs:?}"
        );
    }
    let (tight_xs, _) = tracked_font_run_layout(
        &mut hebrew,
        RTL_PAIR,
        &params,
        &[('\u{05D0}', '\u{05D1}', -100.0)],
    );
    let natural_gap = (shaped_xs[1] - shaped_xs[0]).abs();
    let tight_gap = (tight_xs[1] - tight_xs[0]).abs();
    assert!(
        (natural_gap - tight_gap - 10.0).abs() < 1e-3,
        "-100 per mille must tighten the natural RTL step by exactly 10 px: {natural_gap} -> \
         {tight_gap}"
    );
}

/// `Optical` re-spaces pairs by measured ink, which would otherwise overwrite the
/// one distance the user dictated. An overridden pair must therefore land on the
/// SAME pen under `Optical` as under `Auto`/`Fixed` — the third leg of the "every
/// mode" contract, which the other tests do not exercise.
#[test]
fn an_overridden_pair_lands_identically_under_optical() {
    let mut params = base_params();
    params.text = "AV".to_string();
    params.font_size_px = 100.0;

    params.kerning_mode = KerningMode::Auto;
    let (auto_xs, _) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -50.0)]);
    params.kerning_mode = KerningMode::Optical;
    let (optical_xs, _) =
        fixture_run_layout_with_custom_kerning("AV", &params, &[('A', 'V', -50.0)]);
    assert_eq!(auto_xs.len(), optical_xs.len());
    for (auto_x, optical_x) in auto_xs.iter().zip(optical_xs.iter()) {
        assert!(
            (auto_x - optical_x).abs() < 1e-3,
            "an overridden pair must be mode-independent: {auto_xs:?} vs {optical_xs:?}"
        );
    }

    // Control: without the override `Optical` really does move this pair, so the
    // equality above proves the override wins rather than that the mode is inert.
    let (optical_plain_xs, _) = fixture_run_layout("AV", &params);
    assert!(
        (optical_plain_xs[1] - optical_xs[1]).abs() > 1e-2,
        "the optical pass must actually move 'AV' when it is not overridden: \
         {optical_plain_xs:?} vs {optical_xs:?}"
    );
}

// ---- Characterization of the advance between two adjacent glyphs (pen side) ----
//
// Recorded BEFORE the pair decision moved to one owner, so the move can be checked bit for
// bit. Fonts come from the tracked `fonts/ui` stack only (`crate::characterization`).

/// Shapes `text` as ONE line with the tracked `font` and runs the production
/// `horizontal_run_layout` over it, with `pairs` bound to that face the way
/// `CustomKerningMap::record` binds them in a render. Returns `(glyph_xs, line_width_px)`.
fn tracked_font_run_layout(
    font: &mut crate::characterization::LoadedFont,
    text: &str,
    params: &TextRenderParams,
    pairs: &[(char, char, f32)],
) -> (Vec<f32>, f32) {
    use crate::font_provider::CustomKerningTable;
    use crate::font_registry::CustomKerningMap;
    use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping};

    let mut custom_kerning = CustomKerningMap::default();
    if !pairs.is_empty() {
        custom_kerning.insert(
            font.face_id,
            Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
            "characterization",
        );
    }
    let font_system = &mut font.font_system;
    let em = params.font_size_px;
    let mut buffer = Buffer::new(font_system, Metrics::new(em, em * 1.2));
    buffer.set_size(font_system, None, None);
    let attrs = Attrs::new()
        .family(Family::Name(font.family.as_str()))
        .metrics(Metrics::new(em, em));
    buffer.set_text(font_system, text, &attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);

    let mut caches = super::GlyphCaches::new();
    let run = buffer.layout_runs().next().expect("one layout run");
    let layout = super::horizontal_run_layout(
        super::RunPen {
            params,
            inline_style_spans: None,
            layout_line_offsets: &[0],
            custom_kerning: &custom_kerning,
            font_size_px: em,
        },
        &run,
        font_system,
        &mut caches,
    );
    (layout.glyph_xs, layout.line_width_px)
}

/// The pen matrix: every text x mode x tracking x table x em of
/// `crate::characterization`, as `glyph_xs` plus `line_width_px` per row.
fn pen_characterization_matrix() -> Vec<crate::characterization::MatrixRow> {
    use crate::characterization::{
        MATRIX_EMS, MATRIX_MODES, MATRIX_TABLES, MATRIX_TEXTS, MATRIX_TRACKING, MatrixRow,
        TrackedFont, case_name,
    };

    let mut latin = TrackedFont::Latin.load();
    let mut hebrew = TrackedFont::Hebrew.load();
    let mut rows = Vec::new();
    for text in &MATRIX_TEXTS {
        let font = match text.font {
            TrackedFont::Latin => &mut latin,
            TrackedFont::Hebrew => &mut hebrew,
        };
        for (mode_label, mode) in MATRIX_MODES {
            for (tracking_label, kerning_px, kerning_percent) in MATRIX_TRACKING {
                for (table_label, pairs) in MATRIX_TABLES {
                    for em in MATRIX_EMS {
                        let mut params = base_params();
                        params.text = text.text.to_string();
                        params.font_size_px = em;
                        params.kerning_mode = mode;
                        params.kerning_px = kerning_px;
                        params.kerning_percent = kerning_percent;
                        let (xs, width) = tracked_font_run_layout(font, text.text, &params, pairs);
                        rows.push(MatrixRow {
                            case: case_name(text, mode_label, tracking_label, table_label, em),
                            fields: vec![("xs", xs), ("width", vec![width])],
                        });
                    }
                }
            }
        }
    }
    rows
}

/// The horizontal pen (`horizontal_run_layout`, all three modes, tracking, overrides,
/// both directions) must reproduce the recorded matrix BIT FOR BIT. A deliberate
/// behaviour change regenerates the fixture with
/// `MS_TEXT_RENDER_REGEN_CHARACTERIZATION=1` and names the rows it moved.
#[test]
fn pen_layout_matches_the_recorded_characterization() {
    crate::characterization::check_against_fixture(
        "pair_advance_pen_matrix.json",
        "ms-text-render pen characterization: pipeline::horizontal_run_layout glyph_xs and \
         line_width_px as f32 bit patterns (see src/characterization.rs)",
        &pen_characterization_matrix(),
    );
}

/// `KerningMode::Fixed` WITHOUT an override follows a right-to-left run like an override
/// does: leftwards, by the own advance of the glyph that SPANS the step (the visually
/// left one). The tracked Hebrew font has no kerning anywhere in this text, so the
/// un-kerned `Fixed` pen must land exactly on the shaped `Auto` pen.
#[test]
fn fixed_mode_walks_an_unoverridden_rtl_run_leftwards_on_the_spanning_advance() {
    use crate::characterization::TrackedFont;

    const RTL_TEXT: &str = "\u{05D0}\u{05D1}\u{05D2}";
    let mut hebrew = TrackedFont::Hebrew.load();
    let mut params = base_params();
    params.text = RTL_TEXT.to_string();
    params.font_size_px = 100.0;

    params.kerning_mode = KerningMode::Auto;
    let (auto_xs, auto_width) = tracked_font_run_layout(&mut hebrew, RTL_TEXT, &params, &[]);
    assert_eq!(auto_xs.len(), 3, "the fixture must shape three glyphs");
    assert!(
        auto_xs[2] < auto_xs[1] && auto_xs[1] < auto_xs[0],
        "precondition: cosmic-text lays this run out right-to-left; got {auto_xs:?}"
    );

    params.kerning_mode = KerningMode::Fixed;
    let (fixed_xs, fixed_width) = tracked_font_run_layout(&mut hebrew, RTL_TEXT, &params, &[]);
    // Equal up to float rounding: the hmtx advance and the shaped advance are the
    // same number reached through different scalings.
    for (fixed_x, auto_x) in fixed_xs.iter().zip(auto_xs.iter()) {
        assert!(
            (fixed_x - auto_x).abs() < 1e-3,
            "Fixed must walk leftwards on the spanning advance: {fixed_xs:?} vs {auto_xs:?}"
        );
    }
    assert!((fixed_width - auto_width).abs() < 1e-3);
}

/// Un-overridden `KerningMode::Optical` follows a right-to-left run too: leftwards, from
/// the spanning glyph's advance, with the ink gap measured between the sides that
/// actually FACE each other (`cur`'s right, `prev`'s left).
#[test]
fn optical_mode_walks_an_unoverridden_rtl_run_leftwards() {
    use crate::characterization::TrackedFont;

    const RTL_TEXT: &str = "\u{05D0}\u{05D1}\u{05D2}";
    let mut hebrew = TrackedFont::Hebrew.load();
    let mut params = base_params();
    params.text = RTL_TEXT.to_string();
    params.font_size_px = 100.0;
    params.kerning_mode = KerningMode::Auto;
    let (auto_xs, _) = tracked_font_run_layout(&mut hebrew, RTL_TEXT, &params, &[]);
    params.kerning_mode = KerningMode::Optical;
    let (optical_xs, _) = tracked_font_run_layout(&mut hebrew, RTL_TEXT, &params, &[]);
    assert_eq!(optical_xs.len(), 3);
    assert!(
        optical_xs[2] < optical_xs[1] && optical_xs[1] < optical_xs[0],
        "Optical must walk an RTL run leftwards; got {optical_xs:?}"
    );
    // The normalization nudges pairs; it never moves one by anything near a glyph
    // width (it is clamped well inside the em), so each step stays close to the
    // shaped one.
    for idx in 1..3 {
        let optical_step = optical_xs[idx] - optical_xs[idx - 1];
        let auto_step = auto_xs[idx] - auto_xs[idx - 1];
        assert!(
            (optical_step - auto_step).abs() < 25.0,
            "pair {idx}: optical step {optical_step} vs shaped {auto_step}"
        );
    }
}

/// Faux bold widens a right-to-left pair the way it widens a left-to-right one: the
/// growth is the spanning glyph's extra, signed onto the run axis, so every step gets
/// LONGER (more negative), never shorter.
#[test]
fn faux_bold_growth_widens_an_rtl_pair_like_an_ltr_pair() {
    use crate::characterization::TrackedFont;

    let step_growth = |font: TrackedFont, text: &str| -> f32 {
        let mut loaded = font.load();
        let mut params = base_params();
        params.text = text.to_string();
        params.font_size_px = 100.0;
        params.kerning_mode = KerningMode::Auto;
        let (plain_xs, _) = tracked_font_run_layout(&mut loaded, text, &params, &[]);
        // Forced, and not `outward_only`: the extra is then the same for every glyph,
        // so LTR and RTL growth are directly comparable.
        params.force_bold = true;
        params.faux_bold = Some(FauxBoldParams {
            outward_only: false,
            ..test_faux_bold(10.0, 5.0)
        });
        let (bold_xs, _) = tracked_font_run_layout(&mut loaded, text, &params, &[]);
        assert_eq!(plain_xs.len(), 2, "{text}: two glyphs");
        (bold_xs[1] - bold_xs[0]).abs() - (plain_xs[1] - plain_xs[0]).abs()
    };
    let ltr_growth = step_growth(TrackedFont::Latin, "HH");
    let rtl_growth = step_growth(TrackedFont::Hebrew, "\u{05D4}\u{05D4}");
    assert!(ltr_growth > 0.0, "precondition: faux bold widens an LTR step: {ltr_growth}");
    assert!(
        (rtl_growth - ltr_growth).abs() < 1e-3,
        "an RTL step must grow by the same faux extra as an LTR one: {rtl_growth} vs {ltr_growth}"
    );
}

/// The wrap's width of `text` (the same measurement the line breaker scores) with the
/// pen of `params` over `pairs` bound to `font`'s face.
fn wrap_line_width(
    font: &mut crate::characterization::LoadedFont,
    params: &TextRenderParams,
    pairs: &[(char, char, f32)],
    text: &str,
) -> f32 {
    use crate::font_provider::CustomKerningTable;
    use crate::font_registry::CustomKerningMap;
    use cosmic_text::{Attrs, Family};

    let mut map = CustomKerningMap::default();
    if !pairs.is_empty() {
        map.insert(
            font.face_id,
            Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
            "fixture",
        );
    }
    let attrs = Attrs::new().family(Family::Name(font.family.as_str()));
    let mut pen = super::PenRunWidth::new(params, &map, params.font_size_px);
    crate::wrap::measured_line_width_px_for_tests(
        &mut font.font_system,
        &attrs,
        params.font_size_px,
        &mut pen,
        text,
    )
}

/// THE WRAP MEASURES A LINE EXACTLY AS THE PEN DRAWS IT. Under every kerning mode
/// (`Auto`, `Fixed`, `Optical`), with manual tracking in px and in percent, with
/// user-authored pairs (including a ligature candidate `fi`, pairs the table does not
/// list, tightening and widening ones), with and without faux bold, left-to-right and
/// right-to-left, the width the line breaker scores
/// (`wrap::measured_line_width_px_for_tests`, the same code path) equals the pen's own
/// `line_width_px` — the extent the line is laid out and aligned with.
#[test]
fn the_wrap_width_equals_the_pen_width_for_every_mode_and_tracking() {
    use crate::characterization::TrackedFont;

    let cases: [(TrackedFont, &str); 6] = [
        (TrackedFont::Latin, "AVATAR WAY To"),
        (TrackedFont::Latin, "AVAVAVAVAV AVAVAVAVAV"),
        (TrackedFont::Latin, "office fit AV"),
        (TrackedFont::Latin, "To Wa Yo il"),
        (TrackedFont::Hebrew, "\u{05D0}\u{05D1}\u{05D2} \u{05D0}\u{05D1}"),
        (TrackedFont::Hebrew, "\u{05D0}\u{05D1}"),
    ];
    let tables: [&[(char, char, f32)]; 4] = [
        &[],
        &[('A', 'V', -120.0), ('T', 'o', -80.0), ('f', 'i', -300.0), ('\u{05D0}', '\u{05D1}', -150.0)],
        &[('A', 'V', 0.0), ('W', 'a', 90.0), ('\u{05D0}', '\u{05D1}', 0.0)],
        &[('A', 'V', 100.0), ('\u{05D0}', '\u{05D1}', 200.0)],
    ];
    for (font, text) in cases {
        let mut loaded = font.load();
        for mode in [KerningMode::Auto, KerningMode::Fixed, KerningMode::Optical] {
            for (kerning_px, kerning_percent) in [(0.0, 0.0), (3.5, 0.0), (0.0, -20.0), (6.0, 15.0)] {
                for pairs in tables {
                    for (em, faux) in [(36.0f32, false), (100.0, false), (100.0, true)] {
                        let mut params = base_params();
                        params.text = text.to_string();
                        params.font_size_px = em;
                        params.kerning_mode = mode;
                        params.kerning_px = kerning_px;
                        params.kerning_percent = kerning_percent;
                        // Faux bold grows the pen too; the wrap measures it.
                        params.force_bold = faux;
                        params.faux_bold = faux.then(|| test_faux_bold(10.0, 5.0));
                        let (xs, line_width_px) =
                            tracked_font_run_layout(&mut loaded, text, &params, pairs);
                        let wrap_px = wrap_line_width(&mut loaded, &params, pairs, text);
                        assert!(
                            (wrap_px - line_width_px).abs() < 1e-3,
                            "{text:?} {mode:?} tracking ({kerning_px}, {kerning_percent}%) em {em} \
                             faux {faux} table {pairs:?}: wrap {wrap_px} vs pen {line_width_px} \
                             (xs {xs:?})"
                        );
                    }
                }
            }
        }
    }
}

/// Manual tracking is a distance between letters in BOTH directions: positive tracking
/// spreads a right-to-left run (every leftward step grows by exactly the tracking px)
/// and negative tracking tightens it, as left-to-right.
#[test]
fn tracking_spreads_and_tightens_an_rtl_run_like_an_ltr_run() {
    use crate::characterization::TrackedFont;

    const RTL_TEXT: &str = "\u{05D0}\u{05D1}\u{05D2}";
    let mut hebrew = TrackedFont::Hebrew.load();
    let mut params = base_params();
    params.text = RTL_TEXT.to_string();
    params.font_size_px = 100.0;
    let (plain_xs, plain_width) = tracked_font_run_layout(&mut hebrew, RTL_TEXT, &params, &[]);
    for tracking in [5.0f32, -4.0] {
        params.kerning_px = tracking;
        let (xs, width) = tracked_font_run_layout(&mut hebrew, RTL_TEXT, &params, &[]);
        for idx in 1..xs.len() {
            let plain_step = plain_xs[idx - 1] - plain_xs[idx];
            let step = xs[idx - 1] - xs[idx];
            assert!(
                (step - plain_step - tracking).abs() < 1e-3,
                "tracking {tracking}: leftward step {idx} must grow by it: {xs:?} vs {plain_xs:?}"
            );
        }
        assert!(
            (width - plain_width - 2.0 * tracking).abs() < 1e-3,
            "tracking {tracking}: the line's extent grows by it per step: {width} vs {plain_width}"
        );
    }
}

/// A RIGHT-TO-LEFT LINE IS ALIGNED BY ITS REAL DRAWN EXTENT. When the pen changes an
/// RTL line's length (a tightening or widening override, `Fixed`, `Optical`, faux
/// bold, tracking), the change lands on the line's LEFT end; the left end must stay
/// on the shaper's left end (`x = 0` here), so that `line_width_px` (`max(x + w)`) is
/// the drawn extent. Then a left-aligned line's ink starts at the box's left edge, a
/// right-aligned one ends at its right edge, and the width moves by exactly the
/// authored delta (10 px per 100 per mille at em 100) — no gap left behind a
/// tightened line, no overflow past the left edge for a widened one.
#[test]
fn an_rtl_line_is_aligned_by_its_real_drawn_extent() {
    use crate::characterization::TrackedFont;
    use crate::types::HorizontalAlign;

    const TEXT: &str = "\u{05D0}\u{05D1}\u{05D2} \u{05D0}\u{05D1}";
    const BOX: u32 = 400;
    let mut hebrew = TrackedFont::Hebrew.load();
    let mut plain = base_params();
    plain.text = TEXT.to_string();
    plain.font_size_px = 100.0;
    let (plain_xs, plain_width) = tracked_font_run_layout(&mut hebrew, TEXT, &plain, &[]);
    let shaped_left = plain_xs.iter().copied().fold(f32::INFINITY, f32::min);
    assert!(shaped_left.abs() < 1e-3, "precondition: the shaper starts the line at 0");

    let tighten: &[(char, char, f32)] = &[('\u{05D0}', '\u{05D1}', -100.0)];
    let widen: &[(char, char, f32)] = &[('\u{05D0}', '\u{05D1}', 100.0)];
    let mut fixed = plain.clone();
    fixed.kerning_mode = KerningMode::Fixed;
    let mut optical = plain.clone();
    optical.kerning_mode = KerningMode::Optical;
    let mut faux = plain.clone();
    faux.force_bold = true;
    faux.faux_bold = Some(test_faux_bold(10.0, 5.0));
    let mut tracked = plain.clone();
    tracked.kerning_px = 5.0;
    let variants: [(&str, &TextRenderParams, &[crate::characterization::AuthoredPair]); 6] = [
        ("tighten", &plain, tighten),
        ("widen", &plain, widen),
        ("fixed + tighten", &fixed, tighten),
        ("optical", &optical, &[]),
        ("faux bold", &faux, &[]),
        ("tracking", &tracked, &[]),
    ];
    for (label, params, pairs) in variants {
        let (xs, width) = tracked_font_run_layout(&mut hebrew, TEXT, params, pairs);
        let left = xs.iter().copied().fold(f32::INFINITY, f32::min);
        assert!(
            (left - shaped_left).abs() < 1e-3,
            "{label}: the RTL line's left end must stay on the shaper's: {xs:?}"
        );
        let left_aligned = super::horizontal_line_offset(BOX, width, HorizontalAlign::LEFT) as f32;
        let right_aligned = super::horizontal_line_offset(BOX, width, HorizontalAlign::RIGHT) as f32;
        assert!((left_aligned + left).abs() < 1e-3, "{label}: left-aligned ink starts at the box edge");
        assert!(
            (right_aligned + width - BOX as f32).abs() <= 0.5,
            "{label}: right-aligned line ends at the box edge"
        );
    }
    // Exactly the authored change, twice (two alef -> bet pairs), on the width.
    let (_, tight_width) = tracked_font_run_layout(&mut hebrew, TEXT, &plain, tighten);
    let (_, wide_width) = tracked_font_run_layout(&mut hebrew, TEXT, &plain, widen);
    assert!((plain_width - tight_width - 20.0).abs() < 1e-3, "{tight_width} vs {plain_width}");
    assert!((wide_width - plain_width - 20.0).abs() < 1e-3, "{wide_width} vs {plain_width}");
}

/// Wraps `text` in a free whole-word box of `width_px` with the pen of `params` over
/// `pairs`; returns the laid-out lines.
fn wrap_free(
    font: &mut crate::characterization::LoadedFont,
    params: &TextRenderParams,
    pairs: &[(char, char, f32)],
    text: &str,
    width_px: f32,
) -> Vec<String> {
    use crate::font_provider::CustomKerningTable;
    use crate::font_registry::CustomKerningMap;
    use crate::types::{TextShape, TextWrapMode};
    use crate::wrap::{ShapeWrapRequest, reshape_text_for_shape};
    use cosmic_text::{Attrs, Family};

    let mut map = CustomKerningMap::default();
    if !pairs.is_empty() {
        map.insert(
            font.face_id,
            Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
            "fixture",
        );
    }
    let attrs = Attrs::new().family(Family::Name(font.family.as_str()));
    let mut pen = super::PenRunWidth::new(params, &map, params.font_size_px);
    reshape_text_for_shape(ShapeWrapRequest {
        text,
        font_system: &mut font.font_system,
        attrs: &attrs,
        run_width: &mut pen,
        font_size_px: params.font_size_px,
        line_height_px: params.font_size_px * 1.2,
        base_width_px: width_px,
        wrap_mode: TextWrapMode::WholeWords,
        hyphen_dicts: None,
        word_break_policy: None,
        shape: TextShape::Free,
        min_width_percent: 55.0,
        shape_variant: 5,
        allow_moderate_trees: false,
        hanging_punctuation: 0.0,
        preserve_edge_spaces: false,
    })
    .text
    .lines()
    .map(str::to_string)
    .collect()
}

/// THE BREAK FOLLOWS THE DRAWN WIDTH. For a setting that makes the drawn line LONGER
/// (px / percent tracking, `Fixed` on a font that kerns its pairs tighter) and one that
/// makes it SHORTER (a tightening override, negative tracking), a box between the
/// plain and the drawn one-line width must break the text exactly when the DRAWN line
/// does not fit: the per-unit capacity estimate the candidates are cut by is measured
/// by the same pen, so a fitting line is offered and a non-fitting one is not.
#[test]
fn a_line_breaks_where_its_drawn_width_says_it_does() {
    use crate::characterization::TrackedFont;

    const TEXT: &str = "AVAVAVAVAV AVAVAVAVAV";
    let mut latin = TrackedFont::Latin.load();
    let mut plain = base_params();
    plain.text = TEXT.to_string();
    plain.font_size_px = 48.0;
    let plain_px = wrap_line_width(&mut latin, &plain, &[], TEXT);

    let mut tracked_px = plain.clone();
    tracked_px.kerning_px = 4.0;
    let mut tracked_percent = plain.clone();
    tracked_percent.kerning_percent = 12.0;
    let mut fixed = plain.clone();
    fixed.kerning_mode = KerningMode::Fixed;
    let mut negative = plain.clone();
    negative.kerning_px = -3.0;
    /// One setting under test: label, params and the override pairs.
    type Variant<'v> = (&'v str, &'v TextRenderParams, &'v [crate::characterization::AuthoredPair]);
    let tighten: &[(char, char, f32)] = &[('A', 'V', -120.0)];
    let variants: [Variant<'_>; 5] = [
        ("tracking px", &tracked_px, &[]),
        ("tracking percent", &tracked_percent, &[]),
        ("fixed", &fixed, &[]),
        ("negative tracking", &negative, &[]),
        ("override", &plain, tighten),
    ];
    for (label, params, pairs) in variants {
        let drawn_px = wrap_line_width(&mut latin, params, pairs, TEXT);
        assert!(
            (drawn_px - plain_px).abs() > 10.0,
            "{label}: precondition, the setting must change the line noticeably: \
             {drawn_px} vs {plain_px}"
        );
        let between = (drawn_px + plain_px) * 0.5;
        let drawn_lines = wrap_free(&mut latin, params, pairs, TEXT, between);
        let plain_lines = wrap_free(&mut latin, &plain, &[], TEXT, between);
        let (expected_drawn, expected_plain) = if drawn_px < plain_px { (1, 2) } else { (2, 1) };
        assert_eq!(drawn_lines.len(), expected_drawn, "{label}: {drawn_lines:?} at {between}");
        assert_eq!(plain_lines.len(), expected_plain, "{label} control: {plain_lines:?}");
        // And the drawn text always fits a box as wide as its own drawn line.
        assert_eq!(
            wrap_free(&mut latin, params, pairs, TEXT, drawn_px + 1.0).len(),
            1,
            "{label}: a box as wide as the drawn line holds it"
        );
    }
}

/// THE FORM METRIC SUMS TO THE PEN'S WIDTH. `forms::GlyphWidths` (per-character
/// advances + per-pair kerns at 1000 units per em) built with the block's
/// `MetricKerning` must measure a line within one unit per glyph of the pen's
/// `line_width_px` at em 1000 (each pair kern and advance is rounded once), under
/// `Auto` and `Fixed`, with tracking in px and in percent, with and without
/// user-authored pairs. (`Optical` is measured like `Auto` by contract, and a
/// right-to-left line's pen width is pending its own fix, so neither is pinned here.)
#[test]
fn the_form_metric_sums_to_the_pen_width_under_fixed_and_tracking() {
    use crate::characterization::TrackedFont;
    use crate::font_provider::CustomKerningTable;
    use crate::font_registry::CustomKerningMap;
    use crate::wrap::forms::{
        DEFAULT_WIDTH_TOLERANCE, GlyphWidths, InlineTagScope, LineWidthMetric, MetricKerning,
    };
    use cosmic_text::{Attrs, Family, Metrics};

    const EM: f32 = 1000.0;
    let texts = ["AVATAR WAY To", "AVAVAVAVAV AVAVAVAVAV", "To Wa Yo il"];
    let tables: [&[(char, char, f32)]; 2] = [&[], &[('A', 'V', -120.0), ('T', 'o', -80.0)]];
    let mut latin = TrackedFont::Latin.load();
    for text in texts {
        for mode in [KerningMode::Auto, KerningMode::Fixed] {
            for (kerning_px, kerning_percent) in [(0.0, 0.0), (35.0, 0.0), (0.0, -20.0), (60.0, 15.0)] {
                for pairs in tables {
                    let mut params = base_params();
                    params.text = text.to_string();
                    params.font_size_px = EM;
                    params.kerning_mode = mode;
                    params.kerning_px = kerning_px;
                    params.kerning_percent = kerning_percent;
                    let (xs, pen_width) = tracked_font_run_layout(&mut latin, text, &params, pairs);

                    let mut map = CustomKerningMap::default();
                    if !pairs.is_empty() {
                        map.insert(
                            latin.face_id,
                            Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
                            "fixture",
                        );
                    }
                    let attrs = Attrs::new()
                        .family(Family::Name(latin.family.as_str()))
                        .metrics(Metrics::new(EM, EM));
                    let metric = GlyphWidths::build(
                        &mut latin.font_system,
                        &attrs,
                        text,
                        0.0,
                        DEFAULT_WIDTH_TOLERANCE,
                        InlineTagScope::NoBreakOnly,
                        MetricKerning::new(&map, mode, kerning_px, kerning_percent, EM, 100.0),
                    );
                    let metric_width = metric.line_width(text) as f32;
                    let glyphs = xs.len() as f32;
                    assert!(
                        (metric_width - pen_width).abs() <= glyphs,
                        "{text:?} {mode:?} tracking ({kerning_px}, {kerning_percent}%) table \
                         {pairs:?}: metric {metric_width} vs pen {pen_width}"
                    );
                }
            }
        }
    }
}

/// The form metric sums to the pen's width on a RIGHT-TO-LEFT line too, now that an
/// RTL line's width is its drawn extent: `Auto` and `Fixed`, with tightening and
/// widening overrides, and with manual tracking in px and percent (signed onto the run
/// axis by the pen, added to the step's magnitude by the metric).
#[test]
fn the_form_metric_sums_to_the_pen_width_on_an_rtl_line() {
    use crate::characterization::TrackedFont;
    use crate::font_provider::CustomKerningTable;
    use crate::font_registry::CustomKerningMap;
    use crate::wrap::forms::{
        DEFAULT_WIDTH_TOLERANCE, GlyphWidths, InlineTagScope, LineWidthMetric, MetricKerning,
    };
    use cosmic_text::{Attrs, Family, Metrics};

    const EM: f32 = 1000.0;
    const TEXT: &str = "\u{05D0}\u{05D1}\u{05D2} \u{05D0}\u{05D1}";
    let tables: [&[(char, char, f32)]; 3] = [
        &[],
        &[('\u{05D0}', '\u{05D1}', -150.0)],
        &[('\u{05D0}', '\u{05D1}', 200.0)],
    ];
    let mut hebrew = TrackedFont::Hebrew.load();
    for mode in [KerningMode::Auto, KerningMode::Fixed] {
        for (kerning_px, kerning_percent) in [(0.0, 0.0), (35.0, 0.0), (0.0, -20.0), (60.0, 15.0)] {
        for pairs in tables {
            let mut params = base_params();
            params.text = TEXT.to_string();
            params.font_size_px = EM;
            params.kerning_mode = mode;
            params.kerning_px = kerning_px;
            params.kerning_percent = kerning_percent;
            let (xs, pen_width) = tracked_font_run_layout(&mut hebrew, TEXT, &params, pairs);
            let mut map = CustomKerningMap::default();
            if !pairs.is_empty() {
                map.insert(
                    hebrew.face_id,
                    Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())),
                    "fixture",
                );
            }
            let attrs = Attrs::new()
                .family(Family::Name(hebrew.family.as_str()))
                .metrics(Metrics::new(EM, EM));
            let metric = GlyphWidths::build(
                &mut hebrew.font_system,
                &attrs,
                TEXT,
                0.0,
                DEFAULT_WIDTH_TOLERANCE,
                InlineTagScope::NoBreakOnly,
                MetricKerning::new(&map, mode, kerning_px, kerning_percent, EM, 100.0),
            );
            let metric_width = metric.line_width(TEXT) as f32;
            assert!(
                (metric_width - pen_width).abs() <= xs.len() as f32,
                "{mode:?} tracking ({kerning_px}, {kerning_percent}%) table {pairs:?}: metric \
                 {metric_width} vs pen {pen_width} (xs {xs:?})"
            );
        }
        }
    }
}

/// The pen's tracking basis is `|shaped step|.max(prev.w.max(em / 2))`: a narrow left
/// glyph is FLOORED at half the em. `i` at em 100 is far narrower than 50 px, so a
/// 50 % tracking grows the `i -> l` step by exactly `0.5 * 50 = 25` px (not by half
/// of `i`'s own advance). Pinned because the formula and vertical paths use different
/// bases (`TrackingBasis`, behaviour change B3 declined: they stay different).
#[test]
fn tracking_basis_is_floored_at_half_the_em_on_the_pen() {
    use crate::characterization::TrackedFont;

    let mut latin = TrackedFont::Latin.load();
    let mut params = base_params();
    params.text = "il".to_string();
    params.font_size_px = 100.0;
    params.kerning_mode = KerningMode::Auto;

    let (plain_xs, _) = tracked_font_run_layout(&mut latin, "il", &params, &[]);
    params.kerning_percent = 50.0;
    let (tracked_xs, _) = tracked_font_run_layout(&mut latin, "il", &params, &[]);
    assert_eq!(plain_xs.len(), 2);
    let plain_step = plain_xs[1] - plain_xs[0];
    let tracked_step = tracked_xs[1] - tracked_xs[0];
    assert!(
        plain_step < 50.0,
        "precondition: `i` must be narrower than half the em; step {plain_step}"
    );
    assert!(
        (tracked_step - plain_step - 25.0).abs() < 1e-3,
        "50 % tracking must add 0.5 * max(step, i.w, em/2) = 25 px; got {}",
        tracked_step - plain_step
    );
}

