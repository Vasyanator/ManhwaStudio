/*
File: crates/ms-custom-fonts/src/hybrid/skeleton/tests.rs

Purpose:
Tests of the skeleton blend: synthetic strokes (thickness interpolation, corners, orientation),
bundled `fonts/ui` glyphs (skip when absent), hostile input and determinism.
*/

use std::path::Path;

use kurbo::Point;

use super::{SkeletonFallback, blend};
use crate::hybrid::sdf;
use crate::outline::{Contour, Outline, OutlineCollector, Seg, flatten_contour};

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

/// Outline and advance of `ch` (normalized to 1000 units per em).
fn glyph(data: &[u8], ch: char) -> Option<(Outline, f64)> {
    let face = ttf_parser::Face::parse(data, 0).ok()?;
    let id = face.glyph_index(ch).filter(|id| id.0 != 0)?;
    let scale = 1000.0 / f64::from(face.units_per_em());
    let mut collector = OutlineCollector::new(scale);
    face.outline_glyph(id, &mut collector)?;
    let advance = f64::from(face.glyph_hor_advance(id)?) * scale;
    let mut outline = collector.finish();
    if face.tables().glyf.is_none() && face.tables().cff.is_some() {
        // CFF contours run counter-clockwise; the crate's model is TrueType (outer clockwise).
        outline = outline.iter().map(crate::outline::reverse_contour).collect();
    }
    Some((outline, advance))
}

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Contour {
    // Clockwise in y-up coordinates (TrueType outer contour).
    Contour { start: Point::new(x0, y0), segs: vec![Seg::Line(Point::new(x0, y1)), Seg::Line(Point::new(x1, y1)), Seg::Line(Point::new(x1, y0))] }
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

/// Shoelace area of a polygon (positive = counter-clockwise in y-up coordinates).
fn signed_area(polygon: &[Point]) -> f64 {
    let mut sum = 0.0;
    for (position, a) in polygon.iter().enumerate() {
        let b = polygon[(position + 1) % polygon.len()];
        sum += a.x * b.y - b.x * a.y;
    }
    0.5 * sum
}

/// Nonzero-winding inside test.
fn inside(point: Point, polygons: &[Vec<Point>]) -> bool {
    let mut winding = 0;
    for polygon in polygons {
        for (position, &a) in polygon.iter().enumerate() {
            let b = polygon[(position + 1) % polygon.len()];
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

/// Filled area of an outline (nonzero), on a 2-unit lattice.
fn filled_area(outline: &Outline) -> f64 {
    let polygons = polygons(outline);
    let Some(bounds) = sdf::bounds(&polygons) else {
        return 0.0;
    };
    let mut count = 0_u32;
    let mut y = bounds.y0 + 1.0;
    while y < bounds.y1 {
        let mut x = bounds.x0 + 1.0;
        while x < bounds.x1 {
            count += u32::from(inside(Point::new(x, y), &polygons));
            x += 2.0;
        }
        y += 2.0;
    }
    f64::from(count) * 4.0
}

/// Width of the ink run containing `x_hint` on the horizontal line `y` (0 when outside).
fn run_width(outline: &Outline, y: f64, x_hint: f64) -> f64 {
    let polygons = polygons(outline);
    if !inside(Point::new(x_hint, y), &polygons) {
        return 0.0;
    }
    let step = 0.05;
    let mut left = x_hint;
    while inside(Point::new(left - step, y), &polygons) {
        left -= step;
    }
    let mut right = x_hint;
    while inside(Point::new(right + step, y), &polygons) {
        right += step;
    }
    right - left
}

/// Every contour is closed with at least 3 points and the outermost (largest) contour runs
/// clockwise, like the crate's outlines.
fn assert_valid(outline: &Outline) {
    assert_ne!(outline.as_slice(), []);
    let polygons = polygons(outline);
    for polygon in &polygons {
        assert!(polygon.len() >= 3);
        assert!(polygon.iter().all(|point| point.x.is_finite() && point.y.is_finite()));
    }
    let largest = polygons.iter().max_by(|a, b| signed_area(a).abs().total_cmp(&signed_area(b).abs())).expect("non-empty");
    assert!(signed_area(largest) < 0.0, "outer contour must be clockwise");
}

#[test]
fn stems_blend_to_the_intermediate_thickness() {
    // Same centre line x = 200, widths 80 and 160.
    let thin = vec![rectangle(160.0, 0.0, 240.0, 700.0)];
    let thick = vec![rectangle(120.0, 0.0, 280.0, 700.0)];
    for (weight, expected) in [(0.5, 120.0), (0.25, 140.0), (0.75, 100.0)] {
        let blended = blend(&[&thin, &thick], &[weight, 1.0 - weight], &[400.0, 400.0]).expect("a stem blends by skeleton");
        assert_valid(&blended);
        assert_eq!(blended.len(), 1, "one contour");
        let width = run_width(&blended, 350.0, 200.0);
        assert!((width - expected).abs() < 3.0, "weight {weight}: width {width}, expected {expected}");
        // Flat terminals survive (corner spurs): the corners are filled, the ends reach 0 / 700.
        let half = expected / 2.0;
        let polygons = polygons(&blended);
        for corner in [Point::new(200.0 - half + 6.0, 6.0), Point::new(200.0 + half - 6.0, 694.0)] {
            assert!(inside(corner, &polygons), "weight {weight}: corner {corner:?} not filled");
        }
        let bounds = sdf::bounds(&polygons).expect("ink");
        assert!(bounds.y0.abs() < 3.0 && (bounds.y1 - 700.0).abs() < 3.0, "weight {weight}: {bounds:?}");
    }
}

#[test]
fn orientation_matches_the_distance_field_path() {
    let a = vec![rectangle(100.0, 0.0, 180.0, 700.0)];
    let b = vec![rectangle(90.0, 0.0, 200.0, 700.0)];
    let skeleton = blend(&[&a, &b], &[0.5, 0.5], &[300.0, 300.0]).expect("blends");
    let field = sdf::blend(&[&a, &b], &[0.5, 0.5], &[300.0, 300.0]).expect("blends");
    let clockwise = |outline: &Outline| signed_area(&polygons(outline)[0]) < 0.0;
    assert_eq!(clockwise(&skeleton), clockwise(&field));
}

#[test]
fn a_stroke_one_design_lacks_grows_with_its_weight() {
    // An "L" against a plain stem: the foot exists in one source only.
    let stem = vec![rectangle(100.0, 0.0, 180.0, 700.0)];
    let ell = vec![Contour {
        start: Point::new(100.0, 0.0),
        segs: vec![
            Seg::Line(Point::new(100.0, 700.0)),
            Seg::Line(Point::new(180.0, 700.0)),
            Seg::Line(Point::new(180.0, 80.0)),
            Seg::Line(Point::new(500.0, 80.0)),
            Seg::Line(Point::new(500.0, 0.0)),
        ],
    }];
    let reach = |weight: f64| -> f64 {
        let blended = blend(&[&ell, &stem], &[weight, 1.0 - weight], &[550.0, 550.0]).expect("blends by skeleton");
        assert_valid(&blended);
        sdf::bounds(&polygons(&blended)).expect("ink").x1
    };
    let (quarter, half, three_quarters) = (reach(0.25), reach(0.5), reach(0.75));
    assert!(quarter < half && half < three_quarters, "{quarter} {half} {three_quarters}");
    // The foot grows from the stem (it does not fade in place): its length scales with the weight.
    let foot = |x1: f64| x1 - 180.0;
    let ratio = foot(quarter) / foot(half);
    assert!((ratio - 0.5).abs() < 0.15 * 0.5 + 0.1, "quarter/half foot ratio {ratio}");
}

#[test]
fn hostile_input_falls_back_without_panicking() {
    let normal = vec![rectangle(0.0, 0.0, 100.0, 700.0)];
    let broken = vec![rectangle(0.0, 0.0, f64::NAN, 700.0)];
    assert!(matches!(blend(&[&broken, &normal], &[0.5, 0.5], &[300.0, 300.0]), Err(SkeletonFallback::Extent(_))));
    let huge = vec![rectangle(0.0, 0.0, 1.0e9, 1.0e9)];
    assert!(matches!(blend(&[&huge, &normal], &[0.5, 0.5], &[300.0, 300.0]), Err(SkeletonFallback::Extent(_))));
    // A degenerate contour (no area) has no ink on the grid.
    let flat = vec![rectangle(0.0, 0.0, 100.0, 0.0)];
    assert!(blend(&[&flat, &normal], &[0.5, 0.5], &[300.0, 300.0]).is_err());
    // 200 disjoint bars: more strokes than the matcher takes.
    let bars: Outline = (0..200).map(|k| rectangle(f64::from(k) * 40.0, 0.0, f64::from(k) * 40.0 + 20.0, 700.0)).collect();
    assert!(matches!(blend(&[&bars, &normal], &[0.5, 0.5], &[8000.0, 300.0]), Err(SkeletonFallback::TooManyStrokes { .. })));
    // Unrelated shapes: a ring against a bar far away does not match.
    let bar = vec![rectangle(0.0, 0.0, 700.0, 60.0)];
    let ring = vec![rectangle(0.0, 0.0, 700.0, 700.0), {
        let mut inner = rectangle(80.0, 80.0, 620.0, 620.0);
        inner = crate::outline::reverse_contour(&inner);
        inner
    }];
    assert!(blend(&[&ring, &bar], &[0.5, 0.5], &[700.0, 700.0]).is_err());
}

#[test]
fn blending_is_deterministic() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    for ch in ['g', 'R', '&', 'Ж'] {
        let (Some((a, advance_a)), Some((b, advance_b))) = (glyph(&regular, ch), glyph(&han, ch)) else {
            continue;
        };
        let first = blend(&[&a, &b], &[0.4, 0.6], &[advance_a, advance_b]);
        let second = blend(&[&a, &b], &[0.4, 0.6], &[advance_a, advance_b]);
        assert_eq!(first, second, "{ch}");
    }
}

#[test]
fn regular_and_bold_stems_blend_between() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let (Some((a, advance_a)), Some((b, advance_b))) = (glyph(&regular, 'l'), glyph(&bold, 'l')) else {
        panic!("Noto has 'l'");
    };
    let blended = blend(&[&a, &b], &[0.5, 0.5], &[advance_a, advance_b]).expect("'l' blends by skeleton");
    assert_valid(&blended);
    let bounds = sdf::bounds(&polygons(&blended)).expect("ink");
    let mid_y = f64::midpoint(bounds.y0, bounds.y1);
    let mid_x = f64::midpoint(bounds.x0, bounds.x1);
    let width_of = |outline: &Outline| {
        let box_ = sdf::bounds(&polygons(outline)).expect("ink");
        run_width(outline, mid_y, f64::midpoint(box_.x0, box_.x1))
    };
    let (thin, thick, mixed) = (width_of(&a), width_of(&b), run_width(&blended, mid_y, mid_x));
    assert!(thin < thick);
    assert!((mixed - f64::midpoint(thin, thick)).abs() < 3.0, "stem {mixed}, sources {thin} / {thick}");
}

#[test]
fn bowls_keep_their_counters() {
    let (Some(regular), Some(bold)) = (bundled(NOTO_REGULAR), bundled(NOTO_BOLD)) else {
        return;
    };
    let (Some((a, advance_a)), Some((b, advance_b))) = (glyph(&regular, 'o'), glyph(&bold, 'o')) else {
        panic!("Noto has 'o'");
    };
    let blended = blend(&[&a, &b], &[0.5, 0.5], &[advance_a, advance_b]).expect("'o' blends by skeleton");
    assert_valid(&blended);
    assert_eq!(blended.len(), 2, "outer contour and counter");
    let (area_a, area_b, area) = (filled_area(&a), filled_area(&b), filled_area(&blended));
    assert!(area > area_a.min(area_b) * 0.97 && area < area_a.max(area_b) * 1.03, "{area} vs {area_a} / {area_b}");
}

#[test]
fn different_designs_produce_plausible_outlines() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    // Same letters drawn by two unrelated designs.
    for ch in ['g', '1', 'a', 'R', 'Q', 'k'] {
        let (Some((a, advance_a)), Some((b, advance_b))) = (glyph(&regular, ch), glyph(&han, ch)) else {
            continue;
        };
        match blend(&[&a, &b], &[0.5, 0.5], &[advance_a, advance_b]) {
            Ok(blended) => {
                assert_valid(&blended);
                let (area_a, area_b, area) = (filled_area(&a), filled_area(&b), filled_area(&blended));
                assert!(area > 0.7 * area_a.min(area_b) && area < 1.3 * area_a.max(area_b), "{ch}: {area} vs {area_a} / {area_b}");
            }
            Err(fallback) => eprintln!("{ch}: skeleton fell back ({fallback})"),
        }
    }
}

#[test]
fn a_foot_one_design_lacks_grows_with_its_weight() {
    let (Some(regular), Some(han)) = (bundled(NOTO_REGULAR), bundled(SOURCE_HAN)) else {
        return;
    };
    // Source Han's "1" stands on a foot, Noto's does not.
    let (Some((plain, advance_plain)), Some((footed, advance_footed))) = (glyph(&regular, '1'), glyph(&han, '1')) else {
        panic!("both fonts have '1'");
    };
    let foot_width = |weight: f64| -> f64 {
        let blended = blend(&[&footed, &plain], &[weight, 1.0 - weight], &[advance_footed, advance_plain]).expect("'1' blends by skeleton");
        assert_valid(&blended);
        let polygons = polygons(&blended);
        let bounds = sdf::bounds(&polygons).expect("ink");
        // Ink width just above the baseline.
        let y = bounds.y0 + 15.0;
        let mut x = bounds.x0;
        let (mut left, mut right) = (f64::INFINITY, f64::NEG_INFINITY);
        while x <= bounds.x1 {
            if inside(Point::new(x, y), &polygons) {
                left = left.min(x);
                right = right.max(x);
            }
            x += 0.5;
        }
        right - left
    };
    let widths = [foot_width(0.25), foot_width(0.5), foot_width(0.75)];
    assert!(widths[0] < widths[1] && widths[1] < widths[2], "foot grows with the footed design's weight: {widths:?}");
}

#[test]
fn a_glyph_with_too_many_strokes_compiles_through_the_distance_field() {
    use std::sync::atomic::AtomicBool;

    use crate::outline::TtPoint;
    use crate::sfnt::{FontBuild, FontMetrics, FontNames, GlyphData, write_font};
    use crate::{BlendAlgorithm, CompileScope, CustomFontKind, CustomFontRecipe, HybridParams, HybridSource, MissingGlyphPolicy, SourceFont, compile};

    let point = |x, y| TtPoint { x, y, on: true };
    let bar = |x: i16| vec![point(x, 0), point(x, 700), point(x + 20, 700), point(x + 20, 0)];
    let face = |contours: Vec<Vec<TtPoint>>, advance: u16, name: &str| {
        write_font(&FontBuild {
            glyphs: vec![GlyphData { advance: 500, contours: Vec::new() }, GlyphData { advance, contours }],
            cmap: vec![('A', 1)],
            kern_pairs: Vec::new(),
            metrics: FontMetrics { ascender: 800, descender: -200, weight_class: 400, ..FontMetrics::default() },
            names: FontNames {
                family: name.to_owned(),
                subfamily: "Regular".to_owned(),
                unique_id: name.to_owned(),
                full_name: name.to_owned(),
                version: "Version 1.000".to_owned(),
                postscript: name.to_owned(),
            },
        })
        .expect("writes")
    };
    let comb = face((0..200).map(|k| bar(k * 40)).collect(), 8000, "Comb");
    let stem = face(vec![bar(100)], 300, "Stem");
    let recipe = CustomFontRecipe {
        kind: CustomFontKind::Hybrid(HybridParams {
            sources: ["Comb", "Stem"].iter().map(|&identity| HybridSource { identity: identity.to_owned(), weight: 1.0, missing_override: None }).collect(),
            missing_default: MissingGlyphPolicy::NoInfluence,
            algorithm: BlendAlgorithm::Skeleton,
            property_weights: None,
        }),
    };
    let sources = [
        SourceFont { identity: "Comb", data: &comb, face_index: 0, user_kerning: &[] },
        SourceFont { identity: "Stem", data: &stem, face_index: 0, user_kerning: &[] },
    ];
    let compiled = compile(&recipe, &sources, CompileScope::Full, &|_, _| {}, &AtomicBool::new(false)).expect("compiles through the fallback");
    assert_eq!(compiled.stats.skeleton_fallback, 1, "{:?}", compiled.stats);
    assert_eq!(compiled.stats.skeleton_fallback_reasons.get("too_many_strokes"), Some(&1));
}

#[test]
fn dense_outlines_are_refused_before_raster_work() {
    // Many tiny disjoint contours (what the 16-bit point / contour envelope of a hostile font
    // allows): more contours than the skeleton analysis takes.
    let tiny: Outline = (0..21_845).map(|k| {
        let x = f64::from(k % 200) * 4.0;
        let y = f64::from(k / 200) * 4.0;
        Contour { start: Point::new(x, y), segs: vec![Seg::Line(Point::new(x, y + 2.0)), Seg::Line(Point::new(x + 2.0, y))] }
    }).collect();
    let normal = vec![rectangle(0.0, 0.0, 100.0, 700.0)];
    assert_eq!(blend(&[&tiny, &normal], &[0.5, 0.5], &[800.0, 300.0]), Err(SkeletonFallback::TooComplex));
    // Few contours, but curves that flatten 64-fold: refused by the point budget.
    let wavy = vec![Contour {
        start: Point::new(0.0, 0.0),
        segs: (0..400).map(|k| {
            let x = f64::from(k + 1) * 2.0;
            Seg::Cubic(Point::new(x - 1.5, 5000.0), Point::new(x - 0.5, -5000.0), Point::new(x, 0.0))
        }).collect(),
    }];
    assert_eq!(blend(&[&wavy, &normal], &[0.5, 0.5], &[800.0, 300.0]), Err(SkeletonFallback::TooComplex));
}
