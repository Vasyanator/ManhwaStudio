/*
File: crates/ms-custom-fonts/src/hybrid/tests.rs

Purpose:
End-to-end tests of the hybrid compiler on the tracked bundled fonts (`fonts/ui`), addressed
from `CARGO_MANIFEST_DIR`. A test whose font is absent logs one line and returns.

Covers: deterministic bytes (repeat, 1 vs N threads), output parses and maps every included
character, `Disable` / `NoInfluence`, single-source reproduction, near-1:0 weights, kerning
inheritance rules, preview naming, cancel, source matching errors, loading + rendering
through `ms-text-render`, baked own kerning (`with_kerning_overrides`) reaching the shaper,
property weights (split equal to plain = same glyph tables; shapes and spacing from different
sources) and the algorithm dispatch (`Sdf`, `NoBlend`, `Skeleton` / `Transfer` path counts,
`BlendStats` sums).
*/

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kurbo::Point;

use crate::outline::{Outline, OutlineCollector, flatten_contour};
use crate::{
    BlendAlgorithm, CompileError, CompileScope, CompiledFont, CustomFontKind, CustomFontRecipe, HybridParams, HybridSource,
    KerningPairPerMille, MissingGlyphPolicy, PropertyWeights, SourceFont, compile, coverage_report, included_chars,
    with_kerning_overrides,
};

const NOTO_REGULAR: &str = "core/00-NotoSans-Regular.ttf";
const NOTO_BOLD: &str = "bold/00-NotoSans-Bold.ttf";
const SOURCE_HAN: &str = "core/01-SourceHanSansK-Regular.otf";

/// Reads a bundled `fonts/ui` file, or logs and returns `None` when it is absent.
fn bundled(relative: &str) -> Option<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts/ui").join(relative);
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            eprintln!("skipping: bundled font {} is unavailable ({error})", path.display());
            None
        }
    }
}

fn recipe(sources: &[(&str, f32, Option<MissingGlyphPolicy>)], missing_default: MissingGlyphPolicy) -> CustomFontRecipe {
    CustomFontRecipe {
        kind: CustomFontKind::Hybrid(HybridParams {
            sources: sources
                .iter()
                .map(|&(identity, weight, missing_override)| HybridSource { identity: identity.to_owned(), weight, missing_override })
                .collect(),
            missing_default,
            algorithm: BlendAlgorithm::Auto,
            property_weights: None,
        }),
    }
}

fn source<'a>(identity: &'a str, data: &'a [u8], user_kerning: &'a [KerningPairPerMille]) -> SourceFont<'a> {
    SourceFont { identity, data, face_index: 0, user_kerning }
}

fn run(recipe: &CustomFontRecipe, sources: &[SourceFont<'_>], scope: CompileScope<'_>) -> CompiledFont {
    compile(recipe, sources, scope, &|_, _| {}, &AtomicBool::new(false)).expect("compiles")
}

fn chars(text: &str) -> BTreeSet<char> {
    text.chars().collect()
}

/// Outline of `ch` in a face (normalized to 1000 units per em).
fn outline_of(data: &[u8], ch: char) -> Option<Outline> {
    let face = ttf_parser::Face::parse(data, 0).ok()?;
    let glyph = face.glyph_index(ch).filter(|glyph| glyph.0 != 0)?;
    let mut collector = OutlineCollector::new(1000.0 / f64::from(face.units_per_em()));
    face.outline_glyph(glyph, &mut collector)?;
    Some(collector.finish())
}

fn polygons(outline: &Outline) -> Vec<Vec<Point>> {
    outline
        .iter()
        .map(|contour| {
            let mut polygon = Vec::new();
            flatten_contour(contour, 0.2, &mut polygon);
            polygon
        })
        .collect()
}

fn distance_to_polygons(point: Point, polygons: &[Vec<Point>]) -> f64 {
    let mut best = f64::INFINITY;
    for polygon in polygons {
        for index in 0..polygon.len() {
            let a = polygon[index];
            let b = polygon[(index + 1) % polygon.len()];
            let ab = b - a;
            let t = if ab.hypot2() > 0.0 { ((point - a).dot(ab) / ab.hypot2()).clamp(0.0, 1.0) } else { 0.0 };
            best = best.min(point.distance(a + ab * t));
        }
    }
    best
}

/// Symmetric Hausdorff distance between two outlines' boundaries, font units. Only meaningful
/// for outlines without overlapping contours (a source's overlaps have interior boundaries
/// that a blended union does not); see `fill_mismatch` for the general comparison.
fn hausdorff(a: &Outline, b: &Outline) -> f64 {
    let (pa, pb) = (polygons(a), polygons(b));
    let one_way = |from: &[Vec<Point>], to: &[Vec<Point>]| {
        from.iter().flatten().map(|&point| distance_to_polygons(point, to)).fold(0.0, f64::max)
    };
    one_way(&pa, &pb).max(one_way(&pb, &pa))
}

/// Nonzero-winding inside test of a point against polygons.
fn inside(point: Point, polygons: &[Vec<Point>]) -> bool {
    let mut winding = 0;
    for polygon in polygons {
        for index in 0..polygon.len() {
            let a = polygon[index];
            let b = polygon[(index + 1) % polygon.len()];
            if (a.y <= point.y) != (b.y <= point.y) {
                let x = a.x + (point.y - a.y) * (b.x - a.x) / (b.y - a.y);
                if x < point.x {
                    winding += if b.y > a.y { 1 } else { -1 };
                }
            }
        }
    }
    winding != 0
}

/// Fraction of a 4-unit sample lattice over both outlines' extent whose inside/outside state
/// differs, ignoring samples within `band` units of `b`'s boundary (quantization/rounding).
fn fill_mismatch(a: &Outline, b: &Outline, band: f64) -> f64 {
    let (pa, pb) = (polygons(a), polygons(b));
    let all: Vec<Point> = pa.iter().chain(&pb).flatten().copied().collect();
    let (x0, y0, x1, y1) = all.iter().fold((f64::MAX, f64::MAX, f64::MIN, f64::MIN), |acc, point| {
        (acc.0.min(point.x), acc.1.min(point.y), acc.2.max(point.x), acc.3.max(point.y))
    });
    let (mut differ, mut total) = (0_u32, 0_u32);
    let mut y = y0;
    while y <= y1 {
        let mut x = x0;
        while x <= x1 {
            let point = Point::new(x, y);
            if distance_to_polygons(point, &pb) > band {
                total += 1;
                if inside(point, &pa) != inside(point, &pb) {
                    differ += 1;
                }
            }
            x += 4.0;
        }
        y += 4.0;
    }
    f64::from(differ) / f64::from(total.max(1))
}

#[test]
fn bytes_are_deterministic_across_runs_and_thread_counts() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    let recipe = recipe(&[("Noto", 0.5, None), ("Han", 0.5, None)], MissingGlyphPolicy::NoInfluence);
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    let set = chars("AaBgQ@&8ЖжЯ한글 ");
    let one = rayon::ThreadPoolBuilder::new().num_threads(1).build().expect("pool");
    let many = rayon::ThreadPoolBuilder::new().num_threads(4).build().expect("pool");
    let first = one.install(|| run(&recipe, &sources, CompileScope::Subset(&set)));
    let second = many.install(|| run(&recipe, &sources, CompileScope::Subset(&set)));
    let third = many.install(|| run(&recipe, &sources, CompileScope::Subset(&set)));
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(second.bytes, third.bytes);
}

#[test]
fn output_parses_and_maps_every_included_char() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    let recipe = recipe(&[("Noto", 0.6, None), ("Han", 0.4, None)], MissingGlyphPolicy::Disable);
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    // '한' is missing in Noto Sans -> disabled by the default policy.
    let set = chars("Hello, мир! 한");
    let compiled = run(&recipe, &sources, CompileScope::Subset(&set));
    let face = ttf_parser::Face::parse(&compiled.bytes, 0).expect("the output is a valid font");
    assert_eq!(face.units_per_em(), 1000);
    for ch in "Hello, мир!".chars() {
        let glyph = face.glyph_index(ch).expect("included char is mapped");
        assert_ne!(glyph.0, 0);
        if !ch.is_whitespace() {
            assert!(outline_of(&compiled.bytes, ch).is_some_and(|outline| !outline.is_empty()), "{ch} has ink");
        }
        assert!(face.glyph_hor_advance(glyph).unwrap_or(0) > 0, "{ch} has an advance");
    }
    assert!(face.glyph_index('한').is_none(), "Disable omits the character");
    assert_eq!(compiled.report.disabled, 1);
    assert_eq!(compiled.report.disabled_sample, vec!['한']);
    assert!(compiled.postscript_name.starts_with("MSCustomPreview-"));
    let family = face
        .names()
        .into_iter()
        .find(|name| name.name_id == ttf_parser::name_id::FAMILY && name.is_unicode())
        .and_then(|name| name.to_string());
    assert_eq!(family.as_deref(), Some(compiled.family_name.as_str()));
}

#[test]
fn no_influence_copies_the_only_source_and_full_names_differ_from_preview() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    let recipe = recipe(&[("Noto", 0.5, None), ("Han", 0.5, None)], MissingGlyphPolicy::NoInfluence);
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    let set = chars("한");
    let compiled = run(&recipe, &sources, CompileScope::Subset(&set));
    let hybrid = outline_of(&compiled.bytes, '한').expect("kept by NoInfluence");
    let original = outline_of(&han, '한').expect("Source Han has it");
    let distance = hausdorff(&hybrid, &original);
    assert!(distance < 1.5, "single-source glyph reproduced within quantization, got {distance}");
    let face = ttf_parser::Face::parse(&compiled.bytes, 0).expect("valid");
    let han_face = ttf_parser::Face::parse(&han, 0).expect("valid");
    let advance = |face: &ttf_parser::Face<'_>| face.glyph_index('한').and_then(|glyph| face.glyph_hor_advance(glyph));
    assert_eq!(advance(&face), advance(&han_face));

    // A per-source override wins over the default.
    let overridden = crate::hybrid::tests::recipe(
        &[("Noto", 0.5, Some(MissingGlyphPolicy::Disable)), ("Han", 0.5, None)],
        MissingGlyphPolicy::NoInfluence,
    );
    let disabled = run(&overridden, &sources, CompileScope::Subset(&set));
    assert_eq!(disabled.report.disabled, 1);
    assert_ne!(disabled.key, compiled.key);
    assert_ne!(disabled.postscript_name, compiled.postscript_name);
}

#[test]
fn near_zero_weight_approximates_the_dominant_source() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    let recipe = recipe(&[("Noto", 1.0, None), ("Han", 0.0001, None)], MissingGlyphPolicy::NoInfluence);
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    let set = chars("agЖ8");
    let compiled = run(&recipe, &sources, CompileScope::Subset(&set));
    for ch in set {
        let hybrid = outline_of(&compiled.bytes, ch).expect("compiled");
        let original = outline_of(&regular, ch).expect("Noto has it");
        // Noto draws some letters with overlapping contours; the blend is their union, so the
        // filled regions are compared, plus the one-way boundary distance hybrid -> source.
        let mismatch = fill_mismatch(&hybrid, &original, 3.0);
        assert!(mismatch < 0.002, "{ch}: filled area differs on {mismatch} of the samples");
        let near = polygons(&hybrid).iter().flatten().map(|&point| distance_to_polygons(point, &polygons(&original))).fold(0.0, f64::max);
        assert!(near < 3.0, "{ch}: hybrid boundary within 3 units of the source, got {near}");
    }
}

#[test]
fn blend_lies_between_the_sources() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let recipe = recipe(&[("Regular", 0.5, None), ("Bold", 0.5, None)], MissingGlyphPolicy::Disable);
    let sources = [source("Regular", &regular, &[]), source("Bold", &bold, &[])];
    let set = chars("lo");
    let compiled = run(&recipe, &sources, CompileScope::Subset(&set));
    let face = ttf_parser::Face::parse(&compiled.bytes, 0).expect("valid");
    let regular_face = ttf_parser::Face::parse(&regular, 0).expect("valid");
    let bold_face = ttf_parser::Face::parse(&bold, 0).expect("valid");
    let width = |face: &ttf_parser::Face<'_>| {
        let glyph = face.glyph_index('l').expect("mapped");
        let rect = face.glyph_bounding_box(glyph).expect("ink");
        i32::from(rect.x_max) - i32::from(rect.x_min)
    };
    let (thin, thick, mixed) = (width(&regular_face), width(&bold_face), width(&face));
    assert!(thin < mixed && mixed < thick, "stem width {mixed} lies between {thin} and {thick}");
}

#[test]
fn kerning_inherits_by_weight_with_absent_pairs_as_zero() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    // User pairs on characters with no built-in kerning between them: '1' and '2'.
    let regular_pairs = [
        KerningPairPerMille { left: '1', right: '2', em: -100.0 },
        KerningPairPerMille { left: '2', right: '1', em: 40.0 },
    ];
    let bold_pairs = [KerningPairPerMille { left: '2', right: '1', em: 80.0 }];
    // Built-in 'A''V' cancelled by a user 0 in both sources.
    let cancel_regular = [KerningPairPerMille { left: 'A', right: 'V', em: 0.0 }];
    let recipe = recipe(&[("Regular", 3.0, None), ("Bold", 1.0, None)], MissingGlyphPolicy::Disable);
    let set = chars("12AV3");

    let with_pairs = [source("Regular", &regular, &regular_pairs), source("Bold", &bold, &bold_pairs)];
    let compiled = run(&recipe, &with_pairs, CompileScope::Subset(&set));
    let kern = |bytes: &[u8], left: char, right: char| -> Option<i16> {
        let face = ttf_parser::Face::parse(bytes, 0).expect("valid");
        let (l, r) = (face.glyph_index(left)?, face.glyph_index(right)?);
        face.tables().kern?.subtables.into_iter().find_map(|subtable| subtable.glyphs_kerning(l, r))
    };
    // 0.75 * -100 + 0.25 * 0 (Bold lacks the pair: default spacing).
    assert_eq!(kern(&compiled.bytes, '1', '2'), Some(-75));
    // 0.75 * 40 + 0.25 * 80.
    assert_eq!(kern(&compiled.bytes, '2', '1'), Some(50));
    // In no source.
    assert_eq!(kern(&compiled.bytes, '3', '3'), None);
    let built_in = kern(&compiled.bytes, 'A', 'V');
    assert!(built_in.is_some_and(|value| value < 0), "built-in A/V is inherited, got {built_in:?}");

    let cancelled = [source("Regular", &regular, &cancel_regular), source("Bold", &bold, &cancel_regular)];
    let compiled = run(&recipe, &cancelled, CompileScope::Subset(&set));
    assert_eq!(kern(&compiled.bytes, 'A', 'V'), None, "a user 0 overrides the built-in pair in both sources");
}

#[test]
fn coverage_report_counts_the_full_union() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let recipe = recipe(&[("Regular", 1.0, None), ("Bold", 1.0, None)], MissingGlyphPolicy::Disable);
    let sources = [source("Regular", &regular, &[]), source("Bold", &bold, &[])];
    let report = coverage_report(&recipe, &sources).expect("readable");
    assert!(report.total_chars > 1000);
    assert!(report.disabled <= report.missing_in_some);
    assert_eq!(report.disabled_sample.len(), report.disabled.min(64));
}

#[test]
fn included_chars_follows_the_missing_glyph_rule_and_matches_the_compiled_cmap() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    // '한' is missing in Noto Sans; U+10FFFD (plane-16 private use) is in neither source.
    let candidates = chars("Aж한\u{10FFFD}");

    let disable = recipe(&[("Noto", 1.0, None), ("Han", 1.0, None)], MissingGlyphPolicy::Disable);
    let included = included_chars(&disable, &sources, &candidates).expect("readable");
    assert_eq!(included, chars("Aж"), "Disable drops the partial char; a char no source has is never included");
    let compiled = run(&disable, &sources, CompileScope::Subset(&candidates));
    let face = ttf_parser::Face::parse(&compiled.bytes, 0).expect("valid");
    let mapped: BTreeSet<char> = candidates.iter().copied().filter(|&ch| face.glyph_index(ch).is_some()).collect();
    assert_eq!(mapped, included, "the answer is the compiled cmap over the same candidates");

    let no_influence = recipe(&[("Noto", 1.0, None), ("Han", 1.0, None)], MissingGlyphPolicy::NoInfluence);
    assert_eq!(included_chars(&no_influence, &sources, &candidates).expect("readable"), chars("Aж한"));
    // The per-source override of the source that LACKS the char decides, not the default.
    let overridden = recipe(&[("Noto", 1.0, Some(MissingGlyphPolicy::Disable)), ("Han", 1.0, None)], MissingGlyphPolicy::NoInfluence);
    assert_eq!(included_chars(&overridden, &sources, &candidates).expect("readable"), chars("Aж"));
    let other_override = recipe(&[("Noto", 1.0, None), ("Han", 1.0, Some(MissingGlyphPolicy::Disable))], MissingGlyphPolicy::NoInfluence);
    assert_eq!(included_chars(&other_override, &sources, &candidates).expect("readable"), chars("Aж한"));

    assert!(included_chars(&disable, &sources, &BTreeSet::new()).expect("readable").is_empty());
    let only_one = [source("Noto", &regular, &[])];
    assert!(matches!(included_chars(&disable, &only_one, &candidates), Err(CompileError::SourceMissing { .. })));
}

#[test]
fn sources_must_line_up_with_the_recipe_and_cancel_stops() {
    let Some(regular) = bundled(NOTO_REGULAR) else {
        return;
    };
    let recipe = recipe(&[("A", 1.0, None), ("B", 1.0, None)], MissingGlyphPolicy::Disable);
    let set = chars("a");
    let never = AtomicBool::new(false);
    let only_one = [source("A", &regular, &[])];
    assert!(matches!(
        compile(&recipe, &only_one, CompileScope::Subset(&set), &|_, _| {}, &never),
        Err(CompileError::SourceMissing { identity }) if identity == "B"
    ));
    let swapped = [source("B", &regular, &[]), source("A", &regular, &[])];
    assert!(matches!(
        compile(&recipe, &swapped, CompileScope::Subset(&set), &|_, _| {}, &never),
        Err(CompileError::SourceMismatch { index: 0, .. })
    ));
    let garbage = [source("A", &regular, &[]), source("B", b"not a font", &[])];
    assert!(matches!(
        compile(&recipe, &garbage, CompileScope::Subset(&set), &|_, _| {}, &never),
        Err(CompileError::SourceUnparsable { .. })
    ));
    let bad_pair = [KerningPairPerMille { left: 'a', right: 'b', em: f32::NAN }];
    let with_nan = [source("A", &regular, &bad_pair), source("B", &regular, &[])];
    assert!(matches!(
        compile(&recipe, &with_nan, CompileScope::Subset(&set), &|_, _| {}, &never),
        Err(CompileError::InvalidUserKerning { .. })
    ));
    let ok = [source("A", &regular, &[]), source("B", &regular, &[])];
    let cancelled = AtomicBool::new(true);
    assert!(matches!(compile(&recipe, &ok, CompileScope::Subset(&set), &|_, _| {}, &cancelled), Err(CompileError::Cancelled)));
}

/// Renders `text` with the compiled face through the production renderer.
fn render(compiled: &CompiledFont, text: &str) -> ms_text_render::types::RenderedTextImage {
    use ms_text_render::{FontContent, FontContentSet, font_content_id, render_text_to_image};
    let content = FontContent {
        name: compiled.postscript_name.clone(),
        original_name: compiled.family_name.clone(),
        content_id: font_content_id(&compiled.bytes),
        data: Arc::new(compiled.bytes.clone()),
        face_index: 0,
        custom_kerning: None,
    };
    let provider = FontContentSet::new(vec![content]);
    render_text_to_image(&render_params(text, &compiled.postscript_name), &provider, None).expect("renders")
}

/// Plain horizontal render parameters (the gallery's baseline) for `font`.
fn render_params(text: &str, font: &str) -> ms_text_render::types::TextRenderParams {
    use ms_text_render::types::{
        AntiAliasingMode, HorizontalAlign, KerningMode, LinePlacementReference, RenderExtraInfoRequest,
        TextDrawnLinesLayoutParams, TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams, TextShape,
        TextVectorLinesLayoutParams, TextWrapMode, VerticalLineDirection,
    };
    TextRenderParams {
        text: text.to_owned(),
        text_color: [0, 0, 0, 255],
        font_name: font.to_owned(),
        font_size_px: 40.0,
        line_spacing_px: 0.0,
        line_spacing_percent: 20.0,
        kerning_mode: KerningMode::Auto,
        kerning_px: 0.0,
        kerning_percent: 0.0,
        glyph_height_percent: 100.0,
        glyph_width_percent: 100.0,
        width_px: 2000,
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
        anti_aliasing: AntiAliasingMode::Strong,
        global_rotation_deg: 0.0,
        line_placement_percent: 0.0,
        line_placement_reference: LinePlacementReference::GlyphHeight,
        raster_transform: None,
        extra_info: RenderExtraInfoRequest::default(),
    }
}

#[test]
fn compiled_face_loads_and_renders_without_fallback() {
    use ms_text_render::{FontContent, font_content_id};

    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    let text = "Hybrid мир 한글";
    let recipe = recipe(&[("Noto", 0.5, None), ("Han", 0.5, None)], MissingGlyphPolicy::NoInfluence);
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    let set = chars(text);
    let compiled = run(&recipe, &sources, CompileScope::Subset(&set));

    let content = FontContent {
        name: compiled.postscript_name.clone(),
        original_name: compiled.family_name.clone(),
        content_id: font_content_id(&compiled.bytes),
        data: Arc::new(compiled.bytes.clone()),
        face_index: 0,
        custom_kerning: None,
    };
    let mut system = ms_text_render::new_render_font_system();
    let mut cache = ms_text_render::FontFaceCache::new();
    let registered = ms_text_render::load_font_content(&mut system, &mut cache, &content, 0).expect("fontdb loads the face");
    assert_eq!(registered.family_name.as_deref(), Some(compiled.family_name.as_str()));

    let image = render(&compiled, text);
    assert!(image.width > 100 && image.height > 10);
    assert!(image.rgba.chunks(4).any(|pixel| pixel[3] > 0), "ink was drawn");
    assert!(image.font_fallbacks.is_empty(), "every char drawn by the hybrid: {:?}", image.font_fallbacks);
}

#[test]
fn the_shaper_applies_the_inherited_kern_table() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let recipe = recipe(&[("Regular", 0.5, None), ("Bold", 0.5, None)], MissingGlyphPolicy::Disable);
    let set = chars("AV");
    let kerned = run(&recipe, &[source("Regular", &regular, &[]), source("Bold", &bold, &[])], CompileScope::Subset(&set));
    let cancel = [KerningPairPerMille { left: 'A', right: 'V', em: 0.0 }, KerningPairPerMille { left: 'V', right: 'A', em: 0.0 }];
    let plain = run(&recipe, &[source("Regular", &regular, &cancel), source("Bold", &bold, &cancel)], CompileScope::Subset(&set));
    let text = "AVAVAVAVAV";
    let (kerned_width, plain_width) = (render(&kerned, text).width, render(&plain, text).width);
    // Nine A/V pairs of about -30 units at 40 px: roughly a dozen pixels narrower.
    assert!(kerned_width + 5 < plain_width, "kern pairs tighten the run: {kerned_width} vs {plain_width}");
}

#[test]
fn baked_own_kerning_reaches_the_shaper_and_keeps_the_names() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let set = chars("AV");
    let cancel = [KerningPairPerMille { left: 'A', right: 'V', em: 0.0 }, KerningPairPerMille { left: 'V', right: 'A', em: 0.0 }];
    let tighter_pairs = [KerningPairPerMille { left: 'A', right: 'V', em: -150.0 }, KerningPairPerMille { left: 'V', right: 'A', em: -150.0 }];
    // The renderer's font pool is process-wide and resolves a face by its family name, so every
    // face rendered here has a name no other render in this process uses: each case gets its
    // own recipe weights, and of a rewritten face only the rewritten bytes are rendered.
    let case = |weight: f32, overrides: &[KerningPairPerMille]| -> (CompiledFont, CompiledFont) {
        let recipe = recipe(&[("Regular", weight, None), ("Bold", 1.0 - weight, None)], MissingGlyphPolicy::Disable);
        let kerned = run(&recipe, &[source("Regular", &regular, &[]), source("Bold", &bold, &[])], CompileScope::Subset(&set));
        let plain = run(&recipe, &[source("Regular", &regular, &cancel), source("Bold", &bold, &cancel)], CompileScope::Subset(&set));
        assert_eq!(with_kerning_overrides(&kerned.bytes, &[]).expect("rewrites"), kerned.bytes, "no overrides: same file");
        let rewritten = CompiledFont { bytes: with_kerning_overrides(&kerned.bytes, overrides).expect("rewrites"), ..kerned };
        (rewritten, plain)
    };
    let text = "AVAVAVAVAV";
    let width = |font: &CompiledFont| render(font, text).width;

    let (cancelled, plain) = case(0.6, &cancel);
    assert_eq!(width(&cancelled), width(&plain), "cancelling every pair renders like a face without them");

    let (tighter, plain) = case(0.7, &tighter_pairs);
    let face = ttf_parser::Face::parse(&tighter.bytes, 0).expect("the rewritten face parses");
    let post_script = face
        .names()
        .into_iter()
        .filter(|name| name.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
        .find_map(|name| name.to_string());
    assert_eq!(post_script.as_deref(), Some(tighter.postscript_name.as_str()), "the PostScript name is kept");
    // Nine pairs of -150 per mille at 40 px: about 54 px narrower than without kerning.
    let (tight_width, plain_width) = (width(&tighter), width(&plain));
    assert!(tight_width + 30 < plain_width, "baked pairs tighten the run: {tight_width} vs {plain_width}");
}

#[test]
fn three_and_four_sources_compile() {
    let (Some(regular), Some(bold), Some(han)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD), bundled(SOURCE_HAN)) else {
        return;
    };
    let set = chars("AgЖ8한");
    let three = recipe(&[("Regular", 1.0, None), ("Bold", 1.0, None), ("Han", 2.0, None)], MissingGlyphPolicy::NoInfluence);
    let sources = [source("Regular", &regular, &[]), source("Bold", &bold, &[]), source("Han", &han, &[])];
    let compiled = run(&three, &sources, CompileScope::Subset(&set));
    let face = ttf_parser::Face::parse(&compiled.bytes, 0).expect("valid");
    assert!(set.iter().all(|&ch| face.glyph_index(ch).is_some()));

    let four = recipe(
        &[("Regular", 1.0, None), ("Bold", 1.0, None), ("Han", 1.0, None), ("Regular2", 1.0, None)],
        MissingGlyphPolicy::Disable,
    );
    let sources = [
        source("Regular", &regular, &[]),
        source("Bold", &bold, &[]),
        source("Han", &han, &[]),
        source("Regular2", &regular, &[]),
    ];
    let compiled = run(&four, &sources, CompileScope::Subset(&set));
    let face = ttf_parser::Face::parse(&compiled.bytes, 0).expect("valid");
    assert!(face.glyph_index('한').is_none(), "missing in three sources with Disable");
    assert!(face.glyph_index('g').is_some());
}

/// `recipe` with an algorithm and property weights.
fn recipe_with(
    sources: &[(&str, f32, Option<MissingGlyphPolicy>)],
    missing_default: MissingGlyphPolicy,
    algorithm: BlendAlgorithm,
    property_weights: Option<Vec<PropertyWeights>>,
) -> CustomFontRecipe {
    let mut built = recipe(sources, missing_default);
    let CustomFontKind::Hybrid(params) = &mut built.kind;
    params.algorithm = algorithm;
    params.property_weights = property_weights;
    built
}

/// The raw bytes of table `tag` of an sfnt file, `None` when absent.
fn table(font: &[u8], tag: [u8; 4]) -> Option<&[u8]> {
    let count = usize::from(u16::from_be_bytes([*font.get(4)?, *font.get(5)?]));
    (0..count).find_map(|index| {
        let record = font.get(12 + 16 * index..28 + 16 * index)?;
        if record[..4] != tag {
            return None;
        }
        let read = |at: usize| usize::try_from(u32::from_be_bytes([record[at], record[at + 1], record[at + 2], record[at + 3]])).ok();
        let (offset, length) = (read(8)?, read(12)?);
        font.get(offset..offset + length)
    })
}

/// Compiled glyphs of a face (every glyph but `.notdef`).
fn compiled_glyphs(compiled: &CompiledFont) -> usize {
    usize::from(ttf_parser::Face::parse(&compiled.bytes, 0).expect("valid").number_of_glyphs()) - 1
}

#[test]
fn property_weights_equal_to_the_plain_weights_produce_the_same_glyphs() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    let set = chars("AaBgQ@&8ЖжЯ한글 AV");
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    let plain = recipe(&[("Noto", 0.6, None), ("Han", 0.4, None)], MissingGlyphPolicy::NoInfluence);
    let same = Some(vec![
        PropertyWeights { shape: 0.6, spacing: 0.6, kerning: 0.6 },
        PropertyWeights { shape: 0.4, spacing: 0.4, kerning: 0.4 },
    ]);
    let split = recipe_with(&[("Noto", 0.6, None), ("Han", 0.4, None)], MissingGlyphPolicy::NoInfluence, BlendAlgorithm::Auto, same);
    let (a, b) = (run(&plain, &sources, CompileScope::Subset(&set)), run(&split, &sources, CompileScope::Subset(&set)));
    // The names (and `head`'s whole-file checksum) carry the key, which differs by the kind
    // tag; every table the blend produces must be byte-identical.
    assert_ne!(a.key, b.key);
    for tag in [b"glyf", b"loca", b"hmtx", b"hhea", b"kern", b"cmap", b"OS/2", b"maxp", b"post"] {
        let (left, right) = (table(&a.bytes, *tag), table(&b.bytes, *tag));
        assert!(left.is_some() || tag == b"kern", "table {} present", String::from_utf8_lossy(tag));
        assert_eq!(left, right, "table {} differs", String::from_utf8_lossy(tag));
    }
    assert_eq!(a.stats, b.stats);
}

#[test]
fn shapes_from_one_source_and_spacing_from_another() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let set = chars("lonH");
    let weights = Some(vec![
        PropertyWeights { shape: 1.0, spacing: 0.0001, kerning: 1.0 },
        PropertyWeights { shape: 0.0001, spacing: 1.0, kerning: 1.0 },
    ]);
    let recipe = recipe_with(&[("Regular", 1.0, None), ("Bold", 1.0, None)], MissingGlyphPolicy::Disable, BlendAlgorithm::Auto, weights);
    let compiled = run(&recipe, &[source("Regular", &regular, &[]), source("Bold", &bold, &[])], CompileScope::Subset(&set));
    let face = ttf_parser::Face::parse(&compiled.bytes, 0).expect("valid");
    let bold_face = ttf_parser::Face::parse(&bold, 0).expect("valid");
    let advance = |face: &ttf_parser::Face<'_>, ch: char| face.glyph_index(ch).and_then(|glyph| face.glyph_hor_advance(glyph)).map(i32::from);
    for ch in set {
        let (mixed, spacing_source) = (advance(&face, ch).expect("mapped"), advance(&bold_face, ch).expect("mapped"));
        assert!((mixed - spacing_source).abs() <= 1, "{ch}: advance {mixed} follows the spacing source's {spacing_source}");
        let distance = hausdorff(&outline_of(&compiled.bytes, ch).expect("ink"), &outline_of(&regular, ch).expect("ink"));
        assert!(distance < 2.0, "{ch}: outline follows the shape source within 2 units, got {distance}");
    }
}

#[test]
fn no_blend_copies_the_heaviest_source_and_ties_go_to_recipe_order() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let set = chars("lonH");
    let compile_no_blend = |sources: &[(&str, f32, Option<MissingGlyphPolicy>)], fonts: &[SourceFont<'_>]| {
        run(&recipe_with(sources, MissingGlyphPolicy::Disable, BlendAlgorithm::NoBlend, None), fonts, CompileScope::Subset(&set))
    };
    let regular_first = [source("Regular", &regular, &[]), source("Bold", &bold, &[])];
    let bold_first = [source("Bold", &bold, &[]), source("Regular", &regular, &[])];
    let cases: [(CompiledFont, &[u8], &[u8]); 3] = [
        (compile_no_blend(&[("Regular", 1.0, None), ("Bold", 3.0, None)], &regular_first), &bold, &regular),
        (compile_no_blend(&[("Regular", 1.0, None), ("Bold", 1.0, None)], &regular_first), &regular, &bold),
        (compile_no_blend(&[("Bold", 1.0, None), ("Regular", 1.0, None)], &bold_first), &bold, &regular),
    ];
    for (compiled, winner, loser) in &cases {
        assert_eq!(compiled.stats.no_blend, compiled_glyphs(compiled));
        for ch in set.iter().copied() {
            let output = outline_of(&compiled.bytes, ch).expect("ink");
            let copied = hausdorff(&output, &outline_of(winner, ch).expect("ink"));
            assert!(copied < 0.75, "{ch}: the winner's outline is copied (quantization only), got {copied}");
            assert!(hausdorff(&output, &outline_of(loser, ch).expect("ink")) > 5.0, "{ch}: not the other source");
        }
    }
}

#[test]
fn forced_sdf_and_the_stats_count_every_glyph() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let sources = [source("Regular", &regular, &[]), source("Bold", &bold, &[])];
    let inked = chars("lonH");
    let auto = run(&recipe(&[("Regular", 1.0, None), ("Bold", 1.0, None)], MissingGlyphPolicy::Disable), &sources, CompileScope::Subset(&inked));
    assert!(auto.stats.compatible + auto.stats.matched > 0, "same-family glyphs interpolate under Auto: {:?}", auto.stats);
    assert_eq!(auto.stats.sdf + auto.stats.compatible + auto.stats.matched, compiled_glyphs(&auto));
    let forced = recipe_with(&[("Regular", 1.0, None), ("Bold", 1.0, None)], MissingGlyphPolicy::Disable, BlendAlgorithm::Sdf, None);
    let sdf = run(&forced, &sources, CompileScope::Subset(&inked));
    assert_eq!(sdf.stats.sdf, compiled_glyphs(&sdf));
    assert_ne!(table(&sdf.bytes, *b"glyf"), table(&auto.bytes, *b"glyf"));
    // With a space (no ink anywhere) the copy count joins in; the counts add up to the glyphs.
    let with_space = chars("lo H");
    let sdf = run(&forced, &sources, CompileScope::Subset(&with_space));
    assert_eq!((sdf.stats.copied, sdf.stats.sdf), (1, 3));
    assert_eq!(sdf.stats.glyphs(), compiled_glyphs(&sdf));
}

#[test]
fn skeleton_and_transfer_count_their_paths() {
    let (Some(regular), Some(bold), Some(han)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD), bundled(SOURCE_HAN)) else {
        return;
    };
    let set = chars("lo AgЖ");
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    let skeleton = recipe_with(&[("Noto", 1.0, None), ("Han", 1.0, None)], MissingGlyphPolicy::NoInfluence, BlendAlgorithm::Skeleton, None);
    let compiled = run(&skeleton, &sources, CompileScope::Subset(&set));
    let stats = &compiled.stats;
    // Skeleton never takes the matched path or Auto's SDF path; a glyph it gives up on is
    // counted as a fallback with its reason.
    assert_eq!((stats.matched, stats.sdf, stats.no_blend, stats.transfer), (0, 0, 0, 0));
    assert_eq!(stats.glyphs(), compiled_glyphs(&compiled));
    assert_eq!(stats.skeleton_fallback_reasons.values().sum::<usize>(), stats.skeleton_fallback);

    // Transfer: '한' is absent from the base (Regular, NoInfluence) and inked in two other
    // sources, so it is transferred from the heaviest of them and counted as substituted.
    let transfer_set = chars("lo 한");
    let transfer = recipe_with(
        &[("Regular", 1.0, None), ("Han", 1.0, None), ("Han2", 1.0, None), ("Bold", 1.0, None)],
        MissingGlyphPolicy::NoInfluence,
        BlendAlgorithm::Transfer { base_source: 0 },
        None,
    );
    let fonts = [source("Regular", &regular, &[]), source("Han", &han, &[]), source("Han2", &han, &[]), source("Bold", &bold, &[])];
    let compiled = run(&transfer, &fonts, CompileScope::Subset(&transfer_set));
    let stats = &compiled.stats;
    assert_eq!((stats.transfer, stats.transfer_base_substituted, stats.copied), (3, 1, 1), "{stats:?}");
    assert_eq!(stats.glyphs(), compiled_glyphs(&compiled));

    // A glyph with ink in ONE source is transferred too (font-wide parameters): '한' only in
    // Han (not the base) is substituted, 'l' / 'o' use the base; the space
    // (no ink) stays a copy.
    let pair = recipe_with(&[("Regular", 1.0, None), ("Han", 1.0, None)], MissingGlyphPolicy::NoInfluence, BlendAlgorithm::Transfer { base_source: 0 }, None);
    let compiled = run(&pair, &[source("Regular", &regular, &[]), source("Han", &han, &[])], CompileScope::Subset(&transfer_set));
    let stats = &compiled.stats;
    assert_eq!((stats.transfer, stats.transfer_base_substituted, stats.copied), (3, 1, 1), "{stats:?}");
    assert_eq!(stats.glyphs(), compiled_glyphs(&compiled));
}

#[test]
fn every_algorithm_is_deterministic_across_thread_counts() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    let set = chars("AaBgQ@&8Жж한 ");
    let sources = [source("Noto", &regular, &[]), source("Han", &han, &[])];
    let one = rayon::ThreadPoolBuilder::new().num_threads(1).build().expect("pool");
    let many = rayon::ThreadPoolBuilder::new().num_threads(4).build().expect("pool");
    for algorithm in [BlendAlgorithm::Sdf, BlendAlgorithm::NoBlend, BlendAlgorithm::Skeleton, BlendAlgorithm::Transfer { base_source: 1 }] {
        let recipe = recipe_with(&[("Noto", 0.5, None), ("Han", 0.5, None)], MissingGlyphPolicy::NoInfluence, algorithm, None);
        let first = one.install(|| run(&recipe, &sources, CompileScope::Subset(&set)));
        let second = many.install(|| run(&recipe, &sources, CompileScope::Subset(&set)));
        assert_eq!(first.bytes, second.bytes, "{algorithm:?}");
        assert_eq!(first.stats, second.stats, "{algorithm:?}");
    }
}
